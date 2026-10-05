//! Keel middleware for agent harness integrations.
//!
//! Keel drives coding agent harnesses through their native protocols and
//! exposes them through one provider-neutral interface, [`Provider`].
//!
//! Types are grouped by concept and meant to be used through their module:
//! [`catalog`] (models and their settings), [`session`] (conversations) and
//! [`turn`] (what happened in a conversation).

pub mod catalog;
mod error;
pub mod provider;
pub mod session;
pub mod turn;

pub use catalog::Catalog;
pub use error::{Error, Result};
pub use provider::{BoxFuture, Provider};
pub use session::Session;
