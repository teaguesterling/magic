# SQL Queries

MAGIC stores all data in DuckDB, giving you the full power of SQL to query your shell history.

## Available Views

BIRD creates several views for easy querying:

| View | Description |
|------|-------------|
| `invocations` | All command executions |
| `outputs` | Stdout/stderr for each command |
| `sessions` | Shell session information |
| `recent_invocations` | Commands from last 7 days |
| `invocations_today` | Commands from today |
| `failed_invocations` | Commands with non-zero exit code |
| `invocations_with_outputs` | Joined invocations and outputs |
| `clients` | Aggregated client information |

## Schema Reference

### invocations

| Column | Type | Description |
|--------|------|-------------|
| `id` | UUID | Unique invocation ID |
| `session_id` | VARCHAR | Shell session identifier |
| `timestamp` | TIMESTAMP | When the command was run |
| `duration_ms` | BIGINT | Execution time in milliseconds |
| `cwd` | VARCHAR | Working directory |
| `cmd` | VARCHAR | Full command string |
| `executable` | VARCHAR | Base command name |
| `exit_code` | INTEGER | Exit status |
| `format_hint` | VARCHAR | Detected output format |
| `client_id` | VARCHAR | Machine identifier |
| `hostname` | VARCHAR | Host name |
| `username` | VARCHAR | User name |
| `date` | DATE | Partition date |

### outputs

| Column | Type | Description |
|--------|------|-------------|
| `id` | UUID | Unique output ID |
| `invocation_id` | UUID | Reference to invocation |
| `stream` | VARCHAR | "stdout" or "stderr" |
| `content_hash` | VARCHAR | BLAKE3 hash of content |
| `byte_length` | BIGINT | Size in bytes |
| `storage_type` | VARCHAR | "inline" or "blob" |
| `storage_ref` | VARCHAR | Location of content |
| `content_type` | VARCHAR | MIME type hint |
| `date` | DATE | Partition date |

## Common Queries

### Recent Activity

```sql
-- Commands from today
SELECT cmd, exit_code, duration_ms
FROM invocations_today
ORDER BY timestamp DESC;

-- Commands from last 7 days
SELECT cmd, exit_code, timestamp
FROM recent_invocations
LIMIT 50;
```

### Finding Failures

```sql
-- All failed commands
SELECT cmd, exit_code, timestamp, cwd
FROM failed_invocations
ORDER BY timestamp DESC
LIMIT 20;

-- Failed commands by frequency
SELECT cmd, COUNT(*) as failures
FROM invocations
WHERE exit_code != 0
GROUP BY cmd
ORDER BY failures DESC
LIMIT 10;

-- Commands that sometimes fail (flaky)
SELECT
    cmd,
    COUNT(*) as total_runs,
    SUM(CASE WHEN exit_code = 0 THEN 1 ELSE 0 END) as successes,
    SUM(CASE WHEN exit_code != 0 THEN 1 ELSE 0 END) as failures,
    ROUND(100.0 * SUM(CASE WHEN exit_code = 0 THEN 1 ELSE 0 END) / COUNT(*), 1) as success_rate
FROM invocations
WHERE cmd LIKE '%test%'
GROUP BY cmd
HAVING failures > 0 AND successes > 0
ORDER BY success_rate ASC;
```

### Performance Analysis

```sql
-- Slowest commands today
SELECT cmd, duration_ms, timestamp
FROM invocations_today
WHERE duration_ms IS NOT NULL
ORDER BY duration_ms DESC
LIMIT 10;

-- Average duration by command
SELECT
    cmd,
    COUNT(*) as runs,
    ROUND(AVG(duration_ms)) as avg_ms,
    MIN(duration_ms) as min_ms,
    MAX(duration_ms) as max_ms
FROM invocations
WHERE duration_ms IS NOT NULL
GROUP BY cmd
HAVING runs > 5
ORDER BY avg_ms DESC
LIMIT 20;

-- Commands that got slower over time
SELECT
    date,
    cmd,
    AVG(duration_ms) as avg_duration
FROM invocations
WHERE cmd = 'make test'
GROUP BY date, cmd
ORDER BY date DESC;
```

### Storage Analysis

```sql
-- Output sizes by stream
SELECT
    stream,
    COUNT(*) as count,
    SUM(byte_length) / 1024 / 1024 as total_mb,
    AVG(byte_length) / 1024 as avg_kb
FROM outputs
GROUP BY stream;

-- Deduplication effectiveness
SELECT
    COUNT(*) as total_outputs,
    COUNT(DISTINCT content_hash) as unique_blobs,
    ROUND(100.0 * (1 - COUNT(DISTINCT content_hash)::FLOAT / COUNT(*)), 1) as dedup_percent
FROM outputs;

-- Largest outputs
SELECT
    i.cmd,
    o.stream,
    o.byte_length / 1024 as size_kb,
    o.storage_type
FROM outputs o
JOIN invocations i ON o.invocation_id = i.id
ORDER BY o.byte_length DESC
LIMIT 10;
```

### Session Analysis

```sql
-- Active sessions
SELECT
    session_id,
    client_id,
    MIN(registered_at) as started,
    COUNT(*) as command_count
FROM sessions s
JOIN invocations i ON s.session_id = i.session_id
GROUP BY s.session_id, s.client_id
ORDER BY started DESC;

-- Commands per day
SELECT
    date,
    COUNT(*) as commands,
    COUNT(DISTINCT session_id) as sessions
FROM invocations
GROUP BY date
ORDER BY date DESC
LIMIT 30;
```

### Working Directory Analysis

```sql
-- Most active directories
SELECT
    cwd,
    COUNT(*) as commands,
    COUNT(DISTINCT cmd) as unique_commands
FROM invocations
GROUP BY cwd
ORDER BY commands DESC
LIMIT 10;

-- Commands by project
SELECT
    REGEXP_EXTRACT(cwd, '.*/([^/]+)$') as project,
    COUNT(*) as commands,
    SUM(CASE WHEN exit_code != 0 THEN 1 ELSE 0 END) as failures
FROM invocations
GROUP BY project
HAVING commands > 10
ORDER BY commands DESC;
```

## Advanced Queries

### Time-Based Patterns

```sql
-- Commands by hour of day
SELECT
    EXTRACT(HOUR FROM timestamp) as hour,
    COUNT(*) as commands
FROM invocations
GROUP BY hour
ORDER BY hour;

-- Weekend vs weekday activity
SELECT
    CASE
        WHEN EXTRACT(DOW FROM timestamp) IN (0, 6) THEN 'Weekend'
        ELSE 'Weekday'
    END as day_type,
    COUNT(*) as commands,
    ROUND(AVG(duration_ms)) as avg_duration
FROM invocations
GROUP BY day_type;
```

### Command Patterns

```sql
-- Most common command prefixes
SELECT
    SPLIT_PART(cmd, ' ', 1) as command,
    COUNT(*) as uses
FROM invocations
GROUP BY command
ORDER BY uses DESC
LIMIT 20;

-- Git command breakdown
SELECT
    cmd,
    COUNT(*) as uses,
    SUM(CASE WHEN exit_code = 0 THEN 1 ELSE 0 END) as successes
FROM invocations
WHERE cmd LIKE 'git %'
GROUP BY cmd
ORDER BY uses DESC;
```

### Cross-Machine Queries

```sql
-- Activity by machine
SELECT
    hostname,
    COUNT(*) as commands,
    MIN(timestamp) as first_seen,
    MAX(timestamp) as last_seen
FROM invocations
GROUP BY hostname;

-- Shared commands across machines
SELECT
    cmd,
    COUNT(DISTINCT hostname) as machines,
    COUNT(*) as total_runs
FROM invocations
GROUP BY cmd
HAVING machines > 1
ORDER BY machines DESC, total_runs DESC;
```

## Using with DuckDB CLI

You can also query directly with the DuckDB CLI:

```bash
# Open the database
duckdb ~/.local/share/bird/db/bird.duckdb

# Run queries
D SELECT * FROM invocations_today LIMIT 5;

# Export to CSV
D COPY (SELECT * FROM invocations WHERE date = CURRENT_DATE) TO 'today.csv';

# Export to Parquet
D COPY (SELECT * FROM invocations) TO 'all_invocations.parquet';
```

## Tips

1. **Use date filtering** - The `date` column is the partition key, so filtering by date is very fast
2. **Use views** - Pre-built views like `recent_invocations` have common filters applied
3. **Export results** - Use `shq sql "..." > output.txt` or DuckDB's COPY command
4. **Complex queries** - For very complex analysis, use DuckDB CLI directly for better formatting options
