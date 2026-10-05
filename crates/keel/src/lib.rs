//! Keel middleware for agent harness integrations.
//!
//! Keel drives coding agent harnesses through their native protocols and
//! exposes them through one provider-neutral interface, [`Provider`].
//! Codex and Claude Code are supported.
//!
//! Types are grouped by concept and meant to be used through their module:
//! [`catalog`] (models and their settings), [`session`] (conversations) and
//! [`turn`] (what happened in a conversation).
//!
//! A typical flow: pick a [`Provider`], read its [`catalog`](Provider::catalog)
//! to offer models, [`start`](Provider::start_session) a session,
//! [`subscribe`](Session::subscribe) to its events, then
//! [`send`](Session::send) messages. The `examples` directory has runnable
//! programs for each step: `models`, `sessions`, and `chat`.

pub mod catalog;
pub mod claude;
pub mod codex;
mod error;
mod files;
pub mod provider;
mod rpc;
pub mod session;
pub mod turn;

pub use catalog::Catalog;
pub use error::{Error, Result};
pub use provider::{BoxFuture, Provider};
pub use session::Session;
