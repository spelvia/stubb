//! Turns: one user input and everything the agent did in response.
//!
//! A turn is made of [`Item`]s such as messages, reasoning, and tool calls.
//! Types are meant to be used through the module path, for example
//! `turn::Item` or `turn::Status`.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Query {
    pub limit: Option<u32>,
    pub cursor: Option<String>,
}

/// Turns in chronological order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    pub turns: Vec<Turn>,
    pub next_cursor: Option<String>,
}

/// One user input and the work the agent did in response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Turn {
    pub id: String,
    pub status: Status,
    pub items: Vec<Item>,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub duration_ms: Option<i64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Status {
    InProgress,
    Completed,
    Interrupted,
    Failed,
}

/// One step of a turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Item {
    UserMessage {
        id: String,
        content: Vec<UserContent>,
    },
    AgentMessage {
        id: String,
        text: String,
    },
    Reasoning {
        id: String,
        summary: Vec<String>,
    },
    Plan {
        id: String,
        text: String,
    },
    ToolCall(ToolCall),
    /// An item Keel does not model yet, kept verbatim.
    Other {
        id: Option<String>,
        kind: String,
        raw: Value,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum UserContent {
    Text { text: String },
    Image { source: String },
    Other { kind: String },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub id: String,
    pub status: ToolStatus,
    pub duration_ms: Option<i64>,
    pub detail: ToolDetail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolStatus {
    InProgress,
    Completed,
    Failed,
    Declined,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ToolDetail {
    Command {
        command: String,
        cwd: Option<PathBuf>,
        output: Option<String>,
        exit_code: Option<i32>,
    },
    FileChange {
        changes: Vec<FileChange>,
    },
    Mcp {
        server: String,
        tool: String,
        arguments: Value,
        error: Option<String>,
    },
    /// Any other tool, identified by the provider's tool name.
    Other {
        name: String,
        input: Value,
        output: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChange {
    pub path: PathBuf,
    pub kind: FileChangeKind,
    /// Unified diff as reported by the provider.
    pub diff: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum FileChangeKind {
    Add,
    Delete,
    Update { move_path: Option<PathBuf> },
}

/// Streamed content for an item, appended to the field named by the variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Delta {
    /// Text of an [`Item::AgentMessage`].
    Text { text: String },
    /// Text of an [`Item::Reasoning`] summary.
    Reasoning { text: String },
    /// Output of a [`ToolDetail::Command`].
    Output { text: String },
}

/// A user message that starts a new turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Input {
    pub text: String,
}

impl Input {
    pub fn text(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

/// Acknowledgement that the provider accepted an [`Input`] and started a turn.
/// Progress and the outcome arrive as session events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Receipt {
    pub turn_id: String,
}

/// Token counts reported by the provider.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenCount {
    pub input: i64,
    pub cached_input: i64,
    pub output: i64,
    pub reasoning_output: i64,
    pub total: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    /// The most recent model call.
    pub last: TokenCount,
    /// The whole session so far.
    pub total: TokenCount,
    pub context_window: Option<i64>,
}
