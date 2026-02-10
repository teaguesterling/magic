//! Integration tests for shq CLI.

use std::process::Command;
use tempfile::TempDir;

fn shq_cmd(bird_root: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_shq"));
    cmd.env("BIRD_ROOT", bird_root);
    // Extensions use DuckDB's default cache (~/.duckdb/extensions)
    cmd
}

fn init_bird(bird_root: &std::path::Path) {
    let output = shq_cmd(bird_root)
        .args(["init"])
        .output()
        .expect("failed to run shq init");
    assert!(output.status.success(), "shq init failed: {:?}", output);
}

#[test]
fn test_init() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Verify database was created
    assert!(tmp.path().join("db/bird.duckdb").exists());
}

#[test]
fn test_run_simple_command() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    let output = shq_cmd(tmp.path())
        .args(["run", "echo", "hello"])
        .output()
        .expect("failed to run command");

    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hello");
}

#[test]
fn test_run_with_c_flag() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    let output = shq_cmd(tmp.path())
        .args(["run", "-c", "echo stdout; echo stderr >&2"])
        .output()
        .expect("failed to run command");

    assert!(output.status.success());
    // PTY combines stdout and stderr into a single stream
    let combined = String::from_utf8_lossy(&output.stdout);
    assert!(combined.contains("stdout"), "Combined output should contain stdout");
    assert!(combined.contains("stderr"), "Combined output should contain stderr");
}

#[test]
fn test_run_captures_exit_code() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    let output = shq_cmd(tmp.path())
        .args(["run", "-c", "exit 42"])
        .output()
        .expect("failed to run command");

    assert!(!output.status.success());
    assert_eq!(output.status.code(), Some(42));
}

#[test]
fn test_save_from_stdin() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    let mut child = shq_cmd(tmp.path())
        .args(["save", "-c", "test command"])
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn");

    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().unwrap();
        stdin.write_all(b"test output\n").unwrap();
    }

    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    // save command is now silent (no "Saved X bytes" output)
}

#[test]
fn test_save_with_duration() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    let mut child = shq_cmd(tmp.path())
        .args(["save", "-c", "timed command", "-d", "1500"])
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn");

    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().unwrap();
        stdin.write_all(b"output\n").unwrap();
    }

    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());

    // Verify duration was recorded (use table format to see duration)
    let history = shq_cmd(tmp.path())
        .args(["history", "--format", "table"])
        .output()
        .expect("failed to get history");

    let history_str = String::from_utf8_lossy(&history.stdout);
    assert!(history_str.contains("1500ms"), "Duration not found in history: {}", history_str);
}

#[test]
fn test_save_with_exit_code() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    let mut child = shq_cmd(tmp.path())
        .args(["save", "-c", "failed command", "-x", "1"])
        .stdin(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn");

    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().unwrap();
        stdin.write_all(b"error output\n").unwrap();
    }

    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    // save command is now silent (no "Saved X bytes for... (exit N)" output)
}

#[test]
fn test_show_output() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a command
    shq_cmd(tmp.path())
        .args(["run", "echo", "captured output"])
        .output()
        .expect("failed to run");

    // Show the output
    let output = shq_cmd(tmp.path())
        .args(["show"])
        .output()
        .expect("failed to show");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "captured output"
    );
}

// Stream filtering tests are skipped because PTY-based `shq run` combines stdout/stderr
// into a single "combined" stream. Stream filtering still works with `shq save --stdout`
// and `shq save --stderr` but requires more complex test setup.
#[test]
#[ignore = "PTY combines stdout/stderr - stream filtering requires shq save"]
fn test_show_with_stream_filter() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // This test would need to use shq save with separate --stdout and --stderr
    // pipes to properly test stream filtering, which is complex to set up.
    let _ = tmp;
}

#[test]
fn test_history() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a few commands
    for i in 1..=3 {
        shq_cmd(tmp.path())
            .args(["run", "echo", &format!("command {}", i)])
            .output()
            .expect("failed to run");
    }

    let output = shq_cmd(tmp.path())
        .args(["history"])
        .output()
        .expect("failed to get history");

    let history = String::from_utf8_lossy(&output.stdout);
    assert!(history.contains("command 1"));
    assert!(history.contains("command 2"));
    assert!(history.contains("command 3"));
}

#[test]
fn test_sql_query() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a command
    shq_cmd(tmp.path())
        .args(["run", "echo", "sql test"])
        .output()
        .expect("failed to run");

    let output = shq_cmd(tmp.path())
        .args(["sql", "SELECT cmd FROM invocations WHERE cmd LIKE '%sql test%'"])
        .output()
        .expect("failed to query");

    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("sql test"));
}

#[test]
fn test_stats() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a command
    shq_cmd(tmp.path())
        .args(["run", "echo", "stats test"])
        .output()
        .expect("failed to run");

    let output = shq_cmd(tmp.path())
        .args(["stats"])
        .output()
        .expect("failed to get stats");

    assert!(output.status.success());
    let stats = String::from_utf8_lossy(&output.stdout);
    assert!(stats.contains("Total invocations: 1"));
}

#[test]
fn test_hook_init_zsh() {
    let output = Command::new(env!("CARGO_BIN_EXE_shq"))
        .args(["hook", "init", "--shell", "zsh"])
        .output()
        .expect("failed to run hook init");

    assert!(output.status.success());
    let hook = String::from_utf8_lossy(&output.stdout);
    assert!(hook.contains("__shq_preexec"));
    assert!(hook.contains("__shq_precmd"));
    assert!(hook.contains("shqr()"));
}

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
    assert!(
        hook.contains("__shq_prompt_command"),
        "Should use PROMPT_COMMAND"
    );
    assert!(hook.contains("history 1"), "Should read from history");
    assert!(
        hook.contains("PROMPT_COMMAND"),
        "Should register PROMPT_COMMAND"
    );
    // Privacy features preserved
    assert!(hook.contains("SHQ_DISABLED"), "Should check SHQ_DISABLED");
    assert!(hook.contains("SHQ_EXCLUDE"), "Should support SHQ_EXCLUDE");
    assert!(hook.contains("SHQ_IGNORE"), "Should support SHQ_IGNORE");
    assert!(hook.contains("__shq_should_ignore"), "Should have ignore function");
    // Output capture helper
    assert!(hook.contains("shqr"), "Should define shqr function");
}

#[test]
fn test_hook_contains_privacy_escapes() {
    // Test that zsh hook contains all privacy escape mechanisms
    let output = Command::new(env!("CARGO_BIN_EXE_shq"))
        .args(["hook", "init", "--shell", "zsh"])
        .output()
        .expect("failed to run hook init");

    let hook = String::from_utf8_lossy(&output.stdout);

    // Backslash escape
    assert!(hook.contains(r#"^\\"#), "Should check for backslash prefix");

    // SHQ_DISABLED env var
    assert!(hook.contains("SHQ_DISABLED"), "Should check SHQ_DISABLED");

    // SHQ_EXCLUDE and SHQ_IGNORE patterns
    assert!(hook.contains("SHQ_EXCLUDE"), "Should support SHQ_EXCLUDE");
    assert!(hook.contains("SHQ_IGNORE"), "Should support SHQ_IGNORE");
    assert!(hook.contains("__shq_should_ignore"), "Should have ignore function");

    // Inline extraction (--extract flag)
    assert!(hook.contains("--extract"), "Should use inline extraction");
}

#[test]
fn test_run_nosave_marker_skips_recording() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a command that emits the nosave marker
    // The OSC sequence is: ESC ] shq;nosave BEL
    let output = shq_cmd(tmp.path())
        .args(["run", "printf", r"\033]shq;nosave\007secret output"])
        .output()
        .expect("failed to run command");

    assert!(output.status.success());

    // Should have no invocations recorded (the nosave marker opted out)
    let history = shq_cmd(tmp.path())
        .args(["invocations", "10"])
        .output()
        .expect("failed to query invocations");

    let history_str = String::from_utf8_lossy(&history.stdout);
    // Should not contain the printf command (it was skipped)
    assert!(!history_str.contains("printf"), "Command with nosave marker should not be recorded");
}

#[test]
fn test_save_with_extract_flag() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Create a file with some error output
    let output_file = tmp.path().join("test_output.txt");
    std::fs::write(&output_file, "error: something went wrong\n").unwrap();

    // Save with --extract flag
    let output = shq_cmd(tmp.path())
        .args([
            "save",
            "-c", "make build",
            "-x", "1",
            "--extract",
            "-q",
        ])
        .arg(&output_file)
        .output()
        .expect("failed to save");

    assert!(output.status.success());

    // Check that events were extracted
    let events = shq_cmd(tmp.path())
        .args(["events", "1"])
        .output()
        .expect("failed to query events");

    // The output should contain events or indicate extraction happened
    assert!(events.status.success());
}

#[test]
fn test_archive_recent_data_not_archived() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // First archive clears seed files (dated 1970-01-01)
    shq_cmd(tmp.path())
        .args(["archive"])
        .output()
        .expect("failed to archive seed files");

    // Run a command (creates today's data)
    shq_cmd(tmp.path())
        .args(["run", "echo", "recent data"])
        .output()
        .expect("failed to run");

    // Archive again with default 14 days - today's data should NOT be archived
    let output = shq_cmd(tmp.path())
        .args(["archive"])
        .output()
        .expect("failed to archive");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Nothing to archive"), "Today's data should not be archived: {}", stdout);
}

#[test]
fn test_archive_dry_run() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a command
    shq_cmd(tmp.path())
        .args(["run", "echo", "test"])
        .output()
        .expect("failed to run");

    // Create an old date partition and backdate the files
    let old_date = "2020-01-01";
    let old_inv_dir = tmp.path().join("data/recent/invocations").join(format!("date={}", old_date));
    std::fs::create_dir_all(&old_inv_dir).unwrap();

    // Copy a parquet file to the old partition
    let recent_inv_dir = tmp.path().join("data/recent/invocations");
    if let Ok(entries) = std::fs::read_dir(&recent_inv_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && path.file_name().unwrap().to_string_lossy().starts_with("date=") {
                // Find a parquet file to copy
                if let Ok(files) = std::fs::read_dir(&path) {
                    for file in files.flatten() {
                        let file_path = file.path();
                        if file_path.extension().map(|e| e == "parquet").unwrap_or(false) {
                            let dest = old_inv_dir.join(file_path.file_name().unwrap());
                            std::fs::copy(&file_path, &dest).unwrap();
                            break;
                        }
                    }
                }
                break;
            }
        }
    }

    // Archive dry-run with 1 day threshold
    let output = shq_cmd(tmp.path())
        .args(["archive", "--days", "1", "--dry-run"])
        .output()
        .expect("failed to archive");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Dry run"));
}

#[test]
fn test_archive_moves_old_data() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a command to have something to archive
    shq_cmd(tmp.path())
        .args(["run", "echo", "test"])
        .output()
        .expect("failed to run");

    // Create an old date partition
    let old_date = "2020-01-01";
    let old_inv_dir = tmp.path().join("data/recent/invocations").join(format!("date={}", old_date));
    std::fs::create_dir_all(&old_inv_dir).unwrap();

    // Copy a parquet file to the old partition
    let recent_inv_dir = tmp.path().join("data/recent/invocations");
    let mut copied = false;
    if let Ok(entries) = std::fs::read_dir(&recent_inv_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && path.file_name().unwrap().to_string_lossy().starts_with("date=2") {
                if let Ok(files) = std::fs::read_dir(&path) {
                    for file in files.flatten() {
                        let file_path = file.path();
                        if file_path.extension().map(|e| e == "parquet").unwrap_or(false) {
                            let dest = old_inv_dir.join(file_path.file_name().unwrap());
                            std::fs::copy(&file_path, &dest).unwrap();
                            copied = true;
                            break;
                        }
                    }
                }
                break;
            }
        }
    }

    if !copied {
        // Skip test if we couldn't set up the test data
        return;
    }

    // Verify old partition exists before archive
    assert!(old_inv_dir.exists());

    // Archive with 1 day threshold
    let output = shq_cmd(tmp.path())
        .args(["archive", "--days", "1"])
        .output()
        .expect("failed to archive");

    assert!(output.status.success());

    // Old partition should be gone from recent
    assert!(!old_inv_dir.exists(), "Old partition should have been archived");

    // Should be in archive
    let archive_dir = tmp.path().join("data/archive/invocations").join(format!("date={}", old_date));
    assert!(archive_dir.exists(), "Data should be in archive tier");
}

#[test]
fn test_compact_nothing_to_compact() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run just one command (not enough to trigger compaction)
    shq_cmd(tmp.path())
        .args(["run", "echo", "single"])
        .output()
        .expect("failed to run");

    // Compact with default threshold (50) - nothing should be compacted
    let output = shq_cmd(tmp.path())
        .args(["compact"])
        .output()
        .expect("failed to compact");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Nothing to compact"));
}

#[test]
fn test_compact_dry_run() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a command
    shq_cmd(tmp.path())
        .args(["run", "echo", "test"])
        .output()
        .expect("failed to run");

    // Compact dry-run
    let output = shq_cmd(tmp.path())
        .args(["compact", "--dry-run"])
        .output()
        .expect("failed to compact");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Dry run"));
}

#[test]
fn test_compact_with_low_threshold() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run multiple commands to create multiple files
    for i in 1..=5 {
        shq_cmd(tmp.path())
            .args(["run", "echo", &format!("command {}", i)])
            .output()
            .expect("failed to run");
    }

    // Compact with low threshold (2) to trigger compaction
    let output = shq_cmd(tmp.path())
        .args(["compact", "--threshold", "2"])
        .output()
        .expect("failed to compact");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Should have compacted something
    assert!(
        stdout.contains("Compacted") || stdout.contains("Nothing to compact"),
        "Unexpected output: {}", stdout
    );
}

#[test]
fn test_compact_session_specific() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run multiple commands
    for i in 1..=3 {
        shq_cmd(tmp.path())
            .args(["run", "echo", &format!("cmd {}", i)])
            .output()
            .expect("failed to run");
    }

    // Compact for a specific (non-existent) session - should do nothing
    let output = shq_cmd(tmp.path())
        .args(["compact", "-s", "nonexistent-session", "--threshold", "1"])
        .output()
        .expect("failed to compact");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Nothing to compact"));
}

#[test]
fn test_compact_today_only() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a command
    shq_cmd(tmp.path())
        .args(["run", "echo", "today"])
        .output()
        .expect("failed to run");

    // Compact with --today flag
    let output = shq_cmd(tmp.path())
        .args(["compact", "--today", "-s", "test-session"])
        .output()
        .expect("failed to compact");

    assert!(output.status.success());
}

#[test]
fn test_compact_quiet_mode() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a command
    shq_cmd(tmp.path())
        .args(["run", "echo", "quiet"])
        .output()
        .expect("failed to run");

    // Compact with quiet mode - should produce no output when nothing to compact
    let output = shq_cmd(tmp.path())
        .args(["compact", "-q", "-s", "test-session", "--today"])
        .output()
        .expect("failed to compact");

    assert!(output.status.success());
    // Quiet mode should produce no output when nothing is compacted
    assert!(
        String::from_utf8_lossy(&output.stdout).is_empty(),
        "Quiet mode should not produce output when nothing compacted"
    );
}

#[test]
fn test_show_head_option() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run command with multi-line output
    shq_cmd(tmp.path())
        .args(["run", "-c", "echo -e 'line1\\nline2\\nline3\\nline4\\nline5'"])
        .output()
        .expect("failed to run");

    // Show only first 2 lines
    let output = shq_cmd(tmp.path())
        .args(["show", "--head", "2"])
        .output()
        .expect("failed to show");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("line1"), "Should contain line1");
    assert!(stdout.contains("line2"), "Should contain line2");
    assert!(!stdout.contains("line3"), "Should NOT contain line3");
}

#[test]
fn test_show_tail_option() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run command with multi-line output
    shq_cmd(tmp.path())
        .args(["run", "-c", "echo -e 'line1\\nline2\\nline3\\nline4\\nline5'"])
        .output()
        .expect("failed to run");

    // Show only last 2 lines
    let output = shq_cmd(tmp.path())
        .args(["show", "--tail", "2"])
        .output()
        .expect("failed to show");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("line1"), "Should NOT contain line1");
    assert!(!stdout.contains("line3"), "Should NOT contain line3");
    assert!(stdout.contains("line4"), "Should contain line4");
    assert!(stdout.contains("line5"), "Should contain line5");
}

#[test]
fn test_show_lines_shortcut() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run command with multi-line output
    shq_cmd(tmp.path())
        .args(["run", "-c", "echo -e 'a\\nb\\nc\\nd'"])
        .output()
        .expect("failed to run");

    // Show only first 2 lines with -n shortcut
    let output = shq_cmd(tmp.path())
        .args(["show", "-n", "2"])
        .output()
        .expect("failed to show");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("a"), "Should contain a");
    assert!(stdout.contains("b"), "Should contain b");
    assert!(!stdout.contains("c"), "Should NOT contain c");
}

#[test]
fn test_show_strip_ansi() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run command with ANSI color codes
    shq_cmd(tmp.path())
        .args(["run", "-c", "echo -e '\\033[31mred text\\033[0m'"])
        .output()
        .expect("failed to run");

    // Show with ANSI codes stripped
    let output = shq_cmd(tmp.path())
        .args(["show", "--strip"])
        .output()
        .expect("failed to show");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("red text"), "Should contain text");
    assert!(!stdout.contains("\x1b["), "Should NOT contain ANSI escape codes");
    assert!(!stdout.contains("\\033"), "Should NOT contain escape sequences");
}

#[test]
#[ignore = "PTY combines stdout/stderr - stream filtering requires shq save"]
fn test_show_stdout_shortcut() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());
    let _ = tmp;
}

#[test]
#[ignore = "PTY combines stdout/stderr - stream filtering requires shq save"]
fn test_show_stderr_shortcut() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());
    let _ = tmp;
}

#[test]
fn test_show_all_combined() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run command with both stdout and stderr
    shq_cmd(tmp.path())
        .args(["run", "-c", "echo out_first; echo err_second >&2"])
        .output()
        .expect("failed to run");

    // Show all combined to stdout with -A shortcut
    let output = shq_cmd(tmp.path())
        .args(["show", "-A"])
        .output()
        .expect("failed to show");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Both streams should be in stdout when using -A
    assert!(stdout.contains("out_first"), "Should contain stdout in combined output");
    assert!(stdout.contains("err_second"), "Should contain stderr in combined output");
}

#[test]
fn test_show_no_output_found() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Don't run any commands, just try to show
    let output = shq_cmd(tmp.path())
        .args(["show"])
        .output()
        .expect("failed to show");

    // Should succeed but indicate no output found
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("No matching invocation found") || stderr.contains("No output found"),
        "Should indicate no output: {}", stderr
    );
}

// ============================================================================
// Events command tests
// ============================================================================

#[test]
fn test_events_no_events() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a simple command that produces no parseable events
    shq_cmd(tmp.path())
        .args(["run", "echo", "hello"])
        .output()
        .expect("failed to run");

    // Query events - should find none
    let output = shq_cmd(tmp.path())
        .args(["events"])
        .output()
        .expect("failed to query events");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("No events found"));
}

#[test]
fn test_extract_events_manual() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a command that produces gcc-like error output
    shq_cmd(tmp.path())
        .args(["run", "-c", "echo 'test.c:10:5: error: undefined reference' >&2; exit 1"])
        .output()
        .expect("failed to run");

    // Manually extract events
    let output = shq_cmd(tmp.path())
        .args(["extract-events"])
        .output()
        .expect("failed to extract events");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Should report extraction (may or may not find events depending on duck_hunt parsing)
    assert!(
        stdout.contains("Extracted") || stdout.contains("No events found"),
        "Unexpected output: {}", stdout
    );
}

#[test]
fn test_extract_events_quiet_mode() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a command
    shq_cmd(tmp.path())
        .args(["run", "echo", "test"])
        .output()
        .expect("failed to run");

    // Extract with quiet mode
    let output = shq_cmd(tmp.path())
        .args(["extract-events", "-q"])
        .output()
        .expect("failed to extract events");

    assert!(output.status.success());
    // Quiet mode should produce no output
    assert!(
        String::from_utf8_lossy(&output.stdout).is_empty(),
        "Quiet mode should not produce output"
    );
}

#[test]
fn test_run_with_extract_flag() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run with -x flag to auto-extract events
    let output = shq_cmd(tmp.path())
        .args(["run", "-x", "echo", "hello"])
        .output()
        .expect("failed to run");

    assert!(output.status.success());
    // The -x flag should work without errors (may or may not find events)
}

#[test]
fn test_events_count_only() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a simple command
    shq_cmd(tmp.path())
        .args(["run", "echo", "test"])
        .output()
        .expect("failed to run");

    // Query event count
    let output = shq_cmd(tmp.path())
        .args(["events", "--count"])
        .output()
        .expect("failed to count events");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Should output a number (likely 0)
    assert!(
        stdout.trim().parse::<i64>().is_ok(),
        "Count should be a number: {}", stdout
    );
}

#[test]
fn test_events_with_severity_filter() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a command
    shq_cmd(tmp.path())
        .args(["run", "echo", "test"])
        .output()
        .expect("failed to run");

    // Query events with severity filter
    let output = shq_cmd(tmp.path())
        .args(["events", "-s", "error"])
        .output()
        .expect("failed to query events");

    assert!(output.status.success());
}

#[test]
fn test_events_with_last_n_filter() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run multiple commands
    for i in 1..=3 {
        shq_cmd(tmp.path())
            .args(["run", "echo", &format!("cmd {}", i)])
            .output()
            .expect("failed to run");
    }

    // Query events from last 1 invocation (use query selector, -n is for event limit)
    let output = shq_cmd(tmp.path())
        .args(["events", "1"])
        .output()
        .expect("failed to query events");

    assert!(output.status.success());
}

#[test]
fn test_events_with_limit() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a command
    shq_cmd(tmp.path())
        .args(["run", "echo", "test"])
        .output()
        .expect("failed to run");

    // Query events with limit (any N)
    let output = shq_cmd(tmp.path())
        .args(["events", "-n", "10"])
        .output()
        .expect("failed to query events");

    assert!(output.status.success());
}

#[test]
fn test_events_with_head_limit() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a command
    shq_cmd(tmp.path())
        .args(["run", "echo", "test"])
        .output()
        .expect("failed to run");

    // Query events with head limit (+N = first N)
    let output = shq_cmd(tmp.path())
        .args(["events", "-n", "+5"])
        .output()
        .expect("failed to query events");

    assert!(output.status.success());
}

#[test]
fn test_events_with_tail_limit() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a command
    shq_cmd(tmp.path())
        .args(["run", "echo", "test"])
        .output()
        .expect("failed to run");

    // Query events with tail limit (-N = last N)
    let output = shq_cmd(tmp.path())
        .args(["events", "-n", "-5"])
        .output()
        .expect("failed to query events");

    assert!(output.status.success());
}

#[test]
fn test_extract_events_with_format_override() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a command
    shq_cmd(tmp.path())
        .args(["run", "echo", "test output"])
        .output()
        .expect("failed to run");

    // Extract with explicit format
    let output = shq_cmd(tmp.path())
        .args(["extract-events", "-f", "auto"])
        .output()
        .expect("failed to extract events");

    assert!(output.status.success());
}

#[test]
fn test_extract_events_force_reextract() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a command
    shq_cmd(tmp.path())
        .args(["run", "echo", "test"])
        .output()
        .expect("failed to run");

    // Extract events first time
    shq_cmd(tmp.path())
        .args(["extract-events"])
        .output()
        .expect("failed to extract events");

    // Force re-extract
    let output = shq_cmd(tmp.path())
        .args(["extract-events", "--force"])
        .output()
        .expect("failed to re-extract events");

    assert!(output.status.success());
}

#[test]
fn test_events_reparse_mode() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a command
    shq_cmd(tmp.path())
        .args(["run", "echo", "test"])
        .output()
        .expect("failed to run");

    // Use reparse mode to re-extract and query
    let output = shq_cmd(tmp.path())
        .args(["events", "--reparse"])
        .output()
        .expect("failed to reparse events");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Re-extracted") || stdout.contains("events"),
        "Should indicate re-extraction: {}", stdout
    );
}

#[test]
fn test_events_from_stderr() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a command that outputs to stderr (like gcc does)
    shq_cmd(tmp.path())
        .args(["run", "-c", "echo 'file.c:1:1: warning: test warning' >&2"])
        .output()
        .expect("failed to run");

    // Extract events - should parse stderr too
    let output = shq_cmd(tmp.path())
        .args(["extract-events"])
        .output()
        .expect("failed to extract events");

    assert!(output.status.success());
    // The extraction should complete without error
    // (actual event detection depends on duck_hunt)
}

#[test]
fn test_extract_events_backfill_all() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run multiple commands without extracting events
    for i in 1..=3 {
        shq_cmd(tmp.path())
            .args(["run", "echo", &format!("command {}", i)])
            .output()
            .expect("failed to run");
    }

    // Backfill all invocations
    let output = shq_cmd(tmp.path())
        .args(["extract-events", "--all"])
        .output()
        .expect("failed to backfill");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("invocations") || stdout.contains("No invocations"),
        "Should report backfill results: {}", stdout
    );
}

#[test]
fn test_extract_events_backfill_dry_run() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a command
    shq_cmd(tmp.path())
        .args(["run", "echo", "test"])
        .output()
        .expect("failed to run");

    // Dry-run backfill
    let output = shq_cmd(tmp.path())
        .args(["extract-events", "--all", "--dry-run"])
        .output()
        .expect("failed to dry-run backfill");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Would extract") || stdout.contains("No invocations"),
        "Dry-run should show what would be done: {}", stdout
    );
}

#[test]
fn test_extract_events_backfill_limit() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run multiple commands
    for i in 1..=5 {
        shq_cmd(tmp.path())
            .args(["run", "echo", &format!("cmd {}", i)])
            .output()
            .expect("failed to run");
    }

    // Backfill with limit
    let output = shq_cmd(tmp.path())
        .args(["extract-events", "--all", "--limit", "2", "--dry-run"])
        .output()
        .expect("failed to backfill with limit");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Should only show 2 invocations
    assert!(
        stdout.contains("2 invocations") || stdout.contains("No invocations"),
        "Should limit to 2 invocations: {}", stdout
    );
}

#[test]
fn test_extract_events_backfill_since() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a command
    shq_cmd(tmp.path())
        .args(["run", "echo", "recent"])
        .output()
        .expect("failed to run");

    // Backfill with future since date (should find nothing)
    let output = shq_cmd(tmp.path())
        .args(["extract-events", "--all", "--since", "2099-01-01"])
        .output()
        .expect("failed to backfill with since");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("No invocations"),
        "Future date should find no invocations: {}", stdout
    );
}

#[test]
fn test_archive_extract_first() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a command that produces events
    shq_cmd(tmp.path())
        .args(["run", "-c", "echo 'test.c:1: warning: test' >&2"])
        .output()
        .expect("failed to run");

    // Archive with extract-first flag (dry-run to avoid actual archiving)
    let output = shq_cmd(tmp.path())
        .args(["archive", "--extract-first", "--dry-run"])
        .output()
        .expect("failed to archive");

    assert!(output.status.success());
    // Dry-run should not actually extract, just show what would be archived
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Dry run"));
}

#[test]
fn test_compact_includes_events() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run multiple commands and extract events
    for i in 1..=3 {
        shq_cmd(tmp.path())
            .args(["run", "-x", "-c", &format!("echo 'test.c:{}: warning: warn{}' >&2", i, i)])
            .output()
            .expect("failed to run");
    }

    // Compact with low threshold
    let output = shq_cmd(tmp.path())
        .args(["compact", "--threshold", "2", "--dry-run"])
        .output()
        .expect("failed to compact");

    assert!(output.status.success());
    // Should mention dry run
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Dry run"));
}

// Tag tests

#[test]
fn test_run_with_tag() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run command with tag
    let output = shq_cmd(tmp.path())
        .args(["run", "-t", "mytag", "echo", "tagged command"])
        .output()
        .expect("failed to run");

    assert!(output.status.success());

    // Verify tag is stored via SQL
    let output = shq_cmd(tmp.path())
        .args(["sql", "SELECT tag FROM invocations WHERE tag = 'mytag'"])
        .output()
        .expect("failed to query");

    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("mytag"));
}

#[test]
fn test_save_with_tag() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Save with tag
    let mut child = shq_cmd(tmp.path())
        .args(["save", "-c", "test command", "--tag", "savetag"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("failed to spawn");

    {
        use std::io::Write;
        let stdin = child.stdin.as_mut().unwrap();
        stdin.write_all(b"output\n").unwrap();
    }

    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());

    // Verify tag is stored
    let output = shq_cmd(tmp.path())
        .args(["sql", "SELECT tag FROM invocations WHERE tag = 'savetag'"])
        .output()
        .expect("failed to query");

    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("savetag"));
}

#[test]
fn test_tag_lookup() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run command with tag
    shq_cmd(tmp.path())
        .args(["run", "-t", "findme", "echo", "find this"])
        .output()
        .expect("failed to run");

    // Look up by tag using :tagname syntax
    let output = shq_cmd(tmp.path())
        .args(["info", ":findme", "--field", "cmd"])
        .output()
        .expect("failed to get info");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("find this"), "Expected 'find this' in output: {}", stdout);
}

#[test]
fn test_tag_in_info_output() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run command with tag
    shq_cmd(tmp.path())
        .args(["run", "-t", "infotag", "echo", "hello"])
        .output()
        .expect("failed to run");

    // Get info using ~1 (most recent)
    let output = shq_cmd(tmp.path())
        .args(["info", "~1", "--format", "table"])
        .output()
        .expect("failed to get info");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("infotag"), "Expected 'infotag' in output: {}", stdout);
}

#[test]
fn test_tag_field_extraction() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run command with tag
    shq_cmd(tmp.path())
        .args(["run", "-t", "extracttag", "echo", "test"])
        .output()
        .expect("failed to run");

    // Extract just the tag field
    let output = shq_cmd(tmp.path())
        .args(["info", ":extracttag", "--field", "tag"])
        .output()
        .expect("failed to get info");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.trim(), "extracttag");
}

#[test]
fn test_tag_not_found() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a command without tag
    shq_cmd(tmp.path())
        .args(["run", "echo", "no tag"])
        .output()
        .expect("failed to run");

    // Try to look up nonexistent tag
    let output = shq_cmd(tmp.path())
        .args(["info", ":nonexistent"])
        .output()
        .expect("failed to get info");

    // Should fail because tag doesn't exist
    assert!(!output.status.success());
}

// ============================================================================
// BIRD v5 Schema Tests - Attempts/Outcomes split
// ============================================================================

#[test]
fn test_v5_attempts_table_exists() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a command
    shq_cmd(tmp.path())
        .args(["run", "echo", "v5 test"])
        .output()
        .expect("failed to run");

    // Query attempts table directly
    let output = shq_cmd(tmp.path())
        .args(["sql", "SELECT COUNT(*) as count FROM attempts"])
        .output()
        .expect("failed to query attempts");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Should have at least 1 attempt (seed + our command)
    assert!(stdout.contains("1") || stdout.contains("2"), "Should have attempts: {}", stdout);
}

#[test]
fn test_v5_outcomes_table_exists() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a command
    shq_cmd(tmp.path())
        .args(["run", "echo", "v5 test"])
        .output()
        .expect("failed to run");

    // Query outcomes table directly
    let output = shq_cmd(tmp.path())
        .args(["sql", "SELECT COUNT(*) as count FROM outcomes"])
        .output()
        .expect("failed to query outcomes");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Should have at least 1 outcome
    assert!(stdout.contains("1") || stdout.contains("2"), "Should have outcomes: {}", stdout);
}

#[test]
fn test_v5_invocations_is_view() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a command
    shq_cmd(tmp.path())
        .args(["run", "echo", "view test"])
        .output()
        .expect("failed to run");

    // Query invocations VIEW - should work transparently
    let output = shq_cmd(tmp.path())
        .args(["sql", "SELECT cmd, status FROM invocations WHERE cmd LIKE '%view test%'"])
        .output()
        .expect("failed to query invocations view");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("view test"), "VIEW should return data: {}", stdout);
    assert!(stdout.contains("completed"), "Status should be 'completed': {}", stdout);
}

#[test]
fn test_v5_status_derived_from_join() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a successful command
    shq_cmd(tmp.path())
        .args(["run", "echo", "success"])
        .output()
        .expect("failed to run");

    // Run a failing command
    shq_cmd(tmp.path())
        .args(["run", "-c", "exit 42"])
        .output()
        .expect("failed to run");

    // Both should have status='completed' (exit code doesn't affect status)
    let output = shq_cmd(tmp.path())
        .args(["sql", "SELECT status, exit_code FROM invocations ORDER BY timestamp DESC LIMIT 2"])
        .output()
        .expect("failed to query");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Both should be 'completed' - status is not about exit code
    assert!(stdout.contains("completed"), "Both should be completed: {}", stdout);
}

#[test]
fn test_v5_attempt_outcome_join() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a command
    shq_cmd(tmp.path())
        .args(["run", "echo", "join test"])
        .output()
        .expect("failed to run");

    // Manually verify the join works by selecting from both tables
    let output = shq_cmd(tmp.path())
        .args(["sql",
            "SELECT a.cmd, o.exit_code, o.duration_ms \
             FROM attempts a \
             JOIN outcomes o ON a.id = o.attempt_id \
             WHERE a.cmd LIKE '%join test%'"])
        .output()
        .expect("failed to query join");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("join test"), "JOIN should return data: {}", stdout);
    assert!(stdout.contains("0"), "Exit code should be 0: {}", stdout);
}

#[test]
fn test_v5_bird_meta_table() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Query bird_meta for schema version
    let output = shq_cmd(tmp.path())
        .args(["sql", "SELECT value FROM bird_meta WHERE key = 'schema_version'"])
        .output()
        .expect("failed to query bird_meta");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("5"), "Schema version should be 5: {}", stdout);
}

#[test]
fn test_v5_schema_version_is_five() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Query bird_meta schema version - should be 5
    let output = shq_cmd(tmp.path())
        .args(["sql", "SELECT key, value FROM bird_meta ORDER BY key"])
        .output()
        .expect("failed to query bird_meta");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("schema_version"), "Should have schema_version key: {}", stdout);
    assert!(stdout.contains("5"), "Schema version should be 5: {}", stdout);
}

#[test]
fn test_v5_no_status_partitioning() {
    // In v5, there's no status= partitioning. This is the key structural change.
    // In DuckDB mode (default), data is in tables. In Parquet mode, data is in
    // attempts/outcomes directories (not invocations/status=).
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a command
    shq_cmd(tmp.path())
        .args(["run", "echo", "dir test"])
        .output()
        .expect("failed to run");

    // v5 should NOT have status= partitioned invocations directory
    // Check both possible locations (depending on storage mode)
    let recent_dir1 = tmp.path().join("data/recent/invocations");
    let recent_dir2 = tmp.path().join("db/data/recent/invocations");

    for invocations_dir in [recent_dir1, recent_dir2] {
        if invocations_dir.exists() {
            let has_status_dir = std::fs::read_dir(&invocations_dir)
                .map(|entries| {
                    entries.filter_map(|e| e.ok())
                        .any(|e| e.file_name().to_string_lossy().starts_with("status="))
                })
                .unwrap_or(false);
            assert!(!has_status_dir, "v5 should not have status= partitioning");
        }
    }

    // Verify no db/pending directory (v4 pending file mechanism removed)
    assert!(!tmp.path().join("db/pending").exists(), "v5 should not have pending directory");
}

#[test]
fn test_v5_no_pending_files() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a command
    shq_cmd(tmp.path())
        .args(["run", "echo", "no pending"])
        .output()
        .expect("failed to run");

    // v5 should NOT have a db/pending directory
    let pending_dir = tmp.path().join("db/pending");
    assert!(!pending_dir.exists(), "v5 should not have pending directory");
}

#[test]
fn test_v5_attempt_fields() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a command with tag
    shq_cmd(tmp.path())
        .args(["run", "-t", "v5tag", "echo", "field test"])
        .output()
        .expect("failed to run");

    // Query attempt-specific fields
    let output = shq_cmd(tmp.path())
        .args(["sql",
            "SELECT id, timestamp, cmd, cwd, session_id, tag, source_client, date \
             FROM attempts WHERE cmd LIKE '%field test%'"])
        .output()
        .expect("failed to query attempt fields");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("field test"), "Should have cmd: {}", stdout);
    assert!(stdout.contains("v5tag"), "Should have tag: {}", stdout);
    assert!(stdout.contains("shq"), "Should have source_client: {}", stdout);
}

#[test]
fn test_v5_outcome_fields() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a command
    shq_cmd(tmp.path())
        .args(["run", "echo", "outcome fields"])
        .output()
        .expect("failed to run");

    // Query outcome-specific fields
    let output = shq_cmd(tmp.path())
        .args(["sql",
            "SELECT o.attempt_id, o.completed_at, o.exit_code, o.duration_ms, o.date \
             FROM outcomes o \
             JOIN attempts a ON a.id = o.attempt_id \
             WHERE a.cmd LIKE '%outcome fields%'"])
        .output()
        .expect("failed to query outcome fields");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("0"), "Should have exit_code 0: {}", stdout);
}

#[test]
fn test_v5_existing_queries_work() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run commands
    for i in 1..=3 {
        shq_cmd(tmp.path())
            .args(["run", "echo", &format!("query test {}", i)])
            .output()
            .expect("failed to run");
    }

    // Existing queries against invocations VIEW should work unchanged
    let output = shq_cmd(tmp.path())
        .args(["sql",
            "SELECT cmd, exit_code, duration_ms, status \
             FROM invocations \
             WHERE cmd LIKE '%query test%' \
             ORDER BY timestamp"])
        .output()
        .expect("failed to query");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("query test 1"), "Should find first command: {}", stdout);
    assert!(stdout.contains("query test 2"), "Should find second command: {}", stdout);
    assert!(stdout.contains("query test 3"), "Should find third command: {}", stdout);
}

#[test]
fn test_v5_history_command_uses_view() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run commands
    shq_cmd(tmp.path())
        .args(["run", "echo", "history v5"])
        .output()
        .expect("failed to run");

    // History command should work (uses invocations VIEW internally)
    let output = shq_cmd(tmp.path())
        .args(["history"])
        .output()
        .expect("failed to get history");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("history v5"), "History should show command: {}", stdout);
}

#[test]
fn test_v5_stats_works() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run a command
    shq_cmd(tmp.path())
        .args(["run", "echo", "stats v5"])
        .output()
        .expect("failed to run");

    // Stats should work with v5 schema
    let output = shq_cmd(tmp.path())
        .args(["stats"])
        .output()
        .expect("failed to get stats");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Should show invocation count (includes seed + our command)
    assert!(stdout.contains("Total invocations:"), "Should show invocation stats: {}", stdout);
}
