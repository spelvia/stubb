//! Discover native ancestry before RPC's exact-thread filter. Early descendant
//! events are held only until native evidence connects them to an observed root.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::super::{events, types};
use super::Attachment;
use crate::providers::codex::transport::rpc::EventSink;
use crate::{observation::agent, session};

const EARLY_EVENTS: usize = 1024;

pub(in crate::providers::codex) struct Router {
    state: Mutex<State>,
    metadata: mpsc::Sender<String>,
}

#[derive(Default)]
struct State {
    roots: HashMap<String, Arc<Attachment>>,
    nodes: BTreeMap<String, agent::Summary>,
    known_roots: HashSet<String>,
    referenced_by: HashMap<String, HashSet<String>>,
    pending: VecDeque<(String, session::Event)>,
    requested: HashSet<String>,
    loaded: HashMap<String, bool>,
}

impl Router {
    pub(in crate::providers::codex) fn new(metadata: mpsc::Sender<String>) -> Self {
        Self {
            state: Mutex::new(State::default()),
            metadata,
        }
    }

    /// Sessions attached and not closed.
    pub(in crate::providers::codex) fn live_sessions(&self) -> usize {
        let state = self.state.lock().expect("routing lock");
        state
            .roots
            .values()
            .filter(|root| !root.is_closed())
            .count()
    }

    pub(in crate::providers::codex) fn terminal_targets(&self) -> Vec<(String, Arc<Attachment>)> {
        let state = self.state.lock().expect("routing lock");
        state
            .roots
            .values()
            .flat_map(|root| {
                root.terminal_threads()
                    .into_iter()
                    .map(|id| (id, Arc::clone(root)))
            })
            .collect()
    }

    pub(in crate::providers::codex) fn observe(&self, info: &session::Info) -> Arc<Attachment> {
        let mut state = self.state.lock().expect("routing lock");
        if state
            .roots
            .get(&info.summary.id)
            .is_some_and(|root| root.is_closed())
        {
            state.roots.remove(&info.summary.id);
        }
        state.known_roots.insert(info.summary.id.clone());
        let root = Arc::clone(
            state
                .roots
                .entry(info.summary.id.clone())
                .or_insert_with(|| Arc::new(Attachment::new(info))),
        );
        root.refresh_metadata(info);
        state.reconcile();
        root
    }

    pub(in crate::providers::codex) fn detach(&self, id: &str, expected: &Arc<Attachment>) -> bool {
        let mut state = self.state.lock().expect("routing lock");
        if !state
            .roots
            .get(id)
            .is_some_and(|root| Arc::ptr_eq(root, expected))
        {
            return false;
        }
        state.roots.remove(id);
        true
    }

    pub(in crate::providers::codex) fn notification(&self, method: &str, params: &Value) {
        let mut state = self.state.lock().expect("routing lock");
        if let Some(thread) = params
            .get("thread")
            .and_then(|thread| serde_json::from_value::<types::Thread>(thread.clone()).ok())
        {
            state.metadata(thread);
        }
        let Some(id) = thread_id(params) else {
            return;
        };
        state.loaded.insert(
            id.into(),
            method != "thread/closed" && params["status"]["type"] != "notLoaded",
        );
        if let Some(item) = params.get("item") {
            let parent = text(item, "senderThreadId").unwrap_or_else(|| id.into());
            let origin = agent::Origin {
                agent_id: parent.clone(),
                turn_id: text(params, "turnId"),
                item_id: None,
            };
            match item["type"].as_str() {
                Some("subAgentActivity") => {
                    if let Some(child) = item["agentThreadId"].as_str() {
                        state
                            .referenced_by
                            .entry(child.into())
                            .or_default()
                            .insert(id.into());
                        let started = item["kind"] == "started";
                        state.discover(child, started.then_some(parent), started.then_some(origin));
                    }
                }
                Some("collabAgentToolCall") => {
                    if let Some(receivers) = item["receiverThreadIds"].as_array() {
                        for child in receivers.iter().filter_map(Value::as_str) {
                            state
                                .referenced_by
                                .entry(child.into())
                                .or_default()
                                .insert(id.into());
                            let spawning = item["tool"] == "spawnAgent";
                            let origin = spawning.then(|| agent::Origin {
                                item_id: text(item, "id"),
                                ..origin.clone()
                            });
                            state.discover(child, spawning.then(|| parent.clone()), origin);
                        }
                    }
                }
                _ => {}
            }
        }
        state.reconcile();
        state.deliver(id, events::from_notification(method, params.clone()));
        // Reads enrich effective model/name/parent metadata without resuming work.
        let ids: Vec<_> = state
            .nodes
            .keys()
            .filter(|id| {
                !state.roots.contains_key(*id)
                    && !state.requested.contains(*id)
                    && (state.root_for(id).is_some()
                        || state.referenced_by.get(*id).is_some_and(|references| {
                            references
                                .iter()
                                .any(|owner| state.root_for(owner).is_some())
                        }))
            })
            .cloned()
            .collect();
        for id in ids {
            if self.metadata.try_send(id.clone()).is_ok() {
                state.requested.insert(id);
            } else {
                state.gap("metadata queue full", Some(&id));
            }
        }
    }

    pub(in crate::providers::codex) fn event(&self, params: &Value, event: session::Event) {
        if let Some(id) = thread_id(params) {
            self.state.lock().expect("routing lock").deliver(id, event);
        }
    }

    /// The live session a thread belongs to: itself, or the root of the
    /// sub-agent tree it is part of.
    pub(in crate::providers::codex) fn root(&self, thread_id: &str) -> Option<Arc<Attachment>> {
        self.state
            .lock()
            .expect("routing lock")
            .root_for(thread_id)
            .cloned()
    }

    pub(in crate::providers::codex) fn metadata(&self, thread: types::Thread) {
        let mut state = self.state.lock().expect("routing lock");
        state.metadata(thread);
        state.reconcile();
    }

    pub(in crate::providers::codex) fn metadata_failed(&self, id: &str, message: &str) {
        self.state
            .lock()
            .expect("routing lock")
            .gap(message, Some(id));
    }
}

impl State {
    fn discover(&mut self, id: &str, parent: Option<String>, origin: Option<agent::Origin>) {
        if self.roots.contains_key(id) {
            return;
        }
        let node = self
            .nodes
            .entry(id.into())
            .or_insert_with(|| agent::Summary {
                id: id.into(),
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
            });
        if let Some(parent) = parent.filter(|parent| parent != id)
            && node.parent == agent::Parent::Unknown
        {
            node.parent = agent::Parent::Agent { id: parent };
        }
        if node.origin.is_none() {
            node.origin = origin;
        }
    }

    fn metadata(&mut self, thread: types::Thread) {
        self.loaded.insert(
            thread.id.clone(),
            !matches!(thread.status, types::ThreadStatus::NotLoaded),
        );
        let spawn = &thread.source["subAgent"]["thread_spawn"];
        let parent = thread
            .parent_thread_id
            .or_else(|| text(spawn, "parent_thread_id"));
        self.discover(&thread.id, parent.clone(), None);
        let Some(node) = self.nodes.get_mut(&thread.id) else {
            return;
        };
        if let Some(parent) = parent.filter(|parent| parent != &thread.id) {
            node.parent = agent::Parent::Agent { id: parent };
        }
        node.name = thread
            .agent_nickname
            .or_else(|| text(spawn, "agent_nickname"))
            .or(thread.name);
        node.role = thread.agent_role.or_else(|| text(spawn, "agent_role"));
        node.model = thread.model;
        if !thread.preview.is_empty() {
            node.description = Some(thread.preview);
        }
        node.created_at = (thread.created_at != 0).then_some(thread.created_at);
        node.updated_at = (thread.updated_at != 0).then_some(thread.updated_at);
    }

    fn root_for(&self, id: &str) -> Option<&Arc<Attachment>> {
        let mut current = id;
        let mut visited = HashSet::new();
        loop {
            if !visited.insert(current) {
                return None;
            }
            if let Some(root) = self.roots.get(current) {
                return (!root.is_closed()).then_some(root);
            }
            let agent::Parent::Agent { id: parent } = &self.nodes.get(current)?.parent else {
                return None;
            };
            current = parent;
        }
    }

    fn was_attached(&self, id: &str) -> bool {
        let mut current = id;
        let mut visited = HashSet::new();
        while visited.insert(current) {
            if self.known_roots.contains(current) {
                return true;
            }
            let Some(node) = self.nodes.get(current) else {
                return false;
            };
            let agent::Parent::Agent { id: parent } = &node.parent else {
                return false;
            };
            current = parent;
        }
        false
    }

    fn reconcile(&mut self) {
        // Announce in ancestry order even when the grandchild was observed first.
        let mut announced = HashSet::new();
        for id in self.nodes.keys() {
            if let Some(root) = self.root_for(id) {
                let mut chain = vec![];
                let mut current = id.as_str();
                while !self.roots.contains_key(current) && !announced.contains(current) {
                    let Some(node) = self.nodes.get(current) else {
                        break;
                    };
                    chain.push(node);
                    let agent::Parent::Agent { id: parent } = &node.parent else {
                        break;
                    };
                    current = parent;
                }
                for node in chain.into_iter().rev() {
                    root.upsert_agent(node.clone());
                    if let Some(loaded) = self.loaded.get(&node.id) {
                        root.set_loaded(&node.id, *loaded);
                    }
                    announced.insert(node.id.as_str());
                }
            }
        }
        let pending = std::mem::take(&mut self.pending);
        for (id, event) in pending {
            if let Some(root) = self.root_for(&id) {
                root.emit_for(&id, event);
            } else if !self.was_attached(&id) {
                self.pending.push_back((id, event));
            }
        }
    }

    fn deliver(&mut self, id: &str, event: session::Event) {
        if let Some(root) = self.root_for(id) {
            root.emit_for(id, event);
        } else if !self.was_attached(id) {
            if self.pending.len() == EARLY_EVENTS {
                self.pending.pop_front();
                self.gap(
                    "early traffic buffer overflow; some unassociated records were lost",
                    None,
                );
            }
            self.pending.push_back((id.into(), event));
        }
    }

    fn gap(&self, message: &str, id: Option<&str>) {
        let event = session::Event::Raw {
            name: "keel/observationGap".into(),
            payload: json!({"agentId": id, "message": message}),
        };
        if let Some(root) = id.and_then(|id| self.root_for(id)) {
            root.emit(event);
        } else {
            for root in self.roots.values() {
                root.emit(event.clone());
            }
        }
    }
}

fn text(value: &Value, key: &str) -> Option<String> {
    value[key].as_str().map(str::to_owned)
}
fn thread_id(params: &Value) -> Option<&str> {
    params["threadId"]
        .as_str()
        .or_else(|| params["thread"]["id"].as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observation as obs;

    fn setup() -> (
        Router,
        Arc<Attachment>,
        tokio::sync::broadcast::Receiver<obs::Record>,
    ) {
        let (tx, _rx) = mpsc::channel(128);
        let router = Router::new(tx);
        let info = serde_json::from_value(json!({
            "summary": {"id":"root","provider":"codex","title":null,"preview":"","cwd":"/fixture",
                "model":"gpt-6-luna","effort":"medium","status":"idle","createdAt":1,"updatedAt":1,"source":"appServer"},
            "model":"gpt-6-luna","effort":"medium","fastMode":false,"cwd":"/fixture",
            "approvalPolicy":null,"sandbox":null,"restored":false,"persisted":false
        })).unwrap();
        let root = router.observe(&info);
        let records = root.subscribe_records();
        (router, root, records)
    }

    fn drain(records: &mut tokio::sync::broadcast::Receiver<obs::Record>) -> Vec<obs::Record> {
        std::iter::from_fn(|| records.try_recv().ok()).collect()
    }

    #[test]
    fn stored_unloaded_children_are_visible_without_terminal_polling() {
        let (router, root, _) = setup();
        let fixture: Value =
            serde_json::from_str(include_str!("../fixtures/codex-observation-0.159.0.json"))
                .unwrap();
        router.metadata(serde_json::from_value(fixture["threads"][1].clone()).unwrap());
        assert!(
            root.snapshot()
                .agents
                .iter()
                .any(|agent| agent.id == "child")
        );
        assert_eq!(root.terminal_threads(), vec!["root"]);
        router.notification(
            "thread/status/changed",
            &json!({"threadId":"child","status":{"type":"active","activeFlags":[]}}),
        );
        assert!(root.terminal_threads().contains(&"child".to_string()));
    }

    #[test]
    fn captured_recursive_traffic_is_routed_once_with_native_parent_and_model() {
        let (router, root, mut records) = setup();
        let mut legacy = root.subscribe();
        let fixture: Value =
            serde_json::from_str(include_str!("../fixtures/codex-observation-0.159.0.json"))
                .unwrap();
        for notification in fixture["notifications"].as_array().unwrap() {
            router.notification(
                notification["method"].as_str().unwrap(),
                &notification["params"],
            );
        }
        for thread in fixture["threads"].as_array().unwrap() {
            router.metadata(serde_json::from_value(thread.clone()).unwrap());
        }
        let records = drain(&mut records);
        assert!(
            records
                .windows(2)
                .all(|pair| pair[0].seq + 1 == pair[1].seq)
        );
        let mut announced = HashSet::new();
        let mut replies = HashMap::<String, usize>::new();
        let mut summaries = HashMap::new();
        for record in records {
            match record.event {
                obs::Event::Agent { agent } => {
                    announced.insert(agent.id.clone());
                    summaries.insert(agent.id.clone(), agent);
                }
                obs::Event::Activity { event, .. } => {
                    if let Some(id) = &record.agent_id {
                        assert!(announced.contains(id));
                    }
                    if let session::Event::ItemEnd {
                        item: crate::turn::Item::AgentMessage { text, .. },
                        ..
                    } = event
                        && text.contains("nested-ok")
                    {
                        *replies.entry(record.agent_id.unwrap()).or_default() += 1;
                    }
                }
                _ => {}
            }
        }
        assert_eq!(
            summaries["child"].parent,
            agent::Parent::Agent { id: "root".into() }
        );
        assert_eq!(
            summaries["grandchild"].parent,
            agent::Parent::Agent { id: "child".into() }
        );
        for id in ["root", "child", "grandchild"] {
            assert_eq!(summaries[id].model.as_deref(), Some("gpt-6-luna"));
            assert_eq!(replies[id], 1);
        }
        let legacy: Vec<_> = std::iter::from_fn(|| legacy.try_recv().ok()).collect();
        assert_eq!(legacy.iter().filter(|event| matches!(event, session::Event::ItemEnd { item: crate::turn::Item::AgentMessage { text, .. }, .. } if text.contains("nested-ok"))).count(), 1);
    }

    #[test]
    fn early_grandchild_activity_waits_for_native_ancestry_without_using_paths() {
        let (router, _root, mut records) = setup();
        router.notification(
            "item/agentMessage/delta",
            &json!({"threadId":"grandchild","turnId":"u","itemId":"i","delta":"early"}),
        );
        router.notification("item/started", &json!({"threadId":"child","turnId":"u","item":{
            "type":"subAgentActivity","id":"s2","kind":"started","agentThreadId":"grandchild","agentPath":"/misleading/path"}}));
        assert!(
            !drain(&mut records)
                .iter()
                .any(|record| record.agent_id.as_deref() == Some("grandchild"))
        );
        router.notification(
            "item/started",
            &json!({"threadId":"root","turnId":"u","item":{
            "type":"subAgentActivity","id":"s1","kind":"started","agentThreadId":"child"}}),
        );
        let records = drain(&mut records);
        let child = records.iter().position(|record| matches!(&record.event, obs::Event::Agent { agent } if agent.id == "child")).unwrap();
        let grandchild = records.iter().position(|record| matches!(&record.event, obs::Event::Agent { agent } if agent.id == "grandchild")).unwrap();
        let activity = records
            .iter()
            .position(|record| record.agent_id.as_deref() == Some("grandchild"))
            .unwrap();
        assert!(child < grandchild && grandchild < activity);
        assert_eq!(records.iter().filter(|record| matches!(&record.event, obs::Event::Activity { event: session::Event::ItemDelta { delta: crate::turn::Delta::Text { text }, .. }, .. } if text == "early")).count(), 1);
    }

    #[test]
    fn buffering_is_bounded_and_reports_a_coverage_gap() {
        let (router, _root, mut records) = setup();
        for _ in 0..=EARLY_EVENTS {
            router.notification("future/event", &json!({"threadId":"unassociated"}));
        }
        assert_eq!(router.state.lock().unwrap().pending.len(), EARLY_EVENTS);
        assert!(drain(&mut records).iter().any(|record| record.agent_id.is_none() && matches!(&record.event,
            obs::Event::Activity { event: session::Event::Raw { name, .. }, .. } if name == "keel/observationGap")));
    }
    #[test]
    fn coordination_reference_queries_metadata_without_inventing_parentage() {
        let (mut router, _root, mut records) = setup();
        let (tx, mut rx) = mpsc::channel(8);
        router.metadata = tx;
        router.notification(
            "turn/started",
            &json!({"threadId":"child","turn":{"id":"u","status":"inProgress","items":[]}}),
        );
        router.notification("item/started", &json!({"threadId":"root","turnId":"root-turn","item":{
            "type":"collabAgentToolCall","id":"wait","tool":"wait","senderThreadId":"root","receiverThreadIds":["child"]}}));
        assert_eq!(rx.try_recv().unwrap(), "child");
        assert_eq!(
            router.state.lock().unwrap().nodes["child"].parent,
            agent::Parent::Unknown
        );
        assert!(
            !drain(&mut records)
                .iter()
                .any(|record| record.agent_id.as_deref() == Some("child"))
        );
        let fixture: Value =
            serde_json::from_str(include_str!("../fixtures/codex-observation-0.159.0.json"))
                .unwrap();
        router.metadata(serde_json::from_value(fixture["threads"][1].clone()).unwrap());
        let records = drain(&mut records);
        assert!(records.iter().any(|record| matches!(&record.event, obs::Event::Agent { agent } if agent.id == "child" && agent.parent == (agent::Parent::Agent { id: "root".into() }))));
        assert_eq!(
            records
                .iter()
                .filter(|record| record.agent_id.as_deref() == Some("child")
                    && matches!(
                        &record.event,
                        obs::Event::Activity {
                            event: session::Event::TurnStart { .. },
                            ..
                        }
                    ))
                .count(),
            1
        );
    }
}
