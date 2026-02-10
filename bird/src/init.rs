//! BIRD initialization - creates directory structure and database.
//!
//! # Schema Architecture
//!
//! BIRD uses a multi-schema architecture for flexible data organization:
//!
//! ## Data Schemas (contain actual tables)
//! - `local` - Locally generated data (tables in DuckDB mode, parquet views in parquet mode)
//! - `cached_<name>` - One per remote, contains data pulled/synced from that remote
//! - `cached_placeholder` - Empty tables (ensures `caches` views work with no cached data)
//!
//! ## Attached Schemas (live remote connections)
//! - `remote_<name>` - Attached remote databases (read-only)
//! - `remote_placeholder` - Empty tables (ensures `remotes` views work with no remotes)
//!
//! ## Union Schemas (dynamic views)
//! - `caches` - Union of all `cached_*` schemas
//! - `remotes` - Union of all `remote_*` schemas
//! - `main` - Union of `local` + `caches` (all data we own locally)
//! - `unified` - Union of `main` + `remotes` (everything)
//! - `cwd` - Views filtered to current working directory
//!
//! ## Reserved Schema Names
//! - `local`, `main`, `unified`, `cwd`, `caches`, `remotes` - Core schemas
//! - `cached_*` - Reserved prefix for cached remote data
//! - `remote_*` - Reserved prefix for attached remotes
//! - `project` - Reserved for attached project-level database

use std::fs;

use crate::config::StorageMode;
use crate::{Config, Error, Result};

/// Initialize a new BIRD installation.
///
/// Creates the directory structure and initializes the DuckDB database
/// with the schema architecture.
pub fn initialize(config: &Config) -> Result<()> {
    let bird_root = &config.bird_root;

    // Check if already initialized
    if config.db_path().exists() {
        return Err(Error::AlreadyInitialized(bird_root.clone()));
    }

    // Create directory structure
    create_directories(config)?;

    // Initialize DuckDB with schemas
    init_database(config)?;

    // Save config
    config.save()?;

    // Create default event-formats.toml
    create_event_formats_config(config)?;

    Ok(())
}

/// Create the BIRD directory structure.
fn create_directories(config: &Config) -> Result<()> {
    // Common directories for both modes
    let mut dirs = vec![
        config.bird_root.join("db"),
        config.blobs_dir(), // blobs/content
        config.archive_dir().join("blobs/content"),
        config.extensions_dir(),
        config.sql_dir(),
    ];

    // Parquet mode needs partition directories
    if config.storage_mode == StorageMode::Parquet {
        dirs.extend([
            config.recent_dir().join("invocations"),
            config.recent_dir().join("outputs"),
            config.recent_dir().join("sessions"),
            config.recent_dir().join("events"),
        ]);
    }

    for dir in &dirs {
        fs::create_dir_all(dir)?;
    }

    Ok(())
}

/// Initialize the DuckDB database with schema architecture.
fn init_database(config: &Config) -> Result<()> {
    let conn = duckdb::Connection::open(config.db_path())?;

    // Enable community extensions
    conn.execute("SET allow_community_extensions = true", [])?;

    // Install and load required extensions
    // This pre-installs to the default location so connect() is fast
    install_extensions(&conn)?;

    // Set file search path so views use relative paths
    let data_dir = config.data_dir();
    conn.execute(
        &format!("SET file_search_path = '{}'", data_dir.display()),
        [],
    )?;

    // Create core schemas
    create_core_schemas(&conn)?;

    // Create blob_registry table in main schema (used by both modes)
    create_blob_registry(&conn)?;

    // Mode-specific initialization for local schema
    match config.storage_mode {
        StorageMode::Parquet => {
            // Create seed parquet files with correct schema but no rows
            create_seed_files(&conn, config)?;
            // Create local schema with views over parquet files
            create_local_parquet_views(&conn)?;
        }
        StorageMode::DuckDB => {
            // Create local schema with tables for direct storage
            create_local_tables(&conn)?;
        }
    }

    // Create placeholder schemas (for empty unions)
    create_placeholder_schemas(&conn)?;

    // Create union schemas (caches, remotes, main, bird)
    create_union_schemas(&conn)?;

    // Create helper views in main schema
    create_helper_views(&conn)?;

    // Create cwd schema views (placeholders, rebuilt at connection time)
    create_cwd_views(&conn)?;

    Ok(())
}

/// Create core schemas used by BIRD.
fn create_core_schemas(conn: &duckdb::Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        -- Data schemas
        CREATE SCHEMA IF NOT EXISTS local;
        CREATE SCHEMA IF NOT EXISTS cached_placeholder;
        CREATE SCHEMA IF NOT EXISTS remote_placeholder;

        -- Union schemas
        CREATE SCHEMA IF NOT EXISTS caches;
        CREATE SCHEMA IF NOT EXISTS remotes;
        -- main already exists as default schema
        CREATE SCHEMA IF NOT EXISTS unified;
        CREATE SCHEMA IF NOT EXISTS cwd;
        "#,
    )?;
    Ok(())
}

/// Create placeholder schemas with empty tables.
/// These ensure union views work even when no cached/remote schemas exist.
fn create_placeholder_schemas(conn: &duckdb::Connection) -> Result<()> {
    // Cached placeholder - empty tables with correct schema
    conn.execute_batch(
        r#"
        CREATE TABLE cached_placeholder.sessions (
            session_id VARCHAR, client_id VARCHAR, invoker VARCHAR, invoker_pid INTEGER,
            invoker_type VARCHAR, registered_at TIMESTAMP, cwd VARCHAR, date DATE,
            _source VARCHAR
        );
        CREATE TABLE cached_placeholder.invocations (
            id UUID, session_id VARCHAR, timestamp TIMESTAMP, duration_ms BIGINT,
            cwd VARCHAR, cmd VARCHAR, executable VARCHAR, runner_id VARCHAR, exit_code INTEGER,
            status VARCHAR, format_hint VARCHAR, client_id VARCHAR, hostname VARCHAR,
            username VARCHAR, tag VARCHAR, date DATE, _source VARCHAR
        );
        CREATE TABLE cached_placeholder.outputs (
            id UUID, invocation_id UUID, stream VARCHAR, content_hash VARCHAR,
            byte_length BIGINT, storage_type VARCHAR, storage_ref VARCHAR,
            content_type VARCHAR, date DATE, _source VARCHAR
        );
        CREATE TABLE cached_placeholder.events (
            id UUID, invocation_id UUID, client_id VARCHAR, hostname VARCHAR,
            event_type VARCHAR, severity VARCHAR, ref_file VARCHAR, ref_line INTEGER,
            ref_column INTEGER, message VARCHAR, error_code VARCHAR, test_name VARCHAR,
            status VARCHAR, format_used VARCHAR, date DATE, _source VARCHAR
        );
        "#,
    )?;

    // Remote placeholder - same structure
    conn.execute_batch(
        r#"
        CREATE TABLE remote_placeholder.sessions (
            session_id VARCHAR, client_id VARCHAR, invoker VARCHAR, invoker_pid INTEGER,
            invoker_type VARCHAR, registered_at TIMESTAMP, cwd VARCHAR, date DATE,
            _source VARCHAR
        );
        CREATE TABLE remote_placeholder.invocations (
            id UUID, session_id VARCHAR, timestamp TIMESTAMP, duration_ms BIGINT,
            cwd VARCHAR, cmd VARCHAR, executable VARCHAR, runner_id VARCHAR, exit_code INTEGER,
            status VARCHAR, format_hint VARCHAR, client_id VARCHAR, hostname VARCHAR,
            username VARCHAR, tag VARCHAR, date DATE, _source VARCHAR
        );
        CREATE TABLE remote_placeholder.outputs (
            id UUID, invocation_id UUID, stream VARCHAR, content_hash VARCHAR,
            byte_length BIGINT, storage_type VARCHAR, storage_ref VARCHAR,
            content_type VARCHAR, date DATE, _source VARCHAR
        );
        CREATE TABLE remote_placeholder.events (
            id UUID, invocation_id UUID, client_id VARCHAR, hostname VARCHAR,
            event_type VARCHAR, severity VARCHAR, ref_file VARCHAR, ref_line INTEGER,
            ref_column INTEGER, message VARCHAR, error_code VARCHAR, test_name VARCHAR,
            status VARCHAR, format_used VARCHAR, date DATE, _source VARCHAR
        );
        "#,
    )?;

    Ok(())
}

/// Create union schemas that combine data from multiple sources.
/// Initially these just reference placeholders; they get rebuilt when remotes are added.
fn create_union_schemas(conn: &duckdb::Connection) -> Result<()> {
    // caches = union of all cached_* schemas (initially just placeholder)
    conn.execute_batch(
        r#"
        CREATE OR REPLACE VIEW caches.sessions AS SELECT * FROM cached_placeholder.sessions;
        CREATE OR REPLACE VIEW caches.invocations AS SELECT * FROM cached_placeholder.invocations;
        CREATE OR REPLACE VIEW caches.outputs AS SELECT * FROM cached_placeholder.outputs;
        CREATE OR REPLACE VIEW caches.events AS SELECT * FROM cached_placeholder.events;
        "#,
    )?;

    // remotes = union of all remote_* schemas (initially just placeholder)
    conn.execute_batch(
        r#"
        CREATE OR REPLACE VIEW remotes.sessions AS SELECT * FROM remote_placeholder.sessions;
        CREATE OR REPLACE VIEW remotes.invocations AS SELECT * FROM remote_placeholder.invocations;
        CREATE OR REPLACE VIEW remotes.outputs AS SELECT * FROM remote_placeholder.outputs;
        CREATE OR REPLACE VIEW remotes.events AS SELECT * FROM remote_placeholder.events;
        "#,
    )?;

    // main = local + caches (all data we own)
    conn.execute_batch(
        r#"
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
        "#,
    )?;

    // unified = main + remotes (everything)
    conn.execute_batch(
        r#"
        CREATE OR REPLACE VIEW unified.sessions AS
            SELECT * FROM main.sessions
            UNION ALL BY NAME SELECT * FROM remotes.sessions;
        CREATE OR REPLACE VIEW unified.invocations AS
            SELECT * FROM main.invocations
            UNION ALL BY NAME SELECT * FROM remotes.invocations;
        CREATE OR REPLACE VIEW unified.outputs AS
            SELECT * FROM main.outputs
            UNION ALL BY NAME SELECT * FROM remotes.outputs;
        CREATE OR REPLACE VIEW unified.events AS
            SELECT * FROM main.events
            UNION ALL BY NAME SELECT * FROM remotes.events;
        "#,
    )?;

    // unified.qualified_* views - deduplicated with source list
    conn.execute_batch(
        r#"
        CREATE OR REPLACE VIEW unified.qualified_sessions AS
            SELECT * EXCLUDE (_source), list(DISTINCT _source) as _sources
            FROM unified.sessions
            GROUP BY ALL;
        CREATE OR REPLACE VIEW unified.qualified_invocations AS
            SELECT * EXCLUDE (_source), list(DISTINCT _source) as _sources
            FROM unified.invocations
            GROUP BY ALL;
        CREATE OR REPLACE VIEW unified.qualified_outputs AS
            SELECT * EXCLUDE (_source), list(DISTINCT _source) as _sources
            FROM unified.outputs
            GROUP BY ALL;
        CREATE OR REPLACE VIEW unified.qualified_events AS
            SELECT * EXCLUDE (_source), list(DISTINCT _source) as _sources
            FROM unified.events
            GROUP BY ALL;
        "#,
    )?;

    Ok(())
}

/// Create local schema with views over Parquet files (for Parquet mode).
///
/// In parquet mode, local data is stored in parquet files.
/// Views in the local schema read from these files.
/// Uses `file_row_number = true` to handle empty directories gracefully.
fn create_local_parquet_views(conn: &duckdb::Connection) -> Result<()> {
    // Note: We use UNION ALL with seed files to ensure views work even when
    // main directories are empty. The seed files are in date=1970-01-01 and
    // contain no data rows, just schema.
    conn.execute_batch(
        r#"
        -- Sessions view: read from parquet files
        CREATE OR REPLACE VIEW local.sessions AS
        SELECT * EXCLUDE (filename, file_row_number)
        FROM read_parquet(
            'recent/sessions/**/*.parquet',
            union_by_name = true,
            hive_partitioning = true,
            filename = true,
            file_row_number = true
        );

        -- Invocations view: read from parquet files
        CREATE OR REPLACE VIEW local.invocations AS
        SELECT * EXCLUDE (filename, file_row_number)
        FROM read_parquet(
            'recent/invocations/**/*.parquet',
            union_by_name = true,
            hive_partitioning = true,
            filename = true,
            file_row_number = true
        );

        -- Outputs view: read from parquet files
        CREATE OR REPLACE VIEW local.outputs AS
        SELECT * EXCLUDE (filename, file_row_number)
        FROM read_parquet(
            'recent/outputs/**/*.parquet',
            union_by_name = true,
            hive_partitioning = true,
            filename = true,
            file_row_number = true
        );

        -- Events view: read from parquet files
        CREATE OR REPLACE VIEW local.events AS
        SELECT * EXCLUDE (filename, file_row_number)
        FROM read_parquet(
            'recent/events/**/*.parquet',
            union_by_name = true,
            hive_partitioning = true,
            filename = true,
            file_row_number = true
        );
        "#,
    )?;
    Ok(())
}

/// Create local schema with tables for direct storage (for DuckDB mode).
fn create_local_tables(conn: &duckdb::Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        -- Sessions table
        CREATE TABLE IF NOT EXISTS local.sessions (
            session_id VARCHAR,
            client_id VARCHAR,
            invoker VARCHAR,
            invoker_pid INTEGER,
            invoker_type VARCHAR,
            registered_at TIMESTAMP,
            cwd VARCHAR,
            date DATE
        );

        -- Invocations table
        CREATE TABLE IF NOT EXISTS local.invocations (
            id UUID,
            session_id VARCHAR,
            timestamp TIMESTAMP,
            duration_ms BIGINT,
            cwd VARCHAR,
            cmd VARCHAR,
            executable VARCHAR,
            runner_id VARCHAR,
            exit_code INTEGER,
            status VARCHAR DEFAULT 'completed',
            format_hint VARCHAR,
            client_id VARCHAR,
            hostname VARCHAR,
            username VARCHAR,
            tag VARCHAR,
            date DATE
        );

        -- Outputs table
        CREATE TABLE IF NOT EXISTS local.outputs (
            id UUID,
            invocation_id UUID,
            stream VARCHAR,
            content_hash VARCHAR,
            byte_length BIGINT,
            storage_type VARCHAR,
            storage_ref VARCHAR,
            content_type VARCHAR,
            date DATE
        );

        -- Events table
        CREATE TABLE IF NOT EXISTS local.events (
            id UUID,
            invocation_id UUID,
            client_id VARCHAR,
            hostname VARCHAR,
            event_type VARCHAR,
            severity VARCHAR,
            ref_file VARCHAR,
            ref_line INTEGER,
            ref_column INTEGER,
            message VARCHAR,
            error_code VARCHAR,
            test_name VARCHAR,
            status VARCHAR,
            format_used VARCHAR,
            date DATE
        );
        "#,
    )?;
    Ok(())
}

/// Create helper views in main schema.
fn create_helper_views(conn: &duckdb::Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        -- Recent invocations helper view
        CREATE OR REPLACE VIEW main.recent_invocations AS
        SELECT *
        FROM main.invocations
        WHERE date >= CURRENT_DATE - INTERVAL '7 days'
        ORDER BY timestamp DESC;

        -- Invocations today helper view
        CREATE OR REPLACE VIEW main.invocations_today AS
        SELECT *
        FROM main.invocations
        WHERE date = CURRENT_DATE
        ORDER BY timestamp DESC;

        -- Failed invocations helper view
        CREATE OR REPLACE VIEW main.failed_invocations AS
        SELECT *
        FROM main.invocations
        WHERE exit_code != 0
        ORDER BY timestamp DESC;

        -- Invocations with outputs (joined view)
        CREATE OR REPLACE VIEW main.invocations_with_outputs AS
        SELECT
            i.*,
            o.id as output_id,
            o.stream,
            o.byte_length,
            o.storage_type,
            o.storage_ref
        FROM main.invocations i
        LEFT JOIN main.outputs o ON i.id = o.invocation_id;

        -- Clients view (derived from sessions)
        CREATE OR REPLACE VIEW main.clients AS
        SELECT
            client_id,
            MIN(registered_at) as first_seen,
            MAX(registered_at) as last_seen,
            COUNT(DISTINCT session_id) as session_count
        FROM main.sessions
        GROUP BY client_id;

        -- Events with invocation context (joined view)
        CREATE OR REPLACE VIEW main.events_with_context AS
        SELECT
            e.*,
            i.cmd,
            i.timestamp,
            i.cwd,
            i.exit_code
        FROM main.events e
        JOIN main.invocations i ON e.invocation_id = i.id;
        "#,
    )?;
    Ok(())
}

/// Create cwd schema views filtered to current working directory.
/// These views are dynamically regenerated when the connection opens.
/// Note: Initial creation uses a placeholder; actual filtering happens at connection time.
fn create_cwd_views(conn: &duckdb::Connection) -> Result<()> {
    // cwd views filter main data to entries where cwd starts with current directory
    // The actual current directory is set via a variable at connection time
    conn.execute_batch(
        r#"
        -- Placeholder views - these get rebuilt with actual cwd at connection time
        CREATE OR REPLACE VIEW cwd.sessions AS
        SELECT * FROM main.sessions WHERE false;
        CREATE OR REPLACE VIEW cwd.invocations AS
        SELECT * FROM main.invocations WHERE false;
        CREATE OR REPLACE VIEW cwd.outputs AS
        SELECT * FROM main.outputs WHERE false;
        CREATE OR REPLACE VIEW cwd.events AS
        SELECT * FROM main.events WHERE false;
        "#,
    )?;
    Ok(())
}

/// Ensure a DuckDB extension is loaded, installing if necessary.
///
/// Attempts in order:
/// 1. LOAD (extension might already be available)
/// 2. INSTALL from default repository, then LOAD
/// 3. INSTALL FROM community, then LOAD
///
/// Includes retry logic to handle race conditions when multiple processes
/// try to install extensions concurrently.
fn ensure_extension(conn: &duckdb::Connection, name: &str) -> Result<bool> {
    // Retry up to 3 times to handle concurrent installation races
    for attempt in 0..3 {
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

        // If not the last attempt, wait a bit before retrying
        if attempt < 2 {
            std::thread::sleep(std::time::Duration::from_millis(100 * (attempt as u64 + 1)));
        }
    }

    Ok(false)
}

/// Install and load all required extensions during initialization.
/// This pre-populates the extension cache so connect() is fast.
fn install_extensions(conn: &duckdb::Connection) -> Result<()> {
    // Required extensions - fail if not available
    for name in ["parquet", "icu", "httpfs", "json"] {
        if !ensure_extension(conn, name)? {
            return Err(Error::Config(format!(
                "Required extension '{}' could not be installed",
                name
            )));
        }
    }

    // Optional community extensions - warn if not available
    for (name, desc) in [
        ("scalarfs", "data: URL support for inline blobs"),
        ("duck_hunt", "log/output parsing for event extraction"),
    ] {
        if !ensure_extension(conn, name)? {
            eprintln!("Warning: {} extension not available ({})", name, desc);
        }
    }

    Ok(())
}


/// Create the blob_registry table for tracking deduplicated blobs.
fn create_blob_registry(conn: &duckdb::Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS blob_registry (
            content_hash  VARCHAR PRIMARY KEY,  -- BLAKE3 hash
            byte_length   BIGINT NOT NULL,      -- Original uncompressed size
            ref_count     INTEGER DEFAULT 1,    -- Number of outputs referencing this blob
            first_seen    TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
            last_accessed TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
            storage_path  VARCHAR NOT NULL      -- Relative path to blob file
        );
        "#,
    )?;
    Ok(())
}

/// Create seed parquet files with correct schema but no rows.
fn create_seed_files(conn: &duckdb::Connection, config: &Config) -> Result<()> {
    // Create invocations seed (in status=completed partition)
    let invocations_seed_dir = config
        .recent_dir()
        .join("invocations")
        .join("status=completed")
        .join("date=1970-01-01");
    fs::create_dir_all(&invocations_seed_dir)?;

    let invocations_seed_path = invocations_seed_dir.join("_seed.parquet");
    conn.execute_batch(&format!(
        r#"
        COPY (
            SELECT
                NULL::UUID as id,
                NULL::VARCHAR as session_id,
                NULL::TIMESTAMP as timestamp,
                NULL::BIGINT as duration_ms,
                NULL::VARCHAR as cwd,
                NULL::VARCHAR as cmd,
                NULL::VARCHAR as executable,
                NULL::VARCHAR as runner_id,
                NULL::INTEGER as exit_code,
                NULL::VARCHAR as status,
                NULL::VARCHAR as format_hint,
                NULL::VARCHAR as client_id,
                NULL::VARCHAR as hostname,
                NULL::VARCHAR as username,
                NULL::VARCHAR as tag,
                NULL::DATE as date
            WHERE false
        ) TO '{}' (FORMAT PARQUET);
        "#,
        invocations_seed_path.display()
    ))?;

    // Create outputs seed
    let outputs_seed_dir = config.recent_dir().join("outputs").join("date=1970-01-01");
    fs::create_dir_all(&outputs_seed_dir)?;

    let outputs_seed_path = outputs_seed_dir.join("_seed.parquet");
    conn.execute_batch(&format!(
        r#"
        COPY (
            SELECT
                NULL::UUID as id,
                NULL::UUID as invocation_id,
                NULL::VARCHAR as stream,
                NULL::VARCHAR as content_hash,
                NULL::BIGINT as byte_length,
                NULL::VARCHAR as storage_type,
                NULL::VARCHAR as storage_ref,
                NULL::VARCHAR as content_type,
                NULL::DATE as date
            WHERE false
        ) TO '{}' (FORMAT PARQUET);
        "#,
        outputs_seed_path.display()
    ))?;

    // Create sessions seed
    let sessions_seed_dir = config.recent_dir().join("sessions").join("date=1970-01-01");
    fs::create_dir_all(&sessions_seed_dir)?;

    let sessions_seed_path = sessions_seed_dir.join("_seed.parquet");
    conn.execute_batch(&format!(
        r#"
        COPY (
            SELECT
                NULL::VARCHAR as session_id,
                NULL::VARCHAR as client_id,
                NULL::VARCHAR as invoker,
                NULL::INTEGER as invoker_pid,
                NULL::VARCHAR as invoker_type,
                NULL::TIMESTAMP as registered_at,
                NULL::VARCHAR as cwd,
                NULL::DATE as date
            WHERE false
        ) TO '{}' (FORMAT PARQUET);
        "#,
        sessions_seed_path.display()
    ))?;

    // Create events seed
    let events_seed_dir = config.recent_dir().join("events").join("date=1970-01-01");
    fs::create_dir_all(&events_seed_dir)?;

    let events_seed_path = events_seed_dir.join("_seed.parquet");
    conn.execute_batch(&format!(
        r#"
        COPY (
            SELECT
                NULL::UUID as id,
                NULL::UUID as invocation_id,
                NULL::VARCHAR as client_id,
                NULL::VARCHAR as hostname,
                NULL::VARCHAR as event_type,
                NULL::VARCHAR as severity,
                NULL::VARCHAR as ref_file,
                NULL::INTEGER as ref_line,
                NULL::INTEGER as ref_column,
                NULL::VARCHAR as message,
                NULL::VARCHAR as error_code,
                NULL::VARCHAR as test_name,
                NULL::VARCHAR as status,
                NULL::VARCHAR as format_used,
                NULL::DATE as date
            WHERE false
        ) TO '{}' (FORMAT PARQUET);
        "#,
        events_seed_path.display()
    ))?;

    Ok(())
}

/// Create the default event-formats.toml configuration file.
fn create_event_formats_config(config: &Config) -> Result<()> {
    let path = config.event_formats_path();
    if !path.exists() {
        fs::write(&path, DEFAULT_EVENT_FORMATS_CONFIG)?;
    }
    Ok(())
}

/// Default content for event-formats.toml.
pub const DEFAULT_EVENT_FORMATS_CONFIG: &str = r#"# Event format detection rules for duck_hunt
# Patterns are glob-matched against the command string
# First matching rule wins; use 'auto' for duck_hunt's built-in detection

# C/C++ compilers
[[rules]]
pattern = "*gcc*"
format = "gcc"

[[rules]]
pattern = "*g++*"
format = "gcc"

[[rules]]
pattern = "*clang*"
format = "gcc"

[[rules]]
pattern = "*clang++*"
format = "gcc"

# Rust
[[rules]]
pattern = "*cargo build*"
format = "cargo_build"

[[rules]]
pattern = "*cargo test*"
format = "cargo_test_json"

[[rules]]
pattern = "*cargo check*"
format = "cargo_build"

[[rules]]
pattern = "*rustc*"
format = "rustc"

# Python
[[rules]]
pattern = "*pytest*"
format = "pytest_text"

[[rules]]
pattern = "*python*-m*pytest*"
format = "pytest_text"

[[rules]]
pattern = "*mypy*"
format = "mypy"

[[rules]]
pattern = "*flake8*"
format = "flake8"

[[rules]]
pattern = "*pylint*"
format = "pylint"

# JavaScript/TypeScript
[[rules]]
pattern = "*eslint*"
format = "eslint"

[[rules]]
pattern = "*tsc*"
format = "typescript"

[[rules]]
pattern = "*jest*"
format = "jest"

# Build systems
[[rules]]
pattern = "*make*"
format = "make_error"

[[rules]]
pattern = "*cmake*"
format = "cmake"

[[rules]]
pattern = "*ninja*"
format = "ninja"

# Go
[[rules]]
pattern = "*go build*"
format = "go_build"

[[rules]]
pattern = "*go test*"
format = "go_test"

# Default: use duck_hunt's auto-detection
[default]
format = "auto"
"#;

/// Check if BIRD is initialized at the given location.
pub fn is_initialized(config: &Config) -> bool {
    config.db_path().exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_initialize_creates_structure() {
        let tmp = TempDir::new().unwrap();
        let config = Config::with_root(tmp.path());

        initialize(&config).unwrap();

        // Check directories exist
        assert!(config.db_path().exists());
        assert!(config.recent_dir().join("invocations").exists());
        assert!(config.recent_dir().join("outputs").exists());
        assert!(config.recent_dir().join("sessions").exists());
        assert!(config.blobs_dir().exists());
        assert!(config.extensions_dir().exists());
        assert!(config.sql_dir().exists());
        assert!(config.bird_root.join("config.toml").exists());
    }

    #[test]
    fn test_initialize_twice_fails() {
        let tmp = TempDir::new().unwrap();
        let config = Config::with_root(tmp.path());

        initialize(&config).unwrap();

        // Second init should fail
        let result = initialize(&config);
        assert!(matches!(result, Err(Error::AlreadyInitialized(_))));
    }

    #[test]
    fn test_is_initialized() {
        let tmp = TempDir::new().unwrap();
        let config = Config::with_root(tmp.path());

        assert!(!is_initialized(&config));
        initialize(&config).unwrap();
        assert!(is_initialized(&config));
    }
}
