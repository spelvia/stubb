//! One `claude` process in stream-json mode.
//!
//! This is the protocol the official Claude Agent SDKs speak: newline-
//! delimited JSON on stdin/stdout. Keel writes user messages and
//! `control_request`s (`initialize`, `interrupt`); Claude Code writes its
//! messages, `control_response`s, and its own `control_request`s, such as
//! `can_use_tool` permission prompts.

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

use super::live;
use crate::{Error, Result, observation, session};

const STDERR_TAIL_BYTES: usize = 8 * 1024;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
const DECLINED: &str =
    "Keel cannot approve tool use yet, so this request was declined. Continue without it.";

/// Request full descendant conversations on the existing native stream.
/// This adds observation traffic; it does not create agents or change permissions.
pub(super) fn initialize_request() -> Value {
    json!({ "subtype": "initialize", "forwardSubagentText": true })
}

/// Which conversation the process works on.
pub(super) enum Conversation<'a> {
    /// No session (used to read the model catalog).
    None,
    New {
        id: &'a str,
    },
    Resume {
        id: &'a str,
    },
}

pub(super) struct Launch<'a> {
    pub executable: &'a Path,
    pub cwd: &'a Path,
    pub conversation: Conversation<'a>,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub fast_mode: Option<bool>,
}

pub(super) struct ClaudeProcess {
    shared: Arc<Shared>,
    child: tokio::sync::Mutex<Child>,
    tasks: Vec<JoinHandle<()>>,
}

struct Shared {
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    pending: Mutex<HashMap<String, oneshot::Sender<std::result::Result<Value, String>>>>,
    next_id: AtomicU64,
    observation: live::Attachment,
    /// Set once the process is gone; holds the reason reported to callers.
    closed: Mutex<Option<String>>,
    /// Keel asked the process to stop, so its exit is not a disconnect.
    closing: AtomicBool,
    stderr_tail: Mutex<String>,
}

impl ClaudeProcess {
    pub(super) fn spawn(launch: &Launch<'_>) -> Result<Self> {
        let mut command = Command::new(launch.executable);
        command
            .args([
                "--output-format",
                "stream-json",
                "--verbose",
                "--input-format",
                "stream-json",
                "--include-partial-messages",
                "--permission-prompt-tool",
                "stdio",
            ])
            .current_dir(launch.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        match launch.conversation {
            Conversation::None => {}
            Conversation::New { id } => {
                command.arg(format!("--session-id={id}"));
            }
            Conversation::Resume { id } => {
                command.arg(format!("--resume={id}"));
            }
        }
        if let Some(model) = launch.model {
            command.args(["--model", model]);
        }
        if let Some(effort) = launch.effort {
            command.args(["--effort", effort]);
        }
        if let Some(fast_mode) = launch.fast_mode {
            command.args(["--settings", &json!({ "fastMode": fast_mode }).to_string()]);
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
            observation: live::Attachment::new(match launch.conversation {
                Conversation::None => "",
                Conversation::New { id } | Conversation::Resume { id } => id,
            }),
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
        self.shared.observation.subscribe()
    }

    pub(super) fn configure_observation(&self, info: &session::Info) {
        self.shared.observation.configure(info);
    }

    pub(super) fn subscribe_records(&self) -> broadcast::Receiver<observation::Record> {
        self.shared.observation.subscribe_records()
    }

    pub(super) fn snapshot(&self) -> observation::Snapshot {
        self.shared.observation.snapshot()
    }

    /// Sends a `control_request` and waits for its response payload.
    pub(super) async fn control(&self, request: Value, timeout: Duration) -> Result<Value> {
        let method = request
            .get("subtype")
            .and_then(Value::as_str)
            .unwrap_or("control")
            .to_owned();
        if let Some(reason) = self.shared.closed_reason() {
            return Err(Error::ProcessExited { stderr: reason });
        }
        let id = format!(
            "keel_{}",
            self.shared.next_id.fetch_add(1, Ordering::Relaxed)
        );
        let (reply_tx, reply_rx) = oneshot::channel();
        self.shared
            .pending
            .lock()
            .expect("pending lock")
            .insert(id.clone(), reply_tx);
        let message = json!({ "type": "control_request", "request_id": id, "request": request });
        if let Err(error) = self.shared.write(&message).await {
            self.shared
                .pending
                .lock()
                .expect("pending lock")
                .remove(&id);
            return Err(error);
        }
        match tokio::time::timeout(timeout, reply_rx).await {
            Ok(Ok(Ok(response))) => Ok(response),
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

    /// Starts a turn. The message uuid becomes the turn id: Claude Code
    /// stores it and echoes it back as `user_message_uuid`.
    pub(super) async fn send_user(
        &self,
        session_id: &str,
        turn_id: &str,
        text: &str,
    ) -> Result<()> {
        self.shared.observation.turn_started(turn_id, text);
        let sent = self
            .shared
            .write(&json!({
                "type": "user",
                "uuid": turn_id,
                "session_id": session_id,
                "parent_tool_use_id": null,
                "message": { "role": "user", "content": [{ "type": "text", "text": text }] },
            }))
            .await;
        if let Err(error) = &sent {
            // The turn was announced but never reached Claude Code; end it.
            self.shared
                .observation
                .turn_failed(turn_id, &error.to_string());
        }
        sent
    }

    /// Closes stdin and waits briefly for a clean exit before killing.
    /// Subscribers learn that the client closed the session.
    pub(super) async fn close(&self) {
        // An exited process already announced `Closed { Exited }`.
        if !self.shared.closing.swap(true, Ordering::Relaxed) && self.is_alive() {
            self.shared.observation.close(session::CloseReason::Client);
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

impl Drop for ClaudeProcess {
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
        match message.get("type").and_then(Value::as_str) {
            Some("control_response") => {
                self.complete(message.get("response").unwrap_or(&Value::Null))
            }
            Some("control_request") => self.answer(&message).await,
            Some("control_cancel_request" | "keep_alive") => {}
            _ => {
                self.observation.message(&message);
            }
        }
    }

    fn complete(&self, response: &Value) {
        let Some(id) = response.get("request_id").and_then(Value::as_str) else {
            return;
        };
        let Some(reply_tx) = self.pending.lock().expect("pending lock").remove(id) else {
            return;
        };
        let reply = if response.get("subtype").and_then(Value::as_str) == Some("error") {
            Err(response
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("control request failed")
                .to_owned())
        } else {
            Ok(response.get("response").cloned().unwrap_or(Value::Null))
        };
        let _ = reply_tx.send(reply);
    }

    /// Requests from Claude Code. Permission prompts are declined, since
    /// Keel cannot approve tool use yet; anything else is unsupported.
    async fn answer(&self, message: &Value) {
        let id = message.get("request_id").cloned().unwrap_or(Value::Null);
        let request = message.get("request").unwrap_or(&Value::Null);
        let subtype = request.get("subtype").and_then(Value::as_str).unwrap_or("");
        let reply = if subtype == "can_use_tool" {
            let tool = request
                .get("tool_name")
                .and_then(Value::as_str)
                .unwrap_or("");
            let tool_use_id = request.get("tool_use_id").and_then(Value::as_str);
            self.observation.tool_declined(
                tool,
                tool_use_id,
                request.get("input").unwrap_or(&Value::Null),
            );
            json!({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": id,
                    "response": { "behavior": "deny", "message": DECLINED, "toolUseID": tool_use_id },
                },
            })
        } else {
            json!({
                "type": "control_response",
                "response": {
                    "subtype": "error",
                    "request_id": id,
                    "error": format!("{subtype} is not supported by Keel"),
                },
            })
        };
        let _ = self.write(&reply).await;
    }

    /// Marks the process gone and fails pending requests. An exit Keel did
    /// not ask for is reported to subscribers as a disconnect.
    fn close(&self) {
        let stderr = self
            .stderr_tail
            .lock()
            .expect("stderr lock")
            .trim()
            .to_owned();
        let reason = if stderr.is_empty() {
            "claude exited".to_owned()
        } else {
            format!("claude exited: {stderr}")
        };
        *self.closed.lock().expect("closed lock") = Some(reason.clone());
        self.pending.lock().expect("pending lock").clear();
        if !self.closing.load(Ordering::Relaxed) {
            self.observation
                .close(session::CloseReason::Exited { message: reason });
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
