use tokio::sync::broadcast;

use super::{Event, Modes};
use crate::{BoxFuture, Error, Result, observation, request, turn};

/// The provider-specific half of a [`super::Session`].
///
/// Stubb's providers implement it; so can a host that adds its own provider,
/// for example a mock for its tests. [`super::Session`] forwards to these methods,
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
    /// The agent's named modes. Adapters without modes keep the default,
    /// which reports the operation as unsupported.
    fn modes(&self) -> Result<Modes> {
        Err(Error::Unsupported { operation: "modes" })
    }
    /// Switches the agent's mode. Adapters without modes keep the default,
    /// which reports the operation as unsupported.
    fn set_mode<'a>(&'a self, _mode_id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async {
            Err(Error::Unsupported {
                operation: "set_mode",
            })
        })
    }
    /// Detaches from the session, announcing [`Event::Closed`] with
    /// [`super::CloseReason::Client`] if it was still live.
    fn close(&self) -> BoxFuture<'_, Result<()>>;
}
