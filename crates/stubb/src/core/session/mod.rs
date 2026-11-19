//! Sessions: durable conversations with a harness. Codex calls them threads.
//!
//! [`Session`] is the handle to a session loaded into a running harness; the
//! other types describe stored and live sessions. Use them through the module
//! path, for example `session::Summary` or `session::Event`. Identifiers are
//! the provider's own and are passed back verbatim.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::{Result, observation, request, turn};

mod driver;
mod event;

pub use driver::Driver;
pub use event::{CloseReason, EVENT_FORMAT, Event};

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

    /// The agent's named modes, such as `ask` or `code`, and the current
    /// one. Only ACP agents that report modes have them; other sessions
    /// report the operation unsupported.
    pub fn modes(&self) -> Result<Modes> {
        self.driver.modes()
    }

    /// Switches the agent to one of its [`modes`](Session::modes), from
    /// now on. A mode the agent does not offer is
    /// [`crate::Error::InvalidSetting`].
    pub async fn set_mode(&self, mode_id: &str) -> Result<()> {
        self.driver.set_mode(mode_id).await
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
    /// [`crate::Error::RequestNotFound`].
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

/// Where to fork a stored session with
/// [`Provider::fork_session`](crate::Provider::fork_session).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fork {
    /// The last turn to keep, by the id [`Event::TurnStart`] and
    /// [`Provider::list_turns`](crate::Provider::list_turns) report: the
    /// fork holds the turns up to and including it. `None` keeps every
    /// turn.
    pub at_turn: Option<String>,
}

/// Runtime state of a session inside the provider process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum Status {
    /// Stored but not loaded into a running harness.
    NotLoaded,
    Idle,
    /// A turn or background work is running.
    Active,
    Error,
    /// A status this version of Stubb does not know (deserialization only).
    #[serde(other)]
    Unknown,
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
    /// Stubb then declines them itself. See [`crate::request`].
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

/// Named modes an agent offers, such as `ask`, `architect` and `code`. A
/// mode changes how the agent works, often what it may do without asking.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Modes {
    /// The id of the mode the agent is in.
    pub current: String,
    pub available: Vec<Mode>,
}

/// One of an agent's [`Modes`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Mode {
    /// Pass this to [`Session::set_mode`].
    pub id: String,
    pub name: String,
    pub description: Option<String>,
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

    /// Events stored in format 1 read back to the same values and the same
    /// JSON. If this test has to change, so does [`EVENT_FORMAT`].
    #[test]
    fn format_1_events_round_trip() {
        let stored = [
            json!({ "type": "status", "status": "active" }),
            json!({ "type": "closed", "reason": { "type": "exited", "message": "boom" } }),
            json!({ "type": "turnStart", "turnId": "t" }),
            json!({ "type": "itemStart", "turnId": "t", "item": {
                "type": "toolCall", "id": "c", "status": "inProgress", "durationMs": null,
                "detail": { "kind": "command", "command": "ls", "cwd": null, "output": null, "exitCode": null }
            } }),
            json!({ "type": "itemDelta", "turnId": "t", "itemId": "m", "delta": { "type": "text", "text": "hi" } }),
            json!({ "type": "itemEnd", "turnId": "t", "item": { "type": "agentMessage", "id": "m", "text": "hi" } }),
            json!({ "type": "turnEnd", "turn": {
                "id": "t", "status": "completed", "items": [], "startedAt": 1, "completedAt": 2,
                "durationMs": 1000, "error": null
            } }),
            json!({ "type": "requestResolved", "id": "r", "outcome": {
                "type": "answered", "decision": { "type": "deny", "message": null }
            } }),
            json!({ "type": "raw", "name": "x", "payload": { "a": 1 } }),
        ];
        for json in stored {
            let event: Event = serde_json::from_value(json.clone()).unwrap();
            assert!(
                !matches!(event, Event::Unknown(_)),
                "{json} is a known event"
            );
            assert_eq!(serde_json::to_value(&event).unwrap(), json);
        }
    }

    /// A newer Stubb may add fields; they are ignored.
    #[test]
    fn newer_events_with_extra_fields_still_read() {
        let event: Event = serde_json::from_value(json!({
            "type": "turnStart", "turnId": "t", "agentId": "a", "v": 2
        }))
        .unwrap();
        assert_eq!(
            event,
            Event::TurnStart {
                turn_id: "t".into()
            }
        );

        let event: Event = serde_json::from_value(json!({
            "type": "itemEnd", "turnId": "t", "attachments": [],
            "item": { "type": "agentMessage", "id": "m", "text": "hi", "phase": "final" }
        }))
        .unwrap();
        assert_eq!(
            event,
            Event::ItemEnd {
                turn_id: "t".into(),
                item: turn::Item::AgentMessage {
                    id: "m".into(),
                    text: "hi".into()
                },
            }
        );
    }

    /// A newer Stubb may add event types, item and tool kinds and enum
    /// values; they read as `Unknown`, and unknown events keep their JSON.
    #[test]
    fn newer_event_types_and_values_read_as_unknown() {
        let future = json!({ "type": "compaction", "turnId": "t", "tokens": 12 });
        let event: Event = serde_json::from_value(future.clone()).unwrap();
        assert_eq!(event, Event::Unknown(future.clone()));
        assert_eq!(serde_json::to_value(&event).unwrap(), future);

        let event: Event =
            serde_json::from_value(json!({ "type": "status", "status": "sleeping" })).unwrap();
        assert_eq!(
            event,
            Event::Status {
                status: Status::Unknown
            }
        );

        let event: Event = serde_json::from_value(json!({
            "type": "closed", "reason": { "type": "evicted", "by": "x" }
        }))
        .unwrap();
        assert!(matches!(
            event,
            Event::Closed {
                reason: CloseReason::Unknown(_)
            }
        ));

        let item = json!({ "type": "webSearch", "id": "w", "query": "stubb" });
        let event: Event =
            serde_json::from_value(json!({ "type": "itemEnd", "turnId": "t", "item": item }))
                .unwrap();
        assert_eq!(
            event,
            Event::ItemEnd {
                turn_id: "t".into(),
                item: turn::Item::Unknown(item)
            }
        );

        let event: Event = serde_json::from_value(json!({
            "type": "itemEnd", "turnId": "t", "item": {
                "type": "toolCall", "id": "c", "status": "queued", "durationMs": null,
                "detail": { "kind": "browser", "url": "https://example.com" }
            }
        }))
        .unwrap();
        let Event::ItemEnd {
            item: turn::Item::ToolCall(call),
            ..
        } = event
        else {
            panic!("expected a tool call, got {event:?}");
        };
        assert_eq!(call.status, turn::ToolStatus::Unknown);
        assert!(matches!(call.detail, turn::ToolDetail::Unknown(_)));

        let event: Event = serde_json::from_value(json!({
            "type": "itemDelta", "turnId": "t", "itemId": "i", "delta": { "type": "image", "data": "" }
        }))
        .unwrap();
        assert!(matches!(
            event,
            Event::ItemDelta {
                delta: turn::Delta::Unknown(_),
                ..
            }
        ));

        let event: Event = serde_json::from_value(json!({
            "type": "turnEnd", "turn": { "id": "t", "status": "paused", "items": [] }
        }))
        .unwrap();
        let Event::TurnEnd { turn } = event else {
            panic!("expected a turn end, got {event:?}");
        };
        assert_eq!(turn.status, turn::Status::Unknown);

        let event: Event = serde_json::from_value(json!({
            "type": "request", "request": {
                "id": "r", "turnId": null, "native": {}, "kind": { "type": "question", "text": "?" }
            }
        }))
        .unwrap();
        let Event::Request { request } = event else {
            panic!("expected a request, got {event:?}");
        };
        assert!(matches!(request.kind, request::Kind::Unknown(_)));
    }
}
