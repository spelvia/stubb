//! Maps Claude Code's stream-json output onto session events.
//!
//! The SDK streams raw API events (`stream_event`): content blocks open,
//! receive deltas and close. Text and thinking become agent-message and
//! thinking items; a tool call becomes a tool item once its input is
//! complete and finishes when the matching `tool_result` arrives. Full
//! `assistant` messages repeat streamed content and are only used when a
//! message was not streamed. Messages from sub-agents are skipped.
//!
//! Usage is reported after each model call, from its `message_delta`. The
//! `result` at the end of a turn adds up every call of the turn, so it only
//! feeds the running total: as "the last call" it would overstate how full
//! the context is many times over.

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use super::tools;
use crate::request;
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
    /// Every call of earlier turns, as their `result`s reported them.
    total: turn::TokenCount,
    /// The streaming call's usage from its `message_start`, for counts its
    /// `message_delta`s leave out, and the usage last reported for it.
    call_start: Option<Value>,
    call: Option<turn::TokenCount>,
    /// This turn's calls so far, and the latest of them.
    turn_calls: turn::TokenCount,
    last_call: Option<turn::TokenCount>,
    context_window: Option<i64>,
    /// A compaction was reported started and has not ended.
    compacting: bool,
    has_reply: bool,
    /// A task notification arrived and no query has started since: the next
    /// query (its `system/init`) may be Claude Code running it on its own,
    /// as it does on resume for a background task the last process left.
    notification_pending: bool,
    /// The query running now: started for a notification, and whether it
    /// has produced anything.
    query: Query,
    /// The open turn is one Claude Code started without the host's input.
    automatic: bool,
    /// The next `result` ends an automatic turn already ended when the
    /// host's turn began, not the host's turn.
    stale_result: bool,
}

/// One query Claude Code runs, from its `system/init` to its `result`.
#[derive(Default)]
struct Query {
    from_notification: bool,
    output: bool,
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
    /// A native delegated execution. Its spawning tool ID is the turn identity
    /// until Claude reports a separate continuation; no user input is invented.
    pub(super) fn delegated_turn(&mut self, turn_id: &str) -> Vec<Event> {
        self.has_reply = false;
        self.turn_id = Some(turn_id.to_owned());
        vec![Event::TurnStart {
            turn_id: turn_id.to_owned(),
        }]
    }

    /// A user message Stubb sent; `turn_id` is its uuid.
    #[cfg(test)]
    pub(super) fn turn_started(&mut self, turn_id: &str, text: &str) -> Vec<Event> {
        self.turn_started_with(
            turn_id,
            vec![UserContent::Text {
                text: text.to_owned(),
            }],
        )
    }

    /// A user message Stubb sent, with images or other content.
    pub(super) fn turn_started_with(
        &mut self,
        turn_id: &str,
        content: Vec<UserContent>,
    ) -> Vec<Event> {
        // An automatic turn still open ends here: the host's turn takes over,
        // and that turn's own result, still to come, ends nothing.
        let mut ended = Vec::new();
        if self.automatic
            && let Some(open) = self.turn_id.take()
        {
            self.automatic = false;
            self.stale_result = true;
            ended.push(Event::TurnEnd {
                turn: turn::Turn {
                    id: open,
                    status: turn::Status::Completed,
                    items: Vec::new(),
                    started_at: None,
                    completed_at: None,
                    duration_ms: None,
                    error: None,
                },
            });
        }
        self.has_reply = false;
        self.turn_id = Some(turn_id.to_owned());
        let item = Item::UserMessage {
            id: turn_id.to_owned(),
            content,
        };
        ended.extend([
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
        ]);
        ended
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

    /// Claude Code asked to use a tool and Stubb declined.
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

    /// Claude Code asked to use a tool and waits for the host's answer.
    pub(super) fn requested(&mut self, mut request: request::Request) -> Vec<Event> {
        request.turn_id = self.turn_id.clone();
        vec![Event::Request { request }]
    }

    /// A forwarded request is no longer pending. A refused tool call then
    /// ends declined, like one Stubb declined itself.
    pub(super) fn request_resolved(
        &mut self,
        id: &str,
        outcome: request::Outcome,
        tool_use_id: Option<&str>,
    ) -> Vec<Event> {
        if let (request::Outcome::Answered { decision }, Some(tool)) = (&outcome, tool_use_id)
            && decision.is_refusal()
        {
            self.declined.insert(tool.to_owned());
        }
        vec![Event::RequestResolved {
            id: id.to_owned(),
            outcome,
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
        // Claude Code works without the host's input too, such as after a
        // background task ends: that work is a turn of its own.
        let working = matches!(
            message.get("type").and_then(Value::as_str),
            Some("assistant" | "stream_event")
        );
        if working {
            self.query.output = true;
        }
        if working && self.turn_id.is_none() {
            let turn_id =
                text(message, "uuid").unwrap_or_else(|| format!("claude-{}", uuid::Uuid::new_v4()));
            let mut events = self.delegated_turn(&turn_id);
            self.automatic = true;
            events.insert(
                0,
                Event::Status {
                    status: session::Status::Active,
                },
            );
            events.extend(self.content(message));
            return events;
        }
        self.content(message)
    }

    /// The attachment already resolved this forwarded frame's native owner.
    /// Root user UUIDs must not replace the delegated execution's identity.
    pub(super) fn delegated_message(&mut self, message: &Value) -> Vec<Event> {
        self.content(message)
    }

    fn content(&mut self, message: &Value) -> Vec<Event> {
        let events = match message.get("type").and_then(Value::as_str) {
            Some("stream_event") => self.stream(message.get("event").unwrap_or(&Value::Null)),
            Some("assistant") => self.assistant(message),
            Some("user") => self.tool_results(message),
            Some("result") if self.ends_no_turn(message) => vec![raw(message)],
            Some("result") => self.result(message),
            Some("system") => match text(message, "subtype").as_deref() {
                Some("status") => self.status(message),
                Some("task_notification") => {
                    self.notification_pending = true;
                    vec![raw(message)]
                }
                Some("init") => {
                    self.query = Query {
                        from_notification: std::mem::take(&mut self.notification_pending),
                        output: false,
                    };
                    vec![raw(message)]
                }
                Some("compact_boundary") => self.compacted(message),
                _ => vec![raw(message)],
            },
            _ => vec![raw(message)],
        };
        if events.iter().any(|event| matches!(event, Event::ItemEnd { item: Item::AgentMessage { text, .. }, .. } if !text.is_empty())) {
            self.has_reply = true;
        }
        events
    }

    /// Whether a `result` ends no turn of the host's: the result of an
    /// automatic turn the host's turn already ended, or of a query that did
    /// nothing, run for a task notification or with no turn open. A query
    /// that did something always ends its turn.
    fn ends_no_turn(&mut self, message: &Value) -> bool {
        let query = std::mem::take(&mut self.query);
        // Notifications handled within this query are done with.
        self.notification_pending = false;
        if std::mem::take(&mut self.stale_result) {
            // The ended automatic turn's calls still count for the session,
            // and none of them is the host turn's.
            let calls = token_count(message.get("usage").unwrap_or(&Value::Null), None);
            self.total = add(self.total, calls);
            self.turn_calls = turn::TokenCount::default();
            self.call_start = None;
            self.call = None;
            self.last_call = None;
            return true;
        }
        let idle = !query.output && message.get("num_turns").and_then(Value::as_u64) == Some(0);
        idle && (query.from_notification || self.turn_id.is_none())
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
                self.call_start = event
                    .get("message")
                    .and_then(|message| message.get("usage"))
                    .cloned();
                self.call = None;
                Vec::new()
            }
            // A message may have several, each with its call's usage so far.
            Some("message_delta") => match event.get("usage") {
                Some(usage) => {
                    let count = token_count(usage, self.call_start.as_ref());
                    self.model_call(count)
                }
                None => Vec::new(),
            },
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
            detail: tools::detail(&name, &input, None, false),
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
            events.push(Event::ItemEnd {
                turn_id: self.turn(),
                item: Item::ToolCall(ToolCall {
                    id,
                    status,
                    duration_ms: None,
                    detail: tools::detail(&name, &input, block.get("content"), failed),
                }),
            });
        }
        events
    }

    /// A main-agent model call's usage so far; it replaces what the same
    /// call reported before.
    fn model_call(&mut self, last: turn::TokenCount) -> Vec<Event> {
        if last.total == 0 {
            return Vec::new();
        }
        if let Some(earlier) = self.call.replace(last) {
            self.turn_calls = subtract(self.turn_calls, earlier);
        }
        self.last_call = Some(last);
        self.turn_calls = add(self.turn_calls, last);
        vec![Event::Usage {
            turn_id: self.turn(),
            usage: turn::Usage {
                last,
                total: add(self.total, self.turn_calls),
                context_window: self.context_window,
            },
        }]
    }

    /// `system/status`: Claude reports `compacting` when a compaction starts
    /// (and again every 30 seconds while it runs), then a status with its
    /// `compact_result`. A success is reported by the `compact_boundary`
    /// that follows, which has the token counts.
    fn status(&mut self, message: &Value) -> Vec<Event> {
        let status = message.get("status").and_then(Value::as_str);
        let compaction = |status| Event::Compaction {
            turn_id: self.turn_id.clone(),
            compaction: turn::Compaction::new(status),
        };
        match (status, text(message, "compact_result").as_deref()) {
            (Some("compacting"), _) if self.compacting => Vec::new(),
            (Some("compacting"), _) => {
                let event = compaction(turn::CompactionStatus::InProgress);
                self.compacting = true;
                vec![event]
            }
            (_, Some("success")) => Vec::new(),
            (_, Some(_)) => {
                let mut compaction = turn::Compaction::new(turn::CompactionStatus::Failed);
                compaction.error = text(message, "compact_error");
                self.compacting = false;
                vec![Event::Compaction {
                    turn_id: self.turn_id.clone(),
                    compaction,
                }]
            }
            (None, None) if self.compacting => {
                let event = compaction(turn::CompactionStatus::Cancelled);
                self.compacting = false;
                vec![event]
            }
            _ => vec![raw(message)],
        }
    }

    fn compacted(&mut self, message: &Value) -> Vec<Event> {
        let metadata = message.get("compact_metadata").unwrap_or(&Value::Null);
        let mut compaction = turn::Compaction::new(turn::CompactionStatus::Completed);
        compaction.trigger = match text(metadata, "trigger").as_deref() {
            Some("manual") => Some(turn::CompactionTrigger::Manual),
            Some("auto") => Some(turn::CompactionTrigger::Auto),
            Some(_) => Some(turn::CompactionTrigger::Unknown),
            None => None,
        };
        compaction.tokens_before = metadata.get("pre_tokens").and_then(Value::as_i64);
        compaction.tokens_after = metadata.get("post_tokens").and_then(Value::as_i64);
        self.compacting = false;
        vec![Event::Compaction {
            turn_id: self.turn_id.clone(),
            compaction,
        }]
    }

    fn result(&mut self, message: &Value) -> Vec<Event> {
        let turn_id = self.turn();
        // Every model call of the turn, added up.
        let calls = token_count(message.get("usage").unwrap_or(&Value::Null), None);
        self.total = add(self.total, calls);
        self.turn_calls = turn::TokenCount::default();
        // Without a streamed call (Claude Code always streams them), the
        // turn's sum is the closest there is.
        let last = self.last_call.take().unwrap_or(calls);
        let context_window = message
            .get("modelUsage")
            .and_then(Value::as_object)
            .and_then(|models| {
                models
                    .values()
                    .filter_map(|model| model.get("contextWindow").and_then(Value::as_i64))
                    .max()
            })
            .or(self.context_window);
        self.context_window = context_window;

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
        self.automatic = false;
        self.blocks.clear();
        // A turn without model calls, such as `/compact` or one interrupted
        // before its first call, says nothing about the context.
        let usage = (last.total > 0).then(|| Event::Usage {
            turn_id: turn_id.clone(),
            usage: turn::Usage {
                last,
                total: self.total,
                context_window,
            },
        });
        let final_text = message["result"]
            .as_str()
            .filter(|value| !value.trim().is_empty());
        let reply = final_text
            .filter(|_| status == turn::Status::Completed && !self.has_reply)
            .map(|value| Event::ItemEnd {
                turn_id: turn_id.clone(),
                item: Item::AgentMessage {
                    id: format!("{turn_id}:result"),
                    text: value.to_owned(),
                },
            });
        let accounting = message["total_cost_usd"]
            .as_f64()
            .filter(|cost| cost.is_finite() && *cost >= 0.0)
            .map(|cost| Event::TurnAccounting {
                turn_id: turn_id.clone(),
                tokens: (calls.total > 0).then_some(calls.total),
                session_cost_usd: Some(cost),
            });
        reply
            .into_iter()
            .chain(usage)
            .chain(accounting)
            .chain([
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
            ])
            .collect()
    }
}

/// Claude's usage block as a token count. Input includes cache reads and
/// writes, so `input` plus `output` is the context the call used. Counts
/// missing or `null` in `usage` are taken from `fallback`.
fn token_count(usage: &Value, fallback: Option<&Value>) -> turn::TokenCount {
    let count = |key: &str| {
        usage
            .get(key)
            .and_then(Value::as_i64)
            .or_else(|| {
                fallback
                    .and_then(|fallback| fallback.get(key))
                    .and_then(Value::as_i64)
            })
            .unwrap_or(0)
    };
    let cached = count("cache_read_input_tokens");
    let input = count("input_tokens") + cached + count("cache_creation_input_tokens");
    let output = count("output_tokens");
    turn::TokenCount {
        input,
        cached_input: cached,
        output,
        reasoning_output: 0,
        total: input + output,
    }
}

fn add(a: turn::TokenCount, b: turn::TokenCount) -> turn::TokenCount {
    turn::TokenCount {
        input: a.input + b.input,
        cached_input: a.cached_input + b.cached_input,
        output: a.output + b.output,
        reasoning_output: a.reasoning_output + b.reasoning_output,
        total: a.total + b.total,
    }
}

fn subtract(a: turn::TokenCount, b: turn::TokenCount) -> turn::TokenCount {
    turn::TokenCount {
        input: a.input - b.input,
        cached_input: a.cached_input - b.cached_input,
        output: a.output - b.output,
        reasoning_output: a.reasoning_output - b.reasoning_output,
        total: a.total - b.total,
    }
}

fn raw(message: &Value) -> Event {
    let kind = text(message, "type").unwrap_or_else(|| "unknown".to_owned());
    let subtype = text(message, "subtype").map_or(String::new(), |s| format!("/{s}"));
    Event::Raw {
        name: format!("claude/{kind}{subtype}"),
        payload: message.clone(),
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

    fn turn_ends(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::TurnEnd { turn } => Some(turn.id.clone()),
                _ => None,
            })
            .collect()
    }

    /// As Claude Code 2.1.330 resumes a session whose background task the
    /// last process left running: the stopped task's notification runs as a
    /// query of its own before the host's message.
    #[test]
    fn a_stale_task_notification_does_not_end_the_hosts_turn() {
        let mut state = SessionState::default();
        let mut events = state.turn_started("u1", "Reply with just: two");
        for line in [
            json!({ "type": "system", "subtype": "task_notification", "task_id": "b1",
                    "status": "stopped", "summary": "Background shell command didn't finish" }),
            json!({ "type": "system", "subtype": "init" }),
            json!({ "type": "result", "subtype": "success", "num_turns": 0, "result": "",
                    "duration_ms": 46, "is_error": false }),
            json!({ "type": "system", "subtype": "init" }),
            json!({ "type": "assistant", "parent_tool_use_id": null, "uuid": "a1",
                    "message": { "id": "m1", "role": "assistant",
                                 "content": [{ "type": "text", "text": "two" }] } }),
            json!({ "type": "result", "subtype": "success", "num_turns": 1, "result": "two",
                    "duration_ms": 1173, "is_error": false }),
        ] {
            events.extend(message(&mut state, line));
        }
        assert_eq!(turn_ends(&events), ["u1"]);
        assert!(events.iter().any(|event| matches!(
            event,
            Event::ItemEnd { turn_id, item: Item::AgentMessage { text, .. } } if turn_id == "u1" && text == "two"
        )));
    }

    fn assistant(uuid: &str, text: &str) -> Value {
        json!({ "type": "assistant", "parent_tool_use_id": null, "uuid": uuid,
                "message": { "id": format!("m-{uuid}"), "role": "assistant",
                             "content": [{ "type": "text", "text": text }] } })
    }

    #[test]
    fn a_host_turn_ends_an_open_automatic_turn_whose_result_then_ends_nothing() {
        let mut state = SessionState::default();
        let mut events = message(&mut state, assistant("a1", "The task finished."));
        events.extend(state.turn_started("h1", "Next, please"));
        // The automatic turn's own result, then the host's query.
        for line in [
            json!({ "type": "result", "subtype": "success", "num_turns": 1, "result": "",
                    "is_error": false, "usage": { "input_tokens": 100, "output_tokens": 10 } }),
            json!({ "type": "system", "subtype": "init" }),
            assistant("a2", "Done."),
            json!({ "type": "result", "subtype": "success", "num_turns": 1, "result": "Done.",
                    "is_error": false, "usage": { "input_tokens": 20, "output_tokens": 5 } }),
        ] {
            events.extend(message(&mut state, line));
        }
        assert_eq!(turn_ends(&events), ["a1", "h1"]);
        // Both turns' calls count for the session; the last call is the host's.
        let usage = events.iter().rev().find_map(|event| match event {
            Event::Usage { turn_id, usage } if turn_id == "h1" => Some(usage),
            _ => None,
        });
        let usage = usage.expect("the host turn's usage");
        assert_eq!(usage.total.total, 135);
        assert_eq!(usage.last.total, 25);
    }

    #[test]
    fn notifications_handled_in_a_turn_leave_the_next_host_command_its_end() {
        let mut state = SessionState::default();
        let mut events = state.turn_started("u1", "Wait for the tasks");
        for line in [
            json!({ "type": "system", "subtype": "task_notification", "task_id": "b1", "status": "completed" }),
            json!({ "type": "system", "subtype": "task_notification", "task_id": "b2", "status": "completed" }),
            assistant("a1", "Both finished."),
            json!({ "type": "result", "subtype": "success", "num_turns": 2, "result": "Both finished.",
                    "is_error": false }),
        ] {
            events.extend(message(&mut state, line));
        }
        // A command answered without a model call, such as `/compact`.
        events.extend(state.turn_started("u2", "/compact"));
        for line in [
            json!({ "type": "system", "subtype": "init" }),
            json!({ "type": "result", "subtype": "success", "num_turns": 0, "result": "Compacted.",
                    "is_error": false }),
        ] {
            events.extend(message(&mut state, line));
        }
        assert_eq!(turn_ends(&events), ["u1", "u2"]);
        assert!(events.iter().any(|event| matches!(
            event,
            Event::ItemEnd { turn_id, item: Item::AgentMessage { text, .. } } if turn_id == "u2" && text == "Compacted."
        )));
    }

    #[test]
    fn work_without_the_hosts_input_is_a_turn_of_its_own() {
        let mut state = SessionState::default();
        let mut events = message(
            &mut state,
            json!({ "type": "assistant", "parent_tool_use_id": null, "uuid": "a2",
                    "message": { "id": "m2", "role": "assistant",
                                 "content": [{ "type": "text", "text": "The task finished." }] } }),
        );
        assert!(matches!(
            &events[..2],
            [Event::Status { status: session::Status::Active }, Event::TurnStart { turn_id }] if turn_id == "a2"
        ));
        events.extend(message(
            &mut state,
            json!({ "type": "result", "subtype": "success", "num_turns": 1, "result": "",
                    "duration_ms": 900, "is_error": false }),
        ));
        assert_eq!(turn_ends(&events), ["a2"]);
    }

    fn stream(state: &mut SessionState, event: Value) -> Vec<Event> {
        message(
            state,
            json!({ "type": "stream_event", "parent_tool_use_id": null, "event": event }),
        )
    }

    #[test]
    fn a_final_only_command_reply_is_visible_and_a_streamed_reply_is_not_repeated() {
        let result = json!({"type":"result","subtype":"success","is_error":false,"result":"Done"});
        let mut handler = SessionState::default();
        handler.turn_started("first", "/command");
        let replies = |events: &[Event]| {
            events.iter().filter(|e| matches!(e, Event::ItemEnd {item: Item::AgentMessage {text,..},..} if text=="Done")).count()
        };
        assert_eq!(replies(&message(&mut handler, result.clone())), 1);
        handler.turn_started("second", "/command");
        stream(
            &mut handler,
            json!({"type":"message_start","message":{"id":"m1"}}),
        );
        stream(
            &mut handler,
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text"}}),
        );
        stream(
            &mut handler,
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Done"}}),
        );
        assert_eq!(
            replies(&stream(
                &mut handler,
                json!({"type":"content_block_stop","index":0})
            )),
            1
        );
        assert_eq!(replies(&message(&mut handler, result)), 0);
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
        // No model call finished, so there is no usage to report.
        assert!(
            matches!(&end[..], [Event::Status { .. }, Event::TurnEnd { turn }] if turn.status == turn::Status::Interrupted)
        );
        assert!(
            message(
                &mut handler,
                json!({ "type": "assistant", "parent_tool_use_id": "task1", "message": {} })
            )
            .is_empty()
        );
    }

    /// From a recorded Claude Code 2.1.326 turn with two model calls; the
    /// `result` adds both up.
    #[test]
    fn usage_is_reported_per_model_call() {
        let mut handler = SessionState::default();
        handler.turn_started("t1", "hi");
        let call = |input: i64, cache_read: i64, cache_write: i64, output: i64| {
            [
                json!({ "type": "stream_event", "parent_tool_use_id": null, "event": {
                    "type": "message_start", "message": { "id": format!("m{cache_read}"), "usage": {
                        "input_tokens": input, "cache_read_input_tokens": cache_read,
                        "cache_creation_input_tokens": cache_write, "output_tokens": 1 } } } }),
                json!({ "type": "stream_event", "parent_tool_use_id": null, "event": {
                    "type": "message_delta", "usage": { "output_tokens": output } } }),
            ]
        };
        let [start, delta] = call(2, 11_457, 11_985, 145);
        assert!(message(&mut handler, start).is_empty());
        let first = message(&mut handler, delta);
        assert!(matches!(&first[..], [Event::Usage { turn_id, usage }]
            if turn_id == "t1" && usage.last.total == 23_589 && usage.total.total == 23_589));

        let [start, delta] = call(2, 23_442, 2_046, 87);
        message(&mut handler, start);
        let second = message(&mut handler, delta);
        assert!(matches!(&second[..], [Event::Usage { usage, .. }]
            if usage.last.total == 25_577 && usage.total.total == 49_166));

        // A sub-agent's call does not change the main agent's context.
        assert!(message(&mut handler, json!({ "type": "stream_event", "parent_tool_use_id": "task1",
            "event": { "type": "message_delta", "usage": { "input_tokens": 900_000, "output_tokens": 1 } } }))
        .is_empty());

        let end = message(
            &mut handler,
            json!({ "type": "result", "subtype": "success", "is_error": false,
            "usage": { "input_tokens": 4, "cache_read_input_tokens": 34_899, "cache_creation_input_tokens": 14_031, "output_tokens": 232 },
            "modelUsage": { "claude-haiku-5-5": { "contextWindow": 200_000 } } }),
        );
        assert!(matches!(&end[0], Event::Usage { usage, .. }
            if usage.last.total == 25_577 && usage.total.total == 49_166 && usage.context_window == Some(200_000)));

        // The next turn's calls know the window and add to the total.
        handler.turn_started("t2", "again");
        let [start, delta] = call(2, 25_000, 100, 10);
        message(&mut handler, start);
        let next = message(&mut handler, delta);
        assert!(matches!(&next[..], [Event::Usage { usage, .. }]
            if usage.context_window == Some(200_000) && usage.total.total == 49_166 + 25_112));
    }

    /// From a recorded `/compact` in Claude Code 2.1.326.
    #[test]
    fn compaction_is_reported_once_with_its_token_counts() {
        let mut handler = SessionState::default();
        handler.turn_started("t1", "/compact");
        let compacting = json!({ "type": "system", "subtype": "status", "status": "compacting" });
        let started = message(&mut handler, compacting.clone());
        assert!(
            matches!(&started[..], [Event::Compaction { turn_id: Some(turn), compaction }]
            if turn == "t1" && compaction.status == turn::CompactionStatus::InProgress)
        );
        // Claude repeats the status every 30 seconds while it compacts.
        assert!(message(&mut handler, compacting).is_empty());
        assert!(
            message(
                &mut handler,
                json!({ "type": "system", "subtype": "status", "status": null,
            "compact_result": "success" })
            )
            .is_empty()
        );
        let done = message(
            &mut handler,
            json!({ "type": "system", "subtype": "compact_boundary",
            "compact_metadata": { "trigger": "manual", "pre_tokens": 25_578, "post_tokens": 7_546,
                "cumulative_dropped_tokens": 18_032, "duration_ms": 3_977 } }),
        );
        assert!(matches!(&done[..], [Event::Compaction { compaction, .. }]
            if compaction.status == turn::CompactionStatus::Completed
                && compaction.trigger == Some(turn::CompactionTrigger::Manual)
                && compaction.tokens_before == Some(25_578)
                && compaction.tokens_after == Some(7_546)));

        // Other statuses still pass through.
        let other = message(
            &mut handler,
            json!({ "type": "system", "subtype": "status", "status": "requesting" }),
        );
        assert!(matches!(&other[..], [Event::Raw { name, .. }] if name == "claude/system/status"));
    }

    #[test]
    fn a_failed_or_abandoned_compaction_ends_it() {
        let mut handler = SessionState::default();
        let compacting = json!({ "type": "system", "subtype": "status", "status": "compacting" });
        message(&mut handler, compacting.clone());
        let failed = message(
            &mut handler,
            json!({ "type": "system", "subtype": "status", "status": null,
            "compact_result": "failed", "compact_error": "prompt too long" }),
        );
        assert!(
            matches!(&failed[..], [Event::Compaction { turn_id: None, compaction }]
            if compaction.status == turn::CompactionStatus::Failed
                && compaction.error.as_deref() == Some("prompt too long"))
        );

        message(&mut handler, compacting);
        let cancelled = message(
            &mut handler,
            json!({ "type": "system", "subtype": "status", "status": null }),
        );
        assert!(
            matches!(&cancelled[..], [Event::Compaction { compaction, .. }]
            if compaction.status == turn::CompactionStatus::Cancelled)
        );
    }

    #[test]
    fn a_call_with_several_deltas_counts_once() {
        let mut handler = SessionState::default();
        handler.turn_started("t1", "hi");
        stream(
            &mut handler,
            json!({ "type": "message_start", "message": { "id": "m1",
            "usage": { "input_tokens": 100, "cache_read_input_tokens": 900, "output_tokens": 1 } } }),
        );
        // Counts a delta leaves out or sends as null come from `message_start`.
        let first = stream(
            &mut handler,
            json!({ "type": "message_delta",
            "usage": { "input_tokens": null, "output_tokens": 5 } }),
        );
        assert!(matches!(&first[..], [Event::Usage { usage, .. }]
            if usage.last.total == 1005 && usage.total.total == 1005));
        let second = stream(
            &mut handler,
            json!({ "type": "message_delta", "usage": { "output_tokens": 10 } }),
        );
        assert!(matches!(&second[..], [Event::Usage { usage, .. }]
            if usage.last.total == 1010 && usage.total.total == 1010));
    }
}
