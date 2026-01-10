//! Store - handles writing and reading records.
//!
//! Uses DuckDB to write Parquet files and query across them.

mod atomic;
mod compact;
mod events;
mod invocations;
mod outputs;
mod sessions;

use std::fs;

use chrono::{DateTime, NaiveDate, NaiveTime, TimeDelta, Utc};
use duckdb::{
    params,
    types::{TimeUnit, ValueRef},
    Connection,
};

use crate::config::StorageMode;
use crate::schema::{EventRecord, InvocationRecord, SessionRecord};
use crate::{Config, Error, Result};

// Re-export types from submodules
pub use compact::{ArchiveStats, AutoCompactOptions, CompactOptions, CompactStats};
pub use events::{EventFilters, EventSummary, FormatConfig, FormatRule};
pub use invocations::InvocationSummary;
pub use outputs::OutputInfo;

/// A batch of related records to write atomically.
///
/// Use this when you want to write an invocation along with its outputs,
/// session, and/or events in a single transaction.
#[derive(Debug, Default)]
pub struct InvocationBatch {
    /// The invocation record (required).
    pub invocation: Option<InvocationRecord>,

    /// Output streams with their content: (stream_name, content).
    /// Common streams: "stdout", "stderr", "combined".
    pub outputs: Vec<(String, Vec<u8>)>,

    /// Session record (optional, created if not already registered).
    pub session: Option<SessionRecord>,

    /// Pre-extracted events (optional).
    pub events: Option<Vec<EventRecord>>,
}

impl InvocationBatch {
    /// Create a new batch with an invocation.
    pub fn new(invocation: InvocationRecord) -> Self {
        Self {
            invocation: Some(invocation),
            outputs: Vec::new(),
            session: None,
            events: None,
        }
    }

    /// Add an output stream.
    pub fn with_output(mut self, stream: impl Into<String>, content: Vec<u8>) -> Self {
        self.outputs.push((stream.into(), content));
        self
    }

    /// Add a session record.
    pub fn with_session(mut self, session: SessionRecord) -> Self {
        self.session = Some(session);
        self
    }

    /// Add pre-extracted events.
    pub fn with_events(mut self, events: Vec<EventRecord>) -> Self {
        self.events = Some(events);
        self
    }
}

/// A BIRD store for reading and writing records.
pub struct Store {
    config: Config,
}

impl Store {
    /// Open an existing BIRD store.
    pub fn open(config: Config) -> Result<Self> {
        if !config.db_path().exists() {
            return Err(Error::NotInitialized(config.bird_root.clone()));
        }
        Ok(Self { config })
    }

    /// Get a DuckDB connection to the store.
    pub fn connection(&self) -> Result<Connection> {
        let conn = Connection::open(&self.config.db_path())?;

        // Load bundled extensions
        conn.execute("LOAD parquet", [])?;
        conn.execute("LOAD icu", [])?;

        // Load community extensions (uses ~/.duckdb/extensions cache)
        conn.execute("SET allow_community_extensions = true", [])?;
        conn.execute("LOAD scalarfs", [])?;
        conn.execute("LOAD duck_hunt", [])?;

        // Set file search path so views resolve relative paths correctly
        conn.execute(
            &format!(
                "SET file_search_path = '{}'",
                self.config.data_dir().display()
            ),
            [],
        )?;

        Ok(conn)
    }

    /// Get config reference.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Query the store using SQL.
    ///
    /// Returns results as a Vec of rows, where each row is a Vec of string values.
    pub fn query(&self, sql: &str) -> Result<QueryResult> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(sql)?;

        // Execute the query first to get column info
        let mut rows_iter = stmt.query([])?;

        // Get column info from the rows iterator
        let column_count = rows_iter.as_ref().map(|r| r.column_count()).unwrap_or(0);
        let column_names: Vec<String> = if let Some(row_ref) = rows_iter.as_ref() {
            (0..column_count)
                .map(|i| {
                    row_ref
                        .column_name(i)
                        .map(|s| s.to_string())
                        .unwrap_or_else(|_| format!("col{}", i))
                })
                .collect()
        } else {
            Vec::new()
        };

        // Collect all rows
        let mut result_rows = Vec::new();
        while let Some(row) = rows_iter.next()? {
            let mut values = Vec::with_capacity(column_count);
            for i in 0..column_count {
                // Get value as generic ValueRef and convert to string
                let value = match row.get_ref(i)? {
                    ValueRef::Null => "NULL".to_string(),
                    ValueRef::Boolean(b) => b.to_string(),
                    ValueRef::TinyInt(n) => n.to_string(),
                    ValueRef::SmallInt(n) => n.to_string(),
                    ValueRef::Int(n) => n.to_string(),
                    ValueRef::BigInt(n) => n.to_string(),
                    ValueRef::HugeInt(n) => n.to_string(),
                    ValueRef::UTinyInt(n) => n.to_string(),
                    ValueRef::USmallInt(n) => n.to_string(),
                    ValueRef::UInt(n) => n.to_string(),
                    ValueRef::UBigInt(n) => n.to_string(),
                    ValueRef::Float(f) => f.to_string(),
                    ValueRef::Double(f) => f.to_string(),
                    ValueRef::Decimal(d) => d.to_string(),
                    ValueRef::Timestamp(unit, val) => {
                        // Convert to microseconds then to DateTime
                        let micros = match unit {
                            TimeUnit::Second => val * 1_000_000,
                            TimeUnit::Millisecond => val * 1_000,
                            TimeUnit::Microsecond => val,
                            TimeUnit::Nanosecond => val / 1_000,
                        };
                        DateTime::<Utc>::from_timestamp_micros(micros)
                            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
                            .unwrap_or_else(|| format!("<invalid timestamp {}>", val))
                    }
                    ValueRef::Date32(days) => {
                        // Days since 1970-01-01
                        NaiveDate::from_ymd_opt(1970, 1, 1)
                            .and_then(|epoch| epoch.checked_add_signed(TimeDelta::days(days as i64)))
                            .map(|d| d.format("%Y-%m-%d").to_string())
                            .unwrap_or_else(|| format!("<invalid date {}>", days))
                    }
                    ValueRef::Time64(unit, val) => {
                        // Convert to microseconds then to NaiveTime
                        let micros = match unit {
                            TimeUnit::Second => val * 1_000_000,
                            TimeUnit::Millisecond => val * 1_000,
                            TimeUnit::Microsecond => val,
                            TimeUnit::Nanosecond => val / 1_000,
                        };
                        let secs = (micros / 1_000_000) as u32;
                        let micro_part = (micros % 1_000_000) as u32;
                        NaiveTime::from_num_seconds_from_midnight_opt(secs, micro_part * 1000)
                            .map(|t| t.format("%H:%M:%S").to_string())
                            .unwrap_or_else(|| format!("<invalid time {}>", val))
                    }
                    ValueRef::Interval { months, days, nanos } => {
                        format!("{} months {} days {} ns", months, days, nanos)
                    }
                    ValueRef::Text(s) => String::from_utf8_lossy(s).to_string(),
                    ValueRef::Blob(b) => format!("<blob {} bytes>", b.len()),
                    _ => "<complex>".to_string(),
                };
                values.push(value);
            }
            result_rows.push(values);
        }

        Ok(QueryResult {
            columns: column_names,
            rows: result_rows,
        })
    }

    /// Get the last invocation with its output (if any).
    pub fn last_invocation_with_output(
        &self,
    ) -> Result<Option<(InvocationSummary, Option<OutputInfo>)>> {
        if let Some(inv) = self.last_invocation()? {
            let output = self.get_output(&inv.id)?;
            Ok(Some((inv, output)))
        } else {
            Ok(None)
        }
    }

    /// Write a batch of related records atomically.
    ///
    /// This is the preferred way to write an invocation with its outputs,
    /// session, and events together. In DuckDB mode, all writes are wrapped
    /// in a transaction. In Parquet mode, files are written atomically.
    pub fn write_batch(&self, batch: &InvocationBatch) -> Result<()> {
        let invocation = batch
            .invocation
            .as_ref()
            .ok_or_else(|| Error::Storage("Batch must contain an invocation".to_string()))?;

        match self.config.storage_mode {
            StorageMode::Parquet => self.write_batch_parquet(batch, invocation),
            StorageMode::DuckDB => self.write_batch_duckdb(batch, invocation),
        }
    }

    /// Write batch using Parquet files (multi-writer safe).
    fn write_batch_parquet(
        &self,
        batch: &InvocationBatch,
        invocation: &InvocationRecord,
    ) -> Result<()> {
        // For Parquet mode, we write each record type separately.
        // Atomicity is per-file (temp + rename), but not across files.
        // This is acceptable because Parquet mode prioritizes concurrent writes.

        // Write session first (if provided and not already registered)
        if let Some(ref session) = batch.session {
            self.ensure_session(session)?;
        }

        // Write invocation
        self.write_invocation(invocation)?;

        let date = invocation.date();
        let inv_id = invocation.id;

        // Write outputs
        for (stream, content) in &batch.outputs {
            self.store_output(
                inv_id,
                stream,
                content,
                date,
                invocation.executable.as_deref(),
            )?;
        }

        // Write events (if provided)
        if let Some(ref events) = batch.events {
            if !events.is_empty() {
                self.write_events(events)?;
            }
        }

        Ok(())
    }

    /// Write batch using DuckDB tables with transaction.
    fn write_batch_duckdb(
        &self,
        batch: &InvocationBatch,
        invocation: &InvocationRecord,
    ) -> Result<()> {
        let conn = self.connection()?;

        // Begin transaction
        conn.execute("BEGIN TRANSACTION", [])?;

        let result = self.write_batch_duckdb_inner(&conn, batch, invocation);

        match result {
            Ok(()) => {
                conn.execute("COMMIT", [])?;
                Ok(())
            }
            Err(e) => {
                // Rollback on error
                let _ = conn.execute("ROLLBACK", []);
                Err(e)
            }
        }
    }

    /// Inner implementation for DuckDB batch write (within transaction).
    fn write_batch_duckdb_inner(
        &self,
        conn: &Connection,
        batch: &InvocationBatch,
        invocation: &InvocationRecord,
    ) -> Result<()> {
        use base64::Engine;

        let date = invocation.date();
        let inv_id = invocation.id;

        // Write session (if provided)
        if let Some(ref session) = batch.session {
            // Check if session exists
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sessions_table WHERE session_id = ?",
                    params![&session.session_id],
                    |row| row.get(0),
                )
                .unwrap_or(0);

            if exists == 0 {
                conn.execute(
                    r#"INSERT INTO sessions_table VALUES (?, ?, ?, ?, ?, ?, ?, ?)"#,
                    params![
                        session.session_id,
                        session.client_id,
                        session.invoker,
                        session.invoker_pid,
                        session.invoker_type,
                        session.registered_at.to_rfc3339(),
                        session.cwd,
                        session.date.to_string(),
                    ],
                )?;
            }
        }

        // Write invocation
        conn.execute(
            r#"INSERT INTO invocations_table VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
            params![
                invocation.id.to_string(),
                invocation.session_id,
                invocation.timestamp.to_rfc3339(),
                invocation.duration_ms,
                invocation.cwd,
                invocation.cmd,
                invocation.executable,
                invocation.exit_code,
                invocation.format_hint,
                invocation.client_id,
                invocation.hostname,
                invocation.username,
                date.to_string(),
            ],
        )?;

        // Write outputs
        for (stream, content) in &batch.outputs {
            // Compute hash
            let hash = blake3::hash(content);
            let hash_hex = hash.to_hex().to_string();

            // Route by size
            let (storage_type, storage_ref) = if content.len() < self.config.inline_threshold {
                // Inline: use data: URL
                let b64 = base64::engine::general_purpose::STANDARD.encode(content);
                let data_url = format!("data:application/octet-stream;base64,{}", b64);
                ("inline".to_string(), data_url)
            } else {
                // Blob: write file and register
                let cmd_hint = invocation.executable.as_deref().unwrap_or("output");
                let blob_path = self.config.blob_path(&hash_hex, cmd_hint);

                if let Some(parent) = blob_path.parent() {
                    fs::create_dir_all(parent)?;
                }

                let rel_path = blob_path
                    .strip_prefix(&self.config.data_dir())
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| blob_path.to_string_lossy().to_string());

                // Write blob atomically
                let wrote_new = atomic::write_file(&blob_path, content)?;

                if wrote_new {
                    conn.execute(
                        "INSERT INTO blob_registry (content_hash, byte_length, storage_path) VALUES (?, ?, ?)",
                        params![&hash_hex, content.len() as i64, &rel_path],
                    )?;
                } else {
                    conn.execute(
                        "UPDATE blob_registry SET ref_count = ref_count + 1, last_accessed = CURRENT_TIMESTAMP WHERE content_hash = ?",
                        params![&hash_hex],
                    )?;
                }

                ("blob".to_string(), format!("file://{}", rel_path))
            };

            // Write output record
            let output_id = uuid::Uuid::now_v7();
            conn.execute(
                r#"INSERT INTO outputs_table VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
                params![
                    output_id.to_string(),
                    inv_id.to_string(),
                    stream,
                    hash_hex,
                    content.len() as i64,
                    storage_type,
                    storage_ref,
                    Option::<String>::None, // content_type
                    date.to_string(),
                ],
            )?;
        }

        // Write events (if provided)
        if let Some(ref events) = batch.events {
            for event in events {
                conn.execute(
                    r#"INSERT INTO events_table VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
                    params![
                        event.id.to_string(),
                        event.invocation_id.to_string(),
                        event.client_id,
                        event.hostname,
                        event.event_type,
                        event.severity,
                        event.ref_file,
                        event.ref_line,
                        event.ref_column,
                        event.message,
                        event.error_code,
                        event.test_name,
                        event.status,
                        event.format_used,
                        event.date.to_string(),
                    ],
                )?;
            }
        }

        Ok(())
    }
}

/// Result of a SQL query.
#[derive(Debug)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// Sanitize a string for use in filenames.
fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            ' ' => '-',
            c if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' => c,
            _ => '_',
        })
        .take(64)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::initialize;
    use crate::schema::SessionRecord;
    use tempfile::TempDir;

    fn setup_store() -> (TempDir, Store) {
        let tmp = TempDir::new().unwrap();
        let config = Config::with_root(tmp.path());
        initialize(&config).unwrap();
        let store = Store::open(config).unwrap();
        (tmp, store)
    }

    fn setup_store_duckdb() -> (TempDir, Store) {
        let tmp = TempDir::new().unwrap();
        let config = Config::with_duckdb_mode(tmp.path());
        initialize(&config).unwrap();
        let store = Store::open(config).unwrap();
        (tmp, store)
    }

    #[test]
    fn test_store_open_uninitialized_fails() {
        let tmp = TempDir::new().unwrap();
        let config = Config::with_root(tmp.path());

        let result = Store::open(config);
        assert!(matches!(result, Err(Error::NotInitialized(_))));
    }

    #[test]
    fn test_sanitize_filename() {
        assert_eq!(sanitize_filename("make test"), "make-test");
        assert_eq!(sanitize_filename("/usr/bin/gcc"), "_usr_bin_gcc");
        assert_eq!(sanitize_filename("a:b*c?d"), "a_b_c_d");
    }

    // Batch write tests - Parquet mode

    #[test]
    fn test_batch_write_parquet_invocation_only() {
        let (_tmp, store) = setup_store();

        let inv = InvocationRecord::new("test-session", "echo hello", "/home/user", 0, "test@client");

        let batch = InvocationBatch::new(inv);
        store.write_batch(&batch).unwrap();

        assert_eq!(store.invocation_count().unwrap(), 1);
    }

    #[test]
    fn test_batch_write_parquet_with_output() {
        let (_tmp, store) = setup_store();

        let inv = InvocationRecord::new("test-session", "echo hello", "/home/user", 0, "test@client");
        let inv_id = inv.id;

        let batch = InvocationBatch::new(inv)
            .with_output("stdout", b"hello world\n".to_vec());

        store.write_batch(&batch).unwrap();

        let outputs = store.get_outputs(&inv_id.to_string(), None).unwrap();
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].stream, "stdout");
    }

    #[test]
    fn test_batch_write_parquet_with_session() {
        let (_tmp, store) = setup_store();

        let inv = InvocationRecord::new("test-session", "echo hello", "/home/user", 0, "test@client");
        let session = SessionRecord::new("test-session", "test@client", "bash", 12345, "shell");

        let batch = InvocationBatch::new(inv).with_session(session);
        store.write_batch(&batch).unwrap();

        assert!(store.session_exists("test-session").unwrap());
    }

    #[test]
    fn test_batch_write_parquet_full() {
        let (_tmp, store) = setup_store();

        let inv = InvocationRecord::new("test-session", "make test", "/home/user", 1, "test@client");
        let inv_id = inv.id;
        let session = SessionRecord::new("test-session", "test@client", "bash", 12345, "shell");

        let batch = InvocationBatch::new(inv)
            .with_session(session)
            .with_output("stdout", b"Building...\n".to_vec())
            .with_output("stderr", b"error: failed\n".to_vec());

        store.write_batch(&batch).unwrap();

        assert_eq!(store.invocation_count().unwrap(), 1);
        assert!(store.session_exists("test-session").unwrap());

        let outputs = store.get_outputs(&inv_id.to_string(), None).unwrap();
        assert_eq!(outputs.len(), 2);
    }

    // Batch write tests - DuckDB mode

    #[test]
    fn test_batch_write_duckdb_invocation_only() {
        let (_tmp, store) = setup_store_duckdb();

        let inv = InvocationRecord::new("test-session", "echo hello", "/home/user", 0, "test@client");

        let batch = InvocationBatch::new(inv);
        store.write_batch(&batch).unwrap();

        assert_eq!(store.invocation_count().unwrap(), 1);
    }

    #[test]
    fn test_batch_write_duckdb_with_output() {
        let (_tmp, store) = setup_store_duckdb();

        let inv = InvocationRecord::new("test-session", "echo hello", "/home/user", 0, "test@client");
        let inv_id = inv.id;

        let batch = InvocationBatch::new(inv)
            .with_output("stdout", b"hello world\n".to_vec());

        store.write_batch(&batch).unwrap();

        let outputs = store.get_outputs(&inv_id.to_string(), None).unwrap();
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].stream, "stdout");
    }

    #[test]
    fn test_batch_write_duckdb_with_session() {
        let (_tmp, store) = setup_store_duckdb();

        let inv = InvocationRecord::new("test-session", "echo hello", "/home/user", 0, "test@client");
        let session = SessionRecord::new("test-session", "test@client", "bash", 12345, "shell");

        let batch = InvocationBatch::new(inv).with_session(session);
        store.write_batch(&batch).unwrap();

        assert!(store.session_exists("test-session").unwrap());
    }

    #[test]
    fn test_batch_write_duckdb_full() {
        let (_tmp, store) = setup_store_duckdb();

        let inv = InvocationRecord::new("test-session", "make test", "/home/user", 1, "test@client");
        let inv_id = inv.id;
        let session = SessionRecord::new("test-session", "test@client", "bash", 12345, "shell");

        let batch = InvocationBatch::new(inv)
            .with_session(session)
            .with_output("stdout", b"Building...\n".to_vec())
            .with_output("stderr", b"error: failed\n".to_vec());

        store.write_batch(&batch).unwrap();

        assert_eq!(store.invocation_count().unwrap(), 1);
        assert!(store.session_exists("test-session").unwrap());

        let outputs = store.get_outputs(&inv_id.to_string(), None).unwrap();
        assert_eq!(outputs.len(), 2);
    }

    #[test]
    fn test_batch_requires_invocation() {
        let (_tmp, store) = setup_store();

        let batch = InvocationBatch::default();
        let result = store.write_batch(&batch);

        assert!(result.is_err());
    }
}
