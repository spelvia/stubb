//! Observing what an agent does across a session: its [`agent`] tree,
//! native [`task`]s and [`control`] receipts, and the session-wide record
//! stream.
//!
//! Session-wide observation contracts, separate from the legacy root event stream.
//!
//! These types describe a negotiated harness surface, not Stubb orchestration.
//! Adapters must explicitly implement observation; empty results cannot stand in
//! for unsupported discovery. Check effective capabilities:
//! native support and adapter coverage vary by feature.

pub mod agent;
mod capabilities;
pub mod control;
mod records;
pub mod task;

pub use records::*;

#[cfg(test)]
mod tests;
