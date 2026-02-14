//! Outcome storage operations (v5 schema).
//!
//! An outcome is created when an invocation completes. It links back to
//! the attempt that started the invocation.

use std::fs;

use duckdb::params;
use uuid::Uuid;

use super::atomic;
use super::Store;
use crate::config::StorageMode;
use crate::schema::OutcomeRecord;
use crate::Result;

impl Store {
    /// Write an outcome record to the store (v5 schema).
    ///
    /// Call this when an invocation completes (success, failure, or crash).
    /// The attempt should have been written at invocation start using `write_attempt()`.
    ///
    /// Behavior depends on storage mode:
    /// - Parquet: Creates a new Parquet file in the appropriate date partition
    /// - DuckDB: Inserts directly into the local.outcomes table
    pub fn write_outcome(&self, record: &OutcomeRecord) -> Result<()> {
        match self.config.storage_mode {
            StorageMode::Parquet => self.write_outcome_parquet(record),
            StorageMode::DuckDB => self.write_outcome_duckdb(record),
        }
    }

    /// Write outcome to a Parquet file (multi-writer safe).
    fn write_outcome_parquet(&self, record: &OutcomeRecord) -> Result<()> {
        let conn = self.connection_with_options(false)?;
        let date = record.date;

        // Ensure the partition directory exists
        let partition_dir = self.config.outcomes_dir(&date);
        fs::create_dir_all(&partition_dir)?;

        // Generate filename: {attempt_id}.parquet
        let filename = format!("{}.parquet", record.attempt_id);
        let file_path = partition_dir.join(&filename);

        // Convert metadata HashMap to DuckDB MAP format
        let metadata_map = if record.metadata.is_empty() {
            "map([],[]::JSON[])".to_string()
        } else {
            let entries: Vec<String> = record.metadata.iter()
                .map(|(k, v)| {
                    let key = k.replace('\'', "''");
                    let value = v.to_string().replace('\'', "''");
                    format!("struct_pack(k := '{}', v := '{}'::JSON)", key, value)
                })
                .collect();
            format!("map_from_entries([{}])", entries.join(", "))
        };

        // Write via DuckDB using COPY
        conn.execute_batch(
            r#"
            CREATE OR REPLACE TEMP TABLE temp_outcome (
                attempt_id UUID,
                completed_at TIMESTAMP,
                exit_code INTEGER,
                duration_ms BIGINT,
                signal INTEGER,
                timeout BOOLEAN,
                metadata MAP(VARCHAR, JSON),
                date DATE
            );
            "#,
        )?;

        // Insert with dynamic SQL for the MAP
        conn.execute(
            &format!(
                r#"
                INSERT INTO temp_outcome VALUES (
                    ?, ?, ?, ?, ?, ?, {}, ?
                )
                "#,
                metadata_map
            ),
            params![
                record.attempt_id.to_string(),
                record.completed_at.to_rfc3339(),
                record.exit_code,
                record.duration_ms,
                record.signal,
                record.timeout,
                date.to_string(),
            ],
        )?;

        // Atomic write: COPY to temp file, then rename
        let temp_path = atomic::temp_path(&file_path);
        conn.execute(
            &format!(
                "COPY temp_outcome TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD)",
                temp_path.display()
            ),
            [],
        )?;
        conn.execute("DROP TABLE temp_outcome", [])?;

        // Rename temp to final (atomic on POSIX)
        atomic::rename_into_place(&temp_path, &file_path)?;

        Ok(())
    }

    /// Write outcome directly to DuckDB table.
    fn write_outcome_duckdb(&self, record: &OutcomeRecord) -> Result<()> {
        let conn = self.connection()?;
        let date = record.date;

        // Convert metadata HashMap to DuckDB MAP format
        let metadata_map = if record.metadata.is_empty() {
            "map([],[]::JSON[])".to_string()
        } else {
            let entries: Vec<String> = record.metadata.iter()
                .map(|(k, v)| {
                    let key = k.replace('\'', "''");
                    let value = v.to_string().replace('\'', "''");
                    format!("struct_pack(k := '{}', v := '{}'::JSON)", key, value)
                })
                .collect();
            format!("map_from_entries([{}])", entries.join(", "))
        };

        conn.execute(
            &format!(
                r#"
                INSERT INTO local.outcomes VALUES (
                    ?, ?, ?, ?, ?, ?, {}, ?
                )
                "#,
                metadata_map
            ),
            params![
                record.attempt_id.to_string(),
                record.completed_at.to_rfc3339(),
                record.exit_code,
                record.duration_ms,
                record.signal,
                record.timeout,
                date.to_string(),
            ],
        )?;

        Ok(())
    }

    /// Complete an invocation by writing an outcome (v5 schema).
    ///
    /// This is the v5 equivalent of `complete_pending_invocation()`.
    pub fn complete_invocation(
        &self,
        attempt_id: Uuid,
        exit_code: i32,
        duration_ms: Option<i64>,
        date: chrono::NaiveDate,
    ) -> Result<()> {
        let outcome = OutcomeRecord::completed(attempt_id, exit_code, duration_ms, date);
        self.write_outcome(&outcome)
    }

    /// Mark an invocation as orphaned (crashed without cleanup).
    pub fn orphan_invocation(&self, attempt_id: Uuid, date: chrono::NaiveDate) -> Result<()> {
        let outcome = OutcomeRecord::orphaned(attempt_id, date);
        self.write_outcome(&outcome)
    }

    /// Mark an invocation as killed by signal.
    pub fn kill_invocation(
        &self,
        attempt_id: Uuid,
        signal: i32,
        duration_ms: Option<i64>,
        date: chrono::NaiveDate,
    ) -> Result<()> {
        let outcome = OutcomeRecord::killed(attempt_id, signal, duration_ms, date);
        self.write_outcome(&outcome)
    }

    /// Mark an invocation as timed out.
    pub fn timeout_invocation(
        &self,
        attempt_id: Uuid,
        duration_ms: i64,
        date: chrono::NaiveDate,
    ) -> Result<()> {
        let outcome = OutcomeRecord::timed_out(attempt_id, duration_ms, date);
        self.write_outcome(&outcome)
    }

    /// Get the count of outcomes in the store.
    pub fn outcome_count(&self) -> Result<i64> {
        let conn = self.connection()?;

        let result: std::result::Result<i64, _> =
            conn.query_row("SELECT COUNT(*) FROM outcomes", [], |row| row.get(0));

        match result {
            Ok(count) => Ok(count),
            Err(e) => {
                if e.to_string().contains("No files found") || e.to_string().contains("does not exist") {
                    Ok(0)
                } else {
                    Err(e.into())
                }
            }
        }
    }

    /// Recover orphaned invocations (v5 schema).
    ///
    /// Finds attempts without outcomes where the runner is no longer alive,
    /// and marks them as orphaned. Returns statistics about the operation.
    ///
    /// This is safe to run periodically (e.g., during compaction) and is
    /// idempotent - it won't create duplicate outcomes.
    pub fn recover_orphans(&self) -> Result<super::pending::RecoveryStats> {
        use super::pending::{is_runner_alive, RecoveryStats};
        use chrono::NaiveDate;

        let conn = self.connection()?;
        let mut stats = RecoveryStats::default();

        // Find pending attempts (attempts without matching outcomes)
        // machine_id stores the runner_id in v5 schema
        let mut stmt = conn.prepare(
            r#"
            SELECT a.id, a.timestamp::DATE as date, a.machine_id as runner_id
            FROM attempts a
            LEFT JOIN outcomes o ON a.id = o.attempt_id
            WHERE o.attempt_id IS NULL
            "#,
        )?;

        let pending: Vec<(String, String, Option<String>)> = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })?
            .filter_map(|r| r.ok())
            .collect();

        stats.pending_checked = pending.len();

        for (id_str, date_str, runner_id) in pending {
            // Check if runner is still alive
            let alive = runner_id
                .as_ref()
                .map(|r| is_runner_alive(r))
                .unwrap_or(false);

            if alive {
                stats.still_running += 1;
                continue;
            }

            // Runner is dead - mark as orphaned
            let attempt_id = match uuid::Uuid::parse_str(&id_str) {
                Ok(id) => id,
                Err(_) => {
                    stats.errors += 1;
                    continue;
                }
            };

            let date = match NaiveDate::parse_from_str(&date_str, "%Y-%m-%d") {
                Ok(d) => d,
                Err(_) => {
                    stats.errors += 1;
                    continue;
                }
            };

            match self.orphan_invocation(attempt_id, date) {
                Ok(()) => stats.orphaned += 1,
                Err(_) => stats.errors += 1,
            }
        }

        Ok(stats)
    }
}

#[cfg(test)]
mod tests {
    // Tests will be added after init.rs is updated for v5 schema
}
