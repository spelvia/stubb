//! The interface every harness integration implements.

use std::future::Future;
use std::pin::Pin;

use crate::catalog::Catalog;
use crate::session::{self, Session};
use crate::{Result, turn};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A harness Keel can drive, such as Codex.
///
/// Stored sessions can be listed and read without loading them. Starting or
/// resuming a session loads it into the harness and returns a live
/// [`Session`].
pub trait Provider: Send + Sync {
    /// Keel provider identifier, for example `codex`.
    fn id(&self) -> &'static str;

    fn catalog(&self) -> BoxFuture<'_, Result<Catalog>>;

    fn list_sessions(&self, query: session::Query) -> BoxFuture<'_, Result<session::Page>>;

    fn read_session<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<session::Summary>>;

    /// Stored turns of a session, oldest first.
    fn list_turns<'a>(
        &'a self,
        id: &'a str,
        query: turn::Query,
    ) -> BoxFuture<'a, Result<turn::Page>>;

    fn start_session(&self, spec: session::Spec) -> BoxFuture<'_, Result<Session>>;

    fn resume_session<'a>(
        &'a self,
        id: &'a str,
        settings: session::Settings,
    ) -> BoxFuture<'a, Result<Session>>;

    /// Stops any harness processes owned by this provider.
    fn shutdown(&self) -> BoxFuture<'_, ()>;
}
