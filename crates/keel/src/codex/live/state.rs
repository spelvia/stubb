//! Authoritative attachment state, record publication and turn/item reduction.

use std::collections::{BTreeMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::broadcast;

use super::controls;
use crate::{agent, observation as obs, session, turn};

pub(super) struct State {
    pub(super) stream_id: String,
    pub(super) seq: u64,
    pub(super) root: Branch,
    pub(super) children: BTreeMap<String, Branch>,
    pub(super) announced: bool,
    pub(super) tasks: super::tasks::Tasks,
    pub(super) controls: BTreeMap<String, controls::Pending>,
}

pub(super) struct Branch {
    pub(super) agent: agent::Summary,
    pub(super) closed: bool,
    pub(super) loaded: bool,
    pub(super) status: session::Status,
    pub(super) turns: BTreeMap<String, Materialized>,
}

pub(super) struct Materialized {
    pub(super) turn: turn::Turn,
    pub(super) ended: bool,
    pub(super) open_items: HashSet<String>,
}

impl State {
    pub(super) fn publish_status_change(
        &mut self,
        before: session::Status,
        records: &broadcast::Sender<obs::Record>,
    ) {
        let status = self.aggregate_status();
        if status != before {
            self.publish(
                records,
                None,
                obs::Event::Activity {
                    event: session::Event::Status { status },
                    usage_scope: None,
                },
            );
        }
    }

    pub(super) fn announce(&mut self, records: &broadcast::Sender<obs::Record>) {
        self.announced = true;
        self.publish(
            records,
            None,
            obs::Event::Agent {
                agent: self.root.agent.clone(),
            },
        );
    }

    pub(super) fn publish(
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

    pub(super) fn aggregate_status(&self) -> session::Status {
        if self.root.closed {
            return session::Status::NotLoaded;
        }
        if self.tasks.running()
            || self.root.status == session::Status::Active
            || self
                .children
                .values()
                .any(|child| child.status == session::Status::Active)
        {
            session::Status::Active
        } else {
            self.root.status
        }
    }

    pub(super) fn new(info: &session::Info) -> Self {
        Self {
            stream_id: uuid::Uuid::new_v4().to_string(),
            seq: 0,
            root: Branch {
                agent: agent::Summary {
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
                loaded: true,
                status: info.summary.status,
                turns: BTreeMap::new(),
            },
            children: BTreeMap::new(),
            announced: false,
            tasks: Default::default(),
            controls: BTreeMap::new(),
        }
    }

    pub(super) fn snapshot(&self) -> obs::Snapshot {
        let branches = std::iter::once(&self.root).chain(self.children.values());
        obs::Snapshot {
            stream_id: self.stream_id.clone(),
            seq: self.seq,
            status: self.aggregate_status(),
            root_agent_id: self.root.agent.id.clone(),
            agents: std::iter::once(self.root.agent.clone())
                .chain(self.children.values().map(|branch| branch.agent.clone()))
                .collect(),
            tasks: self.tasks.summaries(),
            active_turns: branches
                .flat_map(|branch| {
                    branch.turns.values().map(|materialized| agent::OwnedTurn {
                        agent_id: branch.agent.id.clone(),
                        turn: materialized.turn.clone(),
                    })
                })
                .collect(),
            pending_controls: self
                .controls
                .values()
                .filter(|pending| pending.receipt.is_pending())
                .map(|pending| pending.receipt.clone())
                .collect(),
            capabilities: super::capabilities(),
        }
    }

    pub(super) fn apply(
        &mut self,
        id: &str,
        event: session::Event,
        records: &broadcast::Sender<obs::Record>,
        legacy: &broadcast::Sender<session::Event>,
    ) {
        if self.root.closed {
            return;
        }
        if !self.announced {
            self.announce(records);
        }
        let root_event = id == self.root.agent.id;
        let old_status = self.aggregate_status();
        let branch = if root_event {
            &mut self.root
        } else {
            let Some(branch) = self.children.get_mut(id) else {
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
        branch.loaded = true;
        let previous = branch.agent.clone();
        branch.reduce(&event);
        let replacement = (previous != branch.agent).then(|| branch.agent.clone());
        if let Some(agent) = replacement {
            self.publish(records, None, obs::Event::Agent { agent });
        }
        if root_event && matches!(event, session::Event::Closed { .. }) {
            let replacements: Vec<_> = self
                .children
                .values_mut()
                .filter_map(|child| {
                    if child.status == session::Status::Active || !child.turns.is_empty() {
                        child.agent.status = agent::Status::Unknown;
                        child.status = session::Status::NotLoaded;
                        child.turns.clear();
                        Some(child.agent.clone())
                    } else {
                        None
                    }
                })
                .collect();
            for agent in replacements {
                self.publish(records, None, obs::Event::Agent { agent });
            }
        }
        let updates = if matches!(event, session::Event::Closed { .. }) {
            self.tasks.lost(if root_event { None } else { Some(id) })
        } else {
            self.tasks.event(id, &event)
        };
        for task in updates {
            self.publish(records, None, obs::Event::Task { task });
        }
        self.control_event(id, &event, records);
        let status = self.aggregate_status();
        if (old_status != status || matches!(event, session::Event::Status { .. }))
            && !(root_event && matches!(event, session::Event::Closed { .. }))
        {
            self.publish(
                records,
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
            self.publish(
                records,
                agent_id,
                obs::Event::Activity {
                    event: event.clone(),
                    usage_scope,
                },
            );
        }
        if root_event {
            let _ = legacy.send(event);
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
                    self.agent.status = agent::Status::Running;
                }
                if *status == session::Status::Error {
                    self.agent.status = agent::Status::Failed;
                }
            }
            session::Event::TurnStart { turn_id } => {
                self.turn(turn_id);
                self.agent.status = agent::Status::Running;
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
                self.agent.status = match turn.status {
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
                    self.agent.status = agent::Status::Closed;
                } else if !self.turns.is_empty() {
                    // Detach/process loss is not confirmation that work stopped.
                    self.agent.status = agent::Status::Unknown;
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
