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

/// Options for auto-compaction.
#[derive(Debug)]
pub struct AutoCompactOptions {
    /// Compact when a session has more than this many files.
    pub file_threshold: usize,
    /// Migrate data older than this many days to archive.
    pub archive_days: u32,
    /// If true, don't actually make changes.
    pub dry_run: bool,
    /// If set, only compact files for this specific session.
    pub session_filter: Option<String>,
}

impl Default for AutoCompactOptions {
    fn default() -> Self {
        Self {
            file_threshold: 50,
            archive_days: 14,
            dry_run: false,
            session_filter: None,
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

        let conn = self.connection()?;

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

    /// Compact files in a partition directory, grouped by session.
    ///
    /// For each session with more than `file_threshold` files, compacts oldest
    /// files while keeping the most recent ones.
    pub fn compact_partition(
        &self,
        partition_dir: &Path,
        file_threshold: usize,
        session_filter: Option<&str>,
        dry_run: bool,
    ) -> Result<CompactStats> {
        let mut total_stats = CompactStats::default();

        // Group files by session
        let mut session_files: HashMap<String, Vec<PathBuf>> = HashMap::new();

        for entry in fs::read_dir(partition_dir)? {
            let entry = entry?;
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();

            // Skip non-parquet, seed files, and already-compacted files
            if !name.ends_with(".parquet") || is_seed_file(&name) {
                continue;
            }

            // Extract session from filename
            if let Some(session) = extract_session(&name) {
                // Apply session filter if specified
                if let Some(filter) = session_filter {
                    if session != filter {
                        continue;
                    }
                }

                session_files.entry(session).or_default().push(path);
            }
        }

        // Compact each session that exceeds threshold
        let mut any_compacted = false;
        for (session, files) in session_files {
            // Include compacted files in count but don't re-compact them
            let non_compacted: Vec<PathBuf> = files
                .iter()
                .filter(|p| {
                    !is_compacted_file(&p.file_name().unwrap_or_default().to_string_lossy())
                })
                .cloned()
                .collect();

            if non_compacted.len() >= file_threshold {
                // Only compact non-compacted files, keep threshold files
                let mut to_process = non_compacted;
                let stats = self.compact_session_files(
                    partition_dir,
                    &session,
                    &mut to_process,
                    file_threshold,
                    dry_run,
                )?;
                if stats.sessions_compacted > 0 {
                    any_compacted = true;
                    total_stats.add(&stats);
                }
            }
        }

        if any_compacted {
            total_stats.partitions_compacted = 1;
        }

        Ok(total_stats)
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

        for data_type in &["invocations", "outputs", "sessions"] {
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

        for data_type in &["invocations", "outputs", "sessions"] {
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

        for data_type in &["invocations", "outputs", "sessions"] {
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

        for data_type in &["invocations", "outputs", "sessions"] {
            let data_dir = archive_dir.join(data_type);
            let stats = self.compact_data_type(&data_dir, file_threshold, None, dry_run)?;
            total_stats.add(&stats);
        }

        Ok(total_stats)
    }

    /// Migrate old data from recent to archive.
    ///
    /// Moves entire date partitions that are older than the specified days.
    pub fn archive_old_data(&self, older_than_days: u32, dry_run: bool) -> Result<ArchiveStats> {
        let mut stats = ArchiveStats::default();
        let cutoff_date = Utc::now().date_naive() - chrono::Duration::days(older_than_days as i64);

        let recent_dir = self.config().recent_dir();
        let archive_dir = self.config().archive_dir();

        for data_type in &["invocations", "outputs", "sessions"] {
            let recent_data_dir = recent_dir.join(data_type);
            let archive_data_dir = archive_dir.join(data_type);

            if !recent_data_dir.exists() {
                continue;
            }

            for entry in fs::read_dir(&recent_data_dir)? {
                let entry = entry?;
                let path = entry.path();

                if !path.is_dir() {
                    continue;
                }

                // Parse date from partition name (date=YYYY-MM-DD)
                let dir_name = entry.file_name().to_string_lossy().to_string();
                if let Some(date_str) = dir_name.strip_prefix("date=") {
                    if let Ok(date) = NaiveDate::parse_from_str(date_str, "%Y-%m-%d") {
                        if date < cutoff_date {
                            let dest_dir = archive_data_dir.join(&dir_name);

                            if dry_run {
                                // Count files for dry run
                                let file_count =
                                    fs::read_dir(&path)?.filter_map(|e| e.ok()).count();
                                let bytes: u64 = fs::read_dir(&path)?
                                    .filter_map(|e| e.ok())
                                    .filter_map(|e| fs::metadata(e.path()).ok())
                                    .map(|m| m.len())
                                    .sum();

                                stats.partitions_archived += 1;
                                stats.files_moved += file_count;
                                stats.bytes_moved += bytes;
                            } else {
                                // Create destination and move partition
                                fs::create_dir_all(&dest_dir)?;

                                let file_count = move_partition(&path, &dest_dir)?;
                                let bytes: u64 = fs::read_dir(&dest_dir)?
                                    .filter_map(|e| e.ok())
                                    .filter_map(|e| fs::metadata(e.path()).ok())
                                    .map(|m| m.len())
                                    .sum();

                                // Remove empty source directory
                                let _ = fs::remove_dir(&path);

                                stats.partitions_archived += 1;
                                stats.files_moved += file_count;
                                stats.bytes_moved += bytes;
                            }
                        }
                    }
                }
            }
        }

        Ok(stats)
    }

    /// Run auto-compaction based on options.
    pub fn auto_compact(&self, opts: &AutoCompactOptions) -> Result<(CompactStats, ArchiveStats)> {
        // First, archive old data (skip if doing session-specific compaction)
        let archive_stats = if opts.session_filter.is_none() {
            self.archive_old_data(opts.archive_days, opts.dry_run)?
        } else {
            ArchiveStats::default()
        };

        // Then compact based on mode
        let compact_stats = if let Some(ref session) = opts.session_filter {
            // Client-specific mode
            self.compact_for_session(session, opts.file_threshold, opts.dry_run)?
        } else {
            // Global mode: compact both recent and archive
            let mut stats = self.compact_recent(opts.file_threshold, opts.dry_run)?;
            let archive_compact = self.compact_archive(opts.file_threshold, opts.dry_run)?;
            stats.add(&archive_compact);
            stats
        };

        Ok((compact_stats, archive_stats))
    }
}

/// Move all files from source partition to destination.
fn move_partition(src: &Path, dest: &Path) -> Result<usize> {
    let mut count = 0;

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dest_path = dest.join(entry.file_name());

        fs::rename(&src_path, &dest_path)?;
        count += 1;
    }

    Ok(count)
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
}
