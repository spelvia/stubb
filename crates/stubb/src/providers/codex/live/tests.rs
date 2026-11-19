use super::*;
use crate::turn;
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

fn make_attachment() -> Attachment {
    Attachment::new(&info())
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
fn snapshot_watermark_is_atomic_and_keeps_owned_turns_and_pending_stops() {
    let attachment = make_attachment();
    let mut receiver = attachment.subscribe_records();
    attachment.emit(session::Event::TurnStart {
        turn_id: "turn".into(),
    });
    attachment.emit(session::Event::ItemDelta {
        turn_id: "turn".into(),
        item_id: "message".into(),
        delta: turn::Delta::Text {
            text: "before".into(),
        },
    });
    let (receipt, _) = attachment
        .begin_stop(crate::control::Target::Agent { id: "root".into() })
        .unwrap();
    let snapshot = attachment.snapshot();
    assert_eq!(snapshot.pending_controls[0].id, receipt.id);
    assert!(
        matches!(&snapshot.active_turns[0].turn.items[0], turn::Item::AgentMessage { text, .. } if text == "before")
    );
    assert_eq!(snapshot.seq, drain(&mut receiver).last().unwrap().seq);
    attachment.emit(session::Event::ItemDelta {
        turn_id: "turn".into(),
        item_id: "message".into(),
        delta: turn::Delta::Text {
            text: "after".into(),
        },
    });
    let records = drain(&mut receiver);
    assert!(
        records
            .iter()
            .all(|record| record.seq > snapshot.seq && record.stream_id == snapshot.stream_id)
    );
    attachment.emit(session::Event::Closed {
        reason: session::CloseReason::Client,
    });
    assert_eq!(attachment.snapshot().status, session::Status::NotLoaded);
    assert!(attachment.snapshot().active_turns.is_empty());
}

#[test]
fn controls_bind_generations_and_preserve_completion_before_ack() {
    use crate::control::{Disposition as D, Target};
    let attachment = make_attachment();
    attachment.emit(session::Event::TurnStart {
        turn_id: "old".into(),
    });
    let (receipt, request) = attachment
        .begin_stop(Target::Agent { id: "root".into() })
        .unwrap();
    assert!(request.is_some());
    assert_eq!(
        attachment
            .begin_stop(Target::Agent { id: "root".into() })
            .unwrap()
            .0
            .id,
        receipt.id
    );
    attachment.emit(end("old"));
    attachment.emit(session::Event::TurnStart {
        turn_id: "new".into(),
    });
    assert_eq!(
        attachment.stop_result(&receipt.id, Ok(true)).disposition,
        D::AlreadyFinished
    );
    assert_eq!(
        attachment.state.lock().unwrap().root.agent.status,
        agent::Status::Running
    );
    let (new, _) = attachment
        .begin_stop(Target::Agent { id: "root".into() })
        .unwrap();
    let mut interrupted = end("new");
    if let session::Event::TurnEnd { turn } = &mut interrupted {
        turn.status = turn::Status::Interrupted;
    }
    attachment.emit(interrupted);
    assert_eq!(
        attachment.stop_result(&new.id, Ok(true)).disposition,
        D::Confirmed
    );
    assert!(matches!(
        attachment.begin_stop(Target::Task {
            id: "foreign".into()
        }),
        Err(crate::Error::TaskNotFound { .. })
    ));
}

#[test]
fn stop_terminal_waits_for_native_ack_and_keeps_stopped_on_late_list() {
    use crate::control::{Disposition as D, Target};
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../fixtures/codex-observation-0.159.0.json")).unwrap();
    let attachment = make_attachment();
    let page: super::tasks::Page = serde_json::from_value(fixture["terminalList"].clone()).unwrap();
    attachment.terminals("root", page.data);
    let id = attachment.state.lock().unwrap().tasks.summaries()[0]
        .id
        .clone();
    let (receipt, request) = attachment
        .begin_stop(Target::Task { id: id.clone() })
        .unwrap();
    assert!(request.is_some());
    let notification = fixture["terminalNotifications"]
        .as_array()
        .unwrap()
        .last()
        .unwrap();
    attachment.emit(super::super::events::from_notification(
        notification["method"].as_str().unwrap(),
        notification["params"].clone(),
    ));
    assert_eq!(
        attachment.state.lock().unwrap().tasks.summaries()[0].status,
        crate::task::Status::Failed
    );
    assert_eq!(
        attachment.stop_result(&receipt.id, Ok(true)).disposition,
        D::Confirmed
    );
    let page: super::tasks::Page = serde_json::from_value(fixture["terminalList"].clone()).unwrap();
    attachment.terminals("root", page.data);
    assert_eq!(
        attachment.state.lock().unwrap().tasks.summaries()[0].status,
        crate::task::Status::Stopped
    );
    assert!(
        attachment
            .begin_stop(Target::Task { id })
            .unwrap()
            .1
            .is_none()
    );
}

#[test]
fn captured_terminal_retains_owner_after_turn_and_bootstraps_until_native_exit() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("../fixtures/codex-observation-0.159.0.json")).unwrap();
    let attachment = make_attachment();
    let mut receiver = attachment.subscribe_records();
    let page: super::tasks::Page = serde_json::from_value(fixture["terminalList"].clone()).unwrap();
    let notifications = fixture["terminalNotifications"].as_array().unwrap();
    for notification in notifications.iter().take(2) {
        attachment.emit(super::super::events::from_notification(
            notification["method"].as_str().unwrap(),
            notification["params"].clone(),
        ));
    }
    attachment.terminals("root", page.data);
    let task = attachment.state.lock().unwrap().tasks.summaries()[0].clone();
    assert_eq!(task.owner_agent_id.as_deref(), Some("root"));
    assert_eq!(task.status, crate::task::Status::Running);
    assert_eq!(
        attachment.state.lock().unwrap().aggregate_status(),
        session::Status::Active
    );
    let mut late = attachment.subscribe_records();
    assert!(drain(&mut late).iter().any(|record| matches!(&record.event, obs::Event::Task { task: replacement } if replacement.id == task.id)));
    attachment.terminal_failure("root", "test transport loss");
    assert_eq!(
        attachment.state.lock().unwrap().tasks.summaries()[0].status,
        crate::task::Status::Unknown
    );
    for notification in notifications.iter().skip(2) {
        attachment.emit(super::super::events::from_notification(
            notification["method"].as_str().unwrap(),
            notification["params"].clone(),
        ));
    }
    let summary = attachment.state.lock().unwrap().tasks.summaries()[0].clone();
    assert_eq!(summary.id, task.id);
    assert_eq!(summary.status, crate::task::Status::Failed);
    let records = drain(&mut receiver);
    assert!(
        records
            .windows(2)
            .all(|pair| pair[1].seq == pair[0].seq + 1)
    );
    assert!(records.iter().any(|record| matches!(&record.event, obs::Event::Activity { event: session::Event::Raw { name, .. }, .. } if name == "keel/taskMonitoringGap")));
}

#[test]
fn records_announce_root_attribute_items_and_preserve_legacy_projection() {
    let attachment = make_attachment();
    let mut records = attachment.subscribe_records();
    let mut legacy = attachment.subscribe();
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
        attachment.emit(event.clone());
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
    let state = attachment.state.lock().unwrap();
    assert_eq!(state.seq, records.last().unwrap().seq);
    assert!(state.root.turns.is_empty());
    assert_eq!(state.root.agent.status, agent::Status::Completed);
}

#[test]
fn reducer_materializes_deltas_and_final_items_replace_them() {
    let attachment = make_attachment();
    let mut records = attachment.subscribe_records();
    attachment.emit(session::Event::TurnStart {
        turn_id: "u".into(),
    });
    for text in ["hel", "lo"] {
        attachment.emit(session::Event::ItemDelta {
            turn_id: "u".into(),
            item_id: "i".into(),
            delta: turn::Delta::Text { text: text.into() },
        });
    }
    let stream_id = attachment.state.lock().unwrap().stream_id.clone();
    let mut refreshed = info();
    refreshed.model = "reported-model".into();
    attachment.refresh_metadata(&refreshed);
    assert!(
        matches!(drain(&mut records).last().map(|record| &record.event),
            Some(obs::Event::Agent { agent }) if agent.model.as_deref() == Some("reported-model") && agent.status == agent::Status::Running)
    );
    {
        let state = attachment.state.lock().unwrap();
        assert_eq!(state.stream_id, stream_id);
        assert!(
            matches!(&state.root.turns["u"].turn.items[0], turn::Item::AgentMessage { text, .. } if text == "hello")
        );
    }
    attachment.emit(session::Event::ItemEnd {
        turn_id: "u".into(),
        item: turn::Item::AgentMessage {
            id: "i".into(),
            text: "final".into(),
        },
    });
    let state = attachment.state.lock().unwrap();
    assert_eq!(state.root.turns["u"].turn.items.len(), 1);
    assert!(
        matches!(&state.root.turns["u"].turn.items[0], turn::Item::AgentMessage { text, .. } if text == "final")
    );
}

#[test]
fn unfinished_command_survives_turn_end_and_native_idle_until_item_end() {
    let attachment = make_attachment();
    let mut records = attachment.subscribe_records();
    let mut legacy = attachment.subscribe();
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
    attachment.emit(session::Event::ItemStart {
        turn_id: "u".into(),
        item: item.clone(),
    });
    attachment.emit(end("u"));
    attachment.emit(session::Event::Status {
        status: session::Status::Idle,
    });
    attachment.emit(session::Event::ItemDelta {
        turn_id: "u".into(),
        item_id: "command".into(),
        delta: turn::Delta::Output {
            text: "done".into(),
        },
    });
    {
        let state = attachment.state.lock().unwrap();
        assert_eq!(state.root.status, session::Status::Active);
        assert_eq!(state.root.turns["u"].turn.status, turn::Status::Completed);
        assert!(
            matches!(&state.root.turns["u"].turn.items[0], turn::Item::ToolCall(turn::ToolCall {
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
    attachment.emit(session::Event::ItemEnd {
        turn_id: "u".into(),
        item,
    });
    let state = attachment.state.lock().unwrap();
    assert!(state.root.turns.is_empty());
    assert_eq!(state.root.status, session::Status::Idle);
}

#[test]
fn subscribers_share_sequence_and_bootstrap_without_resetting_stream() {
    let attachment = make_attachment();
    let mut first = attachment.subscribe_records();
    attachment.emit(session::Event::TurnStart {
        turn_id: "u".into(),
    });
    let mut second = attachment.subscribe_records();
    attachment.emit(session::Event::Raw {
        name: "future/event".into(),
        payload: json!({"threadId":"root"}),
    });
    let first = drain(&mut first);
    let second = drain(&mut second);
    assert_eq!(&first[first.len() - second.len()..], second);
    assert!(
        first
            .windows(2)
            .all(|pair| pair[0].seq + 1 == pair[1].seq && pair[0].stream_id == pair[1].stream_id)
    );
    assert!(matches!(second[0].event, obs::Event::Agent { .. }));
    assert_ne!(
        first[0].stream_id,
        make_attachment().state.lock().unwrap().stream_id
    );
}

#[test]
fn process_loss_is_not_work_failure_and_duplicate_close_is_suppressed() {
    let attachment = make_attachment();
    let mut records = attachment.subscribe_records();
    attachment.emit(session::Event::TurnStart {
        turn_id: "u".into(),
    });
    let closed = session::Event::Closed {
        reason: session::CloseReason::Exited {
            message: "lost".into(),
        },
    };
    attachment.emit(closed.clone());
    attachment.emit(closed);
    attachment.emit(end("u"));
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
    let state = attachment.state.lock().unwrap();
    assert_eq!(state.root.agent.status, agent::Status::Unknown);
    assert_eq!(state.root.status, session::Status::NotLoaded);
    assert!(state.root.turns.is_empty());
    assert!(state.root.closed);
}
#[test]
fn siblings_have_independent_items_and_outlive_the_root_turn() {
    let attachment = make_attachment();
    let mut records = attachment.subscribe_records();
    for id in ["a", "b"] {
        attachment.upsert_agent(agent::Summary {
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
        attachment.emit_for(
            id,
            session::Event::TurnStart {
                turn_id: "same-turn".into(),
            },
        );
    }
    for (id, text) in [("a", "alpha"), ("b", "beta")] {
        attachment.emit_for(
            id,
            session::Event::ItemDelta {
                turn_id: "same-turn".into(),
                item_id: "same-item".into(),
                delta: turn::Delta::Text { text: text.into() },
            },
        );
    }
    attachment.emit(end("root-turn"));
    {
        let state = attachment.state.lock().unwrap();
        assert_eq!(state.aggregate_status(), session::Status::Active);
        for (id, expected) in [("a", "alpha"), ("b", "beta")] {
            assert!(
                matches!(&state.children[id].turns["same-turn"].turn.items[0], turn::Item::AgentMessage { text, .. } if text == expected)
            );
        }
    }
    attachment.emit_for(
        "a",
        session::Event::Closed {
            reason: session::CloseReason::Agent,
        },
    );
    attachment.emit_for(
        "b",
        session::Event::ItemEnd {
            turn_id: "same-turn".into(),
            item: turn::Item::AgentMessage {
                id: "same-item".into(),
                text: "beta".into(),
            },
        },
    );
    attachment.emit_for("b", end("same-turn"));
    attachment.emit_for(
        "b",
        session::Event::TurnStart {
            turn_id: "continued".into(),
        },
    );
    attachment.emit_for(
        "b",
        session::Event::Closed {
            reason: session::CloseReason::Agent,
        },
    );
    let state = attachment.state.lock().unwrap();
    assert!(!state.root.closed);
    assert_eq!(state.children["a"].agent.status, agent::Status::Closed);
    assert_eq!(state.children["b"].agent.status, agent::Status::Closed);
    assert_eq!(state.aggregate_status(), session::Status::Idle);
    drop(state);
    attachment.emit_for(
        "b",
        session::Event::TurnStart {
            turn_id: "reopened".into(),
        },
    );
    assert_eq!(
        attachment.state.lock().unwrap().children["b"].agent.status,
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
