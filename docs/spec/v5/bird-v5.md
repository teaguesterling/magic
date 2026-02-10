# BIRD v5 Specification

**Version:** 5.0
**Date:** 2026-02-10
**Status:** Implemented
**Authors:** shq/BIRD team, blq team

---

## Executive Summary

BIRD v5 introduces a layered architecture that separates **data layers** (what you query) from **storage layers** (how you store). This enables diverse clients to participate at appropriate complexity levels while maintaining interoperability.

**Key changes from v4:**
- Attempts/outcomes split for clean crash recovery
- `MAP(VARCHAR, JSON)` metadata for extensibility
- Schema ownership: BIRD owns `main`, applications get their own schemas
- Storage-agnostic D-layers (tables, views, or parquet - implementation choice)

---

## Part 1: Core Principles

### 1.1 What BIRD Is

BIRD is:
1. A set of named relations (attempts, outcomes, invocations, outputs, events, sessions)
2. Accessible via DuckDB
3. With optional blob storage and partitioning

BIRD is not:
- A specific file format (parquet is one option)
- A specific write pattern (tables vs views vs files)
- A monolithic spec (pick the layers you need)

### 1.2 Layer Separation

**D-layers** define the logical query interface - what relations exist and their schemas.

**S-layers** define storage implementation - how data is written and organized.

These are orthogonal. A client can implement any combination.

### 1.3 Schema Ownership

```
main                          -- BIRD spec owns this
├── bird_meta                -- Required: schema version, client info
├── attempts                 -- D0: what was tried
├── outcomes                 -- D1: what happened
├── invocations              -- D1: view joining attempts + outcomes
├── outputs                  -- D2: captured stdout/stderr
├── events                   -- D3: parsed diagnostics
└── sessions                 -- D4: session metadata

blq                           -- Application-specific schema
shq                           -- Application-specific schema
myapp                         -- Application-specific schema

bird_*                        -- Reserved for BIRD extensions (D5+)
```

Applications create their own schemas for app-specific views, tables, and conveniences.

---

## Part 2: Data Layers (D0-D4)

Data layers define the **logical query interface**. Whether these are implemented as tables, views over tables, or views over parquet files is a storage layer concern.

### D0: Attempts (Core)

The minimum viable BIRD database. Records what commands were attempted.

```sql
-- Required: schema metadata
CREATE TABLE bird_meta (
    key       VARCHAR PRIMARY KEY,
    value     VARCHAR NOT NULL
);
-- Required keys:
--   schema_version: '5'
--   primary_client: 'blq' | 'shq' | etc.
--   primary_client_version: '0.7.0'
--   created_at: ISO timestamp

-- D0: Attempts relation
attempts (
    -- Identity
    id                UUID PRIMARY KEY,      -- UUIDv7 (time-ordered)
    timestamp         TIMESTAMP NOT NULL,    -- When attempt started

    -- Command
    cmd               VARCHAR NOT NULL,      -- Full command string
    executable        VARCHAR,               -- Extracted executable name
    cwd               VARCHAR,               -- Working directory

    -- Grouping
    session_id        VARCHAR,               -- Client-defined session identifier
    tag               VARCHAR,               -- Client/user-defined label

    -- Source
    source_client     VARCHAR NOT NULL,      -- Which BIRD client: blq, shq, etc.
    machine_id        VARCHAR,               -- user@hostname
    hostname          VARCHAR,               -- Hostname alone

    -- Hints
    format_hint       VARCHAR,               -- Expected output format (gcc, cargo, pytest)

    -- Extensibility
    metadata          MAP(VARCHAR, JSON),    -- Namespaced metadata (see §2.7)

    -- Partitioning
    date              DATE NOT NULL          -- For hive partitioning
)

-- D0: Invocations view (attempts with NULL outcome columns)
invocations (
    -- All attempts columns, plus:
    completed_at      TIMESTAMP,             -- NULL at D0
    exit_code         INTEGER,               -- NULL at D0
    duration_ms       BIGINT,                -- NULL at D0
    signal            INTEGER,               -- NULL at D0
    timeout           BOOLEAN,               -- NULL at D0
    status            VARCHAR                -- 'pending' at D0
)
```

**D0 provides:** Metadata-enriched command history. Useful for audit logs, shell history, even without outcomes.

### D1: Outcomes

Adds completion information. The `invocations` view becomes the LEFT JOIN of attempts and outcomes.

```sql
-- D1: Outcomes relation
outcomes (
    attempt_id        UUID PRIMARY KEY,      -- References attempts.id
    completed_at      TIMESTAMP NOT NULL,    -- When command finished
    exit_code         INTEGER,               -- NULL = crashed/unknown
    duration_ms       BIGINT NOT NULL,       -- Wall-clock duration
    signal            INTEGER,               -- If killed by signal (SIGTERM=15, SIGKILL=9)
    timeout           BOOLEAN,               -- If killed by timeout

    -- Extensibility
    metadata          MAP(VARCHAR, JSON),    -- Namespaced metadata

    -- Partitioning
    date              DATE NOT NULL
)

-- D1: Invocations view (refined)
CREATE VIEW invocations AS
SELECT
    a.id,
    a.timestamp,
    a.cmd,
    a.executable,
    a.cwd,
    a.session_id,
    a.tag,
    a.source_client,
    a.machine_id,
    a.hostname,
    a.format_hint,
    a.date,
    o.completed_at,
    o.exit_code,
    o.duration_ms,
    o.signal,
    o.timeout,
    -- Merge metadata from both attempts and outcomes
    map_concat(
        COALESCE(a.metadata, MAP {}),
        COALESCE(o.metadata, MAP {})
    ) AS metadata,
    -- Derive status from join
    CASE
        WHEN o.attempt_id IS NULL THEN 'pending'
        WHEN o.exit_code IS NULL THEN 'orphaned'
        ELSE 'completed'
    END AS status
FROM attempts a
LEFT JOIN outcomes o ON a.id = o.attempt_id;
```

**Status derivation:**
| Condition | Status |
|-----------|--------|
| No matching outcome | `pending` |
| Outcome exists, exit_code IS NULL | `orphaned` |
| Outcome exists, exit_code IS NOT NULL | `completed` |

**D1 provides:** Execution tracking with results. Crash recovery via anti-join query.

### D2: Outputs

Adds captured stdout/stderr with content-addressed storage.

```sql
outputs (
    id                UUID PRIMARY KEY,      -- UUIDv7
    invocation_id     UUID NOT NULL,         -- References attempts.id

    -- Stream identification
    stream            VARCHAR NOT NULL,      -- 'stdout', 'stderr', 'combined'

    -- Content identification
    content_hash      VARCHAR NOT NULL,      -- BLAKE3 hash (64 hex chars)
    byte_length       BIGINT NOT NULL,

    -- Storage location
    storage_type      VARCHAR NOT NULL,      -- 'inline', 'blob'
    storage_ref       VARCHAR NOT NULL,      -- URI to content

    -- Partitioning
    date              DATE NOT NULL
)
```

**Storage reference formats:**
| Type | Format | Example |
|------|--------|---------|
| Inline | `data:` URI | `data:application/octet-stream;base64,SGVsbG8=` |
| Local blob | `file:` path | `file:ab/abc123def.bin.gz` |
| Remote | Full URL | `s3://bucket/blobs/ab/abc123def.bin.gz` |

**D2 provides:** Full output capture with deduplication.

### D3: Events

Adds parsed diagnostics (errors, warnings, test results).

```sql
events (
    id                UUID PRIMARY KEY,      -- UUIDv7
    invocation_id     UUID NOT NULL,         -- References attempts.id

    -- Classification
    severity          VARCHAR,               -- 'error', 'warning', 'info', 'note'
    event_type        VARCHAR,               -- 'diagnostic', 'test_result', etc.

    -- Source location (ref_* = source code location)
    ref_file          VARCHAR,               -- Source file path
    ref_line          INTEGER,               -- Line number
    ref_column        INTEGER,               -- Column number

    -- Content
    message           VARCHAR,               -- Error/warning message

    -- Parsing metadata
    format_used       VARCHAR NOT NULL,      -- Parser format (gcc, cargo, pytest)

    -- Standardized extension fields (all nullable)
    error_code        VARCHAR,               -- E0308, W0401, etc.
    tool_name         VARCHAR,               -- gcc, pytest, ruff
    category          VARCHAR,               -- compile, lint, test
    fingerprint       VARCHAR,               -- Deduplication hash
    test_name         VARCHAR,               -- Test name (for test_result events)
    test_status       VARCHAR,               -- passed, failed, skipped
    log_line_start    INTEGER,               -- Position in output
    log_line_end      INTEGER,

    -- Escape hatch
    metadata          JSON,                  -- Client-specific data

    -- Partitioning
    date              DATE NOT NULL
)
```

**D3 provides:** Structured diagnostics for build analysis.

### D4: Sessions

Adds session metadata for grouping and context.

```sql
sessions (
    session_id        VARCHAR PRIMARY KEY,   -- Matches attempts.session_id
    source_client     VARCHAR NOT NULL,      -- Which BIRD client
    invoker           VARCHAR NOT NULL,      -- zsh, bash, blq, python, etc.
    invoker_pid       INTEGER,               -- Process ID
    invoker_type      VARCHAR,               -- 'shell', 'cli', 'mcp', 'ci', 'script'
    registered_at     TIMESTAMP NOT NULL,    -- When session started
    cwd               VARCHAR,               -- Initial working directory

    -- Partitioning
    date              DATE NOT NULL
)
```

**Session ID conventions:**
| Context | session_id format |
|---------|-------------------|
| Shell hook (shq) | `zsh-{pid}`, `bash-{pid}` |
| CLI run (blq) | `blq-{parent_pid}` or `blq-run-{n}` |
| MCP session | `mcp-{server_pid}-{conversation_id}` |
| CI workflow | `{provider}-{run_id}` |
| One-off | Random UUID |

**D4 provides:** Session context and grouping.

### D5: Extensions (Reserved)

Reserved for future BIRD extensions:
- Multi-schema linkage
- Remote database attachment
- Federation

Schemas with `bird_` prefix are reserved for D5+ features.

---

## 2.7 Metadata Conventions

Metadata uses `MAP(VARCHAR, JSON)` with namespaced keys to prevent collisions.

### Well-Known Keys

| Key | Purpose | Example |
|-----|---------|---------|
| `vcs` | Version control state | `{"provider": "git", "commit": "abc123", "branch": "main", "dirty": false}` |
| `ci` | CI/CD context | `{"provider": "github_actions", "run_id": "123", "job": "test"}` |
| `env` | Environment variables | `{"PATH": "/usr/bin", "CC": "gcc"}` |
| `resources` | Resource usage (outcomes) | `{"peak_memory_mb": 1024, "cpu_time_ms": 45000}` |
| `timing` | Detailed timing (outcomes) | `{"user_ms": 4200, "sys_ms": 800}` |
| `distributed` | Distributed execution (outcomes) | `{"host": "worker-03", "queue": "high-priority"}` |

### Client-Specific Keys

Clients use their own namespace for client-specific data:

```sql
-- blq-specific
metadata['blq'] = '{"source_type": "run", "registered_cmd": "test-all", "timeout_ms": 300000}'

-- shq-specific
metadata['shq'] = '{"capture_mode": "pty", "shell_level": 2}'
```

### Querying Metadata

```sql
-- Find invocations on main branch
SELECT * FROM invocations
WHERE metadata['vcs']->>'branch' = 'main';

-- Aggregate by CI provider
SELECT metadata['ci']->>'provider' AS provider, COUNT(*)
FROM invocations
WHERE metadata['ci'] IS NOT NULL
GROUP BY 1;

-- High memory usage
SELECT * FROM invocations
WHERE CAST(metadata['resources']->>'peak_memory_mb' AS INTEGER) > 1000;
```

### Application-Level Typing

Applications can create typed views:

```sql
CREATE VIEW blq.runs AS
SELECT
    *,
    CAST(metadata['vcs'] AS STRUCT(
        provider VARCHAR,
        commit VARCHAR,
        branch VARCHAR,
        dirty BOOLEAN
    )) AS vcs,
    CAST(metadata['ci'] AS STRUCT(
        provider VARCHAR,
        run_id VARCHAR,
        job VARCHAR
    )) AS ci
FROM main.invocations;
```

---

## Part 3: Storage Layers (S1-S4)

Storage layers define **how data is written**. The D-layers specify the query interface; S-layers implement it.

### S1: DuckDB File

The database is a single `bird.duckdb` file.

**Conventions:**
- No long-held connections (connect → query/write → disconnect)
- WAL mode recommended
- Parquet extension required

**Implementation options:**

*Option A: Single table (simple, for short invocations)*
```sql
CREATE TABLE invocations_store (...);  -- All columns

CREATE VIEW attempts AS SELECT <attempt_cols> FROM invocations_store;
CREATE VIEW outcomes AS SELECT <outcome_cols> FROM invocations_store WHERE completed_at IS NOT NULL;
CREATE VIEW invocations AS SELECT * FROM invocations_store;  -- Or use the join view
```

*Option B: Separate tables (for attempts/outcomes split)*
```sql
CREATE TABLE attempts (...);
CREATE TABLE outcomes (...);
CREATE VIEW invocations AS <join query>;
```

*Option C: Transactional (for long-running commands)*
```sql
BEGIN;
INSERT INTO attempts (...) VALUES (...);
-- Hold transaction open during execution...
INSERT INTO outcomes (...) VALUES (...);
COMMIT;
```

### S2: Blob Store

Content-addressed file storage for outputs.

**Directory structure:**
```
blobs/
└── content/
    └── {hash[0:2]}/
        └── {hash}[--{hint}].bin[.gz|.zst]
```

**Conventions:**
- Hash: BLAKE3, 64 hex characters
- Sharding: First 2 hex chars as subdirectory
- Compression: Optional, indicated by extension (.gz, .zst)
- Hint: Optional command hint for human readability
- Atomic writes: temp file + rename
- Inline threshold: Outputs < 4KB stored inline as base64 data URIs

**Examples:**
```
blobs/content/ab/abc123def...789.bin
blobs/content/ab/abc123def...789--make-test.bin.gz
```

### S3: Parquet Delegation

For multi-writer scenarios. DuckDB views over parquet files.

**Directory structure:**
```
data/
├── attempts/
│   └── date=YYYY-MM-DD/
│       └── {session}--{uuid}.parquet
├── outcomes/
│   └── date=YYYY-MM-DD/
│       └── {session}--{uuid}.parquet
├── complete/                              -- Short invocations (single file)
│   └── date=YYYY-MM-DD/
│       └── {session}--{uuid}.parquet
├── outputs/
│   └── date=YYYY-MM-DD/
└── events/
    └── date=YYYY-MM-DD/
```

**View definitions:**
```sql
CREATE VIEW attempts AS FROM read_parquet('data/attempts/*/*.parquet')
    UNION ALL BY NAME FROM read_parquet('data/complete/*/*.parquet');

CREATE VIEW outcomes AS FROM read_parquet('data/outcomes/*/*.parquet')
    UNION ALL BY NAME FROM read_parquet('data/complete/*/*.parquet');

CREATE VIEW invocations AS
    FROM read_parquet('data/complete/*/*.parquet')
    UNION ALL BY NAME (
        FROM read_parquet('data/attempts/*/*.parquet') a
        LEFT JOIN read_parquet('data/outcomes/*/*.parquet') o
        ON a.id = o.attempt_id
    );
```

**Write patterns:**

| Duration | Pattern |
|----------|---------|
| Short (< threshold) | Single `complete/*.parquet` file with all columns |
| Long (unknown) | `attempts/*.parquet` at start, `outcomes/*.parquet` at end |

### S4: Partitioning

For large-scale deployments with hot/cold tiering.

**Directory structure:**
```
data/
├── recent/                      -- Hot tier (configurable, default 14 days)
│   ├── attempts/
│   │   └── date=YYYY-MM-DD/
│   ├── outcomes/
│   ├── complete/
│   ├── outputs/
│   └── events/
└── archive/                     -- Cold tier
    └── year=YYYY/
        └── week=WW/
            └── {table}--{source_client}--compacted.parquet
```

**Compaction:** Merge small files into larger ones per partition.

**Archival:** Move data older than threshold to archive tier.

---

## Part 4: Capabilities

Capabilities describe what a client supports.

| Capability | Description |
|------------|-------------|
| **Reader** | Can query D0-D4 relations, resolve blob references |
| **Writer** | Can write to D0-D4 via S1+ |
| **Event** | Can parse events from outputs (duck_hunt or compatible) |
| **Remote** | Supports push/pull sync to remote databases |

### Client Profiles

| Client | Data Layers | Storage Layers | Capabilities |
|--------|-------------|----------------|--------------|
| shq | D0-D4 | S1-S4 | Reader, Writer, Remote |
| blq | D0-D3 | S1-S2 | Reader, Writer, Event, Remote |
| Minimal logger | D0-D1 | S1 | Writer |
| IDE plugin | D0-D3 | S1 | Reader, Event |
| CI exporter | D0-D2 | S1-S2 | Writer, Remote |

---

## Part 5: Directory Structure

```
$BIRD_ROOT/                          # ~/.local/share/bird or .bird/
├── db/
│   └── bird.duckdb                  # DuckDB database (D0-D4 relations)
├── blobs/
│   └── content/                     # S2: Content-addressed storage
│       └── {hash[0:2]}/{hash}.bin[.gz]
├── data/                            # S3-S4: Parquet files (if used)
│   ├── recent/
│   │   ├── attempts/
│   │   ├── outcomes/
│   │   ├── complete/
│   │   ├── outputs/
│   │   └── events/
│   └── archive/
├── config.toml                      # Configuration
└── {app}.{purpose}.toml             # App-specific files
```

**Git tracking:**
- Track: `config.toml`, `*.toml` (app configs)
- Ignore: `db/`, `blobs/`, `data/`

---

## Part 6: Configuration

Configuration uses TOML format exclusively.

### 6.1 Main Configuration

```toml
# config.toml

[bird]
schema_version = "5"
primary_client = "shq"
primary_client_version = "0.2.0"

[storage]
mode = "parquet"              # "duckdb" or "parquet"
inline_threshold = 4096       # Bytes; outputs smaller than this are inline
compression = "gzip"          # "none", "gzip", or "zstd"

[capture]
hot_days = 14                 # Days before archiving

[sync]
default_remote = "team"
```

### 6.2 App-Specific Configuration

Apps use namespaced files: `{app}.{purpose}.toml`

```toml
# blq.commands.toml
[commands.build]
cmd = "make -j8"
format_hint = "gcc"
timeout = 600

[commands.test]
cmd = "pytest"
format_hint = "pytest"
```

```toml
# shq.hints.toml
[[hints]]
pattern = "cargo *"
format = "cargo"

[[hints]]
pattern = "pytest*"
format = "pytest"
```

---

## Part 7: Interoperability

### 6.1 Multi-Client UUIDs

When a BIRD client invokes another BIRD client, share the UUID:

```bash
# Parent sets environment
export BIRD_INVOCATION_UUID=<uuid>
export BIRD_PARENT_CLIENT=shq

# Child checks and uses shared UUID
```

### 6.2 Cross-Client Queries

Use DuckDB's ATTACH for cross-database queries:

```sql
ATTACH 'other.bird.duckdb' AS other;
SELECT * FROM main.invocations
UNION ALL
SELECT * FROM other.invocations;
```

### 6.3 Sync Protocol

Push/pull uses UUID-based deduplication:
- Same UUID = same record (deduplicate)
- Different UUIDs = different records (keep both)
- Invocations are append-only; no conflict resolution needed

---

## Part 8: Migration

### From BIRD v4 (shq)

1. Add `bird_meta` table with schema_version = '5'
2. Create `attempts` view over existing invocations (or refactor to table)
3. Create `outcomes` view/table from invocations where exit_code IS NOT NULL
4. Add `metadata` column to attempts and outcomes
5. Remove `status` column (now derived)
6. Remove pending files (no longer needed)

### From blq current

1. Move to `.bird/` directory structure
2. Add `bird_meta` table
3. Map existing tables to D0-D3 schemas
4. Set `source_client = 'blq'` on all records
5. Convert config to TOML

---

## Summary

BIRD v5 provides:

- **D-layers (D0-D4):** Logical query interface with increasing richness
- **S-layers (S1-S4):** Storage implementation options
- **Capabilities:** Reader, Writer, Event, Remote
- **Extensibility:** `MAP(VARCHAR, JSON)` metadata with namespaced keys
- **Clean crash recovery:** Attempts without outcomes = pending

The attempts/outcomes split eliminates pending files and status partitioning. The metadata pattern enables client-specific extensions without schema fragmentation. The schema ownership model (BIRD owns `main`, apps own their schemas) provides clear boundaries.

---

*BIRD v5 Specification - Implemented 2026-02-10*
