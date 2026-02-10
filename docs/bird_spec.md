# BIRD Specification v3 (Dual Storage Backends + Remote Sync)

**BIRD**: Buffer and Invocation Record Database

BIRD is the database backend for shq, using DuckDB for queries and either Parquet files or DuckDB tables for storage. This version adds **dual storage backends** and **remote sync** capabilities.

## Overview

BIRD stores every shell command execution as:
- **Invocation metadata**: timestamp, exit code, duration, working directory
- **Session context**: shell/invoker information, client identity
- **Output streams**: stdout and stderr (with content-addressed storage for large outputs)
- **Parsed events**: Errors, warnings, and structured diagnostics from build tools

### Key Features

- **Dual storage backends**: Choose parquet (multi-writer safe) or duckdb (simpler, single-writer)
- **Content-addressed blobs**: Automatic deduplication for large outputs (70-90% savings)
- **Remote sync**: Push/pull data to S3, MotherDuck, PostgreSQL, or file-based remotes
- **Event parsing**: Extract structured diagnostics from build output (gcc, cargo, pytest, etc.)
- **Date partitioning**: Efficient archival and time-based queries

## Directory Structure

```
$BIRD_ROOT/                          # Default: ~/.local/share/bird
├── db/
│   ├── bird.duckdb                  # DuckDB database (views, tables, or both)
│   ├── pending/                     # In-flight invocations (crash recovery)
│   │   └── <session>--<uuid>.pending
│   ├── data/
│   │   ├── recent/                  # Last 14 days (hot data)
│   │   │   ├── invocations/         # Command execution records
│   │   │   │   └── status=<status>/ # pending, completed, orphaned
│   │   │   │       └── date=YYYY-MM-DD/
│   │   │   │           └── <session>--<exec>--<uuid>.parquet
│   │   │   ├── outputs/             # stdout/stderr content
│   │   │   │   └── date=YYYY-MM-DD/
│   │   │   │       └── <session>--<exec>--<uuid>.parquet
│   │   │   ├── sessions/            # Shell/invoker sessions
│   │   │   │   └── date=YYYY-MM-DD/
│   │   │   │       └── <session>--<invoker>--<uuid>.parquet
│   │   │   ├── events/              # Parsed diagnostics
│   │   │   │   └── date=YYYY-MM-DD/
│   │   │   │       └── <session>--<format>--<uuid>.parquet
│   │   │   └── blobs/
│   │   │       └── content/         # Content-addressed pool
│   │   │           ├── ab/
│   │   │           │   └── <hash>--<cmd-hint>.bin
│   │   │           └── ...          # 256 subdirs (00-ff)
│   │   └── archive/                 # >14 days (cold data)
│   │       ├── invocations/
│   │       │   └── client=<n>/year=YYYY/week=WW/*.parquet
│   │       ├── outputs/
│   │       │   └── client=<n>/year=YYYY/week=WW/*.parquet
│   │       ├── sessions/
│   │       │   └── client=<n>/year=YYYY/week=WW/*.parquet
│   │       ├── events/
│   │       │   └── client=<n>/year=YYYY/week=WW/*.parquet
│   │       └── blobs/
│   │           └── content/         # Archived content pool
│   └── sql/
│       ├── views.sql                # View definitions
│       └── macros.sql               # Macro definitions
├── config.toml                      # Configuration (including remotes)
├── format-hints.toml                # Format detection hints
└── errors.log                       # Capture error log
```

### Directory Rationale

- **db/**: Contains both data and database objects
- **db/data/**: Separates data from metadata (bird.duckdb, SQL files)
- **recent/**: Hot tier - optimized for fast writes and queries (last 14 days)
- **archive/**: Cold tier - optimized for compression, organized by client/year/week
- **blobs/content/**: Content-addressed blob storage with automatic deduplication
  - Subdirectories: First 2 hex chars of hash (`ab/`, `cd/`, etc.)
  - Prevents filesystem slowdown with >10k files/directory
  - Same blob shared by multiple commands → **70-90% storage savings** for CI workloads

## Storage Backends

BIRD supports two storage backends, selected at initialization:

| Mode | CLI Flag | Write Pattern | Compaction | Best For |
|------|----------|---------------|------------|----------|
| **Parquet** | `--mode parquet` (default) | Multi-writer safe (atomic files) | Required | Concurrent shells |
| **DuckDB** | `--mode duckdb` | Single-writer (table inserts) | Not needed | Single-shell usage |

**Key insight**: Reading always goes through DuckDB views, regardless of storage mode.

### Parquet Mode (Default)

Each write creates a unique Parquet file with atomic rename:

```
invocations/date=2024-01-15/zsh-12345--make--01937a2b.parquet
```

**Pros:**
- Multiple shells can write simultaneously (no locks)
- Natural date partitioning
- Easy to inspect with external tools

**Cons:**
- Requires periodic compaction to merge small files
- Many small files until compacted

### DuckDB Mode

Writes go directly to DuckDB tables:

```sql
INSERT INTO invocations_table VALUES (...);
```

**Pros:**
- Simpler implementation
- No compaction needed
- Single file for all data

**Cons:**
- Must serialize writes (connect/disconnect pattern)
- Not suitable for concurrent shell hooks

## Schema

### Invocations Table

A captured command/process execution.

```sql
CREATE TABLE invocations (
    -- Identity
    id                UUID PRIMARY KEY,        -- UUIDv7 (time-ordered)
    session_id        VARCHAR NOT NULL,        -- References sessions.session_id

    -- Timing
    timestamp         TIMESTAMP NOT NULL,
    duration_ms       BIGINT,

    -- Context
    cwd               VARCHAR NOT NULL,        -- Working directory

    -- Command
    cmd               VARCHAR NOT NULL,        -- Full command string
    executable        VARCHAR,                 -- Extracted executable name
    runner_id         VARCHAR,                 -- Runner identifier (for liveness checking)

    -- Result
    exit_code         INTEGER,                 -- NULL while pending
    status            VARCHAR DEFAULT 'completed',  -- pending, completed, orphaned

    -- Format detection
    format_hint       VARCHAR,                 -- Detected format (gcc, cargo, pytest)

    -- Client identity
    client_id         VARCHAR NOT NULL,        -- user@hostname
    hostname          VARCHAR,
    username          VARCHAR,
    tag               VARCHAR,                 -- User-assigned tag

    -- Partitioning (status is first-level hive partition)
    date              DATE NOT NULL
);
```

**Status Values:**

| Status | Description |
|--------|-------------|
| `pending` | Command is currently running (exit_code is NULL) |
| `completed` | Command finished normally (exit code captured) |
| `orphaned` | Process died without cleanup (crash, SIGKILL, system reboot) |

### Sessions Table

A shell or process that captures invocations.

```sql
CREATE TABLE sessions (
    -- Identity
    session_id        VARCHAR PRIMARY KEY,     -- e.g., "zsh-12345"
    client_id         VARCHAR NOT NULL,        -- user@hostname

    -- Invoker information
    invoker           VARCHAR NOT NULL,        -- e.g., "zsh", "bash", "shq"
    invoker_pid       INTEGER NOT NULL,
    invoker_type      VARCHAR NOT NULL,        -- "shell", "cli", "hook", "script"

    -- Timing
    registered_at     TIMESTAMP NOT NULL,

    -- Context
    cwd               VARCHAR,                 -- Initial working directory

    -- Partitioning
    date              DATE NOT NULL
);
```

### Outputs Table

Captured stdout/stderr from an invocation.

```sql
CREATE TABLE outputs (
    -- Identity
    id                UUID PRIMARY KEY,        -- UUIDv7
    invocation_id     UUID NOT NULL,           -- References invocations.id

    -- Stream
    stream            VARCHAR NOT NULL,        -- 'stdout', 'stderr', or 'combined'

    -- Content identification
    content_hash      VARCHAR NOT NULL,        -- BLAKE3 hash (hex, 64 chars)
    byte_length       BIGINT NOT NULL,

    -- Storage location (polymorphic)
    storage_type      VARCHAR NOT NULL,        -- 'inline' or 'blob'
    storage_ref       VARCHAR NOT NULL,        -- URI to content (see below)

    -- Content metadata
    content_type      VARCHAR,                 -- MIME type or format hint

    -- Partitioning
    date              DATE NOT NULL
);
```

**Storage Reference Formats:**

| Type | Format | Example |
|------|--------|---------|
| Inline (small) | `data:` URI | `data:application/octet-stream;base64,SGVsbG8=` |
| Local blob | `file:` relative path | `file:ab/abc123--make.bin` |
| S3 blob | Full S3 URL | `s3://bucket/blobs/ab/abc123--make.bin` |

### Events Table

Parsed diagnostics from invocation output (errors, warnings, test results).

```sql
CREATE TABLE events (
    -- Identity
    id                UUID PRIMARY KEY,        -- UUIDv7
    invocation_id     UUID NOT NULL,           -- References invocations.id

    -- Client identity (denormalized for cross-client queries)
    client_id         VARCHAR NOT NULL,
    hostname          VARCHAR,

    -- Event classification
    event_type        VARCHAR,                 -- 'diagnostic', 'test_result', etc.
    severity          VARCHAR,                 -- 'error', 'warning', 'info', 'note'

    -- Source location
    ref_file          VARCHAR,                 -- Source file path
    ref_line          INTEGER,                 -- Line number
    ref_column        INTEGER,                 -- Column number

    -- Content
    message           VARCHAR,                 -- Error/warning message
    error_code        VARCHAR,                 -- e.g., "E0308", "W0401"

    -- Test-specific fields
    test_name         VARCHAR,                 -- Test name (for test results)
    status            VARCHAR,                 -- 'passed', 'failed', 'skipped'

    -- Parsing metadata
    format_used       VARCHAR NOT NULL,        -- Parser format (gcc, cargo, pytest)

    -- Partitioning
    date              DATE NOT NULL
);
```

## Output Storage

### Inline Storage (Small Outputs)

Outputs under `inline_threshold` (default: 4KB) are stored as base64 data URIs:

```
storage_type:  'inline'
storage_ref:   'data:application/octet-stream;base64,SGVsbG8gV29ybGQK'
```

**Benefits:**
- No separate file needed
- Fast queries (data in parquet/table)
- Simple backups

### Blob Storage (Large Outputs)

Large outputs are stored as content-addressed files:

```
storage_type:  'blob'
storage_ref:   'file:ab/abc123def--make.bin'
```

**Filename format:**
```
{hash[0:2]}/{hash}--{cmd-hint}.bin

Example: ab/abc123def456789...--make-test.bin
```

**Benefits:**
- Automatic deduplication (same hash = same file)
- 70-90% storage savings for repetitive CI workloads
- Integrity verification via hash

### Blob Resolution

Blobs can exist in multiple locations (local, archive, remote S3). BIRD uses a `blob_roots` list for resolution:

```sql
-- Set at connection time
SET VARIABLE blob_roots = [
    '/home/user/.local/share/bird/db/data/recent/blobs/content',
    's3://team-bucket/bird/blobs'
];

-- Resolve storage_ref across all roots
SELECT resolve_storage_ref(storage_ref) FROM outputs;
```

## Filename Formats

### Parquet Files (Invocations, Outputs, Sessions, Events)

```
<session>--<hint>--<uuid>.parquet
```

**Components:**
- `<session>`: Session identifier (sanitized, max 32 chars)
- `<hint>`: Executable name or format name (sanitized, max 64 chars)
- `<uuid>`: UUIDv7 (timestamp-ordered, collision-free)

**Examples:**
```
zsh-12345--make--01937a2b-3c4d-7e8f-9012-3456789abcde.parquet
zsh-12345--cargo--01937a2c-1234-5678-9abc-def012345678.parquet
```

### Compacted Files

```
<session>--__compacted-N__--<uuid>.parquet
```

**Example:**
```
zsh-12345--__compacted-0__--01937b5e-6f7a-8b9c-0123-456789abcdef.parquet
```

The compaction generation `N` increments each time a partition is compacted.

### Content-Addressed Blobs

```
{hash[0:2]}/{hash}--{cmd-hint}.bin
```

**Format:**
- First 2 hex chars as subdirectory (prevents filesystem slowdown)
- BLAKE3 hash (64 hex chars)
- Command hint for human readability
- `.bin` extension (uncompressed by default)

**Example:**
```
ab/abc123def456789abcdef0123456789abcdef0123456789abcdef0123456789--make-test.bin
```

## Remote Storage

BIRD supports syncing data to remote databases for backup, sharing, and cross-machine access.

### Remote Types

| Type | URI Format | Description |
|------|------------|-------------|
| `file` | `/path/to/bird.duckdb` | Local or network file |
| `s3` | `s3://bucket/path/bird.duckdb` | S3-compatible storage |
| `motherduck` | `md:database_name` | MotherDuck cloud |
| `postgres` | `postgres:dbname=...` | PostgreSQL database |

### Configuration

Remotes are configured in `config.toml`:

```toml
[[remotes]]
name = "team"
type = "s3"
uri = "s3://team-bucket/bird/bird.duckdb"
credential_provider = "credential_chain"
auto_attach = true

[[remotes]]
name = "backup"
type = "file"
uri = "/mnt/backup/bird.duckdb"
mode = "read_only"
auto_attach = false

[sync]
default_remote = "team"
sync_invocations = true
sync_outputs = true
sync_events = true
```

### Querying Remotes

Remotes are attached as DuckDB schemas using `ATTACH`:

```sql
-- Auto-attached remotes available as schemas
SELECT * FROM "remote_team".invocations WHERE client_id = 'alice@laptop';

-- Query across local and remote
SELECT * FROM invocations
UNION ALL
SELECT * FROM "remote_team".invocations;
```

### Push/Pull Sync

Data sync uses `INSERT INTO ... SELECT` with anti-join to avoid duplicates:

```bash
# Push local data to remote
shq push --remote team              # Push all new data
shq push --remote team --since 7d   # Push last 7 days
shq push --remote team --dry-run    # Preview what would be pushed

# Pull remote data to local
shq pull --remote team                    # Pull all new data
shq pull --remote team --client bob@work  # Pull specific client
```

**Sync order matters** - data is synced in dependency order:
1. Sessions (referenced by invocations)
2. Invocations (referenced by outputs/events)
3. Outputs
4. Events

## Compaction

### When to Compact

Compaction merges many small parquet files into fewer large ones:

- **Trigger**: When a session has more than N files (default: 50) in a date partition
- **Automatic**: Shell hooks run background compaction after each command
- **Manual**: `shq compact` for full compaction

### How Compaction Works

1. Find sessions with file count exceeding threshold
2. Read all files for that session/date into memory
3. Write consolidated file with `__compacted-N__` naming
4. Delete original files

**Note:** Blobs are never compacted - they're already deduplicated by content hash.

## Archival

Move old data from recent tier to archive tier:

```bash
shq archive              # Archive data older than 14 days (default)
shq archive --days 30    # Archive data older than 30 days
shq archive --dry-run    # Preview what would be archived
```

Archive tier uses different partitioning optimized for cold storage:
- Organized by `client/year/week` instead of `date`
- Larger consolidated files
- Blobs move to archive pool when all referencing invocations are archived

## Performance Targets

### Capture

- **Hook overhead:** <5ms per command (critical!)
- **`shq run` overhead:** <10ms vs native execution
- **Write latency:** <50ms for typical output (<1MB)
- **Hash overhead:** ~2ms for 5MB output (BLAKE3 is fast!)
- **Dedup check:** ~1ms (indexed lookup)

### Query

- **Recent data (<14 days):** <100ms
- **Archive data (30 days):** <500ms
- **Full scan (1 year, 100K commands):** <5s

### Storage

**Without deduplication:**
- 10K commands/day × 5MB average output = 50GB/day
- 1 year = 18TB

**With content-addressing (90% dedup):**
- 10K commands/day × 0.5MB unique = 5GB/day  
- 1 year = 1.8TB
- **Savings: 90% (16TB saved!)**

## Concurrency Model

### Lock-Free Capture (Normal Operation)

- Each command writes unique parquet file (UUIDv7)
- Blob writes use atomic rename (handles races)
- Multiple shells can write simultaneously
- No coordination needed

### Blob Race Condition Handling

```rust
// Atomic write (handles concurrent writes of same hash)
let temp = format!(".tmp.{}.bin.gz", hash);
write_compressed(&temp, data)?;

match fs::rename(&temp, &final_path) {
    Ok(_) => Ok(final_path),
    Err(e) if e.kind() == AlreadyExists => {
        // Another process wrote same hash, that's fine!
        fs::remove_file(&temp)?;
        Ok(final_path)  // Use existing file
    },
    Err(e) => Err(e)
}
```

### Locked Compaction (Administrative)

- Uses `compaction.lock` for parquet compaction
- Blob pool requires no locking (content-addressed)

### In-Flight Invocation Tracking

To handle crashes/interrupts during command execution, BIRD tracks in-flight invocations in a lightweight file that doesn't require DuckDB access:

```
$BIRD_ROOT/db/pending/<session_id>--<uuid>.pending
```

**Lifecycle:**

1. **Command starts**:
   - Create JSON pending file (fast, crash-safe marker)
   - Write parquet to `status=pending/date=.../` partition
2. **Command runs**: Output captured to temp files/buffers
3. **Command ends**:
   - Write final invocation record to `status=completed/date=.../`
   - Delete pending parquet file from `status=pending/`
   - Delete JSON pending file

**Pending File Format (JSON):**

```json
{
  "id": "01937a2b-3c4d-7e8f-9012-3456789abcde",
  "session_id": "zsh-12345",
  "timestamp": "2024-01-15T10:30:00Z",
  "cmd": "make test",
  "cwd": "/home/user/project",
  "client_id": "user@hostname",
  "runner_id": "pid:12345"
}
```

**Runner ID Formats:**

| Context | Format | Example |
|---------|--------|---------|
| Local process | `pid:<pid>` | `pid:12345` |
| GitHub Actions | `gha:run:<run_id>` | `gha:run:123456789` |
| Kubernetes | `k8s:pod:<pod_name>` | `k8s:pod:build-abc123` |
| Docker | `docker:<container_id>` | `docker:a1b2c3d4e5f6` |

**Runner-Based Liveness Checking:**

The runner_id enables checking if an execution context is still active:

```rust
/// Check if a runner is still alive based on its ID format
fn is_runner_alive(runner_id: &str) -> bool {
    if let Some(pid_str) = runner_id.strip_prefix("pid:") {
        // Local process: use kill(pid, 0)
        if let Ok(pid) = pid_str.parse::<i32>() {
            return is_process_alive(pid);
        }
    } else if runner_id.starts_with("gha:") {
        // GitHub Actions: check via API or assume alive if recent
        return true; // Can't easily check, assume alive
    } else if runner_id.starts_with("k8s:") {
        // Kubernetes: could check pod status via kubectl/API
        return true; // Requires k8s access
    }
    // Unknown format - assume alive to be safe
    true
}

/// Check if a local process is still alive
fn is_process_alive(pid: i32) -> bool {
    #[cfg(unix)]
    {
        use nix::sys::signal::kill;
        use nix::unistd::Pid;
        kill(Pid::from_raw(pid), None).is_ok()
    }
    #[cfg(not(unix))]
    {
        true // Conservative: assume alive on non-Unix
    }
}
```

**Recovery Logic:**

```rust
fn recover_pending_invocations(store: &Store) -> Result<RecoveryStats> {
    let pending_dir = store.config().pending_dir();
    let mut stats = RecoveryStats::default();

    for entry in fs::read_dir(&pending_dir)?.flatten() {
        let path = entry.path();
        if path.extension() != Some("pending".as_ref()) {
            continue;
        }

        let content = fs::read_to_string(&path)?;
        let pending: PendingInvocation = serde_json::from_str(&content)?;

        if is_runner_alive(&pending.runner_id) {
            // Runner still active - leave pending file alone
            stats.still_running += 1;
            continue;
        }

        // Runner is dead - record as orphaned
        let record = InvocationRecord {
            id: pending.id,
            session_id: pending.session_id,
            timestamp: pending.timestamp,
            duration_ms: None,  // Unknown
            cwd: pending.cwd,
            cmd: pending.cmd,
            executable: extract_executable(&pending.cmd),
            runner_id: Some(pending.runner_id),
            exit_code: None,    // Unknown/crashed
            status: "orphaned".to_string(),
            // ... other fields
        };

        store.write_invocation_with_status(&record, "orphaned")?;
        fs::remove_file(&path)?;
        stats.orphaned += 1;
    }

    Ok(stats)
}
```

**Recovery Scenarios:**

| Scenario | Runner Check | Action |
|----------|--------------|--------|
| Pending file, runner alive | Liveness check succeeds | Leave alone (still running) |
| Pending file, runner dead | Liveness check fails | Record as orphaned, delete pending |
| Pending file, stale (>24h) | Any | Record as orphaned (runner_id may have been recycled) |
| Pending file, unknown runner type | N/A | Treat as stale after max_age |

**Benefits:**

- **Fast**: Single file write at command start (~1ms)
- **Crash-safe**: Plain file survives DuckDB crashes
- **Concurrent-safe**: Unique filename per invocation
- **Debuggable**: Human-readable JSON with PID
- **Recoverable**: Can reconstruct incomplete records on restart
- **Liveness check**: PID enables detection of still-running vs dead processes

**Cleanup:**

Pending files are automatically cleaned up:
- On normal command completion: deleted immediately after writing to `status=completed/`
- Via `shq clean`: checks all pending files, records orphaned ones
- Background task: periodic cleanup of orphaned pending files

### Clean/Prune Command

The `shq clean` command processes orphaned invocations and cleans up stale data:

```bash
# Check for orphaned processes and record them
shq clean

# Preview what would be cleaned (no changes)
shq clean --dry-run

# Force cleanup of pending files older than N hours (default: 24)
shq clean --max-age 12

# Also prune old archive data
shq clean --prune --older-than 90d
```

**Clean Operation:**

1. **Scan pending files** in `$BIRD_ROOT/db/pending/`
2. **Check PID liveness** for each pending invocation
3. **For dead processes**:
   - Read partial output if available
   - Write invocation record to `status=orphaned/date=.../`
   - Delete JSON pending file and parquet from `status=pending/`
4. **For stale files** (older than `--max-age`):
   - Assume process is dead (PID may have been recycled)
   - Same handling as dead processes

**Prune Operation (--prune):**

Removes old data from the archive tier:

| Data Type | Default Retention | Flag |
|-----------|-------------------|------|
| Invocations | 365 days | `--older-than` |
| Outputs | 90 days | `--outputs-older-than` |
| Events | 365 days | `--events-older-than` |
| Orphaned blobs | 30 days | `--blobs-older-than` |

```rust
pub struct CleanOptions {
    pub dry_run: bool,
    pub max_age_hours: u32,      // For pending files (default: 24)
    pub prune: bool,             // Enable archive pruning
    pub older_than_days: u32,    // For archive data (default: 365)
}

pub struct CleanStats {
    pub pending_checked: usize,
    pub still_running: usize,
    pub orphaned: usize,
    pub pruned_files: usize,
    pub bytes_freed: u64,
}
```

**Safety:**

- `--dry-run` always shows what would be done without making changes
- Blobs are only deleted if no invocations reference them (ref_count = 0)
- Archive pruning requires explicit `--prune` flag

## Error Handling

**Principle: Never break the shell.**

### Capture Failures

If capture fails:
- Command still executes normally
- Error logged to `errors.log`
- Shell continues unaffected

### Blob Write Failures

If blob write fails:
- Fall back to inline storage (even if large)
- Or: Store empty reference with error flag
- Never block command execution

### Deduplication Failures

If dedup check fails:
- Write new blob (safe fallback)
- Some duplication acceptable vs blocking

## Configuration

Configuration is stored in `$BIRD_ROOT/config.toml`:

```toml
# Client identity
client_id = "user@hostname"

# Storage settings
storage_mode = "parquet"      # "parquet" or "duckdb"
hot_days = 14                 # Days before archiving
inline_threshold = 4096       # Bytes (inline vs blob)
auto_extract = false          # Auto-extract events after shq run

# Remote storage
[[remotes]]
name = "team"
type = "s3"
uri = "s3://bucket/bird/bird.duckdb"
credential_provider = "credential_chain"
auto_attach = true

[[remotes]]
name = "backup"
type = "file"
uri = "/mnt/backup/bird.duckdb"
mode = "read_only"
auto_attach = false

# Sync settings
[sync]
default_remote = "team"
sync_invocations = true
sync_outputs = true
sync_events = true
sync_blobs = false            # Blob sync not yet implemented
```

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `BIRD_ROOT` | `~/.local/share/bird` | Base directory for BIRD data |
| `SHQ_DISABLED` | unset | Set to `1` to disable all capture |
| `SHQ_EXCLUDE` | unset | Colon-separated patterns to exclude |
| `BIRD_INVOCATION_UUID` | unset | Shared invocation UUID for nested clients |
| `BIRD_PARENT_CLIENT` | unset | Name of parent BIRD client (e.g., "shq") |

## Multi-Client Integration

BIRD supports multiple clients (shq, blq, etc.) that can invoke each other. To avoid duplicate recording of the same invocation across databases, clients share a common UUID.

### Nested Invocations

When a BIRD client runs a command that invokes another BIRD client:

```bash
# shq captures this invocation and sets env vars for child
$ shq run blq run cargo test

# Child process receives:
#   BIRD_INVOCATION_UUID=<shared-uuid>
#   BIRD_PARENT_CLIENT=shq
```

**Protocol:**
1. Parent client generates invocation UUID before spawning child
2. Parent sets `BIRD_INVOCATION_UUID` to the UUID string
3. Parent sets `BIRD_PARENT_CLIENT` to its own name (e.g., "shq", "blq")
4. Child client checks for `BIRD_INVOCATION_UUID` and uses it if present
5. Both clients record with the same UUID, enabling deduplication

**Implementation (Rust):**
```rust
// Check for inherited UUID
let id = std::env::var("BIRD_INVOCATION_UUID")
    .ok()
    .and_then(|s| Uuid::parse_str(&s).ok())
    .unwrap_or_else(Uuid::now_v7);
```

**Implementation (Python):**
```python
import os
import uuid

inv_uuid = os.environ.get("BIRD_INVOCATION_UUID")
if inv_uuid:
    invocation_id = uuid.UUID(inv_uuid)
else:
    invocation_id = uuid.uuid4()  # or uuid7
```

## Project Detection

BIRD clients can detect project-level databases by looking for `.bird/` directories.

### Directory Structure

```
/home/user/projects/myapp/
├── .bird/                    # Project-level BIRD
│   ├── bird.duckdb          # Project database (CI builds, etc.)
│   ├── config.toml          # Project sync config
│   └── blobs/content/       # Project blobs
├── src/
└── ...
```

### Detection Algorithm

Walk up from current directory looking for `.bird/`:

```rust
fn find_project(start: &Path) -> Option<ProjectInfo> {
    let mut current = start.to_path_buf();
    loop {
        let bird_dir = current.join(".bird");
        if bird_dir.is_dir() {
            return Some(ProjectInfo {
                root: current,
                bird_dir,
                db_path: bird_dir.join("bird.duckdb"),
            });
        }
        if !current.pop() { break; }
    }
    None
}
```

### Dynamic Attachment

User-level clients (shq) can attach project databases as read-only:

```sql
-- Automatic when in project directory
ATTACH '/path/to/project/.bird/bird.duckdb' AS project (READ_ONLY);

-- Query project data alongside user data
SELECT * FROM project.invocations;  -- CI builds
SELECT * FROM invocations;          -- Local commands
```

## Format Hints

Format hints help BIRD detect the output format for event parsing:

```toml
# $BIRD_ROOT/format-hints.toml

# Pattern-based hints
[[hints]]
pattern = "make*"
format = "gcc"

[[hints]]
pattern = "cargo *"
format = "cargo"

[[hints]]
pattern = "pytest*"
format = "pytest"

# Default format for unknown commands
default_format = "auto"
```

Supported formats: `gcc`, `cargo`, `pytest`, `eslint`, `tsc`, `go`, `auto`

---

*Version 3: Dual storage backends + remote sync* 🎯
