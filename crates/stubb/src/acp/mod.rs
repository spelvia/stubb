//! Agents that speak the Agent Client Protocol (ACP).
//!
//! [ACP](https://agentclientprotocol.com) is an open protocol between
//! editors and coding agents: JSON-RPC over the agent's standard input and
//! output. Many agents speak it, natively or through an adapter; the
//! [`registry`](crate::registry) lists them and how to start them. One
//! [`AcpProvider`] drives one such agent, given a command that starts it
//! (a [`Launch`]).
//!
//! Each live session is its own agent process. Stubb starts it, completes
//! `initialize` (protocol version 1), and creates the session with
//! `session/new` in the session's folder, or loads a stored one with
//! `session/resume` or `session/load`, whichever the agent offers. Turns are
//! `session/prompt` requests; their `session/update` notifications become
//! Stubb's turn events, `session/cancel` interrupts them, and the agent's
//! `session/request_permission` requests follow Stubb's
//! [request flow](crate::request).
//!
//! Stubb offers the agent its file system, restricted to the session's
//! folder (and, for reading, its additional folders), but no terminals:
//! agents run commands themselves. Images are sent only to agents that
//! accept them.
//!
//! Models and efforts come from the session's configuration options of
//! category `model` and `thought_level`; an agent without them has one
//! model, `default`. Its modes (a `mode` option, or ACP session modes) are
//! available through [`Session::modes`](crate::Session::modes) and [`Session::set_mode`](crate::Session::set_mode). Reading
//! models needs a session, so [`AcpProvider::catalog`] starts the agent and
//! creates a session in the system's temporary folder.
//!
//! Stored sessions are listed through `session/list` when the agent offers
//! it. ACP has no way to read a stored session's turns without loading it,
//! so [`Provider::list_turns`](crate::Provider::list_turns) is unsupported, as are account quotas.
//!
//! ```no_run
//! # async fn demo() -> stubb::Result<()> {
//! use stubb::acp::{AcpConfig, AcpProvider, Launch};
//! use stubb::{Provider, session};
//!
//! let launch = Launch::new("npx").args(["-y", "@agentclientprotocol/claude-agent-acp"]);
//! let provider = AcpProvider::new(AcpConfig::new("claude-acp", "Claude Agent", launch));
//! let session = provider
//!     .start_session(session::Spec {
//!         cwd: "/path/to/project".into(),
//!         settings: Default::default(),
//!     })
//!     .await?;
//! # Ok(())
//! # }
//! ```

mod config;
mod live;
mod probe;
#[cfg(test)]
mod tests;
pub use probe::{AgentInfo, AuthMethod, Capabilities, Report, probe};
mod protocol;
mod provider;
use protocol::connection;

pub use crate::core::discovery::launch::Launch;
pub use provider::{AcpConfig, AcpProvider};
