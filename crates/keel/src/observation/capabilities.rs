//! Effective adapter support and coverage of native features.

use serde::{Deserialize, Serialize};

/// Support and observed coverage are distinct. Complete coverage is relative to
/// the negotiated native surface and does not promise access to invisible work.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Feature {
    pub support: Support,
    pub coverage: Coverage,
    pub reason: Option<String>,
    pub gaps: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Support {
    Supported,
    Unsupported,
    #[default]
    Unknown,
    Denied,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Coverage {
    Complete,
    Partial,
    #[default]
    Unknown,
}

/// Effective support depends on adapter implementation, executable version,
/// configuration and extensions, rather than the selected model alone.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    pub records: Feature,
    pub snapshots: Feature,
    pub agent_discovery: Feature,
    pub parent_links: Feature,
    pub nested_discovery: Feature,
    pub agent_lifecycle: Feature,
    pub agent_transcript: Feature,
    pub agent_history: Feature,
    pub task_discovery: Feature,
    pub task_lifecycle: Feature,
    pub task_output: Feature,
    pub task_history: Feature,
    pub stop_agent: Feature,
    /// Only native subtree controls may implement this; Keel never simulates it.
    pub stop_subtree: Feature,
    pub stop_task: Feature,
}
