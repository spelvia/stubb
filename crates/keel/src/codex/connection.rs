//! A long-lived `codex app-server` process shared by every request and
//! session of one [`Codex`](super::Codex) provider. Sessions are routed by
//! Codex thread id.

use std::collections::HashMap;
use std::ops::Deref;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;
use tokio::process::Command;

use super::events;
use super::observation::Observation;
use super::types::{ClientInfo, InitializeCapabilities, InitializeParams, InitializeResponse};
use crate::rpc::{self, Handler};
use crate::{Result, session};

pub(crate) struct AppServer {
    rpc: rpc::Connection,
    user_agent: Option<String>,
    observations: Mutex<HashMap<String, Arc<Observation>>>,
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
                    // Descendant discovery and background-terminal APIs require
                    // this opt-in. It exposes protocol methods, not tool permissions.
                    capabilities: InitializeCapabilities {
                        experimental_api: true,
                    },
                },
                request_timeout,
            )
            .await?;
        rpc.notify("initialized").await?;
        Ok(Self {
            rpc,
            user_agent: init.user_agent,
            observations: Mutex::new(HashMap::new()),
        })
    }

    pub(crate) fn user_agent(&self) -> Option<&str> {
        self.user_agent.as_deref()
    }

    pub(super) fn observe(&self, info: &session::Info) -> Arc<Observation> {
        let mut observations = self.observations.lock().expect("observations lock");
        if observations
            .get(&info.summary.id)
            .is_some_and(|state| state.is_closed())
        {
            observations.remove(&info.summary.id);
        }
        let observation = Arc::clone(observations.entry(info.summary.id.clone()).or_insert_with(
            || {
                let observation = Arc::new(Observation::new(info));
                self.rpc
                    .register_session(&info.summary.id, observation.clone());
                observation
            },
        ));
        observation.refresh_metadata(info);
        observation
    }

    pub(super) fn detach(&self, session_id: &str, expected: &Arc<Observation>) -> bool {
        let mut observations = self.observations.lock().expect("observations lock");
        if !observations
            .get(session_id)
            .is_some_and(|state| Arc::ptr_eq(state, expected))
        {
            return false;
        }
        observations.remove(session_id);
        self.rpc.unregister_session(session_id);
        true
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
