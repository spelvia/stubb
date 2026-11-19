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

use crate::codex::transport::rpc::EventSink;
use crate::{agent, observation as obs, session};
use state::{Branch, State};

const BUFFER: usize = 1024;

pub(super) struct Attachment {
    state: Mutex<State>,
    legacy: broadcast::Sender<session::Event>,
    records: broadcast::Sender<obs::Record>,
}

impl Attachment {
    pub(super) fn new(info: &session::Info) -> Self {
        Self {
            state: Mutex::new(State::new(info)),
            legacy: broadcast::channel(BUFFER).0,
            records: broadcast::channel(BUFFER).0,
        }
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
        self.state
            .lock()
            .expect("observation lock")
            .apply(id, event, &self.records, &self.legacy);
    }
}

impl EventSink for Attachment {
    fn emit(&self, event: session::Event) {
        let id = self
            .state
            .lock()
            .expect("observation lock")
            .root
            .agent
            .id
            .clone();
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
        support: obs::Support::Supported, coverage: obs::Coverage::Partial,
        reason: Some("Read-only running-terminal polling and live command events".into()),
        gaps: vec!["Commands that finish between polls may never appear as background tasks".into(),
            "List absence does not establish an outcome; final command events are required".into(),
            "Output is delivered through attributed command items and deltas; no output replay or stored task history".into()],
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
        task_lifecycle: tasks.clone(),
        task_output: tasks,
        task_history: unavailable.clone(),
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
