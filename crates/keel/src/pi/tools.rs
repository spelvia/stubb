//! Maps Pi tool calls onto Keel's tool details.

use std::path::PathBuf;

use serde_json::Value;

use crate::turn::{FileChange, FileChangeKind, ToolDetail};

/// `result` is the tool's result: a live `tool_execution_end` result or a
/// stored `toolResult` message. Both carry `content` and `details`.
pub(super) fn detail(name: &str, args: &Value, result: Option<&Value>, failed: bool) -> ToolDetail {
    let text = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or_default();
    let output = result.and_then(output_text);
    match name {
        "bash" => ToolDetail::Command {
            command: text("command").to_owned(),
            cwd: None,
            output,
            // Only live results report the exit code.
            exit_code: result
                .and_then(|result| result.pointer("/structuredContent/exit_code"))
                .and_then(Value::as_i64)
                .map(|code| code as i32),
        },
        "edit" => ToolDetail::FileChange {
            changes: vec![FileChange {
                path: PathBuf::from(text("path")),
                kind: FileChangeKind::Update { move_path: None },
                diff: result
                    .and_then(|result| result.pointer("/details/patch"))
                    .and_then(Value::as_str)
                    .map_or_else(|| edits_diff(args), str::to_owned),
            }],
        },
        // Write creates or replaces the whole file.
        "write" => ToolDetail::FileChange {
            changes: vec![FileChange {
                path: PathBuf::from(text("path")),
                kind: FileChangeKind::Add,
                diff: prefixed("+", text("content")),
            }],
        },
        _ => match name
            .strip_prefix("mcp__")
            .and_then(|rest| rest.split_once("__"))
        {
            Some((server, tool)) => ToolDetail::Mcp {
                server: server.to_owned(),
                tool: tool.to_owned(),
                arguments: args.clone(),
                error: if failed { output } else { None },
            },
            None => ToolDetail::Other {
                name: name.to_owned(),
                input: args.clone(),
                output,
            },
        },
    }
}

/// Text blocks of a tool result's content.
pub(super) fn output_text(result: &Value) -> Option<String> {
    let text: Vec<&str> = result
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect();
    (!text.is_empty()).then(|| text.join("\n"))
}

/// A minimal unified-style diff of the replacements an `edit` call asked
/// for, used when no patch was reported.
fn edits_diff(args: &Value) -> String {
    let replace = |edit: &Value| {
        let text = |key: &str| edit.get(key).and_then(Value::as_str).unwrap_or_default();
        format!(
            "@@\n{}\n{}",
            prefixed("-", text("oldText")),
            prefixed("+", text("newText"))
        )
    };
    match args.get("edits").and_then(Value::as_array) {
        Some(edits) => edits.iter().map(replace).collect::<Vec<_>>().join("\n"),
        None => replace(args),
    }
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
        let result = json!({
            "content": [{ "type": "text", "text": "a.rs\n" }],
            "structuredContent": { "exit_code": 0 },
        });
        assert!(matches!(
            detail("bash", &json!({ "command": "ls" }), Some(&result), false),
            ToolDetail::Command { command, output: Some(out), exit_code: Some(0), .. } if command == "ls" && out == "a.rs\n"
        ));

        let args = json!({ "path": "a.txt", "edits": [{ "oldText": "x", "newText": "y" }] });
        assert!(matches!(
            detail("edit", &args, None, false),
            ToolDetail::FileChange { changes } if changes[0].diff == "@@\n-x\n+y"
        ));
        let patched = json!({ "content": [], "details": { "patch": "--- a.txt\n+++ a.txt\n" } });
        assert!(matches!(
            detail("edit", &args, Some(&patched), false),
            ToolDetail::FileChange { changes } if changes[0].diff.starts_with("--- a.txt")
        ));

        let failed = json!({ "content": [{ "type": "text", "text": "timeout" }] });
        assert!(matches!(
            detail("mcp__searxng__web_url_read", &json!({}), Some(&failed), true),
            ToolDetail::Mcp { server, tool, error: Some(_), .. } if server == "searxng" && tool == "web_url_read"
        ));
        assert!(matches!(
            detail("read", &json!({ "path": "a.rs" }), None, false),
            ToolDetail::Other { name, .. } if name == "read"
        ));
    }
}
