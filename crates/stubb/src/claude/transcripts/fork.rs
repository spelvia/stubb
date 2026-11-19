//! Forks a stored Claude Code session by copying its transcript.
//!
//! Claude Code's own `--fork-session` copies a whole session when it is
//! resumed. Stubb copies the transcript up to the end of a turn into a new
//! session file instead, so the fork is stored at once and can be resumed
//! like any other session:
//!
//! - Lines are copied as Claude Code wrote them, in order, up to the first
//!   entry of the next turn of the main conversation (see
//!   [`history::turn_starts`]). Queue records for that next prompt, written
//!   just before it, are left out. Messages keep their `uuid`s and
//!   `parentUuid` links, so the copy's chain ends at the kept turn's last
//!   entry, which Claude Code resumes from, and turns keep their ids.
//! - Every top-level `sessionId` naming the original session names the new
//!   one, as in a session Claude Code wrote itself.
//! - Sub-agent transcripts (`<session>/subagents/agent-<id>.jsonl` and their
//!   `.meta.json`) are copied, with the same `sessionId` change, when a kept
//!   entry started or reported them: the agent's `toolUseId` is a kept tool
//!   call, or a kept tool result names its `agentId`. This repeats for agents
//!   started by copied agents. Their transcripts are copied whole.
//! - Other files of the session, such as file-history backups and large tool
//!   results saved under the session's folder, are not copied; the copied
//!   transcript refers to the original's paths.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use serde_json::Value;

use super::{history, store};
use crate::core::runtime::files;

/// What [`fork`] keeps of a transcript.
#[derive(Debug, PartialEq)]
pub(in crate::claude) struct Copy {
    /// The transcript's lines, rewritten for the new session.
    pub lines: Vec<String>,
    /// The parsed entries of `lines`.
    pub entries: Vec<Value>,
}

/// The transcript `lines` of session `from`, cut after turn `at_turn`
/// (everything for `None`) and rewritten for session `to`. `None` when the
/// main conversation has no turn `at_turn`.
pub(in crate::claude) fn transcript(
    lines: Vec<(String, Value)>,
    from: &str,
    to: &str,
    at_turn: Option<&str>,
) -> Option<Copy> {
    let mut lines = lines;
    if let Some(at_turn) = at_turn {
        let entries: Vec<Value> = lines.iter().map(|(_, value)| value.clone()).collect();
        let starts = history::turn_starts(&entries);
        let position = starts.iter().position(|(_, id)| id == at_turn)?;
        let end = starts.get(position + 1).map_or(lines.len(), |(at, _)| *at);
        lines.truncate(end);
        // The next prompt is queued just before it is written.
        if starts.get(position + 1).is_some() {
            while lines
                .last()
                .is_some_and(|(_, value)| value["type"] == "queue-operation")
            {
                lines.pop();
            }
        }
    }
    let (lines, entries) = lines
        .into_iter()
        .map(|(line, value)| rename_session(line, value, from, to))
        .unzip();
    Some(Copy { lines, entries })
}

/// The line with its top-level `sessionId` changed from `from` to `to`.
/// The text is changed in place, so everything else stays exactly as
/// Claude Code wrote it; a line whose text cannot be changed that way is
/// written anew from its value.
fn rename_session(line: String, mut value: Value, from: &str, to: &str) -> (String, Value) {
    if value.get("sessionId").and_then(Value::as_str) != Some(from) {
        return (line, value);
    }
    let quoted = |id: &str| serde_json::to_string(id).expect("a string serializes");
    let pattern = format!("\"sessionId\":{}", quoted(from));
    let renamed = line.replace(&pattern, &format!("\"sessionId\":{}", quoted(to)));
    match serde_json::from_str::<Value>(&renamed) {
        Ok(parsed) if parsed.get("sessionId").and_then(Value::as_str) == Some(to) => {
            (renamed, parsed)
        }
        _ => {
            value["sessionId"] = Value::String(to.to_owned());
            (value.to_string(), value)
        }
    }
}

/// Copies the sub-agent transcripts of session `from` that the kept
/// `entries` refer to, from `source` (the original's session folder) to
/// `target` (the fork's), rewriting their `sessionId` to `to`.
pub(in crate::claude) fn copy_agents(
    entries: &[Value],
    source: &Path,
    target: &Path,
    from: &str,
    to: &str,
) -> std::io::Result<()> {
    let directory = source.join("subagents");
    if !directory.is_dir() {
        return Ok(());
    }
    let mut available = Vec::new();
    for entry in fs::read_dir(&directory)? {
        let path = entry?.path();
        let Some(id) = path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("agent-"))
            .and_then(|name| name.strip_suffix(".jsonl"))
        else {
            continue;
        };
        if !store::safe_id(id) || !path.is_file() {
            continue;
        }
        let meta = fs::read(directory.join(format!("agent-{id}.meta.json")))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok());
        available.push((id.to_owned(), meta));
    }

    let mut references = References::default();
    references.read(entries);
    let mut copied = BTreeSet::new();
    loop {
        let next: Vec<_> = available
            .iter()
            .filter(|(id, meta)| !copied.contains(id) && references.started(id, meta.as_ref()))
            .map(|(id, _)| id.clone())
            .collect();
        if next.is_empty() {
            return Ok(());
        }
        for id in next {
            let name = format!("agent-{id}.jsonl");
            let lines: Vec<_> = files::read_lines(&directory.join(&name))?
                .into_iter()
                .map(|(line, value)| rename_session(line, value, from, to))
                .collect();
            let values: Vec<Value> = lines.iter().map(|(_, value)| value.clone()).collect();
            references.read(&values);
            let text: Vec<&str> = lines.iter().map(|(line, _)| line.as_str()).collect();
            files::write_new_lines(&target.join("subagents").join(&name), &text)?;
            let meta = directory.join(format!("agent-{id}.meta.json"));
            if meta.is_file() {
                fs::copy(
                    &meta,
                    target
                        .join("subagents")
                        .join(format!("agent-{id}.meta.json")),
                )?;
            }
            copied.insert(id);
        }
    }
}

/// Tool calls and reported agents of copied entries.
#[derive(Default)]
struct References {
    tool_calls: BTreeSet<String>,
    agents: BTreeSet<String>,
}

impl References {
    fn read(&mut self, entries: &[Value]) {
        for entry in entries {
            if let Some(agent) = entry["toolUseResult"]["agentId"].as_str() {
                self.agents.insert(agent.to_owned());
            }
            let blocks = entry["message"]["content"].as_array().into_iter().flatten();
            for block in blocks {
                if block["type"] == "tool_use"
                    && let Some(id) = block["id"].as_str()
                {
                    self.tool_calls.insert(id.to_owned());
                }
            }
        }
    }

    fn started(&self, agent: &str, meta: Option<&Value>) -> bool {
        self.agents.contains(agent)
            || meta
                .and_then(|meta| meta["toolUseId"].as_str())
                .is_some_and(|tool| self.tool_calls.contains(tool))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn line(value: Value) -> (String, Value) {
        (value.to_string(), value)
    }

    /// Two turns as Claude Code writes them: metadata records around the
    /// chain, a tool call that starts a sub-agent in the first turn, and the
    /// next prompt's queue records before its user entry.
    fn transcript_lines() -> Vec<(String, Value)> {
        vec![
            line(json!({ "type": "session-owner", "sessionId": "old" })),
            line(json!({ "type": "queue-operation", "operation": "enqueue", "sessionId": "old" })),
            line(json!({ "type": "queue-operation", "operation": "dequeue", "sessionId": "old" })),
            line(
                json!({ "type": "user", "uuid": "u1", "parentUuid": null, "sessionId": "old", "isSidechain": false, "message": { "role": "user", "content": "remember PINEAPPLE" } }),
            ),
            line(json!({ "type": "file-history-snapshot", "messageId": "u1", "snapshot": {} })),
            line(
                json!({ "type": "assistant", "uuid": "a1", "parentUuid": "u1", "sessionId": "old", "isSidechain": false, "message": { "content": [{ "type": "tool_use", "id": "toolu_1", "name": "Agent", "input": {} }] } }),
            ),
            line(
                json!({ "type": "user", "uuid": "r1", "parentUuid": "a1", "sessionId": "old", "isSidechain": false, "toolUseResult": { "agentId": "agent1" }, "message": { "content": [{ "type": "tool_result", "tool_use_id": "toolu_1", "content": "done" }] } }),
            ),
            line(
                json!({ "type": "assistant", "uuid": "a2", "parentUuid": "r1", "sessionId": "old", "isSidechain": false, "message": { "content": [{ "type": "text", "text": "OK" }] } }),
            ),
            line(
                json!({ "type": "last-prompt", "leafUuid": "a2", "lastPrompt": "remember PINEAPPLE", "sessionId": "old" }),
            ),
            line(json!({ "type": "queue-operation", "operation": "enqueue", "sessionId": "old" })),
            line(json!({ "type": "queue-operation", "operation": "dequeue", "sessionId": "old" })),
            line(
                json!({ "type": "user", "uuid": "u2", "parentUuid": "a2", "sessionId": "old", "isSidechain": false, "message": { "role": "user", "content": [{ "type": "text", "text": "remember BANANA" }] } }),
            ),
            line(
                json!({ "type": "assistant", "uuid": "a3", "parentUuid": "u2", "sessionId": "old", "isSidechain": false, "message": { "content": [{ "type": "tool_use", "id": "toolu_2", "name": "Agent", "input": {} }] } }),
            ),
        ]
    }

    #[test]
    fn cuts_after_the_turn_and_renames_the_session() {
        let copy = transcript(transcript_lines(), "old", "new", Some("u1")).unwrap();
        let kinds: Vec<_> = copy
            .entries
            .iter()
            .map(|entry| {
                entry["uuid"]
                    .as_str()
                    .unwrap_or(entry["type"].as_str().unwrap())
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "session-owner",
                "queue-operation",
                "queue-operation",
                "u1",
                "file-history-snapshot",
                "a1",
                "r1",
                "a2",
                "last-prompt"
            ]
        );
        for (text, entry) in copy.lines.iter().zip(&copy.entries) {
            assert_eq!(&serde_json::from_str::<Value>(text).unwrap(), entry);
            if entry.get("sessionId").is_some() {
                assert_eq!(entry["sessionId"], "new");
            }
        }
        // The copy's chain ends at the kept turn's last message.
        let turns = history::turns(&copy.entries);
        assert_eq!(
            turns
                .iter()
                .map(|turn| turn.id.as_str())
                .collect::<Vec<_>>(),
            ["u1"]
        );
    }

    #[test]
    fn keeps_every_turn_without_a_turn_and_rejects_unknown_turns() {
        let lines = transcript_lines();
        let count = lines.len();
        let copy = transcript(lines.clone(), "old", "new", None).unwrap();
        assert_eq!(copy.lines.len(), count);
        let turns = history::turns(&copy.entries);
        assert_eq!(turns.len(), 2);
        let last = transcript(lines.clone(), "old", "new", Some("u2")).unwrap();
        assert_eq!(last.lines.len(), count);
        // Only turn ids of the main conversation; not other entries' uuids.
        assert_eq!(transcript(lines.clone(), "old", "new", Some("a1")), None);
        assert_eq!(transcript(lines, "old", "new", Some("missing")), None);
    }

    #[test]
    fn renames_only_the_session_and_keeps_the_text() {
        let original = r#"{"parentUuid":"p","sessionId":"old","message":{"content":"a \"sessionId\":\"x\""},"version":"2.1.326"}"#;
        let value: Value = serde_json::from_str(original).unwrap();
        let (text, value) = rename_session(original.to_owned(), value, "old", "new");
        assert_eq!(
            text,
            r#"{"parentUuid":"p","sessionId":"new","message":{"content":"a \"sessionId\":\"x\""},"version":"2.1.326"}"#
        );
        assert_eq!(value["sessionId"], "new");

        // Another session's records stay as they are.
        let other = r#"{"sessionId":"other"}"#;
        let (text, _) = rename_session(
            other.to_owned(),
            serde_json::from_str(other).unwrap(),
            "old",
            "new",
        );
        assert_eq!(text, other);

        // Text Stubb cannot change in place is written anew.
        let spaced = r#"{ "sessionId" : "old", "n": 1 }"#;
        let (text, value) = rename_session(
            spaced.to_owned(),
            serde_json::from_str(spaced).unwrap(),
            "old",
            "new",
        );
        assert_eq!(value, json!({ "sessionId": "new", "n": 1 }));
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), value);
    }

    #[test]
    fn copies_the_agents_kept_entries_started() {
        let root = std::env::temp_dir().join(format!("stubb-claude-fork-{}", uuid::Uuid::new_v4()));
        let source = root.join("old");
        let target = root.join("new");
        let agents = source.join("subagents");
        fs::create_dir_all(&agents).unwrap();
        let agent = |id: &str, tool: &str, calls: &str| {
            fs::write(
                agents.join(format!("agent-{id}.jsonl")),
                format!(
                    "{}\n",
                    json!({ "type": "assistant", "sessionId": "old", "agentId": id, "isSidechain": true,
                        "message": { "content": [{ "type": "tool_use", "id": calls, "name": "Agent", "input": {} }] } })
                ),
            )
            .unwrap();
            fs::write(
                agents.join(format!("agent-{id}.meta.json")),
                json!({ "toolUseId": tool }).to_string(),
            )
            .unwrap();
        };
        // agent1 was started in the kept turn, nested by agent1, and agent2
        // in the next turn.
        agent("agent1", "toolu_1", "toolu_nested");
        agent("nested", "toolu_nested", "toolu_none");
        agent("agent2", "toolu_2", "toolu_none2");

        let copy = transcript(transcript_lines(), "old", "new", Some("u1")).unwrap();
        copy_agents(&copy.entries, &source, &target, "old", "new").unwrap();
        let mut names: Vec<_> = fs::read_dir(target.join("subagents"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "agent-agent1.jsonl",
                "agent-agent1.meta.json",
                "agent-nested.jsonl",
                "agent-nested.meta.json"
            ]
        );
        let copied = files::read_entries(&target.join("subagents/agent-agent1.jsonl")).unwrap();
        assert_eq!(copied[0]["sessionId"], "new");
        assert_eq!(copied[0]["agentId"], "agent1");
        fs::remove_dir_all(root).unwrap();
    }
}
