//! Real Claude process/reader tests against deterministic native stream-json.
use keel::claude::{Claude, ClaudeConfig};
use keel::{Session, agent, control::Disposition as D, observation as obs, session, task, turn};
use std::{fs, path::PathBuf, time::Duration};

struct Fixture {
    path: PathBuf,
    provider: Claude,
    session: Session,
}
impl Fixture {
    async fn open(scenario: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "keel-claude-peer-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(path.join("config")).unwrap();
        fs::write(path.join("scenario"), scenario).unwrap();
        let provider = Claude::new(ClaudeConfig {
            executable: env!("CARGO_BIN_EXE_keel-test-harness").into(),
            config_dir: Some(path.join("config")),
            request_timeout: Duration::from_millis(500),
            ..Default::default()
        });
        let session = provider
            .start_session(session::Spec {
                cwd: path.clone(),
                settings: session::Settings {
                    model: Some("claude-sonnet-4-6".into()),
                    effort: Some("medium".into()),
                    fast_mode: Some(false),
                },
            })
            .await
            .unwrap();
        Self {
            path,
            provider,
            session,
        }
    }
    async fn seed(&self) -> obs::Snapshot {
        self.session.send(turn::Input::text("seed")).await.unwrap();
        until(&self.session, |s| {
            s.tasks.iter().any(|t| t.id == "command")
                && s.agents[0].status == agent::Status::Completed
                && s.active_turns.iter().any(|t| {
                    t.agent_id == "child"
                        && t.turn
                            .items
                            .iter()
                            .any(|i| matches!(i,turn::Item::AgentMessage {text,..} if text=="half"))
                })
        })
        .await
    }
    async fn close(&self) {
        self.provider.shutdown().await;
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
async fn until(session: &Session, test: impl Fn(&obs::Snapshot) -> bool) -> obs::Snapshot {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let s = session.snapshot().await.unwrap();
            if test(&s) {
                return s;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("native fixture state did not arrive")
}
fn drain<T: Clone>(r: &mut tokio::sync::broadcast::Receiver<T>) -> Vec<T> {
    std::iter::from_fn(|| r.try_recv().ok()).collect()
}

#[tokio::test]
async fn recursive_interleaving_keeps_owned_items_logs_and_legacy_root_projection() {
    let f = Fixture::open("normal").await;
    let mut records = f.session.subscribe_records().unwrap();
    let mut legacy = f.session.subscribe();
    assert_eq!(f.session.snapshot().await.unwrap().agents[0].model, None);
    let s = f.seed().await;
    assert_eq!(s.agents[0].model.as_deref(), Some("claude-sonnet-4-6"));
    assert_eq!(s.status, session::Status::Active);
    assert_eq!(
        s.agents.iter().find(|a| a.id == "grand").unwrap().parent,
        agent::Parent::Agent { id: "child".into() }
    );
    let command = s.tasks.iter().find(|t| t.id == "command").unwrap();
    assert_eq!(command.owner_agent_id.as_deref(), Some("child"));
    assert_eq!(
        command.origin.as_ref().unwrap().turn_id.as_deref(),
        Some("spawn-child")
    );
    assert_eq!(command.progress.as_ref().unwrap().duration_ms, Some(10));
    let output = f
        .session
        .task_output("command", Default::default())
        .await
        .unwrap();
    assert_eq!(output.text, "fixture log");
    assert_eq!(output.availability, task::OutputAvailability::Partial);
    let rows = drain(&mut records);
    let old = drain(&mut legacy);
    let item = |e: &session::Event| {
        matches!(
            e,
            session::Event::ItemStart { .. }
                | session::Event::ItemDelta { .. }
                | session::Event::ItemEnd { .. }
        )
    };
    assert_eq!(
        rows.iter()
            .filter_map(|r| match &r.event {
                obs::Event::Activity { event, .. }
                    if r.agent_id.as_deref() == Some(f.session.id()) && item(event) =>
                    Some(event.clone()),
                _ => None,
            })
            .collect::<Vec<_>>(),
        old.into_iter().filter(item).collect::<Vec<_>>()
    );
    assert!(rows.iter().any(|r|r.agent_id.as_deref()==Some("grand")&&matches!(&r.event,obs::Event::Activity {event:session::Event::ItemEnd {item:turn::Item::AgentMessage {text,..},..},..} if text=="nested fixture")));
    assert!(rows.windows(2).all(|r| r[0].seq + 1 == r[1].seq));
    f.close().await;
}

#[tokio::test]
async fn acknowledged_command_stop_waits_for_matching_native_outcome() {
    let f = Fixture::open("normal").await;
    f.seed().await;
    let mut records = f.session.subscribe_records().unwrap();
    let receipt = f.session.stop_task("command").await.unwrap();
    assert_eq!(receipt.disposition, D::Accepted);
    assert_eq!(
        f.session
            .snapshot()
            .await
            .unwrap()
            .tasks
            .iter()
            .find(|t| t.id == "command")
            .unwrap()
            .status,
        task::Status::Stopping
    );
    f.session.send(turn::Input::text("confirm")).await.unwrap();
    let s = until(&f.session, |s| {
        s.tasks
            .iter()
            .any(|t| t.id == "command" && t.status == task::Status::Stopped)
    })
    .await;
    assert!(drain(&mut records).iter().any(|r|matches!(&r.event,obs::Event::Control {receipt:r} if r.id==receipt.id&&r.disposition==D::Confirmed)));
    assert_eq!(
        s.agents.iter().find(|a| a.id == "sibling").unwrap().status,
        agent::Status::Running
    );
    assert_eq!(
        f.session
            .task_output("command", Default::default())
            .await
            .unwrap()
            .availability,
        task::OutputAvailability::Complete
    );
    assert_eq!(
        f.session.stop_task("command").await.unwrap().disposition,
        D::AlreadyFinished
    );
    f.close().await;
}

#[tokio::test]
async fn terminal_before_ack_preserves_continuation_and_creation_parent() {
    for (scenario, expected) in [
        ("before-ack", D::Confirmed),
        ("natural", D::AlreadyFinished),
    ] {
        let f = Fixture::open(scenario).await;
        f.seed().await;
        let receipt = f.session.stop_agent("child").await.unwrap();
        assert_eq!(receipt.disposition, expected);
        assert_eq!(receipt.turn_id.as_deref(), Some("spawn-child"));
        let s = until(&f.session, |s| {
            s.active_turns
                .iter()
                .any(|t| t.agent_id == "child" && t.turn.id == "resume-child")
        })
        .await;
        let child = s.agents.iter().find(|a| a.id == "child").unwrap();
        assert_eq!(
            child.parent,
            agent::Parent::Agent {
                id: f.session.id().into()
            }
        );
        assert_eq!(
            child.origin.as_ref().unwrap().item_id.as_deref(),
            Some("spawn-child")
        );
        assert_eq!(
            s.tasks.iter().find(|t| t.id == "child").unwrap().status,
            task::Status::Running
        );
        assert!(s.pending_controls.is_empty());
        f.close().await;
    }
}

#[tokio::test]
async fn duplicate_agent_and_task_targets_share_one_native_stop() {
    let f = Fixture::open("pending").await;
    f.seed().await;
    let mut records = f.session.subscribe_records().unwrap();
    let other = f.session.clone();
    let stop = tokio::spawn(async move { other.stop_agent("child").await.unwrap() });
    until(&f.session, |s| !s.pending_controls.is_empty()).await;
    let repeat = f.session.stop_agent("child").await.unwrap();
    let task = f.session.stop_task("child").await.unwrap();
    assert_eq!(repeat.disposition, D::Requested);
    assert_eq!(task.disposition, D::Requested);
    assert_ne!(repeat.id, task.id);
    f.session.send(turn::Input::text("confirm")).await.unwrap();
    let receipt = stop.await.unwrap();
    assert_eq!(receipt.id, repeat.id);
    assert_eq!(receipt.disposition, D::Confirmed);
    let s = until(&f.session, |s| s.pending_controls.is_empty()).await;
    assert_eq!(
        s.agents.iter().find(|a| a.id == "child").unwrap().status,
        agent::Status::Interrupted
    );
    let counts: Vec<_> = drain(&mut records)
        .into_iter()
        .filter_map(|r| match r.event {
            obs::Event::Activity {
                event: session::Event::Raw { name, payload },
                ..
            } if name == "claude/system/fixture_stop" => payload["count"].as_u64(),
            _ => None,
        })
        .collect();
    assert_eq!(counts, vec![1]);
    f.close().await;
}

#[tokio::test]
async fn lost_ack_is_unconfirmed_until_native_evidence_and_rejection_stays_rejected() {
    for (scenario, expected) in [("lost-ack", D::Unconfirmed), ("rejected", D::Rejected)] {
        let f = Fixture::open(scenario).await;
        f.seed().await;
        let mut records = f.session.subscribe_records().unwrap();
        let receipt = f.session.stop_agent("child").await.unwrap();
        assert_eq!(receipt.disposition, expected);
        assert_eq!(
            f.session
                .snapshot()
                .await
                .unwrap()
                .agents
                .iter()
                .find(|a| a.id == "child")
                .unwrap()
                .status,
            agent::Status::Running
        );
        if scenario == "lost-ack" {
            f.session.send(turn::Input::text("confirm")).await.unwrap();
            until(&f.session, |s| s.pending_controls.is_empty()).await;
            assert!(drain(&mut records).iter().any(|r|matches!(&r.event,obs::Event::Control {receipt:r} if r.id==receipt.id&&r.disposition==D::Confirmed)));
        }
        f.close().await;
    }
}

#[tokio::test]
async fn crash_and_detach_do_not_claim_cancellation() {
    for scenario in ["exit", "detach"] {
        let f = Fixture::open(scenario).await;
        f.seed().await;
        let stream = f.session.snapshot().await.unwrap().stream_id;
        let other = f.session.clone();
        let stop = tokio::spawn(async move { other.stop_agent("child").await.unwrap() });
        if scenario == "detach" {
            until(&f.session, |s| !s.pending_controls.is_empty()).await;
            f.session.close().await.unwrap();
        }
        let receipt = stop.await.unwrap();
        assert_eq!(receipt.disposition, D::Unconfirmed);
        let s = until(&f.session, |s| s.status == session::Status::NotLoaded).await;
        assert_eq!(
            s.agents.iter().find(|a| a.id == "child").unwrap().status,
            agent::Status::Unknown
        );
        assert_eq!(
            s.tasks.iter().find(|t| t.id == "command").unwrap().status,
            task::Status::Unknown
        );
        assert!(s.active_turns.is_empty());
        let other = f
            .provider
            .start_session(session::Spec {
                cwd: f.path.clone(),
                settings: Default::default(),
            })
            .await
            .unwrap();
        assert_ne!(other.snapshot().await.unwrap().stream_id, stream);
        f.close().await;
    }
}

#[tokio::test]
async fn receiver_lag_recovers_at_atomic_watermark_then_new_records_follow() {
    let f = Fixture::open("normal").await;
    f.seed().await;
    let mut records = f.session.subscribe_records().unwrap();
    f.session.send(turn::Input::text("flood")).await.unwrap();
    until(&f.session, |s| {
        s.active_turns.iter().any(|t| {
            t.agent_id == "child"
                && t.turn
                    .items
                    .iter()
                    .any(|i| matches!(i,turn::Item::AgentMessage {text,..} if text.len()==1204))
        })
    })
    .await;
    assert!(matches!(
        records.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_))
    ));
    let s = f.session.snapshot().await.unwrap();
    let mut late = f.session.subscribe_records().unwrap();
    let rows = drain(&mut late);
    assert!(
        rows.iter()
            .all(|r| r.stream_id == s.stream_id && r.seq > s.seq)
    );
    assert!(
        rows.iter()
            .any(|r| matches!(&r.event,obs::Event::Agent {agent} if agent.id=="child"))
    );
    f.close().await;
}

#[tokio::test]
async fn native_root_interrupt_and_fail_closed_child_permission_are_attributed() {
    let f = Fixture::open("normal").await;
    f.seed().await;
    let mut records = f.session.subscribe_records().unwrap();
    let mut legacy = f.session.subscribe();
    f.session
        .send(turn::Input::text("permissions"))
        .await
        .unwrap();
    until(&f.session, |s| {
        s.agents[0].status == agent::Status::Completed
    })
    .await;
    let rows = drain(&mut records);
    assert!(rows.iter().any(|r| r.agent_id.as_deref() == Some("child")
        && matches!(
            &r.event,
            obs::Event::Activity {
                event: session::Event::RequestDeclined { .. },
                ..
            }
        )));
    assert!(
        !drain(&mut legacy)
            .iter()
            .any(|e| matches!(e, session::Event::RequestDeclined { .. }))
    );
    let denied = rows
        .iter()
        .find_map(|r| match &r.event {
            obs::Event::Activity {
                event: session::Event::Raw { name, payload },
                ..
            } if name == "claude/system/fixture_permission" => Some(payload),
            _ => None,
        })
        .unwrap();
    assert_eq!(denied["response"]["response"]["behavior"], "deny");
    let turn = f.session.send(turn::Input::text("wait")).await.unwrap();
    until(&f.session, |s| {
        s.active_turns.iter().any(|t| {
            t.agent_id == f.session.id()
                && t.turn
                    .items
                    .iter()
                    .any(|i| matches!(i,turn::Item::ToolCall(c) if c.id=="root-command"))
        })
    })
    .await;
    let receipt = f.session.stop_agent(f.session.id()).await.unwrap();
    assert_eq!(receipt.turn_id.as_deref(), Some(turn.turn_id.as_str()));
    assert_eq!(receipt.disposition, D::Confirmed);
    assert_eq!(
        f.session.snapshot().await.unwrap().agents[0].status,
        agent::Status::Interrupted
    );
    f.close().await;
}

#[tokio::test]
async fn legacy_interrupt_refuses_an_old_turn_without_stopping_the_current_one() {
    let f = Fixture::open("normal").await;
    f.seed().await;
    let old = f.session.send(turn::Input::text("wait")).await.unwrap();
    until(&f.session, |s| {
        s.active_turns
            .iter()
            .any(|t| t.agent_id == f.session.id() && t.turn.id == old.turn_id)
    })
    .await;
    f.session.interrupt(&old.turn_id).await.unwrap();
    let next = f.session.send(turn::Input::text("wait")).await.unwrap();
    assert!(matches!(
        f.session.interrupt(&old.turn_id).await,
        Err(keel::Error::Unsupported {
            operation: "interrupt_stale_turn"
        })
    ));
    let s = f.session.snapshot().await.unwrap();
    assert!(
        s.active_turns
            .iter()
            .any(|t| t.agent_id == f.session.id() && t.turn.id == next.turn_id)
    );
    f.session.interrupt(&next.turn_id).await.unwrap();
    f.close().await;
}

#[tokio::test]
async fn stored_membership_cursors_and_resume_never_infer_active_executions() {
    use serde_json::json;
    let f = Fixture::open("normal").await;
    let id = f.session.id().to_owned();
    let encoded: String = f
        .path
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let project = f.path.join("config/projects").join(encoded);
    let side = project.join(&id).join("subagents");
    fs::create_dir_all(&side).unwrap();
    let entries = [
        json!({"type":"user","uuid":"input","promptId":"p","cwd":f.path,"timestamp":"2026-10-07T00:00:00.000Z","message":{"content":"fixture input"}}),
        json!({"type":"assistant","uuid":"assistant","message":{"model":"claude-sonnet-4-6","content":[{"type":"tool_use","id":"spawn-child","name":"Agent","input":{}}]}}),
        json!({"type":"user","promptId":"p","timestamp":"2026-10-07T00:00:01.000Z","toolUseResult":{"agentId":"child","status":"completed","resolvedModel":"claude-sonnet-4-6"},"message":{"content":[{"type":"tool_result","tool_use_id":"spawn-child","content":"fixture result"}]}}),
    ];
    fs::write(
        project.join(format!("{id}.jsonl")),
        entries.iter().map(|e| format!("{e}\n")).collect::<String>(),
    )
    .unwrap();
    fs::write(
        side.join("agent-child.meta.json"),
        json!({"toolUseId":"spawn-child","agentType":"general-purpose","model":"sonnet"})
            .to_string(),
    )
    .unwrap();
    fs::write(side.join("agent-child.jsonl"),format!("{}\n{}\n",json!({"type":"user","agentId":"child","sessionId":id,"isSidechain":true,"uuid":"child-input","promptId":"p","message":{"content":"child fixture"}}),json!({"type":"assistant","agentId":"child","sessionId":id,"isSidechain":true,"uuid":"child-response","message":{"model":"claude-sonnet-4-6","content":[{"type":"text","text":"fixture reply"}]}}))).unwrap();
    let first = f
        .provider
        .list_agents(
            &id,
            agent::Query {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(first.agents[0].id, id);
    let cursor = first.next_cursor.unwrap();
    let next = f
        .provider
        .list_agents(
            &id,
            agent::Query {
                cursor: Some(cursor.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(next.agents[0].id, "child");
    assert!(
        f.provider
            .list_agents(
                &id,
                agent::Query {
                    cursor: Some(cursor),
                    parent_id: Some(id.clone()),
                    ..Default::default()
                }
            )
            .await
            .is_err()
    );
    assert!(matches!(
        f.provider
            .list_agent_turns(&id, "foreign", Default::default())
            .await,
        Err(keel::Error::AgentNotFound { .. })
    ));
    let owned = f
        .provider
        .list_agent_turns(&id, "child", Default::default())
        .await
        .unwrap();
    assert_eq!(owned.turns[0].agent_id, "child");
    assert_eq!(owned.turns[0].turn.id, "spawn-child");
    assert_eq!(owned.turns[0].turn.status, turn::Status::Completed);
    f.session.close().await.unwrap();
    let restored = f
        .provider
        .resume_session(&id, Default::default())
        .await
        .unwrap();
    let s = restored.snapshot().await.unwrap();
    assert_eq!(s.status, session::Status::Idle);
    assert!(s.active_turns.is_empty());
    assert_eq!(
        s.agents.iter().find(|a| a.id == "child").unwrap().status,
        agent::Status::Completed
    );
    assert_eq!(
        restored.stop_agent("child").await.unwrap().disposition,
        D::AlreadyFinished
    );
    restored.send(turn::Input::text("resume")).await.unwrap();
    until(&restored, |s| {
        s.active_turns
            .iter()
            .any(|t| t.agent_id == "child" && t.turn.id == "resume-child")
    })
    .await;
    f.close().await;
}
