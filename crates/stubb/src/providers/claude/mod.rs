//! Native Claude Code integration.
//!
//! Stubb runs the installed `claude` executable in stream-json mode, the same
//! protocol the official Claude Agent SDKs use, so nothing besides Claude
//! Code is needed on the machine. Each live session is its own `claude`
//! process. Stored sessions are read directly from Claude Code's transcript
//! files. All mapping onto Stubb's types happens here.

mod agents;
mod events;
#[cfg(test)]
mod fixtures;
mod live;
mod output;
mod permissions;
pub(crate) mod probe;
mod process;
mod stored_tasks;
mod tools;
mod usage;
pub use permissions::{PermissionMode, Permissions};
pub(crate) use provider::PROVIDER_ID;
mod driver;
mod provider;
mod transcripts;
use transcripts::{fork, history, store};

pub use provider::{Claude, ClaudeConfig};
