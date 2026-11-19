//! A supervised child process speaking newline-delimited JSON-RPC (without
//! the `jsonrpc` field) over stdio, as `codex app-server` does. It is
//! used by Codex, whose [`Handler`] interprets the app-server's messages.
//!
//! A reader task owns stdout and routes each line: responses complete the
//! matching pending request; notifications and requests from the child are
//! handed to the provider's [`Handler`], and resulting events go to the
//! session they name.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::session;
use crate::{Error, Result};

const STDERR_TAIL_BYTES: usize = 8 * 1024;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Synchronous delivery from the reader into the provider's state reducer.
/// Implementations serialize state updates and publication at this boundary.
pub(in crate::codex) trait EventSink: Send + Sync {
    fn emit(&self, event: session::Event);
}

/// Provider-specific interpretation of messages from the child.
pub(in crate::codex) trait Handler: Send + Sync + 'static {
    /// Provider routing/discovery before exact-session filtering. Returning true
    /// means the provider consumed or buffered this notification itself.
    fn route_notification(&self, _method: &str, _params: &Value) -> bool {
        false
    }

    fn route_event(&self, _params: &Value, _event: &session::Event) -> bool {
        false
    }
    /// The session a notification or request belongs to.
    fn session_id<'a>(&self, params: &'a Value) -> Option<&'a str>;

    /// Events for one notification about a registered session; may be none or several.
    fn notification(&self, method: &str, params: Value) -> Vec<session::Event>;

    /// Reply to a request from the child, and an optional event for its session.
    fn request(&self, id: &Value, method: &str, params: &Value) -> (Value, Option<session::Event>);

    /// Called when a session stops being routed, to drop per-session state.
    fn session_closed(&self, _session_id: &str) {}
}

pub(in crate::codex) struct Connection {
    shared: Arc<Shared>,
    child: tokio::sync::Mutex<Child>,
    tasks: Vec<JoinHandle<()>>,
}

struct Shared {
    label: &'static str,
    handler: Arc<dyn Handler>,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    pending: Mutex<HashMap<i64, oneshot::Sender<Reply>>>,
    sessions: Mutex<HashMap<String, Arc<dyn EventSink>>>,
    next_id: AtomicI64,
    /// Set once the process is gone; holds the reason reported to callers.
    closed: Mutex<Option<String>>,
    stderr_tail: Mutex<String>,
}

type Reply = std::result::Result<Value, RpcFailure>;

struct RpcFailure {
    code: i64,
    message: String,
}

impl Connection {
    /// Spawns `command` with piped stdio. `label` names the process in errors.
    pub(in crate::codex) fn spawn(
        mut command: Command,
        label: &'static str,
        handler: Arc<dyn Handler>,
    ) -> Result<Self> {
        let executable = PathBuf::from(command.as_std().get_program());
        crate::process::die_with_parent(&mut command);
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|source| {
                if source.kind() == std::io::ErrorKind::NotFound {
                    Error::ExecutableNotFound {
                        executable: executable.clone(),
                    }
                } else {
                    Error::SpawnFailed {
                        executable: executable.clone(),
                        source,
                    }
                }
            })?;

        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");

        let shared = Arc::new(Shared {
            label,
            handler,
            stdin: tokio::sync::Mutex::new(Some(stdin)),
            pending: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            next_id: AtomicI64::new(0),
            closed: Mutex::new(None),
            stderr_tail: Mutex::new(String::new()),
        });

        let stderr_task = tokio::spawn({
            let shared = Arc::clone(&shared);
            async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    shared.append_stderr(&line);
                }
            }
        });
        let reader_task = tokio::spawn(read_loop(Arc::clone(&shared), stdout));

        Ok(Self {
            shared,
            child: tokio::sync::Mutex::new(child),
            tasks: vec![reader_task, stderr_task],
        })
    }

    pub(in crate::codex) fn is_alive(&self) -> bool {
        self.shared.closed.lock().expect("closed lock").is_none()
    }

    /// Requests sent and not answered yet.
    pub(in crate::codex) fn pending_requests(&self) -> usize {
        self.shared.pending.lock().expect("pending lock").len()
    }

    pub(in crate::codex) async fn request<P, R>(
        &self,
        method: &str,
        params: &P,
        timeout: Duration,
    ) -> Result<R>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        if let Some(reason) = self.shared.closed_reason() {
            return Err(Error::ProcessExited { stderr: reason });
        }
        let params = serde_json::to_value(params).map_err(|source| Error::Decode {
            method: method.to_owned(),
            source,
        })?;
        let id = self.shared.next_id.fetch_add(1, Ordering::Relaxed);
        let (reply_tx, reply_rx) = oneshot::channel();
        self.shared
            .pending
            .lock()
            .expect("pending lock")
            .insert(id, reply_tx);

        let sent = self
            .shared
            .write(&json!({ "id": id, "method": method, "params": params }))
            .await;
        if let Err(error) = sent {
            self.shared
                .pending
                .lock()
                .expect("pending lock")
                .remove(&id);
            return Err(error);
        }

        let reply = match tokio::time::timeout(timeout, reply_rx).await {
            Ok(Ok(reply)) => reply,
            // The reader dropped the sender because the process exited.
            Ok(Err(_)) => {
                return Err(Error::ProcessExited {
                    stderr: self.shared.closed_reason().unwrap_or_default(),
                });
            }
            Err(_) => {
                self.shared
                    .pending
                    .lock()
                    .expect("pending lock")
                    .remove(&id);
                return Err(Error::Timeout(timeout));
            }
        };
        let value = reply.map_err(|failure| Error::Rpc {
            method: method.to_owned(),
            code: failure.code,
            message: failure.message,
        })?;
        serde_json::from_value(value).map_err(|source| Error::Decode {
            method: method.to_owned(),
            source,
        })
    }

    pub(in crate::codex) async fn notify(&self, method: &str) -> Result<()> {
        self.shared.write(&json!({ "method": method })).await
    }

    /// Routes events for `session_id` directly into its provider reducer.
    pub(in crate::codex) fn register_session(&self, session_id: &str, sink: Arc<dyn EventSink>) {
        self.shared
            .sessions
            .lock()
            .expect("sessions lock")
            .insert(session_id.to_owned(), sink);
    }

    pub(in crate::codex) fn unregister_session(&self, session_id: &str) {
        self.shared
            .sessions
            .lock()
            .expect("sessions lock")
            .remove(session_id);
        self.shared.handler.session_closed(session_id);
    }

    /// Closes stdin and waits briefly for a clean exit before killing.
    pub(in crate::codex) async fn shutdown(&self) {
        self.shared.stdin.lock().await.take();
        let mut child = self.child.lock().await;
        if tokio::time::timeout(SHUTDOWN_GRACE, child.wait())
            .await
            .is_err()
        {
            let _ = child.kill().await;
        }
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl Shared {
    async fn write(&self, message: &Value) -> Result<()> {
        let mut stdin = self.stdin.lock().await;
        let Some(stdin) = stdin.as_mut() else {
            return Err(Error::ProcessExited {
                stderr: self.closed_reason().unwrap_or_default(),
            });
        };
        let mut line = message.to_string().into_bytes();
        line.push(b'\n');
        stdin.write_all(&line).await?;
        stdin.flush().await?;
        Ok(())
    }

    fn closed_reason(&self) -> Option<String> {
        self.closed.lock().expect("closed lock").clone()
    }

    fn append_stderr(&self, line: &str) {
        let mut tail = self.stderr_tail.lock().expect("stderr lock");
        tail.push_str(line);
        tail.push('\n');
        if tail.len() > STDERR_TAIL_BYTES {
            let mut cut = tail.len() - STDERR_TAIL_BYTES;
            while !tail.is_char_boundary(cut) {
                cut += 1;
            }
            tail.drain(..cut);
        }
    }

    async fn dispatch(&self, line: &str) {
        let Ok(message) = serde_json::from_str::<Value>(line) else {
            return;
        };
        let method = message.get("method").and_then(Value::as_str);
        let params = || message.get("params").cloned().unwrap_or(Value::Null);
        match (message.get("id"), method) {
            (Some(id), None) => self.complete(id, &message),
            (Some(id), Some(method)) => {
                let params = params();
                let (reply, event) = self.handler.request(id, method, &params);
                let _ = self.write(&reply).await;
                if let Some(event) = event
                    && !self.handler.route_event(&params, &event)
                {
                    self.emit(&params, vec![event]);
                }
            }
            (None, Some(method)) => {
                let params = params();
                // Skip the mapping work for sessions nobody routes.
                if !self.handler.route_notification(method, &params)
                    && self.sender(&params).is_some()
                {
                    let events = self.handler.notification(method, params.clone());
                    self.emit(&params, events);
                }
            }
            (None, None) => {}
        }
    }

    fn complete(&self, id: &Value, message: &Value) {
        let Some(id) = id.as_i64() else { return };
        let Some(reply_tx) = self.pending.lock().expect("pending lock").remove(&id) else {
            return;
        };
        let reply = match message.get("error") {
            Some(error) => Err(RpcFailure {
                code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                message: error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            }),
            None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
        };
        let _ = reply_tx.send(reply);
    }

    fn sender(&self, params: &Value) -> Option<Arc<dyn EventSink>> {
        let session_id = self.handler.session_id(params)?;
        self.sessions
            .lock()
            .expect("sessions lock")
            .get(session_id)
            .cloned()
    }

    /// Sends `events` to the session named in `params`, if one is registered.
    fn emit(&self, params: &Value, events: Vec<session::Event>) {
        if let Some(sender) = self.sender(params) {
            for event in events {
                sender.emit(event);
            }
        }
    }

    /// Marks the connection dead, fails pending requests, and tells every
    /// registered session it must be resumed elsewhere.
    fn close(&self) {
        let stderr = self
            .stderr_tail
            .lock()
            .expect("stderr lock")
            .trim()
            .to_owned();
        let reason = if stderr.is_empty() {
            format!("{} exited", self.label)
        } else {
            format!("{} exited: {stderr}", self.label)
        };
        *self.closed.lock().expect("closed lock") = Some(reason.clone());
        self.pending.lock().expect("pending lock").clear();
        for (_, events) in self.sessions.lock().expect("sessions lock").drain() {
            events.emit(session::Event::Closed {
                reason: session::CloseReason::Exited {
                    message: reason.clone(),
                },
            });
        }
    }
}

async fn read_loop(shared: Arc<Shared>, stdout: ChildStdout) {
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        shared.dispatch(&line).await;
    }
    // Give the stderr reader a moment to capture the exit message.
    tokio::time::sleep(Duration::from_millis(100)).await;
    shared.close();
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Passthrough;
    impl Handler for Passthrough {
        fn session_id<'a>(&self, params: &'a Value) -> Option<&'a str> {
            params["threadId"].as_str()
        }
        fn notification(&self, method: &str, params: Value) -> Vec<session::Event> {
            vec![session::Event::Raw {
                name: method.into(),
                payload: params,
            }]
        }
        fn request(&self, id: &Value, method: &str, _: &Value) -> (Value, Option<session::Event>) {
            (
                json!({"id": id, "result": {}}),
                Some(session::Event::RequestDeclined {
                    method: method.into(),
                    turn_id: None,
                    summary: None,
                }),
            )
        }
    }

    #[derive(Default)]
    struct Captured(Mutex<Vec<session::Event>>);
    impl EventSink for Captured {
        fn emit(&self, event: session::Event) {
            self.0.lock().unwrap().push(event);
        }
    }

    #[tokio::test]
    async fn reader_dispatch_and_process_exit_reach_the_registered_sink_directly() {
        let sink = Arc::new(Captured::default());
        let shared = Shared {
            label: "fixture",
            handler: Arc::new(Passthrough),
            stdin: tokio::sync::Mutex::new(None),
            pending: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::from([(
                "root".into(),
                sink.clone() as Arc<dyn EventSink>,
            )])),
            next_id: AtomicI64::new(0),
            closed: Mutex::new(None),
            stderr_tail: Mutex::new(String::new()),
        };
        shared
            .dispatch(r#"{"method":"known","params":{"threadId":"root"}}"#)
            .await;
        shared
            .dispatch(r#"{"method":"unrouted","params":{"threadId":"child"}}"#)
            .await;
        shared
            .dispatch(r#"{"id":7,"method":"approval","params":{"threadId":"root"}}"#)
            .await;
        shared.close();
        let events = sink.0.lock().unwrap();
        assert_eq!(events.len(), 3);
        assert!(matches!(&events[0], session::Event::Raw { name, .. } if name == "known"));
        assert!(
            matches!(&events[1], session::Event::RequestDeclined { method, .. } if method == "approval")
        );
        assert!(matches!(
            &events[2],
            session::Event::Closed {
                reason: session::CloseReason::Exited { .. }
            }
        ));
        assert!(shared.sessions.lock().unwrap().is_empty());
    }
}
