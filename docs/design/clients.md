# Cross-Client Architecture & Query Micro-Language

## Overview

BIRD supports querying across multiple data sources (shells, Claude Code, CI, etc.)
using a unified micro-language that combines source selection, filtering, and ranging.

## Concepts

| Term | Description | Examples |
|------|-------------|----------|
| **Host** | Machine/hostname | `laptop`, `server1` |
| **Type** | Category of data source | `shell`, `claude-code`, `ci`, `agent` |
| **Client** | Specific tool within type | `zsh`, `bash`, `magic` (project) |
| **Session** | Instance identifier | PID, conversation UUID, workflow run ID |
| **Tag** | Saved filter/query | `my-project`, `work` |

## Query Syntax

**Full pattern**:
```
[source][path][filters][range]
```

Where each component is optional and detected by syntax.

---

## Source Selectors

**Format**: `host:type:client:session:`

Empty segments mean "current context" (narrowing from right):
```
:                     → current session
::                    → current client (all sessions)
:::                   → current type (all clients)
::::                  → current host (all types)
```

Explicit wildcards with `*`:
```
*:*:*:*:              → everything everywhere
*:shell:*:*:          → all shells on all hosts
laptop:shell:zsh:*:   → all zsh sessions on laptop
laptop:shell:zsh:123: → specific session
```

**Examples**:
```
shell:                → this host, shell type, any client/session
shell:zsh:            → this host, zsh shells
*:shell:              → all shells everywhere
laptop:*:*:*:         → everything on laptop
```

---

## Path Filters (Working Directory)

Paths are detected by leading `.`, `..`, `~/`, or `/`:

```
.                     → current directory
./                    → current directory (same)
./src/                → ./src subdirectory
../                   → parent directory
~/                    → home directory
~/Projects/magic/     → specific home-relative path
/tmp/                 → absolute path
```

**Examples**:
```
.~1                   → last command in current directory
~/Projects/~5         → last 5 commands in ~/Projects
../~3                 → last 3 in parent dir
```

---

## Command Regex

**Format**: `%/pattern/`

```
%/make/               → cmd contains "make"
%/^cargo test/        → cmd starts with "cargo test"
%/\.rs$/              → cmd ends with ".rs"
```

---

## Field Filters

**Format**: `%field<op>value`

**Operators**:
| Op | Meaning |
|----|---------|
| `=` | equals |
| `<>` | not equals (preferred) |
| `!=` | not equals (needs shell escaping) |
| `~=` | regex match |
| `>` | greater than |
| `<` | less than |
| `>=` | greater or equal |
| `<=` | less or equal |

**Fields**:
- `cmd` - command string
- `exit` - exit code
- `cwd` - working directory
- `duration` - execution time (ms)
- `host` - hostname
- `type` - source type
- `client` - client name
- `session` - session ID

**Examples**:
```
%exit<>0              → non-zero exit code
%duration>5000        → commands taking > 5 seconds
%cwd~=/duck_hunt/     → cwd matches pattern
%cmd~=^make           → cmd starts with "make"
```

---

## Tags (Saved Filters)

**Format**: `%tag-name` (no operator) or bare `tag-name`

Tags are saved filter combinations that expand at query time.

```
%my-project           → expand saved filter "my-project"
my-project            → same (bare word = tag fallback)
```

**Defining tags**:
```bash
shq tag add my-project 'shell::%cwd~=/Projects/duck_hunt/'
shq tag add failures '%exit<>0'
shq tag add slow '%duration>10000'
```

---

## Range Selectors

**Format**: `~N` or `~N:~M`

Uses `~` prefix (like git's `HEAD~N`), or bare integers:

```
1 or ~1               → last 1 command
5 or ~5               → last 5 commands
~5:2 or ~5:~2         → from 5th-last to 2nd-last (3 commands)
~10:5                 → commands 10-5 ago (5 commands)
```

**Note**: `~` + digits = range, `~/` = home path (disambiguated by `/`)

---

## Combined Examples

```bash
# Simple
~1                              # Last command (current session)
~5                              # Last 5 commands (current session)

# Source + range
shell:~5                        # Last 5 shell commands (this host)
*:*:*:*:~10                     # Last 10 everywhere
laptop:shell:zsh:~1             # Last zsh command on laptop

# Path + range
.~1                             # Last command in current dir
~/Projects/magic/~5             # Last 5 in that directory

# Filters + range
%exit<>0~10                     # Last 10 failed commands
%/make/~1                       # Last make command
%duration>5000~5                # Last 5 slow commands

# Source + path + range
shell:.~1                       # Shell commands in current dir, last 1

# Source + filter + range
shell:%exit<>0~5                # Last 5 failed shell commands
*:*:*:*:%/cargo test/~10        # Last 10 cargo test runs everywhere

# Tag + range
my-project~1                    # Last command matching saved filter
%failures~10                    # Last 10 using "failures" tag

# Complex
laptop:shell:zsh:%cwd~=/magic/%/make/~3
# On laptop, zsh shells, in magic dirs, make commands, last 3
```

---

## Detection Rules (Parser Priority)

1. `.`, `..` + optional path → **cwd filter**
2. `~/` + path → **cwd filter** (home)
3. `/` + path → **cwd filter** (absolute)
4. `~` + digit(s) → **range**
5. `%/` ... `/` → **cmd regex**
6. `%field<op>value` → **field filter**
7. `%bare-word` → **tag**
8. Contains `:` → **source selector**
9. Bare word → **tag fallback**

---

## Special Values

| Symbol | Meaning |
|--------|---------|
| `*` | Wildcard (matches anything) |
| `:` | Current session (empty source) |
| `::` | Current client |
| `:::` | Current type |
| `::::` | Current host |

---

## Configuration

**Default filter scope** (when using bare `%filter` without source):
```bash
shq config set filter.default-scope ":"       # current session
shq config set filter.default-scope ":::"     # current host (default)
shq config set filter.default-scope "*:*:*:*" # everything
```

---

## External Sources

External data (Claude Code, CI) is accessed via SQL views defined in `~/.bird/sources.sql`:

```sql
CREATE OR REPLACE VIEW claude_code_invocations AS
SELECT
  json_extract_string(message, '$.content[0].id') as id,
  'claude-code' as client_type,
  regexp_extract(filename, '/projects/([^/]+)/', 1) as invoker,
  json_extract_string(message, '$.content[0].input.command') as cmd,
  timestamp::TIMESTAMP as timestamp,
  cwd,
  uuid as session_id
FROM read_json_auto('~/.claude/projects/*/*.jsonl',
                    format='newline_delimited',
                    maximum_object_size=50000000)
WHERE type = 'assistant'
  AND json_extract_string(message, '$.content[0].type') = 'tool_use'
  AND json_extract_string(message, '$.content[0].name') = 'Bash';
```

**Unified view**:
```sql
CREATE OR REPLACE VIEW all_invocations AS
SELECT *, 'shell' as source FROM invocations
UNION ALL
SELECT *, 'claude-code' as source FROM claude_code_invocations;
```

---

## Commands

```bash
# Help
shq quick-help                  # Quick reference card (alias: ?)

# Query
shq output ~1                   # Last command output (aliases: o, show)
shq output shell:%/make/~5      # Last 5 make commands from shells
shq invocations ~20             # Last 20 invocations (aliases: i, history)
shq events %exit<>0~10          # Last 10 error events (alias: e)
shq info ~1                     # Info about last command (alias: I)
shq rerun ~1                    # Re-run last command (aliases: R, !!)

# Tags
shq tag list                    # Show all tags
shq tag add <name> '<filter>'   # Create tag
shq tag remove <name>           # Delete tag

# Sources
shq sources                     # List external sources
shq sources reload              # Reload sources.sql

# Import/freeze external data to parquet
shq compact --source claude-code:*:*:
shq archive --source *:*:*:*:   # Archive everything
```

---

## Implementation Phases

1. **Parser** - Detect and parse all syntax components
2. **Tag Registry** - Store and resolve saved filters
3. **Source Loading** - Load `~/.bird/sources.sql` on init
4. **Claude Code View** - First external source
5. **Query Router** - Translate micro-language to SQL
6. **Freeze/Import** - Copy external data to native parquet
