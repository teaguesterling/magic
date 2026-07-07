//! Privacy protections for captured commands.
//!
//! Two layers, both applied to the *default* capture path (not just the
//! opt-in retrospective buffer):
//!
//! 1. **Exclusion** — commands matching sensitive patterns
//!    (`config.privacy.exclude_patterns`) are not persisted at all.
//! 2. **Redaction** — commands that are stored get a best-effort redaction
//!    pass over recognizable secret shapes (env assignments to secret-named
//!    variables, `--password`/`-p<pw>` values, `Bearer` tokens, `token=`
//!    values, well-known credential prefixes like `ghp_`/`AKIA`).
//!
//! Redaction is **best-effort pattern matching, not a guarantee**: secrets
//! passed in shapes we do not recognize are stored verbatim. Exclusion
//! patterns are the stronger tool; users handling unusual secret shapes
//! should extend `privacy.exclude_patterns` or use the leading-space /
//! `SHQ_DISABLED` escapes.

use crate::Config;

/// Replacement text for redacted secret values.
pub const REDACTED: &str = "[REDACTED]";

/// Check whether a command must be excluded from persistence entirely.
pub fn should_exclude(config: &Config, cmd: &str) -> bool {
    config
        .privacy
        .exclude_patterns
        .iter()
        .any(|p| matches_glob_pattern(p, cmd))
}

/// Substrings that mark a `name=value` or `--name value` pair as secret.
const SECRET_NAME_MARKERS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "token",
    "apikey",
    "api_key",
    "api-key",
    "privatekey",
    "private_key",
    "private-key",
    "credential",
    "accesskey",
    "access_key",
    "access-key",
    "sessionkey",
    "session_key",
    "session-key",
    "bearer",
];

/// Long-form options whose *next* argument is a secret value.
const SECRET_VALUE_OPTIONS: &[&str] = &[
    "--password",
    "--passwd",
    "--secret",
    "--token",
    "--api-key",
    "--apikey",
    "--access-token",
    "--auth-token",
    "--client-secret",
    "--private-key",
    "--session-token",
];

/// Well-known credential prefixes (redact the whole token when seen).
const SECRET_TOKEN_PREFIXES: &[&str] = &[
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "glpat-",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xoxr-",
    "xoxs-",
    "sk-ant-",
];

/// Commands whose attached `-p<value>` argument is a password.
/// Deliberately narrow: `-p` means "port" or "parents" for many other tools.
const ATTACHED_P_COMMANDS: &[&str] = &["mysql", "mysqldump", "mysqladmin", "mariadb", "mariadb-dump"];

/// Redact recognizable secret values in a command line.
///
/// The command structure (program, flags, whitespace) is preserved; only the
/// secret values are replaced with [`REDACTED`]. Best-effort — see module docs.
pub fn redact_command(cmd: &str) -> String {
    let first_word = cmd
        .split_whitespace()
        .next()
        .map(|w| {
            // basename of the executable, lowercased
            w.rsplit('/').next().unwrap_or(w).to_ascii_lowercase()
        })
        .unwrap_or_default();
    let attached_p = ATTACHED_P_COMMANDS.iter().any(|c| *c == first_word);

    let mut out = String::with_capacity(cmd.len());
    let mut redact_next_word = false;

    // Walk whitespace-delimited tokens, preserving the exact whitespace.
    let mut rest = cmd;
    while !rest.is_empty() {
        // Leading whitespace
        let token_start = rest
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(rest.len());
        out.push_str(&rest[..token_start]);
        rest = &rest[token_start..];
        if rest.is_empty() {
            break;
        }
        // Token
        let token_end = rest
            .find(|c: char| c.is_whitespace())
            .unwrap_or(rest.len());
        let token = &rest[..token_end];
        rest = &rest[token_end..];

        if redact_next_word {
            redact_next_word = false;
            // `--password` with no value prompts interactively; the next
            // token may be another flag rather than a secret.
            if !token.starts_with('-') {
                out.push_str(&replace_token_value(token));
                continue;
            }
        }

        let bare = trim_token(token);
        let bare_lower = bare.to_ascii_lowercase();

        // `--password foo` / `--token foo` style: flag now, value next word.
        if SECRET_VALUE_OPTIONS.iter().any(|o| *o == bare_lower) {
            out.push_str(token);
            redact_next_word = true;
            continue;
        }

        // `Bearer <token>` (e.g. inside an Authorization header argument).
        if bare_lower == "bearer" {
            out.push_str(token);
            redact_next_word = true;
            continue;
        }

        // Attached mysql-style password: `-pSECRET`.
        if attached_p && token.starts_with("-p") && token.len() > 2 && !token.starts_with("--") {
            out.push_str("-p");
            out.push_str(REDACTED);
            continue;
        }

        // Well-known credential prefixes anywhere in the token
        // (covers `https://user:ghp_xxx@host` embeddings too).
        if SECRET_TOKEN_PREFIXES
            .iter()
            .any(|p| bare_lower.contains(p))
            || looks_like_aws_key_id(bare)
        {
            out.push_str(&replace_token_value(token));
            continue;
        }

        // `name=value` pairs (env assignments, --opt=value, URL params).
        out.push_str(&redact_kv_in_token(token));
    }

    out
}

/// AKIA/ASIA-prefixed AWS access key ids (20 uppercase alphanumerics).
fn looks_like_aws_key_id(tok: &str) -> bool {
    (tok.starts_with("AKIA") || tok.starts_with("ASIA"))
        && tok.len() == 20
        && tok.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

/// Strip surrounding quote characters from a token for matching purposes.
fn trim_token(token: &str) -> &str {
    token.trim_matches(|c| c == '"' || c == '\'' || c == ':' || c == ',' || c == ';')
}

/// Replace a token's content with [REDACTED], preserving trailing quotes.
fn replace_token_value(token: &str) -> String {
    let mut leading = 0;
    for c in token.chars() {
        if c == '"' || c == '\'' {
            leading += c.len_utf8();
        } else {
            break;
        }
    }
    let trailing_start = token
        .char_indices()
        .rev()
        .take_while(|(_, c)| *c == '"' || *c == '\'')
        .map(|(i, _)| i)
        .last()
        .unwrap_or(token.len());
    let trailing_start = trailing_start.max(leading);
    format!(
        "{}{}{}",
        &token[..leading],
        REDACTED,
        &token[trailing_start..]
    )
}

/// Redact `marker=value` occurrences inside a single token.
///
/// Handles `export AWS_SECRET_ACCESS_KEY=...`, `--password=...`, and URL
/// query parameters like `?api_token=...` (value ends at `&`, `;`, quote,
/// or end of token).
fn redact_kv_in_token(token: &str) -> String {
    let lower = token.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut out = String::with_capacity(token.len());
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == b'=' {
            // Look back at the name run [A-Za-z0-9_-] preceding '='.
            let mut name_start = i;
            while name_start > 0 {
                let c = bytes[name_start - 1];
                if c.is_ascii_alphanumeric() || c == b'_' || c == b'-' {
                    name_start -= 1;
                } else {
                    break;
                }
            }
            let name = &lower[name_start..i];
            let is_secret = !name.is_empty()
                && SECRET_NAME_MARKERS.iter().any(|m| name.contains(m));

            if is_secret {
                // Find where the value ends.
                let value_start = i + 1;
                let mut value_end = token.len();
                for (j, c) in token[value_start..].char_indices() {
                    if matches!(c, '&' | ';' | '"' | '\'') {
                        value_end = value_start + j;
                        break;
                    }
                }
                out.push('=');
                if value_end > value_start {
                    out.push_str(REDACTED);
                }
                // The remainder may contain more k=v pairs (URL params).
                out.push_str(&redact_kv_in_token(&token[value_end..]));
                return out;
            }
        }
        // Copy this byte's char from the original token.
        let ch_len = token[i..].chars().next().map(|c| c.len_utf8()).unwrap_or(1);
        out.push_str(&token[i..i + ch_len]);
        i += ch_len;
    }

    out
}

/// Simple glob pattern matching.
///
/// Supports:
/// - `*` matches any sequence of characters
/// - Case-insensitive matching
pub fn matches_glob_pattern(pattern: &str, text: &str) -> bool {
    if !pattern.contains('*') {
        return pattern.eq_ignore_ascii_case(text);
    }

    let pattern_lower = pattern.to_lowercase();
    let text_lower = text.to_lowercase();

    let parts: Vec<&str> = pattern_lower.split('*').collect();

    if parts.is_empty() {
        return true;
    }

    let mut pos = 0;

    // First part must match at start (unless pattern starts with *)
    if !pattern_lower.starts_with('*') {
        if !text_lower.starts_with(parts[0]) {
            return false;
        }
        pos = parts[0].len();
    }

    // Middle parts must appear in order
    for part in parts.iter().skip(if pattern_lower.starts_with('*') { 0 } else { 1 }) {
        if part.is_empty() {
            continue;
        }
        if let Some(found) = text_lower[pos..].find(part) {
            pos += found + part.len();
        } else {
            return false;
        }
    }

    // Last part must match at end (unless pattern ends with *)
    if !pattern_lower.ends_with('*') && !parts.is_empty() {
        let last = parts.last().unwrap();
        if !last.is_empty() && !text_lower.ends_with(last) {
            return false;
        }
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_redacts_secret_env_assignment() {
        let out = redact_command("export AWS_SECRET_ACCESS_KEY=CANARY123");
        assert!(!out.contains("CANARY123"), "secret leaked: {}", out);
        assert!(out.starts_with("export AWS_SECRET_ACCESS_KEY="));
        assert!(out.contains(REDACTED));
    }

    #[test]
    fn test_redacts_password_flag_value() {
        let out = redact_command("mysql --password=hunter2 -e 'select 1'");
        assert!(!out.contains("hunter2"), "secret leaked: {}", out);

        let out = redact_command("some-tool --password hunter2 --verbose");
        assert!(!out.contains("hunter2"), "secret leaked: {}", out);
        assert!(out.contains("--verbose"));
    }

    #[test]
    fn test_redacts_mysql_attached_password() {
        let out = redact_command("mysql -phunter2 -u root db");
        assert!(!out.contains("hunter2"), "secret leaked: {}", out);
        assert!(out.contains("-p[REDACTED]"));
        assert!(out.contains("-u root db"));
    }

    #[test]
    fn test_does_not_mangle_port_or_parents_flags() {
        // -p means port for psql, parents for mkdir: must NOT redact.
        assert_eq!(redact_command("psql -p5432 -h localhost"), "psql -p5432 -h localhost");
        assert_eq!(redact_command("mkdir -p foo/bar"), "mkdir -p foo/bar");
        assert_eq!(redact_command("cp -p a b"), "cp -p a b");
    }

    #[test]
    fn test_redacts_bearer_token() {
        let out = redact_command(r#"curl -H "Authorization: Bearer eyJabc.def.ghi" https://api"#);
        assert!(!out.contains("eyJabc"), "secret leaked: {}", out);
        assert!(out.contains("Bearer [REDACTED]\""), "quote not preserved: {}", out);
        assert!(out.contains("https://api"));
    }

    #[test]
    fn test_redacts_url_token_param() {
        let out = redact_command("curl https://example.com/x?a=1&api_token=sekrit&b=2");
        assert!(!out.contains("sekrit"), "secret leaked: {}", out);
        assert!(out.contains("a=1"));
        assert!(out.contains("b=2"));
    }

    #[test]
    fn test_redacts_known_credential_prefixes() {
        let out = redact_command("git push https://x:ghp_abcdefghijklmnop@github.com/u/r");
        // whole token replaced (contains the credential)
        assert!(!out.contains("ghp_abcdefghijklmnop"), "secret leaked: {}", out);

        let out = redact_command("aws configure set aws_access_key_id AKIAIOSFODNN7EXAMPLE");
        assert!(!out.contains("AKIAIOSFODNN7EXAMPLE"), "secret leaked: {}", out);
    }

    #[test]
    fn test_leaves_benign_commands_untouched() {
        for cmd in [
            "cargo build --release",
            "git status",
            "ls -la /tmp",
            "echo hello world",
            "make -j8 test",
        ] {
            assert_eq!(redact_command(cmd), cmd);
        }
    }

    #[test]
    fn test_redaction_preserves_structure_multibyte() {
        // Multibyte input must not panic and must preserve non-secret text.
        let out = redact_command("echo héllo ★ MY_TOKEN=秘密の値 done");
        assert!(!out.contains("秘密の値"), "secret leaked: {}", out);
        assert!(out.contains("héllo ★"));
        assert!(out.contains("done"));
    }

    #[test]
    fn test_should_exclude_uses_privacy_patterns() {
        let config = Config::with_root("/tmp/test-privacy");
        assert!(should_exclude(&config, "export API_TOKEN=xyz"));
        assert!(should_exclude(&config, "ssh user@host"));
        assert!(should_exclude(&config, "printenv"));
        assert!(!should_exclude(&config, "cargo build"));
        assert!(!should_exclude(&config, "git status"));
    }
}
