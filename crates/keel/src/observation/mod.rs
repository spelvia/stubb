//! Session-wide observation contracts, separate from the legacy root event stream.
//!
//! These types describe a negotiated harness surface, not Keel orchestration.
//! Adapters must explicitly implement observation; empty results cannot stand in
//! for unsupported discovery. Check effective capabilities:
//! native support and adapter coverage vary by feature.

use serde::{Deserialize, Serialize};

mod capabilities;

pub use capabilities::{Capabilities, Coverage, Feature, Support};

use crate::{agent, session, task};

/// One observation in a live attachment. Sequences begin at one and increase
/// under the same boundary that updates snapshot state. A new attachment gets a
/// new stream ID; sequences are not durable replay cursors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    pub stream_id: String,
    pub seq: u64,
    /// Keel observation time in Unix milliseconds, not a native event timestamp.
    pub at: i64,
    /// Activity owner, including the root. Turn/item/usage activity uses `Some`;
    /// aggregate status, root attachment closure and agent/task replacements
    /// use `None`. Unresolved raw traffic may also use `None` while retaining
    /// native IDs in its payload. Agent/task updates identify the affected
    /// resource in payload, rather than naming the emitter here.
    pub agent_id: Option<String>,
    pub event: Event,
}

/// Full state replacements and attributed legacy activity. Announce each agent
/// before its activity, using an unknown-parent placeholder if necessary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Event {
    Control {
        receipt: crate::control::Receipt,
    },
    Agent {
        agent: agent::Summary,
    },
    Task {
        task: task::Summary,
    },
    /// Turn/item IDs belong to the record's agent. Root turn completion does not
    /// end observation of descendants or background commands. `Closed` denotes
    /// root attachment closure; child closure is an agent summary update.
    Activity {
        event: session::Event,
        /// Present for usage activity; missing information is `Unknown`, never
        /// an implicit promise that totals exclude descendants.
        usage_scope: Option<UsageScope>,
    },
    /// An observation this version of Keel does not know, kept as it was
    /// read (deserialization only).
    #[serde(untagged)]
    Unknown(serde_json::Value),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UsageScope {
    AgentOnly,
    IncludesDescendants,
    /// Not reported, or a scope this version of Keel does not know.
    #[serde(other)]
    Unknown,
}

/// Atomic state at `seq` (zero before any records). Subscribe first, buffer,
/// fetch this snapshot, then apply records from the same stream with greater
/// sequences. Repeat after receiver lag or attachment changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub stream_id: String,
    pub seq: u64,
    pub status: session::Status,
    pub root_agent_id: String,
    pub agents: Vec<agent::Summary>,
    pub tasks: Vec<task::Summary>,
    /// Materialized turns/items still receiving activity, including commands
    /// that continue after their owning turn has ended.
    pub active_turns: Vec<agent::OwnedTurn>,
    pub pending_controls: Vec<crate::control::Receipt>,
    pub capabilities: Capabilities,
}

#[cfg(test)]
mod tests;
