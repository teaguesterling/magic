//! Invocation storage operations.
//!
//! V5 schema: Invocations are now composed of attempts + outcomes.
//! - write_invocation() writes both attempt and outcome (for completed commands)
//! - For long-running commands, use start_invocation() and complete_invocation()

use duckdb::params;

use super::Store;
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
    /// Write an invocation record to the store (v5 schema).
    ///
    /// This writes both an attempt and an outcome record, since the invocation
    /// is already complete. For long-running commands, use start_invocation()
    /// followed by complete_invocation().
    pub fn write_invocation(&self, record: &InvocationRecord) -> Result<()> {
        // Convert InvocationRecord to v5 attempt + outcome
        let attempt = record.to_attempt();
        let outcome = record.to_outcome();

        // Write the attempt
        self.write_attempt(&attempt)?;

        // Write the outcome (if the invocation is completed)
        if let Some(outcome) = outcome {
            self.write_outcome(&outcome)?;
        }

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
    ///
    /// V5 schema: Updates the tag on the attempts table.
    pub fn set_tag(&self, invocation_id: &str, tag: Option<&str>) -> Result<()> {
        let conn = self.connection()?;

        conn.execute(
            "UPDATE local.attempts SET tag = ? WHERE id = ?",
            params![tag, invocation_id],
        )?;

        Ok(())
    }

    /// Start a pending invocation (v5 schema).
    ///
    /// V5: Writes an attempt record. No pending file needed - pending status
    /// is derived from attempts without matching outcomes.
    ///
    /// Returns the pending invocation for backward compatibility.
    #[deprecated(note = "V4 API. Use start_invocation() with AttemptRecord instead.")]
    pub fn start_pending_invocation(
        &self,
        record: &InvocationRecord,
    ) -> Result<super::pending::PendingInvocation> {
        use super::pending::PendingInvocation;

        // Create pending invocation marker for backward compatibility
        let pending = PendingInvocation::from_record(record)
            .ok_or_else(|| crate::error::Error::Storage("Missing runner_id".to_string()))?;

        // V5: Write attempt record (no outcome yet = pending status)
        let attempt = record.to_attempt();
        self.write_attempt(&attempt)?;

        Ok(pending)
    }

    /// Complete a pending invocation (v5 schema).
    ///
    /// V5: Writes an outcome record. The attempt was already written by
    /// start_pending_invocation().
    #[deprecated(note = "V4 API. Use complete_invocation() with OutcomeRecord instead.")]
    pub fn complete_pending_invocation(
        &self,
        record: &InvocationRecord,
        _pending: &super::pending::PendingInvocation,
    ) -> Result<()> {
        // V5: Write outcome record (the attempt already exists)
        if let Some(outcome) = record.to_outcome() {
            self.write_outcome(&outcome)?;
        }

        Ok(())
    }

    /// Recover orphaned invocations (v5 schema).
    ///
    /// V5: Scans attempts without outcomes and checks if the runner is still alive.
    /// If not alive, writes an orphaned outcome record.
    ///
    /// Note: This now looks at machine_id field which stores the runner_id for local invocations.
    pub fn recover_orphaned_invocations(
        &self,
        max_age_hours: u32,
        dry_run: bool,
    ) -> Result<super::pending::RecoveryStats> {
        use super::pending::{is_runner_alive, RecoveryStats};

        // V5: Get pending attempts (attempts without outcomes)
        let pending_attempts = self.get_pending_attempts()?;
        let mut stats = RecoveryStats::default();

        let now = chrono::Utc::now();
        let max_age = chrono::Duration::hours(max_age_hours as i64);

        for attempt in pending_attempts {
            stats.pending_checked += 1;

            // Check if too old (runner ID might have been recycled)
            let age = now.signed_duration_since(attempt.timestamp);
            let is_stale = age > max_age;

            // Check if runner is still alive (machine_id stores runner_id for local invocations)
            let runner_alive = if let Some(ref runner_id) = attempt.machine_id {
                !is_stale && is_runner_alive(runner_id)
            } else {
                // No runner_id means we can't check - mark as orphaned if stale
                !is_stale
            };

            if runner_alive {
                stats.still_running += 1;
                continue;
            }

            if dry_run {
                stats.orphaned += 1;
                continue;
            }

            // V5: Write orphaned outcome record
            match self.orphan_invocation(attempt.id, attempt.date) {
                Ok(()) => {
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

        // V5: Check no .tmp files in attempts directory
        let date = record.date();
        let attempts_dir = store.config().attempts_dir(&date);
        let temps: Vec<_> = std::fs::read_dir(&attempts_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_str().unwrap_or("").starts_with(".tmp."))
            .collect();
        assert!(
            temps.is_empty(),
            "No temp files should remain in {:?}",
            attempts_dir
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
        let (_tmp, store) = setup_store();

        // V5: Use AttemptRecord and OutcomeRecord for pending lifecycle
        use crate::schema::AttemptRecord;

        // Create an attempt (invocation start)
        let attempt = AttemptRecord::new(
            "test-session",
            "long-running-command",
            "/home/user",
            "test@client",
        );

        // Start the invocation (writes attempt, no outcome)
        store.start_invocation(&attempt).unwrap();

        // Verify attempt was written
        let count = store.attempt_count().unwrap();
        assert_eq!(count, 1, "Attempt should be written");

        // Verify invocation shows as pending (no outcome yet)
        let pending = store.get_pending_attempts().unwrap();
        assert_eq!(pending.len(), 1, "Should have one pending attempt");
        assert_eq!(pending[0].id, attempt.id);

        // Complete the invocation (write outcome)
        store.complete_invocation(attempt.id, 0, Some(100), attempt.date).unwrap();

        // Verify outcome was written
        let outcome_count = store.outcome_count().unwrap();
        assert_eq!(outcome_count, 1, "Outcome should be written");

        // Verify no pending attempts remain
        let pending_after = store.get_pending_attempts().unwrap();
        assert!(pending_after.is_empty(), "No pending attempts after completion");

        // Verify invocation shows as completed in the view
        let invocations = store.recent_invocations(10).unwrap();
        assert_eq!(invocations.len(), 1, "Should have one invocation");
        assert_eq!(invocations[0].exit_code, 0);
    }

    #[test]
    fn test_recover_orphaned_invocations() {
        let (_tmp, store) = setup_store();

        // V5: Create an attempt without an outcome (simulating a crash)
        // The machine_id field stores the runner_id (pid:NNNN format) for local invocations
        use crate::schema::AttemptRecord;

        let mut attempt = AttemptRecord::new(
            "test-session",
            "crashed-command",
            "/home/user",
            "test@client",
        );
        // Set machine_id to a dead PID (non-existent process) with pid: prefix
        attempt.machine_id = Some("pid:999999999".to_string());

        // Write attempt only (no outcome = pending status)
        store.write_attempt(&attempt).unwrap();

        // Verify attempt exists as pending
        let pending = store.get_pending_attempts().unwrap();
        assert_eq!(pending.len(), 1, "Should have one pending attempt");

        // Run recovery (should mark as orphaned since PID doesn't exist)
        let stats = store.recover_orphaned_invocations(24, false).unwrap();

        assert_eq!(stats.pending_checked, 1);
        assert_eq!(stats.orphaned, 1);
        assert_eq!(stats.still_running, 0);

        // Verify orphaned outcome was written
        let outcome_count = store.outcome_count().unwrap();
        assert_eq!(outcome_count, 1, "Orphaned outcome should be written");

        // Verify no pending attempts remain
        let pending_after = store.get_pending_attempts().unwrap();
        assert!(pending_after.is_empty(), "No pending attempts after recovery");
    }

    #[test]
    fn test_recover_skips_running_processes() {
        let (_tmp, store) = setup_store();

        // V5: Create an attempt with the current process PID (still alive)
        use crate::schema::AttemptRecord;

        let mut attempt = AttemptRecord::new(
            "test-session",
            "running-command",
            "/home/user",
            "test@client",
        );
        // Set machine_id to current process PID (still alive) with pid: prefix
        attempt.machine_id = Some(format!("pid:{}", std::process::id()));

        // Write attempt only (no outcome = pending status)
        store.write_attempt(&attempt).unwrap();

        // Verify attempt was written
        let attempt_count = store.attempt_count().unwrap();
        assert_eq!(attempt_count, 1, "Attempt should be written");

        // Verify we can get pending attempts
        let pending_before = store.get_pending_attempts().unwrap();
        assert_eq!(pending_before.len(), 1, "Should have one pending attempt before recovery");

        // Run recovery (should skip since process is still alive)
        let stats = store.recover_orphaned_invocations(24, false).unwrap();

        assert_eq!(stats.pending_checked, 1, "Should check one pending attempt");
        assert_eq!(stats.still_running, 1, "Should detect process is still running");
        assert_eq!(stats.orphaned, 0, "Should not orphan running process");

        // Verify attempt is still pending (no outcome written)
        let pending_after = store.get_pending_attempts().unwrap();
        assert_eq!(pending_after.len(), 1, "Attempt should still be pending");
    }
}
