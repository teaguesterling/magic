//! Configuration for BIRD.
//!
//! BIRD_ROOT resolution order:
//! 1. Explicit path passed to Config::new()
//! 2. BIRD_ROOT environment variable
//! 3. Default: ~/.local/share/bird

use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

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
