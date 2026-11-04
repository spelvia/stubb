//! Questions an agent asks the host application during a turn, such as
//! permission to use a tool, and the host's answers.
//!
//! Requests are forwarded only when a session is started or resumed with
//! [`Settings::forward_requests`](crate::session::Settings::forward_requests).
//! The agent then waits: Keel reports [`Event::Request`](crate::session::Event::Request),
//! the host answers with [`Session::respond`](crate::Session::respond), and
//! Keel reports [`Event::RequestResolved`](crate::session::Event::RequestResolved).
//! Without the setting, Keel declines requests itself and reports
//! [`Event::RequestDeclined`](crate::session::Event::RequestDeclined), so a
//! host that never answers cannot stall a turn.
//!
//! A request can also be withdrawn without an answer, for example when the
//! agent cancels it after an interrupt or its process exits; it is then
//! resolved with [`Outcome::Withdrawn`].

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A question waiting for [`Session::respond`](crate::Session::respond).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    /// Pass this to [`Session::respond`](crate::Session::respond). Unique
    /// within the session while the request is pending.
    pub id: String,
    /// The turn the agent was working on, when known.
    pub turn_id: Option<String>,
    pub kind: Kind,
    /// The provider's request as received, for details Keel does not map.
    pub native: Value,
}

/// What the agent is asking for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
#[non_exhaustive]
pub enum Kind {
    /// Permission to call a tool with the given input.
    Tool {
        /// Native tool name, such as `Write` or `Bash`.
        name: String,
        /// Name to show people, when the provider reports one.
        display_name: Option<String>,
        /// Short description of this call, when the provider reports one.
        description: Option<String>,
        input: Value,
        /// The tool call's item id in the turn, when known.
        tool_call_id: Option<String>,
        /// Whether [`Decision::AllowForSession`] is available: the provider
        /// suggested a rule that allows calls like this one from now on.
        can_allow_for_session: bool,
    },
}

/// The host's answer to a [`Request`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Decision {
    /// Allow this call once.
    Allow,
    /// Allow this call and apply the provider's own suggested rule for
    /// similar calls in this session. Claude Code applies its
    /// `permission_suggestions`. Only valid when the request reports
    /// `can_allow_for_session`.
    AllowForSession,
    /// Refuse this call; the agent continues without it. The message, when
    /// given, is passed to the agent as the reason.
    Deny { message: Option<String> },
    /// Refuse this call and stop the turn.
    Cancel,
}

impl Decision {
    /// Whether the call was refused.
    pub fn is_refusal(&self) -> bool {
        matches!(self, Self::Deny { .. } | Self::Cancel)
    }
}

/// How a request stopped being pending.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Outcome {
    /// The host answered with [`Session::respond`](crate::Session::respond).
    Answered { decision: Decision },
    /// The request ended without an answer: the agent cancelled it, or the
    /// session closed.
    Withdrawn,
}
