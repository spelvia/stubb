//! Native Codex integration through `codex app-server`.
//!
//! A [`Codex`] provider owns at most one app-server process: one execution
//! context. The process starts on first use and is shared by catalog
//! requests and every session; if it exits, the next call starts a new one
//! and live sessions receive [`session::Event::Closed`].

mod connection;
mod events;
mod history;
mod observation;
#[cfg(test)]
mod observation_tests;
mod routing;
mod terminals;
mod types;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::sync::broadcast;

use crate::catalog::{self, Catalog, FastMode, ProviderInfo};
use crate::provider::BoxFuture;
use crate::session::{self, Session};
use crate::{Error, Provider, Result, turn};
use connection::AppServer;
use types::{ModelListParams, ModelListResponse, ThreadSettingsParams};

pub(crate) const PROVIDER_ID: &str = "codex";
const PAGE_LIMIT: u32 = 100;
const MAX_PAGES: usize = 50;
/// Thread sources included in session lists; sub-agent threads are left out.
const SESSION_SOURCES: [&str; 4] = ["cli", "vscode", "exec", "appServer"];

/// How to reach a Codex installation.
#[derive(Debug, Clone)]
pub struct CodexConfig {
    /// The `codex` executable; a bare name is resolved through `PATH`.
    pub executable: PathBuf,
    /// Include models Codex hides from its default picker.
    pub include_hidden: bool,
    /// Deadline for starting the app-server and completing the handshake.
    pub startup_timeout: Duration,
    /// Deadline for each request.
    pub request_timeout: Duration,
}

impl Default for CodexConfig {
    fn default() -> Self {
        Self {
            executable: PathBuf::from("codex"),
            include_hidden: false,
            startup_timeout: Duration::from_secs(30),
            request_timeout: Duration::from_secs(30),
        }
    }
}

/// The Codex provider for one execution context.
pub struct Codex {
    config: CodexConfig,
    connection: tokio::sync::Mutex<Option<Arc<AppServer>>>,
    /// Last catalog, used to validate settings and resolve fast-mode tiers.
    catalog: Mutex<Option<Catalog>>,
}

impl Codex {
    pub fn new(config: CodexConfig) -> Self {
        Self {
            config,
            connection: tokio::sync::Mutex::new(None),
            catalog: Mutex::new(None),
        }
    }

    pub fn config(&self) -> &CodexConfig {
        &self.config
    }

    /// Whether an app-server process is currently running.
    pub async fn is_connected(&self) -> bool {
        self.connection
            .lock()
            .await
            .as_ref()
            .is_some_and(|connection| connection.is_alive())
    }

    pub async fn catalog(&self) -> Result<Catalog> {
        let connection = self.connection().await?;
        let mut models = Vec::new();
        let mut cursor = None;
        for _ in 0..MAX_PAGES {
            let page: ModelListResponse = connection
                .request(
                    "model/list",
                    &ModelListParams {
                        cursor: cursor.take(),
                        limit: Some(PAGE_LIMIT),
                        include_hidden: Some(self.config.include_hidden),
                    },
                    self.config.request_timeout,
                )
                .await?;
            models.extend(page.data);
            match page.next_cursor {
                Some(next) => cursor = Some(next),
                None => break,
            }
        }
        let catalog = build_catalog(connection.user_agent().map(str::to_owned), models);
        *self.catalog.lock().expect("catalog lock") = Some(catalog.clone());
        Ok(catalog)
    }

    /// Stored sessions, most recently updated first.
    pub async fn list_sessions(&self, query: session::Query) -> Result<session::Page> {
        let response: types::ThreadListResponse = self
            .request(
                "thread/list",
                &types::ThreadListParams {
                    cursor: query.cursor,
                    limit: query.limit,
                    sort_key: "updated_at",
                    cwd: query.cwd.map(|cwd| cwd.to_string_lossy().into_owned()),
                    source_kinds: SESSION_SOURCES.to_vec(),
                },
            )
            .await?;
        Ok(session::Page {
            sessions: response.data.into_iter().map(history::summary).collect(),
            next_cursor: response.next_cursor,
        })
    }

    pub async fn read_session(&self, id: &str) -> Result<session::Summary> {
        let response: types::ThreadReadResponse = self
            .request("thread/read", &types::ThreadIdParams { thread_id: id })
            .await
            .map_err(|error| unknown_session(id, error))?;
        Ok(history::summary(response.thread))
    }

    /// Stored turns, oldest first. Works without loading the session.
    pub async fn list_turns(&self, id: &str, query: turn::Query) -> Result<turn::Page> {
        let response: types::ThreadTurnsListResponse = self
            .request(
                "thread/turns/list",
                &types::ThreadTurnsListParams {
                    thread_id: id,
                    cursor: query.cursor,
                    limit: query.limit,
                    sort_direction: "asc",
                    items_view: "full",
                },
            )
            .await
            .map_err(|error| unknown_session(id, error))?;
        Ok(turn::Page {
            turns: response.data.into_iter().map(history::turn).collect(),
            next_cursor: response.next_cursor,
        })
    }

    /// Starts a new thread. Codex keeps it in memory until its first turn.
    pub async fn start_session(&self, spec: session::Spec) -> Result<Session> {
        let settings = self.settings_params(&spec.settings, true).await?;
        let connection = self.connection().await?;
        let response: types::ThreadSessionResponse = connection
            .request(
                "thread/start",
                &types::ThreadStartParams {
                    cwd: spec.cwd.to_string_lossy().into_owned(),
                    settings,
                },
                self.config.request_timeout,
            )
            .await?;
        Ok(self.open(connection, response, false))
    }

    /// Loads a stored thread into the app-server so it can take new turns.
    pub async fn resume_session(&self, id: &str, settings: session::Settings) -> Result<Session> {
        let settings = self.settings_params(&settings, false).await?;
        let connection = self.connection().await?;
        let response: types::ThreadSessionResponse = connection
            .request(
                "thread/resume",
                &types::ThreadResumeParams {
                    thread_id: id,
                    exclude_turns: true,
                    settings,
                },
                self.config.request_timeout,
            )
            .await
            .map_err(|error| match error {
                // Codex allows one writer per thread across all processes.
                Error::Rpc { message, .. } if message.contains("active writer") => {
                    Error::SessionInUse {
                        id: id.to_owned(),
                        message,
                    }
                }
                other => unknown_session(id, other),
            })?;
        Ok(self.open(connection, response, true))
    }

    pub async fn shutdown(&self) {
        let connection = self.connection.lock().await.take();
        if let Some(connection) = connection {
            connection.shutdown().await;
        }
    }

    async fn connection(&self) -> Result<Arc<AppServer>> {
        let mut slot = self.connection.lock().await;
        if let Some(connection) = slot.as_ref().filter(|connection| connection.is_alive()) {
            return Ok(Arc::clone(connection));
        }
        let timeout = self.config.startup_timeout;
        let connection = tokio::time::timeout(
            timeout,
            AppServer::start(&self.config.executable, self.config.request_timeout),
        )
        .await
        .map_err(|_| Error::Timeout(timeout))??;
        let connection = Arc::new(connection);
        *slot = Some(Arc::clone(&connection));
        Ok(connection)
    }

    async fn request<P, R>(&self, method: &str, params: &P) -> Result<R>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        self.connection()
            .await?
            .request(method, params, self.config.request_timeout)
            .await
    }

    async fn known_catalog(&self) -> Result<Catalog> {
        let cached = self.catalog.lock().expect("catalog lock").clone();
        match cached {
            Some(catalog) => Ok(catalog),
            None => self.catalog().await,
        }
    }

    /// Validates requested settings against the catalog and translates them.
    /// Without an explicit model, a new thread is checked against the default
    /// model; a resumed thread keeps its stored model, so only an explicit
    /// fast-mode request needs one.
    async fn settings_params(
        &self,
        settings: &session::Settings,
        new_thread: bool,
    ) -> Result<ThreadSettingsParams> {
        let needs_catalog = settings.model.is_some()
            || settings.fast_mode == Some(true)
            || (new_thread && settings.effort.is_some());
        let model = if needs_catalog {
            let catalog = self.known_catalog().await?;
            let model = match settings.model.as_deref() {
                Some(id) => catalog.models.iter().find(|model| model.id == id).cloned(),
                None if new_thread => catalog
                    .models
                    .iter()
                    .find(|model| model.is_default)
                    .cloned(),
                None => None,
            };
            if model.is_none() && (settings.model.is_some() || settings.fast_mode == Some(true)) {
                return Err(Error::InvalidSetting {
                    setting: "model",
                    value: settings.model.clone().unwrap_or_default(),
                    reason:
                        "not in Codex's model list; fast mode needs an explicit model when resuming"
                            .to_owned(),
                });
            }
            model
        } else {
            None
        };

        if let (Some(effort), Some(model)) = (&settings.effort, &model)
            && !model.efforts.iter().any(|option| &option.id == effort)
        {
            return Err(Error::InvalidSetting {
                setting: "effort",
                value: effort.clone(),
                reason: format!("{} does not support it", model.id),
            });
        }

        let service_tier = match settings.fast_mode {
            None => None,
            Some(false) => Some(None),
            Some(true) => match model.as_ref().map(|model| &model.fast_mode) {
                Some(FastMode::Available { tier_id, .. }) => Some(Some(tier_id.clone())),
                _ => {
                    return Err(Error::InvalidSetting {
                        setting: "fast_mode",
                        value: "true".to_owned(),
                        reason: "the model has no fast tier".to_owned(),
                    });
                }
            },
        };

        Ok(ThreadSettingsParams {
            model: settings.model.clone(),
            service_tier,
            config: settings.effort.as_ref().map(|effort| {
                HashMap::from([(
                    "model_reasoning_effort".to_owned(),
                    Value::String(effort.clone()),
                )])
            }),
        })
    }

    fn open(
        &self,
        connection: Arc<AppServer>,
        response: types::ThreadSessionResponse,
        restored: bool,
    ) -> Session {
        let fast_tier = self
            .catalog
            .lock()
            .expect("catalog lock")
            .as_ref()
            .and_then(|catalog| {
                catalog
                    .models
                    .iter()
                    .find(|model| model.id == response.model)
            })
            .and_then(|model| match &model.fast_mode {
                FastMode::Available { tier_id, .. } => Some(tier_id.clone()),
                FastMode::Unavailable => None,
            });
        let fast_mode = match (&response.service_tier, fast_tier) {
            (Some(tier), Some(fast_tier)) => *tier == fast_tier,
            (Some(_), None) => true,
            (None, _) => false,
        };

        let thread_id = response.thread.id.clone();
        let info = session::Info {
            model: response.model,
            effort: response.reasoning_effort,
            fast_mode,
            cwd: response.cwd,
            approval_policy: history::describe(&response.approval_policy),
            sandbox: history::describe(&response.sandbox),
            restored,
            persisted: restored,
            summary: history::summary(response.thread),
        };
        let observation = connection.observe(&info);
        let driver = CodexSession {
            connection,
            thread_id,
            observation,
            request_timeout: self.config.request_timeout,
        };
        Session::new(info, Arc::new(driver))
    }
}

struct CodexSession {
    connection: Arc<AppServer>,
    thread_id: String,
    observation: Arc<observation::Observation>,
    request_timeout: Duration,
}

impl CodexSession {
    async fn stop(&self, target: crate::control::Target) -> Result<crate::control::Receipt> {
        let (receipt, request) = self.observation.begin_stop(target)?;
        let Some(request) = request else {
            return Ok(receipt);
        };
        let result: Result<bool> = match request {
            observation::controls::Request::Agent { id, turn_id } => self
                .connection
                .request::<_, Value>(
                    "turn/interrupt",
                    &types::TurnInterruptParams {
                        thread_id: &id,
                        turn_id: &turn_id,
                    },
                    self.request_timeout,
                )
                .await
                .map(|_| true),
            observation::controls::Request::Task {
                owner,
                item_id,
                process_id,
            } => {
                // Check native item/process membership before sending the private
                // reference. The native protocol has no atomic item guard.
                match connection::terminal_list(&self.connection, &owner, self.request_timeout)
                    .await
                {
                    Ok(terminals)
                        if terminals.iter().any(|terminal| {
                            terminal.item_id == item_id && terminal.process_id == process_id
                        }) =>
                    {
                        #[derive(serde::Deserialize)]
                        struct Response {
                            terminated: bool,
                        }
                        self.connection
                            .request::<_, Response>(
                                "thread/backgroundTerminals/terminate",
                                &serde_json::json!({"threadId":owner,"processId":process_id}),
                                self.request_timeout,
                            )
                            .await
                            .map(|response| response.terminated)
                    }
                    Ok(_) => Ok(false),
                    Err(error) => Err(error),
                }
            }
        };
        let receipt = self
            .observation
            .stop_result(&receipt.id, result.as_ref().copied());
        result.map(|_| receipt)
    }
}

impl session::Driver for CodexSession {
    fn subscribe(&self) -> broadcast::Receiver<session::Event> {
        self.observation.subscribe()
    }

    fn subscribe_records(&self) -> Result<broadcast::Receiver<crate::observation::Record>> {
        Ok(self.observation.subscribe_records())
    }

    fn observation_capabilities(&self) -> Result<crate::observation::Capabilities> {
        Ok(self.observation.capabilities())
    }

    fn stop_agent<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<crate::control::Receipt>> {
        Box::pin(self.stop(crate::control::Target::Agent { id: id.into() }))
    }
    fn stop_task<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<crate::control::Receipt>> {
        Box::pin(self.stop(crate::control::Target::Task { id: id.into() }))
    }

    fn send(&self, input: turn::Input) -> BoxFuture<'_, Result<turn::Receipt>> {
        Box::pin(async move {
            let response: types::TurnStartResponse = self
                .connection
                .request(
                    "turn/start",
                    &types::TurnStartParams {
                        thread_id: &self.thread_id,
                        input: vec![types::UserInput::Text { text: &input.text }],
                    },
                    self.request_timeout,
                )
                .await?;
            Ok(turn::Receipt {
                turn_id: response.turn.id,
            })
        })
    }

    fn interrupt<'a>(&'a self, turn_id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.connection
                .request::<_, Value>(
                    "turn/interrupt",
                    &types::TurnInterruptParams {
                        thread_id: &self.thread_id,
                        turn_id,
                    },
                    self.request_timeout,
                )
                .await
                .map(|_| ())
        })
    }

    fn close(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            // A dead connection already announced `Closed { Exited }`.
            if !self.connection.is_alive() {
                self.connection.detach(&self.thread_id, &self.observation);
                return Ok(());
            }
            crate::rpc::EventSink::emit(
                &*self.observation,
                session::Event::Closed {
                    reason: session::CloseReason::Client,
                },
            );
            if !self.connection.detach(&self.thread_id, &self.observation) {
                return Ok(());
            }
            self.connection
                .request::<_, Value>(
                    "thread/unsubscribe",
                    &types::ThreadIdParams {
                        thread_id: &self.thread_id,
                    },
                    self.request_timeout,
                )
                .await
                .map(|_| ())
        })
    }
}

impl Provider for Codex {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn catalog(&self) -> BoxFuture<'_, Result<Catalog>> {
        Box::pin(Codex::catalog(self))
    }

    fn list_sessions(&self, query: session::Query) -> BoxFuture<'_, Result<session::Page>> {
        Box::pin(Codex::list_sessions(self, query))
    }

    fn read_session<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<session::Summary>> {
        Box::pin(Codex::read_session(self, id))
    }

    fn list_turns<'a>(
        &'a self,
        id: &'a str,
        query: turn::Query,
    ) -> BoxFuture<'a, Result<turn::Page>> {
        Box::pin(Codex::list_turns(self, id, query))
    }

    fn start_session(&self, spec: session::Spec) -> BoxFuture<'_, Result<Session>> {
        Box::pin(Codex::start_session(self, spec))
    }

    fn resume_session<'a>(
        &'a self,
        id: &'a str,
        settings: session::Settings,
    ) -> BoxFuture<'a, Result<Session>> {
        Box::pin(Codex::resume_session(self, id, settings))
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(Codex::shutdown(self))
    }
}

/// Codex reports an unknown or malformed thread id only in its error
/// message; Keel reports it as [`Error::SessionNotFound`].
fn unknown_session(id: &str, error: Error) -> Error {
    const UNKNOWN: [&str; 4] = [
        "thread not loaded",
        "no rollout found",
        "invalid thread id",
        "invalid session id",
    ];
    match error {
        Error::Rpc { message, .. } if UNKNOWN.iter().any(|prefix| message.starts_with(prefix)) => {
            Error::SessionNotFound { id: id.to_owned() }
        }
        other => other,
    }
}

fn build_catalog(user_agent: Option<String>, models: Vec<types::Model>) -> Catalog {
    Catalog {
        provider: ProviderInfo {
            id: PROVIDER_ID.to_owned(),
            version: user_agent.as_deref().and_then(version_from_user_agent),
            user_agent,
        },
        models: models.into_iter().map(model_info).collect(),
    }
}

fn model_info(model: types::Model) -> catalog::Model {
    let fast_mode = fast_mode(&model);
    catalog::Model {
        id: model.id,
        label: model.display_name,
        description: model.description,
        is_default: model.is_default,
        efforts: model
            .supported_reasoning_efforts
            .into_iter()
            .map(|effort| catalog::Effort {
                id: effort.reasoning_effort,
                description: effort.description,
            })
            .collect(),
        default_effort: model.default_reasoning_effort,
        fast_mode,
    }
}

/// Codex advertises speed as service tiers; the tier named "Fast" (currently
/// id `priority`) is fast mode. Older releases only list `fast` in the
/// deprecated `additionalSpeedTiers` field.
fn fast_mode(model: &types::Model) -> FastMode {
    let tier = model
        .service_tiers
        .iter()
        .find(|tier| tier.name.eq_ignore_ascii_case("fast") || tier.id == "fast");
    if let Some(tier) = tier {
        return FastMode::Available {
            tier_id: tier.id.clone(),
            label: tier.name.clone(),
            description: tier.description.clone(),
            default_on: model.default_service_tier.as_deref() == Some(tier.id.as_str()),
        };
    }
    if model
        .additional_speed_tiers
        .iter()
        .any(|tier| tier == "fast")
    {
        return FastMode::Available {
            tier_id: "fast".to_owned(),
            label: "Fast".to_owned(),
            description: String::new(),
            default_on: model.default_service_tier.as_deref() == Some("fast"),
        };
    }
    FastMode::Unavailable
}

/// The user agent starts with `<client>/<codex version>`.
fn version_from_user_agent(user_agent: &str) -> Option<String> {
    let product = user_agent.split_whitespace().next()?;
    let (_, version) = product.split_once('/')?;
    Some(version.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("fixtures/codex-model-list-0.159.3.json");

    #[test]
    fn reports_unknown_threads_as_not_found() {
        let rpc = |message: &str| Error::Rpc {
            method: "thread/read".to_owned(),
            code: -32600,
            message: message.to_owned(),
        };
        assert!(matches!(
            unknown_session("t1", rpc("thread not loaded: t1")),
            Error::SessionNotFound { id } if id == "t1"
        ));
        assert!(matches!(
            unknown_session("t1", rpc("no rollout found for thread id t1")),
            Error::SessionNotFound { .. }
        ));
        assert!(matches!(
            unknown_session("t1", rpc("rate limited")),
            Error::Rpc { .. }
        ));
    }

    fn fixture_catalog() -> Catalog {
        let page: ModelListResponse = serde_json::from_str(FIXTURE).expect("fixture decodes");
        build_catalog(
            Some("keel/0.159.3 (NixOS 26.11.0; x86_64) unknown (keel; 0.1.0)".to_owned()),
            page.data,
        )
    }

    fn model<'a>(catalog: &'a Catalog, id: &str) -> &'a catalog::Model {
        catalog
            .models
            .iter()
            .find(|model| model.id == id)
            .unwrap_or_else(|| panic!("model {id} present"))
    }

    fn effort_ids(model: &catalog::Model) -> Vec<&str> {
        model
            .efforts
            .iter()
            .map(|effort| effort.id.as_str())
            .collect()
    }

    #[test]
    fn maps_every_model_with_one_default() {
        let catalog = fixture_catalog();
        assert_eq!(catalog.models.len(), 8);
        let defaults: Vec<_> = catalog.models.iter().filter(|m| m.is_default).collect();
        assert_eq!(defaults.len(), 1);
        assert_eq!(defaults[0].id, "gpt-6.1-sol");
        assert_eq!(catalog.provider.version.as_deref(), Some("0.159.3"));
    }

    #[test]
    fn efforts_depend_on_the_model() {
        let catalog = fixture_catalog();
        assert_eq!(
            effort_ids(model(&catalog, "gpt-5.5")),
            ["low", "medium", "high", "xhigh"]
        );
        assert!(!effort_ids(model(&catalog, "gpt-6-luna")).contains(&"ultra"));
        assert!(effort_ids(model(&catalog, "gpt-6.1-sol")).contains(&"ultra"));
        assert_eq!(
            model(&catalog, "gpt-6-luna").default_effort.as_deref(),
            Some("medium")
        );
    }

    #[test]
    fn fast_mode_comes_from_service_tiers() {
        let catalog = fixture_catalog();
        match &model(&catalog, "gpt-6-sol").fast_mode {
            FastMode::Available {
                tier_id,
                label,
                default_on,
                ..
            } => {
                assert_eq!(tier_id, "priority");
                assert_eq!(label, "Fast");
                assert!(*default_on);
            }
            FastMode::Unavailable => panic!("gpt-6-sol supports fast mode"),
        }
        assert!(matches!(
            model(&catalog, "gpt-6.1-sol").fast_mode,
            FastMode::Available {
                default_on: false,
                ..
            }
        ));
    }

    #[test]
    fn reads_version_from_user_agent() {
        assert_eq!(version_from_user_agent("unknown").as_deref(), None);
        assert_eq!(
            version_from_user_agent("keel/0.159.0 (Linux; x86_64)").as_deref(),
            Some("0.159.0")
        );
    }
}
