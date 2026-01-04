# BIRD Specification v2 (Content-Addressed Storage)

**BIRD**: Buffer and Invocation Record Database

BIRD is the database backend for shq, using DuckDB and Parquet for efficient storage and querying of shell command history. This version adds **content-addressed blob storage** for automatic deduplication.

## Overview

BIRD stores every shell command execution as:
- **Command metadata**: timestamp, exit code, duration, working directory
- **Output streams**: stdout and stderr (with smart content-addressed storage for large outputs)
- **Parsed events**: Errors, warnings, and other structured log data

Data is organized by date for efficient querying and archival. Large outputs (≥1MB) are deduplicated via content-addressed storage.

## Directory Structure

```
$BIRD_ROOT/                          # Default: ~/.local/share/bird
├── db/
│   ├── bird.duckdb                  # Pre-configured views and macros
│   ├── data/
│   │   ├── recent/                  # Last 14 days (hot data)
│   │   │   ├── commands/
│   │   │   │   └── date=YYYY-MM-DD/
│   │   │   │       └── <session>--<exec>--<uuid>.parquet
│   │   │   ├── outputs/
│   │   │   │   └── date=YYYY-MM-DD/
│   │   │   │       └── <session>--<exec>--<uuid>.parquet
│   │   │   └── blobs/
│   │   │       └── content/         # Content-addressed pool
│   │   │           ├── ab/
│   │   │           │   └── abc123def456...789.bin.gz
│   │   │           ├── cd/
│   │   │           │   └── cde456789012...345.bin.gz
│   │   │           └── ...          # 256 subdirs (00-ff)
│   │   └── archive/                 # >14 days (cold data)
│   │       └── by-week/
│   │           ├── commands/
│   │           │   └── client=<n>/year=YYYY/week=WW/*.parquet
│   │           ├── outputs/
│   │           │   └── client=<n>/year=YYYY/week=WW/*.parquet
│   │           └── blobs/
│   │               └── content/     # Global content pool (archived)
│   │                   ├── ab/
│   │                   └── cd/
│   └── sql/
│       ├── init.sql                 # Complete initialization
│       ├── views.sql                # View definitions
│       └── macros.sql               # Macro definitions
├── config.toml                      # Configuration
└── errors.log                       # Capture error log
```

### Directory Rationale

- **db/**: Contains both data and database objects
- **db/data/**: Separates data from metadata (bird.duckdb, SQL files)
- **recent/**: Optimized for fast access, date-partitioned
- **archive/**: Optimized for compression, organized by client/year/week
- **blobs/content/**: Content-addressed blob storage with automatic deduplication
  - Subdirectories: First 2 hex chars of hash (`ab/`, `cd/`, etc.)
  - Prevents filesystem slowdown with >10k files/directory
  - Same blob shared by multiple commands → **70-90% storage savings** for CI workloads
  - See **CONTENT_ADDRESSED_BLOBS.md** for full design

## Schema

### Commands Table (Parquet)

```sql
CREATE TABLE commands (
    -- Identity
    id                UUID PRIMARY KEY,
    session_id        TEXT NOT NULL,           -- Session identifier
    
    -- Timing
    timestamp         TIMESTAMP NOT NULL,
    duration_ms       BIGINT,
    
    -- Context
    cwd               TEXT NOT NULL,
    env_hash          TEXT,                     -- Hash of environment
    
    -- Command
    cmd               TEXT NOT NULL,
    executable        TEXT,                     -- Extracted executable name
    args              TEXT[],                   -- Parsed arguments (if available)
    
    -- Result
    exit_code         INT NOT NULL,
    
    -- Format detection
    format_hint       TEXT,                     -- Detected format (gcc, pytest, etc.)
    
    -- Output references
    stdout_file       TEXT,                     -- Path to stdout redirect
    stderr_file       TEXT,                     -- Path to stderr redirect
    has_stdout        BOOLEAN DEFAULT FALSE,
    has_stderr        BOOLEAN DEFAULT FALSE,
    
    -- Metadata
    client_id         TEXT NOT NULL,            -- Client identifier
    hostname          TEXT,
    username          TEXT,
    
    -- Partitioning
    date              DATE GENERATED ALWAYS AS (CAST(timestamp AS DATE))
);
```

### Outputs Table (Parquet) - Updated for Content-Addressing

```sql
CREATE TABLE outputs (
    -- Identity
    id                UUID PRIMARY KEY,
    command_id        UUID NOT NULL,            -- References commands.id
    
    -- Content identification
    content_hash      TEXT NOT NULL,            -- BLAKE3 hash (hex)
    byte_length       BIGINT NOT NULL,
    
    -- Storage location (polymorphic)
    storage_type      TEXT NOT NULL,            -- 'inline', 'blob', 'tarfs', 'archive'
    storage_ref       TEXT NOT NULL,            -- URI to content
    
    -- Content metadata
    stream            TEXT NOT NULL,            -- 'stdout' or 'stderr'
    content_type      TEXT,                     -- MIME type or format hint
    encoding          TEXT DEFAULT 'utf-8',
    
    -- Status
    compressed        BOOLEAN DEFAULT FALSE,
    truncated         BOOLEAN DEFAULT FALSE,
    
    -- Partitioning
    date              DATE NOT NULL
);

CREATE INDEX idx_outputs_hash ON outputs(content_hash);
CREATE INDEX idx_outputs_storage ON outputs(storage_type);
```

**Key changes:**
- `content_hash`: BLAKE3 hash of content (deduplication key)
- `storage_type`: How content is stored ('inline', 'blob', 'tarfs', 'archive')
- `storage_ref`: URI to actual content
- Removed `content` BLOB column (now in storage_ref)
- Removed `file_ref` (now unified as storage_ref)

### Blob Registry Table (DuckDB)

```sql
CREATE TABLE blob_registry (
    -- Identity
    content_hash      TEXT PRIMARY KEY,        -- BLAKE3 hash
    byte_length       BIGINT NOT NULL,
    compression       TEXT DEFAULT 'gzip',     -- gzip, zstd, none
    
    -- Reference tracking
    ref_count         INT DEFAULT 0,           -- How many outputs reference this
    first_seen        TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    last_accessed     TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    
    -- Storage location
    storage_tier      TEXT,                    -- 'recent', 'archive'
    storage_path      TEXT,                    -- Relative path from BIRD_ROOT
    
    -- Integrity
    verified_at       TIMESTAMP,               -- Last integrity check
    corrupt           BOOLEAN DEFAULT FALSE
);

CREATE INDEX idx_blob_registry_refs ON blob_registry(ref_count);
CREATE INDEX idx_blob_registry_tier ON blob_registry(storage_tier);
CREATE INDEX idx_blob_registry_accessed ON blob_registry(last_accessed);
```

**Purpose:**
- Central registry of all blobs
- Tracks reference counts for garbage collection
- Enables integrity verification
- Supports storage tier migration

## Storage Modes

### Small Outputs (<1MB): Inline Storage

```sql
content_hash:  'abc123def...'
storage_type:  'inline'
storage_ref:   'data:application/octet-stream;base64,SGVsbG8gV29ybGQK'
```

**Benefits:**
- No separate file needed
- Fast queries (data in parquet)
- Simple backups

### Large Outputs (≥1MB): Content-Addressed Blob

```sql
content_hash:  'abc123def...'
storage_type:  'blob'
storage_ref:   'file://recent/blobs/content/ab/abc123def.bin.gz'
```

**Benefits:**
- Automatic deduplication (same hash = same file)
- 70-90% storage savings for repetitive CI workloads
- Integrity verification via hash

**Filename format:**
```
recent/blobs/content/{hash[0:2]}/{hash}.bin.gz

Example:
recent/blobs/content/ab/abc123def456789abcdef0123456789abcdef0123456789.bin.gz
```

### Compacted: Tarfs (Not Applicable to Blobs)

Blobs use content-addressing and don't need compaction. Only parquet files are compacted.

### Archived: Global Content Pool

```sql
content_hash:  'abc123def...'
storage_type:  'archive'
storage_ref:   'file://archive/blobs/content/ab/abc123def.bin.gz'
```

Blobs move to archive pool when all referencing commands are archived.

## Filename Formats

### Commands/Outputs (Recent and Archive)

```
<session>--<executable>--<uuid>.parquet
```

**Components:**
- `<session>`: Session identifier (sanitized, max 32 chars)
- `<executable>`: Executable name from command (sanitized, max 64 chars)
- `<uuid>`: UUIDv7 (timestamp-ordered, collision-free)

**Example:**
```
laptop-12345--make--01937a2b-3c4d-7e8f-9012-3456789abcde.parquet
```

### Compacted Files

```
<session>--__compacted-N__--<uuid>.parquet
```

**Example:**
```
laptop-12345--__compacted-0__--01937b5e-6f7a-8b9c-0123-456789abcdef.parquet
```

### Content-Addressed Blobs

```
{hash[0:2]}/{hash}.bin.gz
```

**Format:**
- First 2 hex chars as subdirectory
- BLAKE3 hash (64 hex chars)
- `.bin.gz` extension (gzip compressed binary)

**Example:**
```
ab/abc123def456789abcdef0123456789abcdef0123456789abcdef0123456789.bin.gz
```

**Why not UUIDs?**
- Hash = content, enabling automatic deduplication
- Same output → same filename → reuse existing file
- Huge storage savings for CI workloads

## Capture Flow (with Deduplication)

```rust
pub fn capture_output(&mut self, data: &[u8], stream: &str) -> Result<OutputId> {
    // 1. Compute content hash
    let hash = blake3::hash(data);
    let hash_hex = hash.to_hex();
    
    // 2. Size-based routing
    let (storage_type, storage_ref) = if data.len() < SIZE_THRESHOLD {
        // Small: inline with data: URI
        ("inline", format!("data:;base64,{}", base64::encode(data)))
    } else {
        // Large: check if blob exists (deduplication!)
        if let Some(path) = self.check_blob_exists(&hash_hex)? {
            // DEDUP HIT: Increment ref count, reuse path
            self.increment_ref_count(&hash_hex)?;
            ("blob", format!("file://{}", path))
        } else {
            // DEDUP MISS: Write new blob
            let path = self.write_blob(&hash_hex, data)?;
            self.register_blob(&hash_hex, data.len(), &path)?;
            ("blob", format!("file://{}", path))
        }
    };
    
    // 3. Insert output record
    self.db.execute(
        "INSERT INTO outputs 
            (id, command_id, stream, content_hash, byte_length, storage_type, storage_ref, date)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        params![uuid, cmd_id, stream, &hash_hex, data.len(), storage_type, storage_ref, date]
    )?;
    
    Ok(output_id)
}
```

**Key insight:** Same hash → reuse existing file → storage savings!

## Compaction (Updated)

### Parquet Files: Yes (As Before)

**When:**
- Date partition exceeds threshold (default: 100 files)
- Triggered hourly or manually

**How:**
- Oldest 50% of files merged into `__compacted-N__` file
- Applies to commands/ and outputs/ directories

**See:** BLOB_COMPACTION_SUMMARY.md for details

### Blob Files: No Compaction Needed!

**Why not?**
- Content-addressed blobs are already deduplicated
- No date partitioning (blobs are timeless, referenced by hash)
- Compaction benefit comes from merging small parquets, not blob files

**What happens instead:**
- Blobs stay in content pool indefinitely
- Garbage collection removes unreferenced blobs (optional)
- Archival moves old blobs to archive pool (preserves dedup)

## Garbage Collection (Optional)

### When to GC?

Blobs can accumulate if commands are deleted but blobs remain.

### Strategy 1: Never Delete (MVP)

```
✅ Simplest
✅ Storage is cheap  
✅ Content-addressing already saves 70-90%
❌ Disk usage grows over time
```

### Strategy 2: Reference Counting

```sql
-- Find orphaned blobs
SELECT content_hash, storage_path, ref_count
FROM blob_registry
WHERE ref_count = 0
  AND last_accessed < NOW() - INTERVAL '30 days';

-- Delete (after grace period)
DELETE FROM blob_registry WHERE ref_count = 0 AND ...;
```

**Recommended for production.**

### Strategy 3: Mark-and-Sweep (Batch)

```sql
-- Mark: Find all referenced hashes
CREATE TEMP TABLE referenced AS
SELECT DISTINCT content_hash FROM outputs;

-- Sweep: Delete unreferenced
DELETE FROM blob_registry
WHERE content_hash NOT IN (SELECT * FROM referenced)
  AND last_accessed < NOW() - INTERVAL '30 days';
```

**Recommended for migrations.**

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

## Migration from UUID Blobs

For existing BIRD installations:

```bash
bird migrate-to-content-addressed

# Process:
# 1. Hash all existing blobs
# 2. Move to content-addressed paths  
# 3. Update outputs.file_ref → outputs.storage_ref
# 4. Populate blob_registry
# 5. Remove old managed/ directory
```

**See:** CONTENT_ADDRESSED_BLOBS.md for migration details

## Configuration

```toml
[storage]
threshold_bytes = 1048576     # 1MB (inline vs blob)
compression = "gzip"          # gzip, zstd, none
compression_level = 6         # 1-9 for gzip

[deduplication]
enabled = true                # Enable content-addressing
hash_algorithm = "blake3"     # blake3, sha256

[garbage_collection]
enabled = false               # Disable for MVP
strategy = "ref_counting"     # ref_counting, mark_sweep
grace_period_days = 30        # Keep orphaned blobs for 30 days
```

## Summary of Changes

| Aspect | Old (UUID) | New (Content-Addressed) |
|--------|-----------|-------------------------|
| **Blob filename** | `{uuid}.bin.zst` | `{hash[0:2]}/{hash}.bin.gz` |
| **Duplication** | Every output unique | Identical outputs shared |
| **Directory** | `managed/` | `blobs/content/` with subdirs |
| **Schema** | `file_ref` column | `content_hash` + `storage_ref` + `storage_type` |
| **Registry** | None | `blob_registry` table |
| **Compression** | zstd | gzip (DuckDB compatible) |
| **Compaction** | Tar archives | Not needed (already deduped) |
| **Storage** | O(n) commands | O(unique content) |
| **Savings** | 0% | **70-90% for CI workloads** |

## References

- **CONTENT_ADDRESSED_BLOBS.md** - Complete design document
- **STORAGE_LIFECYCLE.md** - Updated lifecycle with deduplication
- **BLOB_COMPACTION_SUMMARY.md** - Why blobs don't need compaction
- **COMPRESSION_DECISION.md** - Why gzip not zstd

---

*Version 2: Content-addressed storage for automatic deduplication* 🎯
