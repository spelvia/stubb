//! Materialized active items for snapshot recovery, owned by one native agent.

use std::collections::{BTreeMap, HashSet};

use crate::claude::events::SessionState;
use crate::{agent, session, turn};

pub(super) struct Branch {
    pub(super) agent: agent::Summary,
    pub(super) mapper: SessionState,
    pub(super) current: Option<String>,
    pub(super) spawn: Option<String>,
    pub(super) executions: HashSet<String>,
    turns: BTreeMap<String, Materialized>,
}

struct Materialized {
    turn: turn::Turn,
    ended: bool,
    open: HashSet<String>,
}

impl Branch {
    pub(super) fn new(agent: agent::Summary) -> Self {
        Self {
            agent,
            mapper: Default::default(),
            current: None,
            spawn: None,
            executions: HashSet::new(),
            turns: BTreeMap::new(),
        }
    }

    pub(super) fn has_active_turns(&self) -> bool {
        !self.turns.is_empty()
    }
    pub(super) fn clear_turns(&mut self) {
        self.turns.clear();
    }
    pub(super) fn active_turns(&self) -> impl Iterator<Item = agent::OwnedTurn> + '_ {
        self.turns.values().map(|m| agent::OwnedTurn {
            agent_id: self.agent.id.clone(),
            turn: m.turn.clone(),
        })
    }

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
            open: HashSet::new(),
        })
    }

    pub(super) fn reduce(&mut self, event: &session::Event) {
        match event {
            session::Event::TurnStart { turn_id } => {
                self.current = Some(turn_id.clone());
                self.turn(turn_id);
                self.agent.status = agent::Status::Running;
            }
            session::Event::ItemStart { turn_id, item }
            | session::Event::ItemEnd { turn_id, item } => {
                let m = self.turn(turn_id);
                if let Some(id) = item_id(item) {
                    if matches!(event, session::Event::ItemStart { .. }) {
                        m.open.insert(id.into());
                    } else {
                        m.open.remove(id);
                    }
                }
                upsert(&mut m.turn.items, item.clone());
            }
            session::Event::ItemDelta {
                turn_id,
                item_id: id,
                delta,
            } => {
                let m = self.turn(turn_id);
                m.open.insert(id.clone());
                if let Some(item) = m
                    .turn
                    .items
                    .iter_mut()
                    .find(|item| item_id(item) == Some(id))
                {
                    match (item, delta) {
                        (
                            turn::Item::AgentMessage { text, .. },
                            turn::Delta::Text { text: addition },
                        )
                        | (
                            turn::Item::Thinking { text, .. },
                            turn::Delta::Thinking { text: addition },
                        ) => text.push_str(addition),
                        _ => {}
                    }
                }
            }
            session::Event::TurnEnd { turn } => {
                if self.current.as_ref() == Some(&turn.id) {
                    self.current = None;
                }
                let m = self.turn(&turn.id);
                m.ended = true;
                m.turn = turn::Turn {
                    items: std::mem::take(&mut m.turn.items),
                    ..turn.clone()
                };
                // A native execution bookend ends its unfinished tool items too.
                // Their outcome stays as reported; it is never inferred as success.
                m.open.clear();
                self.agent.status = match turn.status {
                    turn::Status::Completed => agent::Status::Completed,
                    turn::Status::Failed => agent::Status::Failed,
                    turn::Status::Interrupted => agent::Status::Interrupted,
                    turn::Status::InProgress => agent::Status::Running,
                    turn::Status::Unknown => agent::Status::Unknown,
                };
            }
            _ => {}
        }
        self.turns.retain(|_, m| !m.ended || !m.open.is_empty());
    }
}

fn item_id(item: &turn::Item) -> Option<&str> {
    match item {
        turn::Item::UserMessage { id, .. }
        | turn::Item::AgentMessage { id, .. }
        | turn::Item::Thinking { id, .. }
        | turn::Item::Plan { id, .. } => Some(id),
        turn::Item::ToolCall(call) => Some(&call.id),
        turn::Item::Other { id, .. } => id.as_deref(),
        turn::Item::Unknown(raw) => raw.get("id").and_then(serde_json::Value::as_str),
    }
}

fn upsert(items: &mut Vec<turn::Item>, item: turn::Item) {
    if let Some(index) =
        item_id(&item).and_then(|id| items.iter().position(|i| item_id(i) == Some(id)))
    {
        items[index] = item;
    } else {
        items.push(item);
    }
}
