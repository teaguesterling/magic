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
# Add to ~/.zshrc
eval "$(shq hook init)"
```

On first shell startup, this:
1. Creates `$BIRD_ROOT` directory structure
2. Initializes `bird.duckdb` (on first query, not at hook init)
3. Installs precmd/preexec hooks
4. Sets up error logging

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
1. preexec hook captures command text
    ↓
2. Command executes normally
    ↓
3. precmd hook captures exit code
    ↓
4. shq save (async, background)
    ↓
5. BIRD parquet written
```

### Hook Implementation

```bash
__shq_preexec() {
    # Capture command text before execution
    __shq_last_cmd="$1"
    __shq_start_time=$EPOCHREALTIME
}

__shq_precmd() {
    local exit=$?
    local cmd="$__shq_last_cmd"
    
    # Reset for next command
    __shq_last_cmd=""
    
    # Skip if no command (e.g., empty prompt)
    [[ -z "$cmd" ]] && return
    
    # Skip if command starts with space (privacy escape)
    [[ "$cmd" =~ ^[[:space:]] ]] && return
    
    # Skip if command starts with backslash (explicit skip)
    [[ "$cmd" =~ ^\\ ]] && return
    
    # Calculate duration
    local duration=0
    if [[ -n "$__shq_start_time" ]]; then
        duration=$(( (EPOCHREALTIME - __shq_start_time) * 1000 ))
        duration=${duration%.*}  # Truncate decimals
    fi
    
    # Capture to BIRD (async, non-blocking)
    # Important: Fork to background with &!
    # Important: Redirect stderr to error log
    (
        shq save \
            --cmd "$cmd" \
            --exit "$exit" \
            --duration "$duration" \
            2>> "${BIRD_ROOT:-$HOME/.local/share/bird}/errors.log"
    ) &!
    
    # Check for recent errors (non-blocking check)
    __shq_check_errors
}

# Register hooks
preexec_functions+=(__shq_preexec)
precmd_functions+=(__shq_precmd)
```

## Privacy & Escape Sequences

### Built-In Privacy

Commands NOT captured:
- **Leading space**: ` echo $SECRET` (zsh convention)
- **Leading backslash**: `\curl api.example.com` (explicit skip)
- **Empty commands**: Just pressing Enter

### Disabling Temporarily

```bash
# Disable for this session
shq hook disable

# Re-enable
shq hook enable

# Check status
shq hook status
```

## Error Handling

### Design Principle: Never Break the Shell

The shell hook MUST be bulletproof. If shq fails, your shell continues normally.

### Error Log

All hook errors logged to: `$BIRD_ROOT/errors.log`

```bash
# Example error log entry
[2024-12-30T15:23:45Z] Failed to write parquet: disk full
[2024-12-30T15:24:10Z] Cannot create session directory: permission denied
```

### Visual Error Indicator

When errors occur, a subtle indicator appears in your prompt:

```bash
# Normal prompt
❯ 

# With recent errors (last 5 minutes)
❯ ⚠

# After user acknowledges
❯ 
```

Implementation:

```bash
__shq_check_errors() {
    # Only check once per minute (avoid overhead)
    local now=$(date +%s)
    local last_check=${__shq_last_error_check:-0}
    [[ $((now - last_check)) -lt 60 ]] && return
    
    __shq_last_error_check=$now
    
    # Check if errors in last 5 minutes
    local error_log="${BIRD_ROOT:-$HOME/.local/share/bird}/errors.log"
    [[ ! -f "$error_log" ]] && return
    
    local recent_errors=$(tail -100 "$error_log" 2>/dev/null | \
        awk -v cutoff="$(date -d '5 minutes ago' +%s)" '
            /^\[/ {
                gsub(/[^0-9]/, "", $1)
                if ($1 > cutoff) print
            }
        ' | wc -l)
    
    if [[ $recent_errors -gt 0 ]]; then
        __shq_has_errors=1
    else
        __shq_has_errors=0
    fi
}

# Add to prompt (optional, user preference)
__shq_prompt_indicator() {
    [[ "${__shq_has_errors:-0}" -eq 1 ]] && echo "⚠"
}

# User adds to their prompt:
# PS1='$(git_prompt)$(shq_prompt_indicator) ❯ '
```

### Viewing Errors

```bash
# Show recent errors
shq errors

# Show all errors
shq errors --all

# Clear error log
shq errors --clear

# Example output:
# Recent errors (last 24 hours):
# 
# [2024-12-30 15:23:45] Failed to write parquet: disk full
# [2024-12-30 15:24:10] Cannot create directory: permission denied
# 
# Fix: Check disk space with 'df -h'
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

```bash
# Show hook statistics
shq stats hook

# Example output:
# Hook Performance (last 1000 commands):
# 
# Avg precmd latency:  1.2ms
# Max precmd latency:  4.8ms
# Background save avg: 23ms
# Background save max: 156ms
# 
# Failed captures:     2 (0.2%)
# Error log entries:   2
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
# Check hook performance
shq stats hook

# If precmd is slow (>5ms):
# 1. Check disk I/O (is disk full?)
# 2. Check error log size (rotate if huge)
# 3. Disable temporarily: shq hook disable
```

### Errors Not Appearing

```bash
# Check error log location
echo $BIRD_ROOT/errors.log

# Manually check log
tail -20 $BIRD_ROOT/errors.log

# Test error detection
shq errors --test
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

## Security Considerations

### Sensitive Commands

Best practices:
- Use leading space for passwords: ` export API_KEY=secret`
- Use backslash for API calls: `\curl -H "Authorization: $TOKEN" api.example.com`
- Or disable temporarily: `shq hook disable`

### Multi-User Systems

- Each user has own `$BIRD_ROOT` (default: `~/.local/share/bird`)
- No cross-user data leakage
- File permissions: 700 (user-only)

### Log Rotation

Error log can grow large. Rotate periodically:

```bash
# In crontab
0 0 * * 0 $HOME/.local/bin/shq errors --rotate
```

Or configure automatic rotation:

```toml
[hook]
error_log_max_size = "10M"    # Rotate when >10MB
error_log_keep_count = 3       # Keep last 3 rotated logs
```

---

*Part of the MAGIC ecosystem* 🏀
