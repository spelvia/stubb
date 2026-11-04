//! One native stream-json attachment: attributed activity and recursive agents.

mod controls;
mod state;
mod tasks;
#[cfg(test)]
mod tests;
mod turns;

use std::sync::Mutex;

use serde_json::Value;
use tokio::sync::broadcast;

use crate::{observation as obs, session};
pub(in crate::claude) use controls::Request as StopRequest;
use state::State;

const BUFFER: usize = 1024;

pub(super) struct Attachment {
    state: Mutex<State>,
    legacy: broadcast::Sender<session::Event>,
    records: broadcast::Sender<obs::Record>,
}

impl Attachment {
    pub(super) fn new(id: &str) -> Self {
        Self {
            state: Mutex::new(State::new(id)),
            legacy: broadcast::channel(BUFFER).0,
            records: broadcast::channel(BUFFER).0,
        }
    }

    pub(super) fn configure(&self, info: &session::Info) {
        let mut state = self.state.lock().expect("observation lock");
        // A requested alias is not an effective native model. Restore only a
        // previously reported model until this process reports its own.
        if state.root.agent.model.is_none()
            && info.restored
            && info.summary.model.as_deref() == Some(info.model.as_str())
        {
            state.root.agent.model = info.summary.model.clone();
        }
        state.root.agent.created_at = Some(info.summary.created_at);
        state.root.agent.updated_at = Some(info.summary.updated_at);
        state.announce_root(&self.records);
    }

    pub(super) fn restore(
        &self,
        stored: crate::claude::agents::Stored,
        tasks: Vec<crate::claude::stored_tasks::Entry>,
    ) {
        self.state
            .lock()
            .expect("observation lock")
            .restore(stored, tasks, &self.records);
    }

    pub(super) fn subscribe(&self) -> broadcast::Receiver<session::Event> {
        self.legacy.subscribe()
    }

    pub(super) fn subscribe_records(&self) -> broadcast::Receiver<obs::Record> {
        let mut state = self.state.lock().expect("observation lock");
        let receiver = self.records.subscribe();
        state.announce_all(&self.records);
        receiver
    }

    pub(super) fn snapshot(&self) -> obs::Snapshot {
        self.state.lock().expect("observation lock").snapshot()
    }

    pub(super) fn begin_root_stop(&self, turn: &str) -> crate::Result<controls::Request> {
        self.state
            .lock()
            .expect("observation lock")
            .begin_root_stop(turn, &self.records)
    }

    pub(super) fn begin_stop(
        &self,
        target: crate::control::Target,
    ) -> crate::Result<controls::Request> {
        self.state
            .lock()
            .expect("observation lock")
            .begin_stop(target, &self.records)
    }
    pub(super) fn finish_stop(
        &self,
        id: &str,
        result: &crate::Result<()>,
    ) -> crate::control::Receipt {
        self.state
            .lock()
            .expect("observation lock")
            .finish_stop(id, result, &self.records)
    }
    pub(super) fn stop_is_current(&self, id: &str) -> bool {
        self.state
            .lock()
            .expect("observation lock")
            .stop_is_current(id)
    }
    pub(super) fn stop_receipt(&self, id: &str) -> crate::control::Receipt {
        self.state
            .lock()
            .expect("observation lock")
            .stop_receipt(id)
    }

    pub(super) fn output_binding(&self, id: &str) -> crate::Result<super::output::Binding> {
        self.state
            .lock()
            .expect("observation lock")
            .output_binding(id)
    }

    pub(super) fn output_availability(
        &self,
        id: &str,
        availability: crate::task::OutputAvailability,
    ) {
        self.state
            .lock()
            .expect("observation lock")
            .output_availability(id, availability, &self.records);
    }

    pub(super) fn message(&self, message: &Value) {
        self.state
            .lock()
            .expect("observation lock")
            .message(message, &self.records, &self.legacy);
    }

    pub(super) fn turn_started(&self, id: &str, text: &str) {
        let mut state = self.state.lock().expect("observation lock");
        let events = state.root.mapper.turn_started(id, text);
        state.root_events(events, &self.records, &self.legacy);
    }

    pub(super) fn turn_failed(&self, id: &str, error: &str) {
        let mut state = self.state.lock().expect("observation lock");
        let events = state.root.mapper.turn_failed(id, error);
        state.root_events(events, &self.records, &self.legacy);
    }

    pub(super) fn tool_declined(&self, tool: &str, id: Option<&str>, input: &Value) {
        self.state.lock().expect("observation lock").tool_declined(
            tool,
            id,
            input,
            &self.records,
            &self.legacy,
        );
    }

    pub(super) fn requested(&self, tool_use_id: Option<&str>, request: crate::request::Request) {
        self.state.lock().expect("observation lock").requested(
            tool_use_id,
            request,
            &self.records,
            &self.legacy,
        );
    }

    pub(super) fn request_resolved(
        &self,
        tool_use_id: Option<&str>,
        id: &str,
        outcome: crate::request::Outcome,
    ) {
        self.state
            .lock()
            .expect("observation lock")
            .request_resolved(tool_use_id, id, outcome, &self.records, &self.legacy);
    }

    pub(super) fn close(&self, reason: session::CloseReason) {
        self.state
            .lock()
            .expect("observation lock")
            .close(reason, &self.records, &self.legacy);
    }
}

pub(super) fn capabilities() -> obs::Capabilities {
    let live = obs::Feature {
        support: obs::Support::Supported,
        coverage: obs::Coverage::Partial,
        reason: Some("Native task_started IDs and parent_tool_use_id conversations on this attachment".into()),
        gaps: vec![
            "Requires native task IDs and descendant forwarding; older CLIs may omit them".into(),
            "Resume restores stored summaries; only native live bookends establish active executions".into(),
            "Unresolved traffic is bounded; overflow reports observationGap without guessing ancestry".into(),
            "Delegated turns use native spawning tool IDs; continuations without task_started remain raw".into(),
        ],
    };
    obs::Capabilities {
        records: live.clone(),
        snapshots: obs::Feature {
            support: obs::Support::Supported,
            coverage: obs::Coverage::Partial,
            reason: Some("Atomic live agent state, active items and stream watermark".into()),
            gaps: vec![
                "Completed transcripts and usage are not replayed; command logs use task_output".into(),
            ],
        },
        agent_discovery: live.clone(),
        parent_links: live.clone(),
        nested_discovery: live.clone(),
        agent_lifecycle: live.clone(),
        agent_transcript: live.clone(),
        agent_history: obs::Feature {support:obs::Support::Supported,coverage:obs::Coverage::Partial,reason:Some("Session-scoped native sidechains with invocation ownership".into()),gaps:vec!["Retention and compaction can remove native history; unavailable invocation outcomes stay inProgress without asserting running".into()]},
        task_discovery: live.clone(),
        task_lifecycle: live.clone(),
        task_output: obs::Feature {
            support: obs::Support::Supported, coverage: obs::Coverage::Partial,
            reason: Some("Bounded native command-file reads via task_output".into()),
            gaps: vec!["Native output paths may be missing or expired; agent runs use owned transcripts rather than raw JSONL logs".into()],
        },
        task_history: obs::Feature {support:obs::Support::Supported,coverage:obs::Coverage::Partial,reason:Some("Structured stored tool results and matching native queued notifications".into()),gaps:vec!["Unreported runtime state remains unknown; output files may expire independently".into()]},
        stop_agent: obs::Feature {support:obs::Support::Supported,coverage:obs::Coverage::Partial,
            reason:Some("Native root interrupt and stop_task for captured delegated executions".into()),
            gaps:vec!["Native stop_task has no atomic generation guard; task scope is checked before writing and outcomes are correlated".into(),"Owned work follows native stop semantics; no context closure or simulated subtree stop".into()]},
        stop_subtree: obs::Feature {support:obs::Support::Unsupported,coverage:obs::Coverage::Unknown,reason:Some("Claude exposes individual interrupt/stop_task controls, no atomic subtree-stop primitive".into()),gaps:vec![]},
        stop_task: obs::Feature {support:obs::Support::Supported,coverage:obs::Coverage::Partial,
            reason:Some("Native stop_task with acknowledged and confirmed outcomes".into()),
            gaps:vec!["Requires native task and spawning tool identities; acknowledgement alone cannot prove termination".into()]},
    }
}
