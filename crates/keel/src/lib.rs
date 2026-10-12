//! Keel middleware for agent harness integrations.
//!
//! Keel drives coding agent harnesses through their native protocols and
//! exposes them through one provider-neutral interface, [`Provider`].
//! Codex, Claude Code and Pi are supported.
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

pub mod agent;
pub mod catalog;
pub mod claude;
pub mod codex;
pub mod control;
mod error;
mod files;
pub mod observation;
pub mod permission;
pub mod pi;
pub mod provider;
mod rpc;
pub mod session;
pub mod task;
pub mod turn;
pub mod usage;

pub use catalog::Catalog;
pub use error::{Error, Result};
pub use provider::{BoxFuture, Provider};
pub use session::Session;
