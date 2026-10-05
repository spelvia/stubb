//! Maps Pi's RPC session events onto Keel session events.
//!
//! One Keel turn is one prompt: it lasts until Pi reports `agent_settled`,
//! across Pi's own inner turns, retries and compaction. Pi does not name
//! user messages, so a turn is named after the timestamp Pi gives the
//! prompt's user message, which is also stored; the turn starts once Pi
//! echoes that message. Text and thinking
//! blocks stream as agent-message and reasoning items; their final content
//! comes from the assistant's `message_end`. A tool call becomes an item
//! when Pi starts executing it and finishes with `tool_execution_end`.
//! Anything else passes through as [`Event::Raw`].

use std::collections::HashMap;
use std::time::Instant;

use serde_json::Value;

use super::{history, tools};
use crate::session::{self, Event};
use crate::turn::{self, Item, ToolCall, ToolStatus, UserContent};

/// The message of Node's `AbortError`, which Pi records when a run is
/// aborted while a tool executes.
const ABORT_ERROR: &str = "This operation was aborted";

/// Mapping state for one live session.
#[derive(Default)]
pub(super) struct SessionState {
    turn_id: Option<String>,
    /// Keel sent a prompt whose user message Pi has not echoed yet.
    awaiting_prompt: bool,
    /// The turn the last prompt started, until the sender has taken it.
    prompted: Option<String>,
    started: Option<Instant>,
    /// Timestamp of the streaming assistant message, which names its items.
    message: i64,
    /// Running tools: id → (name, arguments, output sent so far).
    tools: HashMap<String, (String, Value, String)>,
    /// How the last assistant message of the turn ended.
    outcome: Option<(turn::Status, Option<String>)>,
    total: turn::TokenCount,
    context_window: Option<i64>,
}

impl SessionState {
    pub(super) fn set_context_window(&mut self, context_window: Option<i64>) {
        self.context_window = context_window;
    }

    /// Keel is about to send a prompt. False while another turn runs.
    pub(super) fn prompt_sent(&mut self) -> bool {
        if self.turn_id.is_some() || self.awaiting_prompt {
            return false;
        }
        self.awaiting_prompt = true;
        true
    }

    /// Pi rejected the prompt, so no turn starts.
    pub(super) fn prompt_failed(&mut self) {
        self.awaiting_prompt = false;
    }

    /// Pi handled the prompt itself, for example as an extension command,
    /// so no run follows: the turn starts and ends at once.
    pub(super) fn prompt_handled(&mut self, turn_id: &str, text: &str) -> Vec<Event> {
        self.awaiting_prompt = false;
        let mut events = self.begin(turn_id.to_owned());
        events.push(user_message(
            turn_id,
            vec![UserContent::Text {
                text: text.to_owned(),
            }],
        ));
        events.extend(self.end());
        events
    }

    /// The id of the turn the last prompt started, once.
    pub(super) fn take_prompted(&mut self) -> Option<String> {
        self.prompted.take()
    }

    /// Pi asked for user input through an extension dialog and Keel declined.
    pub(super) fn dialog_declined(&self, method: &str, title: Option<&str>) -> Vec<Event> {
        vec![Event::RequestDeclined {
            method: format!("extension_ui:{method}"),
            turn_id: self.turn_id.clone(),
            summary: title.map(str::to_owned),
        }]
    }

    /// One session event from Pi's stdout.
    pub(super) fn event(&mut self, record: &Value) -> Vec<Event> {
        let kind = record.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            // A run Keel did not start, for example one an extension began.
            "agent_start" if self.turn_id.is_none() && !self.awaiting_prompt => {
                self.begin(uuid::Uuid::new_v4().to_string())
            }
            "agent_start" | "agent_end" | "turn_start" | "turn_end" => Vec::new(),
            "agent_settled" if self.turn_id.is_some() => self.end(),
            "agent_settled" => Vec::new(),
            "message_start" => {
                let message = record.get("message").unwrap_or(&Value::Null);
                match role(message) {
                    Some("assistant") => {
                        self.message = message
                            .get("timestamp")
                            .and_then(Value::as_i64)
                            .unwrap_or(0);
                        Vec::new()
                    }
                    Some("user") if self.awaiting_prompt => self.prompt_started(message),
                    _ => Vec::new(),
                }
            }
            "message_update" => {
                self.update(record.get("assistantMessageEvent").unwrap_or(&Value::Null))
            }
            "message_end" => {
                let message = record.get("message").unwrap_or(&Value::Null);
                if role(message) == Some("assistant") {
                    self.assistant_end(message)
                } else {
                    Vec::new()
                }
            }
            "tool_execution_start" => self.tool_start(record),
            "tool_execution_update" => self.tool_update(record),
            "tool_execution_end" => self.tool_end(record),
            "auto_retry_start" => vec![Event::Error {
                turn_id: self.turn_id.clone(),
                message: text(record, "errorMessage").to_owned(),
                retrying: true,
            }],
            _ => vec![Event::Raw {
                name: format!("pi/{kind}"),
                payload: record.clone(),
            }],
        }
    }

    /// Pi echoed the user message of Keel's prompt: the turn starts.
    fn prompt_started(&mut self, message: &Value) -> Vec<Event> {
        self.awaiting_prompt = false;
        let id = turn_id(message);
        self.prompted = Some(id.clone());
        let content = history::user_content(message.get("content").unwrap_or(&Value::Null));
        let mut events = self.begin(id.clone());
        events.push(user_message(&id, content));
        events
    }

    fn turn(&self) -> String {
        self.turn_id.clone().unwrap_or_default()
    }

    fn begin(&mut self, turn_id: String) -> Vec<Event> {
        self.turn_id = Some(turn_id.clone());
        self.started = Some(Instant::now());
        self.outcome = None;
        vec![
            Event::Status {
                status: session::Status::Active,
            },
            Event::TurnStart { turn_id },
        ]
    }

    fn end(&mut self) -> Vec<Event> {
        let (status, error) = self
            .outcome
            .take()
            .unwrap_or((turn::Status::Completed, None));
        let duration_ms = self
            .started
            .take()
            .map(|started| started.elapsed().as_millis() as i64);
        let id = self.turn_id.take().unwrap_or_default();
        self.tools.clear();
        vec![
            Event::Status {
                status: session::Status::Idle,
            },
            Event::TurnEnd {
                turn: turn::Turn {
                    id,
                    status,
                    items: Vec::new(),
                    started_at: None,
                    completed_at: None,
                    duration_ms,
                    error,
                },
            },
        ]
    }

    fn item_id(&self, update: &Value) -> String {
        let index = update
            .get("contentIndex")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        format!("{}:{index}", self.message)
    }

    /// Streaming content of the assistant message.
    fn update(&mut self, update: &Value) -> Vec<Event> {
        let turn_id = self.turn();
        let item_id = self.item_id(update);
        let delta = || text(update, "delta").to_owned();
        let event = match update.get("type").and_then(Value::as_str) {
            Some("text_start") => Event::ItemStart {
                turn_id,
                item: Item::AgentMessage {
                    id: item_id,
                    text: String::new(),
                },
            },
            Some("thinking_start") => Event::ItemStart {
                turn_id,
                item: Item::Reasoning {
                    id: item_id,
                    summary: Vec::new(),
                },
            },
            Some("text_delta") => Event::ItemDelta {
                turn_id,
                item_id,
                delta: turn::Delta::Text { text: delta() },
            },
            Some("thinking_delta") => Event::ItemDelta {
                turn_id,
                item_id,
                delta: turn::Delta::Reasoning { text: delta() },
            },
            // Tool calls become items when they run; block ends are taken
            // from the final message.
            _ => return Vec::new(),
        };
        vec![event]
    }

    /// The final assistant message: completes its text and thinking items
    /// and reports usage.
    fn assistant_end(&mut self, message: &Value) -> Vec<Event> {
        let turn_id = self.turn();
        let timestamp = message
            .get("timestamp")
            .and_then(Value::as_i64)
            .unwrap_or(self.message);
        let blocks = message.get("content").and_then(Value::as_array);
        let mut events = Vec::new();
        for (index, block) in blocks.into_iter().flatten().enumerate() {
            let id = format!("{timestamp}:{index}");
            let item = match block.get("type").and_then(Value::as_str) {
                Some("text") => Item::AgentMessage {
                    id,
                    text: text(block, "text").trim().to_owned(),
                },
                Some("thinking") => {
                    let thinking = text(block, "thinking").trim();
                    Item::Reasoning {
                        id,
                        summary: if thinking.is_empty() {
                            Vec::new()
                        } else {
                            vec![thinking.to_owned()]
                        },
                    }
                }
                _ => continue,
            };
            events.push(Event::ItemEnd {
                turn_id: turn_id.clone(),
                item,
            });
        }

        self.outcome = Some(outcome(message));
        // An aborted request reports no usage.
        if let Some(usage) = message
            .get("usage")
            .filter(|usage| usage.get("totalTokens").and_then(Value::as_i64) != Some(0))
        {
            events.push(self.usage(turn_id, usage));
        }
        events
    }

    fn usage(&mut self, turn_id: String, usage: &Value) -> Event {
        let count = |key: &str| usage.get(key).and_then(Value::as_i64).unwrap_or(0);
        let cached = count("cacheRead");
        let input = count("input") + cached + count("cacheWrite");
        let output = count("output");
        let last = turn::TokenCount {
            input,
            cached_input: cached,
            output,
            reasoning_output: count("reasoning"),
            total: input + output,
        };
        self.total = turn::TokenCount {
            input: self.total.input + last.input,
            cached_input: self.total.cached_input + last.cached_input,
            output: self.total.output + last.output,
            reasoning_output: self.total.reasoning_output + last.reasoning_output,
            total: self.total.total + last.total,
        };
        Event::Usage {
            turn_id,
            usage: turn::Usage {
                last,
                total: self.total,
                context_window: self.context_window,
            },
        }
    }

    fn tool_start(&mut self, record: &Value) -> Vec<Event> {
        let id = text(record, "toolCallId").to_owned();
        let name = text(record, "toolName").to_owned();
        let args = record.get("args").cloned().unwrap_or(Value::Null);
        let item = Item::ToolCall(ToolCall {
            id: id.clone(),
            status: ToolStatus::InProgress,
            duration_ms: None,
            detail: tools::detail(&name, &args, None, false),
        });
        self.tools.insert(id, (name, args, String::new()));
        vec![Event::ItemStart {
            turn_id: self.turn(),
            item,
        }]
    }

    /// Partial output of a running command. Pi reports the output so far;
    /// Keel streams what was added.
    fn tool_update(&mut self, record: &Value) -> Vec<Event> {
        let turn_id = self.turn();
        let id = text(record, "toolCallId");
        let Some((name, _, sent)) = self.tools.get_mut(id) else {
            return Vec::new();
        };
        let output = record
            .get("partialResult")
            .and_then(tools::output_text)
            .unwrap_or_default();
        if name != "bash" || output.len() <= sent.len() || !output.starts_with(sent.as_str()) {
            return Vec::new();
        }
        let delta = output[sent.len()..].to_owned();
        *sent = output;
        vec![Event::ItemDelta {
            turn_id,
            item_id: id.to_owned(),
            delta: turn::Delta::Output { text: delta },
        }]
    }

    fn tool_end(&mut self, record: &Value) -> Vec<Event> {
        let id = text(record, "toolCallId").to_owned();
        let (name, args) = match self.tools.remove(&id) {
            Some((name, args, _)) => (name, args),
            None => (
                text(record, "toolName").to_owned(),
                record.get("args").cloned().unwrap_or(Value::Null),
            ),
        };
        let failed = record.get("isError").and_then(Value::as_bool) == Some(true);
        vec![Event::ItemEnd {
            turn_id: self.turn(),
            item: Item::ToolCall(ToolCall {
                id,
                status: if failed {
                    ToolStatus::Failed
                } else {
                    ToolStatus::Completed
                },
                duration_ms: None,
                detail: tools::detail(&name, &args, record.get("result"), failed),
            }),
        }]
    }
}

/// How an assistant message ended the turn, with the error of a failed one.
/// Pi reports an abort during a tool call as an error with the platform's
/// abort message, so that counts as interrupted too.
pub(super) fn outcome(message: &Value) -> (turn::Status, Option<String>) {
    let error = message.get("errorMessage").and_then(Value::as_str);
    match message.get("stopReason").and_then(Value::as_str) {
        Some("aborted") => (turn::Status::Interrupted, None),
        Some("error") if error == Some(ABORT_ERROR) => (turn::Status::Interrupted, None),
        Some("error") => (
            turn::Status::Failed,
            Some(error.unwrap_or("the turn failed").to_owned()),
        ),
        _ => (turn::Status::Completed, None),
    }
}

/// The turn a user message starts is named after the message's timestamp.
pub(super) fn turn_id(message: &Value) -> String {
    message
        .get("timestamp")
        .and_then(Value::as_i64)
        .unwrap_or_default()
        .to_string()
}

fn user_message(turn_id: &str, content: Vec<UserContent>) -> Event {
    Event::ItemEnd {
        turn_id: turn_id.to_owned(),
        item: Item::UserMessage {
            id: turn_id.to_owned(),
            content,
        },
    }
}

fn role(message: &Value) -> Option<&str> {
    message.get("role").and_then(Value::as_str)
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Sends a prompt and has Pi echo its user message.
    fn prompt(state: &mut SessionState, timestamp: i64) {
        assert!(state.prompt_sent());
        state.event(&json!({ "type": "message_start",
            "message": { "role": "user", "timestamp": timestamp, "content": "hi" } }));
        assert_eq!(state.take_prompted(), Some(timestamp.to_string()));
    }

    fn update(state: &mut SessionState, update: Value) -> Vec<Event> {
        state.event(&json!({ "type": "message_update", "assistantMessageEvent": update }))
    }

    #[test]
    fn maps_a_prompt_with_a_tool_call() {
        let mut state = SessionState::default();
        state.set_context_window(Some(131_072));
        assert!(state.prompt_sent());
        // The run starts before Pi echoes the prompt; the turn waits for it.
        assert!(state.event(&json!({ "type": "agent_start" })).is_empty());
        assert!(!state.prompt_sent());
        let started = state.event(&json!({ "type": "message_start",
            "message": { "role": "user", "timestamp": 5, "content": [{ "type": "text", "text": "list files" }] } }));
        assert_eq!(state.take_prompted().as_deref(), Some("5"));
        assert!(matches!(&started[1], Event::TurnStart { turn_id } if turn_id == "5"));
        assert!(
            matches!(&started[2], Event::ItemEnd { item: Item::UserMessage { id, content }, .. } if id == "5" && content.len() == 1)
        );

        state.event(&json!({ "type": "message_start", "message": { "role": "assistant", "timestamp": 7, "content": [] } }));
        let opened = update(
            &mut state,
            json!({ "type": "thinking_start", "contentIndex": 0 }),
        );
        assert!(
            matches!(&opened[0], Event::ItemStart { item: Item::Reasoning { id, .. }, .. } if id == "7:0")
        );
        let delta = update(
            &mut state,
            json!({ "type": "thinking_delta", "contentIndex": 0, "delta": "use ls" }),
        );
        assert!(matches!(
            &delta[0],
            Event::ItemDelta { delta: turn::Delta::Reasoning { text }, turn_id, .. } if text == "use ls" && turn_id == "5"
        ));
        assert!(update(&mut state, json!({ "type": "toolcall_start", "contentIndex": 1, "id": "call1", "toolName": "bash" })).is_empty());

        let ended = state.event(&json!({ "type": "message_end", "message": {
            "role": "assistant", "timestamp": 7, "stopReason": "toolUse",
            "content": [
                { "type": "thinking", "thinking": "use ls\n" },
                { "type": "toolCall", "id": "call1", "name": "bash", "arguments": { "command": "ls" } }
            ],
            "usage": { "input": 100, "output": 20, "cacheRead": 50, "cacheWrite": 0, "reasoning": 5 },
        } }));
        assert!(
            matches!(&ended[0], Event::ItemEnd { item: Item::Reasoning { summary, .. }, .. } if summary[0] == "use ls")
        );
        assert!(matches!(
            &ended[1],
            Event::Usage { usage, .. } if usage.last.input == 150 && usage.last.total == 170 && usage.context_window == Some(131_072)
        ));

        let tool = state.event(&json!({ "type": "tool_execution_start", "toolCallId": "call1", "toolName": "bash", "args": { "command": "ls" } }));
        assert!(matches!(
            &tool[0],
            Event::ItemStart { item: Item::ToolCall(ToolCall { status: ToolStatus::InProgress, detail: turn::ToolDetail::Command { command, .. }, .. }), .. } if command == "ls"
        ));
        let partial = |text: &str| {
            json!({ "type": "tool_execution_update", "toolCallId": "call1", "toolName": "bash",
            "partialResult": { "content": [{ "type": "text", "text": text }] } })
        };
        state.event(&partial("a.rs\n"));
        let more = state.event(&partial("a.rs\nb.rs\n"));
        assert!(
            matches!(&more[0], Event::ItemDelta { delta: turn::Delta::Output { text }, .. } if text == "b.rs\n")
        );
        let done = state.event(&json!({ "type": "tool_execution_end", "toolCallId": "call1", "toolName": "bash", "isError": false,
            "result": { "content": [{ "type": "text", "text": "a.rs\nb.rs\n" }], "structuredContent": { "exit_code": 0 } } }));
        assert!(matches!(
            &done[0],
            Event::ItemEnd {
                item: Item::ToolCall(ToolCall {
                    status: ToolStatus::Completed,
                    detail: turn::ToolDetail::Command {
                        exit_code: Some(0),
                        ..
                    },
                    ..
                }),
                ..
            }
        ));

        state.event(&json!({ "type": "message_end", "message": { "role": "assistant", "timestamp": 8, "stopReason": "stop",
            "content": [{ "type": "text", "text": "\n\nTwo files." }] } }));
        assert!(
            state
                .event(&json!({ "type": "agent_end", "messages": [] }))
                .is_empty()
        );
        let end = state.event(&json!({ "type": "agent_settled" }));
        assert!(matches!(
            &end[0],
            Event::Status {
                status: session::Status::Idle
            }
        ));
        assert!(
            matches!(&end[1], Event::TurnEnd { turn } if turn.id == "5" && turn.status == turn::Status::Completed)
        );
    }

    #[test]
    fn aborted_and_failed_turns() {
        let mut state = SessionState::default();
        prompt(&mut state, 1);
        state.event(&json!({ "type": "message_end", "message": { "role": "assistant", "stopReason": "aborted", "content": [] } }));
        let end = state.event(&json!({ "type": "agent_settled" }));
        assert!(
            matches!(&end[1], Event::TurnEnd { turn } if turn.status == turn::Status::Interrupted)
        );

        prompt(&mut state, 2);
        state.event(
            &json!({ "type": "message_end", "message": { "role": "assistant", "stopReason": "error",
            "errorMessage": "connection refused", "content": [] } }),
        );
        let end = state.event(&json!({ "type": "agent_settled" }));
        assert!(matches!(
            &end[1],
            Event::TurnEnd { turn } if turn.status == turn::Status::Failed && turn.error.as_deref() == Some("connection refused")
        ));

        prompt(&mut state, 3);
        state.event(&json!({ "type": "message_end", "message": { "role": "assistant", "stopReason": "error",
            "errorMessage": "This operation was aborted", "content": [], "usage": { "totalTokens": 0 } } }));
        let end = state.event(&json!({ "type": "agent_settled" }));
        assert!(
            matches!(&end[1], Event::TurnEnd { turn } if turn.status == turn::Status::Interrupted && turn.error.is_none())
        );

        assert!(state.prompt_sent());
        state.prompt_failed();
        let handled = state.prompt_handled("t4", "/command");
        assert!(matches!(&handled[1], Event::TurnStart { turn_id } if turn_id == "t4"));
        assert!(
            matches!(&handled[4], Event::TurnEnd { turn } if turn.id == "t4" && turn.status == turn::Status::Completed)
        );
    }

    #[test]
    fn unknown_events_pass_through() {
        let mut state = SessionState::default();
        let raw = state.event(&json!({ "type": "compaction_start", "reason": "threshold" }));
        assert!(matches!(&raw[0], Event::Raw { name, .. } if name == "pi/compaction_start"));
    }
}
