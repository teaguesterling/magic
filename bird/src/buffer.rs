//! Retrospective buffer for capturing shell command output.
//!
//! The buffer allows users to retroactively save commands they didn't explicitly
//! capture with `shq run`. Shell hooks write output to the buffer, and users can
//! promote entries to permanent storage with `shq save ~N`.

use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Read};
use std::time::{Duration, SystemTime};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Config, Result, Error};

/// Metadata for a buffer entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BufferMeta {
    /// Unique identifier for this entry.
    pub id: Uuid,
    /// Command that was executed.
    pub cmd: String,
    /// Working directory when command was executed.
    pub cwd: String,
    /// Exit code (None if command is still running or was killed).
    pub exit_code: Option<i32>,
    /// Duration in milliseconds.
    pub duration_ms: Option<i64>,
    /// When the command started.
    pub started_at: DateTime<Utc>,
    /// When the command completed (None if still running).
    pub completed_at: Option<DateTime<Utc>>,
    /// Size of output in bytes.
    pub output_size: u64,
    /// Session ID for grouping.
    pub session_id: String,
}

impl BufferMeta {
    /// Create new buffer metadata.
    pub fn new(id: Uuid, cmd: &str, cwd: &str, session_id: &str) -> Self {
        Self {
            id,
            cmd: cmd.to_string(),
            cwd: cwd.to_string(),
            exit_code: None,
            duration_ms: None,
            started_at: Utc::now(),
            completed_at: None,
            output_size: 0,
            session_id: session_id.to_string(),
        }
    }

    /// Mark as completed with exit code and duration.
    pub fn complete(&mut self, exit_code: i32, duration_ms: i64, output_size: u64) {
        self.exit_code = Some(exit_code);
        self.duration_ms = Some(duration_ms);
        self.completed_at = Some(Utc::now());
        self.output_size = output_size;
    }
}

/// A buffer entry with metadata and output path.
#[derive(Debug)]
pub struct BufferEntry {
    pub meta: BufferMeta,
    pub output_path: std::path::PathBuf,
}

/// Manager for the retrospective buffer.
pub struct Buffer {
    config: Config,
}

impl Buffer {
    /// Create a new buffer manager.
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    /// Check if buffering is enabled.
    pub fn is_enabled(&self) -> bool {
        self.config.buffer.enabled
    }

    /// Initialize the buffer directory with secure permissions.
    pub fn init(&self) -> Result<()> {
        let dir = self.config.buffer_dir();
        if !dir.exists() {
            fs::create_dir_all(&dir)?;
            // Set directory permissions to 700 (owner only)
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?;
            }
        }
        Ok(())
    }

    /// Check if a command should be excluded from buffering.
    ///
    /// Combines hooks.ignore_patterns and buffer.exclude_patterns.
    pub fn should_exclude(&self, cmd: &str) -> bool {
        let cmd_lower = cmd.to_lowercase();

        // Check hooks ignore patterns
        for pattern in &self.config.hooks.ignore_patterns {
            if matches_glob_pattern(pattern, cmd) {
                return true;
            }
        }

        // Check buffer-specific exclude patterns
        for pattern in &self.config.buffer.exclude_patterns {
            if matches_glob_pattern(pattern, cmd) || matches_glob_pattern(pattern, &cmd_lower) {
                return true;
            }
        }

        false
    }

    /// Start a new buffer entry. Returns the entry ID for writing output.
    pub fn start_entry(&self, cmd: &str, cwd: &str, session_id: &str) -> Result<Uuid> {
        if !self.is_enabled() {
            return Err(Error::Config("Buffer is not enabled".to_string()));
        }

        if self.should_exclude(cmd) {
            return Err(Error::Config("Command is excluded from buffering".to_string()));
        }

        self.init()?;

        let id = Uuid::now_v7();
        let meta = BufferMeta::new(id, cmd, cwd, session_id);

        // Write initial metadata
        let meta_path = self.config.buffer_meta_path(&id);
        let file = File::create(&meta_path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&meta_path, fs::Permissions::from_mode(0o600))?;
        }
        serde_json::to_writer(BufWriter::new(file), &meta)?;

        // Create empty output file
        let output_path = self.config.buffer_output_path(&id);
        File::create(&output_path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&output_path, fs::Permissions::from_mode(0o600))?;
        }

        Ok(id)
    }

    /// Complete a buffer entry with exit code and duration.
    pub fn complete_entry(&self, id: &Uuid, exit_code: i32, duration_ms: i64) -> Result<()> {
        let meta_path = self.config.buffer_meta_path(id);
        let output_path = self.config.buffer_output_path(id);

        // Read existing metadata
        let file = File::open(&meta_path)?;
        let mut meta: BufferMeta = serde_json::from_reader(BufReader::new(file))?;

        // Get output size
        let output_size = fs::metadata(&output_path).map(|m| m.len()).unwrap_or(0);

        // Update metadata
        meta.complete(exit_code, duration_ms, output_size);

        // Write updated metadata
        let file = File::create(&meta_path)?;
        serde_json::to_writer(BufWriter::new(file), &meta)?;

        // Rotate if needed
        self.rotate()?;

        Ok(())
    }

    /// List all buffer entries, sorted by start time (most recent first).
    pub fn list_entries(&self) -> Result<Vec<BufferEntry>> {
        let dir = self.config.buffer_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }

        let mut entries = Vec::new();

        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();

            // Only process .meta files
            if path.extension().map(|e| e == "meta").unwrap_or(false) {
                if let Ok(file) = File::open(&path) {
                    if let Ok(meta) = serde_json::from_reader::<_, BufferMeta>(BufReader::new(file)) {
                        let output_path = self.config.buffer_output_path(&meta.id);
                        if output_path.exists() {
                            entries.push(BufferEntry { meta, output_path });
                        }
                    }
                }
            }
        }

        // Sort by start time, most recent first
        entries.sort_by(|a, b| b.meta.started_at.cmp(&a.meta.started_at));

        Ok(entries)
    }

    /// Get a buffer entry by position (1 = most recent).
    pub fn get_by_position(&self, position: usize) -> Result<Option<BufferEntry>> {
        let entries = self.list_entries()?;
        Ok(entries.into_iter().nth(position.saturating_sub(1)))
    }

    /// Get a buffer entry by ID.
    pub fn get_by_id(&self, id: &Uuid) -> Result<Option<BufferEntry>> {
        let meta_path = self.config.buffer_meta_path(id);
        let output_path = self.config.buffer_output_path(id);

        if !meta_path.exists() || !output_path.exists() {
            return Ok(None);
        }

        let file = File::open(&meta_path)?;
        let meta: BufferMeta = serde_json::from_reader(BufReader::new(file))?;

        Ok(Some(BufferEntry { meta, output_path }))
    }

    /// Read output from a buffer entry.
    pub fn read_output(&self, entry: &BufferEntry) -> Result<Vec<u8>> {
        let mut content = Vec::new();
        File::open(&entry.output_path)?.read_to_end(&mut content)?;
        Ok(content)
    }

    /// Delete a buffer entry.
    pub fn delete_entry(&self, id: &Uuid) -> Result<()> {
        let meta_path = self.config.buffer_meta_path(id);
        let output_path = self.config.buffer_output_path(id);

        if meta_path.exists() {
            fs::remove_file(&meta_path)?;
        }
        if output_path.exists() {
            fs::remove_file(&output_path)?;
        }

        Ok(())
    }

    /// Clear all buffer entries.
    pub fn clear(&self) -> Result<usize> {
        let entries = self.list_entries()?;
        let count = entries.len();

        for entry in entries {
            self.delete_entry(&entry.meta.id)?;
        }

        Ok(count)
    }

    /// Rotate buffer entries based on configured limits.
    pub fn rotate(&self) -> Result<usize> {
        let mut entries = self.list_entries()?;
        let mut removed = 0;

        let max_entries = self.config.buffer.max_entries;
        let max_size_bytes = self.config.buffer.max_size_mb * 1024 * 1024;
        let max_age = Duration::from_secs(self.config.buffer.max_age_hours as u64 * 3600);
        let now = SystemTime::now();

        // Calculate total size
        let mut total_size: u64 = entries.iter().map(|e| e.meta.output_size).sum();

        // Remove entries that exceed limits (oldest first, so reverse)
        entries.reverse();
        let mut keep_count = 0;

        for entry in entries {
            let should_remove =
                // Exceeded max entries
                keep_count >= max_entries ||
                // Exceeded max size
                total_size > max_size_bytes as u64 ||
                // Exceeded max age
                entry.meta.started_at.signed_duration_since(
                    DateTime::<Utc>::from(now - max_age)
                ).num_seconds() < 0;

            if should_remove {
                total_size = total_size.saturating_sub(entry.meta.output_size);
                self.delete_entry(&entry.meta.id)?;
                removed += 1;
            } else {
                keep_count += 1;
            }
        }

        Ok(removed)
    }
}

/// Simple glob pattern matching.
///
/// Supports:
/// - `*` matches any sequence of characters
/// - Case-insensitive matching for patterns with `*`
fn matches_glob_pattern(pattern: &str, text: &str) -> bool {
    if !pattern.contains('*') {
        return pattern.eq_ignore_ascii_case(text);
    }

    let pattern_lower = pattern.to_lowercase();
    let text_lower = text.to_lowercase();

    let parts: Vec<&str> = pattern_lower.split('*').collect();

    if parts.is_empty() {
        return true;
    }

    let mut pos = 0;

    // First part must match at start (unless pattern starts with *)
    if !pattern_lower.starts_with('*') {
        if !text_lower.starts_with(parts[0]) {
            return false;
        }
        pos = parts[0].len();
    }

    // Middle parts must appear in order
    for part in parts.iter().skip(if pattern_lower.starts_with('*') { 0 } else { 1 }) {
        if part.is_empty() {
            continue;
        }
        if let Some(found) = text_lower[pos..].find(part) {
            pos += found + part.len();
        } else {
            return false;
        }
    }

    // Last part must match at end (unless pattern ends with *)
    if !pattern_lower.ends_with('*') && !parts.is_empty() {
        let last = parts.last().unwrap();
        if !last.is_empty() && !text_lower.ends_with(last) {
            return false;
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_glob_pattern_exact() {
        assert!(matches_glob_pattern("exit", "exit"));
        assert!(matches_glob_pattern("exit", "EXIT"));
        assert!(!matches_glob_pattern("exit", "exit 0"));
    }

    #[test]
    fn test_glob_pattern_star_end() {
        assert!(matches_glob_pattern("shq *", "shq run"));
        assert!(matches_glob_pattern("shq *", "shq show foo"));
        assert!(!matches_glob_pattern("shq *", "blq run"));
    }

    #[test]
    fn test_glob_pattern_star_middle() {
        assert!(matches_glob_pattern("*password*", "echo password123"));
        assert!(matches_glob_pattern("*password*", "PASSWORD_FILE"));
        assert!(matches_glob_pattern("*password*", "my_password_var"));
        assert!(!matches_glob_pattern("*password*", "echo hello"));
    }

    #[test]
    fn test_glob_pattern_star_start() {
        assert!(matches_glob_pattern("*token", "my_token"));
        assert!(matches_glob_pattern("*token", "API_TOKEN"));
        assert!(!matches_glob_pattern("*token", "token_var"));
    }

    #[test]
    fn test_glob_pattern_case_insensitive() {
        assert!(matches_glob_pattern("*SECRET*", "my_secret_key"));
        assert!(matches_glob_pattern("*secret*", "MY_SECRET_KEY"));
    }

    #[test]
    fn test_buffer_exclude_patterns() {
        let config = Config::with_root("/tmp/test");
        let buffer = Buffer::new(config);

        // Should exclude sensitive commands
        assert!(buffer.should_exclude("ssh user@host"));
        assert!(buffer.should_exclude("gpg --decrypt file"));
        assert!(buffer.should_exclude("export API_TOKEN=xxx"));
        assert!(buffer.should_exclude("printenv"));

        // Should not exclude normal commands
        assert!(!buffer.should_exclude("cargo build"));
        assert!(!buffer.should_exclude("git status"));
        assert!(!buffer.should_exclude("make test"));
    }
}
