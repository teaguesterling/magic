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
        let conn = self.connection()?;
        let date = record.date();

        // Ensure the partition directory exists
        let partition_dir = self.config.invocations_dir(&date);
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
                exit_code INTEGER,
                format_hint VARCHAR,
                client_id VARCHAR,
                hostname VARCHAR,
                username VARCHAR,
                date DATE
            );
            "#,
        )?;

        conn.execute(
            r#"
            INSERT INTO temp_invocation VALUES (
                ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?
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
                record.exit_code,
                record.format_hint,
                record.client_id,
                record.hostname,
                record.username,
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
                ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?
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
                record.exit_code,
                record.format_hint,
                record.client_id,
                record.hostname,
                record.username,
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
}
