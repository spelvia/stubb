//! One `pi --mode rpc` process.
//!
//! Pi's RPC protocol is newline-delimited JSON on stdin/stdout. Keel writes
//! commands with an `id`; Pi answers each with a `response` record carrying
//! the same id, and streams session events without one. Extensions can ask
//! for user input through `extension_ui_request` dialogs, which Keel
//! declines.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{broadcast, oneshot};
use tokio::task::JoinHandle;

use super::events::SessionState;
use crate::{Error, Result, session};

const STDERR_TAIL_BYTES: usize = 8 * 1024;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
const EVENT_BUFFER: usize = 1024;
/// Extension UI methods that wait for an answer.
const DIALOGS: [&str; 4] = ["select", "confirm", "input", "editor"];

/// Which session the process works on.
pub(super) enum Conversation<'a> {
    /// No session (used to read the model catalog).
    None,
    New {
        id: &'a str,
    },
    Resume {
        path: &'a Path,
    },
}

pub(super) struct Launch<'a> {
    pub executable: &'a Path,
    pub agent_dir: Option<&'a Path>,
    pub cwd: &'a Path,
    pub conversation: Conversation<'a>,
}

pub(super) struct PiProcess {
    shared: Arc<Shared>,
    child: tokio::sync::Mutex<Child>,
    tasks: Vec<JoinHandle<()>>,
}

struct Shared {
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    pending: Mutex<HashMap<String, oneshot::Sender<std::result::Result<Value, String>>>>,
    next_id: AtomicU64,
    state: Mutex<SessionState>,
    /// Told the turn id once Pi echoes the user message of a sent prompt.
    prompted: Mutex<Option<oneshot::Sender<String>>>,
    events: broadcast::Sender<session::Event>,
    /// Set once the process is gone; holds the reason reported to callers.
    closed: Mutex<Option<String>>,
    /// Keel asked the process to stop, so its exit is not a disconnect.
    closing: AtomicBool,
    stderr_tail: Mutex<String>,
}

impl PiProcess {
    pub(super) fn spawn(launch: &Launch<'_>) -> Result<Self> {
        let mut command = Command::new(launch.executable);
        command
            .args(["--mode", "rpc"])
            .current_dir(launch.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(agent_dir) = launch.agent_dir {
            command.env("PI_CODING_AGENT_DIR", agent_dir);
        }
        match launch.conversation {
            Conversation::None => {
                command.arg("--no-session");
            }
            Conversation::New { id } => {
                command.args(["--session-id", id]);
            }
            Conversation::Resume { path } => {
                command.arg("--session").arg(path);
            }
        }

        let mut child = command.spawn().map_err(|source| {
            let executable = PathBuf::from(launch.executable);
            if source.kind() == std::io::ErrorKind::NotFound {
                Error::ExecutableNotFound { executable }
            } else {
                Error::SpawnFailed { executable, source }
            }
        })?;
        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");

        let shared = Arc::new(Shared {
            stdin: tokio::sync::Mutex::new(Some(stdin)),
            pending: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            state: Mutex::new(SessionState::default()),
            prompted: Mutex::new(None),
            events: broadcast::channel(EVENT_BUFFER).0,
            closed: Mutex::new(None),
            closing: AtomicBool::new(false),
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

    pub(super) fn is_alive(&self) -> bool {
        self.shared.closed.lock().expect("closed lock").is_none()
    }

    pub(super) fn subscribe(&self) -> broadcast::Receiver<session::Event> {
        self.shared.events.subscribe()
    }

    pub(super) fn set_context_window(&self, context_window: Option<i64>) {
        self.shared
            .state
            .lock()
            .expect("state lock")
            .set_context_window(context_window);
    }

    /// Sends a command, such as `{"type": "get_state"}`, and waits for the
    /// `data` of its response.
    pub(super) async fn command(&self, mut command: Value, timeout: Duration) -> Result<Value> {
        let method = command
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("command")
            .to_owned();
        if let Some(reason) = self.shared.closed_reason() {
            return Err(Error::ProcessExited { stderr: reason });
        }
        let id = format!(
            "keel_{}",
            self.shared.next_id.fetch_add(1, Ordering::Relaxed)
        );
        command["id"] = Value::String(id.clone());
        let (reply_tx, reply_rx) = oneshot::channel();
        self.shared
            .pending
            .lock()
            .expect("pending lock")
            .insert(id.clone(), reply_tx);
        if let Err(error) = self.shared.write(&command).await {
            self.shared
                .pending
                .lock()
                .expect("pending lock")
                .remove(&id);
            return Err(error);
        }
        match tokio::time::timeout(timeout, reply_rx).await {
            Ok(Ok(Ok(data))) => Ok(data),
            Ok(Ok(Err(message))) => Err(Error::Rpc {
                method,
                code: 0,
                message,
            }),
            Ok(Err(_)) => Err(Error::ProcessExited {
                stderr: self.shared.closed_reason().unwrap_or_default(),
            }),
            Err(_) => {
                self.shared
                    .pending
                    .lock()
                    .expect("pending lock")
                    .remove(&id);
                Err(Error::Timeout(timeout))
            }
        }
    }

    /// Starts a turn by sending a prompt, and returns the turn id once Pi
    /// has echoed the prompt's user message, before any reply.
    pub(super) async fn send_user(&self, text: &str, timeout: Duration) -> Result<String> {
        if !self.shared.state.lock().expect("state lock").prompt_sent() {
            return Err(Error::Rpc {
                method: "prompt".to_owned(),
                code: 0,
                message: "a turn is already running".to_owned(),
            });
        }
        let (started_tx, started_rx) = oneshot::channel();
        *self.shared.prompted.lock().expect("prompted lock") = Some(started_tx);
        let sent = self
            .command(json!({ "type": "prompt", "message": text }), timeout)
            .await;
        let handled = match &sent {
            Ok(data) => data.get("disposition").and_then(Value::as_str) == Some("handled"),
            Err(_) => false,
        };
        if sent.is_err() || handled {
            self.shared.prompted.lock().expect("prompted lock").take();
            let mut state = self.shared.state.lock().expect("state lock");
            let Err(error) = sent else {
                let turn_id = uuid::Uuid::new_v4().to_string();
                let events = state.prompt_handled(&turn_id, text);
                drop(state);
                self.shared.emit(events);
                return Ok(turn_id);
            };
            state.prompt_failed();
            return Err(error);
        }
        match tokio::time::timeout(timeout, started_rx).await {
            Ok(Ok(turn_id)) => Ok(turn_id),
            Ok(Err(_)) => Err(Error::ProcessExited {
                stderr: self.shared.closed_reason().unwrap_or_default(),
            }),
            Err(_) => Err(Error::Timeout(timeout)),
        }
    }

    /// Closes stdin and waits briefly for a clean exit before killing.
    /// Subscribers learn that the client closed the session.
    pub(super) async fn close(&self) {
        // An exited process already announced `Closed { Exited }`.
        if !self.shared.closing.swap(true, Ordering::Relaxed) && self.is_alive() {
            self.shared.emit(vec![session::Event::Closed {
                reason: session::CloseReason::Client,
            }]);
        }
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

impl Drop for PiProcess {
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

    fn emit(&self, events: Vec<session::Event>) {
        for event in events {
            // No subscribers is fine; the event is simply dropped.
            let _ = self.events.send(event);
        }
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
        let Ok(record) = serde_json::from_str::<Value>(line) else {
            return;
        };
        match record.get("type").and_then(Value::as_str) {
            Some("response") => self.complete(&record),
            Some("extension_ui_request") => self.extension_ui(&record).await,
            _ => {
                let (events, prompted) = {
                    let mut state = self.state.lock().expect("state lock");
                    (state.event(&record), state.take_prompted())
                };
                self.emit(events);
                if let Some(turn_id) = prompted
                    && let Some(started_tx) = self.prompted.lock().expect("prompted lock").take()
                {
                    let _ = started_tx.send(turn_id);
                }
            }
        }
    }

    fn complete(&self, response: &Value) {
        let Some(id) = response.get("id").and_then(Value::as_str) else {
            return;
        };
        let Some(reply_tx) = self.pending.lock().expect("pending lock").remove(id) else {
            return;
        };
        let reply = if response.get("success").and_then(Value::as_bool) == Some(true) {
            Ok(response.get("data").cloned().unwrap_or(Value::Null))
        } else {
            Err(response
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("command failed")
                .to_owned())
        };
        let _ = reply_tx.send(reply);
    }

    /// Dialogs are cancelled, since Keel cannot ask the user yet;
    /// notifications and status updates pass through as raw events.
    async fn extension_ui(&self, request: &Value) {
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");
        if !DIALOGS.contains(&method) {
            self.emit(vec![session::Event::Raw {
                name: format!("pi/extension_ui/{method}"),
                payload: request.clone(),
            }]);
            return;
        }
        let events = self
            .state
            .lock()
            .expect("state lock")
            .dialog_declined(method, request.get("title").and_then(Value::as_str));
        self.emit(events);
        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let _ = self
            .write(&json!({ "type": "extension_ui_response", "id": id, "cancelled": true }))
            .await;
    }

    /// Marks the process gone and fails pending commands. An exit Keel did
    /// not ask for is reported to subscribers as a disconnect.
    fn close(&self) {
        let stderr = self
            .stderr_tail
            .lock()
            .expect("stderr lock")
            .trim()
            .to_owned();
        let reason = if stderr.is_empty() {
            "pi exited".to_owned()
        } else {
            format!("pi exited: {stderr}")
        };
        *self.closed.lock().expect("closed lock") = Some(reason.clone());
        self.pending.lock().expect("pending lock").clear();
        if !self.closing.load(Ordering::Relaxed) {
            self.emit(vec![session::Event::Closed {
                reason: session::CloseReason::Exited { message: reason },
            }]);
        }
    }
}

async fn read_loop(shared: Arc<Shared>, stdout: ChildStdout) {
    // Records are split on LF only: Unicode line separators are valid inside
    // Pi's JSON strings.
    let mut lines = BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        shared.dispatch(&line).await;
    }
    // Give the stderr reader a moment to capture the exit message.
    tokio::time::sleep(Duration::from_millis(100)).await;
    shared.close();
}
