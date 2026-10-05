//! Maps Claude Code's stream-json output onto session events.
//!
//! The SDK streams raw API events (`stream_event`): content blocks open,
//! receive deltas and close. Text and thinking become agent-message and
//! thinking items; a tool call becomes a tool item once its input is
//! complete and finishes when the matching `tool_result` arrives. Full
//! `assistant` messages repeat streamed content and are only used when a
//! message was not streamed. Messages from sub-agents are skipped.

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use super::tools;
use crate::session::{self, Event};
use crate::turn::{self, Item, ToolCall, ToolStatus, UserContent};

/// Mapping state for one live session.
#[derive(Default)]
pub(super) struct SessionState {
    turn_id: Option<String>,
    /// API message currently streaming, and every message seen streaming.
    message_id: Option<String>,
    streamed: HashSet<String>,
    /// Open content blocks of the streaming message, by index.
    blocks: HashMap<u64, Block>,
    /// Tool calls waiting for their result: id → (name, input).
    tools: HashMap<String, (String, Value)>,
    declined: HashSet<String>,
    total: turn::TokenCount,
}

struct Block {
    id: String,
    kind: BlockKind,
    buffer: String,
}

enum BlockKind {
    Text,
    Thinking,
    Tool { name: String },
}

impl SessionState {
    /// A user message Keel sent; `turn_id` is its uuid.
    pub(super) fn turn_started(&mut self, turn_id: &str, text: &str) -> Vec<Event> {
        self.turn_id = Some(turn_id.to_owned());
        let item = Item::UserMessage {
            id: turn_id.to_owned(),
            content: vec![UserContent::Text {
                text: text.to_owned(),
            }],
        };
        vec![
            Event::Status {
                status: session::Status::Active,
            },
            Event::TurnStart {
                turn_id: turn_id.to_owned(),
            },
            Event::ItemEnd {
                turn_id: turn_id.to_owned(),
                item,
            },
        ]
    }

    /// A turn that was announced but could not be delivered.
    pub(super) fn turn_failed(&mut self, turn_id: &str, message: &str) -> Vec<Event> {
        self.turn_id = None;
        vec![
            Event::Status {
                status: session::Status::Idle,
            },
            Event::TurnEnd {
                turn: turn::Turn {
                    id: turn_id.to_owned(),
                    status: turn::Status::Failed,
                    items: Vec::new(),
                    started_at: None,
                    completed_at: None,
                    duration_ms: None,
                    error: Some(message.to_owned()),
                },
            },
        ]
    }

    /// Claude Code asked to use a tool and Keel declined.
    pub(super) fn tool_declined(
        &mut self,
        tool: &str,
        tool_use_id: Option<&str>,
        input: &Value,
    ) -> Vec<Event> {
        if let Some(id) = tool_use_id {
            self.declined.insert(id.to_owned());
        }
        let command = input.get("command").and_then(Value::as_str);
        vec![Event::RequestDeclined {
            method: format!("can_use_tool:{tool}"),
            turn_id: self.turn_id.clone(),
            summary: Some(command.unwrap_or(tool).to_owned()),
        }]
    }

    /// One line of Claude Code output.
    pub(super) fn message(&mut self, message: &Value) -> Vec<Event> {
        if message
            .get("parent_tool_use_id")
            .is_some_and(|id| !id.is_null())
        {
            return Vec::new();
        }
        if let Some(turn_id) = text(message, "user_message_uuid") {
            self.turn_id = Some(turn_id);
        }
        match message.get("type").and_then(Value::as_str) {
            Some("stream_event") => self.stream(message.get("event").unwrap_or(&Value::Null)),
            Some("assistant") => self.assistant(message),
            Some("user") => self.tool_results(message),
            Some("result") => self.result(message),
            other => {
                let subtype = text(message, "subtype").map_or(String::new(), |s| format!("/{s}"));
                vec![Event::Raw {
                    name: format!("claude/{}{subtype}", other.unwrap_or("unknown")),
                    payload: message.clone(),
                }]
            }
        }
    }

    fn turn(&self) -> String {
        self.turn_id.clone().unwrap_or_default()
    }

    fn stream(&mut self, event: &Value) -> Vec<Event> {
        let index = event.get("index").and_then(Value::as_u64).unwrap_or(0);
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                let id = event
                    .get("message")
                    .and_then(|message| text(message, "id"))
                    .unwrap_or_default();
                self.streamed.insert(id.clone());
                self.message_id = Some(id);
                self.blocks.clear();
                Vec::new()
            }
            Some("content_block_start") => {
                let block = event.get("content_block").unwrap_or(&Value::Null);
                let item_id = format!("{}:{index}", self.message_id.as_deref().unwrap_or(""));
                let (kind, item) = match block.get("type").and_then(Value::as_str) {
                    Some("text") => (
                        BlockKind::Text,
                        Some(Item::AgentMessage {
                            id: item_id.clone(),
                            text: String::new(),
                        }),
                    ),
                    Some("thinking") => (
                        BlockKind::Thinking,
                        Some(Item::Thinking {
                            id: item_id.clone(),
                            text: String::new(),
                            summarized: false,
                        }),
                    ),
                    Some("tool_use") => (
                        BlockKind::Tool {
                            name: text(block, "name").unwrap_or_default(),
                        },
                        None,
                    ),
                    _ => return Vec::new(),
                };
                let id = match kind {
                    BlockKind::Tool { .. } => text(block, "id").unwrap_or(item_id),
                    _ => item_id,
                };
                self.blocks.insert(
                    index,
                    Block {
                        id,
                        kind,
                        buffer: String::new(),
                    },
                );
                item.map(|item| Event::ItemStart {
                    turn_id: self.turn(),
                    item,
                })
                .into_iter()
                .collect()
            }
            Some("content_block_delta") => {
                let turn_id = self.turn();
                let Some(block) = self.blocks.get_mut(&index) else {
                    return Vec::new();
                };
                let delta = event.get("delta").unwrap_or(&Value::Null);
                let piece = |key: &str| text(delta, key).unwrap_or_default();
                match (&block.kind, delta.get("type").and_then(Value::as_str)) {
                    (BlockKind::Text, Some("text_delta")) => {
                        let delta = piece("text");
                        block.buffer.push_str(&delta);
                        vec![Event::ItemDelta {
                            turn_id,
                            item_id: block.id.clone(),
                            delta: turn::Delta::Text { text: delta },
                        }]
                    }
                    (BlockKind::Thinking, Some("thinking_delta")) => {
                        let delta = piece("thinking");
                        block.buffer.push_str(&delta);
                        vec![Event::ItemDelta {
                            turn_id,
                            item_id: block.id.clone(),
                            delta: turn::Delta::Thinking { text: delta },
                        }]
                    }
                    (BlockKind::Tool { .. }, Some("input_json_delta")) => {
                        block.buffer.push_str(&piece("partial_json"));
                        Vec::new()
                    }
                    _ => Vec::new(),
                }
            }
            Some("content_block_stop") => match self.blocks.remove(&index) {
                Some(block) => self.finish_block(block),
                None => Vec::new(),
            },
            _ => Vec::new(),
        }
    }

    fn finish_block(&mut self, block: Block) -> Vec<Event> {
        let turn_id = self.turn();
        let event = match block.kind {
            BlockKind::Text => Event::ItemEnd {
                turn_id,
                item: Item::AgentMessage {
                    id: block.id,
                    text: block.buffer,
                },
            },
            BlockKind::Thinking => Event::ItemEnd {
                turn_id,
                item: Item::Thinking {
                    id: block.id,
                    text: block.buffer,
                    summarized: false,
                },
            },
            BlockKind::Tool { name } => {
                let input = serde_json::from_str(&block.buffer).unwrap_or_else(|_| json!({}));
                return self.tool_started(block.id, name, input);
            }
        };
        vec![event]
    }

    fn tool_started(&mut self, id: String, name: String, input: Value) -> Vec<Event> {
        let item = Item::ToolCall(ToolCall {
            id: id.clone(),
            status: ToolStatus::InProgress,
            duration_ms: None,
            detail: tools::detail(&name, &input, None),
        });
        self.tools.insert(id, (name, input));
        vec![Event::ItemStart {
            turn_id: self.turn(),
            item,
        }]
    }

    /// Content of a message that was not streamed, such as a synthetic error.
    fn assistant(&mut self, message: &Value) -> Vec<Event> {
        let body = message.get("message").unwrap_or(&Value::Null);
        let message_id = text(body, "id").unwrap_or_default();
        if self.streamed.contains(&message_id) {
            return Vec::new();
        }
        let blocks = body.get("content").and_then(Value::as_array);
        let mut events = Vec::new();
        for (index, block) in blocks.into_iter().flatten().enumerate() {
            let id = format!("{message_id}:{index}");
            match block.get("type").and_then(Value::as_str) {
                Some("text") => events.push(Event::ItemEnd {
                    turn_id: self.turn(),
                    item: Item::AgentMessage {
                        id,
                        text: text(block, "text").unwrap_or_default(),
                    },
                }),
                Some("thinking") => events.push(Event::ItemEnd {
                    turn_id: self.turn(),
                    item: Item::Thinking {
                        id,
                        text: text(block, "thinking").unwrap_or_default(),
                        summarized: false,
                    },
                }),
                Some("tool_use") => events.extend(self.tool_started(
                    text(block, "id").unwrap_or(id),
                    text(block, "name").unwrap_or_default(),
                    block.get("input").cloned().unwrap_or_else(|| json!({})),
                )),
                _ => {}
            }
        }
        events
    }

    fn tool_results(&mut self, message: &Value) -> Vec<Event> {
        let content = message
            .get("message")
            .and_then(|body| body.get("content"))
            .and_then(Value::as_array);
        let mut events = Vec::new();
        for block in content.into_iter().flatten() {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let id = text(block, "tool_use_id").unwrap_or_default();
            let (name, input) = self
                .tools
                .remove(&id)
                .unwrap_or_else(|| ("unknown".to_owned(), Value::Null));
            let failed = block.get("is_error").and_then(Value::as_bool) == Some(true);
            let status = if self.declined.remove(&id) {
                ToolStatus::Declined
            } else if failed {
                ToolStatus::Failed
            } else {
                ToolStatus::Completed
            };
            let output = block.get("content").and_then(tools::result_text);
            events.push(Event::ItemEnd {
                turn_id: self.turn(),
                item: Item::ToolCall(ToolCall {
                    id,
                    status,
                    duration_ms: None,
                    detail: tools::detail(&name, &input, output),
                }),
            });
        }
        events
    }

    fn result(&mut self, message: &Value) -> Vec<Event> {
        let turn_id = self.turn();
        let usage = message.get("usage").unwrap_or(&Value::Null);
        let count = |key: &str| usage.get(key).and_then(Value::as_i64).unwrap_or(0);
        let cached = count("cache_read_input_tokens");
        let input = count("input_tokens") + cached + count("cache_creation_input_tokens");
        let output = count("output_tokens");
        let last = turn::TokenCount {
            input,
            cached_input: cached,
            output,
            reasoning_output: 0,
            total: input + output,
        };
        self.total = turn::TokenCount {
            input: self.total.input + last.input,
            cached_input: self.total.cached_input + last.cached_input,
            output: self.total.output + last.output,
            reasoning_output: 0,
            total: self.total.total + last.total,
        };
        let context_window = message
            .get("modelUsage")
            .and_then(Value::as_object)
            .and_then(|models| {
                models
                    .values()
                    .filter_map(|model| model.get("contextWindow").and_then(Value::as_i64))
                    .max()
            });

        let failed = message.get("is_error").and_then(Value::as_bool) == Some(true)
            || text(message, "subtype").as_deref() != Some("success");
        let interrupted =
            text(message, "terminal_reason").is_some_and(|reason| reason.starts_with("aborted"));
        let status = if interrupted {
            turn::Status::Interrupted
        } else if failed {
            turn::Status::Failed
        } else {
            turn::Status::Completed
        };
        let error = (status == turn::Status::Failed).then(|| {
            message
                .get("errors")
                .and_then(Value::as_array)
                .map(|errors| {
                    errors
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join("; ")
                })
                .filter(|errors| !errors.is_empty())
                .or_else(|| text(message, "result"))
                .unwrap_or_else(|| "the turn failed".to_owned())
        });

        self.turn_id = None;
        self.blocks.clear();
        vec![
            Event::Usage {
                turn_id: turn_id.clone(),
                usage: turn::Usage {
                    last,
                    total: self.total,
                    context_window,
                },
            },
            Event::Status {
                status: session::Status::Idle,
            },
            Event::TurnEnd {
                turn: turn::Turn {
                    id: turn_id,
                    status,
                    items: Vec::new(),
                    started_at: None,
                    completed_at: None,
                    duration_ms: message.get("duration_ms").and_then(Value::as_i64),
                    error,
                },
            },
        ]
    }
}

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(state: &mut SessionState, message: Value) -> Vec<Event> {
        state.message(&message)
    }

    fn stream(state: &mut SessionState, event: Value) -> Vec<Event> {
        message(
            state,
            json!({ "type": "stream_event", "parent_tool_use_id": null, "event": event }),
        )
    }

    #[test]
    fn maps_a_streamed_turn_with_a_tool_call() {
        let mut handler = SessionState::default();
        let started = handler.turn_started("t1", "list files");
        assert!(matches!(&started[1], Event::TurnStart { turn_id } if turn_id == "t1"));

        stream(
            &mut handler,
            json!({ "type": "message_start", "message": { "id": "m1" } }),
        );
        let opened = stream(
            &mut handler,
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text" } }),
        );
        assert!(
            matches!(&opened[0], Event::ItemStart { item: Item::AgentMessage { id, .. }, .. } if id == "m1:0")
        );
        let delta = stream(
            &mut handler,
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "Listing" } }),
        );
        assert!(
            matches!(&delta[0], Event::ItemDelta { delta: turn::Delta::Text { text }, turn_id, .. } if text == "Listing" && turn_id == "t1")
        );
        let done = stream(
            &mut handler,
            json!({ "type": "content_block_stop", "index": 0 }),
        );
        assert!(
            matches!(&done[0], Event::ItemEnd { item: Item::AgentMessage { text, .. }, .. } if text == "Listing")
        );

        stream(
            &mut handler,
            json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "tool_use", "id": "tool1", "name": "Bash" } }),
        );
        stream(
            &mut handler,
            json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "input_json_delta", "partial_json": "{\"command\":\"ls\"}" } }),
        );
        let tool = stream(
            &mut handler,
            json!({ "type": "content_block_stop", "index": 1 }),
        );
        assert!(matches!(
            &tool[0],
            Event::ItemStart { item: Item::ToolCall(ToolCall { status: ToolStatus::InProgress, detail: turn::ToolDetail::Command { command, .. }, .. }), .. } if command == "ls"
        ));

        // The full assistant message repeats streamed content and is ignored.
        assert!(
            message(
                &mut handler,
                json!({ "type": "assistant", "message": { "id": "m1", "content": [] } })
            )
            .is_empty()
        );

        handler.tool_declined("Bash", Some("tool1"), &json!({ "command": "ls" }));
        let result = message(
            &mut handler,
            json!({ "type": "user", "message": { "content": [{ "type": "tool_result", "tool_use_id": "tool1", "is_error": true, "content": "declined" }] } }),
        );
        assert!(matches!(
            &result[0],
            Event::ItemEnd {
                item: Item::ToolCall(ToolCall {
                    status: ToolStatus::Declined,
                    ..
                }),
                ..
            }
        ));

        let end = message(
            &mut handler,
            json!({ "type": "result", "subtype": "success", "is_error": false, "duration_ms": 1200,
                    "terminal_reason": "completed",
                    "usage": { "input_tokens": 3, "cache_read_input_tokens": 10, "cache_creation_input_tokens": 0, "output_tokens": 5 },
                    "modelUsage": { "claude-sonnet-4-6": { "contextWindow": 200000 } } }),
        );
        assert!(
            matches!(&end[0], Event::Usage { usage, .. } if usage.last.total == 18 && usage.context_window == Some(200_000))
        );
        assert!(
            matches!(&end[2], Event::TurnEnd { turn } if turn.id == "t1" && turn.status == turn::Status::Completed)
        );
    }

    #[test]
    fn a_turn_that_failed_to_send_ends_failed() {
        let mut handler = SessionState::default();
        handler.turn_started("t1", "hi");
        let end = handler.turn_failed("t1", "claude exited");
        assert!(matches!(
            &end[1],
            Event::TurnEnd { turn } if turn.id == "t1"
                && turn.status == turn::Status::Failed
                && turn.error.as_deref() == Some("claude exited")
        ));
    }

    #[test]
    fn interrupted_and_subagent_messages() {
        let mut handler = SessionState::default();
        let end = message(
            &mut handler,
            json!({ "type": "result", "subtype": "error_during_execution", "is_error": true, "terminal_reason": "aborted_streaming", "usage": {} }),
        );
        assert!(
            matches!(&end[2], Event::TurnEnd { turn } if turn.status == turn::Status::Interrupted)
        );
        assert!(
            message(
                &mut handler,
                json!({ "type": "assistant", "parent_tool_use_id": "task1", "message": {} })
            )
            .is_empty()
        );
    }
}
