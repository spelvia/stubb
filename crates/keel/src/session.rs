//! Sessions: durable conversations with a harness. Codex calls them threads.
//!
//! [`Session`] is the handle to a session loaded into a running harness; the
//! other types describe stored and live sessions. Use them through the module
//! path, for example `session::Summary` or `session::Event`. Identifiers are
//! the provider's own and are passed back verbatim.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast;

use crate::provider::BoxFuture;
use crate::{Result, turn};

/// A session loaded into a running harness.
///
/// Cloning shares the same session. Dropping every handle does not close the
/// session; call [`Session::close`] for that.
#[derive(Clone)]
pub struct Session {
    info: Info,
    driver: Arc<dyn Driver>,
}

impl Session {
    /// Wraps a provider's [`Driver`]. Providers call this from
    /// [`Provider::start_session`](crate::Provider::start_session) and
    /// [`Provider::resume_session`](crate::Provider::resume_session).
    pub fn new(info: Info, driver: Arc<dyn Driver>) -> Self {
        Self { info, driver }
    }

    pub fn id(&self) -> &str {
        &self.info.summary.id
    }

    /// Configuration reported when the session was started or resumed.
    pub fn info(&self) -> &Info {
        &self.info
    }

    /// Receives events emitted after this call.
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.driver.subscribe()
    }

    /// Starts a turn with `input`. Subscribe first to receive all of its
    /// events; the receipt only confirms that the turn started.
    pub async fn send(&self, input: turn::Input) -> Result<turn::Receipt> {
        self.driver.send(input).await
    }

    /// Asks the provider to stop a running turn. The turn then completes with
    /// status [`turn::Status::Interrupted`].
    pub async fn interrupt(&self, turn_id: &str) -> Result<()> {
        self.driver.interrupt(turn_id).await
    }

    /// Detaches from the session. Stored history is kept.
    pub async fn close(&self) -> Result<()> {
        self.driver.close().await
    }
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session").field("info", &self.info).finish()
    }
}

/// The provider-specific half of a [`Session`].
///
/// Keel's providers implement it; so can a host that adds its own provider,
/// for example a mock for its tests. [`Session`] forwards to these methods,
/// so their contracts are the ones documented there.
///
/// A turn's events: [`Event::Status`] `Active`, then [`Event::TurnStart`]
/// with the receipt's turn id, the user message as an [`Event::ItemEnd`],
/// the agent's items, and finally [`Event::TurnEnd`]. The session reports
/// [`Event::Status`] `Idle` once the turn is over. Every item event names
/// that turn.
pub trait Driver: Send + Sync {
    /// A receiver for events emitted from now on.
    fn subscribe(&self) -> broadcast::Receiver<Event>;
    /// Starts a turn; returns once the agent accepted it. The receipt's turn
    /// id is the one the turn's events carry and the one it is stored under.
    fn send(&self, input: turn::Input) -> BoxFuture<'_, Result<turn::Receipt>>;
    /// Asks the agent to stop a running turn, which then ends with
    /// [`turn::Status::Interrupted`].
    fn interrupt<'a>(&'a self, turn_id: &'a str) -> BoxFuture<'a, Result<()>>;
    /// Detaches from the session, announcing [`Event::Closed`] with
    /// [`CloseReason::Client`] if it was still live.
    fn close(&self) -> BoxFuture<'_, Result<()>>;
}

/// Filters for listing stored sessions.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Query {
    /// Only sessions whose working directory matches exactly.
    pub cwd: Option<PathBuf>,
    pub limit: Option<u32>,
    /// Opaque cursor from a previous [`Page`].
    pub cursor: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    pub sessions: Vec<Summary>,
    pub next_cursor: Option<String>,
}

/// Stored metadata for a session; available without loading it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub id: String,
    pub provider: String,
    pub title: Option<String>,
    /// Usually the first user message.
    pub preview: String,
    pub cwd: PathBuf,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub status: Status,
    /// Unix timestamps in seconds.
    pub created_at: i64,
    pub updated_at: i64,
    /// Which client created the session, as reported by the provider.
    pub source: Option<String>,
}

/// Runtime state of a session inside the provider process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Status {
    /// Stored but not loaded into a running harness.
    NotLoaded,
    Idle,
    /// A turn or background work is running.
    Active,
    Error,
}

/// Settings requested when starting or resuming a session. `None` keeps the
/// provider's default or the session's stored value.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub model: Option<String>,
    pub effort: Option<String>,
    pub fast_mode: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Spec {
    pub cwd: PathBuf,
    #[serde(default)]
    pub settings: Settings,
}

/// Effective configuration of a live session as reported by the provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Info {
    pub summary: Summary,
    pub model: String,
    pub effort: Option<String>,
    pub fast_mode: bool,
    pub cwd: PathBuf,
    pub approval_policy: Option<String>,
    pub sandbox: Option<String>,
    /// Whether the session was restored from storage rather than created.
    pub restored: bool,
    /// Whether the provider has written the session to storage. Codex only
    /// does so after the first turn; until then the session lives in memory.
    pub persisted: bool,
}

/// Events for a live session, delivered to every subscriber.
///
/// Turns and items follow one lifecycle: a `…Start`, optional deltas, and a
/// `…End` carrying the final state. A consumer that does not show live
/// typing can ignore [`Event::ItemDelta`] and keep only [`Event::ItemEnd`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Event {
    /// The session's runtime state changed, for example to working or idle.
    Status { status: Status },
    /// The live session is over; resume it to continue.
    Closed { reason: CloseReason },
    /// The agent accepted a message and started working on it.
    TurnStart { turn_id: String },
    /// The turn is over; `turn.status` says how. Its items were delivered
    /// by the item events, so `turn.items` is empty.
    TurnEnd { turn: turn::Turn },
    /// A new item began; it may still be incomplete.
    ItemStart { turn_id: String, item: turn::Item },
    /// Streamed content to append to an item.
    ItemDelta {
        turn_id: String,
        item_id: String,
        delta: turn::Delta,
    },
    /// The final state of an item; replaces anything built from deltas.
    ItemEnd { turn_id: String, item: turn::Item },
    /// Token usage after a model call.
    Usage { turn_id: String, usage: turn::Usage },
    /// The agent reported an error. While `retrying`, the turn continues.
    Error {
        turn_id: Option<String>,
        message: String,
        retrying: bool,
    },
    /// The agent asked for something Keel cannot answer yet, such as an
    /// approval, so Keel declined it. Approval events will replace this.
    RequestDeclined {
        method: String,
        turn_id: Option<String>,
        summary: Option<String>,
    },
    /// Provider traffic Keel does not map yet, passed through unchanged.
    Raw { name: String, payload: Value },
}

/// Why a live session ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum CloseReason {
    /// [`Session::close`] was called.
    Client,
    /// The agent unloaded the session.
    Agent,
    /// The agent process exited unexpectedly.
    Exited { message: String },
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// The wire format is camelCase throughout, including fields of enum variants.
    #[test]
    fn events_serialize_with_camel_case_fields() {
        let event = Event::ItemDelta {
            turn_id: "u".into(),
            item_id: "i".into(),
            delta: turn::Delta::Text { text: "hi".into() },
        };
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            json!({ "type": "itemDelta", "turnId": "u", "itemId": "i", "delta": { "type": "text", "text": "hi" } })
        );

        let command = turn::ToolDetail::Command {
            command: "ls".into(),
            cwd: None,
            output: None,
            exit_code: Some(0),
        };
        assert_eq!(
            serde_json::to_value(&command).unwrap()["exitCode"],
            json!(0)
        );

        let fast = crate::catalog::FastMode::Available {
            tier_id: "priority".into(),
            label: "Fast".into(),
            description: String::new(),
            default_on: true,
        };
        assert_eq!(
            serde_json::to_value(&fast).unwrap()["tierId"],
            json!("priority")
        );
    }
}
