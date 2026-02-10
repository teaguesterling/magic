# Response to BIRD v5 Proposal: A Path Forward

**From:** BIRD spec maintainers (shq)
**To:** blq development team
**Date:** 2026-02-09
**Status:** Response for discussion

---

## Executive Summary

Thank you for this thoughtful proposal. The blq team's experience implementing BIRD-compatible storage in a different context (CLI build tool vs shell hooks) provides valuable perspective. The core insight—that BIRD v4 optimizes for shq's specific needs but the concepts generalize—is correct.

We agree with the direction: **BIRD v5 should adopt a layered architecture** that enables diverse clients to participate at appropriate complexity levels. However, we propose some modifications to ensure the layers are coherent and the interoperability contract is clear.

This response identifies areas of strong agreement, points of tension requiring resolution, and proposes a synthesis for BIRD v5.

---

## Part 1: Strong Agreement

### 1.1 Layered Architecture Is Correct

The proposal correctly identifies that different clients have different needs:

| Client Type | Needs Sessions? | Needs Outputs? | Needs Multi-Writer? | Needs Crash Recovery? |
|-------------|-----------------|----------------|---------------------|-----------------------|
| Shell hooks (shq) | Yes | Yes | Yes | Yes |
| CLI build tool (blq) | No | Yes | No | No |
| CI system | Optional | Yes | Optional | Maybe |
| IDE plugin | Maybe | Optional | No | No |

A one-size-fits-all spec forces unnecessary complexity on simple clients. The layered approach is the right solution.

### 1.2 Schema Versioning (bird_meta) Is Essential

BIRD v4 has no in-database schema versioning. The proposal's `bird_meta` table is a necessary addition:

```sql
CREATE TABLE bird_meta (
    key       VARCHAR PRIMARY KEY,
    value     VARCHAR NOT NULL
);

-- Required entries
INSERT INTO bird_meta VALUES ('schema_version', '5');
INSERT INTO bird_meta VALUES ('created_at', '2026-02-09T10:30:00Z');
INSERT INTO bird_meta VALUES ('primary_client', 'blq');
INSERT INTO bird_meta VALUES ('primary_client_version', '0.7.0');
```

**Agreed:** This should be mandatory in BIRD v5.

### 1.3 Blob Compression Is Obvious Win

The proposal's compression data is compelling:

```
Build log compression ratios:
- gzip: 11-23% of original (77-89% storage savings)
- zstd: 12-23% of original (similar)

1.5MB uncompressed → ~200-300KB compressed
```

BIRD v4 mandates uncompressed blobs, which wastes storage.

**Agreed:** Compression should be supported. Use file extension to indicate format:
- `.bin` - uncompressed (current default)
- `.bin.gz` - gzip compressed
- `.bin.zst` - zstd compressed

Storage references indicate compression: `file:ab/abc123.bin.gz`

### 1.4 Events Extension Mechanism Is Needed

BIRD v4's events schema is minimal. The proposal adds genuinely useful fields:

| Field | Purpose | Queryable? |
|-------|---------|------------|
| `tool_name` | Which tool (gcc, pytest, ruff) | Yes |
| `category` | Event category (compile, lint, test) | Yes |
| `fingerprint` | Deduplication hash | Yes |
| `context` | Surrounding log lines | No (too large) |
| `log_line_start/end` | Position in output | Yes |

**Agreed:** Adopt Option B (standardized extension columns) with Option A (JSON metadata) as escape hatch:

```sql
CREATE TABLE events (
    -- Core fields (v4)
    id, invocation_id, severity, message, ref_file, ref_line, ref_column, format_used,

    -- Standardized extensions (new in v5, all nullable)
    error_code        VARCHAR,   -- E0308, F401, etc.
    tool_name         VARCHAR,   -- gcc, pytest, ruff
    category          VARCHAR,   -- compile, lint, test
    fingerprint       VARCHAR,   -- Dedup hash
    test_name         VARCHAR,   -- For test events
    test_status       VARCHAR,   -- passed, failed, skipped
    log_line_start    INTEGER,   -- Position in output
    log_line_end      INTEGER,

    -- Escape hatch for client-specific data
    metadata          JSON,

    date DATE NOT NULL
);
```

### 1.5 TOML Standardization

**Agreed:** TOML for all configuration. The Norway problem and YAML's implicit typing are real footguns.

### 1.6 Git-Tracked vs Local Separation

**Agreed:** The spec should explicitly define which files are git-trackable:

```
.bird/
├── config.toml           # GIT: Project configuration
├── {app}.{purpose}.toml  # GIT: App-specific config
├── .gitignore            # GIT: Auto-generated
│
├── db/                   # LOCAL: All runtime data
│   ├── bird.duckdb
│   └── ...
└── blobs/                # LOCAL: Content storage
```

### 1.7 App Namespacing

**Agreed:** `{app}.{purpose}.toml` naming convention for client-specific files in `.bird/`:

- `blq.commands.toml` - blq command registry
- `shq.hints.toml` - shq format hints
- `myapp.config.toml` - custom app config

This prevents conflicts while keeping a single discovery point.

---

## Part 2: Points of Tension

### 2.1 What Is "Core"?

**Proposal says:** Layer 0 (Core) = invocations table only
**Concern:** This is too minimal

If Layer 0 is just invocations, two "BIRD-compatible" clients might share almost nothing useful. A client recording "make test" with no output, no exit code tracking, no timing—what value does that provide?

**Counter-proposal:** Core should include minimum viable utility:

```sql
-- Layer 0: Absolute minimum
CREATE TABLE bird_meta (...);  -- Required for versioning

CREATE TABLE invocations (
    id                UUID PRIMARY KEY,
    timestamp         TIMESTAMP NOT NULL,
    cmd               VARCHAR NOT NULL,
    exit_code         INTEGER,         -- NULL for pending/crashed
    duration_ms       BIGINT,
    cwd               VARCHAR,
    hostname          VARCHAR,
    source_client     VARCHAR NOT NULL,  -- Which BIRD client wrote this
    date              DATE NOT NULL
);
```

**Rationale:** Without at least exit codes and timing, you can't answer "what commands failed?" or "what commands are slow?" These are the most basic queries.

### 2.2 Semantic Confusion: `client_id`

**BIRD v4 usage:** `client_id` = `user@hostname` (identifies the machine/user)
**Proposal usage:** `client_id` = BIRD client name (blq, shq)

These are different concepts being given the same name.

**Proposed resolution:**

```sql
-- Which machine/user ran this
machine_id        VARCHAR NOT NULL,     -- user@hostname (was client_id in v4)
hostname          VARCHAR,
username          VARCHAR,

-- Which BIRD client recorded this
source_client     VARCHAR NOT NULL,     -- "blq", "shq", etc.
source_version    VARCHAR,              -- "0.7.0"
```

This preserves v4 semantics while adding the proposal's concept cleanly.

### 2.3 Schema Layers vs Storage Profiles

The proposal conflates two orthogonal concerns:

1. **Schema layers:** What tables/columns exist (invocations, outputs, events, sessions)
2. **Storage profiles:** How data is stored (single-writer DuckDB, multi-writer parquet)

Layer 4 (Multi-Writer & Crash Recovery) isn't really a schema layer—it's an implementation choice that's orthogonal to what data you store.

**Proposed separation:**

**Schema Layers (what you store):**
- Layer 0: Core (invocations + bird_meta)
- Layer 1: Outputs
- Layer 2: Events
- Layer 3: Sessions

**Storage Profiles (how you store):**
- Profile A: Single-Writer (DuckDB tables, simplified layout)
- Profile B: Multi-Writer (parquet files, pending tracking, status partitioning)

A client can implement any schema layer with any storage profile. For example:
- blq: Layers 0-2, Profile A
- shq: Layers 0-3, Profile B
- CI system: Layers 0-2, Profile B

### 2.4 Sessions: Optional or Minimal?

**Proposal says:** Sessions are optional (Layer 3)
**shq reality:** Sessions are tightly coupled to invocations

The issue: If sessions are truly optional, `invocations.session_id` becomes nullable. But then queries like "show commands from this shell" become unreliable.

**Proposed resolution:** Define minimal session support:

```sql
-- Invocations always have a session_id
session_id        VARCHAR NOT NULL,

-- But the sessions table itself is optional (Layer 3)
-- Clients without Layer 3 use synthetic session IDs: "blq-{pid}" or "cli-{run_id}"
```

This way, invocations always group by session (enabling "commands from this session" queries), but the sessions table with rich metadata is optional.

### 2.5 Shared Database Coordination

**Proposal suggests:** Multiple clients can share `.bird/db/bird.duckdb`
**Concern:** Who manages schema migrations?

If blq and shq share a database:
- blq upgrades to schema v6, adds new columns
- shq (still on v5) connects—what happens?

**Need to resolve:**
1. Schema version negotiation protocol
2. Column addition rules (nullable only? specific naming?)
3. Migration ownership (primary_client in bird_meta?)
4. Conflict resolution for concurrent migrations

**Proposed rules:**
```
1. Check bird_meta.schema_version on connect
2. If higher than supported: read-only mode, log warning
3. If lower than expected: run migrations only if this client is primary_client
4. Migrations MUST be additive (new nullable columns, new tables)
5. Migrations MUST NOT delete or rename existing columns
```

### 2.6 What Does "BIRD-Compatible" Mean?

The proposal lacks a clear interoperability contract. If I claim my tool is "BIRD v5 compatible," what does that guarantee?

**Proposed compatibility levels:**

| Level | Requirements | Can Read | Can Write |
|-------|--------------|----------|-----------|
| **Reader** | Can query BIRD databases | Yes | No |
| **Writer L0** | Can write core invocations | L0+ | L0 |
| **Writer L1** | Can write outputs | L0-1+ | L0-1 |
| **Writer L2** | Can write events | L0-2+ | L0-2 |
| **Writer L3** | Can write sessions | L0-3+ | L0-3 |

**Compatibility rules:**
1. Readers MUST ignore unknown columns
2. Writers MUST preserve columns they don't understand
3. Writers MUST set `source_client` on all records they create
4. Writers SHOULD NOT modify records from other clients

---

## Part 3: Proposed BIRD v5 Structure

Based on the above analysis, here's a proposed outline:

### Part 1: Core Concepts

- What BIRD is (command execution recording with queryable history)
- Design principles (crash-safe, queryable, extensible, multi-client)
- Key terms (invocation, session, output, event, blob)
- Versioning (bird_meta table, semantic versioning)

### Part 2: Schema Layers

**Layer 0 - Core (mandatory):**
- `bird_meta` table (schema version, client info)
- `invocations` table (command metadata, timing, results)
- Required fields: id, timestamp, cmd, exit_code, duration_ms, session_id, source_client, date
- Optional extension columns: git_commit, git_branch, git_dirty, ci_context

**Layer 1 - Outputs (optional):**
- `outputs` table (captured stdout/stderr)
- Content-addressed blob storage
- Compression support (.bin, .bin.gz, .bin.zst)

**Layer 2 - Events (optional):**
- `events` table (parsed diagnostics)
- Core fields + standardized extensions + JSON metadata

**Layer 3 - Sessions (optional):**
- `sessions` table (shell/invoker tracking)
- Rich metadata (invoker type, initial cwd, registration time)

### Part 3: Storage Profiles

**Profile A - Single-Writer:**
```
.bird/
├── db/
│   └── bird.duckdb
├── blobs/
│   └── content/
├── config.toml
└── {app}.{purpose}.toml
```

**Profile B - Multi-Writer:**
```
.bird/
├── db/
│   ├── bird.duckdb
│   ├── pending/
│   └── data/
│       ├── recent/
│       │   ├── invocations/status=.../date=.../
│       │   └── ...
│       └── archive/
├── blobs/
│   └── content/
├── config.toml
└── {app}.{purpose}.toml
```

### Part 4: Features

- Blob compression (optional)
- In-flight tracking (Profile B)
- Hot/cold tiering (Profile B)
- Compaction (Profile B)
- Remote sync
- Multi-client coordination

### Part 5: Interoperability

- Discovery (`.bird/` ascending, then $BIRD_ROOT, then ~/.local/share/bird)
- Multi-client protocol (source_client, BIRD_INVOCATION_UUID)
- Schema evolution (additive-only, bird_meta versioning)
- Compatibility levels (Reader, Writer L0-L3)

### Part 6: Extensions

- Git metadata (git_commit, git_branch, git_dirty)
- CI context (ci JSON column)
- Command registry ({app}.commands.toml)
- Format hints ({app}.hints.toml)
- MCP server (standard tool set)

### Part 7: Configuration

- Format: TOML
- Standard sections: bird, storage, capture, sync, mcp
- App-specific files: {app}.{purpose}.toml
- Git tracking guidance

---

## Part 4: Resolved Questions

Based on further discussion, the following positions have been agreed:

### Q1: Layer 0 (Invocations Only) Is Sufficient

**Resolution:** Yes, Layer 0 with just invocations is valid.

Layer 0 is analogous to metadata-enriched bash history, which has genuine value:
- "What commands did I run yesterday?"
- "What failed in the last hour?"
- "How long do my builds take?"

These queries don't require output capture. A minimal BIRD client that only records invocations is still useful and interoperable.

**Note:** One could argue that extracted events are more valuable than raw output for many use cases. A client implementing L0 + L2 (invocations + events, skipping outputs) is valid—it captures what matters (the errors) without the storage cost of full output.

### Q2: Sessions Are Client-Defined

**Resolution:** Session semantics are entirely up to the client.

- Shell hooks (shq): Session = shell instance (persistent, long-lived)
- CLI tools (blq): Session = single run (`blq run build` creates one session)
- CI systems: Session = workflow run or job
- Sessionless clients: Can use UUID per invocation

The `session_id` field is required (for grouping), but the sessions table (Layer 3) with rich metadata remains optional. Most clients will find session semantics natural—even CI workflows have implicit sessions.

### Q3: Storage Layers Should Be Separate from Data Layers

**Resolution:** The BIRD spec should define two orthogonal layer hierarchies.

**Insight:** The core of BIRD is:
1. The DuckDB database (or DuckDB-queryable interface)
2. The directory it lives in
3. Configuration and initialization conventions

The underlying physical storage is an implementation detail. Valid BIRD implementations include:
- DuckDB with native tables
- DuckDB with parquet file views
- DuckDB attached to PostgreSQL
- DuckDB attached to MotherDuck

**Proposed Storage Layers:**

| Layer | Description | Examples |
|-------|-------------|----------|
| S0 | Abstract SQL schema + blob references | The logical schema only |
| S1 | DuckDB database file | `bird.duckdb` with conventions (no long-held handles, locking) |
| S2 | Blob store file conventions | Content-addressed `.bin[.gz\|.zst]` files, sharding |
| S3 | Storage delegation | Parquet files with views, PostgreSQL backend |
| S4 | Partitioning | Status/date hive partitioning, hot/cold tiers |

This means:
- A minimal BIRD client only needs S0 (schema) + S1 (DuckDB file)
- Blob storage (S2) is optional if you only use inline storage
- Parquet delegation (S3) is for multi-writer scenarios
- Partitioning (S4) is for large-scale deployments

### Q4: Per-App Databases, Sync Handles Interop

**Resolution:** Each app owns its own database. Cross-app interoperability comes from sync/push/pull, not shared writes.

**Benefits:**
- No migration coordination problem
- No locking conflicts
- Clear ownership
- Apps can upgrade independently

**Migration strategy:**
- Apps don't need to write old formats
- Old formats can be ingested via sync/pull
- The `source_client` field identifies origin

**Cross-app queries:** Use DuckDB's `ATTACH` to query multiple databases:
```sql
ATTACH 'shq.bird.duckdb' AS shq;
ATTACH 'blq.bird.duckdb' AS blq;

-- Query across both
SELECT * FROM shq.invocations
UNION ALL
SELECT * FROM blq.invocations
ORDER BY timestamp DESC;
```

### Q5: Capability Levels (Refined)

**Resolution:** Define capability levels across multiple dimensions.

**Data Capabilities:**

| Level | Tables | Description |
|-------|--------|-------------|
| D0 | invocations, bird_meta | Metadata-enriched history |
| D1 | + outputs | Output capture |
| D2 | + events | Parsed diagnostics |
| D3 | + sessions | Session tracking |

**Storage Capabilities:**

| Level | Features | Description |
|-------|----------|-------------|
| S1 | DuckDB file | Basic storage |
| S2 | + blob store | Content-addressed blobs |
| S3 | + delegation | Parquet or PostgreSQL backend |
| S4 | + partitioning | Hive partitioning, tiers |

**Interoperability Capabilities:**

| Capability | Description |
|------------|-------------|
| Reader | Can query BIRD databases (D0-D3, S1-S2 for blobs) |
| Writer | Can write to BIRD databases (D0-D3, S1+) |
| Event | Can parse events from other clients' outputs |
| Remote | Supports push/pull sync semantics |

**Example client profiles:**

| Client | Data | Storage | Interop |
|--------|------|---------|---------|
| shq | D0-D3 | S1-S4 | Reader, Writer, Remote |
| blq | D0-D2 | S1-S2 | Reader, Writer, Event, Remote |
| IDE plugin | D0-D2 | S1 | Reader, Event |
| CI exporter | D0-D1 | S1-S2 | Writer, Remote |

---

## Part 5: Refined BIRD v5 Architecture

Based on the above resolutions, here's the refined structure:

### Layered Architecture Overview

```
┌─────────────────────────────────────────────────────────────┐
│                    BIRD v5 Architecture                      │
├─────────────────────────────────────────────────────────────┤
│  DATA LAYERS (what you store)                               │
│  ┌───────┐ ┌─────────┐ ┌────────┐ ┌──────────┐             │
│  │  D0   │ │   D1    │ │   D2   │ │    D3    │             │
│  │ Invoc │→│ Outputs │→│ Events │→│ Sessions │             │
│  └───────┘ └─────────┘ └────────┘ └──────────┘             │
├─────────────────────────────────────────────────────────────┤
│  STORAGE LAYERS (how you store)                             │
│  ┌───────┐ ┌─────────┐ ┌────────────┐ ┌─────────────┐      │
│  │  S1   │ │   S2    │ │     S3     │ │     S4      │      │
│  │DuckDB │→│  Blobs  │→│ Delegation │→│ Partitioning│      │
│  └───────┘ └─────────┘ └────────────┘ └─────────────┘      │
├─────────────────────────────────────────────────────────────┤
│  CAPABILITIES (what you support)                            │
│  ┌────────┐ ┌────────┐ ┌───────┐ ┌────────┐                │
│  │ Reader │ │ Writer │ │ Event │ │ Remote │                │
│  └────────┘ └────────┘ └───────┘ └────────┘                │
└─────────────────────────────────────────────────────────────┘
```

### Core Principle

**BIRD is:**
1. A DuckDB database (or DuckDB-queryable interface)
2. A directory structure convention
3. A schema contract

**BIRD is not:**
- A specific file format (parquet is one option)
- A specific backend (DuckDB tables, parquet views, PostgreSQL are all valid)
- A monolithic spec (pick the layers you need)

---

## Part 6: Recommended Next Steps

### Immediate (Spec Definition)

1. **Write BIRD v5 spec document** with:
   - Data Layers (D0-D3) with exact schemas
   - Storage Layers (S0-S4) with conventions
   - Capability definitions (Reader, Writer, Event, Remote)

2. **Resolve semantic confusion:**
   - `machine_id` = user@hostname (where it ran)
   - `source_client` = blq, shq, etc. (who recorded it)
   - Document clearly in spec

3. **Define DuckDB conventions (S1):**
   - Connection patterns (connect/query/disconnect, no long handles)
   - Locking behavior (or lack thereof)
   - WAL handling
   - Extension requirements (parquet at minimum)

4. **Define blob conventions (S2):**
   - Content-addressing (BLAKE3)
   - Directory sharding ({hash[0:2]}/{hash}.bin)
   - Compression options (.gz, .zst)
   - Inline threshold

### Near-term (Validation)

5. **Create reference databases:**
   - `reference-d0.bird.duckdb` - Minimal (invocations only)
   - `reference-d0d1.bird.duckdb` - With outputs
   - `reference-d0d1d2.bird.duckdb` - With events
   - `reference-full.bird.duckdb` - All layers

6. **Create validation tool:**
   - Check schema conformance
   - Validate bird_meta entries
   - Test blob resolution
   - Report capability level

### Medium-term (Ecosystem)

7. **Define sync protocol:**
   - Push/pull semantics
   - Conflict resolution (last-write-wins? merge?)
   - Remote types (S3, MotherDuck, PostgreSQL, file)

8. **Define Event interop:**
   - Standard format parsers (gcc, cargo, pytest, eslint, etc.)
   - Parser registration/discovery
   - Cross-client event parsing

9. **Document migration paths:**
   - BIRD v4 → v5 (shq)
   - blq current → BIRD v5
   - Generic ingest format for other tools

---

## Summary

The blq proposal has helped crystallize BIRD v5 into a cleaner architecture:

- **Data layers** define what you store (invocations → outputs → events → sessions)
- **Storage layers** define how you store it (DuckDB → blobs → delegation → partitioning)
- **Capabilities** define what you support (Reader, Writer, Event, Remote)

This separation means:
- A minimal client (D0, S1) is trivial to implement
- Clients can adopt layers incrementally
- Different storage backends serve different needs
- Interoperability is well-defined at each level

The key insight from blq: **BIRD should be abstract at its core**, with DuckDB as the query interface but not mandating specific physical storage. This opens BIRD to a broader ecosystem while maintaining interoperability.

---

*Prepared jointly by BIRD spec maintainers and blq development team.*
*Looking forward to BIRD v5.*
