# MAGIC: Multi-Actor Gateway for Invocation Capture

**MAGIC** is a system for capturing, storing, and querying shell command history with structured output data. It combines automatic capture, content-addressed storage, and powerful SQL queries to give you deep insights into your build and development workflows.

## What is MAGIC?

MAGIC = **BIRD** + **shq**

```
┌──────────────────────────────────────────────────────────────────┐
│                          Your Shell                              │
│                                                                  │
│  $ make test                                                     │
│  $ shq o                 # show output from last command         │
│  $ shq i                 # browse command history                │
│  $ shq R %/make/~1       # re-run last make command              │
│  $ shq q "SELECT * FROM invocations WHERE exit_code != 0"        │
└──────────────────────────────────────────────────────────────────┘
                            │
                            ▼
┌──────────────────────────────────────────────────────────────────┐
│                     shq (Shell Query)                            │
│                                                                  │
│  • Captures commands automatically via shell hooks               │
│  • Query with micro-language: shq o %exit<>0~5                   │
│  • Re-run commands: shq R, manage data: shq archive/compact      │
└──────────────────────────────────────────────────────────────────┘
                            │
                            ▼
┌──────────────────────────────────────────────────────────────────┐
│                  BIRD (Storage Layer)                            │
│                                                                  │
│  • DuckDB-based database with Parquet files                      │
│  • Content-addressed blob storage (70-90% space savings!)        │
│  • Hot/warm/cold tiering for lifecycle management                │
│  • SQL-queryable: invocations, outputs, sessions                 │
└──────────────────────────────────────────────────────────────────┘
```

## Key Features

### Automatic Capture
Every command you run is automatically captured with:

- Command text and arguments
- Exit code and duration
- Stdout and stderr (content-addressed)
- Working directory, hostname, timestamp

### Intelligent Storage (70-90% Savings)
Content-addressed blob storage with automatic deduplication:

- 100 identical test runs → 1 blob file (99% savings)
- CI/CD workflows with repeated outputs → 70-90% reduction
- Fast BLAKE3 hashing at 3GB/s

### Clean Crash Recovery (v5)
The attempts/outcomes schema split provides reliable crash recovery:

- **Attempts:** Record what commands were started
- **Outcomes:** Record how they completed
- **Pending detection:** SQL query instead of file markers
- Status derived from JOIN (pending/completed/orphaned)

### SQL-Powered Queries
Query your shell history with the full power of DuckDB:

```sql
-- Find all failed commands this week
SELECT cmd, exit_code, duration_ms, timestamp
FROM invocations
WHERE exit_code != 0 AND date >= CURRENT_DATE - 7;

-- Which commands take the longest?
SELECT cmd, AVG(duration_ms) as avg_duration
FROM invocations
GROUP BY cmd
ORDER BY avg_duration DESC
LIMIT 10;
```

### Data Lifecycle Management
Automatic tiering keeps your data organized:

- **Recent (0-14 days):** Fast SSD, individual parquet files
- **Archive (>14 days):** Compacted, deduplicated storage

## Quick Start

```bash
# Install shq
cargo install --git https://github.com/yourorg/magic shq

# Initialize BIRD database
shq init

# Add to your shell config (~/.zshrc or ~/.bashrc)
eval "$(shq hook init)"

# Start using your shell normally - commands are captured automatically!
make test
shq show        # See output from last command
shq history     # Browse recent commands
```

## Next Steps

- [Getting Started](getting-started.md) - Detailed installation and setup
- [Shell Integration](shq_shell_integration.md) - How the hooks work
- [shq Commands](shq_implementation.md) - Full command reference
- [SQL Queries](sql-queries.md) - Query examples and tips
- [BIRD v5 Specification](spec/v5/bird-v5.md) - Database schema and architecture
- [Spec Changelog](SPEC_CHANGELOG.md) - Version history
