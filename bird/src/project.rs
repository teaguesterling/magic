//! Project detection for BIRD.
//!
//! Finds project-level `.bird/` directories by walking up from the current directory.

use std::path::{Path, PathBuf};

/// The name of the BIRD project directory.
pub const BIRD_DIR_NAME: &str = ".bird";

/// The name of the project database file.
pub const BIRD_DB_NAME: &str = "bird.duckdb";

/// Result of project detection.
#[derive(Debug, Clone)]
pub struct ProjectInfo {
    /// Root directory of the project (contains `.bird/`).
    pub root: PathBuf,

    /// Path to the `.bird/` directory.
    pub bird_dir: PathBuf,

    /// Path to the project database.
    pub db_path: PathBuf,
}

impl ProjectInfo {
    /// Get the blobs directory for this project.
    pub fn blobs_dir(&self) -> PathBuf {
        self.bird_dir.join("blobs").join("content")
    }

    /// Check if the project database exists.
    pub fn is_initialized(&self) -> bool {
        self.db_path.exists()
    }
}

/// Find the project root by walking up from the given directory.
///
/// Looks for a `.bird/` directory containing a `bird.duckdb` file.
/// Returns `None` if no project is found before reaching the filesystem root.
///
/// # Arguments
///
/// * `start_dir` - Directory to start searching from (typically current working directory)
///
/// # Example
///
/// ```no_run
/// use bird::project::find_project;
/// use std::env;
///
/// if let Some(project) = find_project(&env::current_dir().unwrap()) {
///     println!("Found project at: {}", project.root.display());
/// }
/// ```
pub fn find_project(start_dir: &Path) -> Option<ProjectInfo> {
    let mut current = start_dir.to_path_buf();

    loop {
        let bird_dir = current.join(BIRD_DIR_NAME);
        let db_path = bird_dir.join(BIRD_DB_NAME);

        // Check if .bird/ exists (even if not fully initialized)
        if bird_dir.is_dir() {
            return Some(ProjectInfo {
                root: current,
                bird_dir,
                db_path,
            });
        }

        // Move to parent directory
        if !current.pop() {
            break;
        }
    }

    None
}

/// Find project from current working directory.
///
/// Convenience function that starts from `std::env::current_dir()`.
pub fn find_current_project() -> Option<ProjectInfo> {
    std::env::current_dir().ok().and_then(|cwd| find_project(&cwd))
}

/// Check if a directory is inside a BIRD project.
pub fn is_in_project(dir: &Path) -> bool {
    find_project(dir).is_some()
}

/// Get the relative path from project root to the given path.
///
/// Returns `None` if the path is not under the project root.
pub fn project_relative_path(project: &ProjectInfo, path: &Path) -> Option<PathBuf> {
    path.strip_prefix(&project.root).ok().map(|p| p.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_find_project_exists() {
        let tmp = TempDir::new().unwrap();
        let project_root = tmp.path().join("my-project");
        let bird_dir = project_root.join(".bird");
        std::fs::create_dir_all(&bird_dir).unwrap();

        // Create a subdirectory to search from
        let subdir = project_root.join("src").join("lib");
        std::fs::create_dir_all(&subdir).unwrap();

        let result = find_project(&subdir);
        assert!(result.is_some());

        let project = result.unwrap();
        assert_eq!(project.root, project_root);
        assert_eq!(project.bird_dir, bird_dir);
    }

    #[test]
    fn test_find_project_not_found() {
        let tmp = TempDir::new().unwrap();
        let result = find_project(tmp.path());
        assert!(result.is_none());
    }

    #[test]
    fn test_project_relative_path() {
        let project = ProjectInfo {
            root: PathBuf::from("/home/user/project"),
            bird_dir: PathBuf::from("/home/user/project/.bird"),
            db_path: PathBuf::from("/home/user/project/.bird/bird.duckdb"),
        };

        let abs_path = Path::new("/home/user/project/src/main.rs");
        let rel = project_relative_path(&project, abs_path);
        assert_eq!(rel, Some(PathBuf::from("src/main.rs")));

        let outside = Path::new("/other/path");
        assert!(project_relative_path(&project, outside).is_none());
    }

    #[test]
    fn test_is_initialized() {
        let tmp = TempDir::new().unwrap();
        let bird_dir = tmp.path().join(".bird");
        std::fs::create_dir_all(&bird_dir).unwrap();

        let project = ProjectInfo {
            root: tmp.path().to_path_buf(),
            bird_dir: bird_dir.clone(),
            db_path: bird_dir.join("bird.duckdb"),
        };

        // Not initialized yet
        assert!(!project.is_initialized());

        // Create the database file
        std::fs::write(&project.db_path, b"").unwrap();
        assert!(project.is_initialized());
    }
}
