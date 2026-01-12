//! Configuration for BIRD.
//!
//! BIRD_ROOT resolution order:
//! 1. Explicit path passed to Config::new()
//! 2. BIRD_ROOT environment variable
//! 3. Default: ~/.local/share/bird

use std::path::{Path, PathBuf};
use std::str::FromStr;

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Storage mode for BIRD data.
///
/// - **Parquet**: Multi-writer safe using atomic file creation. Suitable for
///   concurrent shell hooks (shq). Requires periodic compaction.
/// - **DuckDB**: Single-writer using direct table inserts. Simpler but requires
///   serialized writes. Suitable for sequential CLI tools (blq).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageMode {
    /// Write to Parquet files (multi-writer safe, requires compaction)
    #[default]
    Parquet,
    /// Write directly to DuckDB tables (single-writer, no compaction needed)
    DuckDB,
}

impl std::fmt::Display for StorageMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StorageMode::Parquet => write!(f, "parquet"),
            StorageMode::DuckDB => write!(f, "duckdb"),
        }
    }
}

impl FromStr for StorageMode {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "parquet" => Ok(StorageMode::Parquet),
            "duckdb" => Ok(StorageMode::DuckDB),
            _ => Err(Error::Config(format!(
                "Invalid storage mode '{}': expected 'parquet' or 'duckdb'",
                s
            ))),
        }
    }
}

/// Type of remote storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RemoteType {
    /// S3-compatible object storage (s3://, gs://)
    S3,
    /// MotherDuck cloud database (md:)
    MotherDuck,
    /// PostgreSQL database
    Postgres,
    /// Local or network file path
    File,
}

impl std::fmt::Display for RemoteType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RemoteType::S3 => write!(f, "s3"),
            RemoteType::MotherDuck => write!(f, "motherduck"),
            RemoteType::Postgres => write!(f, "postgres"),
            RemoteType::File => write!(f, "file"),
        }
    }
}

impl FromStr for RemoteType {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "s3" | "gcs" => Ok(RemoteType::S3),
            "motherduck" | "md" => Ok(RemoteType::MotherDuck),
            "postgres" | "postgresql" | "pg" => Ok(RemoteType::Postgres),
            "file" | "local" => Ok(RemoteType::File),
            _ => Err(Error::Config(format!(
                "Invalid remote type '{}': expected 's3', 'motherduck', 'postgres', or 'file'",
                s
            ))),
        }
    }
}

/// Access mode for remote storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteMode {
    /// Read and write access
    #[default]
    ReadWrite,
    /// Read-only access
    ReadOnly,
}

impl std::fmt::Display for RemoteMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RemoteMode::ReadWrite => write!(f, "read_write"),
            RemoteMode::ReadOnly => write!(f, "read_only"),
        }
    }
}

/// Configuration for a remote storage location.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteConfig {
    /// Remote name (used as schema name: remote_{name})
    pub name: String,

    /// Type of remote storage
    #[serde(rename = "type")]
    pub remote_type: RemoteType,

    /// URI for the remote (e.g., s3://bucket/path/bird.duckdb, md:database_name)
    pub uri: String,

    /// Access mode (read_write or read_only)
    #[serde(default)]
    pub mode: RemoteMode,

    /// Credential provider for S3 (e.g., "credential_chain", "config")
    #[serde(default)]
    pub credential_provider: Option<String>,

    /// Whether to auto-attach on connection open
    #[serde(default = "default_true")]
    pub auto_attach: bool,
}

fn default_true() -> bool {
    true
}

impl RemoteConfig {
    /// Create a new remote config.
    pub fn new(name: impl Into<String>, remote_type: RemoteType, uri: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            remote_type,
            uri: uri.into(),
            mode: RemoteMode::default(),
            credential_provider: None,
            auto_attach: true,
        }
    }

    /// Set read-only mode.
    pub fn read_only(mut self) -> Self {
        self.mode = RemoteMode::ReadOnly;
        self
    }

    /// Get the DuckDB schema name for this remote.
    pub fn schema_name(&self) -> String {
        format!("remote_{}", self.name)
    }

    /// Get the quoted DuckDB schema name for this remote (for use in SQL).
    pub fn quoted_schema_name(&self) -> String {
        format!("\"remote_{}\"", self.name)
    }

    /// Generate the ATTACH SQL statement for this remote.
    pub fn attach_sql(&self) -> String {
        let mode_clause = match self.mode {
            RemoteMode::ReadOnly => " (READ_ONLY)",
            RemoteMode::ReadWrite => "",
        };

        let type_clause = match self.remote_type {
            RemoteType::Postgres => " (TYPE postgres)",
            _ => "",
        };

        format!(
            "ATTACH '{}' AS {}{}{}",
            self.uri,
            self.quoted_schema_name(),
            type_clause,
            mode_clause
        )
    }

    /// Get the base URL for blob storage (for S3/GCS remotes).
    pub fn blob_base_url(&self) -> Option<String> {
        match self.remote_type {
            RemoteType::S3 => {
                // Extract bucket/prefix from URI, append /blobs
                // e.g., s3://bucket/path/bird.duckdb -> s3://bucket/path/blobs
                if let Some(stripped) = self.uri.strip_suffix(".duckdb") {
                    Some(format!("{}/blobs", stripped))
                } else {
                    Some(format!("{}/blobs", self.uri.trim_end_matches('/')))
                }
            }
            _ => None,
        }
    }
}

/// Sync configuration for push/pull operations.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SyncConfig {
    /// Default remote for push/pull operations.
    #[serde(default)]
    pub default_remote: Option<String>,

    /// Push data after compact operations.
    #[serde(default)]
    pub push_on_compact: bool,

    /// Push data before archive operations.
    #[serde(default)]
    pub push_on_archive: bool,

    /// Sync invocations table.
    #[serde(default = "default_true")]
    pub sync_invocations: bool,

    /// Sync outputs table.
    #[serde(default = "default_true")]
    pub sync_outputs: bool,

    /// Sync events table.
    #[serde(default = "default_true")]
    pub sync_events: bool,

    /// Sync blob content files.
    #[serde(default)]
    pub sync_blobs: bool,

    /// Minimum blob size to sync (bytes). Smaller blobs stay inline.
    #[serde(default = "default_blob_sync_min")]
    pub blob_sync_min_bytes: usize,
}

fn default_blob_sync_min() -> usize {
    1024 // 1KB
}

/// BIRD configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Root directory for all BIRD data.
    pub bird_root: PathBuf,

    /// Client identifier for this machine.
    #[serde(default = "default_client_id")]
    pub client_id: String,

    /// Days to keep data in hot tier before archiving.
    #[serde(default = "default_hot_days")]
    pub hot_days: u32,

    /// Threshold in bytes for inline vs blob storage.
    #[serde(default = "default_inline_threshold")]
    pub inline_threshold: usize,

    /// Automatically extract events after `shq run` commands.
    #[serde(default)]
    pub auto_extract: bool,

    /// Storage mode for writing data.
    /// - parquet: Multi-writer safe, requires compaction (default)
    /// - duckdb: Single-writer, no compaction needed
    #[serde(default)]
    pub storage_mode: StorageMode,

    /// Remote storage configurations.
    #[serde(default)]
    pub remotes: Vec<RemoteConfig>,

    /// Sync configuration for push/pull operations.
    #[serde(default)]
    pub sync: SyncConfig,
}

fn default_client_id() -> String {
    // Deterministic: username@hostname
    let username = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "unknown".to_string());
    let hostname = gethostname::gethostname()
        .to_string_lossy()
        .to_string();
    format!("{}@{}", username, hostname)
}

fn default_hot_days() -> u32 {
    14
}

fn default_inline_threshold() -> usize {
    4_096 // 4KB - small for easy testing of blob storage
}

impl Config {
    /// Create a new config with the given BIRD_ROOT.
    pub fn with_root(bird_root: impl Into<PathBuf>) -> Self {
        Self {
            bird_root: bird_root.into(),
            client_id: default_client_id(),
            hot_days: default_hot_days(),
            inline_threshold: default_inline_threshold(),
            auto_extract: false,
            storage_mode: StorageMode::default(),
            remotes: Vec::new(),
            sync: SyncConfig::default(),
        }
    }

    /// Create a new config with DuckDB storage mode.
    pub fn with_duckdb_mode(bird_root: impl Into<PathBuf>) -> Self {
        Self {
            bird_root: bird_root.into(),
            client_id: default_client_id(),
            hot_days: default_hot_days(),
            inline_threshold: default_inline_threshold(),
            auto_extract: false,
            storage_mode: StorageMode::DuckDB,
            remotes: Vec::new(),
            sync: SyncConfig::default(),
        }
    }

    /// Create a config using default BIRD_ROOT resolution.
    pub fn default_location() -> Result<Self> {
        let bird_root = resolve_bird_root()?;
        Ok(Self::with_root(bird_root))
    }

    /// Load config from BIRD_ROOT/config.toml, or create default.
    pub fn load() -> Result<Self> {
        let bird_root = resolve_bird_root()?;
        Self::load_from(&bird_root)
    }

    /// Load config from a specific BIRD_ROOT.
    pub fn load_from(bird_root: &Path) -> Result<Self> {
        let config_path = bird_root.join("config.toml");

        if config_path.exists() {
            let contents = std::fs::read_to_string(&config_path)?;
            let mut config: Config = toml::from_str(&contents)
                .map_err(|e| Error::Config(format!("Failed to parse config: {}", e)))?;
            // Ensure bird_root matches the actual location
            config.bird_root = bird_root.to_path_buf();
            Ok(config)
        } else {
            Ok(Self::with_root(bird_root))
        }
    }

    /// Save config to BIRD_ROOT/config.toml.
    pub fn save(&self) -> Result<()> {
        let config_path = self.bird_root.join("config.toml");
        let contents = toml::to_string_pretty(self)
            .map_err(|e| Error::Config(format!("Failed to serialize config: {}", e)))?;
        std::fs::write(config_path, contents)?;
        Ok(())
    }

    // Path helpers

    /// Path to the DuckDB database file.
    pub fn db_path(&self) -> PathBuf {
        self.bird_root.join("db/bird.duckdb")
    }

    /// Path to the data directory.
    pub fn data_dir(&self) -> PathBuf {
        self.bird_root.join("db/data")
    }

    /// Path to the recent (hot) data directory.
    pub fn recent_dir(&self) -> PathBuf {
        self.data_dir().join("recent")
    }

    /// Path to the archive (cold) data directory.
    pub fn archive_dir(&self) -> PathBuf {
        self.data_dir().join("archive")
    }

    /// Path to invocations parquet files for a given date.
    pub fn invocations_dir(&self, date: &chrono::NaiveDate) -> PathBuf {
        self.recent_dir()
            .join("invocations")
            .join(format!("date={}", date))
    }

    /// Path to outputs parquet files for a given date.
    pub fn outputs_dir(&self, date: &chrono::NaiveDate) -> PathBuf {
        self.recent_dir()
            .join("outputs")
            .join(format!("date={}", date))
    }

    /// Path to sessions parquet files for a given date.
    pub fn sessions_dir(&self, date: &chrono::NaiveDate) -> PathBuf {
        self.recent_dir()
            .join("sessions")
            .join(format!("date={}", date))
    }

    /// Path to the SQL files directory.
    pub fn sql_dir(&self) -> PathBuf {
        self.bird_root.join("db/sql")
    }

    /// Path to the DuckDB extensions directory.
    pub fn extensions_dir(&self) -> PathBuf {
        self.bird_root.join("db/extensions")
    }

    /// Path to the blobs content directory.
    pub fn blobs_dir(&self) -> PathBuf {
        self.recent_dir().join("blobs/content")
    }

    /// Path to a specific blob file by hash and command.
    pub fn blob_path(&self, hash: &str, cmd_hint: &str) -> PathBuf {
        let prefix = &hash[..2.min(hash.len())];
        let sanitized_cmd = sanitize_for_filename(cmd_hint);
        self.blobs_dir()
            .join(prefix)
            .join(format!("{}--{}.bin", hash, sanitized_cmd))
    }

    /// Path to the event-formats.toml config file (legacy).
    pub fn event_formats_path(&self) -> PathBuf {
        self.bird_root.join("event-formats.toml")
    }

    /// Path to the format-hints.toml config file.
    pub fn format_hints_path(&self) -> PathBuf {
        self.bird_root.join("format-hints.toml")
    }

    /// Path to events parquet files for a given date.
    pub fn events_dir(&self, date: &chrono::NaiveDate) -> PathBuf {
        self.recent_dir()
            .join("events")
            .join(format!("date={}", date))
    }

    // Remote management helpers

    /// Get a remote by name.
    pub fn get_remote(&self, name: &str) -> Option<&RemoteConfig> {
        self.remotes.iter().find(|r| r.name == name)
    }

    /// Add a remote configuration.
    pub fn add_remote(&mut self, remote: RemoteConfig) {
        // Remove existing remote with same name
        self.remotes.retain(|r| r.name != remote.name);
        self.remotes.push(remote);
    }

    /// Remove a remote by name. Returns true if removed.
    pub fn remove_remote(&mut self, name: &str) -> bool {
        let len_before = self.remotes.len();
        self.remotes.retain(|r| r.name != name);
        self.remotes.len() < len_before
    }

    /// Get all blob roots for multi-location resolution.
    /// Returns local blobs dir first, then remote blob URLs.
    pub fn blob_roots(&self) -> Vec<String> {
        let mut roots = vec![self.blobs_dir().to_string_lossy().to_string()];

        for remote in &self.remotes {
            if let Some(blob_url) = remote.blob_base_url() {
                roots.push(blob_url);
            }
        }

        roots
    }

    /// Get remotes that should be auto-attached.
    pub fn auto_attach_remotes(&self) -> Vec<&RemoteConfig> {
        self.remotes.iter().filter(|r| r.auto_attach).collect()
    }
}

/// Sanitize a string for use in filenames (used for blob naming).
fn sanitize_for_filename(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            ' ' => '-',
            c if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' => c,
            _ => '_',
        })
        .take(32) // Shorter for blob filenames
        .collect()
}

/// Resolve BIRD_ROOT using the standard resolution order.
fn resolve_bird_root() -> Result<PathBuf> {
    // 1. Environment variable
    if let Ok(path) = std::env::var("BIRD_ROOT") {
        return Ok(PathBuf::from(path));
    }

    // 2. XDG data directory (via directories crate)
    if let Some(proj_dirs) = ProjectDirs::from("", "", "bird") {
        return Ok(proj_dirs.data_dir().to_path_buf());
    }

    // 3. Fallback to ~/.local/share/bird
    let home = std::env::var("HOME")
        .map_err(|_| Error::Config("Could not determine home directory".to_string()))?;
    Ok(PathBuf::from(home).join(".local/share/bird"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_config_with_root() {
        let config = Config::with_root("/tmp/test-bird");
        assert_eq!(config.bird_root, PathBuf::from("/tmp/test-bird"));
        assert_eq!(config.hot_days, 14);
        assert_eq!(config.inline_threshold, 4_096);
    }

    #[test]
    fn test_blob_path() {
        let config = Config::with_root("/tmp/test-bird");
        let path = config.blob_path("abcdef123456", "make test");
        assert_eq!(
            path,
            PathBuf::from("/tmp/test-bird/db/data/recent/blobs/content/ab/abcdef123456--make-test.bin")
        );
    }

    #[test]
    fn test_config_paths() {
        let config = Config::with_root("/tmp/test-bird");
        assert_eq!(config.db_path(), PathBuf::from("/tmp/test-bird/db/bird.duckdb"));
        assert_eq!(config.recent_dir(), PathBuf::from("/tmp/test-bird/db/data/recent"));
    }

    #[test]
    fn test_config_save_load() {
        let tmp = TempDir::new().unwrap();
        let bird_root = tmp.path().to_path_buf();

        // Create the directory structure
        std::fs::create_dir_all(&bird_root).unwrap();

        let config = Config::with_root(&bird_root);
        config.save().unwrap();

        let loaded = Config::load_from(&bird_root).unwrap();
        assert_eq!(loaded.hot_days, config.hot_days);
        assert_eq!(loaded.inline_threshold, config.inline_threshold);
    }
}
