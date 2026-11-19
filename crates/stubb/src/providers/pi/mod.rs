//! Native Pi coding agent integration.
//!
//! Stubb runs the installed `pi` executable in RPC mode (`pi --mode rpc`),
//! Pi's documented protocol for embedding it in other programs. Each live
//! session is its own `pi` process. Stored sessions are read directly from
//! Pi's session files. All mapping onto Stubb's types happens here.
//!
//! Pi reaches models through the providers configured in its own settings,
//! so a Stubb model id is `<provider>/<model id>`, for example
//! `anthropic/claude-sonnet-4-5`. Reasoning effort is Pi's thinking level.
//!
//! Pi has no approval step: its tools run without asking, so unlike Codex
//! and Claude Code, a Pi session executes commands and edits files.

mod events;
pub(crate) mod probe;
mod process;
mod tools;
pub(crate) use provider::PROVIDER_ID;

/// This provider's id, as [`crate::Provider::id`] and [`crate::discovery::Cli::id`]
/// report it. A `const`, so it can be matched on: `match cli.id() { ID => … }`.
pub const ID: &str = PROVIDER_ID;

mod cli;
pub use cli::CLI;
mod driver;
mod provider;
mod transcripts;
use transcripts::{fork, history, store};

pub use provider::{Pi, PiConfig};
