# shq MVP Implementation Prompt

You are working on `teaguesterling/magic`, a Rust workspace with two crates: `bird` (storage library) and `shq` (CLI). The project captures shell command history into a DuckDB database and lets users query it with SQL.

The codebase is substantial — ~20 CLI subcommands, dual storage backends, remote sync, event parsing, a query micro-language — but has never been validated end-to-end as a daily-driver tool. Your job is to cut an MVP branch and get the core capture→store→query loop working reliably in bash.

---

## 1. Branch Setup

Create an `mvp` branch from `main`:

```bash
git checkout -b mvp
```

Do NOT delete features from main. Instead:
- Default `shq init` to `--mode duckdb` (change the default in the clap arg from `"parquet"` to `"duckdb"`)
- Make extension loading (`duck_hunt`, `scalarfs`) graceful — if they fail to load, log a warning and continue. Currently `Store::connect()` in `bird/src/store/mod.rs` does hard `LOAD` calls that will error if the extensions aren't installed. Wrap these in a helper that catches the DuckDB error and continues.
- Remove `--extract` and `--compact` flags from the `shq save` call inside the bash hook (the MVP hook should just save, not trigger event extraction or compaction)
- Ensure `shq init --mode duckdb` works from a clean state, creates the database, and `shq sql "SELECT 1"` succeeds

Verify the branch compiles and tests pass:
```bash
cargo build --release
cargo test
```

Document any test failures in a `MVP-BUGS.md` at the repo root. Don't fix them yet — just catalog.

---

## 2. Rewrite the Bash Hook (PS0 + PROMPT_COMMAND + history)

The current bash hook in `shq/src/commands.rs` (the `BASH_HOOK` constant) uses a `DEBUG` trap, which has a fundamental flaw: for pipelines like `ls | grep foo`, `$BASH_COMMAND` only captures the last simple command (`grep foo`), not the full command line the user typed.

Replace the `BASH_HOOK` constant with the implementation below that uses `PS0` for pre-execution timing and `history 1` for the full command line. This requires bash 4.4+ (2016), which is acceptable for MVP.

### New BASH_HOOK

```rust
const BASH_HOOK: &str = r#"# shq shell integration for bash (requires bash 4.4+)
# Add to ~/.bashrc: eval "$(shq hook init --shell bash)"
#
# Privacy escapes (command not recorded):
#   - Start command with a space: " ls -la"
#   - Start command with backslash: "\ls -la"
#
# Temporary disable: export SHQ_DISABLED=1
# Exclude patterns: export SHQ_EXCLUDE="*password*:*secret*"

# --- Guard: only init once per shell ---
[[ -n "$__shq_initialized" ]] && return 0
__shq_initialized=1

# --- Version check ---
if [[ "${BASH_VERSINFO[0]}" -lt 4 ]] || { [[ "${BASH_VERSINFO[0]}" -eq 4 ]] && [[ "${BASH_VERSINFO[1]}" -lt 4 ]]; }; then
    echo "shq: bash 4.4+ required (found ${BASH_VERSION}). Hook not installed." >&2
    return 0
fi

# --- Session ID (stable for this shell instance) ---
__shq_session_id="bash-$$"
__shq_start_ms=""

# --- Helpers ---

# Check if command matches any colon-delimited exclude pattern
__shq_excluded() {
    [[ -z "$SHQ_EXCLUDE" ]] && return 1
    local cmd="$1"
    local IFS=':'
    for pattern in $SHQ_EXCLUDE; do
        # Use bash pattern matching (extglob not required for simple globs)
        if [[ "$cmd" == $pattern ]]; then
            return 0
        fi
    done
    return 1
}

# Check if command is a shq/blq read-only query — don't record these
__shq_is_query() {
    local cmd="$1"
    [[ "$cmd" =~ ^shq\ +(output|show|o|invocations|history|i|info|I|events|e|stats|sql|q|quick-help|\?) ]] && return 0
    [[ "$cmd" =~ ^blq\ +(show|list|errors|context|stats) ]] && return 0
    return 1
}

# Milliseconds since epoch (uses $EPOCHREALTIME from bash 5.0+, falls back to date)
__shq_now_ms() {
    if [[ -n "$EPOCHREALTIME" ]]; then
        local sec="${EPOCHREALTIME%.*}"
        local frac="${EPOCHREALTIME#*.}"
        # Pad or truncate fractional part to 3 digits
        frac="${frac}000"
        echo "${sec}${frac:0:3}"
    else
        date +%s%3N
    fi
}

# --- PS0: fires after command is read, before execution ---
# We use PS0 to record the start time. The trick: PS0 is expanded for
# display, so we use an arithmetic expansion side-effect that evaluates
# to an empty string but sets __shq_start_ms as a side effect.
PS0='$(__shq_ps0_hook)'

__shq_ps0_hook() {
    __shq_start_ms=$(__shq_now_ms)
    # Print nothing — PS0 output appears before command output
}

# --- PROMPT_COMMAND: fires after command completes, before prompt ---
__shq_prompt_command() {
    local exit_code=$?

    # Skip if disabled
    [[ -n "$SHQ_DISABLED" ]] && return

    # Get the full command line from history (solves the pipeline problem)
    local cmd
    cmd=$(HISTTIMEFORMAT='' history 1 | sed 's/^[ ]*[0-9]*[ ]*//')

    # Skip if empty (just pressed Enter, or history didn't record it)
    [[ -z "$cmd" ]] && return

    # Skip if command starts with space (privacy escape)
    # Note: if HISTCONTROL=ignorespace, history 1 won't return it anyway,
    # but check explicitly for HISTCONTROL configs that don't strip it.
    [[ "$cmd" =~ ^[[:space:]] ]] && return

    # Skip if command starts with backslash (privacy escape)
    [[ "$cmd" =~ ^\\ ]] && return

    # Skip if command matches exclude pattern
    __shq_excluded "$cmd" && return

    # Skip shq/blq query commands (prevent recursive recording)
    __shq_is_query "$cmd" && return

    # Calculate duration in milliseconds
    local duration=0
    if [[ -n "$__shq_start_ms" ]]; then
        local now_ms
        now_ms=$(__shq_now_ms)
        duration=$(( now_ms - __shq_start_ms ))
        # Guard against negative durations (clock skew, etc.)
        (( duration < 0 )) && duration=0
    fi
    __shq_start_ms=""

    # Save to BIRD (async, non-blocking — never slow down the prompt)
    (
        shq save -c "$cmd" -x "$exit_code" -d "$duration" \
            --session-id "$__shq_session_id" \
            --invoker-pid $$ \
            --invoker bash \
            -q \
            </dev/null \
            2>> "${BIRD_ROOT:-$HOME/.local/share/bird}/errors.log"
    ) &
    disown 2>/dev/null
}

# --- shqr: run a command with full output capture ---
# Usage: shqr <command> [args...]
# Example: shqr make test
# Unlike the default hook (metadata only), shqr also captures stdout/stderr.
shqr() {
    # Check if disabled
    if [[ -n "$SHQ_DISABLED" ]]; then
        eval "$*"
        return $?
    fi

    local cmd="$*"
    local tmpdir
    tmpdir=$(mktemp -d)
    local stdout_file="$tmpdir/stdout"
    local stderr_file="$tmpdir/stderr"
    local start_ms
    start_ms=$(__shq_now_ms)

    # Run command, tee-ing output to files while displaying to terminal
    { eval "$cmd" ; } > >(tee "$stdout_file") 2> >(tee "$stderr_file" >&2)
    local exit_code=${PIPESTATUS[0]:-$?}

    # Calculate duration
    local now_ms
    now_ms=$(__shq_now_ms)
    local duration=$(( now_ms - start_ms ))
    (( duration < 0 )) && duration=0

    # Save to BIRD with captured output (synchronous — user explicitly asked for capture)
    shq save -c "$cmd" -x "$exit_code" -d "$duration" \
        -o "$stdout_file" -e "$stderr_file" \
        --session-id "$__shq_session_id" \
        --invoker-pid $$ \
        --invoker bash \
        -q \
        2>> "${BIRD_ROOT:-$HOME/.local/share/bird}/errors.log"

    # Cleanup temp files
    rm -rf "$tmpdir"

    return $exit_code
}

# --- Register ---
# Prepend to PROMPT_COMMAND so we capture exit code before other hooks modify it
if [[ -z "$PROMPT_COMMAND" ]]; then
    PROMPT_COMMAND="__shq_prompt_command"
else
    PROMPT_COMMAND="__shq_prompt_command; $PROMPT_COMMAND"
fi
"#;
```

### Key differences from the old hook

| Aspect | Old (DEBUG trap) | New (PS0 + PROMPT_COMMAND) |
|--------|-----------------|---------------------------|
| Command text | `$BASH_COMMAND` (last simple command in pipeline) | `history 1` (full command line as typed) |
| Timing | `$EPOCHREALTIME` in DEBUG trap | `__shq_now_ms` in PS0 hook |
| Pipeline `ls \| grep foo` | Captures `grep foo` ❌ | Captures `ls \| grep foo` ✅ |
| Trigger count | Fires per simple command (multiple per pipeline) | Fires once per prompt cycle |
| bash-preexec dependency | None | None |
| Minimum bash | 4.0 | 4.4 (for PS0) |

### Update the integration test

The existing test in `shq/tests/integration.rs` (`test_hook_init_bash`) checks for `__shq_debug` and `PROMPT_COMMAND`. Update it to check for the new hook structure:

```rust
#[test]
fn test_hook_init_bash() {
    let output = Command::new(env!("CARGO_BIN_EXE_shq"))
        .args(["hook", "init", "--shell", "bash"])
        .output()
        .expect("failed to run hook init");

    assert!(output.status.success());
    let hook = String::from_utf8_lossy(&output.stdout);
    // New PS0-based hook
    assert!(hook.contains("__shq_ps0_hook"), "Should use PS0 hook");
    assert!(hook.contains("__shq_prompt_command"), "Should use PROMPT_COMMAND");
    assert!(hook.contains("history 1"), "Should read from history");
    assert!(hook.contains("PROMPT_COMMAND"), "Should register PROMPT_COMMAND");
    // Privacy features preserved
    assert!(hook.contains("SHQ_DISABLED"), "Should check SHQ_DISABLED");
    assert!(hook.contains("SHQ_EXCLUDE"), "Should support SHQ_EXCLUDE");
    assert!(hook.contains("__shq_excluded"), "Should have exclude function");
    assert!(hook.contains("__shq_is_query"), "Should have query detection");
    // Output capture helper
    assert!(hook.contains("shqr"), "Should define shqr function");
}
```

### Things to watch for during testing

1. **`HISTCONTROL=erasedups`** — If the user runs the same command twice in a row, `history 1` may return nothing the second time. Test this. If it's a problem, fall back to a `__shq_last_cmd` variable set in PS0 (read READLINE_LINE if available in bash 5.1+, otherwise accept the limitation and document it).

2. **`HISTIGNORE`** — Commands matching HISTIGNORE won't appear in `history 1`. This is actually fine — it aligns with the user's existing intent to exclude those commands.

3. **PS0 and other tools** — If the user has an existing PS0, we need to append, not replace. The hook should check and chain:
   ```bash
   if [[ -n "$PS0" ]]; then
       PS0='$(__shq_ps0_hook)'"$PS0"
   else
       PS0='$(__shq_ps0_hook)'
   fi
   ```

4. **`__shq_now_ms` overhead** — On bash <5.0 (no `$EPOCHREALTIME`), this shells out to `date +%s%3N` twice per command. That's ~4ms total, which is within budget but worth noting.

---

## 3. MVP Scope

### In scope (must work reliably)

| Command | What it does | Priority |
|---------|-------------|----------|
| `shq init` | Create DuckDB database at `~/.local/share/bird/` | P0 |
| `shq hook init --shell bash` | Emit the bash hook code | P0 |
| `shq save -c CMD -x EXIT -d DUR ...` | Write an invocation record (called by hook) | P0 |
| `shq i` / `shq invocations` | List recent commands (table format) | P0 |
| `shq o` / `shq output` | Show stdout/stderr of a command | P0 |
| `shq sql "..."` | Run arbitrary SQL against the database | P0 |
| `shq I` / `shq info` | Detailed view of one invocation | P1 |
| `shq R` / `shq rerun` | Re-execute a previous command | P1 |
| `shq run -c CMD` | Run a command with output capture | P1 |
| `shq stats` | Database statistics summary | P2 |
| `shq ?` / `shq quick-help` | Quick reference card | P2 |

### In scope (query micro-language subset)

These patterns must work when passed to `shq i`, `shq o`, `shq I`, `shq R`:

| Pattern | Meaning |
|---------|---------|
| `~N` | Last N commands |
| `%exit<>0` | Non-zero exit code |
| `%/pattern/` | Command regex |
| `%exit<>0~10` | Last 10 failures |
| `%/make/~1` | Last `make` command |

### Out of scope (exists in codebase, don't break it, don't test/fix it)

Everything else. Specifically:
- Parquet storage mode (code stays, not the default, not tested)
- Remote sync (`push`, `pull`, `remote add/list/remove/attach/test/status`)
- Event parsing and extraction (`events`, `extract-events`, `update-extensions`)
- Format hints management (`format-hints list/add/remove/check`)
- Archive tiering (`archive`)
- Compaction (`compact`) — not needed in DuckDB mode
- Source selectors in query language (`shell:`, `*:*:*:*:`)
- Path filters in query language (`.`, `~/path`)
- Tags in query language (`%my-project`)
- Project-level `.bird/` detection and attachment
- Multi-schema union views (cached, remote, unified) — keep local + main
- Nested invocation UUID sharing (`BIRD_INVOCATION_UUID`)
- `duck_hunt` and `scalarfs` extensions (graceful if missing)

### MVP defaults

- Storage mode: **DuckDB** (not Parquet)
- Hook save flags: `shq save -c CMD -x EXIT -d DUR --session-id SID --invoker-pid PID --invoker bash -q` (no `--extract`, no `--compact`)
- Extension loading: **best-effort** (don't fail if missing)

---

## 4. Bash Tutorial Walkthrough

After implementing the above, verify the full end-to-end flow by following this tutorial manually in a bash shell. Every step should work. If any step fails, fix it before proceeding.

### Step 0: Build

```bash
cd /path/to/magic
git checkout mvp
cargo build --release

# Put shq on PATH
export PATH="$PWD/target/release:$PATH"

# Verify
shq --version
```

### Step 1: Initialize

```bash
# Start fresh (remove any previous BIRD data)
rm -rf ~/.local/share/bird

# Initialize with DuckDB mode (should be the default)
shq init

# Verify: should print something like "BIRD initialized at ~/.local/share/bird (mode: duckdb)"
```

#### Verify the database exists

```bash
ls -la ~/.local/share/bird/db/bird.duckdb
# Should exist and be non-empty

shq sql "SELECT 1 AS test"
# Should print:
# test
# ----
# 1

shq sql "SELECT count(*) AS n FROM invocations"
# Should print:
# n
# -
# 0
```

### Step 2: Manual capture (without hook)

Test `shq run` to verify the capture→store→query path before involving the hook:

```bash
shq run -c "echo hello world"
# Should print: hello world

shq i
# Should show 1 invocation with "echo hello world", exit code 0

shq o
# Should print: hello world

shq sql "SELECT cmd, exit_code, duration_ms FROM invocations"
# Should show the echo command with exit_code=0
```

Test a failing command:

```bash
shq run -c "ls /nonexistent/path"
# Should print ls error message and exit non-zero

shq i ~2
# Should show both commands, the ls with non-zero exit code

shq sql "SELECT cmd, exit_code FROM invocations WHERE exit_code != 0"
# Should show the failed ls command
```

### Step 3: Install the bash hook

```bash
# Preview the hook code
shq hook init --shell bash
# Should print the PS0 + PROMPT_COMMAND based hook

# Now actually install it for this session
eval "$(shq hook init --shell bash)"
```

#### Verify hook is active

```bash
# Check PS0 is set
echo "PS0 is: '$PS0'"
# Should contain __shq_ps0_hook

# Check PROMPT_COMMAND
echo "PROMPT_COMMAND is: '$PROMPT_COMMAND'"
# Should contain __shq_prompt_command
```

### Step 4: Capture via hook

Run some real commands. The hook captures metadata only (no stdout/stderr content) — but it records the command, exit code, duration, cwd, and timestamp.

```bash
# Simple commands
echo "captured by hook"
ls -la
pwd
date

# Check they were captured
shq i ~5
# Should show all 4 commands above (plus the shq i itself should NOT appear
# because __shq_is_query skips shq query commands)
```

#### Test pipelines (the reason we rewrote the hook)

```bash
echo hello | tr a-z A-Z | cat
# Should be captured as "echo hello | tr a-z A-Z | cat" (full pipeline)

shq i ~1
# Verify it shows the full pipeline, NOT just "cat"
```

#### Test exit codes

```bash
false
# exit code 1

shq i ~1
# Should show "false" with exit_code=1

grep nonexistent /dev/null
# exit code 1

shq sql "SELECT cmd, exit_code FROM invocations WHERE exit_code != 0 ORDER BY timestamp DESC LIMIT 5"
# Should show both failed commands
```

#### Test duration

```bash
sleep 2

shq I ~1
# Should show duration_ms around 2000 (±100ms)
```

### Step 5: Test privacy escapes

```bash
# Space-prefixed commands should NOT be recorded
 echo "this is secret"
# (note the leading space)

shq i ~1
# Should NOT show "echo this is secret" — should show whatever was before it

# Backslash-prefixed
\echo "also secret"

shq i ~1
# Should NOT show "echo also secret"

# SHQ_DISABLED
export SHQ_DISABLED=1
echo "not captured while disabled"
unset SHQ_DISABLED
echo "this IS captured again"

shq i ~2
# Should show "this IS captured again" and the unset, but NOT "not captured while disabled"

# SHQ_EXCLUDE
export SHQ_EXCLUDE="*password*:*secret*"
echo "my password is hunter2"
echo "normal command"

shq i ~1
# Should show "normal command" but NOT the password one
unset SHQ_EXCLUDE
```

### Step 6: Test shqr (output capture)

The default hook only captures metadata. `shqr` also captures stdout/stderr:

```bash
shqr echo "captured with output"

shq o ~1
# Should print: captured with output

shqr ls /tmp

shq o ~1
# Should print the contents of /tmp

# Stderr capture
shqr ls /nonexistent 2>&1

shq o -E ~1
# Should show the "No such file" error from stderr
```

### Step 7: Query micro-language

```bash
# Run a few varied commands first
shqr make --version 2>/dev/null || true
shqr cargo --version 2>/dev/null || true
echo "just testing"
false

# Last 3 commands
shq i ~3

# Failed commands only
shq i %exit<>0

# Last 5 failed commands
shq i %exit<>0~5

# Commands matching a regex
shq i '%/version/~5'
# Should show the make --version and cargo --version commands

# Output of the last "version" command
shq o '%/version/~1'
```

### Step 8: SQL power queries

Verify that DuckDB's full SQL engine is available:

```bash
# Commands by hour today
shq sql "
  SELECT
    date_part('hour', timestamp) AS hour,
    count(*) AS commands,
    count(*) FILTER (WHERE exit_code != 0) AS failures
  FROM invocations
  WHERE date = CURRENT_DATE
  GROUP BY 1
  ORDER BY 1
"

# Average duration by command
shq sql "
  SELECT
    split_part(cmd, ' ', 1) AS program,
    count(*) AS runs,
    round(avg(duration_ms)) AS avg_ms
  FROM invocations
  GROUP BY 1
  ORDER BY runs DESC
  LIMIT 10
"

# Most recent working directories
shq sql "
  SELECT cwd, count(*) AS n
  FROM invocations
  GROUP BY 1
  ORDER BY n DESC
  LIMIT 5
"
```

### Step 9: Rerun

```bash
# Re-run the last command
shq R -n
# Dry run — should print what it would run

shq R
# Actually re-runs it

# Re-run a specific past command
shq R '%/echo/~1'
# Re-runs the last echo command
```

### Step 10: Stats and info

```bash
shq stats
# Should show invocation count, last command, storage mode, etc.

shq I ~1
# Detailed info about the last command

shq ?
# Quick reference card — verify it's accurate for the MVP commands
```

---

## Verification Checklist

Before tagging v0.1.0-mvp, all of the following must be true:

- [ ] `cargo build --release` succeeds with no errors
- [ ] `cargo test` passes (document any pre-existing failures that aren't MVP-related)
- [ ] `shq init` creates a DuckDB-mode database
- [ ] `shq run -c "echo test" && shq i && shq o` round-trips correctly
- [ ] `eval "$(shq hook init --shell bash)"` installs without errors in bash 4.4+
- [ ] Simple commands captured with correct command text, exit code, and duration
- [ ] Pipelines captured as full command line (not just last component)
- [ ] Privacy escapes work (space-prefix, backslash-prefix, SHQ_DISABLED, SHQ_EXCLUDE)
- [ ] `shq i/o/sql/I/R` all produce correct output
- [ ] `shq i %exit<>0~5` and `shq i '%/pattern/~N'` work
- [ ] No perceptible prompt delay with hook active
- [ ] `shq` query commands are NOT self-recorded
- [ ] Missing DuckDB extensions (duck_hunt, scalarfs) produce warnings, not errors
- [ ] No panics or unhandled errors during the full tutorial walkthrough
