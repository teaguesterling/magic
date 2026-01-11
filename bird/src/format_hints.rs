//! Format hints configuration for command-to-format detection.
//!
//! Supports multiple configuration styles:
//!
//! ```toml
//! [format-hints]
//! # Simple form - default priority (500)
//! "*lint*" = "eslint"
//! "cargo*" = "cargo"
//!
//! # Structured form - explicit priority inline
//! "*pytest*" = { format = "pytest", priority = 100 }
//!
//! # Priority sections - all entries inherit the section's priority
//! [format-hints.1000]
//! "mycompany-*" = "gcc"
//!
//! [format-hints.100]
//! "legacy-*" = "text"
//! ```

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Default priority for simple pattern = "format" entries.
pub const DEFAULT_PRIORITY: i32 = 500;

/// A single format hint rule.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FormatHint {
    /// Glob pattern to match against command/executable.
    pub pattern: String,
    /// Format name (e.g., "gcc", "pytest", "cargo_build").
    pub format: String,
    /// Priority (higher wins). Default is 500.
    #[serde(default = "default_priority")]
    pub priority: i32,
}

fn default_priority() -> i32 {
    DEFAULT_PRIORITY
}

impl FormatHint {
    /// Create a new format hint with default priority.
    pub fn new(pattern: impl Into<String>, format: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            format: format.into(),
            priority: DEFAULT_PRIORITY,
        }
    }

    /// Create a new format hint with explicit priority.
    pub fn with_priority(pattern: impl Into<String>, format: impl Into<String>, priority: i32) -> Self {
        Self {
            pattern: pattern.into(),
            format: format.into(),
            priority,
        }
    }
}

/// Format hints configuration.
#[derive(Debug, Clone, Default)]
pub struct FormatHints {
    /// All hints sorted by priority (highest first).
    hints: Vec<FormatHint>,
    /// Default format when no hints match.
    default_format: String,
}

impl FormatHints {
    /// Create an empty FormatHints with default "auto" fallback.
    pub fn new() -> Self {
        Self {
            hints: Vec::new(),
            default_format: "auto".to_string(),
        }
    }

    /// Load format hints from a TOML file.
    pub fn load(path: &Path) -> Result<Self> {
        if !path.exists() {
            return Ok(Self::new());
        }

        let contents = fs::read_to_string(path)?;
        Self::parse(&contents)
    }

    /// Parse format hints from TOML string.
    pub fn parse(toml_str: &str) -> Result<Self> {
        let value: toml::Value = toml::from_str(toml_str)
            .map_err(|e| Error::Config(format!("Failed to parse format-hints: {}", e)))?;

        let mut hints = Vec::new();
        let mut default_format = "auto".to_string();

        // Get the format-hints table
        if let Some(format_hints) = value.get("format-hints").and_then(|v| v.as_table()) {
            for (key, val) in format_hints {
                // Check if it's a priority subsection (numeric key)
                if let Ok(priority) = key.parse::<i32>() {
                    // Priority section: [format-hints.1000]
                    if let Some(section) = val.as_table() {
                        for (pattern, format_val) in section {
                            let hint = parse_hint_value(pattern, format_val, Some(priority))?;
                            hints.push(hint);
                        }
                    }
                } else if key == "default" {
                    // Default format setting
                    if let Some(s) = val.as_str() {
                        default_format = s.to_string();
                    }
                } else {
                    // Regular entry in [format-hints] section
                    let hint = parse_hint_value(key, val, None)?;
                    hints.push(hint);
                }
            }
        }

        // Also check for legacy [[rules]] format for backwards compatibility
        if let Some(rules) = value.get("rules").and_then(|v| v.as_array()) {
            for rule in rules {
                if let (Some(pattern), Some(format)) = (
                    rule.get("pattern").and_then(|v| v.as_str()),
                    rule.get("format").and_then(|v| v.as_str()),
                ) {
                    let priority = rule
                        .get("priority")
                        .and_then(|v| v.as_integer())
                        .map(|p| p as i32)
                        .unwrap_or(DEFAULT_PRIORITY);
                    hints.push(FormatHint::with_priority(pattern, format, priority));
                }
            }
        }

        // Check for legacy [default] section
        if let Some(default) = value.get("default").and_then(|v| v.as_table()) {
            if let Some(format) = default.get("format").and_then(|v| v.as_str()) {
                default_format = format.to_string();
            }
        }

        // Sort by priority (highest first), then by pattern for stable ordering
        hints.sort_by(|a, b| {
            b.priority.cmp(&a.priority).then_with(|| a.pattern.cmp(&b.pattern))
        });

        Ok(Self { hints, default_format })
    }

    /// Save format hints to a TOML file.
    pub fn save(&self, path: &Path) -> Result<()> {
        let contents = self.to_toml();
        fs::write(path, contents)?;
        Ok(())
    }

    /// Convert to TOML string.
    pub fn to_toml(&self) -> String {
        let mut output = String::new();
        output.push_str("# Format hints for command-to-format detection\n");
        output.push_str("# Higher priority values take precedence\n\n");

        // Group by priority
        let mut by_priority: HashMap<i32, Vec<&FormatHint>> = HashMap::new();
        for hint in &self.hints {
            by_priority.entry(hint.priority).or_default().push(hint);
        }

        // Get sorted priorities (highest first)
        let mut priorities: Vec<_> = by_priority.keys().copied().collect();
        priorities.sort_by(|a, b| b.cmp(a));

        // Output default priority (500) entries in [format-hints] section
        if let Some(default_hints) = by_priority.get(&DEFAULT_PRIORITY) {
            output.push_str("[format-hints]\n");
            for hint in default_hints {
                output.push_str(&format!("\"{}\" = \"{}\"\n", hint.pattern, hint.format));
            }
            if self.default_format != "auto" {
                output.push_str(&format!("default = \"{}\"\n", self.default_format));
            }
            output.push('\n');
        } else if self.default_format != "auto" {
            output.push_str("[format-hints]\n");
            output.push_str(&format!("default = \"{}\"\n", self.default_format));
            output.push('\n');
        }

        // Output other priority sections
        for priority in priorities {
            if priority == DEFAULT_PRIORITY {
                continue;
            }
            if let Some(hints) = by_priority.get(&priority) {
                output.push_str(&format!("[format-hints.{}]\n", priority));
                for hint in hints {
                    output.push_str(&format!("\"{}\" = \"{}\"\n", hint.pattern, hint.format));
                }
                output.push('\n');
            }
        }

        output
    }

    /// Get all hints (sorted by priority, highest first).
    pub fn hints(&self) -> &[FormatHint] {
        &self.hints
    }

    /// Get the default format.
    pub fn default_format(&self) -> &str {
        &self.default_format
    }

    /// Set the default format.
    pub fn set_default_format(&mut self, format: impl Into<String>) {
        self.default_format = format.into();
    }

    /// Add a hint (maintains sorted order).
    pub fn add(&mut self, hint: FormatHint) {
        // Remove any existing hint with the same pattern
        self.hints.retain(|h| h.pattern != hint.pattern);
        self.hints.push(hint);
        self.hints.sort_by(|a, b| {
            b.priority.cmp(&a.priority).then_with(|| a.pattern.cmp(&b.pattern))
        });
    }

    /// Remove a hint by pattern. Returns true if removed.
    pub fn remove(&mut self, pattern: &str) -> bool {
        let len_before = self.hints.len();
        self.hints.retain(|h| h.pattern != pattern);
        self.hints.len() < len_before
    }

    /// Find a hint by pattern.
    pub fn get(&self, pattern: &str) -> Option<&FormatHint> {
        self.hints.iter().find(|h| h.pattern == pattern)
    }

    /// Detect format for a command string.
    /// Returns the format from the highest-priority matching hint, or default.
    pub fn detect(&self, cmd: &str) -> &str {
        for hint in &self.hints {
            if pattern_matches(&hint.pattern, cmd) {
                return &hint.format;
            }
        }
        &self.default_format
    }
}

/// Parse a hint value (string or structured).
fn parse_hint_value(pattern: &str, val: &toml::Value, section_priority: Option<i32>) -> Result<FormatHint> {
    match val {
        toml::Value::String(format) => {
            let priority = section_priority.unwrap_or(DEFAULT_PRIORITY);
            Ok(FormatHint::with_priority(pattern, format, priority))
        }
        toml::Value::Table(table) => {
            let format = table
                .get("format")
                .and_then(|v| v.as_str())
                .ok_or_else(|| Error::Config(format!("Missing 'format' field for pattern '{}'", pattern)))?;
            let priority = table
                .get("priority")
                .and_then(|v| v.as_integer())
                .map(|p| p as i32)
                .or(section_priority)
                .unwrap_or(DEFAULT_PRIORITY);
            Ok(FormatHint::with_priority(pattern, format, priority))
        }
        _ => Err(Error::Config(format!(
            "Invalid value for pattern '{}': expected string or table",
            pattern
        ))),
    }
}

/// Simple glob pattern matching.
/// `*` matches any characters (including none).
pub fn pattern_matches(pattern: &str, text: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();

    if parts.len() == 1 {
        return pattern == text;
    }

    // First part must match at start (if not empty)
    if !parts[0].is_empty() && !text.starts_with(parts[0]) {
        return false;
    }
    let mut pos = parts[0].len();

    // Middle parts must appear in order
    for part in &parts[1..parts.len() - 1] {
        if part.is_empty() {
            continue;
        }
        match text[pos..].find(part) {
            Some(found) => pos += found + part.len(),
            None => return false,
        }
    }

    // Last part must match at end (if not empty)
    let last = parts[parts.len() - 1];
    if !last.is_empty() && !text[pos..].ends_with(last) {
        return false;
    }

    true
}

/// Convert a glob pattern to SQL LIKE pattern.
pub fn glob_to_like(pattern: &str) -> String {
    let mut result = String::with_capacity(pattern.len() + 10);
    for c in pattern.chars() {
        match c {
            '*' => result.push('%'),
            '?' => result.push('_'),
            '%' => result.push_str("\\%"),
            '_' => result.push_str("\\_"),
            '\\' => result.push_str("\\\\"),
            _ => result.push(c),
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_format() {
        let toml = r#"
[format-hints]
"*lint*" = "eslint"
"cargo*" = "cargo"
"#;
        let hints = FormatHints::parse(toml).unwrap();
        assert_eq!(hints.hints().len(), 2);

        let eslint = hints.get("*lint*").unwrap();
        assert_eq!(eslint.format, "eslint");
        assert_eq!(eslint.priority, DEFAULT_PRIORITY);
    }

    #[test]
    fn test_parse_structured_format() {
        let toml = r#"
[format-hints]
"*pytest*" = { format = "pytest", priority = 100 }
"#;
        let hints = FormatHints::parse(toml).unwrap();
        let pytest = hints.get("*pytest*").unwrap();
        assert_eq!(pytest.format, "pytest");
        assert_eq!(pytest.priority, 100);
    }

    #[test]
    fn test_parse_priority_sections() {
        let toml = r#"
[format-hints]
"*lint*" = "eslint"

[format-hints.1000]
"mycompany-*" = "gcc"

[format-hints.100]
"legacy-*" = "text"
"#;
        let hints = FormatHints::parse(toml).unwrap();
        assert_eq!(hints.hints().len(), 3);

        // Check priorities
        let mycompany = hints.get("mycompany-*").unwrap();
        assert_eq!(mycompany.priority, 1000);

        let legacy = hints.get("legacy-*").unwrap();
        assert_eq!(legacy.priority, 100);

        let lint = hints.get("*lint*").unwrap();
        assert_eq!(lint.priority, DEFAULT_PRIORITY);

        // Should be sorted by priority (highest first)
        assert_eq!(hints.hints()[0].pattern, "mycompany-*");
        assert_eq!(hints.hints()[1].pattern, "*lint*");
        assert_eq!(hints.hints()[2].pattern, "legacy-*");
    }

    #[test]
    fn test_parse_legacy_rules() {
        let toml = r#"
[[rules]]
pattern = "*gcc*"
format = "gcc"

[[rules]]
pattern = "*make*"
format = "make"
priority = 100

[default]
format = "text"
"#;
        let hints = FormatHints::parse(toml).unwrap();
        assert_eq!(hints.hints().len(), 2);
        assert_eq!(hints.default_format(), "text");

        let gcc = hints.get("*gcc*").unwrap();
        assert_eq!(gcc.format, "gcc");
        assert_eq!(gcc.priority, DEFAULT_PRIORITY);

        let make = hints.get("*make*").unwrap();
        assert_eq!(make.format, "make");
        assert_eq!(make.priority, 100);
    }

    #[test]
    fn test_detect() {
        let toml = r#"
[format-hints]
"*lint*" = "eslint"

[format-hints.1000]
"mycompany-*" = "gcc"
"#;
        let hints = FormatHints::parse(toml).unwrap();

        // High priority match
        assert_eq!(hints.detect("mycompany-build"), "gcc");

        // Default priority match
        assert_eq!(hints.detect("eslint check"), "eslint");
        assert_eq!(hints.detect("npm run lint"), "eslint");

        // No match -> default
        assert_eq!(hints.detect("cargo test"), "auto");
    }

    #[test]
    fn test_priority_ordering() {
        let toml = r#"
[format-hints]
"*build*" = "generic"

[format-hints.1000]
"mycompany-build*" = "gcc"
"#;
        let hints = FormatHints::parse(toml).unwrap();

        // High priority should win even though both match
        assert_eq!(hints.detect("mycompany-build main.c"), "gcc");

        // Only generic matches
        assert_eq!(hints.detect("npm run build"), "generic");
    }

    #[test]
    fn test_add_remove() {
        let mut hints = FormatHints::new();

        hints.add(FormatHint::new("*test*", "pytest"));
        assert_eq!(hints.hints().len(), 1);

        hints.add(FormatHint::with_priority("*build*", "gcc", 1000));
        assert_eq!(hints.hints().len(), 2);

        // High priority should be first
        assert_eq!(hints.hints()[0].pattern, "*build*");

        // Remove
        assert!(hints.remove("*test*"));
        assert_eq!(hints.hints().len(), 1);
        assert!(!hints.remove("*nonexistent*"));
    }

    #[test]
    fn test_to_toml() {
        let mut hints = FormatHints::new();
        hints.add(FormatHint::new("*lint*", "eslint"));
        hints.add(FormatHint::with_priority("mycompany-*", "gcc", 1000));
        hints.add(FormatHint::with_priority("legacy-*", "text", 100));

        let toml = hints.to_toml();

        // Parse it back
        let parsed = FormatHints::parse(&toml).unwrap();
        assert_eq!(parsed.hints().len(), 3);

        // Check they're equivalent
        assert_eq!(parsed.get("*lint*").unwrap().format, "eslint");
        assert_eq!(parsed.get("mycompany-*").unwrap().priority, 1000);
        assert_eq!(parsed.get("legacy-*").unwrap().priority, 100);
    }

    #[test]
    fn test_pattern_matches() {
        assert!(pattern_matches("*gcc*", "gcc -o foo foo.c"));
        assert!(pattern_matches("*gcc*", "/usr/bin/gcc main.c"));
        assert!(pattern_matches("cargo *", "cargo build"));
        assert!(pattern_matches("cargo *", "cargo test --release"));
        assert!(!pattern_matches("cargo *", "rustc main.rs"));
        assert!(pattern_matches("*", "anything"));
        assert!(pattern_matches("exact", "exact"));
        assert!(!pattern_matches("exact", "not exact"));
    }
}
