//! Native identities, routing and sequenced publication under one lock.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use tokio::sync::broadcast;

use super::turns::Branch;
use crate::{agent, observation as obs, session, turn};

type Records = broadcast::Sender<obs::Record>;
type Legacy = broadcast::Sender<session::Event>;
const EARLY_FRAMES: usize = 256;
const EARLY_BYTES: usize = 1024 * 1024;

pub(super) struct State {
    pub(super) root: Branch,
    children: BTreeMap<String, Branch>,
    /// A spawning tool call's ID identifies its delegated conversation.
    delegated: HashMap<String, String>,
    /// Native tool ownership, not inferred from depth or descriptions.
    owners: HashMap<String, agent::Origin>,
    early: VecDeque<Value>,
    early_bytes: usize,
    stream_id: String,
    seq: u64,
    announced: bool,
    closed: bool,
    gap: bool,
}

impl State {
    pub(super) fn new(id: &str) -> Self {
        Self {
            root: Branch::new(agent::Summary {
                id: id.into(),
                parent: agent::Parent::Root,
                name: None,
                role: None,
                model: None,
                description: None,
                background: None,
                status: agent::Status::Unknown,
                origin: None,
                created_at: None,
                updated_at: None,
            }),
            children: BTreeMap::new(),
            delegated: HashMap::new(),
            owners: HashMap::new(),
            early: VecDeque::new(),
            early_bytes: 0,
            stream_id: uuid::Uuid::new_v4().to_string(),
            seq: 0,
            announced: false,
            closed: false,
            gap: false,
        }
    }

    fn publish(&mut self, records: &Records, owner: Option<String>, event: obs::Event) {
        // The short-lived catalog process has no observed session.
        if self.root.agent.id.is_empty() {
            return;
        }
        self.seq = self
            .seq
            .checked_add(1)
            .expect("observation sequence exhausted");
        let _ = records.send(obs::Record {
            stream_id: self.stream_id.clone(),
            seq: self.seq,
            at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                .try_into()
                .unwrap_or(i64::MAX),
            agent_id: owner,
            event,
        });
    }

    pub(super) fn announce_root(&mut self, records: &Records) {
        self.announced = true;
        self.publish(
            records,
            None,
            obs::Event::Agent {
                agent: self.root.agent.clone(),
            },
        );
    }

    pub(super) fn announce_all(&mut self, records: &Records) {
        self.announce_root(records);
        let agents = self
            .children
            .values()
            .map(|b| b.agent.clone())
            .collect::<Vec<_>>();
        for agent in agents {
            self.publish(records, None, obs::Event::Agent { agent });
        }
    }

    fn status(&self) -> session::Status {
        if self.closed {
            session::Status::NotLoaded
        } else if std::iter::once(&self.root)
            .chain(self.children.values())
            .any(|b| b.agent.status == agent::Status::Running || b.has_active_turns())
        {
            session::Status::Active
        } else {
            session::Status::Idle
        }
    }

    pub(super) fn snapshot(&self) -> obs::Snapshot {
        let mut capabilities = super::capabilities();
        if self.gap {
            capabilities
                .agent_transcript
                .gaps
                .push("This attachment dropped unresolved native traffic".into());
        }
        obs::Snapshot {
            stream_id: self.stream_id.clone(),
            seq: self.seq,
            status: self.status(),
            root_agent_id: self.root.agent.id.clone(),
            agents: std::iter::once(&self.root)
                .chain(self.children.values())
                .map(|b| b.agent.clone())
                .collect(),
            tasks: vec![],
            active_turns: std::iter::once(&self.root)
                .chain(self.children.values())
                .flat_map(Branch::active_turns)
                .collect(),
            pending_controls: vec![],
            capabilities,
        }
    }

    fn raw(&mut self, name: String, payload: Value, records: &Records) {
        self.publish(
            records,
            None,
            obs::Event::Activity {
                event: session::Event::Raw { name, payload },
                usage_scope: None,
            },
        );
    }

    fn gap(&mut self, message: &Value, reason: &str, records: &Records) {
        self.gap = true;
        self.raw(
            "keel/observationGap".into(),
            json!({"reason":reason, "native":message}),
            records,
        );
    }

    pub(super) fn root_events(
        &mut self,
        events: Vec<session::Event>,
        records: &Records,
        legacy: &Legacy,
    ) {
        let id = self.root.agent.id.clone();
        for event in events {
            self.apply(&id, event, records, legacy);
        }
    }

    fn apply(&mut self, id: &str, event: session::Event, records: &Records, legacy: &Legacy) {
        if self.closed {
            return;
        }
        if !self.announced {
            self.announce_root(records);
        }
        let before = self.status();
        let root = id == self.root.agent.id;
        let branch = if root {
            &mut self.root
        } else {
            let Some(branch) = self.children.get_mut(id) else {
                return;
            };
            branch
        };
        let previous = branch.agent.clone();
        branch.reduce(&event);
        let replacement = (previous != branch.agent).then(|| branch.agent.clone());
        if let session::Event::ItemStart {
            turn_id,
            item: turn::Item::ToolCall(call),
        } = &event
        {
            self.owners.insert(
                call.id.clone(),
                agent::Origin {
                    agent_id: id.into(),
                    turn_id: Some(turn_id.clone()),
                    item_id: Some(call.id.clone()),
                },
            );
        }
        if let Some(agent) = replacement {
            self.publish(records, None, obs::Event::Agent { agent });
        }
        let status = self.status();
        if status != before || matches!(event, session::Event::Status { .. }) {
            self.publish(
                records,
                None,
                obs::Event::Activity {
                    event: session::Event::Status { status },
                    usage_scope: None,
                },
            );
        }
        if !matches!(event, session::Event::Status { .. }) {
            let owner = (!matches!(event, session::Event::Raw { .. })).then(|| id.to_owned());
            let usage_scope =
                matches!(event, session::Event::Usage { .. }).then_some(obs::UsageScope::Unknown);
            self.publish(
                records,
                owner,
                obs::Event::Activity {
                    event: event.clone(),
                    usage_scope,
                },
            );
        }
        if root {
            let _ = legacy.send(event);
        }
        self.resolve_parents(records);
    }

    fn resolve_parents(&mut self, records: &Records) {
        let repairs = self
            .delegated
            .iter()
            .filter_map(|(tool, id)| {
                let origin = self.owners.get(tool)?;
                let branch = self.children.get(id)?;
                if branch.spawn.as_deref() != Some(tool) {
                    return None;
                }
                (branch.agent.origin.as_ref() != Some(origin)).then(|| (id.clone(), origin.clone()))
            })
            .collect::<Vec<_>>();
        for (id, origin) in repairs {
            let branch = self.children.get_mut(&id).unwrap();
            // The ownership graph is explicit; no parent can be inferred from depth.
            branch.agent.parent = agent::Parent::Agent {
                id: origin.agent_id.clone(),
            };
            branch.agent.origin = Some(origin);
            let agent = branch.agent.clone();
            self.publish(records, None, obs::Event::Agent { agent });
        }
    }

    fn start_agent(&mut self, message: &Value, records: &Records, legacy: &Legacy) {
        let (Some(id), Some(tool)) = (text(message, "task_id"), text(message, "tool_use_id"))
        else {
            self.gap(
                message,
                "Agent start omitted native task or spawning tool identity",
                records,
            );
            return;
        };
        if id == self.root.agent.id {
            self.gap(
                message,
                "Agent task identity conflicts with the root",
                records,
            );
            return;
        }
        if self
            .delegated
            .get(&tool)
            .is_some_and(|existing| existing != &id)
        {
            self.gap(
                message,
                "Spawning tool identity was reused for a different agent",
                records,
            );
            return;
        }
        self.delegated.insert(tool.clone(), id.clone());
        let origin = self.owners.get(&tool).cloned();
        let branch = self.children.entry(id.clone()).or_insert_with(|| {
            Branch::new(agent::Summary {
                id: id.clone(),
                parent: agent::Parent::Unknown,
                name: None,
                role: None,
                model: None,
                description: None,
                background: None,
                status: agent::Status::Unknown,
                origin: None,
                created_at: None,
                updated_at: None,
            })
        });
        // Duplicated bookends must not reset an active or completed execution.
        if !branch.executions.insert(tool.clone()) {
            return;
        }
        if branch.current.is_some() {
            branch.executions.remove(&tool);
            self.gap(
                message,
                "Overlapping executions reuse the same native agent identity",
                records,
            );
            return;
        }
        branch.current = Some(tool.clone());
        branch.agent.role = text(message, "subagent_type");
        branch.agent.description = text(message, "description");
        branch.agent.background = message.get("is_backgrounded").and_then(Value::as_bool);
        if branch.spawn.is_none() {
            branch.spawn = Some(tool.clone());
            branch.agent.origin = origin.clone();
            branch.agent.parent = origin.map_or(agent::Parent::Unknown, |o| agent::Parent::Agent {
                id: o.agent_id,
            });
        }
        let agent = branch.agent.clone();
        let events = branch.mapper.delegated_turn(&tool);
        self.publish(records, None, obs::Event::Agent { agent });
        for event in events {
            self.apply(&id, event, records, legacy);
        }
        self.drain_early(records, legacy);
    }

    fn finish_agent(&mut self, message: &Value, records: &Records, legacy: &Legacy) {
        let Some(id) = text(message, "task_id") else {
            return;
        };
        let Some(branch) = self.children.get_mut(&id) else {
            return;
        };
        let Some(tool) = text(message, "tool_use_id") else {
            self.gap(
                message,
                "Agent completion omitted its execution identity",
                records,
            );
            return;
        };
        if branch.current.as_deref() != Some(&tool) {
            return;
        }
        let status = match message.get("status").and_then(Value::as_str) {
            Some("completed") => turn::Status::Completed,
            Some("failed") => turn::Status::Failed,
            Some("stopped") => turn::Status::Interrupted,
            _ => return,
        };
        branch.current = None;
        self.apply(
            &id,
            session::Event::TurnEnd {
                turn: turn::Turn {
                    id: tool,
                    status,
                    items: vec![],
                    started_at: None,
                    completed_at: None,
                    duration_ms: message
                        .pointer("/usage/duration_ms")
                        .and_then(Value::as_i64),
                    error: (status == turn::Status::Failed)
                        .then(|| text(message, "summary"))
                        .flatten(),
                },
            },
            records,
            legacy,
        );
    }

    pub(super) fn message(&mut self, message: &Value, records: &Records, legacy: &Legacy) {
        if self.closed {
            return;
        }
        if !self.announced {
            self.announce_root(records);
        }
        if let Some(tool) = text(message, "parent_tool_use_id") {
            if self.delegated.contains_key(&tool) {
                self.forward(message, &tool, records, legacy);
            } else {
                self.buffer(message, records);
            }
            return;
        }
        let subtype = message.get("subtype").and_then(Value::as_str);
        if message["type"] == "system" {
            if subtype == Some("task_started")
                && (message["task_type"] == "local_agent"
                    || (message.get("task_type").is_none()
                        && message.get("subagent_type").is_some_and(|v| v.is_string())))
            {
                self.start_agent(message, records, legacy);
            } else if subtype == Some("task_notification") {
                self.finish_agent(message, records, legacy);
            }
        }
        if let Some(model) = message.pointer("/message/model").and_then(Value::as_str)
            && message["type"] == "assistant"
            && self.root.agent.model.as_deref() != Some(model)
        {
            self.root.agent.model = Some(model.into());
            self.announce_root(records);
        }
        let events = self.root.mapper.message(message);
        self.root_events(events, records, legacy);
        self.drain_early(records, legacy);
    }

    fn forward(&mut self, message: &Value, tool: &str, records: &Records, legacy: &Legacy) {
        let id = self.delegated[tool].clone();
        let branch = self.children.get_mut(&id).unwrap();
        if branch.current.as_deref() != Some(tool) {
            self.raw(
                "claude/unresolvedContinuation".into(),
                message.clone(),
                records,
            );
            return;
        }
        if let Some(model) = message.pointer("/message/model").and_then(Value::as_str)
            && branch.agent.model.as_deref() != Some(model)
        {
            branch.agent.model = Some(model.into());
            let agent = branch.agent.clone();
            self.publish(records, None, obs::Event::Agent { agent });
        }
        let events = self
            .children
            .get_mut(&id)
            .unwrap()
            .mapper
            .delegated_message(message);
        for event in events {
            self.apply(&id, event, records, legacy);
        }
    }

    fn buffer(&mut self, message: &Value, records: &Records) {
        let size = message.to_string().len();
        if self.early.len() >= EARLY_FRAMES || self.early_bytes + size > EARLY_BYTES {
            self.gap(
                message,
                "Unresolved descendant traffic exceeded buffering limit",
                records,
            );
            return;
        }
        self.early_bytes += size;
        self.early.push_back(message.clone());
    }

    fn drain_early(&mut self, records: &Records, legacy: &Legacy) {
        let pending = std::mem::take(&mut self.early);
        self.early_bytes = 0;
        for message in pending {
            let tool = text(&message, "parent_tool_use_id").unwrap();
            if self.delegated.contains_key(&tool) {
                self.forward(&message, &tool, records, legacy);
            } else {
                self.early_bytes += message.to_string().len();
                self.early.push_back(message);
            }
        }
    }

    pub(super) fn tool_declined(
        &mut self,
        tool: &str,
        id: Option<&str>,
        input: &Value,
        records: &Records,
        legacy: &Legacy,
    ) {
        if self.closed {
            return;
        }
        let owner = id
            .and_then(|id| self.owners.get(id))
            .map(|o| o.agent_id.clone());
        let Some(owner) = owner else {
            // Keep the legacy permission notice, but unresolved ownership is
            // never a promise that the request originated in the root agent.
            let events = self.root.mapper.tool_declined(tool, id, input);
            for event in events {
                let _ = legacy.send(event.clone());
                let event = match event {
                    session::Event::RequestDeclined {
                        method, summary, ..
                    } => session::Event::RequestDeclined {
                        method,
                        summary,
                        turn_id: None,
                    },
                    event => event,
                };
                self.publish(
                    records,
                    None,
                    obs::Event::Activity {
                        event,
                        usage_scope: None,
                    },
                );
            }
            return;
        };
        let branch = if owner == self.root.agent.id {
            &mut self.root
        } else {
            self.children.get_mut(&owner).unwrap()
        };
        let events = branch.mapper.tool_declined(tool, id, input);
        for event in events {
            self.apply(&owner, event, records, legacy);
        }
    }

    pub(super) fn close(
        &mut self,
        reason: session::CloseReason,
        records: &Records,
        legacy: &Legacy,
    ) {
        if self.closed {
            return;
        }
        if !self.early.is_empty() {
            self.gap(
                &json!({"frames":self.early.len()}),
                "Attachment closed with unresolved descendant traffic",
                records,
            );
        }
        self.closed = true;
        self.early.clear();
        self.early_bytes = 0;
        let agents = std::iter::once(&mut self.root)
            .chain(self.children.values_mut())
            .filter_map(|b| {
                let active = b.current.take().is_some()
                    || b.has_active_turns()
                    || b.agent.status == agent::Status::Running;
                b.clear_turns();
                if active {
                    b.agent.status = agent::Status::Unknown;
                    Some(b.agent.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        for agent in agents {
            self.publish(records, None, obs::Event::Agent { agent });
        }
        let event = session::Event::Closed { reason };
        self.publish(
            records,
            None,
            obs::Event::Activity {
                event: event.clone(),
                usage_scope: None,
            },
        );
        let _ = legacy.send(event);
    }
}

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}
