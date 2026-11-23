//! Stored background commands, read from native rollout files.
//!
//! Codex's stored turns (`thread/turns/list`) leave out command items that
//! completed after their turn, and nothing in a stored command item says
//! whether it ran in the background. The rollout file records each item when
//! it completes, after the turn's own completion for a command that outlived
//! its turn: those are the stored tasks. Read-only; consumers never supply
//! paths, and only the paths Codex reports for the session's threads are read.

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Codex, agents, output, provider, types};
use crate::{Error, Result, observation::agent, observation::task};

pub(super) struct Stored {
    pub(super) summary: task::Summary,
    pub(super) output: Option<String>,
}

/// Commands of `thread` that completed after their turn, in rollout order.
pub(super) fn read(path: &Path, thread: &str) -> Result<Vec<Stored>> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(error) => return Err(error.into()),
    };
    let mut reader = BufReader::new(file);
    let mut ended = HashSet::new();
    let mut seen = HashSet::new();
    let mut tasks = vec![];
    let mut line = Vec::new();
    loop {
        line.clear();
        if reader.read_until(b'\n', &mut line)? == 0 {
            break;
        }
        // Most lines are other records; skip them before parsing.
        let Ok(text) = std::str::from_utf8(&line) else {
            continue;
        };
        if ![
            "\"task_complete\"",
            "\"turn_aborted\"",
            "\"item_completed\"",
        ]
        .iter()
        .any(|kind| text.contains(kind))
        {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        if record["type"] != "event_msg" {
            continue;
        }
        let payload = &record["payload"];
        let Some(turn_id) = payload["turn_id"].as_str() else {
            continue;
        };
        match payload["type"].as_str() {
            Some("task_complete" | "turn_aborted") => {
                ended.insert(turn_id.to_owned());
            }
            Some("item_completed")
                if ended.contains(turn_id)
                    && payload["thread_id"].as_str().is_none_or(|id| id == thread) =>
            {
                if let Some(stored) = command(thread, turn_id, &payload["item"])
                    && seen.insert(stored.summary.id.clone())
                {
                    tasks.push(stored);
                }
            }
            _ => {}
        }
    }
    Ok(tasks)
}

fn command(thread: &str, turn_id: &str, item: &Value) -> Option<Stored> {
    if item["type"] != "CommandExecution" {
        return None;
    }
    let id = item["id"].as_str()?;
    let command = match &item["command"] {
        Value::String(command) => command.clone(),
        Value::Array(parts) => {
            let parts: Vec<_> = parts.iter().filter_map(Value::as_str).collect();
            match parts.as_slice() {
                // A login-shell wrapper: show the script it runs.
                [_, "-c" | "-lc", script] => (*script).to_owned(),
                parts => parts.join(" "),
            }
        }
        _ => return None,
    };
    let output = item["aggregated_output"].as_str().map(str::to_owned);
    let duration = &item["duration"];
    let duration_ms = duration["secs"]
        .as_u64()
        .map(|secs| secs * 1000 + duration["nanos"].as_u64().unwrap_or(0) / 1_000_000);
    Some(Stored {
        summary: task::Summary {
            id: id.into(),
            owner_agent_id: Some(thread.into()),
            parent_id: None,
            origin: Some(agent::Origin {
                agent_id: thread.into(),
                turn_id: Some(turn_id.into()),
                item_id: Some(id.into()),
            }),
            kind: task::Kind::Command { command },
            description: None,
            background: Some(true),
            ambient: None,
            status: match item["status"].as_str() {
                Some("completed") => task::Status::Completed,
                Some("failed" | "declined") => task::Status::Failed,
                _ => task::Status::Unknown,
            },
            output: if output.is_some() {
                task::OutputAvailability::Complete
            } else {
                task::OutputAvailability::Unavailable
            },
            progress: duration_ms.map(|duration_ms| task::Progress {
                duration_ms: Some(duration_ms),
                ..Default::default()
            }),
        },
        output,
    })
}

#[derive(Serialize, Deserialize)]
struct Cursor {
    root: String,
    owner: Option<String>,
    offset: usize,
}
const PREFIX: &str = "keel:codex-tasks:v1:";

fn cursor_error(value: &str) -> Error {
    Error::InvalidSetting {
        setting: "cursor",
        value: value.into(),
        reason: "Invalid or differently scoped task cursor".into(),
    }
}

impl Codex {
    /// The root and every stored descendant, or only `owner`, with their
    /// rollout files.
    async fn task_threads(
        &self,
        root: &str,
        owner: Option<&str>,
    ) -> Result<Vec<(String, PathBuf)>> {
        let root_thread = self.agent_thread(root).await?;
        if let Some(owner) = owner {
            self.membership(root, owner).await?;
            let thread = if owner == root {
                root_thread
            } else {
                self.agent_thread(owner).await?
            };
            return Ok(thread
                .path
                .map(|path| (thread.id, path))
                .into_iter()
                .collect());
        }
        let mut threads: Vec<_> = root_thread
            .path
            .map(|path| (root_thread.id, path))
            .into_iter()
            .collect();
        let mut cursor = None;
        let mut seen = HashSet::new();
        for _ in 0..provider::MAX_PAGES {
            let response: types::ThreadListResponse = self
                .request(
                    "thread/list",
                    &types::ThreadListParams {
                        cursor,
                        limit: Some(provider::PAGE_LIMIT),
                        sort_key: "created_at",
                        sort_direction: "asc",
                        cwd: None,
                        source_kinds: agents::SOURCES.to_vec(),
                        parent_thread_id: None,
                        ancestor_thread_id: Some(root.into()),
                    },
                )
                .await?;
            threads.extend(
                response
                    .data
                    .into_iter()
                    .filter_map(|thread| thread.path.map(|path| (thread.id, path))),
            );
            match response.next_cursor {
                None => return Ok(threads),
                Some(next) if seen.insert(next.clone()) => cursor = Some(next),
                Some(_) => return Err(cursor_error("repeated stored-discovery cursor")),
            }
        }
        Err(cursor_error("stored-discovery page limit exceeded"))
    }

    async fn stored_tasks(&self, root: &str, owner: Option<&str>) -> Result<Vec<Stored>> {
        let threads = self.task_threads(root, owner).await?;
        crate::core::runtime::files::blocking(move || {
            let mut tasks = vec![];
            let mut ids = HashSet::new();
            for (thread, path) in threads {
                for stored in read(&path, &thread)? {
                    // Native item IDs are unique; never let a duplicate shadow one.
                    if ids.insert(stored.summary.id.clone()) {
                        tasks.push(stored);
                    }
                }
            }
            Ok(tasks)
        })
        .await
    }

    pub async fn list_tasks(&self, root: &str, query: task::Query) -> Result<task::Page> {
        let limit = agents::limit(query.limit)? as usize;
        let start = match query.cursor.as_deref() {
            Some(value) => {
                let cursor: Cursor = value
                    .strip_prefix(PREFIX)
                    .and_then(|json| serde_json::from_str(json).ok())
                    .ok_or_else(|| cursor_error(value))?;
                if cursor.root != root || cursor.owner != query.owner_agent_id {
                    return Err(cursor_error(value));
                }
                cursor.offset
            }
            None => 0,
        };
        let tasks = self
            .stored_tasks(root, query.owner_agent_id.as_deref())
            .await?;
        if start > tasks.len() {
            return Err(cursor_error(query.cursor.as_deref().unwrap_or("")));
        }
        let end = (start + limit).min(tasks.len());
        let next_cursor = (end < tasks.len()).then(|| {
            format!(
                "{PREFIX}{}",
                serde_json::to_string(&Cursor {
                    root: root.into(),
                    owner: query.owner_agent_id.clone(),
                    offset: end,
                })
                .expect("cursor serialization")
            )
        });
        Ok(task::Page {
            tasks: tasks
                .into_iter()
                .skip(start)
                .take(end - start)
                .map(|stored| stored.summary)
                .collect(),
            next_cursor,
        })
    }

    pub async fn task_output(
        &self,
        root: &str,
        id: &str,
        query: task::OutputQuery,
    ) -> Result<task::OutputPage> {
        let stored = self
            .stored_tasks(root, None)
            .await?
            .into_iter()
            .find(|stored| stored.summary.id == id)
            .ok_or_else(|| Error::TaskNotFound {
                session_id: root.into(),
                id: id.into(),
            })?;
        let text = stored.output.unwrap_or_default();
        output::page(
            output::View {
                session: root,
                task: id,
                generation: "stored",
                text: &text,
                start: 0,
                terminal: true,
                availability: stored.summary.output,
            },
            query,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_commands_that_outlive_their_turn_are_tasks() {
        let folder = std::env::temp_dir().join(format!("stubb-rollout-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&folder).unwrap();
        let path = folder.join("rollout.jsonl");
        std::fs::write(&path, include_str!("fixtures/codex-rollout-0.170.0.jsonl")).unwrap();
        let tasks = read(&path, "root").unwrap();
        assert_eq!(
            tasks
                .iter()
                .map(|t| t.summary.id.as_str())
                .collect::<Vec<_>>(),
            ["exec-server", "exec-interrupted"]
        );
        let server = &tasks[0];
        assert_eq!(server.summary.status, task::Status::Failed);
        assert_eq!(
            server.summary.kind,
            task::Kind::Command {
                command: "npm run dev".into()
            }
        );
        assert_eq!(
            server.summary.origin.as_ref().unwrap().turn_id.as_deref(),
            Some("turn-1")
        );
        assert_eq!(
            server.summary.progress.as_ref().unwrap().duration_ms,
            Some(61_500)
        );
        assert_eq!(
            server.output.as_deref(),
            Some("ready on :3000\nGET / 200\n")
        );
        assert_eq!(
            tasks[1].summary.output,
            task::OutputAvailability::Unavailable
        );
        assert!(
            read(&folder.join("missing.jsonl"), "root")
                .unwrap()
                .is_empty()
        );
        std::fs::remove_dir_all(folder).unwrap();
    }
}
