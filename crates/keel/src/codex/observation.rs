//! Recursive session observation reduction. The RPC reader delivers directly to this sink;
//! no broadcast subscriber or asynchronous relay owns authoritative state.

use std::collections::{BTreeMap, HashSet};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::broadcast;

use crate::rpc::EventSink;
use crate::{agent, observation as obs, session, turn};

const BUFFER: usize = 1024;

pub(super) struct Observation {
    state: Mutex<State>,
    legacy: broadcast::Sender<session::Event>,
    records: broadcast::Sender<obs::Record>,
}

struct State {
    stream_id: String,
    seq: u64,
    branch: Branch,
    children: BTreeMap<String, Branch>,
    announced: bool,
}

struct Branch {
    root: agent::Summary,
    closed: bool,
    status: session::Status,
    turns: BTreeMap<String, Materialized>,
}

impl std::ops::Deref for State {
    type Target = Branch;
    fn deref(&self) -> &Branch {
        &self.branch
    }
}
impl std::ops::DerefMut for State {
    fn deref_mut(&mut self) -> &mut Branch {
        &mut self.branch
    }
}

struct Materialized {
    turn: turn::Turn,
    ended: bool,
    open_items: HashSet<String>,
}

impl Observation {
    pub(super) fn new(info: &session::Info) -> Self {
        Self {
            state: Mutex::new(State {
                stream_id: uuid::Uuid::new_v4().to_string(),
                seq: 0,
                branch: Branch {
                    root: agent::Summary {
                        id: info.summary.id.clone(),
                        parent: agent::Parent::Root,
                        name: None,
                        role: None,
                        model: Some(info.model.clone()),
                        description: None,
                        background: None,
                        // An idle attachment alone does not establish a work outcome.
                        status: agent::Status::Unknown,
                        origin: None,
                        created_at: Some(info.summary.created_at),
                        updated_at: Some(info.summary.updated_at),
                    },
                    closed: false,
                    status: info.summary.status,
                    turns: BTreeMap::new(),
                },
                children: BTreeMap::new(),
                announced: false,
            }),
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
            .map(|branch| branch.root.clone())
            .collect();
        for agent in agents {
            state.publish(&self.records, None, obs::Event::Agent { agent });
        }
        receiver
    }

    pub(super) fn capabilities(&self) -> obs::Capabilities {
        capabilities()
    }

    pub(super) fn is_closed(&self) -> bool {
        self.state.lock().expect("observation lock").closed
    }

    pub(super) fn refresh_metadata(&self, info: &session::Info) {
        let mut state = self.state.lock().expect("observation lock");
        let previous = state.root.clone();
        state.root.model = Some(info.model.clone());
        state.root.created_at = Some(info.summary.created_at);
        state.root.updated_at = Some(info.summary.updated_at);
        if state.root != previous {
            state.announce(&self.records);
        }
    }
}

impl Observation {
    pub(super) fn upsert_agent(&self, agent: agent::Summary) {
        let mut state = self.state.lock().expect("observation lock");
        if state.closed || agent.id == state.root.id {
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
                root: agent.clone(),
                status: session::Status::Idle,
                closed: false,
                turns: BTreeMap::new(),
            });
        // Metadata reads do not overwrite a newer observed work outcome.
        let mut replacement = agent;
        replacement.status = branch.root.status;
        if existed && replacement == branch.root {
            return;
        }
        branch.root = replacement.clone();
        state.publish(
            &self.records,
            None,
            obs::Event::Agent { agent: replacement },
        );
    }

    pub(super) fn emit_for(&self, id: &str, event: session::Event) {
        let mut state = self.state.lock().expect("observation lock");
        if state.closed {
            return;
        }
        if !state.announced {
            state.announce(&self.records);
        }
        let root_event = id == state.root.id;
        let old_status = state.aggregate_status();
        let branch = if root_event {
            &mut state.branch
        } else {
            let Some(branch) = state.children.get_mut(id) else {
                return;
            };
            branch
        };
        if branch.closed {
            // A native new turn can reopen a previously closed child context.
            // Root attachment closure still requires a new observation stream.
            if !root_event && matches!(event, session::Event::TurnStart { .. }) {
                branch.closed = false;
            } else {
                return;
            }
        }
        let previous = branch.root.clone();
        branch.reduce(&event);
        let replacement = (previous != branch.root).then(|| branch.root.clone());
        if let Some(agent) = replacement {
            state.publish(&self.records, None, obs::Event::Agent { agent });
        }
        if root_event && matches!(event, session::Event::Closed { .. }) {
            let replacements: Vec<_> = state
                .children
                .values_mut()
                .filter_map(|child| {
                    if child.status == session::Status::Active || !child.turns.is_empty() {
                        child.root.status = agent::Status::Unknown;
                        child.status = session::Status::NotLoaded;
                        child.turns.clear();
                        Some(child.root.clone())
                    } else {
                        None
                    }
                })
                .collect();
            for agent in replacements {
                state.publish(&self.records, None, obs::Event::Agent { agent });
            }
        }
        let status = state.aggregate_status();
        if (old_status != status || matches!(event, session::Event::Status { .. }))
            && !(root_event && matches!(event, session::Event::Closed { .. }))
        {
            state.publish(
                &self.records,
                None,
                obs::Event::Activity {
                    event: session::Event::Status { status },
                    usage_scope: None,
                },
            );
        }
        // Child status/closure are node updates, never root session closures.
        if !matches!(event, session::Event::Status { .. })
            && (root_event || !matches!(event, session::Event::Closed { .. }))
        {
            let agent_id = if matches!(event, session::Event::Closed { .. })
                || matches!(&event, session::Event::Raw { name, .. } if name == "keel/observationGap")
            {
                None
            } else {
                Some(id.into())
            };
            let usage_scope =
                matches!(event, session::Event::Usage { .. }).then_some(obs::UsageScope::Unknown);
            state.publish(
                &self.records,
                agent_id,
                obs::Event::Activity {
                    event: event.clone(),
                    usage_scope,
                },
            );
        }
        if root_event {
            let _ = self.legacy.send(event);
        }
    }
}

impl EventSink for Observation {
    fn emit(&self, event: session::Event) {
        let id = self.state.lock().expect("observation lock").root.id.clone();
        self.emit_for(&id, event);
    }
}

impl State {
    fn announce(&mut self, records: &broadcast::Sender<obs::Record>) {
        self.announced = true;
        self.publish(
            records,
            None,
            obs::Event::Agent {
                agent: self.root.clone(),
            },
        );
    }

    fn publish(
        &mut self,
        records: &broadcast::Sender<obs::Record>,
        agent_id: Option<String>,
        event: obs::Event,
    ) {
        self.seq = self
            .seq
            .checked_add(1)
            .expect("observation sequence exhausted");
        let _ = records.send(obs::Record {
            stream_id: self.stream_id.clone(),
            seq: self.seq,
            at: now_ms(),
            agent_id,
            event,
        });
    }

    fn aggregate_status(&self) -> session::Status {
        if self.closed {
            return session::Status::NotLoaded;
        }
        if self.branch.status == session::Status::Active
            || self
                .children
                .values()
                .any(|child| child.status == session::Status::Active)
        {
            session::Status::Active
        } else {
            self.branch.status
        }
    }
}

impl Branch {
    fn turn(&mut self, id: &str) -> &mut Materialized {
        self.turns.entry(id.into()).or_insert_with(|| Materialized {
            turn: turn::Turn {
                id: id.into(),
                status: turn::Status::InProgress,
                items: vec![],
                started_at: None,
                completed_at: None,
                duration_ms: None,
                error: None,
            },
            ended: false,
            open_items: HashSet::new(),
        })
    }

    fn reduce(&mut self, event: &session::Event) {
        match event {
            session::Event::Status { status } => {
                self.status = *status;
                if *status == session::Status::Active {
                    self.root.status = agent::Status::Running;
                }
                if *status == session::Status::Error {
                    self.root.status = agent::Status::Failed;
                }
            }
            session::Event::TurnStart { turn_id } => {
                self.turn(turn_id);
                self.root.status = agent::Status::Running;
                self.status = session::Status::Active;
            }
            session::Event::ItemStart { turn_id, item }
            | session::Event::ItemEnd { turn_id, item } => {
                let materialized = self.turn(turn_id);
                if let Some(id) = item_id(item) {
                    if matches!(event, session::Event::ItemStart { .. }) {
                        materialized.open_items.insert(id.into());
                    } else {
                        materialized.open_items.remove(id);
                    }
                }
                upsert(&mut materialized.turn.items, item.clone());
            }
            session::Event::ItemDelta {
                turn_id,
                item_id: id,
                delta,
            } => {
                let materialized = self.turn(turn_id);
                materialized.open_items.insert(id.clone());
                let items = &mut materialized.turn.items;
                if !items.iter().any(|item| item_id(item) == Some(id)) {
                    let placeholder = match delta {
                        turn::Delta::Text { .. } => Some(turn::Item::AgentMessage {
                            id: id.clone(),
                            text: String::new(),
                        }),
                        turn::Delta::Thinking { .. } => Some(turn::Item::Thinking {
                            id: id.clone(),
                            text: String::new(),
                            summarized: true,
                        }),
                        // No command text or tool metadata is known yet.
                        turn::Delta::Output { .. } => None,
                    };
                    if let Some(item) = placeholder {
                        items.push(item);
                    }
                }
                if let Some(item) = items.iter_mut().find(|item| item_id(item) == Some(id)) {
                    append(item, delta);
                }
            }
            session::Event::TurnEnd { turn } => {
                let materialized = self.turn(&turn.id);
                materialized.ended = true;
                materialized.turn = turn::Turn {
                    items: std::mem::take(&mut materialized.turn.items),
                    ..turn.clone()
                };
                self.root.status = match turn.status {
                    turn::Status::Completed => agent::Status::Completed,
                    turn::Status::Failed => agent::Status::Failed,
                    turn::Status::Interrupted => agent::Status::Interrupted,
                    turn::Status::InProgress => agent::Status::Running,
                };
                self.status = session::Status::Idle;
            }
            session::Event::Closed { reason } => {
                self.closed = true;
                if matches!(reason, session::CloseReason::Agent) {
                    self.root.status = agent::Status::Closed;
                } else if !self.turns.is_empty() {
                    // Detach/process loss is not confirmation that work stopped.
                    self.root.status = agent::Status::Unknown;
                }
                self.turns.clear();
                self.status = session::Status::NotLoaded;
            }
            _ => {}
        }
        self.turns
            .retain(|_, turn| !turn.ended || !turn.open_items.is_empty());
        if !self.closed && !self.turns.is_empty() {
            self.status = session::Status::Active;
        } else if !self.closed && self.status == session::Status::Active {
            // Only turn/item lifecycle can establish that materialized activity
            // ended; do not override a native Active with no known turn.
            if matches!(event, session::Event::ItemEnd { .. }) {
                self.status = session::Status::Idle;
            }
        }
    }
}

fn item_id(item: &turn::Item) -> Option<&str> {
    match item {
        turn::Item::UserMessage { id, .. }
        | turn::Item::AgentMessage { id, .. }
        | turn::Item::Thinking { id, .. }
        | turn::Item::Plan { id, .. } => Some(id),
        turn::Item::ToolCall(tool) => Some(&tool.id),
        turn::Item::Other { id, .. } => id.as_deref(),
    }
}

fn upsert(items: &mut Vec<turn::Item>, item: turn::Item) {
    if let Some(index) = item_id(&item).and_then(|id| {
        items
            .iter()
            .position(|candidate| item_id(candidate) == Some(id))
    }) {
        items[index] = item;
    } else {
        items.push(item);
    }
}

fn append(item: &mut turn::Item, delta: &turn::Delta) {
    match (item, delta) {
        (turn::Item::AgentMessage { text, .. }, turn::Delta::Text { text: addition })
        | (turn::Item::Thinking { text, .. }, turn::Delta::Thinking { text: addition }) => {
            text.push_str(addition)
        }
        (
            turn::Item::ToolCall(turn::ToolCall {
                detail: turn::ToolDetail::Command { output, .. },
                ..
            }),
            turn::Delta::Output { text },
        ) => {
            output.get_or_insert_default().push_str(text);
        }
        _ => {}
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
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
        gaps: vec!["Stored descendant discovery on resume is not implemented".into(), "Early traffic buffering is bounded; transcript gaps are reported as raw observationGap events".into()],
    };
    obs::Capabilities {
        records: live.clone(),
        snapshots: unavailable.clone(),
        agent_discovery: live.clone(),
        parent_links: live.clone(),
        nested_discovery: live.clone(),
        agent_lifecycle: live.clone(),
        agent_transcript: live,
        agent_history: unavailable.clone(),
        task_discovery: unavailable.clone(),
        task_lifecycle: unavailable.clone(),
        task_output: unavailable.clone(),
        task_history: unavailable.clone(),
        stop_agent: unavailable.clone(),
        stop_subtree: unavailable.clone(),
        stop_task: unavailable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn info() -> session::Info {
        serde_json::from_value(json!({
            "summary": {"id": "root", "provider": "codex", "title": null,
                "preview": "", "cwd": "/fixture", "model": "gpt-6-luna", "effort": "medium",
                "status": "idle", "createdAt": 1, "updatedAt": 1, "source": "appServer"},
            "model": "gpt-6-luna", "effort": "medium", "fastMode": false, "cwd": "/fixture",
            "approvalPolicy": null, "sandbox": null, "restored": false, "persisted": false
        }))
        .unwrap()
    }

    fn make_observation() -> Observation {
        Observation::new(&info())
    }

    fn drain(receiver: &mut broadcast::Receiver<obs::Record>) -> Vec<obs::Record> {
        let mut records = vec![];
        while let Ok(record) = receiver.try_recv() {
            records.push(record);
        }
        records
    }

    fn end(id: &str) -> session::Event {
        session::Event::TurnEnd {
            turn: turn::Turn {
                id: id.into(),
                status: turn::Status::Completed,
                items: vec![],
                started_at: Some(1),
                completed_at: Some(2),
                duration_ms: Some(1000),
                error: None,
            },
        }
    }

    #[test]
    fn records_announce_root_attribute_items_and_preserve_legacy_projection() {
        let observation = make_observation();
        let mut records = observation.subscribe_records();
        let mut legacy = observation.subscribe();
        let events = [
            session::Event::TurnStart {
                turn_id: "u".into(),
            },
            session::Event::ItemDelta {
                turn_id: "u".into(),
                item_id: "i".into(),
                delta: turn::Delta::Text {
                    text: "partial".into(),
                },
            },
            session::Event::ItemEnd {
                turn_id: "u".into(),
                item: turn::Item::AgentMessage {
                    id: "i".into(),
                    text: "final".into(),
                },
            },
            end("u"),
        ];
        for event in &events {
            observation.emit(event.clone());
        }
        for event in &events {
            assert_eq!(&legacy.try_recv().unwrap(), event);
        }
        let records = drain(&mut records);
        assert!(
            matches!(&records[0].event, obs::Event::Agent { agent } if agent.id == "root" && agent.parent == agent::Parent::Root)
        );
        let activity: Vec<_> = records
            .iter()
            .filter(|record| {
                matches!(record.event, obs::Event::Activity { .. }) && record.agent_id.is_some()
            })
            .collect();
        assert_eq!(activity.len(), events.len());
        assert!(
            activity
                .iter()
                .all(|record| record.agent_id.as_deref() == Some("root"))
        );
        let state = observation.state.lock().unwrap();
        assert_eq!(state.seq, records.last().unwrap().seq);
        assert!(state.turns.is_empty());
        assert_eq!(state.root.status, agent::Status::Completed);
    }

    #[test]
    fn reducer_materializes_deltas_and_final_items_replace_them() {
        let observation = make_observation();
        let mut records = observation.subscribe_records();
        observation.emit(session::Event::TurnStart {
            turn_id: "u".into(),
        });
        for text in ["hel", "lo"] {
            observation.emit(session::Event::ItemDelta {
                turn_id: "u".into(),
                item_id: "i".into(),
                delta: turn::Delta::Text { text: text.into() },
            });
        }
        let stream_id = observation.state.lock().unwrap().stream_id.clone();
        let mut refreshed = info();
        refreshed.model = "reported-model".into();
        observation.refresh_metadata(&refreshed);
        assert!(
            matches!(drain(&mut records).last().map(|record| &record.event),
            Some(obs::Event::Agent { agent }) if agent.model.as_deref() == Some("reported-model") && agent.status == agent::Status::Running)
        );
        {
            let state = observation.state.lock().unwrap();
            assert_eq!(state.stream_id, stream_id);
            assert!(
                matches!(&state.turns["u"].turn.items[0], turn::Item::AgentMessage { text, .. } if text == "hello")
            );
        }
        observation.emit(session::Event::ItemEnd {
            turn_id: "u".into(),
            item: turn::Item::AgentMessage {
                id: "i".into(),
                text: "final".into(),
            },
        });
        let state = observation.state.lock().unwrap();
        assert_eq!(state.turns["u"].turn.items.len(), 1);
        assert!(
            matches!(&state.turns["u"].turn.items[0], turn::Item::AgentMessage { text, .. } if text == "final")
        );
    }

    #[test]
    fn unfinished_command_survives_turn_end_and_native_idle_until_item_end() {
        let observation = make_observation();
        let mut records = observation.subscribe_records();
        let mut legacy = observation.subscribe();
        let item = turn::Item::ToolCall(turn::ToolCall {
            id: "command".into(),
            status: turn::ToolStatus::InProgress,
            duration_ms: None,
            detail: turn::ToolDetail::Command {
                command: "sleep 60".into(),
                cwd: None,
                output: None,
                exit_code: None,
            },
        });
        observation.emit(session::Event::ItemStart {
            turn_id: "u".into(),
            item: item.clone(),
        });
        observation.emit(end("u"));
        observation.emit(session::Event::Status {
            status: session::Status::Idle,
        });
        observation.emit(session::Event::ItemDelta {
            turn_id: "u".into(),
            item_id: "command".into(),
            delta: turn::Delta::Output {
                text: "done".into(),
            },
        });
        {
            let state = observation.state.lock().unwrap();
            assert_eq!(state.status, session::Status::Active);
            assert_eq!(state.turns["u"].turn.status, turn::Status::Completed);
            assert!(
                matches!(&state.turns["u"].turn.items[0], turn::Item::ToolCall(turn::ToolCall {
                detail: turn::ToolDetail::Command { output: Some(text), .. }, ..
            }) if text == "done")
            );
        }
        let events = drain(&mut records);
        assert!(events.iter().any(|record| record.agent_id.is_none()
            && matches!(
                record.event,
                obs::Event::Activity {
                    event: session::Event::Status {
                        status: session::Status::Active
                    },
                    ..
                }
            )));
        assert!(
            std::iter::from_fn(|| legacy.try_recv().ok()).any(|event| event
                == session::Event::Status {
                    status: session::Status::Idle
                })
        );
        observation.emit(session::Event::ItemEnd {
            turn_id: "u".into(),
            item,
        });
        let state = observation.state.lock().unwrap();
        assert!(state.turns.is_empty());
        assert_eq!(state.status, session::Status::Idle);
    }

    #[test]
    fn subscribers_share_sequence_and_bootstrap_without_resetting_stream() {
        let observation = make_observation();
        let mut first = observation.subscribe_records();
        observation.emit(session::Event::TurnStart {
            turn_id: "u".into(),
        });
        let mut second = observation.subscribe_records();
        observation.emit(session::Event::Raw {
            name: "future/event".into(),
            payload: json!({"threadId":"root"}),
        });
        let first = drain(&mut first);
        let second = drain(&mut second);
        assert_eq!(&first[first.len() - second.len()..], second);
        assert!(
            first.windows(2).all(
                |pair| pair[0].seq + 1 == pair[1].seq && pair[0].stream_id == pair[1].stream_id
            )
        );
        assert!(matches!(second[0].event, obs::Event::Agent { .. }));
        assert_ne!(
            first[0].stream_id,
            make_observation().state.lock().unwrap().stream_id
        );
    }

    #[test]
    fn process_loss_is_not_work_failure_and_duplicate_close_is_suppressed() {
        let observation = make_observation();
        let mut records = observation.subscribe_records();
        observation.emit(session::Event::TurnStart {
            turn_id: "u".into(),
        });
        let closed = session::Event::Closed {
            reason: session::CloseReason::Exited {
                message: "lost".into(),
            },
        };
        observation.emit(closed.clone());
        observation.emit(closed);
        observation.emit(end("u"));
        let records = drain(&mut records);
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(
                    record.event,
                    obs::Event::Activity {
                        event: session::Event::Closed { .. },
                        ..
                    }
                ))
                .count(),
            1
        );
        let state = observation.state.lock().unwrap();
        assert_eq!(state.root.status, agent::Status::Unknown);
        assert_eq!(state.status, session::Status::NotLoaded);
        assert!(state.turns.is_empty());
        assert!(state.closed);
    }
    #[test]
    fn siblings_have_independent_items_and_outlive_the_root_turn() {
        let observation = make_observation();
        let mut records = observation.subscribe_records();
        for id in ["a", "b"] {
            observation.upsert_agent(agent::Summary {
                id: id.into(),
                parent: agent::Parent::Agent { id: "root".into() },
                name: None,
                role: None,
                model: None,
                description: None,
                background: None,
                status: agent::Status::Unknown,
                origin: None,
                created_at: None,
                updated_at: None,
            });
            observation.emit_for(
                id,
                session::Event::TurnStart {
                    turn_id: "same-turn".into(),
                },
            );
        }
        for (id, text) in [("a", "alpha"), ("b", "beta")] {
            observation.emit_for(
                id,
                session::Event::ItemDelta {
                    turn_id: "same-turn".into(),
                    item_id: "same-item".into(),
                    delta: turn::Delta::Text { text: text.into() },
                },
            );
        }
        observation.emit(end("root-turn"));
        {
            let state = observation.state.lock().unwrap();
            assert_eq!(state.aggregate_status(), session::Status::Active);
            for (id, expected) in [("a", "alpha"), ("b", "beta")] {
                assert!(
                    matches!(&state.children[id].turns["same-turn"].turn.items[0], turn::Item::AgentMessage { text, .. } if text == expected)
                );
            }
        }
        observation.emit_for(
            "a",
            session::Event::Closed {
                reason: session::CloseReason::Agent,
            },
        );
        observation.emit_for(
            "b",
            session::Event::ItemEnd {
                turn_id: "same-turn".into(),
                item: turn::Item::AgentMessage {
                    id: "same-item".into(),
                    text: "beta".into(),
                },
            },
        );
        observation.emit_for("b", end("same-turn"));
        observation.emit_for(
            "b",
            session::Event::TurnStart {
                turn_id: "continued".into(),
            },
        );
        observation.emit_for(
            "b",
            session::Event::Closed {
                reason: session::CloseReason::Agent,
            },
        );
        let state = observation.state.lock().unwrap();
        assert!(!state.closed);
        assert_eq!(state.children["a"].root.status, agent::Status::Closed);
        assert_eq!(state.children["b"].root.status, agent::Status::Closed);
        assert_eq!(state.aggregate_status(), session::Status::Idle);
        drop(state);
        observation.emit_for(
            "b",
            session::Event::TurnStart {
                turn_id: "reopened".into(),
            },
        );
        assert_eq!(
            observation.state.lock().unwrap().children["b"].root.status,
            agent::Status::Running
        );
        assert!(!drain(&mut records).iter().any(|record| matches!(
            record.event,
            obs::Event::Activity {
                event: session::Event::Closed { .. },
                ..
            }
        )));
    }
}
