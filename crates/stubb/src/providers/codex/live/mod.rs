//! Live Codex attachments: subscriptions, snapshots and native activity.
//! The RPC reader reduces authoritative state directly under one lock.

mod controls;
mod routing;
mod state;
pub(super) mod tasks;

pub(super) use controls::Request as StopRequest;
pub(super) use routing::Router;

use std::collections::BTreeMap;
use std::sync::Mutex;

use tokio::sync::broadcast;

use crate::providers::codex::transport::rpc::EventSink;
use crate::{observation as obs, observation::agent, session};
use state::{Branch, State};

const BUFFER: usize = 1024;

pub(super) struct Attachment {
    state: Mutex<State>,
    legacy: broadcast::Sender<session::Event>,
    records: broadcast::Sender<obs::Record>,
    requests: Mutex<Requests>,
}

/// Server requests forwarded to the host and not answered yet.
#[derive(Default)]
struct Requests {
    /// The session forwards requests ([`session::Settings::forward_requests`]).
    forward: bool,
    pending: std::collections::HashMap<String, super::requests::Pending>,
}

impl Attachment {
    pub(super) fn new(info: &session::Info) -> Self {
        Self {
            state: Mutex::new(State::new(info)),
            legacy: broadcast::channel(BUFFER).0,
            records: broadcast::channel(BUFFER).0,
            requests: Mutex::new(Requests::default()),
        }
    }

    /// Whether server requests go to the host rather than being declined.
    pub(super) fn set_forward_requests(&self, forward: bool) {
        self.requests.lock().expect("requests lock").forward = forward;
    }

    pub(super) fn forwards_requests(&self) -> bool {
        !self.is_closed() && self.requests.lock().expect("requests lock").forward
    }

    /// Keeps a forwarded request until the host answers, and reports it on
    /// the session's stream, where the host answers even a sub-agent's.
    pub(super) fn forward(
        &self,
        request: crate::request::Request,
        pending: super::requests::Pending,
    ) {
        self.requests
            .lock()
            .expect("requests lock")
            .pending
            .insert(request.id.clone(), pending);
        self.emit(session::Event::Request { request });
    }

    /// Runs `answer` on a pending request and removes it when it succeeds.
    pub(super) fn answer<T>(
        &self,
        id: &str,
        answer: impl FnOnce(&super::requests::Pending) -> crate::Result<T>,
    ) -> crate::Result<(super::requests::Pending, T)> {
        let mut requests = self.requests.lock().expect("requests lock");
        let not_found = || crate::Error::RequestNotFound {
            session_id: self.root_id(),
            id: id.to_owned(),
        };
        let value = answer(requests.pending.get(id).ok_or_else(not_found)?)?;
        let pending = requests.pending.remove(id).ok_or_else(not_found)?;
        Ok((pending, value))
    }

    /// Puts back a request whose reply never left Stubb, so it can be
    /// answered again.
    pub(super) fn restore(&self, id: &str, pending: super::requests::Pending) {
        self.requests
            .lock()
            .expect("requests lock")
            .pending
            .entry(id.to_owned())
            .or_insert(pending);
    }

    /// A pending request ended without an answer.
    pub(super) fn withdraw(&self, id: &str) {
        let removed = self
            .requests
            .lock()
            .expect("requests lock")
            .pending
            .remove(id);
        if removed.is_some() {
            self.emit(session::Event::RequestResolved {
                id: id.to_owned(),
                outcome: crate::request::Outcome::Withdrawn,
            });
        }
    }

    fn root_id(&self) -> String {
        self.state
            .lock()
            .expect("observation lock")
            .root
            .agent
            .id
            .clone()
    }

    pub(super) fn subscribe(&self) -> broadcast::Receiver<session::Event> {
        self.legacy.subscribe()
    }

    pub(super) fn subscribe_records(&self) -> broadcast::Receiver<obs::Record> {
        // A late subscriber still needs root metadata. Publication is session-
        // wide and sequenced, so existing observers receive the replacement too.
        let mut state = self.state.lock().expect("observation lock");
        let receiver = self.records.subscribe();
        state.announce(&self.records);
        let agents: Vec<_> = state
            .children
            .values()
            .map(|branch| branch.agent.clone())
            .collect();
        for agent in agents {
            state.publish(&self.records, None, obs::Event::Agent { agent });
        }
        for task in state.tasks.summaries() {
            state.publish(&self.records, None, obs::Event::Task { task });
        }
        for receipt in state
            .controls
            .values()
            .map(|pending| pending.receipt.clone())
            .collect::<Vec<_>>()
        {
            state.publish(&self.records, None, obs::Event::Control { receipt });
        }
        receiver
    }

    pub(super) fn snapshot(&self) -> obs::Snapshot {
        self.state.lock().expect("observation lock").snapshot()
    }

    pub(super) fn capabilities(&self) -> obs::Capabilities {
        capabilities()
    }

    pub(super) fn is_closed(&self) -> bool {
        self.state.lock().expect("observation lock").root.closed
    }

    pub(super) fn refresh_metadata(&self, info: &session::Info) {
        let mut state = self.state.lock().expect("observation lock");
        let previous = state.root.agent.clone();
        state.root.agent.model = Some(info.model.clone());
        state.root.agent.created_at = Some(info.summary.created_at);
        state.root.agent.updated_at = Some(info.summary.updated_at);
        if state.root.agent != previous {
            state.announce(&self.records);
        }
    }
}

impl Attachment {
    pub(super) fn upsert_agent(&self, agent: agent::Summary) {
        let mut state = self.state.lock().expect("observation lock");
        if state.root.closed || agent.id == state.root.agent.id {
            return;
        }
        if !state.announced {
            state.announce(&self.records);
        }
        let existed = state.children.contains_key(&agent.id);
        let branch = state
            .children
            .entry(agent.id.clone())
            .or_insert_with(|| Branch {
                agent: agent.clone(),
                status: session::Status::Idle,
                closed: false,
                loaded: true,
                turns: BTreeMap::new(),
            });
        // Metadata reads do not overwrite a newer observed work outcome.
        let mut replacement = agent;
        replacement.status = branch.agent.status;
        if existed && replacement == branch.agent {
            return;
        }
        branch.agent = replacement.clone();
        state.publish(
            &self.records,
            None,
            obs::Event::Agent { agent: replacement },
        );
    }

    pub(super) fn emit_for(&self, id: &str, event: session::Event) {
        if matches!(event, session::Event::Closed { .. }) && id == self.root_id() {
            // Withdraw what the host can no longer answer, while the stream
            // still takes events.
            let pending: Vec<String> = self
                .requests
                .lock()
                .expect("requests lock")
                .pending
                .keys()
                .cloned()
                .collect();
            for id in pending {
                self.withdraw(&id);
            }
        }
        self.state
            .lock()
            .expect("observation lock")
            .apply(id, event, &self.records, &self.legacy);
    }
}

impl EventSink for Attachment {
    fn emit(&self, event: session::Event) {
        let id = self.root_id();
        self.emit_for(&id, event);
    }
}

fn capabilities() -> obs::Capabilities {
    let unavailable = obs::Feature {
        support: obs::Support::Unsupported,
        coverage: obs::Coverage::Unknown,
        reason: Some("Not implemented by this adapter checkpoint".into()),
        gaps: vec![],
    };
    let live = obs::Feature {
        support: obs::Support::Supported,
        coverage: obs::Coverage::Partial,
        reason: Some("Live root and descendant activity on the attached app-server connection".into()),
        gaps: vec!["Stored discovery on resume is bounded to 5000 native descendants; gaps are reported".into(), "Early traffic buffering is bounded; transcript gaps are reported as raw observationGap events".into()],
    };
    let tasks = obs::Feature {
        support: obs::Support::Supported,
        coverage: obs::Coverage::Partial,
        reason: Some("Read-only running-terminal polling and live command events".into()),
        gaps: vec![
            "Commands that finish between polls may never appear as background tasks".into(),
            "List absence does not establish an outcome; final command events are required".into(),
        ],
    };
    let output = obs::Feature {
        support: obs::Support::Supported, coverage: obs::Coverage::Partial,
        reason: Some("Bounded reads of command output retained from attributed command items and deltas".into()),
        gaps: vec![format!("Only the last {} MiB of each task's output is retained; earlier output is reported as partial", super::output::RETAINED >> 20),
            "Output produced before the attachment saw the command start is missing unless the final command item carries it".into()],
    };
    obs::Capabilities {
        records: live.clone(),
        snapshots: obs::Feature { support: obs::Support::Supported, coverage: obs::Coverage::Partial,
            reason: Some("Atomic reducer state and stream watermark".into()),
            gaps: vec!["Completed transcripts and usage totals are not replayed by snapshots".into(), "Native traffic lost before routing cannot be recovered from reducer state".into()],
        },
        agent_discovery: live.clone(),
        parent_links: live.clone(),
        nested_discovery: live.clone(),
        agent_lifecycle: live.clone(),
        agent_transcript: live,
        agent_history: obs::Feature { support: obs::Support::Supported, coverage: obs::Coverage::Partial,
            reason: Some("Read-only native stored descendant discovery and owned transcript pagination".into()),
            gaps: vec!["Only native persisted threads are discoverable; unreported historical outcomes remain unknown".into()],
        },
        task_discovery: tasks.clone(),
        task_lifecycle: tasks,
        task_output: output,
        task_history: obs::Feature {
            support: obs::Support::Supported, coverage: obs::Coverage::Partial,
            reason: Some("Read-only stored commands that outlived their turn, with their output, from native rollout files".into()),
            gaps: vec!["Background commands that finished within their turn, or never finished, are not distinguishable in stored history".into(),
                "Output is what Codex recorded when the command ended".into()],
        },
        stop_agent: obs::Feature {
            support: obs::Support::Supported, coverage: obs::Coverage::Partial,
            reason: Some("Interrupts the captured active native turn; does not close agent contexts".into()),
            gaps: vec!["Owned tasks and descendants are governed by native interrupt semantics".into()],
        },
        stop_subtree: unavailable.clone(),
        stop_task: obs::Feature {
            support: obs::Support::Supported, coverage: obs::Coverage::Partial,
            reason: Some("Native background-terminal termination with receipt/outcome tracking".into()),
            gaps: vec!["Native termination targets a process reference without an atomic item-generation guard".into()],
        },
    }
}

#[cfg(test)]
mod tests;
