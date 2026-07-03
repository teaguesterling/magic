//! Storage permission hardening.
//!
//! Shell history contains sensitive material, so everything BIRD stores must
//! be owner-only — at least as strict as the `~/.bash_history` (0600)
//! baseline this tool replaces:
//!
//! - the data root (`BIRD_ROOT`) is `0700`, which gates the whole tree
//!   regardless of what a library (DuckDB) creates inside it, and
//! - every file BIRD itself writes (DB, parquet, blobs, running output,
//!   `errors.log`, `config.toml`) is `0600`.
//!
//! Files written by earlier versions at a looser umask are repaired
//! best-effort every time the store is opened.

use std::fs;
use std::path::Path;

use crate::{Config, Result};

/// Owner-only directory mode.
pub const DIR_MODE: u32 = 0o700;
/// Owner-only file mode.
pub const FILE_MODE: u32 = 0o600;

/// Set a unix permission mode on a path.
#[cfg(unix)]
pub fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
pub fn set_mode(_path: &Path, _mode: u32) -> std::io::Result<()> {
    Ok(())
}

/// Best-effort owner-only chmod for a directory (errors ignored).
pub fn harden_dir(path: &Path) {
    let _ = set_mode(path, DIR_MODE);
}

/// Best-effort owner-only chmod for a file (errors ignored).
pub fn harden_file(path: &Path) {
    let _ = set_mode(path, FILE_MODE);
}

/// Path of the shell-hook error log (created by shell redirection, so it
/// inherits the shell's umask and needs repair here).
pub fn errors_log_path(config: &Config) -> std::path::PathBuf {
    config.bird_root.join("errors.log")
}

/// Create the data root if needed and enforce `0700` on it.
///
/// The root chmod is a hard requirement at initialization time: if we cannot
/// make the root owner-only we refuse to proceed, because everything under
/// it is captured command history.
pub fn ensure_secure_root(config: &Config) -> Result<()> {
    let root = &config.bird_root;
    if !root.exists() {
        fs::create_dir_all(root)?;
    }
    set_mode(root, DIR_MODE)?;
    Ok(())
}

/// Recursively harden a directory tree: directories 0700, files 0600.
///
/// Best-effort (per-entry errors ignored). Intended for small trees, e.g.
/// a freshly initialized data root where DuckDB has just created the
/// database and seed files at the process umask.
pub fn harden_tree(path: &Path) {
    if path.is_dir() {
        harden_dir(path);
        if let Ok(entries) = fs::read_dir(path) {
            for entry in entries.flatten() {
                harden_tree(&entry.path());
            }
        }
    } else {
        harden_file(path);
    }
}

/// Best-effort permission repair for an existing installation.
///
/// Called on every store open: re-hardens the root and the well-known
/// sensitive files near the top of the tree (DB, WAL, error log, config,
/// running dir). Deeper files (old parquet/blobs) are gated by the root's
/// `0700` and are re-created hardened as data churns.
pub fn repair_permissions(config: &Config) {
    let root = &config.bird_root;
    harden_dir(root);

    let db = config.db_path();
    let wal = db.with_file_name(format!(
        "{}.wal",
        db.file_name().and_then(|n| n.to_str()).unwrap_or("bird.duckdb")
    ));
    for f in [db, wal, errors_log_path(config), root.join("config.toml")] {
        if f.exists() {
            harden_file(&f);
        }
    }
    for d in [root.join("db"), config.running_dir(), config.buffer_dir()] {
        if d.exists() {
            harden_dir(&d);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::TempDir;

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn test_ensure_secure_root_creates_0700() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("bird-root");
        let config = Config::with_root(&root);

        ensure_secure_root(&config).unwrap();
        assert_eq!(mode_of(&root), 0o700);
    }

    #[test]
    fn test_repair_fixes_loose_permissions() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("bird-root");
        let config = Config::with_root(&root);
        fs::create_dir_all(root.join("db")).unwrap();
        fs::write(root.join("errors.log"), b"x").unwrap();
        fs::write(root.join("config.toml"), b"x").unwrap();
        // Simulate a legacy loose install
        set_mode(&root, 0o755).unwrap();
        set_mode(&root.join("config.toml"), 0o644).unwrap();

        repair_permissions(&config);

        assert_eq!(mode_of(&root), 0o700);
        assert_eq!(mode_of(&root.join("config.toml")), 0o600);
        assert_eq!(mode_of(&root.join("errors.log")), 0o600);
    }
}
