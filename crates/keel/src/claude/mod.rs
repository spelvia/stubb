//! Native Claude Code integration.
//!
//! Keel runs the installed `claude` executable in stream-json mode, the same
//! protocol the official Claude Agent SDKs use, so nothing besides Claude
//! Code is needed on the machine. Each live session is its own `claude`
//! process. Stored sessions are read directly from Claude Code's transcript
//! files. All mapping onto Keel's types happens here.

mod agents;
mod events;
#[cfg(test)]
mod fixtures;
mod history;
mod live;
mod output;
mod process;
mod store;
mod stored_tasks;
mod tools;
mod usage;

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
use process::{ClaudeProcess, Conversation, Launch};
use store::Store;

pub(crate) const PROVIDER_ID: &str = "claude";
const DEFAULT_PAGE: u32 = 50;
const DEFAULT_TURN_PAGE: usize = 20;

/// How to reach the Claude Code installation.
#[derive(Debug, Clone)]
pub struct ClaudeConfig {
    /// The `claude` executable; a bare name is resolved through `PATH`.
    pub executable: PathBuf,
    /// Claude Code's configuration directory, where sessions are stored.
    /// Defaults to `$CLAUDE_CONFIG_DIR`, then `~/.claude`.
    pub config_dir: Option<PathBuf>,
    /// Deadline for starting Claude Code and completing the handshake.
    pub startup_timeout: Duration,
    /// Deadline for each control request, such as an interrupt.
    pub request_timeout: Duration,
    /// The environment of each `claude` process: the host's whole
    /// environment by default.
    pub environment: Environment,
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self {
            executable: PathBuf::from("claude"),
            config_dir: None,
            startup_timeout: Duration::from_secs(30),
            request_timeout: Duration::from_secs(30),
            environment: Environment::inherit(),
        }
    }
}

/// The Claude Code provider.
pub struct Claude {
    config: ClaudeConfig,
    store: Arc<Store>,
    /// Last catalog, used to validate settings.
    catalog: Mutex<Option<Catalog>>,
    /// Live session processes, so `shutdown` can stop them.
    live: Mutex<HashMap<String, Weak<ClaudeProcess>>>,
}

impl Claude {
    pub fn new(config: ClaudeConfig) -> Self {
        // Where the process will look, with the environment it gets.
        let env = &config.environment;
        let config_dir = config
            .config_dir
            .clone()
            .or_else(|| env.value("CLAUDE_CONFIG_DIR").map(PathBuf::from))
            .or_else(|| {
                env.value("HOME")
                    .map(|home| Path::new(&home).join(".claude"))
            })
            .unwrap_or_else(|| PathBuf::from(".claude"));
        Self {
            store: Arc::new(Store::new(&config_dir)),
            config,
            catalog: Mutex::new(None),
            live: Mutex::new(HashMap::new()),
        }
    }

    pub fn config(&self) -> &ClaudeConfig {
        &self.config
    }

    /// Reads account quotas through Claude Code's experimental `get_usage`
    /// control request. This short-lived process never sends a user message.
    /// Native CLIs without the operation return an RPC error.
    pub async fn account_usage(&self) -> Result<crate::usage::Account> {
        let cwd = std::env::temp_dir();
        let process = ClaudeProcess::spawn(&Launch {
            executable: &self.config.executable,
            config_dir: self.config.config_dir.as_deref(),
            environment: &self.config.environment,
            cwd: &cwd,
            conversation: Conversation::None,
            model: None,
            effort: None,
            fast_mode: None,
            forward_requests: false,
            additional_directories: &[],
        })?;
        let result = async {
            process
                .control(json!({"subtype":"initialize"}), self.config.startup_timeout)
                .await?;
            let value = process
                .control(json!({"subtype":"get_usage"}), self.config.request_timeout)
                .await?;
            if !value.is_object() {
                return Err(Error::Unsupported {
                    operation: "account_usage",
                });
            }
            Ok(usage::account(value))
        }
        .await;
        process.close().await;
        result
    }

    /// Models Claude Code offers, read from a short-lived `claude` process.
    /// Claude also accepts full model ids that are not listed.
    pub async fn catalog(&self) -> Result<Catalog> {
        let cwd = std::env::temp_dir();
        let process = ClaudeProcess::spawn(&Launch {
            executable: &self.config.executable,
            config_dir: self.config.config_dir.as_deref(),
            environment: &self.config.environment,
            cwd: &cwd,
            conversation: Conversation::None,
            model: None,
            effort: None,
            fast_mode: None,
            forward_requests: false,
            additional_directories: &[],
        })?;
        let version = tokio::spawn(version(
            self.config.executable.clone(),
            self.config.environment.clone(),
        ));
        let init = process
            .control(
                json!({ "subtype": "initialize" }),
                self.config.startup_timeout,
            )
            .await;
        process.close().await;
        let init = init?;
        let version = version.await.ok().flatten();

        // An opt-in reason only means "not requested"; others mean fast mode
        // is unavailable to this account right now.
        let fast_note = init
            .get("fast_mode_disabled_reason")
            .and_then(Value::as_str)
            .filter(|reason| !reason.starts_with("sdk_opt_in"))
            .map(|reason| format!("Currently unavailable: {reason}"));
        let models = init.get("models").and_then(Value::as_array);
        let catalog = Catalog {
            provider: ProviderInfo {
                id: PROVIDER_ID.to_owned(),
                version,
                user_agent: None,
            },
            models: models
                .into_iter()
                .flatten()
                .map(|model| catalog_model(model, &fast_note))
                .collect(),
        };
        *self.catalog.lock().expect("catalog lock") = Some(catalog.clone());
        Ok(catalog)
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

    /// Stored turns, oldest first.
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

    /// Starts a new session. Claude Code stores it after its first turn.
    pub async fn start_session(&self, spec: session::Spec) -> Result<Session> {
        self.validate(&spec.settings).await?;
        let id = uuid::Uuid::new_v4().to_string();
        let summary = session::Summary {
            id: id.clone(),
            provider: PROVIDER_ID.to_owned(),
            title: None,
            preview: String::new(),
            cwd: spec.cwd.clone(),
            model: spec.settings.model.clone(),
            effort: spec.settings.effort.clone(),
            status: session::Status::Idle,
            created_at: now(),
            updated_at: now(),
            source: None,
        };
        self.open(Conversation::New { id: &id }, summary, spec.settings, false)
            .await
    }

    /// Loads a stored session into a new `claude` process in the session's
    /// working directory.
    pub async fn resume_session(&self, id: &str, settings: session::Settings) -> Result<Session> {
        self.validate(&settings).await?;
        let mut summary = self.read_session(id).await?;
        summary.status = session::Status::Idle;
        self.open(Conversation::Resume { id }, summary, settings, true)
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

    async fn open(
        &self,
        conversation: Conversation<'_>,
        summary: session::Summary,
        settings: session::Settings,
        restored: bool,
    ) -> Result<Session> {
        let bootstrap = if restored {
            let stored = self.stored(&summary.id).await?;
            Some(
                files::blocking(move || {
                    let tasks = stored_tasks::read(&stored)?.into_values().collect();
                    Ok((stored, tasks))
                })
                .await?,
            )
        } else {
            None
        };
        let process = ClaudeProcess::spawn(&Launch {
            executable: &self.config.executable,
            config_dir: self.config.config_dir.as_deref(),
            environment: &self.config.environment,
            cwd: &summary.cwd,
            conversation,
            model: settings.model.as_deref(),
            effort: settings.effort.as_deref(),
            fast_mode: settings.fast_mode,
            forward_requests: settings.forward_requests,
            additional_directories: &settings.additional_directories,
        })?;
        let init = match process
            .control(process::initialize_request(), self.config.startup_timeout)
            .await
        {
            Ok(init) => init,
            Err(error) => {
                process.close().await;
                return Err(error);
            }
        };
        let process = Arc::new(process);
        {
            let mut live = self.live.lock().expect("live lock");
            live.retain(|_, process| process.strong_count() > 0);
            live.insert(summary.id.clone(), Arc::downgrade(&process));
        }

        let text = |key: &str| init.get(key).and_then(Value::as_str).map(str::to_owned);
        let info = session::Info {
            model: settings
                .model
                .or_else(|| summary.model.clone())
                .unwrap_or_else(|| "default".to_owned()),
            // Claude Code does not report the effort back; this is what was requested.
            effort: settings.effort,
            fast_mode: text("fast_mode_state").as_deref() == Some("on"),
            cwd: summary.cwd.clone(),
            permissions: None,
            approval_policy: text("current_permission_mode"),
            sandbox: None,
            restored,
            persisted: restored,
            summary,
        };
        process.configure_observation(&info);
        if let Some((stored, tasks)) = bootstrap {
            process.restore_observation(stored, tasks);
        }

        let driver = ClaudeSession {
            process,
            session_id: info.summary.id.clone(),
            request_timeout: self.config.request_timeout,
        };
        Ok(Session::new(info, Arc::new(driver)))
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

    /// Checks settings against the catalog where it can. Model ids that are
    /// not listed are passed through, since Claude accepts full model ids;
    /// their effort is then not checked.
    async fn validate(&self, settings: &session::Settings) -> Result<()> {
        if settings.permissions.is_some() {
            return Err(Error::Unsupported {
                operation: "permission_configuration",
            });
        }
        if settings.model.is_none() && settings.effort.is_none() && settings.fast_mode != Some(true)
        {
            return Ok(());
        }
        let cached = self.catalog.lock().expect("catalog lock").clone();
        let catalog = match cached {
            Some(catalog) => catalog,
            None => self.catalog().await?,
        };
        let model = match settings.model.as_deref() {
            Some(id) => catalog.models.iter().find(|model| model.id == id),
            None => catalog.models.iter().find(|model| model.is_default),
        };
        if let (Some(effort), Some(model)) = (&settings.effort, model)
            && !model.efforts.is_empty()
            && !model.efforts.iter().any(|option| &option.id == effort)
        {
            return Err(Error::InvalidSetting {
                setting: "effort",
                value: effort.clone(),
                reason: format!("{} does not support it", model.id),
            });
        }
        if settings.fast_mode == Some(true)
            && !model.is_some_and(|model| matches!(model.fast_mode, FastMode::Available { .. }))
        {
            return Err(Error::InvalidSetting {
                setting: "fast_mode",
                value: "true".to_owned(),
                reason: "the model is not listed with fast mode support".to_owned(),
            });
        }
        Ok(())
    }
}

/// `claude --version`, e.g. `2.1.284`.
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

fn catalog_model(model: &Value, fast_note: &Option<String>) -> catalog::Model {
    let text = |key: &str| {
        model
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let id = text("value");
    let efforts = model
        .get("supportedEffortLevels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|id| catalog::Effort {
            id: id.to_owned(),
            description: String::new(),
        })
        .collect();
    let fast = model.get("supportsFastMode").and_then(Value::as_bool) == Some(true);
    catalog::Model {
        is_default: id == "default",
        label: text("displayName"),
        description: text("description"),
        efforts,
        default_effort: None,
        fast_mode: if fast {
            FastMode::Available {
                tier_id: "fast".to_owned(),
                label: "Fast".to_owned(),
                description: fast_note.clone().unwrap_or_default(),
                default_on: false,
            }
        } else {
            FastMode::Unavailable
        },
        id,
    }
}

struct ClaudeSession {
    process: Arc<ClaudeProcess>,
    session_id: String,
    request_timeout: Duration,
}

impl session::Driver for ClaudeSession {
    fn subscribe(&self) -> broadcast::Receiver<session::Event> {
        self.process.subscribe()
    }

    fn subscribe_records(&self) -> Result<broadcast::Receiver<crate::observation::Record>> {
        Ok(self.process.subscribe_records())
    }

    fn observation_capabilities(&self) -> Result<crate::observation::Capabilities> {
        Ok(live::capabilities())
    }

    fn snapshot(&self) -> BoxFuture<'_, Result<crate::observation::Snapshot>> {
        Box::pin(async { Ok(self.process.snapshot()) })
    }

    fn stop_agent<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<crate::control::Receipt>> {
        Box::pin(async move {
            self.process
                .stop(
                    crate::control::Target::Agent { id: id.into() },
                    self.request_timeout,
                )
                .await
        })
    }
    fn stop_task<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<crate::control::Receipt>> {
        Box::pin(async move {
            self.process
                .stop(
                    crate::control::Target::Task { id: id.into() },
                    self.request_timeout,
                )
                .await
        })
    }

    fn task_output<'a>(
        &'a self,
        id: &'a str,
        query: crate::task::OutputQuery,
    ) -> BoxFuture<'a, Result<crate::task::OutputPage>> {
        Box::pin(async move { self.process.task_output(id, query).await })
    }

    fn send(&self, input: turn::Input) -> BoxFuture<'_, Result<turn::Receipt>> {
        Box::pin(async move {
            if !self.process.is_alive() {
                return Err(Error::ProcessExited {
                    stderr: "the Claude Code process for this session has exited".to_owned(),
                });
            }
            let turn_id = uuid::Uuid::new_v4().to_string();
            self.process
                .send_user(&self.session_id, &turn_id, &input)
                .await?;
            Ok(turn::Receipt { turn_id })
        })
    }

    fn interrupt<'a>(&'a self, turn_id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.process
                .interrupt_turn(turn_id, self.request_timeout)
                .await
        })
    }

    fn respond<'a>(
        &'a self,
        request_id: &'a str,
        decision: crate::request::Decision,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.process
                .respond(&self.session_id, request_id, decision)
                .await
        })
    }

    fn close(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.process.close().await;
            Ok(())
        })
    }
}

impl Provider for Claude {
    fn account_usage(&self) -> BoxFuture<'_, Result<crate::usage::Account>> {
        Box::pin(self.account_usage())
    }

    fn id(&self) -> &'static str {
        PROVIDER_ID
    }

    fn catalog(&self) -> BoxFuture<'_, Result<Catalog>> {
        Box::pin(Claude::catalog(self))
    }

    fn list_sessions(&self, query: session::Query) -> BoxFuture<'_, Result<session::Page>> {
        Box::pin(Claude::list_sessions(self, query))
    }

    fn read_session<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<session::Summary>> {
        Box::pin(Claude::read_session(self, id))
    }

    fn list_turns<'a>(
        &'a self,
        id: &'a str,
        query: turn::Query,
    ) -> BoxFuture<'a, Result<turn::Page>> {
        Box::pin(Claude::list_turns(self, id, query))
    }

    fn list_agents<'a>(
        &'a self,
        root: &'a str,
        query: crate::agent::Query,
    ) -> BoxFuture<'a, Result<crate::agent::Page>> {
        Box::pin(Claude::list_agents(self, root, query))
    }
    fn list_agent_turns<'a>(
        &'a self,
        root: &'a str,
        id: &'a str,
        query: turn::Query,
    ) -> BoxFuture<'a, Result<crate::agent::TurnPage>> {
        Box::pin(Claude::list_agent_turns(self, root, id, query))
    }

    fn list_tasks<'a>(
        &'a self,
        root: &'a str,
        query: crate::task::Query,
    ) -> BoxFuture<'a, Result<crate::task::Page>> {
        Box::pin(Claude::list_tasks(self, root, query))
    }
    fn task_output<'a>(
        &'a self,
        root: &'a str,
        id: &'a str,
        query: crate::task::OutputQuery,
    ) -> BoxFuture<'a, Result<crate::task::OutputPage>> {
        Box::pin(Claude::task_output(self, root, id, query))
    }

    fn start_session(&self, spec: session::Spec) -> BoxFuture<'_, Result<Session>> {
        Box::pin(Claude::start_session(self, spec))
    }

    fn resume_session<'a>(
        &'a self,
        id: &'a str,
        settings: session::Settings,
    ) -> BoxFuture<'a, Result<Session>> {
        Box::pin(Claude::resume_session(self, id, settings))
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(Claude::shutdown(self))
    }
}
