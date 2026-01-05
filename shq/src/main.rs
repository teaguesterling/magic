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

    /// Shell hook integration
    Hook {
        #[command(subcommand)]
        action: HookAction,
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
        Commands::Run { shell_cmd, cmd } => commands::run(shell_cmd.as_deref(), &cmd),
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
        Commands::Show { selector, stream, stdout_only, stderr_only, all_combined } => {
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
            commands::show(&selector, resolved_stream)
        }
        Commands::History { limit } => commands::history(limit),
        Commands::Sql { query } => commands::sql(&query),
        Commands::Stats => commands::stats(),
        Commands::Hook { action } => match action {
            HookAction::Init { shell } => commands::hook_init(shell.as_deref()),
        },
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}
