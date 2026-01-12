//! Remote sync operations (push/pull).
//!
//! Provides functionality to sync data between local and remote DuckDB databases.

use chrono::{NaiveDate, TimeDelta, Utc};
use duckdb::Connection;

use crate::{Error, RemoteConfig, Result};

/// Statistics from a push operation.
#[derive(Debug, Default)]
pub struct PushStats {
    pub sessions: usize,
    pub invocations: usize,
    pub outputs: usize,
    pub events: usize,
}

impl std::fmt::Display for PushStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} sessions, {} invocations, {} outputs, {} events",
            self.sessions, self.invocations, self.outputs, self.events
        )
    }
}

/// Statistics from a pull operation.
#[derive(Debug, Default)]
pub struct PullStats {
    pub sessions: usize,
    pub invocations: usize,
    pub outputs: usize,
    pub events: usize,
}

impl std::fmt::Display for PullStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} sessions, {} invocations, {} outputs, {} events",
            self.sessions, self.invocations, self.outputs, self.events
        )
    }
}

/// Options for push operation.
#[derive(Debug, Default)]
pub struct PushOptions {
    /// Only push data since this date.
    pub since: Option<NaiveDate>,
    /// Show what would be pushed without actually pushing.
    pub dry_run: bool,
}

/// Options for pull operation.
#[derive(Debug, Default)]
pub struct PullOptions {
    /// Only pull data since this date.
    pub since: Option<NaiveDate>,
    /// Only pull data from this client.
    pub client_id: Option<String>,
}

/// Parse a "since" string into a date.
///
/// Supports:
/// - Duration: "7d", "2w", "1m" (days, weeks, months)
/// - Date: "2024-01-15"
pub fn parse_since(s: &str) -> Result<NaiveDate> {
    let s = s.trim();

    // Try duration first (7d, 2w, 1m)
    if let Some(days) = parse_duration_days(s) {
        let date = Utc::now().date_naive() - TimeDelta::days(days);
        return Ok(date);
    }

    // Try date format (YYYY-MM-DD)
    NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .map_err(|e| Error::Config(format!("Invalid date '{}': {}", s, e)))
}

/// Parse a duration string into days.
fn parse_duration_days(s: &str) -> Option<i64> {
    let s = s.trim().to_lowercase();

    if let Some(num) = s.strip_suffix('d') {
        num.parse::<i64>().ok()
    } else if let Some(num) = s.strip_suffix('w') {
        num.parse::<i64>().ok().map(|n| n * 7)
    } else if let Some(num) = s.strip_suffix('m') {
        num.parse::<i64>().ok().map(|n| n * 30)
    } else {
        None
    }
}

impl super::Store {
    /// Push local data to a remote.
    ///
    /// Syncs sessions, invocations, outputs, and events in dependency order.
    /// Only pushes records that don't already exist on the remote (by id).
    pub fn push(&self, remote: &RemoteConfig, opts: PushOptions) -> Result<PushStats> {
        // Use connection without auto-attach to avoid conflicts and unnecessary views
        let conn = self.connection_with_options(false)?;

        // Attach only the target remote
        self.attach_remote(&conn, remote)?;

        let schema = remote.quoted_schema_name();

        // Ensure remote has the required tables
        ensure_remote_schema(&conn, &schema)?;

        let mut stats = PushStats::default();

        if opts.dry_run {
            // Count what would be pushed
            stats.sessions = count_sessions_to_push(&conn, &schema, opts.since)?;
            stats.invocations = count_table_to_push(&conn, "invocations", &schema, opts.since)?;
            stats.outputs = count_table_to_push(&conn, "outputs", &schema, opts.since)?;
            stats.events = count_table_to_push(&conn, "events", &schema, opts.since)?;
        } else {
            // Actually push in dependency order
            stats.sessions = push_sessions(&conn, &schema, opts.since)?;
            stats.invocations = push_table(&conn, "invocations", &schema, opts.since)?;
            stats.outputs = push_table(&conn, "outputs", &schema, opts.since)?;
            stats.events = push_table(&conn, "events", &schema, opts.since)?;
        }

        Ok(stats)
    }

    /// Pull data from a remote to local.
    ///
    /// Syncs sessions, invocations, outputs, and events in dependency order.
    /// Only pulls records that don't already exist locally (by id).
    pub fn pull(&self, remote: &RemoteConfig, opts: PullOptions) -> Result<PullStats> {
        // Use connection without auto-attach to avoid conflicts
        let conn = self.connection_with_options(false)?;

        // Attach only the target remote
        self.attach_remote(&conn, remote)?;

        let schema = remote.quoted_schema_name();

        let mut stats = PullStats::default();

        // Pull in dependency order
        stats.sessions = pull_sessions(&conn, &schema, opts.since, opts.client_id.as_deref())?;
        stats.invocations = pull_table(&conn, "invocations", &schema, opts.since, opts.client_id.as_deref())?;
        stats.outputs = pull_table(&conn, "outputs", &schema, opts.since, opts.client_id.as_deref())?;
        stats.events = pull_table(&conn, "events", &schema, opts.since, opts.client_id.as_deref())?;

        Ok(stats)
    }
}

/// Ensure the remote schema has the required tables.
fn ensure_remote_schema(conn: &Connection, schema: &str) -> Result<()> {
    // Create tables in the remote schema (idempotent with IF NOT EXISTS)
    let sql = format!(
        r#"
        CREATE TABLE IF NOT EXISTS {schema}.invocations_table (
            id UUID, session_id VARCHAR, timestamp TIMESTAMP, duration_ms BIGINT,
            cwd VARCHAR, cmd VARCHAR, executable VARCHAR, exit_code INTEGER,
            format_hint VARCHAR, client_id VARCHAR, hostname VARCHAR, username VARCHAR, date DATE
        );
        CREATE TABLE IF NOT EXISTS {schema}.outputs_table (
            id UUID, invocation_id UUID, stream VARCHAR, content_hash VARCHAR,
            byte_length BIGINT, storage_type VARCHAR, storage_ref VARCHAR,
            content_type VARCHAR, date DATE
        );
        CREATE TABLE IF NOT EXISTS {schema}.sessions_table (
            session_id VARCHAR, client_id VARCHAR, invoker VARCHAR, invoker_pid INTEGER,
            invoker_type VARCHAR, registered_at TIMESTAMP, cwd VARCHAR, date DATE
        );
        CREATE TABLE IF NOT EXISTS {schema}.events_table (
            id UUID, invocation_id UUID, client_id VARCHAR, hostname VARCHAR,
            event_type VARCHAR, severity VARCHAR, ref_file VARCHAR, ref_line INTEGER,
            ref_column INTEGER, message VARCHAR, error_code VARCHAR, test_name VARCHAR,
            status VARCHAR, format_used VARCHAR, date DATE
        );
        -- Views for convenience
        CREATE OR REPLACE VIEW {schema}.invocations AS SELECT * FROM {schema}.invocations_table;
        CREATE OR REPLACE VIEW {schema}.outputs AS SELECT * FROM {schema}.outputs_table;
        CREATE OR REPLACE VIEW {schema}.sessions AS SELECT * FROM {schema}.sessions_table;
        CREATE OR REPLACE VIEW {schema}.events AS SELECT * FROM {schema}.events_table;
        "#,
        schema = schema
    );
    conn.execute_batch(&sql)?;
    Ok(())
}

/// Build the WHERE clause for time filtering.
fn since_clause(since: Option<NaiveDate>, timestamp_col: &str) -> String {
    since
        .map(|d| format!("AND {} >= '{}'", timestamp_col, d))
        .unwrap_or_default()
}

/// Build the WHERE clause for client filtering.
fn client_clause(client_id: Option<&str>) -> String {
    client_id
        .map(|c| format!("AND client_id = '{}'", c.replace('\'', "''")))
        .unwrap_or_default()
}

/// Count sessions that would be pushed.
fn count_sessions_to_push(
    conn: &Connection,
    remote_schema: &str,
    since: Option<NaiveDate>,
) -> Result<usize> {
    let since_filter = since_clause(since, "i.timestamp");

    let sql = format!(
        r#"
        SELECT COUNT(DISTINCT s.session_id)
        FROM main.sessions s
        JOIN main.invocations i ON i.session_id = s.session_id
        WHERE NOT EXISTS (
            SELECT 1 FROM {remote}.sessions r WHERE r.session_id = s.session_id
        )
        {since}
        "#,
        remote = remote_schema,
        since = since_filter,
    );

    let count: i64 = conn.query_row(&sql, [], |row| row.get(0))?;
    Ok(count as usize)
}

/// Count records that would be pushed for a table.
fn count_table_to_push(
    conn: &Connection,
    table: &str,
    remote_schema: &str,
    since: Option<NaiveDate>,
) -> Result<usize> {
    // Different tables have different timestamp handling
    let sql = match table {
        "invocations" => {
            let since_filter = since_clause(since, "l.timestamp");
            format!(
                r#"
                SELECT COUNT(*)
                FROM main.{table} l
                WHERE NOT EXISTS (
                    SELECT 1 FROM {remote}.{table} r WHERE r.id = l.id
                )
                {since}
                "#,
                table = table,
                remote = remote_schema,
                since = since_filter,
            )
        }
        "outputs" | "events" => {
            // Join to invocations for timestamp filtering
            let since_filter = since_clause(since, "i.timestamp");
            format!(
                r#"
                SELECT COUNT(*)
                FROM main.{table} l
                JOIN main.invocations i ON i.id = l.invocation_id
                WHERE NOT EXISTS (
                    SELECT 1 FROM {remote}.{table} r WHERE r.id = l.id
                )
                {since}
                "#,
                table = table,
                remote = remote_schema,
                since = since_filter,
            )
        }
        _ => {
            // Generic case (no timestamp filter)
            format!(
                r#"
                SELECT COUNT(*)
                FROM main.{table} l
                WHERE NOT EXISTS (
                    SELECT 1 FROM {remote}.{table} r WHERE r.id = l.id
                )
                "#,
                table = table,
                remote = remote_schema,
            )
        }
    };

    let count: i64 = conn.query_row(&sql, [], |row| row.get(0))?;
    Ok(count as usize)
}

/// Push sessions that are referenced by invocations being pushed.
fn push_sessions(
    conn: &Connection,
    remote_schema: &str,
    since: Option<NaiveDate>,
) -> Result<usize> {
    let since_filter = since_clause(since, "i.timestamp");

    let sql = format!(
        r#"
        INSERT INTO {remote}.sessions_table
        SELECT DISTINCT s.*
        FROM main.sessions s
        JOIN main.invocations i ON i.session_id = s.session_id
        WHERE NOT EXISTS (
            SELECT 1 FROM {remote}.sessions r WHERE r.session_id = s.session_id
        )
        {since}
        "#,
        remote = remote_schema,
        since = since_filter,
    );

    let count = conn.execute(&sql, [])?;
    Ok(count)
}

/// Push records from a table to remote.
fn push_table(
    conn: &Connection,
    table: &str,
    remote_schema: &str,
    since: Option<NaiveDate>,
) -> Result<usize> {
    // Different tables have different timestamp handling
    let sql = match table {
        "invocations" => {
            let since_filter = since_clause(since, "l.timestamp");
            format!(
                r#"
                INSERT INTO {remote}.{table}_table
                SELECT *
                FROM main.{table} l
                WHERE NOT EXISTS (
                    SELECT 1 FROM {remote}.{table} r WHERE r.id = l.id
                )
                {since}
                "#,
                table = table,
                remote = remote_schema,
                since = since_filter,
            )
        }
        "outputs" | "events" => {
            // Join to invocations for timestamp filtering
            let since_filter = since_clause(since, "i.timestamp");
            format!(
                r#"
                INSERT INTO {remote}.{table}_table
                SELECT l.*
                FROM main.{table} l
                JOIN main.invocations i ON i.id = l.invocation_id
                WHERE NOT EXISTS (
                    SELECT 1 FROM {remote}.{table} r WHERE r.id = l.id
                )
                {since}
                "#,
                table = table,
                remote = remote_schema,
                since = since_filter,
            )
        }
        _ => {
            // Generic case (no timestamp filter)
            format!(
                r#"
                INSERT INTO {remote}.{table}_table
                SELECT *
                FROM main.{table} l
                WHERE NOT EXISTS (
                    SELECT 1 FROM {remote}.{table} r WHERE r.id = l.id
                )
                "#,
                table = table,
                remote = remote_schema,
            )
        }
    };

    let count = conn.execute(&sql, [])?;
    Ok(count)
}

/// Pull sessions from remote.
fn pull_sessions(
    conn: &Connection,
    remote_schema: &str,
    since: Option<NaiveDate>,
    client_id: Option<&str>,
) -> Result<usize> {
    let since_filter = since_clause(since, "r.registered_at");
    let client_filter = client_clause(client_id);

    let sql = format!(
        r#"
        INSERT INTO main.sessions_table
        SELECT *
        FROM {remote}.sessions r
        WHERE NOT EXISTS (
            SELECT 1 FROM main.sessions l WHERE l.session_id = r.session_id
        )
        {since}
        {client}
        "#,
        remote = remote_schema,
        since = since_filter,
        client = client_filter,
    );

    let count = conn.execute(&sql, [])?;
    Ok(count)
}

/// Pull records from a remote table to local.
fn pull_table(
    conn: &Connection,
    table: &str,
    remote_schema: &str,
    since: Option<NaiveDate>,
    client_id: Option<&str>,
) -> Result<usize> {
    let client_filter = client_clause(client_id);

    // Different tables have different timestamp handling
    let sql = match table {
        "invocations" => {
            let since_filter = since_clause(since, "r.timestamp");
            format!(
                r#"
                INSERT INTO main.{table}_table
                SELECT *
                FROM {remote}.{table} r
                WHERE NOT EXISTS (
                    SELECT 1 FROM main.{table} l WHERE l.id = r.id
                )
                {since}
                {client}
                "#,
                table = table,
                remote = remote_schema,
                since = since_filter,
                client = client_filter,
            )
        }
        "outputs" | "events" => {
            // Join to invocations for timestamp filtering
            let since_filter = since_clause(since, "i.timestamp");
            format!(
                r#"
                INSERT INTO main.{table}_table
                SELECT r.*
                FROM {remote}.{table} r
                JOIN {remote}.invocations i ON i.id = r.invocation_id
                WHERE NOT EXISTS (
                    SELECT 1 FROM main.{table} l WHERE l.id = r.id
                )
                {since}
                {client}
                "#,
                table = table,
                remote = remote_schema,
                since = since_filter,
                client = if client_id.is_some() {
                    format!("AND i.client_id = '{}'", client_id.unwrap().replace('\'', "''"))
                } else {
                    String::new()
                },
            )
        }
        _ => {
            // Generic case (no timestamp filter)
            format!(
                r#"
                INSERT INTO main.{table}_table
                SELECT *
                FROM {remote}.{table} r
                WHERE NOT EXISTS (
                    SELECT 1 FROM main.{table} l WHERE l.id = r.id
                )
                {client}
                "#,
                table = table,
                remote = remote_schema,
                client = client_filter,
            )
        }
    };

    let count = conn.execute(&sql, [])?;
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_since_days() {
        let today = Utc::now().date_naive();
        let result = parse_since("7d").unwrap();
        assert_eq!(result, today - TimeDelta::days(7));
    }

    #[test]
    fn test_parse_since_weeks() {
        let today = Utc::now().date_naive();
        let result = parse_since("2w").unwrap();
        assert_eq!(result, today - TimeDelta::days(14));
    }

    #[test]
    fn test_parse_since_months() {
        let today = Utc::now().date_naive();
        let result = parse_since("1m").unwrap();
        assert_eq!(result, today - TimeDelta::days(30));
    }

    #[test]
    fn test_parse_since_date() {
        let result = parse_since("2024-01-15").unwrap();
        assert_eq!(result, NaiveDate::from_ymd_opt(2024, 1, 15).unwrap());
    }

    #[test]
    fn test_parse_since_invalid() {
        assert!(parse_since("invalid").is_err());
    }
}
