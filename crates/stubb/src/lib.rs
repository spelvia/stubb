//! Stubb middleware for agent harness integrations.
//!
//! Stubb drives coding agent harnesses through their native protocols and
//! exposes them through one provider-neutral interface, [`Provider`].
//! Codex, Claude Code and Pi are supported natively, and any agent that
//! speaks the Agent Client Protocol through [`acp`], with the agents of the
//! official ACP [`registry`].
//!
//! Types are grouped by concept and meant to be used through their module:
//! [`catalog`] (models and their settings), [`session`] (conversations) and
//! [`turn`] (what happened in a conversation).
//! Optional [`observation`] contracts describe attributed activity from
//! [`agent`] trees and native [`task`] resources. Adapters without this support
//! return [`Error::Unsupported`], rather than empty observation results.
//!
//! A typical flow: pick a [`Provider`], read its [`catalog`](Provider::catalog)
//! to offer models, [`start`](Provider::start_session) a session,
//! [`subscribe`](Session::subscribe) to its events, then
//! [`send`](Session::send) messages. The `examples` directory has runnable
//! programs for each step: `models`, `sessions`, and `chat`.

mod core;
pub mod providers;

pub use core::catalog::Catalog;
pub use core::discovery::{probe, profile, registry, update};
pub use core::error::{Error, Result};
pub use core::observation::{agent, records as observation, task};
pub use core::provider::{BoxFuture, Provider};
pub use core::runtime::process;
pub use core::session::Session;
pub use core::{catalog, control, permission, provider, request, session, turn, usage};
