//! Whether an ACP agent is usable, without creating a session.
//!
//! [`probe`] looks for the agent's program like [`crate::probe`] does, on
//! `PATH` and then through the login shell. Given a deadline, it also starts
//! the agent and completes `initialize` to read the agent's name, version,
//! sign-in methods and capabilities. It never creates a session or sends a
//! prompt.

use std::path::PathBuf;
use std::time::Duration;

use agent_client_protocol::v1::schema as wire;
use serde::{Deserialize, Serialize};

use super::Launch;
use super::connection::{Connection, Connector};
use crate::process::Environment;

/// What [`probe`] found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Report {
    /// The program that was found, or `None` when it is not installed.
    pub path: Option<PathBuf>,
    /// Whether `initialize` succeeded. `false` when it was not attempted.
    pub initialized: bool,
    /// The agent's name and version, when it reports them.
    pub agent: Option<AgentInfo>,
    /// The ACP protocol version the agent chose.
    pub protocol_version: Option<u16>,
    /// Ways the agent can sign in. An empty list says nothing about whether
    /// it is signed in.
    pub auth_methods: Vec<AuthMethod>,
    pub capabilities: Option<Capabilities>,
    /// Why something could not be determined, in a sentence for people.
    pub detail: Option<String>,
}

impl Report {
    /// Whether the program was found.
    pub fn installed(&self) -> bool {
        self.path.is_some()
    }
}

/// The agent's `agentInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct AgentInfo {
    pub name: String,
    pub title: Option<String>,
    pub version: String,
}

/// One of the agent's `authMethods`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct AuthMethod {
    /// Pass this as [`AcpConfig::auth_method`](super::AcpConfig::auth_method).
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    /// The method's definition as received, such as a terminal command.
    pub native: serde_json::Value,
}

/// What the agent can do, from its `agentCapabilities`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Capabilities {
    /// `session/load`: stored sessions can be continued, replaying history.
    pub load_session: bool,
    /// `session/resume`: stored sessions can be continued without replay.
    pub resume_session: bool,
    /// `session/list`: stored sessions can be listed.
    pub list_sessions: bool,
    /// `session/close`.
    pub close_session: bool,
    /// Prompts may contain images.
    pub image_prompts: bool,
}

/// Probes the agent `launch` starts. With `initialize`, the agent is
/// started with `environment` and must answer `initialize` within that
/// time; it is stopped again right after.
pub async fn probe(
    launch: &Launch,
    environment: &Environment,
    initialize: Option<Duration>,
) -> Report {
    let mut report = Report {
        path: crate::core::discovery::executable::resolve(&launch.program).await,
        initialized: false,
        agent: None,
        protocol_version: None,
        auth_methods: Vec::new(),
        capabilities: None,
        detail: None,
    };
    let Some(path) = &report.path else {
        report.detail = Some(format!("`{}` was not found", launch.program.display()));
        return report;
    };
    let Some(timeout) = initialize else {
        return report;
    };
    let launch = Launch {
        program: path.clone(),
        ..launch.clone()
    };
    let connector = Connector::Process {
        launch,
        environment: environment.clone(),
    };
    let cwd = std::env::temp_dir();
    let opened = tokio::time::timeout(timeout, Connection::open(&connector, &cwd, timeout, None))
        .await
        .unwrap_or(Err(crate::Error::Timeout(timeout)));
    match opened {
        Ok(connection) => {
            connection.shutdown().await;
            fill(&mut report, &connection.init);
        }
        Err(error) => report.detail = Some(error.to_string()),
    }
    report
}

fn fill(report: &mut Report, init: &wire::InitializeResponse) {
    report.initialized = true;
    report.protocol_version = serde_json::to_value(init.protocol_version)
        .ok()
        .and_then(|version| version.as_u64())
        .and_then(|version| u16::try_from(version).ok());
    report.agent = init.agent_info.as_ref().map(|info| AgentInfo {
        name: info.name.clone(),
        title: info.title.clone(),
        version: info.version.clone(),
    });
    report.auth_methods = init.auth_methods.iter().map(auth_method).collect();
    let capabilities = &init.agent_capabilities;
    let sessions = &capabilities.session_capabilities;
    report.capabilities = Some(Capabilities {
        load_session: capabilities.load_session,
        resume_session: sessions.resume.is_some(),
        list_sessions: sessions.list.is_some(),
        close_session: sessions.close.is_some(),
        image_prompts: capabilities.prompt_capabilities.image,
    });
}

fn auth_method(method: &wire::AuthMethod) -> AuthMethod {
    let native = serde_json::to_value(method).unwrap_or_default();
    let text = |key: &str| native.get(key).and_then(|v| v.as_str()).map(str::to_owned);
    AuthMethod {
        id: text("id").unwrap_or_default(),
        name: text("name").unwrap_or_default(),
        description: text("description"),
        native,
    }
}

#[cfg(test)]
pub(super) fn report_from(init: &wire::InitializeResponse) -> Report {
    let mut report = Report {
        path: None,
        initialized: false,
        agent: None,
        protocol_version: None,
        auth_methods: Vec::new(),
        capabilities: None,
        detail: None,
    };
    fill(&mut report, init);
    report
}
