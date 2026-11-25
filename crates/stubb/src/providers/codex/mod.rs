//! Native Codex integration through `codex app-server`.
//!
//! A [`Codex`] provider owns at most one app-server process: one execution
//! context. The process starts on first use and is shared by catalog
//! requests and every session; if it exits, the next call starts a new one
//! and live sessions receive [`session::Event::Closed`](crate::session::Event::Closed). Once nothing has
//! used it for [`CodexConfig::idle_timeout`], with no live session and no
//! request waiting, it is closed, and the next call starts it again.
//!
//! Commands Codex keeps running in the background are a live session's
//! tasks. [`Session::task_output`](crate::Session::task_output) reads what
//! such a command printed, from the output Codex streamed to this
//! attachment; the last MiB of each task is kept. Stored sessions list, as
//! tasks, the commands that completed after their turn, read from the
//! session's rollout files with
//! [`Provider::list_tasks`](crate::Provider::list_tasks) and
//! [`Provider::task_output`](crate::Provider::task_output).

mod agents;
mod events;
#[cfg(test)]
mod fixtures;
mod history;
mod live;
mod output;
mod permissions;
pub(crate) mod probe;
mod requests;
pub use permissions::{
    ApprovalPolicy, EffectivePermissions, GranularApproval, Permissions, Sandbox, SandboxPolicy,
};
#[cfg(all(test, unix))]
mod transport_tests;
mod usage;
pub(crate) use provider::PROVIDER_ID;

/// This provider's id, as [`crate::Provider::id`] and [`crate::discovery::Cli::id`]
/// report it. A `const`, so it can be matched on: `match cli.id() { ID => … }`.
pub const ID: &str = PROVIDER_ID;

mod cli;
pub use cli::CLI;
mod driver;
mod provider;
mod stored_tasks;
mod transport;
use transport::{connection, types};

pub use provider::{Codex, CodexConfig};
