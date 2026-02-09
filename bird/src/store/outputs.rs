//! Output storage operations.

use std::fs;

use duckdb::params;

use super::atomic;
use super::{sanitize_filename, Store};
use crate::config::StorageMode;
use crate::schema::OutputRecord;
use crate::{Config, Error, Result};

/// Info about stored output.
#[derive(Debug)]
pub struct OutputInfo {
    pub storage_type: String,
    pub storage_ref: String,
    pub stream: String,
    pub byte_length: i64,
    pub content_hash: String,
}

impl Store {
    /// Store output content, routing to inline or blob based on size.
    ///
    /// This is the high-level method for storing invocation output. It:
    /// 1. Computes the BLAKE3 hash
    /// 2. Routes small content to inline (data: URL) or large to blob (file: URL)
    /// 3. Handles deduplication for blobs
    /// 4. Writes the output record to Parquet
    pub fn store_output(
        &self,
        invocation_id: uuid::Uuid,
        stream: &str,
        content: &[u8],
        date: chrono::NaiveDate,
        cmd_hint: Option<&str>,
    ) -> Result<()> {
        use base64::Engine;

        // Compute hash
        let hash = blake3::hash(content);
        let hash_hex = hash.to_hex().to_string();

        // Route by size
        let (storage_type, storage_ref) = if content.len() < self.config.inline_threshold {
            // Inline: use data: URL
            let b64 = base64::engine::general_purpose::STANDARD.encode(content);
            let data_url = format!("data:application/octet-stream;base64,{}", b64);
            ("inline".to_string(), data_url)
        } else {
            // Blob: check for dedup, write file if needed
            let conn = self.connection()?;

            // Check if blob already exists (dedup check)
            let existing: std::result::Result<String, _> = conn.query_row(
                "SELECT storage_path FROM blob_registry WHERE content_hash = ?",
                params![&hash_hex],
                |row| row.get(0),
            );

            let storage_path = match existing {
                Ok(path) => {
                    // DEDUP HIT - increment ref count
                    conn.execute(
                        "UPDATE blob_registry SET ref_count = ref_count + 1, last_accessed = CURRENT_TIMESTAMP WHERE content_hash = ?",
                        params![&hash_hex],
                    )?;
                    path
                }
                Err(_) => {
                    // DEDUP MISS in registry - write new blob atomically
                    let cmd_hint = cmd_hint.unwrap_or("output");
                    let blob_path = self.config.blob_path(&hash_hex, cmd_hint);

                    // Ensure subdirectory exists
                    if let Some(parent) = blob_path.parent() {
                        fs::create_dir_all(parent)?;
                    }

                    // Compute relative path for storage_ref
                    let rel_path = blob_path
                        .strip_prefix(self.config.data_dir())
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_else(|_| blob_path.to_string_lossy().to_string());

                    // Atomic write: temp file + rename (handles concurrent writes)
                    let wrote_new = atomic::write_file(&blob_path, content)?;

                    if wrote_new {
                        // We wrote the file - register in blob_registry
                        conn.execute(
                            "INSERT INTO blob_registry (content_hash, byte_length, storage_path) VALUES (?, ?, ?)",
                            params![&hash_hex, content.len() as i64, &rel_path],
                        )?;
                    } else {
                        // Another process wrote this blob concurrently - increment ref_count
                        conn.execute(
                            "UPDATE blob_registry SET ref_count = ref_count + 1, last_accessed = CURRENT_TIMESTAMP WHERE content_hash = ?",
                            params![&hash_hex],
                        )?;
                    }

                    rel_path
                }
            };

            ("blob".to_string(), format!("file://{}", storage_path))
        };

        // Create and write the output record
        let record = OutputRecord {
            id: uuid::Uuid::now_v7(),
            invocation_id,
            stream: stream.to_string(),
            content_hash: hash_hex,
            byte_length: content.len(),
            storage_type,
            storage_ref,
            content_type: None,
            date,
        };

        self.write_output(&record)
    }

    /// Write an output record to the store (low-level).
    ///
    /// Behavior depends on storage mode:
    /// - Parquet: Creates a new Parquet file in the appropriate date partition
    /// - DuckDB: Inserts directly into the local.outputs
    pub fn write_output(&self, record: &OutputRecord) -> Result<()> {
        match self.config.storage_mode {
            StorageMode::Parquet => self.write_output_parquet(record),
            StorageMode::DuckDB => self.write_output_duckdb(record),
        }
    }

    /// Write output to a Parquet file (multi-writer safe).
    fn write_output_parquet(&self, record: &OutputRecord) -> Result<()> {
        let conn = self.connection()?;

        // Ensure the partition directory exists
        let partition_dir = self.config.outputs_dir(&record.date);
        fs::create_dir_all(&partition_dir)?;

        // Generate filename: {invocation_id}--{stream}--{id}.parquet
        let filename = format!(
            "{}--{}--{}.parquet",
            record.invocation_id,
            sanitize_filename(&record.stream),
            record.id
        );
        let file_path = partition_dir.join(&filename);

        // Write via DuckDB
        conn.execute_batch(
            r#"
            CREATE OR REPLACE TEMP TABLE temp_output (
                id UUID,
                invocation_id UUID,
                stream VARCHAR,
                content_hash VARCHAR,
                byte_length BIGINT,
                storage_type VARCHAR,
                storage_ref VARCHAR,
                content_type VARCHAR,
                date DATE
            );
            "#,
        )?;

        conn.execute(
            r#"
            INSERT INTO temp_output VALUES (
                ?, ?, ?, ?, ?, ?, ?, ?, ?
            )
            "#,
            params![
                record.id.to_string(),
                record.invocation_id.to_string(),
                record.stream,
                record.content_hash,
                record.byte_length as i64,
                record.storage_type,
                record.storage_ref,
                record.content_type,
                record.date.to_string(),
            ],
        )?;

        // Atomic write: COPY to temp file, then rename
        let temp_path = atomic::temp_path(&file_path);
        conn.execute(
            &format!(
                "COPY temp_output TO '{}' (FORMAT PARQUET, COMPRESSION ZSTD)",
                temp_path.display()
            ),
            [],
        )?;
        conn.execute("DROP TABLE temp_output", [])?;

        // Rename temp to final (atomic on POSIX)
        atomic::rename_into_place(&temp_path, &file_path)?;

        Ok(())
    }

    /// Write output directly to DuckDB table.
    fn write_output_duckdb(&self, record: &OutputRecord) -> Result<()> {
        let conn = self.connection()?;

        conn.execute(
            r#"
            INSERT INTO local.outputs VALUES (
                ?, ?, ?, ?, ?, ?, ?, ?, ?
            )
            "#,
            params![
                record.id.to_string(),
                record.invocation_id.to_string(),
                record.stream,
                record.content_hash,
                record.byte_length as i64,
                record.storage_type,
                record.storage_ref,
                record.content_type,
                record.date.to_string(),
            ],
        )?;

        Ok(())
    }

    /// Get outputs for an invocation by ID, optionally filtered by stream.
    pub fn get_outputs(
        &self,
        invocation_id: &str,
        stream_filter: Option<&str>,
    ) -> Result<Vec<OutputInfo>> {
        let conn = self.connection()?;

        let sql = match stream_filter {
            Some(stream) => format!(
                r#"
                SELECT storage_type, storage_ref, stream, byte_length, content_hash
                FROM outputs
                WHERE invocation_id = '{}' AND stream = '{}'
                ORDER BY stream
                "#,
                invocation_id, stream
            ),
            None => format!(
                r#"
                SELECT storage_type, storage_ref, stream, byte_length, content_hash
                FROM outputs
                WHERE invocation_id = '{}'
                ORDER BY stream
                "#,
                invocation_id
            ),
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
            Ok(OutputInfo {
                storage_type: row.get(0)?,
                storage_ref: row.get(1)?,
                stream: row.get(2)?,
                byte_length: row.get(3)?,
                content_hash: row.get(4)?,
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

    /// Get output for an invocation by ID (first match, for backwards compat).
    pub fn get_output(&self, invocation_id: &str) -> Result<Option<OutputInfo>> {
        let outputs = self.get_outputs(invocation_id, None)?;
        Ok(outputs.into_iter().next())
    }

    /// Read content from storage using DuckDB's read_blob (handles both data: and file:// URLs).
    pub fn read_output_content(&self, output: &OutputInfo) -> Result<Vec<u8>> {
        let conn = self.connection()?;

        // Resolve the storage_ref to an absolute path for file:// URLs
        let resolved_ref = if output.storage_ref.starts_with("file://") {
            let rel_path = output.storage_ref.strip_prefix("file://").unwrap();
            let abs_path = self.config.data_dir().join(rel_path);
            format!("file://{}", abs_path.display())
        } else {
            output.storage_ref.clone()
        };

        let content: Vec<u8> = conn
            .query_row(
                "SELECT content FROM read_blob(?)",
                params![&resolved_ref],
                |row| row.get(0),
            )
            .map_err(|e| Error::Storage(format!("Failed to read blob: {}", e)))?;

        Ok(content)
    }
}

impl OutputInfo {
    /// Read the content from storage (inline or blob).
    /// Prefer Store::read_output_content() which uses DuckDB for unified access.
    #[deprecated(note = "Use Store::read_output_content() instead for DuckDB-based reads")]
    pub fn read_content(&self, config: &Config) -> Result<Vec<u8>> {
        use base64::Engine;

        match self.storage_type.as_str() {
            "inline" => {
                // Parse data: URL and decode base64
                if let Some(b64_part) = self.storage_ref.split(',').nth(1) {
                    base64::engine::general_purpose::STANDARD
                        .decode(b64_part)
                        .map_err(|e| Error::Storage(format!("Failed to decode base64: {}", e)))
                } else {
                    Err(Error::Storage("Invalid data: URL format".to_string()))
                }
            }
            "blob" => {
                // Read raw file
                let rel_path = self
                    .storage_ref
                    .strip_prefix("file://")
                    .ok_or_else(|| Error::Storage("Invalid file:// URL".to_string()))?;

                let full_path = config.data_dir().join(rel_path);
                fs::read(&full_path).map_err(|e| {
                    Error::Storage(format!("Failed to read blob {}: {}", full_path.display(), e))
                })
            }
            other => Err(Error::Storage(format!("Unknown storage type: {}", other))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::init::initialize;
    use crate::schema::InvocationRecord;
    use crate::Config;
    use duckdb::params;
    use tempfile::TempDir;

    fn setup_store() -> (TempDir, Store) {
        let tmp = TempDir::new().unwrap();
        let config = Config::with_root(tmp.path());
        initialize(&config).unwrap();
        let store = Store::open(config).unwrap();
        (tmp, store)
    }

    #[test]
    fn test_write_and_get_output() {
        let (_tmp, store) = setup_store();

        // Create an invocation first
        let inv = InvocationRecord::new(
            "test-session",
            "echo hello",
            "/home/user",
            0,
            "test@client",
        );
        let inv_id = inv.id;
        let date = inv.date();
        store.write_invocation(&inv).unwrap();

        // Write stdout output
        let content = b"hello world\n";
        let output = OutputRecord::new_inline(inv_id, "stdout", content, date);
        store.write_output(&output).unwrap();

        // Retrieve it
        let outputs = store.get_outputs(&inv_id.to_string(), None).unwrap();
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].stream, "stdout");
        assert_eq!(outputs[0].byte_length, 12);
        assert_eq!(outputs[0].storage_type, "inline");
    }

    #[test]
    fn test_write_separate_streams() {
        let (_tmp, store) = setup_store();

        let inv = InvocationRecord::new(
            "test-session",
            "compile",
            "/home/user",
            1,
            "test@client",
        );
        let inv_id = inv.id;
        let date = inv.date();
        store.write_invocation(&inv).unwrap();

        // Write stdout
        let stdout_content = b"Building...\nDone.\n";
        let stdout_output = OutputRecord::new_inline(inv_id, "stdout", stdout_content, date);
        store.write_output(&stdout_output).unwrap();

        // Write stderr
        let stderr_content = b"warning: unused variable\n";
        let stderr_output = OutputRecord::new_inline(inv_id, "stderr", stderr_content, date);
        store.write_output(&stderr_output).unwrap();

        // Get all outputs
        let all_outputs = store.get_outputs(&inv_id.to_string(), None).unwrap();
        assert_eq!(all_outputs.len(), 2);

        // Get only stdout
        let stdout_only = store
            .get_outputs(&inv_id.to_string(), Some("stdout"))
            .unwrap();
        assert_eq!(stdout_only.len(), 1);
        assert_eq!(stdout_only[0].stream, "stdout");
        assert_eq!(stdout_only[0].byte_length, 18);

        // Get only stderr
        let stderr_only = store
            .get_outputs(&inv_id.to_string(), Some("stderr"))
            .unwrap();
        assert_eq!(stderr_only.len(), 1);
        assert_eq!(stderr_only[0].stream, "stderr");
        assert_eq!(stderr_only[0].byte_length, 25);
    }

    #[test]
    fn test_get_outputs_nonexistent() {
        let (_tmp, store) = setup_store();

        let outputs = store.get_outputs("nonexistent-id", None).unwrap();
        assert!(outputs.is_empty());
    }

    #[test]
    fn test_output_content_hash() {
        let (_tmp, store) = setup_store();

        let inv = InvocationRecord::new(
            "test-session",
            "test",
            "/home/user",
            0,
            "test@client",
        );
        let inv_id = inv.id;
        let date = inv.date();
        store.write_invocation(&inv).unwrap();

        let content = b"test content";
        let output = OutputRecord::new_inline(inv_id, "stdout", content, date);
        let expected_hash = output.content_hash.clone();
        store.write_output(&output).unwrap();

        // Verify hash is stored and retrievable
        let outputs = store.get_outputs(&inv_id.to_string(), None).unwrap();
        assert_eq!(outputs[0].content_hash, expected_hash);
        assert!(!outputs[0].content_hash.is_empty());
    }

    #[test]
    fn test_output_decode_inline() {
        let content = b"hello world";
        let inv_id = uuid::Uuid::now_v7();
        let date = chrono::Utc::now().date_naive();

        let output = OutputRecord::new_inline(inv_id, "stdout", content, date);

        // Verify we can decode the content back
        let decoded = output.decode_content().expect("should decode");
        assert_eq!(decoded, content);
    }

    #[test]
    fn test_store_output_inline_small_content() {
        let (_tmp, store) = setup_store();

        let inv = InvocationRecord::new(
            "test-session",
            "echo hello",
            "/home/user",
            0,
            "test@client",
        );
        let inv_id = inv.id;
        let date = inv.date();
        store.write_invocation(&inv).unwrap();

        // Small content should be stored inline (under 4KB threshold)
        let content = b"hello world\n";
        store
            .store_output(inv_id, "stdout", content, date, Some("echo"))
            .unwrap();

        let outputs = store.get_outputs(&inv_id.to_string(), None).unwrap();
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].storage_type, "inline");
        assert!(outputs[0].storage_ref.starts_with("data:"));

        // Verify content can be read back via DuckDB
        let read_back = store.read_output_content(&outputs[0]).unwrap();
        assert_eq!(read_back, content);
    }

    #[test]
    fn test_store_output_blob_large_content() {
        let (_tmp, store) = setup_store();

        let inv = InvocationRecord::new(
            "test-session",
            "cat bigfile",
            "/home/user",
            0,
            "test@client",
        );
        let inv_id = inv.id;
        let date = inv.date();
        store.write_invocation(&inv).unwrap();

        // Large content should be stored as blob (over 4KB threshold)
        let content: Vec<u8> = (0..5000).map(|i| (i % 256) as u8).collect();
        store
            .store_output(inv_id, "stdout", &content, date, Some("cat"))
            .unwrap();

        let outputs = store.get_outputs(&inv_id.to_string(), None).unwrap();
        assert_eq!(outputs.len(), 1);
        assert_eq!(outputs[0].storage_type, "blob");
        assert!(outputs[0].storage_ref.starts_with("file://"));

        // Verify content can be read back via DuckDB
        let read_back = store.read_output_content(&outputs[0]).unwrap();
        assert_eq!(read_back, content);
    }

    #[test]
    fn test_store_output_blob_deduplication() {
        let (_tmp, store) = setup_store();

        // Create two invocations
        let inv1 = InvocationRecord::new(
            "test-session",
            "cat file",
            "/home/user",
            0,
            "test@client",
        );
        let inv1_id = inv1.id;
        let date1 = inv1.date();
        store.write_invocation(&inv1).unwrap();

        let inv2 = InvocationRecord::new(
            "test-session",
            "cat file",
            "/home/user",
            0,
            "test@client",
        );
        let inv2_id = inv2.id;
        let date2 = inv2.date();
        store.write_invocation(&inv2).unwrap();

        // Same large content for both
        let content: Vec<u8> = (0..5000).map(|i| (i % 256) as u8).collect();

        store
            .store_output(inv1_id, "stdout", &content, date1, Some("cat"))
            .unwrap();
        store
            .store_output(inv2_id, "stdout", &content, date2, Some("cat"))
            .unwrap();

        // Both should point to same blob (same content_hash)
        let outputs1 = store.get_outputs(&inv1_id.to_string(), None).unwrap();
        let outputs2 = store.get_outputs(&inv2_id.to_string(), None).unwrap();

        assert_eq!(outputs1[0].content_hash, outputs2[0].content_hash);
        assert_eq!(outputs1[0].storage_ref, outputs2[0].storage_ref);

        // Verify ref_count is 2
        let conn = store.connection().unwrap();
        let ref_count: i32 = conn
            .query_row(
                "SELECT ref_count FROM blob_registry WHERE content_hash = ?",
                params![&outputs1[0].content_hash],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(ref_count, 2);

        // Both should read back correctly via DuckDB
        assert_eq!(store.read_output_content(&outputs1[0]).unwrap(), content);
        assert_eq!(store.read_output_content(&outputs2[0]).unwrap(), content);
    }

    #[test]
    fn test_store_output_blob_file_created() {
        let (_tmp, store) = setup_store();

        let inv = InvocationRecord::new(
            "test-session",
            "generate",
            "/home/user",
            0,
            "test@client",
        );
        let inv_id = inv.id;
        let date = inv.date();
        store.write_invocation(&inv).unwrap();

        let content: Vec<u8> = (0..5000).map(|i| (i % 256) as u8).collect();
        store
            .store_output(inv_id, "stdout", &content, date, Some("generate"))
            .unwrap();

        // Verify blob file was created
        let outputs = store.get_outputs(&inv_id.to_string(), None).unwrap();
        let rel_path = outputs[0].storage_ref.strip_prefix("file://").unwrap();
        let full_path = store.config().data_dir().join(rel_path);
        assert!(
            full_path.exists(),
            "Blob file should exist at {:?}",
            full_path
        );
        assert!(full_path.to_string_lossy().ends_with(".bin"));
    }
}
