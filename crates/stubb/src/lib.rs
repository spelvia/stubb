//! Embed coding agents in your application.
//!
//! Stubb drives coding agents through their native protocols and exposes them
//! through one provider-neutral interface, [`Provider`]. A [`Session`] is a
//! conversation with an agent: [`subscribe`](Session::subscribe) to its
//! events, then [`send`](Session::send) messages.
//!
//! # Layout
//!
//! - [`providers`]: the agents Stubb integrates. Codex, Claude Code and Pi are
//!   driven natively, and any agent that speaks the Agent Client Protocol
//!   through [`providers::acp`]. Each sits behind a Cargo feature of the same
//!   name, all enabled by default.
//! - [`session`], [`turn`], [`catalog`], [`request`], [`permission`] and
//!   [`usage`]: the shared concepts every provider speaks, from models and
//!   their settings to the questions an agent asks during a turn.
//! - [`observation`]: optional contracts for attributed activity across a
//!   session, such as subagent trees and native tasks. Providers without this
//!   support return [`Error::Unsupported`] rather than empty results.
//! - [`discovery`]: finding, describing and updating the agents installed on
//!   this computer, without starting them.
//! - [`process`]: how Stubb starts agent processes and what they inherit.
//!
//! Provider-specific behavior stays visible: each provider reports its
//! [`capabilities`](Provider::capabilities), and unsupported operations
//! return [`Error::Unsupported`].
//!
//! # Example
//!
//! ```no_run
//! # #[cfg(feature = "claude")]
//! # async fn demo() -> stubb::Result<()> {
//! use stubb::providers::claude::{Claude, ClaudeConfig};
//! use stubb::{session, turn};
//!
//! let provider = Claude::new(ClaudeConfig::default());
//! let live = provider
//!     .start_session(session::Spec {
//!         cwd: std::env::current_dir()?,
//!         settings: session::Settings::default(),
//!     })
//!     .await?;
//! let mut events = live.subscribe();
//! live.send(turn::Input::text("Summarize this repository.")).await?;
//! while let Ok(event) = events.recv().await {
//!     println!("{event:?}");
//! }
//! # Ok(())
//! # }
//! ```
//!
//! The `examples` directory has runnable programs for each step: `quickstart`,
//! `models`, `sessions`, `chat`, and more.

#![cfg_attr(docsrs, feature(doc_cfg))]
// Shared helpers exist for the providers; with some compiled out, a few go
// unused. The full build (every provider) still checks for dead code.
#![cfg_attr(
    not(all(feature = "claude", feature = "codex", feature = "pi", feature = "acp")),
    allow(dead_code)
)]

mod core;
pub mod providers;

pub use core::catalog::Catalog;
pub use core::discovery;
pub use core::error::{Error, Result};
pub use core::observation;
pub use core::provider::{BoxFuture, Provider};
pub use core::runtime::process;
pub use core::session::Session;
pub use core::{catalog, permission, provider, request, session, turn, usage};
