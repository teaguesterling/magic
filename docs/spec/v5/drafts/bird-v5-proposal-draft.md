# BIRD v5 Proposal: Unified Direction

**To:** blq development team
**From:** BIRD spec maintainers (shq)
**Date:** 2026-02-09
**Status:** Draft for final alignment

---

## Summary

We're aligned on the attempts/outcomes split. This response refines the model further: **D-layers define the logical query interface, S-layers define storage and write semantics.** This separation elegantly handles the short-vs-long invocation problem while keeping the spec abstract.

---

## Core Principle: D-Layers Are Query Interfaces

The D-layers don't specify tables vs views, parquet vs DuckDB. They specify **what named relations exist and what columns they have.**

A BIRD-compliant database at level D1 means:
- You can `SELECT * FROM attempts` and get attempt records
- You can `SELECT * FROM outcomes` and get outcome records
- You can `SELECT * FROM invocations` and get the joined view

*How* those relations are populated is an S-layer concern.

---

## Refined Data Layers

### D0: Core (attempts + invocations view)

The minimum. Records what commands were attempted.

**Required relations:**

```sql
-- bird_meta: schema versioning (always required)
CREATE TABLE bird_meta (
    key       VARCHAR PRIMARY KEY,
    value     VARCHAR NOT NULL
);
-- Required keys: schema_version, primary_client, primary_client_version

-- attempts: what was tried
-- (may be table, view over parquet, or view over invocations table)
attempts (
    id                UUID PRIMARY KEY,      -- UUIDv7
    timestamp         TIMESTAMP NOT NULL,
    cmd               VARCHAR NOT NULL,
    cwd               VARCHAR,
    session_id        VARCHAR,               -- NULL allowed at D0
    source_client     VARCHAR NOT NULL,      -- blq, shq, etc.
    machine_id        VARCHAR,               -- user@hostname
    hostname          VARCHAR,
    executable        VARCHAR,
    format_hint       VARCHAR,
    date              DATE NOT NULL
)

-- invocations: the canonical query interface
-- At D0, this is attempts with NULL outcome columns
invocations (
    -- From attempts
    id, timestamp, cmd, cwd, session_id, source_client, machine_id,
    hostname, executable, format_hint, date,

    -- Outcome columns (NULL at D0)
    completed_at      TIMESTAMP,             -- NULL
    exit_code         INTEGER,               -- NULL
    duration_ms       BIGINT,                -- NULL
    signal            INTEGER,               -- NULL
    status            VARCHAR                -- 'pending' (derived or literal)
)
```

**D0 invariant:** `invocations` always exists. At D0, outcome columns are NULL and status = 'pending'.

### D1: Outcomes

Adds completion information.

**Additional relations:**

```sql
-- outcomes: what happened
outcomes (
    attempt_id        UUID PRIMARY KEY,      -- References attempts.id
    completed_at      TIMESTAMP NOT NULL,
    exit_code         INTEGER,               -- NULL = crashed/unknown
    duration_ms       BIGINT NOT NULL,
    signal            INTEGER,               -- If killed by signal
    timeout           BOOLEAN,               -- If killed by timeout
    date              DATE NOT NULL
)

-- invocations view updated:
-- status = CASE WHEN outcomes.attempt_id IS NULL THEN 'pending' ELSE 'completed' END
-- (or 'orphaned' if exit_code IS NULL but outcome exists)
```

**D1 invariant:** Pending detection is `attempts LEFT JOIN outcomes WHERE outcome IS NULL`.

### D2: Outputs

Adds captured stdout/stderr.

```sql
outputs (
    id                UUID PRIMARY KEY,
    invocation_id     UUID NOT NULL,         -- References attempts.id
    stream            VARCHAR NOT NULL,      -- stdout, stderr, combined
    content_hash      VARCHAR NOT NULL,      -- BLAKE3
    byte_length       BIGINT NOT NULL,
    storage_type      VARCHAR NOT NULL,      -- inline, blob
    storage_ref       VARCHAR NOT NULL,
    date              DATE NOT NULL
)
```

### D3: Events

Adds parsed diagnostics.

```sql
events (
    id                UUID PRIMARY KEY,
    invocation_id     UUID NOT NULL,

    -- Core fields
    severity          VARCHAR,               -- error, warning, info, note
    event_type        VARCHAR,               -- diagnostic, test_result
    message           VARCHAR,
    ref_file          VARCHAR,
    ref_line          INTEGER,
    ref_column        INTEGER,
    format_used       VARCHAR NOT NULL,

    -- Standardized extensions
    error_code        VARCHAR,
    tool_name         VARCHAR,
    category          VARCHAR,
    fingerprint       VARCHAR,
    test_name         VARCHAR,
    test_status       VARCHAR,
    log_line_start    INTEGER,
    log_line_end      INTEGER,

    -- Escape hatch
    metadata          JSON,

    date              DATE NOT NULL
)
```

### D4: Sessions

Adds session metadata.

```sql
sessions (
    session_id        VARCHAR PRIMARY KEY,
    source_client     VARCHAR NOT NULL,
    invoker           VARCHAR NOT NULL,      -- zsh, bash, blq, etc.
    invoker_pid       INTEGER,
    invoker_type      VARCHAR,               -- shell, cli, mcp, ci
    registered_at     TIMESTAMP NOT NULL,
    cwd               VARCHAR,               -- Initial working directory
    date              DATE NOT NULL
)
```

### D5: Multi-Schema Linkage (Future)

Cross-database queries, remote schema attachment, federated views. Not specified in v5 initial release.

---

## Storage Layers: Write Semantics

S-layers define how data gets into the D-layer relations. The spec doesn't mandate a specific approach—these are reference implementations.

### S1: DuckDB File

The database is a single `bird.duckdb` file.

**Conventions:**
- No long-held connections (connect → query → disconnect)
- WAL mode enabled
- Parquet extension loaded

**Write patterns:**

*Option A: Single invocations table (simple)*
```sql
-- Physical storage
CREATE TABLE invocations_table (...);  -- All columns

-- D-layer views
CREATE VIEW attempts AS SELECT <attempt columns> FROM invocations_table;
CREATE VIEW outcomes AS SELECT <outcome columns> FROM invocations_table WHERE exit_code IS NOT NULL;
CREATE VIEW invocations AS SELECT * FROM invocations_table;
```

*Option B: Transactional (for long-running)*
```sql
-- Start transaction at attempt
BEGIN;
INSERT INTO invocations_table (id, timestamp, cmd, ...) VALUES (...);
-- Transaction held open...

-- On completion: update and commit
UPDATE invocations_table SET exit_code = $1, duration_ms = $2, ... WHERE id = $id;
COMMIT;

-- On timeout/crash: rollback or leave pending
```

### S2: Blob Store

Content-addressed file storage for outputs.

**Conventions:**
- Path: `blobs/content/{hash[0:2]}/{hash}[--{hint}].bin[.gz|.zst]`
- Hash: BLAKE3, 64 hex chars
- Compression: optional (.gz or .zst suffix)
- Atomic writes via temp file + rename

**Storage reference formats:**
- Inline: `data:application/octet-stream;base64,...`
- Local: `file:ab/abc123...def.bin.gz`
- Remote: `s3://bucket/blobs/ab/abc123...def.bin.gz`

### S3: Parquet Delegation

For multi-writer scenarios. DuckDB views over parquet files.

**Conventions:**
```sql
-- Pattern-based views
CREATE VIEW attempts AS FROM read_parquet('data/**/attempt-*.parquet');
CREATE VIEW outcomes AS FROM read_parquet('data/**/outcome-*.parquet');
CREATE VIEW complete_invocations AS FROM read_parquet('data/**/complete-*.parquet');

-- Unified invocations view
CREATE VIEW invocations AS
    FROM complete_invocations
    UNION ALL BY NAME (
        FROM attempts LEFT JOIN outcomes USING (id)
    );
```

**Write patterns:**

*Short invocations (known duration < threshold):*
```
Write single file: complete-{session}--{uuid}.parquet
Contains: all attempt + outcome columns
```

*Long invocations (unknown duration):*
```
At start:  attempt-{session}--{uuid}.parquet
At end:    outcome-{session}--{uuid}.parquet
```

The `invocations` view unions both patterns seamlessly.

### S4: Partitioning

For large-scale deployments. Hive-style partitioning.

**Conventions:**
```
data/
├── recent/                      # Hot tier (configurable, default 14 days)
│   ├── attempts/
│   │   └── date=YYYY-MM-DD/
│   ├── outcomes/
│   │   └── date=YYYY-MM-DD/
│   ├── complete/
│   │   └── date=YYYY-MM-DD/
│   └── ...
└── archive/                     # Cold tier
    └── year=YYYY/week=WW/
```

---

## Capability Matrix

| Capability | Description |
|------------|-------------|
| **Reader** | Can query D0-D4 relations |
| **Writer** | Can write to D0-D4 via S1+ |
| **Event** | Can parse events from outputs (duck_hunt or compatible) |
| **Remote** | Supports push/pull sync |

**Client profiles:**

| Client | Data | Storage | Capabilities |
|--------|------|---------|--------------|
| shq | D0-D4 | S1-S4 | Reader, Writer, Remote |
| blq | D0-D3 | S1-S2 | Reader, Writer, Event, Remote |
| Minimal logger | D0-D1 | S1 | Writer |
| IDE plugin | D0-D3 | S1 | Reader, Event |

---

## Answers to blq's Open Questions

### 1. Orphan handling

`exit_code = NULL` in an outcome record means crashed/unknown. The `status` derivation:

```sql
status = CASE
    WHEN outcomes.attempt_id IS NULL THEN 'pending'
    WHEN outcomes.exit_code IS NULL THEN 'orphaned'
    ELSE 'completed'
END
```

No separate status column needed—it's derived from the join + NULL check.

### 2. Outcome without attempt

Allowed. For imported/legacy data, an outcome can exist without a matching attempt. Foreign key is soft (no constraint) or constraint is deferrable.

Queries should handle this gracefully:
```sql
-- Safe: includes orphan outcomes
SELECT * FROM invocations;  -- Uses LEFT JOIN

-- For strict matching only
SELECT * FROM attempts a INNER JOIN outcomes o ON a.id = o.attempt_id;
```

### 3. Multiple outcomes

No. One attempt → one outcome. Retries are new attempts with new UUIDs.

If a command is re-run, that's a new attempt with its own UUID. The original attempt keeps its original outcome (or remains pending if it crashed).

---

## Directory Structure

```
$BIRD_ROOT/                          # ~/.local/share/bird or .bird/
├── db/
│   └── bird.duckdb                  # D0-D4 relations (tables or views)
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

---

## Migration Notes

### From BIRD v4 (shq)

- `invocations` table → becomes source for `attempts` and `outcomes` views
- `status` column → derived from join, can be dropped
- `runner_id` → optional, used for PID-based liveness checking (S-layer concern)
- Pending files → no longer needed; pending = attempt without outcome

### From blq current

- Adopt `.bird/` directory structure
- Add `bird_meta` table
- Map existing tables to D0-D3 schemas
- `source_client = 'blq'` on all records

---

## Next Steps

1. **Finalize D0-D1 schemas** - Exact column names and types
2. **Define `invocations` view precisely** - Handle all edge cases
3. **Reference implementation** - Create test databases at each level
4. **Validation tool** - `bird validate` command

---

## Summary

BIRD v5 is defined by:

- **D-layers (D0-D4)**: Logical query interface. What relations exist and their schemas.
- **S-layers (S1-S4)**: Storage implementation. How data is written and organized.
- **Capabilities**: Reader, Writer, Event, Remote.

The attempts/outcomes split handles crash recovery elegantly. The short-vs-long invocation pattern (complete files vs attempt+outcome files) handles both use cases. The `invocations` view unifies them.

**BIRD is:**
1. A set of named relations (attempts, outcomes, invocations, outputs, events, sessions)
2. Accessible via DuckDB
3. With optional blob storage and partitioning

**BIRD is not:**
- A specific file format
- A specific write pattern
- A monolithic spec

---

*Ready for blq team review.*
