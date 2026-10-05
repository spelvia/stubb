//! Maps Claude Code tool calls onto Keel's tool details.

use std::path::PathBuf;

use serde_json::Value;

use crate::turn::{FileChange, FileChangeKind, ToolDetail};

pub(super) fn detail(name: &str, input: &Value, output: Option<String>) -> ToolDetail {
    let text = |key: &str| input.get(key).and_then(Value::as_str).unwrap_or_default();
    match name {
        "Bash" => ToolDetail::Command {
            command: text("command").to_owned(),
            cwd: None,
            output,
            // Claude Code reports failure through the tool result, not an exit code.
            exit_code: None,
        },
        "Edit" | "MultiEdit" | "Write" | "NotebookEdit" => ToolDetail::FileChange {
            changes: file_changes(name, input),
        },
        _ => match name
            .strip_prefix("mcp__")
            .and_then(|rest| rest.split_once("__"))
        {
            Some((server, tool)) => ToolDetail::Mcp {
                server: server.to_owned(),
                tool: tool.to_owned(),
                arguments: input.clone(),
                error: None,
            },
            None => ToolDetail::Other {
                name: name.to_owned(),
                input: input.clone(),
                output,
            },
        },
    }
}

/// Text of a tool result's content, which is a string or a list of blocks.
pub(super) fn result_text(content: &Value) -> Option<String> {
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => {
            let text: Vec<&str> = blocks
                .iter()
                .filter_map(|block| block.get("text").and_then(Value::as_str))
                .collect();
            (!text.is_empty()).then(|| text.join("\n"))
        }
        _ => None,
    }
}

fn file_changes(name: &str, input: &Value) -> Vec<FileChange> {
    let text = |value: &Value, key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let path = PathBuf::from(
        input
            .get("file_path")
            .or_else(|| input.get("notebook_path"))
            .and_then(Value::as_str)
            .unwrap_or_default(),
    );
    let (kind, diff) = match name {
        // Write creates or replaces the whole file.
        "Write" => (FileChangeKind::Add, prefixed("+", &text(input, "content"))),
        "MultiEdit" => {
            let edits = input.get("edits").and_then(Value::as_array);
            let diff = edits
                .into_iter()
                .flatten()
                .map(|edit| replace_diff(&text(edit, "old_string"), &text(edit, "new_string")))
                .collect::<Vec<_>>()
                .join("\n");
            (FileChangeKind::Update { move_path: None }, diff)
        }
        "NotebookEdit" => (
            FileChangeKind::Update { move_path: None },
            prefixed("+", &text(input, "new_source")),
        ),
        _ => (
            FileChangeKind::Update { move_path: None },
            replace_diff(&text(input, "old_string"), &text(input, "new_string")),
        ),
    };
    vec![FileChange { path, kind, diff }]
}

/// A minimal unified-style diff of one replacement.
fn replace_diff(old: &str, new: &str) -> String {
    format!("@@\n{}\n{}", prefixed("-", old), prefixed("+", new))
}

fn prefixed(prefix: &str, text: &str) -> String {
    text.lines()
        .map(|line| format!("{prefix}{line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn maps_known_tools() {
        assert!(matches!(
            detail("Bash", &json!({ "command": "ls" }), Some("a\nb".into())),
            ToolDetail::Command { command, output: Some(_), .. } if command == "ls"
        ));
        assert!(matches!(
            detail("Edit", &json!({ "file_path": "/a.rs", "old_string": "x", "new_string": "y" }), None),
            ToolDetail::FileChange { changes } if changes[0].diff == "@@\n-x\n+y"
        ));
        assert!(matches!(
            detail("mcp__github__create_issue", &json!({}), None),
            ToolDetail::Mcp { server, tool, .. } if server == "github" && tool == "create_issue"
        ));
        assert!(matches!(
            detail("Read", &json!({ "file_path": "/a.rs" }), None),
            ToolDetail::Other { name, .. } if name == "Read"
        ));
    }

    #[test]
    fn reads_result_text() {
        assert_eq!(result_text(&json!("ok")).as_deref(), Some("ok"));
        assert_eq!(
            result_text(&json!([{ "type": "text", "text": "a" }, { "type": "text", "text": "b" }]))
                .as_deref(),
            Some("a\nb")
        );
        assert_eq!(result_text(&json!(null)), None);
    }
}
