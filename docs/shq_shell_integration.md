# shq Shell Integration

This document describes how shq integrates with your shell for automatic command capture.

## Overview

The shell integration provides:
- **Automatic capture** of every command via shell hooks
- **Zero-config** after initial setup (one-time)
- **Non-intrusive** error handling (never breaks your shell)
- **Minimal overhead** (<5ms per command)

## Installation

### One-Time Setup

```bash
# Add to ~/.zshrc (zsh)
eval "$(shq hook init)"

# Or for bash, add to ~/.bashrc
eval "$(shq hook init --shell bash)"
```

On first shell startup, this:
1. Generates unique session ID for this shell instance
2. Installs precmd/preexec hooks (zsh) or DEBUG trap + PROMPT_COMMAND (bash)
3. Sets up error logging (stderr redirected to `$BIRD_ROOT/errors.log`)
4. Defines `shqr()` helper function for running commands with output capture

### What Gets Installed

```bash
# Hook functions (injected into your shell)
__shq_preexec() { ... }   # Runs before each command
__shq_precmd() { ... }    # Runs after each command
```

## How It Works

### Command Lifecycle

```
User types: make test
    ↓
1. preexec hook captures command text and start time
    ↓
2. Command executes normally (output NOT captured by default)
    ↓
3. precmd hook captures exit code and calculates duration
    ↓
4. Background process forks off (non-blocking)
    │
    ├─→ shq save writes command metadata to parquet
    │
    └─→ shq compact checks if session needs compaction
    ↓
5. Shell prompt returns immediately
```

**Note:** Default hooks capture command metadata only, not output. Use `shqr` or `shq run` to capture full output.

### Hook Implementation (zsh)

```bash
# Session ID based on this shell's PID (stable across commands)
__shq_session_id="zsh-$$"

# Capture command before execution
__shq_preexec() {
    __shq_last_cmd="$1"
    __shq_start_time=$EPOCHREALTIME
}

# Capture result after execution (metadata only - no output capture)
__shq_precmd() {
    local exit_code=$?
    local cmd="$__shq_last_cmd"

    # Reset for next command
    __shq_last_cmd=""

    # Skip if no command (empty prompt)
    [[ -z "$cmd" ]] && return

    # Skip if command starts with space (privacy escape)
    [[ "$cmd" =~ ^[[:space:]] ]] && return

    # Calculate duration in milliseconds
    local duration=0
    if [[ -n "$__shq_start_time" ]]; then
        duration=$(( (EPOCHREALTIME - __shq_start_time) * 1000 ))
        duration=${duration%.*}  # Truncate decimals
    fi
    __shq_start_time=""

    # Save to BIRD and check compaction (async, non-blocking)
    (
        shq save -c "$cmd" -x "$exit_code" -d "$duration" \
            --session-id "$__shq_session_id" \
            --invoker-pid $$ \
            --invoker zsh \
            </dev/null \
            2>> "${BIRD_ROOT:-$HOME/.local/share/bird}/errors.log"
        # Quick compaction check for this session (today only, quiet)
        shq compact -s "$__shq_session_id" --today -q \
            2>> "${BIRD_ROOT:-$HOME/.local/share/bird}/errors.log"
    ) &!
}

# Run command with full output capture
# Usage: shqr <command> [args...]
shqr() {
    local cmd="$*"
    local tmpdir=$(mktemp -d)
    local stdout_file="$tmpdir/stdout"
    local stderr_file="$tmpdir/stderr"
    local start_time=$EPOCHREALTIME

    # Run command, capturing output while still displaying to terminal
    { eval "$cmd" } > >(tee "$stdout_file") 2> >(tee "$stderr_file" >&2)
    local exit_code=${pipestatus[1]:-$?}

    # Calculate duration
    local duration=$(( (EPOCHREALTIME - start_time) * 1000 ))
    duration=${duration%.*}
    duration=${duration:-0}

    # Save to BIRD with captured output
    shq save -c "$cmd" -x "$exit_code" -d "$duration" \
        -o "$stdout_file" -e "$stderr_file" \
        --session-id "$__shq_session_id" \
        --invoker-pid $$ \
        --invoker zsh \
        2>> "${BIRD_ROOT:-$HOME/.local/share/bird}/errors.log"

    # Cleanup
    rm -rf "$tmpdir"

    # Quick compaction check (background, quiet)
    (shq compact -s "$__shq_session_id" --today -q \
        2>> "${BIRD_ROOT:-$HOME/.local/share/bird}/errors.log") &!

    return $exit_code
}

# Register hooks
autoload -Uz add-zsh-hook
add-zsh-hook preexec __shq_preexec
add-zsh-hook precmd __shq_precmd
```

### Background Compaction

The shell hooks include automatic background compaction to prevent file count growth:

1. After each command is saved, `shq compact` runs with:
   - `-s "$__shq_session_id"` - Only compact files for this session
   - `--today` - Only check today's partition (fast)
   - `-q` - Quiet mode (no output unless compaction occurs)

2. This keeps the recent tier manageable without blocking the shell

3. When the file count for a session exceeds the threshold (default: 50), files are merged into a single compacted file

4. All output is redirected to the error log to avoid cluttering the terminal

## Privacy & Escape Sequences

### Built-In Privacy

Commands NOT captured:
- **Leading space**: ` echo $SECRET` (zsh convention)
- **Leading backslash**: `\curl api.example.com` (explicit skip)
- **Empty commands**: Just pressing Enter

### Disabling Temporarily

To temporarily disable capture, you can unset the hook functions:

```bash
# Disable for this session (zsh)
add-zsh-hook -d precmd __shq_precmd
add-zsh-hook -d preexec __shq_preexec

# Re-enable by sourcing the hook again
eval "$(shq hook init)"
```

## Error Handling

### Design Principle: Never Break the Shell

The shell hook MUST be bulletproof. If shq fails, your shell continues normally.

### Error Log

All hook errors are logged to: `$BIRD_ROOT/errors.log`

```bash
# Example error log entry
[2024-12-30T15:23:45Z] Failed to write parquet: disk full
[2024-12-30T15:24:10Z] Cannot create session directory: permission denied
```

### Viewing Errors

```bash
# View recent errors
tail -20 "${BIRD_ROOT:-$HOME/.local/share/bird}/errors.log"

# View all errors
cat "${BIRD_ROOT:-$HOME/.local/share/bird}/errors.log"

# Clear error log
: > "${BIRD_ROOT:-$HOME/.local/share/bird}/errors.log"
```

## Performance

### Overhead Targets

- **preexec**: <1ms (just variable assignment)
- **precmd**: <2ms (before background fork)
- **Background save**: <50ms (doesn't block prompt)
- **Total perceived overhead**: <2ms

### Optimization Strategies

1. **Async everything**: Fork to background immediately
2. **Batch writes**: If needed, buffer multiple commands
3. **Efficient serialization**: Use binary format internally
4. **Minimal parsing**: Defer expensive operations to query time

### Performance Monitoring

Performance can be monitored by checking the error log and database statistics:

```bash
# Check overall statistics
shq stats

# Query recent capture performance
shq sql "SELECT AVG(duration_ms) as avg_cmd_duration FROM invocations_today"
```

## Hook Behavior Details

### What Gets Captured

```bash
# Simple commands
make test           # ✓ Captured

# Pipelines
cat file | grep x   # ✓ Captured (full pipeline as one command)

# Redirects
ls > output.txt     # ✓ Captured (including redirect)

# Background jobs
./server &          # ✓ Captured (with & suffix)

# Command substitution
echo $(date)        # ✓ Captured (including $(date))

# Aliases
ll                  # ✓ Captured (as "ll", not expanded form)
```

### What Doesn't Get Captured

```bash
# Privacy escapes
 password123        # ✗ Leading space
\secret-command     # ✗ Leading backslash

# Shell built-ins (optional, configurable)
cd /tmp             # ✗ (by default, can enable)
export VAR=value    # ✗ (by default, can enable)

# Empty prompts
[just press Enter]  # ✗ No command
```

### Configuration

```toml
# ~/.config/bird/config.toml

[hook]
enabled = true
capture_builtins = false      # Capture cd, export, etc?
capture_aliases = true         # Capture before or after expansion?
min_duration_ms = 0            # Only capture if duration > N ms
error_indicator = true         # Show ⚠ in prompt on errors
async_timeout_ms = 5000        # Kill background save if takes >5s
```

## Multi-Shell Support

### Current: zsh

Full support with precmd/preexec hooks.

### Future: bash

Bash doesn't have `preexec`, need to use `DEBUG` trap:

```bash
__shq_bash_debug() {
    [[ "$BASH_COMMAND" == "__shq_"* ]] && return
    __shq_last_cmd="$BASH_COMMAND"
}
trap '__shq_bash_debug' DEBUG
```

### Future: fish

Fish has event handlers:

```fish
function __shq_preexec --on-event fish_preexec
    set -g __shq_last_cmd $argv
end
```

## Troubleshooting

### Hook Not Working

```bash
# Check if hook is installed
type __shq_precmd
# Should show function definition

# Check if registered
echo $precmd_functions
# Should include __shq_precmd

# Reinstall
eval "$(shq hook init --force)"
```

### Slow Prompt

```bash
# If precmd is slow (>5ms):
# 1. Check disk I/O (is disk full?)
df -h

# 2. Check error log size (rotate if huge)
ls -la "${BIRD_ROOT:-$HOME/.local/share/bird}/errors.log"

# 3. Disable temporarily
add-zsh-hook -d precmd __shq_precmd
add-zsh-hook -d preexec __shq_preexec
```

### Checking for Errors

```bash
# Check error log location
echo "${BIRD_ROOT:-$HOME/.local/share/bird}/errors.log"

# View recent errors
tail -20 "${BIRD_ROOT:-$HOME/.local/share/bird}/errors.log"
```

## Advanced: Custom Integration

### Integration with Existing Prompts

```bash
# If you have a custom precmd:
my_precmd() {
    # Your existing code
    update_git_info
    update_virtual_env
}

# Add shq to the chain:
precmd_functions=(my_precmd __shq_precmd)
```

### Integration with Other Tools

```bash
# Atuin compatibility
# shq and atuin can coexist (both use hooks)

# direnv compatibility
# Load direnv before shq

# tmux compatibility
# No issues, shq is per-shell
```

## Buffer Mode

When buffer mode is enabled, shell hooks automatically save commands to a rotating buffer instead of permanent storage. This provides "retroactive capture" - you can promote interesting commands to permanent storage after the fact.

### Enabling Buffer Mode

```bash
# Enable buffer mode
shq buffer enable --on

# Disable buffer mode (return to normal capture)
shq buffer enable --off

# Check status
shq buffer status
```

### How Buffer Mode Works

1. At shell startup, hooks check buffer status and cache it in `$__shq_buffer_enabled`
2. When buffer mode is enabled, `shq save --to-buffer` is used instead of `shq save`
3. Commands are saved to `$BIRD_ROOT/buffer/` as metadata + output files
4. Buffer automatically rotates based on configured limits

### Buffer Configuration

The buffer uses these settings (in `config.toml`):

```toml
[buffer]
enabled = false           # Toggled with shq buffer enable
max_entries = 100         # Maximum buffer entries
max_size_mb = 50          # Maximum total buffer size
max_age_hours = 24        # Auto-delete entries older than this
exclude_patterns = [      # Commands never saved to buffer
  "*password*",
  "*passwd*",
  "*secret*",
  "*credential*",
  "*token*",
  "*bearer*",
  "*api_key*",
  "*apikey*",
  "*api-key*",
  "*private_key*",
  "*privatekey*",
  "ssh *",
  "ssh-*",
  "gpg *",
  "pass *",
  "vault *",
  "aws sts *",
  "aws secretsmanager *",
  "export *SECRET*",
  "export *TOKEN*",
  "export *KEY*",
  "export *PASSWORD*",
  "printenv",
  "env",
]
```

### Viewing and Promoting Buffer Entries

```bash
# List buffered commands
shq buffer list

# Show output from buffer entry
shq buffer show ~1    # Most recent
shq buffer show ~3    # 3rd most recent

# Promote to permanent storage (not yet implemented)
# shq save ~3

# Clear all buffer entries
shq buffer clear
```

### Security Notes

Buffer mode is designed with security in mind:
- **Disabled by default**: Must be explicitly enabled
- **Extensive exclude patterns**: Sensitive commands (passwords, tokens, SSH, etc.) are never buffered
- **Secure permissions**: Buffer files use 0600 permissions (owner-only)
- **Automatic rotation**: Old entries are deleted based on age/size limits

## Security Considerations

### Sensitive Commands

Best practices:
- Use leading space for passwords: ` export API_KEY=secret`
- Use backslash for API calls: `\curl -H "Authorization: $TOKEN" api.example.com`
- Or disable temporarily: `shq hook disable`
- Enable buffer mode with exclude patterns for sensitive commands

### Multi-User Systems

- Each user has own `$BIRD_ROOT` (default: `~/.local/share/bird`)
- No cross-user data leakage
- File permissions: 700 (user-only)

### Log Rotation

Error log can grow large. Rotate periodically:

```bash
# In crontab - rotate weekly
0 0 * * 0 mv "${HOME}/.local/share/bird/errors.log" "${HOME}/.local/share/bird/errors.log.old" 2>/dev/null

# Or clear if not needed
0 0 * * 0 : > "${HOME}/.local/share/bird/errors.log"
```

---

*Part of the MAGIC ecosystem* 🏀
