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
#[non_exhaustive]
pub enum Status {
    InProgress,
    Completed,
    Interrupted,
    Failed,
    /// A status this version of Stubb does not know (deserialization only).
    #[serde(other)]
    Unknown,
}

/// One step of a turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[non_exhaustive]
pub enum Item {
    UserMessage {
        id: String,
        content: Vec<UserContent>,
    },
    AgentMessage {
        id: String,
        text: String,
    },
    /// What the model thought before acting.
    Thinking {
        id: String,
        text: String,
        /// The provider reported a summary of its thinking rather than the
        /// thinking itself, as Codex does.
        summarized: bool,
    },
    Plan {
        id: String,
        text: String,
    },
    ToolCall(ToolCall),
    /// An item Stubb does not model yet, kept verbatim.
    Other {
        id: Option<String>,
        kind: String,
        raw: Value,
    },
    /// An item type this version of Stubb does not know, kept as it was read
    /// (deserialization only; see [`crate::session::EVENT_FORMAT`]).
    #[serde(untagged)]
    Unknown(Value),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[non_exhaustive]
pub enum UserContent {
    Text {
        text: String,
    },
    Image {
        source: String,
    },
    Other {
        kind: String,
    },
    /// Content this version of Stubb does not know, kept as it was read
    /// (deserialization only).
    #[serde(untagged)]
    Unknown(Value),
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
#[non_exhaustive]
pub enum ToolStatus {
    InProgress,
    Completed,
    Failed,
    Declined,
    /// A status this version of Stubb does not know (deserialization only).
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[non_exhaustive]
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
    /// An MCP tool call as Stubb reported it before 0.1.1-rc.2, without its
    /// result. Stubb now reports [`ToolDetail::McpCall`]; this variant
    /// remains so stored events still read.
    Mcp {
        server: String,
        tool: String,
        arguments: Value,
        error: Option<String>,
    },
    /// A call to a tool of an MCP server, with its result.
    McpCall(McpCall),
    /// Any other tool, identified by the provider's tool name.
    Other {
        name: String,
        input: Value,
        output: Option<String>,
    },
    /// A tool kind this version of Stubb does not know, kept as it was read
    /// (deserialization only).
    #[serde(untagged)]
    Unknown(Value),
}

/// A call to an MCP server's tool: [`ToolDetail::McpCall`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct McpCall {
    pub server: String,
    pub tool: String,
    #[serde(default)]
    pub arguments: Value,
    /// What the tool returned, once it completed.
    #[serde(default)]
    pub result: Option<McpResult>,
    /// Why the call failed.
    #[serde(default)]
    pub error: Option<String>,
}

impl McpCall {
    pub(crate) fn new(
        server: impl Into<String>,
        tool: impl Into<String>,
        arguments: Value,
    ) -> Self {
        Self {
            server: server.into(),
            tool: tool.into(),
            arguments,
            result: None,
            error: None,
        }
    }
}

/// The result of an [`McpCall`], as the MCP server returned it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct McpResult {
    /// MCP content blocks, such as `{"type": "text", "text": "..."}` or
    /// `{"type": "image", "data": "...", "mimeType": "image/png"}`.
    #[serde(default)]
    pub content: Vec<Value>,
    /// The tool's structured result, when it returns one.
    #[serde(default)]
    pub structured_content: Option<Value>,
}

impl McpResult {
    /// A result from tool-result content: a list of blocks, or a string,
    /// which becomes one text block.
    pub(crate) fn from_content(content: &Value) -> Self {
        let content = match content {
            Value::Array(blocks) => blocks.clone(),
            Value::String(text) => vec![serde_json::json!({ "type": "text", "text": text })],
            Value::Null => Vec::new(),
            other => vec![other.clone()],
        };
        Self {
            content,
            structured_content: None,
        }
    }

    /// The text of its text blocks, joined by newlines.
    pub fn text(&self) -> Option<String> {
        let text: Vec<&str> = self
            .content
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect();
        (!text.is_empty()).then(|| text.join("\n"))
    }
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
#[non_exhaustive]
pub enum FileChangeKind {
    Add,
    Delete,
    Update {
        move_path: Option<PathBuf>,
    },
    /// A change kind this version of Stubb does not know, kept as it was read
    /// (deserialization only).
    #[serde(untagged)]
    Unknown(Value),
}

/// Streamed content for an item, appended to the field named by the variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
#[non_exhaustive]
pub enum Delta {
    /// Text of an [`Item::AgentMessage`].
    Text { text: String },
    /// Text of an [`Item::Thinking`].
    Thinking { text: String },
    /// Output of a [`ToolDetail::Command`].
    Output { text: String },
    /// A delta this version of Stubb does not know, kept as it was read
    /// (deserialization only).
    #[serde(untagged)]
    Unknown(Value),
}

/// A user message that starts a new turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Input {
    pub text: String,
    /// Images on the agent's machine, sent with the text. Claude Code and Pi
    /// receive their bytes; Codex reads them from their paths.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<Image>,
}

impl Input {
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            images: Vec::new(),
        }
    }

    /// Adds images to the message.
    #[must_use]
    pub fn with_images(mut self, images: Vec<Image>) -> Self {
        self.images = images;
        self
    }
}

/// An image file for an [`Input`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Image {
    /// An absolute path on the agent's machine.
    pub path: std::path::PathBuf,
    /// `image/png`, `image/jpeg`, `image/gif` or `image/webp`.
    pub mime_type: String,
}

impl Image {
    pub fn new(path: impl Into<std::path::PathBuf>, mime_type: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            mime_type: mime_type.into(),
        }
    }

    /// The file's bytes, base64-encoded.
    pub(crate) async fn base64(&self) -> crate::Result<String> {
        use base64::Engine as _;
        let bytes =
            tokio::fs::read(&self.path)
                .await
                .map_err(|source| crate::Error::ImageUnreadable {
                    path: self.path.clone(),
                    source,
                })?;
        Ok(base64::engine::general_purpose::STANDARD.encode(bytes))
    }
}

/// How a user message with images is recorded: the images, then the text.
pub(crate) fn user_content(input: &Input) -> Vec<UserContent> {
    input
        .images
        .iter()
        .map(|image| UserContent::Image {
            source: image.path.display().to_string(),
        })
        .chain([UserContent::Text {
            text: input.text.clone(),
        }])
        .collect()
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
    /// The main agent's most recent model call. Its `input` plus `output` is
    /// how much of the context window is in use; sub-agents' calls are not
    /// counted here.
    pub last: TokenCount,
    /// Every model call since this live session started. Resuming a session
    /// starts the count again. ACP agents only report how full the context
    /// is, so for them this equals `last` and falls after a compaction.
    pub total: TokenCount,
    pub context_window: Option<i64>,
}

/// A context compaction: the agent replaced older parts of the conversation
/// with a summary to free context window.
///
/// Reported as it starts ([`CompactionStatus::InProgress`]) and again when it
/// ends; agents that only report the end send that alone. Fields the agent
/// does not report are `None`. After a completed compaction, the next
/// [`Usage`] reflects the smaller context; `tokens_after` gives it earlier.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Compaction {
    /// The agent's ID for this compaction, when it has one. Start and end
    /// carry the same ID.
    pub id: Option<String>,
    pub status: CompactionStatus,
    pub trigger: Option<CompactionTrigger>,
    /// Tokens in context before the compaction.
    pub tokens_before: Option<i64>,
    /// Tokens in context after it; some agents only estimate this.
    pub tokens_after: Option<i64>,
    /// Why it failed.
    pub error: Option<String>,
}

impl Compaction {
    pub(crate) fn new(status: CompactionStatus) -> Self {
        Self {
            id: None,
            status,
            trigger: None,
            tokens_before: None,
            tokens_after: None,
            error: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum CompactionStatus {
    InProgress,
    Completed,
    Failed,
    Cancelled,
    /// A status this version of Stubb does not know (deserialization only).
    #[serde(other)]
    Unknown,
}

/// What started a compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum CompactionTrigger {
    /// Someone asked for it, for example with `/compact`.
    Manual,
    /// The context was nearly full.
    Auto,
    /// A model call failed because the context was full; the agent compacts
    /// and retries.
    Overflow,
    /// A trigger this version of Stubb does not know (deserialization only).
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_are_recorded_before_the_text() {
        let input =
            Input::text("what is this?").with_images(vec![Image::new("/tmp/a.png", "image/png")]);
        assert_eq!(
            user_content(&input),
            vec![
                UserContent::Image {
                    source: "/tmp/a.png".into()
                },
                UserContent::Text {
                    text: "what is this?".into()
                },
            ]
        );
    }

    #[tokio::test]
    async fn images_are_read_as_base64() {
        let path = std::env::temp_dir().join(format!("stubb-image-{}.bin", std::process::id()));
        std::fs::write(&path, b"hi").unwrap();
        assert_eq!(
            Image::new(&path, "image/png").base64().await.unwrap(),
            "aGk="
        );
        std::fs::remove_file(&path).unwrap();
        let missing = Image::new("/nonexistent/stubb.png", "image/png")
            .base64()
            .await;
        assert_eq!(missing.unwrap_err().code(), "image_unreadable");
    }
}
