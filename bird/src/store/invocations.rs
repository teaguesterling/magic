//! Invocation storage operations.

use std::fs;

use duckdb::params;

use super::atomic;
use super::{sanitize_filename, Store};
use crate::config::StorageMode;
use crate::query::{CompareOp, Query, QueryComponent};
use crate::schema::InvocationRecord;
use crate::Result;

/// Summary of an invocation (for listing).
#[derive(Debug)]
pub struct InvocationSummary {
    pub id: String,
    pub cmd: String,
    pub exit_code: i32,
    pub timestamp: String,
    pub duration_ms: Option<i64>,
}

impl Store {
    /// Write an invocation record to the store.
    ///
    /// Behavior depends on storage mode:
    /// - Parquet: Creates a new Parquet file in the appropriate date partition
    /// - DuckDB: Inserts directly into the local.invocations
    pub fn write_invocation(&self, record: &InvocationRecord) -> Result<()> {
        match self.config.storage_mode {
            StorageMode::Parquet => self.write_invocation_parquet(record),
            StorageMode::DuckDB => self.write_invocation_duckdb(record),
        }
    }

    /// Write invocation to a Parquet file (multi-writer safe).
    fn write_invocation_parquet(&self, record: &InvocationRecord) -> Result<()> {
        let conn = self.connection_with_options(false)?;
        let date = record.date();

        // Ensure the partition directory exists (status-partitioned)
        let partition_dir = self.config.invocations_dir_with_status(&record.status, &date);
        fs::create_dir_all(&partition_dir)?;

        // Generate filename: {session}--{executable}--{id}.parquet
        let executable = record.executable.as_deref().unwrap_or("unknown");
        let filename = format!(
            "{}--{}--{}.parquet",
            sanitize_filename(&record.session_id),
            sanitize_filename(executable),
            record.id
        );
        let file_path = partition_dir.join(&filename);

        // Write via DuckDB using COPY
        conn.execute_batch(
            r#"
            CREATE OR REPLACE TEMP TABLE temp_invocation (
                id UUID,
                session_id VARCHAR,
                timestamp TIMESTAMP,
                duration_ms BIGINT,
                cwd VARCHAR,
                cmd VARCHAR,
                executable VARCHAR,
                runner_id VARCHAR,
                exit_code INTEGER,
                status VARCHAR,
                format_hint VARCHAR,
                client_id VARCHAR,
                hostname VARCHAR,
                username VARCHAR,
                tag VARCHAR,
                date DATE
            );
            "#,
        )?;

        conn.execute(
            r#"
            INSERT INTO temp_invocation VALUES (
                ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?
            )
            "#,
            params![
                record.id.to_string(),
                record.session_id,
                record.timestamp.to_rfc3339(),
                record.duration_ms,
                record.cwd,
                record.cmd,
                record.executable,
                record.runner_id,
                record.exit_code,
                record.status,
                record.format_hint,
                record.client_id,
                record.hostname,
                record.username,
                record.tag,
                date.to_string(),
            ],
        )?;

        // Atomic write: COPY to temp file, then rename
        let temp_path = atomic::temp_path(&file_path);
        conn.execute(
            &format!(
                "COPY temp_invocation TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD)",
                temp_path.display()
            ),
            [],
        )?;
        conn.execute("DROP TABLE temp_invocation", [])?;

        // Rename temp to final (atomic on POSIX)
        atomic::rename_into_place(&temp_path, &file_path)?;

        Ok(())
    }

    /// Write invocation directly to DuckDB table.
    fn write_invocation_duckdb(&self, record: &InvocationRecord) -> Result<()> {
        let conn = self.connection()?;
        let date = record.date();

        conn.execute(
            r#"
            INSERT INTO local.invocations VALUES (
                ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?
            )
            "#,
            params![
                record.id.to_string(),
                record.session_id,
                record.timestamp.to_rfc3339(),
                record.duration_ms,
                record.cwd,
                record.cmd,
                record.executable,
                record.runner_id,
                record.exit_code,
                record.status,
                record.format_hint,
                record.client_id,
                record.hostname,
                record.username,
                record.tag,
                date.to_string(),
            ],
        )?;

        Ok(())
    }

    /// Get recent invocations (last 7 days).
    pub fn recent_invocations(&self, limit: usize) -> Result<Vec<InvocationSummary>> {
        let conn = self.connection()?;

        let sql = format!(
            r#"
            SELECT id::VARCHAR, cmd, exit_code, timestamp::VARCHAR, duration_ms
            FROM recent_invocations
            LIMIT {}
            "#,
            limit
        );

        let mut stmt = match conn.prepare(&sql) {
            Ok(stmt) => stmt,
            Err(e) => {
                if e.to_string().contains("No files found") {
                    return Ok(Vec::new());
                }
                return Err(e.into());
            }
        };

        let rows = stmt.query_map([], |row| {
            Ok(InvocationSummary {
                id: row.get(0)?,
                cmd: row.get(1)?,
                exit_code: row.get(2)?,
                timestamp: row.get(3)?,
                duration_ms: row.get(4)?,
            })
        });

        match rows {
            Ok(rows) => {
                let mut results = Vec::new();
                for row in rows {
                    results.push(row?);
                }
                Ok(results)
            }
            Err(e) => {
                if e.to_string().contains("No files found") {
                    Ok(Vec::new())
                } else {
                    Err(e.into())
                }
            }
        }
    }

    /// Get the last invocation (most recent).
    pub fn last_invocation(&self) -> Result<Option<InvocationSummary>> {
        let invocations = self.recent_invocations(1)?;
        Ok(invocations.into_iter().next())
    }

    /// Query invocations with filters from the query micro-language.
    ///
    /// Supports:
    /// - `~N` range selector (limit to N results)
    /// - `%exit<>0` field filters (exit code, duration, etc.)
    /// - `%/pattern/` command regex
    ///
    /// Use `default_limit` to specify the limit when no range is provided:
    /// - 20 for listing commands (shq i)
    /// - 1 for single-item commands (shq o, shq I, shq R)
    pub fn query_invocations_with_limit(
        &self,
        query: &Query,
        default_limit: usize,
    ) -> Result<Vec<InvocationSummary>> {
        let conn = self.connection()?;

        // Build WHERE clauses from query filters
        let mut where_clauses: Vec<String> = Vec::new();

        for component in &query.filters {
            match component {
                QueryComponent::CommandRegex(pattern) => {
                    // Use regexp_matches for regex filtering
                    let escaped = pattern.replace('\'', "''");
                    where_clauses.push(format!("regexp_matches(cmd, '{}')", escaped));
                }
                QueryComponent::FieldFilter(filter) => {
                    // Map field names to SQL column names
                    let column = match filter.field.as_str() {
                        "exit" | "exit_code" => "exit_code",
                        "duration" | "duration_ms" => "duration_ms",
                        "cmd" | "command" => "cmd",
                        "cwd" => "cwd",
                        other => other, // Pass through unknown fields
                    };

                    let escaped_value = filter.value.replace('\'', "''");

                    let clause = match filter.op {
                        CompareOp::Eq => format!("{} = '{}'", column, escaped_value),
                        CompareOp::NotEq => format!("{} <> '{}'", column, escaped_value),
                        CompareOp::Gt => format!("{} > '{}'", column, escaped_value),
                        CompareOp::Lt => format!("{} < '{}'", column, escaped_value),
                        CompareOp::Gte => format!("{} >= '{}'", column, escaped_value),
                        CompareOp::Lte => format!("{} <= '{}'", column, escaped_value),
                        CompareOp::Regex => {
                            format!("regexp_matches({}::VARCHAR, '{}')", column, escaped_value)
                        }
                    };
                    where_clauses.push(clause);
                }
                QueryComponent::Tag(_) => {
                    // Tags not implemented in MVP
                }
            }
        }

        // Build the SQL query
        let where_sql = if where_clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", where_clauses.join(" AND "))
        };

        let limit = query.range.map(|r| r.start).unwrap_or(default_limit);

        let sql = format!(
            r#"
            SELECT id::VARCHAR, cmd, exit_code, timestamp::VARCHAR, duration_ms
            FROM recent_invocations
            {}
            LIMIT {}
            "#,
            where_sql, limit
        );

        let mut stmt = match conn.prepare(&sql) {
            Ok(stmt) => stmt,
            Err(e) => {
                if e.to_string().contains("No files found") {
                    return Ok(Vec::new());
                }
                return Err(e.into());
            }
        };

        let rows = stmt.query_map([], |row| {
            Ok(InvocationSummary {
                id: row.get(0)?,
                cmd: row.get(1)?,
                exit_code: row.get(2)?,
                timestamp: row.get(3)?,
                duration_ms: row.get(4)?,
            })
        });

        match rows {
            Ok(rows) => {
                let mut results = Vec::new();
                for row in rows {
                    results.push(row?);
                }
                Ok(results)
            }
            Err(e) => {
                if e.to_string().contains("No files found") {
                    Ok(Vec::new())
                } else {
                    Err(e.into())
                }
            }
        }
    }

    /// Query invocations with default limit of 20 (for listing).
    pub fn query_invocations(&self, query: &Query) -> Result<Vec<InvocationSummary>> {
        self.query_invocations_with_limit(query, 20)
    }

    /// Count total invocations in the store.
    pub fn invocation_count(&self) -> Result<i64> {
        let conn = self.connection()?;

        let result: std::result::Result<i64, _> =
            conn.query_row("SELECT COUNT(*) FROM invocations", [], |row| row.get(0));

        match result {
            Ok(count) => Ok(count),
            Err(e) => {
                if e.to_string().contains("No files found") {
                    Ok(0)
                } else {
                    Err(e.into())
                }
            }
        }
    }

    /// Find an invocation by its tag.
    /// Returns the full invocation ID if found.
    pub fn find_by_tag(&self, tag: &str) -> Result<Option<String>> {
        let conn = self.connection()?;

        // Normalize tag (remove leading : if present)
        let tag = tag.trim_start_matches(':');

        let result: std::result::Result<String, _> = conn.query_row(
            "SELECT id::VARCHAR FROM invocations WHERE tag = ?",
            params![tag],
            |row| row.get(0),
        );

        match result {
            Ok(id) => Ok(Some(id)),
            Err(duckdb::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Set or update the tag on an invocation.
    pub fn set_tag(&self, invocation_id: &str, tag: Option<&str>) -> Result<()> {
        let conn = self.connection()?;

        conn.execute(
            "UPDATE local.invocations SET tag = ? WHERE id = ?",
            params![tag, invocation_id],
        )?;

        Ok(())
    }

    /// Start a pending invocation.
    ///
    /// This:
    /// 1. Creates a JSON pending file for crash recovery
    /// 2. Writes the invocation to the status=pending partition
    ///
    /// Returns the pending invocation for later completion.
    pub fn start_pending_invocation(
        &self,
        record: &InvocationRecord,
    ) -> Result<super::pending::PendingInvocation> {
        use super::pending::{write_pending_file, PendingInvocation};

        // Create pending invocation marker
        let pending = PendingInvocation::from_record(record)
            .ok_or_else(|| crate::error::Error::Storage("Missing runner_id".to_string()))?;

        // Write pending file first (crash-safe marker)
        let pending_dir = self.config.pending_dir();
        write_pending_file(&pending_dir, &pending)?;

        // Write to status=pending partition
        self.write_invocation(record)?;

        Ok(pending)
    }

    /// Complete a pending invocation.
    ///
    /// This:
    /// 1. Writes the completed record to status=completed partition
    /// 2. Deletes the pending parquet file from status=pending partition
    /// 3. Deletes the JSON pending file
    pub fn complete_pending_invocation(
        &self,
        record: &InvocationRecord,
        pending: &super::pending::PendingInvocation,
    ) -> Result<()> {
        use super::pending::delete_pending_file;

        // Write completed record
        self.write_invocation(record)?;

        // Delete pending parquet file
        let pending_date = pending.timestamp.date_naive();
        let pending_partition = self.config.invocations_dir_with_status("pending", &pending_date);
        let executable = record.executable.as_deref().unwrap_or("unknown");
        let pending_filename = format!(
            "{}--{}--{}.parquet",
            sanitize_filename(&pending.session_id),
            sanitize_filename(executable),
            pending.id
        );
        let pending_parquet = pending_partition.join(&pending_filename);
        if pending_parquet.exists() {
            let _ = fs::remove_file(&pending_parquet);
        }

        // Delete JSON pending file
        let pending_dir = self.config.pending_dir();
        delete_pending_file(&pending_dir, pending.id, &pending.session_id)?;

        Ok(())
    }

    /// Recover orphaned invocations from pending files.
    ///
    /// This scans pending files and marks invocations as orphaned if:
    /// - The runner is no longer alive
    /// - The pending file is older than max_age_hours
    pub fn recover_orphaned_invocations(
        &self,
        max_age_hours: u32,
        dry_run: bool,
    ) -> Result<super::pending::RecoveryStats> {
        use super::pending::{
            delete_pending_file, is_runner_alive, list_pending_files, RecoveryStats,
        };

        let pending_dir = self.config.pending_dir();
        let pending_files = list_pending_files(&pending_dir)?;
        let mut stats = RecoveryStats::default();

        let now = chrono::Utc::now();
        let max_age = chrono::Duration::hours(max_age_hours as i64);

        for pending in pending_files {
            stats.pending_checked += 1;

            // Check if too old (runner ID might have been recycled)
            let age = now.signed_duration_since(pending.timestamp);
            let is_stale = age > max_age;

            // Check if runner is still alive
            let runner_alive = !is_stale && is_runner_alive(&pending.runner_id);

            if runner_alive {
                stats.still_running += 1;
                continue;
            }

            if dry_run {
                stats.orphaned += 1;
                continue;
            }

            // Create orphaned record
            let orphaned_record = InvocationRecord {
                id: pending.id,
                session_id: pending.session_id.clone(),
                timestamp: pending.timestamp,
                duration_ms: None, // Unknown
                cwd: pending.cwd.clone(),
                cmd: pending.cmd.clone(),
                executable: extract_executable(&pending.cmd),
                runner_id: Some(pending.runner_id.clone()),
                exit_code: None, // Unknown/crashed
                status: "orphaned".to_string(),
                format_hint: None,
                client_id: pending.client_id.clone(),
                hostname: None, // Not available from pending file
                username: None, // Not available from pending file
                tag: None,
            };

            // Write to status=orphaned partition
            match self.write_invocation(&orphaned_record) {
                Ok(()) => {
                    // Delete pending parquet file
                    let pending_date = pending.timestamp.date_naive();
                    let pending_partition =
                        self.config.invocations_dir_with_status("pending", &pending_date);
                    let executable = orphaned_record.executable.as_deref().unwrap_or("unknown");
                    let pending_filename = format!(
                        "{}--{}--{}.parquet",
                        sanitize_filename(&pending.session_id),
                        sanitize_filename(executable),
                        pending.id
                    );
                    let pending_parquet = pending_partition.join(&pending_filename);
                    let _ = fs::remove_file(&pending_parquet);

                    // Delete JSON pending file
                    let _ = delete_pending_file(&pending_dir, pending.id, &pending.session_id);

                    stats.orphaned += 1;
                }
                Err(_) => {
                    stats.errors += 1;
                }
            }
        }

        Ok(stats)
    }
}

/// Extract executable name from command string.
fn extract_executable(cmd: &str) -> Option<String> {
    cmd.split_whitespace()
        .next()
        .map(|s| s.rsplit('/').next().unwrap_or(s).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::initialize;
    use crate::Config;
    use tempfile::TempDir;

    fn setup_store() -> (TempDir, Store) {
        let tmp = TempDir::new().unwrap();
        let config = Config::with_root(tmp.path());
        initialize(&config).unwrap();
        let store = Store::open(config).unwrap();
        (tmp, store)
    }

    #[test]
    fn test_write_and_count_invocation() {
        let (_tmp, store) = setup_store();

        let record = InvocationRecord::new(
            "test-session",
            "make test",
            "/home/user/project",
            0,
            "test@client",
        );

        store.write_invocation(&record).unwrap();

        let count = store.invocation_count().unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_write_and_query_invocation() {
        let (_tmp, store) = setup_store();

        let record = InvocationRecord::new(
            "test-session",
            "cargo build",
            "/home/user/project",
            0,
            "test@client",
        )
        .with_duration(1500);

        store.write_invocation(&record).unwrap();

        // Query using SQL
        let result = store
            .query("SELECT cmd, exit_code, duration_ms FROM invocations")
            .unwrap();

        assert_eq!(result.columns, vec!["cmd", "exit_code", "duration_ms"]);
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0], "cargo build");
        assert_eq!(result.rows[0][1], "0");
        assert_eq!(result.rows[0][2], "1500");
    }

    #[test]
    fn test_recent_invocations_empty() {
        let (_tmp, store) = setup_store();

        let recent = store.recent_invocations(10).unwrap();
        assert!(recent.is_empty());
    }

    #[test]
    fn test_recent_invocations() {
        let (_tmp, store) = setup_store();

        // Write a few invocations
        for i in 0..3 {
            let record = InvocationRecord::new(
                "test-session",
                format!("command-{}", i),
                "/home/user",
                i,
                "test@client",
            );
            store.write_invocation(&record).unwrap();
        }

        let recent = store.recent_invocations(10).unwrap();
        assert_eq!(recent.len(), 3);
    }

    #[test]
    fn test_atomic_parquet_no_temp_files() {
        let (_tmp, store) = setup_store();

        let record = InvocationRecord::new(
            "test-session",
            "test",
            "/home/user",
            0,
            "test@client",
        );
        store.write_invocation(&record).unwrap();

        // Check no .tmp files in invocations directory
        let date = record.date();
        let inv_dir = store.config().invocations_dir(&date);
        let temps: Vec<_> = std::fs::read_dir(&inv_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_str().unwrap_or("").starts_with(".tmp."))
            .collect();
        assert!(
            temps.is_empty(),
            "No temp files should remain in {:?}",
            inv_dir
        );
    }

    // DuckDB mode tests

    fn setup_store_duckdb() -> (TempDir, Store) {
        let tmp = TempDir::new().unwrap();
        let config = Config::with_duckdb_mode(tmp.path());
        initialize(&config).unwrap();
        let store = Store::open(config).unwrap();
        (tmp, store)
    }

    #[test]
    fn test_duckdb_mode_write_and_count_invocation() {
        let (_tmp, store) = setup_store_duckdb();

        let record = InvocationRecord::new(
            "test-session",
            "make test",
            "/home/user/project",
            0,
            "test@client",
        );

        store.write_invocation(&record).unwrap();

        let count = store.invocation_count().unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_duckdb_mode_write_and_query_invocation() {
        let (_tmp, store) = setup_store_duckdb();

        let record = InvocationRecord::new(
            "test-session",
            "cargo build",
            "/home/user/project",
            0,
            "test@client",
        )
        .with_duration(1500);

        store.write_invocation(&record).unwrap();

        // Query using SQL
        let result = store
            .query("SELECT cmd, exit_code, duration_ms FROM invocations")
            .unwrap();

        assert_eq!(result.columns, vec!["cmd", "exit_code", "duration_ms"]);
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0][0], "cargo build");
        assert_eq!(result.rows[0][1], "0");
        assert_eq!(result.rows[0][2], "1500");
    }

    #[test]
    fn test_duckdb_mode_recent_invocations() {
        let (_tmp, store) = setup_store_duckdb();

        // Write a few invocations
        for i in 0..3 {
            let record = InvocationRecord::new(
                "test-session",
                format!("command-{}", i),
                "/home/user",
                i,
                "test@client",
            );
            store.write_invocation(&record).unwrap();
        }

        let recent = store.recent_invocations(10).unwrap();
        assert_eq!(recent.len(), 3);
    }

    #[test]
    fn test_duckdb_mode_no_parquet_files() {
        let (tmp, store) = setup_store_duckdb();

        let record = InvocationRecord::new(
            "test-session",
            "test",
            "/home/user",
            0,
            "test@client",
        );
        store.write_invocation(&record).unwrap();

        // Check that no parquet files were created in recent/invocations
        let invocations_dir = tmp.path().join("db/data/recent/invocations");
        if invocations_dir.exists() {
            let parquet_files: Vec<_> = std::fs::read_dir(&invocations_dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_str().unwrap_or("").ends_with(".parquet"))
                .collect();
            assert!(
                parquet_files.is_empty(),
                "DuckDB mode should not create parquet files"
            );
        }
    }

    #[test]
    fn test_pending_invocation_lifecycle() {
        let (tmp, store) = setup_store();

        // Create a pending invocation (using current process PID)
        let record = InvocationRecord::new_pending_local(
            "test-session",
            "long-running-command",
            "/home/user",
            std::process::id() as i32,
            "test@client",
        );

        // Start the pending invocation
        let pending = store.start_pending_invocation(&record).unwrap();

        // Verify pending file was created
        let pending_dir = tmp.path().join("db/pending");
        let pending_path = pending.path(&pending_dir);
        assert!(pending_path.exists(), "Pending file should exist");

        // Verify invocation was written to status=pending partition
        let date = record.date();
        let pending_partition = tmp
            .path()
            .join("db/data/recent/invocations")
            .join("status=pending")
            .join(format!("date={}", date));
        assert!(pending_partition.exists(), "Pending partition should exist");

        // Complete the invocation
        let completed_record = record.complete(0, Some(100));
        store
            .complete_pending_invocation(&completed_record, &pending)
            .unwrap();

        // Verify pending file was deleted
        assert!(!pending_path.exists(), "Pending file should be deleted");

        // Verify completed record was written
        let completed_partition = tmp
            .path()
            .join("db/data/recent/invocations")
            .join("status=completed")
            .join(format!("date={}", date));
        assert!(
            completed_partition.exists(),
            "Completed partition should exist"
        );
    }

    #[test]
    fn test_recover_orphaned_invocations() {
        let (tmp, store) = setup_store();

        // Create a pending invocation with a dead PID
        let record = InvocationRecord::new_pending_local(
            "test-session",
            "crashed-command",
            "/home/user",
            999999999, // PID that doesn't exist
            "test@client",
        );

        // Write pending file manually (simulating a crash scenario)
        let pending =
            crate::store::pending::PendingInvocation::from_record(&record).unwrap();
        let pending_dir = tmp.path().join("db/pending");
        crate::store::pending::write_pending_file(&pending_dir, &pending).unwrap();

        // Write to status=pending partition
        store.write_invocation(&record).unwrap();

        // Verify pending file exists
        let pending_path = pending.path(&pending_dir);
        assert!(pending_path.exists(), "Pending file should exist before recovery");

        // Run recovery
        let stats = store.recover_orphaned_invocations(24, false).unwrap();

        assert_eq!(stats.pending_checked, 1);
        assert_eq!(stats.orphaned, 1);
        assert_eq!(stats.still_running, 0);

        // Verify pending file was deleted
        assert!(!pending_path.exists(), "Pending file should be deleted after recovery");

        // Verify orphaned record was written
        let date = record.date();
        let orphaned_partition = tmp
            .path()
            .join("db/data/recent/invocations")
            .join("status=orphaned")
            .join(format!("date={}", date));
        assert!(
            orphaned_partition.exists(),
            "Orphaned partition should exist"
        );
    }

    #[test]
    fn test_recover_skips_running_processes() {
        let (tmp, store) = setup_store();

        // Create a pending invocation with the current process PID (still alive)
        let record = InvocationRecord::new_pending_local(
            "test-session",
            "running-command",
            "/home/user",
            std::process::id() as i32,
            "test@client",
        );

        // Write pending file
        let pending =
            crate::store::pending::PendingInvocation::from_record(&record).unwrap();
        let pending_dir = tmp.path().join("db/pending");
        crate::store::pending::write_pending_file(&pending_dir, &pending).unwrap();

        // Run recovery
        let stats = store.recover_orphaned_invocations(24, false).unwrap();

        assert_eq!(stats.pending_checked, 1);
        assert_eq!(stats.still_running, 1);
        assert_eq!(stats.orphaned, 0);

        // Verify pending file was NOT deleted
        let pending_path = pending.path(&pending_dir);
        assert!(pending_path.exists(), "Pending file should still exist for running process");
    }
}
