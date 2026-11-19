//! Selected native traffic captured from a live session, not a synthetic scenario.

use serde_json::Value;

use super::{events::SessionState, process};
use crate::session::Event;

fn capture() -> Value {
    serde_json::from_str(include_str!("claude-observation-2.1.289.json")).unwrap()
}

#[test]
fn initialize_requests_full_descendant_text_without_changing_permissions() {
    let capture = capture();
    assert_eq!(
        process::initialize_request(),
        capture["frames"][0]["message"]["request"]
    );
}

#[test]
fn native_nested_ancestry_joins_task_ids_to_forwarded_tool_calls() {
    let capture = capture();
    let frames = capture["frames"].as_array().unwrap();
    for (task, tool, owner_tool, depth) in [
        ("child", "spawn-child", Value::Null, 1),
        ("nested-parent", "spawn-parent", Value::Null, 1),
        (
            "grandchild",
            "spawn-grandchild",
            Value::String("spawn-parent".into()),
            2,
        ),
    ] {
        let start = frames
            .iter()
            .map(|f| &f["message"])
            .find(|m| m["subtype"] == "task_started" && m["task_id"] == task)
            .unwrap();
        assert_eq!(start["tool_use_id"], tool);
        assert_eq!(start["spawn_depth"], depth);
        assert_eq!(start["task_type"], "local_agent");
        let owner = frames
            .iter()
            .map(|f| &f["message"])
            .find(|m| {
                m["type"] == "assistant"
                    && m["message"]["content"]
                        .as_array()
                        .is_some_and(|blocks| blocks.iter().any(|b| b["id"] == tool))
            })
            .unwrap();
        // Actual tool ownership supplies the parent; depth alone cannot.
        assert_eq!(owner["parent_tool_use_id"], owner_tool);
        let reply = frames
            .iter()
            .map(|f| &f["message"])
            .find(|m| m["type"] == "assistant" && m["parent_tool_use_id"] == tool)
            .unwrap();
        assert_eq!(reply["message"]["model"], "claude-sonnet-4-6");
    }
}

#[test]
fn descendant_messages_do_not_leak_into_root_projection_and_task_frames_are_lossless() {
    let capture = capture();
    let mut state = SessionState::default();
    let mut descendants = 0;
    let mut task_frames = 0;
    for frame in capture["frames"].as_array().unwrap() {
        let m = &frame["message"];
        if frame["direction"] == "in" {
            if m["type"] == "user" {
                state.turn_started(m["uuid"].as_str().unwrap(), "fixture input");
            }
            continue;
        }
        let events = state.message(m);
        if m.get("parent_tool_use_id").is_some_and(|id| !id.is_null()) {
            assert!(events.is_empty());
            descendants += 1;
        } else if m["type"] == "system" {
            let [Event::Raw { name, payload }] = events.as_slice() else {
                panic!("native task frames must remain available before normalization");
            };
            assert_eq!(
                name,
                &format!("claude/system/{}", m["subtype"].as_str().unwrap())
            );
            assert_eq!(payload, m);
            task_frames += 1;
        }
    }
    assert!(descendants >= 5);
    assert!(task_frames >= 10);
}

#[test]
fn background_completion_is_independent_of_root_turn_and_live_membership() {
    let capture = capture();
    let frames = capture["frames"].as_array().unwrap();
    let at = |predicate: &dyn Fn(&Value) -> bool| {
        frames
            .iter()
            .position(|f| predicate(&f["message"]))
            .unwrap()
    };
    let start = at(&|m| m["subtype"] == "task_started" && m["task_id"] == "bash-task");
    let root_end = frames[start..]
        .iter()
        .position(|f| f["message"]["type"] == "result")
        .unwrap()
        + start;
    let completion = at(&|m| m["subtype"] == "task_notification" && m["task_id"] == "bash-task");
    assert!(start < root_end && root_end < completion);
    assert_eq!(frames[start]["message"]["is_backgrounded"], true);
    assert_eq!(frames[start]["message"]["task_type"], "local_bash");
    assert_eq!(frames[completion]["message"]["status"], "completed");
    assert_eq!(
        frames[completion]["message"]["output_file"],
        "/tmp/claude-fixture/tasks/bash-task.output"
    );
    let membership = frames
        .iter()
        .filter(|f| f["message"]["subtype"] == "background_tasks_changed")
        .collect::<Vec<_>>();
    assert_eq!(membership.len(), 2);
    // This CLI emits task objects, rather than the ID array in older SDK types.
    assert_eq!(membership[0]["message"]["tasks"][0]["task_id"], "bash-task");
    assert_eq!(membership[1]["message"]["tasks"], serde_json::json!([]));
}
