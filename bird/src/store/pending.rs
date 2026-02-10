//! Pending invocation file operations for crash recovery.
//!
//! **V5 Schema Note**: In v5, pending detection is done via the invocations VIEW
//! (attempts without matching outcomes). The pending file mechanism is deprecated
//! but kept for backward compatibility with v4 recovery code.
//!
//! The following are still used in v5:
//! - `is_runner_alive()` - For checking if a process is still running
//! - `RecoveryStats` - For reporting recovery operation results
//!
//! The following are deprecated in v5:
//! - `PendingInvocation` - No longer written; kept for backward compatibility
//! - `write_pending_file()` - No longer called in v5
//! - `delete_pending_file()` - No longer called in v5
//! - `list_pending_files()` - No longer called in v5

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::Result;
use crate::schema::InvocationRecord;

/// A pending invocation marker stored as JSON.
///
/// This is a lightweight representation of an in-flight invocation,
/// stored as a JSON file for crash recovery.
///
/// **Deprecated in v5**: In v5 schema, pending detection is done via the
/// invocations VIEW (attempts without matching outcomes). This struct is
/// kept for backward compatibility with v4 code.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingInvocation {
    /// Unique identifier (matches the parquet record).
    pub id: Uuid,

    /// Session identifier.
    pub session_id: String,

    /// When the invocation started.
    pub timestamp: DateTime<Utc>,

    /// Working directory.
    pub cwd: String,

    /// Full command string.
    pub cmd: String,

    /// Runner identifier for liveness checking.
    pub runner_id: String,

    /// Client identifier.
    pub client_id: String,
}

impl PendingInvocation {
    /// Create a pending invocation from an InvocationRecord.
    pub fn from_record(record: &InvocationRecord) -> Option<Self> {
        let runner_id = record.runner_id.clone()?;
        Some(Self {
            id: record.id,
            session_id: record.session_id.clone(),
            timestamp: record.timestamp,
            cwd: record.cwd.clone(),
            cmd: record.cmd.clone(),
            runner_id,
            client_id: record.client_id.clone(),
        })
    }

    /// Get the filename for this pending invocation.
    pub fn filename(&self) -> String {
        format!("{}--{}.pending", self.session_id, self.id)
    }

    /// Get the full path for this pending file in the given directory.
    pub fn path(&self, pending_dir: &Path) -> PathBuf {
        pending_dir.join(self.filename())
    }
}

/// Write a pending invocation file.
///
/// **Deprecated in v5**: Pending files are no longer written. Use
/// `Store::start_invocation()` with `AttemptRecord` instead.
#[deprecated(note = "V4 API. In v5, pending detection is via the invocations view.")]
pub fn write_pending_file(pending_dir: &Path, pending: &PendingInvocation) -> Result<PathBuf> {
    fs::create_dir_all(pending_dir)?;
    let path = pending.path(pending_dir);
    let content = serde_json::to_string_pretty(pending)?;
    fs::write(&path, content)?;
    Ok(path)
}

/// Delete a pending invocation file.
///
/// **Deprecated in v5**: Pending files are no longer used.
#[deprecated(note = "V4 API. In v5, pending detection is via the invocations view.")]
pub fn delete_pending_file(pending_dir: &Path, id: Uuid, session_id: &str) -> Result<bool> {
    let filename = format!("{}--{}.pending", session_id, id);
    let path = pending_dir.join(filename);
    if path.exists() {
        fs::remove_file(&path)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// List all pending invocation files.
///
/// **Deprecated in v5**: Use `Store::get_pending_attempts()` instead.
#[deprecated(note = "V4 API. Use Store::get_pending_attempts() in v5.")]
pub fn list_pending_files(pending_dir: &Path) -> Result<Vec<PendingInvocation>> {
    if !pending_dir.exists() {
        return Ok(Vec::new());
    }

    let mut pending = Vec::new();
    for entry in fs::read_dir(pending_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().map(|e| e == "pending").unwrap_or(false) {
            match fs::read_to_string(&path) {
                Ok(content) => match serde_json::from_str::<PendingInvocation>(&content) {
                    Ok(p) => pending.push(p),
                    Err(e) => {
                        eprintln!("Warning: failed to parse pending file {:?}: {}", path, e);
                    }
                },
                Err(e) => {
                    eprintln!("Warning: failed to read pending file {:?}: {}", path, e);
                }
            }
        }
    }

    Ok(pending)
}

/// Check if a runner is still alive based on its runner_id.
///
/// Supports various runner ID formats:
/// - `pid:12345` - Local process ID
/// - `gha:run:12345678` - GitHub Actions run
/// - `k8s:pod:abc123` - Kubernetes pod
pub fn is_runner_alive(runner_id: &str) -> bool {
    if let Some(pid_str) = runner_id.strip_prefix("pid:") {
        // Local process - check if PID exists
        if let Ok(pid) = pid_str.parse::<i32>() {
            return is_pid_alive(pid);
        }
    } else if runner_id.starts_with("gha:") {
        // GitHub Actions - we can't reliably check, assume dead after max_age
        // TODO: Could use GitHub API to check run status
        return false;
    } else if runner_id.starts_with("k8s:") {
        // Kubernetes - we can't reliably check, assume dead after max_age
        // TODO: Could use kubectl to check pod status
        return false;
    }

    // Unknown format - assume dead
    false
}

/// Check if a local PID is still alive.
#[cfg(unix)]
fn is_pid_alive(pid: i32) -> bool {
    // Send signal 0 to check if process exists
    // This works even for processes owned by other users
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(not(unix))]
fn is_pid_alive(_pid: i32) -> bool {
    // On non-Unix, we can't easily check - assume alive
    true
}

/// Statistics from recovery operations.
#[derive(Debug, Default, Clone)]
pub struct RecoveryStats {
    /// Number of pending files checked.
    pub pending_checked: usize,

    /// Number still running (runner alive).
    pub still_running: usize,

    /// Number marked as orphaned.
    pub orphaned: usize,

    /// Number of errors encountered.
    pub errors: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_pending_file_lifecycle() {
        let tmp = TempDir::new().unwrap();
        let pending_dir = tmp.path().join("pending");

        // Create a pending invocation
        let record = InvocationRecord::new_pending_local(
            "test-session",
            "echo hello",
            "/tmp",
            std::process::id() as i32,
            "test@localhost",
        );

        let pending = PendingInvocation::from_record(&record).unwrap();

        // Write pending file
        let path = write_pending_file(&pending_dir, &pending).unwrap();
        assert!(path.exists());

        // List pending files
        let files = list_pending_files(&pending_dir).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].id, record.id);
        assert_eq!(files[0].cmd, "echo hello");

        // Delete pending file
        let deleted = delete_pending_file(&pending_dir, record.id, &record.session_id).unwrap();
        assert!(deleted);
        assert!(!path.exists());

        // List should be empty now
        let files = list_pending_files(&pending_dir).unwrap();
        assert!(files.is_empty());
    }

    #[test]
    fn test_is_runner_alive_current_process() {
        let pid = std::process::id() as i32;
        let runner_id = format!("pid:{}", pid);
        assert!(is_runner_alive(&runner_id));
    }

    #[test]
    fn test_is_runner_alive_dead_process() {
        // PID 1 exists but we can't signal it, PID 999999 likely doesn't exist
        let runner_id = "pid:999999999";
        assert!(!is_runner_alive(runner_id));
    }

    #[test]
    fn test_is_runner_alive_unknown_format() {
        assert!(!is_runner_alive("unknown:123"));
        assert!(!is_runner_alive("gha:run:12345"));
        assert!(!is_runner_alive("k8s:pod:abc123"));
    }

    #[test]
    fn test_pending_filename() {
        let pending = PendingInvocation {
            id: Uuid::parse_str("01234567-89ab-cdef-0123-456789abcdef").unwrap(),
            session_id: "test-session".to_string(),
            timestamp: Utc::now(),
            cwd: "/tmp".to_string(),
            cmd: "echo hello".to_string(),
            runner_id: "pid:12345".to_string(),
            client_id: "test@localhost".to_string(),
        };

        assert_eq!(
            pending.filename(),
            "test-session--01234567-89ab-cdef-0123-456789abcdef.pending"
        );
    }
}
