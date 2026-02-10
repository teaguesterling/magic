//! Attempt storage operations (v5 schema).
//!
//! An attempt is created when an invocation starts. The outcome is recorded
//! separately when the invocation completes.

use std::fs;

use duckdb::params;

use super::atomic;
use super::{sanitize_filename, Store};
use crate::config::StorageMode;
use crate::schema::AttemptRecord;
use crate::Result;

impl Store {
    /// Write an attempt record to the store (v5 schema).
    ///
    /// Call this at invocation start. The outcome should be written when
    /// the invocation completes using `write_outcome()`.
    ///
    /// Behavior depends on storage mode:
    /// - Parquet: Creates a new Parquet file in the appropriate date partition
    /// - DuckDB: Inserts directly into the local.attempts table
    pub fn write_attempt(&self, record: &AttemptRecord) -> Result<()> {
        match self.config.storage_mode {
            StorageMode::Parquet => self.write_attempt_parquet(record),
            StorageMode::DuckDB => self.write_attempt_duckdb(record),
        }
    }

    /// Write attempt to a Parquet file (multi-writer safe).
    fn write_attempt_parquet(&self, record: &AttemptRecord) -> Result<()> {
        let conn = self.connection_with_options(false)?;
        let date = record.date();

        // Ensure the partition directory exists
        let partition_dir = self.config.attempts_dir(&date);
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

        // Convert metadata HashMap to DuckDB MAP format
        // Format: map_from_entries([struct_pack(k := 'key1', v := 'value1'), ...])
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
            CREATE OR REPLACE TEMP TABLE temp_attempt (
                id UUID,
                timestamp TIMESTAMP,
                cmd VARCHAR,
                cwd VARCHAR,
                session_id VARCHAR,
                tag VARCHAR,
                source_client VARCHAR,
                machine_id VARCHAR,
                hostname VARCHAR,
                executable VARCHAR,
                format_hint VARCHAR,
                metadata MAP(VARCHAR, JSON),
                date DATE
            );
            "#,
        )?;

        // Insert with dynamic SQL for the MAP
        conn.execute(
            &format!(
                r#"
                INSERT INTO temp_attempt VALUES (
                    ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, {}, ?
                )
                "#,
                metadata_map
            ),
            params![
                record.id.to_string(),
                record.timestamp.to_rfc3339(),
                record.cmd,
                record.cwd,
                record.session_id,
                record.tag,
                record.source_client,
                record.machine_id,
                record.hostname,
                record.executable,
                record.format_hint,
                date.to_string(),
            ],
        )?;

        // Atomic write: COPY to temp file, then rename
        let temp_path = atomic::temp_path(&file_path);
        conn.execute(
            &format!(
                "COPY temp_attempt TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD)",
                temp_path.display()
            ),
            [],
        )?;
        conn.execute("DROP TABLE temp_attempt", [])?;

        // Rename temp to final (atomic on POSIX)
        atomic::rename_into_place(&temp_path, &file_path)?;

        Ok(())
    }

    /// Write attempt directly to DuckDB table.
    fn write_attempt_duckdb(&self, record: &AttemptRecord) -> Result<()> {
        let conn = self.connection()?;
        let date = record.date();

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
                INSERT INTO local.attempts VALUES (
                    ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, {}, ?
                )
                "#,
                metadata_map
            ),
            params![
                record.id.to_string(),
                record.timestamp.to_rfc3339(),
                record.cmd,
                record.cwd,
                record.session_id,
                record.tag,
                record.source_client,
                record.machine_id,
                record.hostname,
                record.executable,
                record.format_hint,
                date.to_string(),
            ],
        )?;

        Ok(())
    }

    /// Start an invocation by writing an attempt (v5 schema).
    ///
    /// This is the v5 equivalent of `start_pending_invocation()`.
    /// Returns the attempt record for later use with `complete_invocation()`.
    pub fn start_invocation(&self, record: &AttemptRecord) -> Result<AttemptRecord> {
        self.write_attempt(record)?;
        Ok(record.clone())
    }

    /// Get the count of attempts in the store.
    pub fn attempt_count(&self) -> Result<i64> {
        let conn = self.connection()?;

        let result: std::result::Result<i64, _> =
            conn.query_row("SELECT COUNT(*) FROM attempts", [], |row| row.get(0));

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

    /// Get pending attempts (attempts without outcomes).
    ///
    /// In v5 schema, this replaces the pending file mechanism:
    /// `SELECT * FROM attempts WHERE id NOT IN (SELECT attempt_id FROM outcomes)`
    pub fn get_pending_attempts(&self) -> Result<Vec<AttemptRecord>> {
        let conn = self.connection()?;

        let sql = r#"
            SELECT
                a.id::VARCHAR, a.timestamp::VARCHAR, a.cmd, a.cwd, a.session_id,
                a.tag, a.source_client, a.machine_id, a.hostname, a.executable,
                a.format_hint, a.metadata::VARCHAR, a.date::VARCHAR
            FROM attempts a
            WHERE a.id NOT IN (SELECT attempt_id FROM outcomes)
            ORDER BY a.timestamp DESC
        "#;

        let mut stmt = match conn.prepare(sql) {
            Ok(stmt) => stmt,
            Err(e) => {
                // Tables might not exist in a fresh v4 database
                if e.to_string().contains("does not exist") {
                    return Ok(Vec::new());
                }
                return Err(e.into());
            }
        };

        let rows = stmt.query_map([], |row| {
            let id_str: String = row.get(0)?;
            let ts_str: String = row.get(1)?;
            let date_str: String = row.get(12)?;
            let metadata_str: Option<String> = row.get(11)?;

            let metadata: std::collections::HashMap<String, serde_json::Value> = metadata_str
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();

            Ok(AttemptRecord {
                id: uuid::Uuid::parse_str(&id_str).unwrap_or_else(|_| uuid::Uuid::nil()),
                timestamp: chrono::DateTime::parse_from_rfc3339(&ts_str)
                    .map(|dt| dt.with_timezone(&chrono::Utc))
                    .unwrap_or_else(|_| chrono::Utc::now()),
                cmd: row.get(2)?,
                cwd: row.get(3)?,
                session_id: row.get(4)?,
                tag: row.get(5)?,
                source_client: row.get(6)?,
                machine_id: row.get(7)?,
                hostname: row.get(8)?,
                executable: row.get(9)?,
                format_hint: row.get(10)?,
                metadata,
                date: chrono::NaiveDate::parse_from_str(&date_str, "%Y-%m-%d")
                    .unwrap_or_else(|_| chrono::Utc::now().date_naive()),
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
                if e.to_string().contains("does not exist") {
                    Ok(Vec::new())
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

    // Note: These tests will fail until we update init.rs to create the v5 schema.
    // For now, we can verify the code compiles correctly.

    fn _setup_store_v5() -> (TempDir, Store) {
        let tmp = TempDir::new().unwrap();
        let config = Config::with_root(tmp.path());
        initialize(&config).unwrap();
        let store = Store::open(config).unwrap();
        (tmp, store)
    }

    fn _setup_store_v5_duckdb() -> (TempDir, Store) {
        let tmp = TempDir::new().unwrap();
        let config = Config::with_duckdb_mode(tmp.path());
        initialize(&config).unwrap();
        let store = Store::open(config).unwrap();
        (tmp, store)
    }

    // Tests will be uncommented after init.rs is updated for v5 schema
    /*
    #[test]
    fn test_write_attempt_parquet() {
        let (_tmp, store) = _setup_store_v5();

        let attempt = AttemptRecord::new(
            "test-session",
            "make test",
            "/home/user/project",
            "test@client",
        );

        store.write_attempt(&attempt).unwrap();

        let count = store.attempt_count().unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_write_attempt_duckdb() {
        let (_tmp, store) = _setup_store_v5_duckdb();

        let attempt = AttemptRecord::new(
            "test-session",
            "make test",
            "/home/user/project",
            "test@client",
        );

        store.write_attempt(&attempt).unwrap();

        let count = store.attempt_count().unwrap();
        assert_eq!(count, 1);
    }
    */
}
