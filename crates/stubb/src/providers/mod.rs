//! The agents Stubb integrates. Each implements [`crate::Provider`] and is
//! compiled only when its Cargo feature (`claude`, `codex`, `pi`, `acp`) is
//! enabled; all four are enabled by default.

#[cfg(feature = "acp")]
pub mod acp;
#[cfg(feature = "claude")]
pub mod claude;
#[cfg(feature = "codex")]
pub mod codex;
#[cfg(feature = "pi")]
pub mod pi;
