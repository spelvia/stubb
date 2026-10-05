//! The interface every harness integration implements.

use std::future::Future;
use std::pin::Pin;

use crate::catalog::Catalog;
use crate::session::{self, Session};
use crate::{Error, Result, agent, turn};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A harness Keel can drive, such as Codex.
///
/// Stored sessions can be listed and read without loading them. Starting or
/// resuming a session loads it into the harness and returns a live
/// [`Session`].
///
/// Every provider behaves the same way where these docs say so, so a host
/// can treat providers alike; [`session::Driver`] describes the live half.
/// The `contract` integration test checks this against the installed
/// agents.
pub trait Provider: Send + Sync {
    /// Keel provider identifier, for example `codex`.
    fn id(&self) -> &'static str;

    /// Selectable models. At most one is the default, and a model's
    /// default effort is one of its efforts.
    fn catalog(&self) -> BoxFuture<'_, Result<Catalog>>;

    /// Stored sessions, most recently updated first. With a `cwd`, only
    /// sessions whose working directory is exactly that path.
    fn list_sessions(&self, query: session::Query) -> BoxFuture<'_, Result<session::Page>>;

    /// A stored session's metadata. An unknown id is
    /// [`Error::SessionNotFound`].
    fn read_session<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<session::Summary>>;

    /// Stored turns of a session, oldest first. A turn has the same id it
    /// had live, and starts with its user message. An unknown id is
    /// [`Error::SessionNotFound`].
    fn list_turns<'a>(
        &'a self,
        id: &'a str,
        query: turn::Query,
    ) -> BoxFuture<'a, Result<turn::Page>>;

    /// Stored agents belonging to this root session, without loading or
    /// restarting their work. Query filters are mutually exclusive. Unknown
    /// roots return `SessionNotFound`; adapters without support return
    /// `Unsupported`, not an empty page.
    fn list_agents<'a>(
        &'a self,
        _session_id: &'a str,
        query: agent::Query,
    ) -> BoxFuture<'a, Result<agent::Page>> {
        Box::pin(async move {
            query.validate()?;
            Err(Error::Unsupported {
                operation: "list_agents",
            })
        })
    }

    /// Stored turns of a selected agent, oldest first and explicitly owned.
    /// Implementations must validate membership in the root session; unrelated
    /// or unknown agents return `AgentNotFound`. Cursors are scoped to that
    /// session/agent. This does not change root-only [`Provider::list_turns`].
    fn list_agent_turns<'a>(
        &'a self,
        _session_id: &'a str,
        _agent_id: &'a str,
        _query: turn::Query,
    ) -> BoxFuture<'a, Result<agent::TurnPage>> {
        Box::pin(async {
            Err(Error::Unsupported {
                operation: "list_agent_turns",
            })
        })
    }

    /// Starts a new session in `spec.cwd`. Settings the provider cannot
    /// honour, such as an effort the model does not offer, are
    /// [`Error::InvalidSetting`] and no
    /// session starts.
    fn start_session(&self, spec: session::Spec) -> BoxFuture<'_, Result<Session>>;

    /// Loads a stored session to continue it, in its own working directory.
    /// An unknown id is
    /// [`Error::SessionNotFound`]; settings
    /// are checked as for [`start_session`](Provider::start_session).
    fn resume_session<'a>(
        &'a self,
        id: &'a str,
        settings: session::Settings,
    ) -> BoxFuture<'a, Result<Session>>;

    /// Stops any harness processes owned by this provider.
    fn shutdown(&self) -> BoxFuture<'_, ()>;
}
