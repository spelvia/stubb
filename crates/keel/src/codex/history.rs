//! Maps Codex threads, turns and items onto Keel's session model.

use serde::de::DeserializeOwned;
use serde_json::Value;

use super::types;
use crate::session;
use crate::turn::{
    self, FileChange, FileChangeKind, Item, ToolCall, ToolDetail, ToolStatus, Turn, UserContent,
};

pub(super) fn summary(thread: types::Thread) -> session::Summary {
    session::Summary {
        id: thread.id,
        provider: super::PROVIDER_ID.to_owned(),
        title: thread.name,
        preview: thread.preview,
        cwd: thread.cwd,
        model: thread.model,
        effort: thread.reasoning_effort,
        status: thread.status.into(),
        created_at: thread.created_at,
        updated_at: thread.updated_at,
        source: describe(&thread.source),
    }
}

impl From<types::ThreadStatus> for session::Status {
    fn from(status: types::ThreadStatus) -> Self {
        match status {
            types::ThreadStatus::NotLoaded => Self::NotLoaded,
            types::ThreadStatus::Idle => Self::Idle,
            types::ThreadStatus::Active => Self::Active,
            types::ThreadStatus::SystemError => Self::Error,
        }
    }
}

pub(super) fn turn(turn: types::Turn) -> Turn {
    Turn {
        id: turn.id,
        status: match turn.status {
            types::TurnStatus::InProgress => turn::Status::InProgress,
            types::TurnStatus::Completed => turn::Status::Completed,
            types::TurnStatus::Interrupted => turn::Status::Interrupted,
            types::TurnStatus::Failed => turn::Status::Failed,
        },
        items: turn.items.into_iter().map(item).collect(),
        started_at: turn.started_at,
        completed_at: turn.completed_at,
        duration_ms: turn.duration_ms,
        error: turn.error.map(|error| error.message),
    }
}

/// Decodes one raw item. Unknown types, and known types that fail to decode,
/// become [`Item::Other`] so history stays readable as Codex evolves.
pub(super) fn item(raw: Value) -> Item {
    let kind = raw
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_owned();
    let mapped = match kind.as_str() {
        "userMessage" => decode::<types::UserMessageItem>(&raw).map(|item| Item::UserMessage {
            id: item.id,
            content: item.content.iter().map(user_content).collect(),
        }),
        "agentMessage" => decode::<types::TextItem>(&raw).map(|item| Item::AgentMessage {
            id: item.id,
            text: item.text,
        }),
        "plan" => decode::<types::TextItem>(&raw).map(|item| Item::Plan {
            id: item.id,
            text: item.text,
        }),
        // Codex only reports a summary of its reasoning, in parts.
        "reasoning" => decode::<types::ReasoningItem>(&raw).map(|item| Item::Thinking {
            id: item.id,
            text: item.summary.join("\n\n"),
            summarized: true,
        }),
        "commandExecution" => decode::<types::CommandExecutionItem>(&raw).map(|item| {
            Item::ToolCall(ToolCall {
                id: item.id,
                status: item.status.into(),
                duration_ms: item.duration_ms,
                detail: ToolDetail::Command {
                    command: item.command,
                    cwd: item.cwd,
                    output: item.aggregated_output,
                    exit_code: item.exit_code,
                },
            })
        }),
        "fileChange" => decode::<types::FileChangeItem>(&raw).map(|item| {
            Item::ToolCall(ToolCall {
                id: item.id,
                status: item.status.into(),
                duration_ms: None,
                detail: ToolDetail::FileChange {
                    changes: item.changes.into_iter().map(file_change).collect(),
                },
            })
        }),
        "mcpToolCall" => decode::<types::McpToolCallItem>(&raw).map(|item| {
            Item::ToolCall(ToolCall {
                id: item.id,
                status: item.status.into(),
                duration_ms: item.duration_ms,
                detail: ToolDetail::Mcp {
                    server: item.server,
                    tool: item.tool,
                    arguments: item.arguments,
                    error: item.error.as_ref().and_then(describe),
                },
            })
        }),
        _ => None,
    };
    mapped.unwrap_or_else(|| Item::Other {
        id: raw.get("id").and_then(Value::as_str).map(str::to_owned),
        kind,
        raw,
    })
}

fn decode<T: DeserializeOwned>(raw: &Value) -> Option<T> {
    serde_json::from_value(raw.clone()).ok()
}

fn user_content(raw: &Value) -> UserContent {
    let kind = raw.get("type").and_then(Value::as_str).unwrap_or("unknown");
    let text = |key: &str| raw.get(key).and_then(Value::as_str).map(str::to_owned);
    match kind {
        "text" => UserContent::Text {
            text: text("text").unwrap_or_default(),
        },
        "image" | "localImage" => UserContent::Image {
            source: text("url")
                .or_else(|| text("path"))
                .unwrap_or_else(|| kind.to_owned()),
        },
        other => UserContent::Other {
            kind: other.to_owned(),
        },
    }
}

fn file_change(change: types::FileUpdateChange) -> FileChange {
    FileChange {
        path: change.path,
        kind: match change.kind {
            types::PatchChangeKind::Add => FileChangeKind::Add,
            types::PatchChangeKind::Delete => FileChangeKind::Delete,
            types::PatchChangeKind::Update { move_path } => FileChangeKind::Update { move_path },
        },
        diff: change.diff,
    }
}

impl From<types::ToolStatus> for ToolStatus {
    fn from(status: types::ToolStatus) -> Self {
        match status {
            types::ToolStatus::InProgress => Self::InProgress,
            types::ToolStatus::Completed => Self::Completed,
            types::ToolStatus::Failed => Self::Failed,
            types::ToolStatus::Declined => Self::Declined,
        }
    }
}

/// Renders a loosely typed protocol value (string, tagged object, or other
/// JSON) as display text.
pub(super) fn describe(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(text) => Some(text.clone()),
        Value::Object(map) => map
            .get("type")
            .or_else(|| map.get("message"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| Some(value.to_string())),
        other => Some(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn maps_a_turn_with_tools_and_unknown_items() {
        let raw = json!({
            "id": "turn-1",
            "status": "completed",
            "itemsView": "full",
            "startedAt": 10,
            "completedAt": 20,
            "durationMs": 10000,
            "items": [
                { "type": "userMessage", "id": "u1", "content": [{ "type": "text", "text": "run the tests", "textElements": [] }] },
                { "type": "reasoning", "id": "r1", "summary": ["Plan the run"], "content": [] },
                { "type": "commandExecution", "id": "c1", "command": "cargo test", "cwd": "/repo",
                  "processId": null, "source": "agent", "status": "completed", "commandActions": [],
                  "aggregatedOutput": "ok", "exitCode": 0, "durationMs": 1200 },
                { "type": "fileChange", "id": "f1", "status": "completed",
                  "changes": [{ "path": "/repo/a.rs", "kind": { "type": "update", "move_path": null }, "diff": "@@" }] },
                { "type": "contextCompaction", "id": "x1" },
                { "type": "agentMessage", "id": "a1", "text": "All green." }
            ]
        });
        let turn = turn(serde_json::from_value(raw).expect("turn decodes"));

        assert_eq!(turn.status, turn::Status::Completed);
        assert_eq!(turn.items.len(), 6);
        assert!(matches!(
            &turn.items[0],
            Item::UserMessage { content, .. } if content == &[UserContent::Text { text: "run the tests".into() }]
        ));
        assert!(matches!(
            &turn.items[2],
            Item::ToolCall(ToolCall {
                status: ToolStatus::Completed,
                detail: ToolDetail::Command {
                    exit_code: Some(0),
                    ..
                },
                ..
            })
        ));
        assert!(matches!(
            &turn.items[3],
            Item::ToolCall(ToolCall { detail: ToolDetail::FileChange { changes }, .. })
                if matches!(changes[0].kind, FileChangeKind::Update { move_path: None })
        ));
        assert!(matches!(&turn.items[4], Item::Other { kind, .. } if kind == "contextCompaction"));
        assert!(matches!(&turn.items[5], Item::AgentMessage { text, .. } if text == "All green."));
    }

    #[test]
    fn active_status_ignores_its_flags() {
        let status: types::ThreadStatus = serde_json::from_value(
            json!({ "type": "active", "activeFlags": ["waitingOnApproval"] }),
        )
        .expect("status decodes");
        assert_eq!(session::Status::from(status), session::Status::Active);
    }

    #[test]
    fn malformed_known_item_is_kept_raw() {
        let item = item(json!({ "type": "commandExecution", "id": "c1" }));
        assert!(
            matches!(item, Item::Other { kind, id: Some(id), .. } if kind == "commandExecution" && id == "c1")
        );
    }
}
