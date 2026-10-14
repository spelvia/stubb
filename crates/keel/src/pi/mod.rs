//! Native Pi coding agent integration.
//!
//! Keel runs the installed `pi` executable in RPC mode (`pi --mode rpc`),
//! Pi's documented protocol for embedding it in other programs. Each live
//! session is its own `pi` process. Stored sessions are read directly from
//! Pi's session files. All mapping onto Keel's types happens here.
//!
//! Pi reaches models through the providers configured in its own settings,
//! so a Keel model id is `<provider>/<model id>`, for example
//! `anthropic/claude-sonnet-4-5`. Reasoning effort is Pi's thinking level.
//!
//! Pi has no approval step: its tools run without asking, so unlike Codex
//! and Claude Code, a Pi session executes commands and edits files.

mod events;
mod history;
mod process;
mod store;
mod tools;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::process::Command;
use tokio::sync::broadcast;

use crate::catalog::{self, Catalog, FastMode, ProviderInfo};
use crate::files::{self, blocking, cursor_offset, now};
use crate::process::Environment;
use crate::provider::BoxFuture;
use crate::session::{self, Session};
use crate::{Error, Provider, Result, turn};
use process::{Conversation, Launch, PiProcess};
use store::Store;

pub(crate) const PROVIDER_ID: &str = "pi";
const DEFAULT_PAGE: u32 = 50;
const DEFAULT_TURN_PAGE: usize = 20;
/// Pi's thinking levels, lowest first.
const THINKING_LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// How to reach the Pi installation.
#[derive(Debug, Clone)]
pub struct PiConfig {
    /// The `pi` executable; a bare name is resolved through `PATH`.
    pub executable: PathBuf,
    /// Pi's agent directory, holding its settings and sessions. Defaults to
    /// `$PI_CODING_AGENT_DIR`, then `~/.pi/agent`. When set, it is also
    /// passed to `pi`.
    pub agent_dir: Option<PathBuf>,
    /// Deadline for starting Pi and answering its first command.
    pub startup_timeout: Duration,
    /// Deadline for each command, such as a prompt or an abort.
    pub request_timeout: Duration,
    /// The environment of each `pi` process: the host's whole environment
    /// by default.
    pub environment: Environment,
}

impl Default for PiConfig {
    fn default() -> Self {
        Self {
            executable: PathBuf::from("pi"),
            agent_dir: None,
            startup_timeout: Duration::from_secs(30),
            request_timeout: Duration::from_secs(30),
            environment: Environment::inherit(),
        }
    }
}

/// The Pi provider.
pub struct Pi {
    config: PiConfig,
    store: Arc<Store>,
    /// Live session processes, so `shutdown` can stop them.
    live: Mutex<HashMap<String, Weak<PiProcess>>>,
}

impl Pi {
    pub fn new(config: PiConfig) -> Self {
        // Where the process will look, with the environment it gets.
        let env = &config.environment;
        let agent_dir = config
            .agent_dir
            .clone()
            .or_else(|| env.value("PI_CODING_AGENT_DIR").map(PathBuf::from))
            .or_else(|| {
                env.value("HOME")
                    .map(|home| Path::new(&home).join(".pi/agent"))
            })
            .unwrap_or_else(|| PathBuf::from(".pi/agent"));
        Self {
            store: Arc::new(Store::new(&agent_dir)),
            config,
            live: Mutex::new(HashMap::new()),
        }
    }

    pub fn config(&self) -> &PiConfig {
        &self.config
    }

    /// Models configured in Pi, read from a short-lived `pi` process.
    pub async fn catalog(&self) -> Result<Catalog> {
        let process = self.spawn(&std::env::temp_dir(), Conversation::None)?;
        let version = tokio::spawn(version(
            self.config.executable.clone(),
            self.config.environment.clone(),
        ));
        let loaded = async {
            let state = process
                .command(json!({ "type": "get_state" }), self.config.startup_timeout)
                .await?;
            let models = process
                .command(
                    json!({ "type": "get_available_models" }),
                    self.config.request_timeout,
                )
                .await?;
            Ok::<_, Error>((state, models))
        }
        .await;
        process.close().await;
        let (state, models) = loaded?;
        let version = version.await.ok().flatten();

        let default = state.get("model").map(model_id);
        let default_level = state.get("thinkingLevel").and_then(Value::as_str);
        let models = models.get("models").and_then(Value::as_array);
        Ok(Catalog {
            provider: ProviderInfo {
                id: PROVIDER_ID.to_owned(),
                version,
                user_agent: None,
            },
            models: models
                .into_iter()
                .flatten()
                .map(|model| catalog_model(model, default.as_deref(), default_level))
                .collect(),
        })
    }

    /// Stored sessions, most recently modified first.
    pub async fn list_sessions(&self, query: session::Query) -> Result<session::Page> {
        let store = Arc::clone(&self.store);
        let limit = query.limit.unwrap_or(DEFAULT_PAGE) as usize;
        let offset = cursor_offset(query.cursor.as_deref());
        blocking(move || {
            let files = store.list(query.cwd.as_deref())?;
            let page = files.iter().skip(offset).take(limit);
            let sessions = page
                .map(|file| Ok(store::summary(file, &files::read_entries(&file.path)?)))
                .collect::<std::io::Result<Vec<_>>>()?;
            let next = offset + sessions.len();
            Ok(session::Page {
                next_cursor: (next < files.len()).then(|| next.to_string()),
                sessions,
            })
        })
        .await
    }

    pub async fn read_session(&self, id: &str) -> Result<session::Summary> {
        let (file, entries) = self.load(id).await?;
        Ok(store::summary(&file, &entries))
    }

    /// Stored turns of the session's active branch, oldest first.
    pub async fn list_turns(&self, id: &str, query: turn::Query) -> Result<turn::Page> {
        let (_, entries) = self.load(id).await?;
        let turns = history::turns(&entries);
        let start = cursor_offset(query.cursor.as_deref()).min(turns.len());
        let end = (start
            + query
                .limit
                .map_or(DEFAULT_TURN_PAGE, |limit| limit as usize))
        .min(turns.len());
        Ok(turn::Page {
            next_cursor: (end < turns.len()).then(|| end.to_string()),
            turns: turns.into_iter().skip(start).take(end - start).collect(),
        })
    }

    /// Starts a new session. Pi stores it after its first turn.
    pub async fn start_session(&self, spec: session::Spec) -> Result<Session> {
        if spec.settings.permissions.is_some() {
            return Err(Error::Unsupported {
                operation: "permission_configuration",
            });
        }
        let id = uuid::Uuid::new_v4().to_string();
        let summary = session::Summary {
            id: id.clone(),
            provider: PROVIDER_ID.to_owned(),
            title: None,
            preview: String::new(),
            cwd: spec.cwd.clone(),
            model: None,
            effort: None,
            status: session::Status::Idle,
            created_at: now(),
            updated_at: now(),
            source: None,
        };
        self.open(Conversation::New { id: &id }, summary, spec.settings, false)
            .await
    }

    /// Loads a stored session into a new `pi` process in the session's
    /// working directory.
    pub async fn resume_session(&self, id: &str, settings: session::Settings) -> Result<Session> {
        if settings.permissions.is_some() {
            return Err(Error::Unsupported {
                operation: "permission_configuration",
            });
        }
        let (file, entries) = self.load(id).await?;
        let mut summary = store::summary(&file, &entries);
        summary.status = session::Status::Idle;
        self.open(
            Conversation::Resume { path: &file.path },
            summary,
            settings,
            true,
        )
        .await
    }

    /// Stops every live session process.
    pub async fn shutdown(&self) {
        let live: Vec<_> = self
            .live
            .lock()
            .expect("live lock")
            .drain()
            .filter_map(|(_, process)| process.upgrade())
            .collect();
        for process in live {
            process.close().await;
        }
    }

    fn spawn(&self, cwd: &Path, conversation: Conversation<'_>) -> Result<PiProcess> {
        PiProcess::spawn(&Launch {
            executable: &self.config.executable,
            agent_dir: self.config.agent_dir.as_deref(),
            environment: &self.config.environment,
            cwd,
            conversation,
        })
    }

    async fn open(
        &self,
        conversation: Conversation<'_>,
        mut summary: session::Summary,
        settings: session::Settings,
        restored: bool,
    ) -> Result<Session> {
        if settings.fast_mode == Some(true) {
            return Err(Error::InvalidSetting {
                setting: "fast_mode",
                value: "true".to_owned(),
                reason: "Pi has no fast mode".to_owned(),
            });
        }
        let process = self.spawn(&summary.cwd, conversation)?;
        let state = match self.apply(&process, &settings).await {
            Ok(state) => state,
            Err(error) => {
                process.close().await;
                return Err(error);
            }
        };
        let model = state.get("model");
        process.set_context_window(
            model
                .and_then(|model| model.get("contextWindow"))
                .and_then(Value::as_i64),
        );
        let process = Arc::new(process);
        {
            let mut live = self.live.lock().expect("live lock");
            live.retain(|_, process| process.strong_count() > 0);
            live.insert(summary.id.clone(), Arc::downgrade(&process));
        }

        let reasoning = model
            .and_then(|model| model.get("reasoning"))
            .and_then(Value::as_bool);
        let effort = state
            .get("thinkingLevel")
            .and_then(Value::as_str)
            .filter(|_| reasoning == Some(true))
            .map(str::to_owned);
        let model = model.map(model_id).unwrap_or_else(|| "default".to_owned());
        summary.model = Some(model.clone());
        summary.effort = effort.clone();
        let info = session::Info {
            model,
            effort,
            fast_mode: false,
            cwd: summary.cwd.clone(),
            permissions: None,
            approval_policy: None,
            sandbox: None,
            restored,
            persisted: restored,
            summary,
        };
        let driver = PiSession {
            process,
            request_timeout: self.config.request_timeout,
        };
        Ok(Session::new(info, Arc::new(driver)))
    }

    /// Applies the requested model and thinking level, and returns Pi's
    /// state afterwards. Pi silently clamps unsupported thinking levels, so
    /// Keel checks them first.
    async fn apply(&self, process: &PiProcess, settings: &session::Settings) -> Result<Value> {
        let get_state = json!({ "type": "get_state" });
        let mut state = process
            .command(get_state.clone(), self.config.startup_timeout)
            .await?;
        let timeout = self.config.request_timeout;
        let current = state.get("model").map(model_id);
        if let Some(model) = &settings.model
            && current.as_ref() != Some(model)
        {
            let invalid = |reason: String| Error::InvalidSetting {
                setting: "model",
                value: model.clone(),
                reason,
            };
            let Some((provider, id)) = model.split_once('/') else {
                return Err(invalid("expected <provider>/<model id>".to_owned()));
            };
            let set = json!({ "type": "set_model", "provider": provider, "modelId": id });
            process
                .command(set, timeout)
                .await
                .map_err(|error| match error {
                    Error::Rpc { message, .. } => invalid(message),
                    error => error,
                })?;
        }
        if let Some(effort) = &settings.effort {
            let levels = process
                .command(json!({ "type": "get_available_thinking_levels" }), timeout)
                .await?;
            let levels = levels.get("levels").and_then(Value::as_array);
            if !levels.into_iter().flatten().any(|level| level == effort) {
                return Err(Error::InvalidSetting {
                    setting: "effort",
                    value: effort.clone(),
                    reason: "the model does not support this thinking level".to_owned(),
                });
            }
            process
                .command(
                    json!({ "type": "set_thinking_level", "level": effort }),
                    timeout,
                )
                .await?;
        }
        if settings.model.is_some() || settings.effort.is_some() {
            state = process.command(get_state, timeout).await?;
        }
        Ok(state)
    }

    async fn load(&self, id: &str) -> Result<(store::SessionFile, Vec<Value>)> {
        let store = Arc::clone(&self.store);
        let id = id.to_owned();
        blocking(move || {
            let Some(file) = store.find(&id)? else {
                return Err(Error::SessionNotFound { id });
            };
            let entries = files::read_entries(&file.path)?;
            Ok((file, entries))
        })
        .await
    }
}

/// `pi --version`, e.g. `1.0.2`.
async fn version(executable: PathBuf, environment: Environment) -> Option<String> {
    let mut command = Command::new(executable);
    command.arg("--version");
    environment.apply(&mut command);
    let output = tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .ok()?
        .ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.split_whitespace().next().map(str::to_owned)
}

/// Keel's id for a Pi model object.
fn model_id(model: &Value) -> String {
    let text = |key: &str| model.get(key).and_then(Value::as_str).unwrap_or_default();
    store::model_id(text("provider"), text("id"))
}

fn catalog_model(
    model: &Value,
    default: Option<&str>,
    default_level: Option<&str>,
) -> catalog::Model {
    let id = model_id(model);
    let text = |key: &str| model.get(key).and_then(Value::as_str).unwrap_or_default();
    let efforts = thinking_levels(model);
    let description = match model.get("contextWindow").and_then(Value::as_i64) {
        Some(window) => format!("{}, {}K context", text("provider"), window / 1000),
        None => text("provider").to_owned(),
    };
    catalog::Model {
        is_default: default == Some(id.as_str()),
        label: if text("name").is_empty() {
            text("id").to_owned()
        } else {
            text("name").to_owned()
        },
        description,
        default_effort: default_level
            .filter(|level| efforts.iter().any(|effort| effort.id == *level))
            .map(str::to_owned),
        efforts,
        fast_mode: FastMode::Unavailable,
        id,
    }
}

/// The thinking levels Pi offers for a model, as Pi's
/// `getSupportedThinkingLevels` computes them: none without reasoning
/// support; `xhigh` and `max` only when the model maps them; any level the
/// model maps to `null` is unsupported.
fn thinking_levels(model: &Value) -> Vec<catalog::Effort> {
    if model.get("reasoning").and_then(Value::as_bool) != Some(true) {
        return Vec::new();
    }
    let map = model.get("thinkingLevelMap");
    THINKING_LEVELS
        .iter()
        .filter(|level| match map.and_then(|map| map.get(**level)) {
            Some(Value::Null) => false,
            Some(_) => true,
            None => !matches!(**level, "xhigh" | "max"),
        })
        .map(|level| catalog::Effort {
            id: (*level).to_owned(),
            description: String::new(),
        })
        .collect()
}

struct PiSession {
    process: Arc<PiProcess>,
    request_timeout: Duration,
}

impl session::Driver for PiSession {
    fn subscribe(&self) -> broadcast::Receiver<session::Event> {
        self.process.subscribe()
    }

    fn send(&self, input: turn::Input) -> BoxFuture<'_, Result<turn::Receipt>> {
        Box::pin(async move {
            if !self.process.is_alive() {
                return Err(Error::ProcessExited {
                    stderr: "the Pi process for this session has exited".to_owned(),
                });
            }
            let turn_id = self
                .process
                .send_user(&input.text, self.request_timeout)
                .await?;
            Ok(turn::Receipt { turn_id })
        })
    }

    /// Pi aborts whatever is running; there is only ever one turn.
    fn interrupt<'a>(&'a self, _turn_id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.process
                .command(json!({ "type": "abort" }), self.request_timeout)
                .await
                .map(|_| ())
        })
    }

    fn close(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.process.close().await;
            Ok(())
        })
    }
}

impl Provider for Pi {
    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn catalog(&self) -> BoxFuture<'_, Result<Catalog>> {
        Box::pin(Pi::catalog(self))
    }

    fn list_sessions(&self, query: session::Query) -> BoxFuture<'_, Result<session::Page>> {
        Box::pin(Pi::list_sessions(self, query))
    }

    fn read_session<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<session::Summary>> {
        Box::pin(Pi::read_session(self, id))
    }

    fn list_turns<'a>(
        &'a self,
        id: &'a str,
        query: turn::Query,
    ) -> BoxFuture<'a, Result<turn::Page>> {
        Box::pin(Pi::list_turns(self, id, query))
    }

    fn start_session(&self, spec: session::Spec) -> BoxFuture<'_, Result<Session>> {
        Box::pin(Pi::start_session(self, spec))
    }

    fn resume_session<'a>(
        &'a self,
        id: &'a str,
        settings: session::Settings,
    ) -> BoxFuture<'a, Result<Session>> {
        Box::pin(Pi::resume_session(self, id, settings))
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(Pi::shutdown(self))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_thinking_levels_like_pi() {
        let ids = |model: Value| -> Vec<String> {
            thinking_levels(&model)
                .into_iter()
                .map(|effort| effort.id)
                .collect()
        };
        assert!(ids(json!({ "reasoning": false })).is_empty());
        assert_eq!(
            ids(json!({ "reasoning": true })),
            ["off", "minimal", "low", "medium", "high"]
        );
        assert_eq!(
            ids(
                json!({ "reasoning": true, "thinkingLevelMap": { "minimal": null, "xhigh": "xhigh" } })
            ),
            ["off", "low", "medium", "high", "xhigh"]
        );
    }

    #[test]
    fn catalogs_a_model() {
        let model = json!({ "id": "qwen", "name": "Qwen", "provider": "vllm", "reasoning": true, "contextWindow": 131_072 });
        let model = catalog_model(&model, Some("vllm/qwen"), Some("medium"));
        assert_eq!(model.id, "vllm/qwen");
        assert!(model.is_default);
        assert_eq!(model.description, "vllm, 131K context");
        assert_eq!(model.default_effort.as_deref(), Some("medium"));
    }
}
