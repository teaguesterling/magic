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
shq history

# View output from last command
shq show
```

## Basic Usage

### View Command History

```bash
# Show last 20 commands (default)
shq history

# Show last 50 commands
shq history -n 50
```

### View Command Output

```bash
# Show output from last command
shq show

# Show only stdout
shq show -O

# Show only stderr
shq show -E

# Show both combined to stdout
shq show -A
```

### Run with Capture

```bash
# Run a command and capture it
shq run make test

# Run a shell command
shq run -c "echo hello && echo world"
```

### SQL Queries

```bash
# Direct SQL access
shq sql "SELECT cmd, exit_code FROM invocations LIMIT 10"

# Find failed commands
shq sql "SELECT * FROM failed_invocations LIMIT 5"

# Today's commands
shq sql "SELECT * FROM invocations_today"
```

### Statistics

```bash
# Show database statistics
shq stats
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

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `BIRD_ROOT` | `~/.local/share/bird` | Base directory for BIRD data |

## Privacy

Commands starting with a space or backslash are not captured:

```bash
# Not captured (leading space)
 echo "secret password"

# Not captured (leading backslash)
\curl -H "Authorization: $TOKEN" api.example.com

# Captured normally
echo "public command"
```

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
