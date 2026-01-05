//! BIRD initialization - creates directory structure and database.

use std::fs;

use crate::{Config, Error, Result};

/// Initialize a new BIRD installation.
///
/// Creates the directory structure and initializes the DuckDB database
/// with views for querying Parquet files.
pub fn initialize(config: &Config) -> Result<()> {
    let bird_root = &config.bird_root;

    // Check if already initialized
    if config.db_path().exists() {
        return Err(Error::AlreadyInitialized(bird_root.clone()));
    }

    // Create directory structure
    create_directories(config)?;

    // Initialize DuckDB with views
    init_database(config)?;

    // Save config
    config.save()?;

    // Create default event-formats.toml
    create_event_formats_config(config)?;

    Ok(())
}

/// Create the BIRD directory structure.
fn create_directories(config: &Config) -> Result<()> {
    let dirs = [
        config.bird_root.join("db"),
        config.recent_dir().join("invocations"),
        config.recent_dir().join("outputs"),
        config.recent_dir().join("sessions"),
        config.recent_dir().join("events"),
        config.blobs_dir(), // blobs/content
        config.archive_dir().join("blobs/content"),
        config.extensions_dir(),
        config.sql_dir(),
    ];

    for dir in &dirs {
        fs::create_dir_all(dir)?;
    }

    Ok(())
}

/// Initialize the DuckDB database with views.
fn init_database(config: &Config) -> Result<()> {
    let conn = duckdb::Connection::open(&config.db_path())?;

    // Set up custom extensions directory and install scalarfs
    install_extensions(&conn, config)?;

    // Create seed parquet files with correct schema but no rows
    // This ensures the glob pattern always matches at least one file
    create_seed_files(&conn, config)?;

    // Create blob_registry table
    create_blob_registry(&conn)?;

    // Set file search path so views use relative paths
    let data_dir = config.data_dir();
    conn.execute(
        &format!("SET file_search_path = '{}'", data_dir.display()),
        [],
    )?;

    // Create views that read from Parquet files using glob patterns
    conn.execute_batch(
        r#"
        -- Invocations view: reads all invocation parquet files
        CREATE OR REPLACE VIEW invocations AS
        SELECT * EXCLUDE (filename)
        FROM read_parquet(
            'recent/invocations/**/*.parquet',
            union_by_name = true,
            hive_partitioning = true,
            filename = true
        );

        -- Outputs view: reads all output parquet files
        CREATE OR REPLACE VIEW outputs AS
        SELECT * EXCLUDE (filename)
        FROM read_parquet(
            'recent/outputs/**/*.parquet',
            union_by_name = true,
            hive_partitioning = true,
            filename = true
        );

        -- Sessions view: reads all session parquet files
        CREATE OR REPLACE VIEW sessions AS
        SELECT * EXCLUDE (filename)
        FROM read_parquet(
            'recent/sessions/**/*.parquet',
            union_by_name = true,
            hive_partitioning = true,
            filename = true
        );

        -- Recent invocations helper view
        CREATE OR REPLACE VIEW recent_invocations AS
        SELECT *
        FROM invocations
        WHERE date >= CURRENT_DATE - INTERVAL '7 days'
        ORDER BY timestamp DESC;

        -- Invocations today helper view
        CREATE OR REPLACE VIEW invocations_today AS
        SELECT *
        FROM invocations
        WHERE date = CURRENT_DATE
        ORDER BY timestamp DESC;

        -- Failed invocations helper view
        CREATE OR REPLACE VIEW failed_invocations AS
        SELECT *
        FROM invocations
        WHERE exit_code != 0
        ORDER BY timestamp DESC;

        -- Invocations with outputs (joined view)
        CREATE OR REPLACE VIEW invocations_with_outputs AS
        SELECT
            i.*,
            o.id as output_id,
            o.stream,
            o.byte_length,
            o.storage_type,
            o.storage_ref
        FROM invocations i
        LEFT JOIN outputs o ON i.id = o.invocation_id;

        -- Clients view (derived from sessions)
        CREATE OR REPLACE VIEW clients AS
        SELECT
            client_id,
            MIN(registered_at) as first_seen,
            MAX(registered_at) as last_seen,
            COUNT(DISTINCT session_id) as session_count
        FROM sessions
        GROUP BY client_id;

        -- Events view: reads all event parquet files (parsed log entries)
        CREATE OR REPLACE VIEW events AS
        SELECT * EXCLUDE (filename)
        FROM read_parquet(
            'recent/events/**/*.parquet',
            union_by_name = true,
            hive_partitioning = true,
            filename = true
        );

        -- Events with invocation context (joined view)
        CREATE OR REPLACE VIEW events_with_context AS
        SELECT
            e.*,
            i.cmd,
            i.timestamp,
            i.cwd,
            i.exit_code
        FROM events e
        JOIN invocations i ON e.invocation_id = i.id;
        "#,
    )?;

    Ok(())
}

/// Install required DuckDB extensions.
fn install_extensions(conn: &duckdb::Connection, config: &Config) -> Result<()> {
    // Disable autoinstall to avoid network requests
    conn.execute("SET autoinstall_known_extensions = false", [])?;

    // Load bundled extensions (parquet, icu are bundled in DuckDB 1.0+)
    conn.execute("LOAD parquet", [])?;
    conn.execute("LOAD icu", [])?;

    // Set custom extensions directory
    conn.execute(
        &format!(
            "SET extension_directory = '{}'",
            config.extensions_dir().display()
        ),
        [],
    )?;

    // Allow community extensions
    conn.execute("SET allow_community_extensions = true", [])?;

    // Install scalarfs from community repository for data: URL support
    // This provides read_blob() support for data: URIs
    conn.execute("INSTALL scalarfs FROM community", [])?;
    conn.execute("LOAD scalarfs", [])?;

    // Install duck_hunt from community repository for log parsing
    // This provides read_duck_hunt_log() for parsing build/test output
    conn.execute("INSTALL duck_hunt FROM community", [])?;
    conn.execute("LOAD duck_hunt", [])?;

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
    // Create invocations seed
    let invocations_seed_dir = config
        .recent_dir()
        .join("invocations")
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
                NULL::INTEGER as exit_code,
                NULL::VARCHAR as format_hint,
                NULL::VARCHAR as client_id,
                NULL::VARCHAR as hostname,
                NULL::VARCHAR as username,
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
