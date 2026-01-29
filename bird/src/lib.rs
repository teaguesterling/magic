//! BIRD: Buffer and Invocation Record Database
//!
//! Storage layer for shell command history using DuckDB and Parquet.

pub mod config;
pub mod error;
pub mod format_hints;
pub mod init;
pub mod project;
pub mod query;
pub mod schema;
pub mod store;

pub use config::{Config, RemoteConfig, RemoteMode, RemoteType, StorageMode, SyncConfig};
pub use error::{Error, Result};
pub use format_hints::{FormatHint, FormatHints};
pub use project::{find_current_project, find_project, is_in_project, ProjectInfo};
pub use query::{parse_query, CompareOp, FieldFilter, PathFilter, Query, QueryComponent, RangeSelector, SourceSelector};
pub use schema::{InvocationRecord, OutputRecord, SessionRecord, BIRD_INVOCATION_UUID_VAR, BIRD_PARENT_CLIENT_VAR};
pub use store::{
    parse_since, ArchiveStats, AutoCompactOptions, BuiltinFormat, CompactOptions, CompactStats,
    ConnectionOptions, EventFilters, EventSummary, FormatMatch, FormatSource, InvocationBatch,
    InvocationSummary, OutputInfo, PullOptions, PullStats, PushOptions, PushStats, Store,
};
