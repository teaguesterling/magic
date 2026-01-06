//! Event storage operations - parsing and querying log events.

use std::fs;

use chrono::NaiveDate;
use duckdb::params;
use serde::Deserialize;
use uuid::Uuid;

use super::atomic;
use super::Store;
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

/// Simple glob-like pattern matching for command strings.
/// Supports `*` as wildcard matching any characters.
fn pattern_matches(pattern: &str, text: &str) -> bool {
    // Split pattern by * and check if all parts appear in order
    let parts: Vec<&str> = pattern.split('*').collect();

    if parts.is_empty() {
        return true;
    }

    // Handle edge cases
    if parts.len() == 1 {
        // No wildcards - exact match
        return pattern == text;
    }

    let mut pos = 0;

    // First part must match at start (if not empty)
    if !parts[0].is_empty() {
        if !text.starts_with(parts[0]) {
            return false;
        }
        pos = parts[0].len();
    }

    // Middle parts must appear in order
    for part in &parts[1..parts.len() - 1] {
        if part.is_empty() {
            continue;
        }
        if let Some(found) = text[pos..].find(part) {
            pos += found + part.len();
        } else {
            return false;
        }
    }

    // Last part must match at end (if not empty)
    let last = parts[parts.len() - 1];
    if !last.is_empty() {
        if !text[pos..].ends_with(last) {
            return false;
        }
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

    /// Detect format for a command using the config rules.
    pub fn detect_format(&self, cmd: &str) -> Result<String> {
        let config = self.load_format_config()?;
        Ok(config.detect_format(cmd))
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

        // Collect content from all outputs
        let mut all_content = String::new();
        for (storage_type, storage_ref) in &outputs {
            let content = match storage_type.as_str() {
                "inline" => {
                    // Decode base64 from data: URL
                    if let Some(b64_part) = storage_ref.split(',').nth(1) {
                        use base64::Engine;
                        base64::engine::general_purpose::STANDARD
                            .decode(b64_part)
                            .ok()
                            .and_then(|bytes| String::from_utf8(bytes).ok())
                    } else {
                        None
                    }
                }
                "blob" => {
                    // Read from file
                    if let Some(rel_path) = storage_ref.strip_prefix("file://") {
                        let abs_path = self.config.data_dir().join(rel_path);
                        std::fs::read_to_string(&abs_path).ok()
                    } else {
                        None
                    }
                }
                _ => None,
            };
            if let Some(c) = content {
                all_content.push_str(&c);
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

        Ok(count as usize)
    }

    /// Write event records to the store.
    pub fn write_events(&self, records: &[EventRecord]) -> Result<()> {
        if records.is_empty() {
            return Ok(());
        }

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
    pub fn delete_events_for_invocation(&self, invocation_id: &str) -> Result<usize> {
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
