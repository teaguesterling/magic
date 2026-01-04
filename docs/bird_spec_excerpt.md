# BIRD Schema for Content-Addressed Storage

This is an excerpt from bird_spec.md showing only the essential schema definitions you need.

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

## Commands Table (Parquet)

Located: `db/data/recent/commands/date=YYYY-MM-DD/*.parquet`

```sql
CREATE TABLE commands (
    -- Identity
    id                UUID PRIMARY KEY,
    
    -- Command details
    cmd               TEXT NOT NULL,
    program           TEXT,
    args              TEXT[],
    
    -- Execution
    timestamp         TIMESTAMP NOT NULL,
    duration_ms       BIGINT,
    exit_code         INT,
    
    -- Context
    cwd               TEXT,
    env_hash          TEXT,
    
    -- Session
    session_name      TEXT NOT NULL,
    session_id        UUID NOT NULL,
    exec_id           BIGINT NOT NULL,
    
    -- Streams
    has_stdout        BOOLEAN DEFAULT FALSE,
    has_stderr        BOOLEAN DEFAULT FALSE,
    
    -- Metadata
    client_id         TEXT NOT NULL,
    hostname          TEXT,
    username          TEXT,
    
    -- Partitioning
    date              DATE GENERATED ALWAYS AS (CAST(timestamp AS DATE))
);
```

## Outputs Table (Parquet) - Content-Addressed

Located: `db/data/recent/outputs/date=YYYY-MM-DD/*.parquet`

```sql
CREATE TABLE outputs (
    -- Identity
    id                UUID PRIMARY KEY,
    command_id        UUID NOT NULL,            -- References commands.id
    
    -- Content identification (NEW!)
    content_hash      TEXT NOT NULL,            -- BLAKE3 hash (hex)
    byte_length       BIGINT NOT NULL,
    
    -- Storage location (NEW! - polymorphic)
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

**Key changes from UUID-based:**
- `content_hash`: BLAKE3 hash of content (deduplication key)
- `storage_type`: How content is stored ('inline', 'blob', 'tarfs', 'archive')
- `storage_ref`: URI to actual content
- Removed `content` BLOB column (now in storage_ref)
- Removed `file_ref` (now unified as storage_ref)

## Blob Registry Table (DuckDB)

Located: `db/bird.duckdb` (in-memory metadata, not partitioned)

```sql
CREATE TABLE blob_registry (
    -- Identity
    content_hash      TEXT PRIMARY KEY,        -- BLAKE3 hash
    byte_length       BIGINT NOT NULL,
    compression       TEXT DEFAULT 'gzip',     -- gzip, zstd, none
    
    -- Reference tracking (NEW!)
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
- Tracks all content-addressed blobs
- Enables deduplication (check if hash exists before writing)
- Reference counting for garbage collection
- Integrity verification

## Storage Type Examples

### Inline Storage (< 1MB)

```sql
INSERT INTO outputs VALUES (
    id: '...',
    command_id: '...',
    content_hash: 'abc123...',
    byte_length: 1024,
    storage_type: 'inline',
    storage_ref: 'data:application/octet-stream;base64,SGVsbG8gV29ybGQh',
    stream: 'stdout',
    date: '2026-01-02'
);
```

**Read:**
```rust
// Decode base64 from data: URI
let b64 = storage_ref.split(',').nth(1)?;
let content = base64::decode(b64)?;
```

### Blob Storage (≥ 1MB, recent)

```sql
INSERT INTO outputs VALUES (
    id: '...',
    command_id: '...',
    content_hash: 'abc123def456...789',
    byte_length: 5242880,  -- 5MB
    storage_type: 'blob',
    storage_ref: 'file://recent/blobs/content/ab/abc123def456...789.bin.gz',
    stream: 'stdout',
    date: '2026-01-02'
);

-- Also insert into blob_registry
INSERT INTO blob_registry VALUES (
    content_hash: 'abc123def456...789',
    byte_length: 5242880,
    ref_count: 1,
    storage_tier: 'recent',
    storage_path: 'recent/blobs/content/ab/abc123def456...789.bin.gz'
);
```

**Read:**
```rust
// Extract path from file:// URI
let path = storage_ref.strip_prefix("file://")?;
let full_path = bird_root.join("db/data").join(path);

// Decompress gzip
let file = File::open(full_path)?;
let mut decoder = GzDecoder::new(file);
let mut content = Vec::new();
decoder.read_to_end(&mut content)?;
```

### Archive Storage (≥ 1MB, old)

```sql
INSERT INTO outputs VALUES (
    id: '...',
    command_id: '...',
    content_hash: 'def789...',
    byte_length: 10485760,  -- 10MB
    storage_type: 'archive',
    storage_ref: 'file://archive/blobs/content/de/def789abc123...456.bin.gz',
    stream: 'stdout',
    date: '2025-12-15'
);

-- Same blob_registry, different tier
UPDATE blob_registry 
SET storage_tier = 'archive',
    storage_path = 'archive/blobs/content/de/def789abc123...456.bin.gz'
WHERE content_hash = 'def789...';
```

## Deduplication Example

### First Write (DEDUP MISS)

```sql
-- Command 1 produces 5MB output
-- Hash: abc123def456...789

-- Check blob_registry
SELECT storage_path FROM blob_registry WHERE content_hash = 'abc123def456...789';
-- Result: (empty) → DEDUP MISS

-- Write blob to filesystem
-- blobs/content/ab/abc123def456...789.bin.gz

-- Insert into blob_registry
INSERT INTO blob_registry (content_hash, byte_length, ref_count, storage_path)
VALUES ('abc123def456...789', 5242880, 1, 'recent/blobs/content/ab/abc123def456...789.bin.gz');

-- Insert into outputs
INSERT INTO outputs (command_id, content_hash, storage_type, storage_ref, ...)
VALUES ('cmd1', 'abc123def456...789', 'blob', 'file://recent/blobs/content/ab/abc123def456...789.bin.gz', ...);
```

### Second Write (DEDUP HIT!)

```sql
-- Command 2 produces SAME 5MB output
-- Hash: abc123def456...789 (identical!)

-- Check blob_registry
SELECT storage_path FROM blob_registry WHERE content_hash = 'abc123def456...789';
-- Result: 'recent/blobs/content/ab/abc123def456...789.bin.gz' → DEDUP HIT!

-- NO FILESYSTEM WRITE! (blob already exists)

-- Increment ref_count
UPDATE blob_registry 
SET ref_count = ref_count + 1,
    last_accessed = CURRENT_TIMESTAMP
WHERE content_hash = 'abc123def456...789';

-- Insert into outputs (reuse existing blob path)
INSERT INTO outputs (command_id, content_hash, storage_type, storage_ref, ...)
VALUES ('cmd2', 'abc123def456...789', 'blob', 'file://recent/blobs/content/ab/abc123def456...789.bin.gz', ...);
```

**Result:**
- 2 commands
- 2 output records
- 1 blob file (shared!)
- ref_count = 2
- Storage: 5MB (not 10MB!)

## Query Examples

### Get output content

```sql
-- Get storage info
SELECT storage_type, storage_ref, content_hash
FROM outputs
WHERE command_id = 'abc-123-def-456';

-- Then resolve in application code:
-- - If inline: decode data: URI
-- - If blob/archive: read file:// path and decompress
```

### Find duplicate outputs

```sql
SELECT 
    content_hash,
    COUNT(*) as num_commands,
    SUM(byte_length) as total_would_be,
    MAX(byte_length) as actual_storage,
    (SUM(byte_length) - MAX(byte_length)) as bytes_saved
FROM outputs
GROUP BY content_hash
HAVING COUNT(*) > 1
ORDER BY bytes_saved DESC;
```

### Blob usage statistics

```sql
SELECT 
    storage_tier,
    COUNT(*) as num_blobs,
    SUM(byte_length) as total_bytes,
    SUM(ref_count) as total_refs,
    AVG(ref_count) as avg_refs_per_blob
FROM blob_registry
GROUP BY storage_tier;
```

### Find heavily-referenced blobs

```sql
SELECT 
    content_hash,
    ref_count,
    byte_length,
    storage_path,
    first_seen,
    last_accessed
FROM blob_registry
WHERE ref_count > 10
ORDER BY ref_count DESC;
```

## Migration SQL

```sql
-- Step 1: Add new columns to outputs
ALTER TABLE outputs ADD COLUMN content_hash TEXT;
ALTER TABLE outputs ADD COLUMN storage_type TEXT DEFAULT 'inline';
ALTER TABLE outputs ADD COLUMN storage_ref TEXT;

-- Step 2: Create blob_registry
CREATE TABLE blob_registry (
    content_hash      TEXT PRIMARY KEY,
    byte_length       BIGINT NOT NULL,
    compression       TEXT DEFAULT 'gzip',
    ref_count         INT DEFAULT 0,
    first_seen        TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    last_accessed     TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    storage_tier      TEXT,
    storage_path      TEXT,
    verified_at       TIMESTAMP,
    corrupt           BOOLEAN DEFAULT FALSE
);

-- Step 3: Create indexes
CREATE INDEX idx_outputs_hash ON outputs(content_hash);
CREATE INDEX idx_outputs_storage ON outputs(storage_type);
CREATE INDEX idx_blob_registry_refs ON blob_registry(ref_count);
CREATE INDEX idx_blob_registry_tier ON blob_registry(storage_tier);
CREATE INDEX idx_blob_registry_accessed ON blob_registry(last_accessed);

-- Step 4: Migrate existing data (done by application code)
-- For each old file_ref:
--   1. Read content
--   2. Compute BLAKE3 hash
--   3. Write to content-addressed path
--   4. Update outputs row
--   5. Insert/update blob_registry

-- Step 5: After migration, make columns NOT NULL
ALTER TABLE outputs ALTER COLUMN content_hash SET NOT NULL;
ALTER TABLE outputs ALTER COLUMN storage_type SET NOT NULL;
ALTER TABLE outputs ALTER COLUMN storage_ref SET NOT NULL;

-- Step 6: Drop old columns (optional, after verification)
-- ALTER TABLE outputs DROP COLUMN content;
-- ALTER TABLE outputs DROP COLUMN file_ref;
```

## Summary

**Key Tables:**
1. `commands` - Command metadata (unchanged)
2. `outputs` - Output records with content_hash and storage_ref (NEW)
3. `blob_registry` - Tracks blobs and reference counts (NEW)

**Key Concepts:**
- Content hash (BLAKE3) identifies unique content
- Multiple outputs can reference same blob (deduplication)
- Reference counting tracks usage
- Polymorphic storage (inline, blob, archive)

**Storage Savings:**
- 70-90% for typical CI/CD workloads
- Automatic, transparent to queries
- Minimal performance overhead (<3ms per command)
