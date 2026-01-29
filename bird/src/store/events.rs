//! Event storage operations - parsing and querying log events.

use std::fs;

use chrono::NaiveDate;
use duckdb::params;
use serde::Deserialize;
use uuid::Uuid;

use super::atomic;
use super::Store;
use crate::config::StorageMode;
use crate::schema::EventRecord;
use crate::{Error, Result};

/// A format detection rule from event-formats.toml.
#[derive(Debug, Clone, Deserialize)]
pub struct FormatRule {
    /// Glob pattern to match against command string.
    pub pattern: String,
    /// Format to use if pattern matches.
    pub format: String,
}

/// Default format configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct DefaultFormat {
    /// Default format when no rules match.
    pub format: String,
}

/// Event format configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct FormatConfig {
    /// List of format detection rules.
    #[serde(default)]
    pub rules: Vec<FormatRule>,
    /// Default format configuration.
    #[serde(default)]
    pub default: Option<DefaultFormat>,
}

impl Default for FormatConfig {
    fn default() -> Self {
        Self {
            rules: Vec::new(),
            default: Some(DefaultFormat {
                format: "auto".to_string(),
            }),
        }
    }
}

impl FormatConfig {
    /// Load format config from a TOML file.
    pub fn load(path: &std::path::Path) -> Result<Self> {
        if path.exists() {
            let contents = fs::read_to_string(path)?;
            toml::from_str(&contents)
                .map_err(|e| Error::Config(format!("Failed to parse event-formats.toml: {}", e)))
        } else {
            Ok(Self::default())
        }
    }

    /// Detect format for a command string.
    /// Patterns use simple glob-like matching:
    /// - `*` matches any characters (including none)
    /// - Patterns are case-sensitive
    pub fn detect_format(&self, cmd: &str) -> String {
        // Check rules in order
        for rule in &self.rules {
            if pattern_matches(&rule.pattern, cmd) {
                return rule.format.clone();
            }
        }

        // Fall back to default
        self.default
            .as_ref()
            .map(|d| d.format.clone())
            .unwrap_or_else(|| "auto".to_string())
    }
}

/// Convert a glob pattern to SQL LIKE pattern.
/// `*` becomes `%`, `?` becomes `_`, and special chars are escaped.
fn glob_to_like(pattern: &str) -> String {
    let mut result = String::with_capacity(pattern.len() + 10);
    for c in pattern.chars() {
        match c {
            '*' => result.push('%'),
            '?' => result.push('_'),
            '%' => result.push_str("\\%"),
            '_' => result.push_str("\\_"),
            '\\' => result.push_str("\\\\"),
            _ => result.push(c),
        }
    }
    result
}

/// Simple glob pattern matching for in-memory FormatConfig.
/// Used by FormatConfig::detect_format() when no database is available.
fn pattern_matches(pattern: &str, text: &str) -> bool {
    // Convert glob to regex-like matching
    let parts: Vec<&str> = pattern.split('*').collect();

    if parts.len() == 1 {
        return pattern == text;
    }

    // First part must match at start (if not empty)
    if !parts[0].is_empty() && !text.starts_with(parts[0]) {
        return false;
    }
    let mut pos = parts[0].len();

    // Middle parts must appear in order
    for part in &parts[1..parts.len() - 1] {
        if part.is_empty() {
            continue;
        }
        match text[pos..].find(part) {
            Some(found) => pos += found + part.len(),
            None => return false,
        }
    }

    // Last part must match at end (if not empty)
    let last = parts[parts.len() - 1];
    if !last.is_empty() && !text[pos..].ends_with(last) {
        return false;
    }

    true
}

/// Summary of an event (for listing).
#[derive(Debug)]
pub struct EventSummary {
    pub id: String,
    pub invocation_id: String,
    pub severity: Option<String>,
    pub message: Option<String>,
    pub ref_file: Option<String>,
    pub ref_line: Option<i32>,
    pub error_code: Option<String>,
    pub test_name: Option<String>,
    pub status: Option<String>,
}

/// Filters for querying events.
#[derive(Debug, Default)]
pub struct EventFilters {
    /// Filter by severity (error, warning, info, note).
    pub severity: Option<String>,
    /// Filter by invocation ID.
    pub invocation_id: Option<String>,
    /// Filter by multiple invocation IDs (for last_n queries).
    pub invocation_ids: Option<Vec<String>>,
    /// Filter by command pattern (glob).
    pub cmd_pattern: Option<String>,
    /// Filter by client ID.
    pub client_id: Option<String>,
    /// Filter by hostname.
    pub hostname: Option<String>,
    /// Filter by date range start.
    pub date_from: Option<NaiveDate>,
    /// Filter by date range end.
    pub date_to: Option<NaiveDate>,
    /// Maximum number of events to return.
    pub limit: Option<usize>,
}

impl Store {
    /// Load format config from BIRD_ROOT/event-formats.toml.
    pub fn load_format_config(&self) -> Result<FormatConfig> {
        FormatConfig::load(&self.config.event_formats_path())
    }

    /// Detect format for a command using DuckDB SQL matching.
    ///
    /// Uses SQL LIKE patterns for matching, which prepares for future
    /// integration with duck_hunt_match_command_patterns().
    pub fn detect_format(&self, cmd: &str) -> Result<String> {
        let config = self.load_format_config()?;

        // If no rules, fall back to default
        if config.rules.is_empty() {
            return Ok(config
                .default
                .as_ref()
                .map(|d| d.format.clone())
                .unwrap_or_else(|| "auto".to_string()));
        }

        let conn = self.connection()?;

        // Create temp table with rules (convert glob to LIKE patterns)
        conn.execute_batch("CREATE OR REPLACE TEMP TABLE format_rules (priority INT, pattern VARCHAR, format VARCHAR)")?;

        {
            let mut stmt = conn.prepare("INSERT INTO format_rules VALUES (?, ?, ?)")?;
            for (i, rule) in config.rules.iter().enumerate() {
                let like_pattern = glob_to_like(&rule.pattern);
                stmt.execute(params![i as i32, like_pattern, rule.format.clone()])?;
            }
        }

        // Query for matching format using SQL LIKE
        // This can be easily swapped with duck_hunt_match_command_patterns() later
        let result: std::result::Result<String, _> = conn.query_row(
            "SELECT format FROM format_rules WHERE ? LIKE pattern ORDER BY priority LIMIT 1",
            params![cmd],
            |row| row.get(0),
        );

        match result {
            Ok(format) => Ok(format),
            Err(_) => {
                // No match - fall back to default
                Ok(config
                    .default
                    .as_ref()
                    .map(|d| d.format.clone())
                    .unwrap_or_else(|| "auto".to_string()))
            }
        }
    }

    /// Extract events from an invocation's output using duck_hunt.
    ///
    /// Parses the stdout/stderr of an invocation and stores the extracted events.
    /// Returns the number of events extracted.
    pub fn extract_events(
        &self,
        invocation_id: &str,
        format_override: Option<&str>,
    ) -> Result<usize> {
        let conn = self.connection()?;

        // Get invocation info for format detection and metadata
        let (cmd, client_id, hostname, date): (String, String, Option<String>, String) = conn
            .query_row(
                "SELECT cmd, client_id, hostname, date::VARCHAR FROM invocations WHERE id = ?",
                params![invocation_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .map_err(|e| Error::NotFound(format!("Invocation {}: {}", invocation_id, e)))?;

        // Determine format to use
        let format = match format_override {
            Some(f) => f.to_string(),
            None => self.detect_format(&cmd)?,
        };

        // Get output info for this invocation (both stdout and stderr)
        let outputs: Vec<(String, String)> = {
            let mut stmt = conn.prepare(
                "SELECT storage_type, storage_ref FROM outputs WHERE invocation_id = ? AND stream IN ('stdout', 'stderr')",
            )?;
            let rows = stmt.query_map(params![invocation_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
            rows.filter_map(|r| r.ok()).collect()
        };

        if outputs.is_empty() {
            return Ok(0);
        }

        // Collect content from all outputs using DuckDB's read_blob (handles both data: and file:// URLs)
        let mut all_content = String::new();
        for (_storage_type, storage_ref) in &outputs {
            // Resolve relative file:// URLs to absolute paths
            let resolved_ref = if storage_ref.starts_with("file://") {
                let rel_path = storage_ref.strip_prefix("file://").unwrap();
                let abs_path = self.config.data_dir().join(rel_path);
                format!("file://{}", abs_path.display())
            } else {
                storage_ref.clone()
            };

            // Use scalarfs read_blob for unified content access
            let content: std::result::Result<Vec<u8>, _> = conn.query_row(
                "SELECT content FROM read_blob(?)",
                params![&resolved_ref],
                |row| row.get(0),
            );

            if let Ok(bytes) = content {
                if let Ok(text) = String::from_utf8(bytes) {
                    all_content.push_str(&text);
                }
            }
        }

        if all_content.is_empty() {
            return Ok(0);
        }

        // Parse the date
        let date = date
            .parse::<NaiveDate>()
            .map_err(|e| Error::Storage(format!("Invalid date: {}", e)))?;

        // Ensure the events partition directory exists
        let partition_dir = self.config.events_dir(&date);
        fs::create_dir_all(&partition_dir)?;

        // Create temp table for events
        conn.execute_batch(
            r#"
            CREATE OR REPLACE TEMP TABLE temp_events (
                id UUID,
                invocation_id UUID,
                client_id VARCHAR,
                hostname VARCHAR,
                event_type VARCHAR,
                severity VARCHAR,
                ref_file VARCHAR,
                ref_line INTEGER,
                ref_column INTEGER,
                message VARCHAR,
                error_code VARCHAR,
                test_name VARCHAR,
                status VARCHAR,
                format_used VARCHAR,
                date DATE
            );
            "#,
        )?;

        // Escape content for SQL (replace single quotes)
        let escaped_content = all_content.replace("'", "''");

        // Parse with duck_hunt using parse_duck_hunt_log (takes content directly)
        let sql = format!(
            r#"
            INSERT INTO temp_events
            SELECT
                uuid() as id,
                '{invocation_id}'::UUID as invocation_id,
                '{client_id}' as client_id,
                {hostname} as hostname,
                event_type,
                severity,
                ref_file,
                ref_line::INTEGER,
                ref_column::INTEGER,
                message,
                error_code,
                test_name,
                status,
                '{format}' as format_used,
                '{date}'::DATE as date
            FROM parse_duck_hunt_log('{content}', '{format}')
            WHERE event_type IS NOT NULL OR message IS NOT NULL;
            "#,
            invocation_id = invocation_id,
            client_id = client_id.replace("'", "''"),
            hostname = hostname
                .as_ref()
                .map(|h| format!("'{}'", h.replace("'", "''")))
                .unwrap_or_else(|| "NULL".to_string()),
            content = escaped_content,
            format = format.replace("'", "''"),
            date = date,
        );

        if let Err(e) = conn.execute_batch(&sql) {
            // duck_hunt might fail on some formats - log and continue
            eprintln!("Warning: duck_hunt parsing failed: {}", e);
            conn.execute("DROP TABLE IF EXISTS temp_events", [])?;
            return Ok(0);
        }

        // Count how many events were extracted
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM temp_events", [], |row| row.get(0))?;

        if count == 0 {
            conn.execute("DROP TABLE temp_events", [])?;
            return Ok(0);
        }

        // Write to storage based on mode
        match self.config.storage_mode {
            StorageMode::Parquet => {
                // Write to parquet file
                let filename = format!("{}--{}.parquet", invocation_id, Uuid::now_v7());
                let file_path = partition_dir.join(&filename);

                let temp_path = atomic::temp_path(&file_path);
                conn.execute(
                    &format!(
                        "COPY temp_events TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD)",
                        temp_path.display()
                    ),
                    [],
                )?;
                conn.execute("DROP TABLE temp_events", [])?;

                // Rename temp to final (atomic on POSIX)
                atomic::rename_into_place(&temp_path, &file_path)?;
            }
            StorageMode::DuckDB => {
                // Insert directly into local.events
                conn.execute_batch("INSERT INTO local.events SELECT * FROM temp_events")?;
                conn.execute("DROP TABLE temp_events", [])?;
            }
        }

        Ok(count as usize)
    }

    /// Write event records to the store.
    ///
    /// Behavior depends on storage mode:
    /// - Parquet: Creates Parquet files partitioned by date
    /// - DuckDB: Inserts directly into the local.events
    pub fn write_events(&self, records: &[EventRecord]) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }

        match self.config.storage_mode {
            StorageMode::Parquet => self.write_events_parquet(records),
            StorageMode::DuckDB => self.write_events_duckdb(records),
        }
    }

    /// Write events to Parquet files (multi-writer safe).
    fn write_events_parquet(&self, records: &[EventRecord]) -> Result<()> {
        let conn = self.connection()?;

        // Group by date for partitioning
        let mut by_date: std::collections::HashMap<NaiveDate, Vec<&EventRecord>> =
            std::collections::HashMap::new();
        for record in records {
            by_date.entry(record.date).or_default().push(record);
        }

        for (date, date_records) in by_date {
            let partition_dir = self.config.events_dir(&date);
            fs::create_dir_all(&partition_dir)?;

            // Create temp table
            conn.execute_batch(
                r#"
                CREATE OR REPLACE TEMP TABLE temp_events (
                    id UUID,
                    invocation_id UUID,
                    client_id VARCHAR,
                    hostname VARCHAR,
                    event_type VARCHAR,
                    severity VARCHAR,
                    ref_file VARCHAR,
                    ref_line INTEGER,
                    ref_column INTEGER,
                    message VARCHAR,
                    error_code VARCHAR,
                    test_name VARCHAR,
                    status VARCHAR,
                    format_used VARCHAR,
                    date DATE
                );
                "#,
            )?;

            // Insert records
            for record in &date_records {
                conn.execute(
                    r#"
                    INSERT INTO temp_events VALUES (
                        ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?
                    )
                    "#,
                    params![
                        record.id.to_string(),
                        record.invocation_id.to_string(),
                        record.client_id,
                        record.hostname,
                        record.event_type,
                        record.severity,
                        record.ref_file,
                        record.ref_line,
                        record.ref_column,
                        record.message,
                        record.error_code,
                        record.test_name,
                        record.status,
                        record.format_used,
                        date.to_string(),
                    ],
                )?;
            }

            // Write to parquet
            let filename = format!(
                "{}--{}.parquet",
                date_records[0].invocation_id,
                Uuid::now_v7()
            );
            let file_path = partition_dir.join(&filename);

            let temp_path = atomic::temp_path(&file_path);
            conn.execute(
                &format!(
                    "COPY temp_events TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD)",
                    temp_path.display()
                ),
                [],
            )?;
            conn.execute("DROP TABLE temp_events", [])?;

            atomic::rename_into_place(&temp_path, &file_path)?;
        }

        Ok(())
    }

    /// Write events directly to DuckDB table.
    fn write_events_duckdb(&self, records: &[EventRecord]) -> Result<()> {
        let conn = self.connection()?;

        for record in records {
            conn.execute(
                r#"
                INSERT INTO local.events VALUES (
                    ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?
                )
                "#,
                params![
                    record.id.to_string(),
                    record.invocation_id.to_string(),
                    record.client_id,
                    record.hostname,
                    record.event_type,
                    record.severity,
                    record.ref_file,
                    record.ref_line,
                    record.ref_column,
                    record.message,
                    record.error_code,
                    record.test_name,
                    record.status,
                    record.format_used,
                    record.date.to_string(),
                ],
            )?;
        }

        Ok(())
    }

    /// Query events with optional filters.
    pub fn query_events(&self, filters: &EventFilters) -> Result<Vec<EventSummary>> {
        let conn = self.connection()?;

        // Build WHERE clause
        let mut conditions = Vec::new();

        if let Some(ref sev) = filters.severity {
            conditions.push(format!("e.severity = '{}'", sev.replace("'", "''")));
        }

        if let Some(ref inv_id) = filters.invocation_id {
            conditions.push(format!(
                "e.invocation_id = '{}'",
                inv_id.replace("'", "''")
            ));
        }

        if let Some(ref inv_ids) = filters.invocation_ids {
            if !inv_ids.is_empty() {
                let ids_list: Vec<String> = inv_ids
                    .iter()
                    .map(|id| format!("'{}'", id.replace("'", "''")))
                    .collect();
                conditions.push(format!("e.invocation_id IN ({})", ids_list.join(", ")));
            }
        }

        if let Some(ref client) = filters.client_id {
            conditions.push(format!("e.client_id = '{}'", client.replace("'", "''")));
        }

        if let Some(ref host) = filters.hostname {
            conditions.push(format!("e.hostname = '{}'", host.replace("'", "''")));
        }

        if let Some(ref date_from) = filters.date_from {
            conditions.push(format!("e.date >= '{}'", date_from));
        }

        if let Some(ref date_to) = filters.date_to {
            conditions.push(format!("e.date <= '{}'", date_to));
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };

        let limit_clause = filters
            .limit
            .map(|l| format!("LIMIT {}", l))
            .unwrap_or_default();

        // If filtering by command pattern, we need to join with invocations
        let sql = if filters.cmd_pattern.is_some() {
            let cmd_pattern = filters.cmd_pattern.as_ref().unwrap().replace("'", "''");
            format!(
                r#"
                SELECT
                    e.id::VARCHAR,
                    e.invocation_id::VARCHAR,
                    e.severity,
                    e.message,
                    e.ref_file,
                    e.ref_line,
                    e.error_code,
                    e.test_name,
                    e.status
                FROM events e
                JOIN invocations i ON e.invocation_id = i.id
                {}
                {} i.cmd LIKE '{}'
                ORDER BY i.timestamp DESC
                {}
                "#,
                if conditions.is_empty() {
                    "WHERE"
                } else {
                    &format!("{} AND", where_clause)
                },
                if conditions.is_empty() { "" } else { "" },
                cmd_pattern,
                limit_clause
            )
        } else {
            format!(
                r#"
                SELECT
                    e.id::VARCHAR,
                    e.invocation_id::VARCHAR,
                    e.severity,
                    e.message,
                    e.ref_file,
                    e.ref_line,
                    e.error_code,
                    e.test_name,
                    e.status
                FROM events e
                {}
                ORDER BY e.date DESC
                {}
                "#,
                where_clause, limit_clause
            )
        };

        let mut stmt = match conn.prepare(&sql) {
            Ok(stmt) => stmt,
            Err(e) => {
                if e.to_string().contains("No files found") {
                    return Ok(Vec::new());
                }
                return Err(e.into());
            }
        };

        let rows = stmt.query_map([], |row| {
            Ok(EventSummary {
                id: row.get(0)?,
                invocation_id: row.get(1)?,
                severity: row.get(2)?,
                message: row.get(3)?,
                ref_file: row.get(4)?,
                ref_line: row.get(5)?,
                error_code: row.get(6)?,
                test_name: row.get(7)?,
                status: row.get(8)?,
            })
        });

        match rows {
            Ok(rows) => {
                let mut results = Vec::new();
                for row in rows {
                    results.push(row?);
                }
                Ok(results)
            }
            Err(e) => {
                if e.to_string().contains("No files found") {
                    Ok(Vec::new())
                } else {
                    Err(e.into())
                }
            }
        }
    }

    /// Count events matching the given filters.
    pub fn event_count(&self, filters: &EventFilters) -> Result<i64> {
        let conn = self.connection()?;

        let mut conditions = Vec::new();

        if let Some(ref sev) = filters.severity {
            conditions.push(format!("severity = '{}'", sev.replace("'", "''")));
        }

        if let Some(ref inv_id) = filters.invocation_id {
            conditions.push(format!(
                "invocation_id = '{}'",
                inv_id.replace("'", "''")
            ));
        }

        if let Some(ref inv_ids) = filters.invocation_ids {
            if !inv_ids.is_empty() {
                let ids_list: Vec<String> = inv_ids
                    .iter()
                    .map(|id| format!("'{}'", id.replace("'", "''")))
                    .collect();
                conditions.push(format!("invocation_id IN ({})", ids_list.join(", ")));
            }
        }

        if let Some(ref client) = filters.client_id {
            conditions.push(format!("client_id = '{}'", client.replace("'", "''")));
        }

        let where_clause = if conditions.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", conditions.join(" AND "))
        };

        let sql = format!("SELECT COUNT(*) FROM events {}", where_clause);

        let result: std::result::Result<i64, _> = conn.query_row(&sql, [], |row| row.get(0));

        match result {
            Ok(count) => Ok(count),
            Err(e) => {
                if e.to_string().contains("No files found") {
                    Ok(0)
                } else {
                    Err(e.into())
                }
            }
        }
    }

    /// Delete events for an invocation (for re-extraction).
    ///
    /// Behavior depends on storage mode:
    /// - Parquet: Deletes parquet files containing the events
    /// - DuckDB: Deletes rows from local.events
    pub fn delete_events_for_invocation(&self, invocation_id: &str) -> Result<usize> {
        match self.config.storage_mode {
            StorageMode::Parquet => self.delete_events_parquet(invocation_id),
            StorageMode::DuckDB => self.delete_events_duckdb(invocation_id),
        }
    }

    /// Delete events from parquet files.
    fn delete_events_parquet(&self, invocation_id: &str) -> Result<usize> {
        // Since we're using parquet files, we need to find and delete the files
        // This is a simplified approach - in production you might want to rewrite
        // the parquet files without these records
        let conn = self.connection()?;

        // Get the date(s) for this invocation's events
        let dates: Vec<String> = {
            let sql = format!(
                "SELECT DISTINCT date::VARCHAR FROM events WHERE invocation_id = '{}'",
                invocation_id.replace("'", "''")
            );
            let mut stmt = match conn.prepare(&sql) {
                Ok(stmt) => stmt,
                Err(e) => {
                    if e.to_string().contains("No files found") {
                        return Ok(0);
                    }
                    return Err(e.into());
                }
            };
            let rows = stmt.query_map([], |row| row.get(0))?;
            rows.filter_map(|r| r.ok()).collect()
        };

        let mut deleted = 0;

        for date_str in dates {
            let date = date_str
                .parse::<NaiveDate>()
                .map_err(|e| Error::Storage(format!("Invalid date: {}", e)))?;

            let partition_dir = self.config.events_dir(&date);

            // Find and delete parquet files that start with this invocation_id
            if partition_dir.exists() {
                for entry in fs::read_dir(&partition_dir)? {
                    let entry = entry?;
                    let name = entry.file_name();
                    let name_str = name.to_string_lossy();
                    if name_str.starts_with(invocation_id) && name_str.ends_with(".parquet") {
                        fs::remove_file(entry.path())?;
                        deleted += 1;
                    }
                }
            }
        }

        Ok(deleted)
    }

    /// Delete events from DuckDB table.
    fn delete_events_duckdb(&self, invocation_id: &str) -> Result<usize> {
        let conn = self.connection()?;

        // Count events before deletion
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM local.events WHERE invocation_id = ?",
                params![invocation_id],
                |row| row.get(0),
            )
            .unwrap_or(0);

        if count > 0 {
            conn.execute(
                "DELETE FROM local.events WHERE invocation_id = ?",
                params![invocation_id],
            )?;
        }

        Ok(count as usize)
    }

    /// Get invocations that have outputs but no events extracted yet.
    ///
    /// Useful for backfilling events from existing invocations.
    pub fn invocations_without_events(
        &self,
        since: Option<NaiveDate>,
        limit: Option<usize>,
    ) -> Result<Vec<super::InvocationSummary>> {
        let conn = self.connection()?;

        // Default to last 30 days if not specified
        let since_date = since.unwrap_or_else(|| {
            chrono::Utc::now().date_naive() - chrono::Duration::days(30)
        });

        let limit_clause = limit
            .map(|l| format!("LIMIT {}", l))
            .unwrap_or_else(|| "LIMIT 1000".to_string());

        let sql = format!(
            r#"
            SELECT i.id::VARCHAR, i.cmd, i.exit_code, i.timestamp::VARCHAR, i.duration_ms
            FROM invocations i
            WHERE EXISTS (SELECT 1 FROM outputs o WHERE o.invocation_id = i.id)
              AND NOT EXISTS (SELECT 1 FROM events e WHERE e.invocation_id = i.id)
              AND i.date >= '{}'
            ORDER BY i.timestamp DESC
            {}
            "#,
            since_date, limit_clause
        );

        let mut stmt = match conn.prepare(&sql) {
            Ok(stmt) => stmt,
            Err(e) => {
                // Handle case where tables don't exist yet
                if e.to_string().contains("No files found") {
                    return Ok(Vec::new());
                }
                return Err(e.into());
            }
        };

        let rows = stmt.query_map([], |row| {
            Ok(super::InvocationSummary {
                id: row.get(0)?,
                cmd: row.get(1)?,
                exit_code: row.get(2)?,
                timestamp: row.get(3)?,
                duration_ms: row.get(4)?,
            })
        })?;

        let results: Vec<_> = rows.filter_map(|r| r.ok()).collect();
        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::initialize;
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
    fn test_format_config_detect() {
        let config = FormatConfig {
            rules: vec![
                FormatRule {
                    pattern: "*gcc*".to_string(),
                    format: "gcc".to_string(),
                },
                FormatRule {
                    pattern: "*cargo test*".to_string(),
                    format: "cargo_test_json".to_string(),
                },
            ],
            default: Some(DefaultFormat {
                format: "auto".to_string(),
            }),
        };

        assert_eq!(config.detect_format("gcc -o foo foo.c"), "gcc");
        assert_eq!(config.detect_format("/usr/bin/gcc main.c"), "gcc");
        assert_eq!(config.detect_format("cargo test --release"), "cargo_test_json");
        assert_eq!(config.detect_format("make test"), "auto");
    }

    #[test]
    fn test_glob_to_like() {
        use super::glob_to_like;

        // Basic wildcards
        assert_eq!(glob_to_like("*"), "%");
        assert_eq!(glob_to_like("?"), "_");
        assert_eq!(glob_to_like("*gcc*"), "%gcc%");
        assert_eq!(glob_to_like("cargo test*"), "cargo test%");
        assert_eq!(glob_to_like("*cargo test*"), "%cargo test%");

        // Escape special LIKE chars
        assert_eq!(glob_to_like("100%"), "100\\%");
        assert_eq!(glob_to_like("file_name"), "file\\_name");

        // Mixed
        assert_eq!(glob_to_like("*test?file*"), "%test_file%");
    }

    #[test]
    fn test_store_detect_format_sql() {
        let (tmp, store) = setup_store();

        // Write a config file with rules
        let config_path = tmp.path().join("event-formats.toml");
        std::fs::write(
            &config_path,
            r#"
[[rules]]
pattern = "*gcc*"
format = "gcc"

[[rules]]
pattern = "*cargo test*"
format = "cargo_test_json"

[[rules]]
pattern = "pytest*"
format = "pytest_json"

[default]
format = "auto"
"#,
        )
        .unwrap();

        // Test SQL-based detection
        assert_eq!(store.detect_format("gcc -o foo foo.c").unwrap(), "gcc");
        assert_eq!(store.detect_format("/usr/bin/gcc main.c").unwrap(), "gcc");
        assert_eq!(
            store.detect_format("cargo test --release").unwrap(),
            "cargo_test_json"
        );
        assert_eq!(store.detect_format("pytest tests/").unwrap(), "pytest_json");
        assert_eq!(store.detect_format("make test").unwrap(), "auto");
    }

    #[test]
    fn test_store_has_events_dir() {
        let (tmp, store) = setup_store();
        let date = chrono::Utc::now().date_naive();
        let events_dir = store.config().events_dir(&date);
        assert!(events_dir.starts_with(tmp.path()));
        assert!(events_dir.to_string_lossy().contains("events"));
    }

    #[test]
    fn test_query_events_empty() {
        let (_tmp, store) = setup_store();

        let events = store.query_events(&EventFilters::default()).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn test_event_count_empty() {
        let (_tmp, store) = setup_store();

        let count = store.event_count(&EventFilters::default()).unwrap();
        assert_eq!(count, 0);
    }
}
