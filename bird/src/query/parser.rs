//! Query parser for the cross-client micro-language.

use std::path::PathBuf;

/// A parsed query containing all components.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Query {
    /// Source selector (host:type:client:session:)
    pub source: Option<SourceSelector>,
    /// Working directory filter
    pub path: Option<PathFilter>,
    /// Field filters and tags
    pub filters: Vec<QueryComponent>,
    /// Range selector (~N or ~N:~M)
    pub range: Option<RangeSelector>,
}

/// Source selector for cross-client queries.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceSelector {
    /// Hostname (None = current, Some("*") = all)
    pub host: Option<String>,
    /// Source type: shell, claude-code, ci, agent
    pub source_type: Option<String>,
    /// Client name: zsh, bash, magic project name
    pub client: Option<String>,
    /// Session identifier: PID, UUID, etc.
    pub session: Option<String>,
}

impl Default for SourceSelector {
    #[inline]
    fn default() -> Self {
        Self {
            host: None,
            source_type: None,
            client: None,
            session: None,
        }
    }
}

/// Path-based working directory filter.
#[derive(Debug, Clone, PartialEq)]
pub enum PathFilter {
    /// Current directory (.)
    Current,
    /// Relative path (./foo, ../bar)
    Relative(PathBuf),
    /// Home-relative path (~/Projects)
    Home(PathBuf),
    /// Absolute path (/tmp/foo)
    Absolute(PathBuf),
}

/// A query component (filter or tag).
#[derive(Debug, Clone, PartialEq)]
pub enum QueryComponent {
    /// Command regex: %/pattern/
    CommandRegex(String),
    /// Field filter: %field<op>value
    FieldFilter(FieldFilter),
    /// Tag reference: %tag-name
    Tag(String),
}

/// Field filter with comparison operator.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldFilter {
    /// Field name (cmd, exit, cwd, duration, host, type, client, session)
    pub field: String,
    /// Comparison operator
    pub op: CompareOp,
    /// Value to compare against
    pub value: String,
}

/// Comparison operators for field filters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    /// `=` equals
    Eq,
    /// `<>` or `!=` not equals (prefer `<>` to avoid shell history expansion)
    NotEq,
    /// `~=` regex match
    Regex,
    /// `>` greater than
    Gt,
    /// `<` less than
    Lt,
    /// `>=` greater or equal
    Gte,
    /// `<=` less or equal
    Lte,
}

/// Range selector for limiting results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RangeSelector {
    /// Start offset from most recent (e.g., 5 = 5th most recent)
    pub start: usize,
    /// End offset (None = just start count, Some = range)
    pub end: Option<usize>,
}

/// Parse a query string into structured components.
pub fn parse_query(input: &str) -> Query {
    let mut query = Query::default();
    let input = input.trim();

    if input.is_empty() {
        return query;
    }

    // Track remaining input as we parse components
    let mut remaining = input;

    // Try to parse source selector first (contains ':' and ends with ':')
    if let Some((source, rest)) = try_parse_source(remaining) {
        query.source = Some(source);
        remaining = rest;
    }

    // Try to parse path filter
    if let Some((path, rest)) = try_parse_path(remaining) {
        query.path = Some(path);
        remaining = rest;
    }

    // Parse remaining components (filters, tags, range)
    while !remaining.is_empty() {
        remaining = remaining.trim_start();

        // Check for range at end
        if let Some((range, rest)) = try_parse_range(remaining) {
            query.range = Some(range);
            remaining = rest;
            continue;
        }

        // Check for filter/tag (starts with %)
        if let Some((component, rest)) = try_parse_filter(remaining) {
            query.filters.push(component);
            remaining = rest;
            continue;
        }

        // Bare word = tag fallback (consume until range or end)
        if let Some((tag, rest)) = try_parse_bare_tag(remaining) {
            query.filters.push(QueryComponent::Tag(tag));
            remaining = rest;
            continue;
        }

        // Unknown content, skip a character
        if !remaining.is_empty() {
            remaining = &remaining[1..];
        }
    }

    query
}

/// Try to parse a source selector (ends with ':').
fn try_parse_source(input: &str) -> Option<(SourceSelector, &str)> {
    // Source selector must contain ':' and the relevant part ends with ':'
    // But we need to find where the source selector ends

    // Count colons to determine format
    let mut colon_count = 0;
    let mut end_pos = 0;

    for (i, c) in input.char_indices() {
        if c == ':' {
            colon_count += 1;
            end_pos = i + 1;
            // Max 4 colons for host:type:client:session:
            if colon_count >= 4 {
                break;
            }
        } else if c == '~' || c == '%' || c == '/' || c == '.' {
            // Start of another component
            break;
        }
    }

    if colon_count == 0 {
        return None;
    }

    let source_str = &input[..end_pos];
    let rest = &input[end_pos..];

    // Parse the source string
    let parts: Vec<&str> = source_str.trim_end_matches(':').split(':').collect();

    // Interpret based on number of parts
    let selector = match parts.len() {
        1 => {
            // Just type (e.g., "shell:")
            SourceSelector {
                source_type: parse_selector_part(parts[0]),
                ..Default::default()
            }
        }
        2 => {
            // type:client (e.g., "shell:zsh:")
            SourceSelector {
                source_type: parse_selector_part(parts[0]),
                client: parse_selector_part(parts[1]),
                ..Default::default()
            }
        }
        3 => {
            // host:type:client (e.g., "laptop:shell:zsh:")
            SourceSelector {
                host: parse_selector_part(parts[0]),
                source_type: parse_selector_part(parts[1]),
                client: parse_selector_part(parts[2]),
                ..Default::default()
            }
        }
        4 => {
            // host:type:client:session (e.g., "laptop:shell:zsh:123:")
            SourceSelector {
                host: parse_selector_part(parts[0]),
                source_type: parse_selector_part(parts[1]),
                client: parse_selector_part(parts[2]),
                session: parse_selector_part(parts[3]),
            }
        }
        _ => return None,
    };

    Some((selector, rest))
}

/// Parse a single selector part (empty = None, * = Some("*"), otherwise literal).
fn parse_selector_part(s: &str) -> Option<String> {
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

/// Try to parse a path filter.
fn try_parse_path(input: &str) -> Option<(PathFilter, &str)> {
    // Check for path patterns

    // Current directory: "." alone or "./" or ".~N" (followed by range)
    if input.starts_with('.') && !input.starts_with("..") {
        let second_char = input.chars().nth(1);
        match second_char {
            None => {
                // Just "."
                return Some((PathFilter::Current, ""));
            }
            Some('/') => {
                // "./" or "./path"
                let end = find_path_end(input);
                let path_str = &input[..end];
                let rest = &input[end..];
                if path_str == "./" {
                    return Some((PathFilter::Current, rest));
                } else {
                    return Some((PathFilter::Relative(PathBuf::from(path_str)), rest));
                }
            }
            Some('~') => {
                // ".~N" - current dir followed by range
                return Some((PathFilter::Current, &input[1..]));
            }
            Some('%') => {
                // ".%filter" - current dir followed by filter
                return Some((PathFilter::Current, &input[1..]));
            }
            _ => {}
        }
    }

    if input.starts_with("../") || input == ".." {
        // Parent relative path
        let end = find_path_end(input);
        let path_str = &input[..end];
        let rest = &input[end..];
        return Some((PathFilter::Relative(PathBuf::from(path_str)), rest));
    }

    if input.starts_with("~/") {
        // Home-relative path (NOT range since it has /)
        let end = find_path_end(input);
        let path_str = &input[2..end]; // Skip ~/
        let rest = &input[end..];
        return Some((PathFilter::Home(PathBuf::from(path_str)), rest));
    }

    if input.starts_with('/') {
        // Absolute path
        let end = find_path_end(input);
        let path_str = &input[..end];
        let rest = &input[end..];
        return Some((PathFilter::Absolute(PathBuf::from(path_str)), rest));
    }

    None
}

/// Find where a path ends (before filter/range markers).
fn find_path_end(input: &str) -> usize {
    for (i, c) in input.char_indices() {
        // Range marker after path
        if c == '~' && i > 0 {
            // Check if next char is digit (range) vs / (part of path)
            if let Some(next) = input[i + 1..].chars().next() {
                if next.is_ascii_digit() {
                    return i;
                }
            }
        }
        // Filter marker
        if c == '%' {
            return i;
        }
    }
    input.len()
}

/// Try to parse a range selector (~N, ~N:~M, or bare N).
fn try_parse_range(input: &str) -> Option<(RangeSelector, &str)> {
    // Try ~N format first
    if input.starts_with('~') {
        // Check that next char is digit (not / for home path)
        let chars: Vec<char> = input.chars().collect();
        if chars.len() < 2 || !chars[1].is_ascii_digit() {
            return None;
        }

        // Parse ~N
        let mut end = 1;
        while end < input.len() && input[end..].chars().next().is_some_and(|c| c.is_ascii_digit()) {
            end += 1;
        }

        let start: usize = input[1..end].parse().ok()?;

        // Check for :M or :~M range
        if input[end..].starts_with(':') {
            let after_colon = &input[end + 1..];
            // Skip optional ~
            let range_rest = after_colon.strip_prefix('~').unwrap_or(after_colon);

            let mut range_end = 0;
            while range_end < range_rest.len()
                && range_rest[range_end..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_digit())
            {
                range_end += 1;
            }

            if range_end > 0 {
                let end_val: usize = range_rest[..range_end].parse().ok()?;
                let rest = &range_rest[range_end..];
                return Some((
                    RangeSelector {
                        start,
                        end: Some(end_val),
                    },
                    rest,
                ));
            }
        }

        return Some((
            RangeSelector { start, end: None },
            &input[end..],
        ));
    }

    // Try bare integer (N = ~N)
    let first = input.chars().next()?;
    if first.is_ascii_digit() {
        let mut end = 0;
        while end < input.len() && input[end..].chars().next().is_some_and(|c| c.is_ascii_digit()) {
            end += 1;
        }

        if end > 0 {
            let start: usize = input[..end].parse().ok()?;
            return Some((
                RangeSelector { start, end: None },
                &input[end..],
            ));
        }
    }

    None
}

/// Try to parse a filter component (%, %/, %field<op>value).
fn try_parse_filter(input: &str) -> Option<(QueryComponent, &str)> {
    if !input.starts_with('%') {
        return None;
    }

    let after_percent = &input[1..];

    // Command regex: %/pattern/
    if let Some(after_slash) = after_percent.strip_prefix('/') {
        // Find closing /
        if let Some(end) = after_slash.find('/') {
            let pattern = &after_slash[..end];
            let rest = &after_slash[end + 1..];
            return Some((QueryComponent::CommandRegex(pattern.to_string()), rest));
        }
    }

    // Filter aliases: %failed, %success, %error
    if let Some(rest) = after_percent.strip_prefix("failed") {
        let filter = FieldFilter {
            field: "exit".to_string(),
            op: CompareOp::NotEq,
            value: "0".to_string(),
        };
        return Some((QueryComponent::FieldFilter(filter), rest));
    }
    if let Some(rest) = after_percent.strip_prefix("success") {
        let filter = FieldFilter {
            field: "exit".to_string(),
            op: CompareOp::Eq,
            value: "0".to_string(),
        };
        return Some((QueryComponent::FieldFilter(filter), rest));
    }
    if let Some(rest) = after_percent.strip_prefix("ok") {
        let filter = FieldFilter {
            field: "exit".to_string(),
            op: CompareOp::Eq,
            value: "0".to_string(),
        };
        return Some((QueryComponent::FieldFilter(filter), rest));
    }

    // Try field filter: %field<op>value
    if let Some((filter, rest)) = try_parse_field_filter(after_percent) {
        return Some((QueryComponent::FieldFilter(filter), rest));
    }

    // Tag: %bare-word (no operator)
    let end = find_filter_end(after_percent);
    if end > 0 {
        let tag = &after_percent[..end];
        let rest = &after_percent[end..];
        return Some((QueryComponent::Tag(tag.to_string()), rest));
    }

    None
}

/// Try to parse a field filter (field<op>value).
fn try_parse_field_filter(input: &str) -> Option<(FieldFilter, &str)> {
    // Known field names
    let fields = ["cmd", "exit", "cwd", "duration", "host", "type", "client", "session"];

    for field in &fields {
        if let Some(after_field) = input.strip_prefix(field) {

            // Try each operator (order matters: check 2-char ops before 1-char)
            let (op, op_len) = if after_field.starts_with("~=") {
                (CompareOp::Regex, 2)
            } else if after_field.starts_with("<>") || after_field.starts_with("!=") {
                // Both <> and != mean not-equal (<> preferred since ! needs shell escaping)
                (CompareOp::NotEq, 2)
            } else if after_field.starts_with(">=") {
                (CompareOp::Gte, 2)
            } else if after_field.starts_with("<=") {
                (CompareOp::Lte, 2)
            } else if after_field.starts_with('=') {
                (CompareOp::Eq, 1)
            } else if after_field.starts_with('>') {
                (CompareOp::Gt, 1)
            } else if after_field.starts_with('<') {
                (CompareOp::Lt, 1)
            } else {
                continue;
            };

            let after_op = &after_field[op_len..];
            let value_end = find_filter_end(after_op);
            let value = &after_op[..value_end];
            let rest = &after_op[value_end..];

            return Some((
                FieldFilter {
                    field: field.to_string(),
                    op,
                    value: value.to_string(),
                },
                rest,
            ));
        }
    }

    None
}

/// Find where a filter value/tag ends.
fn find_filter_end(input: &str) -> usize {
    for (i, c) in input.char_indices() {
        if c == '~' || c == '%' || c.is_whitespace() {
            return i;
        }
    }
    input.len()
}

/// Try to parse a bare tag (word without %).
fn try_parse_bare_tag(input: &str) -> Option<(String, &str)> {
    if input.is_empty() {
        return None;
    }

    // Must start with alphanumeric or -
    let first = input.chars().next()?;
    if !first.is_alphanumeric() && first != '-' && first != '_' {
        return None;
    }

    let end = find_filter_end(input);
    if end > 0 {
        let tag = &input[..end];
        let rest = &input[end..];
        Some((tag.to_string(), rest))
    } else {
        None
    }
}

impl Query {
    /// Check if this query matches everything (no filters applied).
    pub fn is_match_all(&self) -> bool {
        self.source.is_none()
            && self.path.is_none()
            && self.filters.is_empty()
            && self.range.is_none()
    }

    /// Check if source selector is for all sources.
    pub fn is_all_sources(&self) -> bool {
        match &self.source {
            None => false,
            Some(s) => {
                s.host.as_deref() == Some("*")
                    && s.source_type.as_deref() == Some("*")
                    && s.client.as_deref() == Some("*")
                    && s.session.as_deref() == Some("*")
            }
        }
    }
}

impl std::fmt::Display for CompareOp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CompareOp::Eq => write!(f, "="),
            CompareOp::NotEq => write!(f, "<>"), // Canonical form (shell-friendly)
            CompareOp::Regex => write!(f, "~="),
            CompareOp::Gt => write!(f, ">"),
            CompareOp::Lt => write!(f, "<"),
            CompareOp::Gte => write!(f, ">="),
            CompareOp::Lte => write!(f, "<="),
        }
    }
}
