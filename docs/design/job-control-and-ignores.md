# Job Control and Command Ignore System

## Problem Statement

### 1. Ctrl-Z/Job Control Broken in `shq run`

When running `shq run <command>` or `%run <command>`:
- Ctrl-Z stops `shq` itself, not the inner command
- The inner command terminates unexpectedly
- `bg`/`fg` don't work as expected

Root cause: `shq run` uses Rust's `Command::output()` which:
- Blocks synchronously waiting for child completion
- Keeps shq as the foreground process
- Child is in same process group, so SIGTSTP affects both
- When shq stops, it can't continue reading child output

### 2. Command Ignore System Too Rigid

Current `__shq_is_shq_command()` function:
- Hard-coded regex patterns
- Scattered across multiple hook variants
- Missing obvious ignores (fg, bg, jobs, etc.)
- Too granular (listing individual subcommands)

## Proposed Solutions

### Solution 1: Proper Job Control for `shq run`

**Approach: Give child process terminal control**

The key insight is that shq needs to act more like a shell:
1. Put the child in its own process group
2. Give the child terminal control (foreground)
3. Wait for child with WUNTRACED flag to detect stops
4. Handle stopped/continued states properly

**Key Implementation Points:**

1. **Use `spawn()` not `output()`** - non-blocking child creation

2. **Create new process group for child** using `nix::unistd::setpgid`

3. **Give terminal to child** using `nix::unistd::tcsetpgrp`

4. **Wait with WUNTRACED** to detect when child is stopped

5. **Async output capture** - use threads to read stdout/stderr while monitoring child status

**Signal Flow:**

```
User presses Ctrl-Z
        |
        v
SIGTSTP sent to foreground process group
        |
        v
Child receives SIGTSTP (since it owns terminal)
        |
        v
Child stops, waitpid returns WIFSTOPPED
        |
        v
shq saves partial output, prints "[1]+ Stopped"
        |
        v
shq returns terminal to shell, exits with special status
```

**Challenges:**
- Partial output capture when stopped
- Resuming capture after `fg` (may need shell cooperation)
- May need shq to become a thin wrapper

**Alternative: PTY-based approach**

Use a pseudo-terminal:
1. shq creates a PTY pair
2. Child runs attached to PTY slave
3. shq reads from PTY master
4. Terminal signals pass through naturally
5. Works for interactive programs too

Libraries: `pty` crate, `nix::pty`

### Solution 2: Flexible Command Ignore System

**Environment variable approach (simple, extensible):**

```bash
# Default ignores (colon-separated, like SHQ_EXCLUDE)
: ${SHQ_IGNORE:="shq *:shqr *:blq *:%*:fg:bg:jobs:exit:logout:clear:history"}

__shq_should_ignore() {
    local cmd="$1"
    local IFS=':'
    for pattern in $SHQ_IGNORE; do
        [[ "$cmd" == $pattern ]] && return 0
    done
    return 1
}
```

This:
- Uses same pattern matching as SHQ_EXCLUDE
- Has sensible defaults
- Users can customize via environment
- No config file needed

**Default ignore patterns:**
- `shq *` - All shq commands (they record themselves or are queries)
- `shqr *` - shqr wrapper (records itself)
- `blq *` - blq commands (queries)
- `%*` - All % aliases (expand to shq commands)
- `fg`, `bg`, `jobs` - Job control (noise, cause issues)
- `exit`, `logout` - Session end (nothing useful to record)
- `clear`, `history` - Shell utilities (noise)

## Implementation Plan

### Phase 1: Quick Fixes (v0.1.2)
1. Simplify `__shq_is_shq_command()` to use SHQ_IGNORE pattern
2. Add sensible defaults including fg/bg/jobs
3. Document that `shq run` doesn't support job control yet

### Phase 2: Job Control (v0.2.0)
1. Research PTY vs process group approach
2. Prototype with simple test case
3. Implement async output capture with signal handling
4. Test edge cases:
   - Ctrl-Z during output
   - Multiple bg/fg cycles
   - Nested commands
   - Pipelines

## Testing Scenarios

```bash
# Should work after job control fix:
shq run sleep 60
# Ctrl-Z -> shows "[1]+ Stopped"
bg          # -> resumes in background
fg          # -> brings back to foreground
# Command completes, output captured

# Edge cases:
shq run bash -c "sleep 10; echo done"  # Ctrl-Z mid-script
shq run make -j8                        # Ctrl-Z during parallel build
shq run vim file.txt                    # Interactive (needs PTY)
```

## Decision Points

1. **PTY vs Process Groups**: PTY is more robust for interactive programs but adds complexity. Process groups may be sufficient for most cases.

2. **Partial Recording**: When Ctrl-Z'd, should we:
   - Record nothing until completion?
   - Record partial output with "interrupted" status?
   - Create a "pending" record that's updated on resume?

3. **Shell Integration**: Does `fg` need to know about shq, or can shq handle this transparently?
