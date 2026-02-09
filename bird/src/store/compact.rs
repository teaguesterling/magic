//! Compaction and archival operations.
//!
//! Supports two modes:
//! - **Client-specific**: Compact files for a single session/client (used by shell hooks)
//! - **Global**: Compact files across all sessions (used for scheduled maintenance)

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{NaiveDate, Utc};
use uuid::Uuid;

use super::atomic;
use super::Store;
use crate::Result;

/// Check if a filename is a compacted file (contains `__compacted-N__`).
fn is_compacted_file(name: &str) -> bool {
    name.contains("__compacted-")
}

/// Check if a filename is a seed file.
fn is_seed_file(name: &str) -> bool {
    name.starts_with("_seed")
}

/// Extract the session/group key from a filename.
///
/// For files like `session--cmd--uuid.parquet`, returns `session`.
/// For files like `session_id.parquet`, returns `session_id`.
/// For compacted files like `session--__compacted-0__--uuid.parquet`, returns `session`.
fn extract_session(name: &str) -> Option<String> {
    let stem = name.strip_suffix(".parquet")?;

    // Find first segment before "--"
    if let Some(idx) = stem.find("--") {
        Some(stem[..idx].to_string())
    } else {
        // No "--", use whole stem (e.g., session files)
        Some(stem.to_string())
    }
}

/// Extract the compaction sequence number from a filename, if present.
fn extract_compaction_number(name: &str) -> Option<u32> {
    // Pattern: __compacted-N__
    if let Some(start) = name.find("__compacted-") {
        let after_prefix = &name[start + 12..]; // skip "__compacted-"
        if let Some(end) = after_prefix.find("__") {
            if let Ok(n) = after_prefix[..end].parse::<u32>() {
                return Some(n);
            }
        }
    }
    None
}

/// Find the next compaction sequence number for a session in a partition.
fn next_compaction_number(partition_dir: &Path, session: &str) -> u32 {
    fs::read_dir(partition_dir)
        .ok()
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    // Only consider compacted files for this session
                    if name.starts_with(&format!("{}--__compacted-", session)) {
                        extract_compaction_number(&name)
                    } else {
                        None
                    }
                })
                .max()
                .map(|n| n + 1)
                .unwrap_or(0)
        })
        .unwrap_or(0)
}

/// Get modification time for sorting files by age.
fn file_mtime(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .map(|t| t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs())
        .unwrap_or(0)
}

/// Statistics from a compaction operation.
#[derive(Debug, Default)]
pub struct CompactStats {
    pub partitions_compacted: usize,
    pub sessions_compacted: usize,
    pub files_before: usize,
    pub files_after: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

impl CompactStats {
    pub fn add(&mut self, other: &CompactStats) {
        self.partitions_compacted += other.partitions_compacted;
        self.sessions_compacted += other.sessions_compacted;
        self.files_before += other.files_before;
        self.files_after += other.files_after;
        self.bytes_before += other.bytes_before;
        self.bytes_after += other.bytes_after;
    }
}

/// Statistics from an archive operation.
#[derive(Debug, Default)]
pub struct ArchiveStats {
    pub partitions_archived: usize,
    pub files_moved: usize,
    pub bytes_moved: u64,
}

/// Options for compaction operations.
#[derive(Debug, Clone)]
pub struct CompactOptions {
    /// Compact when a session has more than this many non-compacted files.
    pub file_threshold: usize,
    /// Re-compact when a session has more than this many compacted files.
    /// Set to 0 to disable re-compaction.
    pub recompact_threshold: usize,
    /// If true, consolidate ALL files (including compacted) into a single file.
    pub consolidate: bool,
    /// If true, don't actually make changes.
    pub dry_run: bool,
    /// If set, only compact files for this specific session.
    pub session_filter: Option<String>,
}

impl Default for CompactOptions {
    fn default() -> Self {
        Self {
            file_threshold: 50,
            recompact_threshold: 10,
            consolidate: false,
            dry_run: false,
            session_filter: None,
        }
    }
}

/// Options for auto-compaction (includes archive settings).
#[derive(Debug)]
pub struct AutoCompactOptions {
    /// Compact options.
    pub compact: CompactOptions,
    /// Migrate data older than this many days to archive.
    pub archive_days: u32,
}

impl Default for AutoCompactOptions {
    fn default() -> Self {
        Self {
            compact: CompactOptions::default(),
            archive_days: 14,
        }
    }
}

impl Store {
    /// Compact files for a specific session in a partition.
    ///
    /// Keeps the most recent `keep_count` files, compacts the rest.
    /// Uses naming: `<session>--__compacted-N__--<uuid>.parquet`
    fn compact_session_files(
        &self,
        partition_dir: &Path,
        session: &str,
        files: &mut Vec<PathBuf>,
        keep_count: usize,
        dry_run: bool,
    ) -> Result<CompactStats> {
        // Sort by modification time (oldest first)
        files.sort_by_key(|p| file_mtime(p));

        // Keep the most recent `keep_count` files
        let to_keep = files.len().saturating_sub(keep_count).max(0);
        if to_keep < 2 {
            // Need at least 2 files to compact
            return Ok(CompactStats::default());
        }

        let files_to_compact: Vec<PathBuf> = files.drain(..to_keep).collect();
        let files_before = files_to_compact.len();
        let bytes_before: u64 = files_to_compact
            .iter()
            .filter_map(|p| fs::metadata(p).ok())
            .map(|m| m.len())
            .sum();

        if dry_run {
            return Ok(CompactStats {
                partitions_compacted: 0, // Will be set by caller
                sessions_compacted: 1,
                files_before,
                files_after: 1,
                bytes_before,
                bytes_after: bytes_before, // Estimate
            });
        }

        // Use minimal connection to avoid view setup overhead
        let conn = self.connection_with_options(false)?;

        // Build list of files to read
        let file_list: Vec<String> = files_to_compact
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        let file_list_sql = file_list
            .iter()
            .map(|f| format!("'{}'", f))
            .collect::<Vec<_>>()
            .join(", ");

        // Create temp table with data from selected files
        conn.execute(
            &format!(
                "CREATE OR REPLACE TEMP TABLE compact_temp AS
                 SELECT * FROM read_parquet([{}], union_by_name = true)",
                file_list_sql
            ),
            [],
        )?;

        // Generate compacted filename: <session>--__compacted-N__--<uuid>.parquet
        let seq_num = next_compaction_number(partition_dir, session);
        let uuid = Uuid::now_v7();
        let compacted_name = format!("{}--__compacted-{}__--{}.parquet", session, seq_num, uuid);
        let compacted_path = partition_dir.join(&compacted_name);
        let temp_path = atomic::temp_path(&compacted_path);

        conn.execute(
            &format!(
                "COPY compact_temp TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD)",
                temp_path.display()
            ),
            [],
        )?;

        conn.execute("DROP TABLE compact_temp", [])?;

        // Get size of new file
        let bytes_after = fs::metadata(&temp_path)?.len();

        // Remove old files
        for file in &files_to_compact {
            fs::remove_file(file)?;
        }

        // Rename compacted file to final location
        atomic::rename_into_place(&temp_path, &compacted_path)?;

        Ok(CompactStats {
            partitions_compacted: 0, // Will be set by caller
            sessions_compacted: 1,
            files_before,
            files_after: 1,
            bytes_before,
            bytes_after,
        })
    }

    /// Consolidate ALL files for a session into a single file.
    ///
    /// Unlike regular compaction, this merges everything including previously
    /// compacted files into a single `data_0.parquet` file.
    fn consolidate_session_files(
        &self,
        partition_dir: &Path,
        _session: &str,
        files: Vec<PathBuf>,
        dry_run: bool,
    ) -> Result<CompactStats> {
        if files.len() < 2 {
            return Ok(CompactStats::default());
        }

        let files_before = files.len();
        let bytes_before: u64 = files
            .iter()
            .filter_map(|p| fs::metadata(p).ok())
            .map(|m| m.len())
            .sum();

        if dry_run {
            return Ok(CompactStats {
                partitions_compacted: 0,
                sessions_compacted: 1,
                files_before,
                files_after: 1,
                bytes_before,
                bytes_after: bytes_before,
            });
        }

        // Use minimal connection to avoid view setup overhead
        let conn = self.connection_with_options(false)?;

        // Build list of files to read
        let file_list_sql = files
            .iter()
            .map(|p| format!("'{}'", p.display()))
            .collect::<Vec<_>>()
            .join(", ");

        // Create temp table with data from all files
        conn.execute(
            &format!(
                "CREATE OR REPLACE TEMP TABLE consolidate_temp AS
                 SELECT * FROM read_parquet([{}], union_by_name = true)",
                file_list_sql
            ),
            [],
        )?;

        // Use simple data_0.parquet naming for consolidated files
        let uuid = Uuid::now_v7();
        let consolidated_name = format!("data_{}.parquet", uuid);
        let consolidated_path = partition_dir.join(&consolidated_name);
        let temp_path = atomic::temp_path(&consolidated_path);

        conn.execute(
            &format!(
                "COPY consolidate_temp TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD)",
                temp_path.display()
            ),
            [],
        )?;

        conn.execute("DROP TABLE consolidate_temp", [])?;

        let bytes_after = fs::metadata(&temp_path)?.len();

        // Remove old files
        for file in &files {
            fs::remove_file(file)?;
        }

        // Rename consolidated file to final location
        atomic::rename_into_place(&temp_path, &consolidated_path)?;

        Ok(CompactStats {
            partitions_compacted: 0,
            sessions_compacted: 1,
            files_before,
            files_after: 1,
            bytes_before,
            bytes_after,
        })
    }

    /// Compact files in a partition directory, grouped by session.
    ///
    /// Behavior depends on options:
    /// - `consolidate`: Merge ALL files into single file per session
    /// - `file_threshold`: Compact when > N non-compacted files
    /// - `recompact_threshold`: Re-compact when > N compacted files exist
    pub fn compact_partition_with_opts(
        &self,
        partition_dir: &Path,
        opts: &CompactOptions,
    ) -> Result<CompactStats> {
        let mut total_stats = CompactStats::default();

        // Group files by session, separating compacted from non-compacted
        let mut session_files: HashMap<String, (Vec<PathBuf>, Vec<PathBuf>)> = HashMap::new();

        for entry in fs::read_dir(partition_dir)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            // Skip non-parquet and seed files
            if !name.ends_with(".parquet") || is_seed_file(&name) {
                continue;
            }

            // Extract session from filename
            if let Some(session) = extract_session(&name) {
                // Apply session filter if specified
                if let Some(ref filter) = opts.session_filter {
                    if session != *filter {
                        continue;
                    }
                }

                let entry = session_files.entry(session).or_insert_with(|| (vec![], vec![]));
                if is_compacted_file(&name) || name.starts_with("data_") {
                    entry.1.push(path); // compacted/consolidated
                } else {
                    entry.0.push(path); // non-compacted
                }
            }
        }

        let mut any_compacted = false;
        for (session, (non_compacted, compacted)) in session_files {
            if opts.consolidate {
                // Consolidate mode: merge ALL files into one
                let all_files: Vec<PathBuf> = non_compacted.into_iter().chain(compacted).collect();
                if all_files.len() >= 2 {
                    let stats = self.consolidate_session_files(
                        partition_dir,
                        &session,
                        all_files,
                        opts.dry_run,
                    )?;
                    if stats.sessions_compacted > 0 {
                        any_compacted = true;
                        total_stats.add(&stats);
                    }
                }
            } else {
                // Regular compaction mode

                // First: compact non-compacted files if threshold exceeded
                if non_compacted.len() >= opts.file_threshold {
                    let mut to_process = non_compacted;
                    let stats = self.compact_session_files(
                        partition_dir,
                        &session,
                        &mut to_process,
                        opts.file_threshold,
                        opts.dry_run,
                    )?;
                    if stats.sessions_compacted > 0 {
                        any_compacted = true;
                        total_stats.add(&stats);
                    }
                }

                // Second: re-compact compacted files if recompact threshold exceeded
                if opts.recompact_threshold > 0 && compacted.len() >= opts.recompact_threshold {
                    let stats = self.consolidate_session_files(
                        partition_dir,
                        &session,
                        compacted,
                        opts.dry_run,
                    )?;
                    if stats.sessions_compacted > 0 {
                        any_compacted = true;
                        total_stats.add(&stats);
                    }
                }
            }
        }

        if any_compacted {
            total_stats.partitions_compacted = 1;
        }

        Ok(total_stats)
    }

    /// Compact files in a partition directory (legacy API).
    pub fn compact_partition(
        &self,
        partition_dir: &Path,
        file_threshold: usize,
        session_filter: Option<&str>,
        dry_run: bool,
    ) -> Result<CompactStats> {
        let opts = CompactOptions {
            file_threshold,
            recompact_threshold: 0, // Disable re-compaction for legacy API
            consolidate: false,
            dry_run,
            session_filter: session_filter.map(|s| s.to_string()),
        };
        self.compact_partition_with_opts(partition_dir, &opts)
    }

    /// Compact all partitions in a data type directory (invocations, outputs, sessions).
    pub fn compact_data_type(
        &self,
        data_dir: &Path,
        file_threshold: usize,
        session_filter: Option<&str>,
        dry_run: bool,
    ) -> Result<CompactStats> {
        let mut total_stats = CompactStats::default();

        if !data_dir.exists() {
            return Ok(total_stats);
        }

        // Iterate over date partitions
        for entry in fs::read_dir(data_dir)? {
            let entry = entry?;
            let path = entry.path();

            if !path.is_dir() {
                continue;
            }

            let stats = self.compact_partition(&path, file_threshold, session_filter, dry_run)?;
            total_stats.add(&stats);
        }

        Ok(total_stats)
    }

    /// Compact recent data for a specific session (used by shell hooks).
    ///
    /// Checks all date partitions in recent data.
    pub fn compact_for_session(
        &self,
        session_id: &str,
        file_threshold: usize,
        dry_run: bool,
    ) -> Result<CompactStats> {
        let mut total_stats = CompactStats::default();
        let recent_dir = self.config().recent_dir();

        for data_type in &["invocations", "outputs", "sessions", "events"] {
            let data_dir = recent_dir.join(data_type);
            let stats =
                self.compact_data_type(&data_dir, file_threshold, Some(session_id), dry_run)?;
            total_stats.add(&stats);
        }

        Ok(total_stats)
    }

    /// Fast compaction check for today's partition only (used by shell hooks).
    ///
    /// This is the most lightweight check - only looks at today's date partition.
    pub fn compact_session_today(
        &self,
        session_id: &str,
        file_threshold: usize,
        dry_run: bool,
    ) -> Result<CompactStats> {
        let mut total_stats = CompactStats::default();
        let recent_dir = self.config().recent_dir();
        let today = Utc::now().date_naive();
        let date_partition = format!("date={}", today.format("%Y-%m-%d"));

        for data_type in &["invocations", "outputs", "sessions", "events"] {
            let partition_dir = recent_dir.join(data_type).join(&date_partition);
            if partition_dir.exists() {
                let stats = self.compact_partition(
                    &partition_dir,
                    file_threshold,
                    Some(session_id),
                    dry_run,
                )?;
                total_stats.add(&stats);
            }
        }

        Ok(total_stats)
    }

    /// Compact all recent data that exceeds the file threshold (global mode).
    pub fn compact_recent(&self, file_threshold: usize, dry_run: bool) -> Result<CompactStats> {
        let mut total_stats = CompactStats::default();
        let recent_dir = self.config().recent_dir();

        for data_type in &["invocations", "outputs", "sessions", "events"] {
            let data_dir = recent_dir.join(data_type);
            let stats = self.compact_data_type(&data_dir, file_threshold, None, dry_run)?;
            total_stats.add(&stats);
        }

        Ok(total_stats)
    }

    /// Compact all archive data that exceeds the file threshold.
    pub fn compact_archive(&self, file_threshold: usize, dry_run: bool) -> Result<CompactStats> {
        let mut total_stats = CompactStats::default();
        let archive_dir = self.config().archive_dir();

        for data_type in &["invocations", "outputs", "sessions", "events"] {
            let data_dir = archive_dir.join(data_type);
            let stats = self.compact_data_type(&data_dir, file_threshold, None, dry_run)?;
            total_stats.add(&stats);
        }

        Ok(total_stats)
    }

    /// Migrate old data from recent to archive with consolidation.
    ///
    /// Consolidates all files in each date partition into a single parquet file
    /// in the archive, then removes the source files.
    pub fn archive_old_data(&self, older_than_days: u32, dry_run: bool) -> Result<ArchiveStats> {
        let mut stats = ArchiveStats::default();
        let cutoff_date = Utc::now().date_naive() - chrono::Duration::days(older_than_days as i64);

        let recent_dir = self.config().recent_dir();
        let archive_dir = self.config().archive_dir();

        for data_type in &["invocations", "outputs", "sessions", "events"] {
            let recent_data_dir = recent_dir.join(data_type);
            let archive_data_dir = archive_dir.join(data_type);

            if !recent_data_dir.exists() {
                continue;
            }

            // Collect partitions to archive
            // Use <= so that "older_than_days=0" archives everything including today
            // Skip seed partitions (date=1970-01-01) which contain schema-only files
            let seed_date = NaiveDate::from_ymd_opt(1970, 1, 1).unwrap();
            let partitions_to_archive: Vec<(NaiveDate, PathBuf, String)> = fs::read_dir(&recent_data_dir)?
                .filter_map(|e| e.ok())
                .filter_map(|e| {
                    let path = e.path();
                    if !path.is_dir() {
                        return None;
                    }
                    let dir_name = e.file_name().to_string_lossy().to_string();
                    let date_str = dir_name.strip_prefix("date=")?;
                    let date = NaiveDate::parse_from_str(date_str, "%Y-%m-%d").ok()?;
                    // Skip seed partition
                    if date == seed_date {
                        return None;
                    }
                    if date <= cutoff_date {
                        Some((date, path, dir_name))
                    } else {
                        None
                    }
                })
                .collect();

            for (_date, partition_path, dir_name) in partitions_to_archive {
                let dest_dir = archive_data_dir.join(&dir_name);

                // Count source stats
                let source_files: Vec<PathBuf> = fs::read_dir(&partition_path)?
                    .filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.extension().map(|ext| ext == "parquet").unwrap_or(false))
                    .collect();

                if source_files.is_empty() {
                    continue;
                }

                let file_count = source_files.len();
                let bytes_before: u64 = source_files
                    .iter()
                    .filter_map(|p| fs::metadata(p).ok())
                    .map(|m| m.len())
                    .sum();

                if dry_run {
                    stats.partitions_archived += 1;
                    stats.files_moved += file_count;
                    stats.bytes_moved += bytes_before;
                    continue;
                }

                // Skip if archive partition already exists with data
                if dest_dir.exists() {
                    let existing_files = fs::read_dir(&dest_dir)?
                        .filter_map(|e| e.ok())
                        .any(|e| e.path().extension().map(|ext| ext == "parquet").unwrap_or(false));
                    if existing_files {
                        // Already archived, skip
                        continue;
                    }
                }

                fs::create_dir_all(&dest_dir)?;

                // Consolidate using DuckDB's COPY
                // Use minimal connection to avoid view setup (which can fail if
                // some data type directories are empty)
                let conn = self.connection_with_options(false)?;
                let src_glob = format!("{}/*.parquet", partition_path.display());
                let dest_file = dest_dir.join("data_0.parquet");
                let temp_file = dest_dir.join(".data_0.parquet.tmp");

                conn.execute(
                    &format!(
                        "COPY (SELECT * FROM read_parquet('{}', union_by_name = true)) \
                         TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD)",
                        src_glob,
                        temp_file.display()
                    ),
                    [],
                )?;

                // Atomic rename
                fs::rename(&temp_file, &dest_file)?;

                // Get consolidated size
                let bytes_after = fs::metadata(&dest_file)?.len();

                // Remove source files
                for file in &source_files {
                    let _ = fs::remove_file(file);
                }
                let _ = fs::remove_dir(&partition_path);

                stats.partitions_archived += 1;
                stats.files_moved += file_count;
                stats.bytes_moved += bytes_after;
            }
        }

        Ok(stats)
    }

    /// Run auto-compaction based on options.
    pub fn auto_compact(&self, opts: &AutoCompactOptions) -> Result<(CompactStats, ArchiveStats)> {
        // First, archive old data (skip if doing session-specific compaction)
        let archive_stats = if opts.compact.session_filter.is_none() {
            self.archive_old_data(opts.archive_days, opts.compact.dry_run)?
        } else {
            ArchiveStats::default()
        };

        // Then compact based on mode
        let compact_stats = if let Some(ref session) = opts.compact.session_filter {
            // Client-specific mode
            self.compact_for_session_with_opts(session, &opts.compact)?
        } else {
            // Global mode: compact both recent and archive
            let mut stats = self.compact_recent_with_opts(&opts.compact)?;
            let archive_compact = self.compact_archive_with_opts(&opts.compact)?;
            stats.add(&archive_compact);
            stats
        };

        Ok((compact_stats, archive_stats))
    }

    /// Compact recent data with full options.
    pub fn compact_recent_with_opts(&self, opts: &CompactOptions) -> Result<CompactStats> {
        let mut total_stats = CompactStats::default();
        let recent_dir = self.config().recent_dir();

        for data_type in &["invocations", "outputs", "sessions", "events"] {
            let data_dir = recent_dir.join(data_type);
            let stats = self.compact_data_type_with_opts(&data_dir, opts)?;
            total_stats.add(&stats);
        }

        Ok(total_stats)
    }

    /// Compact archive data with full options.
    pub fn compact_archive_with_opts(&self, opts: &CompactOptions) -> Result<CompactStats> {
        let mut total_stats = CompactStats::default();
        let archive_dir = self.config().archive_dir();

        for data_type in &["invocations", "outputs", "sessions", "events"] {
            let data_dir = archive_dir.join(data_type);
            let stats = self.compact_data_type_with_opts(&data_dir, opts)?;
            total_stats.add(&stats);
        }

        Ok(total_stats)
    }

    /// Compact data type directory with full options.
    pub fn compact_data_type_with_opts(
        &self,
        data_dir: &Path,
        opts: &CompactOptions,
    ) -> Result<CompactStats> {
        let mut total_stats = CompactStats::default();

        if !data_dir.exists() {
            return Ok(total_stats);
        }

        for entry in fs::read_dir(data_dir)? {
            let entry = entry?;
            let path = entry.path();

            if !path.is_dir() {
                continue;
            }

            let stats = self.compact_partition_with_opts(&path, opts)?;
            total_stats.add(&stats);
        }

        Ok(total_stats)
    }

    /// Compact files for a specific session with full options.
    pub fn compact_for_session_with_opts(
        &self,
        session_id: &str,
        opts: &CompactOptions,
    ) -> Result<CompactStats> {
        let mut total_stats = CompactStats::default();
        let recent_dir = self.config().recent_dir();

        let session_opts = CompactOptions {
            session_filter: Some(session_id.to_string()),
            ..opts.clone()
        };

        for data_type in &["invocations", "outputs", "sessions", "events"] {
            let data_dir = recent_dir.join(data_type);
            let stats = self.compact_data_type_with_opts(&data_dir, &session_opts)?;
            total_stats.add(&stats);
        }

        Ok(total_stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::initialize;
    use crate::schema::InvocationRecord;
    use crate::Config;
    use tempfile::TempDir;

    fn setup_store() -> (TempDir, Store) {
        let tmp = TempDir::new().unwrap();
        let config = Config::with_root(tmp.path());
        initialize(&config).unwrap();
        let store = Store::open(config).unwrap();
        (tmp, store)
    }

    fn setup_store_duckdb() -> (TempDir, Store) {
        let tmp = TempDir::new().unwrap();
        let config = Config::with_duckdb_mode(tmp.path());
        initialize(&config).unwrap();
        let store = Store::open(config).unwrap();
        (tmp, store)
    }

    #[test]
    fn test_extract_session() {
        assert_eq!(
            extract_session("zsh-1234--make--uuid.parquet"),
            Some("zsh-1234".to_string())
        );
        assert_eq!(
            extract_session("session-id.parquet"),
            Some("session-id".to_string())
        );
        assert_eq!(
            extract_session("zsh-1234--__compacted-0__--uuid.parquet"),
            Some("zsh-1234".to_string())
        );
    }

    #[test]
    fn test_is_compacted_file() {
        assert!(is_compacted_file("zsh-1234--__compacted-0__--uuid.parquet"));
        assert!(is_compacted_file("session--__compacted-5__--abc.parquet"));
        assert!(!is_compacted_file("zsh-1234--make--uuid.parquet"));
        assert!(!is_compacted_file("session.parquet"));
    }

    #[test]
    fn test_extract_compaction_number() {
        assert_eq!(
            extract_compaction_number("zsh--__compacted-0__--uuid.parquet"),
            Some(0)
        );
        assert_eq!(
            extract_compaction_number("zsh--__compacted-42__--uuid.parquet"),
            Some(42)
        );
        assert_eq!(
            extract_compaction_number("zsh--make--uuid.parquet"),
            None
        );
    }

    #[test]
    fn test_compact_recent_no_files() {
        let (_tmp, store) = setup_store();

        let stats = store.compact_recent(2, false).unwrap();
        assert_eq!(stats.partitions_compacted, 0);
    }

    #[test]
    fn test_compact_recent_with_files() {
        let (_tmp, store) = setup_store();

        // Write multiple invocations to create multiple files
        for i in 0..5 {
            let record = InvocationRecord::new(
                "test-session",
                format!("command-{}", i),
                "/home/user",
                0,
                "test@client",
            );
            store.write_invocation(&record).unwrap();
        }

        // With threshold of 2, should compact oldest 3 (keeping 2)
        let stats = store.compact_recent(2, false).unwrap();
        assert_eq!(stats.sessions_compacted, 1);
        assert_eq!(stats.files_before, 3); // 5 - 2 kept = 3 compacted
        assert_eq!(stats.files_after, 1);
    }

    #[test]
    fn test_compact_for_session() {
        let (_tmp, store) = setup_store();

        // Write files for two different sessions
        for i in 0..5 {
            let record = InvocationRecord::new(
                "session-a",
                format!("command-{}", i),
                "/home/user",
                0,
                "test@client",
            );
            store.write_invocation(&record).unwrap();
        }
        for i in 0..3 {
            let record = InvocationRecord::new(
                "session-b",
                format!("command-{}", i),
                "/home/user",
                0,
                "test@client",
            );
            store.write_invocation(&record).unwrap();
        }

        // Compact only session-a with threshold of 2
        let stats = store.compact_for_session("session-a", 2, false).unwrap();
        assert_eq!(stats.sessions_compacted, 1);
        assert_eq!(stats.files_before, 3); // 5 - 2 kept = 3

        // session-b should be untouched (only 3 files, below threshold)
        let date = chrono::Utc::now().date_naive();
        let inv_dir = store.config().invocations_dir(&date);
        let session_b_count = std::fs::read_dir(&inv_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("session-b"))
            .count();
        assert_eq!(session_b_count, 3);
    }

    #[test]
    fn test_compact_dry_run() {
        let (_tmp, store) = setup_store();

        // Write multiple invocations
        for i in 0..5 {
            let record = InvocationRecord::new(
                "test-session",
                format!("command-{}", i),
                "/home/user",
                0,
                "test@client",
            );
            store.write_invocation(&record).unwrap();
        }

        // Dry run should report stats but not actually compact
        let stats = store.compact_recent(2, true).unwrap();
        assert_eq!(stats.sessions_compacted, 1);
        assert_eq!(stats.files_before, 3);

        // Files should still be there
        let date = chrono::Utc::now().date_naive();
        let inv_dir = store.config().invocations_dir(&date);
        let file_count = std::fs::read_dir(&inv_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|ext| ext == "parquet").unwrap_or(false))
            .filter(|e| !e.file_name().to_string_lossy().starts_with("_seed"))
            .count();
        assert_eq!(file_count, 5);
    }

    #[test]
    fn test_compacted_file_naming() {
        let (_tmp, store) = setup_store();

        // Write enough files to trigger compaction
        for i in 0..5 {
            let record = InvocationRecord::new(
                "zsh-9999",
                format!("cmd-{}", i),
                "/home/user",
                0,
                "test@client",
            );
            store.write_invocation(&record).unwrap();
        }

        // Compact with threshold of 2
        store.compact_recent(2, false).unwrap();

        // Check that compacted file has correct naming
        let date = chrono::Utc::now().date_naive();
        let inv_dir = store.config().invocations_dir(&date);
        let compacted_files: Vec<_> = std::fs::read_dir(&inv_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| is_compacted_file(&e.file_name().to_string_lossy()))
            .collect();

        assert_eq!(compacted_files.len(), 1);
        let name = compacted_files[0].file_name().to_string_lossy().to_string();
        assert!(name.starts_with("zsh-9999--__compacted-0__--"));
        assert!(name.ends_with(".parquet"));
    }

    // ===== DuckDB Mode Tests =====
    // In DuckDB mode, compact should be a no-op since there are no parquet files.

    #[test]
    fn test_compact_duckdb_mode_no_op() {
        let (_tmp, store) = setup_store_duckdb();

        // Write data in DuckDB mode (goes to local.invocations table, not parquet files)
        for i in 0..10 {
            let record = InvocationRecord::new(
                "test-session",
                format!("command-{}", i),
                "/home/user",
                0,
                "test@client",
            );
            store.write_invocation(&record).unwrap();
        }

        // Verify data is stored (not in parquet files)
        let conn = store.connection().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM local.invocations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 10, "Data should be in DuckDB table");

        // Compact should be a no-op (no parquet files to compact)
        let stats = store.compact_recent(2, false).unwrap();
        assert_eq!(stats.partitions_compacted, 0);
        assert_eq!(stats.sessions_compacted, 0);
        assert_eq!(stats.files_before, 0);
        assert_eq!(stats.files_after, 0);

        // Data should still be there
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM local.invocations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 10, "Data should be unaffected");
    }

    #[test]
    fn test_compact_for_session_duckdb_mode_no_op() {
        let (_tmp, store) = setup_store_duckdb();

        // Write data in DuckDB mode
        for i in 0..5 {
            let record = InvocationRecord::new(
                "session-a",
                format!("command-{}", i),
                "/home/user",
                0,
                "test@client",
            );
            store.write_invocation(&record).unwrap();
        }

        // Session-specific compact should also be a no-op
        let stats = store.compact_for_session("session-a", 2, false).unwrap();
        assert_eq!(stats.partitions_compacted, 0);
        assert_eq!(stats.sessions_compacted, 0);
    }

    #[test]
    fn test_compact_session_today_duckdb_mode_no_op() {
        let (_tmp, store) = setup_store_duckdb();

        // Write data in DuckDB mode
        let record = InvocationRecord::new(
            "test-session",
            "echo hello",
            "/home/user",
            0,
            "test@client",
        );
        store.write_invocation(&record).unwrap();

        // Today's session compact should be a no-op
        let stats = store.compact_session_today("test-session", 2, false).unwrap();
        assert_eq!(stats.partitions_compacted, 0);
        assert_eq!(stats.sessions_compacted, 0);
    }

    #[test]
    fn test_auto_compact_duckdb_mode_no_op() {
        let (_tmp, store) = setup_store_duckdb();

        // Write data
        for i in 0..5 {
            let record = InvocationRecord::new(
                "test-session",
                format!("command-{}", i),
                "/home/user",
                0,
                "test@client",
            );
            store.write_invocation(&record).unwrap();
        }

        // Auto-compact should be a no-op
        let opts = AutoCompactOptions::default();
        let (compact_stats, archive_stats) = store.auto_compact(&opts).unwrap();

        assert_eq!(compact_stats.partitions_compacted, 0);
        assert_eq!(compact_stats.sessions_compacted, 0);
        assert_eq!(archive_stats.partitions_archived, 0);
    }

    #[test]
    fn test_archive_old_data_duckdb_mode_no_op() {
        let (_tmp, store) = setup_store_duckdb();

        // Write data
        let record = InvocationRecord::new(
            "test-session",
            "echo hello",
            "/home/user",
            0,
            "test@client",
        );
        store.write_invocation(&record).unwrap();

        // Archive should be a no-op (no parquet partitions to archive)
        let stats = store.archive_old_data(0, false).unwrap(); // 0 days = archive everything
        assert_eq!(stats.partitions_archived, 0);
        assert_eq!(stats.files_moved, 0);
    }

    // ===== Archive Tests (Parquet Mode) =====

    #[test]
    fn test_archive_old_data_moves_partitions() {
        let (_tmp, store) = setup_store();

        // Write multiple invocations
        for i in 0..3 {
            let record = InvocationRecord::new(
                "test-session",
                format!("command-{}", i),
                "/home/user",
                0,
                "test@client",
            );
            store.write_invocation(&record).unwrap();
        }

        // Verify files exist in recent
        let date = chrono::Utc::now().date_naive();
        let recent_dir = store.config().invocations_dir(&date);
        let recent_count = std::fs::read_dir(&recent_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .map(|ext| ext == "parquet")
                    .unwrap_or(false)
            })
            .filter(|e| !e.file_name().to_string_lossy().starts_with("_seed"))
            .count();
        assert_eq!(recent_count, 3, "Should have 3 files in recent");

        // Archive with 0 days (archive everything)
        let stats = store.archive_old_data(0, false).unwrap();

        // Archives 4 data types: invocations, outputs, sessions, events
        // Only invocations has data, but all 4 partitions exist
        assert!(stats.partitions_archived >= 1, "Should archive at least 1 partition");
        assert!(stats.files_moved > 0, "Should move files");

        // Verify files moved to archive
        let archive_dir = store
            .config()
            .archive_dir()
            .join("invocations")
            .join(format!("date={}", date));
        assert!(archive_dir.exists(), "Archive partition should exist");

        // Archive consolidates to single file
        let archive_files: Vec<_> = std::fs::read_dir(&archive_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .map(|ext| ext == "parquet")
                    .unwrap_or(false)
            })
            .collect();
        assert_eq!(archive_files.len(), 1, "Archive should have 1 consolidated file");

        // Recent partition should be removed or empty (only seed files remain)
        let remaining = std::fs::read_dir(&recent_dir)
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok())
                    .filter(|e| !e.file_name().to_string_lossy().starts_with("_seed"))
                    .count()
            })
            .unwrap_or(0);
        assert_eq!(remaining, 0, "Recent partition should have no data files");
    }

    #[test]
    fn test_archive_dry_run() {
        let (_tmp, store) = setup_store();

        // Write data
        for i in 0..3 {
            let record = InvocationRecord::new(
                "test-session",
                format!("command-{}", i),
                "/home/user",
                0,
                "test@client",
            );
            store.write_invocation(&record).unwrap();
        }

        // Dry run should report stats but not move files
        let stats = store.archive_old_data(0, true).unwrap();
        // Archives 4 data types, but counts partitions with files
        assert!(stats.partitions_archived >= 1, "Should report at least 1 partition");
        assert!(stats.files_moved > 0, "Should report files to move");

        // Files should still be in recent
        let date = chrono::Utc::now().date_naive();
        let recent_dir = store.config().invocations_dir(&date);
        let recent_count = std::fs::read_dir(&recent_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .map(|ext| ext == "parquet")
                    .unwrap_or(false)
            })
            .filter(|e| !e.file_name().to_string_lossy().starts_with("_seed"))
            .count();
        assert_eq!(recent_count, 3, "Files should still be in recent after dry run");
    }

    #[test]
    fn test_archive_respects_age_threshold() {
        let (_tmp, store) = setup_store();

        // Write data today
        let record = InvocationRecord::new(
            "test-session",
            "echo hello",
            "/home/user",
            0,
            "test@client",
        );
        store.write_invocation(&record).unwrap();

        // Archive with 7 days threshold - today's data should NOT be archived
        let stats = store.archive_old_data(7, false).unwrap();
        assert_eq!(stats.partitions_archived, 0, "Today's data should not be archived with 7 day threshold");

        // Verify data is still queryable (via filesystem check, not view)
        let date = chrono::Utc::now().date_naive();
        let recent_dir = store.config().invocations_dir(&date);
        let file_count = std::fs::read_dir(&recent_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| !e.file_name().to_string_lossy().starts_with("_seed"))
            .count();
        assert_eq!(file_count, 1, "Data file should still exist");
    }

    // ===== Consolidation Tests =====

    #[test]
    fn test_consolidate_merges_all_files() {
        let (_tmp, store) = setup_store();

        // Write multiple files
        for i in 0..5 {
            let record = InvocationRecord::new(
                "test-session",
                format!("command-{}", i),
                "/home/user",
                0,
                "test@client",
            );
            store.write_invocation(&record).unwrap();
        }

        // Compact first to create a compacted file
        store.compact_recent(2, false).unwrap();

        // Now consolidate everything
        let opts = CompactOptions {
            consolidate: true,
            ..Default::default()
        };
        let stats = store.compact_recent_with_opts(&opts).unwrap();

        // Should consolidate all files (compacted + remaining) into one
        assert!(stats.sessions_compacted > 0, "Should consolidate session files");

        // Verify single file remains
        let date = chrono::Utc::now().date_naive();
        let inv_dir = store.config().invocations_dir(&date);
        let file_count = std::fs::read_dir(&inv_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path()
                    .extension()
                    .map(|ext| ext == "parquet")
                    .unwrap_or(false)
            })
            .filter(|e| !e.file_name().to_string_lossy().starts_with("_seed"))
            .count();
        assert_eq!(file_count, 1, "Should have single consolidated file");
    }

    #[test]
    fn test_recompact_threshold() {
        let (_tmp, store) = setup_store();

        // Create many compacted files by doing multiple compact cycles
        // First, write and compact several batches
        for batch in 0..3 {
            for i in 0..5 {
                let record = InvocationRecord::new(
                    "test-session",
                    format!("batch-{}-cmd-{}", batch, i),
                    "/home/user",
                    0,
                    "test@client",
                );
                store.write_invocation(&record).unwrap();
            }
            // Compact after each batch (creates compacted files)
            store.compact_recent(2, false).unwrap();
        }

        // Count compacted files
        let date = chrono::Utc::now().date_naive();
        let inv_dir = store.config().invocations_dir(&date);
        let compacted_count = std::fs::read_dir(&inv_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| is_compacted_file(&e.file_name().to_string_lossy()))
            .count();

        // With recompact_threshold=2, should trigger re-compaction
        let opts = CompactOptions {
            file_threshold: 50, // High threshold so we don't compact non-compacted
            recompact_threshold: 2,
            ..Default::default()
        };
        let stats = store.compact_recent_with_opts(&opts).unwrap();

        // If we had enough compacted files, should have re-compacted
        if compacted_count >= 2 {
            assert!(
                stats.sessions_compacted > 0 || stats.files_before > 0,
                "Should trigger re-compaction when threshold exceeded"
            );
        }
    }

    #[test]
    fn test_auto_compact_parquet_mode() {
        let (_tmp, store) = setup_store();

        // Write enough files to trigger compaction
        for i in 0..10 {
            let record = InvocationRecord::new(
                "test-session",
                format!("command-{}", i),
                "/home/user",
                0,
                "test@client",
            );
            store.write_invocation(&record).unwrap();
        }

        // Verify files before compact
        let date = chrono::Utc::now().date_naive();
        let inv_dir = store.config().invocations_dir(&date);
        let files_before = std::fs::read_dir(&inv_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| !e.file_name().to_string_lossy().starts_with("_seed"))
            .count();
        assert_eq!(files_before, 10, "Should have 10 files before compact");

        // Run auto_compact with low threshold
        let opts = AutoCompactOptions {
            compact: CompactOptions {
                file_threshold: 3,
                ..Default::default()
            },
            archive_days: 14, // Don't archive today's data
        };
        let (compact_stats, archive_stats) = store.auto_compact(&opts).unwrap();

        // Should compact but not archive (data is from today)
        assert!(compact_stats.sessions_compacted > 0, "Should compact files");
        assert_eq!(archive_stats.partitions_archived, 0, "Should not archive today's data");

        // Verify files were compacted (fewer files now)
        let files_after = std::fs::read_dir(&inv_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| !e.file_name().to_string_lossy().starts_with("_seed"))
            .count();
        assert!(files_after < files_before, "Should have fewer files after compact");
    }

    #[test]
    fn test_compact_preserves_data_integrity() {
        let (_tmp, store) = setup_store();

        // Write known data
        let commands: Vec<String> = (0..10).map(|i| format!("command-{}", i)).collect();
        for cmd in &commands {
            let record = InvocationRecord::new(
                "test-session",
                cmd.clone(),
                "/home/user",
                0,
                "test@client",
            );
            store.write_invocation(&record).unwrap();
        }

        // Verify data before compact
        let conn = store.connection().unwrap();
        let count_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM local.invocations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count_before, 10);

        // Compact
        store.compact_recent(2, false).unwrap();

        // Verify data after compact - count should be same
        let count_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM local.invocations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count_after, 10, "Compaction should preserve all records");

        // Verify all commands are present
        let mut found_cmds: Vec<String> = conn
            .prepare("SELECT cmd FROM local.invocations ORDER BY cmd")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        found_cmds.sort();
        let mut expected = commands.clone();
        expected.sort();
        assert_eq!(found_cmds, expected, "All commands should be preserved");
    }
}
