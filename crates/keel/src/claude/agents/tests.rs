use super::*;
use serde_json::json;
use std::path::PathBuf;

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("keel-claude-stored-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(path.join("root/subagents")).unwrap();
        Self(path)
    }
    fn entries(&self, path: &str, entries: &[Value]) {
        fs::write(
            self.0.join(path),
            entries.iter().map(|e| format!("{e}\n")).collect::<String>(),
        )
        .unwrap();
    }
    fn child(&self, id: &str, meta: Value, entries: &[Value]) {
        fs::write(
            self.0.join(format!("root/subagents/agent-{id}.meta.json")),
            meta.to_string(),
        )
        .unwrap();
        self.entries(&format!("root/subagents/agent-{id}.jsonl"), entries);
    }
    fn load(&self, entries: Vec<Value>) -> Stored {
        Stored::read(
            &store::SessionFile {
                id: "root".into(),
                path: self.0.join("root.jsonl"),
                modified: 10,
            },
            entries,
        )
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn user(id: &str, agent: Option<&str>, prompt: &str, text: &str) -> Value {
    json!({"type":"user","sessionId":"root","agentId":agent,"isSidechain":agent.is_some(),"uuid":id,"promptId":prompt,"timestamp":"2026-10-07T00:00:00.000Z","message":{"content":text}})
}
fn spawn(id: &str, name: &str) -> Value {
    json!({"type":"assistant","uuid":format!("a-{id}"),"message":{"content":[{"type":"tool_use","id":id,"name":name,"input":{}}]}})
}
fn result(tool: &str, id: &str, prompt: &str, resumed: bool) -> Value {
    json!({"type":"user","promptId":prompt,"timestamp":"2026-10-07T00:00:01.000Z","message":{"content":[{"type":"tool_result","tool_use_id":tool,"content":"native result"}]},"toolUseResult":if resumed {json!({"resumedAgentId":id,"success":true})} else {json!({"agentId":id,"status":"completed","resolvedModel":"claude-sonnet-4-6"})}})
}

#[test]
fn native_sidechains_keep_membership_nested_ownership_and_continuation_ids() {
    let f = Fixture::new();
    let mut child = vec![
        user("input-1", Some("child"), "p1", "first"),
        spawn("grand-spawn", "Agent"),
        result("grand-spawn", "grand", "p1", false),
        user("input-2", Some("child"), "p2", "second"),
    ];
    child.last_mut().unwrap()["isMeta"] = json!(true);
    child.last_mut().unwrap()["origin"] = json!({"kind":"coordinator"});
    for e in &mut child {
        e["sessionId"] = json!("root");
        e["agentId"] = json!("child");
    }
    f.child(
        "child",
        json!({"toolUseId":"spawn","model":"sonnet"}),
        &child,
    );
    f.child(
        "grand",
        json!({"toolUseId":"grand-spawn","parentAgentId":"child"}),
        &[user("g", Some("grand"), "p1", "nested")],
    );
    f.child("unrelated",json!({}),&[json!({"type":"user","sessionId":"other","agentId":"unrelated","message":{"content":"secret"}})]);
    let mut queue = json!({"type":"queue-operation","operation":"enqueue","timestamp":"2026-10-07T00:00:02.000Z","content":"<task-notification><task-id>child</task-id><tool-use-id>resume</tool-use-id><status>stopped</status></task-notification>"});
    queue["sessionId"] = json!("root");
    let stored = f.load(vec![
        user("u1", None, "p1", "start"),
        spawn("spawn", "Agent"),
        result("spawn", "child", "p1", false),
        user("u2", None, "p2", "continue"),
        spawn("resume", "SendMessage"),
        result("resume", "child", "p2", true),
        queue,
    ]);
    assert!(stored.member("unrelated").is_err());
    assert_eq!(stored.agents.len(), 3);
    let c = stored.agents.iter().find(|a| a.id == "child").unwrap();
    assert_eq!(c.model.as_deref(), Some("claude-sonnet-4-6"));
    assert_eq!(c.status, agent::Status::Interrupted);
    assert_eq!(c.origin.as_ref().unwrap().item_id.as_deref(), Some("spawn"));
    let g = stored.agents.iter().find(|a| a.id == "grand").unwrap();
    assert_eq!(g.parent, agent::Parent::Agent { id: "child".into() });
    assert_eq!(g.origin.as_ref().unwrap().turn_id.as_deref(), Some("spawn"));
    assert_eq!(
        stored.turns["child"]
            .iter()
            .map(|t| (&*t.id, t.status))
            .collect::<Vec<_>>(),
        vec![
            ("spawn", turn::Status::Completed),
            ("resume", turn::Status::Interrupted)
        ]
    );
    assert!(stored.descendant("grand", "root"));
    assert!(!stored.descendant("root", "grand"));
    let tasks = super::super::stored_tasks::read(&stored).unwrap();
    assert_eq!(tasks["child"].summary.status, crate::task::Status::Stopped);
    assert_eq!(tasks["child"].tool.as_deref(), Some("resume"));
}

#[test]
fn expired_transcripts_keep_discovery_and_unknown_models_are_absent() {
    let f = Fixture::new();
    f.child(
        "pending",
        json!({"model":"sonnet","toolUseId":"unreported"}),
        &[user("u", Some("pending"), "p", "wait")],
    );
    let stored = f.load(vec![
        user("u", None, "p", "start"),
        spawn("spawn", "Agent"),
        result("spawn", "expired", "p", false),
    ]);
    assert!(stored.member("expired").is_ok());
    assert!(stored.turns["expired"].is_empty());
    let pending = stored.agents.iter().find(|a| a.id == "pending").unwrap();
    assert_eq!(pending.model, None);
    assert_eq!(pending.status, agent::Status::Unknown);
    assert_eq!(pending.parent, agent::Parent::Unknown);
}

#[test]
fn observation_cursors_reject_other_roots_filters_and_operations() {
    let scope = || Scope {
        root: "root".into(),
        kind: "agents".into(),
        owner: None,
        parent: None,
        ancestor: Some("child".into()),
    };
    let (values, cursor) = paginate(vec![1, 2, 3], scope(), Some(1), None).unwrap();
    assert_eq!(values, vec![1]);
    let (values, _) = paginate(vec![1, 2, 3], scope(), Some(2), cursor.as_deref()).unwrap();
    assert_eq!(values, vec![2, 3]);
    let mut changed = scope();
    changed.root = "other".into();
    assert!(paginate(vec![1, 2, 3], changed, None, cursor.as_deref()).is_err());
    let mut changed = scope();
    changed.kind = "turns".into();
    assert!(paginate(vec![1, 2, 3], changed, None, cursor.as_deref()).is_err());
    assert!(paginate(vec![1], scope(), Some(0), None).is_err());
    assert!(paginate(vec![1], scope(), None, Some("1")).is_err());
    assert!(!store::safe_id("../../secret"));
    assert!(!store::safe_id(".."));
    assert!(store::safe_id("agent-123"));
}

#[cfg(unix)]
#[test]
fn sidechain_symlinks_cannot_read_files_outside_this_session() {
    let f = Fixture::new();
    f.entries(
        "outside.jsonl",
        &[user("u", Some("escape"), "p", "private")],
    );
    std::os::unix::fs::symlink(
        f.0.join("outside.jsonl"),
        f.0.join("root/subagents/agent-escape.jsonl"),
    )
    .unwrap();
    assert!(f.load(vec![]).member("escape").is_err());
}

#[test]
fn creation_order_preserves_native_milliseconds_and_expired_outcomes() {
    let f = Fixture::new();
    for (id, time) in [
        ("z", "2026-10-07T00:00:00.001Z"),
        ("a", "2026-10-07T00:00:00.002Z"),
    ] {
        let mut e = user(id, Some(id), "p", "request");
        e["timestamp"] = json!(time);
        f.child(id, json!({}), &[e]);
    }
    let mut r = result("spawn", "expired", "p", false);
    r["toolUseResult"]["status"] = json!("async_launched");
    let stored=f.load(vec![user("u",None,"p","start"),spawn("spawn","Agent"),r,json!({"type":"queue-operation","operation":"enqueue","timestamp":"2026-10-07T00:00:02.000Z","content":"<task-notification><task-id>expired</task-id><tool-use-id>spawn</tool-use-id><status>completed</status></task-notification>"})]);
    assert_eq!(stored.agents[1].id, "z");
    assert_eq!(stored.agents[2].id, "a");
    assert_eq!(
        stored
            .agents
            .iter()
            .find(|a| a.id == "expired")
            .unwrap()
            .status,
        agent::Status::Completed
    );
}
