# Content-Addressed Blob Storage

## The Optimization

**Replace UUID-based blob naming with content-addressed (hash-based) naming to enable automatic deduplication.**

### Current Problem
```bash
# CI runs same tests 100 times
make test  # Creates uuid-001.bin.gz (5MB)
make test  # Creates uuid-002.bin.gz (5MB, identical!)
make test  # Creates uuid-003.bin.gz (5MB, identical!)
# Result: 500MB stored, but only 5MB unique content
```

### Solution
```bash
make test  # Hash: abc123... → abc123.bin.gz (5MB written)
make test  # Hash: abc123... → reuses existing file (0 bytes!)
make test  # Hash: abc123... → reuses existing file (0 bytes!)
# Result: 5MB stored, 100 commands reference same blob
# Savings: 99% storage reduction!
```

## Design Decisions

### 1. Hash Algorithm: BLAKE3

**Why BLAKE3?**
- ✅ Very fast (~3 GB/s vs SHA256's ~200 MB/s)
- ✅ Cryptographically secure (no intentional collisions)
- ✅ Modern, gaining adoption
- ✅ Collision probability: 2^-256 (impossible)

**Cost**: ~1.7ms to hash 5MB output (negligible compared to 50ms compression)

### 2. Filename Scheme: Pure Content Addressing

```
Filename: {hash}.bin.gz

Example: abc123def456789...xyz.bin.gz
```

**No command info in filename because:**
- Different commands can produce identical output (that's the point!)
- Database tracks which commands reference which hashes
- Simpler, more correct implementation

### 3. Storage Layout: Subdirectory Sharding

```
recent/blobs/content/
├── ab/
│   ├── abc123def456.bin.gz
│   └── abf999888777.bin.gz
├── cd/
│   └── cde234567890.bin.gz
└── ...

Pattern: content/{hash[0:2]}/{hash}.bin.gz
```

**Why subdirectories?**
- Many filesystems slow down with >10k files/directory
- 256 subdirs (00-ff) = ~390 files/dir for 100k blobs
- Standard pattern (git, npm cache, etc.)

### 4. Inline Storage: Still Use data: URIs

```
Small outputs (<1MB): Store in parquet with data: URI
Large outputs (≥1MB): Store as content-addressed blob

Both cases: Store content_hash for integrity
```

**Example inline:**
```sql
content_hash: 'abc123def...'
storage_type: 'inline'
storage_ref:  'data:application/octet-stream;base64,SGVsbG8...'
```

**Example blob:**
```sql
content_hash: 'abc123def...'
storage_type: 'blob'  
storage_ref:  'file://recent/blobs/content/ab/abc123def.bin.gz'
```

## Database Schema

### Updated Outputs Table

```sql
CREATE TABLE outputs (
    id              BIGINT PRIMARY KEY,
    command_id      BIGINT REFERENCES commands(id),
    stream          TEXT CHECK (stream IN ('stdout', 'stderr')),
    
    -- Content identification
    content_hash    TEXT NOT NULL,      -- BLAKE3 hash (hex)
    byte_length     BIGINT NOT NULL,
    content_type    TEXT,
    
    -- Storage location (polymorphic)
    storage_type    TEXT NOT NULL,      -- 'inline', 'blob', 'tarfs', 'archive'
    storage_ref     TEXT NOT NULL,      -- URI to content
    
    truncated       BOOLEAN DEFAULT FALSE,
    created_at      TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX idx_outputs_hash ON outputs(content_hash);
CREATE INDEX idx_outputs_storage ON outputs(storage_type);
```

### New Blob Registry Table

```sql
-- Central registry of all blobs
CREATE TABLE blob_registry (
    content_hash    TEXT PRIMARY KEY,
    byte_length     BIGINT NOT NULL,
    compression     TEXT DEFAULT 'gzip',
    
    -- Reference tracking
    ref_count       INT DEFAULT 0,
    first_seen      TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    last_accessed   TIMESTAMP DEFAULT CURRENT_TIMESTAMP,
    
    -- Storage location
    storage_tier    TEXT,               -- 'recent', 'archive'
    storage_path    TEXT,
    
    -- Integrity checking
    verified_at     TIMESTAMP,
    corrupt         BOOLEAN DEFAULT FALSE
);

CREATE INDEX idx_blob_registry_refs ON blob_registry(ref_count);
CREATE INDEX idx_blob_registry_tier ON blob_registry(storage_tier);
```

## Capture Flow

```rust
pub fn capture_output(&mut self, data: &[u8], stream: &str) -> Result<OutputId> {
    // 1. Compute content hash
    let hash = blake3::hash(data);
    let hash_hex = hash.to_hex();
    
    // 2. Size-based routing
    let (storage_type, storage_ref) = if data.len() < SIZE_THRESHOLD {
        // Small: inline with data: URI
        self.store_inline(data)?
    } else {
        // Large: content-addressed blob
        self.store_blob(&hash_hex, data)?
    };
    
    // 3. Insert output record
    let output_id = self.db.execute(
        "INSERT INTO outputs 
            (command_id, stream, content_hash, byte_length, storage_type, storage_ref)
         VALUES (?, ?, ?, ?, ?, ?)
         RETURNING id",
        params![self.command_id, stream, &hash_hex, data.len(), 
               &storage_type, &storage_ref]
    )?;
    
    Ok(output_id)
}

fn store_blob(&mut self, hash: &str, data: &[u8]) -> Result<(String, String)> {
    // Check if blob already exists
    let existing = self.db.query_row(
        "SELECT storage_path FROM blob_registry WHERE content_hash = ?",
        params![hash]
    );
    
    if let Ok(path) = existing {
        // DEDUP HIT! Just increment ref count
        self.db.execute(
            "UPDATE blob_registry 
             SET ref_count = ref_count + 1, 
                 last_accessed = CURRENT_TIMESTAMP
             WHERE content_hash = ?",
            params![hash]
        )?;
        
        return Ok(("blob".into(), format!("file://{}", path)));
    }
    
    // DEDUP MISS: Write new blob
    let blob_path = self.write_blob(hash, data)?;
    
    // Register in blob_registry
    self.db.execute(
        "INSERT INTO blob_registry 
            (content_hash, byte_length, ref_count, storage_tier, storage_path)
         VALUES (?, ?, 1, 'recent', ?)",
        params![hash, data.len(), blob_path.to_str().unwrap()]
    )?;
    
    Ok(("blob".into(), format!("file://{}", blob_path.display())))
}

fn write_blob(&self, hash: &str, data: &[u8]) -> Result<PathBuf> {
    // Path: content/{first_2_chars}/{full_hash}.bin.gz
    let subdir = &hash[0..2];
    let dir = self.blob_pool.join("content").join(subdir);
    fs::create_dir_all(&dir)?;
    
    let path = dir.join(format!("{}.bin.gz", hash));
    let temp = dir.join(format!(".tmp.{}.bin.gz", hash));
    
    // Write to temp file
    {
        let file = fs::File::create(&temp)?;
        let mut encoder = GzEncoder::new(file, Compression::new(6));
        encoder.write_all(data)?;
        encoder.finish()?;
    }
    
    // Atomic rename (handles race conditions)
    match fs::rename(&temp, &path) {
        Ok(_) => Ok(path),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            // Another process wrote same hash concurrently
            fs::remove_file(&temp)?;
            Ok(path)  // Safe to use existing file
        },
        Err(e) => Err(e.into()),
    }
}
```

## Deduplication Examples

### Example 1: CI Test Suite (90% savings)

```bash
# Day 1: First run
make test  # Output: 5MB
# → Hash: abc123...
# → Writes: recent/blobs/content/ab/abc123.bin.gz (5MB)
# → ref_count: 1

# Day 2-365: Same tests (no code changes)
make test  # Output: 5MB (identical)
# → Hash: abc123... (same!)
# → Reuses existing file (0 bytes written)
# → ref_count: 2, 3, 4, ... 365

Storage: 5MB (not 1.8GB!)
Savings: 1.795GB (99.7%)
```

### Example 2: Build Logs (Partial Dedup)

```bash
# Build 1: Success with 5 warnings
gcc main.c
# → Hash: def456...
# → Writes: content/de/def456.bin.gz (10KB)

# Build 2: Success with same 5 warnings
gcc main.c  
# → Hash: def456... (same!)
# → Reuses existing file

# Build 3: Success with 3 warnings (different)
gcc main.c
# → Hash: 789abc... (different)
# → Writes: content/78/789abc.bin.gz (10KB)

# Build 4: Same as build 1
gcc main.c
# → Hash: def456... (same as build 1!)
# → Reuses existing file

Storage: 20KB (not 40KB)
Savings: 50%
```

### Example 3: Different Commands, Same Output

```bash
# Completely different commands that happen to produce identical output
./test.sh     # Hash: fff000...
./verify.sh   # Hash: fff000... (identical output!)

# Both reference same blob
Storage: 5MB (not 10MB)
Savings: 5MB
```

## Compaction Strategy (Simplified)

### Key Insight: Content Pool Doesn't Need Compaction

**Compaction applied to:** Parquet files (commands/outputs tables)
**Not applied to:** Blob content pool

**Why?**
- Content pool is already optimized (deduplicated)
- No date partitioning (blobs are timeless)
- Compaction benefit comes from merging small parquets, not blob files

### Storage Layout

```
recent/
├── commands/date=YYYY-MM-DD/
│   ├── laptop--make--*.parquet        # Individual
│   └── __compacted-0__.parquet        # Merged (compaction)
│
├── outputs/date=YYYY-MM-DD/
│   ├── laptop--make--*.parquet        # Individual
│   └── __compacted-0__.parquet        # Merged (compaction)
│
└── blobs/content/                     # No compaction needed!
    ├── ab/abc123.bin.gz               # Referenced by many commands
    └── cd/cde456.bin.gz               # Referenced by many commands
```

**Queries work transparently:**
```sql
-- This works whether blobs are deduplicated or not
SELECT * FROM read_duck_hunt_log(
    (SELECT file_ref FROM outputs WHERE id = ?),
    'gcc'
);
```

## Garbage Collection

### When to Delete Blobs?

**Can't use date alone** - blob might be referenced by commands across multiple dates

**Need reference tracking:**

```sql
-- Find orphaned blobs (zero references)
SELECT b.content_hash, b.storage_path, b.ref_count, b.last_accessed
FROM blob_registry b
WHERE b.ref_count = 0
  AND b.last_accessed < NOW() - INTERVAL '30 days';
```

### GC Strategies

#### Strategy 1: Never Delete (MVP Recommendation)

```
✅ Simplest implementation
✅ Storage is cheap
✅ Content addressing gives us dedup anyway
✅ No risk of deleting referenced blobs
```

**When to use:** MVP, small-scale deployments

#### Strategy 2: Reference Counting (Production)

```sql
-- Increment on capture
UPDATE blob_registry 
SET ref_count = ref_count + 1 
WHERE content_hash = ?;

-- Decrement on command deletion
UPDATE blob_registry 
SET ref_count = ref_count - 1 
WHERE content_hash IN (
    SELECT content_hash FROM outputs 
    WHERE command_id IN (...)
);

-- Delete when zero (after grace period)
DELETE FROM blob_registry 
WHERE ref_count = 0 
  AND last_accessed < NOW() - INTERVAL '30 days';

-- Delete actual files
-- (separate process reads deleted registry entries)
```

**When to use:** Production systems, long-running deployments

#### Strategy 3: Mark-and-Sweep (Batch)

```sql
-- Mark phase: Find all referenced hashes
CREATE TEMP TABLE referenced_hashes AS
SELECT DISTINCT content_hash FROM outputs;

-- Sweep phase: Delete unreferenced
DELETE FROM blob_registry 
WHERE content_hash NOT IN (SELECT * FROM referenced_hashes)
  AND last_accessed < NOW() - INTERVAL '30 days';
```

**When to use:** Periodic cleanup jobs, migration scenarios

## Archival Strategy (Modified)

### Challenge: Content-Addressed Blobs Span Multiple Dates

A single blob might be referenced by commands from many different weeks:

```
Blob abc123.bin.gz referenced by:
- Week 50: 20 commands
- Week 51: 35 commands  
- Week 52: 15 commands
```

Which week's archive does it belong in?

### Solution: Global Blob Pool

```
archive/
├── by-week/                          # Date-partitioned structured data
│   ├── commands/year=2024/week=52.parquet
│   └── outputs/year=2024/week=52.parquet
│
└── blobs/content/                    # Global content pool
    ├── ab/abc123.bin.gz
    └── cd/cde456.bin.gz
```

**Archival Process:**
1. Archive parquet files by week (commands, outputs)
2. Keep blob pool separate (no duplication)
3. Blob moves to archive pool when ALL references are archived

**Benefits:**
- No blob duplication across weeks
- Clean separation: structured (parquet) vs binary (blobs)
- Simple GC: Delete when ref_count = 0 (all refs archived/deleted)

### Archival Code

```rust
pub fn archive_week(&self, week: Week) -> Result<()> {
    // 1. Archive parquet files (commands, outputs)
    self.archive_parquets(week)?;
    
    // 2. Find blobs that are now fully archived
    let fully_archived = self.db.query(
        "SELECT DISTINCT o.content_hash
         FROM outputs o
         JOIN commands c ON c.id = o.command_id
         WHERE o.storage_type = 'blob'
           AND date_trunc('week', c.timestamp) = ?
           AND NOT EXISTS (
               SELECT 1 FROM commands c2
               JOIN outputs o2 ON o2.command_id = c2.id
               WHERE o2.content_hash = o.content_hash
                 AND c2.timestamp >= ?
           )",
        params![week.start(), week.end()]
    )?;
    
    // 3. Move blobs to archive pool
    for hash in fully_archived {
        let src = format!("recent/blobs/content/{}/{}.bin.gz", 
                         &hash[0..2], hash);
        let dst = format!("archive/blobs/content/{}/{}.bin.gz",
                         &hash[0..2], hash);
        
        fs::create_dir_all(Path::new(&dst).parent().unwrap())?;
        fs::rename(&src, &dst)?;
        
        // Update blob_registry
        self.db.execute(
            "UPDATE blob_registry 
             SET storage_tier = 'archive', storage_path = ?
             WHERE content_hash = ?",
            params![dst, hash]
        )?;
    }
    
    Ok(())
}
```

## Storage Calculations

### Scenario: Daily CI (100 builds/day)

**Without Content-Addressing:**
```
Daily: 100 builds × 5MB output = 500MB
Weekly: 7 × 500MB = 3.5GB
Annual: 52 × 3.5GB = 182GB
```

**With Content-Addressing (90% identical):**
```
Unique outputs per day: 10 (10% variation)
Shared outputs: 90 builds reuse same 10 blobs

Daily storage: 10 × 5MB = 50MB
Weekly: 7 × 50MB = 350MB
Annual: 52 × 350MB = 18.2GB

Savings: 164GB (90% reduction!)
```

### Dedup Ratio by Workload Type

| Workload | Typical Dedup | Notes |
|----------|---------------|-------|
| **CI tests (no changes)** | 95-99% | Same tests, same output |
| **CI tests (active dev)** | 70-90% | Some outputs change |
| **Build logs** | 60-80% | Warnings/errors repeat |
| **Lint output** | 80-95% | Same style issues |
| **Test coverage** | 50-70% | Coverage changes |
| **Compilation errors** | 90-95% | Same errors until fixed |
| **Ad-hoc commands** | 20-40% | High variance |

**Expected average: 70-80% dedup ratio** for typical CI/CD workloads

## Performance Impact

### Hashing Cost

```
BLAKE3 throughput: ~3 GB/s
5MB output: 5MB / 3GB/s = 1.7ms
Compared to gzip: ~50ms

Overhead: 1.7ms / 50ms = 3.4% (negligible)
```

### Database Lookups

```sql
-- Dedup check (indexed)
SELECT storage_path FROM blob_registry WHERE content_hash = ?;
-- Time: <1ms

-- Ref count update (indexed)
UPDATE blob_registry SET ref_count = ref_count + 1 WHERE content_hash = ?;
-- Time: <1ms
```

### Total Overhead Per Capture

```
Compute hash:    1.7ms
Dedup check:     0.5ms  
Ref count upd:   0.5ms
─────────────────────
Total:           2.7ms (negligible!)
```

## Monitoring & Metrics

### Deduplication Dashboard

```sql
-- Overall stats
SELECT 
    COUNT(*) as total_outputs,
    COUNT(DISTINCT content_hash) as unique_blobs,
    COUNT(*) - COUNT(DISTINCT content_hash) as duplicates,
    ROUND(100.0 * (1 - COUNT(DISTINCT content_hash)::float / COUNT(*)), 2) as dedup_pct
FROM outputs
WHERE storage_type = 'blob';

-- Storage savings
SELECT 
    COUNT(*) * AVG(byte_length) / 1024.0 / 1024.0 / 1024.0 as would_use_gb,
    SUM(DISTINCT byte_length) / 1024.0 / 1024.0 / 1024.0 as actual_use_gb,
    ROUND(100.0 * (1 - SUM(DISTINCT byte_length)::float / (COUNT(*) * AVG(byte_length))), 2) as saved_pct
FROM outputs
WHERE storage_type = 'blob';
```

### Top Deduplicated Outputs

```sql
-- Find most frequently duplicated blobs
SELECT 
    b.content_hash,
    b.ref_count,
    b.byte_length / 1024.0 / 1024.0 as size_mb,
    b.ref_count * b.byte_length / 1024.0 / 1024.0 as saved_mb,
    COUNT(DISTINCT c.program) as unique_programs
FROM blob_registry b
JOIN outputs o ON o.content_hash = b.content_hash
JOIN commands c ON c.id = o.command_id
WHERE b.ref_count > 5
GROUP BY b.content_hash, b.ref_count, b.byte_length
ORDER BY saved_mb DESC
LIMIT 20;
```

## Implementation Checklist

### Phase 1: Core (Week 1-2)
- [ ] Add BLAKE3 dependency
- [ ] Add content_hash column to outputs table
- [ ] Create blob_registry table
- [ ] Update capture flow to compute hash
- [ ] Implement dedup check (if exists → increment ref_count)
- [ ] Implement content-addressed write
- [ ] Add collision detection (verify on hash match)

### Phase 2: Read Path (Week 3)
- [ ] Update blob reader to check blob_registry
- [ ] Support all storage types: inline, blob, tarfs, archive
- [ ] Add integrity verification (rehash on read, optional)

### Phase 3: Reference Tracking (Week 4)
- [ ] Implement ref_count increment on capture
- [ ] Implement ref_count decrement on deletion
- [ ] Add database triggers for automatic ref counting
- [ ] Add `bird gc` command (mark orphaned blobs)

### Phase 4: Archival (Week 5)
- [ ] Separate blob pool from date-based archives
- [ ] Update archival to handle content-addressed blobs
- [ ] Implement blob migration to archive pool
- [ ] Add blob GC to archival process

### Phase 5: Monitoring (Week 6)
- [ ] Add dedup stats to `bird status`
- [ ] Create dedup dashboard (Grafana/custom)
- [ ] Add alerts for low dedup ratios
- [ ] Add collision detection alerts (should never fire!)

## Migration from UUID Scheme

### Backward Compatibility

Old schema:
```sql
storage_ref: 'file://recent/blobs/date=2024-12-30/uuid.bin.gz'
```

New schema:
```sql
content_hash: 'abc123def456...'
storage_type: 'blob'
storage_ref:  'file://recent/blobs/content/ab/abc123def456.bin.gz'
```

### Migration Script

```rust
pub fn migrate_to_content_addressed() -> Result<()> {
    // 1. Add new columns with defaults
    db.execute("
        ALTER TABLE outputs ADD COLUMN content_hash TEXT;
        ALTER TABLE outputs ADD COLUMN storage_type TEXT DEFAULT 'blob';
    ")?;
    
    // 2. Create blob_registry table
    db.execute("
        CREATE TABLE blob_registry (...);
    ")?;
    
    // 3. Backfill hashes for existing blobs
    let old_blobs = db.query(
        "SELECT id, storage_ref FROM outputs 
         WHERE storage_ref LIKE 'file://%/date=%'"
    )?;
    
    for (id, old_path) in old_blobs {
        // Read and hash
        let data = decompress_file(&old_path)?;
        let hash = blake3::hash(&data).to_hex();
        
        // Move to content-addressed location
        let new_path = content_path(&hash);
        fs::create_dir_all(new_path.parent().unwrap())?;
        fs::rename(&old_path, &new_path)?;
        
        // Update outputs table
        db.execute(
            "UPDATE outputs 
             SET content_hash = ?, storage_ref = ?
             WHERE id = ?",
            params![&hash, format!("file://{}", new_path), id]
        )?;
        
        // Register blob
        db.execute(
            "INSERT INTO blob_registry (content_hash, byte_length, ref_count, storage_path)
             VALUES (?, ?, 1, ?)
             ON CONFLICT (content_hash) 
             DO UPDATE SET ref_count = ref_count + 1",
            params![&hash, data.len(), new_path]
        )?;
    }
    
    // 4. Make content_hash NOT NULL (after backfill)
    db.execute("ALTER TABLE outputs ALTER COLUMN content_hash SET NOT NULL")?;
    
    Ok(())
}
```

## Summary

✅ **Content-addressed storage** using BLAKE3 hash
✅ **Automatic deduplication** (70-90% savings for CI workloads)
✅ **Minimal overhead** (~3ms per capture)
✅ **Reference tracking** via blob_registry
✅ **Atomic writes** handle race conditions
✅ **Global blob pool** simplifies archival
✅ **Backward compatible** with migration path

### Key Benefits

| Metric | Before | After | Improvement |
|--------|--------|-------|-------------|
| **CI storage** | 182GB/year | 18GB/year | 90% reduction |
| **Capture overhead** | 50ms | 53ms | 6% overhead |
| **Dedup ratio** | 0% | 70-90% | Automatic |
| **GC complexity** | Date-based | Ref-counting | More precise |

### Next Steps

1. **Read**: This document (you're here!)
2. **Implement**: Phase 1 (core content-addressing)
3. **Test**: Verify dedup with CI workload
4. **Monitor**: Track dedup ratios
5. **Optimize**: Tune hash algorithm, ref counting

---

*Excellent optimization! This transforms storage from O(n) to O(unique content).* 🎯
