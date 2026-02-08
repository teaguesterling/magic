//! Error types for BIRD operations.

use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("DuckDB error: {0}")]
    DuckDb(#[from] duckdb::Error),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Storage error: {0}")]
    Storage(String),

    #[error("BIRD not initialized at {0}")]
    NotInitialized(PathBuf),

    #[error("BIRD already initialized at {0}")]
    AlreadyInitialized(PathBuf),

    #[error("Invalid path: {0}")]
    InvalidPath(String),

    #[error("Not found: {0}")]
    NotFound(String),

    #[error("Extension error: {0}")]
    Extension(String),
}

pub type Result<T> = std::result::Result<T, Error>;
