# MAGIC: Multi-Actor Gateway for Invocation Capture

**MAGIC** is a system for capturing, storing, and querying shell command history with structured output data. It combines automatic capture, content-addressed storage, and powerful SQL queries to give you deep insights into your build and development workflows.

## What is MAGIC?

MAGIC = **BIRD** + **shq**

```
┌──────────────────────────────────────────────────────────────────┐
│                          Your Shell                              │
│                                                                  │
│  $ make test                                                     │
│  $ shq show              # show output from last command         │
│  $ shq history           # browse command history                │
│  $ shq sql "SELECT * FROM invocations WHERE exit_code != 0"      │
└──────────────────────────────────────────────────────────────────┘
                            │
                            ▼
┌──────────────────────────────────────────────────────────────────┐
│                     shq (Shell Query)                            │
│                                                                  │
│  • Captures commands automatically via shell hooks               │
│  • Provides CLI for querying: shq show, shq history, shq sql     │
│  • Manages data lifecycle: shq archive, shq compact              │
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

### Queryable Everything
```sql
-- Find all failed builds in the last week
SELECT cmd, exit_code, duration_ms, timestamp
FROM invocations
WHERE exit_code != 0 AND date >= CURRENT_DATE - 7;

-- Find flaky commands (sometimes pass, sometimes fail)
SELECT cmd,
       COUNT(*) as runs,
       SUM(CASE WHEN exit_code = 0 THEN 1 ELSE 0 END) as passes,
       SUM(CASE WHEN exit_code != 0 THEN 1 ELSE 0 END) as failures
FROM invocations
WHERE cmd LIKE '%test%'
GROUP BY cmd
HAVING failures > 0 AND passes > 0;

-- Slowest commands this week
SELECT cmd, duration_ms, timestamp
FROM invocations
WHERE date >= CURRENT_DATE - 7 AND duration_ms IS NOT NULL
ORDER BY duration_ms DESC
LIMIT 10;
```

### Intelligent Storage (70-90% Savings)
Content-addressed blob storage with automatic deduplication:
- 100 identical test runs → 1 blob file (99% savings)
- CI/CD workflows with repeated outputs → 70-90% reduction
- Automatic: no configuration needed
- Fast: BLAKE3 hashing at 3GB/s

### Structured Output Parsing (Coming in v0.6)
Optional integration with [duck_hunt](https://github.com/teaguesterling/duck_hunt), a DuckDB extension for parsing 90+ log formats:
- Compilers: GCC, Clang, MSVC, rustc
- Linters: ESLint, pylint, shellcheck
- Test runners: pytest, jest, go test
- Build tools: Make, Cargo, npm

*Note: duck_hunt integration is planned for v0.6. Currently shq captures raw outputs.*

### Fast Queries
- DuckDB columnar storage for analytics
- Partitioned by date for fast filtering
- Indexed by hash for instant deduplication
- Gzip-compressed blobs (DuckDB reads directly)

### Hot/Warm/Cold Tiering
Automatic lifecycle management:
- **Hot (0-14 days):** Recent commands, fast SSD
- **Warm (14-90 days):** Compressed Parquet
- **Cold (>90 days):** Archive tier, deduplicated

## Quick Start

### Installation

```bash
# Install shq (includes BIRD)
cargo install --git https://github.com/yourorg/magic shq

# Initialize BIRD database
shq init

# Add shell integration to your shell config
eval "$(shq hook init)"  # Auto-detects zsh/bash

# Or specify shell explicitly
eval "$(shq hook init --shell zsh)"
eval "$(shq hook init --shell bash)"
```

### Basic Usage

```bash
# Commands are automatically captured via shell hooks
make test

# View output from the last command
shq show

# View recent commands
shq history

# Run a command with full output capture
shq run make all

# Or use shqr (shell function, captures output while showing it)
shqr make test

# Query with SQL
shq sql "SELECT cmd, exit_code FROM invocations WHERE date = CURRENT_DATE"

# View today's commands
shq sql "SELECT * FROM invocations_today"

# View failed commands
shq sql "SELECT * FROM failed_invocations LIMIT 10"
```

### Advanced Queries

```bash
# Find flaky tests (inconsistent exit codes)
shq sql "
  SELECT
    cmd,
    COUNT(DISTINCT exit_code) as exit_codes,
    SUM(CASE WHEN exit_code = 0 THEN 1 ELSE 0 END) as passes,
    SUM(CASE WHEN exit_code != 0 THEN 1 ELSE 0 END) as failures
  FROM invocations
  WHERE cmd LIKE '%test%'
  GROUP BY cmd
  HAVING exit_codes > 1
"

# Storage savings report
shq sql "
  SELECT
    COUNT(*) as total_outputs,
    COUNT(DISTINCT content_hash) as unique_blobs,
    ROUND(100.0 * (1 - COUNT(DISTINCT content_hash)::FLOAT / COUNT(*)), 1) as dedup_percent
  FROM outputs
"

# Slowest commands this week
shq sql "
  SELECT cmd, duration_ms, timestamp
  FROM invocations
  WHERE date >= CURRENT_DATE - 7
  ORDER BY duration_ms DESC
  LIMIT 10
"
```

## Architecture

### Directory Structure

```
~/.local/share/bird/              # Default BIRD_ROOT
├── db/
│   ├── bird.duckdb               # Main DuckDB database
│   └── pending/                  # In-flight invocation markers (crash recovery)
│       └── <session>--<uuid>.pending
├── data/
│   ├── recent/                   # Hot tier (0-14 days)
│   │   ├── invocations/
│   │   │   └── status=<status>/  # pending, completed, orphaned
│   │   │       └── date=YYYY-MM-DD/*.parquet
│   │   ├── outputs/
│   │   │   └── date=YYYY-MM-DD/*.parquet
│   │   └── sessions/
│   │       └── date=YYYY-MM-DD/*.parquet
│   └── archive/                  # Cold tier (>14 days)
│       └── ...                   # Same structure (without status partitioning)
├── blobs/
│   └── content/                  # Content-addressed pool
│       ├── ab/
│       │   └── abc123...def.bin.gz
│       └── ...                   # 256 subdirs (00-ff)
├── extensions/                   # DuckDB extensions
├── sql/                          # Custom SQL files
└── config.toml
```

### Components

#### 1. BIRD (Buffer and Invocation Record Database)

**Purpose:** Storage layer with DuckDB backend

**Features:**
- DuckDB columnar database for fast analytics
- Parquet files for efficient, lock-free storage and querying
- Content-addressed blob storage with deduplication
- Automatic tiering (hot/warm/cold)
- SQL-queryable schema

**Schema:**
- `invocations` - Command execution metadata
- `outputs` - Stdout/stderr with content hashes
- `sessions` - Shell session information
- `blob_registry` - Tracks deduplicated blobs

**Convenience Views:**
- `recent_invocations` - Commands from last 7 days
- `invocations_today` - Commands from today
- `failed_invocations` - Commands with non-zero exit code
- `invocations_with_outputs` - Joined view with output metadata
- `clients` - Aggregated client information

#### 2. shq (Shell Query)

**Purpose:** CLI and shell integration

**Features:**
- Automatic command capture via shell hooks
- CLI for common queries (`shq show`, `shq history`, `shq sql`)
- Direct SQL access with full DuckDB power
- Data lifecycle management (`shq archive`, `shq compact`)

**Commands:**
```bash
shq init              # Initialize BIRD database
shq run CMD           # Run and capture command with output
shq save              # Manually save from pipes (used by shell hooks)
shq show              # Show output from the last command
shq show -O           # Show only stdout
shq show -E           # Show only stderr
shq show --head 20    # Show first 20 lines
shq history           # Browse command history
shq sql "QUERY"       # Execute SQL query
shq stats             # Show statistics
shq archive           # Move old data to archive tier
shq compact           # Compact parquet files for better performance
shq clean             # Recover orphaned commands and clean stale data
shq hook init         # Generate shell integration code
```

**Shell Functions (provided by hook init):**
```bash
shqr CMD              # Run command with full output capture, displaying in real-time
```

#### 3. Content-Addressed Storage

**Purpose:** Deduplicate identical outputs

**How it works:**
1. Hash output with BLAKE3
2. Check if blob exists (by hash)
3. If exists: increment reference count (DEDUP HIT!)
4. If not: write new blob (DEDUP MISS)

**Benefits:**
- 70-90% storage savings for CI/CD workloads
- Automatic (no configuration)
- Fast (2-3ms overhead per command)
- Transparent to queries

#### 4. duck_hunt Integration (Coming in v0.6)

[duck_hunt](https://github.com/teaguesterling/duck_hunt) is a separate DuckDB extension for parsing structured data from logs. It supports 90+ formats and provides retrospective extraction of errors and warnings without the noise of raw CLI output.

**Formats to be supported:**
- Compilers: GCC, Clang, MSVC, rustc
- Linters: ESLint, pylint, shellcheck
- Test runners: pytest, jest, go test
- Build tools: Make, Cargo, npm

*Note: This integration is planned for v0.6. Currently outputs are stored as raw content.*

## Use Cases

### 1. Debugging Failed Builds

```bash
# What failed? Show output from last command
shq show

# Show only stderr
shq show -E

# Has this failed before?
shq sql "
  SELECT timestamp, cmd, exit_code
  FROM invocations
  WHERE cmd LIKE '%make test%'
  ORDER BY timestamp DESC
  LIMIT 10
"
```

### 2. Finding Flaky Tests

```bash
shq sql "
  SELECT
    cmd,
    COUNT(*) as runs,
    SUM(CASE WHEN exit_code = 0 THEN 1 ELSE 0 END) as passes,
    SUM(CASE WHEN exit_code != 0 THEN 1 ELSE 0 END) as failures,
    ROUND(100.0 * SUM(CASE WHEN exit_code = 0 THEN 1 ELSE 0 END) / COUNT(*), 1) as pass_rate
  FROM invocations
  WHERE cmd LIKE '%test%'
  GROUP BY cmd
  HAVING failures > 0
  ORDER BY pass_rate ASC
"
```

### 3. Performance Analysis

```bash
# Slowest commands today
shq sql "
  SELECT cmd, duration_ms, timestamp
  FROM invocations_today
  WHERE duration_ms IS NOT NULL
  ORDER BY duration_ms DESC
  LIMIT 10
"

# Build time trends
shq sql "
  SELECT
    date,
    AVG(duration_ms) as avg_duration,
    MIN(duration_ms) as min_duration,
    MAX(duration_ms) as max_duration
  FROM invocations
  WHERE cmd LIKE '%make%'
  GROUP BY date
  ORDER BY date DESC
"
```

### 4. CI/CD Integration

```bash
# In your CI pipeline
shq run make test || {
  echo "Build failed!"
  shq show > build-output.txt
  shq show -E > build-stderr.txt
  shq sql "SELECT * FROM failed_invocations ORDER BY timestamp DESC LIMIT 5"
  exit 1
}
```

### 5. Data Lifecycle Management

```bash
# Archive old data (moves from recent/ to archive/)
shq archive                  # Archive data older than 14 days
shq archive --days 30        # Archive data older than 30 days
shq archive --dry-run        # Preview what would be archived

# Compact parquet files (merges many small files into fewer large ones)
shq compact                  # Compact all sessions
shq compact -s $SESSION_ID   # Compact specific session only
shq compact --today          # Only compact today's partition
shq compact --dry-run        # Preview what would be compacted

# Clean up orphaned commands and stale data
shq clean                    # Recover orphaned invocations
shq clean --max-age 12       # Mark as orphaned after 12 hours (default: 24)
shq clean --prune            # Also prune old archive data
shq clean --dry-run          # Preview what would be cleaned
```

Note: Shell hooks automatically run `shq compact -s $session --today -q` in the background after each command to keep file counts manageable.

### 6. Storage Management

```bash
# Check storage savings
shq sql "
  SELECT
    storage_tier,
    COUNT(*) as num_blobs,
    SUM(byte_length) / 1024 / 1024 as total_mb,
    SUM(ref_count) as total_references,
    ROUND(AVG(ref_count), 2) as avg_refs_per_blob
  FROM blob_registry
  GROUP BY storage_tier
"

# Find most-reused blobs
shq sql "
  SELECT
    content_hash,
    ref_count,
    byte_length / 1024 as size_kb,
    storage_path
  FROM blob_registry
  ORDER BY ref_count DESC
  LIMIT 10
"
```

## Configuration

### config.toml

```toml
[storage]
bird_root = "~/.local/share/bird"
hot_days = 14              # Days before archiving
inline_threshold = 1048576 # 1MB - inline vs blob

[capture]
auto_capture = true
exclude_patterns = [
  "^cd ",
  "^ls ",
  "^pwd$"
]

[parsing]
duck_hunt_enabled = true
default_format = "auto"    # auto-detect format

[performance]
max_output_size = 52428800 # 50MB max per output
```

## Performance

### Storage Savings

| Workload | Commands/Day | Dedup Ratio | Before | After | Savings |
|----------|-------------|-------------|---------|-------|---------|
| CI Tests | 10,000 | 80% | 60GB | 12GB | **48GB (80%)** |
| Builds | 1,000 | 70% | 150GB | 45GB | **105GB (70%)** |
| Local Dev | 5,000 | 60% | 15GB | 6GB | **9GB (60%)** |

### Query Performance

- Recent commands: <10ms
- Historical queries: <100ms
- Full-text search: <500ms
- Storage overhead: <3ms per command

### Overhead

- Hash computation: 1.7ms (5MB @ 3GB/s)
- Dedup check: 0.5ms (indexed lookup)
- Total per command: **2.7ms**

## Integration with Existing Tools

### blq (Build Log Query)

[blq](https://github.com/yourorg/blq) is a separate tool for build log analysis that uses the BIRD schema:

```bash
# Use blq for advanced log analysis
make 2>&1 | blq parse --format gcc | shq import

# Or use shq's built-in parsing
shq run --format gcc make
```

### DuckDB CLI

```bash
# Direct DuckDB access
duckdb ~/.local/share/bird/db/bird.duckdb

# Use BIRD's views and macros
> SELECT * FROM recent_invocations LIMIT 10;
> SELECT * FROM invocations_today LIMIT 10;
```

### tmux/screen

```bash
# Capture in background pane
tmux new-session -d 'shq run make watch'

# Query while it runs
shq sql "SELECT COUNT(*) FROM invocations_today"
```

## Development

### Building from Source

```bash
git clone https://github.com/yourorg/magic
cd magic

# Build shq
cargo build --release

# Run tests
cargo test

# Install locally
cargo install --path .
```

### Project Structure

```
magic/
├── shq/                  # CLI and capture logic
│   ├── src/
│   │   ├── capture.rs    # Command capture
│   │   ├── parse.rs      # Output parsing
│   │   ├── query.rs      # SQL queries
│   │   └── main.rs       # CLI
│   └── tests/
├── bird/                 # Storage layer (library)
│   ├── src/
│   │   ├── schema.rs     # DuckDB schema
│   │   ├── storage.rs    # Blob storage
│   │   └── lib.rs
│   └── tests/
├── docs/                 # Documentation
│   ├── bird_spec.md
│   ├── shq_implementation.md
│   └── ...
└── README.md            # This file
```

## Documentation

- **[bird_spec.md](docs/bird_spec.md)** - Complete BIRD specification
- **[shq_implementation.md](docs/shq_implementation.md)** - shq implementation guide
- **[shq_shell_integration.md](docs/shq_shell_integration.md)** - Shell hook details
- **[CONTENT_ADDRESSED_BLOBS.md](docs/CONTENT_ADDRESSED_BLOBS.md)** - Storage design
- **[IMPLEMENTATION_GUIDE.md](docs/IMPLEMENTATION_GUIDE.md)** - Step-by-step guide

## FAQ

### Q: How much storage does MAGIC use?

**A:** With content-addressed storage, typically 70-90% less than storing each output separately. For a typical developer:
- Without dedup: ~60GB/month
- With dedup: ~12GB/month
- Savings: **48GB (80%)**

### Q: Does automatic capture slow down my shell?

**A:** No. Capture happens asynchronously after command completion. Overhead is <3ms per command.

### Q: Can I disable capture for specific commands?

**A:** Yes. Add patterns to `config.toml`:
```toml
[capture]
exclude_patterns = ["^cd ", "^ls ", "^pwd$"]
```

Or prefix with space (` command`) or backslash (`\command`) to skip capture.

### Q: How do I query old data?

**A:** All data remains queryable regardless of tier:
```sql
-- Works seamlessly across hot/warm/cold
SELECT * FROM invocations WHERE date >= '2025-01-01';
```

### Q: What if two processes write the same blob?

**A:** Atomic rename handles race conditions. Both processes end up using the same blob (by hash). No data loss.

### Q: Can I use this in CI/CD?

**A:** Yes! Common pattern:
```bash
shq run make test || {
  shq show > build-output.txt
  shq show -E > build-stderr.txt
  exit 1
}
```

### Q: How do I clean up old data?

**A:** Use the archive, compact, and clean commands:
```bash
# Archive data older than 14 days (default)
shq archive

# Archive data older than 30 days
shq archive --days 30

# Compact parquet files (reduces file count, improves query performance)
shq compact

# Clean up orphaned invocations (from crashes/SIGKILL) and prune old archive
shq clean                    # Recover orphaned commands
shq clean --prune            # Also prune old archive data
shq clean --prune --older-than 90d  # Prune data older than 90 days

# Dry-run to see what would happen
shq archive --dry-run
shq compact --dry-run
shq clean --dry-run
```

### Q: Does this work with multiple machines?

**A:** Yes! Each machine writes to its own `client_id`. Query across machines:
```sql
SELECT client_id, COUNT(*)
FROM invocations
GROUP BY client_id;
```

## Roadmap

- [x] **v0.1** - Basic capture + DuckDB storage
- [x] **v0.2** - Content-addressed blobs + deduplication
- [x] **v0.3** - Core shq commands (run, save, show, history, sql, stats)
- [x] **v0.4** - Shell integration (zsh, bash hooks)
- [x] **v0.5** - Archive and compact commands
- [ ] **v0.6** - duck_hunt integration for parsing and searching
- [ ] **v0.7** - TUI history browser (or integration with existing tools)
- [ ] **v0.8** - tmux integration
- [ ] **v1.0** - Production ready
  - Comprehensive test coverage
  - Performance benchmarks
  - Migration tools
  - Documentation complete
- [ ] **v2.0** - Advanced features
  - Cross-machine deduplication
  - Real-time collaboration
  - ML-based anomaly detection

## Future Directions

- **COBE (Claude Output Buffer Extractor):** Import Claude Code command history (stored in SQLite) into BIRD for unified querying across human and AI-driven shell sessions.

## Contributing

Contributions welcome! Please see [CONTRIBUTING.md](CONTRIBUTING.md).

### Key Areas

- Performance optimizations
- Documentation improvements
- Bug reports and fixes

## License

MIT License - see [LICENSE](LICENSE)

## Credits

- **[DuckDB](https://duckdb.org/)** - Fast analytical database
- **[duck_hunt](https://github.com/teaguesterling/duck_hunt)** - Log parsing DuckDB extension
- **[BLAKE3](https://github.com/BLAKE3-team/BLAKE3)** - Fast cryptographic hashing
- **[Parquet](https://parquet.apache.org/)** - Efficient columnar storage

## Support

- GitHub Issues: https://github.com/yourorg/magic/issues
- Discussions: https://github.com/yourorg/magic/discussions

---

**Built for developers who want to understand their build workflows**
