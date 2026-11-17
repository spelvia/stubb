//! Native tasks initiated by the harness. Keel does not launch commands.

use serde::{Deserialize, Serialize};

/// Complete native task state, retained after completion for a live attachment.
/// Task IDs are opaque within the session; they are not operating-system PIDs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub id: String,
    /// `None` means ownership is unresolved.
    pub owner_agent_id: Option<String>,
    /// Native task nesting only; do not infer this from agent ancestry.
    pub parent_id: Option<String>,
    pub origin: Option<crate::agent::Origin>,
    pub kind: Kind,
    pub description: Option<String>,
    pub background: Option<bool>,
    /// Housekeeping work, if reported by the harness.
    pub ambient: Option<bool>,
    pub status: Status,
    pub output: OutputAvailability,
    /// Native progress only; absent metrics are never estimated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<Progress>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[non_exhaustive]
pub enum Kind {
    Command {
        command: String,
    },
    /// Refers to an existing agent node, rather than creating another agent.
    AgentRun {
        agent_id: String,
    },
    Other {
        name: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum Status {
    Unknown,
    Pending,
    Running,
    Paused,
    /// A stop was accepted; this does not confirm termination.
    Stopping,
    Completed,
    Failed,
    Stopped,
}

/// Output availability is independent of execution status. Absence from a
/// running-only native listing establishes neither an exit code nor an outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum OutputAvailability {
    Unknown,
    Unavailable,
    Partial,
    Complete,
}

/// Usage and progress reported by the native task surface.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub total_tokens: Option<u64>,
    pub tool_uses: Option<u64>,
    pub duration_ms: Option<u64>,
    pub last_tool_name: Option<String>,
    pub summary: Option<String>,
}

/// Read-only stored task discovery. Filters apply before pagination.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Query {
    pub owner_agent_id: Option<String>,
    pub limit: Option<u32>,
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    pub tasks: Vec<Summary>,
    pub next_cursor: Option<String>,
}

/// Bounded log reads. Cursors are scoped to the session, task and native file;
/// they must not be reused after file replacement or truncation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputQuery {
    /// Maximum bytes, from 4 to 65536. Defaults to 16384.
    pub limit: Option<u32>,
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OutputPage {
    pub task_id: String,
    pub text: String,
    /// Continue from this cursor, including at the current end of a growing log.
    pub next_cursor: Option<String>,
    pub availability: OutputAvailability,
    /// At the end of the bytes currently available; a running file may grow.
    pub eof: bool,
}
