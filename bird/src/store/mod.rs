//! Store - handles writing and reading records.
//!
//! Uses DuckDB to write Parquet files and query across them.

mod atomic;
mod compact;
mod events;
mod invocations;
mod outputs;
mod remote;
mod sessions;

use std::fs;
use std::thread;
use std::time::Duration;

use chrono::{DateTime, NaiveDate, NaiveTime, TimeDelta, Utc};
use duckdb::{
    params,
    types::{TimeUnit, Value, ValueRef},
    Connection,
};

use crate::config::StorageMode;
use crate::schema::{EventRecord, InvocationRecord, SessionRecord};
use crate::{Config, Error, Result};

/// Format a DuckDB Value to a human-readable string.
/// Handles complex types like List, Array, Map, and Struct recursively.
fn format_value(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_string(),
        Value::Boolean(b) => b.to_string(),
        Value::TinyInt(n) => n.to_string(),
        Value::SmallInt(n) => n.to_string(),
        Value::Int(n) => n.to_string(),
        Value::BigInt(n) => n.to_string(),
        Value::HugeInt(n) => n.to_string(),
        Value::UTinyInt(n) => n.to_string(),
        Value::USmallInt(n) => n.to_string(),
        Value::UInt(n) => n.to_string(),
        Value::UBigInt(n) => n.to_string(),
        Value::Float(f) => f.to_string(),
        Value::Double(f) => f.to_string(),
        Value::Decimal(d) => d.to_string(),
        Value::Timestamp(_, micros) => {
            DateTime::<Utc>::from_timestamp_micros(*micros)
                .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
                .unwrap_or_else(|| format!("<timestamp {}>", micros))
        }
        Value::Text(s) => s.clone(),
        Value::Blob(b) => format!("<blob {} bytes>", b.len()),
        Value::Date32(days) => {
            NaiveDate::from_ymd_opt(1970, 1, 1)
                .and_then(|epoch| epoch.checked_add_signed(TimeDelta::days(*days as i64)))
                .map(|d| d.format("%Y-%m-%d").to_string())
                .unwrap_or_else(|| format!("<date {}>", days))
        }
        Value::Time64(_, micros) => {
            let secs = (*micros / 1_000_000) as u32;
            let micro_part = (*micros % 1_000_000) as u32;
            NaiveTime::from_num_seconds_from_midnight_opt(secs, micro_part * 1000)
                .map(|t| t.format("%H:%M:%S").to_string())
                .unwrap_or_else(|| format!("<time {}>", micros))
        }
        Value::Interval { months, days, nanos } => {
            format!("{} months {} days {} ns", months, days, nanos)
        }
        // Complex types
        Value::List(items) => {
            let formatted: Vec<String> = items.iter().map(format_value).collect();
            format!("[{}]", formatted.join(", "))
        }
        Value::Array(items) => {
            let formatted: Vec<String> = items.iter().map(format_value).collect();
            format!("[{}]", formatted.join(", "))
        }
        Value::Map(map) => {
            let formatted: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{}: {}", format_value(k), format_value(v)))
                .collect();
            format!("{{{}}}", formatted.join(", "))
        }
        Value::Struct(fields) => {
            let formatted: Vec<String> = fields
                .iter()
                .map(|(k, v)| format!("{}: {}", k, format_value(v)))
                .collect();
            format!("{{{}}}", formatted.join(", "))
        }
        Value::Enum(s) => s.clone(),
        _ => "<unknown>".to_string(),
    }
}

// Re-export types from submodules
pub use compact::{ArchiveStats, AutoCompactOptions, CompactOptions, CompactStats};
pub use events::{EventFilters, EventSummary, FormatConfig, FormatRule};
pub use invocations::InvocationSummary;
pub use outputs::OutputInfo;
pub use remote::{parse_since, PullOptions, PullStats, PushOptions, PushStats};

// Re-export format detection types (defined below)
// BuiltinFormat, FormatMatch, FormatSource are defined at the bottom of this file

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

/// Options for creating a database connection.
///
/// Controls what gets loaded and attached when opening a connection.
/// Use this for explicit control over connection behavior.
///
/// # Connection Stages
///
/// 1. **Extensions** (always): parquet, icu, scalarfs, duck_hunt
/// 2. **Blob resolution** (always): S3 credentials, blob_roots macro
/// 3. **Migration** (optional): Upgrade existing installations to new schema
/// 4. **Remotes** (optional): Attach remote databases, rebuild remotes.* views
/// 5. **Project** (optional): Attach project-local database if in a project
/// 6. **CWD views** (optional): Rebuild cwd.* views for current directory
#[derive(Debug, Clone, Default)]
pub struct ConnectionOptions {
    /// Attach configured remotes (default: true).
    /// When true, remote databases are attached and remotes.* views are rebuilt
    /// to include the attached data.
    pub attach_remotes: bool,

    /// Attach project database if in a project directory (default: true).
    pub attach_project: bool,

    /// Rebuild cwd.* views for current working directory (default: true).
    /// These views filter main.* data to entries matching the current directory.
    pub create_ephemeral_views: bool,

    /// Run migration for existing installations (default: false).
    /// Only enable this for explicit upgrade operations.
    pub run_migration: bool,
}

impl ConnectionOptions {
    /// Create options for a full connection (default behavior).
    pub fn full() -> Self {
        Self {
            attach_remotes: true,
            attach_project: true,
            create_ephemeral_views: true,
            run_migration: false,
        }
    }

    /// Create options for a minimal connection (no attachments).
    /// Useful for write operations that don't need remote data.
    pub fn minimal() -> Self {
        Self {
            attach_remotes: false,
            attach_project: false,
            create_ephemeral_views: false,
            run_migration: false,
        }
    }

    /// Create options for a migration/upgrade connection.
    pub fn for_migration() -> Self {
        Self {
            attach_remotes: false,
            attach_project: false,
            create_ephemeral_views: false,
            run_migration: true,
        }
    }
}

/// Ensure a DuckDB extension is loaded, installing if necessary.
///
/// Attempts in order:
/// 1. LOAD (extension might already be available)
/// 2. INSTALL from default repository, then LOAD
/// 3. INSTALL FROM community, then LOAD
///
/// Returns Ok(true) if loaded successfully, Ok(false) if extension unavailable.
fn ensure_extension(conn: &Connection, name: &str) -> Result<bool> {
    // Try loading directly first (already installed/cached)
    if conn.execute(&format!("LOAD {}", name), []).is_ok() {
        return Ok(true);
    }

    // Try installing from default repository
    if conn.execute(&format!("INSTALL {}", name), []).is_ok()
        && conn.execute(&format!("LOAD {}", name), []).is_ok()
    {
        return Ok(true);
    }

    // Try installing from community repository
    if conn.execute(&format!("INSTALL {} FROM community", name), []).is_ok()
        && conn.execute(&format!("LOAD {}", name), []).is_ok()
    {
        return Ok(true);
    }

    Ok(false)
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

    /// Open a DuckDB connection with retry and exponential backoff.
    ///
    /// DuckDB uses file locking for concurrent access. When multiple processes
    /// (e.g., background shell hook saves) try to access the database simultaneously,
    /// this method retries with exponential backoff to avoid lock conflicts.
    fn open_connection_with_retry(&self) -> Result<Connection> {
        const MAX_RETRIES: u32 = 10;
        const INITIAL_DELAY_MS: u64 = 10;
        const MAX_DELAY_MS: u64 = 1000;

        let db_path = self.config.db_path();
        let mut delay_ms = INITIAL_DELAY_MS;
        let mut last_error = None;

        for attempt in 0..MAX_RETRIES {
            match Connection::open(&db_path) {
                Ok(conn) => return Ok(conn),
                Err(e) => {
                    let err_msg = e.to_string();
                    // Check if this is a lock conflict error
                    if err_msg.contains("Could not set lock")
                        || err_msg.contains("Conflicting lock")
                        || err_msg.contains("database is locked")
                    {
                        last_error = Some(e);
                        if attempt < MAX_RETRIES - 1 {
                            // Add jitter to avoid thundering herd
                            let jitter = (attempt as u64 * 7) % 10;
                            thread::sleep(Duration::from_millis(delay_ms + jitter));
                            delay_ms = (delay_ms * 2).min(MAX_DELAY_MS);
                            continue;
                        }
                    } else {
                        // Non-lock error, fail immediately
                        return Err(e.into());
                    }
                }
            }
        }

        // All retries exhausted
        Err(last_error
            .map(|e| e.into())
            .unwrap_or_else(|| Error::Storage("Failed to open database after retries".to_string())))
    }

    /// Get a DuckDB connection with full features (attachments, ephemeral views).
    pub fn connection(&self) -> Result<Connection> {
        self.connect(ConnectionOptions::full())
    }

    /// Get a DuckDB connection with optional remote attachment (legacy API).
    pub fn connection_with_options(&self, attach_remotes: bool) -> Result<Connection> {
        let opts = if attach_remotes {
            ConnectionOptions::full()
        } else {
            ConnectionOptions::minimal()
        };
        self.connect(opts)
    }

    /// Get a DuckDB connection with explicit options.
    ///
    /// This is the main connection method. Use `ConnectionOptions` to control:
    /// - Whether remotes are attached
    /// - Whether project database is attached
    /// - Whether ephemeral views are created
    /// - Whether migration should run
    ///
    /// Uses retry with exponential backoff to handle concurrent access.
    pub fn connect(&self, opts: ConnectionOptions) -> Result<Connection> {
        let conn = self.open_connection_with_retry()?;

        // ===== Load required extensions =====
        // Uses default extension directory (typically ~/.duckdb/extensions)
        // Falls back to community repository if not in default
        conn.execute("SET allow_community_extensions = true", [])?;

        for ext in ["parquet", "icu"] {
            if !ensure_extension(&conn, ext)? {
                return Err(Error::Extension(format!(
                    "Required extension '{}' could not be loaded",
                    ext
                )));
            }
        }

        // Optional community extensions - warn if missing
        for (ext, desc) in [
            ("scalarfs", "data: URL support for inline blobs"),
            ("duck_hunt", "log/output parsing for event extraction"),
        ] {
            if !ensure_extension(&conn, ext)? {
                eprintln!("Warning: {} extension not available ({})", ext, desc);
            }
        }

        // Set file search path so views resolve relative paths correctly
        conn.execute(
            &format!(
                "SET file_search_path = '{}'",
                self.config.data_dir().display()
            ),
            [],
        )?;

        // ===== Optional: Run migration for existing installations =====
        if opts.run_migration {
            self.migrate_to_new_schema(&conn)?;
        }

        // ===== Always set up blob resolution =====
        // S3 credentials needed before blob_roots is used
        self.setup_s3_credentials(&conn)?;
        self.setup_blob_resolution(&conn)?;

        // ===== Optional: Attach remotes and create access macros =====
        if opts.attach_remotes && !self.config.remotes.is_empty() {
            self.attach_remotes(&conn)?;
            self.create_remote_macros(&conn)?;
        }

        // ===== Optional: Attach project database =====
        if opts.attach_project {
            self.attach_project_db(&conn)?;
        }

        // ===== Optional: Create cwd macros =====
        // These TEMPORARY macros filter by current working directory
        if opts.create_ephemeral_views {
            self.create_cwd_macros(&conn)?;
        }

        Ok(conn)
    }

    /// Attach project-level `.bird/` database if we're in a project directory.
    ///
    /// The project database is attached as read-only under schema "project".
    /// This allows queries like `SELECT * FROM project.invocations`.
    fn attach_project_db(&self, conn: &Connection) -> Result<()> {
        use crate::project::find_current_project;

        let Some(project) = find_current_project() else {
            return Ok(()); // Not in a project
        };

        if !project.is_initialized() {
            return Ok(()); // Project not initialized
        }

        // Don't attach if project DB is the same as user DB
        if project.db_path == self.config.db_path() {
            return Ok(());
        }

        // Attach as read-only
        let attach_sql = format!(
            "ATTACH '{}' AS project (READ_ONLY)",
            project.db_path.display()
        );

        if let Err(e) = conn.execute(&attach_sql, []) {
            // Log but don't fail - project DB might be locked or inaccessible
            eprintln!("Note: Could not attach project database: {}", e);
        }

        Ok(())
    }

    /// Migrate existing installations to the new schema architecture.
    ///
    /// Checks if the `local` schema exists; if not, creates the new schema
    /// structure and migrates data from old `*_table` tables.
    fn migrate_to_new_schema(&self, conn: &Connection) -> Result<()> {
        // Check if already migrated (local schema exists)
        let local_exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM information_schema.schemata WHERE schema_name = 'local'",
                [],
                |row| row.get(0),
            )
            .unwrap_or(false);

        if local_exists {
            return Ok(());
        }

        // This is an old installation - need to migrate
        // For now, just create the new schemas. Data migration would require
        // moving data from old tables/views to new structure.
        // TODO: Implement full data migration if needed

        eprintln!("Note: Migrating to new schema architecture...");

        // Create core schemas
        conn.execute_batch(
            r#"
            CREATE SCHEMA IF NOT EXISTS local;
            CREATE SCHEMA IF NOT EXISTS cached_placeholder;
            CREATE SCHEMA IF NOT EXISTS remote_placeholder;
            CREATE SCHEMA IF NOT EXISTS caches;
            CREATE SCHEMA IF NOT EXISTS remotes;
            CREATE SCHEMA IF NOT EXISTS unified;
            CREATE SCHEMA IF NOT EXISTS cwd;
            "#,
        )?;

        // For DuckDB mode, create local tables
        if self.config.storage_mode == crate::StorageMode::DuckDB {
            conn.execute_batch(
                r#"
                CREATE TABLE IF NOT EXISTS local.sessions (
                    session_id VARCHAR, client_id VARCHAR, invoker VARCHAR, invoker_pid INTEGER,
                    invoker_type VARCHAR, registered_at TIMESTAMP, cwd VARCHAR, date DATE
                );
                CREATE TABLE IF NOT EXISTS local.invocations (
                    id UUID, session_id VARCHAR, timestamp TIMESTAMP, duration_ms BIGINT,
                    cwd VARCHAR, cmd VARCHAR, executable VARCHAR, exit_code INTEGER,
                    format_hint VARCHAR, client_id VARCHAR, hostname VARCHAR, username VARCHAR,
                    tag VARCHAR, date DATE
                );
                CREATE TABLE IF NOT EXISTS local.outputs (
                    id UUID, invocation_id UUID, stream VARCHAR, content_hash VARCHAR,
                    byte_length BIGINT, storage_type VARCHAR, storage_ref VARCHAR,
                    content_type VARCHAR, date DATE
                );
                CREATE TABLE IF NOT EXISTS local.events (
                    id UUID, invocation_id UUID, client_id VARCHAR, hostname VARCHAR,
                    event_type VARCHAR, severity VARCHAR, ref_file VARCHAR, ref_line INTEGER,
                    ref_column INTEGER, message VARCHAR, error_code VARCHAR, test_name VARCHAR,
                    status VARCHAR, format_used VARCHAR, date DATE
                );
                "#,
            )?;

            // Copy data from old tables if they exist
            let old_tables_exist: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM duckdb_tables() WHERE table_name = 'sessions_table'",
                    [],
                    |row| row.get(0),
                )
                .unwrap_or(false);

            if old_tables_exist {
                conn.execute_batch(
                    r#"
                    INSERT INTO local.sessions SELECT * FROM sessions_table;
                    INSERT INTO local.invocations SELECT * FROM invocations_table;
                    INSERT INTO local.outputs SELECT * FROM outputs_table;
                    INSERT INTO local.events SELECT * FROM events_table;
                    "#,
                )?;
            }
        } else {
            // Parquet mode - create views over parquet files
            conn.execute_batch(
                r#"
                CREATE OR REPLACE VIEW local.sessions AS
                SELECT * EXCLUDE (filename) FROM read_parquet(
                    'recent/sessions/**/*.parquet',
                    union_by_name = true, hive_partitioning = true, filename = true
                );
                CREATE OR REPLACE VIEW local.invocations AS
                SELECT * EXCLUDE (filename) FROM read_parquet(
                    'recent/invocations/**/*.parquet',
                    union_by_name = true, hive_partitioning = true, filename = true
                );
                CREATE OR REPLACE VIEW local.outputs AS
                SELECT * EXCLUDE (filename) FROM read_parquet(
                    'recent/outputs/**/*.parquet',
                    union_by_name = true, hive_partitioning = true, filename = true
                );
                CREATE OR REPLACE VIEW local.events AS
                SELECT * EXCLUDE (filename) FROM read_parquet(
                    'recent/events/**/*.parquet',
                    union_by_name = true, hive_partitioning = true, filename = true
                );
                "#,
            )?;
        }

        // Create placeholder schemas
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS cached_placeholder.sessions (
                session_id VARCHAR, client_id VARCHAR, invoker VARCHAR, invoker_pid INTEGER,
                invoker_type VARCHAR, registered_at TIMESTAMP, cwd VARCHAR, date DATE, _source VARCHAR
            );
            CREATE TABLE IF NOT EXISTS cached_placeholder.invocations (
                id UUID, session_id VARCHAR, timestamp TIMESTAMP, duration_ms BIGINT,
                cwd VARCHAR, cmd VARCHAR, executable VARCHAR, exit_code INTEGER,
                format_hint VARCHAR, client_id VARCHAR, hostname VARCHAR, username VARCHAR, date DATE, _source VARCHAR
            );
            CREATE TABLE IF NOT EXISTS cached_placeholder.outputs (
                id UUID, invocation_id UUID, stream VARCHAR, content_hash VARCHAR,
                byte_length BIGINT, storage_type VARCHAR, storage_ref VARCHAR,
                content_type VARCHAR, date DATE, _source VARCHAR
            );
            CREATE TABLE IF NOT EXISTS cached_placeholder.events (
                id UUID, invocation_id UUID, client_id VARCHAR, hostname VARCHAR,
                event_type VARCHAR, severity VARCHAR, ref_file VARCHAR, ref_line INTEGER,
                ref_column INTEGER, message VARCHAR, error_code VARCHAR, test_name VARCHAR,
                status VARCHAR, format_used VARCHAR, date DATE, _source VARCHAR
            );
            CREATE TABLE IF NOT EXISTS remote_placeholder.sessions (
                session_id VARCHAR, client_id VARCHAR, invoker VARCHAR, invoker_pid INTEGER,
                invoker_type VARCHAR, registered_at TIMESTAMP, cwd VARCHAR, date DATE, _source VARCHAR
            );
            CREATE TABLE IF NOT EXISTS remote_placeholder.invocations (
                id UUID, session_id VARCHAR, timestamp TIMESTAMP, duration_ms BIGINT,
                cwd VARCHAR, cmd VARCHAR, executable VARCHAR, exit_code INTEGER,
                format_hint VARCHAR, client_id VARCHAR, hostname VARCHAR, username VARCHAR, date DATE, _source VARCHAR
            );
            CREATE TABLE IF NOT EXISTS remote_placeholder.outputs (
                id UUID, invocation_id UUID, stream VARCHAR, content_hash VARCHAR,
                byte_length BIGINT, storage_type VARCHAR, storage_ref VARCHAR,
                content_type VARCHAR, date DATE, _source VARCHAR
            );
            CREATE TABLE IF NOT EXISTS remote_placeholder.events (
                id UUID, invocation_id UUID, client_id VARCHAR, hostname VARCHAR,
                event_type VARCHAR, severity VARCHAR, ref_file VARCHAR, ref_line INTEGER,
                ref_column INTEGER, message VARCHAR, error_code VARCHAR, test_name VARCHAR,
                status VARCHAR, format_used VARCHAR, date DATE, _source VARCHAR
            );
            "#,
        )?;

        // Create union schemas
        conn.execute_batch(
            r#"
            CREATE OR REPLACE VIEW caches.sessions AS SELECT * FROM cached_placeholder.sessions;
            CREATE OR REPLACE VIEW caches.invocations AS SELECT * FROM cached_placeholder.invocations;
            CREATE OR REPLACE VIEW caches.outputs AS SELECT * FROM cached_placeholder.outputs;
            CREATE OR REPLACE VIEW caches.events AS SELECT * FROM cached_placeholder.events;

            CREATE OR REPLACE VIEW remotes.sessions AS SELECT * FROM remote_placeholder.sessions;
            CREATE OR REPLACE VIEW remotes.invocations AS SELECT * FROM remote_placeholder.invocations;
            CREATE OR REPLACE VIEW remotes.outputs AS SELECT * FROM remote_placeholder.outputs;
            CREATE OR REPLACE VIEW remotes.events AS SELECT * FROM remote_placeholder.events;

            CREATE OR REPLACE VIEW main.sessions AS
                SELECT *, 'local' as _source FROM local.sessions
                UNION ALL BY NAME SELECT * FROM caches.sessions;
            CREATE OR REPLACE VIEW main.invocations AS
                SELECT *, 'local' as _source FROM local.invocations
                UNION ALL BY NAME SELECT * FROM caches.invocations;
            CREATE OR REPLACE VIEW main.outputs AS
                SELECT *, 'local' as _source FROM local.outputs
                UNION ALL BY NAME SELECT * FROM caches.outputs;
            CREATE OR REPLACE VIEW main.events AS
                SELECT *, 'local' as _source FROM local.events
                UNION ALL BY NAME SELECT * FROM caches.events;

            CREATE OR REPLACE VIEW unified.sessions AS
                SELECT * FROM main.sessions UNION ALL BY NAME SELECT * FROM remotes.sessions;
            CREATE OR REPLACE VIEW unified.invocations AS
                SELECT * FROM main.invocations UNION ALL BY NAME SELECT * FROM remotes.invocations;
            CREATE OR REPLACE VIEW unified.outputs AS
                SELECT * FROM main.outputs UNION ALL BY NAME SELECT * FROM remotes.outputs;
            CREATE OR REPLACE VIEW unified.events AS
                SELECT * FROM main.events UNION ALL BY NAME SELECT * FROM remotes.events;

            -- Qualified views: deduplicated with source list
            CREATE OR REPLACE VIEW unified.qualified_sessions AS
                SELECT * EXCLUDE (_source), list(DISTINCT _source) as _sources
                FROM unified.sessions GROUP BY ALL;
            CREATE OR REPLACE VIEW unified.qualified_invocations AS
                SELECT * EXCLUDE (_source), list(DISTINCT _source) as _sources
                FROM unified.invocations GROUP BY ALL;
            CREATE OR REPLACE VIEW unified.qualified_outputs AS
                SELECT * EXCLUDE (_source), list(DISTINCT _source) as _sources
                FROM unified.outputs GROUP BY ALL;
            CREATE OR REPLACE VIEW unified.qualified_events AS
                SELECT * EXCLUDE (_source), list(DISTINCT _source) as _sources
                FROM unified.events GROUP BY ALL;
            "#,
        )?;

        Ok(())
    }

    /// Set up S3 credentials for all remotes that use S3.
    /// This is called early so that blob resolution can access S3 paths.
    fn setup_s3_credentials(&self, conn: &Connection) -> Result<()> {
        // Check if any remote uses S3
        let has_s3 = self.config.remotes.iter().any(|r| {
            r.remote_type == crate::config::RemoteType::S3
        });

        if !has_s3 {
            return Ok(());
        }

        // Load httpfs for S3 support
        conn.execute("LOAD httpfs", [])?;

        // Set up credentials for each S3 remote
        for remote in &self.config.remotes {
            if remote.remote_type == crate::config::RemoteType::S3 {
                if let Some(provider) = &remote.credential_provider {
                    let secret_sql = format!(
                        "CREATE SECRET IF NOT EXISTS \"bird_{}\" (TYPE s3, PROVIDER {})",
                        remote.name, provider
                    );
                    if let Err(e) = conn.execute(&secret_sql, []) {
                        eprintln!("Warning: Failed to create S3 secret for {}: {}", remote.name, e);
                    }
                }
            }
        }

        Ok(())
    }

    /// Set up blob_roots variable and blob resolution macros.
    ///
    /// Storage refs use URI schemes to indicate type:
    /// - `data:`, `data+varchar:`, `data+blob:` - inline content (scalarfs)
    /// - `file:path` - relative path, resolved against blob_roots
    /// - Absolute paths (`s3://`, `/path/`) - used directly
    fn setup_blob_resolution(&self, conn: &Connection) -> Result<()> {
        let blob_roots = self.config.blob_roots();

        // Format as SQL array literal
        let roots_sql: String = blob_roots
            .iter()
            .map(|r| format!("'{}'", r.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(", ");

        // Set blob_roots variable
        conn.execute(&format!("SET VARIABLE blob_roots = [{}]", roots_sql), [])?;

        // Helper: check if ref is inline data (scalarfs data: protocol)
        conn.execute(
            r#"CREATE OR REPLACE MACRO is_inline_data(ref) AS (
                ref[:5] = 'data:' OR ref[:5] = 'data+'
            )"#,
            [],
        )?;

        // Helper: check if ref is a relative file: path
        conn.execute(
            r#"CREATE OR REPLACE MACRO is_file_ref(ref) AS (
                ref[:5] = 'file:'
            )"#,
            [],
        )?;

        // Resolve storage ref to list of paths for pathvariable:
        // - Inline data: pass through as single-element list
        // - file: refs: expand to glob patterns across all blob_roots
        // - Other (absolute paths): pass through
        conn.execute(
            r#"CREATE OR REPLACE MACRO resolve_storage_ref(ref) AS (
                CASE
                    WHEN is_inline_data(ref) THEN [ref]
                    WHEN is_file_ref(ref) THEN
                        [format('{}/{}*', root, ref[6:]) FOR root IN getvariable('blob_roots')]
                    ELSE [ref]
                END
            )"#,
            [],
        )?;

        Ok(())
    }

    /// Attach configured remotes to the connection.
    /// Note: S3 credentials are already set up by setup_s3_credentials().
    fn attach_remotes(&self, conn: &Connection) -> Result<()> {
        for remote in self.config.auto_attach_remotes() {
            // Attach the remote database
            let attach_sql = remote.attach_sql();
            if let Err(e) = conn.execute(&attach_sql, []) {
                eprintln!("Warning: Failed to attach remote {}: {}", remote.name, e);
            }
        }

        Ok(())
    }

    /// Create TEMPORARY macros for accessing remote data.
    ///
    /// We use TEMPORARY macros to avoid persisting references to attached databases
    /// in the catalog. Persisted references cause database corruption when the
    /// attachment is not present.
    ///
    /// Usage: `SELECT * FROM remotes_invocations()` or `SELECT * FROM remote_<name>_invocations()`
    fn create_remote_macros(&self, conn: &Connection) -> Result<()> {
        let remotes = self.config.auto_attach_remotes();
        if remotes.is_empty() {
            return Ok(());
        }

        // Create per-remote TEMPORARY macros for each table type
        for remote in &remotes {
            let schema = remote.quoted_schema_name();
            let name = &remote.name;
            // Sanitize name for use in macro identifier
            let safe_name = name.replace(['-', '.'], "_");

            for table in &["sessions", "invocations", "outputs", "events"] {
                let macro_name = format!("\"remote_{safe_name}_{table}\"");
                let sql = format!(
                    r#"CREATE OR REPLACE TEMPORARY MACRO {macro_name}() AS TABLE (
                        SELECT *, '{name}' as _source FROM {schema}.{table}
                    )"#,
                    macro_name = macro_name,
                    name = name,
                    schema = schema,
                    table = table
                );
                if let Err(e) = conn.execute(&sql, []) {
                    eprintln!("Warning: Failed to create macro {}: {}", macro_name, e);
                }
            }
        }

        // Create combined remotes_* TEMPORARY macros that union all remotes
        for table in &["sessions", "invocations", "outputs", "events"] {
            let mut union_parts: Vec<String> = remotes
                .iter()
                .map(|r| {
                    let safe_name = r.name.replace(['-', '.'], "_");
                    format!("SELECT * FROM \"remote_{safe_name}_{table}\"()", safe_name = safe_name, table = table)
                })
                .collect();

            // Include placeholder for empty case
            union_parts.push(format!("SELECT * FROM remote_placeholder.{}", table));

            let sql = format!(
                r#"CREATE OR REPLACE TEMPORARY MACRO remotes_{table}() AS TABLE (
                    {union}
                )"#,
                table = table,
                union = union_parts.join(" UNION ALL BY NAME ")
            );
            if let Err(e) = conn.execute(&sql, []) {
                eprintln!("Warning: Failed to create remotes_{} macro: {}", table, e);
            }
        }

        Ok(())
    }

    /// Create TEMPORARY macros for cwd-filtered data.
    ///
    /// These filter main.* data to entries matching the current working directory.
    /// Uses TEMPORARY macros to avoid persisting anything that changes per-connection.
    ///
    /// Usage: `SELECT * FROM cwd_invocations()`
    fn create_cwd_macros(&self, conn: &Connection) -> Result<()> {
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        let cwd_escaped = cwd.replace('\'', "''");

        // Create TEMPORARY macros for cwd-filtered data
        let macros = format!(
            r#"
            CREATE OR REPLACE TEMPORARY MACRO cwd_sessions() AS TABLE (
                SELECT * FROM main.sessions WHERE cwd LIKE '{}%'
            );
            CREATE OR REPLACE TEMPORARY MACRO cwd_invocations() AS TABLE (
                SELECT * FROM main.invocations WHERE cwd LIKE '{}%'
            );
            CREATE OR REPLACE TEMPORARY MACRO cwd_outputs() AS TABLE (
                SELECT o.* FROM main.outputs o
                JOIN main.invocations i ON o.invocation_id = i.id
                WHERE i.cwd LIKE '{}%'
            );
            CREATE OR REPLACE TEMPORARY MACRO cwd_events() AS TABLE (
                SELECT e.* FROM main.events e
                JOIN main.invocations i ON e.invocation_id = i.id
                WHERE i.cwd LIKE '{}%'
            );
            "#,
            cwd_escaped, cwd_escaped, cwd_escaped, cwd_escaped
        );

        conn.execute_batch(&macros)?;
        Ok(())
    }

    /// Manually attach a specific remote.
    pub fn attach_remote(&self, conn: &Connection, remote: &crate::RemoteConfig) -> Result<()> {
        // Load httpfs if needed
        conn.execute("LOAD httpfs", [])?;

        // Set up credentials
        if let Some(provider) = &remote.credential_provider {
            if remote.remote_type == crate::config::RemoteType::S3 {
                let secret_sql = format!(
                    "CREATE SECRET IF NOT EXISTS \"bird_{}\" (TYPE s3, PROVIDER {})",
                    remote.name, provider
                );
                conn.execute(&secret_sql, [])?;
            }
        }

        // Attach
        conn.execute(&remote.attach_sql(), [])?;
        Ok(())
    }

    /// Detach a remote.
    pub fn detach_remote(&self, conn: &Connection, name: &str) -> Result<()> {
        conn.execute(&format!("DETACH \"remote_{}\"", name), [])?;
        Ok(())
    }

    /// Test connection to a remote. Returns Ok if successful.
    pub fn test_remote(&self, remote: &crate::RemoteConfig) -> Result<()> {
        let conn = self.connection_with_options(false)?;
        self.attach_remote(&conn, remote)?;

        // Try a simple query
        let test_sql = format!(
            "SELECT 1 FROM {}.invocations LIMIT 1",
            remote.quoted_schema_name()
        );
        conn.execute(&test_sql, [])?;

        Ok(())
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
                    other => {
                        // Convert to owned Value for complex types (List, Array, Map, Struct)
                        let owned: Value = other.into();
                        format_value(&owned)
                    }
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
                    "SELECT COUNT(*) FROM local.sessions WHERE session_id = ?",
                    params![&session.session_id],
                    |row| row.get(0),
                )
                .unwrap_or(0);

            if exists == 0 {
                conn.execute(
                    r#"INSERT INTO local.sessions VALUES (?, ?, ?, ?, ?, ?, ?, ?)"#,
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
            r#"INSERT INTO local.invocations VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
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
                invocation.tag,
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
                    .strip_prefix(self.config.data_dir())
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
                r#"INSERT INTO local.outputs VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
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
                    r#"INSERT INTO local.events VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"#,
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

    /// Load format hints from the config file.
    pub fn load_format_hints(&self) -> Result<crate::FormatHints> {
        let path = self.config.format_hints_path();

        // Try new format-hints.toml first
        if path.exists() {
            return crate::FormatHints::load(&path);
        }

        // Fall back to legacy event-formats.toml
        let legacy_path = self.config.event_formats_path();
        if legacy_path.exists() {
            return crate::FormatHints::load(&legacy_path);
        }

        Ok(crate::FormatHints::new())
    }

    /// Save format hints to the config file.
    pub fn save_format_hints(&self, hints: &crate::FormatHints) -> Result<()> {
        hints.save(&self.config.format_hints_path())
    }

    /// Detect format for a command using format hints.
    ///
    /// Priority:
    /// 1. User-defined format hints (by priority)
    /// 2. Default format from config (or "auto")
    ///
    /// Note: duck_hunt detects formats from content analysis, not command names.
    /// Use format hints to map commands to formats, then duck_hunt parses the output.
    pub fn detect_format_for_command(&self, cmd: &str) -> Result<String> {
        let hints = self.load_format_hints()?;
        Ok(hints.detect(cmd).to_string())
    }

    /// Get list of duck_hunt built-in formats.
    ///
    /// Note: duck_hunt detects formats from content analysis, not command patterns.
    /// This lists available format names that can be used with duck_hunt parsing.
    pub fn list_builtin_formats(&self) -> Result<Vec<BuiltinFormat>> {
        let conn = self.connection()?;

        let mut stmt = conn.prepare(
            "SELECT format, description, priority FROM duck_hunt_formats() ORDER BY priority DESC, format"
        )?;

        let rows = stmt.query_map([], |row| {
            Ok(BuiltinFormat {
                format: row.get(0)?,
                pattern: row.get::<_, String>(1)?, // description as "pattern" for display
                priority: row.get(2)?,
            })
        })?;

        let results: Vec<_> = rows.filter_map(|r| r.ok()).collect();
        Ok(results)
    }

    /// Check which format would be detected for a command.
    /// Returns the format name and source (user-defined or default).
    ///
    /// Note: duck_hunt detects formats from content, not command names.
    /// This only checks user-defined format hints.
    pub fn check_format(&self, cmd: &str) -> Result<FormatMatch> {
        let hints = self.load_format_hints()?;

        // Check user-defined hints
        for hint in hints.hints() {
            if crate::format_hints::pattern_matches(&hint.pattern, cmd) {
                return Ok(FormatMatch {
                    format: hint.format.clone(),
                    source: FormatSource::UserDefined {
                        pattern: hint.pattern.clone(),
                        priority: hint.priority,
                    },
                });
            }
        }

        // No match - use default
        Ok(FormatMatch {
            format: hints.default_format().to_string(),
            source: FormatSource::Default,
        })
    }
}

/// A built-in format from duck_hunt.
#[derive(Debug, Clone)]
pub struct BuiltinFormat {
    pub format: String,
    pub pattern: String,
    pub priority: i32,
}

/// Result of format detection.
#[derive(Debug, Clone)]
pub struct FormatMatch {
    pub format: String,
    pub source: FormatSource,
}

/// Source of a format match.
#[derive(Debug, Clone)]
pub enum FormatSource {
    UserDefined { pattern: String, priority: i32 },
    Builtin { pattern: String, priority: i32 },
    Default,
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

    // Extension loading tests

    #[test]
    fn test_ensure_extension_parquet() {
        // Parquet is an official extension, should always be available
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let result = ensure_extension(&conn, "parquet").unwrap();
        assert!(result, "parquet extension should be loadable");
    }

    #[test]
    fn test_ensure_extension_icu() {
        // ICU is an official extension, should always be available
        let conn = duckdb::Connection::open_in_memory().unwrap();
        let result = ensure_extension(&conn, "icu").unwrap();
        assert!(result, "icu extension should be loadable");
    }

    #[test]
    fn test_ensure_extension_community() {
        // Community extensions require allow_community_extensions
        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute("SET allow_community_extensions = true", []).unwrap();

        // scalarfs and duck_hunt are community extensions
        let result = ensure_extension(&conn, "scalarfs").unwrap();
        assert!(result, "scalarfs extension should be loadable from community");

        let result = ensure_extension(&conn, "duck_hunt").unwrap();
        assert!(result, "duck_hunt extension should be loadable from community");
    }

    #[test]
    fn test_ensure_extension_nonexistent() {
        let conn = duckdb::Connection::open_in_memory().unwrap();
        conn.execute("SET allow_community_extensions = true", []).unwrap();

        // A made-up extension should return false (not error)
        let result = ensure_extension(&conn, "nonexistent_fake_extension_xyz").unwrap();
        assert!(!result, "nonexistent extension should return false");
    }

    #[test]
    fn test_extension_loading_is_cached() {
        // Once installed, extensions should load quickly from cache
        let conn = duckdb::Connection::open_in_memory().unwrap();

        // First load might install
        ensure_extension(&conn, "parquet").unwrap();

        // Second load should be fast (from cache)
        let start = std::time::Instant::now();
        ensure_extension(&conn, "parquet").unwrap();
        let elapsed = start.elapsed();

        // Should be very fast if cached (< 100ms)
        assert!(elapsed.as_millis() < 100, "cached extension load took {:?}", elapsed);
    }
}
