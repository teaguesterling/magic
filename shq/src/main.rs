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
    Init {
        /// Storage mode: parquet (multi-writer, needs compaction) or duckdb (single-writer, simpler)
        #[arg(short = 'm', long = "mode", default_value = "parquet")]
        mode: String,
    },

    /// Run a command and capture it to BIRD
    #[command(visible_alias = "r")]
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

        /// Run compaction in background after command completes
        #[arg(short = 'C', long = "compact")]
        compact: bool,

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

        /// Extract events after saving (uses config default if not specified)
        #[arg(long = "extract")]
        extract: bool,

        /// Run compaction check after saving
        #[arg(long = "compact")]
        compact: bool,

        /// Suppress informational output
        #[arg(short = 'q', long = "quiet")]
        quiet: bool,
    },

    /// Show captured output from invocation(s)
    #[command(visible_aliases = ["o", "show"])]
    Output {
        /// Query selector (e.g., ~1, shell:%exit<>0~5, %/make/~1)
        #[arg(default_value = "~1")]
        query: String,

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

    /// List invocation history
    #[command(visible_aliases = ["i", "history"])]
    Invocations {
        /// Query selector (e.g., ~20, shell:~10, %exit<>0~5)
        #[arg(default_value = "~20")]
        query: String,

        /// Output format: table, json, oneline
        #[arg(short = 'f', long = "format", default_value = "table")]
        format: String,
    },

    /// Show detailed info about an invocation
    #[command(visible_alias = "I")]
    Info {
        /// Query selector (e.g., ~1, %/make/~1)
        #[arg(default_value = "~1")]
        query: String,

        /// Output format: table, json
        #[arg(short = 'f', long = "format", default_value = "table")]
        format: String,
    },

    /// Re-run a previous command
    #[command(visible_aliases = ["R", "!!"])]
    Rerun {
        /// Query selector (e.g., ~1, ~3, %/make/~1)
        #[arg(default_value = "~1")]
        query: String,

        /// Print command without executing
        #[arg(short = 'n', long = "dry-run")]
        dry_run: bool,

        /// Don't capture the re-run (just execute)
        #[arg(short = 'N', long = "no-capture")]
        no_capture: bool,
    },

    /// Execute SQL query
    #[command(visible_alias = "q")]
    Sql {
        /// SQL query to execute
        query: String,
    },

    /// Quick reference for commands and query syntax
    #[command(name = "quick-help", visible_alias = "?")]
    QuickHelp,

    /// Show statistics
    Stats {
        /// Output format: table, json
        #[arg(short = 'f', long = "format", default_value = "table")]
        format: String,
    },

    /// Move old data from recent to archive
    Archive {
        /// Archive data older than this many days
        #[arg(short = 'd', long = "days", default_value = "14")]
        days: u32,

        /// Show what would be done without making changes
        #[arg(short = 'n', long = "dry-run")]
        dry_run: bool,

        /// Extract events from invocations before archiving (backfill)
        #[arg(short = 'x', long = "extract-first")]
        extract_first: bool,
    },

    /// Compact parquet files to reduce storage and improve query performance
    Compact {
        /// Compact when a session has more than this many non-compacted files
        #[arg(short = 't', long = "threshold", default_value = "50")]
        file_threshold: usize,

        /// Re-compact when more than this many compacted files exist (0 = disabled)
        #[arg(short = 'r', long = "recompact-threshold", default_value = "10")]
        recompact_threshold: usize,

        /// Consolidate ALL files into a single file per session (full merge)
        #[arg(short = 'c', long = "consolidate")]
        consolidate: bool,

        /// Extract events from invocations before compacting
        #[arg(short = 'x', long = "extract-first")]
        extract_first: bool,

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

    /// Manage format detection hints
    #[command(name = "format-hints", visible_alias = "fh")]
    FormatHints {
        #[command(subcommand)]
        action: FormatHintsAction,
    },

    /// Manage remote storage connections
    Remote {
        #[command(subcommand)]
        action: RemoteAction,
    },

    /// Query parsed events (errors, warnings, test results) from invocation outputs
    #[command(visible_alias = "e")]
    Events {
        /// Query selector (e.g., ~10, %exit<>0~5, %/cargo/~10)
        #[arg(default_value = "~10")]
        query: String,

        /// Filter by severity (error, warning, info, note)
        #[arg(short = 's', long = "severity")]
        severity: Option<String>,

        /// Show count only
        #[arg(long = "count")]
        count_only: bool,

        /// Number of events: N (any), +N (first N), -N (last N)
        #[arg(short = 'n', long = "lines", default_value = "50", allow_hyphen_values = true)]
        lines: String,

        /// Re-parse events from original blobs (ignore cached events)
        #[arg(long = "reparse")]
        reparse: bool,

        /// Override format detection (e.g., gcc, pytest, cargo)
        #[arg(short = 'f', long = "format")]
        format: Option<String>,
    },

    /// Update DuckDB extensions to latest versions
    UpdateExtensions {
        /// Show what would be done without making changes
        #[arg(short = 'n', long = "dry-run")]
        dry_run: bool,
    },

    /// Extract events from an invocation's output
    ExtractEvents {
        /// Invocation ID (default: last invocation, ignored if --all)
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

        /// Extract from all invocations that don't have events yet
        #[arg(short = 'a', long = "all")]
        all: bool,

        /// Only process invocations since this date (YYYY-MM-DD, default: 30 days ago)
        #[arg(long = "since")]
        since: Option<String>,

        /// Maximum number of invocations to process (default: 1000)
        #[arg(short = 'n', long = "limit")]
        limit: Option<usize>,

        /// Show what would be extracted without actually extracting
        #[arg(long = "dry-run")]
        dry_run: bool,
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

#[derive(Subcommand)]
enum FormatHintsAction {
    /// List format hints (user-defined and built-in)
    List {
        /// Filter by format name or pattern (substring match)
        filter: Option<String>,

        /// Show only user-defined hints
        #[arg(short = 'u', long)]
        user_only: bool,

        /// Show only built-in hints from duck_hunt
        #[arg(short = 'b', long)]
        builtin_only: bool,
    },

    /// Add a format hint
    Add {
        /// Glob pattern to match (e.g., "*mycompiler*", "custom-build*")
        pattern: String,

        /// Format name (e.g., gcc, pytest, cargo_build)
        format: String,

        /// Priority (higher wins, default: 500)
        #[arg(short = 'p', long)]
        priority: Option<i32>,
    },

    /// Remove a format hint by pattern
    Remove {
        /// Pattern to remove
        pattern: String,
    },

    /// Check which format would be detected for a command
    Check {
        /// Command to check
        command: String,
    },

    /// Set the default format (when no patterns match)
    SetDefault {
        /// Default format (e.g., auto, text)
        format: String,
    },
}

#[derive(Subcommand)]
enum RemoteAction {
    /// Add a remote storage connection
    Add {
        /// Name for this remote (e.g., team, backup, ci)
        name: String,

        /// Remote type: s3, motherduck, postgres, or file
        #[arg(short = 't', long = "type")]
        remote_type: String,

        /// URI for the remote (e.g., s3://bucket/path/bird.duckdb, md:database_name)
        #[arg(short = 'u', long)]
        uri: String,

        /// Mount as read-only
        #[arg(long)]
        read_only: bool,

        /// Credential provider for S3 (e.g., credential_chain)
        #[arg(long)]
        credential_provider: Option<String>,

        /// Don't auto-attach on connection open
        #[arg(long)]
        no_auto_attach: bool,
    },

    /// List configured remotes
    List,

    /// Remove a remote configuration
    Remove {
        /// Name of the remote to remove
        name: String,
    },

    /// Test connection to a remote
    Test {
        /// Name of the remote to test (tests all if not specified)
        name: Option<String>,
    },

    /// Manually attach a remote (for current session only)
    Attach {
        /// Name of the remote to attach
        name: String,
    },

    /// Show sync status
    Status,
}

/// Parse lines argument: N (any), +N (first N), -N (last N).
fn parse_lines_arg(s: &str) -> (usize, commands::LimitOrder) {
    use commands::LimitOrder;
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('+') {
        (rest.parse().unwrap_or(50), LimitOrder::First)
    } else if let Some(rest) = s.strip_prefix('-') {
        (rest.parse().unwrap_or(50), LimitOrder::Last)
    } else {
        (s.parse().unwrap_or(50), LimitOrder::Any)
    }
}

fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Commands::Init { mode } => commands::init(&mode),
        Commands::Run { shell_cmd, extract, no_extract, format, compact, cmd } => {
            // Resolve extract behavior: --extract forces on, --no-extract forces off, otherwise use config
            let extract_override = if extract {
                Some(true)
            } else if no_extract {
                Some(false)
            } else {
                None
            };
            commands::run(shell_cmd.as_deref(), &cmd, extract_override, format.as_deref(), compact)
        }
        Commands::Save { file, command, exit_code, duration_ms, stream, stdout_file, stderr_file, session_id, invoker_pid, invoker, invoker_type, extract, compact, quiet } => {
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
                extract,
                compact,
                quiet,
            )
        }
        Commands::Output { query, stream, stdout_only, stderr_only, all_combined, pager, raw: _, strip, head, tail, lines } => {
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
            let opts = commands::OutputOptions {
                pager,
                strip_ansi: strip,
                head: head.or(lines),
                tail,
            };
            commands::output(&query, resolved_stream, &opts)
        }
        Commands::Invocations { query, format } => commands::invocations(&query, &format),
        Commands::Info { query, format } => commands::info(&query, &format),
        Commands::Rerun { query, dry_run, no_capture } => commands::rerun(&query, dry_run, no_capture),
        Commands::Sql { query } => commands::sql(&query),
        Commands::QuickHelp => commands::quick_help(),
        Commands::Stats { format } => commands::stats(&format),
        Commands::Archive { days, dry_run, extract_first } => commands::archive(days, dry_run, extract_first),
        Commands::Compact { file_threshold, recompact_threshold, consolidate, extract_first, session, today_only, quiet, recent_only, archive_only, dry_run } => {
            commands::compact(file_threshold, recompact_threshold, consolidate, extract_first, session.as_deref(), today_only, quiet, recent_only, archive_only, dry_run)
        }
        Commands::Hook { action } => match action {
            HookAction::Init { shell } => commands::hook_init(shell.as_deref()),
        },
        Commands::FormatHints { action } => match action {
            FormatHintsAction::List { filter, user_only, builtin_only } => {
                let show_builtin = !user_only;
                let show_user = !builtin_only;
                commands::format_hints_list(show_builtin, show_user, filter.as_deref())
            },
            FormatHintsAction::Add { pattern, format, priority } => {
                commands::format_hints_add(&pattern, &format, priority)
            },
            FormatHintsAction::Remove { pattern } => commands::format_hints_remove(&pattern),
            FormatHintsAction::Check { command } => commands::format_hints_check(&command),
            FormatHintsAction::SetDefault { format } => commands::format_hints_set_default(&format),
        },
        Commands::Remote { action } => match action {
            RemoteAction::Add { name, remote_type, uri, read_only, credential_provider, no_auto_attach } => {
                commands::remote_add(&name, &remote_type, &uri, read_only, credential_provider.as_deref(), !no_auto_attach)
            },
            RemoteAction::List => commands::remote_list(),
            RemoteAction::Remove { name } => commands::remote_remove(&name),
            RemoteAction::Test { name } => commands::remote_test(name.as_deref()),
            RemoteAction::Attach { name } => commands::remote_attach(&name),
            RemoteAction::Status => commands::remote_status(),
        },
        Commands::Events { query, severity, count_only, lines, reparse, format } => {
            // Parse lines: N (any), +N (first N), -N (last N)
            let (limit, order) = parse_lines_arg(&lines);
            commands::events(&query, severity.as_deref(), count_only, limit, order, reparse, format.as_deref())
        }
        Commands::UpdateExtensions { dry_run } => commands::update_extensions(dry_run),
        Commands::ExtractEvents { selector, format, quiet, force, all, since, limit, dry_run } => {
            commands::extract_events(&selector, format.as_deref(), quiet, force, all, since.as_deref(), limit, dry_run)
        }
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
