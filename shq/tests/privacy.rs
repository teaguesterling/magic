//! Privacy and storage-hardening regression tests.
//!
//! These are reproduce-first tests for the class of issues where the
//! *default* capture path stored secrets verbatim in world-readable files:
//!
//! 1. sensitive commands must be excluded / redacted on the default save
//!    path (not only in the opt-in retrospective buffer),
//! 2. the data root must be 0700 and stored files 0600,
//! 3. `--to-buffer` must not silently fall through to permanent storage
//!    with captured output when the buffer is disabled,
//! 4. the bash hook's space-prefix privacy escape must work and empty-Enter
//!    must not duplicate saves,
//! 5. multibyte query input must not panic the parser.

use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

use tempfile::TempDir;

fn shq_cmd(bird_root: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_shq"));
    cmd.env("BIRD_ROOT", bird_root);
    cmd
}

fn init_bird(bird_root: &Path) {
    let output = shq_cmd(bird_root)
        .args(["init"])
        .output()
        .expect("failed to run shq init");
    assert!(output.status.success(), "shq init failed: {:?}", output);
}

/// Save a command on the default path (metadata only, empty stdin).
fn save_cmd(bird_root: &Path, extra_args: &[&str], command: &str) {
    let mut args = vec!["save", "-c", command, "-x", "0", "-q", "--no-extract"];
    args.extend_from_slice(extra_args);
    let output = shq_cmd(bird_root)
        .args(&args)
        .stdin(Stdio::null())
        .output()
        .expect("failed to run shq save");
    assert!(output.status.success(), "shq save failed: {:?}", output);
}

/// Return all stored command lines.
fn stored_commands(bird_root: &Path) -> String {
    let output = shq_cmd(bird_root)
        .args(["sql", "SELECT cmd FROM attempts"])
        .output()
        .expect("failed to run shq sql");
    assert!(output.status.success(), "shq sql failed: {:?}", output);
    String::from_utf8_lossy(&output.stdout).to_string()
}

// ============================================================================
// 1. Default-path exclusion and redaction
// ============================================================================

#[test]
fn test_default_save_excludes_sensitive_command() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // Matches the `export *SECRET*` sensitive pattern: must not be persisted
    // at all on the DEFAULT save path (no --to-buffer).
    save_cmd(tmp.path(), &[], "export AWS_SECRET_ACCESS_KEY=CANARY_EXCLUDED");

    let cmds = stored_commands(tmp.path());
    assert!(
        !cmds.contains("CANARY_EXCLUDED"),
        "secret-bearing command was persisted on the default path:\n{}",
        cmds
    );
}

#[test]
fn test_default_save_redacts_password_value() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // `mysql ...` does not match an exclude pattern, so it IS stored — but
    // the attached password must be redacted.
    save_cmd(tmp.path(), &[], "mysql -pCANARY_PW -u root testdb");

    let cmds = stored_commands(tmp.path());
    assert!(
        !cmds.contains("CANARY_PW"),
        "password value stored verbatim on the default path:\n{}",
        cmds
    );
    assert!(
        cmds.contains("mysql") && cmds.contains("-u root testdb"),
        "redaction should preserve the command structure:\n{}",
        cmds
    );
}

#[test]
fn test_default_save_keeps_benign_command_verbatim() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    save_cmd(tmp.path(), &[], "cargo build --release");

    let cmds = stored_commands(tmp.path());
    assert!(cmds.contains("cargo build --release"), "benign command mangled:\n{}", cmds);
}

#[test]
fn test_force_capture_stores_verbatim() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());

    // `shq -X` is the explicit opt-out from exclusion/redaction.
    let output = shq_cmd(tmp.path())
        .args(["-X", "save", "-c", "export MY_SECRET=CANARY_FORCED", "-x", "0", "-q", "--no-extract"])
        .stdin(Stdio::null())
        .output()
        .expect("failed to run shq -X save");
    assert!(output.status.success(), "forced save failed: {:?}", output);

    let cmds = stored_commands(tmp.path());
    assert!(
        cmds.contains("CANARY_FORCED"),
        "-X/--force-capture should bypass exclusion and redaction:\n{}",
        cmds
    );
}

// ============================================================================
// 2. Storage permissions
// ============================================================================

#[cfg(unix)]
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[cfg(unix)]
#[test]
fn test_data_root_and_db_are_owner_only() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("bird-root");
    init_bird(&root);
    save_cmd(&root, &[], "echo perms test");

    assert_eq!(
        mode_of(&root),
        0o700,
        "data root must be 0700 (stricter than ~/.bash_history's dir)"
    );
    assert_eq!(
        mode_of(&root.join("db/bird.duckdb")),
        0o600,
        "database file must be 0600 (it contains command history)"
    );
    let config_toml = root.join("config.toml");
    if config_toml.exists() {
        assert_eq!(mode_of(&config_toml), 0o600, "config.toml must be 0600");
    }
}

#[cfg(unix)]
#[test]
fn test_parquet_mode_files_are_owner_only() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("bird-root");
    let output = shq_cmd(&root)
        .args(["init", "-m", "parquet"])
        .output()
        .expect("failed to run shq init");
    assert!(output.status.success(), "shq init -m parquet failed: {:?}", output);

    save_cmd(&root, &[], "echo parquet perms test");

    assert_eq!(mode_of(&root), 0o700, "data root must be 0700");

    // Every parquet data file must be 0600.
    let mut parquet_files = Vec::new();
    collect_files_with_ext(&root, "parquet", &mut parquet_files);
    assert!(
        !parquet_files.is_empty(),
        "expected parquet files to be written in parquet mode"
    );
    for f in parquet_files {
        assert_eq!(
            mode_of(&f),
            0o600,
            "parquet file {} must be 0600",
            f.display()
        );
    }
}

fn collect_files_with_ext(dir: &Path, ext: &str, out: &mut Vec<std::path::PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_files_with_ext(&path, ext, out);
            } else if path.extension().map(|e| e == ext).unwrap_or(false) {
                out.push(path);
            }
        }
    }
}

// ============================================================================
// 3. Buffer -> permanent fall-through
// ============================================================================

#[test]
fn test_to_buffer_with_buffer_disabled_discards_output() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());
    // Buffer is disabled by default.

    // Simulate a shell hook whose cached buffer state is stale: it passes
    // --to-buffer with captured output, but the buffer is disabled.
    let mut child = shq_cmd(tmp.path())
        .args(["save", "-c", "echo hook-captured", "--to-buffer", "-x", "0", "--no-extract"])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn shq save");
    child
        .stdin
        .as_mut()
        .unwrap()
        .write_all(b"OUTPUT_CANARY should never reach permanent storage\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "save --to-buffer failed: {:?}", output);

    // The captured output must NOT have been stored permanently.
    let sql = shq_cmd(tmp.path())
        .args([
            "sql",
            "SELECT COUNT(*) AS n FROM outputs o JOIN attempts a ON o.invocation_id = a.id \
             WHERE a.cmd LIKE '%hook-captured%'",
        ])
        .output()
        .expect("failed to run shq sql");
    assert!(sql.status.success(), "shq sql failed: {:?}", sql);
    let stdout = String::from_utf8_lossy(&sql.stdout);
    // Compare the count cell line-wise: the "(1 rows)" footer also contains
    // a digit, so a substring check would false-positive.
    let count_is_zero = stdout.lines().any(|l| l.trim() == "0");
    let count_is_nonzero = stdout.lines().any(|l| {
        let t = l.trim();
        !t.is_empty() && t.chars().all(|c| c.is_ascii_digit()) && t != "0"
    });
    assert!(
        count_is_zero && !count_is_nonzero,
        "buffer-destined output silently fell through to permanent storage:\n{}",
        stdout
    );

    // And `shq show` must not reveal it either.
    let show = shq_cmd(tmp.path())
        .args(["show"])
        .output()
        .expect("failed to run shq show");
    assert!(
        !String::from_utf8_lossy(&show.stdout).contains("OUTPUT_CANARY"),
        "buffer-destined output visible via shq show"
    );
}

// ============================================================================
// 4. Bash hook: space-prefix escape + empty-Enter dedup
// ============================================================================

#[test]
fn test_bash_hook_space_escape_and_dedup() {
    // Needs a real bash.
    if Command::new("bash").arg("--version").output().is_err() {
        eprintln!("skipping: bash not available");
        return;
    }

    let tmp = TempDir::new().unwrap();
    let hook = Command::new(env!("CARGO_BIN_EXE_shq"))
        .args(["hook", "init", "--shell", "bash", "--no-prompt-indicator"])
        .output()
        .expect("failed to generate bash hook");
    assert!(hook.status.success());
    let hook_path = tmp.path().join("hook.sh");
    std::fs::write(&hook_path, &hook.stdout).unwrap();

    let calls_path = tmp.path().join("calls.log");
    let script = format!(
        r#"
# Stub shq: log save invocations, swallow everything else (e.g. buffer status).
shq() {{
    if [ "$1" = save ]; then
        printf 'CALL %s\n' "$*" >> '{calls}'
    fi
}}
# NOTE: no `set -o history` — in a script that would also record the
# driver's own commands (the __shq_prompt_command calls themselves).
# `history -s` manipulates the history list directly regardless.
export HISTCONTROL=
history -s "bootstrap command from a previous session"
source '{hook}'

# A real command
history -s "echo hello"
__shq_prompt_command

# Empty Enter at the prompt: history unchanged, must NOT re-save
__shq_prompt_command

# Space-prefixed command: privacy escape, must NOT be saved
history -s " secret peek"
__shq_prompt_command

# Give the backgrounded (disowned) save subshells time to finish
sleep 1
"#,
        calls = calls_path.display(),
        hook = hook_path.display(),
    );
    let script_path = tmp.path().join("driver.sh");
    std::fs::write(&script_path, script).unwrap();

    let out = Command::new("bash")
        .arg(&script_path)
        .env("BIRD_ROOT", tmp.path())
        .output()
        .expect("failed to run bash driver");
    assert!(
        out.status.success(),
        "bash driver failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let calls = std::fs::read_to_string(&calls_path).unwrap_or_default();
    let hello_saves = calls
        .lines()
        .filter(|l| l.contains("echo hello"))
        .count();
    assert_eq!(
        hello_saves, 1,
        "empty Enter must not re-save the previous command (HISTCMD dedup); calls:\n{}",
        calls
    );
    assert!(
        !calls.contains("secret peek"),
        "space-prefixed command must be suppressed (privacy escape); calls:\n{}",
        calls
    );
    // The bootstrap entry (pre-hook history) must not be saved either.
    assert!(
        !calls.contains("bootstrap command"),
        "hook must not save pre-existing history at startup; calls:\n{}",
        calls
    );
}

// ============================================================================
// 5. Multibyte query input must not panic
// ============================================================================

#[test]
fn test_multibyte_history_query_does_not_panic() {
    let tmp = TempDir::new().unwrap();
    init_bird(tmp.path());
    save_cmd(tmp.path(), &[], "echo hello");

    // '★' is a 3-byte UTF-8 char that used to hit a byte-slice panic in the
    // query parser (`&remaining[1..]`).
    let output = shq_cmd(tmp.path())
        .args(["history", "★"])
        .output()
        .expect("failed to run shq history");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("panicked"),
        "multibyte query input panicked the parser:\n{}",
        stderr
    );
    assert!(
        output.status.success(),
        "shq history with multibyte input failed: {:?}",
        output
    );
}
