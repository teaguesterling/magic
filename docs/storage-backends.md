# Storage Backends and Schema Architecture

This document describes BIRD's storage backends and multi-schema architecture for organizing local, cached, and remote data.

## Overview

BIRD uses a layered schema architecture that separates:

- **Local data**: Commands captured on this machine
- **Cached data**: Data pulled from remotes (persisted locally)
- **Remote data**: Live access to attached remote databases

This separation enables:
- Clear data provenance (where did this data come from?)
- Efficient sync (only transfer what's needed)
- Flexible querying (local only, cached, or everything)

## Storage Modes

BIRD supports two storage backends, selected at initialization:

| Mode | CLI Flag | Write Pattern | Best For |
|------|----------|---------------|----------|
| **Parquet** | `--mode parquet` (default) | Atomic files, multi-writer safe | Concurrent shells |
| **DuckDB** | `--mode duckdb` | Table inserts, single-writer | Single-shell usage |

```bash
# Initialize with parquet mode (default)
shq init

# Initialize with duckdb mode
shq init --mode duckdb
```

## Schema Architecture

### Data Schemas (Tables)

These schemas contain actual data tables:

| Schema | Description | Persistence |
|--------|-------------|-------------|
| `local` | Locally generated invocations, sessions, outputs, events | Permanent |
| `cached_<name>` | Data pulled from remote `<name>` | Permanent |
| `cached_placeholder` | Empty tables (ensures unions work) | Permanent |
| `remote_placeholder` | Empty tables (ensures unions work) | Permanent |

### Union Schemas (Views)

These schemas provide unified views across multiple data sources:

| Schema | Contents | Description |
|--------|----------|-------------|
| `caches` | Union of all `cached_*` schemas | All pulled/synced data |
| `main` | `local` + `caches` | All data we own locally |

### Attached Schemas

When remotes are attached, they appear as:

| Schema | Description |
|--------|-------------|
| `remote_<name>` | Attached remote database (read-only) |

### Ephemeral Access (Macros)

Some data access uses TEMPORARY macros created per-connection:

| Macro | Description |
|-------|-------------|
| `remotes_invocations()` | Union of all attached remote invocations |
| `remotes_sessions()` | Union of all attached remote sessions |
| `remotes_outputs()` | Union of all attached remote outputs |
| `remotes_events()` | Union of all attached remote events |
| `cwd_invocations()` | Local data filtered to current directory |

**Why macros?** Persisting views that reference attached databases can corrupt the DuckDB catalog when the attachment isn't present in future sessions. TEMPORARY macros avoid this issue.

## Schema Diagram

```
┌─────────────────────────────────────────────────────────────────┐
│                         BIRD Database                            │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  ┌──────────────┐    ┌───────────────────┐                      │
│  │    local     │    │  cached_<remote>  │  (one per remote)    │
│  │  (tables)    │    │     (tables)      │                      │
│  └──────┬───────┘    └─────────┬─────────┘                      │
│         │                      │                                 │
│         │    ┌─────────────────┴─────────────────┐              │
│         │    │                                   │              │
│         │    ▼                                   │              │
│         │  ┌─────────────┐                       │              │
│         │  │   caches    │ (union of cached_*)   │              │
│         │  │   (views)   │                       │              │
│         │  └──────┬──────┘                       │              │
│         │         │                              │              │
│         ▼         ▼                              │              │
│       ┌─────────────────┐                        │              │
│       │      main       │ (local + caches)       │              │
│       │     (views)     │                        │              │
│       └─────────────────┘                        │              │
│                                                  │              │
├──────────────────────────────────────────────────┴──────────────┤
│                    ATTACHED (per-connection)                     │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  ┌───────────────────┐                                          │
│  │  remote_<name>    │  (attached DuckDB file)                  │
│  │   (read-only)     │                                          │
│  └─────────┬─────────┘                                          │
│            │                                                     │
│            ▼                                                     │
│  ┌───────────────────┐                                          │
│  │ remotes_*() macro │  (TEMPORARY, unions all remotes)         │
│  └───────────────────┘                                          │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

## Data Flow

### Push (Local → Remote)

```
local.invocations  ──push──▶  remote_<name>.invocations
```

Push copies data from `local` schema to the remote database:

```bash
shq push --remote team              # Push all new data
shq push --remote team --since 7d   # Push last 7 days
shq push --remote team --dry-run    # Preview only
```

### Pull (Remote → Local Cache)

```
remote_<name>.invocations  ──pull──▶  cached_<name>.invocations
```

Pull copies data from the remote into a local `cached_<name>` schema:

```bash
shq pull --remote team                    # Pull all new data
shq pull --remote team --client bob@work  # Pull specific client
shq pull --remote team --since 30d        # Pull last 30 days
```

### Source Tracking

All data includes a `_source` column tracking its origin:

| Data Location | `_source` Value |
|---------------|-----------------|
| `local.*` | `'local'` |
| `cached_team.*` | `'team'` |
| `remotes_*()` | Remote name |

Query example:
```sql
SELECT cmd, exit_code, _source
FROM main.invocations
ORDER BY timestamp DESC;
```

## Querying Data

### Query Local Data Only

```sql
SELECT * FROM local.invocations;
```

### Query Local + Cached (Main)

```sql
SELECT * FROM main.invocations;
```

### Query Attached Remote Directly

```sql
SELECT * FROM "remote_team".invocations;
```

### Query All Remotes (via Macro)

```sql
SELECT * FROM remotes_invocations();
```

### Query Everything

```sql
-- Local + cached + all attached remotes
SELECT * FROM main.invocations
UNION ALL BY NAME
SELECT * FROM remotes_invocations();
```

### Query Current Directory Only

```sql
SELECT * FROM cwd_invocations();
```

## Connection Options

When opening a database connection, you can control what gets loaded:

```rust
use bird::{Store, ConnectionOptions};

// Full connection (default) - attaches remotes, creates macros
let conn = store.connection()?;

// Minimal connection - no attachments, no macros
let conn = store.connect(ConnectionOptions::minimal())?;

// Custom options
let conn = store.connect(ConnectionOptions {
    attach_remotes: true,
    attach_project: false,
    create_ephemeral_views: true,
    run_migration: false,
})?;
```

## Best Practices

### Avoid Database Corruption

1. **Don't persist views referencing attached databases** - Use TEMPORARY macros instead
2. **Use single connections** - Avoid opening multiple concurrent connections to the same database
3. **Use transactions for DDL** - Wrap schema modifications in transactions

### Efficient Syncing

1. **Use `--since` for incremental sync** - Don't re-sync old data
2. **Push before pull** - Ensure your data is on the remote before pulling others'
3. **Use `--dry-run` first** - Preview what will be transferred

### Schema Naming

- Remote names should be simple identifiers (letters, numbers, underscores)
- Hyphens in remote names are converted to underscores in schema names
- Example: remote `my-team` → schema `cached_my_team`

## Configuration

### Remote Configuration

```toml
# config.toml

[[remotes]]
name = "team"
type = "s3"
uri = "s3://team-bucket/bird/bird.duckdb"
credential_provider = "credential_chain"
auto_attach = true  # Attach on every connection

[[remotes]]
name = "backup"
type = "file"
uri = "/mnt/backup/bird.duckdb"
mode = "read_only"
auto_attach = false  # Only attach on demand
```

### Sync Settings

```toml
[sync]
default_remote = "team"
sync_invocations = true
sync_outputs = true
sync_events = true
```

## Troubleshooting

### "Schema does not exist" Error

This usually means:
- The remote wasn't attached (check `auto_attach` setting)
- The remote database doesn't have the expected tables (run push first)

### "Failed to load metadata pointer" Error

This indicates database corruption, usually caused by:
- Persistent views referencing attached databases
- Multiple concurrent connections modifying the catalog

**Fix:** Delete and reinitialize the database, then re-pull data.

### Slow Queries on Remotes

Attached remotes query over the network. For frequently-accessed data:
1. Pull it locally: `shq pull --remote team`
2. Query from `caches` instead of `remotes`
