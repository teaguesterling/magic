//! shq: Shell Query - CLI for capturing and querying shell command history.

use clap::{Parser, Subcommand};

mod commands;

#[derive(Parser)]
#[command(name = "shq")]
#[command(about = "Shell Query - capture and query shell command history")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Initialize BIRD database
    Init,

    /// Run a command and capture it to BIRD
    Run {
        /// Shell command string (passed to $SHELL -c)
        #[arg(short = 'c', long = "command")]
        shell_cmd: Option<String>,

        /// Extract events from output after command completes (overrides config)
        #[arg(short = 'x', long = "extract", conflicts_with = "no_extract")]
        extract: bool,

        /// Disable event extraction (overrides config)
        #[arg(short = 'X', long = "no-extract", conflicts_with = "extract")]
        no_extract: bool,

        /// Override format detection for event extraction (e.g., gcc, pytest, cargo)
        #[arg(short = 'f', long = "extract-format")]
        format: Option<String>,

        /// The command to run (alternative to -c)
        #[arg(trailing_var_arg = true)]
        cmd: Vec<String>,
    },

    /// Save output from stdin or file to BIRD
    Save {
        /// File to read output from (reads stdin if not provided, unless --stdout/--stderr used)
        file: Option<String>,

        /// Command string (required for Phase A)
        #[arg(short = 'c', long = "command", required = true)]
        command: String,

        /// Exit code of the command
        #[arg(short = 'x', long = "exit-code", default_value = "0")]
        exit_code: i32,

        /// Duration in milliseconds
        #[arg(short = 'd', long = "duration")]
        duration_ms: Option<i64>,

        /// Stream name when reading from stdin/file (default: stdout)
        #[arg(short = 's', long = "stream", default_value = "stdout")]
        stream: String,

        /// File containing stdout (for capturing both streams)
        #[arg(short = 'o', long = "stdout", conflicts_with = "file")]
        stdout_file: Option<String>,

        /// File containing stderr (for capturing both streams)
        #[arg(short = 'e', long = "stderr", conflicts_with = "file")]
        stderr_file: Option<String>,

        /// Session ID (default: shell-{PPID})
        #[arg(long = "session-id")]
        session_id: Option<String>,

        /// Invoker PID (default: PPID)
        #[arg(long = "invoker-pid")]
        invoker_pid: Option<u32>,

        /// Invoker name (default: basename of $SHELL)
        #[arg(long = "invoker")]
        invoker: Option<String>,

        /// Invoker type (default: shell)
        #[arg(long = "invoker-type", default_value = "shell")]
        invoker_type: String,
    },

    /// Show output from a previous command
    Show {
        /// Command ID or offset (negative for relative, e.g., -1 for last)
        #[arg(default_value = "-1", allow_hyphen_values = true)]
        selector: String,

        /// Filter by stream: O/stdout, E/stderr, A/all (combined to stdout)
        /// Default (no flag) shows streams routed to their original fds
        #[arg(short = 's', long = "stream")]
        stream: Option<String>,

        /// Shortcut for -s stdout
        #[arg(short = 'O', long = "stdout", conflicts_with = "stream")]
        stdout_only: bool,

        /// Shortcut for -s stderr
        #[arg(short = 'E', long = "stderr", conflicts_with = "stream")]
        stderr_only: bool,

        /// Shortcut for -s all (combined to stdout)
        #[arg(short = 'A', long = "all", conflicts_with = "stream")]
        all_combined: bool,

        /// Pipe output through pager ($PAGER or less -R)
        #[arg(short = 'P', long = "pager")]
        pager: bool,

        /// Raw output - preserve ANSI escape codes (default)
        #[arg(short = 'R', long = "raw", conflicts_with = "strip")]
        raw: bool,

        /// Strip ANSI escape codes from output
        #[arg(long = "strip")]
        strip: bool,

        /// Show first N lines
        #[arg(long = "head", value_name = "N")]
        head: Option<usize>,

        /// Show last N lines
        #[arg(short = 't', long = "tail", value_name = "N")]
        tail: Option<usize>,

        /// Limit output to N lines (same as --head)
        #[arg(short = 'n', long = "lines", value_name = "N", conflicts_with = "head")]
        lines: Option<usize>,
    },

    /// Show recent command history
    History {
        /// Number of commands to show
        #[arg(short = 'n', default_value = "20")]
        limit: usize,
    },

    /// Execute SQL query
    Sql {
        /// SQL query to execute
        query: String,
    },

    /// Show statistics
    Stats,

    /// Move old data from recent to archive
    Archive {
        /// Archive data older than this many days
        #[arg(short = 'd', long = "days", default_value = "14")]
        days: u32,

        /// Show what would be done without making changes
        #[arg(short = 'n', long = "dry-run")]
        dry_run: bool,
    },

    /// Compact parquet files to reduce storage and improve query performance
    Compact {
        /// Compact when a session has more than this many files (keeps this many recent)
        #[arg(short = 't', long = "threshold", default_value = "50")]
        file_threshold: usize,

        /// Only compact files for this specific session (used by shell hooks)
        #[arg(short = 's', long = "session")]
        session: Option<String>,

        /// Only check today's partition (fast check for shell hooks)
        #[arg(long = "today")]
        today_only: bool,

        /// Suppress output unless compaction occurs
        #[arg(short = 'q', long = "quiet")]
        quiet: bool,

        /// Only compact recent data (skip archive tier)
        #[arg(long = "recent-only")]
        recent_only: bool,

        /// Only compact archive tier (skip recent)
        #[arg(long = "archive-only")]
        archive_only: bool,

        /// Show what would be done without making changes
        #[arg(short = 'n', long = "dry-run")]
        dry_run: bool,
    },

    /// Shell hook integration
    Hook {
        #[command(subcommand)]
        action: HookAction,
    },

    /// Query parsed events (errors, warnings, test results) from invocation outputs
    Events {
        /// Filter by severity (error, warning, info, note)
        #[arg(short = 's', long = "severity")]
        severity: Option<String>,

        /// Filter by command pattern (glob, e.g., "*cargo*")
        #[arg(short = 'c', long = "cmd")]
        cmd_pattern: Option<String>,

        /// Filter by client ID
        #[arg(long = "client")]
        client: Option<String>,

        /// Filter by hostname
        #[arg(long = "hostname")]
        hostname: Option<String>,

        /// Events from the last N invocations
        #[arg(short = 'n', long = "last")]
        last_n: Option<usize>,

        /// Filter events by invocation ID
        #[arg(short = 'i', long = "invocation")]
        invocation_id: Option<String>,

        /// Show count only
        #[arg(long = "count")]
        count_only: bool,

        /// Maximum number of events to show
        #[arg(short = 'l', long = "limit", default_value = "50")]
        limit: usize,

        /// Re-parse events from original blobs (ignore cached events)
        #[arg(long = "reparse")]
        reparse: bool,

        /// Override format detection (e.g., gcc, pytest, cargo)
        #[arg(short = 'f', long = "format")]
        format: Option<String>,
    },

    /// Extract events from an invocation's output
    ExtractEvents {
        /// Invocation ID (default: last invocation)
        #[arg(default_value = "-1", allow_hyphen_values = true)]
        selector: String,

        /// Override format detection (default: auto or from config)
        #[arg(short = 'f', long = "format")]
        format: Option<String>,

        /// Suppress output (for shell hooks)
        #[arg(short = 'q', long = "quiet")]
        quiet: bool,

        /// Re-extract even if events already exist
        #[arg(long = "force")]
        force: bool,
    },
}

#[derive(Subcommand)]
enum HookAction {
    /// Output shell integration code (add to .zshrc/.bashrc)
    Init {
        /// Shell type (zsh, bash). Auto-detected from $SHELL if not specified.
        #[arg(short, long)]
        shell: Option<String>,
    },
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Init => commands::init(),
        Commands::Run { shell_cmd, extract, no_extract, format, cmd } => {
            // Resolve extract behavior: --extract forces on, --no-extract forces off, otherwise use config
            let extract_override = if extract {
                Some(true)
            } else if no_extract {
                Some(false)
            } else {
                None
            };
            commands::run(shell_cmd.as_deref(), &cmd, extract_override, format.as_deref())
        }
        Commands::Save { file, command, exit_code, duration_ms, stream, stdout_file, stderr_file, session_id, invoker_pid, invoker, invoker_type } => {
            commands::save(
                file.as_deref(),
                &command,
                exit_code,
                duration_ms,
                &stream,
                stdout_file.as_deref(),
                stderr_file.as_deref(),
                session_id.as_deref(),
                invoker_pid,
                invoker.as_deref(),
                &invoker_type,
            )
        }
        Commands::Show { selector, stream, stdout_only, stderr_only, all_combined, pager, raw: _, strip, head, tail, lines } => {
            // Resolve stream from flags or -s value
            let resolved_stream = if stdout_only {
                Some("stdout")
            } else if stderr_only {
                Some("stderr")
            } else if all_combined {
                Some("all")
            } else {
                stream.as_deref()
            };
            let opts = commands::ShowOptions {
                pager,
                strip_ansi: strip,
                head: head.or(lines),
                tail,
            };
            commands::show(&selector, resolved_stream, &opts)
        }
        Commands::History { limit } => commands::history(limit),
        Commands::Sql { query } => commands::sql(&query),
        Commands::Stats => commands::stats(),
        Commands::Archive { days, dry_run } => commands::archive(days, dry_run),
        Commands::Compact { file_threshold, session, today_only, quiet, recent_only, archive_only, dry_run } => {
            commands::compact(file_threshold, session.as_deref(), today_only, quiet, recent_only, archive_only, dry_run)
        }
        Commands::Hook { action } => match action {
            HookAction::Init { shell } => commands::hook_init(shell.as_deref()),
        },
        Commands::Events { severity, cmd_pattern, client, hostname, last_n, invocation_id, count_only, limit, reparse, format } => {
            commands::events(
                severity.as_deref(),
                cmd_pattern.as_deref(),
                client.as_deref(),
                hostname.as_deref(),
                last_n,
                invocation_id.as_deref(),
                count_only,
                limit,
                reparse,
                format.as_deref(),
            )
        }
        Commands::ExtractEvents { selector, format, quiet, force } => {
            commands::extract_events(&selector, format.as_deref(), quiet, force)
        }
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
