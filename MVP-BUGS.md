# MVP Known Issues

Issues discovered and fixed during MVP branch creation.

## Fixed Issues

### 1. Query filters now applied in micro-language (FIXED)

The query micro-language filters (`%exit<>0`, `%/pattern/`, `%duration>N`) are now properly applied.

**Examples that work:**
```bash
shq i '%exit<>0'        # Only failed commands
shq i '%/echo/'         # Commands matching regex
shq i '%exit<>0~5'      # Last 5 failed commands
shq i '%duration>1000'  # Commands that took >1 second
shq o '%/make/~1'       # Output of last make command
```

### 2. DuckDB concurrent access with retry logic (FIXED)

Added exponential backoff retry logic for database connections to handle concurrent access from multiple shell hook processes.

- Up to 10 retries with exponential backoff (10ms → 1000ms)
- Jitter added to avoid thundering herd problem
- Gracefully handles lock conflicts from background `shq save` processes

---

## Notes

- Parquet mode is still available but DuckDB is now the default
- Extension loading (duck_hunt, scalarfs) is graceful - missing extensions produce warnings, not errors
- Bash hook uses PS0 + PROMPT_COMMAND + `history 1` for full pipeline capture
- The `shqr` function provides synchronous output capture
- The automatic hook (`PROMPT_COMMAND`) requires a true interactive bash session to test properly
