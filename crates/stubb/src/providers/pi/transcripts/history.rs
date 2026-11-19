//! Groups a stored Pi session into turns.
//!
//! Entries form a tree; the active branch runs from the last appended entry
//! back to the root. A turn starts at each user message and is named after
//! the message's timestamp, as live turns are. Tool results complete the matching tool call. Assistant
//! content items are named `<message timestamp>:<block index>`, as live
//! sessions name them.

use std::collections::HashMap;

use serde_json::Value;

use crate::core::runtime::files::timestamp_millis;
use crate::providers::pi::{events, tools};
use crate::turn::{self, Item, ToolCall, ToolStatus, UserContent};

pub(in crate::providers::pi) fn turns(entries: &[Value]) -> Vec<turn::Turn> {
    let mut turns: Vec<turn::Turn> = Vec::new();
    // Start and latest timestamp of the current turn, in milliseconds.
    let mut span: Option<(i64, i64)> = None;
    // Arguments of tool calls, by id, to rebuild their detail with the result.
    let mut calls: HashMap<String, (String, Value)> = HashMap::new();
    for entry in active_branch(entries) {
        if entry.get("type").and_then(Value::as_str) != Some("message") {
            continue;
        }
        let message = entry.get("message").unwrap_or(&Value::Null);
        let role = message.get("role").and_then(Value::as_str);
        if !matches!(role, Some("user" | "assistant" | "toolResult")) {
            continue;
        }
        let at = entry
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(timestamp_millis);
        if role == Some("user") || turns.is_empty() {
            close_turn(turns.last_mut(), span);
            turns.push(new_turn(&events::turn_id(message)));
            span = at.map(|at| (at, at));
        } else if let (Some((_, latest)), Some(at)) = (&mut span, at) {
            *latest = at;
        }
        let turn = turns.last_mut().expect("a turn exists");
        match role {
            Some("user") => turn.items.push(Item::UserMessage {
                id: turn.id.clone(),
                content: user_content(message.get("content").unwrap_or(&Value::Null)),
            }),
            Some("assistant") => assistant(turn, message, &mut calls),
            _ => tool_result(turn, message, &calls),
        }
    }
    close_turn(turns.last_mut(), span);
    turns
}

/// Entries from the root to the last appended entry.
pub(in crate::providers::pi) fn active_branch(entries: &[Value]) -> Vec<&Value> {
    let by_id: HashMap<&str, &Value> = entries
        .iter()
        .filter_map(|entry| Some((entry.get("id")?.as_str()?, entry)))
        .collect();
    let mut branch = Vec::new();
    let mut next = entries
        .iter()
        .rev()
        .find(|entry| entry.get("type").and_then(Value::as_str) != Some("session"));
    while let Some(entry) = next {
        // A broken or cyclic chain ends the branch.
        if branch.len() > entries.len() {
            break;
        }
        branch.push(entry);
        next = entry
            .get("parentId")
            .and_then(Value::as_str)
            .and_then(|parent| by_id.get(parent).copied());
    }
    branch.reverse();
    branch
}

fn close_turn(turn: Option<&mut turn::Turn>, span: Option<(i64, i64)>) {
    if let (Some(turn), Some((start, end))) = (turn, span) {
        turn.started_at = Some(start / 1000);
        turn.completed_at = Some(end / 1000);
        turn.duration_ms = Some(end - start);
    }
}

fn new_turn(id: &str) -> turn::Turn {
    turn::Turn {
        id: id.to_owned(),
        status: turn::Status::Completed,
        items: Vec::new(),
        started_at: None,
        completed_at: None,
        duration_ms: None,
        error: None,
    }
}

pub(in crate::providers::pi) fn user_content(content: &Value) -> Vec<UserContent> {
    match content {
        Value::String(text) => vec![UserContent::Text { text: text.clone() }],
        Value::Array(blocks) => blocks
            .iter()
            .map(|block| match block.get("type").and_then(Value::as_str) {
                Some("text") => UserContent::Text {
                    text: text(block, "text").to_owned(),
                },
                Some("image") => UserContent::Image {
                    source: "inline image".to_owned(),
                },
                other => UserContent::Other {
                    kind: other.unwrap_or_default().to_owned(),
                },
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn assistant(turn: &mut turn::Turn, message: &Value, calls: &mut HashMap<String, (String, Value)>) {
    let timestamp = message
        .get("timestamp")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let blocks = message.get("content").and_then(Value::as_array);
    for (index, block) in blocks.into_iter().flatten().enumerate() {
        let id = format!("{timestamp}:{index}");
        match block.get("type").and_then(Value::as_str) {
            Some("text") => turn.items.push(Item::AgentMessage {
                id,
                text: text(block, "text").trim().to_owned(),
            }),
            Some("thinking") if !text(block, "thinking").trim().is_empty() => {
                turn.items.push(Item::Thinking {
                    id,
                    text: text(block, "thinking").trim().to_owned(),
                    summarized: false,
                })
            }
            Some("toolCall") => {
                let call_id = block.get("id").and_then(Value::as_str).unwrap_or(&id);
                let name = text(block, "name");
                let args = block.get("arguments").cloned().unwrap_or(Value::Null);
                turn.items.push(Item::ToolCall(ToolCall {
                    id: call_id.to_owned(),
                    status: ToolStatus::InProgress,
                    duration_ms: None,
                    detail: tools::detail(name, &args, None, false),
                }));
                calls.insert(call_id.to_owned(), (name.to_owned(), args));
            }
            _ => {}
        }
    }
    (turn.status, turn.error) = events::outcome(message);
}

/// Fills in the tool call a tool result answers.
fn tool_result(turn: &mut turn::Turn, message: &Value, calls: &HashMap<String, (String, Value)>) {
    let id = text(message, "toolCallId");
    let Some((name, args)) = calls.get(id) else {
        return;
    };
    let failed = message.get("isError").and_then(Value::as_bool) == Some(true);
    for item in &mut turn.items {
        if let Item::ToolCall(call) = item
            && call.id == id
        {
            call.status = if failed {
                ToolStatus::Failed
            } else {
                ToolStatus::Completed
            };
            call.detail = tools::detail(name, args, Some(message), failed);
        }
    }
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn message(id: &str, parent: Option<&str>, message: Value) -> Value {
        json!({
            "type": "message",
            "id": id,
            "parentId": parent,
            "timestamp": "2026-10-05T10:00:00.000Z",
            "message": message,
        })
    }

    #[test]
    fn groups_the_active_branch_into_turns() {
        let turns = turns(&[
            json!({ "type": "session", "id": "s1", "cwd": "/repo" }),
            message("sys", None, json!({ "role": "system", "content": "" })),
            message(
                "u1",
                Some("sys"),
                json!({ "role": "user", "timestamp": 5, "content": [{ "type": "text", "text": "list files" }] }),
            ),
            message(
                "a1",
                Some("u1"),
                json!({ "role": "assistant", "timestamp": 7, "stopReason": "toolUse", "content": [
                    { "type": "thinking", "thinking": "use ls\n" },
                    { "type": "toolCall", "id": "call1", "name": "bash", "arguments": { "command": "ls" } }
                ] }),
            ),
            message(
                "r1",
                Some("a1"),
                json!({ "role": "toolResult", "toolCallId": "call1", "toolName": "bash",
                "content": [{ "type": "text", "text": "a.rs" }], "isError": false }),
            ),
            message(
                "a2",
                Some("r1"),
                json!({ "role": "assistant", "timestamp": 8, "stopReason": "stop",
                "content": [{ "type": "text", "text": "\n\nOne file." }] }),
            ),
            // An abandoned branch from u1, then the active one continues from a2.
            message(
                "x1",
                Some("u1"),
                json!({ "role": "assistant", "timestamp": 9, "stopReason": "stop", "content": [] }),
            ),
            message(
                "u2",
                Some("a2"),
                json!({ "role": "user", "timestamp": 20, "content": "thanks" }),
            ),
            message(
                "a3",
                Some("u2"),
                json!({ "role": "assistant", "timestamp": 10, "stopReason": "error",
                "errorMessage": "connection refused", "content": [] }),
            ),
        ]);

        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].id, "5");
        assert_eq!(turns[0].items.len(), 4);
        assert!(
            matches!(&turns[0].items[1], Item::Thinking { id, text, .. } if id == "7:0" && text == "use ls")
        );
        assert!(matches!(
            &turns[0].items[2],
            Item::ToolCall(ToolCall { status: ToolStatus::Completed, detail: turn::ToolDetail::Command { output: Some(out), .. }, .. }) if out == "a.rs"
        ));
        assert!(
            matches!(&turns[0].items[3], Item::AgentMessage { text, .. } if text == "One file.")
        );
        assert_eq!(turns[0].status, turn::Status::Completed);
        assert_eq!(turns[1].id, "20");
        assert_eq!(turns[1].status, turn::Status::Failed);
        assert_eq!(turns[1].error.as_deref(), Some("connection refused"));
    }
}
