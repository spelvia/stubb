//! The interface every harness integration implements.

use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};

use crate::catalog::Catalog;
use crate::session::{self, Session};
use crate::{Error, Result, agent, turn};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A harness Stubb can drive, such as Codex.
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
    /// Stubb provider identifier, for example `codex`.
    fn id(&self) -> &'static str;

    /// Account-wide quotas, without sending a prompt. Providers and native
    /// CLI versions without a read operation return an explicit error.
    fn account_usage(&self) -> BoxFuture<'_, Result<crate::usage::Account>> {
        Box::pin(async {
            Err(Error::Unsupported {
                operation: "account_usage",
            })
        })
    }

    /// Operations on stored sessions this provider supports, so a host can
    /// offer only what works, such as a button to fork a conversation. The
    /// default supports none of them.
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

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

    /// Native persisted tasks, without starting or resuming work. Availability
    /// and outcomes are limited to what the native transcripts retain.
    fn list_tasks<'a>(
        &'a self,
        _session_id: &'a str,
        _query: crate::task::Query,
    ) -> BoxFuture<'a, Result<crate::task::Page>> {
        Box::pin(async {
            Err(Error::Unsupported {
                operation: "list_tasks",
            })
        })
    }

    /// Read-only native task logs for stored discovery. The implementation must
    /// validate task membership in the session; consumers never supply paths.
    fn task_output<'a>(
        &'a self,
        _session_id: &'a str,
        _task_id: &'a str,
        _query: crate::task::OutputQuery,
    ) -> BoxFuture<'a, Result<crate::task::OutputPage>> {
        Box::pin(async {
            Err(Error::Unsupported {
                operation: "task_output",
            })
        })
    }

    /// Copies a stored session into a new, independent stored session with
    /// the history up to and including [`session::Fork::at_turn`], and
    /// returns the new session's metadata. The original is unchanged.
    /// Continue the fork with [`resume_session`](Provider::resume_session)
    /// and its id; the stored turns keep their ids.
    ///
    /// Fork finished turns: Codex refuses a turn that is still running,
    /// while providers that copy stored files copy what the agent has
    /// written so far.
    ///
    /// An unknown session is [`Error::SessionNotFound`]; a turn the session
    /// does not have is [`Error::TurnNotFound`]. Providers without native
    /// forking return [`Error::Unsupported`] and report
    /// [`Capabilities::fork_session`] as `false`.
    fn fork_session<'a>(
        &'a self,
        _id: &'a str,
        _fork: session::Fork,
    ) -> BoxFuture<'a, Result<session::Summary>> {
        Box::pin(async {
            Err(Error::Unsupported {
                operation: "fork_session",
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

/// Operations on stored sessions a [`Provider`] supports, from
/// [`Provider::capabilities`].
///
/// Every flag is `false` by default. Stubb adds flags as it adds
/// operations, so the struct is `#[non_exhaustive]`: a host's own provider
/// starts from `Capabilities::default()` and sets the flags it supports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Capabilities {
    /// [`Provider::fork_session`] works.
    #[serde(default)]
    pub fork_session: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A provider that implements only what the trait requires.
    struct Minimal;

    impl Provider for Minimal {
        fn id(&self) -> &'static str {
            "minimal"
        }
        fn catalog(&self) -> BoxFuture<'_, Result<Catalog>> {
            unimplemented!()
        }
        fn list_sessions(&self, _: session::Query) -> BoxFuture<'_, Result<session::Page>> {
            unimplemented!()
        }
        fn read_session<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<session::Summary>> {
            unimplemented!()
        }
        fn list_turns<'a>(
            &'a self,
            _: &'a str,
            _: turn::Query,
        ) -> BoxFuture<'a, Result<turn::Page>> {
            unimplemented!()
        }
        fn start_session(&self, _: session::Spec) -> BoxFuture<'_, Result<Session>> {
            unimplemented!()
        }
        fn resume_session<'a>(
            &'a self,
            _: &'a str,
            _: session::Settings,
        ) -> BoxFuture<'a, Result<Session>> {
            unimplemented!()
        }
        fn shutdown(&self) -> BoxFuture<'_, ()> {
            Box::pin(async {})
        }
    }

    #[tokio::test]
    async fn forking_is_unsupported_unless_a_provider_implements_it() {
        assert!(!Minimal.capabilities().fork_session);
        let result = Minimal.fork_session("s", session::Fork::default()).await;
        assert!(matches!(
            result,
            Err(Error::Unsupported {
                operation: "fork_session"
            })
        ));
    }

    #[test]
    fn capabilities_serialize_in_camel_case() {
        let capabilities = Capabilities { fork_session: true };
        assert_eq!(
            serde_json::to_value(capabilities).unwrap(),
            serde_json::json!({ "forkSession": true })
        );
        let read: Capabilities = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(read, Capabilities::default());
        let fork: session::Fork =
            serde_json::from_value(serde_json::json!({ "atTurn": "t1" })).unwrap();
        assert_eq!(fork.at_turn.as_deref(), Some("t1"));
    }
}
