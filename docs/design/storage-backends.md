# BIRD Storage Backends Design

## Overview

BIRD supports two storage backends for different concurrency requirements:

| Mode | Write Pattern | Compaction | Best For |
|------|---------------|------------|----------|
| **Parquet** | Multi-writer safe (atomic files) | Required | Concurrent shells (shq) |
| **DuckDB** | Single-writer (connect/disconnect) | Not needed | Sequential CLIs (blq) |

**Key insight**: Reading always goes through DuckDB, regardless of storage mode. Only writing differs.

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                         Queries                              │
│              (invocations, outputs, events)                  │
└─────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                    DuckDB Read Layer                         │
│  ┌─────────────────────────────────────────────────────────┐│
│  │  Views: invocations, outputs, events, sessions          ││
│  │  Extensions: scalarfs (read_blob), duck_hunt            ││
│  └─────────────────────────────────────────────────────────┘│
└─────────────────────────────────────────────────────────────┘
          │                                    │
          ▼                                    ▼
┌─────────────────────┐            ┌─────────────────────┐
│   Parquet Files     │            │   DuckDB Tables     │
│  (read_parquet)     │            │   (direct SQL)      │
└─────────────────────┘            └─────────────────────┘
          │                                    │
          └─────────────┬──────────────────────┘
                        ▼
              ┌─────────────────────┐
              │  Content Blobs      │
              │ (file://, data:)    │
              │  via read_blob()    │
              └─────────────────────┘
```

## Unified Read Path

All reads go through DuckDB, which handles both storage modes transparently:

### Parquet Mode Views
```sql
CREATE VIEW invocations AS
SELECT * EXCLUDE (filename)
FROM read_parquet('recent/invocations/**/*.parquet',
    union_by_name = true,
    hive_partitioning = true,
    filename = true
);
```

### DuckDB Mode Views
```sql
CREATE VIEW invocations AS
SELECT * FROM invocations_table;
```

### Content Access (Both Modes)
```sql
-- scalarfs handles both data: URLs (inline) and file:// URLs (blobs)
SELECT content FROM read_blob(storage_ref)
```

## Configuration

Storage mode is specified in `config.toml`:

```toml
[storage]
write_mode = "parquet"  # or "duckdb"
# read always uses DuckDB
```

Only affects:
- How new data is written
- Whether compaction is needed
- View definitions at init time

## Mode-Specific Behavior

### Parquet Mode (Default)

**Writes:**
```rust
// Atomic file creation - safe for concurrent writers
let temp = atomic::temp_path(&final_path);
conn.execute(&format!("COPY data TO '{}'", temp))?;
atomic::rename_into_place(&temp, &final_path)?;
```

**Pros:**
- Multiple shells can write simultaneously
- No lock contention
- Natural date partitioning

**Cons:**
- Requires periodic compaction
- Many small files until compacted

### DuckDB Mode

**Writes:**
```rust
// Direct insert - simple but single-writer
conn.execute("INSERT INTO invocations_table VALUES (...)")?;
```

**Pros:**
- Simpler implementation
- No compaction needed
- Single file

**Cons:**
- Must serialize writes (lock or connect/disconnect)
- Not suitable for concurrent shell hooks

## Interoperability

### Cross-Client Queries

Any BIRD client can query any store regardless of mode:

```sql
-- Attach another BIRD store
ATTACH '/path/to/other/bird.duckdb' AS other;

-- Query across stores (works for both modes)
SELECT * FROM other.invocations
UNION ALL
SELECT * FROM invocations;
```

### Export for Sharing

DuckDB mode stores can export to parquet:
```sql
EXPORT DATABASE '/path/to/export' (FORMAT PARQUET);
```

### Blob Storage

Both modes share the same blob storage:
- Content-addressed files in `blobs/content/{hash[0:2]}/{hash}.bin`
- Deduplication via `blob_registry` table
- Access via `read_blob()` with `file://` or `data:` URLs

## Implementation Notes

### Store Trait

```rust
trait StorageBackend {
    fn write_invocation(&self, record: &InvocationRecord) -> Result<()>;
    fn write_output(&self, record: &OutputRecord) -> Result<()>;
    // ... other write methods
}

// Read methods stay on Store - always use DuckDB
impl Store {
    pub fn query_invocations(&self, ...) -> Result<Vec<...>> {
        let conn = self.connection()?;
        // Same SQL works for both modes via views
    }
}
```

### Initialization

```rust
fn init_views(conn: &Connection, mode: StorageMode) -> Result<()> {
    match mode {
        StorageMode::Parquet => {
            conn.execute("CREATE VIEW invocations AS SELECT * FROM read_parquet(...)")?;
        }
        StorageMode::DuckDB => {
            conn.execute("CREATE TABLE invocations_table (...)")?;
            conn.execute("CREATE VIEW invocations AS SELECT * FROM invocations_table")?;
        }
    }
}
```

## Migration Path

1. **New installs**: Choose mode at `init` time (default: parquet)
2. **Existing installs**: Remain parquet (backwards compatible)
3. **Future**: Add `migrate --to-duckdb` if needed

## Tracking Control Integration

Both modes respect tracking control signals:
- Space/backslash prefix (caller opt-out)
- OSC escape `\e]shq;nosave\a` (command opt-out)
- `SHQ_DISABLED` / `SHQ_EXCLUDE` (environment opt-out)
- Query command auto-exclusion

See GitHub issue #3 for full tracking control protocol.
