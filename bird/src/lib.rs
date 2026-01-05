//! BIRD: Buffer and Invocation Record Database
//!
//! Storage layer for shell command history using DuckDB and Parquet.

pub mod config;
pub mod error;
pub mod init;
pub mod schema;
pub mod store;

pub use config::Config;
pub use error::{Error, Result};
pub use schema::{InvocationRecord, OutputRecord, SessionRecord};
pub use store::{ArchiveStats, AutoCompactOptions, CompactStats, EventFilters, EventSummary, InvocationSummary, OutputInfo, Store};
