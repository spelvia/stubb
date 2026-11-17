//! Root and descendant agents observed within a session.
//!
//! IDs are opaque and stable within a provider/session across history reads and
//! resume. Names, depth, and arrival order never establish identity or ancestry.

use serde::{Deserialize, Serialize};

/// Complete agent state. Updates replace the previous summary with this ID.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub id: String,
    pub parent: Parent,
    pub name: Option<String>,
    pub role: Option<String>,
    /// Effective model only when reported by the harness.
    pub model: Option<String>,
    pub description: Option<String>,
    /// `None` means the harness did not report this property.
    pub background: Option<bool>,
    pub status: Status,
    pub origin: Option<Origin>,
    /// Native Unix timestamps in seconds, when available.
    pub created_at: Option<i64>,
    pub updated_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[non_exhaustive]
pub enum Parent {
    /// This is the root agent; exactly one exists in an observed session.
    Root,
    Agent {
        id: String,
    },
    /// Ancestry is unresolved; a later summary may establish the parent.
    Unknown,
}

/// The activity that created an agent or task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Origin {
    pub agent_id: String,
    pub turn_id: Option<String>,
    pub item_id: Option<String>,
}

/// Latest work state, rather than a permanent prohibition on future turns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum Status {
    Unknown,
    Pending,
    Running,
    Waiting,
    Completed,
    Failed,
    Interrupted,
    /// The native agent was explicitly closed.
    Closed,
}

/// Stored discovery filters. Filtering precedes pagination; cursors are scoped
/// to the root session and filter. No filter includes the root and descendants.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Query {
    pub parent_id: Option<String>,
    pub ancestor_id: Option<String>,
    pub limit: Option<u32>,
    pub cursor: Option<String>,
}

impl Query {
    /// Parent and ancestor filters are mutually exclusive.
    pub fn validate(&self) -> crate::Result<()> {
        if self.parent_id.is_some() && self.ancestor_id.is_some() {
            return Err(crate::Error::InvalidSetting {
                setting: "agent filter",
                value: "parentId + ancestorId".into(),
                reason: "parent and ancestor filters are mutually exclusive".into(),
            });
        }
        Ok(())
    }
}

/// Agents in stable creation order; unknown native timestamps remain absent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    pub agents: Vec<Summary>,
    pub next_cursor: Option<String>,
}

/// Explicit ownership without changing the legacy root-only turn type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnedTurn {
    pub agent_id: String,
    pub turn: crate::turn::Turn,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnPage {
    pub turns: Vec<OwnedTurn>,
    pub next_cursor: Option<String>,
}
