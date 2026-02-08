# Rich Prompt Indicator (Future Feature)

## Overview

Enhance the prompt indicator to display detailed status information about the last command capture using a single character with semantic coloring.

## Current Implementation (v0.1.0)

Simple grey indicators:
- `●` (grey) - active, recording commands
- `⏸` (grey) - inactive, shq available but not recording
- No indicator - shq not loaded

## Proposed Enhancement

### Symbol Semantics (Output Capture)

Use half-circle/circle semantics to indicate what output was captured:

| Symbol | Meaning |
|--------|---------|
| `○` | No output captured (command had no stdout/stderr, or regular command without `shqr`) |
| `●` | Captured both stdout and stderr (separate streams) |
| `◐` | Captured stdout only (left half filled) |
| `◑` | Captured stderr only (right half filled) |
| `◉` | Merged mode (stdout+stderr combined into single stream) |

### Color Semantics (Exit Status)

| Color | Meaning |
|-------|---------|
| Cyan (`\e[36m` / `%F{cyan}`) | Exit code 0 (success) |
| Yellow (`\e[33m` / `%F{yellow}`) | Non-zero exit code (failure/error) |
| Grey (`\e[90m` / `%F{242}`) | No command run yet, or inactive |

### Style Semantics (Events)

| Style | Meaning |
|-------|---------|
| Normal | No events extracted |
| **Bold** | Events were extracted from output |

### Inactive/Paused State

| Symbol | Color | Meaning |
|--------|-------|---------|
| `⏸` | Grey | shq loaded but inactive (use `shq-on` to activate) |

## Examples

| Indicator | Meaning |
|-----------|---------|
| `●` (cyan) | Last command succeeded, captured both streams |
| `●` (cyan, bold) | Last command succeeded, captured both streams, events extracted |
| `◐` (yellow) | Last command failed, only stdout captured |
| `◑` (cyan) | Last command succeeded, only stderr captured |
| `◉` (yellow, bold) | Last command failed, merged mode, events extracted |
| `○` (cyan) | Last command succeeded, no output to capture |
| `○` (grey) | Active but no command run yet |
| `⏸` (grey) | Inactive mode |

## Implementation Notes

### Tracking State

The hook needs to track:
- `__shq_last_exit` - exit code of last command
- `__shq_last_has_stdout` - whether stdout had content (file size > 0)
- `__shq_last_has_stderr` - whether stderr had content
- `__shq_last_merged` - whether `--merge` flag was used
- `__shq_last_has_events` - whether events were extracted

### Detecting Events

Options:
1. Have `shq extract` output a parseable line like `EVENTS:5`
2. Have `shq extract` set exit code based on whether events found
3. Query `shq info` after extract to check event count

### PS1 Update Function

```bash
__shq_update_indicator() {
    local symbol color bold=""

    if [[ -z "$__shq_loaded" ]]; then
        SHQ_INDICATOR='\[\e[90m\]⏸\[\e[0m\] '
        return
    fi

    if [[ -z "$__shq_last_exit" ]]; then
        SHQ_INDICATOR='\[\e[90m\]○\[\e[0m\] '
        return
    fi

    # Determine symbol
    if [[ "$__shq_last_merged" == "1" ]]; then
        symbol="◉"
    elif [[ "$__shq_last_has_stdout" == "1" && "$__shq_last_has_stderr" == "1" ]]; then
        symbol="●"
    elif [[ "$__shq_last_has_stdout" == "1" ]]; then
        symbol="◐"
    elif [[ "$__shq_last_has_stderr" == "1" ]]; then
        symbol="◑"
    else
        symbol="○"
    fi

    # Determine color
    if [[ "$__shq_last_exit" == "0" ]]; then
        color="36"  # cyan
    else
        color="33"  # yellow
    fi

    # Bold if events extracted
    if [[ "$__shq_last_has_events" == "1" ]]; then
        bold="1;"
    fi

    SHQ_INDICATOR="\[\e[${bold}${color}m\]${symbol}\[\e[0m\] "
    PS1="${SHQ_INDICATOR}${__shq_orig_ps1}"
}
```

## Integration with shqr

The `shqr` function already captures stdout/stderr to temp files. After the command completes:

```bash
__shq_last_exit=$exit_code
__shq_last_has_stdout=$([[ -s "$tmpdir/stdout" ]] && echo 1 || echo 0)
__shq_last_has_stderr=$([[ -s "$tmpdir/stderr" ]] && echo 1 || echo 0)
# Check for events from extract output
__shq_update_indicator
```

## Regular Commands (No Capture)

When a regular command is run (not via `shqr`), the indicator shows `○` (empty circle) since no output was captured, but still colored by exit status.

## ZSH Implementation

ZSH uses different escape sequences:
- Color: `%F{cyan}`, `%F{yellow}`, `%F{242}`
- Bold: `%B...%b`
- Reset: `%f`

```zsh
__shq_update_indicator() {
    local symbol color bold=""
    # ... similar logic ...

    if [[ "$__shq_last_has_events" == "1" ]]; then
        bold="%B"
        endbold="%b"
    fi

    SHQ_INDICATOR="${bold}%F{${color}}${symbol}%f${endbold} "
    PS1="${SHQ_INDICATOR}${__shq_orig_ps1}"
}
```

## Priority

This is a nice-to-have enhancement. The MVP uses simple grey indicators which are sufficient for basic awareness of shq state.
