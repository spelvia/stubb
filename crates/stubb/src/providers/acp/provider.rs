use super::{Launch, config, connection, live};

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use agent_client_protocol::v1::schema as wire;

use crate::catalog::{Catalog, ProviderInfo};
use crate::core::runtime::files::{now, timestamp_millis};
use crate::process::Environment;
use crate::provider::BoxFuture;
use crate::session::{self, Session};
use crate::{Error, Provider, Result, turn};
use connection::{Connection, Connector};
use live::AcpSession;

/// How many `session/list` pages [`AcpProvider::read_session`] reads to find
/// a session.
const MAX_LIST_PAGES: usize = 50;

/// Which agent an [`AcpProvider`] drives, and how.
#[derive(Debug, Clone)]
pub struct AcpConfig {
    /// Stubb's provider identifier, such as a registry id (`gemini`). It
    /// must not be one of Stubb's native providers (`claude`, `codex`, `pi`).
    pub id: String,
    /// The agent's name, for people.
    pub label: String,
    pub launch: Launch,
    /// The environment of each agent process: the host's whole environment
    /// by default. [`Launch::env`] is set over it.
    pub environment: Environment,
    /// Deadline for starting the agent, `initialize` and creating or loading
    /// a session. Agents started through `npx` or `uvx` may download
    /// themselves first.
    pub startup_timeout: Duration,
    /// Deadline for other requests, such as changing a setting.
    pub request_timeout: Duration,
    /// An `authMethods` id to `authenticate` with after `initialize`, for
    /// agents that require it. See [`Report::auth_methods`](super::Report::auth_methods).
    pub auth_method: Option<String>,
}

impl AcpConfig {
    pub fn new(id: impl Into<String>, label: impl Into<String>, launch: Launch) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            launch,
            environment: Environment::inherit(),
            startup_timeout: Duration::from_secs(120),
            request_timeout: Duration::from_secs(60),
            auth_method: None,
        }
    }

    /// A configuration for a [registry](crate::registry) entry, started with
    /// `launch` (see [`Entry::resolve`](crate::registry::Entry::resolve)).
    pub fn from_entry(entry: &crate::registry::Entry, launch: Launch) -> Self {
        Self::new(&entry.id, &entry.name, launch)
    }
}

/// A provider for one ACP agent.
pub struct AcpProvider {
    config: AcpConfig,
    id: &'static str,
    connector: Connector,
    /// Live session connections, so `shutdown` can stop them.
    live: Mutex<HashMap<String, Weak<Connection>>>,
    /// Folders of sessions started or found here, to resume them.
    folders: Mutex<HashMap<String, PathBuf>>,
    /// What the agent reported in its last `initialize`.
    capabilities: Mutex<Option<wire::AgentCapabilities>>,
}

impl AcpProvider {
    pub fn new(config: AcpConfig) -> Self {
        let connector = Connector::Process {
            launch: config.launch.clone(),
            environment: config.environment.clone(),
        };
        Self::with_connector(config, connector)
    }

    pub(super) fn with_connector(config: AcpConfig, connector: Connector) -> Self {
        Self {
            id: intern(&config.id),
            config,
            connector,
            live: Mutex::new(HashMap::new()),
            folders: Mutex::new(HashMap::new()),
            capabilities: Mutex::new(None),
        }
    }

    pub fn config(&self) -> &AcpConfig {
        &self.config
    }

    /// The agent's name, for people.
    pub fn label(&self) -> &str {
        &self.config.label
    }

    pub(super) async fn connect(&self, cwd: &Path) -> Result<Connection> {
        let connection = Connection::open(
            &self.connector,
            cwd,
            self.config.startup_timeout,
            self.config.auth_method.as_deref(),
        )
        .await?;
        *self.capabilities.lock().expect("capabilities lock") =
            Some(connection.init.agent_capabilities.clone());
        Ok(connection)
    }

    /// Models and efforts the agent offers, read from a session it creates
    /// in the system's temporary folder, which is closed again at once.
    pub async fn catalog(&self) -> Result<Catalog> {
        let cwd = std::env::temp_dir();
        let connection = self.connect(&cwd).await?;
        let result = async {
            let response = connection
                .request(
                    "session/new",
                    wire::NewSessionRequest::new(cwd.clone()),
                    self.config.startup_timeout,
                )
                .await?;
            if connection
                .init
                .agent_capabilities
                .session_capabilities
                .close
                .is_some()
            {
                let close = wire::CloseSessionRequest::new(response.session_id.clone());
                let _ = connection
                    .request("session/close", close, self.config.request_timeout)
                    .await;
            }
            Ok::<_, Error>(response.config_options.unwrap_or_default())
        }
        .await;
        connection.shutdown().await;
        let options = result?;
        let agent = connection.init.agent_info.as_ref();
        Ok(Catalog {
            provider: ProviderInfo {
                id: self.id.to_owned(),
                version: agent.map(|agent| agent.version.clone()),
                user_agent: agent.map(|agent| format!("{}/{}", agent.name, agent.version)),
            },
            models: config::models(&options),
        })
    }

    /// Stored sessions, as the agent's `session/list` reports them, most
    /// recently updated first when the agent orders them so. `limit` is not
    /// passed on: the agent chooses its page size.
    pub async fn list_sessions(&self, query: session::Query) -> Result<session::Page> {
        if let Some(capabilities) = &*self.capabilities.lock().expect("capabilities lock")
            && capabilities.session_capabilities.list.is_none()
        {
            return Err(Error::Unsupported {
                operation: "list_sessions",
            });
        }
        let cwd = query.cwd.clone().unwrap_or_else(std::env::temp_dir);
        let connection = self.connect(&cwd).await?;
        let result = self.list_pages(&connection, query, 1).await;
        connection.shutdown().await;
        let (sessions, next_cursor) = result?;
        Ok(session::Page {
            sessions,
            next_cursor,
        })
    }

    async fn list_pages(
        &self,
        connection: &Connection,
        query: session::Query,
        pages: usize,
    ) -> Result<(Vec<session::Summary>, Option<String>)> {
        if connection
            .init
            .agent_capabilities
            .session_capabilities
            .list
            .is_none()
        {
            return Err(Error::Unsupported {
                operation: "list_sessions",
            });
        }
        let mut sessions = Vec::new();
        let mut cursor = query.cursor;
        for _ in 0..pages {
            let request = wire::ListSessionsRequest::new()
                .cwd(query.cwd.clone())
                .cursor(cursor.take());
            let page = connection
                .request("session/list", request, self.config.request_timeout)
                .await?;
            sessions.extend(page.sessions.iter().map(|info| self.summary(info)));
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
        let mut folders = self.folders.lock().expect("folders lock");
        for summary in &sessions {
            folders.insert(summary.id.clone(), summary.cwd.clone());
        }
        Ok((sessions, cursor))
    }

    fn summary(&self, info: &wire::SessionInfo) -> session::Summary {
        let updated = info
            .updated_at
            .as_deref()
            .and_then(timestamp_millis)
            .map_or(0, |millis| millis / 1000);
        session::Summary {
            id: info.session_id.0.to_string(),
            provider: self.id.to_owned(),
            title: info.title.clone(),
            preview: String::new(),
            cwd: info.cwd.clone(),
            model: None,
            effort: None,
            status: session::Status::NotLoaded,
            created_at: updated,
            updated_at: updated,
            source: None,
        }
    }

    /// A stored session, found through `session/list`.
    pub async fn read_session(&self, id: &str) -> Result<session::Summary> {
        let connection = self.connect(&std::env::temp_dir()).await?;
        let result = self
            .list_pages(&connection, session::Query::default(), MAX_LIST_PAGES)
            .await;
        connection.shutdown().await;
        result?
            .0
            .into_iter()
            .find(|summary| summary.id == id)
            .ok_or_else(|| Error::SessionNotFound { id: id.to_owned() })
    }

    /// Starts a new session in `spec.cwd`.
    pub async fn start_session(&self, spec: session::Spec) -> Result<Session> {
        self.open(None, &spec.cwd, spec.settings).await
    }

    /// Loads a stored session in its own folder, which Stubb knows for
    /// sessions it started or listed, and otherwise asks the agent through
    /// `session/list`. Use [`resume_session_in`](Self::resume_session_in)
    /// for agents that cannot list sessions.
    pub async fn resume_session(&self, id: &str, settings: session::Settings) -> Result<Session> {
        let known = self.folders.lock().expect("folders lock").get(id).cloned();
        let cwd = match known {
            Some(cwd) => cwd,
            None => match self.read_session(id).await {
                Ok(summary) => summary.cwd,
                Err(Error::Unsupported { .. }) => {
                    return Err(Error::Unsupported {
                        operation: "resume_session",
                    });
                }
                Err(error) => return Err(error),
            },
        };
        self.resume_session_in(id, &cwd, settings).await
    }

    /// Loads a stored session that belongs to `cwd`: with `session/resume`
    /// when the agent offers it, otherwise with `session/load`, which
    /// replays its history (Stubb passes the replay on as raw events, before
    /// the session is returned).
    pub async fn resume_session_in(
        &self,
        id: &str,
        cwd: &Path,
        settings: session::Settings,
    ) -> Result<Session> {
        self.open(Some(id), cwd, settings).await
    }

    /// Stops every live session's agent.
    pub async fn shutdown(&self) {
        let live: Vec<_> = self
            .live
            .lock()
            .expect("live lock")
            .drain()
            .filter_map(|(_, connection)| connection.upgrade())
            .collect();
        for connection in live {
            connection.state.withdraw_requests();
            connection.shutdown().await;
            connection.state.emit(session::Event::Closed {
                reason: session::CloseReason::Client,
            });
        }
    }

    async fn open(
        &self,
        resume: Option<&str>,
        cwd: &Path,
        settings: session::Settings,
    ) -> Result<Session> {
        if settings.permissions.is_some() {
            return Err(Error::Unsupported {
                operation: "permission_configuration",
            });
        }
        if settings.fast_mode == Some(true) {
            return Err(Error::InvalidSetting {
                setting: "fast_mode",
                value: "true".to_owned(),
                reason: "ACP agents have no fast mode".to_owned(),
            });
        }
        let connection = Arc::new(self.connect(cwd).await?);
        match self.attach(&connection, resume, cwd, &settings).await {
            Ok(session) => Ok(session),
            Err(error) => {
                connection.shutdown().await;
                Err(error)
            }
        }
    }

    async fn attach(
        &self,
        connection: &Arc<Connection>,
        resume: Option<&str>,
        cwd: &Path,
        settings: &session::Settings,
    ) -> Result<Session> {
        let capabilities = &connection.init.agent_capabilities;
        let directories = if capabilities
            .session_capabilities
            .additional_directories
            .is_some()
        {
            settings.additional_directories.clone()
        } else {
            Vec::new()
        };
        let timeout = self.config.startup_timeout;
        fn not_found(id: &str) -> impl FnOnce(Error) -> Error + '_ {
            move |error| match error {
                Error::Rpc { code: -32002, .. } => Error::SessionNotFound { id: id.to_owned() },
                error => error,
            }
        }
        let (session_id, modes, options) = match resume {
            None => {
                let request = wire::NewSessionRequest::new(cwd.to_path_buf())
                    .additional_directories(directories);
                let response = connection.request("session/new", request, timeout).await?;
                (response.session_id, response.modes, response.config_options)
            }
            Some(id) if capabilities.session_capabilities.resume.is_some() => {
                let request = wire::ResumeSessionRequest::new(id.to_owned(), cwd.to_path_buf())
                    .additional_directories(directories);
                let response = connection
                    .request("session/resume", request, timeout)
                    .await
                    .map_err(not_found(id))?;
                (
                    wire::SessionId::new(id.to_owned()),
                    response.modes,
                    response.config_options,
                )
            }
            Some(id) if capabilities.load_session => {
                let request = wire::LoadSessionRequest::new(id.to_owned(), cwd.to_path_buf())
                    .additional_directories(directories);
                let response = connection
                    .request("session/load", request, timeout)
                    .await
                    .map_err(not_found(id))?;
                (
                    wire::SessionId::new(id.to_owned()),
                    response.modes,
                    response.config_options,
                )
            }
            Some(_) => {
                return Err(Error::Unsupported {
                    operation: "resume_session",
                });
            }
        };
        let state = &connection.state;
        state.attach(
            &session_id.0,
            cwd,
            &settings.additional_directories,
            settings.forward_requests,
        );
        state.set_config(modes, options);
        self.apply(connection, &session_id, settings).await?;

        let options = state.config_options();
        let model = config::current(&options, &wire::SessionConfigOptionCategory::Model)
            .unwrap_or_else(|| config::DEFAULT_MODEL.to_owned());
        let effort = config::current(&options, &wire::SessionConfigOptionCategory::ThoughtLevel);
        let id = session_id.0.to_string();
        let summary = session::Summary {
            id: id.clone(),
            provider: self.id.to_owned(),
            title: None,
            preview: String::new(),
            cwd: cwd.to_path_buf(),
            model: Some(model.clone()),
            effort: effort.clone(),
            status: session::Status::Idle,
            created_at: now(),
            updated_at: now(),
            source: None,
        };
        let info = session::Info {
            model,
            effort,
            fast_mode: false,
            cwd: cwd.to_path_buf(),
            permissions: None,
            approval_policy: None,
            sandbox: None,
            restored: resume.is_some(),
            persisted: resume.is_some(),
            summary,
        };
        {
            let mut live = self.live.lock().expect("live lock");
            live.retain(|_, connection| connection.strong_count() > 0);
            live.insert(id.clone(), Arc::downgrade(connection));
        }
        self.folders
            .lock()
            .expect("folders lock")
            .insert(id, cwd.to_path_buf());
        let driver = AcpSession {
            connection: Arc::clone(connection),
            session_id,
            request_timeout: self.config.request_timeout,
        };
        Ok(Session::new(info, Arc::new(driver)))
    }

    /// Applies the requested model and effort through the session's
    /// configuration options.
    async fn apply(
        &self,
        connection: &Connection,
        session_id: &wire::SessionId,
        settings: &session::Settings,
    ) -> Result<()> {
        let wanted = [
            (
                "model",
                &settings.model,
                wire::SessionConfigOptionCategory::Model,
            ),
            (
                "effort",
                &settings.effort,
                wire::SessionConfigOptionCategory::ThoughtLevel,
            ),
        ];
        for (setting, value, category) in wanted {
            let Some(value) = value else { continue };
            let options = connection.state.config_options();
            let invalid = |reason: &str| Error::InvalidSetting {
                setting,
                value: value.clone(),
                reason: reason.to_owned(),
            };
            let Some((option, current, choices)) = config::select(&options, &category) else {
                if setting == "model" && value == config::DEFAULT_MODEL {
                    continue;
                }
                return Err(invalid("the agent offers no choice"));
            };
            if !choices.iter().any(|choice| choice.value == *value) {
                return Err(invalid("the agent does not offer this value"));
            }
            if current == *value {
                continue;
            }
            let request = wire::SetSessionConfigOptionRequest::new(
                session_id.clone(),
                option.id.clone(),
                wire::SessionConfigValueId::new(value.clone()),
            );
            let response = connection
                .request(
                    "session/set_config_option",
                    request,
                    self.config.request_timeout,
                )
                .await?;
            connection
                .state
                .set_config(None, Some(response.config_options));
        }
        Ok(())
    }
}

/// A `'static` copy of a provider id, made once per distinct id, so ACP
/// providers can report ids chosen at run time through
/// [`Provider::id`].
pub(super) fn intern(id: &str) -> &'static str {
    static IDS: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let mut ids = IDS.get_or_init(Default::default).lock().expect("ids lock");
    if let Some(id) = ids.get(id) {
        return id;
    }
    let id: &'static str = Box::leak(id.to_owned().into_boxed_str());
    ids.insert(id);
    id
}

impl Provider for AcpProvider {
    fn id(&self) -> &'static str {
        self.id
    }

    fn catalog(&self) -> BoxFuture<'_, Result<Catalog>> {
        Box::pin(AcpProvider::catalog(self))
    }

    fn list_sessions(&self, query: session::Query) -> BoxFuture<'_, Result<session::Page>> {
        Box::pin(AcpProvider::list_sessions(self, query))
    }

    fn read_session<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<session::Summary>> {
        Box::pin(AcpProvider::read_session(self, id))
    }

    /// ACP cannot read a stored session's turns without loading it.
    fn list_turns<'a>(
        &'a self,
        _id: &'a str,
        _query: turn::Query,
    ) -> BoxFuture<'a, Result<turn::Page>> {
        Box::pin(async {
            Err(Error::Unsupported {
                operation: "list_turns",
            })
        })
    }

    fn start_session(&self, spec: session::Spec) -> BoxFuture<'_, Result<Session>> {
        Box::pin(AcpProvider::start_session(self, spec))
    }

    fn resume_session<'a>(
        &'a self,
        id: &'a str,
        settings: session::Settings,
    ) -> BoxFuture<'a, Result<Session>> {
        Box::pin(AcpProvider::resume_session(self, id, settings))
    }

    fn shutdown(&self) -> BoxFuture<'_, ()> {
        Box::pin(AcpProvider::shutdown(self))
    }
}
