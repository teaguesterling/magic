# Follow-up from blq: Metadata and Extensibility

**To:** BIRD spec maintainers (shq)
**From:** blq development team
**Date:** 2026-02-09
**Re:** BIRD v5 Proposal - Gaps and Refinements

---

## Summary

The v5 spec is close to meeting blq's needs. This follow-up addresses extensibility gaps and proposes refinements to D0/D1 that enable client-specific metadata without fragmenting the spec.

---

## Key Observations

### 1. D1 Should Include Execution Context, Not Just Outcomes

The current framing of D1 is "outcomes" (exit code, duration). But D1's description—"execution tracking with results"—suggests it should also include execution context that's only known at completion time or is expensive to capture upfront.

**Examples of execution context:**
- Git state: `git_commit`, `git_branch`, `git_dirty`
- CI context: provider, run ID, job name
- Environment snapshot: relevant env vars
- Resource usage: peak memory, CPU time (future)

**Proposal:** D1 adds an `invocation_context` relation (or extends outcomes) with context fields:

```sql
-- Option A: Extend outcomes
outcomes (
    attempt_id        UUID PRIMARY KEY,
    completed_at      TIMESTAMP NOT NULL,
    exit_code         INTEGER,
    duration_ms       BIGINT NOT NULL,
    signal            INTEGER,
    timeout           BOOLEAN,

    -- Execution context (known at completion or expensive to capture)
    git_commit        VARCHAR,
    git_branch        VARCHAR,
    git_dirty         BOOLEAN,

    date              DATE NOT NULL
)

-- Option B: Separate relation
invocation_context (
    attempt_id        UUID PRIMARY KEY,
    git_commit        VARCHAR,
    git_branch        VARCHAR,
    git_dirty         BOOLEAN,
    ci_provider       VARCHAR,
    ci_run_id         VARCHAR,
    ...
)
```

**Alternative:** These could be in D0 (attempts) if we want them available for pending invocations. The `invocations` view would surface them regardless of where they're stored.

### 2. Standardize Metadata Pattern, Not Specific Fields

Different clients need different metadata. Rather than enumerate all possible fields, standardize a metadata column:

```sql
-- On attempts (D0)
metadata          MAP(VARCHAR, JSON)

-- Enables:
metadata['git'] = '{"commit": "abc123", "branch": "main", "dirty": false}'
metadata['ci'] = '{"provider": "github_actions", "run_id": "12345"}'
metadata['env'] = '{"PATH": "/usr/bin", "CC": "gcc"}'
metadata['blq'] = '{"source_type": "run", "timeout_ms": 300000}'
```

**Benefits:**
- Clients store what they need without schema changes
- Namespaced keys prevent collisions (`git`, `ci`, `blq`, `shq`)
- `MAP(VARCHAR, JSON)` allows native DuckDB aggregation and querying
- Schema evolution via convention, not migration

**Query examples:**
```sql
-- Find invocations on main branch
SELECT * FROM invocations
WHERE metadata['git']->>'branch' = 'main';

-- Aggregate by CI provider
SELECT metadata['ci']->>'provider', COUNT(*)
FROM invocations
GROUP BY 1;
```

### 3. Include `tag` Field in Attempts Schema

blq uses `source_name` to track which registered command was run ("build", "test", "lint"). shq could use this for user-defined tagging. The spec already mentions `tag` in v4—let's keep it.

```sql
attempts (
    ...
    tag               VARCHAR,           -- User/client-defined label
    ...
)
```

**Usage:**
| Client | tag value |
|--------|-----------|
| blq | Registered command name: "build", "test" |
| shq | User-defined or auto-detected: "make", "cargo" |
| CI | Job name: "lint", "test-matrix" |

This enables queries like:
```sql
-- Recent build failures
SELECT * FROM invocations WHERE tag = 'build' AND exit_code != 0;

-- Error counts by command
SELECT tag, COUNT(*) FROM events WHERE severity = 'error' GROUP BY tag;
```

---

## Proposed Schema Refinements

### D0: Attempts (refined)

```sql
attempts (
    -- Identity
    id                UUID PRIMARY KEY,
    timestamp         TIMESTAMP NOT NULL,

    -- Command
    cmd               VARCHAR NOT NULL,
    executable        VARCHAR,
    cwd               VARCHAR,

    -- Grouping
    session_id        VARCHAR,
    tag               VARCHAR,              -- NEW: client/user label

    -- Source
    source_client     VARCHAR NOT NULL,
    machine_id        VARCHAR,
    hostname          VARCHAR,

    -- Hints
    format_hint       VARCHAR,

    -- Extensibility
    metadata          MAP(VARCHAR, JSON),   -- NEW: namespaced metadata

    -- Partitioning
    date              DATE NOT NULL
)
```

### D1: Outcomes (refined)

```sql
outcomes (
    attempt_id        UUID PRIMARY KEY,
    completed_at      TIMESTAMP NOT NULL,
    exit_code         INTEGER,
    duration_ms       BIGINT NOT NULL,
    signal            INTEGER,
    timeout           BOOLEAN,

    -- Execution context (optional, known at completion)
    git_commit        VARCHAR,              -- NEW
    git_branch        VARCHAR,              -- NEW
    git_dirty         BOOLEAN,              -- NEW

    date              DATE NOT NULL
)
```

**Or:** Move git fields to attempts if we want them for pending invocations. The `invocations` view surfaces them either way.

### invocations View

```sql
CREATE VIEW invocations AS
SELECT
    a.*,
    o.completed_at,
    o.exit_code,
    o.duration_ms,
    o.signal,
    o.timeout,
    o.git_commit,
    o.git_branch,
    o.git_dirty,
    CASE
        WHEN o.attempt_id IS NULL THEN 'pending'
        WHEN o.exit_code IS NULL THEN 'orphaned'
        ELSE 'completed'
    END AS status
FROM attempts a
LEFT JOIN outcomes o ON a.id = o.attempt_id;
```

---

## blq-Specific Metadata

With the `metadata MAP(VARCHAR, JSON)` column, blq stores:

```sql
metadata['blq'] = '{
    "source_type": "run",           -- run, exec, import, capture
    "registered_cmd": "test-all",   -- if from registry
    "timeout_ms": 300000,
    "capture_output": true
}'

metadata['env'] = '{
    "VIRTUAL_ENV": "/home/user/.venv",
    "CC": "gcc-12"
}'

metadata['ci'] = '{
    "provider": "github_actions",
    "workflow": "CI",
    "run_id": "12345",
    "job": "test"
}'
```

This keeps blq-specific concerns out of the core schema while enabling rich queries.

---

## Naming: "ref" Collision

You noted that duck_hunt uses "ref" for source locations (ref_file, ref_line, ref_column). blq uses "ref" for human-friendly invocation references ("build:3:5").

**Proposal:** blq renames its concept to avoid confusion:
- "ref" → "locator" or "run_ref" or "invocation_ref"

The spec should clarify that `ref_*` columns refer to source code locations (duck_hunt convention), not invocation identifiers.

---

## Summary of Proposals

1. **Add `tag` to attempts** - Client/user-defined label for grouping
2. **Add `metadata MAP(VARCHAR, JSON)` to attempts** - Namespaced extensibility
3. **Add git fields to outcomes** (or attempts) - Execution context
4. **Clarify ref naming** - `ref_*` = source locations, not invocation IDs

These changes make the spec flexible enough for diverse clients while keeping the core schema clean.

---

*— The blq team*
