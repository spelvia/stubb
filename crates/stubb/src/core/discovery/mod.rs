//! Finding, identifying, and updating the agents installed on this computer.
//!
//! [`probe`] finds a built-in agent's executable, reads its version and asks
//! it whether it is signed in, using only cheap status commands: it never
//! starts a session or sends a prompt, so probing costs nothing.
//!
//! ```no_run
//! # #[cfg(feature = "codex")]
//! # async fn demo() {
//! let report = stubb::discovery::probe(stubb::providers::codex::CLI, None).await;
//! println!("{:?}", report.auth);
//! # }
//! ```
//!
//! [`profile`] describes an agent for hosts to show, and [`update`] updates
//! one with its own updater. ACP agents are found through
//! `stubb::providers::acp::registry` and probed with
//! `stubb::providers::acp::probe`.

mod cli;
pub(crate) mod executable;
mod probe;
pub mod profile;
pub(crate) mod status;
pub mod update;

pub use cli::{Cli, cli, clis};
pub use probe::{Auth, Report, probe, probe_all};
