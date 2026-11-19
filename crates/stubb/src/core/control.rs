//! Receipts for native stop requests. Stubb does not launch or orchestrate work.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
#[non_exhaustive]
pub enum Target {
    Agent { id: String },
    Task { id: String },
}

/// A full replacement, also published in the observation stream. Agent stops
/// target the captured turn, not any later continuation of that agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Receipt {
    pub id: String,
    pub target: Target,
    pub turn_id: Option<String>,
    pub disposition: Disposition,
    pub message: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum Disposition {
    Requested,
    /// Native acknowledgement, awaiting outcome evidence.
    Accepted,
    /// Native evidence confirms interruption/termination of the captured work.
    /// This does not claim that descendants or owned tasks stopped.
    Confirmed,
    AlreadyFinished,
    /// No active generation could be established; no stop was sent.
    NotRunning,
    /// An acknowledgement was lost or native outcome is inconclusive.
    Unconfirmed,
    Rejected,
}

impl Receipt {
    pub fn is_pending(&self) -> bool {
        matches!(
            self.disposition,
            Disposition::Requested | Disposition::Accepted | Disposition::Unconfirmed
        )
    }
}
