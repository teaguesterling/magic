# Response: Metadata and Schema Conventions

**To:** blq development team
**From:** BIRD spec maintainers (shq)
**Date:** 2026-02-09
**Re:** Metadata and Extensibility Follow-up

---

## Summary

We're aligned on the extensibility direction. This response confirms the metadata pattern and proposes schema naming conventions.

---

## Metadata: `MAP(VARCHAR, JSON)`

Agreed on `MAP(VARCHAR, JSON)` for both attempts and outcomes.

**Why this type:**
- Namespaced keys prevent collisions (`vcs`, `ci`, `env`, `blq`, `shq`)
- JSON allows per-namespace schema evolution
- Easily aggregatable without parsing - can merge attempts + outcomes metadata:

```sql
-- Merged metadata in invocations view
SELECT
    a.*,
    o.exit_code,
    o.duration_ms,
    map_concat(a.metadata, o.metadata) AS metadata,  -- Merged!
    ...
FROM attempts a
LEFT JOIN outcomes o ON a.id = o.attempt_id;
```

**Both attempts and outcomes get metadata:**

```sql
attempts (
    ...
    metadata          MAP(VARCHAR, JSON),
    ...
)

outcomes (
    ...
    metadata          MAP(VARCHAR, JSON),
    ...
)
```

---

## VCS in Metadata, Not First-Class Columns

We prefer VCS as metadata rather than first-class `git_*` columns.

**Rationale:**
- Not all invocations are in git repos
- Generalizes to other VCS (hg, svn, fossil, etc.)
- Keeps core schema minimal

**Convention:**

```sql
metadata['vcs'] = '{
    "provider": "git",
    "commit": "abc123def456...",
    "branch": "main",
    "dirty": true
}'

-- Or for other VCS:
metadata['vcs'] = '{"provider": "hg", "revision": "123", "branch": "default"}'
metadata['vcs'] = '{"provider": "svn", "revision": 4567}'
```

**Application-level typing:**

Applications can create typed views that cast metadata to structs:

```sql
-- In blq schema
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

Or conversely, applications with first-class VCS columns can cast to JSON when writing to BIRD:

```sql
-- Application has typed data, casts to metadata for BIRD
INSERT INTO main.attempts (id, cmd, metadata, ...)
VALUES (
    $id,
    $cmd,
    MAP {
        'vcs': to_json(STRUCT(provider := 'git', commit := $commit, branch := $branch, dirty := $dirty)),
        'blq': to_json(STRUCT(source_type := 'run', timeout_ms := 300000))
    },
    ...
);
```

---

## Outcomes Metadata Examples

```sql
-- Resource usage
metadata['resources'] = '{"peak_memory_mb": 1024, "cpu_time_ms": 45000}'

-- Distributed execution
metadata['distributed'] = '{"host": "worker-03", "queue": "high-priority", "cluster": "prod"}'

-- Detailed timing
metadata['timing'] = '{"user_ms": 4200, "sys_ms": 800, "wall_ms": 5234}'

-- Client-specific
metadata['blq'] = '{"parsed_events": 47, "format_detected": "pytest"}'
```

---

## Schema Naming Convention

**Proposal:** BIRD owns `main`, applications get their own schemas.

```
main                          -- BIRD spec (D0-D4)
├── bird_meta                -- Schema version, client info
├── attempts                 -- D0
├── outcomes                 -- D1
├── invocations              -- D1 view (attempts LEFT JOIN outcomes)
├── outputs                  -- D2
├── events                   -- D3
└── sessions                 -- D4

blq                           -- blq application schema
├── runs                     -- Typed view over main.invocations
├── commands                 -- Registered command definitions
└── ...

shq                           -- shq application schema
├── format_hints             -- Format detection config
└── ...

bird_*                        -- Reserved for BIRD extensions (D5+)
                             -- e.g., bird_remote_team, bird_archive
```

**Rationale:**
- It's a BIRD database - BIRD relations belong in `main`
- `SELECT * FROM invocations` just works
- Applications namespace their extensions
- `bird` and `bird_*` reserved for future spec use

---

## Agreed: `tag` Field

Keeping `tag` in attempts for client/user-defined labels:

```sql
attempts (
    ...
    tag               VARCHAR,
    ...
)
```

| Client | tag usage |
|--------|-----------|
| blq | Registered command name: "build", "test" |
| shq | User-defined or auto-detected: "make", "cargo" |
| CI | Job name: "lint", "test-matrix" |

---

## Agreed: `ref_*` Naming

`ref_file`, `ref_line`, `ref_column` in events refer to **source code locations** (duck_hunt convention).

blq's human-readable invocation references ("build:3:5") should use a different term - `locator`, `run_ref`, or similar - to avoid confusion.

---

## Updated D0-D1 Schema

```sql
-- D0: Attempts
CREATE TABLE attempts (
    -- Identity
    id                UUID PRIMARY KEY,
    timestamp         TIMESTAMP NOT NULL,

    -- Command
    cmd               VARCHAR NOT NULL,
    executable        VARCHAR,
    cwd               VARCHAR,

    -- Grouping
    session_id        VARCHAR,
    tag               VARCHAR,

    -- Source
    source_client     VARCHAR NOT NULL,
    machine_id        VARCHAR,
    hostname          VARCHAR,

    -- Hints
    format_hint       VARCHAR,

    -- Extensibility
    metadata          MAP(VARCHAR, JSON),

    -- Partitioning
    date              DATE NOT NULL
);

-- D1: Outcomes
CREATE TABLE outcomes (
    attempt_id        UUID PRIMARY KEY,
    completed_at      TIMESTAMP NOT NULL,
    exit_code         INTEGER,
    duration_ms       BIGINT NOT NULL,
    signal            INTEGER,
    timeout           BOOLEAN,

    -- Extensibility
    metadata          MAP(VARCHAR, JSON),

    -- Partitioning
    date              DATE NOT NULL
);

-- D1: Invocations view
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
    map_concat(COALESCE(a.metadata, MAP {}), COALESCE(o.metadata, MAP {})) AS metadata,
    CASE
        WHEN o.attempt_id IS NULL THEN 'pending'
        WHEN o.exit_code IS NULL THEN 'orphaned'
        ELSE 'completed'
    END AS status
FROM attempts a
LEFT JOIN outcomes o ON a.id = o.attempt_id;
```

---

## Next Steps

1. **Finalize D0-D1 schemas** - Confirm column names and types
2. **Define metadata conventions** - Document well-known keys (`vcs`, `ci`, `env`, `resources`)
3. **Draft D2-D4 schemas** - Outputs, events, sessions
4. **Reference implementation** - Create test databases

Let us know if this aligns with blq's needs.

---

*— The shq/BIRD team*
