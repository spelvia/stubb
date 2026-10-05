//! A long-lived `codex app-server` process shared by every request and
//! session of one [`Codex`](super::Codex) provider. Sessions are routed by
//! Codex thread id.

use std::ops::Deref;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::process::Command;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::events;
use super::live::{Attachment, Router, tasks};
use super::types::{ClientInfo, InitializeCapabilities, InitializeParams, InitializeResponse};
use crate::rpc::{self, Handler};
use crate::{Result, session};

pub(crate) struct AppServer {
    rpc: Arc<rpc::Connection>,
    user_agent: Option<String>,
    routing: Arc<Router>,
    metadata_task: JoinHandle<()>,
    terminal_task: JoinHandle<()>,
}

impl AppServer {
    /// Spawns `codex app-server` and completes the initialize handshake.
    pub(crate) async fn start(executable: &Path, request_timeout: Duration) -> Result<Self> {
        let mut command = Command::new(executable);
        command.arg("app-server");
        let (metadata_tx, mut metadata_rx) = mpsc::channel::<String>(128);
        let routing = Arc::new(Router::new(metadata_tx));
        let rpc = Arc::new(rpc::Connection::spawn(
            command,
            "codex app-server",
            Arc::new(CodexHandler(Arc::clone(&routing))),
        )?);
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
        let metadata_task = tokio::spawn({
            let rpc = Arc::clone(&rpc);
            let routing = Arc::clone(&routing);
            async move {
                while let Some(id) = metadata_rx.recv().await {
                    match rpc
                        .request::<_, super::types::ThreadReadResponse>(
                            "thread/read",
                            &super::types::ThreadIdParams { thread_id: &id },
                            request_timeout,
                        )
                        .await
                    {
                        Ok(response) => routing.metadata(response.thread),
                        Err(error) => routing.metadata_failed(&id, &error.to_string()),
                    }
                }
            }
        });
        let terminal_task = tokio::spawn({
            let rpc = Arc::clone(&rpc);
            let routing = Arc::clone(&routing);
            async move {
                let mut interval = tokio::time::interval(Duration::from_secs(1));
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let mut failed = std::collections::HashSet::new();
                loop {
                    interval.tick().await;
                    for (id, attachment) in routing.terminal_targets() {
                        match terminal_list(&rpc, &id, request_timeout).await {
                            Ok(terminals) => {
                                failed.remove(&id);
                                attachment.terminals(&id, terminals);
                            }
                            Err(message) => {
                                if failed.insert(id.clone()) {
                                    attachment.terminal_failure(&id, &message.to_string());
                                }
                            }
                        }
                    }
                }
            }
        });
        Ok(Self {
            rpc,
            user_agent: init.user_agent,
            routing,
            metadata_task,
            terminal_task,
        })
    }

    pub(crate) fn user_agent(&self) -> Option<&str> {
        self.user_agent.as_deref()
    }

    pub(super) fn stored_agent(&self, thread: super::types::Thread) {
        self.routing.metadata(thread);
    }
    pub(super) fn discovery_failed(&self, id: &str, message: &str) {
        self.routing.metadata_failed(id, message);
    }

    pub(super) fn observe(&self, info: &session::Info) -> Arc<Attachment> {
        let attachment = self.routing.observe(info);
        self.rpc
            .register_session(&info.summary.id, attachment.clone());
        attachment
    }

    pub(super) fn detach(&self, session_id: &str, expected: &Arc<Attachment>) -> bool {
        if !self.routing.detach(session_id, expected) {
            return false;
        }
        self.rpc.unregister_session(session_id);
        true
    }
}

pub(super) async fn terminal_list(
    rpc: &rpc::Connection,
    id: &str,
    timeout: Duration,
) -> Result<Vec<tasks::Terminal>> {
    let mut cursor = None;
    let mut seen = std::collections::HashSet::new();
    let mut terminals = vec![];
    // Never reconcile a partial pagination pass as terminal absence.
    for _ in 0..32 {
        let page: tasks::Page = rpc
            .request(
                "thread/backgroundTerminals/list",
                &tasks::ListParams {
                    thread_id: id,
                    cursor: cursor.clone(),
                    limit: 100,
                },
                timeout.min(Duration::from_secs(5)),
            )
            .await?;
        terminals.extend(page.data);
        match page.next_cursor {
            None => return Ok(terminals),
            Some(next) if seen.insert(next.clone()) => cursor = Some(next),
            Some(_) => {
                return Err(crate::Error::InvalidSetting {
                    setting: "terminal pagination",
                    value: id.into(),
                    reason: "Repeated cursor".into(),
                });
            }
        }
    }
    Err(crate::Error::InvalidSetting {
        setting: "terminal pagination",
        value: id.into(),
        reason: "Exceeded 32 pages".into(),
    })
}

impl Deref for AppServer {
    type Target = rpc::Connection;

    fn deref(&self) -> &rpc::Connection {
        &self.rpc
    }
}

impl Drop for AppServer {
    fn drop(&mut self) {
        self.metadata_task.abort();
        self.terminal_task.abort();
    }
}

struct CodexHandler(Arc<Router>);

impl Handler for CodexHandler {
    fn route_notification(&self, method: &str, params: &Value) -> bool {
        self.0.notification(method, params);
        true
    }

    fn route_event(&self, params: &Value, event: &session::Event) -> bool {
        self.0.event(params, event.clone());
        true
    }

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
