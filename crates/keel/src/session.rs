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
use crate::{Error, Result, observation, request, turn};

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

    /// Session-wide attributed activity, including observed descendants and
    /// tasks. This is opt-in; the legacy subscription remains root-only.
    /// Subscribe before sending or fetching a snapshot. Receiver lag requires
    /// snapshot recovery rather than silently skipping records.
    pub fn subscribe_records(&self) -> Result<broadcast::Receiver<observation::Record>> {
        self.driver.subscribe_records()
    }

    /// Effective capabilities for this attachment, without claiming native
    /// functionality that the adapter has not implemented.
    pub fn observation_capabilities(&self) -> Result<observation::Capabilities> {
        self.driver.observation_capabilities()
    }

    /// Atomic state and watermark for the attributed stream. See
    /// [`observation::Snapshot`] for the subscribe-before-snapshot protocol.
    pub async fn snapshot(&self) -> Result<observation::Snapshot> {
        self.driver.snapshot().await
    }

    /// Interrupts the agent's currently observed native turn. Does not close
    /// its context or recursively stop descendants/tasks. Follow control records
    /// for confirmation; an accepted request alone is not a final outcome.
    pub async fn stop_agent(&self, id: &str) -> Result<crate::control::Receipt> {
        self.driver.stop_agent(id).await
    }

    /// Terminates an observed native task by opaque attachment-scoped ID.
    pub async fn stop_task(&self, id: &str) -> Result<crate::control::Receipt> {
        self.driver.stop_task(id).await
    }

    /// Reads native logs of an observed task. No command is started. Unsupported
    /// adapters return a typed error; missing output is reported independently
    /// of task outcome. Use the cursor to follow an append-only log.
    pub async fn task_output(
        &self,
        id: &str,
        query: crate::task::OutputQuery,
    ) -> Result<crate::task::OutputPage> {
        self.driver.task_output(id, query).await
    }

    /// Changes what the agent may do without asking, from now on: Claude
    /// Code switches mode at once; Codex applies it from its next turn. Pi
    /// reports it unsupported, as does a configuration for another provider.
    pub async fn set_permissions(
        &self,
        configuration: crate::permission::Configuration,
    ) -> Result<()> {
        self.driver.set_permissions(configuration).await
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

    /// Answers a pending [`Event::Request`]. Only sessions started or resumed
    /// with [`Settings::forward_requests`] receive requests. An id that is not
    /// pending, because it was answered or withdrawn, is
    /// [`Error::RequestNotFound`].
    pub async fn respond(&self, request_id: &str, decision: request::Decision) -> Result<()> {
        self.driver.respond(request_id, decision).await
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
    /// Attributed stream. The default explicitly reports unavailable support.
    fn subscribe_records(&self) -> Result<broadcast::Receiver<observation::Record>> {
        Err(Error::Unsupported {
            operation: "subscribe_records",
        })
    }
    /// Effective capability inspection for this attachment.
    fn observation_capabilities(&self) -> Result<observation::Capabilities> {
        Err(Error::Unsupported {
            operation: "observation_capabilities",
        })
    }
    /// Atomic state from the same reducer that sequences records.
    fn snapshot(&self) -> BoxFuture<'_, Result<observation::Snapshot>> {
        Box::pin(async {
            Err(Error::Unsupported {
                operation: "snapshot",
            })
        })
    }
    fn stop_agent<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<crate::control::Receipt>> {
        Box::pin(async {
            Err(Error::Unsupported {
                operation: "stop_agent",
            })
        })
    }
    fn stop_task<'a>(&'a self, _id: &'a str) -> BoxFuture<'a, Result<crate::control::Receipt>> {
        Box::pin(async {
            Err(Error::Unsupported {
                operation: "stop_task",
            })
        })
    }
    fn task_output<'a>(
        &'a self,
        _id: &'a str,
        _query: crate::task::OutputQuery,
    ) -> BoxFuture<'a, Result<crate::task::OutputPage>> {
        Box::pin(async {
            Err(Error::Unsupported {
                operation: "task_output",
            })
        })
    }

    /// Starts a turn; returns once the agent accepted it. The receipt's turn
    /// id is the one the turn's events carry and the one it is stored under.
    fn send(&self, input: turn::Input) -> BoxFuture<'_, Result<turn::Receipt>>;
    /// Asks the agent to stop a running turn, which then ends with
    /// [`turn::Status::Interrupted`].
    fn interrupt<'a>(&'a self, turn_id: &'a str) -> BoxFuture<'a, Result<()>>;
    /// Answers a forwarded request. Adapters that never forward requests keep
    /// the default, which reports the operation as unsupported.
    fn respond<'a>(
        &'a self,
        _request_id: &'a str,
        _decision: request::Decision,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async {
            Err(Error::Unsupported {
                operation: "respond",
            })
        })
    }
    /// Changes the permission configuration of the live session. Adapters
    /// that cannot keep the default, which reports it as unsupported.
    fn set_permissions(
        &self,
        _configuration: crate::permission::Configuration,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async {
            Err(Error::Unsupported {
                operation: "set_permissions",
            })
        })
    }
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
    /// Provider-specific start/resume permission overrides.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<crate::permission::Configuration>,
    /// Forward the agent's requests, such as tool permission prompts, as
    /// [`Event::Request`] and wait for [`Session::respond`]. Off by default:
    /// Keel then declines them itself. See [`crate::request`].
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub forward_requests: bool,
    /// Folders outside `cwd` the agent may read without asking, such as
    /// where the host keeps files attached to messages. Claude Code is
    /// given them with `--add-dir`; Codex's sandbox and Pi already read
    /// anywhere.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub additional_directories: Vec<std::path::PathBuf>,
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
    /// Structured requested and native-confirmed permission configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<crate::permission::Info>,
    /// Legacy display description; use `permissions` for structured Codex data.
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
    /// The agent is waiting for an answer; reply with [`Session::respond`].
    /// Only sent when the session forwards requests.
    Request { request: request::Request },
    /// A forwarded request is no longer pending.
    RequestResolved {
        id: String,
        outcome: request::Outcome,
    },
    /// The agent asked for something Keel did not forward, such as an
    /// approval in a session without [`Settings::forward_requests`], so Keel
    /// declined it.
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
