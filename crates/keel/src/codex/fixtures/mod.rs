//! Regression cases captured through keel-tester with GPT-6 Luna.
//!
//! These exercise the existing decoders and mappers, not recursive routing.
//! The fixture also retains native terminal list/stop responses for the later
//! task adapter. Native IDs and the scratch working directory are anonymized.

use std::collections::HashMap;

use serde_json::Value;

use super::{events, history, types};
use crate::session::{Event, Status};
use crate::turn::{Delta, Item, ToolDetail, ToolStatus};

fn capture() -> Value {
    serde_json::from_str(include_str!("codex-observation-0.159.0.json")).unwrap()
}

#[test]
fn stored_descendants_retain_native_ancestry_in_source_metadata() {
    let capture = capture();
    let threads = capture["threads"].as_array().unwrap();
    for (raw, id, parent, depth) in [
        (&threads[1], "child", "root", 1),
        (&threads[2], "grandchild", "child", 2),
    ] {
        let thread: types::Thread = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(thread.id, id);
        assert_eq!(thread.model.as_deref(), Some("gpt-6-luna"));
        assert_eq!(
            thread
                .source
                .pointer("/subAgent/thread_spawn/parent_thread_id"),
            Some(&Value::String(parent.into()))
        );
        assert_eq!(
            thread.source.pointer("/subAgent/thread_spawn/depth"),
            Some(&Value::from(depth))
        );
        // The capture was read after its app-server shut down. Stored existence
        // must not be mistaken for currently running work.
        assert_eq!(history::summary(thread).status, Status::NotLoaded);
    }
}

#[test]
fn child_streams_keep_turn_ids_and_unknown_activity_payloads() {
    let capture = capture();
    let mut text_by_thread = HashMap::<String, String>::new();
    let mut finished_threads = Vec::new();
    let mut activity_count = 0;
    for notification in capture["notifications"].as_array().unwrap() {
        let method = notification["method"].as_str().unwrap();
        let params = &notification["params"];
        let thread_id = params["threadId"].as_str().unwrap();
        match events::from_notification(method, params.clone()) {
            Event::ItemDelta {
                turn_id,
                item_id,
                delta: Delta::Text { text },
            } => {
                assert_eq!(Some(turn_id.as_str()), params["turnId"].as_str());
                assert_eq!(Some(item_id.as_str()), params["itemId"].as_str());
                text_by_thread
                    .entry(thread_id.into())
                    .or_default()
                    .push_str(&text);
            }
            Event::ItemEnd {
                item: Item::Other { kind, raw, .. },
                ..
            } if kind == "subAgentActivity" || kind == "collabAgentToolCall" => {
                // Keep native agent IDs, paths, and coordination fields until
                // Keel has a dedicated normalized mapping for these items.
                assert_eq!(raw, params["item"]);
                activity_count += 1;
            }
            Event::TurnEnd { turn } => {
                assert_eq!(Some(turn.id.as_str()), params["turn"]["id"].as_str());
                assert!(turn.items.is_empty(), "final items must not be repeated");
                finished_threads.push(thread_id);
            }
            _ => {}
        }
    }
    for thread_id in ["root", "child", "grandchild"] {
        assert!(text_by_thread[thread_id].contains("nested-ok"));
        assert!(finished_threads.contains(&thread_id));
    }
    assert!(activity_count > 0);
}

#[test]
fn managed_command_outlives_its_root_turn_and_reports_native_termination() {
    let capture = capture();
    let notifications = capture["terminalNotifications"].as_array().unwrap();
    let Event::ItemStart {
        item: Item::ToolCall(started),
        ..
    } = events::from_notification("item/started", notifications[0]["params"].clone())
    else {
        panic!("expected a running command item");
    };
    assert_eq!(started.status, ToolStatus::InProgress);
    let Event::TurnEnd { turn } =
        events::from_notification("turn/completed", notifications[1]["params"].clone())
    else {
        panic!("expected the root turn to finish before the command");
    };
    assert_eq!(turn.status, crate::turn::Status::Completed);
    let notification = &notifications[2];
    let params = &notification["params"];
    let Event::ItemEnd {
        item: Item::ToolCall(call),
        ..
    } = events::from_notification("item/completed", params.clone())
    else {
        panic!("expected a mapped command item");
    };
    assert_eq!(call.id, started.id);
    // Codex reports its own termination as a failed command item. Later task
    // mapping must correlate a stop receipt rather than invent a successful exit.
    assert_eq!(call.status, ToolStatus::Failed);
    let ToolDetail::Command {
        command, exit_code, ..
    } = call.detail
    else {
        panic!("expected command details");
    };
    assert!(command.contains("sleep 60"));
    assert_eq!(exit_code, Some(-1));
    // The same process was still listed after the root turn ended. A task
    // adapter must not infer process exit from root-turn completion.
    assert_eq!(
        params["item"]["processId"],
        capture["terminalList"]["data"][0]["processId"]
    );
}
