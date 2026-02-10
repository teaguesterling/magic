# Follow-up from blq: Refining Data Layers

**To:** BIRD spec maintainers (shq)
**From:** blq development team
**Date:** 2026-02-09
**Re:** BIRD v5 Collaborative Direction

---

## Summary

We're aligned on the two-hierarchy model (Data Layers × Storage Layers). This follow-up addresses your questions and proposes a refinement to D0 that elegantly solves the pending/crash-recovery problem.

---

## Answers to Your Questions

### 1. Event Parsers: duck_hunt + Open

BIRD should support event extraction via duck_hunt but not be limited to it. The spec should:

- Define the events table schema (which we've agreed on)
- Reference duck_hunt as the canonical parser implementation
- Allow clients to use custom parsers that produce conforming events

If a client has a custom ruff parser or eslint integration, that's fine—as long as the output conforms to the events schema. No registry needed; the schema *is* the contract.

### 2. Sync Conflict Resolution: Merge by UUID

UUIDs are globally unique. Merge-by-UUID is the natural choice:

- Same UUID = same record (deduplicate)
- Different UUIDs = different records (keep both)
- No last-write-wins needed if UUIDs are properly unique (UUIDv7)

For updates (rare), we could use `(uuid, updated_at)` with latest-timestamp-wins, but invocations are append-only in practice.

### 3. MCP: No Standard Tool Set

Agreed—MCP standardization is out of scope for BIRD. BIRD defines storage conventions for invocation data. How clients expose that data (CLI, MCP, REST API, TUI) is their choice.

blq happens to have an MCP server. shq might not. That's fine.

### 4. Migration Timeline

We'll address `.lq/` → `.bird/` migration once the spec is finalized. For now, we're focused on getting the spec right.

---

## Refinement: Sessions as Optional Metadata

We agree that `session_id` should be in invocations, but propose:

- `session_id` can be NULL (truly sessionless invocations) or a random UUID
- The `sessions` table (D3) provides *metadata* about sessions
- Without D3, you have session IDs but no session details—still useful for grouping

This means:
- D0-D2 client: Has `session_id` on invocations, but no sessions table
- D3 client: Also has sessions table with invoker, PID, registration time, etc.

The session ID's *source* varies by context:

| Context | session_id source |
|---------|-------------------|
| Shell hook (shq) | Shell PID, persistent |
| CLI run (blq) | Parent shell PID or per-run UUID |
| MCP session | Server PID + conversation ID |
| CI workflow | Workflow run ID |
| One-off script | Random UUID or NULL |

---

## Proposal: Split Invocations into Attempts + Outcomes

Here's a refinement that elegantly handles pending/crash scenarios without the complexity of pending files and status partitioning.

### The Problem

Current approach:
1. Write pending file before command starts
2. Write invocation record after command completes
3. On crash: pending file exists but no invocation record
4. Recovery: scan pending files, check PID liveness, mark as orphaned

This works but adds complexity: pending directory, JSON files, PID checking, status partitions.

### The Insight

An invocation is really two events:
1. **Attempt**: "I'm about to run `make test`" (known immediately)
2. **Outcome**: "It finished with exit code 0 after 5.2s" (known after completion)

By modeling these separately, crash recovery becomes trivial: an attempt without a matching outcome = incomplete.

### Proposed Schema

```sql
-- D0a: Attempts (written at command start)
CREATE TABLE attempts (
    id                UUID PRIMARY KEY,      -- UUIDv7
    timestamp         TIMESTAMP NOT NULL,    -- When attempt started
    cmd               VARCHAR NOT NULL,
    cwd               VARCHAR,
    session_id        VARCHAR,               -- NULL allowed
    source_client     VARCHAR NOT NULL,      -- blq, shq, etc.
    machine_id        VARCHAR,               -- user@hostname

    -- Optional context
    executable        VARCHAR,
    format_hint       VARCHAR,

    date              DATE NOT NULL
);

-- D0b: Outcomes (written at command completion)
CREATE TABLE outcomes (
    attempt_id        UUID PRIMARY KEY,      -- References attempts.id
    completed_at      TIMESTAMP NOT NULL,
    exit_code         INTEGER NOT NULL,
    duration_ms       BIGINT NOT NULL,

    -- Optional flags
    signal            INTEGER,               -- If killed by signal
    timeout           BOOLEAN,               -- If killed by timeout

    date              DATE NOT NULL
);

-- Convenience view (what most queries use)
CREATE VIEW invocations AS
SELECT
    a.id,
    a.timestamp,
    a.cmd,
    a.cwd,
    a.session_id,
    a.source_client,
    a.machine_id,
    o.completed_at,
    o.exit_code,
    o.duration_ms,
    CASE
        WHEN o.attempt_id IS NULL THEN 'pending'
        ELSE 'completed'
    END AS status,
    a.date
FROM attempts a
LEFT JOIN outcomes o ON a.id = o.attempt_id;
```

### Benefits

1. **Natural crash recovery**: `SELECT * FROM attempts WHERE id NOT IN (SELECT attempt_id FROM outcomes)` = pending/crashed

2. **No pending files**: The database *is* the source of truth for in-flight status

3. **No status partitioning**: Status is derived from join, not stored

4. **Atomic writes**: Each table gets a single atomic INSERT (no update needed)

5. **Cleaner semantics**: Attempt = intent, Outcome = result

6. **Works with any storage layer**: DuckDB tables, parquet files, PostgreSQL—all work

### Revised Data Layers

| Layer | Content | Tables |
|-------|---------|--------|
| D0 | Attempts | attempts, bird_meta |
| D1 | Outcomes | + outcomes |
| D2 | Outputs | + outputs |
| D3 | Events | + events |
| D4 | Sessions | + sessions |

This layering reflects increasing levels of information capture:

- **D0 (Attempts)**: "We tried to run something" — the absolute minimum. Useful for audit logs, command history, even without knowing results. A BIRD-powered shell history would basically be a D0 implementation.
- **D1 (Outcomes)**: "Here's what happened" — exit codes, duration. The `invocations` view joins D0+D1.
- **D2 (Outputs)**: "Here's the raw output" — captured stdout/stderr.
- **D3 (Events)**: "Here's what we parsed from it" — errors, warnings, test results.
- **D4 (Sessions)**: "Here's context about the invoker" — shell info, environment.

A minimal client (D0 only) is essentially enriched command history. Add D1 and you have execution tracking. Add D2-D3 and you have build log analysis. Add D4 for full session context.

The `invocations` view is available at D1+ (requires both attempts and outcomes). D0-only clients query `attempts` directly.

### Write Patterns

**At command start:**
```sql
INSERT INTO attempts (id, timestamp, cmd, cwd, session_id, source_client, date)
VALUES ($uuid, NOW(), 'make test', '/home/user/proj', 'shell-12345', 'shq', CURRENT_DATE);
```

**At command completion:**
```sql
INSERT INTO outcomes (attempt_id, completed_at, exit_code, duration_ms, date)
VALUES ($uuid, NOW(), 0, 5234, CURRENT_DATE);
```

**Query pending:**
```sql
SELECT * FROM invocations WHERE status = 'pending';
-- Or directly:
SELECT * FROM attempts WHERE id NOT IN (SELECT attempt_id FROM outcomes);
```

### Crash Recovery

On startup, a client can:
1. Query for pending attempts from this session/client
2. Check if the process is still running (optional PID check)
3. Either: wait, mark as orphaned (insert outcome with exit_code=NULL), or leave pending

This is simpler than the current pending-file approach and works naturally with sync (pending attempts sync to remote; outcomes sync when available).

---

## Updated Layer Summary

With this refinement:

| Layer | Tables | Purpose |
|-------|--------|---------|
| D0 | attempts, bird_meta | Command attempts (minimum viable) |
| D1 | + outcomes | Completion results (exit code, duration) |
| D2 | + outputs | Captured stdout/stderr |
| D3 | + events | Parsed diagnostics |
| D4 | + sessions | Session metadata |

The `invocations` view requires D0+D1. Storage layers remain orthogonal (S1-S4).

**Typical client profiles:**
- **blq**: D0-D3 + S1-S2 (attempts, outcomes, outputs, events with DuckDB + blobs)
- **shq**: D0-D4 + S1-S4 (full stack with parquet partitioning)
- **Minimal logger**: D0 + S1 (just attempts in DuckDB)

---

## Open Questions

1. **Orphan handling**: Should there be a standard `exit_code` value for "crashed/unknown"? NULL? -1? A separate `status` column in outcomes?

2. **Outcome without attempt**: Can an outcome exist without an attempt? (Imported data, legacy migration) Probably yes—foreign key should be soft.

3. **Multiple outcomes**: Can an attempt have multiple outcomes? (Retry scenarios) Probably no—one attempt, one outcome. Retries are new attempts.

---

## Next Steps

If you're aligned on the attempts/outcomes split, we can:

1. Draft the D0 schema together
2. Define the `invocations` view precisely
3. Specify crash-recovery semantics
4. Move to D1-D3 schemas

Let us know your thoughts.

---

*— The blq team*
