//! Runner liveness checking and recovery statistics.
//!
//! This module provides utilities for:
//! - Checking if a process/runner is still alive
//! - Tracking recovery operation statistics
//!
//! **V5 Schema Note**: In v5, pending detection is done via the invocations VIEW
//! (attempts without matching outcomes). The old pending file mechanism from v4
//! has been removed.

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
    /// Number of pending attempts checked.
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

    #[test]
    fn test_is_runner_alive_current_process() {
        let pid = std::process::id() as i32;
        let runner_id = format!("pid:{}", pid);
        assert!(is_runner_alive(&runner_id));
    }

    #[test]
    fn test_is_runner_alive_dead_process() {
        // PID 999999999 likely doesn't exist
        let runner_id = "pid:999999999";
        assert!(!is_runner_alive(runner_id));
    }

    #[test]
    fn test_is_runner_alive_unknown_format() {
        assert!(!is_runner_alive("unknown:123"));
        assert!(!is_runner_alive("gha:run:12345"));
        assert!(!is_runner_alive("k8s:pod:abc123"));
    }
}
