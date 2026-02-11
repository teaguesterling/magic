//! CLI command implementations.

use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::process::Command;
use std::time::Instant;

use std::fs::File;

use bird::{
    init, parse_query, CompactOptions, Config, ContextMetadata, EventFilters, InvocationBatch,
    InvocationRecord, Query, SessionRecord, StorageMode, Store, BIRD_INVOCATION_UUID_VAR,
    BIRD_PARENT_CLIENT_VAR,
};
use pty_process::blocking::{Command as PtyCommand, open as pty_open};

/// Streaming output writer that writes to both terminal and a temp file.
///
/// This enables `shq show --follow` to tail output while a command is running.
/// The temp file lives at `~/.bird/running/<invocation_id>.out` during execution.
struct StreamingOutput {
    file: File,
    path: std::path::PathBuf,
}

impl StreamingOutput {
    /// Create a new streaming output file for the given invocation.
    fn new(config: &Config, invocation_id: uuid::Uuid) -> io::Result<Self> {
        let running_dir = config.running_dir();
        std::fs::create_dir_all(&running_dir)?;
        let path = config.running_path(&invocation_id);
        let file = File::create(&path)?;
        Ok(Self { file, path })
    }

    /// Write data to the streaming file.
    fn write(&mut self, data: &[u8]) -> io::Result<()> {
        self.file.write_all(data)?;
        self.file.flush()?; // Flush for real-time tailing
        Ok(())
    }

    /// Read all content from the streaming file and delete it.
    fn finish(self) -> io::Result<Vec<u8>> {
        drop(self.file); // Close the file handle
        let content = std::fs::read(&self.path)?;
        let _ = std::fs::remove_file(&self.path); // Clean up
        Ok(content)
    }

    /// Get the path for external access (e.g., for --follow).
    #[allow(dead_code)]
    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

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
/// By default, uses PTY (pseudo-terminal) which means:
/// - Ctrl-Z suspends the command (not shq)
/// - Interactive programs (vim, less) work correctly
/// - Output is displayed in real-time
/// - stdout/stderr are combined into a single stream
///
/// With `no_pty: true`, uses pipes instead:
/// - stdout/stderr are captured separately
/// - Colors and interactivity are lost
/// - Better for non-interactive batch commands
///
/// `tag`: Optional tag (unique alias) for this invocation.
/// `extract_override`: Some(true) forces extraction, Some(false) disables it, None uses config.
/// `format_override`: Override format detection for event extraction.
/// `auto_compact`: If true, spawn background compaction after saving.
/// `no_pty`: If true, use pipes instead of PTY for separate stdout/stderr capture.
pub fn run(shell_cmd: Option<&str>, cmd_args: &[String], tag: Option<&str>, extract_override: Option<bool>, format_override: Option<&str>, auto_compact: bool, no_pty: bool) -> bird::Result<()> {
    // Determine command string and build PTY command
    let (cmd_str, shell, args): (String, String, Vec<String>) = match shell_cmd {
        Some(cmd) => {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
            (cmd.to_string(), shell, vec!["-c".to_string(), cmd.to_string()])
        }
        None => {
            if cmd_args.is_empty() {
                return Err(bird::Error::Config(
                    "No command specified. Use -c \"cmd\" or provide command args".to_string(),
                ));
            }
            // If single arg with spaces, treat as shell command (common UX pattern)
            if cmd_args.len() == 1 && cmd_args[0].contains(' ') {
                let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
                (cmd_args[0].clone(), shell, vec!["-c".to_string(), cmd_args[0].clone()])
            } else {
                (cmd_args.join(" "), cmd_args[0].clone(), cmd_args[1..].to_vec())
            }
        }
    };

    let config = Config::load()?;
    let store = Store::open(config.clone())?;

    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string());

    let invocation_id = uuid::Uuid::now_v7();

    // Branch based on PTY mode
    if no_pty {
        return run_no_pty(
            &cmd_str, &shell, &args, &cwd, invocation_id,
            tag, extract_override, format_override, auto_compact,
            config, store,
        );
    }

    // Allocate PTY
    let (mut pty, pts) = pty_open().map_err(|e| bird::Error::Io(io::Error::other(e)))?;

    // Try to match terminal size
    if let Ok(size) = terminal_size() {
        let _ = pty.resize(pty_process::Size::new(size.0, size.1));
    }

    // Check if stdin is a terminal - if not, disable PTY echo to prevent duplicates
    let stdin_is_tty = unsafe { libc::isatty(libc::STDIN_FILENO) == 1 };
    if !stdin_is_tty {
        disable_pty_echo(pty.as_raw_fd());
    }

    // Build command to run in PTY (builder pattern takes ownership)
    let cmd = PtyCommand::new(&shell)
        .args(&args)
        .env(BIRD_INVOCATION_UUID_VAR, invocation_id.to_string())
        .env(BIRD_PARENT_CLIENT_VAR, "shq");

    // Spawn process in PTY - it becomes session leader with PTY as controlling terminal
    let start = Instant::now();
    let mut child = cmd.spawn(pts)
        .map_err(|e| bird::Error::Io(io::Error::other(e)))?;

    // Set up raw mode for stdin if it's a terminal
    let orig_termios = if stdin_is_tty {
        set_raw_mode(libc::STDIN_FILENO)
    } else {
        None
    };

    // Clone PTY fd for the stdin forwarding thread
    let pty_write_fd = pty.as_raw_fd();

    // Spawn thread to forward stdin to PTY (both tty and piped modes)
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    let running = Arc::new(AtomicBool::new(true));
    let running_clone = running.clone();

    let stdin_handle = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        let stdin_fd = libc::STDIN_FILENO;

        // Set stdin to non-blocking
        set_nonblocking(stdin_fd, true);

        while running_clone.load(Ordering::Relaxed) {
            let n = unsafe {
                libc::read(stdin_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
            };

            if n > 0 {
                let _ = unsafe {
                    libc::write(pty_write_fd, buf.as_ptr() as *const libc::c_void, n as usize)
                };
            } else if n == 0 {
                // EOF on stdin - send Ctrl-D to signal EOF to child
                let ctrl_d = [4u8]; // ASCII EOT (Ctrl-D)
                let _ = unsafe {
                    libc::write(pty_write_fd, ctrl_d.as_ptr() as *const libc::c_void, 1)
                };
                break;
            } else {
                // EAGAIN/EWOULDBLOCK - no data available
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    });

    // Create streaming output file for real-time tailing via `shq show --follow`
    let mut streaming = StreamingOutput::new(&config, invocation_id)
        .map_err(|e| bird::Error::Io(e))?;

    // Read output from PTY and pass through to our stdout while streaming to file
    let mut buf = [0u8; 4096];

    // Set PTY to non-blocking for reading
    set_nonblocking(pty.as_raw_fd(), true);

    loop {
        // Check if child has exited
        match child.try_wait() {
            Ok(Some(_status)) => {
                // Child exited, drain remaining output
                set_nonblocking(pty.as_raw_fd(), false);
                while let Ok(n) = pty.read(&mut buf) {
                    if n == 0 { break; }
                    let _ = streaming.write(&buf[..n]);
                    let _ = io::stdout().write_all(&buf[..n]);
                    let _ = io::stdout().flush();
                }
                break;
            }
            Ok(None) => {
                // Child still running, read available output
                match pty.read(&mut buf) {
                    Ok(0) => {
                        // EOF - child closed PTY
                        break;
                    }
                    Ok(n) => {
                        let _ = streaming.write(&buf[..n]);
                        let _ = io::stdout().write_all(&buf[..n]);
                        let _ = io::stdout().flush();
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        // No data available, sleep briefly
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(_) => {
                        // Read error, child may have exited
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }

    // Stop stdin forwarding thread
    running.store(false, Ordering::Relaxed);
    let _ = stdin_handle.join();

    // Restore terminal mode
    if let Some(termios) = orig_termios {
        restore_termios(libc::STDIN_FILENO, &termios);
    }

    // Wait for child to fully exit and get status
    let status = child.wait().map_err(|e| bird::Error::Io(io::Error::other(e)))?;
    let duration_ms = start.elapsed().as_millis() as i64;
    let exit_code = status.code().unwrap_or(-1);

    // Finalize streaming output - read content and clean up temp file
    let output_buffer = streaming.finish().unwrap_or_default();

    // Check for nosave marker
    if contains_nosave_marker(&output_buffer) {
        if !status.success() {
            std::process::exit(exit_code);
        }
        return Ok(());
    }

    // Create and save records
    let sid = session_id();
    let session = SessionRecord::new(
        &sid,
        &config.client_id,
        invoker_name(),
        invoker_pid(),
        "shell",
    );

    // Collect context metadata (VCS, CI)
    let context = ContextMetadata::collect(Some(std::path::Path::new(&cwd)));

    let mut record = InvocationRecord::with_id(
        invocation_id,
        &sid,
        &cmd_str,
        &cwd,
        exit_code,
        &config.client_id,
    )
    .with_duration(duration_ms)
    .with_metadata(context.into_map());

    if let Some(t) = tag {
        record = record.with_tag(t);
    }

    let inv_id = record.id;
    let mut batch = InvocationBatch::new(record).with_session(session);

    // PTY merges stdout/stderr into a single stream - store as "combined"
    if !output_buffer.is_empty() {
        batch = batch.with_output("combined", output_buffer);
    }

    store.write_batch(&batch)?;

    // Extract events if enabled
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
        let _ = Command::new(std::env::current_exe().unwrap_or_else(|_| "shq".into()))
            .args(["compact", "-s", &session_id, "--today", "-q"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }

    if !status.success() {
        std::process::exit(exit_code);
    }

    Ok(())
}

/// Run a command without PTY, capturing stdout/stderr separately via pipes.
///
/// This mode loses colors and interactivity but gains separate stream capture.
fn run_no_pty(
    cmd_str: &str,
    shell: &str,
    args: &[String],
    cwd: &str,
    invocation_id: uuid::Uuid,
    tag: Option<&str>,
    extract_override: Option<bool>,
    format_override: Option<&str>,
    auto_compact: bool,
    config: Config,
    store: Store,
) -> bird::Result<()> {
    use std::io::{BufRead, BufReader};
    use std::sync::{mpsc, Arc, Mutex};
    use std::thread;

    // Create streaming output file for real-time tailing via `shq show --follow`
    let streaming = StreamingOutput::new(&config, invocation_id)
        .map_err(|e| bird::Error::Io(e))?;
    let streaming = Arc::new(Mutex::new(streaming));

    // Build command with piped stdout/stderr
    let mut child = Command::new(shell)
        .args(args)
        .env(BIRD_INVOCATION_UUID_VAR, invocation_id.to_string())
        .env(BIRD_PARENT_CLIENT_VAR, "shq")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| bird::Error::Io(e))?;

    let start = Instant::now();

    // Take ownership of stdout/stderr handles
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");

    // Buffers to collect output
    let (tx_stdout, rx) = mpsc::channel::<(bool, Vec<u8>)>(); // (is_stderr, data)
    let tx_stderr = tx_stdout.clone();

    // Clone streaming for threads
    let streaming_stdout = Arc::clone(&streaming);
    let streaming_stderr = Arc::clone(&streaming);

    // Spawn thread to read stdout
    let stdout_handle = thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            if let Ok(line) = line {
                // Echo to real stdout
                println!("{}", line);
                // Send to collector (with newline)
                let mut data = line.into_bytes();
                data.push(b'\n');
                // Write to streaming file for --follow
                if let Ok(mut s) = streaming_stdout.lock() {
                    let _ = s.write(&data);
                }
                let _ = tx_stdout.send((false, data));
            }
        }
    });

    // Spawn thread to read stderr
    let stderr_handle = thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines() {
            if let Ok(line) = line {
                // Echo to real stderr
                eprintln!("{}", line);
                // Send to collector (with newline)
                let mut data = line.into_bytes();
                data.push(b'\n');
                // Write to streaming file for --follow
                if let Ok(mut s) = streaming_stderr.lock() {
                    let _ = s.write(&data);
                }
                let _ = tx_stderr.send((true, data));
            }
        }
    });

    // Wait for child to exit
    let status = child.wait().map_err(|e| bird::Error::Io(e))?;
    let duration_ms = start.elapsed().as_millis() as i64;
    let exit_code = status.code().unwrap_or(-1);

    // Wait for reader threads to finish
    let _ = stdout_handle.join();
    let _ = stderr_handle.join();

    // Clean up streaming file (we keep separate stdout/stderr in buffers)
    if let Ok(streaming) = Arc::try_unwrap(streaming) {
        let _ = streaming.into_inner().map(|s| s.finish());
    }

    // Collect all output
    let mut stdout_buffer = Vec::new();
    let mut stderr_buffer = Vec::new();
    while let Ok((is_stderr, data)) = rx.try_recv() {
        if is_stderr {
            stderr_buffer.extend(data);
        } else {
            stdout_buffer.extend(data);
        }
    }

    // Check for nosave marker in combined output
    let combined = [&stdout_buffer[..], &stderr_buffer[..]].concat();
    if contains_nosave_marker(&combined) {
        if !status.success() {
            std::process::exit(exit_code);
        }
        return Ok(());
    }

    // Create and save records
    let sid = session_id();
    let session = SessionRecord::new(
        &sid,
        &config.client_id,
        invoker_name(),
        invoker_pid(),
        "shell",
    );

    // Collect context metadata (VCS, CI)
    let context = ContextMetadata::collect(Some(std::path::Path::new(cwd)));

    let mut record = InvocationRecord::with_id(
        invocation_id,
        &sid,
        cmd_str,
        cwd,
        exit_code,
        &config.client_id,
    )
    .with_duration(duration_ms)
    .with_metadata(context.into_map());

    if let Some(t) = tag {
        record = record.with_tag(t);
    }

    let inv_id = record.id;
    let mut batch = InvocationBatch::new(record).with_session(session);

    // Store stdout and stderr as separate streams
    if !stdout_buffer.is_empty() {
        batch = batch.with_output("stdout", stdout_buffer);
    }
    if !stderr_buffer.is_empty() {
        batch = batch.with_output("stderr", stderr_buffer);
    }

    store.write_batch(&batch)?;

    // Extract events if enabled
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
        let _ = Command::new(std::env::current_exe().unwrap_or_else(|_| "shq".into()))
            .args(["compact", "-s", &session_id, "--today", "-q"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }

    if !status.success() {
        std::process::exit(exit_code);
    }

    Ok(())
}

/// Get terminal size (rows, cols)
fn terminal_size() -> io::Result<(u16, u16)> {
    use std::mem::MaybeUninit;

    let mut size = MaybeUninit::<libc::winsize>::uninit();
    let ret = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, size.as_mut_ptr()) };

    if ret == 0 {
        let size = unsafe { size.assume_init() };
        Ok((size.ws_row, size.ws_col))
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Set file descriptor to non-blocking mode
fn set_nonblocking(fd: i32, nonblocking: bool) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if nonblocking {
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        } else {
            libc::fcntl(fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
        }
    }
}

/// Set terminal to raw mode, returning original termios for restoration
fn set_raw_mode(fd: i32) -> Option<libc::termios> {
    unsafe {
        let mut orig: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut orig) != 0 {
            return None;
        }

        let mut raw = orig;
        // Disable canonical mode, echo, and signal generation
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG | libc::IEXTEN);
        // Disable input processing
        raw.c_iflag &= !(libc::IXON | libc::ICRNL | libc::BRKINT | libc::INPCK | libc::ISTRIP);
        // Disable output processing
        raw.c_oflag &= !libc::OPOST;
        // Set character size to 8 bits
        raw.c_cflag |= libc::CS8;
        // Read returns immediately with whatever is available
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 0;

        if libc::tcsetattr(fd, libc::TCSAFLUSH, &raw) != 0 {
            return None;
        }

        Some(orig)
    }
}

/// Restore terminal to original mode
fn restore_termios(fd: i32, termios: &libc::termios) {
    unsafe {
        libc::tcsetattr(fd, libc::TCSAFLUSH, termios);
    }
}

/// Disable echo on PTY (for piped input to prevent duplicates)
fn disable_pty_echo(fd: i32) {
    unsafe {
        let mut termios: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut termios) == 0 {
            termios.c_lflag &= !libc::ECHO;
            libc::tcsetattr(fd, libc::TCSANOW, &termios);
        }
    }
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
    tag: Option<&str>,
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
    let has_nosave = stdout_content.as_ref().is_some_and(|c| contains_nosave_marker(c))
        || stderr_content.as_ref().is_some_and(|c| contains_nosave_marker(c))
        || single_content.as_ref().is_some_and(|c| contains_nosave_marker(c));

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

    // Collect context metadata (VCS, CI)
    let context = ContextMetadata::collect(Some(std::path::Path::new(&cwd)));

    let mut inv_record = InvocationRecord::new(
        &sid,
        command,
        &cwd,
        exit_code,
        &config.client_id,
    )
    .with_metadata(context.into_map());

    if let Some(ms) = duration_ms {
        inv_record = inv_record.with_duration(ms);
    }
    if let Some(t) = tag {
        inv_record = inv_record.with_tag(t);
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

/// Follow output from a running command in real-time (like tail -f).
///
/// Looks for the streaming output file at `~/.bird/running/<invocation_id>.out`
/// and tails it until the file is deleted (command completed).
fn follow_running_output(config: &Config, invocation_id: &str) -> bird::Result<()> {
    use std::io::{BufRead, BufReader};
    use std::thread;
    use std::time::Duration;

    // Parse UUID from string
    let id = uuid::Uuid::parse_str(invocation_id)
        .map_err(|e| bird::Error::Config(format!("Invalid invocation ID: {}", e)))?;

    let running_path = config.running_path(&id);

    if !running_path.exists() {
        eprintln!("No running output file found for {}", invocation_id);
        eprintln!("The command may have already completed. Try 'shq show {}' instead.", invocation_id);
        return Ok(());
    }

    eprintln!("Following output for {}...", &invocation_id[..8]);
    eprintln!("(Press Ctrl+C to stop)");

    // Open file for reading
    let file = std::fs::File::open(&running_path)?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();

    loop {
        // Try to read a line
        match reader.read_line(&mut line) {
            Ok(0) => {
                // EOF reached, check if file still exists
                if !running_path.exists() {
                    eprintln!("\n[Command completed]");
                    break;
                }
                // File exists but no new data - wait and retry
                thread::sleep(Duration::from_millis(100));
            }
            Ok(_) => {
                // Print the line (it includes newline)
                print!("{}", line);
                let _ = io::stdout().flush();
                line.clear();
            }
            Err(e) => {
                eprintln!("Error reading output: {}", e);
                break;
            }
        }
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
    pub follow: bool,
}

/// Show captured output from invocation(s).
pub fn output(query_str: &str, stream_filter: Option<&str>, opts: &OutputOptions) -> bird::Result<()> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let config = Config::load()?;
    let store = Store::open(config.clone())?;

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

    // First try to find by ID (short or full), then fall back to query system
    let invocation_id = if let Some(id) = try_find_by_id(&store, query_str)? {
        id
    } else {
        match resolve_query_to_invocation(&store, &query) {
            Ok(id) => id,
            Err(bird::Error::NotFound(_)) => {
                eprintln!("No matching invocation found");
                return Ok(());
            }
            Err(e) => return Err(e),
        }
    };

    // Handle --follow mode: tail the running output file
    if opts.follow {
        return follow_running_output(&config, &invocation_id);
    }

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

pub fn init(mode: &str, force: bool) -> bird::Result<()> {
    // Parse storage mode
    let storage_mode: StorageMode = mode.parse()?;

    let mut config = Config::default_location()?;

    if init::is_initialized(&config) {
        if force {
            // Delete existing database directory
            let db_dir = config.bird_root.join("db");
            if db_dir.exists() {
                std::fs::remove_dir_all(&db_dir)?;
                println!("Removed existing database at {}", db_dir.display());
            }
        } else {
            println!("BIRD already initialized at {}", config.bird_root.display());
            println!("Use --force to re-initialize (this will delete all data)");
            return Ok(());
        }
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
pub fn invocations(query_str: &str, format: &str, limit: Option<usize>) -> bird::Result<()> {
    let config = Config::load()?;
    let store = Store::open(config)?;

    // Parse query and apply filters
    let mut query = parse_query(query_str);

    // Override range if -n/--last is provided (last N items)
    if let Some(n) = limit {
        query.range = Some(bird::RangeSelector { start: n, end: Some(0) });
    }

    let invocations = store.query_invocations(&query)?;

    if invocations.is_empty() {
        println!("No invocations recorded yet.");
        return Ok(());
    }

    // Get output info for all invocations (which streams have data)
    let inv_ids: Vec<&str> = invocations.iter().map(|i| i.id.as_str()).collect();
    let output_info = get_output_info_batch(&store, &inv_ids)?;

    match format {
        "json" => {
            // JSON output
            println!("[");
            for (i, inv) in invocations.iter().enumerate() {
                let comma = if i < invocations.len() - 1 { "," } else { "" };
                let out_state = output_info.get(inv.id.as_str()).copied().unwrap_or_default();
                println!(
                    r#"  {{"id": "{}", "timestamp": "{}", "cmd": "{}", "exit_code": {}, "duration_ms": {}, "has_stdout": {}, "has_stderr": {}, "has_combined": {}}}{}"#,
                    inv.id,
                    inv.timestamp,
                    inv.cmd.replace('\\', "\\\\").replace('"', "\\\""),
                    inv.exit_code,
                    inv.duration_ms.unwrap_or(0),
                    out_state.has_stdout,
                    out_state.has_stderr,
                    out_state.has_combined,
                    comma
                );
            }
            println!("]");
        }
        "table" => {
            // Detailed table output
            println!("{:<20} {:<6} {:<10} {:<4} COMMAND", "TIMESTAMP", "EXIT", "DURATION", "OUT");
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

                // Output indicator
                let out_state = output_info.get(inv.id.as_str()).copied().unwrap_or_default();
                let out_indicator = out_state.glyph();

                // Truncate command if too long
                let cmd_display = if inv.cmd.len() > 50 {
                    format!("{}...", &inv.cmd[..47])
                } else {
                    inv.cmd.clone()
                };

                println!(
                    "{:<20} {:<6} {:<10} {:<4} {}",
                    timestamp, inv.exit_code, duration, out_indicator, cmd_display
                );
            }
        }
        "commands" => {
            // Just commands, nothing else
            for inv in invocations {
                println!("{}", inv.cmd);
            }
        }
        _ => {
            // Compact color output (default)
            // Format: ✓ abcd1234 command... ●
            for inv in invocations {
                // Status glyph with color
                let (status_glyph, color_code) = if inv.exit_code == 0 {
                    ("✓", "\x1b[32m") // Green
                } else {
                    ("✗", "\x1b[31m") // Red
                };
                let reset = "\x1b[0m";
                let dim = "\x1b[2m";

                // Short ID (last 8 chars - more unique for UUIDv7)
                let id_len = inv.id.len();
                let short_id = if id_len >= 8 {
                    &inv.id[id_len - 8..]
                } else {
                    &inv.id
                };

                // Output indicator
                let out_state = output_info.get(inv.id.as_str()).copied().unwrap_or_default();
                let out_glyph = out_state.glyph();

                // Truncate command for terminal width (leave room for prefix)
                let max_cmd_len = 65;
                let cmd_display = if inv.cmd.len() > max_cmd_len {
                    format!("{}…", &inv.cmd[..max_cmd_len - 1])
                } else {
                    inv.cmd.clone()
                };

                println!(
                    "{}{}{} {}{}{} {} {}",
                    color_code, status_glyph, reset,
                    dim, short_id, reset,
                    out_glyph,
                    cmd_display
                );
            }
        }
    }

    Ok(())
}

/// Output capture state for display
#[derive(Debug, Clone, Copy, Default)]
struct OutputState {
    has_stdout: bool,
    has_stderr: bool,
    has_combined: bool,
    has_empty: bool,  // Has entry but 0 bytes
}

impl OutputState {
    /// Get display glyph for output state
    fn glyph(&self) -> &'static str {
        if self.has_combined {
            "◉"  // Combined (merged, can't separate)
        } else if self.has_stdout && self.has_stderr {
            "●"  // Both separate streams
        } else if self.has_stdout {
            "◐"  // Stdout only
        } else if self.has_stderr {
            "◑"  // Stderr only
        } else if self.has_empty {
            "○"  // Captured but empty
        } else {
            "·"  // Not captured
        }
    }
}

/// Get output info for a batch of invocation IDs.
fn get_output_info_batch(store: &Store, inv_ids: &[&str]) -> bird::Result<std::collections::HashMap<String, OutputState>> {
    use std::collections::HashMap;

    if inv_ids.is_empty() {
        return Ok(HashMap::new());
    }

    // Build SQL to query output streams for all invocations at once
    let ids_sql = inv_ids.iter().map(|id| format!("'{}'", id)).collect::<Vec<_>>().join(", ");
    let sql = format!(
        "SELECT invocation_id, stream, byte_length FROM outputs WHERE invocation_id IN ({})",
        ids_sql
    );

    let result = store.query(&sql)?;

    let mut info: HashMap<String, OutputState> = HashMap::new();
    for row in &result.rows {
        if row.len() >= 3 {
            let inv_id = row[0].clone();
            let stream = row[1].clone();
            let byte_length: i64 = row[2].parse().unwrap_or(0);

            let entry = info.entry(inv_id).or_default();
            if byte_length > 0 {
                match stream.as_str() {
                    "stdout" => entry.has_stdout = true,
                    "stderr" => entry.has_stderr = true,
                    "combined" => entry.has_combined = true,
                    _ => {}
                }
            } else {
                entry.has_empty = true;
            }
        }
    }

    Ok(info)
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

/// Statistics about the BIRD store.
#[derive(serde::Serialize)]
pub struct BirdStats {
    pub root: String,
    pub client_id: String,
    pub storage_mode: String,
    pub current_session: CurrentSession,
    pub invocations: InvocationStats,
    pub sessions: SessionStats,
    pub events: EventStats,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub remotes: Vec<RemoteInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schemas: Option<SchemaStats>,
}

#[derive(serde::Serialize)]
pub struct RemoteInfo {
    pub name: String,
    pub remote_type: String,
    pub uri: String,
    pub auto_attach: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub invocations: Option<i64>,
}

#[derive(serde::Serialize)]
pub struct SchemaStats {
    pub local: SchemaCounts,
    pub caches: SchemaCounts,
    pub remotes: SchemaCounts,
    pub main: SchemaCounts,
    pub unified: SchemaCounts,
}

#[derive(serde::Serialize)]
pub struct SchemaCounts {
    pub invocations: i64,
    pub sessions: i64,
    pub outputs: i64,
    pub events: i64,
}

#[derive(serde::Serialize)]
pub struct InvocationStats {
    pub total: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last: Option<LastInvocation>,
}

#[derive(serde::Serialize)]
pub struct LastInvocation {
    pub id: String,
    pub cmd: String,
    pub exit_code: i32,
    pub timestamp: String,
}

#[derive(serde::Serialize)]
pub struct SessionStats {
    pub total: i64,
}

#[derive(serde::Serialize)]
pub struct CurrentSession {
    pub hostname: String,
    pub username: String,
    pub shell: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(serde::Serialize)]
pub struct EventStats {
    pub total: i64,
    pub errors: i64,
    pub warnings: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<LastError>,
}

#[derive(serde::Serialize)]
pub struct LastError {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<i32>,
}

pub fn stats(format: &str, details: bool, field: Option<&str>) -> bird::Result<()> {
    let config = Config::load()?;
    let store = Store::open(config.clone())?;

    // Use a single connection for all queries to avoid multiple connection issues
    let conn = store.connection()?;

    // Get current session info from client_id (username@hostname) and environment
    let (username, hostname) = config.client_id.split_once('@')
        .map(|(u, h)| (u.to_string(), h.to_string()))
        .unwrap_or_else(|| (config.client_id.clone(), "unknown".to_string()));
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "unknown".to_string());
    let session_id = std::env::var("__shq_session_id").ok();

    // Gather basic stats using the single connection
    let inv_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM main.invocations", [], |r| r.get(0))
        .unwrap_or(0);
    let session_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM main.sessions", [], |r| r.get(0))
        .unwrap_or(0);

    // Get last invocation
    let last_inv: Option<bird::InvocationSummary> = conn
        .query_row(
            "SELECT id, cmd, exit_code, timestamp FROM main.invocations ORDER BY timestamp DESC LIMIT 1",
            [],
            |row| {
                Ok(bird::InvocationSummary {
                    id: row.get::<_, String>(0)?,
                    cmd: row.get::<_, String>(1)?,
                    exit_code: row.get::<_, i32>(2)?,
                    timestamp: row.get::<_, String>(3)?,
                    duration_ms: None,
                })
            },
        )
        .ok();

    // Event counts
    let event_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM main.events", [], |r| r.get(0))
        .unwrap_or(0);
    let error_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM main.events WHERE severity = 'error'", [], |r| r.get(0))
        .unwrap_or(0);
    let warning_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM main.events WHERE severity = 'warning'", [], |r| r.get(0))
        .unwrap_or(0);

    // Last error event (simplified - not querying for now)
    let last_error: Option<LastError> = None;

    // Gather remote info
    let remotes: Vec<RemoteInfo> = config
        .remotes
        .iter()
        .map(|r| {
            // Try to get invocation count from remote
            let inv_count = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {}.invocations", r.quoted_schema_name()),
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .ok();
            RemoteInfo {
                name: r.name.clone(),
                remote_type: format!("{:?}", r.remote_type).to_lowercase(),
                uri: r.uri.clone(),
                auto_attach: r.auto_attach,
                invocations: inv_count,
            }
        })
        .collect();

    // Gather per-schema stats (only if we have remotes or cached data)
    let schemas = if !config.remotes.is_empty() {
        // Helper to get counts from schema.table pattern
        let get_schema_counts = |schema: &str| -> SchemaCounts {
            SchemaCounts {
                invocations: conn
                    .query_row(&format!("SELECT COUNT(*) FROM {}.invocations", schema), [], |r| r.get(0))
                    .unwrap_or(0),
                sessions: conn
                    .query_row(&format!("SELECT COUNT(*) FROM {}.sessions", schema), [], |r| r.get(0))
                    .unwrap_or(0),
                outputs: conn
                    .query_row(&format!("SELECT COUNT(*) FROM {}.outputs", schema), [], |r| r.get(0))
                    .unwrap_or(0),
                events: conn
                    .query_row(&format!("SELECT COUNT(*) FROM {}.events", schema), [], |r| r.get(0))
                    .unwrap_or(0),
            }
        };

        // Helper to get counts from table-returning macros (remotes use macros)
        let get_macro_counts = |prefix: &str| -> SchemaCounts {
            SchemaCounts {
                invocations: conn
                    .query_row(&format!("SELECT COUNT(*) FROM {}_invocations()", prefix), [], |r| r.get(0))
                    .unwrap_or(0),
                sessions: conn
                    .query_row(&format!("SELECT COUNT(*) FROM {}_sessions()", prefix), [], |r| r.get(0))
                    .unwrap_or(0),
                outputs: conn
                    .query_row(&format!("SELECT COUNT(*) FROM {}_outputs()", prefix), [], |r| r.get(0))
                    .unwrap_or(0),
                events: conn
                    .query_row(&format!("SELECT COUNT(*) FROM {}_events()", prefix), [], |r| r.get(0))
                    .unwrap_or(0),
            }
        };

        Some(SchemaStats {
            local: get_schema_counts("local"),
            caches: get_schema_counts("caches"),
            remotes: get_macro_counts("remotes"),  // Uses remotes_*() macros
            main: get_schema_counts("main"),
            unified: get_schema_counts("unified"),
        })
    } else {
        None
    };

    // Build stats struct
    let stats = BirdStats {
        root: config.bird_root.display().to_string(),
        client_id: config.client_id.clone(),
        storage_mode: config.storage_mode.to_string(),
        current_session: CurrentSession {
            hostname,
            username,
            shell,
            session_id,
        },
        invocations: InvocationStats {
            total: inv_count,
            last: last_inv.map(|inv| LastInvocation {
                id: inv.id.clone(),
                cmd: inv.cmd.clone(),
                exit_code: inv.exit_code,
                timestamp: inv.timestamp.clone(),
            }),
        },
        sessions: SessionStats {
            total: session_count,
        },
        events: EventStats {
            total: event_count,
            errors: error_count,
            warnings: warning_count,
            last_error,
        },
        remotes,
        schemas,
    };

    // Handle --field option for scripting
    if let Some(field_name) = field {
        let value = match field_name {
            "root" => stats.root.clone(),
            "client_id" => stats.client_id.clone(),
            "storage_mode" => stats.storage_mode.clone(),
            "hostname" => stats.current_session.hostname.clone(),
            "username" => stats.current_session.username.clone(),
            "shell" => stats.current_session.shell.clone(),
            "session_id" => stats.current_session.session_id.clone().unwrap_or_default(),
            "invocations" | "invocations.total" => stats.invocations.total.to_string(),
            "sessions" | "sessions.total" => stats.sessions.total.to_string(),
            "events" | "events.total" => stats.events.total.to_string(),
            "errors" | "events.errors" => stats.events.errors.to_string(),
            "warnings" | "events.warnings" => stats.events.warnings.to_string(),
            _ => {
                eprintln!("Unknown field: {}", field_name);
                eprintln!("Available fields: root, client_id, storage_mode, hostname, username, shell, session_id, invocations, sessions, events, errors, warnings");
                return Ok(());
            }
        };
        println!("{}", value);
        return Ok(());
    }

    match format {
        "json" => {
            println!("{}", serde_json::to_string_pretty(&stats).unwrap());
        }
        _ => {
            println!("Root:         {}", stats.root);
            println!("Client ID:    {}", stats.client_id);
            println!("Storage mode: {}", stats.storage_mode);
            if details {
                println!("Hostname:     {}", stats.current_session.hostname);
                println!("Username:     {}", stats.current_session.username);
                println!("Shell:        {}", stats.current_session.shell);
                if let Some(ref sid) = stats.current_session.session_id {
                    println!("Session ID:   {}", sid);
                }
            }
            println!();
            println!("Total invocations: {}", stats.invocations.total);
            println!("Total sessions:    {}", stats.sessions.total);
            if let Some(ref inv) = stats.invocations.last {
                println!("Last command:      {} (exit {})", inv.cmd, inv.exit_code);
            }
            println!();
            println!("Total events:      {}", stats.events.total);
            println!("  Errors:          {}", stats.events.errors);
            println!("  Warnings:        {}", stats.events.warnings);
            if let Some(ref err) = stats.events.last_error {
                let location = match (&err.file, err.line) {
                    (Some(f), Some(l)) => format!(" at {}:{}", f, l),
                    (Some(f), None) => format!(" in {}", f),
                    _ => String::new(),
                };
                let msg = err.message.as_deref().unwrap_or("-");
                println!("  Last error:      {}{}", truncate_string(msg, 40), location);
            }

            // Show remotes
            if !stats.remotes.is_empty() {
                println!();
                println!("Remotes:");
                for r in &stats.remotes {
                    let inv_str = r.invocations.map(|n| format!(" ({} invocations)", n)).unwrap_or_default();
                    let attach = if r.auto_attach { "" } else { " [manual]" };
                    println!("  {} [{}]: {}{}{}", r.name, r.remote_type, r.uri, inv_str, attach);
                }
            }

            // Show per-schema stats
            if let Some(ref s) = stats.schemas {
                println!();
                println!("Schema Summary:");
                println!("  {:12} {:>10} {:>10} {:>10} {:>10}", "SCHEMA", "INVOCS", "SESSIONS", "OUTPUTS", "EVENTS");
                println!("  {:12} {:>10} {:>10} {:>10} {:>10}", "local", s.local.invocations, s.local.sessions, s.local.outputs, s.local.events);
                println!("  {:12} {:>10} {:>10} {:>10} {:>10}", "caches", s.caches.invocations, s.caches.sessions, s.caches.outputs, s.caches.events);
                println!("  {:12} {:>10} {:>10} {:>10} {:>10}", "remotes", s.remotes.invocations, s.remotes.sessions, s.remotes.outputs, s.remotes.events);
                println!("  {:12} {:>10} {:>10} {:>10} {:>10}", "main", s.main.invocations, s.main.sessions, s.main.outputs, s.main.events);
                println!("  {:12} {:>10} {:>10} {:>10} {:>10}", "unified", s.unified.invocations, s.unified.sessions, s.unified.outputs, s.unified.events);
            }
        }
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

/// Output ignore patterns for shell hooks (colon-separated).
pub fn hook_ignore_patterns() -> bird::Result<()> {
    let config = Config::load()?;
    let patterns = config.hooks.ignore_patterns.join(":");
    println!("{}", patterns);
    Ok(())
}

/// Output shell integration code.
pub fn hook_init(shell: Option<&str>, inactive: bool, prompt_indicator: bool, quiet: bool) -> bird::Result<()> {
    use crate::hooks::{self, Shell, Mode};

    // Auto-detect shell from $SHELL if not specified
    let shell_str = shell
        .map(|s| s.to_string())
        .or_else(|| std::env::var("SHELL").ok())
        .unwrap_or_default();

    let shell_type = if shell_str.contains("zsh") {
        Shell::Zsh
    } else if shell_str.contains("bash") {
        Shell::Bash
    } else {
        eprintln!("Unknown shell type. Use --shell zsh or --shell bash");
        std::process::exit(1);
    };

    let mode = if inactive { Mode::Inactive } else { Mode::Active };

    // Output quiet mode variable if requested
    if quiet {
        println!("__shq_quiet=1");
    }

    // Generate and output the hook
    print!("{}", hooks::generate(shell_type, mode, prompt_indicator));

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
#[allow(clippy::too_many_arguments)]
pub fn events(
    query_str: &str,
    severity: Option<&str>,
    count_only: bool,
    limit: usize,
    order: LimitOrder,
    reparse: bool,
    extract: bool,
    format: Option<&str>,
) -> bird::Result<()> {
    let config = Config::load()?;
    let store = Store::open(config)?;

    // Parse query (filters and range applied by query_invocations)
    let query = parse_query(query_str);

    // Handle reparse mode: re-extract events from outputs
    if reparse {
        let invocations = store.query_invocations(&query)?;
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

    // Get invocations matching query (with filters applied)
    let invocations = store.query_invocations(&query)?;
    if invocations.is_empty() {
        println!("No invocations found.");
        return Ok(());
    }

    // Extract events if requested and not already extracted
    if extract {
        for inv in &invocations {
            let existing = store.event_count(&EventFilters {
                invocation_id: Some(inv.id.clone()),
                ..Default::default()
            })?;
            if existing == 0 {
                let _ = store.extract_events(&inv.id, format);
            }
        }
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
        "{:<8} {:<40} {:<30} MESSAGE",
        "SEVERITY", "FILE:LINE", "CODE"
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
#[allow(clippy::too_many_arguments)]
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

/// Resolve a selector (negative offset, ~N syntax, or UUID) to an invocation ID.
fn resolve_invocation_id(store: &Store, selector: &str) -> bird::Result<String> {
    // Handle ~N syntax (e.g., ~1 for most recent, ~2 for second-to-last)
    if let Some(stripped) = selector.strip_prefix('~') {
        if let Ok(n) = stripped.parse::<usize>() {
            if n > 0 {
                let invocations = store.recent_invocations(n)?;
                if let Some(inv) = invocations.last() {
                    return Ok(inv.id.clone());
                } else {
                    return Err(bird::Error::NotFound(format!(
                        "No invocation found at offset ~{}",
                        n
                    )));
                }
            }
        }
    }

    // Handle negative offset (e.g., -1 for last, -2 for second-to-last)
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

    // Try short ID lookup
    if let Some(id) = try_find_by_id(store, selector)? {
        return Ok(id);
    }

    // Assume it's a full UUID
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

/// Check if a string looks like a hex ID (short or full UUID).
fn looks_like_hex_id(s: &str) -> bool {
    // Must be at least 4 chars and only contain hex digits and dashes
    s.len() >= 4 && s.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Try to find an invocation by tag, short ID, or full ID.
/// Returns Some(full_id) if found, None if not found or query doesn't look like an ID/tag.
fn try_find_by_id(store: &Store, query_str: &str) -> bird::Result<Option<String>> {
    let trimmed = query_str.trim();

    // Check for tag lookup (:tagname)
    if let Some(tag) = trimmed.strip_prefix(':') {
        if let Some(id) = store.find_by_tag(tag)? {
            return Ok(Some(id));
        }
        // Tag not found - this is an explicit error, not a fallback to query
        return Err(bird::Error::NotFound(format!("Tag '{}' not found", tag)));
    }

    if !looks_like_hex_id(trimmed) {
        return Ok(None);
    }

    // Try exact match first (full UUID)
    let result = store.query(&format!(
        "SELECT id::VARCHAR FROM invocations WHERE id::VARCHAR = '{}' LIMIT 1",
        trimmed
    ))?;

    if !result.rows.is_empty() {
        return Ok(Some(result.rows[0][0].clone()));
    }

    // Try suffix match (short ID - we show last 8 chars of UUIDv7)
    let result = store.query(&format!(
        "SELECT id::VARCHAR FROM invocations WHERE suffix(id::VARCHAR, '{}') ORDER BY timestamp DESC LIMIT 1",
        trimmed
    ))?;

    if !result.rows.is_empty() {
        return Ok(Some(result.rows[0][0].clone()));
    }

    Ok(None)
}

/// Resolve a query to a single invocation ID.
fn resolve_query_to_invocation(store: &Store, query: &Query) -> bird::Result<String> {
    // For single-item commands, the range selector determines which item:
    // - ~N = single item at position N (1 = most recent)
    // - ~N: = last N items, we take the most recent (position 1)
    // - ~N:~M = range from N to M, we take position M (most recent in range)
    // - No range = default to position 1 (most recent)

    // query_invocations_with_limit handles the range semantics, returning
    // the correct subset. For single-item commands, we just take the first result.
    let invocations = store.query_invocations_with_limit(query, 1)?;

    if let Some(inv) = invocations.first() {
        Ok(inv.id.clone())
    } else {
        Err(bird::Error::NotFound("No matching invocation found".to_string()))
    }
}

/// Show detailed info about an invocation.
pub fn info(query_str: &str, format: &str, field: Option<&str>) -> bird::Result<()> {
    let config = Config::load()?;
    let store = Store::open(config)?;

    // First try to find by ID (short or full)
    let invocation_id = if let Some(id) = try_find_by_id(&store, query_str)? {
        id
    } else {
        // Fall back to query system
        let query = parse_query(query_str);
        resolve_query_to_invocation(&store, &query)?
    };

    // Get full invocation details via SQL
    let result = store.query(&format!(
        "SELECT id, cmd, cwd, exit_code, timestamp, duration_ms, session_id, tag
         FROM invocations
         WHERE id = '{}'",
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
    let tag = &row[7];

    // Get output info
    let outputs = store.get_outputs(&invocation_id, None)?;
    let stdout_size: i64 = outputs.iter().filter(|o| o.stream == "stdout").map(|o| o.byte_length).sum();
    let stderr_size: i64 = outputs.iter().filter(|o| o.stream == "stderr").map(|o| o.byte_length).sum();

    // Get event count
    let event_count = store.event_count(&EventFilters {
        invocation_id: Some(invocation_id.clone()),
        ..Default::default()
    })?;

    // If a specific field is requested, just print that value (for scripting)
    if let Some(f) = field {
        let value = match f.to_lowercase().as_str() {
            "id" => id.to_string(),
            "cmd" | "command" => cmd.to_string(),
            "cwd" | "dir" | "working_dir" => cwd.to_string(),
            "exit" | "exit_code" => exit_code.to_string(),
            "timestamp" | "time" => timestamp.to_string(),
            "duration" | "duration_ms" => duration_ms.to_string(),
            "session" | "session_id" => session_id.to_string(),
            "tag" => tag.to_string(),
            "stdout" | "stdout_bytes" => stdout_size.to_string(),
            "stderr" | "stderr_bytes" => stderr_size.to_string(),
            "events" | "event_count" => event_count.to_string(),
            _ => return Err(bird::Error::Config(format!("Unknown field: {}", f))),
        };
        println!("{}", value);
        return Ok(());
    }

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
            if tag != "NULL" && !tag.is_empty() {
                println!(r#"  "tag": "{}","#, tag);
            }
            println!(r#"  "stdout_bytes": {},"#, stdout_size);
            println!(r#"  "stderr_bytes": {},"#, stderr_size);
            println!(r#"  "event_count": {}"#, event_count);
            println!(r#"}}"#);
        }
        _ => {
            // Table format
            println!("ID:          {}", id);
            println!("Timestamp:   {}", timestamp);
            println!("Command:     {}", cmd);
            println!("Working Dir: {}", cwd);
            println!("Exit Code:   {}", exit_code);
            println!("Duration:    {}ms", duration_ms);
            println!("Session:     {}", session_id);
            if tag != "NULL" && !tag.is_empty() {
                println!("Tag:         {}", tag);
            }
            println!("Stdout:      {} bytes", stdout_size);
            println!("Stderr:      {} bytes", stderr_size);
            println!("Events:      {}", event_count);
        }
    }

    Ok(())
}

/// Re-run a previous command.
pub fn rerun(query_str: &str, dry_run: bool, no_capture: bool) -> bird::Result<()> {
    use std::io::Write;

    let config = Config::load()?;
    let store = Store::open(config)?;

    // First try to find by ID (short or full)
    let invocation_id = if let Some(id) = try_find_by_id(&store, query_str)? {
        id
    } else {
        // Fall back to query system
        let query = parse_query(query_str);
        resolve_query_to_invocation(&store, &query)?
    };

    // Get full invocation details via SQL (need cmd and cwd)
    let result = store.query(&format!(
        "SELECT cmd, cwd FROM invocations WHERE id = '{}'",
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
            invoker_name(),
            invoker_pid(),
            "shell",
        );

        // Collect context metadata (VCS, CI)
        let context = ContextMetadata::collect(Some(std::path::Path::new(cwd)));

        let record = InvocationRecord::new(
            &sid,
            cmd,
            cwd,
            exit_code,
            &config.client_id,
        )
        .with_duration(duration_ms)
        .with_metadata(context.into_map());

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
invocations (i)    List command history     shq i -n 20       shq i %exit<>0~10:
events (e)         Show parsed events       shq e -n 10       shq e -s error~5:
info (I)           Invocation details       shq I ~1          shq I %/test/~1
rerun (R, !!)      Re-run a command         shq R ~1          shq R %/make/~1
run (r)            Run and capture          shq r cargo test  shq r -c "make all"
sql (q)            Execute SQL query        shq q "SELECT * FROM invocations LIMIT 5"

QUERY SYNTAX: [source][path][filters][range]
────────────────────────────────────────────────────────────────────────────────
RANGE         ~1          Most recent command (position 1)
              ~5          5th most recent command (position 5)
              ~10:        Last 10 commands (positions 1-10)
              ~10:~5      Range from position 10 to 5
              -n 10       CLI flag: last 10 (same as ~10:)

SOURCE        Format: host:type:client:session:
              shell:           Shell commands on this host
              shell:bash:      Bash shells only
              shell:zsh:       Zsh shells only
              myhost:shell::   All shells on myhost
              *:*:*:*:         Everything everywhere (all hosts, all types)
              *:shell:*:*:     All shell commands on all hosts

PATH          .           Current directory
              ~/Projects/ Home-relative
              /tmp/       Absolute path

FILTERS       %failed     Non-zero exit code (alias for %exit<>0)
              %success    Successful commands (alias for %exit=0)
              %ok         Same as %success
              %exit<>0    Non-zero exit code
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
shq i %failed~20             Last 20 failed commands
shq i %failed                All failed commands (up to default limit)
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



// ZSH hook without prompt indicator - same as ZSH_HOOK but without PS1 modification

// Format hints management

/// List format hints (user-defined and optionally built-in).
pub fn format_hints_list(show_builtin: bool, show_user: bool, filter: Option<&str>) -> bird::Result<()> {
    let config = Config::load()?;
    let store = Store::open(config)?;

    let hints = store.load_format_hints()?;

    // Helper to check if a hint matches the filter
    let matches_filter = |pattern: &str, format: &str| -> bool {
        match filter {
            None => true,
            Some(f) => {
                let f_lower = f.to_lowercase();
                pattern.to_lowercase().contains(&f_lower) || format.to_lowercase().contains(&f_lower)
            }
        }
    };

    // Show user-defined hints
    if show_user {
        let user_hints: Vec<_> = hints.hints()
            .iter()
            .filter(|h| matches_filter(&h.pattern, &h.format))
            .collect();

        if user_hints.is_empty() {
            if filter.is_some() {
                println!("No user-defined format hints matching filter.");
            } else {
                println!("No user-defined format hints.");
            }
        } else {
            println!("User-defined format hints:");
            println!("{:<6} {:<30} FORMAT", "PRI", "PATTERN");
            println!("{}", "-".repeat(60));
            for hint in user_hints {
                println!("{:<6} {:<30} {}", hint.priority, hint.pattern, hint.format);
            }
        }
        println!();
    }

    // Show built-in formats from duck_hunt
    if show_builtin {
        match store.list_builtin_formats() {
            Ok(formats) => {
                let filtered: Vec<_> = formats.iter()
                    .filter(|f| matches_filter(&f.pattern, &f.format))
                    .collect();

                if filtered.is_empty() {
                    if filter.is_some() {
                        println!("No built-in formats matching filter.");
                    } else {
                        println!("No built-in formats available.");
                    }
                } else {
                    println!("Available formats (from duck_hunt):");
                    println!("{:<6} {:<20} DESCRIPTION", "PRI", "FORMAT");
                    println!("{}", "-".repeat(70));
                    for fmt in filtered {
                        // pattern field contains description for builtin formats
                        let desc = if fmt.pattern.len() > 45 {
                            format!("{}...", &fmt.pattern[..42])
                        } else {
                            fmt.pattern.clone()
                        };
                        println!("{:<6} {:<20} {}", fmt.priority, fmt.format, desc);
                    }
                }
            }
            Err(e) => {
                eprintln!("Warning: Could not list built-in formats: {}", e);
            }
        }
    }

    Ok(())
}

/// Add a format hint.
pub fn format_hints_add(pattern: &str, format: &str, priority: Option<i32>) -> bird::Result<()> {
    let config = Config::load()?;
    let store = Store::open(config)?;

    let mut hints = store.load_format_hints()?;

    let priority = priority.unwrap_or(bird::format_hints::DEFAULT_PRIORITY);
    let hint = bird::FormatHint::with_priority(pattern, format, priority);

    // Check if pattern already exists
    if hints.get(pattern).is_some() {
        println!("Updating existing pattern: {}", pattern);
    }

    hints.add(hint);
    store.save_format_hints(&hints)?;

    println!("Added: {} -> {} (priority {})", pattern, format, priority);
    Ok(())
}

/// Remove a format hint by pattern.
pub fn format_hints_remove(pattern: &str) -> bird::Result<()> {
    let config = Config::load()?;
    let store = Store::open(config)?;

    let mut hints = store.load_format_hints()?;

    if hints.remove(pattern) {
        store.save_format_hints(&hints)?;
        println!("Removed: {}", pattern);
    } else {
        println!("Pattern not found: {}", pattern);
    }

    Ok(())
}

/// Check which format would be detected for a command.
pub fn format_hints_check(cmd: &str) -> bird::Result<()> {
    use bird::FormatSource;

    let config = Config::load()?;
    let store = Store::open(config)?;

    let result = store.check_format(cmd)?;

    println!("Command: {}", cmd);
    println!("Format:  {}", result.format);

    match result.source {
        FormatSource::UserDefined { pattern, priority } => {
            println!("Source:  user-defined (pattern: {}, priority: {})", pattern, priority);
        }
        FormatSource::Builtin { pattern, priority } => {
            println!("Source:  built-in (pattern: {}, priority: {})", pattern, priority);
        }
        FormatSource::Default => {
            println!("Source:  default (no pattern matched)");
        }
    }

    Ok(())
}

/// Set the default format.
pub fn format_hints_set_default(format: &str) -> bird::Result<()> {
    let config = Config::load()?;
    let store = Store::open(config)?;

    let mut hints = store.load_format_hints()?;
    hints.set_default_format(format);
    store.save_format_hints(&hints)?;

    println!("Default format set to: {}", format);
    Ok(())
}

// Remote management commands

/// Add a remote storage connection.
pub fn remote_add(
    name: &str,
    remote_type: &str,
    uri: &str,
    read_only: bool,
    credential_provider: Option<&str>,
    auto_attach: bool,
) -> bird::Result<()> {
    use bird::{RemoteConfig, RemoteMode, RemoteType};
    use std::str::FromStr;

    let mut config = Config::load()?;

    let rtype = RemoteType::from_str(remote_type)?;
    let mut remote = RemoteConfig::new(name, rtype, uri);

    if read_only {
        remote.mode = RemoteMode::ReadOnly;
    }
    if let Some(provider) = credential_provider {
        remote.credential_provider = Some(provider.to_string());
    }
    remote.auto_attach = auto_attach;

    // Check if updating existing
    let updating = config.get_remote(name).is_some();

    config.add_remote(remote);
    config.save()?;

    if updating {
        println!("Updated remote: {}", name);
    } else {
        println!("Added remote: {}", name);
    }
    println!("  Type: {}", remote_type);
    println!("  URI:  {}", uri);
    println!("  Mode: {}", if read_only { "read-only" } else { "read-write" });
    if let Some(provider) = credential_provider {
        println!("  Credentials: {}", provider);
    }
    println!("  Auto-attach: {}", auto_attach);

    Ok(())
}

/// List configured remotes.
pub fn remote_list() -> bird::Result<()> {
    let config = Config::load()?;

    if config.remotes.is_empty() {
        println!("No remotes configured.");
        println!();
        println!("Add a remote with:");
        println!("  shq remote add <name> --type s3 --uri s3://bucket/path/bird.duckdb");
        return Ok(());
    }

    println!("{:<12} {:<12} {:<10} {:<8} URI", "NAME", "TYPE", "MODE", "ATTACH");
    println!("{}", "-".repeat(70));

    for remote in &config.remotes {
        println!(
            "{:<12} {:<12} {:<10} {:<8} {}",
            remote.name,
            remote.remote_type,
            remote.mode,
            if remote.auto_attach { "auto" } else { "manual" },
            remote.uri
        );
    }

    Ok(())
}

/// Remove a remote configuration.
pub fn remote_remove(name: &str) -> bird::Result<()> {
    let mut config = Config::load()?;

    if config.remove_remote(name) {
        config.save()?;
        println!("Removed remote: {}", name);
    } else {
        println!("Remote not found: {}", name);
    }

    Ok(())
}

/// Test connection to a remote.
pub fn remote_test(name: Option<&str>) -> bird::Result<()> {
    let config = Config::load()?;
    let store = Store::open(config.clone())?;

    let remotes_to_test: Vec<_> = if let Some(n) = name {
        match config.get_remote(n) {
            Some(r) => vec![r],
            None => {
                println!("Remote not found: {}", n);
                return Ok(());
            }
        }
    } else {
        config.remotes.iter().collect()
    };

    if remotes_to_test.is_empty() {
        println!("No remotes configured.");
        return Ok(());
    }

    for remote in remotes_to_test {
        print!("Testing {}... ", remote.name);
        match store.test_remote(remote) {
            Ok(()) => println!("OK"),
            Err(e) => println!("FAILED: {}", e),
        }
    }

    Ok(())
}

/// Manually attach a remote (shows SQL to run).
pub fn remote_attach(name: &str) -> bird::Result<()> {
    let config = Config::load()?;

    match config.get_remote(name) {
        Some(remote) => {
            println!("To attach this remote in SQL:");
            println!();
            if remote.credential_provider.is_some() {
                println!("LOAD httpfs;");
                println!(
                    "CREATE SECRET IF NOT EXISTS \"bird_{}\" (TYPE s3, PROVIDER credential_chain);",
                    remote.name
                );
            }
            println!("{};", remote.attach_sql());
            println!();
            println!("Then query with: SELECT * FROM {}.invocations LIMIT 10;", remote.quoted_schema_name());
        }
        None => {
            println!("Remote not found: {}", name);
        }
    }

    Ok(())
}

/// Show remote sync status.
pub fn remote_status() -> bird::Result<()> {
    use bird::PushOptions;

    let config = Config::load()?;
    let store = Store::open(config.clone())?;

    println!("Sync Configuration:");
    println!("  Default remote:    {}", config.sync.default_remote.as_deref().unwrap_or("(none)"));
    println!("  Push on compact:   {}", config.sync.push_on_compact);
    println!("  Push on archive:   {}", config.sync.push_on_archive);
    println!("  Sync invocations:  {}", config.sync.sync_invocations);
    println!("  Sync outputs:      {}", config.sync.sync_outputs);
    println!("  Sync events:       {}", config.sync.sync_events);
    println!("  Sync blobs:        {}", config.sync.sync_blobs);
    if config.sync.sync_blobs {
        println!("  Blob min size:     {} bytes", config.sync.blob_sync_min_bytes);
    }
    println!();

    println!("Blob Roots (search order):");
    for (i, root) in config.blob_roots().iter().enumerate() {
        println!("  {}. {}", i + 1, root);
    }
    println!();

    if config.remotes.is_empty() {
        println!("No remotes configured.");
    } else {
        println!("Configured Remotes:");
        for remote in &config.remotes {
            println!("  {} ({}, {})", remote.name, remote.remote_type, remote.mode);

            // Show pending sync stats (dry-run)
            let opts = PushOptions {
                since: None,
                dry_run: true,
                sync_blobs: true,
            };
            match store.push(remote, opts) {
                Ok(stats) => {
                    let total = stats.sessions + stats.invocations + stats.outputs + stats.events;
                    if total > 0 || stats.blobs.count > 0 {
                        println!("    Pending push: {}", stats);
                    } else {
                        println!("    Pending push: (up to date)");
                    }
                }
                Err(e) => {
                    println!("    Status: error - {}", e);
                }
            }
        }
    }

    Ok(())
}

// Push/Pull commands

/// Push local data to a remote.
pub fn push(remote: Option<&str>, since: Option<&str>, dry_run: bool, sync_blobs: bool) -> bird::Result<()> {
    use bird::{parse_since, PushOptions};

    let config = Config::load()?;
    let store = Store::open(config.clone())?;

    // Resolve remote
    let remote_name = remote
        .map(String::from)
        .or_else(|| config.sync.default_remote.clone())
        .ok_or_else(|| bird::Error::Config(
            "No remote specified and no default remote configured. Use --remote <name> or set sync.default_remote in config.".to_string()
        ))?;

    let remote_config = config.get_remote(&remote_name)
        .ok_or_else(|| bird::Error::Config(format!("Remote '{}' not found", remote_name)))?;

    // Parse since date
    let since_date = since.map(parse_since).transpose()?;

    let opts = PushOptions {
        since: since_date,
        dry_run,
        sync_blobs,
    };

    let stats = store.push(remote_config, opts)?;

    if dry_run {
        println!("Would push to '{}': {}", remote_name, stats);
    } else {
        println!("Pushed to '{}': {}", remote_name, stats);
    }

    Ok(())
}

/// Pull data from a remote to local.
pub fn pull(remote: Option<&str>, client: Option<&str>, since: Option<&str>, sync_blobs: bool) -> bird::Result<()> {
    use bird::{parse_since, PullOptions};

    let config = Config::load()?;
    let store = Store::open(config.clone())?;

    // Resolve remote
    let remote_name = remote
        .map(String::from)
        .or_else(|| config.sync.default_remote.clone())
        .ok_or_else(|| bird::Error::Config(
            "No remote specified and no default remote configured. Use --remote <name> or set sync.default_remote in config.".to_string()
        ))?;

    let remote_config = config.get_remote(&remote_name)
        .ok_or_else(|| bird::Error::Config(format!("Remote '{}' not found", remote_name)))?;

    // Parse since date
    let since_date = since.map(parse_since).transpose()?;

    let opts = PullOptions {
        since: since_date,
        client_id: client.map(String::from),
        sync_blobs,
    };

    let stats = store.pull(remote_config, opts)?;

    println!("Pulled from '{}': {}", remote_name, stats);

    Ok(())
}


