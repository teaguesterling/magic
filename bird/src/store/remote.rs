//! Remote sync operations (push/pull).
//!
//! Provides functionality to sync data between local and remote DuckDB databases.
//!
//! # Schema Architecture
//!
//! - **Push**: Reads from `local` schema, writes to `remote_<name>` schema tables
//! - **Pull**: Reads from `remote_<name>` schema, writes to `cached_<name>` schema tables
//!
//! Remote databases have tables: `sessions`, `invocations`, `outputs`, `events`
//! (no `_table` suffix - consistent naming across all schemas).
//!
//! # Blob Sync
//!
//! When `sync_blobs` is enabled, blob files (outputs stored as file:// refs) are
//! also synced. For file remotes, we prefer hard links (fast, no disk duplication)
//! and fall back to copying when hard links fail (cross-filesystem).

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{NaiveDate, TimeDelta, Utc};
use duckdb::Connection;

use crate::config::RemoteType;
use crate::{Error, RemoteConfig, Result};

/// Statistics from blob sync operations.
#[derive(Debug, Default)]
pub struct BlobStats {
    /// Number of blobs synced.
    pub count: usize,
    /// Total bytes synced.
    pub bytes: u64,
    /// Number of blobs hard-linked (no copy needed).
    pub linked: usize,
    /// Number of blobs copied (hard link failed).
    pub copied: usize,
    /// Number of blobs skipped (already exist).
    pub skipped: usize,
}

impl std::fmt::Display for BlobStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.count == 0 {
            write!(f, "0 blobs")
        } else {
            let kb = self.bytes / 1024;
            write!(
                f,
                "{} blobs ({}KB, {} linked, {} copied, {} skipped)",
                self.count, kb, self.linked, self.copied, self.skipped
            )
        }
    }
}

/// Statistics from a push operation.
#[derive(Debug, Default)]
pub struct PushStats {
    pub sessions: usize,
    pub invocations: usize,
    pub outputs: usize,
    pub events: usize,
    pub blobs: BlobStats,
}

impl std::fmt::Display for PushStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} sessions, {} invocations, {} outputs, {} events",
            self.sessions, self.invocations, self.outputs, self.events
        )?;
        if self.blobs.count > 0 {
            write!(f, ", {}", self.blobs)?;
        }
        Ok(())
    }
}

/// Statistics from a pull operation.
#[derive(Debug, Default)]
pub struct PullStats {
    pub sessions: usize,
    pub invocations: usize,
    pub outputs: usize,
    pub events: usize,
    pub blobs: BlobStats,
}

impl std::fmt::Display for PullStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} sessions, {} invocations, {} outputs, {} events",
            self.sessions, self.invocations, self.outputs, self.events
        )?;
        if self.blobs.count > 0 {
            write!(f, ", {}", self.blobs)?;
        }
        Ok(())
    }
}

/// Options for push operation.
#[derive(Debug, Default)]
pub struct PushOptions {
    /// Only push data since this date.
    pub since: Option<NaiveDate>,
    /// Show what would be pushed without actually pushing.
    pub dry_run: bool,
    /// Sync blob files (not just metadata).
    pub sync_blobs: bool,
}

/// Options for pull operation.
#[derive(Debug, Default)]
pub struct PullOptions {
    /// Only pull data since this date.
    pub since: Option<NaiveDate>,
    /// Only pull data from this client.
    pub client_id: Option<String>,
    /// Sync blob files (not just metadata).
    pub sync_blobs: bool,
}

/// Parse a "since" string into a date.
///
/// Supports:
/// - Duration: "7d", "2w", "1m" (days, weeks, months)
/// - Date: "2024-01-15"
pub fn parse_since(s: &str) -> Result<NaiveDate> {
    let s = s.trim();

    // Try duration first (7d, 2w, 1m)
    if let Some(days) = parse_duration_days(s) {
        let date = Utc::now().date_naive() - TimeDelta::days(days);
        return Ok(date);
    }

    // Try date format (YYYY-MM-DD)
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map_err(|e| Error::Config(format!("Invalid date '{}': {}", s, e)))
}

/// Parse a duration string into days.
fn parse_duration_days(s: &str) -> Option<i64> {
    let s = s.trim().to_lowercase();

    if let Some(num) = s.strip_suffix('d') {
        num.parse::<i64>().ok()
    } else if let Some(num) = s.strip_suffix('w') {
        num.parse::<i64>().ok().map(|n| n * 7)
    } else if let Some(num) = s.strip_suffix('m') {
        num.parse::<i64>().ok().map(|n| n * 30)
    } else {
        None
    }
}

/// Get the cached schema name for a remote (e.g., "cached_team" for remote "team").
#[allow(dead_code)]
pub fn cached_schema_name(remote_name: &str) -> String {
    format!("cached_{}", remote_name)
}

/// Get the quoted cached schema name for SQL.
pub fn quoted_cached_schema_name(remote_name: &str) -> String {
    format!("\"cached_{}\"", remote_name)
}

/// Get the data directory for a file remote.
///
/// For a remote URI like `file:///path/to/remote.duckdb`, returns `/path/to`.
/// This is where blob paths (like `recent/blobs/content/...`) are relative to.
fn file_remote_data_dir(remote: &RemoteConfig) -> Option<PathBuf> {
    if remote.remote_type != RemoteType::File {
        return None;
    }

    // Parse file:// URI to get the database path
    let db_path = remote.uri.strip_prefix("file://")?;
    let db_path = Path::new(db_path);

    // Data directory is the parent of the .duckdb file
    // e.g., /path/to/remote.duckdb -> /path/to
    db_path.parent().map(PathBuf::from)
}

/// Information about a blob to sync.
#[derive(Debug)]
struct BlobInfo {
    content_hash: String,
    storage_path: String,
    byte_length: i64,
}

/// Sync a single blob file using hard link or copy.
///
/// Returns `Ok(true)` if the blob was synced (linked or copied),
/// `Ok(false)` if it already exists at destination.
fn sync_blob_file(src: &Path, dst: &Path, stats: &mut BlobStats) -> Result<bool> {
    // Check if destination already exists
    if dst.exists() {
        stats.skipped += 1;
        return Ok(false);
    }

    // Ensure parent directory exists
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent)?;
    }

    // Try hard link first (fast, no disk duplication)
    match fs::hard_link(src, dst) {
        Ok(()) => {
            stats.linked += 1;
            stats.count += 1;
            if let Ok(meta) = fs::metadata(dst) {
                stats.bytes += meta.len();
            }
            Ok(true)
        }
        Err(_) => {
            // Fall back to copy (cross-filesystem)
            fs::copy(src, dst)?;
            stats.copied += 1;
            stats.count += 1;
            if let Ok(meta) = fs::metadata(dst) {
                stats.bytes += meta.len();
            }
            Ok(true)
        }
    }
}

impl super::Store {
    /// Push local data to a remote.
    ///
    /// Reads from `local` schema, writes to remote's tables.
    /// Only pushes records that don't already exist on the remote (by id).
    /// When `sync_blobs` is enabled, also syncs blob files for file remotes.
    pub fn push(&self, remote: &RemoteConfig, opts: PushOptions) -> Result<PushStats> {
        use crate::config::RemoteMode;

        // Read-only remotes can't be pushed to - return empty stats for dry_run
        if remote.mode == RemoteMode::ReadOnly {
            if opts.dry_run {
                // Nothing to push to a read-only remote
                return Ok(PushStats::default());
            } else {
                return Err(Error::Config(format!(
                    "Cannot push to read-only remote '{}'",
                    remote.name
                )));
            }
        }

        // Use connection without auto-attach to avoid conflicts and unnecessary views
        let conn = self.connection_with_options(false)?;

        // Attach only the target remote
        self.attach_remote(&conn, remote)?;

        let remote_schema = remote.quoted_schema_name();

        // Ensure remote has the required tables (including blob_registry)
        ensure_remote_schema(&conn, &remote_schema)?;

        let mut stats = PushStats::default();

        if opts.dry_run {
            // Count what would be pushed
            stats.sessions = count_sessions_to_push(&conn, &remote_schema, opts.since)?;
            // V5: count only attempts (represents invocation count)
            stats.invocations = count_table_to_push(&conn, "attempts", &remote_schema, opts.since)?;
            stats.outputs = count_table_to_push(&conn, "outputs", &remote_schema, opts.since)?;
            stats.events = count_table_to_push(&conn, "events", &remote_schema, opts.since)?;
            if opts.sync_blobs {
                stats.blobs = count_blobs_to_push(&conn, &remote_schema, opts.since)?;
            }
        } else {
            // Sync blobs first (before pushing output metadata)
            if opts.sync_blobs {
                stats.blobs = self.push_blobs(&conn, remote, &remote_schema, opts.since)?;
            }

            // Actually push in dependency order
            stats.sessions = push_sessions(&conn, &remote_schema, opts.since)?;
            // V5: push attempts first, then outcomes (report attempts count as "invocations")
            stats.invocations = push_table(&conn, "attempts", &remote_schema, opts.since)?;
            let _ = push_table(&conn, "outcomes", &remote_schema, opts.since)?;
            stats.outputs = push_outputs(&conn, &remote_schema, opts.since, opts.sync_blobs)?;
            stats.events = push_table(&conn, "events", &remote_schema, opts.since)?;
        }

        Ok(stats)
    }

    /// Push blob files to a file remote.
    ///
    /// Syncs blob files using hard links (preferred) or copies (fallback).
    /// Also syncs blob_registry entries.
    fn push_blobs(
        &self,
        conn: &Connection,
        remote: &RemoteConfig,
        remote_schema: &str,
        since: Option<NaiveDate>,
    ) -> Result<BlobStats> {
        let mut stats = BlobStats::default();

        // Only file remotes support blob sync for now
        let remote_data_dir = match file_remote_data_dir(remote) {
            Some(dir) => dir,
            None => return Ok(stats), // Not a file remote, skip blob sync
        };

        // Find blobs that need to be synced
        let blobs = get_blobs_to_push(conn, remote_schema, since)?;
        if blobs.is_empty() {
            return Ok(stats);
        }

        let local_data_dir = self.config.data_dir();

        for blob in &blobs {
            // Build source and destination paths
            // storage_path is relative to data_dir (e.g., "recent/blobs/content/ab/hash.bin")
            let src = local_data_dir.join(&blob.storage_path);
            let dst = remote_data_dir.join(&blob.storage_path);

            if !src.exists() {
                // Source blob missing, skip
                continue;
            }

            // Sync the blob file
            sync_blob_file(&src, &dst, &mut stats)?;

            // Sync blob_registry entry
            let escaped_hash = blob.content_hash.replace('\'', "''");
            let escaped_path = blob.storage_path.replace('\'', "''");
            conn.execute(
                &format!(
                    r#"
                    INSERT INTO {schema}.blob_registry (content_hash, byte_length, storage_path)
                    SELECT '{hash}', {len}, '{path}'
                    WHERE NOT EXISTS (
                        SELECT 1 FROM {schema}.blob_registry WHERE content_hash = '{hash}'
                    )
                    "#,
                    schema = remote_schema,
                    hash = escaped_hash,
                    len = blob.byte_length,
                    path = escaped_path,
                ),
                [],
            )?;
        }

        Ok(stats)
    }

    /// Pull data from a remote into local cached_<name> schema.
    ///
    /// Reads from remote's tables, writes to `cached_<name>` schema.
    /// Only pulls records that don't already exist in the cached schema (by id).
    /// After pulling, rebuilds the `caches` union views.
    /// When `sync_blobs` is enabled, also syncs blob files for file remotes.
    pub fn pull(&self, remote: &RemoteConfig, opts: PullOptions) -> Result<PullStats> {
        // Use connection without auto-attach to avoid conflicts
        let conn = self.connection_with_options(false)?;

        // Attach only the target remote
        self.attach_remote(&conn, remote)?;

        let remote_schema = remote.quoted_schema_name();
        let cached_schema = quoted_cached_schema_name(&remote.name);

        // Ensure cached schema exists with required tables
        ensure_cached_schema(&conn, &cached_schema, &remote.name)?;

        // Pull in dependency order (sessions first, then attempts, outcomes, outputs, events)
        // V5: pull attempts first, then outcomes (report attempts count as "invocations")
        let attempts_pulled = pull_table(&conn, "attempts", &remote_schema, &cached_schema, opts.since, opts.client_id.as_deref())?;
        let _ = pull_table(&conn, "outcomes", &remote_schema, &cached_schema, opts.since, opts.client_id.as_deref())?;
        let mut stats = PullStats {
            sessions: pull_sessions(&conn, &remote_schema, &cached_schema, opts.since, opts.client_id.as_deref())?,
            invocations: attempts_pulled,
            outputs: pull_outputs(&conn, &remote_schema, &cached_schema, opts.since, opts.client_id.as_deref(), opts.sync_blobs)?,
            events: pull_table(&conn, "events", &remote_schema, &cached_schema, opts.since, opts.client_id.as_deref())?,
            blobs: BlobStats::default(),
        };

        // Sync blob files after pulling output metadata
        if opts.sync_blobs {
            stats.blobs = self.pull_blobs(&conn, remote, &remote_schema, &cached_schema)?;
        }

        // Rebuild caches union views to include this cached schema
        self.rebuild_caches_schema(&conn)?;

        Ok(stats)
    }

    /// Pull blob files from a file remote.
    ///
    /// Syncs blob files using hard links (preferred) or copies (fallback).
    /// Also registers blobs in the local blob_registry.
    fn pull_blobs(
        &self,
        conn: &Connection,
        remote: &RemoteConfig,
        remote_schema: &str,
        cached_schema: &str,
    ) -> Result<BlobStats> {
        let mut stats = BlobStats::default();

        // Only file remotes support blob sync for now
        let remote_data_dir = match file_remote_data_dir(remote) {
            Some(dir) => dir,
            None => return Ok(stats), // Not a file remote, skip blob sync
        };

        // Find blobs that were pulled (in cached outputs but not in local blob_registry)
        let blobs = get_blobs_to_pull(conn, remote_schema, cached_schema)?;
        if blobs.is_empty() {
            return Ok(stats);
        }

        let local_data_dir = self.config.data_dir();

        for blob in &blobs {
            // Build source and destination paths
            // storage_path is relative to data_dir (e.g., "recent/blobs/content/ab/hash.bin")
            let src = remote_data_dir.join(&blob.storage_path);
            let dst = local_data_dir.join(&blob.storage_path);

            if !src.exists() {
                // Source blob missing on remote, skip
                continue;
            }

            // Sync the blob file
            let synced = sync_blob_file(&src, &dst, &mut stats)?;

            // Register in local blob_registry if we synced a new blob
            if synced {
                let escaped_hash = blob.content_hash.replace('\'', "''");
                let escaped_path = blob.storage_path.replace('\'', "''");
                conn.execute(
                    &format!(
                        r#"
                        INSERT INTO blob_registry (content_hash, byte_length, storage_path)
                        SELECT '{hash}', {len}, '{path}'
                        WHERE NOT EXISTS (
                            SELECT 1 FROM blob_registry WHERE content_hash = '{hash}'
                        )
                        "#,
                        hash = escaped_hash,
                        len = blob.byte_length,
                        path = escaped_path,
                    ),
                    [],
                )?;
            }
        }

        Ok(stats)
    }

    /// Rebuild the `caches` schema views to union all `cached_*` schemas.
    ///
    /// Uses explicit transaction for DDL safety. The caches.* views reference
    /// local cached_* schemas (not attached databases), so they should be safe
    /// to persist.
    /// V5 schema: unions attempts/outcomes tables and creates invocations view.
    pub fn rebuild_caches_schema(&self, conn: &Connection) -> Result<()> {
        // Find all cached_* schemas
        let schemas: Vec<String> = conn
            .prepare("SELECT schema_name FROM information_schema.schemata WHERE schema_name LIKE 'cached_%'")?
            .query_map([], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();

        // Use transaction for DDL safety
        conn.execute("BEGIN TRANSACTION", [])?;

        let result = (|| -> std::result::Result<(), duckdb::Error> {
            // V5: union attempts and outcomes tables
            for table in &["sessions", "attempts", "outcomes", "outputs", "events"] {
                let mut union_parts: Vec<String> = schemas
                    .iter()
                    .map(|s| format!("SELECT * FROM \"{}\".{}", s, table))
                    .collect();

                // Always include placeholder (ensures view is valid even with no cached schemas)
                if !schemas.iter().any(|s| s == "cached_placeholder") {
                    union_parts.push(format!("SELECT * FROM cached_placeholder.{}", table));
                }

                let sql = format!(
                    "CREATE OR REPLACE VIEW caches.{} AS {}",
                    table,
                    union_parts.join(" UNION ALL BY NAME ")
                );
                conn.execute(&sql, [])?;
            }

            // V5: Create invocations view that joins attempts and outcomes
            // Note: We don't include metadata in this view because merging MAPs from
            // different sources is problematic. Use attempts.metadata or outcomes.metadata directly.
            conn.execute(
                r#"
                CREATE OR REPLACE VIEW caches.invocations AS
                SELECT
                    a.id, a.timestamp, a.cmd, a.cwd, a.session_id,
                    a.tag, a.source_client, a.machine_id, a.hostname,
                    a.executable, a.format_hint,
                    o.completed_at, o.exit_code, o.duration_ms, o.signal, o.timeout,
                    a.date,
                    CASE
                        WHEN o.attempt_id IS NULL THEN 'pending'
                        WHEN o.exit_code IS NULL THEN 'orphaned'
                        ELSE 'completed'
                    END AS status,
                    a._source
                FROM caches.attempts a
                LEFT JOIN caches.outcomes o ON a.id = o.attempt_id
                "#,
                [],
            )?;

            Ok(())
        })();

        match result {
            Ok(()) => {
                conn.execute("COMMIT", [])?;
                Ok(())
            }
            Err(e) => {
                let _ = conn.execute("ROLLBACK", []);
                Err(crate::Error::DuckDb(e))
            }
        }
    }
}

/// Ensure the remote schema has the required tables.
/// Tables use consistent naming (no `_table` suffix).
/// V5 schema: uses attempts/outcomes tables instead of invocations.
fn ensure_remote_schema(conn: &Connection, schema: &str) -> Result<()> {
    let sql = format!(
        r#"
        CREATE TABLE IF NOT EXISTS {schema}.sessions (
            session_id VARCHAR, client_id VARCHAR, invoker VARCHAR, invoker_pid INTEGER,
            invoker_type VARCHAR, registered_at TIMESTAMP, cwd VARCHAR, date DATE
        );
        -- V5: attempts table (invocation start)
        CREATE TABLE IF NOT EXISTS {schema}.attempts (
            id UUID, timestamp TIMESTAMP, cmd VARCHAR, cwd VARCHAR, session_id VARCHAR,
            tag VARCHAR, source_client VARCHAR, machine_id VARCHAR, hostname VARCHAR,
            executable VARCHAR, format_hint VARCHAR, metadata MAP(VARCHAR, JSON), date DATE
        );
        -- V5: outcomes table (invocation completion)
        CREATE TABLE IF NOT EXISTS {schema}.outcomes (
            attempt_id UUID, completed_at TIMESTAMP, exit_code INTEGER, duration_ms BIGINT,
            signal INTEGER, timeout BOOLEAN, metadata MAP(VARCHAR, JSON), date DATE
        );
        -- V5: invocations VIEW for compatibility
        -- Note: metadata not included due to MAP_CONCAT complexity; use attempts/outcomes directly
        CREATE OR REPLACE VIEW {schema}.invocations AS
        SELECT
            a.id, a.timestamp, a.cmd, a.cwd, a.session_id,
            a.tag, a.source_client, a.machine_id, a.hostname,
            a.executable, a.format_hint,
            o.completed_at, o.exit_code, o.duration_ms, o.signal, o.timeout,
            a.date,
            CASE
                WHEN o.attempt_id IS NULL THEN 'pending'
                WHEN o.exit_code IS NULL THEN 'orphaned'
                ELSE 'completed'
            END AS status
        FROM {schema}.attempts a
        LEFT JOIN {schema}.outcomes o ON a.id = o.attempt_id;
        CREATE TABLE IF NOT EXISTS {schema}.outputs (
            id UUID, invocation_id UUID, stream VARCHAR, content_hash VARCHAR,
            byte_length BIGINT, storage_type VARCHAR, storage_ref VARCHAR,
            content_type VARCHAR, date DATE
        );
        CREATE TABLE IF NOT EXISTS {schema}.events (
            id UUID, invocation_id UUID, client_id VARCHAR, hostname VARCHAR,
            event_type VARCHAR, severity VARCHAR, ref_file VARCHAR, ref_line INTEGER,
            ref_column INTEGER, message VARCHAR, error_code VARCHAR, test_name VARCHAR,
            status VARCHAR, format_used VARCHAR, date DATE
        );
        CREATE TABLE IF NOT EXISTS {schema}.blob_registry (
            content_hash VARCHAR PRIMARY KEY,
            byte_length BIGINT NOT NULL,
            ref_count INTEGER DEFAULT 1,
            first_seen TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
            last_accessed TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
            storage_path VARCHAR NOT NULL
        );
        "#,
        schema = schema
    );
    conn.execute_batch(&sql)?;
    Ok(())
}

/// Ensure the cached schema exists with required tables.
/// Tables include a `_source` column to track which remote the data came from.
/// V5 schema: uses attempts/outcomes tables instead of invocations.
fn ensure_cached_schema(conn: &Connection, schema: &str, remote_name: &str) -> Result<()> {
    // Create the schema if it doesn't exist
    conn.execute(&format!("CREATE SCHEMA IF NOT EXISTS {}", schema), [])?;

    // Create tables with _source column
    let sql = format!(
        r#"
        CREATE TABLE IF NOT EXISTS {schema}.sessions (
            session_id VARCHAR, client_id VARCHAR, invoker VARCHAR, invoker_pid INTEGER,
            invoker_type VARCHAR, registered_at TIMESTAMP, cwd VARCHAR, date DATE,
            _source VARCHAR DEFAULT '{remote_name}'
        );
        -- V5: attempts table (invocation start)
        CREATE TABLE IF NOT EXISTS {schema}.attempts (
            id UUID, timestamp TIMESTAMP, cmd VARCHAR, cwd VARCHAR, session_id VARCHAR,
            tag VARCHAR, source_client VARCHAR, machine_id VARCHAR, hostname VARCHAR,
            executable VARCHAR, format_hint VARCHAR, metadata MAP(VARCHAR, JSON), date DATE,
            _source VARCHAR DEFAULT '{remote_name}'
        );
        -- V5: outcomes table (invocation completion)
        CREATE TABLE IF NOT EXISTS {schema}.outcomes (
            attempt_id UUID, completed_at TIMESTAMP, exit_code INTEGER, duration_ms BIGINT,
            signal INTEGER, timeout BOOLEAN, metadata MAP(VARCHAR, JSON), date DATE,
            _source VARCHAR DEFAULT '{remote_name}'
        );
        -- V5: invocations VIEW for compatibility
        -- Note: metadata not included due to MAP_CONCAT complexity; use attempts/outcomes directly
        CREATE OR REPLACE VIEW {schema}.invocations AS
        SELECT
            a.id, a.timestamp, a.cmd, a.cwd, a.session_id,
            a.tag, a.source_client, a.machine_id, a.hostname,
            a.executable, a.format_hint,
            o.completed_at, o.exit_code, o.duration_ms, o.signal, o.timeout,
            a.date,
            CASE
                WHEN o.attempt_id IS NULL THEN 'pending'
                WHEN o.exit_code IS NULL THEN 'orphaned'
                ELSE 'completed'
            END AS status,
            a._source
        FROM {schema}.attempts a
        LEFT JOIN {schema}.outcomes o ON a.id = o.attempt_id;
        CREATE TABLE IF NOT EXISTS {schema}.outputs (
            id UUID, invocation_id UUID, stream VARCHAR, content_hash VARCHAR,
            byte_length BIGINT, storage_type VARCHAR, storage_ref VARCHAR,
            content_type VARCHAR, date DATE,
            _source VARCHAR DEFAULT '{remote_name}'
        );
        CREATE TABLE IF NOT EXISTS {schema}.events (
            id UUID, invocation_id UUID, client_id VARCHAR, hostname VARCHAR,
            event_type VARCHAR, severity VARCHAR, ref_file VARCHAR, ref_line INTEGER,
            ref_column INTEGER, message VARCHAR, error_code VARCHAR, test_name VARCHAR,
            status VARCHAR, format_used VARCHAR, date DATE,
            _source VARCHAR DEFAULT '{remote_name}'
        );
        "#,
        schema = schema,
        remote_name = remote_name.replace('\'', "''")
    );
    conn.execute_batch(&sql)?;
    Ok(())
}

/// Build the WHERE clause for time filtering.
fn since_clause(since: Option<NaiveDate>, timestamp_col: &str) -> String {
    since
        .map(|d| format!("AND {} >= '{}'", timestamp_col, d))
        .unwrap_or_default()
}

/// Build the WHERE clause for client filtering.
fn client_clause(client_id: Option<&str>) -> String {
    client_id
        .map(|c| format!("AND client_id = '{}'", c.replace('\'', "''")))
        .unwrap_or_default()
}

/// Count sessions that would be pushed.
/// Reads from `local` schema.
/// V5 schema: joins on attempts instead of invocations.
fn count_sessions_to_push(
    conn: &Connection,
    remote_schema: &str,
    since: Option<NaiveDate>,
) -> Result<usize> {
    let since_filter = since_clause(since, "a.timestamp");

    let sql = format!(
        r#"
        SELECT COUNT(DISTINCT s.session_id)
        FROM local.sessions s
        JOIN local.attempts a ON a.session_id = s.session_id
        WHERE NOT EXISTS (
            SELECT 1 FROM {remote}.sessions r WHERE r.session_id = s.session_id
        )
        {since}
        "#,
        remote = remote_schema,
        since = since_filter,
    );

    let count: i64 = conn.query_row(&sql, [], |row| row.get(0))?;
    Ok(count as usize)
}

/// Count records that would be pushed for a table.
/// Reads from `local` schema.
/// V5 schema: uses attempts/outcomes tables instead of invocations.
fn count_table_to_push(
    conn: &Connection,
    table: &str,
    remote_schema: &str,
    since: Option<NaiveDate>,
) -> Result<usize> {
    let sql = match table {
        // V5: count attempts
        "attempts" => {
            let since_filter = since_clause(since, "l.timestamp");
            format!(
                r#"
                SELECT COUNT(*)
                FROM local.attempts l
                WHERE NOT EXISTS (
                    SELECT 1 FROM {remote}.attempts r WHERE r.id = l.id
                )
                {since}
                "#,
                remote = remote_schema,
                since = since_filter,
            )
        }
        // V5: count outcomes
        "outcomes" => {
            let since_filter = since_clause(since, "a.timestamp");
            format!(
                r#"
                SELECT COUNT(*)
                FROM local.outcomes l
                JOIN local.attempts a ON a.id = l.attempt_id
                WHERE NOT EXISTS (
                    SELECT 1 FROM {remote}.outcomes r WHERE r.attempt_id = l.attempt_id
                )
                {since}
                "#,
                remote = remote_schema,
                since = since_filter,
            )
        }
        "outputs" | "events" => {
            // V5: join on attempts instead of invocations
            let since_filter = since_clause(since, "a.timestamp");
            format!(
                r#"
                SELECT COUNT(*)
                FROM local.{table} l
                JOIN local.attempts a ON a.id = l.invocation_id
                WHERE NOT EXISTS (
                    SELECT 1 FROM {remote}.{table} r WHERE r.id = l.id
                )
                {since}
                "#,
                table = table,
                remote = remote_schema,
                since = since_filter,
            )
        }
        _ => {
            format!(
                r#"
                SELECT COUNT(*)
                FROM local.{table} l
                WHERE NOT EXISTS (
                    SELECT 1 FROM {remote}.{table} r WHERE r.id = l.id
                )
                "#,
                table = table,
                remote = remote_schema,
            )
        }
    };

    let count: i64 = conn.query_row(&sql, [], |row| row.get(0))?;
    Ok(count as usize)
}

/// Push sessions from `local` to remote.
/// V5 schema: joins on attempts instead of invocations.
fn push_sessions(
    conn: &Connection,
    remote_schema: &str,
    since: Option<NaiveDate>,
) -> Result<usize> {
    let since_filter = since_clause(since, "a.timestamp");

    let sql = format!(
        r#"
        INSERT INTO {remote}.sessions
        SELECT DISTINCT s.*
        FROM local.sessions s
        JOIN local.attempts a ON a.session_id = s.session_id
        WHERE NOT EXISTS (
            SELECT 1 FROM {remote}.sessions r WHERE r.session_id = s.session_id
        )
        {since}
        "#,
        remote = remote_schema,
        since = since_filter,
    );

    let count = conn.execute(&sql, [])?;
    Ok(count)
}

/// Push records from `local` to remote.
/// V5 schema: uses attempts/outcomes tables instead of invocations.
fn push_table(
    conn: &Connection,
    table: &str,
    remote_schema: &str,
    since: Option<NaiveDate>,
) -> Result<usize> {
    let sql = match table {
        // V5: Push attempts table
        "attempts" => {
            let since_filter = since_clause(since, "l.timestamp");
            format!(
                r#"
                INSERT INTO {remote}.attempts
                SELECT *
                FROM local.attempts l
                WHERE NOT EXISTS (
                    SELECT 1 FROM {remote}.attempts r WHERE r.id = l.id
                )
                {since}
                "#,
                remote = remote_schema,
                since = since_filter,
            )
        }
        // V5: Push outcomes table
        "outcomes" => {
            let since_filter = since_clause(since, "a.timestamp");
            format!(
                r#"
                INSERT INTO {remote}.outcomes
                SELECT l.*
                FROM local.outcomes l
                JOIN local.attempts a ON a.id = l.attempt_id
                WHERE NOT EXISTS (
                    SELECT 1 FROM {remote}.outcomes r WHERE r.attempt_id = l.attempt_id
                )
                {since}
                "#,
                remote = remote_schema,
                since = since_filter,
            )
        }
        "outputs" | "events" => {
            // V5: Join on attempts instead of invocations
            let since_filter = since_clause(since, "a.timestamp");
            format!(
                r#"
                INSERT INTO {remote}.{table}
                SELECT l.*
                FROM local.{table} l
                JOIN local.attempts a ON a.id = l.invocation_id
                WHERE NOT EXISTS (
                    SELECT 1 FROM {remote}.{table} r WHERE r.id = l.id
                )
                {since}
                "#,
                table = table,
                remote = remote_schema,
                since = since_filter,
            )
        }
        _ => {
            format!(
                r#"
                INSERT INTO {remote}.{table}
                SELECT *
                FROM local.{table} l
                WHERE NOT EXISTS (
                    SELECT 1 FROM {remote}.{table} r WHERE r.id = l.id
                )
                "#,
                table = table,
                remote = remote_schema,
            )
        }
    };

    let count = conn.execute(&sql, [])?;
    Ok(count)
}

/// Pull sessions from remote into cached schema.
fn pull_sessions(
    conn: &Connection,
    remote_schema: &str,
    cached_schema: &str,
    since: Option<NaiveDate>,
    client_id: Option<&str>,
) -> Result<usize> {
    let since_filter = since_clause(since, "r.registered_at");
    let client_filter = client_clause(client_id);

    let sql = format!(
        r#"
        INSERT INTO {cached}.sessions (session_id, client_id, invoker, invoker_pid, invoker_type, registered_at, cwd, date)
        SELECT r.*
        FROM {remote}.sessions r
        WHERE NOT EXISTS (
            SELECT 1 FROM {cached}.sessions l WHERE l.session_id = r.session_id
        )
        {since}
        {client}
        "#,
        cached = cached_schema,
        remote = remote_schema,
        since = since_filter,
        client = client_filter,
    );

    let count = conn.execute(&sql, [])?;
    Ok(count)
}

/// Pull records from remote into cached schema.
/// V5 schema: uses attempts/outcomes tables instead of invocations.
fn pull_table(
    conn: &Connection,
    table: &str,
    remote_schema: &str,
    cached_schema: &str,
    since: Option<NaiveDate>,
    client_id: Option<&str>,
) -> Result<usize> {
    let client_filter = client_clause(client_id);

    let sql = match table {
        // V5: pull attempts
        "attempts" => {
            let since_filter = since_clause(since, "r.timestamp");
            format!(
                r#"
                INSERT INTO {cached}.attempts (id, timestamp, cmd, cwd, session_id, tag, source_client, machine_id, hostname, executable, format_hint, metadata, date)
                SELECT r.*
                FROM {remote}.attempts r
                WHERE NOT EXISTS (
                    SELECT 1 FROM {cached}.attempts l WHERE l.id = r.id
                )
                {since}
                {client}
                "#,
                cached = cached_schema,
                remote = remote_schema,
                since = since_filter,
                client = if client_id.is_some() {
                    format!("AND r.source_client = '{}'", client_id.unwrap().replace('\'', "''"))
                } else {
                    String::new()
                },
            )
        }
        // V5: pull outcomes
        "outcomes" => {
            let since_filter = since_clause(since, "a.timestamp");
            format!(
                r#"
                INSERT INTO {cached}.outcomes (attempt_id, completed_at, exit_code, duration_ms, signal, timeout, metadata, date)
                SELECT r.*
                FROM {remote}.outcomes r
                JOIN {remote}.attempts a ON a.id = r.attempt_id
                WHERE NOT EXISTS (
                    SELECT 1 FROM {cached}.outcomes l WHERE l.attempt_id = r.attempt_id
                )
                {since}
                {client}
                "#,
                cached = cached_schema,
                remote = remote_schema,
                since = since_filter,
                client = if client_id.is_some() {
                    format!("AND a.source_client = '{}'", client_id.unwrap().replace('\'', "''"))
                } else {
                    String::new()
                },
            )
        }
        // V5: join on attempts instead of invocations
        "outputs" => {
            let since_filter = since_clause(since, "a.timestamp");
            format!(
                r#"
                INSERT INTO {cached}.outputs (id, invocation_id, stream, content_hash, byte_length, storage_type, storage_ref, content_type, date)
                SELECT r.*
                FROM {remote}.outputs r
                JOIN {remote}.attempts a ON a.id = r.invocation_id
                WHERE NOT EXISTS (
                    SELECT 1 FROM {cached}.outputs l WHERE l.id = r.id
                )
                {since}
                {client}
                "#,
                cached = cached_schema,
                remote = remote_schema,
                since = since_filter,
                client = if client_id.is_some() {
                    format!("AND a.source_client = '{}'", client_id.unwrap().replace('\'', "''"))
                } else {
                    String::new()
                },
            )
        }
        "events" => {
            let since_filter = since_clause(since, "a.timestamp");
            format!(
                r#"
                INSERT INTO {cached}.events (id, invocation_id, client_id, hostname, event_type, severity, ref_file, ref_line, ref_column, message, error_code, test_name, status, format_used, date)
                SELECT r.*
                FROM {remote}.events r
                JOIN {remote}.attempts a ON a.id = r.invocation_id
                WHERE NOT EXISTS (
                    SELECT 1 FROM {cached}.events l WHERE l.id = r.id
                )
                {since}
                {client}
                "#,
                cached = cached_schema,
                remote = remote_schema,
                since = since_filter,
                client = if client_id.is_some() {
                    format!("AND a.source_client = '{}'", client_id.unwrap().replace('\'', "''"))
                } else {
                    String::new()
                },
            )
        }
        _ => {
            format!(
                r#"
                INSERT INTO {cached}.{table}
                SELECT r.*
                FROM {remote}.{table} r
                WHERE NOT EXISTS (
                    SELECT 1 FROM {cached}.{table} l WHERE l.id = r.id
                )
                {client}
                "#,
                table = table,
                cached = cached_schema,
                remote = remote_schema,
                client = client_filter,
            )
        }
    };

    let count = conn.execute(&sql, [])?;
    Ok(count)
}

/// Count blobs that would be pushed (for dry run).
/// V5 schema: joins on attempts instead of invocations.
fn count_blobs_to_push(
    conn: &Connection,
    remote_schema: &str,
    since: Option<NaiveDate>,
) -> Result<BlobStats> {
    let since_filter = since_clause(since, "a.timestamp");

    let sql = format!(
        r#"
        SELECT COUNT(DISTINCT o.content_hash), COALESCE(SUM(o.byte_length), 0)
        FROM local.outputs o
        JOIN local.attempts a ON a.id = o.invocation_id
        WHERE o.storage_type = 'blob'
          AND NOT EXISTS (
              SELECT 1 FROM {remote}.blob_registry r WHERE r.content_hash = o.content_hash
          )
        {since}
        "#,
        remote = remote_schema,
        since = since_filter,
    );

    let (count, bytes): (i64, i64) = conn.query_row(&sql, [], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(BlobStats {
        count: count as usize,
        bytes: bytes as u64,
        ..Default::default()
    })
}

/// Get blobs that need to be pushed to remote.
/// V5 schema: joins on attempts instead of invocations.
fn get_blobs_to_push(
    conn: &Connection,
    remote_schema: &str,
    since: Option<NaiveDate>,
) -> Result<Vec<BlobInfo>> {
    let since_filter = since_clause(since, "a.timestamp");

    let sql = format!(
        r#"
        SELECT DISTINCT o.content_hash, b.storage_path, o.byte_length
        FROM local.outputs o
        JOIN local.attempts a ON a.id = o.invocation_id
        JOIN blob_registry b ON b.content_hash = o.content_hash
        WHERE o.storage_type = 'blob'
          AND NOT EXISTS (
              SELECT 1 FROM {remote}.blob_registry r WHERE r.content_hash = o.content_hash
          )
        {since}
        "#,
        remote = remote_schema,
        since = since_filter,
    );

    let mut stmt = conn.prepare(&sql)?;
    let blobs = stmt
        .query_map([], |row| {
            Ok(BlobInfo {
                content_hash: row.get(0)?,
                storage_path: row.get(1)?,
                byte_length: row.get(2)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(blobs)
}

/// Get blobs that were pulled (in remote outputs but not in local blob_registry).
fn get_blobs_to_pull(
    conn: &Connection,
    remote_schema: &str,
    cached_schema: &str,
) -> Result<Vec<BlobInfo>> {
    let sql = format!(
        r#"
        SELECT DISTINCT o.content_hash, b.storage_path, o.byte_length
        FROM {cached}.outputs o
        JOIN {remote}.blob_registry b ON b.content_hash = o.content_hash
        WHERE o.storage_type = 'blob'
          AND NOT EXISTS (
              SELECT 1 FROM blob_registry r WHERE r.content_hash = o.content_hash
          )
        "#,
        cached = cached_schema,
        remote = remote_schema,
    );

    let mut stmt = conn.prepare(&sql)?;
    let blobs = stmt
        .query_map([], |row| {
            Ok(BlobInfo {
                content_hash: row.get(0)?,
                storage_path: row.get(1)?,
                byte_length: row.get(2)?,
            })
        })?
        .filter_map(|r| r.ok())
        .collect();

    Ok(blobs)
}

/// Push outputs to remote, optionally converting storage_ref paths.
/// V5 schema: joins on attempts instead of invocations.
fn push_outputs(
    conn: &Connection,
    remote_schema: &str,
    since: Option<NaiveDate>,
    _sync_blobs: bool,
) -> Result<usize> {
    let since_filter = since_clause(since, "a.timestamp");

    // For now, we keep storage_ref as-is. The blob files are synced separately.
    // The storage_ref format (file://recent/blobs/...) is relative and works on both sides.
    let sql = format!(
        r#"
        INSERT INTO {remote}.outputs
        SELECT l.*
        FROM local.outputs l
        JOIN local.attempts a ON a.id = l.invocation_id
        WHERE NOT EXISTS (
            SELECT 1 FROM {remote}.outputs r WHERE r.id = l.id
        )
        {since}
        "#,
        remote = remote_schema,
        since = since_filter,
    );

    let count = conn.execute(&sql, [])?;
    Ok(count)
}

/// Pull outputs from remote, optionally handling storage_ref paths.
/// V5 schema: joins on attempts instead of invocations.
fn pull_outputs(
    conn: &Connection,
    remote_schema: &str,
    cached_schema: &str,
    since: Option<NaiveDate>,
    client_id: Option<&str>,
    _sync_blobs: bool,
) -> Result<usize> {
    let since_filter = since_clause(since, "a.timestamp");
    let client_filter = if client_id.is_some() {
        format!("AND a.source_client = '{}'", client_id.unwrap().replace('\'', "''"))
    } else {
        String::new()
    };

    // For now, we keep storage_ref as-is. The blob files are synced separately.
    // The storage_ref format (file://recent/blobs/...) is relative and works on both sides.
    let sql = format!(
        r#"
        INSERT INTO {cached}.outputs (id, invocation_id, stream, content_hash, byte_length, storage_type, storage_ref, content_type, date)
        SELECT r.*
        FROM {remote}.outputs r
        JOIN {remote}.attempts a ON a.id = r.invocation_id
        WHERE NOT EXISTS (
            SELECT 1 FROM {cached}.outputs l WHERE l.id = r.id
        )
        {since}
        {client}
        "#,
        cached = cached_schema,
        remote = remote_schema,
        since = since_filter,
        client = client_filter,
    );

    let count = conn.execute(&sql, [])?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RemoteConfig, RemoteMode, RemoteType};
    use crate::init::initialize;
    use crate::schema::InvocationRecord;
    use crate::store::{ConnectionOptions, Store};
    use crate::Config;
    use tempfile::TempDir;

    fn setup_store_duckdb() -> (TempDir, Store) {
        let tmp = TempDir::new().unwrap();
        let config = Config::with_duckdb_mode(tmp.path());
        initialize(&config).unwrap();
        let store = Store::open(config).unwrap();
        (tmp, store)
    }

    fn create_file_remote(name: &str, path: &std::path::Path) -> RemoteConfig {
        RemoteConfig {
            name: name.to_string(),
            remote_type: RemoteType::File,
            uri: path.to_string_lossy().to_string(),
            mode: RemoteMode::ReadWrite,
            auto_attach: true,
            credential_provider: None,
        }
    }

    // ===== Date parsing tests =====

    #[test]
    fn test_parse_since_days() {
        let today = Utc::now().date_naive();
        let result = parse_since("7d").unwrap();
        assert_eq!(result, today - TimeDelta::days(7));
    }

    #[test]
    fn test_parse_since_weeks() {
        let today = Utc::now().date_naive();
        let result = parse_since("2w").unwrap();
        assert_eq!(result, today - TimeDelta::days(14));
    }

    #[test]
    fn test_parse_since_months() {
        let today = Utc::now().date_naive();
        let result = parse_since("1m").unwrap();
        assert_eq!(result, today - TimeDelta::days(30));
    }

    #[test]
    fn test_parse_since_date() {
        let result = parse_since("2024-01-15").unwrap();
        assert_eq!(result, NaiveDate::from_ymd_opt(2024, 1, 15).unwrap());
    }

    #[test]
    fn test_parse_since_invalid() {
        assert!(parse_since("invalid").is_err());
    }

    // ===== Push/Pull integration tests =====

    #[test]
    fn test_push_to_file_remote() {
        let (tmp, store) = setup_store_duckdb();

        // Write some local data
        let inv = InvocationRecord::new(
            "test-session",
            "echo hello",
            "/home/user",
            0,
            "test@client",
        );
        store.write_invocation(&inv).unwrap();

        // Create a file remote
        let remote_path = tmp.path().join("remote.duckdb");
        let remote = create_file_remote("test", &remote_path);

        // Push to remote
        let stats = store.push(&remote, PushOptions::default()).unwrap();

        assert_eq!(stats.invocations, 1);
        assert!(remote_path.exists(), "Remote database file should be created");
    }

    #[test]
    fn test_push_is_idempotent() {
        let (tmp, store) = setup_store_duckdb();

        // Write local data
        let inv = InvocationRecord::new(
            "test-session",
            "echo hello",
            "/home/user",
            0,
            "test@client",
        );
        store.write_invocation(&inv).unwrap();

        // Create remote
        let remote_path = tmp.path().join("remote.duckdb");
        let remote = create_file_remote("test", &remote_path);

        // Push twice
        let stats1 = store.push(&remote, PushOptions::default()).unwrap();
        let stats2 = store.push(&remote, PushOptions::default()).unwrap();

        // First push should transfer data, second should be idempotent
        assert_eq!(stats1.invocations, 1);
        assert_eq!(stats2.invocations, 0, "Second push should be idempotent");
    }

    #[test]
    fn test_push_dry_run() {
        let (tmp, store) = setup_store_duckdb();

        // Write local data
        let inv = InvocationRecord::new(
            "test-session",
            "echo hello",
            "/home/user",
            0,
            "test@client",
        );
        store.write_invocation(&inv).unwrap();

        // Create remote
        let remote_path = tmp.path().join("remote.duckdb");
        let remote = create_file_remote("test", &remote_path);

        // Dry run push
        let dry_stats = store
            .push(
                &remote,
                PushOptions {
                    dry_run: true,
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(dry_stats.invocations, 1, "Dry run should count invocations");

        // Actual push should still transfer data (dry run didn't modify)
        let actual_stats = store.push(&remote, PushOptions::default()).unwrap();
        assert_eq!(actual_stats.invocations, 1);
    }

    #[test]
    fn test_pull_from_file_remote() {
        let (tmp, store) = setup_store_duckdb();

        // Write local data and push to remote
        let inv = InvocationRecord::new(
            "test-session",
            "echo hello",
            "/home/user",
            0,
            "test@client",
        );
        store.write_invocation(&inv).unwrap();

        let remote_path = tmp.path().join("remote.duckdb");
        let remote = create_file_remote("test", &remote_path);
        store.push(&remote, PushOptions::default()).unwrap();

        // Clear local data (simulate different client)
        let conn = store.connection().unwrap();
        conn.execute("DELETE FROM local.outcomes; DELETE FROM local.attempts", []).unwrap();
        drop(conn);

        // Pull from remote
        let stats = store.pull(&remote, PullOptions::default()).unwrap();

        assert_eq!(stats.invocations, 1, "Should pull the invocation back");

        // Verify data in cached schema
        let conn = store.connection().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM caches.invocations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "Data should be in cached schema");
    }

    #[test]
    fn test_pull_is_idempotent() {
        let (tmp, store) = setup_store_duckdb();

        // Setup: write, push, clear local
        let inv = InvocationRecord::new(
            "test-session",
            "echo hello",
            "/home/user",
            0,
            "test@client",
        );
        store.write_invocation(&inv).unwrap();

        let remote_path = tmp.path().join("remote.duckdb");
        let remote = create_file_remote("test", &remote_path);
        store.push(&remote, PushOptions::default()).unwrap();

        // Pull twice
        let stats1 = store.pull(&remote, PullOptions::default()).unwrap();
        let stats2 = store.pull(&remote, PullOptions::default()).unwrap();

        assert_eq!(stats1.invocations, 1);
        assert_eq!(stats2.invocations, 0, "Second pull should be idempotent");
    }

    // ===== Remote name handling tests =====

    #[test]
    fn test_remote_name_with_hyphen() {
        let (tmp, store) = setup_store_duckdb();

        // Write local data
        let inv = InvocationRecord::new(
            "test-session",
            "echo hello",
            "/home/user",
            0,
            "test@client",
        );
        store.write_invocation(&inv).unwrap();

        // Create remote with hyphen in name
        let remote_path = tmp.path().join("my-team-remote.duckdb");
        let remote = create_file_remote("my-team", &remote_path);

        // Push should work despite hyphen
        let stats = store.push(&remote, PushOptions::default()).unwrap();
        assert_eq!(stats.invocations, 1);

        // Pull should also work (hyphen in name handled correctly)
        let pull_stats = store.pull(&remote, PullOptions::default()).unwrap();
        // Pull brings data from remote into cached_my_team schema
        assert_eq!(pull_stats.invocations, 1);
    }

    #[test]
    fn test_remote_name_with_dots() {
        let (tmp, store) = setup_store_duckdb();

        let inv = InvocationRecord::new(
            "test-session",
            "echo hello",
            "/home/user",
            0,
            "test@client",
        );
        store.write_invocation(&inv).unwrap();

        // Remote with dots in name
        let remote_path = tmp.path().join("team.v2.duckdb");
        let remote = create_file_remote("team.v2", &remote_path);

        let stats = store.push(&remote, PushOptions::default()).unwrap();
        assert_eq!(stats.invocations, 1);
    }

    // ===== Connection options tests =====

    #[test]
    fn test_connection_minimal_vs_full() {
        let (_tmp, store) = setup_store_duckdb();

        // Minimal connection should work
        let conn_minimal = store.connect(ConnectionOptions::minimal()).unwrap();
        let count: i64 = conn_minimal
            .query_row("SELECT COUNT(*) FROM local.invocations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
        drop(conn_minimal);

        // Full connection should also work
        let conn_full = store.connect(ConnectionOptions::full()).unwrap();
        let count: i64 = conn_full
            .query_row("SELECT COUNT(*) FROM main.invocations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_multiple_sequential_connections() {
        let (_tmp, store) = setup_store_duckdb();

        // Open and close multiple connections sequentially
        // This tests for database corruption issues
        for i in 0..5 {
            let conn = store.connection().unwrap();
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM main.invocations", [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 0, "Connection {} should work", i);
            drop(conn);
        }

        // Write some data
        let inv = InvocationRecord::new(
            "test-session",
            "echo hello",
            "/home/user",
            0,
            "test@client",
        );
        store.write_invocation(&inv).unwrap();

        // More connections should still work and see the data
        for i in 0..3 {
            let conn = store.connection().unwrap();
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM main.invocations", [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, 1, "Connection {} should see the data", i);
            drop(conn);
        }
    }

    // ===== Cached schema tests =====

    #[test]
    fn test_caches_schema_views_work() {
        let (tmp, store) = setup_store_duckdb();

        // Initially caches should be empty
        let conn = store.connection().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM caches.invocations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
        drop(conn);

        // Write and push data
        let inv = InvocationRecord::new(
            "test-session",
            "echo hello",
            "/home/user",
            0,
            "test@client",
        );
        store.write_invocation(&inv).unwrap();

        let remote_path = tmp.path().join("remote.duckdb");
        let remote = create_file_remote("test", &remote_path);
        store.push(&remote, PushOptions::default()).unwrap();

        // Pull creates cached_test schema
        store.pull(&remote, PullOptions::default()).unwrap();

        // Rebuild caches views to include cached_test
        let conn = store.connection().unwrap();
        store.rebuild_caches_schema(&conn).unwrap();

        // caches.invocations should now have data
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM caches.invocations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "caches should include pulled data after rebuild");
    }

    #[test]
    fn test_main_schema_unions_local_and_caches() {
        let (tmp, store) = setup_store_duckdb();

        // Write local data
        let inv1 = InvocationRecord::new(
            "test-session",
            "local command",
            "/home/user",
            0,
            "local@client",
        );
        store.write_invocation(&inv1).unwrap();

        // Push to remote, then pull (simulating another client's data)
        let remote_path = tmp.path().join("remote.duckdb");
        let remote = create_file_remote("team", &remote_path);

        // Push local data to remote
        store.push(&remote, PushOptions::default()).unwrap();

        // Delete local, pull from remote to create cached data
        let conn = store.connection().unwrap();
        conn.execute("DELETE FROM local.outcomes; DELETE FROM local.attempts", []).unwrap();
        drop(conn);

        store.pull(&remote, PullOptions::default()).unwrap();

        // Rebuild caches schema
        let conn = store.connection().unwrap();
        store.rebuild_caches_schema(&conn).unwrap();

        // Write new local data
        let inv2 = InvocationRecord::new(
            "test-session-2",
            "new local command",
            "/home/user",
            0,
            "local@client",
        );
        drop(conn);
        store.write_invocation(&inv2).unwrap();

        // main.invocations should have both local and cached
        let conn = store.connection().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM main.invocations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2, "main should union local + caches");
    }

    // ===== Heterogeneous Storage Mode Tests =====
    // Test querying across different storage modes (parquet and duckdb)

    fn setup_store_parquet() -> (TempDir, Store) {
        let tmp = TempDir::new().unwrap();
        let config = Config::with_root(tmp.path()); // Parquet is the default
        initialize(&config).unwrap();
        let store = Store::open(config).unwrap();
        (tmp, store)
    }

    #[test]
    fn test_heterogeneous_parquet_local_duckdb_remote() {
        // Local store uses parquet mode
        let (_local_tmp, local_store) = setup_store_parquet();

        // Remote store uses duckdb mode
        let remote_tmp = TempDir::new().unwrap();
        let remote_config = Config::with_duckdb_mode(remote_tmp.path());
        initialize(&remote_config).unwrap();
        let remote_store = Store::open(remote_config).unwrap();

        // Write data to remote (DuckDB mode - stored in local.invocations table)
        let remote_inv = InvocationRecord::new(
            "remote-session",
            "remote command",
            "/home/remote",
            0,
            "remote@client",
        );
        remote_store.write_invocation(&remote_inv).unwrap();

        // Verify remote has data
        let remote_conn = remote_store.connection().unwrap();
        let remote_count: i64 = remote_conn
            .query_row("SELECT COUNT(*) FROM local.invocations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remote_count, 1, "Remote should have data in local schema");
        drop(remote_conn);

        // Write data to local (Parquet mode - stored in parquet files)
        let local_inv = InvocationRecord::new(
            "local-session",
            "local command",
            "/home/local",
            0,
            "local@client",
        );
        local_store.write_invocation(&local_inv).unwrap();

        // Configure local to attach the remote (read-only)
        let remote_db_path = remote_tmp.path().join("db/bird.duckdb");
        let remote_config = RemoteConfig {
            name: "duckdb-store".to_string(),
            remote_type: RemoteType::File,
            uri: format!("file://{}", remote_db_path.display()),
            mode: RemoteMode::ReadOnly,
            auto_attach: true,
            credential_provider: None,
        };

        // Manually attach the remote to test heterogeneous querying
        let conn = local_store.connection_with_options(false).unwrap();
        local_store.attach_remote(&conn, &remote_config).unwrap();

        // Create remote macros (this tests detect_remote_table_path)
        // The remote is a DuckDB-mode BIRD database, so tables are in local schema
        let schema = remote_config.quoted_schema_name();
        let table_prefix = local_store.detect_remote_table_path(&conn, &schema);
        assert_eq!(table_prefix, "local.", "Should detect DuckDB mode remote has local. prefix");

        // Query the remote directly
        let remote_count: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM {}.local.invocations", schema),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(remote_count, 1, "Should be able to query DuckDB remote from Parquet local");
    }

    #[test]
    fn test_heterogeneous_duckdb_local_parquet_remote() {
        // Local store uses duckdb mode
        let (_local_tmp, local_store) = setup_store_duckdb();

        // Remote store uses parquet mode
        let remote_tmp = TempDir::new().unwrap();
        let remote_config = Config::with_root(remote_tmp.path());
        initialize(&remote_config).unwrap();
        let remote_store = Store::open(remote_config).unwrap();

        // Write data to remote (Parquet mode - stored in parquet files)
        let remote_inv = InvocationRecord::new(
            "remote-session",
            "remote command",
            "/home/remote",
            0,
            "remote@client",
        );
        remote_store.write_invocation(&remote_inv).unwrap();

        // Write data to local (DuckDB mode - stored in local.invocations table)
        let local_inv = InvocationRecord::new(
            "local-session",
            "local command",
            "/home/local",
            0,
            "local@client",
        );
        local_store.write_invocation(&local_inv).unwrap();

        // Configure local to attach the remote (read-only)
        let remote_db_path = remote_tmp.path().join("db/bird.duckdb");
        let remote_config = RemoteConfig {
            name: "parquet-store".to_string(),
            remote_type: RemoteType::File,
            uri: format!("file://{}", remote_db_path.display()),
            mode: RemoteMode::ReadOnly,
            auto_attach: true,
            credential_provider: None,
        };

        // Manually attach the remote (this should also set file_search_path)
        let conn = local_store.connection_with_options(false).unwrap();
        local_store.attach_remote(&conn, &remote_config).unwrap();

        // Verify detection works - both modes have local.invocations
        let schema = remote_config.quoted_schema_name();
        let table_prefix = local_store.detect_remote_table_path(&conn, &schema);
        assert_eq!(table_prefix, "local.", "BIRD databases have local schema in both modes");

        // Query the remote directly (parquet mode views should resolve via file_search_path)
        let remote_count: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM {}.local.invocations", schema),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(remote_count, 1, "Should be able to query Parquet remote from DuckDB local");
    }

    #[test]
    fn test_heterogeneous_unified_views() {
        // This tests the full heterogeneous setup with unified views
        let (local_tmp, local_store) = setup_store_parquet();

        // Create a DuckDB-mode remote
        let remote_tmp = TempDir::new().unwrap();
        let remote_config = Config::with_duckdb_mode(remote_tmp.path());
        initialize(&remote_config).unwrap();
        let remote_store = Store::open(remote_config).unwrap();

        // Write unique data to remote
        let remote_inv = InvocationRecord::new(
            "remote-session",
            "remote-specific-cmd",
            "/home/remote",
            42,
            "remote@client",
        );
        remote_store.write_invocation(&remote_inv).unwrap();

        // Write unique data to local
        let local_inv = InvocationRecord::new(
            "local-session",
            "local-specific-cmd",
            "/home/local",
            0,
            "local@client",
        );
        local_store.write_invocation(&local_inv).unwrap();

        // Create config with remote
        let remote_db_path = remote_tmp.path().join("db/bird.duckdb");
        let mut config = Config::with_root(local_tmp.path());
        config.remotes.push(RemoteConfig {
            name: "heterogeneous-test".to_string(),
            remote_type: RemoteType::File,
            uri: format!("file://{}", remote_db_path.display()),
            mode: RemoteMode::ReadOnly,
            auto_attach: true,
            credential_provider: None,
        });

        // Open store with remote config
        let store = Store::open(config).unwrap();

        // Connection with auto-attach should set up unified views
        let conn = store.connection().unwrap();

        // Query local data
        let local_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM local.invocations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(local_count, 1, "Local should have 1 record");

        // Query unified view - should include both local and remote
        let unified_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM unified.invocations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(unified_count, 2, "Unified view should have local + remote records");

        // Verify we can see both commands
        let cmds: Vec<String> = conn
            .prepare("SELECT cmd FROM unified.invocations ORDER BY cmd")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(cmds.len(), 2);
        assert!(cmds.contains(&"local-specific-cmd".to_string()));
        assert!(cmds.contains(&"remote-specific-cmd".to_string()));
    }

    #[test]
    fn test_detect_remote_table_path_standalone_db() {
        // Test detection of standalone databases (not BIRD, no local schema)
        let (_tmp, store) = setup_store_duckdb();

        // Create a standalone database (not a BIRD database)
        let standalone_tmp = TempDir::new().unwrap();
        let standalone_db_path = standalone_tmp.path().join("standalone.duckdb");
        {
            let conn = duckdb::Connection::open(&standalone_db_path).unwrap();
            conn.execute(
                "CREATE TABLE invocations (id UUID, cmd VARCHAR)",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO invocations VALUES (gen_random_uuid(), 'test')",
                [],
            )
            .unwrap();
        }

        // Attach as remote
        let remote = RemoteConfig {
            name: "standalone".to_string(),
            remote_type: RemoteType::File,
            uri: format!("file://{}", standalone_db_path.display()),
            mode: RemoteMode::ReadOnly,
            auto_attach: true,
            credential_provider: None,
        };

        let conn = store.connection_with_options(false).unwrap();
        store.attach_remote(&conn, &remote).unwrap();

        // Detect table path (should be empty - no local schema)
        let schema = remote.quoted_schema_name();
        let table_prefix = store.detect_remote_table_path(&conn, &schema);
        assert_eq!(table_prefix, "", "Standalone DB should have no prefix");

        // Query should work
        let count: i64 = conn
            .query_row(
                &format!("SELECT COUNT(*) FROM {}.invocations", schema),
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_push_to_readonly_remote_fails() {
        let (_tmp, store) = setup_store_duckdb();

        // Write local data
        let inv = InvocationRecord::new(
            "test-session",
            "echo hello",
            "/home/user",
            0,
            "test@client",
        );
        store.write_invocation(&inv).unwrap();

        // Create a read-only remote
        let remote_tmp = TempDir::new().unwrap();
        let remote_path = remote_tmp.path().join("remote.duckdb");
        let remote = RemoteConfig {
            name: "readonly".to_string(),
            remote_type: RemoteType::File,
            uri: format!("file://{}", remote_path.display()),
            mode: RemoteMode::ReadOnly,
            auto_attach: true,
            credential_provider: None,
        };

        // Push to read-only should fail
        let result = store.push(&remote, PushOptions::default());
        assert!(result.is_err(), "Push to read-only remote should fail");
        assert!(
            result.unwrap_err().to_string().contains("Cannot push to read-only"),
            "Error should mention read-only"
        );
    }

    #[test]
    fn test_push_to_readonly_remote_dry_run_returns_empty() {
        let (_tmp, store) = setup_store_duckdb();

        // Write local data
        let inv = InvocationRecord::new(
            "test-session",
            "echo hello",
            "/home/user",
            0,
            "test@client",
        );
        store.write_invocation(&inv).unwrap();

        // Create a read-only remote
        let remote_tmp = TempDir::new().unwrap();
        let remote_path = remote_tmp.path().join("remote.duckdb");
        let remote = RemoteConfig {
            name: "readonly".to_string(),
            remote_type: RemoteType::File,
            uri: format!("file://{}", remote_path.display()),
            mode: RemoteMode::ReadOnly,
            auto_attach: true,
            credential_provider: None,
        };

        // Dry run on read-only should return empty stats (nothing to push)
        let stats = store.push(&remote, PushOptions { dry_run: true, ..Default::default() }).unwrap();
        assert_eq!(stats.invocations, 0);
        assert_eq!(stats.sessions, 0);
    }
}
