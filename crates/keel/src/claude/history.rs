//! Groups a stored Claude Code transcript into turns.
//!
//! A transcript is a flat list of entries. A turn starts at each user entry
//! that carries the user's own text; tool results are user entries too and
//! complete the matching tool call. The turn id is the uuid of that first
//! user entry, which is also what live turns use. Side chains (sub-agents)
//! and injected meta messages are skipped.

use serde_json::Value;

use super::store::is_main;
use super::tools;
use crate::files::timestamp_millis;
use crate::turn::{self, Item, ToolCall, ToolStatus, UserContent};

pub(super) fn turns(entries: &[Value]) -> Vec<turn::Turn> {
    let mut turns: Vec<turn::Turn> = Vec::new();
    // Start and latest timestamp of the current turn, in milliseconds.
    let mut span: Option<(i64, i64)> = None;
    for entry in entries {
        let kind = entry.get("type").and_then(Value::as_str);
        if !matches!(kind, Some("user" | "assistant")) || !is_main(entry) {
            continue;
        }
        let uuid = entry
            .get("uuid")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let at = entry
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(timestamp_millis);
        let content = entry
            .get("message")
            .and_then(|message| message.get("content"))
            .cloned()
            .unwrap_or(Value::Null);
        let texts = if kind == Some("user") {
            user_texts(&content)
        } else {
            Vec::new()
        };
        let starts_turn = !texts.is_empty() || turns.is_empty();
        if starts_turn {
            close_turn(turns.last_mut(), span);
            turns.push(new_turn(uuid));
            span = at.map(|at| (at, at));
        } else if let (Some((_, latest)), Some(at)) = (&mut span, at) {
            *latest = at;
        }
        let turn = turns.last_mut().expect("a turn exists");
        match kind {
            Some("user") if !texts.is_empty() => turn.items.push(Item::UserMessage {
                id: uuid.to_owned(),
                content: texts,
            }),
            Some("user") => complete_tools(turn, &content),
            _ => assistant_items(turn, uuid, &content),
        }
    }
    close_turn(turns.last_mut(), span);
    turns
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

/// The user's own text; empty for messages that only carry tool results.
fn user_texts(content: &Value) -> Vec<UserContent> {
    match content {
        Value::String(text) => vec![UserContent::Text { text: text.clone() }],
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| match block.get("type").and_then(Value::as_str) {
                Some("text") => Some(UserContent::Text {
                    text: block
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                }),
                Some("image") => Some(UserContent::Image {
                    source: "inline image".to_owned(),
                }),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn assistant_items(turn: &mut turn::Turn, uuid: &str, content: &Value) {
    let blocks = content.as_array().into_iter().flatten();
    for (index, block) in blocks.enumerate() {
        let id = format!("{uuid}:{index}");
        let text = |key: &str| block.get(key).and_then(Value::as_str).unwrap_or_default();
        match block.get("type").and_then(Value::as_str) {
            Some("text") => turn.items.push(Item::AgentMessage {
                id,
                text: text("text").to_owned(),
            }),
            Some("thinking") if !text("thinking").is_empty() => turn.items.push(Item::Thinking {
                id,
                text: text("thinking").to_owned(),
                summarized: false,
            }),
            Some("tool_use") => {
                let input = block.get("input").cloned().unwrap_or(Value::Null);
                turn.items.push(Item::ToolCall(ToolCall {
                    id: block
                        .get("id")
                        .and_then(Value::as_str)
                        .map_or(id, str::to_owned),
                    status: ToolStatus::InProgress,
                    duration_ms: None,
                    detail: tools::detail(text("name"), &input, None),
                }));
            }
            _ => {}
        }
    }
}

/// Fills in the tool calls a tool-result message answers.
fn complete_tools(turn: &mut turn::Turn, content: &Value) {
    for block in content.as_array().into_iter().flatten() {
        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
            continue;
        };
        let failed = block.get("is_error").and_then(Value::as_bool) == Some(true);
        let output = block.get("content").and_then(tools::result_text);
        for item in &mut turn.items {
            if let Item::ToolCall(call) = item
                && call.id == id
            {
                call.status = if failed {
                    ToolStatus::Failed
                } else {
                    ToolStatus::Completed
                };
                set_output(&mut call.detail, output.clone());
            }
        }
    }
}

fn set_output(detail: &mut turn::ToolDetail, value: Option<String>) {
    match detail {
        turn::ToolDetail::Command { output, .. } | turn::ToolDetail::Other { output, .. } => {
            *output = value;
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn entry(kind: &str, uuid: &str, content: Value) -> Value {
        json!({
            "type": kind,
            "uuid": uuid,
            "timestamp": "2026-10-01T18:00:00.000Z",
            "message": { "content": content },
        })
    }

    #[test]
    fn groups_a_transcript_into_turns() {
        let turns = turns(&[
            entry("user", "u1", json!("list files")),
            entry(
                "assistant",
                "a1",
                json!([
                    { "type": "thinking", "thinking": "use ls" },
                    { "type": "tool_use", "id": "tool1", "name": "Bash", "input": { "command": "ls" } }
                ]),
            ),
            entry(
                "user",
                "r1",
                json!([{ "type": "tool_result", "tool_use_id": "tool1", "content": "a.rs" }]),
            ),
            entry(
                "assistant",
                "a2",
                json!([{ "type": "text", "text": "One file." }]),
            ),
            entry("user", "u2", json!([{ "type": "text", "text": "thanks" }])),
            json!({ "type": "assistant", "uuid": "side", "isSidechain": true, "message": { "content": [] } }),
        ]);

        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].id, "u1");
        assert_eq!(turns[0].items.len(), 4);
        assert!(matches!(
            &turns[0].items[2],
            Item::ToolCall(ToolCall { status: ToolStatus::Completed, detail: turn::ToolDetail::Command { output: Some(out), .. }, .. }) if out == "a.rs"
        ));
        assert!(
            matches!(&turns[0].items[3], Item::AgentMessage { text, .. } if text == "One file.")
        );
        assert_eq!(turns[1].id, "u2");
    }
}
