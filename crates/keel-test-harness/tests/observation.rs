//! Exercises the real child process, JSON-RPC reader, routing and reducers.
use std::time::Duration;

use keel::codex::{Codex, CodexConfig};
use keel::control::Disposition as D;
use keel::{Error, Session, agent, observation, session, task, turn};

async fn open(scenario: &str) -> (Codex, Session) {
    let provider = Codex::new(CodexConfig {
        executable: env!("CARGO_BIN_EXE_keel-test-harness").into(),
        request_timeout: Duration::from_millis(500),
        ..Default::default()
    });
    let session = provider
        .resume_session(scenario, session::Settings::default())
        .await
        .unwrap();
    (provider, session)
}
async fn until(
    session: &Session,
    condition: impl Fn(&observation::Snapshot) -> bool,
) -> observation::Snapshot {
    tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            let snapshot = session.snapshot().await.unwrap();
            if condition(&snapshot) {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("state did not arrive")
}
#[tokio::test]
async fn exact_turn_dedup_and_natural_completion_before_ack_preserve_continuation() {
    let (provider, session) = open("race").await;
    let child = "race/child";
    let stop_session = session.clone();
    let stop = tokio::spawn(async move { stop_session.stop_agent(child).await });
    let snapshot = until(&session, |s| !s.pending_controls.is_empty()).await;
    let original = &snapshot.pending_controls[0];
    let repeated = session.stop_agent(child).await.unwrap();
    assert_eq!(&repeated, original);
    assert_eq!(repeated.turn_id.as_deref(), Some("old-turn"));
    let mut records = session.subscribe_records().unwrap();
    session.send(turn::Input::text("advance")).await.unwrap();
    assert_eq!(stop.await.unwrap().unwrap().disposition, D::AlreadyFinished);
    let snapshot = until(&session, |s| {
        s.active_turns
            .iter()
            .any(|t| t.agent_id == child && t.turn.id == "new-turn")
    })
    .await;
    assert!(snapshot.pending_controls.is_empty());
    assert!(
        snapshot
            .active_turns
            .iter()
            .any(|t| t.agent_id == "race/sibling" && t.turn.status == turn::Status::InProgress)
    );
    let events: Vec<_> = std::iter::from_fn(|| records.try_recv().ok()).collect();
    let native: Vec<_> = events
        .iter()
        .filter_map(|r| match &r.event {
            observation::Event::Activity {
                event:
                    session::Event::Raw {
                        name,
                        payload: data,
                    },
                ..
            } if name == "fake/request" && data["method"] == "turn/interrupt" => Some(data),
            _ => None,
        })
        .collect();
    assert!(native.is_empty(), "dedup issued a second interrupt");
    provider.shutdown().await;
}

#[tokio::test]
async fn interrupted_event_before_ack_confirms_only_captured_turn() {
    let (provider, session) = open("confirm").await;
    let other = session.clone();
    let stop = tokio::spawn(async move { other.stop_agent("confirm/child").await });
    until(&session, |s| !s.pending_controls.is_empty()).await;
    session.send(turn::Input::text("confirm")).await.unwrap();
    assert_eq!(stop.await.unwrap().unwrap().disposition, D::Confirmed);
    let snapshot = session.snapshot().await.unwrap();
    assert!(
        snapshot
            .active_turns
            .iter()
            .any(|t| t.agent_id == "confirm/child" && t.turn.id == "new-turn")
    );
    provider.shutdown().await;
}

#[tokio::test]
async fn lost_ack_is_unconfirmed_and_native_rejection_is_rejected() {
    for scenario in ["lost-ack", "rejected"] {
        let (provider, session) = open(scenario).await;
        let mut records = session.subscribe_records().unwrap();
        let error = session
            .stop_agent(&format!("{scenario}/child"))
            .await
            .unwrap_err();
        let expected = if scenario == "lost-ack" {
            assert!(matches!(error, Error::Timeout(_)));
            D::Unconfirmed
        } else {
            assert!(matches!(error, Error::Rpc { .. }));
            D::Rejected
        };
        let receipts: Vec<_> = std::iter::from_fn(|| records.try_recv().ok())
            .filter_map(|r| match r.event {
                observation::Event::Control { receipt } => Some(receipt),
                _ => None,
            })
            .collect();
        assert_eq!(receipts.last().unwrap().disposition, expected);
        let snapshot = session.snapshot().await.unwrap();
        assert!(
            snapshot
                .active_turns
                .iter()
                .any(|t| t.turn.id == "old-turn" && t.turn.status == turn::Status::InProgress)
        );
        assert_eq!(
            snapshot.pending_controls.len(),
            usize::from(expected == D::Unconfirmed)
        );
        provider.shutdown().await;
    }
}

#[tokio::test]
async fn process_exit_during_stop_marks_unknown_and_new_attachment_has_new_stream() {
    let (provider, session) = open("exit").await;
    let old = session.snapshot().await.unwrap().stream_id;
    let mut records = session.subscribe_records().unwrap();
    assert!(session.stop_agent("exit/child").await.is_err());
    let snapshot = until(&session, |s| s.status == session::Status::NotLoaded).await;
    assert_eq!(snapshot.pending_controls[0].disposition, D::Unconfirmed);
    assert!(
        !snapshot
            .agents
            .iter()
            .any(|a| a.status == agent::Status::Failed)
    );
    let closed = std::iter::from_fn(|| records.try_recv().ok())
        .filter(|r| {
            matches!(
                r.event,
                observation::Event::Activity {
                    event: session::Event::Closed { .. },
                    ..
                }
            )
        })
        .count();
    assert_eq!(closed, 1);
    let resumed = provider
        .resume_session("exit", session::Settings::default())
        .await
        .unwrap();
    assert_ne!(resumed.snapshot().await.unwrap().stream_id, old);
    assert!(session.stop_agent("exit/child").await.is_err());
    provider.shutdown().await;
}

#[tokio::test]
async fn terminal_event_before_ack_and_late_lists_do_not_resurrect_stopped_task() {
    let (provider, session) = open("task-stop").await;
    let snapshot = until(&session, |s| !s.tasks.is_empty()).await;
    let task = &snapshot.tasks[0];
    assert_eq!(
        snapshot.active_turns[0].turn.status,
        turn::Status::Completed
    );
    assert_eq!(
        task.origin.as_ref().unwrap().turn_id.as_deref(),
        Some("owner-turn")
    );
    let stopped = session.stop_task(&task.id).await.unwrap();
    assert_eq!(stopped.disposition, D::Confirmed);
    assert_eq!(
        session.snapshot().await.unwrap().tasks[0].status,
        task::Status::Stopped
    );
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(
        session.snapshot().await.unwrap().tasks[0].status,
        task::Status::Stopped
    );
    assert_eq!(
        session.stop_task(&task.id).await.unwrap().disposition,
        D::AlreadyFinished
    );
    provider.shutdown().await;
}

#[tokio::test]
async fn changed_native_process_and_lost_terminal_ack_never_claim_stop() {
    for scenario in ["task-reused", "task-lost-ack"] {
        let (provider, session) = open(scenario).await;
        let snapshot = until(&session, |s| !s.tasks.is_empty()).await;
        let id = &snapshot.tasks[0].id;
        let mut records = session.subscribe_records().unwrap();
        if scenario == "task-reused" {
            assert_eq!(
                session.stop_task(id).await.unwrap().disposition,
                D::Unconfirmed
            );
        } else {
            assert!(matches!(
                session.stop_task(id).await,
                Err(Error::Timeout(_))
            ));
        }
        let snapshot = session.snapshot().await.unwrap();
        assert_eq!(snapshot.tasks[0].status, task::Status::Unknown);
        assert_eq!(
            snapshot.pending_controls.last().unwrap().disposition,
            D::Unconfirmed
        );
        let native: Vec<_> = std::iter::from_fn(|| records.try_recv().ok())
            .filter_map(|r| match r.event {
                observation::Event::Activity {
                    event:
                        session::Event::Raw {
                            name,
                            payload: data,
                        },
                    ..
                } if name == "fake/request"
                    && data["method"] == "thread/backgroundTerminals/terminate" =>
                {
                    Some(data)
                }
                _ => None,
            })
            .collect();
        assert_eq!(native.len(), usize::from(scenario == "task-lost-ack"));
        provider.shutdown().await;
    }
}

#[tokio::test]
async fn stored_pages_and_interleaved_descendants_keep_parentage_and_item_ownership() {
    let (provider, session) = open("history").await;
    let snapshot = session.snapshot().await.unwrap();
    assert_eq!(snapshot.agents.len(), 4);
    assert_eq!(
        snapshot
            .agents
            .iter()
            .find(|a| a.id == "history/grandchild")
            .unwrap()
            .parent,
        agent::Parent::Agent {
            id: "history/child".into()
        }
    );
    let mut records = session.subscribe_records().unwrap();
    let first = provider
        .list_agent_turns(
            "history",
            "history/child",
            turn::Query {
                limit: Some(1),
                cursor: None,
            },
        )
        .await
        .unwrap();
    let second = provider
        .list_agent_turns(
            "history",
            "history/child",
            turn::Query {
                limit: Some(1),
                cursor: first.next_cursor.clone(),
            },
        )
        .await
        .unwrap();
    assert_eq!(first.turns[0].agent_id, "history/child");
    assert_eq!(second.turns[0].turn.id, "stored2");
    assert!(matches!(
        provider
            .list_agent_turns(
                "history",
                "history/sibling",
                turn::Query {
                    limit: None,
                    cursor: first.next_cursor
                }
            )
            .await,
        Err(Error::InvalidSetting { .. })
    ));
    assert!(matches!(
        provider
            .list_agent_turns("history", "foreign", turn::Query::default())
            .await,
        Err(Error::AgentNotFound { .. })
    ));
    assert!(std::iter::from_fn(||records.try_recv().ok()).all(|r|!matches!(&r.event,observation::Event::Activity{event:session::Event::Raw{name,payload:data},..} if name=="fake/request" && ["thread/resume","turn/start"].contains(&data["method"].as_str().unwrap_or("")))));
    provider.shutdown().await;
    let (provider, session) = open("interleaved").await;
    let snapshot = session.snapshot().await.unwrap();
    for (owner, text) in [
        ("interleaved/child", "child"),
        ("interleaved/grandchild", "grand"),
    ] {
        let owned = snapshot
            .active_turns
            .iter()
            .find(|t| t.agent_id == owner)
            .unwrap();
        assert!(owned.turn.items.iter().any(|item|matches!(item,turn::Item::AgentMessage{id,text:actual} if id=="shared" && actual==text)));
    }
    provider.shutdown().await;
}

#[tokio::test]
async fn repeated_terminal_cursor_reports_gap_without_partial_reconciliation() {
    let (provider, session) = open("terminal-loop").await;
    let mut records = session.subscribe_records().unwrap();
    tokio::time::timeout(Duration::from_secs(3),async {
        loop {
            let record=records.recv().await.unwrap();
            if matches!(&record.event,observation::Event::Activity{event:session::Event::Raw{name,..},..} if name=="keel/taskMonitoringGap") { break; }
        }
    }).await.unwrap();
    assert!(session.snapshot().await.unwrap().tasks.is_empty());
    provider.shutdown().await;
}

#[tokio::test]
async fn receiver_lag_recovers_from_atomic_owned_snapshot_then_applies_newer_records() {
    let (provider, session) = open("history").await;
    let mut records = session.subscribe_records().unwrap();
    session.send(turn::Input::text("burst")).await.unwrap();
    assert!(matches!(
        records.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_))
    ));
    let snapshot = session.snapshot().await.unwrap();
    let active = snapshot
        .active_turns
        .iter()
        .find(|t| t.agent_id == "history" && t.turn.id == "burst")
        .unwrap();
    assert!(
        active
            .turn
            .items
            .iter()
            .any(|i| matches!(i,turn::Item::AgentMessage{text,..} if text.len()==2200))
    );
    let watermark = snapshot.seq;
    while let Ok(record) = records.try_recv() {
        assert!(record.seq <= watermark);
    }
    session.send(turn::Input::text("advance")).await.unwrap();
    let records: Vec<_> = std::iter::from_fn(|| records.try_recv().ok()).collect();
    assert_eq!(records[0].seq, watermark + 1);
    assert!(records.iter().all(|r| r.stream_id == snapshot.stream_id));
    assert!(records.windows(2).all(|p| p[1].seq == p[0].seq + 1));
    provider.shutdown().await;
}

#[tokio::test]
async fn detach_during_pending_stop_is_not_confirmed_by_late_ack_or_new_attachment() {
    let (provider, session) = open("detach").await;
    let other = session.clone();
    let stop = tokio::spawn(async move { other.stop_agent("detach/child").await });
    let snapshot = until(&session, |s| !s.pending_controls.is_empty()).await;
    session.close().await.unwrap();
    let resumed = provider
        .resume_session("detach", session::Settings::default())
        .await
        .unwrap();
    assert_ne!(
        snapshot.stream_id,
        resumed.snapshot().await.unwrap().stream_id
    );
    // Closing an old handle must not unregister the newer attachment.
    session.close().await.unwrap();
    resumed.send(turn::Input::text("advance")).await.unwrap();
    assert_eq!(stop.await.unwrap().unwrap().disposition, D::Unconfirmed);
    assert_eq!(
        session.snapshot().await.unwrap().status,
        session::Status::NotLoaded
    );
    assert!(
        resumed
            .snapshot()
            .await
            .unwrap()
            .active_turns
            .iter()
            .any(|t| t.turn.id == "new-turn")
    );
    provider.shutdown().await;
}

#[tokio::test]
async fn missing_parent_metadata_reports_gap_without_inventing_ownership() {
    let (provider, session) = open("history").await;
    let mut records = session.subscribe_records().unwrap();
    session.send(turn::Input::text("missing")).await.unwrap();
    tokio::time::timeout(Duration::from_secs(3),async {
        loop {let r=records.recv().await.unwrap();
            assert_ne!(r.agent_id.as_deref(),Some("missing"));
            if matches!(&r.event,observation::Event::Activity{event:session::Event::Raw{name,..},..} if name=="keel/observationGap") {break;}
        }
    }).await.unwrap();
    let snapshot = session.snapshot().await.unwrap();
    assert!(!snapshot.agents.iter().any(|a| a.id == "missing"));
    assert!(matches!(
        session.stop_agent("missing").await,
        Err(Error::AgentNotFound { .. })
    ));
    provider.shutdown().await;
}

#[tokio::test]
async fn uncertain_polled_task_stop_publishes_same_aggregate_status_as_snapshot() {
    let (provider, session) = open("task-polled-lost").await;
    let snapshot = until(&session, |s| !s.tasks.is_empty()).await;
    assert_eq!(snapshot.status, session::Status::Active);
    let mut records = session.subscribe_records().unwrap();
    assert!(session.stop_task(&snapshot.tasks[0].id).await.is_err());
    let snapshot = session.snapshot().await.unwrap();
    assert_eq!(snapshot.status, session::Status::Idle);
    let statuses: Vec<_> = std::iter::from_fn(|| records.try_recv().ok())
        .filter_map(|r| match r.event {
            observation::Event::Activity {
                event: session::Event::Status { status },
                ..
            } => Some(status),
            _ => None,
        })
        .collect();
    assert_eq!(statuses.last(), Some(&snapshot.status));
    provider.shutdown().await;
}

#[tokio::test]
async fn accepted_stop_waits_for_matching_interrupted_event() {
    let (provider, session) = open("accepted").await;
    let receipt = session.stop_agent("accepted/child").await.unwrap();
    assert_eq!(receipt.disposition, D::Accepted);
    assert_eq!(
        session.snapshot().await.unwrap().pending_controls[0].id,
        receipt.id
    );
    let mut records = session.subscribe_records().unwrap();
    session.send(turn::Input::text("confirm")).await.unwrap();
    assert!(
        session
            .snapshot()
            .await
            .unwrap()
            .pending_controls
            .is_empty()
    );
    assert!(std::iter::from_fn(||records.try_recv().ok()).any(|r|matches!(r.event,
        observation::Event::Control{receipt:updated} if updated.id==receipt.id && updated.disposition==D::Confirmed)));
    provider.shutdown().await;
}
