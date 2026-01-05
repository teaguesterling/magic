//! Store - handles writing and reading records.
//!
//! Uses DuckDB to write Parquet files and query across them.

mod atomic;
mod compact;
mod invocations;
mod outputs;
mod sessions;

use chrono::{DateTime, NaiveDate, NaiveTime, TimeDelta, Utc};
use duckdb::{
    types::{TimeUnit, ValueRef},
    Connection,
};

use crate::{Config, Error, Result};

// Re-export types from submodules
pub use compact::{ArchiveStats, AutoCompactOptions, CompactStats};
pub use invocations::InvocationSummary;
pub use outputs::OutputInfo;

/// A BIRD store for reading and writing records.
pub struct Store {
    config: Config,
}

impl Store {
    /// Open an existing BIRD store.
    pub fn open(config: Config) -> Result<Self> {
        if !config.db_path().exists() {
            return Err(Error::NotInitialized(config.bird_root.clone()));
        }
        Ok(Self { config })
    }

    /// Get a DuckDB connection to the store.
    pub fn connection(&self) -> Result<Connection> {
        let conn = Connection::open(&self.config.db_path())?;

        // Set custom extensions directory and load scalarfs
        conn.execute(
            &format!(
                "SET extension_directory = '{}'",
                self.config.extensions_dir().display()
            ),
            [],
        )?;
        conn.execute("SET allow_community_extensions = true", [])?;
        conn.execute("LOAD scalarfs", [])?;

        // Set file search path so views resolve relative paths correctly
        conn.execute(
            &format!(
                "SET file_search_path = '{}'",
                self.config.data_dir().display()
            ),
            [],
        )?;

        Ok(conn)
    }

    /// Get config reference.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Query the store using SQL.
    ///
    /// Returns results as a Vec of rows, where each row is a Vec of string values.
    pub fn query(&self, sql: &str) -> Result<QueryResult> {
        let conn = self.connection()?;
        let mut stmt = conn.prepare(sql)?;

        // Execute the query first to get column info
        let mut rows_iter = stmt.query([])?;

        // Get column info from the rows iterator
        let column_count = rows_iter.as_ref().map(|r| r.column_count()).unwrap_or(0);
        let column_names: Vec<String> = if let Some(row_ref) = rows_iter.as_ref() {
            (0..column_count)
                .map(|i| {
                    row_ref
                        .column_name(i)
                        .map(|s| s.to_string())
                        .unwrap_or_else(|_| format!("col{}", i))
                })
                .collect()
        } else {
            Vec::new()
        };

        // Collect all rows
        let mut result_rows = Vec::new();
        while let Some(row) = rows_iter.next()? {
            let mut values = Vec::with_capacity(column_count);
            for i in 0..column_count {
                // Get value as generic ValueRef and convert to string
                let value = match row.get_ref(i)? {
                    ValueRef::Null => "NULL".to_string(),
                    ValueRef::Boolean(b) => b.to_string(),
                    ValueRef::TinyInt(n) => n.to_string(),
                    ValueRef::SmallInt(n) => n.to_string(),
                    ValueRef::Int(n) => n.to_string(),
                    ValueRef::BigInt(n) => n.to_string(),
                    ValueRef::HugeInt(n) => n.to_string(),
                    ValueRef::UTinyInt(n) => n.to_string(),
                    ValueRef::USmallInt(n) => n.to_string(),
                    ValueRef::UInt(n) => n.to_string(),
                    ValueRef::UBigInt(n) => n.to_string(),
                    ValueRef::Float(f) => f.to_string(),
                    ValueRef::Double(f) => f.to_string(),
                    ValueRef::Decimal(d) => d.to_string(),
                    ValueRef::Timestamp(unit, val) => {
                        // Convert to microseconds then to DateTime
                        let micros = match unit {
                            TimeUnit::Second => val * 1_000_000,
                            TimeUnit::Millisecond => val * 1_000,
                            TimeUnit::Microsecond => val,
                            TimeUnit::Nanosecond => val / 1_000,
                        };
                        DateTime::<Utc>::from_timestamp_micros(micros)
                            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
                            .unwrap_or_else(|| format!("<invalid timestamp {}>", val))
                    }
                    ValueRef::Date32(days) => {
                        // Days since 1970-01-01
                        NaiveDate::from_ymd_opt(1970, 1, 1)
                            .and_then(|epoch| epoch.checked_add_signed(TimeDelta::days(days as i64)))
                            .map(|d| d.format("%Y-%m-%d").to_string())
                            .unwrap_or_else(|| format!("<invalid date {}>", days))
                    }
                    ValueRef::Time64(unit, val) => {
                        // Convert to microseconds then to NaiveTime
                        let micros = match unit {
                            TimeUnit::Second => val * 1_000_000,
                            TimeUnit::Millisecond => val * 1_000,
                            TimeUnit::Microsecond => val,
                            TimeUnit::Nanosecond => val / 1_000,
                        };
                        let secs = (micros / 1_000_000) as u32;
                        let micro_part = (micros % 1_000_000) as u32;
                        NaiveTime::from_num_seconds_from_midnight_opt(secs, micro_part * 1000)
                            .map(|t| t.format("%H:%M:%S").to_string())
                            .unwrap_or_else(|| format!("<invalid time {}>", val))
                    }
                    ValueRef::Interval { months, days, nanos } => {
                        format!("{} months {} days {} ns", months, days, nanos)
                    }
                    ValueRef::Text(s) => String::from_utf8_lossy(s).to_string(),
                    ValueRef::Blob(b) => format!("<blob {} bytes>", b.len()),
                    _ => "<complex>".to_string(),
                };
                values.push(value);
            }
            result_rows.push(values);
        }

        Ok(QueryResult {
            columns: column_names,
            rows: result_rows,
        })
    }

    /// Get the last invocation with its output (if any).
    pub fn last_invocation_with_output(
        &self,
    ) -> Result<Option<(InvocationSummary, Option<OutputInfo>)>> {
        if let Some(inv) = self.last_invocation()? {
            let output = self.get_output(&inv.id)?;
            Ok(Some((inv, output)))
        } else {
            Ok(None)
        }
    }
}

/// Result of a SQL query.
#[derive(Debug)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

/// Sanitize a string for use in filenames.
fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            ' ' => '-',
            c if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' => c,
            _ => '_',
        })
        .take(64)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::initialize;
    use tempfile::TempDir;

    fn setup_store() -> (TempDir, Store) {
        let tmp = TempDir::new().unwrap();
        let config = Config::with_root(tmp.path());
        initialize(&config).unwrap();
        let store = Store::open(config).unwrap();
        (tmp, store)
    }

    #[test]
    fn test_store_open_uninitialized_fails() {
        let tmp = TempDir::new().unwrap();
        let config = Config::with_root(tmp.path());

        let result = Store::open(config);
        assert!(matches!(result, Err(Error::NotInitialized(_))));
    }

    #[test]
    fn test_sanitize_filename() {
        assert_eq!(sanitize_filename("make test"), "make-test");
        assert_eq!(sanitize_filename("/usr/bin/gcc"), "_usr_bin_gcc");
        assert_eq!(sanitize_filename("a:b*c?d"), "a_b_c_d");
    }
}
