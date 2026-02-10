//! Context detection for metadata population.
//!
//! This module detects VCS (git) and CI environment context to populate
//! metadata fields on invocations.

use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

use serde_json::{json, Value};

/// Collected context metadata.
#[derive(Debug, Default, Clone)]
pub struct ContextMetadata {
    /// All collected metadata entries.
    pub entries: HashMap<String, Value>,
}

impl ContextMetadata {
    /// Collect all available context metadata.
    ///
    /// This is the main entry point - it collects VCS and CI context.
    pub fn collect(cwd: Option<&Path>) -> Self {
        let mut ctx = Self::default();

        // Collect VCS (git) context
        if let Some(vcs) = collect_git_context(cwd) {
            ctx.entries.insert("vcs".to_string(), vcs);
        }

        // Collect CI context
        if let Some(ci) = collect_ci_context() {
            ctx.entries.insert("ci".to_string(), ci);
        }

        ctx
    }

    /// Check if any metadata was collected.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Merge this context into a HashMap.
    pub fn into_map(self) -> HashMap<String, Value> {
        self.entries
    }
}

/// Collect git repository context.
///
/// Returns a JSON object with:
/// - branch: Current branch name (or HEAD if detached)
/// - commit: Short commit hash
/// - dirty: Whether there are uncommitted changes
/// - remote: Origin remote URL (if available)
fn collect_git_context(cwd: Option<&Path>) -> Option<Value> {
    // Check if we're in a git repo
    let mut cmd = Command::new("git");
    cmd.args(["rev-parse", "--is-inside-work-tree"]);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }

    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }

    let mut vcs = serde_json::Map::new();

    // Get current branch
    let mut cmd = Command::new("git");
    cmd.args(["rev-parse", "--abbrev-ref", "HEAD"]);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    if let Ok(output) = cmd.output() {
        if output.status.success() {
            let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
            vcs.insert("branch".to_string(), json!(branch));
        }
    }

    // Get short commit hash
    let mut cmd = Command::new("git");
    cmd.args(["rev-parse", "--short", "HEAD"]);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    if let Ok(output) = cmd.output() {
        if output.status.success() {
            let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
            vcs.insert("commit".to_string(), json!(commit));
        }
    }

    // Check if dirty (uncommitted changes)
    let mut cmd = Command::new("git");
    cmd.args(["status", "--porcelain"]);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    if let Ok(output) = cmd.output() {
        if output.status.success() {
            let dirty = !output.stdout.is_empty();
            vcs.insert("dirty".to_string(), json!(dirty));
        }
    }

    // Get origin remote URL (sanitized - no credentials)
    let mut cmd = Command::new("git");
    cmd.args(["remote", "get-url", "origin"]);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    if let Ok(output) = cmd.output() {
        if output.status.success() {
            let remote = String::from_utf8_lossy(&output.stdout).trim().to_string();
            // Sanitize: remove any embedded credentials (user:pass@)
            let sanitized = sanitize_git_url(&remote);
            vcs.insert("remote".to_string(), json!(sanitized));
        }
    }

    if vcs.is_empty() {
        None
    } else {
        Some(Value::Object(vcs))
    }
}

/// Sanitize a git URL by removing embedded credentials.
fn sanitize_git_url(url: &str) -> String {
    // Handle HTTPS URLs with credentials: https://user:pass@github.com/...
    if let Some(at_pos) = url.find('@') {
        if url.starts_with("https://") || url.starts_with("http://") {
            // Find the :// and rebuild without credentials
            if let Some(proto_end) = url.find("://") {
                return format!("{}{}", &url[..proto_end + 3], &url[at_pos + 1..]);
            }
        }
    }
    url.to_string()
}

/// Collect CI environment context.
///
/// Detects common CI systems and returns relevant metadata:
/// - GitHub Actions
/// - GitLab CI
/// - Jenkins
/// - CircleCI
/// - Travis CI
fn collect_ci_context() -> Option<Value> {
    let mut ci = serde_json::Map::new();

    // GitHub Actions
    if std::env::var("GITHUB_ACTIONS").is_ok() {
        ci.insert("provider".to_string(), json!("github"));

        if let Ok(run_id) = std::env::var("GITHUB_RUN_ID") {
            ci.insert("run_id".to_string(), json!(run_id));
        }
        if let Ok(run_number) = std::env::var("GITHUB_RUN_NUMBER") {
            ci.insert("run_number".to_string(), json!(run_number));
        }
        if let Ok(workflow) = std::env::var("GITHUB_WORKFLOW") {
            ci.insert("workflow".to_string(), json!(workflow));
        }
        if let Ok(job) = std::env::var("GITHUB_JOB") {
            ci.insert("job".to_string(), json!(job));
        }
        if let Ok(ref_name) = std::env::var("GITHUB_REF_NAME") {
            ci.insert("ref".to_string(), json!(ref_name));
        }
        if let Ok(event) = std::env::var("GITHUB_EVENT_NAME") {
            ci.insert("event".to_string(), json!(event));
        }
        if let Ok(actor) = std::env::var("GITHUB_ACTOR") {
            ci.insert("actor".to_string(), json!(actor));
        }

        return Some(Value::Object(ci));
    }

    // GitLab CI
    if std::env::var("GITLAB_CI").is_ok() {
        ci.insert("provider".to_string(), json!("gitlab"));

        if let Ok(job_id) = std::env::var("CI_JOB_ID") {
            ci.insert("job_id".to_string(), json!(job_id));
        }
        if let Ok(pipeline_id) = std::env::var("CI_PIPELINE_ID") {
            ci.insert("pipeline_id".to_string(), json!(pipeline_id));
        }
        if let Ok(job_name) = std::env::var("CI_JOB_NAME") {
            ci.insert("job_name".to_string(), json!(job_name));
        }
        if let Ok(ref_name) = std::env::var("CI_COMMIT_REF_NAME") {
            ci.insert("ref".to_string(), json!(ref_name));
        }

        return Some(Value::Object(ci));
    }

    // Jenkins
    if std::env::var("JENKINS_URL").is_ok() {
        ci.insert("provider".to_string(), json!("jenkins"));

        if let Ok(build_number) = std::env::var("BUILD_NUMBER") {
            ci.insert("build_number".to_string(), json!(build_number));
        }
        if let Ok(job_name) = std::env::var("JOB_NAME") {
            ci.insert("job_name".to_string(), json!(job_name));
        }
        if let Ok(branch) = std::env::var("GIT_BRANCH") {
            ci.insert("branch".to_string(), json!(branch));
        }

        return Some(Value::Object(ci));
    }

    // CircleCI
    if std::env::var("CIRCLECI").is_ok() {
        ci.insert("provider".to_string(), json!("circleci"));

        if let Ok(build_num) = std::env::var("CIRCLE_BUILD_NUM") {
            ci.insert("build_num".to_string(), json!(build_num));
        }
        if let Ok(job) = std::env::var("CIRCLE_JOB") {
            ci.insert("job".to_string(), json!(job));
        }
        if let Ok(branch) = std::env::var("CIRCLE_BRANCH") {
            ci.insert("branch".to_string(), json!(branch));
        }

        return Some(Value::Object(ci));
    }

    // Travis CI
    if std::env::var("TRAVIS").is_ok() {
        ci.insert("provider".to_string(), json!("travis"));

        if let Ok(build_id) = std::env::var("TRAVIS_BUILD_ID") {
            ci.insert("build_id".to_string(), json!(build_id));
        }
        if let Ok(job_id) = std::env::var("TRAVIS_JOB_ID") {
            ci.insert("job_id".to_string(), json!(job_id));
        }
        if let Ok(branch) = std::env::var("TRAVIS_BRANCH") {
            ci.insert("branch".to_string(), json!(branch));
        }

        return Some(Value::Object(ci));
    }

    // Generic CI detection (many CI systems set CI=true)
    if std::env::var("CI").map(|v| v == "true" || v == "1").unwrap_or(false) {
        ci.insert("provider".to_string(), json!("unknown"));
        return Some(Value::Object(ci));
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_git_url_https_with_creds() {
        let url = "https://user:password@github.com/org/repo.git";
        assert_eq!(sanitize_git_url(url), "https://github.com/org/repo.git");
    }

    #[test]
    fn test_sanitize_git_url_https_no_creds() {
        let url = "https://github.com/org/repo.git";
        assert_eq!(sanitize_git_url(url), "https://github.com/org/repo.git");
    }

    #[test]
    fn test_sanitize_git_url_ssh() {
        let url = "git@github.com:org/repo.git";
        assert_eq!(sanitize_git_url(url), "git@github.com:org/repo.git");
    }

    #[test]
    fn test_collect_git_context_in_repo() {
        // This test runs in the magic repo, so we should get git context
        let ctx = collect_git_context(None);

        // We're in a git repo, so this should return Some
        assert!(ctx.is_some(), "Should detect git context");

        let vcs = ctx.unwrap();
        assert!(vcs.get("branch").is_some(), "Should have branch");
        assert!(vcs.get("commit").is_some(), "Should have commit");
        assert!(vcs.get("dirty").is_some(), "Should have dirty flag");
    }

    #[test]
    fn test_collect_context_metadata() {
        // Should collect at least VCS context since we're in a git repo
        let ctx = ContextMetadata::collect(None);

        assert!(ctx.entries.contains_key("vcs"), "Should have VCS context");
    }
}
