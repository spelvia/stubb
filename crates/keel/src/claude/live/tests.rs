use serde_json::{Value, json};
use tokio::sync::broadcast;

use super::Attachment;
use crate::{agent, observation as obs, session, turn};

fn drain<T: Clone>(receiver: &mut broadcast::Receiver<T>) -> Vec<T> {
    let mut events = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        events.push(event);
    }
    events
}

fn tool(live: &Attachment, parent: Option<&str>, id: &str) {
    live.message(&json!({"type":"assistant", "parent_tool_use_id":parent,
        "message":{"id":format!("msg-{id}"),"model":"claude-sonnet-4-6",
        "content":[{"type":"tool_use","id":id,"name":"Agent","input":{}}]}}));
}

fn start(live: &Attachment, id: &str, tool: &str) {
    live.message(
        &json!({"type":"system","subtype":"task_started","task_id":id,
        "tool_use_id":tool,"task_type":"local_agent","subagent_type":"general-purpose",
        "description":"native delegated work","is_backgrounded":true}),
    );
}

fn finish(live: &Attachment, id: &str, tool: &str, status: &str) {
    live.message(
        &json!({"type":"system","subtype":"task_notification","task_id":id,
        "tool_use_id":tool,"status":status}),
    );
}

#[test]
fn captured_tree_keeps_native_ids_models_and_exact_legacy_projection() {
    let capture: Value =
        serde_json::from_str(include_str!("../fixtures/claude-observation-2.1.289.json")).unwrap();
    let live = Attachment::new("root");
    let mut records = live.subscribe_records();
    let mut legacy = live.subscribe();
    let mut original = crate::claude::events::SessionState::default();
    let mut expected = Vec::new();
    for frame in capture["frames"].as_array().unwrap() {
        let m = &frame["message"];
        if frame["direction"] == "in" {
            if m["type"] == "user" {
                let id = m["uuid"].as_str().unwrap();
                live.turn_started(id, "fixture input");
                expected.extend(original.turn_started(id, "fixture input"));
            }
        } else {
            live.message(m);
            expected.extend(original.message(m));
        }
    }
    assert_eq!(drain(&mut legacy), expected);
    let snapshot = live.snapshot();
    for (id, parent) in [
        ("child", "root"),
        ("nested-parent", "root"),
        ("grandchild", "nested-parent"),
    ] {
        let agent = snapshot.agents.iter().find(|a| a.id == id).unwrap();
        assert_eq!(agent.parent, agent::Parent::Agent { id: parent.into() });
        assert_eq!(agent.model.as_deref(), Some("claude-sonnet-4-6"));
        assert_eq!(agent.status, agent::Status::Completed);
    }
    let records = drain(&mut records);
    for (id, marker) in [("child", "child-ok"), ("grandchild", "nested-ok")] {
        let announced = records
            .iter()
            .position(|r| matches!(&r.event, obs::Event::Agent { agent } if agent.id == id))
            .unwrap();
        let reply = records.iter().position(|r| r.agent_id.as_deref() == Some(id) && matches!(&r.event,
            obs::Event::Activity { event: session::Event::ItemEnd { item: turn::Item::AgentMessage { text, .. }, .. }, .. } if text.contains(marker))).unwrap();
        assert!(announced < reply);
    }
    assert!(
        records
            .windows(2)
            .all(|r| r[1].seq == r[0].seq + 1 && r[1].stream_id == r[0].stream_id)
    );
    assert_eq!(records.last().unwrap().seq, snapshot.seq);
    let command = snapshot.tasks.iter().find(|t| t.id == "bash-task").unwrap();
    assert_eq!(command.status, crate::task::Status::Completed);
    assert_eq!(command.owner_agent_id.as_deref(), Some("root"));
    assert!(
        matches!(&command.kind, crate::task::Kind::Command {command} if command == "sleep 12; printf claude-task-ok")
    );
    assert_eq!(snapshot.tasks.len(), 4);
    assert_eq!(
        snapshot.capabilities.stop_agent.support,
        obs::Support::Supported
    );
}

#[test]
fn early_grandchild_and_unknown_parent_are_repaired_using_explicit_ownership() {
    let live = Attachment::new("root");
    let mut records = live.subscribe_records();
    live.turn_started("root-turn", "nest");
    // Child traffic can precede its native bookend and owning tool call.
    live.message(&json!({"type":"assistant","parent_tool_use_id":"spawn-grandchild",
        "message":{"id":"early-reply","model":"claude-sonnet-4-6","content":[{"type":"text","text":"early-ok"}]}}));
    start(&live, "grandchild", "spawn-grandchild");
    assert_eq!(
        live.snapshot()
            .agents
            .iter()
            .find(|a| a.id == "grandchild")
            .unwrap()
            .parent,
        agent::Parent::Unknown
    );
    // Even an explicit depth is not a parent link.
    start(&live, "child", "spawn-child");
    tool(&live, Some("spawn-child"), "spawn-grandchild");
    tool(&live, None, "spawn-child");
    let snapshot = live.snapshot();
    assert_eq!(
        snapshot
            .agents
            .iter()
            .find(|a| a.id == "grandchild")
            .unwrap()
            .parent,
        agent::Parent::Agent { id: "child".into() }
    );
    assert_eq!(
        snapshot
            .agents
            .iter()
            .find(|a| a.id == "child")
            .unwrap()
            .parent,
        agent::Parent::Agent { id: "root".into() }
    );
    assert!(snapshot.active_turns.iter().any(|t| {
        t.agent_id == "grandchild"
            && t.turn
                .items
                .iter()
                .any(|i| matches!(i, turn::Item::AgentMessage { text, .. } if text == "early-ok"))
    }));
    assert_eq!(drain(&mut records).iter().filter(|r| matches!(&r.event,
        obs::Event::Activity {event:session::Event::ItemEnd {item:turn::Item::AgentMessage {text,..},..},..} if text == "early-ok")).count(), 1);
}

#[test]
fn siblings_outlive_root_and_native_outcomes_do_not_close_contexts() {
    let live = Attachment::new("root");
    live.turn_started("t", "two children");
    for (id, tool_id) in [("left", "spawn-left"), ("right", "spawn-right")] {
        tool(&live, None, tool_id);
        start(&live, id, tool_id);
    }
    live.message(&json!({"type":"result","subtype":"success","is_error":false,"usage":{}}));
    assert_eq!(live.snapshot().status, session::Status::Active);
    finish(&live, "left", "spawn-left", "stopped");
    assert_eq!(
        live.snapshot()
            .agents
            .iter()
            .find(|a| a.id == "left")
            .unwrap()
            .status,
        agent::Status::Interrupted
    );
    assert_eq!(
        live.snapshot()
            .agents
            .iter()
            .find(|a| a.id == "right")
            .unwrap()
            .status,
        agent::Status::Running
    );
    finish(&live, "right", "spawn-right", "completed");
    assert_eq!(live.snapshot().status, session::Status::Idle);
    // A duplicate start cannot resurrect a completed execution.
    start(&live, "right", "spawn-right");
    assert_eq!(live.snapshot().status, session::Status::Idle);
    tool(&live, None, "resume-right");
    start(&live, "right", "resume-right");
    // A late notification from the old execution cannot end the continuation.
    finish(&live, "right", "spawn-right", "failed");
    assert_eq!(
        live.snapshot()
            .agents
            .iter()
            .find(|a| a.id == "right")
            .unwrap()
            .status,
        agent::Status::Running
    );
    finish(&live, "right", "resume-right", "failed");
    assert_eq!(
        live.snapshot()
            .agents
            .iter()
            .find(|a| a.id == "right")
            .unwrap()
            .status,
        agent::Status::Failed
    );
}

#[test]
fn snapshots_materialize_child_streams_at_same_watermark_as_publication() {
    let live = Attachment::new("root");
    let mut records = live.subscribe_records();
    live.turn_started("t", "child");
    tool(&live, None, "spawn");
    start(&live, "child", "spawn");
    for event in [
        json!({"type":"message_start","message":{"id":"message"}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text"}}),
        json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"half"}}),
    ] {
        live.message(&json!({"type":"stream_event","parent_tool_use_id":"spawn","event":event}));
    }
    let snapshot = live.snapshot();
    let child = snapshot
        .active_turns
        .iter()
        .find(|t| t.agent_id == "child")
        .unwrap();
    assert_eq!(child.turn.id, "spawn");
    assert!(matches!(&child.turn.items[0], turn::Item::AgentMessage {text,..} if text == "half"));
    assert_eq!(drain(&mut records).last().unwrap().seq, snapshot.seq);
    live.message(&json!({"type":"stream_event","parent_tool_use_id":"spawn","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" done"}}}));
    let newer = drain(&mut records);
    assert_eq!(newer[0].seq, snapshot.seq + 1);
    assert!(newer.iter().all(|r| r.stream_id == snapshot.stream_id));
    let mut late = live.subscribe_records();
    let bootstrap = drain(&mut late);
    assert!(
        bootstrap
            .iter()
            .any(|r| matches!(&r.event,obs::Event::Agent {agent} if agent.id == "child"))
    );
    assert_eq!(live.snapshot().stream_id, snapshot.stream_id);
}

#[test]
fn unresolved_traffic_is_bounded_and_process_loss_is_unknown_not_interrupted() {
    let live = Attachment::new("root");
    let mut records = live.subscribe_records();
    let mut legacy = live.subscribe();
    live.turn_started("t", "child");
    tool(&live, None, "spawn");
    start(&live, "child", "spawn");
    for _ in 0..257 {
        live.message(&json!({"type":"assistant","parent_tool_use_id":"missing","message":{}}));
    }
    assert!(drain(&mut records).iter().any(|r| r.agent_id.is_none() && matches!(&r.event,
        obs::Event::Activity {event:session::Event::Raw {name,..},..} if name == "keel/observationGap")));
    live.close(session::CloseReason::Exited {
        message: "native process exited".into(),
    });
    let snapshot = live.snapshot();
    assert_eq!(snapshot.status, session::Status::NotLoaded);
    assert!(snapshot.active_turns.is_empty());
    assert!(
        snapshot
            .agents
            .iter()
            .all(|a| a.status == agent::Status::Unknown)
    );
    let seq = snapshot.seq;
    live.close(session::CloseReason::Client);
    finish(&live, "child", "spawn", "stopped");
    assert_eq!(live.snapshot().seq, seq);
    assert_eq!(
        drain(&mut legacy)
            .iter()
            .filter(|e| matches!(e, session::Event::Closed { .. }))
            .count(),
        1
    );
    assert_ne!(
        Attachment::new("root").snapshot().stream_id,
        snapshot.stream_id
    );
}

#[test]
fn child_permissions_are_declined_without_false_root_attribution() {
    let live = Attachment::new("root");
    let mut records = live.subscribe_records();
    let mut legacy = live.subscribe();
    live.turn_started("t", "child");
    tool(&live, None, "spawn");
    start(&live, "child", "spawn");
    tool(&live, Some("spawn"), "child-tool");
    drain(&mut records);
    drain(&mut legacy);
    live.tool_declined(
        "Bash",
        Some("child-tool"),
        &json!({"command":"native command"}),
    );
    assert!(drain(&mut legacy).is_empty());
    assert!(drain(&mut records).iter().any(|r| r.agent_id.as_deref() == Some("child") && matches!(&r.event,
        obs::Event::Activity {event:session::Event::RequestDeclined {turn_id:Some(id),..},..} if id == "spawn")));
    live.message(&json!({"type":"user","parent_tool_use_id":"spawn","message":{"content":[{"type":"tool_result","tool_use_id":"child-tool","is_error":true,"content":"declined"}]}}));
    assert!(drain(&mut records).iter().any(|r| matches!(&r.event,
        obs::Event::Activity {event:session::Event::ItemEnd {item:turn::Item::ToolCall(call),..},..} if call.status == turn::ToolStatus::Declined)));
    live.tool_declined("Bash", Some("unknown-tool"), &json!({}));
    assert!(drain(&mut records).iter().all(|r| r.agent_id.is_none()));
}

#[test]
fn continuation_ownership_does_not_reparent_an_existing_native_context() {
    let live = Attachment::new("root");
    live.turn_started("t", "two children");
    tool(&live, None, "spawn-left");
    start(&live, "left", "spawn-left");
    tool(&live, None, "spawn-right");
    start(&live, "right", "spawn-right");
    finish(&live, "right", "spawn-right", "completed");
    tool(&live, Some("spawn-left"), "resume-right");
    start(&live, "right", "resume-right");
    let right = live
        .snapshot()
        .agents
        .into_iter()
        .find(|a| a.id == "right")
        .unwrap();
    assert_eq!(right.parent, agent::Parent::Agent { id: "root".into() });
    assert_eq!(
        right.origin.unwrap().item_id.as_deref(),
        Some("spawn-right")
    );
    live.message(
        &json!({"type":"system","subtype":"task_started","task_id":"bash",
        "tool_use_id":"bash-tool","task_type":"local_bash","subagent_type":"general-purpose"}),
    );
    assert!(!live.snapshot().agents.iter().any(|a| a.id == "bash"));
}

#[test]
fn task_membership_patch_and_completion_preserve_native_outcomes_and_progress() {
    let live = Attachment::new("root");
    live.message(&json!({"type":"system","subtype":"background_tasks_changed","tasks":[{"task_id":"cmd","task_type":"local_bash","description":"native command"}]}));
    assert_eq!(live.snapshot().tasks[0].owner_agent_id, None);
    assert_eq!(
        live.snapshot().tasks[0].status,
        crate::task::Status::Unknown
    );
    live.message(&json!({"type":"system","subtype":"task_started","task_id":"cmd","tool_use_id":"bash","task_type":"local_bash","is_backgrounded":true}));
    live.message(&json!({"type":"system","subtype":"task_progress","task_id":"cmd","tool_use_id":"bash","description":"progress","usage":{"total_tokens":19,"tool_uses":2,"duration_ms":100},"last_tool_name":"Bash"}));
    live.message(&json!({"type":"system","subtype":"task_updated","task_id":"cmd","patch":{"status":"paused","is_backgrounded":true}}));
    assert_eq!(live.snapshot().tasks[0].status, crate::task::Status::Paused);
    assert_eq!(
        live.snapshot().tasks[0]
            .progress
            .as_ref()
            .unwrap()
            .total_tokens,
        Some(19)
    );
    live.message(&json!({"type":"system","subtype":"background_tasks_changed","tasks":[]}));
    assert_eq!(
        live.snapshot().tasks[0].status,
        crate::task::Status::Unknown
    );
    finish(&live, "cmd", "bash", "failed");
    assert_eq!(live.snapshot().tasks[0].status, crate::task::Status::Failed);
    live.message(&json!({"type":"system","subtype":"background_tasks_changed","tasks":["cmd"]}));
    live.close(session::CloseReason::Client);
    assert_eq!(live.snapshot().tasks[0].status, crate::task::Status::Failed);
}

#[test]
fn task_outcome_before_start_and_missing_logs_are_not_reinterpreted_as_running() {
    let live = Attachment::new("root");
    finish(&live, "cmd", "bash", "completed");
    live.message(&json!({"type":"system","subtype":"task_started","task_id":"cmd","tool_use_id":"bash","task_type":"local_bash","is_backgrounded":true}));
    assert_eq!(
        live.snapshot().tasks[0].status,
        crate::task::Status::Completed
    );
    assert_eq!(
        live.snapshot().tasks[0].output,
        crate::task::OutputAvailability::Unknown
    );
    let binding = live.output_binding("cmd").unwrap();
    assert_eq!(
        crate::claude::output::read(binding, Default::default())
            .unwrap()
            .availability,
        crate::task::OutputAvailability::Unavailable
    );
    assert!(matches!(
        live.output_binding("missing"),
        Err(crate::Error::TaskNotFound { .. })
    ));
}

#[test]
fn native_status_completion_does_not_promise_that_log_writes_are_final() {
    let live = Attachment::new("root");
    live.message(&json!({"type":"system","subtype":"task_started","task_id":"cmd","tool_use_id":"bash","task_type":"local_bash","is_backgrounded":true}));
    live.message(&json!({"type":"system","subtype":"task_updated","task_id":"cmd","patch":{"status":"completed"}}));
    assert_eq!(
        live.snapshot().tasks[0].status,
        crate::task::Status::Completed
    );
    assert!(!live.output_binding("cmd").unwrap().terminal);
    finish(&live, "cmd", "bash", "completed");
    assert!(live.output_binding("cmd").unwrap().terminal);
}

#[test]
fn stops_deduplicate_native_execution_and_wait_for_termination_evidence() {
    use crate::control::{Disposition as D, Target};
    let live = Attachment::new("root");
    live.turn_started("turn", "child");
    tool(&live, None, "spawn");
    start(&live, "child", "spawn");
    let first = live
        .begin_stop(Target::Agent { id: "child".into() })
        .unwrap();
    assert_eq!(
        first.native,
        Some(json!({"subtype":"stop_task","task_id":"child"}))
    );
    let duplicate = live
        .begin_stop(Target::Agent { id: "child".into() })
        .unwrap();
    assert_eq!(first.id, duplicate.id);
    assert!(duplicate.native.is_none());
    let task = live
        .begin_stop(Target::Task { id: "child".into() })
        .unwrap();
    assert!(task.native.is_none());
    assert_ne!(first.id, task.id);
    assert_eq!(
        live.finish_stop(&first.id, &Ok(())).disposition,
        D::Accepted
    );
    assert_eq!(live.stop_receipt(&task.id).disposition, D::Accepted);
    assert_eq!(
        live.snapshot().tasks[0].status,
        crate::task::Status::Stopping
    );
    finish(&live, "child", "spawn", "stopped");
    assert_eq!(live.stop_receipt(&first.id).disposition, D::Confirmed);
    assert_eq!(live.stop_receipt(&task.id).disposition, D::Confirmed);
    assert!(live.snapshot().pending_controls.is_empty());
    assert_eq!(
        live.snapshot().tasks[0].status,
        crate::task::Status::Stopped
    );
    assert_eq!(
        live.begin_stop(Target::Task { id: "child".into() })
            .unwrap()
            .native,
        None
    );
}

#[test]
fn terminal_before_ack_and_new_generations_cannot_be_overwritten() {
    use crate::control::{Disposition as D, Target};
    for (status, expected) in [
        ("completed", D::AlreadyFinished),
        ("stopped", D::Confirmed),
        ("failed", D::AlreadyFinished),
    ] {
        let live = Attachment::new("root");
        tool(&live, None, "spawn");
        start(&live, "child", "spawn");
        let request = live
            .begin_stop(Target::Task { id: "child".into() })
            .unwrap();
        finish(&live, "child", "spawn", status);
        assert!(!live.stop_is_current(&request.id));
        assert_eq!(live.finish_stop(&request.id, &Ok(())).disposition, expected);
        assert_ne!(
            live.snapshot().tasks[0].status,
            crate::task::Status::Stopping
        );
    }
    let live = Attachment::new("root");
    tool(&live, None, "spawn");
    start(&live, "child", "spawn");
    let request = live
        .begin_stop(Target::Task { id: "child".into() })
        .unwrap();
    tool(&live, None, "resume");
    start(&live, "child", "resume");
    assert!(!live.stop_is_current(&request.id));
    assert_eq!(
        live.finish_stop(&request.id, &Ok(())).disposition,
        D::Unconfirmed
    );
    assert_eq!(
        live.snapshot().tasks[0].status,
        crate::task::Status::Running
    );
    finish(&live, "child", "spawn", "stopped");
    assert_eq!(live.stop_receipt(&request.id).disposition, D::Unconfirmed);
    assert_eq!(
        live.snapshot().tasks[0].status,
        crate::task::Status::Running
    );
}

#[test]
fn lost_ack_can_resolve_but_detached_or_rejected_stops_cannot() {
    use crate::control::{Disposition as D, Target};
    let live = Attachment::new("root");
    tool(&live, None, "spawn");
    start(&live, "child", "spawn");
    let request = live
        .begin_stop(Target::Task { id: "child".into() })
        .unwrap();
    assert_eq!(
        live.finish_stop(
            &request.id,
            &Err(crate::Error::Unsupported {
                operation: "timeout"
            })
        )
        .disposition,
        D::Unconfirmed
    );
    finish(&live, "child", "spawn", "stopped");
    assert_eq!(live.stop_receipt(&request.id).disposition, D::Confirmed);
    tool(&live, None, "resume");
    start(&live, "child", "resume");
    let request = live
        .begin_stop(Target::Task { id: "child".into() })
        .unwrap();
    live.close(session::CloseReason::Client);
    assert_eq!(
        live.finish_stop(&request.id, &Ok(())).disposition,
        D::Unconfirmed
    );
    finish(&live, "child", "resume", "stopped");
    assert_eq!(live.stop_receipt(&request.id).disposition, D::Unconfirmed);
    assert_eq!(
        live.snapshot().tasks[0].status,
        crate::task::Status::Unknown
    );
    assert!(matches!(
        live.begin_stop(Target::Agent {
            id: "missing".into()
        }),
        Err(crate::Error::AgentNotFound { .. })
    ));
    assert!(matches!(
        live.begin_stop(Target::Task {
            id: "missing".into()
        }),
        Err(crate::Error::TaskNotFound { .. })
    ));
}

#[test]
fn root_stop_is_scoped_to_current_native_turn() {
    use crate::control::{Disposition as D, Target};
    let live = Attachment::new("root");
    let idle = live
        .begin_stop(Target::Agent { id: "root".into() })
        .unwrap();
    assert!(idle.native.is_none());
    assert_eq!(live.stop_receipt(&idle.id).disposition, D::NotRunning);
    live.turn_started("first", "work");
    let request = live
        .begin_stop(Target::Agent { id: "root".into() })
        .unwrap();
    assert_eq!(request.native, Some(json!({"subtype":"interrupt"})));
    assert_eq!(
        live.finish_stop(&request.id, &Ok(())).disposition,
        D::Accepted
    );
    live.turn_failed("first", "interrupted");
    // A generic turn failure is natural terminal evidence, not an interruption claim.
    assert_eq!(
        live.stop_receipt(&request.id).disposition,
        D::AlreadyFinished
    );
    live.turn_started("second", "continue");
    assert!(!live.stop_is_current(&request.id));
}

#[test]
fn child_outcome_before_identity_bookend_is_recovered_without_resurrection() {
    let live = Attachment::new("root");
    tool(&live, None, "spawn");
    finish(&live, "child", "spawn", "stopped");
    start(&live, "child", "spawn");
    let snap = live.snapshot();
    assert_eq!(
        snap.agents.iter().find(|a| a.id == "child").unwrap().status,
        agent::Status::Interrupted
    );
    assert_eq!(snap.tasks[0].status, crate::task::Status::Stopped);
    assert!(!snap.active_turns.iter().any(|t| t.agent_id == "child"));
    finish(&live, "child", "spawn", "completed");
    assert_eq!(
        live.snapshot().tasks[0].status,
        crate::task::Status::Stopped
    );
}

#[test]
fn native_task_id_without_tool_only_resolves_an_unambiguous_first_execution() {
    let live = Attachment::new("root");
    tool(&live, None, "spawn");
    start(&live, "child", "spawn");
    live.message(&json!({"type":"system","subtype":"task_notification","task_id":"child","status":"completed"}));
    assert_eq!(
        live.snapshot()
            .agents
            .iter()
            .find(|a| a.id == "child")
            .unwrap()
            .status,
        agent::Status::Completed
    );
    tool(&live, None, "resume");
    start(&live, "child", "resume");
    live.message(&json!({"type":"system","subtype":"task_progress","task_id":"child","tool_use_id":"resume","usage":{"total_tokens":10}}));
    live.message(&json!({"type":"system","subtype":"task_progress","task_id":"child","tool_use_id":"spawn","usage":{"total_tokens":999}}));
    live.message(&json!({"type":"system","subtype":"task_progress","task_id":"child","usage":{"total_tokens":888}}));
    live.message(&json!({"type":"system","subtype":"task_notification","task_id":"child","status":"stopped"}));
    let snap = live.snapshot();
    assert_eq!(
        snap.agents.iter().find(|a| a.id == "child").unwrap().status,
        agent::Status::Running
    );
    assert_eq!(
        snap.tasks[0].progress.as_ref().unwrap().total_tokens,
        Some(10)
    );
    assert_eq!(snap.tasks[0].status, crate::task::Status::Running);
}

#[test]
fn forwarded_command_results_bind_logs_using_native_bookend_identity_only() {
    let live = Attachment::new("root");
    live.turn_started("turn", "child");
    tool(&live, None, "spawn");
    start(&live, "child", "spawn");
    live.message(&json!({"type":"assistant","parent_tool_use_id":"spawn","message":{"id":"bash-msg","content":[{"type":"tool_use","id":"bash","name":"Bash","input":{"command":"native command"}}]}}));
    live.message(&json!({"type":"system","subtype":"task_started","task_id":"cmd","tool_use_id":"bash","task_type":"local_bash","is_backgrounded":true}));
    live.message(&json!({"type":"user","parent_tool_use_id":"spawn","message":{"content":[{"type":"tool_result","tool_use_id":"unrelated","content":"/tmp/native/root/tasks/cmd.output"}]}}));
    assert_eq!(live.output_binding("cmd").unwrap().path, None);
    live.message(&json!({"type":"user","parent_tool_use_id":"spawn","message":{"content":[{"type":"tool_result","tool_use_id":"bash","content":"Background output: /tmp/native/root/tasks/cmd.output"}]}}));
    assert_eq!(
        live.output_binding("cmd").unwrap().path,
        Some("/tmp/native/root/tasks/cmd.output".into())
    );
    assert_eq!(
        live.snapshot()
            .tasks
            .iter()
            .find(|t| t.id == "cmd")
            .unwrap()
            .owner_agent_id
            .as_deref(),
        Some("child")
    );
}
