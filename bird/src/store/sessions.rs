//! Session storage operations.

use std::fs;

use duckdb::params;

use super::atomic;
use super::Store;
use crate::config::StorageMode;
use crate::schema::SessionRecord;
use crate::Result;

impl Store {
    /// Write a session record to the store.
    ///
    /// Behavior depends on storage mode:
    /// - Parquet: Creates a new Parquet file in the appropriate date partition
    /// - DuckDB: Inserts directly into the local.sessions
    ///
    /// Sessions are written lazily on first invocation from that session.
    pub fn write_session(&self, record: &SessionRecord) -> Result<()> {
        match self.config.storage_mode {
            StorageMode::Parquet => self.write_session_parquet(record),
            StorageMode::DuckDB => self.write_session_duckdb(record),
        }
    }

    /// Write session to a Parquet file (multi-writer safe).
    fn write_session_parquet(&self, record: &SessionRecord) -> Result<()> {
        let conn = self.connection()?;

        // Ensure the partition directory exists
        let partition_dir = self.config.sessions_dir(&record.date);
        fs::create_dir_all(&partition_dir)?;

        // Generate filename: {session_id}.parquet
        let filename = format!("{}.parquet", record.session_id);
        let file_path = partition_dir.join(&filename);

        // Write via DuckDB using COPY
        conn.execute_batch(
            r#"
            CREATE OR REPLACE TEMP TABLE temp_session (
                session_id VARCHAR,
                client_id VARCHAR,
                invoker VARCHAR,
                invoker_pid INTEGER,
                invoker_type VARCHAR,
                registered_at TIMESTAMP,
                cwd VARCHAR,
                date DATE
            );
            "#,
        )?;

        conn.execute(
            r#"
            INSERT INTO temp_session VALUES (
                ?, ?, ?, ?, ?, ?, ?, ?
            )
            "#,
            params![
                record.session_id,
                record.client_id,
                record.invoker,
                record.invoker_pid,
                record.invoker_type,
                record.registered_at.to_rfc3339(),
                record.cwd,
                record.date.to_string(),
            ],
        )?;

        // Atomic write: COPY to temp file, then rename
        let temp_path = atomic::temp_path(&file_path);
        conn.execute(
            &format!(
                "COPY temp_session TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD)",
                temp_path.display()
            ),
            [],
        )?;
        conn.execute("DROP TABLE temp_session", [])?;

        // Rename temp to final (atomic on POSIX)
        atomic::rename_into_place(&temp_path, &file_path)?;

        Ok(())
    }

    /// Write session directly to DuckDB table.
    fn write_session_duckdb(&self, record: &SessionRecord) -> Result<()> {
        let conn = self.connection()?;

        conn.execute(
            r#"
            INSERT INTO local.sessions VALUES (
                ?, ?, ?, ?, ?, ?, ?, ?
            )
            "#,
            params![
                record.session_id,
                record.client_id,
                record.invoker,
                record.invoker_pid,
                record.invoker_type,
                record.registered_at.to_rfc3339(),
                record.cwd,
                record.date.to_string(),
            ],
        )?;

        Ok(())
    }

    /// Check if a session exists in the store.
    pub fn session_exists(&self, session_id: &str) -> Result<bool> {
        let conn = self.connection()?;

        let result: std::result::Result<i64, _> = conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM sessions WHERE session_id = '{}'",
                session_id
            ),
            [],
            |row| row.get(0),
        );

        match result {
            Ok(count) => Ok(count > 0),
            Err(e) => {
                if e.to_string().contains("No files found") {
                    Ok(false)
                } else {
                    Err(e.into())
                }
            }
        }
    }

    /// Ensure a session is registered, creating it if needed.
    ///
    /// This is called lazily when an invocation is recorded. If the session
    /// doesn't exist, it creates a new session record.
    pub fn ensure_session(&self, record: &SessionRecord) -> Result<()> {
        if !self.session_exists(&record.session_id)? {
            self.write_session(record)?;
        }
        Ok(())
    }

    /// Count total sessions in the store.
    pub fn session_count(&self) -> Result<i64> {
        let conn = self.connection()?;

        let result: std::result::Result<i64, _> =
            conn.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0));

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
    fn test_write_session() {
        let (_tmp, store) = setup_store();

        let record = SessionRecord::new(
            "zsh-12345",
            "user@laptop",
            "zsh",
            12345,
            "shell",
        );

        store.write_session(&record).unwrap();

        let count = store.session_count().unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_session_exists() {
        let (_tmp, store) = setup_store();

        // Should not exist initially
        assert!(!store.session_exists("zsh-12345").unwrap());

        // Write session
        let record = SessionRecord::new(
            "zsh-12345",
            "user@laptop",
            "zsh",
            12345,
            "shell",
        );
        store.write_session(&record).unwrap();

        // Should exist now
        assert!(store.session_exists("zsh-12345").unwrap());
    }

    #[test]
    fn test_ensure_session_creates_new() {
        let (_tmp, store) = setup_store();

        let record = SessionRecord::new(
            "bash-67890",
            "user@laptop",
            "bash",
            67890,
            "shell",
        );

        // Should create session
        store.ensure_session(&record).unwrap();

        assert!(store.session_exists("bash-67890").unwrap());
        assert_eq!(store.session_count().unwrap(), 1);
    }

    #[test]
    fn test_ensure_session_idempotent() {
        let (_tmp, store) = setup_store();

        let record = SessionRecord::new(
            "zsh-11111",
            "user@laptop",
            "zsh",
            11111,
            "shell",
        );

        // Call ensure_session twice
        store.ensure_session(&record).unwrap();
        store.ensure_session(&record).unwrap();

        // Should still only have one session
        assert_eq!(store.session_count().unwrap(), 1);
    }

    #[test]
    fn test_session_count_empty() {
        let (_tmp, store) = setup_store();

        let count = store.session_count().unwrap();
        assert_eq!(count, 0);
    }
}
