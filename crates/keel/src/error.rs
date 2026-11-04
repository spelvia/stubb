use std::path::PathBuf;
use std::time::Duration;

/// `Result` with Keel's [`Error`] as the default error type.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Errors returned by Keel operations.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("executable not found: {executable}")]
    ExecutableNotFound { executable: PathBuf },

    #[error("failed to start {executable}: {source}")]
    SpawnFailed {
        executable: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("agent returned error {code} for {method}: {message}")]
    Rpc {
        method: String,
        code: i64,
        message: String,
    },

    #[error("timed out after {0:?}")]
    Timeout(Duration),

    #[error("agent process exited unexpectedly{}", stderr_suffix(.stderr))]
    ProcessExited { stderr: String },

    #[error("could not decode {method} response: {source}")]
    Decode {
        method: String,
        #[source]
        source: serde_json::Error,
    },

    #[error("invalid {setting} {value:?}: {reason}")]
    InvalidSetting {
        setting: &'static str,
        value: String,
        reason: String,
    },

    #[error("no stored session {id}")]
    SessionNotFound { id: String },

    /// This adapter cannot provide the requested operation. An empty result
    /// must not be used to disguise unavailable discovery or history.
    #[error("unsupported operation: {operation}")]
    Unsupported { operation: &'static str },

    #[error("no agent {id} in session {session_id}")]
    AgentNotFound { session_id: String, id: String },

    #[error("no task {id} in session {session_id}")]
    TaskNotFound { session_id: String, id: String },

    /// No pending request with this id: it was never sent, or it was already
    /// answered or withdrawn.
    #[error("no pending request {id} in session {session_id}")]
    RequestNotFound { session_id: String, id: String },

    /// Another client (for example an IDE or a second process) holds the
    /// session open for writing. Its history can still be read.
    #[error("session {id} is in use by another client: {message}")]
    SessionInUse { id: String, message: String },

    #[error("i/o error talking to the agent: {0}")]
    Io(#[from] std::io::Error),
}

impl Error {
    /// Stable machine-readable code for transports such as HTTP APIs.
    pub fn code(&self) -> &'static str {
        match self {
            Self::ExecutableNotFound { .. } => "executable_not_found",
            Self::SpawnFailed { .. } => "spawn_failed",
            Self::Rpc { .. } => "rpc_error",
            Self::Timeout(_) => "timeout",
            Self::ProcessExited { .. } => "process_exited",
            Self::Decode { .. } => "decode_error",
            Self::InvalidSetting { .. } => "invalid_setting",
            Self::SessionNotFound { .. } => "session_not_found",
            Self::Unsupported { .. } => "unsupported",
            Self::AgentNotFound { .. } => "agent_not_found",
            Self::TaskNotFound { .. } => "task_not_found",
            Self::RequestNotFound { .. } => "request_not_found",
            Self::SessionInUse { .. } => "session_in_use",
            Self::Io(_) => "io_error",
        }
    }
}

fn stderr_suffix(stderr: &str) -> String {
    let stderr = stderr.trim();
    if stderr.is_empty() {
        String::new()
    } else {
        format!(": {stderr}")
    }
}
