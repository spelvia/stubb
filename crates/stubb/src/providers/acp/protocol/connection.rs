//! One ACP agent process and the client side of its connection.
//!
//! The agent speaks JSON-RPC 2.0, one message per line, on its standard
//! input and output. The SDK's connection runs on a background task; Stubb
//! keeps its handle to send requests. The agent's own requests are answered
//! here: permission prompts go through [`State`], file reads and writes are
//! served from disk within the session's folders, and anything else (such
//! as terminals, which Stubb does not offer) is refused by the SDK as an
//! unknown method.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::v1::schema as wire;
use agent_client_protocol::{self as sdk, Agent, Client, ConnectTo, Lines, UntypedMessage};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, oneshot};
use tokio::task::JoinHandle;

use super::events::{self, Turn};
use crate::process::Environment;
use crate::providers::acp::Launch;
use crate::request::{self, Decision};
use crate::session::{self, CloseReason};
use crate::turn;
use crate::{Error, Result};

const EVENT_BUFFER: usize = 1024;
const STDERR_TAIL_BYTES: usize = 8 * 1024;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

pub(in crate::providers::acp) type Cx = sdk::v1::ConnectionTo<Agent>;

/// How a connection reaches its agent: a process, or (in tests) an
/// in-memory stream to a fake agent.
#[derive(Clone)]
pub(in crate::providers::acp) enum Connector {
    Process {
        launch: Launch,
        environment: Environment,
    },
    #[cfg(test)]
    Memory(Arc<dyn Fn() -> tokio::io::DuplexStream + Send + Sync>),
}

/// A running agent and its initialized connection.
pub(in crate::providers::acp) struct Connection {
    pub(in crate::providers::acp) cx: Cx,
    pub(in crate::providers::acp) state: Arc<State>,
    pub(in crate::providers::acp) init: wire::InitializeResponse,
    stop: Mutex<Option<oneshot::Sender<()>>>,
    driver: Mutex<Option<JoinHandle<()>>>,
    child: tokio::sync::Mutex<Option<Child>>,
}

impl Connection {
    /// Starts the agent in `cwd` and completes `initialize`.
    pub(in crate::providers::acp) async fn open(
        connector: &Connector,
        cwd: &Path,
        timeout: Duration,
        auth_method: Option<&str>,
    ) -> Result<Self> {
        let state = Arc::new(State::new());
        let (child, (cx, stop, driver)) = match connector {
            Connector::Process {
                launch,
                environment,
            } => {
                let (mut child, transport) = spawn(launch, environment, cwd).await?;
                let stderr = child.stderr.take().expect("stderr is piped");
                tokio::spawn({
                    let state = Arc::clone(&state);
                    async move {
                        let mut lines = BufReader::new(stderr).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            state.append_stderr(&line);
                        }
                    }
                });
                (Some(child), connect(transport, Arc::clone(&state)).await?)
            }
            #[cfg(test)]
            Connector::Memory(connect_fake) => {
                let (read, write) = tokio::io::split(connect_fake());
                (None, connect(lines(read, write), Arc::clone(&state)).await?)
            }
        };
        let connection = Self {
            cx,
            state,
            init: wire::InitializeResponse::new(agent_client_protocol::schema::ProtocolVersion::V1),
            stop: Mutex::new(Some(stop)),
            driver: Mutex::new(Some(driver)),
            child: tokio::sync::Mutex::new(child),
        };
        match connection.initialize(timeout, auth_method).await {
            Ok(init) => Ok(Self { init, ..connection }),
            Err(error) => {
                connection.shutdown().await;
                Err(error)
            }
        }
    }

    async fn initialize(
        &self,
        timeout: Duration,
        auth_method: Option<&str>,
    ) -> Result<wire::InitializeResponse> {
        let capabilities = wire::ClientCapabilities::new()
            .fs(wire::FileSystemCapabilities::new()
                .read_text_file(true)
                .write_text_file(true))
            .terminal(false)
            .session(
                wire::ClientSessionCapabilities::new()
                    .compaction(wire::CompactionCapabilities::new()),
            );
        let request =
            wire::InitializeRequest::new(agent_client_protocol::schema::ProtocolVersion::V1)
                .client_capabilities(capabilities)
                .client_info(wire::Implementation::new(
                    "stubb",
                    env!("CARGO_PKG_VERSION"),
                ));
        let init = self.request("initialize", request, timeout).await?;
        if let Some(method) = auth_method {
            self.request(
                "authenticate",
                wire::AuthenticateRequest::new(method.to_owned()),
                timeout,
            )
            .await?;
        }
        Ok(init)
    }

    /// Sends a request and waits up to `timeout` for its answer.
    pub(in crate::providers::acp) async fn request<R>(
        &self,
        method: &'static str,
        request: R,
        timeout: Duration,
    ) -> Result<R::Response>
    where
        R: sdk::JsonRpcRequest,
    {
        if let Some(reason) = self.state.closed_reason() {
            return Err(Error::ProcessExited { stderr: reason });
        }
        let sent = self.cx.send_request(request).block_task();
        match tokio::time::timeout(timeout, sent).await {
            Ok(result) => result.map_err(|error| self.state.error(method, error)),
            Err(_) => Err(Error::Timeout(timeout)),
        }
    }

    /// Sends a request without a deadline, for prompts.
    pub(in crate::providers::acp) async fn request_untimed<R>(
        &self,
        method: &'static str,
        request: R,
    ) -> Result<R::Response>
    where
        R: sdk::JsonRpcRequest,
    {
        if let Some(reason) = self.state.closed_reason() {
            return Err(Error::ProcessExited { stderr: reason });
        }
        self.cx
            .send_request(request)
            .block_task()
            .await
            .map_err(|error| self.state.error(method, error))
    }

    pub(in crate::providers::acp) fn notify<N: sdk::JsonRpcNotification>(
        &self,
        notification: N,
    ) -> Result<()> {
        self.cx
            .send_notification(notification)
            .map_err(|error| self.state.error("notification", error))
    }

    pub(in crate::providers::acp) fn is_alive(&self) -> bool {
        self.state.closed_reason().is_none()
    }

    /// Closes the connection, which closes the agent's standard input, and
    /// waits briefly for it to exit before killing it.
    pub(in crate::providers::acp) async fn shutdown(&self) {
        self.state.closing.store(true, Ordering::SeqCst);
        if let Some(stop) = self.stop.lock().expect("stop lock").take() {
            let _ = stop.send(());
        }
        let driver = self.driver.lock().expect("driver lock").take();
        if let Some(driver) = driver
            && tokio::time::timeout(SHUTDOWN_GRACE, driver).await.is_err()
        {
            // The handle was consumed by the timeout; the task ends when
            // the process is killed below.
        }
        if let Some(mut child) = self.child.lock().await.take()
            && tokio::time::timeout(SHUTDOWN_GRACE, child.wait())
                .await
                .is_err()
        {
            let _ = child.kill().await;
        }
        self.state.mark_closed("the agent was shut down".to_owned());
    }
}

/// Spawns the agent process with piped standard streams.
async fn spawn(
    launch: &Launch,
    environment: &Environment,
    cwd: &Path,
) -> Result<(Child, Lines<impl LineSink, impl LineStream>)> {
    let program = crate::core::discovery::executable::resolve(&launch.program)
        .await
        .ok_or_else(|| Error::ExecutableNotFound {
            executable: launch.program.clone(),
        })?;
    let mut command = Command::new(&program);
    command
        .args(&launch.args)
        .current_dir(launch.cwd.as_deref().unwrap_or(cwd))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    crate::process::die_with_parent(&mut command);
    environment.apply(&mut command);
    // A program found through the login shell, such as `npx` from a version
    // manager, needs its interpreter next to it.
    if let Some(path) =
        crate::core::discovery::executable::path_beside(&program, environment.value("PATH"))
    {
        command.env("PATH", path);
    }
    for (name, value) in &launch.env {
        command.env(name, value);
    }
    let mut child = command.spawn().map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            Error::ExecutableNotFound {
                executable: program.clone(),
            }
        } else {
            Error::SpawnFailed {
                executable: program.clone(),
                source,
            }
        }
    })?;
    let stdin = child.stdin.take().expect("stdin is piped");
    let stdout = child.stdout.take().expect("stdout is piped");
    Ok((child, lines(stdout, stdin)))
}

pub(in crate::providers::acp) trait LineSink:
    futures::Sink<String, Error = std::io::Error> + Send + 'static
{
}
impl<T: futures::Sink<String, Error = std::io::Error> + Send + 'static> LineSink for T {}
pub(in crate::providers::acp) trait LineStream:
    futures::Stream<Item = std::io::Result<String>> + Send + 'static
{
}
impl<T: futures::Stream<Item = std::io::Result<String>> + Send + 'static> LineStream for T {}

/// Newline-delimited messages over a byte stream pair. Dropping the sink
/// closes the writer, which is the agent's standard input.
pub(in crate::providers::acp) fn lines(
    read: impl AsyncRead + Send + Unpin + 'static,
    write: impl AsyncWrite + Send + Unpin + 'static,
) -> Lines<impl LineSink, impl LineStream> {
    let incoming = futures::stream::unfold(BufReader::new(read).lines(), |mut lines| async {
        match lines.next_line().await {
            Ok(Some(line)) => Some((Ok(line), lines)),
            Ok(None) => None,
            Err(error) => Some((Err(error), lines)),
        }
    });
    let outgoing = futures::sink::unfold(write, |mut write, line: String| async move {
        write.write_all(line.as_bytes()).await?;
        write.write_all(b"\n").await?;
        write.flush().await?;
        Ok::<_, std::io::Error>(write)
    });
    Lines::new(Box::pin(outgoing), Box::pin(incoming))
}

/// Runs the client side of the connection on a background task and returns
/// its handle, a stop signal and the task.
async fn connect(
    transport: impl ConnectTo<Client> + 'static,
    state: Arc<State>,
) -> Result<(Cx, oneshot::Sender<()>, JoinHandle<()>)> {
    let (cx_tx, cx_rx) = oneshot::channel();
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let driver = tokio::spawn({
        let state = Arc::clone(&state);
        async move {
            let permissions = Arc::clone(&state);
            let reads = Arc::clone(&state);
            let writes = Arc::clone(&state);
            let updates = Arc::clone(&state);
            let result = Client
                .v1()
                .name("stubb")
                .on_receive_request(
                    async move |request: wire::RequestPermissionRequest,
                                responder: sdk::v1::Responder<wire::RequestPermissionResponse>,
                                _cx| {
                        permissions.permission(request, responder);
                        Ok(())
                    },
                    sdk::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: wire::ReadTextFileRequest,
                                responder: sdk::v1::Responder<wire::ReadTextFileResponse>,
                                _cx| {
                        let roots = reads.roots(false);
                        tokio::spawn(async move {
                            let _ =
                                responder.respond_with_result(read_text(&roots, &request).await);
                        });
                        Ok(())
                    },
                    sdk::on_receive_request!(),
                )
                .on_receive_request(
                    async move |request: wire::WriteTextFileRequest,
                                responder: sdk::v1::Responder<wire::WriteTextFileResponse>,
                                _cx| {
                        let roots = writes.roots(true);
                        tokio::spawn(async move {
                            let _ =
                                responder.respond_with_result(write_text(&roots, &request).await);
                        });
                        Ok(())
                    },
                    sdk::on_receive_request!(),
                )
                .on_receive_notification(
                    async move |notification: UntypedMessage, _cx| {
                        if notification.method == "session/update" {
                            updates.update(notification.params);
                        }
                        Ok(())
                    },
                    sdk::on_receive_notification!(),
                )
                .connect_with(transport, async move |cx: Cx| {
                    let _ = cx_tx.send(cx.clone());
                    tokio::select! {
                        _ = stop_rx => {}
                        () = cx.incoming_closed() => {}
                    }
                    Ok(())
                })
                .await;
            let reason = match result {
                Ok(()) => "the agent closed its connection".to_owned(),
                Err(error) => error.message,
            };
            state.disconnected(reason);
        }
    });
    match cx_rx.await {
        Ok(cx) => Ok((cx, stop_tx, driver)),
        Err(_) => Err(Error::ProcessExited {
            stderr: state.stderr_tail(),
        }),
    }
}

/// A permission request waiting for the host.
struct Pending {
    responder: sdk::v1::Responder<wire::RequestPermissionResponse>,
    options: Vec<wire::PermissionOption>,
    tool_call_id: String,
}

/// What the connection knows about its session, shared with the SDK's
/// handlers.
pub(in crate::providers::acp) struct State {
    events: broadcast::Sender<session::Event>,
    inner: Mutex<Inner>,
    next_request: AtomicU64,
    closing: AtomicBool,
    stderr_tail: Mutex<String>,
}

#[derive(Default)]
struct Inner {
    session_id: Option<String>,
    forward_requests: bool,
    /// The session's folder, then folders it may only read.
    roots: Vec<PathBuf>,
    turn: Option<Turn>,
    /// Each compaction's status as last reported, so a patch that leaves it
    /// unchanged (a summary chunk, metadata) is not reported again.
    compactions: HashMap<String, turn::CompactionStatus>,
    config_options: Vec<wire::SessionConfigOption>,
    modes: Option<wire::SessionModeState>,
    pending: HashMap<String, Pending>,
    closed: Option<String>,
}

impl State {
    fn new() -> Self {
        Self {
            events: broadcast::channel(EVENT_BUFFER).0,
            inner: Mutex::new(Inner::default()),
            next_request: AtomicU64::new(1),
            closing: AtomicBool::new(false),
            stderr_tail: Mutex::new(String::new()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect("state lock")
    }

    pub(in crate::providers::acp) fn subscribe(&self) -> broadcast::Receiver<session::Event> {
        self.events.subscribe()
    }

    pub(in crate::providers::acp) fn emit(&self, event: session::Event) {
        let _ = self.events.send(event);
    }

    /// Records the session this connection serves.
    pub(in crate::providers::acp) fn attach(
        &self,
        session_id: &str,
        cwd: &Path,
        additional: &[PathBuf],
        forward_requests: bool,
    ) {
        let mut inner = self.lock();
        inner.session_id = Some(session_id.to_owned());
        inner.forward_requests = forward_requests;
        inner.roots = std::iter::once(cwd.to_path_buf())
            .chain(additional.iter().cloned())
            .collect();
    }

    pub(in crate::providers::acp) fn set_config(
        &self,
        modes: Option<wire::SessionModeState>,
        config_options: Option<Vec<wire::SessionConfigOption>>,
    ) {
        let mut inner = self.lock();
        if modes.is_some() {
            inner.modes = modes;
        }
        if let Some(options) = config_options {
            inner.config_options = options;
        }
    }

    pub(in crate::providers::acp) fn config_options(&self) -> Vec<wire::SessionConfigOption> {
        self.lock().config_options.clone()
    }

    pub(in crate::providers::acp) fn modes(&self) -> Option<wire::SessionModeState> {
        self.lock().modes.clone()
    }

    pub(in crate::providers::acp) fn set_current_mode(&self, mode: &str) {
        if let Some(modes) = self.lock().modes.as_mut() {
            modes.current_mode_id = wire::SessionModeId::new(mode.to_owned());
        }
    }

    /// Where the agent may read (`write` false) or write files.
    fn roots(&self, write: bool) -> Vec<PathBuf> {
        let inner = self.lock();
        if write {
            inner.roots.iter().take(1).cloned().collect()
        } else {
            inner.roots.clone()
        }
    }

    pub(in crate::providers::acp) fn current_turn(&self) -> Option<String> {
        self.lock().turn.as_ref().map(|turn| turn.id.clone())
    }

    /// Starts a turn, unless one is running.
    pub(in crate::providers::acp) fn begin_turn(&self, turn_id: &str) -> bool {
        let mut inner = self.lock();
        if inner.turn.is_some() {
            return false;
        }
        inner.turn = Some(Turn::new(turn_id.to_owned()));
        true
    }

    /// Marks the running turn as interrupted, when it is `turn_id`.
    pub(in crate::providers::acp) fn interrupt(&self, turn_id: &str) -> bool {
        match self.lock().turn.as_mut() {
            Some(turn) if turn.id == turn_id => {
                turn.interrupted = true;
                true
            }
            _ => false,
        }
    }

    pub(in crate::providers::acp) fn is_interrupted(&self) -> bool {
        self.lock()
            .turn
            .as_ref()
            .is_some_and(|turn| turn.interrupted)
    }

    /// Ends the running turn and returns it with the events that close its
    /// items.
    pub(in crate::providers::acp) fn end_turn(
        &self,
        completed: bool,
    ) -> Option<(Turn, Vec<session::Event>)> {
        let mut turn = self.lock().turn.take()?;
        let events = turn.finish(completed);
        Some((turn, events))
    }

    fn update(&self, params: Value) {
        let notification = match serde_json::from_value::<wire::SessionNotification>(params.clone())
        {
            Ok(notification) => notification,
            Err(_) => {
                // An update this SDK version does not know.
                let name = params
                    .pointer("/update/sessionUpdate")
                    .and_then(Value::as_str)
                    .unwrap_or("session_update")
                    .to_owned();
                self.emit(session::Event::Raw {
                    name,
                    payload: params,
                });
                return;
            }
        };
        let events = {
            let mut inner = self.lock();
            match &notification.update {
                wire::SessionUpdate::CurrentModeUpdate(update) => {
                    if let Some(modes) = inner.modes.as_mut() {
                        modes.current_mode_id = update.current_mode_id.clone();
                    }
                }
                wire::SessionUpdate::ConfigOptionUpdate(update) => {
                    inner.config_options.clone_from(&update.config_options);
                }
                _ => {}
            }
            let turn_id = inner.turn.as_ref().map(|turn| turn.id.clone());
            match (&notification.update, inner.turn.as_mut()) {
                // Compactions and context size are not tied to a turn.
                (wire::SessionUpdate::CompactionUpdate(update), _) => {
                    events::compaction(turn_id, update, &mut inner.compactions)
                        .into_iter()
                        .collect()
                }
                (wire::SessionUpdate::UsageUpdate(usage), None) => vec![session::Event::Usage {
                    turn_id: String::new(),
                    usage: events::context_usage(usage),
                }],
                (update, Some(turn)) => turn.update(update),
                (update, None) => events::raw(update).into_iter().collect(),
            }
        };
        for event in events {
            self.emit(event);
        }
    }

    fn permission(
        &self,
        request: wire::RequestPermissionRequest,
        responder: sdk::v1::Responder<wire::RequestPermissionResponse>,
    ) {
        let tool_call_id = request.tool_call.tool_call_id.0.to_string();
        let fields = &request.tool_call.fields;
        let title = fields.title.clone();
        let mut inner = self.lock();
        let turn_id = inner.turn.as_ref().map(|turn| turn.id.clone());
        if !inner.forward_requests {
            if let Some(turn) = inner.turn.as_mut() {
                turn.decline(&tool_call_id);
            }
            drop(inner);
            let outcome = choose(
                &request.options,
                &[
                    wire::PermissionOptionKind::RejectOnce,
                    wire::PermissionOptionKind::RejectAlways,
                ],
            )
            .map_or(wire::RequestPermissionOutcome::Cancelled, selected);
            let _ = responder.respond(wire::RequestPermissionResponse::new(outcome));
            self.emit(session::Event::RequestDeclined {
                method: "session/request_permission".to_owned(),
                turn_id,
                summary: title,
            });
            return;
        }
        let id = format!(
            "permission-{}",
            self.next_request.fetch_add(1, Ordering::Relaxed)
        );
        let name = fields
            .name
            .clone()
            .or_else(|| {
                fields
                    .kind
                    .as_ref()
                    .and_then(|kind| serde_json::to_value(kind).ok())
                    .and_then(|kind| kind.as_str().map(str::to_owned))
            })
            .or_else(|| title.clone())
            .unwrap_or_else(|| "tool".to_owned());
        let can_allow_for_session = request
            .options
            .iter()
            .any(|option| option.kind == wire::PermissionOptionKind::AllowAlways);
        let event = session::Event::Request {
            request: request::Request {
                id: id.clone(),
                turn_id,
                kind: request::Kind::Tool {
                    name,
                    display_name: title,
                    description: None,
                    input: fields.raw_input.clone().unwrap_or(Value::Null),
                    tool_call_id: Some(tool_call_id.clone()),
                    can_allow_for_session,
                },
                native: serde_json::to_value(&request).unwrap_or_default(),
            },
        };
        inner.pending.insert(
            id,
            Pending {
                responder,
                options: request.options,
                tool_call_id,
            },
        );
        drop(inner);
        self.emit(event);
    }

    /// Answers a forwarded permission request. Returns whether the decision
    /// asks to stop the turn.
    pub(in crate::providers::acp) fn respond(&self, id: &str, decision: &Decision) -> Result<bool> {
        let mut inner = self.lock();
        let not_found = || Error::RequestNotFound {
            session_id: inner.session_id.clone().unwrap_or_default(),
            id: id.to_owned(),
        };
        let pending = inner.pending.get(id).ok_or_else(not_found)?;
        let kinds: &[wire::PermissionOptionKind] = match decision {
            Decision::Allow => &[
                wire::PermissionOptionKind::AllowOnce,
                wire::PermissionOptionKind::AllowAlways,
            ],
            Decision::AllowForSession => &[wire::PermissionOptionKind::AllowAlways],
            Decision::Deny { .. } | Decision::Cancel => &[
                wire::PermissionOptionKind::RejectOnce,
                wire::PermissionOptionKind::RejectAlways,
            ],
        };
        let outcome = match (choose(&pending.options, kinds), decision) {
            (Some(option), _) => selected(option),
            (None, Decision::Deny { .. } | Decision::Cancel) => {
                wire::RequestPermissionOutcome::Cancelled
            }
            (None, _) => {
                return Err(Error::InvalidSetting {
                    setting: "decision",
                    value: format!("{decision:?}"),
                    reason: "the agent offered no option for this decision".to_owned(),
                });
            }
        };
        let pending = inner.pending.remove(id).expect("pending request");
        if decision.is_refusal()
            && let Some(turn) = inner.turn.as_mut()
        {
            turn.decline(&pending.tool_call_id);
        }
        drop(inner);
        let _ = pending
            .responder
            .respond(wire::RequestPermissionResponse::new(outcome));
        self.emit(session::Event::RequestResolved {
            id: id.to_owned(),
            outcome: request::Outcome::Answered {
                decision: decision.clone(),
            },
        });
        Ok(matches!(decision, Decision::Cancel))
    }

    /// Answers every pending request as cancelled, as ACP requires when a
    /// turn is cancelled, and reports them withdrawn.
    pub(in crate::providers::acp) fn withdraw_requests(&self) {
        let pending: Vec<(String, Pending)> = self.lock().pending.drain().collect();
        for (id, pending) in pending {
            let _ = pending
                .responder
                .respond(wire::RequestPermissionResponse::new(
                    wire::RequestPermissionOutcome::Cancelled,
                ));
            self.emit(session::Event::RequestResolved {
                id,
                outcome: request::Outcome::Withdrawn,
            });
        }
    }

    pub(in crate::providers::acp) fn closed_reason(&self) -> Option<String> {
        self.lock().closed.clone()
    }

    fn mark_closed(&self, reason: String) -> bool {
        let mut inner = self.lock();
        if inner.closed.is_some() {
            return false;
        }
        inner.closed = Some(reason);
        true
    }

    /// The connection ended: when Stubb did not ask for it, the session is
    /// over and its running turn failed.
    fn disconnected(&self, reason: String) {
        if self.closing.load(Ordering::SeqCst) {
            self.mark_closed(reason);
            return;
        }
        let stderr = self.stderr_tail();
        let message = if stderr.is_empty() { reason } else { stderr };
        if !self.mark_closed(message.clone()) {
            return;
        }
        self.withdraw_requests();
        if let Some((turn, events)) = self.end_turn(false) {
            for event in events {
                self.emit(event);
            }
            self.emit(session::Event::TurnEnd {
                turn: crate::providers::acp::live::ended_turn(
                    turn.id,
                    crate::turn::Status::Failed,
                    turn.started_at,
                    Some(message.clone()),
                ),
            });
        }
        self.emit(session::Event::Closed {
            reason: CloseReason::Exited { message },
        });
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

    pub(in crate::providers::acp) fn stderr_tail(&self) -> String {
        self.stderr_tail
            .lock()
            .expect("stderr lock")
            .trim()
            .to_owned()
    }

    /// Stubb's error for a failed request.
    pub(in crate::providers::acp) fn error(&self, method: &str, error: sdk::Error) -> Error {
        if sdk::is_connection_terminated(&error) || sdk::is_incoming_transport_closed(&error) {
            return Error::ProcessExited {
                stderr: self.stderr_tail(),
            };
        }
        let mut message = error.message;
        if let Some(data) = error.data.filter(|data| !data.is_null()) {
            message = match data {
                Value::String(text) => format!("{message}: {text}"),
                data => format!("{message}: {data}"),
            };
        }
        Error::Rpc {
            method: method.to_owned(),
            code: i64::from(error.code.value()),
            message,
        }
    }
}

fn choose<'a>(
    options: &'a [wire::PermissionOption],
    kinds: &[wire::PermissionOptionKind],
) -> Option<&'a wire::PermissionOption> {
    kinds
        .iter()
        .find_map(|kind| options.iter().find(|option| option.kind == *kind))
}

fn selected(option: &wire::PermissionOption) -> wire::RequestPermissionOutcome {
    wire::RequestPermissionOutcome::Selected(wire::SelectedPermissionOutcome::new(
        option.option_id.clone(),
    ))
}

/// `path` when it lies within one of `roots`, after resolving symbolic
/// links of the part that exists.
fn contained(path: &Path, roots: &[PathBuf]) -> Option<PathBuf> {
    if !path.is_absolute() || path.components().any(|c| c == Component::ParentDir) {
        return None;
    }
    let mut existing = path.to_path_buf();
    let mut rest = Vec::new();
    while !existing.exists() {
        rest.push(existing.file_name()?.to_owned());
        existing = existing.parent()?.to_path_buf();
    }
    let mut real = std::fs::canonicalize(&existing).ok()?;
    for part in rest.into_iter().rev() {
        real.push(part);
    }
    roots
        .iter()
        .filter_map(|root| std::fs::canonicalize(root).ok())
        .any(|root| real.starts_with(root))
        .then_some(real)
}

fn outside(path: &Path) -> sdk::Error {
    sdk::Error::invalid_params().data(format!(
        "{} is outside the folders this session may use",
        path.display()
    ))
}

async fn read_text(
    roots: &[PathBuf],
    request: &wire::ReadTextFileRequest,
) -> std::result::Result<wire::ReadTextFileResponse, sdk::Error> {
    let path = contained(&request.path, roots).ok_or_else(|| outside(&request.path))?;
    let text = tokio::fs::read_to_string(&path).await.map_err(|error| {
        let code = if error.kind() == std::io::ErrorKind::NotFound {
            sdk::ErrorCode::RESOURCE_NOT_FOUND
        } else {
            sdk::ErrorCode::INTERNAL_ERROR
        };
        sdk::Error::new(code, error.to_string())
    })?;
    let start = request
        .line
        .map_or(0, |line| line.saturating_sub(1) as usize);
    let content = if start == 0 && request.limit.is_none() {
        text
    } else {
        let lines = text.split_inclusive('\n').skip(start);
        match request.limit {
            Some(limit) => lines.take(limit as usize).collect(),
            None => lines.collect(),
        }
    };
    Ok(wire::ReadTextFileResponse::new(content))
}

async fn write_text(
    roots: &[PathBuf],
    request: &wire::WriteTextFileRequest,
) -> std::result::Result<wire::WriteTextFileResponse, sdk::Error> {
    let path = contained(&request.path, roots).ok_or_else(|| outside(&request.path))?;
    let internal = |error: std::io::Error| sdk::Error::internal_error().data(error.to_string());
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(internal)?;
    }
    tokio::fs::write(&path, &request.content)
        .await
        .map_err(internal)?;
    Ok(wire::WriteTextFileResponse::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_must_lie_within_the_session_folders() {
        let root = std::env::temp_dir().join(format!("stubb-acp-roots-{}", std::process::id()));
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let roots = vec![root.clone()];
        assert!(contained(&root.join("sub/new.txt"), &roots).is_some());
        assert!(contained(&root.join("missing/deeper/new.txt"), &roots).is_some());
        assert!(contained(&root.join("../escape.txt"), &roots).is_none());
        assert!(contained(Path::new("relative.txt"), &roots).is_none());
        assert!(contained(Path::new("/etc/passwd"), &roots).is_none());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc", root.join("link")).unwrap();
            assert!(contained(&root.join("link/passwd"), &roots).is_none());
        }
        std::fs::remove_dir_all(&root).unwrap();
    }
}
