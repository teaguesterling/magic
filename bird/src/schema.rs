//! Schema definitions for BIRD tables.
//!
//! # BIRD v5 Schema
//!
//! The v5 schema splits invocations into attempts + outcomes:
//!
//! - **attempts**: Record of invocation start (cmd, cwd, timestamp, etc.)
//! - **outcomes**: Record of invocation completion (exit_code, duration, etc.)
//! - **invocations**: VIEW joining attempts LEFT JOIN outcomes with derived status
//!
//! Status is derived from the join:
//! - `pending`: attempt exists but no outcome
//! - `completed`: outcome exists with exit_code
//! - `orphaned`: outcome exists but exit_code is NULL (signal/crash)

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
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

    /// Runner identifier for liveness checking of pending invocations.
    /// Format depends on execution context:
    /// - Local process: "pid:12345"
    /// - GitHub Actions: "gha:run:12345678"
    /// - Kubernetes: "k8s:pod:abc123"
    pub runner_id: Option<String>,

    /// Exit code (None while pending).
    pub exit_code: Option<i32>,

    /// Invocation status: "pending", "completed", "orphaned".
    pub status: String,

    /// Detected output format (e.g., "gcc", "pytest").
    pub format_hint: Option<String>,

    /// Client identifier (user@hostname).
    pub client_id: String,

    /// Hostname where invocation was executed.
    pub hostname: Option<String>,

    /// Username who executed the invocation.
    pub username: Option<String>,

    /// User-defined tag (unique alias for this invocation, like git tags).
    pub tag: Option<String>,

    /// Extensible metadata (VCS info, CI context, etc.).
    /// Stored on the AttemptRecord when converted to v5 schema.
    #[serde(default)]
    pub metadata: HashMap<String, serde_json::Value>,
}

// =============================================================================
// BIRD v5 Schema Types: Attempts and Outcomes
// =============================================================================

/// An attempt record (the start of a command execution).
///
/// This represents the "attempt" to run a command - recorded at invocation start.
/// The outcome (completion) is recorded separately in `OutcomeRecord`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptRecord {
    /// Unique identifier (UUIDv7 for time-ordering).
    pub id: Uuid,

    /// When the attempt started.
    pub timestamp: DateTime<Utc>,

    /// The full command string.
    pub cmd: String,

    /// Working directory when invocation was executed.
    pub cwd: String,

    /// Session identifier (groups related invocations).
    pub session_id: String,

    /// User-defined tag (unique alias for this invocation, like git tags).
    pub tag: Option<String>,

    /// Client identifier (user@hostname or application name).
    pub source_client: String,

    /// Machine identifier (for multi-machine setups).
    pub machine_id: Option<String>,

    /// Hostname where invocation was executed.
    pub hostname: Option<String>,

    /// Extracted executable name (e.g., "make" from "make test").
    pub executable: Option<String>,

    /// Detected output format (e.g., "gcc", "pytest").
    pub format_hint: Option<String>,

    /// Extensible metadata (user-defined key-value pairs).
    /// Stored as MAP(VARCHAR, JSON) in DuckDB.
    pub metadata: HashMap<String, serde_json::Value>,

    /// Date for partitioning.
    pub date: NaiveDate,
}

impl AttemptRecord {
    /// Create a new attempt record.
    ///
    /// If `BIRD_INVOCATION_UUID` is set in the environment, uses that UUID
    /// to enable deduplication across nested BIRD clients.
    pub fn new(
        session_id: impl Into<String>,
        cmd: impl Into<String>,
        cwd: impl Into<String>,
        source_client: impl Into<String>,
    ) -> Self {
        let cmd = cmd.into();
        let now = Utc::now();

        // Check for inherited invocation UUID from parent BIRD client
        let id = if let Ok(uuid_str) = std::env::var(BIRD_INVOCATION_UUID_VAR) {
            Uuid::parse_str(&uuid_str).unwrap_or_else(|_| Uuid::now_v7())
        } else {
            Uuid::now_v7()
        };

        Self {
            id,
            timestamp: now,
            executable: extract_executable(&cmd),
            cmd,
            cwd: cwd.into(),
            session_id: session_id.into(),
            tag: None,
            source_client: source_client.into(),
            machine_id: None,
            hostname: gethostname::gethostname().to_str().map(|s| s.to_string()),
            format_hint: None,
            metadata: HashMap::new(),
            date: now.date_naive(),
        }
    }

    /// Create an attempt record with an explicit UUID.
    pub fn with_id(
        id: Uuid,
        session_id: impl Into<String>,
        cmd: impl Into<String>,
        cwd: impl Into<String>,
        source_client: impl Into<String>,
    ) -> Self {
        let cmd = cmd.into();
        let now = Utc::now();

        Self {
            id,
            timestamp: now,
            executable: extract_executable(&cmd),
            cmd,
            cwd: cwd.into(),
            session_id: session_id.into(),
            tag: None,
            source_client: source_client.into(),
            machine_id: None,
            hostname: gethostname::gethostname().to_str().map(|s| s.to_string()),
            format_hint: None,
            metadata: HashMap::new(),
            date: now.date_naive(),
        }
    }

    /// Set the tag (unique alias for this invocation).
    pub fn with_tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = Some(tag.into());
        self
    }

    /// Set the machine ID.
    pub fn with_machine_id(mut self, machine_id: impl Into<String>) -> Self {
        self.machine_id = Some(machine_id.into());
        self
    }

    /// Set the format hint.
    pub fn with_format_hint(mut self, hint: impl Into<String>) -> Self {
        self.format_hint = Some(hint.into());
        self
    }

    /// Add metadata entry.
    pub fn with_metadata(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.metadata.insert(key.into(), value);
        self
    }

    /// Get the date portion of the timestamp (for partitioning).
    pub fn date(&self) -> NaiveDate {
        self.date
    }
}

/// An outcome record (the completion of a command execution).
///
/// This represents the "outcome" of a command - recorded at invocation end.
/// Links back to an `AttemptRecord` via `attempt_id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutcomeRecord {
    /// The attempt this outcome is for.
    pub attempt_id: Uuid,

    /// When the invocation completed.
    pub completed_at: DateTime<Utc>,

    /// Exit code (None if killed by signal or crashed).
    pub exit_code: Option<i32>,

    /// How long the invocation took in milliseconds.
    pub duration_ms: Option<i64>,

    /// Signal that terminated the process (if killed by signal).
    pub signal: Option<i32>,

    /// Whether the process was terminated due to timeout.
    pub timeout: bool,

    /// Extensible metadata (user-defined key-value pairs).
    /// Stored as MAP(VARCHAR, JSON) in DuckDB.
    pub metadata: HashMap<String, serde_json::Value>,

    /// Date for partitioning (matches the attempt date).
    pub date: NaiveDate,
}

impl OutcomeRecord {
    /// Create a new completed outcome record.
    pub fn completed(attempt_id: Uuid, exit_code: i32, duration_ms: Option<i64>, date: NaiveDate) -> Self {
        Self {
            attempt_id,
            completed_at: Utc::now(),
            exit_code: Some(exit_code),
            duration_ms,
            signal: None,
            timeout: false,
            metadata: HashMap::new(),
            date,
        }
    }

    /// Create an outcome for a process killed by signal.
    pub fn killed(attempt_id: Uuid, signal: i32, duration_ms: Option<i64>, date: NaiveDate) -> Self {
        Self {
            attempt_id,
            completed_at: Utc::now(),
            exit_code: None,
            duration_ms,
            signal: Some(signal),
            timeout: false,
            metadata: HashMap::new(),
            date,
        }
    }

    /// Create an outcome for a timed-out process.
    pub fn timed_out(attempt_id: Uuid, duration_ms: i64, date: NaiveDate) -> Self {
        Self {
            attempt_id,
            completed_at: Utc::now(),
            exit_code: None,
            duration_ms: Some(duration_ms),
            signal: None,
            timeout: true,
            metadata: HashMap::new(),
            date,
        }
    }

    /// Create an orphaned outcome (process crashed or was killed without cleanup).
    pub fn orphaned(attempt_id: Uuid, date: NaiveDate) -> Self {
        Self {
            attempt_id,
            completed_at: Utc::now(),
            exit_code: None,
            duration_ms: None,
            signal: None,
            timeout: false,
            metadata: HashMap::new(),
            date,
        }
    }

    /// Add metadata entry.
    pub fn with_metadata(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.metadata.insert(key.into(), value);
        self
    }
}

// =============================================================================
// Legacy v4 Schema Types (for backwards compatibility)
// =============================================================================

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
            runner_id: None,
            exit_code: Some(exit_code),
            status: "completed".to_string(),
            format_hint: None,
            client_id: client_id.into(),
            hostname: gethostname::gethostname().to_str().map(|s| s.to_string()),
            username: std::env::var("USER").ok(),
            tag: None,
            metadata: HashMap::new(),
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
            runner_id: None,
            exit_code: Some(exit_code),
            status: "completed".to_string(),
            format_hint: None,
            client_id: client_id.into(),
            hostname: gethostname::gethostname().to_str().map(|s| s.to_string()),
            username: std::env::var("USER").ok(),
            tag: None,
            metadata: HashMap::new(),
        }
    }

    /// Create a new pending invocation record.
    ///
    /// Use this when a command starts but hasn't completed yet.
    /// The exit_code is None and status is "pending".
    ///
    /// `runner_id` identifies the execution context for liveness checking:
    /// - Local process: "pid:12345"
    /// - GitHub Actions: "gha:run:12345678"
    /// - Kubernetes: "k8s:pod:abc123"
    pub fn new_pending(
        session_id: impl Into<String>,
        cmd: impl Into<String>,
        cwd: impl Into<String>,
        runner_id: impl Into<String>,
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
            runner_id: Some(runner_id.into()),
            exit_code: None,
            status: "pending".to_string(),
            format_hint: None,
            client_id: client_id.into(),
            hostname: gethostname::gethostname().to_str().map(|s| s.to_string()),
            username: std::env::var("USER").ok(),
            tag: None,
            metadata: HashMap::new(),
        }
    }

    /// Create a pending invocation for a local process.
    ///
    /// Convenience method that formats the PID as "pid:{pid}".
    pub fn new_pending_local(
        session_id: impl Into<String>,
        cmd: impl Into<String>,
        cwd: impl Into<String>,
        pid: i32,
        client_id: impl Into<String>,
    ) -> Self {
        Self::new_pending(session_id, cmd, cwd, format!("pid:{}", pid), client_id)
    }

    /// Mark this invocation as completed with the given exit code.
    pub fn complete(mut self, exit_code: i32, duration_ms: Option<i64>) -> Self {
        self.exit_code = Some(exit_code);
        self.duration_ms = duration_ms;
        self.status = "completed".to_string();
        self
    }

    /// Mark this invocation as orphaned (process died without cleanup).
    pub fn mark_orphaned(mut self) -> Self {
        self.status = "orphaned".to_string();
        self
    }

    /// Set the runner ID.
    pub fn with_runner_id(mut self, runner_id: impl Into<String>) -> Self {
        self.runner_id = Some(runner_id.into());
        self
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

    /// Set the tag (unique alias for this invocation).
    pub fn with_tag(mut self, tag: impl Into<String>) -> Self {
        self.tag = Some(tag.into());
        self
    }

    /// Add a single metadata entry.
    pub fn with_metadata_entry(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.metadata.insert(key.into(), value);
        self
    }

    /// Set all metadata from a HashMap.
    pub fn with_metadata(mut self, metadata: HashMap<String, serde_json::Value>) -> Self {
        self.metadata = metadata;
        self
    }

    /// Merge metadata from a HashMap (existing entries are preserved).
    pub fn merge_metadata(mut self, metadata: HashMap<String, serde_json::Value>) -> Self {
        for (key, value) in metadata {
            self.metadata.entry(key).or_insert(value);
        }
        self
    }

    /// Get the date portion of the timestamp (for partitioning).
    pub fn date(&self) -> NaiveDate {
        self.timestamp.date_naive()
    }

    // =========================================================================
    // V5 Schema Conversion Methods
    // =========================================================================

    /// Convert this invocation record to an AttemptRecord (v5 schema).
    pub fn to_attempt(&self) -> AttemptRecord {
        AttemptRecord {
            id: self.id,
            timestamp: self.timestamp,
            cmd: self.cmd.clone(),
            cwd: self.cwd.clone(),
            session_id: self.session_id.clone(),
            tag: self.tag.clone(),
            source_client: self.client_id.clone(),
            // machine_id stores runner_id for liveness checking
            machine_id: self.runner_id.clone(),
            hostname: self.hostname.clone(),
            executable: self.executable.clone(),
            format_hint: self.format_hint.clone(),
            metadata: self.metadata.clone(),
            date: self.date(),
        }
    }

    /// Convert this invocation record to an OutcomeRecord (v5 schema).
    ///
    /// Returns None if this is a pending invocation (no outcome yet).
    pub fn to_outcome(&self) -> Option<OutcomeRecord> {
        // Pending invocations don't have an outcome
        if self.status == "pending" {
            return None;
        }

        Some(OutcomeRecord {
            attempt_id: self.id,
            completed_at: self.timestamp + chrono::Duration::milliseconds(self.duration_ms.unwrap_or(0)),
            exit_code: self.exit_code,
            duration_ms: self.duration_ms,
            signal: None,
            timeout: false,
            metadata: HashMap::new(),
            date: self.date(),
        })
    }

    /// Create an InvocationRecord from AttemptRecord and optional OutcomeRecord (v5 schema).
    pub fn from_attempt_outcome(attempt: &AttemptRecord, outcome: Option<&OutcomeRecord>) -> Self {
        let (exit_code, duration_ms, status) = match outcome {
            Some(o) => {
                let status = if o.exit_code.is_some() {
                    "completed"
                } else {
                    "orphaned"
                };
                (o.exit_code, o.duration_ms, status.to_string())
            }
            None => (None, None, "pending".to_string()),
        };

        Self {
            id: attempt.id,
            session_id: attempt.session_id.clone(),
            timestamp: attempt.timestamp,
            duration_ms,
            cwd: attempt.cwd.clone(),
            cmd: attempt.cmd.clone(),
            executable: attempt.executable.clone(),
            runner_id: None,
            exit_code,
            status,
            format_hint: attempt.format_hint.clone(),
            client_id: attempt.source_client.clone(),
            hostname: attempt.hostname.clone(),
            username: None,
            tag: attempt.tag.clone(),
            metadata: attempt.metadata.clone(),
        }
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
            let exe = part.rsplit('/').next().unwrap_or(part);
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
    runner_id         VARCHAR,
    exit_code         INTEGER,
    status            VARCHAR DEFAULT 'completed',
    format_hint       VARCHAR,
    client_id         VARCHAR NOT NULL,
    hostname          VARCHAR,
    username          VARCHAR,
    tag               VARCHAR,
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

// =============================================================================
// BIRD v5 Schema SQL Constants
// =============================================================================

/// SQL to create the attempts table (v5 schema).
pub const ATTEMPTS_SCHEMA: &str = r#"
CREATE TABLE attempts (
    id                UUID PRIMARY KEY,
    timestamp         TIMESTAMP NOT NULL,
    cmd               VARCHAR NOT NULL,
    cwd               VARCHAR NOT NULL,
    session_id        VARCHAR NOT NULL,
    tag               VARCHAR,
    source_client     VARCHAR NOT NULL,
    machine_id        VARCHAR,
    hostname          VARCHAR,
    executable        VARCHAR,
    format_hint       VARCHAR,
    metadata          MAP(VARCHAR, JSON),
    date              DATE NOT NULL
);
"#;

/// SQL to create the outcomes table (v5 schema).
pub const OUTCOMES_SCHEMA: &str = r#"
CREATE TABLE outcomes (
    attempt_id        UUID PRIMARY KEY,
    completed_at      TIMESTAMP NOT NULL,
    exit_code         INTEGER,
    duration_ms       BIGINT,
    signal            INTEGER,
    timeout           BOOLEAN DEFAULT FALSE,
    metadata          MAP(VARCHAR, JSON),
    date              DATE NOT NULL
);
"#;

/// SQL to create the bird_meta table for schema versioning.
pub const BIRD_META_SCHEMA: &str = r#"
CREATE TABLE bird_meta (
    key               VARCHAR PRIMARY KEY,
    value             VARCHAR NOT NULL,
    updated_at        TIMESTAMP DEFAULT CURRENT_TIMESTAMP
);
"#;

/// SQL to create the invocations VIEW (v5 schema).
///
/// This joins attempts LEFT JOIN outcomes and derives the status:
/// - `pending`: attempt exists but no outcome
/// - `completed`: outcome exists with exit_code
/// - `orphaned`: outcome exists but exit_code is NULL
pub const INVOCATIONS_VIEW_SCHEMA: &str = r#"
CREATE VIEW invocations AS
SELECT
    a.id,
    a.session_id,
    a.timestamp,
    o.duration_ms,
    a.cwd,
    a.cmd,
    a.executable,
    o.exit_code,
    CASE
        WHEN o.attempt_id IS NULL THEN 'pending'
        WHEN o.exit_code IS NULL THEN 'orphaned'
        ELSE 'completed'
    END AS status,
    a.format_hint,
    a.source_client AS client_id,
    a.hostname,
    a.tag,
    o.signal,
    o.timeout,
    o.completed_at,
    -- Merge metadata from both attempt and outcome (outcome wins on conflict)
    map_concat(COALESCE(a.metadata, MAP{}), COALESCE(o.metadata, MAP{})) AS metadata,
    a.date
FROM attempts a
LEFT JOIN outcomes o ON a.id = o.attempt_id;
"#;

/// Current BIRD schema version.
pub const BIRD_SCHEMA_VERSION: &str = "5";

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
        assert_eq!(record.exit_code, Some(0));
        assert_eq!(record.status, "completed");
        assert!(record.duration_ms.is_none());
        assert!(record.runner_id.is_none());
    }

    #[test]
    fn test_invocation_record_pending() {
        let record = InvocationRecord::new_pending(
            "session-123",
            "make test",
            "/home/user/project",
            "pid:12345",
            "user@laptop",
        );

        assert_eq!(record.session_id, "session-123");
        assert_eq!(record.cmd, "make test");
        assert_eq!(record.runner_id, Some("pid:12345".to_string()));
        assert_eq!(record.exit_code, None);
        assert_eq!(record.status, "pending");
    }

    #[test]
    fn test_invocation_record_pending_local() {
        let record = InvocationRecord::new_pending_local(
            "session-123",
            "make test",
            "/home/user/project",
            12345,
            "user@laptop",
        );

        assert_eq!(record.runner_id, Some("pid:12345".to_string()));
        assert_eq!(record.status, "pending");
    }

    #[test]
    fn test_invocation_record_pending_gha() {
        let record = InvocationRecord::new_pending(
            "gha-session",
            "make test",
            "/github/workspace",
            "gha:run:123456789",
            "runner@github",
        );

        assert_eq!(record.runner_id, Some("gha:run:123456789".to_string()));
        assert_eq!(record.status, "pending");
    }

    #[test]
    fn test_invocation_record_complete() {
        let record = InvocationRecord::new_pending(
            "session-123",
            "make test",
            "/home/user/project",
            "pid:12345",
            "user@laptop",
        )
        .complete(0, Some(1500));

        assert_eq!(record.exit_code, Some(0));
        assert_eq!(record.duration_ms, Some(1500));
        assert_eq!(record.status, "completed");
    }

    #[test]
    fn test_invocation_record_orphaned() {
        let record = InvocationRecord::new_pending(
            "session-123",
            "make test",
            "/home/user/project",
            "pid:12345",
            "user@laptop",
        )
        .mark_orphaned();

        assert_eq!(record.exit_code, None);
        assert_eq!(record.status, "orphaned");
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

    // =========================================================================
    // V5 Schema Tests
    // =========================================================================

    #[test]
    fn test_attempt_record_new() {
        let attempt = AttemptRecord::new(
            "session-123",
            "make test",
            "/home/user/project",
            "user@laptop",
        );

        assert_eq!(attempt.session_id, "session-123");
        assert_eq!(attempt.cmd, "make test");
        assert_eq!(attempt.cwd, "/home/user/project");
        assert_eq!(attempt.source_client, "user@laptop");
        assert_eq!(attempt.executable, Some("make".to_string()));
        assert!(attempt.metadata.is_empty());
    }

    #[test]
    fn test_attempt_record_with_metadata() {
        let attempt = AttemptRecord::new(
            "session-123",
            "make test",
            "/home/user/project",
            "user@laptop",
        )
        .with_metadata("git_branch", serde_json::json!("main"))
        .with_metadata("ci", serde_json::json!(true));

        assert_eq!(attempt.metadata.len(), 2);
        assert_eq!(attempt.metadata.get("git_branch"), Some(&serde_json::json!("main")));
        assert_eq!(attempt.metadata.get("ci"), Some(&serde_json::json!(true)));
    }

    #[test]
    fn test_outcome_record_completed() {
        let attempt_id = Uuid::now_v7();
        let date = Utc::now().date_naive();
        let outcome = OutcomeRecord::completed(attempt_id, 0, Some(1500), date);

        assert_eq!(outcome.attempt_id, attempt_id);
        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(outcome.duration_ms, Some(1500));
        assert_eq!(outcome.signal, None);
        assert!(!outcome.timeout);
    }

    #[test]
    fn test_outcome_record_killed() {
        let attempt_id = Uuid::now_v7();
        let date = Utc::now().date_naive();
        let outcome = OutcomeRecord::killed(attempt_id, 9, Some(500), date);

        assert_eq!(outcome.exit_code, None);
        assert_eq!(outcome.signal, Some(9));
        assert!(!outcome.timeout);
    }

    #[test]
    fn test_outcome_record_timed_out() {
        let attempt_id = Uuid::now_v7();
        let date = Utc::now().date_naive();
        let outcome = OutcomeRecord::timed_out(attempt_id, 30000, date);

        assert_eq!(outcome.exit_code, None);
        assert_eq!(outcome.duration_ms, Some(30000));
        assert!(outcome.timeout);
    }

    #[test]
    fn test_outcome_record_orphaned() {
        let attempt_id = Uuid::now_v7();
        let date = Utc::now().date_naive();
        let outcome = OutcomeRecord::orphaned(attempt_id, date);

        assert_eq!(outcome.exit_code, None);
        assert_eq!(outcome.duration_ms, None);
        assert_eq!(outcome.signal, None);
        assert!(!outcome.timeout);
    }

    #[test]
    fn test_invocation_to_attempt_conversion() {
        let invocation = InvocationRecord::new(
            "session-123",
            "make test",
            "/home/user/project",
            0,
            "user@laptop",
        );

        let attempt = invocation.to_attempt();

        assert_eq!(attempt.id, invocation.id);
        assert_eq!(attempt.session_id, invocation.session_id);
        assert_eq!(attempt.cmd, invocation.cmd);
        assert_eq!(attempt.cwd, invocation.cwd);
        assert_eq!(attempt.source_client, invocation.client_id);
    }

    #[test]
    fn test_invocation_to_outcome_conversion() {
        let invocation = InvocationRecord::new(
            "session-123",
            "make test",
            "/home/user/project",
            0,
            "user@laptop",
        )
        .with_duration(1500);

        let outcome = invocation.to_outcome().expect("Should have outcome for completed invocation");

        assert_eq!(outcome.attempt_id, invocation.id);
        assert_eq!(outcome.exit_code, Some(0));
        assert_eq!(outcome.duration_ms, Some(1500));
    }

    #[test]
    fn test_pending_invocation_has_no_outcome() {
        let invocation = InvocationRecord::new_pending(
            "session-123",
            "make test",
            "/home/user/project",
            "pid:12345",
            "user@laptop",
        );

        assert!(invocation.to_outcome().is_none());
    }

    #[test]
    fn test_invocation_from_attempt_outcome() {
        let attempt = AttemptRecord::new(
            "session-123",
            "make test",
            "/home/user/project",
            "user@laptop",
        );

        // Pending: no outcome
        let pending = InvocationRecord::from_attempt_outcome(&attempt, None);
        assert_eq!(pending.status, "pending");
        assert_eq!(pending.exit_code, None);

        // Completed: outcome with exit code
        let outcome = OutcomeRecord::completed(attempt.id, 0, Some(1500), attempt.date);
        let completed = InvocationRecord::from_attempt_outcome(&attempt, Some(&outcome));
        assert_eq!(completed.status, "completed");
        assert_eq!(completed.exit_code, Some(0));
        assert_eq!(completed.duration_ms, Some(1500));

        // Orphaned: outcome without exit code
        let orphaned_outcome = OutcomeRecord::orphaned(attempt.id, attempt.date);
        let orphaned = InvocationRecord::from_attempt_outcome(&attempt, Some(&orphaned_outcome));
        assert_eq!(orphaned.status, "orphaned");
        assert_eq!(orphaned.exit_code, None);
    }
}
