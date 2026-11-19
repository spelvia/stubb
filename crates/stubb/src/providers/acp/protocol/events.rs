//! ACP `session/update` notifications → Stubb turn events.
//!
//! [`Turn`] follows one prompt turn. Message and thought chunks of the same
//! kind are joined into one item until something else happens; tool calls
//! start with `tool_call` and end with the `tool_call_update` that reports
//! them completed or failed. Updates that are not part of a turn, such as
//! mode changes, are passed on as [`session::Event::Raw`].

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use agent_client_protocol::v1::schema as wire;
use serde_json::Value;

use crate::core::runtime::files::now;
use crate::session;
use crate::turn::{self, Delta, FileChange, FileChangeKind, Item, ToolDetail, ToolStatus};

/// What a streamed item is, so a chunk of another kind closes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Streaming {
    Message,
    Thought,
}

#[derive(Debug)]
struct Open {
    kind: Streaming,
    id: String,
    /// The agent's message id, when it reports one.
    message_id: Option<String>,
    text: String,
}

/// A tool call as last reported, merged from its updates.
#[derive(Debug, Clone, Default)]
struct Tool {
    title: String,
    name: Option<String>,
    kind: Option<wire::ToolKind>,
    status: Option<wire::ToolCallStatus>,
    content: Vec<wire::ToolCallContent>,
    locations: Vec<wire::ToolCallLocation>,
    raw_input: Option<Value>,
    raw_output: Option<Value>,
    started: Option<Instant>,
    ended: bool,
}

/// One prompt turn being mapped.
#[derive(Debug)]
pub(in crate::providers::acp) struct Turn {
    pub(in crate::providers::acp) id: String,
    pub(in crate::providers::acp) started_at: i64,
    /// Stubb asked the agent to cancel this turn.
    pub(in crate::providers::acp) interrupted: bool,
    next_item: u64,
    open: Option<Open>,
    tools: HashMap<String, Tool>,
    /// Tool calls whose permission request was refused.
    declined: Vec<String>,
}

impl Turn {
    pub(in crate::providers::acp) fn new(id: String) -> Self {
        Self {
            id,
            started_at: now(),
            interrupted: false,
            next_item: 0,
            open: None,
            tools: HashMap::new(),
            declined: Vec::new(),
        }
    }

    /// Notes that the tool call's permission was refused, so it ends as
    /// declined rather than failed.
    pub(in crate::providers::acp) fn decline(&mut self, tool_call_id: &str) {
        self.declined.push(tool_call_id.to_owned());
    }

    fn item_id(&mut self) -> String {
        self.next_item += 1;
        format!("{}-{}", self.id, self.next_item)
    }

    /// Maps one update onto events of this turn.
    pub(in crate::providers::acp) fn update(
        &mut self,
        update: &wire::SessionUpdate,
    ) -> Vec<session::Event> {
        let mut events = Vec::new();
        match update {
            wire::SessionUpdate::AgentMessageChunk(chunk) => {
                self.chunk(Streaming::Message, chunk, &mut events);
            }
            wire::SessionUpdate::AgentThoughtChunk(chunk) => {
                self.chunk(Streaming::Thought, chunk, &mut events);
            }
            wire::SessionUpdate::ToolCall(call) => {
                self.close_open(&mut events);
                let id = call.tool_call_id.0.to_string();
                let tool = Tool {
                    title: call.title.clone(),
                    name: call.name.clone(),
                    kind: Some(call.kind),
                    status: Some(call.status),
                    content: call.content.clone(),
                    locations: call.locations.clone(),
                    raw_input: call.raw_input.clone(),
                    raw_output: call.raw_output.clone(),
                    started: Some(Instant::now()),
                    ended: false,
                };
                self.tools.insert(id.clone(), tool);
                self.tool_event(&id, true, &mut events);
            }
            wire::SessionUpdate::ToolCallUpdate(update) => {
                self.close_open(&mut events);
                let id = update.tool_call_id.0.to_string();
                let fields = &update.fields;
                let known = self.tools.contains_key(&id);
                let tool = self.tools.entry(id.clone()).or_insert_with(|| Tool {
                    started: Some(Instant::now()),
                    ..Tool::default()
                });
                if let Some(title) = &fields.title {
                    tool.title.clone_from(title);
                }
                if fields.name.is_some() {
                    tool.name.clone_from(&fields.name);
                }
                if fields.kind.is_some() {
                    tool.kind.clone_from(&fields.kind);
                }
                if fields.status.is_some() {
                    tool.status.clone_from(&fields.status);
                }
                if let Some(content) = &fields.content {
                    tool.content.clone_from(content);
                }
                if let Some(locations) = &fields.locations {
                    tool.locations.clone_from(locations);
                }
                if fields.raw_input.is_some() {
                    tool.raw_input.clone_from(&fields.raw_input);
                }
                if fields.raw_output.is_some() {
                    tool.raw_output.clone_from(&fields.raw_output);
                }
                self.tool_event(&id, !known, &mut events);
            }
            wire::SessionUpdate::Plan(plan) => {
                self.close_open(&mut events);
                events.push(session::Event::ItemEnd {
                    turn_id: self.id.clone(),
                    item: Item::Plan {
                        id: format!("{}-plan", self.id),
                        text: plan_text(plan),
                    },
                });
            }
            wire::SessionUpdate::UsageUpdate(usage) => {
                events.push(session::Event::Usage {
                    turn_id: self.id.clone(),
                    usage: context_usage(usage),
                });
            }
            other => events.extend(raw(other)),
        }
        events
    }

    fn chunk(
        &mut self,
        kind: Streaming,
        chunk: &wire::ContentChunk,
        events: &mut Vec<session::Event>,
    ) {
        let wire::ContentBlock::Text(text) = &chunk.content else {
            self.close_open(events);
            let id = self.item_id();
            events.push(session::Event::ItemEnd {
                turn_id: self.id.clone(),
                item: Item::Other {
                    id: Some(id),
                    kind: "content".to_owned(),
                    raw: serde_json::to_value(&chunk.content).unwrap_or_default(),
                },
            });
            return;
        };
        let message_id = chunk.message_id.as_ref().map(|id| id.0.to_string());
        let continues = self.open.as_ref().is_some_and(|open| {
            open.kind == kind
                && (message_id.is_none()
                    || open.message_id.is_none()
                    || open.message_id == message_id)
        });
        if !continues {
            self.close_open(events);
            let id = self.item_id();
            events.push(session::Event::ItemStart {
                turn_id: self.id.clone(),
                item: streaming_item(kind, &id, String::new()),
            });
            self.open = Some(Open {
                kind,
                id,
                message_id,
                text: String::new(),
            });
        }
        let open = self.open.as_mut().expect("an item is open");
        open.text.push_str(&text.text);
        events.push(session::Event::ItemDelta {
            turn_id: self.id.clone(),
            item_id: open.id.clone(),
            delta: match kind {
                Streaming::Message => Delta::Text {
                    text: text.text.clone(),
                },
                Streaming::Thought => Delta::Thinking {
                    text: text.text.clone(),
                },
            },
        });
    }

    fn close_open(&mut self, events: &mut Vec<session::Event>) {
        if let Some(open) = self.open.take() {
            events.push(session::Event::ItemEnd {
                turn_id: self.id.clone(),
                item: streaming_item(open.kind, &open.id, open.text),
            });
        }
    }

    fn tool_event(&mut self, id: &str, start: bool, events: &mut Vec<session::Event>) {
        let declined = self.declined.iter().any(|declined| declined == id);
        let Some(tool) = self.tools.get_mut(id) else {
            return;
        };
        if tool.ended {
            return;
        }
        let status = tool_status(tool.status.as_ref(), declined);
        let finished = status != ToolStatus::InProgress;
        let item = Item::ToolCall(turn::ToolCall {
            id: id.to_owned(),
            status,
            duration_ms: tool
                .started
                .filter(|_| finished)
                .map(|started| i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX)),
            detail: detail(tool),
        });
        if start {
            events.push(session::Event::ItemStart {
                turn_id: self.id.clone(),
                item: item.clone(),
            });
        }
        if finished {
            tool.ended = true;
            events.push(session::Event::ItemEnd {
                turn_id: self.id.clone(),
                item,
            });
        }
    }

    /// Ends every item still open; tool calls the agent never finished end
    /// as failed when the turn did not complete.
    pub(in crate::providers::acp) fn finish(&mut self, completed: bool) -> Vec<session::Event> {
        let mut events = Vec::new();
        self.close_open(&mut events);
        let mut open: Vec<String> = self
            .tools
            .iter()
            .filter(|(_, tool)| !tool.ended)
            .map(|(id, _)| id.clone())
            .collect();
        open.sort();
        for id in open {
            if let Some(tool) = self.tools.get_mut(&id) {
                tool.status = Some(if completed {
                    wire::ToolCallStatus::Completed
                } else {
                    wire::ToolCallStatus::Failed
                });
            }
            self.tool_event(&id, false, &mut events);
        }
        events
    }
}

/// Events for an update that arrives outside a turn, or that a turn does not
/// map: passed on unchanged, named after its `sessionUpdate` tag.
pub(in crate::providers::acp) fn raw(update: &wire::SessionUpdate) -> Option<session::Event> {
    let payload = serde_json::to_value(update).ok()?;
    let name = payload
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .unwrap_or("session_update")
        .to_owned();
    Some(session::Event::Raw { name, payload })
}

fn streaming_item(kind: Streaming, id: &str, text: String) -> Item {
    match kind {
        Streaming::Message => Item::AgentMessage {
            id: id.to_owned(),
            text,
        },
        Streaming::Thought => Item::Thinking {
            id: id.to_owned(),
            text,
            summarized: false,
        },
    }
}

fn tool_status(status: Option<&wire::ToolCallStatus>, declined: bool) -> ToolStatus {
    match status {
        Some(wire::ToolCallStatus::Completed) => ToolStatus::Completed,
        Some(wire::ToolCallStatus::Failed) if declined => ToolStatus::Declined,
        Some(wire::ToolCallStatus::Failed) => ToolStatus::Failed,
        _ => ToolStatus::InProgress,
    }
}

/// ACP's context window report. `used` tokens are in the context now, so
/// they are both the last and the running count.
fn context_usage(usage: &wire::UsageUpdate) -> turn::Usage {
    let used = i64::try_from(usage.used).unwrap_or(i64::MAX);
    let count = turn::TokenCount {
        input: used,
        total: used,
        ..turn::TokenCount::default()
    };
    turn::Usage {
        last: count,
        total: count,
        context_window: i64::try_from(usage.size).ok(),
    }
}

/// A plan as a Markdown checklist.
fn plan_text(plan: &wire::Plan) -> String {
    plan.entries
        .iter()
        .map(|entry| {
            let mark = match entry.status {
                wire::PlanEntryStatus::Completed => "[x]",
                wire::PlanEntryStatus::InProgress => "[~]",
                _ => "[ ]",
            };
            format!("- {mark} {}", entry.content)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Text content of a tool call, such as a command's output.
fn content_text(content: &[wire::ToolCallContent]) -> Option<String> {
    let text: Vec<&str> = content
        .iter()
        .filter_map(|content| match content {
            wire::ToolCallContent::Content(content) => match &content.content {
                wire::ContentBlock::Text(text) => Some(text.text.as_str()),
                _ => None,
            },
            _ => None,
        })
        .collect();
    (!text.is_empty()).then(|| text.join("\n"))
}

fn output_text(tool: &Tool) -> Option<String> {
    content_text(&tool.content).or_else(|| match &tool.raw_output {
        Some(Value::String(text)) => Some(text.clone()),
        Some(Value::Object(output)) => ["output", "stdout", "content"]
            .iter()
            .find_map(|key| output.get(*key).and_then(Value::as_str))
            .map(str::to_owned),
        _ => None,
    })
}

fn detail(tool: &Tool) -> ToolDetail {
    let diffs: Vec<&wire::Diff> = tool
        .content
        .iter()
        .filter_map(|content| match content {
            wire::ToolCallContent::Diff(diff) => Some(diff),
            _ => None,
        })
        .collect();
    if !diffs.is_empty() {
        return ToolDetail::FileChange {
            changes: diffs.into_iter().map(file_change).collect(),
        };
    }
    let input = tool.raw_input.clone().unwrap_or(Value::Null);
    if tool.kind == Some(wire::ToolKind::Execute) {
        let command = match input.get("command") {
            Some(Value::String(command)) => command.clone(),
            Some(Value::Array(parts)) => parts
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" "),
            _ => tool.title.clone(),
        };
        let exit_code = tool.raw_output.as_ref().and_then(|output| {
            ["exitCode", "exit_code"]
                .iter()
                .find_map(|key| output.get(*key).and_then(Value::as_i64))
                .and_then(|code| i32::try_from(code).ok())
        });
        return ToolDetail::Command {
            command,
            cwd: input.get("cwd").and_then(Value::as_str).map(PathBuf::from),
            output: output_text(tool),
            exit_code,
        };
    }
    ToolDetail::Other {
        name: tool
            .name
            .clone()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| tool.title.clone()),
        input,
        output: output_text(tool),
    }
}

fn file_change(diff: &wire::Diff) -> FileChange {
    FileChange {
        path: diff.path.clone(),
        kind: if diff.old_text.is_none() {
            FileChangeKind::Add
        } else {
            FileChangeKind::Update { move_path: None }
        },
        diff: unified_diff(
            &diff.path,
            diff.old_text.as_deref().unwrap_or_default(),
            &diff.new_text,
        ),
    }
}

/// A one-hunk unified diff from the old and new text ACP reports: the
/// lines both share at the start and end are left out, the rest is
/// replaced.
fn unified_diff(path: &std::path::Path, old: &str, new: &str) -> String {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let prefix = old_lines
        .iter()
        .zip(&new_lines)
        .take_while(|(a, b)| a == b)
        .count();
    let suffix = old_lines[prefix..]
        .iter()
        .rev()
        .zip(new_lines[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let removed = &old_lines[prefix..old_lines.len() - suffix];
    let added = &new_lines[prefix..new_lines.len() - suffix];
    let path = path.display();
    let mut out = format!(
        "--- a/{path}\n+++ b/{path}\n@@ -{},{} +{},{} @@\n",
        prefix + usize::from(!removed.is_empty()),
        removed.len(),
        prefix + usize::from(!added.is_empty()),
        added.len()
    );
    for line in removed {
        out.push('-');
        out.push_str(line);
        out.push('\n');
    }
    for line in added {
        out.push('+');
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn update(value: Value) -> wire::SessionUpdate {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn chunks_of_one_kind_join_into_one_item() {
        let mut turn = Turn::new("t".into());
        let mut events = Vec::new();
        for text in ["Hel", "lo"] {
            events.extend(turn.update(&update(json!({
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": text }
            }))));
        }
        events.extend(turn.update(&update(json!({
            "sessionUpdate": "agent_thought_chunk",
            "content": { "type": "text", "text": "hmm" }
        }))));
        events.extend(turn.finish(true));
        let ends: Vec<&Item> = events
            .iter()
            .filter_map(|event| match event {
                session::Event::ItemEnd { item, .. } => Some(item),
                _ => None,
            })
            .collect();
        assert_eq!(
            ends,
            [
                &Item::AgentMessage {
                    id: "t-1".into(),
                    text: "Hello".into()
                },
                &Item::Thinking {
                    id: "t-2".into(),
                    text: "hmm".into(),
                    summarized: false
                }
            ]
        );
    }

    #[test]
    fn tool_calls_end_with_their_final_update() {
        let mut turn = Turn::new("t".into());
        let start = turn.update(&update(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "c1",
            "title": "Run ls",
            "kind": "execute",
            "status": "pending",
            "rawInput": { "command": ["ls", "-la"] }
        })));
        assert!(matches!(&start[..], [session::Event::ItemStart { .. }]));
        let end = turn.update(&update(json!({
            "sessionUpdate": "tool_call_update",
            "toolCallId": "c1",
            "status": "completed",
            "content": [{ "type": "content", "content": { "type": "text", "text": "a b" } }],
            "rawOutput": { "exitCode": 0 }
        })));
        let [
            session::Event::ItemEnd {
                item: Item::ToolCall(call),
                ..
            },
        ] = &end[..]
        else {
            panic!("expected the tool call to end: {end:?}");
        };
        assert_eq!(call.status, ToolStatus::Completed);
        assert!(call.duration_ms.is_some());
        assert_eq!(
            call.detail,
            ToolDetail::Command {
                command: "ls -la".into(),
                cwd: None,
                output: Some("a b".into()),
                exit_code: Some(0),
            }
        );
    }

    #[test]
    fn diffs_become_file_changes() {
        let mut turn = Turn::new("t".into());
        turn.decline("c2");
        let events = turn.update(&update(json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "c2",
            "title": "Edit",
            "kind": "edit",
            "status": "failed",
            "content": [{ "type": "diff", "path": "/w/a.txt", "oldText": "a\nb\nc\n", "newText": "a\nB\nc\n" }]
        })));
        let Some(session::Event::ItemEnd {
            item: Item::ToolCall(call),
            ..
        }) = events.last()
        else {
            panic!("expected the tool call to end: {events:?}");
        };
        assert_eq!(call.status, ToolStatus::Declined);
        assert_eq!(
            call.detail,
            ToolDetail::FileChange {
                changes: vec![FileChange {
                    path: "/w/a.txt".into(),
                    kind: FileChangeKind::Update { move_path: None },
                    diff: "--- a//w/a.txt\n+++ b//w/a.txt\n@@ -2,1 +2,1 @@\n-b\n+B\n".into(),
                }]
            }
        );
    }

    #[test]
    fn plans_usage_and_other_updates() {
        let mut turn = Turn::new("t".into());
        let plan = turn.update(&update(json!({
            "sessionUpdate": "plan",
            "entries": [
                { "content": "read", "priority": "high", "status": "completed" },
                { "content": "write", "priority": "low", "status": "pending" }
            ]
        })));
        assert_eq!(
            plan,
            [session::Event::ItemEnd {
                turn_id: "t".into(),
                item: Item::Plan {
                    id: "t-plan".into(),
                    text: "- [x] read\n- [ ] write".into()
                }
            }]
        );
        let usage = turn.update(&update(json!({
            "sessionUpdate": "usage_update", "used": 1200, "size": 200000
        })));
        let [session::Event::Usage { usage, .. }] = &usage[..] else {
            panic!("expected usage");
        };
        assert_eq!(usage.context_window, Some(200_000));
        assert_eq!(usage.last.total, 1200);
        let mode = turn.update(&update(json!({
            "sessionUpdate": "current_mode_update", "currentModeId": "code"
        })));
        assert!(
            matches!(&mode[..], [session::Event::Raw { name, .. }] if name == "current_mode_update")
        );
    }
}
