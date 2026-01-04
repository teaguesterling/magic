# shq Implementation Specification

This document specifies the implementation of the `shq` executable (not the shell integration - see shq_shell_integration.md for that).

## Overview

The `shq` binary provides:
- Command capture (`run`, `save`)
- Query interface (`sql`, `show`)
- Build log parsing (`events`, `stats`)
- Error reporting (`errors`)
- Database management (`init`, `compact`, `archive`)

## Command Structure

```
shq <command> [options]

Commands:
  run <cmd>       Run command with capture and log parsing
  save            Save command retroactively (from tmux)
  sql <query>     Execute SQL query
  show <ref>      Show formatted data reference
  events          Show parsed errors/warnings from last run
  stats           Show statistics
  errors          View/manage error log
  init            Initialize BIRD database
  compact         Compact parquet files
  archive         Move old data to archive
  hook            Generate shell integration code
```

## Core Implementation

### 1. Command Capture

#### `shq run <cmd...>`

Execute command with capture and format detection:

```rust
pub fn cmd_run(args: &[String], config: &Config) -> Result<()> {
    let cmd_str = args.join(" ");
    
    // 1. Create command record
    let command_id = UUIDv7::new();
    let start = Instant::now();
    
    // 2. Execute command with output capture
    let output = Command::new("sh")
        .arg("-c")
        .arg(&cmd_str)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?
        .wait_with_output()?;
    
    let duration_ms = start.elapsed().as_millis() as i64;
    
    // 3. Detect format
    let format_hint = detect_format(&cmd_str, &output.stdout)?;
    
    // 4. Write command parquet
    write_command_parquet(CommandRecord {
        id: command_id,
        session_id: get_session_id()?,
        timestamp: Utc::now(),
        duration_ms,
        cwd: env::current_dir()?.display().to_string(),
        cmd: cmd_str.clone(),
        executable: extract_executable(&cmd_str),
        exit_code: output.status.code().unwrap_or(-1),
        format_hint: format_hint.clone(),
        client_id: config.client_id.clone(),
        hostname: gethostname::gethostname().to_string_lossy().to_string(),
        username: env::var("USER").unwrap_or_default(),
        ..Default::default()
    })?;
    
    // 5. Write output parquets (content-addressed blobs for large outputs)
    write_output(&command_id, "stdout", &output.stdout, &format_hint)?;
    write_output(&command_id, "stderr", &output.stderr, &format_hint)?;
    
    // 6. Return exit code
    std::process::exit(output.status.code().unwrap_or(1));
}
```

#### `shq save [options]`

Save command retroactively from tmux:

```rust
pub fn cmd_save(options: &SaveOptions) -> Result<()> {
    let (cmd, exit_code, duration_ms) = if options.from_shell {
        // Called from shell hook (has all info)
        (
            options.cmd.clone().unwrap(),
            options.exit_code.unwrap_or(0),
            options.duration_ms.unwrap_or(0),
        )
    } else {
        // Called manually (scrape from tmux)
        scrape_from_tmux()?
    };
    
    // Write command record (no output capture in this mode)
    let command_id = UUIDv7::new();
    write_command_parquet(CommandRecord {
        id: command_id,
        cmd,
        exit_code,
        duration_ms,
        // ... other fields
    })?;
    
    Ok(())
}

fn scrape_from_tmux() -> Result<(String, i32, i64)> {
    // Use tmux capture-pane to get last command
    let output = Command::new("tmux")
        .args(&["capture-pane", "-p", "-S", "-1"])
        .output()?;
    
    let lines: Vec<&str> = str::from_utf8(&output.stdout)?
        .lines()
        .collect();
    
    // Last line should be the command (simple heuristic)
    let cmd = lines.last()
        .ok_or_else(|| anyhow!("No command found"))?
        .to_string();
    
    // Exit code unknown in retroactive mode
    Ok((cmd, 0, 0))
}
```

### 2. Output Storage

```rust
fn write_output(
    command_id: &UUID,
    stream: &str,
    content: &[u8],
    format_hint: &Option<String>,
) -> Result<()> {
    let config = load_config()?;
    let output_id = UUIDv7::new();
    
    // 1. Compute content hash (BLAKE3)
    let content_hash = blake3::hash(content);
    let hash_hex = content_hash.to_hex().to_string();
    
    // 2. Size-based routing
    let (storage_type, storage_ref) = if content.len() < config.max_inline_bytes {
        // Small: Store inline with data: URI
        let b64 = base64::encode(content);
        ("inline", format!("data:application/octet-stream;base64,{}", b64))
    } else {
        // Large: Check for existing blob (deduplication!)
        if let Some(existing_path) = check_blob_exists(&hash_hex)? {
            // DEDUP HIT: Reuse existing blob
            increment_blob_ref_count(&hash_hex)?;
            ("blob", format!("file://{}", existing_path))
        } else {
            // DEDUP MISS: Write new content-addressed blob
            let blob_path = write_content_addressed_blob(&hash_hex, content)?;
            register_blob(&hash_hex, content.len(), &blob_path)?;
            ("blob", format!("file://{}", blob_path))
        }
    };
    
    // 3. Write output parquet with new schema
    write_output_parquet(OutputRecord {
        id: output_id,
        command_id: *command_id,
        stream: stream.to_string(),
        content_hash: hash_hex,
        byte_length: content.len() as i64,
        storage_type: storage_type.to_string(),
        storage_ref: storage_ref,
        content_type: format_hint.clone(),
        ..Default::default()
    })
}

fn write_content_addressed_blob(hash: &str, content: &[u8]) -> Result<String> {
    let bird_root = get_bird_root()?;
    
    // Subdirectory: first 2 hex chars
    let subdir = &hash[..2];
    let blob_dir = bird_root.join("db/data/recent/blobs/content").join(subdir);
    fs::create_dir_all(&blob_dir)?;
    
    // Filename: full hash + .bin.gz
    let filename = format!("{}.bin.gz", hash);
    let final_path = blob_dir.join(&filename);
    
    // Atomic write with race condition handling
    let temp_path = blob_dir.join(format!(".tmp.{}.bin.gz", hash));
    
    // Compress with gzip (DuckDB-compatible)
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(content)?;
    let compressed = encoder.finish()?;
    
    fs::write(&temp_path, compressed)?;
    
    // Atomic rename (handles concurrent writes of same hash)
    match fs::rename(&temp_path, &final_path) {
        Ok(_) => Ok(format!("recent/blobs/content/{}/{}", subdir, filename)),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Another process wrote same hash - that's fine!
            fs::remove_file(&temp_path).ok();
            Ok(format!("recent/blobs/content/{}/{}", subdir, filename))
        }
        Err(e) => Err(e.into())
    }
}

fn check_blob_exists(hash: &str) -> Result<Option<String>> {
    let conn = get_db_connection()?;
    let mut stmt = conn.prepare(
        "SELECT storage_path FROM blob_registry WHERE content_hash = ?"
    )?;
    
    match stmt.query_row([hash], |row| row.get::<_, String>(0)) {
        Ok(path) => Ok(Some(path)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into())
    }
}

fn register_blob(hash: &str, byte_length: usize, path: &str) -> Result<()> {
    let conn = get_db_connection()?;
    conn.execute(
        "INSERT INTO blob_registry (content_hash, byte_length, ref_count, storage_tier, storage_path)
         VALUES (?, ?, 1, 'recent', ?)",
        params![hash, byte_length as i64, path]
    )?;
    Ok(())
}

fn increment_blob_ref_count(hash: &str) -> Result<()> {
    let conn = get_db_connection()?;
    conn.execute(
        "UPDATE blob_registry 
         SET ref_count = ref_count + 1, last_accessed = CURRENT_TIMESTAMP
         WHERE content_hash = ?",
        params![hash]
    )?;
    Ok()
    
    Ok(path.display().to_string())
}
```

### 3. Parquet Writing

```rust
use arrow::array::*;
use arrow::datatypes::*;
use parquet::arrow::ArrowWriter;
use parquet::file::properties::WriterProperties;

fn write_command_parquet(record: CommandRecord) -> Result<()> {
    let bird_root = get_bird_root()?;
    let date = record.timestamp.date_naive();
    let partition_dir = bird_root.join(format!(
        "db/data/recent/commands/date={}",
        date
    ));
    fs::create_dir_all(&partition_dir)?;
    
    // Generate filename
    let filename = format!(
        "{}--{}--{}.parquet",
        sanitize(&record.session_id),
        sanitize(record.executable.as_ref().unwrap_or(&"unknown".to_string())),
        record.id
    );
    let path = partition_dir.join(filename);
    
    // Create schema
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("session_id", DataType::Utf8, false),
        Field::new("timestamp", DataType::Timestamp(TimeUnit::Microsecond, None), false),
        Field::new("duration_ms", DataType::Int64, true),
        Field::new("cwd", DataType::Utf8, false),
        Field::new("cmd", DataType::Utf8, false),
        Field::new("executable", DataType::Utf8, true),
        Field::new("exit_code", DataType::Int32, false),
        Field::new("format_hint", DataType::Utf8, true),
        Field::new("client_id", DataType::Utf8, false),
        Field::new("hostname", DataType::Utf8, true),
        Field::new("username", DataType::Utf8, true),
    ]));
    
    // Create batch with single row
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(StringArray::from(vec![record.id.to_string()])),
            Arc::new(StringArray::from(vec![record.session_id.clone()])),
            Arc::new(TimestampMicrosecondArray::from(vec![
                record.timestamp.timestamp_micros()
            ])),
            Arc::new(Int64Array::from(vec![record.duration_ms])),
            Arc::new(StringArray::from(vec![record.cwd.clone()])),
            Arc::new(StringArray::from(vec![record.cmd.clone()])),
            Arc::new(StringArray::from(vec![record.executable.clone()])),
            Arc::new(Int32Array::from(vec![record.exit_code])),
            Arc::new(StringArray::from(vec![record.format_hint.clone()])),
            Arc::new(StringArray::from(vec![record.client_id.clone()])),
            Arc::new(StringArray::from(vec![record.hostname.clone()])),
            Arc::new(StringArray::from(vec![record.username.clone()])),
        ],
    )?;
    
    // Write parquet with compression
    let file = fs::File::create(path)?;
    let props = WriterProperties::builder()
        .set_compression(parquet::basic::Compression::ZSTD(
            parquet::basic::ZstdLevel::try_new(3)?
        ))
        .build();
    
    let mut writer = ArrowWriter::try_new(file, schema, Some(props))?;
    writer.write(&batch)?;
    writer.close()?;
    
    Ok(())
}

fn sanitize(s: &str) -> String {
    // Extract basename from path
    let basename = s.split('/').last().unwrap_or(s);
    
    // Sanitize
    basename.chars()
        .map(|c| match c {
            ' ' => '-',
            c if c.is_alphanumeric() || c == '-' => c,
            _ => '_',
        })
        .take(64)
        .collect()
}
```

### 4. Format Detection

```rust
fn detect_format(cmd: &str, output: &[u8]) -> Result<Option<String>> {
    // Extract command name
    let cmd_lower = cmd.to_lowercase();
    
    // Known command patterns
    if cmd_lower.contains("make") || cmd_lower.contains("gcc") || cmd_lower.contains("g++") {
        return Ok(Some("gcc".to_string()));
    }
    if cmd_lower.contains("cargo") {
        return Ok(Some("cargo".to_string()));
    }
    if cmd_lower.contains("pytest") || cmd_lower.contains("python") && output.contains(b"FAILED") {
        return Ok(Some("pytest".to_string()));
    }
    if cmd_lower.contains("eslint") {
        return Ok(Some("eslint".to_string()));
    }
    
    // Content-based detection
    let output_str = str::from_utf8(output).ok();
    if let Some(s) = output_str {
        if s.contains("error:") && (s.contains(".c:") || s.contains(".cpp:")) {
            return Ok(Some("gcc".to_string()));
        }
        if s.contains("ERROR") && s.contains("test_") {
            return Ok(Some("pytest".to_string()));
        }
    }
    
    Ok(None)
}
```

## Query Commands

### `shq sql <query>`

```rust
pub fn cmd_sql(query: &str, config: &Config) -> Result<()> {
    let db_path = ensure_bird_db()?;
    let conn = Connection::open(db_path)?;
    
    // Execute query
    let mut stmt = conn.prepare(query)?;
    let column_count = stmt.column_count();
    let column_names: Vec<String> = (0..column_count)
        .map(|i| stmt.column_name(i).unwrap().to_string())
        .collect();
    
    // Fetch rows
    let mut rows = stmt.query([])?;
    let mut results = Vec::new();
    while let Some(row) = rows.next()? {
        let mut values = Vec::new();
        for i in 0..column_count {
            let value = row.get::<_, String>(i)?;
            values.push(value);
        }
        results.push(values);
    }
    
    // Format as table
    print_table(&column_names, &results);
    
    Ok(())
}

fn print_table(headers: &[String], rows: &[Vec<String>]) {
    use comfy_table::*;
    
    let mut table = Table::new();
    table.set_header(headers);
    for row in rows {
        table.add_row(row);
    }
    println!("{}", table);
}
```

### `shq show <ref>`

```rust
pub fn cmd_show(reference: &str, config: &Config) -> Result<()> {
    // Parse reference (e.g., "@failures", "@last", "command_id")
    let data = resolve_reference(reference, config)?;
    
    // Auto-detect format and display
    if is_json(&data) {
        println!("{}", format_json(&data)?);
    } else if is_table(&data) {
        print_table_from_data(&data)?;
    } else {
        println!("{}", String::from_utf8_lossy(&data));
    }
    
    Ok(())
}
```

### `shq events [options]`

```rust
pub fn cmd_events(options: &EventsOptions, config: &Config) -> Result<()> {
    let db_path = ensure_bird_db()?;
    let conn = Connection::open(db_path)?;
    
    // Get last command with parsed output
    let query = "
        SELECT c.id, c.cmd, o.storage_type, o.storage_ref, o.content_hash, c.format_hint
        FROM commands c
        JOIN outputs o ON c.id = o.command_id
        WHERE c.timestamp = (SELECT MAX(timestamp) FROM commands)
        AND o.stream = 'stdout'
    ";
    
    let mut stmt = conn.prepare(query)?;
    let mut rows = stmt.query([])?;
    
    if let Some(row) = rows.next()? {
        let storage_type: String = row.get(2)?;
        let storage_ref: String = row.get(3)?;
        let content_hash: String = row.get(4)?;
        let format_hint: Option<String> = row.get(5)?;
        
        if let Some(format) = format_hint {
            // Resolve storage reference to file path
            let file_path = resolve_storage_ref(&storage_type, &storage_ref, &content_hash)?;
            
            // Parse with duck_hunt
            let events = parse_with_duck_hunt(&file_path, &format)?;
            
            // Filter by severity if requested
            let filtered = if let Some(severity) = &options.severity {
                events.into_iter()
                    .filter(|e| &e.severity == severity)
                    .collect()
            } else {
                events
            };
            
            // Display
            print_events(&filtered, options.format.as_deref().unwrap_or("table"));
        } else {
            eprintln!("No format hint for this command");
        }
    } else {
        eprintln!("No recent command found");
    }
    
    Ok(())
}

fn resolve_storage_ref(storage_type: &str, storage_ref: &str, _hash: &str) -> Result<String> {
    match storage_type {
        "inline" => {
            // data: URI - need to decode and write to temp file
            if storage_ref.starts_with("data:") {
                let b64_data = storage_ref.split(',').nth(1)
                    .ok_or_else(|| anyhow!("Invalid data: URI"))?;
                let decoded = base64::decode(b64_data)?;
                
                // Write to temp file for duck_hunt
                let temp_path = format!("/tmp/shq-output-{}.tmp", _hash);
                fs::write(&temp_path, decoded)?;
                Ok(temp_path)
            } else {
                Err(anyhow!("Unknown inline storage format"))
            }
        },
        "blob" | "archive" => {
            // file:// URI - extract path
            if storage_ref.starts_with("file://") {
                let rel_path = &storage_ref[7..];
                let bird_root = get_bird_root()?;
                let full_path = bird_root.join("db/data").join(rel_path);
                Ok(full_path.display().to_string())
            } else {
                Err(anyhow!("Unknown blob storage format"))
            }
        },
        _ => Err(anyhow!("Unknown storage type: {}", storage_type))
    }
}

fn parse_with_duck_hunt(file_path: &str, format: &str) -> Result<Vec<Event>> {
    // Call duck_hunt parser
    let conn = Connection::open(":memory:")?;
    conn.execute("INSTALL duck_hunt", [])?;
    conn.execute("LOAD duck_hunt", [])?;
    
    let query = format!(
        "SELECT * FROM read_duck_hunt_log('{}', '{}')",
        file_path, format
    );
    
    let mut stmt = conn.prepare(&query)?;
    let mut rows = stmt.query([])?;
    
    let mut events = Vec::new();
    while let Some(row) = rows.next()? {
        events.push(Event {
            severity: row.get("severity")?,
            message: row.get("message")?,
            file: row.get("file").ok(),
            line: row.get("line").ok(),
            column: row.get("column").ok(),
        });
    }
    
    Ok(events)
}
```

## Error Handling Implementation

**Critical Principle: Never break the shell.**

### Capture Error Handling

```rust
pub fn save_command(cmd: &CaptureCommand) -> Result<()> {
    match write_parquet(cmd) {
        Ok(_) => Ok(()),
        Err(e) => {
            // Log error, don't propagate
            log_error(&format!("Failed to save command: {}", e))?;
            // Shell continues normally
            Ok(())
        }
    }
}

fn log_error(msg: &str) -> Result<()> {
    let error_log = get_bird_root()?.join("errors.log");
    let timestamp = chrono::Utc::now().to_rfc3339();
    let entry = format!("[{}] {}\n", timestamp, msg);
    
    // Append to log (or create if missing)
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(error_log)?;
    file.write_all(entry.as_bytes())?;
    
    Ok(())
}
```

### Performance Critical Paths

**Hook Path (Most Frequent):**
```rust
// Must complete in <5ms
pub fn save_async(cmd: CaptureCommand) {
    // 1. Validate inputs (< 1ms)
    // 2. Serialize to temp buffer (< 2ms)
    // 3. Return immediately
    // 4. Background thread writes to disk
    
    thread::spawn(move || {
        let _ = save_command(&cmd);  // Ignore errors (logged)
    });
}
```

### bird.duckdb Lazy Initialization

```rust
pub fn ensure_bird_db() -> Result<PathBuf> {
    let db_path = get_bird_root()?.join("db/bird.duckdb");
    
    if db_path.exists() {
        return Ok(db_path);
    }
    
    // Create database with views
    let conn = Connection::open(&db_path)?;
    
    // Views use glob patterns (work even if no parquet files yet)
    conn.execute_batch(include_str!("../sql/views.sql"))?;
    conn.execute_batch(include_str!("../sql/macros.sql"))?;
    
    Ok(db_path)
}
```

## Database Management Commands

### `shq init`

```rust
pub fn cmd_init(config: &Config) -> Result<()> {
    let bird_root = get_bird_root()?;
    
    // Create directory structure
    create_dir_all(bird_root.join("db/data/recent/commands"))?;
    create_dir_all(bird_root.join("db/data/recent/outputs"))?;
    create_dir_all(bird_root.join("db/data/recent/managed"))?;
    create_dir_all(bird_root.join("db/data/archive/by-week"))?;
    create_dir_all(bird_root.join("db/sql"))?;
    
    // Copy SQL files
    fs::write(
        bird_root.join("db/sql/views.sql"),
        include_str!("../sql/views.sql")
    )?;
    fs::write(
        bird_root.join("db/sql/macros.sql"),
        include_str!("../sql/macros.sql")
    )?;
    
    // Initialize bird.duckdb
    ensure_bird_db()?;
    
    // Generate client ID if not exists
    if config.client_id.is_empty() {
        let hostname = gethostname::gethostname().to_string_lossy().to_string();
        let random = rand::thread_rng().gen_range(10000..99999);
        let client_id = format!("{}-{}", hostname, random);
        // Save to config...
    }
    
    println!("✓ BIRD initialized at {}", bird_root.display());
    Ok(())
}
```

### `shq compact [options]`

```rust
pub fn cmd_compact(options: &CompactOptions) -> Result<()> {
    // See COMPACTION_APPENDIX.md for full algorithm
    
    let bird_root = get_bird_root()?;
    let lock_path = bird_root.join("compaction.lock");
    
    // Try to acquire lock
    let _lock = acquire_lock(&lock_path)?;
    
    // Compact each date partition
    for date_dir in find_date_partitions(&bird_root)? {
        compact_partition(&date_dir, options)?;
    }
    
    println!("✓ Compaction complete");
    Ok(())
}
```

### `shq archive`

```rust
pub fn cmd_archive(config: &Config) -> Result<()> {
    let bird_root = get_bird_root()?;
    let cutoff_date = Utc::now().date_naive() - Duration::days(config.recent_days as i64);
    
    // Find old partitions
    let recent_dir = bird_root.join("db/data/recent");
    for entry in fs::read_dir(recent_dir)? {
        let path = entry?.path();
        if let Some(date) = extract_date_from_partition(&path) {
            if date < cutoff_date {
                archive_partition(&path, config)?;
            }
        }
    }
    
    Ok(())
}
```

## Testing

```rust
#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_sanitize_filename() {
        assert_eq!(sanitize("/usr/bin/gcc-12"), "gcc-12");
        assert_eq!(sanitize("/usr/local/bin/python3.11"), "python3_11");
        assert_eq!(sanitize("cargo build --release"), "cargo-build---release");
    }
    
    #[test]
    fn test_format_detection() {
        let output = b"error: expected `;` before }";
        assert_eq!(detect_format("gcc main.c", output).unwrap(), Some("gcc".to_string()));
    }
    
    #[test]
    fn test_managed_file_threshold() {
        let small = vec![0u8; 1024]; // 1KB
        let large = vec![0u8; 2_000_000]; // 2MB
        
        assert!(should_use_managed(&small) == false);
        assert!(should_use_managed(&large) == true);
    }
}
```

---

*Part of the MAGIC ecosystem* 🏀
