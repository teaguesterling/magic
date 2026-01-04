# BIRD Integration with blq

This document describes how BIRD integrates with **blq** (Build Log Query) and the **duck_hunt** DuckDB extension.

## Overview

```
┌──────────────────────────────────────────────────────────────────┐
│                         MAGIC Ecosystem                           │
├──────────────────────────────────────────────────────────────────┤
│                                                                   │
│  ┌────────────────────┐          ┌────────────────────┐          │
│  │       shq          │          │        blq         │          │
│  │   (Shell Query)    │◀────────▶│  (Build Log Query) │          │
│  │                    │          │                    │          │
│  │ • Captures history │          │ • Parses formats   │          │
│  │ • Stores in BIRD   │          │ • Analyzes logs    │          │
│  │ • Provides queries │          │ • Aggregates data  │          │
│  └─────────┬──────────┘          └─────────┬──────────┘          │
│            │                               │                     │
│            └───────────┬───────────────────┘                     │
│                        │                                         │
│                        ▼                                         │
│            ┌────────────────────────┐                            │
│            │      duck_hunt         │                            │
│            │  (DuckDB Extension)    │                            │
│            │                        │                            │
│            │ • 80+ log formats      │                            │
│            │ • Unified schema       │                            │
│            │ • SQL integration      │                            │
│            └────────────────────────┘                            │
│                        │                                         │
│                        ▼                                         │
│            ┌────────────────────────┐                            │
│            │       BIRD DB          │                            │
│            │  (DuckDB + Parquet)    │                            │
│            └────────────────────────┘                            │
│                                                                   │
└──────────────────────────────────────────────────────────────────┘
```

## The duck_hunt Extension

**duck_hunt** is a DuckDB extension that provides a unified interface for parsing structured logs.

### Installation

```sql
INSTALL duck_hunt;
LOAD duck_hunt;
```

### Supported Formats

Over 80 log formats including:

**Compilers:**
- GCC, Clang, MSVC
- Rust (cargo, rustc)
- Go (go build)
- Java (javac)

**Build Tools:**
- Make, CMake, Ninja
- Gradle, Maven, Ant
- Bazel, Buck

**Linters:**
- ESLint, TSLint
- Pylint, Flake8, mypy
- RuboCop
- ShellCheck

**Test Frameworks:**
- pytest, unittest
- Jest, Mocha
- JUnit, TestNG
- Go test, Cargo test

**CI Systems:**
- GitHub Actions
- GitLab CI
- Jenkins
- CircleCI

### Unified Schema

All formats parse to a common schema:

```sql
CREATE TABLE parsed_events (
    severity    TEXT,       -- error, warning, info, note
    message     TEXT,       -- Error message
    file        TEXT,       -- Source file path
    line        INTEGER,    -- Line number
    column      INTEGER,    -- Column number (if available)
    code        TEXT,       -- Error code (e.g., E0308)
    rule        TEXT,       -- Rule name (e.g., no-unused-vars)
    category    TEXT,       -- Error category
    suggestion  TEXT,       -- Fix suggestion (if available)
    context     TEXT,       -- Surrounding context
    metadata    JSON        -- Format-specific extras
);
```

### Usage

```sql
-- Parse GCC output
SELECT * FROM read_duck_hunt_log('build.log', 'gcc');

-- Parse pytest output
SELECT * FROM read_duck_hunt_log('test-output.txt', 'pytest');

-- Parse cargo output
SELECT * FROM read_duck_hunt_log('cargo-build.log', 'cargo');
```

## Integration Approaches

### 1. Direct Query Integration

Query BIRD data with duck_hunt parsing inline:

```sql
-- Find all GCC errors from last week
SELECT 
    c.cmd,
    c.timestamp,
    e.*
FROM bird.commands c
CROSS JOIN read_duck_hunt_log(c.stdout_file, c.format_hint) e
WHERE c.format_hint = 'gcc'
  AND e.severity = 'error'
  AND c.date >= current_date - 7;
```

### 2. shq Command Integration

Use `shq events` for convenient access:

```bash
# Parse last command output
shq events

# Filter by severity
shq events --severity error

# Show specific format
shq events --format gcc

# Export to JSON
shq events --format json > errors.json
```

### 3. blq Pipeline Integration

Chain shq and blq for advanced analysis:

```bash
# Capture and analyze in one go
shq run make test | blq analyze --format gcc

# Query BIRD, pipe to blq
shq sql "SELECT stdout_file FROM commands WHERE cmd LIKE 'make%'" \
  | blq from - --format gcc \
  | blq where severity=error \
  | blq stats
```

## Format Detection Strategy

### 1. Command-Based Detection

Detect format from command name:

```rust
fn detect_from_command(cmd: &str) -> Option<String> {
    let lower = cmd.to_lowercase();
    
    if lower.contains("gcc") || lower.contains("g++") {
        return Some("gcc".to_string());
    }
    if lower.contains("cargo") {
        return Some("cargo".to_string());
    }
    if lower.contains("pytest") {
        return Some("pytest".to_string());
    }
    // ... more patterns
    
    None
}
```

### 2. Content-Based Detection

Analyze output to detect format:

```rust
fn detect_from_content(output: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(output);
    
    // GCC pattern: "file.c:42:5: error:"
    if text.contains(":error:") && text.contains(".c:") {
        return Some("gcc".to_string());
    }
    
    // Cargo pattern: "error[E0308]:"
    if text.contains("error[E") && text.contains("]") {
        return Some("cargo".to_string());
    }
    
    // pytest pattern: "FAILED test_foo.py::test_bar"
    if text.contains("FAILED") && text.contains("::") {
        return Some("pytest".to_string());
    }
    
    None
}
```

### 3. Hybrid Approach

Combine both with confidence scoring:

```rust
struct FormatDetection {
    format: String,
    confidence: f32,
    source: DetectionSource,
}

enum DetectionSource {
    Command,
    Content,
    Both,
}

fn detect_format(cmd: &str, output: &[u8]) -> Option<FormatDetection> {
    let from_cmd = detect_from_command(cmd);
    let from_content = detect_from_content(output);
    
    match (from_cmd, from_content) {
        (Some(c1), Some(c2)) if c1 == c2 => Some(FormatDetection {
            format: c1,
            confidence: 0.95,
            source: DetectionSource::Both,
        }),
        (Some(c), None) => Some(FormatDetection {
            format: c,
            confidence: 0.7,
            source: DetectionSource::Command,
        }),
        (None, Some(c)) => Some(FormatDetection {
            format: c,
            confidence: 0.8,
            source: DetectionSource::Content,
        }),
        (None, None) => None,
    }
}
```

## Advanced Query Patterns

### Error Frequency Analysis

```sql
-- Top 10 most common errors
SELECT 
    message,
    COUNT(*) as occurrences,
    COUNT(DISTINCT c.id) as affected_builds
FROM bird.commands c
CROSS JOIN read_duck_hunt_log(c.stdout_file, c.format_hint) e
WHERE e.severity = 'error'
  AND c.date >= current_date - 30
GROUP BY message
ORDER BY occurrences DESC
LIMIT 10;
```

### Error Resolution Tracking

```sql
-- Errors that got fixed
WITH recent_errors AS (
    SELECT DISTINCT e.message, e.file, e.line
    FROM bird.commands c
    CROSS JOIN read_duck_hunt_log(c.stdout_file, c.format_hint) e
    WHERE e.severity = 'error'
      AND c.date = current_date - 7
),
current_errors AS (
    SELECT DISTINCT e.message, e.file, e.line
    FROM bird.commands c
    CROSS JOIN read_duck_hunt_log(c.stdout_file, c.format_hint) e
    WHERE e.severity = 'error'
      AND c.date = current_date
)
SELECT r.* 
FROM recent_errors r
LEFT JOIN current_errors c USING (message, file, line)
WHERE c.message IS NULL;
```

### Build Time vs Error Count

```sql
-- Correlation between errors and duration
SELECT 
    DATE_TRUNC('day', c.timestamp) as day,
    AVG(c.duration_ms) as avg_duration,
    COUNT(DISTINCT e.message) as error_count
FROM bird.commands c
CROSS JOIN read_duck_hunt_log(c.stdout_file, c.format_hint) e
WHERE c.cmd LIKE 'make%'
  AND e.severity = 'error'
GROUP BY day
ORDER BY day;
```

### File-Level Error Heatmap

```sql
-- Which files have most errors?
SELECT 
    e.file,
    COUNT(*) as error_count,
    COUNT(DISTINCT e.line) as affected_lines,
    COUNT(DISTINCT c.date) as days_with_errors
FROM bird.commands c
CROSS JOIN read_duck_hunt_log(c.stdout_file, c.format_hint) e
WHERE e.severity = 'error'
  AND c.date >= current_date - 30
GROUP BY e.file
ORDER BY error_count DESC
LIMIT 20;
```

## BIRD Sync Protocol

For syncing BIRD data across machines.

### Goals

- Conflict-free merging
- Efficient transfer (rsync-friendly)
- Selective sync (by date range, client)
- Preserve parquet structure

### Protocol Design

```
┌─ Machine A ─────────────────┐       ┌─ Shared Storage ─────────┐       ┌─ Machine B ─────────────────┐
│                             │       │                          │       │                             │
│  ~/.local/share/bird/       │       │  /shared/bird/           │       │  ~/.local/share/bird/       │
│  ├── db/data/recent/        │       │  ├── sync/               │       │  ├── db/data/recent/        │
│  │   └── date=2024-12-30/   │──────▶│  │   └── pending/        │◀──────│  │   └── date=2024-12-30/   │
│  └── db/data/archive/       │       │  │       └── laptop-*.parquet  │  └── db/data/archive/       │
│                             │       │  └── archive/            │       │                             │
│  Every 6 hours:             │       │      └── by-week/        │       │  Every 6 hours:             │
│  1. Export new files        │       │                          │       │  1. Check for new files     │
│  2. Upload to shared        │       │  Merge process:          │       │  2. Download and merge      │
│  3. Check for updates       │       │  - No conflicts (UUID)   │       │  3. Update local DB         │
│                             │       │  - Append-only           │       │                             │
└─────────────────────────────┘       └──────────────────────────┘       └─────────────────────────────┘
```

### Sync Commands

```bash
# Push local data to shared storage
shq sync push --remote /shared/bird/

# Pull remote data to local
shq sync pull --remote /shared/bird/

# Bidirectional sync
shq sync --remote /shared/bird/

# Status check
shq sync status --remote /shared/bird/
```

### Conflict Resolution

No conflicts possible because:
- UUIDs guarantee uniqueness
- Parquet files are immutable (append-only)
- Each client writes to separate archive partitions

### Sync Algorithm

```rust
fn sync_to_remote(local: &Path, remote: &Path) -> Result<()> {
    // 1. Find new files (not in remote)
    let local_files = scan_parquet_files(local)?;
    let remote_files = scan_parquet_files(remote)?;
    let new_files: Vec<_> = local_files
        .difference(&remote_files)
        .collect();
    
    // 2. Copy new files (rsync for efficiency)
    for file in new_files {
        rsync(file, remote)?;
    }
    
    // 3. Update sync metadata
    write_sync_manifest(remote, &local_files)?;
    
    Ok(())
}

fn sync_from_remote(local: &Path, remote: &Path) -> Result<()> {
    // 1. Find new files in remote
    let remote_files = scan_parquet_files(remote)?;
    let local_files = scan_parquet_files(local)?;
    let new_files: Vec<_> = remote_files
        .difference(&local_files)
        .collect();
    
    // 2. Copy new files
    for file in new_files {
        rsync(remote.join(file), local)?;
    }
    
    // 3. Refresh DuckDB views
    refresh_views()?;
    
    Ok(())
}
```

## blq Command Examples

### Using blq with BIRD data

```bash
# Analyze all GCC builds from last week
shq sql "
  SELECT stdout_file 
  FROM commands 
  WHERE format_hint = 'gcc' 
    AND date >= current_date - 7
" | blq from - --format gcc \
  | blq stats

# Find most common warnings
shq sql "
  SELECT stdout_file, cmd
  FROM commands
  WHERE format_hint = 'gcc'
" | blq from - --format gcc \
  | blq where severity=warning \
  | blq group message \
  | blq sort count desc \
  | blq head 10

# Compare error rates across dates
for date in 2024-12-{25..30}; do
  echo "=== $date ==="
  shq sql "
    SELECT stdout_file 
    FROM commands 
    WHERE date = '$date' AND format_hint = 'gcc'
  " | blq from - --format gcc \
    | blq where severity=error \
    | blq count
done
```

## Best Practices

### 1. Always Store Format Hint

When running commands, let shq detect and store the format:

```bash
shq run make test           # Auto-detects format
```

Or specify explicitly:

```bash
shq run --format gcc make test
```

### 2. Query Recent Data First

Recent data is faster to query:

```sql
-- Fast: Query recent only
SELECT * FROM bird.commands_recent WHERE exit_code != 0;

-- Slower: Query all history
SELECT * FROM bird.commands WHERE exit_code != 0;
```

### 3. Use Managed Files for Large Outputs

Configure threshold appropriately:

```toml
[capture]
max_inline_bytes = 1048576  # 1MB
```

Files >1MB automatically stored separately, keeping parquet files small and fast.

### 4. Regular Compaction

Run compaction to keep query performance high:

```bash
# Automatic (cron)
0 * * * * shq compact

# Manual
shq compact --recent
```

### 5. Selective Sync

Only sync what you need:

```bash
# Sync only last 7 days
shq sync --since 7d

# Sync specific clients
shq sync --clients laptop,desktop

# Exclude large blob files (content-addressed storage)
shq sync --no-blobs
```

---

*Part of the MAGIC ecosystem* 🏀
