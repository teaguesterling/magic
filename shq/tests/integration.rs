//! Integration tests for shq CLI.

use std::process::Command;
use tempfile::TempDir;

fn shq_cmd(bird_root: &std::path::Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_shq"));
    cmd.env("BIRD_ROOT", bird_root);
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
    assert!(String::from_utf8_lossy(&output.stdout).contains("stdout"));
    assert!(String::from_utf8_lossy(&output.stderr).contains("stderr"));
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
    assert!(String::from_utf8_lossy(&output.stderr).contains("Saved 12 bytes"));
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

    // Verify duration was recorded
    let history = shq_cmd(tmp.path())
        .args(["history"])
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
    assert!(String::from_utf8_lossy(&output.stderr).contains("exit 1"));
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

#[test]
fn test_show_with_stream_filter() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Run command with both streams
    shq_cmd(tmp.path())
        .args(["run", "-c", "echo out; echo err >&2"])
        .output()
        .expect("failed to run");

    // Show only stdout
    let stdout_only = shq_cmd(tmp.path())
        .args(["show", "-s", "stdout"])
        .output()
        .expect("failed to show stdout");

    assert!(String::from_utf8_lossy(&stdout_only.stdout).contains("out"));
    assert!(!String::from_utf8_lossy(&stdout_only.stdout).contains("err"));

    // Show only stderr
    let stderr_only = shq_cmd(tmp.path())
        .args(["show", "-s", "stderr"])
        .output()
        .expect("failed to show stderr");

    assert!(String::from_utf8_lossy(&stderr_only.stderr).contains("err"));
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
    assert!(hook.contains("__shq_debug"));
    assert!(hook.contains("__shq_prompt"));
    assert!(hook.contains("PROMPT_COMMAND"));
}
