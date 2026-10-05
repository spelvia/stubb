//! A long-lived `codex app-server` process shared by every request and
//! session of one [`Codex`](super::Codex) provider. Sessions are routed by
//! Codex thread id.

use std::ops::Deref;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::process::Command;

use super::events;
use super::types::{ClientInfo, InitializeParams, InitializeResponse};
use crate::rpc::{self, Handler};
use crate::{Result, session};

pub(crate) struct AppServer {
    rpc: rpc::Connection,
    user_agent: Option<String>,
}

impl AppServer {
    /// Spawns `codex app-server` and completes the initialize handshake.
    pub(crate) async fn start(executable: &Path, request_timeout: Duration) -> Result<Self> {
        let mut command = Command::new(executable);
        command.arg("app-server");
        let rpc = rpc::Connection::spawn(command, "codex app-server", Arc::new(CodexHandler))?;
        let init: InitializeResponse = rpc
            .request(
                "initialize",
                &InitializeParams {
                    client_info: ClientInfo {
                        name: "keel".to_owned(),
                        title: Some("Keel".to_owned()),
                        version: env!("CARGO_PKG_VERSION").to_owned(),
                    },
                },
                request_timeout,
            )
            .await?;
        rpc.notify("initialized").await?;
        Ok(Self {
            rpc,
            user_agent: init.user_agent,
        })
    }

    pub(crate) fn user_agent(&self) -> Option<&str> {
        self.user_agent.as_deref()
    }
}

impl Deref for AppServer {
    type Target = rpc::Connection;

    fn deref(&self) -> &rpc::Connection {
        &self.rpc
    }
}

struct CodexHandler;

impl Handler for CodexHandler {
    fn session_id<'a>(&self, params: &'a Value) -> Option<&'a str> {
        params
            .get("threadId")
            .or_else(|| params.get("thread").and_then(|thread| thread.get("id")))
            .and_then(Value::as_str)
    }

    fn notification(&self, method: &str, params: Value) -> Vec<session::Event> {
        vec![events::from_notification(method, params)]
    }

    fn request(&self, id: &Value, method: &str, params: &Value) -> (Value, Option<session::Event>) {
        let (reply, event) = events::decline_request(id, method, params);
        (reply, Some(event))
    }
}
