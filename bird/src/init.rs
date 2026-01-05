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

    Ok(())
}

/// Create the BIRD directory structure.
fn create_directories(config: &Config) -> Result<()> {
    let dirs = [
        config.bird_root.join("db"),
        config.recent_dir().join("invocations"),
        config.recent_dir().join("outputs"),
        config.recent_dir().join("sessions"),
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

    Ok(())
}

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
