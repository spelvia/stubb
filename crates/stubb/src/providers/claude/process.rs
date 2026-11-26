//! One `claude` process in stream-json mode.
//!
//! This is the protocol the official Claude Agent SDKs speak: newline-
//! delimited JSON on stdin/stdout. Stubb writes user messages and
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
use crate::process::Environment;
use crate::request::{self, Decision, Outcome};
use crate::{Error, Result, observation, session};

const STDERR_TAIL_BYTES: usize = 8 * 1024;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
const DECLINED: &str =
    "Stubb cannot approve tool use yet, so this request was declined. Continue without it.";
const DENIED: &str = "The user denied this tool call.";

/// Request full descendant conversations on the existing native stream.
/// This adds observation traffic; it does not create agents or change permissions.
pub(super) fn initialize_request() -> Value {
    json!({ "subtype": "initialize", "forwardSubagentText": true })
}

/// Which conversation the process works on.
#[derive(Clone, Copy)]
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
    pub config_dir: Option<&'a Path>,
    pub environment: &'a Environment,
    pub conversation: Conversation<'a>,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub fast_mode: Option<bool>,
    /// Forward permission prompts to the host instead of declining them.
    pub forward_requests: bool,
    /// Folders outside `cwd` it may read (`--add-dir`).
    pub additional_directories: &'a [PathBuf],
    /// `--permission-mode`; `None` keeps Claude Code's own.
    pub permission_mode: Option<super::PermissionMode>,
    /// `--thinking-display`; `None` keeps Claude Code's own.
    pub thinking_display: Option<&'a str>,
}

pub(super) struct ClaudeProcess {
    shared: Arc<Shared>,
    child: tokio::sync::Mutex<Child>,
    tasks: Vec<JoinHandle<()>>,
}

struct Shared {
    commands: Mutex<Option<Vec<Value>>>,
    stdin: tokio::sync::Mutex<Option<ChildStdin>>,
    pending: Mutex<HashMap<String, oneshot::Sender<std::result::Result<Value, String>>>>,
    next_id: AtomicU64,
    observation: live::Attachment,
    /// Set once the process is gone; holds the reason reported to callers.
    closed: Mutex<Option<String>>,
    /// Stubb asked the process to stop, so its exit is not a disconnect.
    closing: AtomicBool,
    stderr_tail: Mutex<String>,
    forward_requests: bool,
    /// Forwarded permission prompts waiting for the host, by native request id.
    requests: Mutex<HashMap<String, Pending>>,
}

/// A forwarded `can_use_tool` prompt and what its answer needs.
struct Pending {
    native_id: Value,
    tool_use_id: Option<String>,
    input: Value,
    suggestions: Option<Value>,
    /// What the host was asked, to check its answer against.
    kind: request::Kind,
}

impl ClaudeProcess {
    pub(super) fn commands(&self) -> Vec<Value> {
        self.shared
            .commands
            .lock()
            .expect("commands lock")
            .clone()
            .unwrap_or_default()
    }

    pub(super) fn initialize_commands(&self, init: &Value) {
        if let Some(commands) = init["commands"].as_array() {
            self.shared
                .commands
                .lock()
                .expect("commands lock")
                .get_or_insert_with(|| commands.clone());
        }
    }

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
        crate::process::die_with_parent(&mut command);
        launch.environment.apply(&mut command);
        if let Some(dir) = launch.config_dir {
            command.env("CLAUDE_CONFIG_DIR", dir);
        }
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
        if let Some(mode) = launch.permission_mode {
            command.args(["--permission-mode", mode.as_str()]);
        }
        if let Some(display) = launch.thinking_display {
            command.args(["--thinking-display", display]);
        }
        for directory in launch.additional_directories {
            command.arg("--add-dir").arg(directory);
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
            commands: Mutex::new(None),
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
            forward_requests: launch.forward_requests,
            requests: Mutex::new(HashMap::new()),
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

    pub(super) fn announce(&self, event: session::Event) {
        self.shared.observation.announce(event);
    }

    pub(super) fn restore_observation(
        &self,
        stored: super::agents::Stored,
        tasks: Vec<super::stored_tasks::Entry>,
    ) {
        self.shared.observation.restore(stored, tasks);
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

    pub(super) async fn task_output(
        &self,
        id: &str,
        query: crate::observation::task::OutputQuery,
    ) -> Result<crate::observation::task::OutputPage> {
        let binding = self.shared.observation.output_binding(id)?;
        let page =
            crate::core::runtime::files::blocking(move || super::output::read(binding, query))
                .await?;
        self.shared
            .observation
            .output_availability(id, page.availability);
        Ok(page)
    }

    /// Sends a `control_request` and waits for its response payload.
    pub(super) async fn control(&self, request: Value, timeout: Duration) -> Result<Value> {
        self.control_guarded(request, timeout, None).await
    }

    pub(super) async fn stop(
        &self,
        target: crate::observation::control::Target,
        timeout: Duration,
    ) -> Result<crate::observation::control::Receipt> {
        let request = self.shared.observation.begin_stop(target)?;
        self.stop_request(request, timeout).await
    }
    pub(super) async fn interrupt_turn(&self, turn: &str, timeout: Duration) -> Result<()> {
        let request = self.shared.observation.begin_root_stop(turn)?;
        let Some(native) = request.native else {
            return Ok(());
        };
        let result = self
            .control_guarded(native, timeout, Some(&request.id))
            .await
            .map(|_| ());
        self.shared.observation.finish_stop(&request.id, &result);
        result
    }
    async fn stop_request(
        &self,
        request: super::live::StopRequest,
        timeout: Duration,
    ) -> Result<crate::observation::control::Receipt> {
        let Some(native) = request.native else {
            return Ok(self.shared.observation.stop_receipt(&request.id));
        };
        let result = self
            .control_guarded(native, timeout, Some(&request.id))
            .await
            .map(|_| ());
        Ok(self.shared.observation.finish_stop(&request.id, &result))
    }

    async fn control_guarded(
        &self,
        request: Value,
        timeout: Duration,
        stop: Option<&str>,
    ) -> Result<Value> {
        let method = request
            .get("subtype")
            .and_then(Value::as_str)
            .unwrap_or("control")
            .to_owned();
        if let Some(reason) = self.shared.closed_reason() {
            return Err(Error::ProcessExited { stderr: reason });
        }
        let id = format!(
            "stubb_{}",
            self.shared.next_id.fetch_add(1, Ordering::Relaxed)
        );
        let (reply_tx, reply_rx) = oneshot::channel();
        self.shared
            .pending
            .lock()
            .expect("pending lock")
            .insert(id.clone(), reply_tx);
        let message = json!({ "type": "control_request", "request_id": id, "request": request });
        if let Err(error) = self.shared.write_guarded(&message, stop).await {
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

    /// Answers a forwarded permission prompt.
    pub(super) async fn respond(
        &self,
        session_id: &str,
        id: &str,
        decision: Decision,
    ) -> Result<()> {
        let not_found = || Error::RequestNotFound {
            session_id: session_id.to_owned(),
            id: id.to_owned(),
        };
        let response = {
            let requests = self.shared.requests.lock().expect("requests lock");
            let pending = requests.get(id).ok_or_else(not_found)?;
            permission_response(id, pending, &decision)?
        };
        let Some(pending) = self
            .shared
            .requests
            .lock()
            .expect("requests lock")
            .remove(id)
        else {
            // Withdrawn while the answer was being prepared.
            return Err(not_found());
        };
        self.shared
            .write(&json!({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": pending.native_id,
                    "response": response,
                },
            }))
            .await?;
        self.shared.observation.request_resolved(
            pending.tool_use_id.as_deref(),
            id,
            Outcome::Answered { decision },
        );
        Ok(())
    }

    /// Starts a turn. The message uuid becomes the turn id: Claude Code
    /// stores it and echoes it back as `user_message_uuid`.
    pub(super) async fn send_user(
        &self,
        session_id: &str,
        turn_id: &str,
        input: &crate::turn::Input,
    ) -> Result<()> {
        // Images go first, as Anthropic advises; each is read before the
        // turn is announced, so an unreadable one fails cleanly.
        let mut content = Vec::with_capacity(input.images.len() + 1);
        for image in &input.images {
            content.push(json!({
                "type": "image",
                "source": { "type": "base64", "media_type": image.mime_type, "data": image.base64().await? },
            }));
        }
        content.push(json!({ "type": "text", "text": input.text }));
        self.shared
            .observation
            .turn_started_with(turn_id, crate::turn::user_content(input));
        let sent = self
            .shared
            .write(&json!({
                "type": "user",
                "uuid": turn_id,
                "session_id": session_id,
                "parent_tool_use_id": null,
                "message": { "role": "user", "content": content },
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
            self.shared.withdraw_all();
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
        self.write_guarded(message, None).await
    }

    async fn write_guarded(&self, message: &Value, stop: Option<&str>) -> Result<()> {
        let mut stdin = self.stdin.lock().await;
        if stop.is_some_and(|id| !self.observation.stop_is_current(id)) {
            return Err(Error::Unsupported {
                operation: "stale_stop_generation",
            });
        }
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
            Some("control_cancel_request") => {
                if let Some(id) = message.get("request_id") {
                    self.withdraw(&native_key(id));
                }
            }
            Some("system") if message["subtype"] == "commands_changed" => {
                if let Some(commands) = message["commands"].as_array() {
                    *self.commands.lock().expect("commands lock") = Some(commands.clone());
                }
            }
            Some("keep_alive") => {}
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

    /// Requests from Claude Code. Permission prompts are forwarded to the
    /// host when the session asked for it, and declined otherwise; anything
    /// else is unsupported.
    async fn answer(&self, message: &Value) {
        let id = message.get("request_id").cloned().unwrap_or(Value::Null);
        let request = message.get("request").unwrap_or(&Value::Null);
        let subtype = request.get("subtype").and_then(Value::as_str).unwrap_or("");
        if subtype == "can_use_tool" && self.forward_requests {
            self.forward(id, request);
            return;
        }
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
                    "error": format!("{subtype} is not supported by Stubb"),
                },
            })
        };
        let _ = self.write(&reply).await;
    }

    /// Reports a permission prompt to the host and keeps it until answered.
    /// The reader moves on, so the rest of the stream keeps flowing.
    fn forward(&self, native_id: Value, request: &Value) {
        let text = |key: &str| request.get(key).and_then(Value::as_str).map(str::to_owned);
        let tool_use_id = text("tool_use_id");
        let input = request.get("input").cloned().unwrap_or(Value::Null);
        let suggestions = request
            .get("permission_suggestions")
            .filter(|s| s.as_array().is_some_and(|s| !s.is_empty()))
            .cloned();
        let id = native_key(&native_id);
        let name = text("tool_name").unwrap_or_default();
        let kind = interactive_kind(&name, &input, tool_use_id.as_deref()).unwrap_or_else(|| {
            request::Kind::Tool {
                name,
                display_name: text("display_name"),
                description: text("description"),
                input: input.clone(),
                tool_call_id: tool_use_id.clone(),
                can_allow_for_session: suggestions.is_some(),
            }
        });
        let forwarded = request::Request {
            id: id.clone(),
            turn_id: None,
            kind: kind.clone(),
            native: request.clone(),
        };
        self.requests.lock().expect("requests lock").insert(
            id,
            Pending {
                native_id,
                tool_use_id: tool_use_id.clone(),
                input,
                suggestions,
                kind,
            },
        );
        self.observation
            .requested(tool_use_id.as_deref(), forwarded);
    }

    /// A pending prompt ended without an answer.
    fn withdraw(&self, id: &str) {
        let pending = self.requests.lock().expect("requests lock").remove(id);
        if let Some(pending) = pending {
            self.observation.request_resolved(
                pending.tool_use_id.as_deref(),
                id,
                Outcome::Withdrawn,
            );
        }
    }

    fn withdraw_all(&self) {
        let ids = self
            .requests
            .lock()
            .expect("requests lock")
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for id in ids {
            self.withdraw(&id);
        }
    }

    /// Marks the process gone and fails pending requests. An exit Stubb did
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
            self.withdraw_all();
            self.observation
                .close(session::CloseReason::Exited { message: reason });
        }
    }
}

/// Request ids are strings in practice; anything else keeps its JSON text.
fn native_key(id: &Value) -> String {
    id.as_str().map_or_else(|| id.to_string(), str::to_owned)
}

/// Claude Code's own tools that ask the user something, mapped to their
/// request kinds; `None` for every other tool, or an input Stubb cannot read.
fn interactive_kind(name: &str, input: &Value, tool_use_id: Option<&str>) -> Option<request::Kind> {
    let text = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    match name {
        // AskUserQuestion: 1-4 questions, each with options. Claude Code adds
        // an "Other" answer to every question itself.
        "AskUserQuestion" => {
            let questions = input
                .get("questions")?
                .as_array()?
                .iter()
                .map(|question| {
                    let text_of = text(question, "question")?;
                    let options = question
                        .get("options")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(|option| {
                            Some(request::QuestionOption {
                                label: text(option, "label")?,
                                description: text(option, "description"),
                                preview: text(option, "preview"),
                            })
                        })
                        .collect();
                    Some(request::Question {
                        id: text_of.clone(),
                        header: text(question, "header"),
                        question: text_of,
                        options,
                        multi_select: question.get("multiSelect").and_then(Value::as_bool)
                            == Some(true),
                        allow_other: true,
                        secret: false,
                    })
                })
                .collect::<Option<Vec<_>>>()?;
            (!questions.is_empty()).then(|| {
                request::Kind::Question(request::Questions {
                    questions,
                    tool_call_id: tool_use_id.map(str::to_owned),
                })
            })
        }
        // ExitPlanMode: Claude Code adds the plan and the file it saved it to.
        "ExitPlanMode" => Some(request::Kind::PlanApproval(request::PlanApproval {
            plan: text(input, "plan").unwrap_or_default(),
            options: PLAN_MODES
                .iter()
                .map(|(mode, label)| {
                    request::Choice::new(mode.as_str(), *label, request::ChoiceKind::AllowOnce)
                })
                .collect(),
            path: text(input, "planFilePath").map(PathBuf::from),
            tool_call_id: tool_use_id.map(str::to_owned),
        })),
        _ => None,
    }
}

/// The permission modes a plan can be approved with, as Claude Code's own
/// plan prompt offers them. Picking one switches the session's mode.
const PLAN_MODES: [(super::PermissionMode, &str); 2] = [
    (
        super::PermissionMode::AcceptEdits,
        "Yes, and accept edits without asking",
    ),
    (
        super::PermissionMode::Default,
        "Yes, and ask before each edit",
    ),
];

/// The `can_use_tool` answer for a decision, in the Agent SDK's shape.
fn permission_response(id: &str, pending: &Pending, decision: &Decision) -> Result<Value> {
    let tool_use_id = &pending.tool_use_id;
    Ok(match (decision, &pending.kind) {
        (Decision::Allow, _) => json!({
            "behavior": "allow",
            "updatedInput": pending.input,
            "toolUseID": tool_use_id,
        }),
        (Decision::AllowForSession, _) => {
            let Some(suggestions) = &pending.suggestions else {
                return Err(Error::Unsupported {
                    operation: "allow_for_session",
                });
            };
            json!({
                "behavior": "allow",
                "updatedInput": pending.input,
                "updatedPermissions": suggestions,
                "toolUseID": tool_use_id,
            })
        }
        (Decision::Deny { message }, _) => json!({
            "behavior": "deny",
            "message": message.as_deref().unwrap_or(DENIED),
            "toolUseID": tool_use_id,
        }),
        (Decision::Cancel, _) => json!({
            "behavior": "deny",
            "message": DENIED,
            "interrupt": true,
            "toolUseID": tool_use_id,
        }),
        (Decision::Answer { answers }, request::Kind::Question(questions)) => {
            let mut input = pending.input.clone();
            let Some(fields) = input.as_object_mut() else {
                return Err(request::invalid(id, "the question has no input to answer"));
            };
            fields.insert(
                "answers".to_owned(),
                Value::Object(question_answers(id, &questions.questions, answers)?),
            );
            json!({
                "behavior": "allow",
                "updatedInput": input,
                "toolUseID": tool_use_id,
            })
        }
        (Decision::Select { option_id }, request::Kind::PlanApproval(plan)) => {
            if !plan.options.iter().any(|option| &option.id == option_id) {
                return Err(request::invalid(
                    id,
                    format!("the plan approval offers no option {option_id:?}"),
                ));
            }
            json!({
                "behavior": "allow",
                "updatedInput": pending.input,
                "updatedPermissions": [
                    { "type": "setMode", "mode": option_id, "destination": "session" }
                ],
                "toolUseID": tool_use_id,
            })
        }
        (decision, _) => {
            return Err(request::invalid(
                id,
                format!("{} does not answer this request", decision.name()),
            ));
        }
    })
}

/// AskUserQuestion's `answers`: each answered question's text to the answer,
/// as Claude Code's own prompt writes it. A multi-select answer is a list
/// (Claude Code joins it with commas); any other answer is one string.
fn question_answers(
    id: &str,
    questions: &[request::Question],
    answers: &[request::Answer],
) -> Result<serde_json::Map<String, Value>> {
    let mut map = serde_json::Map::new();
    for answer in answers {
        let Some(question) = questions.iter().find(|q| q.id == answer.question_id) else {
            return Err(request::invalid(
                id,
                format!("no question {:?}", answer.question_id),
            ));
        };
        if let Some(label) = answer.selected.iter().find(|label| {
            !question
                .options
                .iter()
                .any(|option| &option.label == *label)
        }) {
            return Err(request::invalid(
                id,
                format!("{:?} is not an option of {:?}", label, question.question),
            ));
        }
        if !question.multi_select && answer.selected.len() > 1 {
            return Err(request::invalid(
                id,
                format!("{:?} takes one option", question.question),
            ));
        }
        let values = answer.values();
        if values.is_empty() {
            continue;
        }
        let value = if question.multi_select {
            Value::from(values)
        } else {
            Value::from(values.join(", "))
        };
        map.insert(question.question.clone(), value);
    }
    Ok(map)
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

#[cfg(all(test, unix))]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use serde_json::{Value, json};
    use tokio::sync::broadcast;

    use super::{ClaudeProcess, Conversation, Launch, Pending, permission_response};
    use crate::process::Environment;
    use crate::request::{Answer, Decision, Kind, Outcome};
    use crate::session::Event;

    const PROMPT: &str = r#"{"type":"control_request","request_id":"req-1","request":{"subtype":"can_use_tool","tool_name":"Write","display_name":"Write","input":{"file_path":"hello.txt","content":"hi"},"description":"hello.txt","permission_suggestions":[{"type":"setMode","mode":"acceptEdits","destination":"session"}],"tool_use_id":"toolu_1"}}"#;

    struct Fake {
        folder: PathBuf,
        process: ClaudeProcess,
        events: broadcast::Receiver<Event>,
    }

    impl Fake {
        /// A `claude` that sends `PROMPT`, then `after_prompt`, and writes
        /// the next line it reads to `reply` in the folder.
        fn start(after_prompt: &str, forward_requests: bool) -> Self {
            let folder =
                std::env::temp_dir().join(format!("stubb-request-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&folder).unwrap();
            let executable = folder.join("claude");
            crate::process::write_executable(
                &executable,
                &format!(
                    "#!/bin/sh\nprintf '%s\\n' '{PROMPT}'\n{after_prompt}\nIFS= read -r reply\nprintf '%s' \"$reply\" > \"$(dirname \"$0\")/reply\"\nIFS= read -r unexpected\n"
                ),
            );
            let environment = Environment::inherit();
            let process = ClaudeProcess::spawn(&Launch {
                executable: &executable,
                cwd: &folder,
                config_dir: None,
                environment: &environment,
                conversation: Conversation::New { id: "session-1" },
                model: None,
                effort: None,
                fast_mode: None,
                forward_requests,
                additional_directories: &[],
                permission_mode: None,
                thinking_display: None,
            })
            .unwrap();
            let events = process.subscribe();
            Self {
                folder,
                process,
                events,
            }
        }

        async fn next(&mut self) -> Event {
            tokio::time::timeout(Duration::from_secs(5), self.events.recv())
                .await
                .expect("an event")
                .expect("an open stream")
        }

        async fn reply(&self) -> Value {
            let path = self.folder.join("reply");
            for _ in 0..100 {
                if let Ok(text) = std::fs::read_to_string(&path)
                    && !text.is_empty()
                {
                    return serde_json::from_str(&text).unwrap();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("the fake claude received no reply");
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.folder);
        }
    }

    #[tokio::test]
    async fn forwards_a_prompt_and_writes_the_hosts_answer() {
        let mut fake = Fake::start("", true);
        let Event::Request { request } = fake.next().await else {
            panic!("expected a request");
        };
        assert_eq!(request.id, "req-1");
        assert_eq!(
            request.kind,
            Kind::Tool {
                name: "Write".into(),
                display_name: Some("Write".into()),
                description: Some("hello.txt".into()),
                input: json!({"file_path": "hello.txt", "content": "hi"}),
                tool_call_id: Some("toolu_1".into()),
                can_allow_for_session: true,
            }
        );
        fake.process
            .respond("session-1", "req-1", Decision::Allow)
            .await
            .unwrap();
        assert_eq!(
            fake.next().await,
            Event::RequestResolved {
                id: "req-1".into(),
                outcome: Outcome::Answered {
                    decision: Decision::Allow
                },
            }
        );
        assert_eq!(
            fake.reply().await,
            json!({
                "type": "control_response",
                "response": {
                    "subtype": "success",
                    "request_id": "req-1",
                    "response": {
                        "behavior": "allow",
                        "updatedInput": {"file_path": "hello.txt", "content": "hi"},
                        "toolUseID": "toolu_1",
                    },
                },
            })
        );
        let again = fake
            .process
            .respond("session-1", "req-1", Decision::Allow)
            .await;
        assert!(matches!(again, Err(crate::Error::RequestNotFound { .. })));
    }

    #[tokio::test]
    async fn a_cancelled_prompt_is_withdrawn() {
        let mut fake = Fake::start(
            r#"printf '%s\n' '{"type":"control_cancel_request","request_id":"req-1"}'"#,
            true,
        );
        assert!(matches!(fake.next().await, Event::Request { .. }));
        assert_eq!(
            fake.next().await,
            Event::RequestResolved {
                id: "req-1".into(),
                outcome: Outcome::Withdrawn,
            }
        );
        let late = fake
            .process
            .respond("session-1", "req-1", Decision::Allow)
            .await;
        assert!(matches!(late, Err(crate::Error::RequestNotFound { .. })));
    }

    #[tokio::test]
    async fn without_forwarding_prompts_are_still_declined() {
        let mut fake = Fake::start("", false);
        assert!(matches!(
            fake.next().await,
            Event::RequestDeclined { method, .. } if method == "can_use_tool:Write"
        ));
        assert_eq!(
            fake.reply().await["response"]["response"]["behavior"],
            "deny"
        );
    }

    #[test]
    fn answers_follow_the_agent_sdk_shape() {
        let pending = Pending {
            native_id: json!("req-1"),
            tool_use_id: Some("toolu_1".into()),
            input: json!({"command": "ls"}),
            suggestions: Some(json!([{"type": "setMode", "mode": "acceptEdits"}])),
            kind: Kind::Tool {
                name: "Bash".into(),
                display_name: None,
                description: None,
                input: json!({"command": "ls"}),
                tool_call_id: Some("toolu_1".into()),
                can_allow_for_session: true,
            },
        };
        let session = permission_response("req-1", &pending, &Decision::AllowForSession).unwrap();
        assert_eq!(session["updatedPermissions"][0]["mode"], "acceptEdits");
        let deny = permission_response(
            "req-1",
            &pending,
            &Decision::Deny {
                message: Some("not now".into()),
            },
        )
        .unwrap();
        assert_eq!(deny["behavior"], "deny");
        assert_eq!(deny["message"], "not now");
        assert!(deny.get("interrupt").is_none());
        let cancel = permission_response("req-1", &pending, &Decision::Cancel).unwrap();
        assert!(matches!(
            permission_response(
                "req-1",
                &pending,
                &Decision::Select {
                    option_id: "acceptEdits".into()
                }
            ),
            Err(crate::Error::InvalidDecision { .. })
        ));
        assert_eq!(cancel["interrupt"], true);

        let without = Pending {
            suggestions: None,
            ..pending
        };
        assert!(matches!(
            permission_response("req-1", &without, &Decision::AllowForSession),
            Err(crate::Error::Unsupported { .. })
        ));
    }

    fn pending_for(tool: &str, input: Value) -> Pending {
        Pending {
            native_id: json!("req-1"),
            tool_use_id: Some("toolu_1".into()),
            kind: super::interactive_kind(tool, &input, Some("toolu_1"))
                .expect("an interactive kind"),
            input,
            suggestions: None,
        }
    }

    #[test]
    fn ask_user_question_answers_go_into_the_tool_input() {
        let input = json!({ "questions": [
            { "question": "Which library?", "header": "Library", "multiSelect": false,
              "options": [{ "label": "chrono", "description": "Full" }, { "label": "time", "description": "Small" }] },
            { "question": "Which features?", "header": "Features", "multiSelect": true,
              "options": [{ "label": "serde", "description": "" }, { "label": "tz", "description": "", "preview": "x" }] }
        ] });
        let pending = pending_for("AskUserQuestion", input.clone());
        let Kind::Question(questions) = &pending.kind else {
            panic!("expected questions, got {:?}", pending.kind);
        };
        assert_eq!(questions.tool_call_id.as_deref(), Some("toolu_1"));
        assert_eq!(questions.questions[0].id, "Which library?");
        assert_eq!(questions.questions[0].header.as_deref(), Some("Library"));
        assert!(questions.questions[0].allow_other);
        assert!(questions.questions[1].multi_select);
        assert_eq!(
            questions.questions[1].options[1].preview.as_deref(),
            Some("x")
        );

        let response = permission_response(
            "req-1",
            &pending,
            &Decision::Answer {
                answers: vec![
                    Answer::other("Which library?", "jiff"),
                    Answer::selected("Which features?", ["serde", "tz"]),
                ],
            },
        )
        .unwrap();
        let mut expected = input;
        expected["answers"] = json!({
            "Which library?": "jiff",
            "Which features?": ["serde", "tz"],
        });
        assert_eq!(
            response,
            json!({ "behavior": "allow", "updatedInput": expected, "toolUseID": "toolu_1" })
        );
        for wrong in [
            Decision::Answer {
                answers: vec![Answer::selected("Which library?", ["moment"])],
            },
            Decision::Answer {
                answers: vec![Answer::other("Which colour?", "red")],
            },
            Decision::Select {
                option_id: "default".into(),
            },
        ] {
            assert!(matches!(
                permission_response("req-1", &pending, &wrong),
                Err(crate::Error::InvalidDecision { .. })
            ));
        }
        // A host that only knows tools still gets an answer through.
        assert_eq!(
            permission_response("req-1", &pending, &Decision::Deny { message: None }).unwrap()["behavior"],
            "deny"
        );
    }

    #[test]
    fn plans_are_approved_with_a_permission_mode_or_rejected_with_feedback() {
        let input =
            json!({ "plan": "# Plan\n1. Do it", "planFilePath": "/home/u/.claude/plans/p.md" });
        let pending = pending_for("ExitPlanMode", input.clone());
        let Kind::PlanApproval(plan) = &pending.kind else {
            panic!("expected a plan, got {:?}", pending.kind);
        };
        assert_eq!(plan.plan, "# Plan\n1. Do it");
        assert_eq!(
            plan.path.as_deref(),
            Some(std::path::Path::new("/home/u/.claude/plans/p.md"))
        );
        let ids: Vec<_> = plan.options.iter().map(|o| o.id.as_str()).collect();
        assert_eq!(ids, ["acceptEdits", "default"]);

        assert_eq!(
            permission_response(
                "req-1",
                &pending,
                &Decision::Select {
                    option_id: "acceptEdits".into()
                }
            )
            .unwrap(),
            json!({
                "behavior": "allow",
                "updatedInput": input,
                "updatedPermissions": [{ "type": "setMode", "mode": "acceptEdits", "destination": "session" }],
                "toolUseID": "toolu_1",
            })
        );
        assert_eq!(
            permission_response("req-1", &pending, &Decision::Allow).unwrap(),
            json!({ "behavior": "allow", "updatedInput": input, "toolUseID": "toolu_1" })
        );
        let rejected = permission_response(
            "req-1",
            &pending,
            &Decision::Deny {
                message: Some("Use a smaller change.".into()),
            },
        )
        .unwrap();
        assert_eq!(rejected["behavior"], "deny");
        assert_eq!(rejected["message"], "Use a smaller change.");
        assert!(matches!(
            permission_response(
                "req-1",
                &pending,
                &Decision::Select {
                    option_id: "bypassPermissions".into()
                }
            ),
            Err(crate::Error::InvalidDecision { .. })
        ));
    }

    #[test]
    fn a_single_select_question_takes_one_option() {
        let input = json!({ "questions": [
            { "question": "Which library?", "header": "Library", "multiSelect": false,
              "options": [{ "label": "chrono", "description": "" }, { "label": "time", "description": "" }] }
        ] });
        let pending = pending_for("AskUserQuestion", input);
        let answer = |labels: &[&str]| {
            permission_response(
                "req-1",
                &pending,
                &Decision::Answer {
                    answers: vec![Answer::selected("Which library?", labels.iter().copied())],
                },
            )
        };
        assert!(matches!(
            answer(&["chrono", "time"]),
            Err(crate::Error::InvalidDecision { .. })
        ));
        assert!(answer(&["time"]).is_ok());
    }

    #[test]
    fn unreadable_questions_stay_tool_requests() {
        assert!(super::interactive_kind("AskUserQuestion", &json!({}), None).is_none());
        assert!(super::interactive_kind("Write", &json!({}), None).is_none());
    }
}
