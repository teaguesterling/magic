//! Atomic file write operations.
//!
//! Provides crash-safe and race-safe file writing by using
//! temp files and atomic renames.

use std::fs;
use std::io;
use std::path::Path;

/// Generate a temp path for atomic writes.
/// Format: {dir}/.tmp.{random}.{filename}
pub fn temp_path(final_path: &Path) -> std::path::PathBuf {
    let filename = final_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    let random: u64 = rand::random();
    let temp_name = format!(".tmp.{:016x}.{}", random, filename);
    final_path.with_file_name(temp_name)
}

/// Atomically rename temp file to final path.
/// Returns Ok(true) if renamed, Ok(false) if file already existed (dedup hit).
pub fn rename_into_place(temp_path: &Path, final_path: &Path) -> io::Result<bool> {
    match fs::rename(temp_path, final_path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            // Another process wrote the same file - that's fine for content-addressed storage
            fs::remove_file(temp_path)?;
            Ok(false)
        }
        Err(e) => {
            // Clean up temp file on other errors
            let _ = fs::remove_file(temp_path);
            Err(e)
        }
    }
}

/// Write content to file atomically.
/// Returns Ok(true) if written, Ok(false) if file already existed.
pub fn write_file(final_path: &Path, content: &[u8]) -> io::Result<bool> {
    let temp = temp_path(final_path);
    fs::write(&temp, content)?;
    rename_into_place(&temp, final_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_temp_path() {
        let final_path = Path::new("/tmp/test/data.parquet");
        let temp = temp_path(final_path);

        // Temp path should be in same directory
        assert_eq!(temp.parent(), final_path.parent());

        // Temp path should have .tmp prefix
        let filename = temp.file_name().unwrap().to_str().unwrap();
        assert!(filename.starts_with(".tmp."));
        assert!(filename.ends_with(".data.parquet"));
    }

    #[test]
    fn test_write_file() {
        let tmp = TempDir::new().unwrap();
        let final_path = tmp.path().join("test.bin");

        // First write should succeed and return true
        let result = write_file(&final_path, b"hello");
        assert!(result.is_ok());
        assert!(result.unwrap()); // true = wrote new file

        // File should exist with correct content
        assert!(final_path.exists());
        assert_eq!(fs::read(&final_path).unwrap(), b"hello");

        // No temp files should remain
        let temps: Vec<_> = fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_str().unwrap_or("").starts_with(".tmp."))
            .collect();
        assert!(temps.is_empty(), "No temp files should remain");
    }

    #[test]
    fn test_write_existing_file() {
        let tmp = TempDir::new().unwrap();
        let final_path = tmp.path().join("test.bin");

        // Write initial file
        fs::write(&final_path, b"original").unwrap();

        // Atomic write to existing path should still succeed (overwrites)
        let result = write_file(&final_path, b"new content");
        assert!(result.is_ok());

        // Content should be updated
        assert_eq!(fs::read(&final_path).unwrap(), b"new content");
    }
}
