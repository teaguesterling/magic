//! CLI command implementations.

use std::io::{self, Read};
use std::process::Command;
use std::time::Instant;

use bird::{init, Config, InvocationRecord, SessionRecord, Store};

/// Generate a session ID for grouping related invocations.
fn session_id() -> String {
    // Use parent PID as session identifier (groups invocations in same shell)
    let ppid = std::os::unix::process::parent_id();
    format!("shell-{}", ppid)
}

/// Get the invoker name (typically the shell or calling process).
fn invoker_name() -> String {
    std::env::var("SHELL")
        .map(|s| {
            std::path::Path::new(&s)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or(s)
        })
        .unwrap_or_else(|_| "unknown".to_string())
}

/// Get the invoker PID (parent process).
fn invoker_pid() -> u32 {
    std::os::unix::process::parent_id()
}

pub fn run(shell_cmd: Option<&str>, cmd_args: &[String]) -> bird::Result<()> {
    use std::io::Write;

    // Determine command string and how to execute
    let (cmd_str, mut command) = match shell_cmd {
        Some(cmd) => {
            // Use $SHELL -c "cmd" (or fallback to sh)
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
            let mut c = Command::new(&shell);
            c.arg("-c").arg(cmd);
            (cmd.to_string(), c)
        }
        None => {
            if cmd_args.is_empty() {
                return Err(bird::Error::Config(
                    "No command specified. Use -c \"cmd\" or provide command args".to_string(),
                ));
            }
            let mut c = Command::new(&cmd_args[0]);
            c.args(&cmd_args[1..]);
            (cmd_args.join(" "), c)
        }
    };

    let config = Config::load()?;
    let store = Store::open(config.clone())?;

    // Get current working directory
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());

    // Execute the command and capture output
    let start = Instant::now();
    let output = command.output();
    let duration_ms = start.elapsed().as_millis() as i64;

    let (exit_code, stdout, stderr) = match &output {
        Ok(o) => (
            o.status.code().unwrap_or(-1),
            o.stdout.clone(),
            o.stderr.clone(),
        ),
        Err(_) => (-1, Vec::new(), Vec::new()),
    };

    // Display output to user
    if !stdout.is_empty() {
        io::stdout().write_all(&stdout)?;
    }
    if !stderr.is_empty() {
        io::stderr().write_all(&stderr)?;
    }

    // Ensure session exists (creates on first invocation from this shell)
    let sid = session_id();
    let session = SessionRecord::new(
        &sid,
        &config.client_id,
        &invoker_name(),
        invoker_pid(),
        "shell",
    );
    store.ensure_session(&session)?;

    // Create and store the invocation record
    let record = InvocationRecord::new(
        &sid,
        &cmd_str,
        &cwd,
        exit_code,
        &config.client_id,
    )
    .with_duration(duration_ms);

    let inv_id = record.id;
    let date = record.date();

    store.write_invocation(&record)?;

    // Store stdout and stderr separately (routes to inline or blob based on size)
    let cmd_hint = record.executable.as_deref();
    if !stdout.is_empty() {
        store.store_output(inv_id, "stdout", &stdout, date, cmd_hint)?;
    }
    if !stderr.is_empty() {
        store.store_output(inv_id, "stderr", &stderr, date, cmd_hint)?;
    }

    // Exit with the same code as the wrapped command
    match output {
        Ok(o) => {
            if !o.status.success() {
                std::process::exit(exit_code);
            }
        }
        Err(e) => {
            eprintln!("shq: failed to execute command: {}", e);
            std::process::exit(127);
        }
    }

    Ok(())
}

/// Save output from stdin or file with an explicit command.
#[allow(clippy::too_many_arguments)]
pub fn save(
    file: Option<&str>,
    command: &str,
    exit_code: i32,
    duration_ms: Option<i64>,
    stream: &str,
    stdout_file: Option<&str>,
    stderr_file: Option<&str>,
    explicit_session_id: Option<&str>,
    explicit_invoker_pid: Option<u32>,
    explicit_invoker: Option<&str>,
    explicit_invoker_type: &str,
) -> bird::Result<()> {
    let config = Config::load()?;
    let store = Store::open(config.clone())?;

    // Get current working directory
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());

    // Use explicit values or fall back to auto-detected
    let sid = explicit_session_id
        .map(|s| s.to_string())
        .unwrap_or_else(session_id);
    let inv_pid = explicit_invoker_pid.unwrap_or_else(invoker_pid);
    let inv_name = explicit_invoker
        .map(|s| s.to_string())
        .unwrap_or_else(invoker_name);

    // Ensure session exists
    let session = SessionRecord::new(
        &sid,
        &config.client_id,
        &inv_name,
        inv_pid,
        explicit_invoker_type,
    );
    store.ensure_session(&session)?;

    // Create invocation record
    let mut inv_record = InvocationRecord::new(
        &sid,
        command,
        &cwd,
        exit_code,
        &config.client_id,
    );
    if let Some(ms) = duration_ms {
        inv_record = inv_record.with_duration(ms);
    }
    let inv_id = inv_record.id;
    let date = inv_record.date();

    store.write_invocation(&inv_record)?;

    let cmd_hint = inv_record.executable.as_deref();

    // Handle --stdout/--stderr mode (separate files for each stream)
    if stdout_file.is_some() || stderr_file.is_some() {
        if let Some(path) = stdout_file {
            let content = std::fs::read(path)?;
            store.store_output(inv_id, "stdout", &content, date, cmd_hint)?;
        }
        if let Some(path) = stderr_file {
            let content = std::fs::read(path)?;
            store.store_output(inv_id, "stderr", &content, date, cmd_hint)?;
        }
    } else {
        // Single stream mode: read from file or stdin
        let content = match file {
            Some(path) => std::fs::read(path)?,
            None => {
                let mut buf = Vec::new();
                io::stdin().read_to_end(&mut buf)?;
                buf
            }
        };
        store.store_output(inv_id, stream, &content, date, cmd_hint)?;
    }

    Ok(())
}

/// Options for the show command.
#[derive(Default)]
pub struct ShowOptions {
    pub pager: bool,
    pub strip_ansi: bool,
    pub head: Option<usize>,
    pub tail: Option<usize>,
}

/// Show output from a previous invocation.
pub fn show(selector: &str, stream_filter: Option<&str>, opts: &ShowOptions) -> bird::Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let config = Config::load()?;
    let store = Store::open(config)?;

    // Normalize stream filter aliases
    let (db_filter, combine_to_stdout) = match stream_filter {
        Some("O") | Some("o") => (Some("stdout"), false),
        Some("E") | Some("e") => (Some("stderr"), false),
        Some("A") | Some("a") | Some("all") => (None, true), // No filter, but combine to stdout
        Some(s) => (Some(s), false),
        None => (None, false), // No filter, route to original streams
    };

    // Parse selector: negative number = offset from end, UUID = direct lookup
    let invocation_id = if let Ok(offset) = selector.parse::<i64>() {
        if offset < 0 {
            // Get Nth invocation from the end
            let n = (-offset) as usize;
            let invocations = store.recent_invocations(n)?;
            if let Some(inv) = invocations.last() {
                inv.id.clone()
            } else {
                eprintln!("No invocation found at offset {}", offset);
                return Ok(());
            }
        } else {
            selector.to_string()
        }
    } else {
        // Assume it's a UUID
        selector.to_string()
    };

    // Get outputs for the invocation (optionally filtered by stream)
    let outputs = store.get_outputs(&invocation_id, db_filter)?;

    if outputs.is_empty() {
        eprintln!("No output found for invocation {}", invocation_id);
        return Ok(());
    }

    // Collect content per stream
    let mut stdout_content = Vec::new();
    let mut stderr_content = Vec::new();
    for output_info in &outputs {
        match store.read_output_content(output_info) {
            Ok(content) => {
                if output_info.stream == "stderr" {
                    stderr_content.extend_from_slice(&content);
                } else {
                    stdout_content.extend_from_slice(&content);
                }
            }
            Err(e) => {
                eprintln!("Failed to read output for stream '{}': {}", output_info.stream, e);
            }
        }
    }

    // Helper to process content (strip ANSI, limit lines)
    let process_content = |content: Vec<u8>| -> String {
        let content = if opts.strip_ansi {
            strip_ansi_escapes(&content)
        } else {
            content
        };

        let content_str = String::from_utf8_lossy(&content);

        if opts.head.is_some() || opts.tail.is_some() {
            let lines: Vec<&str> = content_str.lines().collect();
            let selected: Vec<&str> = if let Some(n) = opts.head {
                lines.into_iter().take(n).collect()
            } else if let Some(n) = opts.tail {
                let skip = lines.len().saturating_sub(n);
                lines.into_iter().skip(skip).collect()
            } else {
                lines
            };
            selected.join("\n") + if content_str.ends_with('\n') { "\n" } else { "" }
        } else {
            content_str.into_owned()
        }
    };

    // Output via pager or directly
    if opts.pager {
        // Combine all content for pager
        let mut all_content = stdout_content;
        all_content.extend_from_slice(&stderr_content);
        let final_content = process_content(all_content);

        let pager_cmd = std::env::var("PAGER").unwrap_or_else(|_| "less -R".to_string());
        let parts: Vec<&str> = pager_cmd.split_whitespace().collect();
        if let Some((cmd, args)) = parts.split_first() {
            let mut child = Command::new(cmd)
                .args(args)
                .stdin(Stdio::piped())
                .spawn()
                .map_err(bird::Error::Io)?;

            if let Some(mut stdin) = child.stdin.take() {
                let _ = stdin.write_all(final_content.as_bytes());
            }
            let _ = child.wait();
        }
    } else if combine_to_stdout {
        // Combine all to stdout
        let mut all_content = stdout_content;
        all_content.extend_from_slice(&stderr_content);
        let final_content = process_content(all_content);
        io::stdout().write_all(final_content.as_bytes())?;
    } else {
        // Route to original streams
        if !stdout_content.is_empty() {
            let content = process_content(stdout_content);
            io::stdout().write_all(content.as_bytes())?;
        }
        if !stderr_content.is_empty() {
            let content = process_content(stderr_content);
            io::stderr().write_all(content.as_bytes())?;
        }
    }

    Ok(())
}

/// Strip ANSI escape codes from bytes.
fn strip_ansi_escapes(input: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == 0x1b && i + 1 < input.len() && input[i + 1] == b'[' {
            // Skip CSI sequence: ESC [ ... final_byte
            i += 2;
            while i < input.len() {
                let c = input[i];
                i += 1;
                if (0x40..=0x7e).contains(&c) {
                    break; // Final byte found
                }
            }
        } else if input[i] == 0x1b && i + 1 < input.len() && input[i + 1] == b']' {
            // Skip OSC sequence: ESC ] ... ST (or BEL)
            i += 2;
            while i < input.len() {
                if input[i] == 0x07 {
                    // BEL terminates OSC
                    i += 1;
                    break;
                } else if input[i] == 0x1b && i + 1 < input.len() && input[i + 1] == b'\\' {
                    // ST (ESC \) terminates OSC
                    i += 2;
                    break;
                }
                i += 1;
            }
        } else {
            output.push(input[i]);
            i += 1;
        }
    }
    output
}

pub fn init() -> bird::Result<()> {
    let config = Config::default_location()?;

    if init::is_initialized(&config) {
        println!("BIRD already initialized at {}", config.bird_root.display());
        return Ok(());
    }

    init::initialize(&config)?;
    println!("BIRD initialized at {}", config.bird_root.display());
    println!("Client ID: {}", config.client_id);

    Ok(())
}

pub fn history(limit: usize) -> bird::Result<()> {
    let config = Config::load()?;
    let store = Store::open(config)?;

    let invocations = store.recent_invocations(limit)?;

    if invocations.is_empty() {
        println!("No invocations recorded yet.");
        return Ok(());
    }

    // Simple table output
    println!("{:<20} {:<6} {:<10} {}", "TIMESTAMP", "EXIT", "DURATION", "COMMAND");
    println!("{}", "-".repeat(80));

    for inv in invocations {
        let duration = inv
            .duration_ms
            .map(|d| format!("{}ms", d))
            .unwrap_or_else(|| "-".to_string());

        // Truncate timestamp to just time portion if today
        let timestamp = if inv.timestamp.len() > 19 {
            &inv.timestamp[11..19]
        } else {
            &inv.timestamp
        };

        // Truncate command if too long
        let cmd_display = if inv.cmd.len() > 50 {
            format!("{}...", &inv.cmd[..47])
        } else {
            inv.cmd.clone()
        };

        println!(
            "{:<20} {:<6} {:<10} {}",
            timestamp, inv.exit_code, duration, cmd_display
        );
    }

    Ok(())
}

pub fn sql(query: &str) -> bird::Result<()> {
    let config = Config::load()?;
    let store = Store::open(config)?;

    let result = store.query(query)?;

    if result.rows.is_empty() {
        println!("No results.");
        return Ok(());
    }

    // Calculate column widths
    let mut widths: Vec<usize> = result.columns.iter().map(|c| c.len()).collect();
    for row in &result.rows {
        for (i, val) in row.iter().enumerate() {
            widths[i] = widths[i].max(val.len().min(50));
        }
    }

    // Print header
    for (i, col) in result.columns.iter().enumerate() {
        print!("{:width$} ", col, width = widths[i]);
    }
    println!();

    // Print separator
    for width in &widths {
        print!("{} ", "-".repeat(*width));
    }
    println!();

    // Print rows
    for row in &result.rows {
        for (i, val) in row.iter().enumerate() {
            let display = if val.len() > 50 {
                format!("{}...", &val[..47])
            } else {
                val.clone()
            };
            print!("{:width$} ", display, width = widths[i]);
        }
        println!();
    }

    println!("\n({} rows)", result.rows.len());

    Ok(())
}

pub fn stats() -> bird::Result<()> {
    let config = Config::load()?;
    let store = Store::open(config.clone())?;

    println!("BIRD Statistics");
    println!("===============");
    println!();
    println!("Root:      {}", config.bird_root.display());
    println!("Client ID: {}", config.client_id);
    println!();

    let inv_count = store.invocation_count()?;
    let session_count = store.session_count()?;
    println!("Total invocations: {}", inv_count);
    println!("Total sessions:    {}", session_count);

    if let Some(last) = store.last_invocation()? {
        println!("Last invocation:   {} (exit {})", last.cmd, last.exit_code);
    }

    Ok(())
}

/// Move old data from recent to archive.
pub fn archive(days: u32, dry_run: bool) -> bird::Result<()> {
    let config = Config::load()?;
    let store = Store::open(config)?;

    if dry_run {
        println!("Dry run - no changes will be made\n");
    }

    let stats = store.archive_old_data(days, dry_run)?;

    if stats.partitions_archived > 0 {
        println!(
            "Archived {} partitions ({} files, {})",
            stats.partitions_archived,
            stats.files_moved,
            format_bytes(stats.bytes_moved)
        );
    } else {
        println!("Nothing to archive.");
    }

    Ok(())
}

/// Compact parquet files to reduce storage and improve performance.
pub fn compact(
    file_threshold: usize,
    session: Option<&str>,
    today_only: bool,
    quiet: bool,
    recent_only: bool,
    archive_only: bool,
    dry_run: bool,
) -> bird::Result<()> {
    let config = Config::load()?;
    let store = Store::open(config)?;

    if dry_run && !quiet {
        println!("Dry run - no changes will be made\n");
    }

    // Session-specific compaction (lightweight, used by shell hooks)
    if let Some(session_id) = session {
        let stats = if today_only {
            store.compact_session_today(session_id, file_threshold, dry_run)?
        } else {
            store.compact_for_session(session_id, file_threshold, dry_run)?
        };

        if stats.sessions_compacted > 0 {
            println!("Compacted session '{}':", session_id);
            println!("  {} files -> {} files", stats.files_before, stats.files_after);
            println!(
                "  {} -> {} ({})",
                format_bytes(stats.bytes_before),
                format_bytes(stats.bytes_after),
                format_reduction(stats.bytes_before, stats.bytes_after)
            );
        } else if !quiet {
            println!("Nothing to compact for session '{}'.", session_id);
        }
        return Ok(());
    }

    // Global compaction
    let mut total_stats = bird::CompactStats::default();

    if !archive_only {
        let stats = store.compact_recent(file_threshold, dry_run)?;
        total_stats.add(&stats);
    }

    if !recent_only {
        let stats = store.compact_archive(file_threshold, dry_run)?;
        total_stats.add(&stats);
    }

    if total_stats.sessions_compacted > 0 {
        println!(
            "Compacted {} sessions across {} partitions",
            total_stats.sessions_compacted, total_stats.partitions_compacted
        );
        println!(
            "  {} files -> {} files",
            total_stats.files_before, total_stats.files_after
        );
        println!(
            "  {} -> {} ({})",
            format_bytes(total_stats.bytes_before),
            format_bytes(total_stats.bytes_after),
            format_reduction(total_stats.bytes_before, total_stats.bytes_after)
        );
    } else if !quiet {
        println!("Nothing to compact.");
    }

    Ok(())
}

/// Format bytes for display.
fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} bytes", bytes)
    }
}

/// Format byte reduction as percentage.
fn format_reduction(before: u64, after: u64) -> String {
    if before == 0 {
        return "0%".to_string();
    }
    if after >= before {
        let increase = ((after - before) as f64 / before as f64) * 100.0;
        format!("+{:.1}%", increase)
    } else {
        let reduction = ((before - after) as f64 / before as f64) * 100.0;
        format!("-{:.1}%", reduction)
    }
}

/// Output shell integration code.
pub fn hook_init(shell: Option<&str>) -> bird::Result<()> {
    // Auto-detect shell from $SHELL if not specified
    let shell_type = shell
        .map(|s| s.to_string())
        .or_else(|| std::env::var("SHELL").ok())
        .map(|s| {
            if s.contains("zsh") {
                "zsh"
            } else if s.contains("bash") {
                "bash"
            } else {
                "unknown"
            }
        })
        .unwrap_or("unknown");

    match shell_type {
        "zsh" => print!("{}", ZSH_HOOK),
        "bash" => print!("{}", BASH_HOOK),
        _ => {
            eprintln!("Unknown shell type. Use --shell zsh or --shell bash");
            std::process::exit(1);
        }
    }

    Ok(())
}

const ZSH_HOOK: &str = r#"# shq shell integration for zsh
# Add to ~/.zshrc: eval "$(shq hook init)"

# Session ID based on this shell's PID (stable across commands)
__shq_session_id="zsh-$$"

# Capture command before execution
__shq_preexec() {
    __shq_last_cmd="$1"
    __shq_start_time=$EPOCHREALTIME
}

# Capture result after execution (metadata only - no output capture)
__shq_precmd() {
    local exit_code=$?
    local cmd="$__shq_last_cmd"

    # Reset for next command
    __shq_last_cmd=""

    # Skip if no command (empty prompt)
    [[ -z "$cmd" ]] && return

    # Skip if command starts with space (privacy escape)
    [[ "$cmd" =~ ^[[:space:]] ]] && return

    # Calculate duration in milliseconds
    local duration=0
    if [[ -n "$__shq_start_time" ]]; then
        duration=$(( (EPOCHREALTIME - __shq_start_time) * 1000 ))
        duration=${duration%.*}  # Truncate decimals
    fi
    __shq_start_time=""

    # Save to BIRD and check compaction (async, non-blocking)
    (
        shq save -c "$cmd" -x "$exit_code" -d "$duration" \
            --session-id "$__shq_session_id" \
            --invoker-pid $$ \
            --invoker zsh \
            </dev/null \
            2>> "${BIRD_ROOT:-$HOME/.local/share/bird}/errors.log"
        # Quick compaction check for this session (today only, quiet)
        shq compact -s "$__shq_session_id" --today -q \
            2>> "${BIRD_ROOT:-$HOME/.local/share/bird}/errors.log"
    ) &!
}

# Run command with full output capture
# Usage: shqr <command> [args...]
# Example: shqr make test
shqr() {
    local cmd="$*"
    local tmpdir=$(mktemp -d)
    local stdout_file="$tmpdir/stdout"
    local stderr_file="$tmpdir/stderr"
    local start_time=$EPOCHREALTIME

    # Run command, capturing output while still displaying to terminal
    # Uses process substitution to tee both streams
    { eval "$cmd" } > >(tee "$stdout_file") 2> >(tee "$stderr_file" >&2)
    local exit_code=${pipestatus[1]:-$?}

    # Calculate duration (ensure integer, default to 0)
    local duration=$(( (EPOCHREALTIME - start_time) * 1000 ))
    duration=${duration%.*}
    duration=${duration:-0}

    # Save to BIRD with captured output
    shq save -c "$cmd" -x "$exit_code" -d "$duration" \
        -o "$stdout_file" -e "$stderr_file" \
        --session-id "$__shq_session_id" \
        --invoker-pid $$ \
        --invoker zsh \
        2>> "${BIRD_ROOT:-$HOME/.local/share/bird}/errors.log"

    # Cleanup
    rm -rf "$tmpdir"

    # Quick compaction check (background, quiet)
    (shq compact -s "$__shq_session_id" --today -q \
        2>> "${BIRD_ROOT:-$HOME/.local/share/bird}/errors.log") &!

    return $exit_code
}

# Register hooks
autoload -Uz add-zsh-hook
add-zsh-hook preexec __shq_preexec
add-zsh-hook precmd __shq_precmd
"#;

const BASH_HOOK: &str = r#"# shq shell integration for bash
# Add to ~/.bashrc: eval "$(shq hook init)"

# Session ID based on this shell's PID (stable across commands)
__shq_session_id="bash-$$"

# Use DEBUG trap for preexec equivalent
__shq_debug() {
    # Skip our own functions
    [[ "$BASH_COMMAND" == "__shq_"* ]] && return
    [[ "$BASH_COMMAND" == "shq "* ]] && return

    __shq_last_cmd="$BASH_COMMAND"
    __shq_start_time=$EPOCHREALTIME
}

# Use PROMPT_COMMAND for precmd equivalent (metadata only - no output capture)
__shq_prompt() {
    local exit_code=$?
    local cmd="$__shq_last_cmd"

    # Reset for next command
    __shq_last_cmd=""

    # Skip if no command
    [[ -z "$cmd" ]] && return

    # Skip if command starts with space (privacy escape)
    [[ "$cmd" =~ ^[[:space:]] ]] && return

    # Calculate duration in milliseconds
    local duration=0
    if [[ -n "$__shq_start_time" ]]; then
        duration=$(echo "($EPOCHREALTIME - $__shq_start_time) * 1000" | bc 2>/dev/null || echo 0)
        duration=${duration%.*}
    fi
    __shq_start_time=""

    # Save to BIRD and check compaction (async, background)
    (
        shq save -c "$cmd" -x "$exit_code" -d "$duration" \
            --session-id "$__shq_session_id" \
            --invoker-pid $$ \
            --invoker bash \
            </dev/null \
            2>> "${BIRD_ROOT:-$HOME/.local/share/bird}/errors.log"
        # Quick compaction check for this session (today only, quiet)
        shq compact -s "$__shq_session_id" --today -q \
            2>> "${BIRD_ROOT:-$HOME/.local/share/bird}/errors.log"
    ) &
    disown 2>/dev/null
}

# Run command with full output capture
# Usage: shqr <command> [args...]
# Example: shqr make test
shqr() {
    local cmd="$*"
    local tmpdir=$(mktemp -d)
    local stdout_file="$tmpdir/stdout"
    local stderr_file="$tmpdir/stderr"
    local start_time=$EPOCHREALTIME

    # Run command, capturing output while still displaying to terminal
    # Uses process substitution to tee both streams
    { eval "$cmd" ; } > >(tee "$stdout_file") 2> >(tee "$stderr_file" >&2)
    local exit_code=${PIPESTATUS[0]:-$?}

    # Calculate duration (ensure integer, default to 0)
    local duration=$(echo "($EPOCHREALTIME - $start_time) * 1000" | bc 2>/dev/null || echo 0)
    duration=${duration%.*}
    duration=${duration:-0}

    # Save to BIRD with captured output
    shq save -c "$cmd" -x "$exit_code" -d "$duration" \
        -o "$stdout_file" -e "$stderr_file" \
        --session-id "$__shq_session_id" \
        --invoker-pid $$ \
        --invoker bash \
        2>> "${BIRD_ROOT:-$HOME/.local/share/bird}/errors.log"

    # Cleanup
    rm -rf "$tmpdir"

    # Quick compaction check (background, quiet)
    (shq compact -s "$__shq_session_id" --today -q \
        2>> "${BIRD_ROOT:-$HOME/.local/share/bird}/errors.log") &
    disown 2>/dev/null

    return $exit_code
}

# Register hooks
trap '__shq_debug' DEBUG
PROMPT_COMMAND="__shq_prompt${PROMPT_COMMAND:+; $PROMPT_COMMAND}"
"#;
