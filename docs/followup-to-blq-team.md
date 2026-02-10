# Follow-up: BIRD v5 Collaborative Direction

**To:** blq development team
**From:** BIRD spec maintainers (shq)
**Date:** 2026-02-09
**Re:** Response to BIRD v5 Proposal

---

Hey team,

Thanks for the thoughtful v5 proposal. We've worked through it in detail and have a response document with the full analysis (`response-bird-v5.md`), but wanted to send this shorter follow-up with where we've landed and proposed next steps.

## TL;DR

We're aligned on the direction. Your proposal helped us see that BIRD was over-specified for shq's needs and under-specified for the broader ecosystem. The key refinement: **separate data layers from storage layers**.

## What We Agreed On Immediately

- Layered architecture (yes, absolutely)
- `bird_meta` table for schema versioning (overdue)
- Blob compression support (.gz, .zst)
- Events extension fields (tool_name, category, fingerprint, etc.)
- TOML standardization
- App namespacing (`{app}.{purpose}.toml`)
- Git-tracked vs local file separation

## The Key Refinement: Two Layer Hierarchies

Your proposal had layers 0-5 mixing schema concerns with storage concerns. After discussion, we think there should be two orthogonal hierarchies:

### Data Layers (what you store)

| Layer | Content | Required Tables |
|-------|---------|-----------------|
| D0 | Core | invocations, bird_meta |
| D1 | Outputs | + outputs |
| D2 | Events | + events |
| D3 | Sessions | + sessions |

### Storage Layers (how you store)

| Layer | Features |
|-------|----------|
| S1 | DuckDB database file |
| S2 | Content-addressed blob store |
| S3 | Storage delegation (parquet views, PostgreSQL) |
| S4 | Partitioning (hive, hot/cold tiers) |

This means a client can implement D0-D2 with just S1-S2 (blq's profile), while another implements D0-D3 with S1-S4 (shq's profile). The combinations are independent.

**The core insight you provided:** BIRD should be abstract at its foundation. The spec defines:
1. A DuckDB database (or DuckDB-queryable interface)
2. A directory convention
3. A schema contract

How that's physically realized (native tables, parquet files, PostgreSQL backend) is a storage layer choice, not a spec requirement.

## Resolved Questions

**Layer 0 (invocations only) is valid.** You were right. It's metadata-enriched bash history—still useful. A client could even do D0+D2 (invocations + events, skipping raw output) if events are what matters.

**Sessions are client-defined.** The `session_id` field is required (for grouping), but semantics are up to the client. Shell = persistent session. CLI = per-run session. CI = workflow session. UUID works too.

**Per-app databases.** We agree: each app owns its database. Cross-app interop comes from sync/push/pull and DuckDB's `ATTACH`, not shared writes. This eliminates the migration coordination problem entirely.

**Capability levels.** We propose four orthogonal capabilities:
- **Reader** - Can query BIRD databases
- **Writer** - Can write to BIRD databases
- **Event** - Can parse events from other clients' outputs
- **Remote** - Supports push/pull sync

## Semantic Clarification Needed

One naming issue: BIRD v4 uses `client_id` for `user@hostname` (the machine identity). Your proposal uses it for "which BIRD client" (blq, shq). These are different concepts.

Proposed resolution:
- `machine_id` = user@hostname (where it ran)
- `source_client` = blq, shq, etc. (who recorded it)

This preserves v4 semantics while adding your concept cleanly.

## Proposed Next Steps

### Joint Work

1. **Draft BIRD v5 spec** - We can split this:
   - shq team: Data layers (D0-D3 schemas), storage layers (S3-S4)
   - blq team: Storage layers (S1-S2), capability definitions, sync protocol

2. **Create reference databases** - Minimal test fixtures at each layer combination for compliance testing.

3. **Validation tooling** - A `bird validate` command that checks schema conformance and reports capability level.

### Questions for You

1. **Event interop** - You have parsers for formats we don't (ruff, eslint?). Should the spec define a parser registry, or is this purely client-specific?

2. **Sync protocol** - Your proposal mentions push/pull. What conflict resolution do you envision? Last-write-wins? Merge by UUID?

3. **MCP standardization** - You mentioned MCP server integration. Should BIRD v5 define a standard MCP tool set, or leave this to clients?

4. **blq migration timeline** - How disruptive would moving from `.lq/` to `.bird/` be for your users? Should we define a discovery fallback?

## Where This Leaves Us

We think BIRD v5 can be a genuine multi-client spec rather than "shq's format that others can read." The layered architecture makes adoption tractable—a minimal client is trivial to implement, and clients can add layers as needed.

The blq perspective was exactly what we needed. Shell hooks have different constraints than CLI tools, and the spec should accommodate both without forcing unnecessary complexity on either.

Let us know your thoughts on the refinements and proposed next steps. Happy to jump on a call if that's easier.

---

*Looking forward to BIRD v5.*

— The shq/BIRD team
