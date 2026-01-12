# Getting Started

This guide walks you through installing and configuring MAGIC for your development workflow.

## Prerequisites

- **Rust toolchain** (for building from source)
- **zsh** or **bash** shell
- **~100MB disk space** for the database

## Installation

### From Source

```bash
# Clone the repository
git clone https://github.com/yourorg/magic
cd magic

# Build and install shq
cargo install --path shq

# Verify installation
shq --version
```

### Initialize BIRD

Before using shq, initialize the BIRD database:

```bash
shq init
```

**Storage Mode Options:**

```bash
# Default: Parquet mode (multi-writer safe, for concurrent shells)
shq init

# DuckDB mode (single-writer, simpler, no compaction needed)
shq init --mode duckdb
```

This creates the directory structure at `~/.local/share/bird/`:

```
~/.local/share/bird/
├── db/
│   └── bird.duckdb           # Main DuckDB database
├── data/
│   ├── recent/               # Hot tier (0-14 days)
│   │   ├── invocations/
│   │   ├── outputs/
│   │   └── sessions/
│   └── archive/              # Cold tier (>14 days)
├── blobs/
│   └── content/              # Content-addressed storage
├── extensions/               # DuckDB extensions
├── sql/                      # Custom SQL files
└── config.toml               # Configuration
```

## Shell Integration

### Zsh

Add to your `~/.zshrc`:

```bash
eval "$(shq hook init)"
```

### Bash

Add to your `~/.bashrc`:

```bash
eval "$(shq hook init --shell bash)"
```

### Reload Your Shell

```bash
# Zsh
source ~/.zshrc

# Bash
source ~/.bashrc
```

## Verify Installation

After reloading your shell, run a few commands:

```bash
# Run some commands
echo "Hello, MAGIC!"
ls -la
make --version

# Check that they were captured
shq i              # or: shq invocations

# View output from last command
shq o              # or: shq output
```

## Quick Reference

Run `shq ?` for a quick reference card showing all commands and query syntax.

## Basic Usage

### View Command History

```bash
shq i              # Last 20 commands (default)
shq i 50           # Last 50 commands
shq i %exit<>0~10  # Last 10 failed commands
shq i %/cargo/~20  # Last 20 cargo commands
```

### View Command Output

```bash
shq o              # Output from last command
shq o 3            # Output from 3rd-last command
shq o -E 1         # Only stderr
shq o -A 1         # Both streams combined
shq o %/make/~1    # Output of last make command
```

### Show Invocation Details

```bash
shq I              # Details about last command (alias: info)
shq I 3            # Details about 3rd-last command
shq I -f json 1    # As JSON
```

### Re-run Previous Commands

```bash
shq R              # Re-run last command (alias: rerun)
shq R 3            # Re-run 3rd-last command
shq R %/make/~1    # Re-run last make command
shq R -n %/test/~1 # Dry-run: show what would run
```

### Run with Capture

```bash
shq r make test           # Run and capture (alias: run)
shq r -c "echo hello"     # Run shell command
```

### SQL Queries

```bash
shq q "SELECT cmd, exit_code FROM invocations LIMIT 10"
shq q "SELECT * FROM failed_invocations LIMIT 5"
shq q "SELECT * FROM invocations_today"
```

### Statistics

```bash
# Show database statistics
shq stats

# JSON output (for scripting)
shq stats --json
```

### Events (Parsed Errors/Warnings)

BIRD automatically parses build output for structured events:

```bash
shq e              # Show events from recent commands
shq e %/cargo/~5   # Events from last 5 cargo commands
shq e -s error     # Only errors (filter by severity)
shq e --count      # Count events by severity
```

### Extract Events

Manually extract/re-extract events from outputs:

```bash
shq extract-events         # Extract from last command
shq extract-events --all   # Backfill all commands without events
shq extract-events -f gcc  # Force specific format
```

### Format Hints

Configure format detection for build tools:

```bash
shq format-hints list              # Show configured hints
shq format-hints add "make*" gcc   # Use gcc parser for make commands
shq format-hints remove "make*"    # Remove a hint
shq format-hints set-default cargo # Set default format for unknown commands
```

## Data Lifecycle

### Archive Old Data

Move data older than 14 days to archive tier:

```bash
# Archive with default settings (14 days)
shq archive

# Archive data older than 30 days
shq archive --days 30

# Preview what would be archived
shq archive --dry-run
```

### Compact Files

Merge small parquet files for better performance:

```bash
# Compact all sessions
shq compact

# Compact specific session
shq compact -s shell-12345

# Preview what would be compacted
shq compact --dry-run
```

!!! note "Automatic Compaction"
    Shell hooks automatically run background compaction after each command, so manual compaction is rarely needed.

## Remote Storage

Sync your command history across machines using remote DuckDB databases:

### Configure Remotes

```bash
# Add a remote (S3, local file, MotherDuck, or PostgreSQL)
shq remote add team --type s3 --uri s3://bucket/bird.duckdb
shq remote add backup --type file --uri /mnt/backup/bird.duckdb --read-only

# List configured remotes
shq remote list

# Test remote connectivity
shq remote test team
```

### Push/Pull Data

```bash
# Push local data to remote
shq push --remote team

# Push only recent data
shq push --remote team --since 7d

# Preview what would be pushed
shq push --remote team --dry-run

# Pull data from remote
shq pull --remote team

# Pull specific client's data
shq pull --remote team --client laptop@user
```

### Remote Types

| Type | URI Format | Description |
|------|------------|-------------|
| `file` | `/path/to/bird.duckdb` | Local or network file |
| `s3` | `s3://bucket/path/bird.duckdb` | S3-compatible storage |
| `motherduck` | `md:database_name` | MotherDuck cloud |
| `postgres` | `postgres:dbname=...` | PostgreSQL database |

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `BIRD_ROOT` | `~/.local/share/bird` | Base directory for BIRD data |

## Privacy & Tracking Control

Multiple ways to exclude commands from capture:

### Caller Opt-Out (Shell-Level)

```bash
# Leading space - not captured
 echo "secret password"

# Leading backslash - not captured
\curl -H "Authorization: $TOKEN" api.example.com
```

### Environment Variables

```bash
# Disable all capture
export SHQ_DISABLED=1

# Exclude by pattern (colon-separated)
export SHQ_EXCLUDE="*password*:*secret*:*token*"
```

### Command Opt-Out (OSC Escape)

Programs can signal they don't want to be tracked:

```bash
# In your script:
printf '\e]shq;nosave\a'  # Disable capture for this command
```

### Auto-Excluded Commands

Query commands (`shq i`, `shq o`, `shq sql`, etc.) are automatically excluded to prevent recursion.

## Troubleshooting

### Commands Not Being Captured

1. Check that hooks are installed:
   ```bash
   type __shq_precmd
   ```

2. Verify BIRD is initialized:
   ```bash
   ls ~/.local/share/bird/db/bird.duckdb
   ```

3. Check the error log:
   ```bash
   cat ~/.local/share/bird/errors.log
   ```

### Slow Shell Startup

The hooks are designed to be lightweight. If you experience slowness:

1. Check if BIRD_ROOT is on a slow filesystem
2. Ensure the database isn't corrupted: `shq sql "SELECT 1"`

### Permission Errors

Ensure you have write access to BIRD_ROOT:

```bash
ls -la ~/.local/share/bird/
```

## Next Steps

- [Shell Integration](shq_shell_integration.md) - Deep dive into how hooks work
- [SQL Queries](sql-queries.md) - Advanced query examples
- [BIRD Specification](bird_spec.md) - Technical architecture details
