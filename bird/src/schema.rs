//! Schema definitions for BIRD tables.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// An invocation record (a captured command/process execution).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvocationRecord {
    /// Unique identifier (UUIDv7 for time-ordering).
    pub id: Uuid,

    /// Session identifier (groups related invocations).
    pub session_id: String,

    /// When the invocation started.
    pub timestamp: DateTime<Utc>,

    /// How long the invocation took in milliseconds.
    pub duration_ms: Option<i64>,

    /// Working directory when invocation was executed.
    pub cwd: String,

    /// The full command string.
    pub cmd: String,

    /// Extracted executable name (e.g., "make" from "make test").
    pub executable: Option<String>,

    /// Exit code.
    pub exit_code: i32,

    /// Detected output format (e.g., "gcc", "pytest").
    pub format_hint: Option<String>,

    /// Client identifier (user@hostname).
    pub client_id: String,

    /// Hostname where invocation was executed.
    pub hostname: Option<String>,

    /// Username who executed the invocation.
    pub username: Option<String>,
}

/// Environment variable for sharing invocation UUID between nested BIRD clients.
///
/// When set, nested BIRD clients (e.g., `shq run blq run ...`) will use this UUID
/// instead of generating a new one, allowing the invocation to be deduplicated
/// across databases.
pub const BIRD_INVOCATION_UUID_VAR: &str = "BIRD_INVOCATION_UUID";

/// Environment variable for the parent BIRD client name.
///
/// When set, indicates which BIRD client initiated this invocation.
/// Used to avoid duplicate recording in nested scenarios.
pub const BIRD_PARENT_CLIENT_VAR: &str = "BIRD_PARENT_CLIENT";

impl InvocationRecord {
    /// Create a new invocation record.
    ///
    /// If `BIRD_INVOCATION_UUID` is set in the environment, uses that UUID
    /// to enable deduplication across nested BIRD clients.
    pub fn new(
        session_id: impl Into<String>,
        cmd: impl Into<String>,
        cwd: impl Into<String>,
        exit_code: i32,
        client_id: impl Into<String>,
    ) -> Self {
        let cmd = cmd.into();

        // Check for inherited invocation UUID from parent BIRD client
        let id = if let Ok(uuid_str) = std::env::var(BIRD_INVOCATION_UUID_VAR) {
            Uuid::parse_str(&uuid_str).unwrap_or_else(|_| Uuid::now_v7())
        } else {
            Uuid::now_v7()
        };

        Self {
            id,
            session_id: session_id.into(),
            timestamp: Utc::now(),
            duration_ms: None,
            cwd: cwd.into(),
            executable: extract_executable(&cmd),
            cmd,
            exit_code,
            format_hint: None,
            client_id: client_id.into(),
            hostname: gethostname::gethostname().to_str().map(|s| s.to_string()),
            username: std::env::var("USER").ok(),
        }
    }

    /// Create a new invocation record with an explicit UUID.
    ///
    /// Use this when you need to control the UUID (e.g., for testing or
    /// when the UUID is provided externally).
    pub fn with_id(
        id: Uuid,
        session_id: impl Into<String>,
        cmd: impl Into<String>,
        cwd: impl Into<String>,
        exit_code: i32,
        client_id: impl Into<String>,
    ) -> Self {
        let cmd = cmd.into();
        Self {
            id,
            session_id: session_id.into(),
            timestamp: Utc::now(),
            duration_ms: None,
            cwd: cwd.into(),
            executable: extract_executable(&cmd),
            cmd,
            exit_code,
            format_hint: None,
            client_id: client_id.into(),
            hostname: gethostname::gethostname().to_str().map(|s| s.to_string()),
            username: std::env::var("USER").ok(),
        }
    }

    /// Check if this invocation was inherited from a parent BIRD client.
    pub fn is_inherited() -> bool {
        std::env::var(BIRD_INVOCATION_UUID_VAR).is_ok()
    }

    /// Get the parent BIRD client name, if any.
    pub fn parent_client() -> Option<String> {
        std::env::var(BIRD_PARENT_CLIENT_VAR).ok()
    }

    /// Set the duration.
    pub fn with_duration(mut self, duration_ms: i64) -> Self {
        self.duration_ms = Some(duration_ms);
        self
    }

    /// Set the format hint.
    pub fn with_format_hint(mut self, hint: impl Into<String>) -> Self {
        self.format_hint = Some(hint.into());
        self
    }

    /// Get the date portion of the timestamp (for partitioning).
    pub fn date(&self) -> NaiveDate {
        self.timestamp.date_naive()
    }
}

/// A session record (a shell or process that captures invocations).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRecord {
    /// Session identifier (e.g., "zsh-12345").
    pub session_id: String,

    /// Client identifier (user@hostname).
    pub client_id: String,

    /// Invoker name (e.g., "zsh", "bash", "shq", "python").
    pub invoker: String,

    /// Invoker PID.
    pub invoker_pid: u32,

    /// Invoker type: "shell", "cli", "hook", "script".
    pub invoker_type: String,

    /// When the session was first seen.
    pub registered_at: DateTime<Utc>,

    /// Initial working directory.
    pub cwd: Option<String>,

    /// Date for partitioning.
    pub date: NaiveDate,
}

impl SessionRecord {
    /// Create a new session record.
    pub fn new(
        session_id: impl Into<String>,
        client_id: impl Into<String>,
        invoker: impl Into<String>,
        invoker_pid: u32,
        invoker_type: impl Into<String>,
    ) -> Self {
        let now = Utc::now();
        Self {
            session_id: session_id.into(),
            client_id: client_id.into(),
            invoker: invoker.into(),
            invoker_pid,
            invoker_type: invoker_type.into(),
            registered_at: now,
            cwd: std::env::current_dir()
                .ok()
                .map(|p| p.display().to_string()),
            date: now.date_naive(),
        }
    }
}

/// Extract the executable name from a command string.
fn extract_executable(cmd: &str) -> Option<String> {
    let cmd = cmd.trim();

    // Skip environment variable assignments at the start
    let mut parts = cmd.split_whitespace();
    for part in parts.by_ref() {
        if !part.contains('=') {
            // This is the actual command
            // Extract basename if it's a path
            let exe = part.split('/').last().unwrap_or(part);
            return Some(exe.to_string());
        }
    }

    None
}

/// An output record (stdout/stderr from an invocation).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputRecord {
    /// Unique identifier.
    pub id: Uuid,

    /// Invocation this output belongs to.
    pub invocation_id: Uuid,

    /// Stream type: "stdout", "stderr", or "combined".
    pub stream: String,

    /// BLAKE3 hash of the content.
    pub content_hash: String,

    /// Size in bytes.
    pub byte_length: usize,

    /// Storage type: "inline" or "blob".
    pub storage_type: String,

    /// Storage reference (data: URI for inline, file:// for blob).
    pub storage_ref: String,

    /// Content type hint (e.g., "text/plain", "application/json").
    pub content_type: Option<String>,

    /// Date for partitioning.
    pub date: NaiveDate,
}

impl OutputRecord {
    /// Create a new inline output record.
    ///
    /// For small outputs, content is stored as a base64 data URI.
    pub fn new_inline(
        invocation_id: Uuid,
        stream: impl Into<String>,
        content: &[u8],
        date: NaiveDate,
    ) -> Self {
        use base64::Engine;

        let content_hash = blake3::hash(content).to_hex().to_string();
        let byte_length = content.len();

        // Encode as data URI
        let b64 = base64::engine::general_purpose::STANDARD.encode(content);
        let storage_ref = format!("data:application/octet-stream;base64,{}", b64);

        Self {
            id: Uuid::now_v7(),
            invocation_id,
            stream: stream.into(),
            content_hash,
            byte_length,
            storage_type: "inline".to_string(),
            storage_ref,
            content_type: Some("text/plain".to_string()),
            date,
        }
    }

    /// Decode the content from storage_ref.
    pub fn decode_content(&self) -> Option<Vec<u8>> {
        use base64::Engine;

        if self.storage_type == "inline" {
            // Parse data: URI
            if let Some(b64_part) = self.storage_ref.split(",").nth(1) {
                base64::engine::general_purpose::STANDARD.decode(b64_part).ok()
            } else {
                None
            }
        } else {
            // TODO: Handle blob storage
            None
        }
    }
}

/// An event record (a parsed log entry from an invocation output).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventRecord {
    /// Unique identifier (UUIDv7 for time-ordering).
    pub id: Uuid,

    /// Invocation this event was parsed from.
    pub invocation_id: Uuid,

    /// Client identifier (for cross-client queries).
    pub client_id: String,

    /// Hostname where the invocation ran.
    pub hostname: Option<String>,

    /// Event type from duck_hunt (e.g., "diagnostic", "test_result").
    pub event_type: Option<String>,

    /// Severity level: error, warning, info, note.
    pub severity: Option<String>,

    /// Source file referenced by this event.
    pub ref_file: Option<String>,

    /// Line number in the source file.
    pub ref_line: Option<i32>,

    /// Column number in the source file.
    pub ref_column: Option<i32>,

    /// The event message.
    pub message: Option<String>,

    /// Error/warning code (e.g., "E0308", "W0401").
    pub error_code: Option<String>,

    /// Test name (for test results).
    pub test_name: Option<String>,

    /// Test status: passed, failed, skipped.
    pub status: Option<String>,

    /// Format used for parsing.
    pub format_used: String,

    /// Date for partitioning.
    pub date: NaiveDate,
}

impl EventRecord {
    /// Create a new event record with a fresh UUIDv7.
    pub fn new(
        invocation_id: Uuid,
        client_id: impl Into<String>,
        format_used: impl Into<String>,
        date: NaiveDate,
    ) -> Self {
        Self {
            id: Uuid::now_v7(),
            invocation_id,
            client_id: client_id.into(),
            hostname: gethostname::gethostname().to_str().map(|s| s.to_string()),
            event_type: None,
            severity: None,
            ref_file: None,
            ref_line: None,
            ref_column: None,
            message: None,
            error_code: None,
            test_name: None,
            status: None,
            format_used: format_used.into(),
            date,
        }
    }
}

/// SQL to create the events table schema (for documentation/reference).
pub const EVENTS_SCHEMA: &str = r#"
CREATE TABLE events (
    id                UUID PRIMARY KEY,
    invocation_id     UUID NOT NULL,
    client_id         VARCHAR NOT NULL,
    hostname          VARCHAR,
    event_type        VARCHAR,
    severity          VARCHAR,
    ref_file          VARCHAR,
    ref_line          INTEGER,
    ref_column        INTEGER,
    message           VARCHAR,
    error_code        VARCHAR,
    test_name         VARCHAR,
    status            VARCHAR,
    format_used       VARCHAR NOT NULL,
    date              DATE NOT NULL
);
"#;

/// SQL to create the invocations table schema (for documentation/reference).
pub const INVOCATIONS_SCHEMA: &str = r#"
CREATE TABLE invocations (
    id                UUID PRIMARY KEY,
    session_id        VARCHAR NOT NULL,
    timestamp         TIMESTAMP NOT NULL,
    duration_ms       BIGINT,
    cwd               VARCHAR NOT NULL,
    cmd               VARCHAR NOT NULL,
    executable        VARCHAR,
    exit_code         INTEGER NOT NULL,
    format_hint       VARCHAR,
    client_id         VARCHAR NOT NULL,
    hostname          VARCHAR,
    username          VARCHAR,
    date              DATE NOT NULL
);
"#;

/// SQL to create the sessions table schema (for documentation/reference).
pub const SESSIONS_SCHEMA: &str = r#"
CREATE TABLE sessions (
    session_id        VARCHAR PRIMARY KEY,
    client_id         VARCHAR NOT NULL,
    invoker           VARCHAR NOT NULL,
    invoker_pid       INTEGER NOT NULL,
    invoker_type      VARCHAR NOT NULL,
    registered_at     TIMESTAMP NOT NULL,
    cwd               VARCHAR,
    date              DATE NOT NULL
);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_executable() {
        assert_eq!(extract_executable("make test"), Some("make".to_string()));
        assert_eq!(extract_executable("/usr/bin/gcc -o foo foo.c"), Some("gcc".to_string()));
        assert_eq!(extract_executable("ENV=val make"), Some("make".to_string()));
        assert_eq!(extract_executable("CC=gcc CXX=g++ make"), Some("make".to_string()));
        assert_eq!(extract_executable(""), None);
    }

    #[test]
    fn test_invocation_record_new() {
        let record = InvocationRecord::new(
            "session-123",
            "make test",
            "/home/user/project",
            0,
            "user@laptop",
        );

        assert_eq!(record.session_id, "session-123");
        assert_eq!(record.cmd, "make test");
        assert_eq!(record.executable, Some("make".to_string()));
        assert_eq!(record.exit_code, 0);
        assert!(record.duration_ms.is_none());
    }

    #[test]
    fn test_invocation_record_with_duration() {
        let record = InvocationRecord::new(
            "session-123",
            "make test",
            "/home/user/project",
            0,
            "user@laptop",
        )
        .with_duration(1500);

        assert_eq!(record.duration_ms, Some(1500));
    }

    #[test]
    fn test_session_record_new() {
        let record = SessionRecord::new(
            "zsh-12345",
            "user@laptop",
            "zsh",
            12345,
            "shell",
        );

        assert_eq!(record.session_id, "zsh-12345");
        assert_eq!(record.client_id, "user@laptop");
        assert_eq!(record.invoker, "zsh");
        assert_eq!(record.invoker_pid, 12345);
        assert_eq!(record.invoker_type, "shell");
    }
}
