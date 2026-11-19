use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Status;
use crate::{request, turn};

/// The version of the JSON format of [`Event`] and the types it contains
/// ([`turn::Turn`], [`turn::Item`], [`request::Request`], …).
///
/// Hosts that store events should store this number next to each event and
/// check it when reading them back:
///
/// - Within one format, Stubb only adds: new event types, new item or tool
///   kinds, new enum values and new fields. Readers ignore unknown fields,
///   and Stubb's own types read unknown types and values as their `Unknown`
///   variant ([`Event::Unknown`], [`turn::Item::Unknown`], …), so events a
///   newer Stubb wrote still deserialize.
/// - The format changes, with a new minor version before 1.0, when a field
///   or type is removed or renamed, or its meaning or JSON type changes.
///   Events with a newer format than the host knows should be kept as they
///   are and not interpreted.
///
/// See `docs/compatibility.md` in the repository for the full rules.
pub const EVENT_FORMAT: u32 = 1;

/// Events for a live session, delivered to every subscriber.
///
/// Serialized as JSON in format [`EVENT_FORMAT`]. Deserializing is tolerant:
/// unknown fields are ignored and unknown event types become
/// [`Event::Unknown`].
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
#[non_exhaustive]
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
    /// The agent is waiting for an answer; reply with [`Session::respond`](super::Session::respond).
    /// Only sent when the session forwards requests.
    Request { request: request::Request },
    /// A forwarded request is no longer pending.
    RequestResolved {
        id: String,
        outcome: request::Outcome,
    },
    /// The agent asked for something Stubb did not forward, such as an
    /// approval in a session without [`Settings::forward_requests`](super::Settings::forward_requests), so Stubb
    /// declined it.
    RequestDeclined {
        method: String,
        turn_id: Option<String>,
        summary: Option<String>,
    },
    /// Provider traffic Stubb does not map yet, passed through unchanged.
    Raw { name: String, payload: Value },
    /// An event this version of Stubb does not know, kept as it was read:
    /// a newer Stubb's event type, or a known type in a shape this version
    /// cannot read. Stubb never emits it; it only appears when deserializing
    /// stored events. Serializes back to the same JSON.
    #[serde(untagged)]
    Unknown(Value),
}

/// Why a live session ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[non_exhaustive]
pub enum CloseReason {
    /// [`super::Session::close`] was called.
    Client,
    /// The agent unloaded the session.
    Agent,
    /// The agent process exited unexpectedly.
    Exited { message: String },
    /// A reason this version of Stubb does not know, kept as it was read
    /// (deserialization only).
    #[serde(untagged)]
    Unknown(Value),
}
