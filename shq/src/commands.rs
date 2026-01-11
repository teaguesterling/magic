//! CLI command implementations.

use std::io::{self, Read};
use std::process::Command;
use std::time::Instant;

use bird::{init, parse_query, CompactOptions, Config, EventFilters, InvocationBatch, InvocationRecord, Query, SessionRecord, StorageMode, Store};

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

/// OSC escape sequence that commands can emit to opt out of recording.
/// Format: ESC ] shq;nosave BEL  (or ESC ] shq;nosave ESC \)
const NOSAVE_OSC: &[u8] = b"\x1b]shq;nosave\x07";
const NOSAVE_OSC_ST: &[u8] = b"\x1b]shq;nosave\x1b\\";

/// Check if output contains the nosave marker.
/// Commands can emit this OSC escape sequence to opt out of being recorded.
fn contains_nosave_marker(data: &[u8]) -> bool {
    data.windows(NOSAVE_OSC.len()).any(|w| w == NOSAVE_OSC)
        || data.windows(NOSAVE_OSC_ST.len()).any(|w| w == NOSAVE_OSC_ST)
}

/// Run a command and capture it to BIRD.
///
/// `extract_override`: Some(true) forces extraction, Some(false) disables it, None uses config.
/// `format_override`: Override format detection for event extraction.
/// `auto_compact`: If true, spawn background compaction after saving.
pub fn run(shell_cmd: Option<&str>, cmd_args: &[String], extract_override: Option<bool>, format_override: Option<&str>, auto_compact: bool) -> bird::Result<()> {
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

    // Check for nosave marker - command opted out of recording
    if contains_nosave_marker(&stdout) || contains_nosave_marker(&stderr) {
        // Exit with appropriate code but don't save
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
        return Ok(());
    }

    // Create session and invocation records
    let sid = session_id();
    let session = SessionRecord::new(
        &sid,
        &config.client_id,
        &invoker_name(),
        invoker_pid(),
        "shell",
    );

    let record = InvocationRecord::new(
        &sid,
        &cmd_str,
        &cwd,
        exit_code,
        &config.client_id,
    )
    .with_duration(duration_ms);

    let inv_id = record.id;

    // Build batch with all related records
    let mut batch = InvocationBatch::new(record).with_session(session);

    if !stdout.is_empty() {
        batch = batch.with_output("stdout", stdout);
    }
    if !stderr.is_empty() {
        batch = batch.with_output("stderr", stderr);
    }

    // Write everything atomically
    store.write_batch(&batch)?;

    // Extract events if enabled (via flag or config)
    let should_extract = extract_override.unwrap_or(config.auto_extract);
    if should_extract {
        let count = store.extract_events(&inv_id.to_string(), format_override)?;
        if count > 0 {
            eprintln!("shq: extracted {} events", count);
        }
    }

    // Spawn background compaction if requested
    if auto_compact {
        let session_id = sid.clone();
        // Spawn shq compact in background
        let _ = Command::new(std::env::current_exe().unwrap_or_else(|_| "shq".into()))
            .args(["compact", "-s", &session_id, "--today", "-q"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
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
    extract: bool,
    compact: bool,
    quiet: bool,
) -> bird::Result<()> {
    use std::process::Command;

    // Read content first so we can check for nosave marker
    let (stdout_content, stderr_content, single_content) = if stdout_file.is_some() || stderr_file.is_some() {
        let stdout = stdout_file.map(std::fs::read).transpose()?;
        let stderr = stderr_file.map(std::fs::read).transpose()?;
        (stdout, stderr, None)
    } else {
        let content = match file {
            Some(path) => std::fs::read(path)?,
            None => {
                let mut buf = Vec::new();
                io::stdin().read_to_end(&mut buf)?;
                buf
            }
        };
        (None, None, Some(content))
    };

    // Check for nosave marker - command opted out of recording
    let has_nosave = stdout_content.as_ref().map_or(false, |c| contains_nosave_marker(c))
        || stderr_content.as_ref().map_or(false, |c| contains_nosave_marker(c))
        || single_content.as_ref().map_or(false, |c| contains_nosave_marker(c));

    if has_nosave {
        return Ok(());
    }

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

    // Create session and invocation records
    let session = SessionRecord::new(
        &sid,
        &config.client_id,
        &inv_name,
        inv_pid,
        explicit_invoker_type,
    );

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

    // Build batch with all related records
    let mut batch = InvocationBatch::new(inv_record).with_session(session);

    if let Some(content) = stdout_content {
        batch = batch.with_output("stdout", content);
    }
    if let Some(content) = stderr_content {
        batch = batch.with_output("stderr", content);
    }
    if let Some(content) = single_content {
        batch = batch.with_output(stream, content);
    }

    // Write everything atomically
    store.write_batch(&batch)?;

    // Extract events if requested (uses config default or explicit flag)
    let should_extract = extract || config.auto_extract;
    if should_extract {
        let count = store.extract_events(&inv_id.to_string(), None)?;
        if !quiet && count > 0 {
            eprintln!("shq: extracted {} events", count);
        }
    }

    // Spawn background compaction if requested
    if compact {
        let session_id = sid.clone();
        let _ = Command::new(std::env::current_exe().unwrap_or_else(|_| "shq".into()))
            .args(["compact", "-s", &session_id, "--today", "-q"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }

    Ok(())
}

/// Options for the output command.
#[derive(Default)]
pub struct OutputOptions {
    pub pager: bool,
    pub strip_ansi: bool,
    pub head: Option<usize>,
    pub tail: Option<usize>,
}

/// Show captured output from invocation(s).
pub fn output(query_str: &str, stream_filter: Option<&str>, opts: &OutputOptions) -> bird::Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let config = Config::load()?;
    let store = Store::open(config)?;

    // Parse query
    let query = parse_query(query_str);

    // Normalize stream filter aliases
    let (db_filter, combine_to_stdout) = match stream_filter {
        Some("O") | Some("o") => (Some("stdout"), false),
        Some("E") | Some("e") => (Some("stderr"), false),
        Some("A") | Some("a") | Some("all") => (None, true), // No filter, but combine to stdout
        Some(s) => (Some(s), false),
        None => (None, false), // No filter, route to original streams
    };

    // Resolve invocation(s) from query
    let invocation_id = match resolve_query_to_invocation(&store, &query) {
        Ok(id) => id,
        Err(bird::Error::NotFound(_)) => {
            eprintln!("No matching invocation found");
            return Ok(());
        }
        Err(e) => return Err(e),
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

pub fn init(mode: &str) -> bird::Result<()> {
    // Parse storage mode
    let storage_mode: StorageMode = mode.parse()?;

    let mut config = Config::default_location()?;

    if init::is_initialized(&config) {
        println!("BIRD already initialized at {}", config.bird_root.display());
        return Ok(());
    }

    // Set storage mode before initialization
    config.storage_mode = storage_mode;

    init::initialize(&config)?;
    println!("BIRD initialized at {}", config.bird_root.display());
    println!("Client ID: {}", config.client_id);
    println!("Storage mode: {}", config.storage_mode);

    Ok(())
}

/// Update DuckDB extensions to latest versions.
pub fn update_extensions(dry_run: bool) -> bird::Result<()> {
    let config = Config::load()?;
    let store = Store::open(config)?;

    let extensions = [
        ("scalarfs", "data: URL support for inline blobs"),
        ("duck_hunt", "log/output parsing for event extraction"),
    ];

    if dry_run {
        println!("Would update the following extensions:");
        for (name, desc) in &extensions {
            println!("  {} - {}", name, desc);
        }
        return Ok(());
    }

    println!("Updating DuckDB extensions...\n");

    for (name, desc) in &extensions {
        print!("  {} ({})... ", name, desc);
        match store.query(&format!("FORCE INSTALL {} FROM community", name)) {
            Ok(_) => {
                // Reload the extension
                match store.query(&format!("LOAD {}", name)) {
                    Ok(_) => println!("updated"),
                    Err(e) => println!("installed but failed to load: {}", e),
                }
            }
            Err(e) => println!("failed: {}", e),
        }
    }

    println!("\nExtensions updated. New features available:");
    println!("  - duck_hunt: compression support (.gz/.zst), duck_hunt_detect_format(),");
    println!("               duck_hunt_diagnose_read(), severity_threshold parameter");

    Ok(())
}

/// List invocation history.
pub fn invocations(query_str: &str, format: &str) -> bird::Result<()> {
    let config = Config::load()?;
    let store = Store::open(config)?;

    // Parse query to get limit from range
    let query = parse_query(query_str);
    let limit = query.range.map(|r| r.start).unwrap_or(20);

    // TODO: Apply full query filters (source, path, field filters)
    let invocations = store.recent_invocations(limit)?;

    if invocations.is_empty() {
        println!("No invocations recorded yet.");
        return Ok(());
    }

    match format {
        "json" => {
            // JSON output
            println!("[");
            for (i, inv) in invocations.iter().enumerate() {
                let comma = if i < invocations.len() - 1 { "," } else { "" };
                println!(
                    r#"  {{"id": "{}", "timestamp": "{}", "cmd": "{}", "exit_code": {}, "duration_ms": {}}}{}"#,
                    inv.id,
                    inv.timestamp,
                    inv.cmd.replace('\\', "\\\\").replace('"', "\\\""),
                    inv.exit_code,
                    inv.duration_ms.unwrap_or(0),
                    comma
                );
            }
            println!("]");
        }
        "oneline" => {
            // One-line per invocation
            for inv in &invocations {
                let duration = inv.duration_ms.map(|d| format!("{}ms", d)).unwrap_or_else(|| "-".to_string());
                println!("{} [{}] {} {}", &inv.id[..8], inv.exit_code, duration, inv.cmd);
            }
        }
        _ => {
            // Table output (default)
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
        }
    }

    Ok(())
}

/// Show quick reference for commands and query syntax.
pub fn quick_help() -> bird::Result<()> {
    print!("{}", QUICK_HELP);
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
pub fn archive(days: u32, dry_run: bool, extract_first: bool) -> bird::Result<()> {
    let config = Config::load()?;
    let store = Store::open(config)?;

    if dry_run {
        println!("Dry run - no changes will be made\n");
    }

    // Optionally extract events from invocations before archiving
    if extract_first && !dry_run {
        println!("Extracting events from invocations to be archived...");
        let cutoff_date = chrono::Utc::now().date_naive() - chrono::Duration::days(days as i64);
        let invocations = store.invocations_without_events(Some(cutoff_date), None)?;

        if !invocations.is_empty() {
            let mut total_events = 0;
            for inv in &invocations {
                let count = store.extract_events(&inv.id, None)?;
                total_events += count;
            }
            println!(
                "  Extracted {} events from {} invocations",
                total_events,
                invocations.len()
            );
        }
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
#[allow(clippy::too_many_arguments)]
pub fn compact(
    file_threshold: usize,
    recompact_threshold: usize,
    consolidate: bool,
    extract_first: bool,
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

    // Extract events first if requested
    if extract_first && !dry_run {
        if !quiet {
            println!("Extracting events from invocations before compacting...");
        }
        let invocations = store.invocations_without_events(None, None)?;
        let mut extracted = 0;
        for inv in &invocations {
            let count = store.extract_events(&inv.id, None)?;
            extracted += count;
        }
        if !quiet && extracted > 0 {
            println!("  Extracted {} events from {} invocations\n", extracted, invocations.len());
        }
    }

    let opts = CompactOptions {
        file_threshold,
        recompact_threshold,
        consolidate,
        dry_run,
        session_filter: session.map(|s| s.to_string()),
    };

    // Session-specific compaction (lightweight, used by shell hooks)
    if let Some(session_id) = session {
        let stats = if today_only {
            // today_only uses legacy API (no recompact support for shell hooks)
            store.compact_session_today(session_id, file_threshold, dry_run)?
        } else {
            store.compact_for_session_with_opts(session_id, &opts)?
        };

        if stats.sessions_compacted > 0 {
            let action = if consolidate { "Consolidated" } else { "Compacted" };
            println!("{} session '{}':", action, session_id);
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
        let stats = store.compact_recent_with_opts(&opts)?;
        total_stats.add(&stats);
    }

    if !recent_only {
        let stats = store.compact_archive_with_opts(&opts)?;
        total_stats.add(&stats);
    }

    if total_stats.sessions_compacted > 0 {
        let action = if consolidate { "Consolidated" } else { "Compacted" };
        println!(
            "{} {} sessions across {} partitions",
            action, total_stats.sessions_compacted, total_stats.partitions_compacted
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

/// Order for limiting results.
#[derive(Clone, Copy, Debug)]
pub enum LimitOrder {
    Any,   // Just limit, no specific order
    First, // First N (head)
    Last,  // Last N (tail)
}

/// Query parsed events from invocation outputs.
pub fn events(
    query_str: &str,
    severity: Option<&str>,
    count_only: bool,
    limit: usize,
    order: LimitOrder,
    reparse: bool,
    format: Option<&str>,
) -> bird::Result<()> {
    let config = Config::load()?;
    let store = Store::open(config)?;

    // Parse query to get limit from range
    let query = parse_query(query_str);
    let n = query.range.map(|r| r.start).unwrap_or(10);

    // Handle reparse mode: re-extract events from outputs
    if reparse {
        let invocations = store.recent_invocations(n)?;
        let mut total_events = 0;

        for inv in &invocations {
            // Delete existing events for this invocation
            store.delete_events_for_invocation(&inv.id)?;
            // Re-extract
            let count = store.extract_events(&inv.id, format)?;
            total_events += count;
        }

        println!(
            "Re-extracted {} events from {} invocations",
            total_events,
            invocations.len()
        );
        return Ok(());
    }

    // Get invocations matching query
    // TODO: Apply full query filters (source, path, field filters)
    let invocations = store.recent_invocations(n)?;
    if invocations.is_empty() {
        println!("No invocations found.");
        return Ok(());
    }

    // Build filters
    let inv_ids: Vec<String> = invocations.iter().map(|inv| inv.id.clone()).collect();
    let filters = EventFilters {
        severity: severity.map(|s| s.to_string()),
        invocation_ids: Some(inv_ids),
        limit: Some(limit),
        // TODO: Add order support to EventFilters when needed
        ..Default::default()
    };
    let _ = order; // Will be used when EventFilters supports ordering

    // Count only mode
    if count_only {
        let count = store.event_count(&filters)?;
        println!("{}", count);
        return Ok(());
    }

    // Query events
    let events = store.query_events(&filters)?;

    if events.is_empty() {
        println!("No events found.");
        return Ok(());
    }

    // Display events
    println!(
        "{:<8} {:<40} {:<30} {}",
        "SEVERITY", "FILE:LINE", "CODE", "MESSAGE"
    );
    println!("{}", "-".repeat(100));

    for event in &events {
        let sev = event.severity.as_deref().unwrap_or("-");
        let location = match (&event.ref_file, event.ref_line) {
            (Some(f), Some(l)) => format!("{}:{}", truncate_path(f, 35), l),
            (Some(f), None) => truncate_path(f, 40).to_string(),
            _ => "-".to_string(),
        };
        let code = event
            .error_code
            .as_deref()
            .or(event.test_name.as_deref())
            .unwrap_or("-");
        let message = event
            .message
            .as_deref()
            .map(|m| truncate_string(m, 50))
            .unwrap_or_else(|| "-".to_string());

        // Color based on severity
        let severity_display = match sev {
            "error" => format!("\x1b[31m{:<8}\x1b[0m", sev),
            "warning" => format!("\x1b[33m{:<8}\x1b[0m", sev),
            _ => format!("{:<8}", sev),
        };

        println!(
            "{} {:<40} {:<30} {}",
            severity_display, location, code, message
        );
    }

    println!("\n({} events)", events.len());

    Ok(())
}

/// Extract events from an invocation's output.
pub fn extract_events(
    selector: &str,
    format: Option<&str>,
    quiet: bool,
    force: bool,
    all: bool,
    since: Option<&str>,
    limit: Option<usize>,
    dry_run: bool,
) -> bird::Result<()> {
    let config = Config::load()?;
    let store = Store::open(config)?;

    // Backfill mode: extract from all invocations without events
    if all {
        return extract_events_backfill(&store, format, quiet, since, limit, dry_run);
    }

    // Single invocation mode
    let invocation_id = resolve_invocation_id(&store, selector)?;

    // Check if events already exist
    let existing_count = store.event_count(&EventFilters {
        invocation_id: Some(invocation_id.clone()),
        ..Default::default()
    })?;

    if existing_count > 0 && !force {
        if !quiet {
            println!(
                "Events already exist for invocation {} ({} events). Use --force to re-extract.",
                invocation_id, existing_count
            );
        }
        return Ok(());
    }

    // Delete existing events if forcing
    if force && existing_count > 0 {
        store.delete_events_for_invocation(&invocation_id)?;
    }

    // Extract events
    let count = store.extract_events(&invocation_id, format)?;

    if !quiet {
        if count > 0 {
            println!("Extracted {} events from invocation {}", count, invocation_id);
        } else {
            println!("No events found in invocation {}", invocation_id);
        }
    }

    Ok(())
}

/// Backfill events from all invocations that don't have events yet.
fn extract_events_backfill(
    store: &Store,
    format: Option<&str>,
    quiet: bool,
    since: Option<&str>,
    limit: Option<usize>,
    dry_run: bool,
) -> bird::Result<()> {
    use chrono::NaiveDate;

    // Parse since date if provided
    let since_date = if let Some(date_str) = since {
        Some(
            NaiveDate::parse_from_str(date_str, "%Y-%m-%d")
                .map_err(|e| bird::Error::Config(format!("Invalid date '{}': {}", date_str, e)))?,
        )
    } else {
        None
    };

    // Get invocations without events
    let invocations = store.invocations_without_events(since_date, limit)?;

    if invocations.is_empty() {
        if !quiet {
            println!("No invocations found without events.");
        }
        return Ok(());
    }

    if dry_run {
        println!("Would extract events from {} invocations:", invocations.len());
        for inv in &invocations {
            let cmd_preview: String = inv.cmd.chars().take(60).collect();
            let suffix = if inv.cmd.len() > 60 { "..." } else { "" };
            println!("  {} {}{}", &inv.id[..8], cmd_preview, suffix);
        }
        return Ok(());
    }

    let mut total_events = 0;
    let mut processed = 0;

    for inv in &invocations {
        let count = store.extract_events(&inv.id, format)?;
        total_events += count;
        processed += 1;

        if !quiet && count > 0 {
            println!("  {} events from: {}", count, truncate_cmd(&inv.cmd, 50));
        }
    }

    if !quiet {
        println!(
            "Extracted {} events from {} invocations.",
            total_events, processed
        );
    }

    Ok(())
}

/// Truncate a command string for display.
fn truncate_cmd(cmd: &str, max_len: usize) -> String {
    if cmd.len() <= max_len {
        cmd.to_string()
    } else {
        format!("{}...", &cmd[..max_len])
    }
}

/// Resolve a selector (negative offset or UUID) to an invocation ID.
fn resolve_invocation_id(store: &Store, selector: &str) -> bird::Result<String> {
    if let Ok(offset) = selector.parse::<i64>() {
        if offset < 0 {
            let n = (-offset) as usize;
            let invocations = store.recent_invocations(n)?;
            if let Some(inv) = invocations.last() {
                return Ok(inv.id.clone());
            } else {
                return Err(bird::Error::NotFound(format!(
                    "No invocation found at offset {}",
                    offset
                )));
            }
        }
    }
    // Assume it's a UUID
    Ok(selector.to_string())
}

/// Truncate a path for display, keeping the filename visible.
fn truncate_path(path: &str, max_len: usize) -> &str {
    if path.len() <= max_len {
        return path;
    }
    // Try to keep at least the filename
    if let Some(pos) = path.rfind('/') {
        let filename = &path[pos + 1..];
        if filename.len() < max_len {
            return &path[path.len() - max_len..];
        }
    }
    &path[path.len() - max_len..]
}

/// Truncate a string for display.
fn truncate_string(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len - 3])
    }
}

/// Resolve a query to a single invocation ID.
fn resolve_query_to_invocation(store: &Store, query: &Query) -> bird::Result<String> {
    // For now, use range as offset (e.g., ~1 = last command)
    let n = query.range.map(|r| r.start).unwrap_or(1);

    // TODO: Apply full query filters (source, path, field filters)
    let invocations = store.recent_invocations(n)?;

    if let Some(inv) = invocations.last() {
        Ok(inv.id.clone())
    } else {
        Err(bird::Error::NotFound("No matching invocation found".to_string()))
    }
}

/// Show detailed info about an invocation.
pub fn info(query_str: &str, format: &str) -> bird::Result<()> {
    let config = Config::load()?;
    let store = Store::open(config)?;

    // Parse query and resolve to invocation
    let query = parse_query(query_str);
    let invocation_id = resolve_query_to_invocation(&store, &query)?;

    // Get full invocation details via SQL
    let result = store.query(&format!(
        "SELECT id, cmd, cwd, exit_code, timestamp, duration_ms, session_id
         FROM read_parquet('{}/**/invocations/*.parquet')
         WHERE id = '{}'",
        store.config().bird_root.display(),
        invocation_id
    ))?;

    if result.rows.is_empty() {
        return Err(bird::Error::NotFound(format!("Invocation {} not found", invocation_id)));
    }

    let row = &result.rows[0];
    let id = &row[0];
    let cmd = &row[1];
    let cwd = &row[2];
    let exit_code = &row[3];
    let timestamp = &row[4];
    let duration_ms = &row[5];
    let session_id = &row[6];

    // Get output info
    let outputs = store.get_outputs(&invocation_id, None)?;
    let stdout_size: i64 = outputs.iter().filter(|o| o.stream == "stdout").map(|o| o.byte_length).sum();
    let stderr_size: i64 = outputs.iter().filter(|o| o.stream == "stderr").map(|o| o.byte_length).sum();

    // Get event count
    let event_count = store.event_count(&EventFilters {
        invocation_id: Some(invocation_id.clone()),
        ..Default::default()
    })?;

    match format {
        "json" => {
            println!(r#"{{"#);
            println!(r#"  "id": "{}","#, id);
            println!(r#"  "timestamp": "{}","#, timestamp);
            println!(r#"  "cmd": "{}","#, cmd.replace('\\', "\\\\").replace('"', "\\\""));
            println!(r#"  "cwd": "{}","#, cwd.replace('\\', "\\\\").replace('"', "\\\""));
            println!(r#"  "exit_code": {},"#, exit_code);
            println!(r#"  "duration_ms": {},"#, duration_ms);
            println!(r#"  "session_id": "{}","#, session_id);
            println!(r#"  "stdout_bytes": {},"#, stdout_size);
            println!(r#"  "stderr_bytes": {},"#, stderr_size);
            println!(r#"  "event_count": {}"#, event_count);
            println!(r#"}}"#);
        }
        _ => {
            // Table format
            println!("Invocation Details");
            println!("==================");
            println!();
            println!("ID:          {}", id);
            println!("Timestamp:   {}", timestamp);
            println!("Command:     {}", cmd);
            println!("Working Dir: {}", cwd);
            println!("Exit Code:   {}", exit_code);
            println!("Duration:    {}ms", duration_ms);
            println!("Session:     {}", session_id);
            println!();
            println!("Output:");
            println!("  stdout:    {} bytes", stdout_size);
            println!("  stderr:    {} bytes", stderr_size);
            println!("  events:    {}", event_count);
        }
    }

    Ok(())
}

/// Re-run a previous command.
pub fn rerun(query_str: &str, dry_run: bool, no_capture: bool) -> bird::Result<()> {
    use std::io::Write;

    let config = Config::load()?;
    let store = Store::open(config)?;

    // Parse query and resolve to invocation
    let query = parse_query(query_str);
    let invocation_id = resolve_query_to_invocation(&store, &query)?;

    // Get full invocation details via SQL (need cmd and cwd)
    let result = store.query(&format!(
        "SELECT cmd, cwd FROM read_parquet('{}/**/invocations/*.parquet') WHERE id = '{}'",
        store.config().bird_root.display(),
        invocation_id
    ))?;

    if result.rows.is_empty() {
        return Err(bird::Error::NotFound(format!("Invocation {} not found", invocation_id)));
    }

    let cmd = &result.rows[0][0];
    let cwd = &result.rows[0][1];

    if dry_run {
        println!("Would run: {}", cmd);
        println!("In directory: {}", cwd);
        return Ok(());
    }

    // Print the command being re-run
    eprintln!("\x1b[2m$ {}\x1b[0m", cmd);

    if no_capture {
        // Just execute without capturing
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
        let status = Command::new(&shell)
            .arg("-c")
            .arg(cmd)
            .current_dir(cwd)
            .status()?;

        if !status.success() {
            std::process::exit(status.code().unwrap_or(1));
        }
    } else {
        // Use shq run to capture the command
        let start = std::time::Instant::now();
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
        let output = Command::new(&shell)
            .arg("-c")
            .arg(cmd)
            .current_dir(cwd)
            .output()?;
        let duration_ms = start.elapsed().as_millis() as i64;

        // Display output
        if !output.stdout.is_empty() {
            io::stdout().write_all(&output.stdout)?;
        }
        if !output.stderr.is_empty() {
            io::stderr().write_all(&output.stderr)?;
        }

        let exit_code = output.status.code().unwrap_or(-1);

        // Save to BIRD
        let config = Config::load()?;
        let store = Store::open(config.clone())?;
        let sid = session_id();

        let session = SessionRecord::new(
            &sid,
            &config.client_id,
            &invoker_name(),
            invoker_pid(),
            "shell",
        );

        let record = InvocationRecord::new(
            &sid,
            cmd,
            cwd,
            exit_code,
            &config.client_id,
        )
        .with_duration(duration_ms);

        // Build batch with all related records
        let mut batch = InvocationBatch::new(record).with_session(session);

        if !output.stdout.is_empty() {
            batch = batch.with_output("stdout", output.stdout.clone());
        }
        if !output.stderr.is_empty() {
            batch = batch.with_output("stderr", output.stderr.clone());
        }

        // Write everything atomically
        store.write_batch(&batch)?;

        if !output.status.success() {
            std::process::exit(exit_code);
        }
    }

    Ok(())
}

const QUICK_HELP: &str = r#"
SHQ QUICK REFERENCE
===================

COMMANDS                                    EXAMPLES
────────────────────────────────────────────────────────────────────────────────
output (o, show)   Show captured output     shq o ~1          shq o %/make/~1
invocations (i)    List command history     shq i ~20         shq i %exit<>0~10
events (e)         Show parsed events       shq e ~10         shq e -s error ~5
info (I)           Invocation details       shq I ~1          shq I %/test/~1
rerun (R, !!)      Re-run a command         shq R ~1          shq R %/make/~1
run (r)            Run and capture          shq r cargo test  shq r -c "make all"
sql (q)            Execute SQL query        shq q "SELECT * FROM invocations LIMIT 5"

QUERY SYNTAX: [source][path][filters][range]
────────────────────────────────────────────────────────────────────────────────
RANGE         1 or ~1     Last command
              5 or ~5     Last 5 commands
              ~10:5       Commands 10 to 5 ago

SOURCE        shell:      Shell commands on this host
              shell:zsh:  Zsh shells only
              *:*:*:*:    Everything everywhere

PATH          .           Current directory
              ~/Projects/ Home-relative
              /tmp/       Absolute path

FILTERS       %exit<>0    Non-zero exit code
              %exit=0     Successful commands only
              %duration>5000   Took > 5 seconds
              %cmd~=test  Command matches regex
              %cwd~=/src/ Working dir matches

CMD REGEX     %/make/     Commands containing "make"
              %/^cargo/   Commands starting with "cargo"
              %/test$/    Commands ending with "test"

OPERATORS     =  equals      <>  not equals     ~=  regex match
              >  greater     <   less           >=  gte    <=  lte

EXAMPLES
────────────────────────────────────────────────────────────────────────────────
shq o                        Show output of last command (default: 1)
shq o 1                      Same as above
shq o -E 1                   Show only stderr of last command
shq o %/make/~1              Output of last make command
shq o %exit<>0~1             Output of last failed command

shq i                        Last 20 commands (default)
shq i 50                     Last 50 commands
shq i %exit<>0~20            Last 20 failed commands
shq i %duration>10000~10     Last 10 commands that took >10s
shq i %/cargo/~10            Last 10 cargo commands

shq e                        Events from last 10 commands (default)
shq e 5                      Events from last 5 commands
shq e -s error 10            Only errors from last 10 commands
shq e %/cargo build/~1       Events from last cargo build

shq R                        Re-run last command
shq R 3                      Re-run 3rd-last command
shq R %/make test/~1         Re-run last "make test"
shq R -n %/deploy/~1         Dry-run: show what would run

shq I                        Details about last command
shq I -f json 1              Details as JSON

.~5                          Last 5 commands in current directory
~/Projects/foo/~10           Last 10 in ~/Projects/foo/
shell:%exit<>0~5             Last 5 failed shell commands

"#;

const ZSH_HOOK: &str = r#"# shq shell integration for zsh
# Add to ~/.zshrc: eval "$(shq hook init)"
#
# Privacy escapes (command not recorded):
#   - Start command with a space: " ls -la"
#   - Start command with backslash: "\ls -la"
#
# Temporary disable: export SHQ_DISABLED=1
# Exclude patterns: export SHQ_EXCLUDE="*password*:*secret*"

# Session ID based on this shell's PID (stable across commands)
__shq_session_id="zsh-$$"

# Check if command matches any exclude pattern
__shq_excluded() {
    [[ -z "$SHQ_EXCLUDE" ]] && return 1
    local cmd="$1"
    local IFS=':'
    for pattern in $SHQ_EXCLUDE; do
        [[ "$cmd" == $~pattern ]] && return 0
    done
    return 1
}

# Check if command is a shq/blq query (read-only) command - don't record these
__shq_is_query() {
    local cmd="$1"
    # Skip shq query commands (output, show, invocations, history, info, events, stats, sql, quick-help)
    [[ "$cmd" =~ ^shq[[:space:]]+(output|show|o|invocations|history|i|info|I|events|e|stats|sql|q|quick-help|\?)[[:space:]]*  ]] && return 0
    [[ "$cmd" =~ ^shq[[:space:]]+(output|show|o|invocations|history|i|info|I|events|e|stats|sql|q|quick-help|\?)$ ]] && return 0
    # Skip blq query commands
    [[ "$cmd" =~ ^blq[[:space:]]+(show|list|errors|context|stats)[[:space:]]* ]] && return 0
    [[ "$cmd" =~ ^blq[[:space:]]+(show|list|errors|context|stats)$ ]] && return 0
    return 1
}

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

    # Skip if disabled
    [[ -n "$SHQ_DISABLED" ]] && return

    # Skip if no command (empty prompt)
    [[ -z "$cmd" ]] && return

    # Skip if command starts with space (privacy escape)
    [[ "$cmd" =~ ^[[:space:]] ]] && return

    # Skip if command starts with backslash (privacy escape)
    [[ "$cmd" =~ ^\\ ]] && return

    # Skip if command matches exclude pattern
    __shq_excluded "$cmd" && return

    # Skip shq/blq query commands (prevent recursive recording)
    __shq_is_query "$cmd" && return

    # Calculate duration in milliseconds
    local duration=0
    if [[ -n "$__shq_start_time" ]]; then
        duration=$(( (EPOCHREALTIME - __shq_start_time) * 1000 ))
        duration=${duration%.*}  # Truncate decimals
    fi
    __shq_start_time=""

    # Save to BIRD with inline extraction and compaction (async, non-blocking)
    (
        shq save -c "$cmd" -x "$exit_code" -d "$duration" \
            --session-id "$__shq_session_id" \
            --invoker-pid $$ \
            --invoker zsh \
            --extract --compact -q \
            </dev/null \
            2>> "${BIRD_ROOT:-$HOME/.local/share/bird}/errors.log"
    ) &!
}

# Run command with full output capture
# Usage: shqr <command> [args...]
# Example: shqr make test
shqr() {
    # Check if disabled
    if [[ -n "$SHQ_DISABLED" ]]; then
        eval "$*"
        return $?
    fi

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

    # Save to BIRD with captured output, extraction, and compaction
    shq save -c "$cmd" -x "$exit_code" -d "$duration" \
        -o "$stdout_file" -e "$stderr_file" \
        --session-id "$__shq_session_id" \
        --invoker-pid $$ \
        --invoker zsh \
        --extract --compact -q \
        2>> "${BIRD_ROOT:-$HOME/.local/share/bird}/errors.log"

    # Cleanup
    rm -rf "$tmpdir"

    return $exit_code
}

# Register hooks
autoload -Uz add-zsh-hook
add-zsh-hook preexec __shq_preexec
add-zsh-hook precmd __shq_precmd
"#;

const BASH_HOOK: &str = r#"# shq shell integration for bash
# Add to ~/.bashrc: eval "$(shq hook init)"
#
# Privacy escapes (command not recorded):
#   - Start command with a space: " ls -la"
#   - Start command with backslash: "\ls -la"
#
# Temporary disable: export SHQ_DISABLED=1
# Exclude patterns: export SHQ_EXCLUDE="*password*:*secret*"

# Session ID based on this shell's PID (stable across commands)
__shq_session_id="bash-$$"

# Check if command matches any exclude pattern
__shq_excluded() {
    [[ -z "$SHQ_EXCLUDE" ]] && return 1
    local cmd="$1"
    local IFS=':'
    for pattern in $SHQ_EXCLUDE; do
        # Use bash pattern matching
        if [[ "$cmd" == $pattern ]]; then
            return 0
        fi
    done
    return 1
}

# Check if command is a shq/blq query (read-only) command - don't record these
__shq_is_query() {
    local cmd="$1"
    # Skip shq query commands (output, show, invocations, history, info, events, stats, sql, quick-help)
    [[ "$cmd" =~ ^shq[[:space:]]+(output|show|o|invocations|history|i|info|I|events|e|stats|sql|q|quick-help|\?)[[:space:]]* ]] && return 0
    [[ "$cmd" =~ ^shq[[:space:]]+(output|show|o|invocations|history|i|info|I|events|e|stats|sql|q|quick-help|\?)$ ]] && return 0
    # Skip blq query commands
    [[ "$cmd" =~ ^blq[[:space:]]+(show|list|errors|context|stats)[[:space:]]* ]] && return 0
    [[ "$cmd" =~ ^blq[[:space:]]+(show|list|errors|context|stats)$ ]] && return 0
    return 1
}

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

    # Skip if disabled
    [[ -n "$SHQ_DISABLED" ]] && return

    # Skip if no command
    [[ -z "$cmd" ]] && return

    # Skip if command starts with space (privacy escape)
    [[ "$cmd" =~ ^[[:space:]] ]] && return

    # Skip if command starts with backslash (privacy escape)
    [[ "$cmd" =~ ^\\ ]] && return

    # Skip if command matches exclude pattern
    __shq_excluded "$cmd" && return

    # Skip shq/blq query commands (prevent recursive recording)
    __shq_is_query "$cmd" && return

    # Calculate duration in milliseconds
    local duration=0
    if [[ -n "$__shq_start_time" ]]; then
        duration=$(echo "($EPOCHREALTIME - $__shq_start_time) * 1000" | bc 2>/dev/null || echo 0)
        duration=${duration%.*}
    fi
    __shq_start_time=""

    # Save to BIRD with inline extraction and compaction (async, background)
    (
        shq save -c "$cmd" -x "$exit_code" -d "$duration" \
            --session-id "$__shq_session_id" \
            --invoker-pid $$ \
            --invoker bash \
            --extract --compact -q \
            </dev/null \
            2>> "${BIRD_ROOT:-$HOME/.local/share/bird}/errors.log"
    ) &
    disown 2>/dev/null
}

# Run command with full output capture
# Usage: shqr <command> [args...]
# Example: shqr make test
shqr() {
    # Check if disabled
    if [[ -n "$SHQ_DISABLED" ]]; then
        eval "$*"
        return $?
    fi

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

    # Save to BIRD with captured output, extraction, and compaction
    shq save -c "$cmd" -x "$exit_code" -d "$duration" \
        -o "$stdout_file" -e "$stderr_file" \
        --session-id "$__shq_session_id" \
        --invoker-pid $$ \
        --invoker bash \
        --extract --compact -q \
        2>> "${BIRD_ROOT:-$HOME/.local/share/bird}/errors.log"

    # Cleanup
    rm -rf "$tmpdir"

    return $exit_code
}

# Register hooks
trap '__shq_debug' DEBUG
PROMPT_COMMAND="__shq_prompt${PROMPT_COMMAND:+; $PROMPT_COMMAND}"
"#;
