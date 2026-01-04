# Content-Addressed Storage Implementation Guide

## TL;DR: Files You Need

For implementing content-addressed storage in BIRD/shq, use these files **in this order**:

### 1. Design & Architecture (Read First)
```
1. CONTENT_ADDRESSED_BLOBS.md    - Complete design document
2. bird_spec.md                  - Updated BIRD specification
3. SPEC_CHANGELOG.md             - What changed from UUID-based
```

### 2. Implementation Reference (Code From)
```
4. shq_implementation.md         - Updated Rust code examples
5. MIGRATION_SUMMARY.md          - Phase-by-phase checklist
```

### 3. Supporting Docs (Reference As Needed)
```
6. COMPRESSION_DECISION.md       - Why gzip not zstd
7. STORAGE_LIFECYCLE.md          - Blob lifecycle details
```

---

## Quick Verification: Is bird_spec.md Updated?

✅ **YES!** Here's proof:

### Directory Structure (lines 30-36)
```
└── blobs/
    └── content/         # Content-addressed pool ✅
        ├── ab/
        │   └── abc123def456...789.bin.gz  ✅ Hash-based!
        ├── cd/
        └── ...          # 256 subdirs (00-ff)
```

### Outputs Table (lines 115-143)
```sql
CREATE TABLE outputs (
    id                UUID PRIMARY KEY,
    command_id        UUID NOT NULL,
    
    -- NEW: Content identification
    content_hash      TEXT NOT NULL,     -- BLAKE3 hash ✅
    byte_length       BIGINT NOT NULL,
    
    -- NEW: Polymorphic storage
    storage_type      TEXT NOT NULL,     -- 'inline', 'blob', 'archive' ✅
    storage_ref       TEXT NOT NULL,     -- URI to content ✅
    
    stream            TEXT NOT NULL,
    content_type      TEXT,
    ...
);
```

### Blob Registry Table (lines 155-178)
```sql
CREATE TABLE blob_registry (
    content_hash      TEXT PRIMARY KEY,  -- BLAKE3 hash ✅
    byte_length       BIGINT NOT NULL,
    ref_count         INT DEFAULT 0,     -- Reference tracking ✅
    first_seen        TIMESTAMP,
    last_accessed     TIMESTAMP,
    storage_tier      TEXT,              -- 'recent', 'archive' ✅
    storage_path      TEXT,
    ...
);
```

**All updated! ✅**

---

## Implementation Phases

### Phase 1: Schema (Week 1)

**File:** `bird_spec.md` lines 115-178

**Tasks:**
1. Add columns to outputs table:
   ```sql
   ALTER TABLE outputs ADD COLUMN content_hash TEXT;
   ALTER TABLE outputs ADD COLUMN storage_type TEXT DEFAULT 'inline';
   ALTER TABLE outputs ADD COLUMN storage_ref TEXT;
   ```

2. Create blob_registry table:
   ```sql
   CREATE TABLE blob_registry (...);  -- Full schema in bird_spec.md
   ```

3. Update OutputRecord struct (Rust):
   ```rust
   struct OutputRecord {
       id: UUID,
       command_id: UUID,
       stream: String,
       content_hash: String,        // NEW
       byte_length: i64,
       storage_type: String,        // NEW
       storage_ref: String,         // NEW
       content_type: Option<String>,
       date: Date,
   }
   ```

**Reference:** shq_implementation.md lines 163-172 (updated struct)

---

### Phase 2: Capture Flow (Week 2)

**File:** `shq_implementation.md` lines 143-265

**Key Functions to Implement:**

1. **`write_output()` - Main entry point**
   ```rust
   fn write_output(command_id: &UUID, stream: &str, content: &[u8]) -> Result<()> {
       // 1. Compute BLAKE3 hash
       let hash = blake3::hash(content);
       
       // 2. Size-based routing
       if content.len() < THRESHOLD {
           // Inline: data: URI
       } else {
           // Blob: Check for existing (dedup!)
           if let Some(path) = check_blob_exists(&hash)? {
               increment_ref_count(&hash)?;  // DEDUP HIT
           } else {
               write_content_addressed_blob(&hash, content)?;  // DEDUP MISS
           }
       }
       
       // 3. Insert output record
   }
   ```

2. **`write_content_addressed_blob()` - Lines 196-226**
   ```rust
   fn write_content_addressed_blob(hash: &str, content: &[u8]) -> Result<String> {
       // Subdirectory: first 2 hex chars
       let subdir = &hash[..2];
       let blob_dir = bird_root.join("db/data/recent/blobs/content").join(subdir);
       
       // Filename: {hash}.bin.gz
       let filename = format!("{}.bin.gz", hash);
       
       // Atomic write (handles race conditions)
       // ... see full code in shq_implementation.md
   }
   ```

3. **`check_blob_exists()` - Lines 228-239**
   ```rust
   fn check_blob_exists(hash: &str) -> Result<Option<String>> {
       // Query blob_registry
       // Return storage_path if exists
   }
   ```

4. **`register_blob()` - Lines 241-250**
   ```rust
   fn register_blob(hash: &str, byte_length: usize, path: &str) -> Result<()> {
       // INSERT into blob_registry
       // Set ref_count = 1
   }
   ```

5. **`increment_blob_ref_count()` - Lines 252-260**
   ```rust
   fn increment_blob_ref_count(hash: &str) -> Result<()> {
       // UPDATE blob_registry SET ref_count = ref_count + 1
   }
   ```

**Dependencies:**
```toml
[dependencies]
blake3 = "1.5"           # Fast hashing
flate2 = "1.0"           # Gzip compression
base64 = "0.21"          # For data: URIs
```

---

### Phase 3: Query Flow (Week 3)

**File:** `shq_implementation.md` lines 460-540

**Key Function:**

**`resolve_storage_ref()` - Lines 505-533**
```rust
fn resolve_storage_ref(storage_type: &str, storage_ref: &str, hash: &str) -> Result<String> {
    match storage_type {
        "inline" => {
            // Decode data: URI, write to temp file
            let b64_data = storage_ref.split(',').nth(1)?;
            let decoded = base64::decode(b64_data)?;
            let temp_path = format!("/tmp/shq-output-{}.tmp", hash);
            fs::write(&temp_path, decoded)?;
            Ok(temp_path)
        },
        "blob" | "archive" => {
            // Extract path from file:// URI
            let rel_path = &storage_ref[7..];
            let full_path = bird_root.join("db/data").join(rel_path);
            Ok(full_path.display().to_string())
        },
        _ => Err(anyhow!("Unknown storage type"))
    }
}
```

**Update Queries:**
- Replace `o.file_ref` with `o.storage_type, o.storage_ref, o.content_hash`
- Call `resolve_storage_ref()` before accessing content
- See lines 464-504 for complete example

---

### Phase 4: Testing (Week 4)

**Test Cases:**

1. **Deduplication Test**
   ```rust
   #[test]
   fn test_dedup_same_content() {
       let content = b"identical output";
       
       // First write
       write_output(&cmd1, "stdout", content)?;
       let blob_count_1 = count_blobs()?;
       
       // Second write (same content)
       write_output(&cmd2, "stdout", content)?;
       let blob_count_2 = count_blobs()?;
       
       // Should reuse blob (same count)
       assert_eq!(blob_count_1, blob_count_2);
       
       // But both outputs exist
       assert_eq!(count_outputs()?, 2);
   }
   ```

2. **Reference Counting Test**
   ```rust
   #[test]
   fn test_ref_counting() {
       let hash = write_blob(content)?;
       let refs = get_ref_count(&hash)?;
       assert_eq!(refs, 1);
       
       // Write same content again
       write_output(&cmd2, "stdout", content)?;
       let refs = get_ref_count(&hash)?;
       assert_eq!(refs, 2);  // Incremented!
   }
   ```

3. **Storage URI Resolution Test**
   ```rust
   #[test]
   fn test_resolve_inline() {
       let uri = "data:application/octet-stream;base64,SGVsbG8=";
       let path = resolve_storage_ref("inline", uri, "abc123")?;
       let content = fs::read_to_string(path)?;
       assert_eq!(content, "Hello");
   }
   ```

---

## Storage Savings Calculator

**Your Workload:**
```
Commands/day:     _______
Avg output size:  _______
Dedup ratio:      70% (typical CI)

Before:  commands/day × avg_size × 30 days
After:   commands/day × avg_size × (1 - dedup_ratio) × 30 days
Savings: _______
```

**Example:**
```
10,000 commands/day
200KB avg output
70% dedup

Before:  10k × 200KB × 30 = 60GB/month
After:   10k × 200KB × 0.3 × 30 = 18GB/month
Savings: 42GB (70%)
```

---

## Troubleshooting

### "Hash mismatch on read"
- Corruption in blob file
- Check: `SELECT * FROM blob_registry WHERE corrupt = TRUE`
- Fix: Recompute hash, mark for deletion

### "Dedup not working"
- Check: Are hashes identical? `SELECT content_hash, COUNT(*) FROM outputs GROUP BY content_hash`
- Verify: BLAKE3 version consistent
- Debug: Log dedup hit/miss rates

### "Blob directory growing too large"
- Check: Subdirectory sharding (00-ff)
- Verify: `ls blobs/content/ | wc -l` should show 256 dirs
- Fix: Migrate old blobs to subdirs

---

## Migration from UUID-based

**File:** `MIGRATION_SUMMARY.md`

**Steps:**
```bash
# 1. Backup
cp -r db/data/recent/managed db/data/recent/managed.backup

# 2. Run migration
shq migrate-to-content-addressed

# 3. Verify
shq verify-blobs

# 4. Cleanup (after verification)
rm -rf db/data/recent/managed.backup
```

---

## Performance Targets

✅ **Hash computation:** <2ms per 5MB output  
✅ **Dedup check:** <1ms (indexed query)  
✅ **Total overhead:** <3ms per capture  
✅ **Storage savings:** 70-90% for CI workloads  
✅ **Query speed:** No regression  

---

## Summary Checklist

- [ ] Read CONTENT_ADDRESSED_BLOBS.md (design)
- [ ] Read bird_spec.md (schema)
- [ ] Phase 1: Schema migration
- [ ] Phase 2: Capture flow (write_output, etc.)
- [ ] Phase 3: Query flow (resolve_storage_ref)
- [ ] Phase 4: Testing (dedup, ref counting, URIs)
- [ ] Measure: Storage savings achieved
- [ ] Document: Actual dedup ratios for your workload

**Implementation time:** ~4 weeks  
**Complexity:** Medium (clear interfaces)  
**Risk:** Low (backward compatible)  
**Payoff:** 70-90% storage reduction 🎯

