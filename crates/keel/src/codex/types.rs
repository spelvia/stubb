//! Hand-written subset of the Codex app-server protocol (checked against
//! `codex-rs/app-server-protocol` at `rust-v0.159.0`). Unknown fields are
//! ignored so newer Codex releases keep decoding.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InitializeParams {
    pub client_info: ClientInfo,
    pub capabilities: InitializeCapabilities,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InitializeCapabilities {
    pub experimental_api: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ClientInfo {
    pub name: String,
    pub title: Option<String>,
    pub version: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InitializeResponse {
    #[serde(default)]
    pub user_agent: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ModelListParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    pub limit: Option<u32>,
    pub include_hidden: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ModelListResponse {
    pub data: Vec<Model>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Model {
    pub id: String,
    pub display_name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub is_default: bool,
    #[serde(default)]
    pub supported_reasoning_efforts: Vec<ReasoningEffortOption>,
    #[serde(default)]
    pub default_reasoning_effort: Option<String>,
    #[serde(default)]
    pub service_tiers: Vec<ServiceTier>,
    #[serde(default)]
    pub default_service_tier: Option<String>,
    /// Deprecated by Codex in favour of `service_tiers`; read as a fallback.
    #[serde(default)]
    pub additional_speed_tiers: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReasoningEffortOption {
    pub reasoning_effort: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ServiceTier {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
}

// Threads

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ThreadListParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    pub limit: Option<u32>,
    pub sort_key: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    pub source_kinds: Vec<&'static str>,
    pub sort_direction: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ancestor_thread_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ThreadListResponse {
    pub data: Vec<Thread>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ThreadIdParams<'a> {
    pub thread_id: &'a str,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ThreadReadResponse {
    pub thread: Thread,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Thread {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub preview: String,
    pub cwd: PathBuf,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    pub status: ThreadStatus,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub updated_at: i64,
    /// A string such as `vscode`, or an object for sub-agent threads.
    #[serde(default)]
    pub source: Value,
    #[serde(default)]
    pub parent_thread_id: Option<String>,
    #[serde(default)]
    pub agent_nickname: Option<String>,
    #[serde(default)]
    pub agent_role: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(crate) enum ThreadStatus {
    NotLoaded,
    Idle,
    SystemError,
    Active,
}

/// Fields shared by `thread/start` and `thread/resume`; unset fields keep
/// Codex's defaults or the stored thread's values.
#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ThreadSettingsParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// `Some(None)` explicitly clears the tier (standard speed).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_tier: Option<Option<String>>,
    /// Config overrides such as `model_reasoning_effort`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub config: Option<HashMap<String, Value>>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ThreadStartParams {
    pub cwd: String,
    #[serde(flatten)]
    pub settings: ThreadSettingsParams,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ThreadResumeParams<'a> {
    pub thread_id: &'a str,
    /// Return metadata only; history is paged with `thread/turns/list`.
    pub exclude_turns: bool,
    #[serde(flatten)]
    pub settings: ThreadSettingsParams,
}

/// Response of both `thread/start` and `thread/resume`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ThreadSessionResponse {
    pub thread: Thread,
    pub model: String,
    #[serde(default)]
    pub service_tier: Option<String>,
    pub cwd: PathBuf,
    #[serde(default)]
    pub approval_policy: Value,
    #[serde(default)]
    pub sandbox: Value,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ThreadTurnsListParams<'a> {
    pub thread_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    pub limit: Option<u32>,
    pub sort_direction: &'static str,
    pub items_view: &'static str,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ThreadTurnsListResponse {
    pub data: Vec<Turn>,
    #[serde(default)]
    pub next_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Turn {
    pub id: String,
    /// Kept raw so one unfamiliar item does not fail the whole turn.
    #[serde(default)]
    pub items: Vec<Value>,
    pub status: TurnStatus,
    #[serde(default)]
    pub error: Option<TurnError>,
    #[serde(default)]
    pub started_at: Option<i64>,
    #[serde(default)]
    pub completed_at: Option<i64>,
    #[serde(default)]
    pub duration_ms: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum TurnStatus {
    Completed,
    Interrupted,
    Failed,
    InProgress,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TurnError {
    pub message: String,
}

// Thread items, decoded one variant at a time from the raw item.

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UserMessageItem {
    pub id: String,
    #[serde(default)]
    pub content: Vec<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TextItem {
    pub id: String,
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ReasoningItem {
    pub id: String,
    #[serde(default)]
    pub summary: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CommandExecutionItem {
    pub id: String,
    pub command: String,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    pub status: ToolStatus,
    #[serde(default)]
    pub aggregated_output: Option<String>,
    #[serde(default)]
    pub exit_code: Option<i32>,
    #[serde(default)]
    pub duration_ms: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FileChangeItem {
    pub id: String,
    #[serde(default)]
    pub changes: Vec<FileUpdateChange>,
    pub status: ToolStatus,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FileUpdateChange {
    pub path: PathBuf,
    pub kind: PatchChangeKind,
    #[serde(default)]
    pub diff: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(crate) enum PatchChangeKind {
    Add,
    Delete,
    #[serde(rename_all = "camelCase")]
    Update {
        move_path: Option<PathBuf>,
    },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct McpToolCallItem {
    pub id: String,
    pub server: String,
    pub tool: String,
    pub status: ToolStatus,
    #[serde(default)]
    pub arguments: Value,
    #[serde(default)]
    pub error: Option<Value>,
    #[serde(default)]
    pub duration_ms: Option<i64>,
}

/// Status values shared by command, file-change and MCP items.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum ToolStatus {
    InProgress,
    Completed,
    Failed,
    Declined,
}

// Turns: requests

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TurnStartParams<'a> {
    pub thread_id: &'a str,
    pub input: Vec<UserInput<'a>>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub(crate) enum UserInput<'a> {
    Text { text: &'a str },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TurnStartResponse {
    pub turn: Turn,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TurnInterruptParams<'a> {
    pub thread_id: &'a str,
    pub turn_id: &'a str,
}

// Turns: notifications

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StatusChangedNotification {
    pub status: ThreadStatus,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TurnNotification {
    pub turn: Turn,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ItemNotification {
    pub turn_id: String,
    pub item: Value,
}

/// `item/agentMessage/delta`, `item/reasoning/summaryTextDelta` and
/// `item/commandExecution/outputDelta` share this shape.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeltaNotification {
    pub turn_id: String,
    pub item_id: String,
    pub delta: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TokenUsageNotification {
    pub turn_id: String,
    pub token_usage: ThreadTokenUsage,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ThreadTokenUsage {
    pub total: TokenUsageBreakdown,
    pub last: TokenUsageBreakdown,
    #[serde(default)]
    pub model_context_window: Option<i64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TokenUsageBreakdown {
    #[serde(default)]
    pub total_tokens: i64,
    #[serde(default)]
    pub input_tokens: i64,
    #[serde(default)]
    pub cached_input_tokens: i64,
    #[serde(default)]
    pub output_tokens: i64,
    #[serde(default)]
    pub reasoning_output_tokens: i64,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ErrorNotification {
    pub error: TurnError,
    #[serde(default)]
    pub will_retry: bool,
    #[serde(default)]
    pub turn_id: Option<String>,
}
