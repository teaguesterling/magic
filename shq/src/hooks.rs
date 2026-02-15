//! Shell hook generation for shq.
//!
//! Generates shell integration scripts for bash and zsh with various modes:
//! - Active: Full hook registration with command tracking
//! - Inactive: Only aliases, no automatic tracking
//! - With/without prompt indicator

/// Shell type for hook generation
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shell {
    Bash,
    Zsh,
}

/// Hook mode
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    Active,
    Inactive,
}

/// Generate a shell hook script.
pub fn generate(shell: Shell, mode: Mode, prompt_indicator: bool) -> String {
    let mut out = String::with_capacity(4096);

    // Header comment
    out.push_str(&header(shell, mode, prompt_indicator));

    // Session ID
    out.push_str(&session_id(shell));

    // Active mode: full hooks
    if mode == Mode::Active {
        out.push_str(&ignore_patterns(shell));
        out.push_str(&should_ignore_fn(shell));
        out.push_str(&hook_functions(shell));
        out.push_str(&shqr_function(shell));
        out.push_str(&on_off_functions(shell, prompt_indicator));
        out.push_str(&register_hooks(shell));
    } else {
        // Inactive mode: just on/off functions
        out.push_str(&inactive_on_off_functions(shell));
    }

    // Prompt indicator (if enabled)
    if prompt_indicator {
        out.push_str(&prompt_indicator_setup(shell, mode));
    }

    // Convenience aliases (always)
    out.push_str(&aliases());

    // Startup message (for inactive mode)
    if mode == Mode::Inactive {
        out.push_str(&inactive_message());
    }

    out
}

fn header(shell: Shell, mode: Mode, prompt_indicator: bool) -> String {
    let shell_name = match shell {
        Shell::Bash => "bash",
        Shell::Zsh => "zsh",
    };

    let mode_desc = match (mode, prompt_indicator) {
        (Mode::Inactive, _) => " (inactive mode)",
        (Mode::Active, false) => " (no prompt indicator)",
        (Mode::Active, true) => "",
    };

    let setup_hint = match (mode, prompt_indicator) {
        (Mode::Inactive, _) => "# Hooks are not enabled by default - use shq-on to activate\n",
        _ => &format!("# Add to ~/.{}rc: eval \"$(shq hook init --shell {})\"\n", shell_name, shell_name),
    };

    format!(
        r#"# shq shell integration for {shell_name}{mode_desc}
{setup_hint}#
# Privacy escapes (command not recorded):
#   - Start command with a space: " ls -la"
#   - Start command with backslash: "\ls -la"
#
# Temporary disable: export SHQ_DISABLED=1
# Exclude patterns: export SHQ_EXCLUDE="*password*:*secret*"

"#
    )
}

fn session_id(shell: Shell) -> String {
    match shell {
        Shell::Bash => "__shq_session_id=\"bash-$$\"\n\n".to_string(),
        Shell::Zsh => "__shq_session_id=\"zsh-$$\"\n\n".to_string(),
    }
}

fn ignore_patterns(_shell: Shell) -> String {
    r#"# Default ignore patterns (colon-separated) - shq commands, job control, etc.
: ${SHQ_IGNORE:="shq *:shqr *:blq *:%*:fg:fg *:bg:bg *:jobs:jobs *:exit:logout:clear:history:history *"}

"#
    .to_string()
}

fn should_ignore_fn(shell: Shell) -> String {
    // Zsh uses $~pattern for glob matching, bash uses $pattern
    let pattern_match = match shell {
        Shell::Zsh => "$~pattern",
        Shell::Bash => "$pattern",
    };

    format!(
        r#"# Check if command should be ignored (matches SHQ_IGNORE or SHQ_EXCLUDE)
# Commands starting with "shq -X" or "shq --force-capture" bypass ignore patterns
__shq_should_ignore() {{
    local cmd="$1"
    # Force capture flag bypasses all ignore patterns
    [[ "$cmd" == "shq -X "* || "$cmd" == "shq --force-capture "* ]] && return 1
    local IFS=':'
    for pattern in $SHQ_IGNORE; do
        [[ "$cmd" == {pattern_match} ]] && return 0
    done
    [[ -n "$SHQ_EXCLUDE" ]] && for pattern in $SHQ_EXCLUDE; do
        [[ "$cmd" == {pattern_match} ]] && return 0
    done
    return 1
}}

"#
    )
}

fn hook_functions(shell: Shell) -> String {
    match shell {
        Shell::Zsh => zsh_hook_functions(),
        Shell::Bash => bash_hook_functions(),
    }
}

fn zsh_hook_functions() -> String {
    r#"# Check if buffer mode is enabled (cached at shell startup)
__shq_buffer_enabled=""
if shq buffer status 2>/dev/null | grep -q "Enabled: yes"; then
    __shq_buffer_enabled=1
fi

# Capture command before execution
__shq_preexec() {
    __shq_last_cmd="$1"
    __shq_start_time=$EPOCHREALTIME
}

# Capture result after execution (metadata only - no output capture)
__shq_precmd() {
    local exit_code=$?
    local cmd="$__shq_last_cmd"
    __shq_last_cmd=""

    # Skip if disabled, empty, or privacy escape
    [[ -n "$SHQ_DISABLED" ]] && return
    [[ -z "$cmd" ]] && return
    [[ "$cmd" =~ ^[[:space:]] ]] && return
    [[ "$cmd" =~ ^\\ ]] && return
    __shq_should_ignore "$cmd" && return

    # Calculate duration in milliseconds
    local duration=0
    if [[ -n "$__shq_start_time" ]]; then
        duration=$(( (EPOCHREALTIME - __shq_start_time) * 1000 ))
        duration=${duration%.*}
    fi
    __shq_start_time=""

    # Build save command - use buffer if enabled
    local buffer_flag=""
    [[ -n "$__shq_buffer_enabled" ]] && buffer_flag="--to-buffer"

    # Save to BIRD (async, non-blocking)
    (
        shq save -c "$cmd" -x "$exit_code" -d "$duration" \
            --session-id "$__shq_session_id" \
            --invoker-pid $$ --invoker zsh \
            --compact -q $buffer_flag </dev/null \
            2>> "${BIRD_ROOT:-$HOME/.local/share/bird}/errors.log"
    ) &!
}

"#
    .to_string()
}

fn bash_hook_functions() -> String {
    r#"# Check if buffer mode is enabled (cached at shell startup)
__shq_buffer_enabled=""
if shq buffer status 2>/dev/null | grep -q "Enabled: yes"; then
    __shq_buffer_enabled=1
fi

# Millisecond timer (with fallback for older bash)
__shq_now_ms() {
    if [[ -n "$EPOCHREALTIME" ]]; then
        local sec=${EPOCHREALTIME%.*}
        local frac=${EPOCHREALTIME#*.}
        echo $(( sec * 1000 + 10#${frac:0:3} ))
    else
        echo $(( $(date +%s) * 1000 ))
    fi
}

# PS0 hook: fires after command read, before execution
__shq_ps0_hook() {
    __shq_start_ms=$(__shq_now_ms)
}
PS0='${__shq_cmd:+$(__shq_ps0_hook)}'

# PROMPT_COMMAND hook: fires after command completes
__shq_prompt_command() {
    local exit_code=$?
    local cmd
    cmd=$(HISTTIMEFORMAT='' history 1 | sed 's/^[ ]*[0-9]*[ ]*//')

    # Skip if disabled, empty, or privacy escape
    [[ -n "$SHQ_DISABLED" ]] && { __shq_cmd=""; return; }
    [[ -z "$cmd" ]] && return
    [[ "$cmd" =~ ^[[:space:]] ]] && { __shq_cmd=""; return; }
    [[ "$cmd" =~ ^\\ ]] && { __shq_cmd=""; return; }
    __shq_should_ignore "$cmd" && { __shq_cmd=""; return; }

    # Calculate duration
    local duration=0
    if [[ -n "$__shq_start_ms" ]]; then
        local end_ms=$(__shq_now_ms)
        duration=$(( end_ms - __shq_start_ms ))
    fi

    # Set flag for PS0 timing on next command
    __shq_cmd=1
    __shq_start_ms=""

    # Build save command - use buffer if enabled
    local buffer_flag=""
    [[ -n "$__shq_buffer_enabled" ]] && buffer_flag="--to-buffer"

    # Save to BIRD (background, non-blocking)
    (
        shq save -c "$cmd" -x "$exit_code" -d "$duration" \
            --session-id "$__shq_session_id" \
            --invoker-pid $$ --invoker bash \
            --compact -q $buffer_flag </dev/null \
            2>> "${BIRD_ROOT:-$HOME/.local/share/bird}/errors.log"
    ) & disown
}

"#
    .to_string()
}

fn shqr_function(shell: Shell) -> String {
    let bg_syntax = match shell {
        Shell::Zsh => ") &!",
        Shell::Bash => ") & disown",
    };
    let invoker = match shell {
        Shell::Zsh => "zsh",
        Shell::Bash => "bash",
    };

    format!(
        r#"# Run command with full output capture
shqr() {{
    if [[ -n "$SHQ_DISABLED" ]]; then
        "$@"
        return $?
    fi

    local cmd="$*"
    local start_ms=$(__shq_now_ms 2>/dev/null || echo 0)

    # Create temp files for output
    local stdout_file=$(mktemp)
    local stderr_file=$(mktemp)
    trap "rm -f '$stdout_file' '$stderr_file'" EXIT

    # Run command, capturing output
    "$@" > >(tee "$stdout_file") 2> >(tee "$stderr_file" >&2)
    local exit_code=$?

    local end_ms=$(__shq_now_ms 2>/dev/null || echo 0)
    local duration=$(( end_ms - start_ms ))

    # Use buffer if enabled
    local buffer_flag=""
    [[ -n "$__shq_buffer_enabled" ]] && buffer_flag="--to-buffer"

    # Save with captured output (background)
    (
        shq save -c "$cmd" -x "$exit_code" -d "$duration" \
            --stdout "$stdout_file" --stderr "$stderr_file" \
            --session-id "$__shq_session_id" \
            --invoker-pid $$ --invoker {invoker} \
            --compact -q $buffer_flag \
            2>> "${{BIRD_ROOT:-$HOME/.local/share/bird}}/errors.log"
        rm -f "$stdout_file" "$stderr_file"
    {bg_syntax}

    trap - EXIT
    return $exit_code
}}

"#
    )
}

fn on_off_functions(shell: Shell, prompt_indicator: bool) -> String {
    let unalias_list = "% %run %r %rerun %R %history %h %i %output %o %info %I %events %e %stats %s %S %%";

    match shell {
        Shell::Zsh => {
            let restore_ps1 = if prompt_indicator {
                "    [[ -n \"$__shq_orig_ps1\" ]] && PS1=\"$__shq_orig_ps1\"\n    unset __shq_orig_ps1 SHQ_INDICATOR\n"
            } else {
                ""
            };
            let init_flag = if prompt_indicator { "" } else { " --no-prompt-indicator" };

            format!(
                r#"shq-off() {{
    add-zsh-hook -d preexec __shq_preexec
    add-zsh-hook -d precmd __shq_precmd
    unset __shq_last_cmd __shq_start_time __shq_session_id
    unalias {unalias_list} 2>/dev/null
{restore_ps1}    [[ -z "$__shq_quiet" ]] && echo "shq disabled (use shq-on to re-enable)"
    unset __shq_quiet
}}

shq-on() {{
    [[ -n "$__shq_orig_ps1" ]] && PS1="$__shq_orig_ps1"
    unset __shq_orig_ps1 SHQ_INDICATOR
    eval "$(shq hook init --shell zsh{init_flag})"
}}

"#
            )
        }
        Shell::Bash => {
            let restore_ps1 = if prompt_indicator {
                "    [[ -n \"$__shq_orig_ps1\" ]] && PS1=\"$__shq_orig_ps1\"\n    unset __shq_orig_ps1 SHQ_INDICATOR\n"
            } else {
                ""
            };
            let init_flag = if prompt_indicator { "" } else { " --no-prompt-indicator" };

            format!(
                r#"shq-off() {{
    PROMPT_COMMAND="${{PROMPT_COMMAND//__shq_prompt_command; /}}"
    PROMPT_COMMAND="${{PROMPT_COMMAND//__shq_prompt_command;/}}"
    PROMPT_COMMAND="${{PROMPT_COMMAND//__shq_prompt_command/}}"
    PROMPT_COMMAND="${{PROMPT_COMMAND#; }}"; PROMPT_COMMAND="${{PROMPT_COMMAND#;}}"
    unset __shq_cmd __shq_start_ms __shq_session_id PS0
    unalias {unalias_list} 2>/dev/null
{restore_ps1}    [[ -z "$__shq_quiet" ]] && echo "shq disabled (use shq-on to re-enable)"
    unset __shq_quiet
}}

shq-on() {{
    [[ -n "$__shq_orig_ps1" ]] && PS1="$__shq_orig_ps1"
    unset __shq_orig_ps1 SHQ_INDICATOR
    eval "$(shq hook init --shell bash{init_flag})"
}}

"#
            )
        }
    }
}

fn inactive_on_off_functions(shell: Shell) -> String {
    let unalias_list = "% %run %r %rerun %R %history %h %i %output %o %info %I %events %e %stats %s %S %%";

    match shell {
        Shell::Zsh => format!(
            r#"shq-off() {{
    unalias {unalias_list} 2>/dev/null
    [[ -n "$__shq_orig_ps1" ]] && PS1="$__shq_orig_ps1"
    unset __shq_orig_ps1 SHQ_INDICATOR
    [[ -z "$__shq_quiet" ]] && echo "shq disabled"
    unset __shq_quiet
}}

shq-on() {{
    [[ -n "$__shq_orig_ps1" ]] && PS1="$__shq_orig_ps1"
    unset __shq_orig_ps1 SHQ_INDICATOR
    eval "$(shq hook init --shell zsh)"
}}

"#
        ),
        Shell::Bash => format!(
            r#"shq-off() {{
    PROMPT_COMMAND="${{PROMPT_COMMAND//__shq_prompt_command; /}}"
    PROMPT_COMMAND="${{PROMPT_COMMAND//__shq_prompt_command;/}}"
    PROMPT_COMMAND="${{PROMPT_COMMAND//__shq_prompt_command/}}"
    PROMPT_COMMAND="${{PROMPT_COMMAND#; }}"; PROMPT_COMMAND="${{PROMPT_COMMAND#;}}"
    unset __shq_cmd __shq_start_ms __shq_session_id PS0
    unalias {unalias_list} 2>/dev/null
    [[ -n "$__shq_orig_ps1" ]] && PS1="$__shq_orig_ps1"
    unset __shq_orig_ps1 SHQ_INDICATOR
    [[ -z "$__shq_quiet" ]] && echo "shq disabled"
    unset __shq_quiet
}}

shq-on() {{
    [[ -n "$__shq_orig_ps1" ]] && PS1="$__shq_orig_ps1"
    unset __shq_orig_ps1 SHQ_INDICATOR
    eval "$(shq hook init --shell bash)"
}}

"#
        ),
    }
}

fn register_hooks(shell: Shell) -> String {
    match shell {
        Shell::Zsh => r#"# Register hooks
autoload -Uz add-zsh-hook
add-zsh-hook preexec __shq_preexec
add-zsh-hook precmd __shq_precmd

"#
        .to_string(),
        Shell::Bash => r#"# Register PROMPT_COMMAND
if [[ -z "$PROMPT_COMMAND" ]]; then
    PROMPT_COMMAND="__shq_prompt_command"
else
    PROMPT_COMMAND="__shq_prompt_command; $PROMPT_COMMAND"
fi

"#
        .to_string(),
    }
}

fn prompt_indicator_setup(shell: Shell, mode: Mode) -> String {
    let indicator = match mode {
        Mode::Active => "●",
        Mode::Inactive => "⏸",
    };

    match shell {
        Shell::Zsh => format!(
            r#"# Prompt indicator
__shq_orig_ps1="$PS1"
export SHQ_INDICATOR="%F{{242}}{indicator}%f "
PS1="${{SHQ_INDICATOR}}${{PS1}}"

"#
        ),
        Shell::Bash => format!(
            r#"# Prompt indicator
__shq_orig_ps1="$PS1"
export SHQ_INDICATOR='\[\033[90m\]{indicator}\[\033[0m\] '
PS1="${{SHQ_INDICATOR}}${{PS1}}"

"#
        ),
    }
}

fn aliases() -> String {
    r#"# Convenience aliases
alias %='shq run'
alias %run='shq run'
alias %r='shq run'
alias %rerun='shq rerun'
alias %R='shq rerun'
alias %history='shq history'
alias %h='shq history'
alias %i='shq history'
alias %output='shq output'
alias %o='shq output'
alias %info='shq info'
alias %I='shq info'
alias %events='shq events'
alias %e='shq events'
alias %%='shq'
alias %stats='shq stats'
alias %s='shq stats'
alias %S='shq stats'
"#
    .to_string()
}

fn inactive_message() -> String {
    r#"
[[ -z "$__shq_quiet" ]] && echo "shq loaded (inactive). Use shq-on to enable hooks."
"#
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_bash_active() {
        let hook = generate(Shell::Bash, Mode::Active, true);
        assert!(hook.contains("__shq_prompt_command"));
        assert!(hook.contains("alias %='shq run'"));
        assert!(hook.contains("SHQ_INDICATOR"));
        assert!(hook.contains("●"));
    }

    #[test]
    fn test_generate_bash_inactive() {
        let hook = generate(Shell::Bash, Mode::Inactive, true);
        // Should NOT have hook registration (PROMPT_COMMAND= assignment)
        assert!(!hook.contains("PROMPT_COMMAND=\"__shq_prompt_command"));
        assert!(hook.contains("alias %='shq run'"));
        assert!(hook.contains("⏸"));
        assert!(hook.contains("shq-on"));
    }

    #[test]
    fn test_generate_zsh_active() {
        let hook = generate(Shell::Zsh, Mode::Active, true);
        assert!(hook.contains("add-zsh-hook"));
        assert!(hook.contains("__shq_preexec"));
        assert!(hook.contains("$~pattern")); // zsh glob syntax
    }

    #[test]
    fn test_generate_no_indicator() {
        let hook = generate(Shell::Bash, Mode::Active, false);
        assert!(hook.contains("__shq_prompt_command"));
        // Should NOT have indicator setup (export SHQ_INDICATOR=)
        assert!(!hook.contains("export SHQ_INDICATOR="));
        // But may reference it in cleanup code
    }

    #[test]
    fn test_aliases_present() {
        let hook = generate(Shell::Bash, Mode::Inactive, true);
        assert!(hook.contains("alias %stats='shq stats'"));
        assert!(hook.contains("alias %s='shq stats'"));
        assert!(hook.contains("alias %S='shq stats'"));
    }
}
