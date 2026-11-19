//! Native Codex integration through `codex app-server`.
//!
//! A [`Codex`] provider owns at most one app-server process: one execution
//! context. The process starts on first use and is shared by catalog
//! requests and every session; if it exits, the next call starts a new one
//! and live sessions receive [`session::Event::Closed`](crate::session::Event::Closed). Once nothing has
//! used it for [`CodexConfig::idle_timeout`], with no live session and no
//! request waiting, it is closed, and the next call starts it again.

mod agents;
mod events;
#[cfg(test)]
mod fixtures;
mod history;
mod live;
mod permissions;
pub(crate) mod probe;
pub use permissions::{
    ApprovalPolicy, EffectivePermissions, GranularApproval, Permissions, Sandbox, SandboxPolicy,
};
#[cfg(all(test, unix))]
mod transport_tests;
mod usage;
pub(crate) use provider::PROVIDER_ID;

mod cli;
pub use cli::CLI;
mod driver;
mod provider;
mod transport;
use transport::{connection, types};

pub use provider::{Codex, CodexConfig};
