# BIRD Storage Backends Design

## Overview

BIRD supports two storage backends for different concurrency requirements:

| Mode | CLI Flag | Write Pattern | Compaction | Best For |
|------|----------|---------------|------------|----------|
| **Parquet** | `shq init` (default) | Multi-writer safe (atomic files) | Required | Concurrent shells |
| **DuckDB** | `shq init --mode duckdb` | Single-writer (table inserts) | Not needed | Single-shell usage |

**Key insight**: Reading always goes through DuckDB views, regardless of storage mode. Only writing differs.

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                         Queries                              │
│              (invocations, outputs, events, sessions)        │
└─────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                    DuckDB Read Layer                         │
│  ┌─────────────────────────────────────────────────────────┐│
│  │  Views: invocations, outputs, events, sessions          ││
│  │  Extensions: scalarfs (read_blob), duck_hunt            ││
│  │  Attached: remote_team, remote_backup (via ATTACH)      ││
│  └─────────────────────────────────────────────────────────┘│
└─────────────────────────────────────────────────────────────┘
          │                    │                    │
          ▼                    ▼                    ▼
┌─────────────────┐  ┌─────────────────┐  ┌─────────────────┐
│  Parquet Files  │  │  DuckDB Tables  │  │  Remote DBs     │
│ (read_parquet)  │  │  (direct SQL)   │  │  (ATTACH)       │
└─────────────────┘  └─────────────────┘  └─────────────────┘
          │                    │                    │
          └────────────────────┴────────────────────┘
                              │
                              ▼
              ┌─────────────────────────────────────┐
              │          Content Blobs              │
              │  Local: file:path → blob_roots      │
              │  Remote: s3://bucket/blobs/...      │
              │  Inline: data:base64,...            │
              └─────────────────────────────────────┘
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

## Remote Storage

BIRD supports remote databases for cross-machine sync and backup.

### Remote Types

| Type | URI Format | Use Case |
|------|------------|----------|
| `file` | `/path/to/bird.duckdb` | Network share, local backup |
| `s3` | `s3://bucket/path/bird.duckdb` | Team sharing, cloud backup |
| `motherduck` | `md:database_name` | MotherDuck cloud |
| `postgres` | `postgres:dbname=...` | PostgreSQL integration |

### ATTACH-Based Querying

Remotes are attached as DuckDB schemas at connection time:

```sql
-- Auto-attached remotes become available as schemas
ATTACH 's3://team-bucket/bird/bird.duckdb' AS "remote_team";

-- Query remote data
SELECT * FROM "remote_team".invocations WHERE client_id = 'alice@laptop';

-- Cross-database queries
SELECT * FROM invocations
UNION ALL
SELECT * FROM "remote_team".invocations;
```

### S3 Credentials

S3 remotes use DuckDB's credential chain:

```toml
[[remotes]]
name = "team"
type = "s3"
uri = "s3://bucket/bird.duckdb"
credential_provider = "credential_chain"  # Uses AWS SDK credential chain
```

Credentials are set up before blob resolution to enable S3 glob patterns.

### Push/Pull Sync

Data synchronization uses SQL-based sync with anti-join deduplication:

```sql
-- Push: Insert local records not on remote
INSERT INTO "remote_team".invocations_table
SELECT * FROM main.invocations l
WHERE NOT EXISTS (
    SELECT 1 FROM "remote_team".invocations r WHERE r.id = l.id
);
```

**Sync order** (dependency order):
1. Sessions (referenced by invocations)
2. Invocations (referenced by outputs/events)
3. Outputs
4. Events

### CLI Commands

```bash
# Configure remotes
shq remote add team --type s3 --uri s3://bucket/bird.duckdb
shq remote list
shq remote test team

# Sync data
shq push --remote team              # Push all new data
shq push --remote team --since 7d   # Push last 7 days
shq pull --remote team              # Pull all new data
shq pull --remote team --client bob@work
```

### Blob Storage with Remotes

When remotes are configured, blob resolution checks multiple locations:

```sql
SET VARIABLE blob_roots = [
    '/home/user/.local/share/bird/db/data/recent/blobs/content',
    's3://team-bucket/bird/blobs'
];
```

The `resolve_storage_ref()` macro expands `file:` references to glob patterns across all roots.

## Project-Level Databases

BIRD supports project-level databases in `.bird/` directories, separate from user-level storage.

### Use Cases

- **CI Integration**: blq captures build/test output in `.bird/`, syncs to remote
- **Team Sharing**: Project database contains CI runs from all team members
- **Local + Project**: shq attaches project DB for unified queries

### Automatic Attachment

When shq opens a connection from within a project directory:

```sql
-- Attached automatically as read-only
ATTACH '/path/to/project/.bird/bird.duckdb' AS project (READ_ONLY);
```

### Querying Both

```sql
-- Local commands only
SELECT * FROM invocations WHERE client_id LIKE 'shq:%';

-- Project CI builds only
SELECT * FROM project.invocations WHERE client_id LIKE 'blq:%';

-- Combined view
SELECT * FROM invocations
UNION ALL
SELECT * FROM project.invocations
ORDER BY timestamp DESC;
```

### Multi-Client Coordination

When running nested BIRD clients (e.g., `shq run blq run ...`), they share an invocation UUID via environment variables:

| Variable | Purpose |
|----------|---------|
| `BIRD_INVOCATION_UUID` | Shared UUID for deduplication |
| `BIRD_PARENT_CLIENT` | Name of parent client (e.g., "shq") |

This allows the same invocation to be recorded in both user and project databases with the same ID.
