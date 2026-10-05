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
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
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
pub enum OutputAvailability {
    Unknown,
    Unavailable,
    Partial,
    Complete,
}
