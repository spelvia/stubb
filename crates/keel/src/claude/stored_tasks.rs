//! Read-only tasks joined to structured native results and queued notifications.
use std::collections::BTreeMap;
use std::path::PathBuf;

use super::{
    Claude,
    agents::{self, Scope, Stored},
    output,
};
use crate::{Error, Result, agent, task, turn};

#[derive(Clone)]
pub(super) struct Entry {
    pub(super) summary: task::Summary,
    pub(super) tool: Option<String>,
    pub(super) path: Option<PathBuf>,
    pub(super) terminal: bool,
}
impl Entry {
    pub(super) fn binding(&self, root: &str) -> output::Binding {
        output::Binding {
            session: root.into(),
            task: self.summary.id.clone(),
            path: self.path.clone(),
            terminal: self.terminal,
            agent: matches!(self.summary.kind, task::Kind::AgentRun { .. }),
        }
    }
}

pub(super) fn read(stored: &Stored) -> Result<BTreeMap<String, Entry>> {
    let mut tasks = BTreeMap::new();
    let mut commands = BTreeMap::new();
    for turns in stored.turns.values() {
        for t in turns {
            for item in &t.items {
                if let turn::Item::ToolCall(call) = item
                    && let turn::ToolDetail::Command { command, .. } = &call.detail
                {
                    commands.insert(call.id.clone(), command.clone());
                }
            }
        }
    }
    for a in &stored.agents {
        if a.id == stored.root {
            continue;
        }
        tasks.insert(
            a.id.clone(),
            Entry {
                summary: task::Summary {
                    id: a.id.clone(),
                    owner_agent_id: a.origin.as_ref().map(|o| o.agent_id.clone()),
                    parent_id: None,
                    origin: a.origin.clone(),
                    kind: task::Kind::AgentRun {
                        agent_id: a.id.clone(),
                    },
                    description: a.description.clone(),
                    background: a.background,
                    ambient: None,
                    status: match a.status {
                        agent::Status::Completed => task::Status::Completed,
                        agent::Status::Failed => task::Status::Failed,
                        agent::Status::Interrupted => task::Status::Stopped,
                        _ => task::Status::Unknown,
                    },
                    output: task::OutputAvailability::Unavailable,
                    progress: None,
                },
                tool: a.origin.as_ref().and_then(|o| o.item_id.clone()),
                path: None,
                terminal: false,
            },
        );
    }
    let mut entries: Vec<_> = stored.entries.values().flatten().collect();
    entries.sort_by_key(|e| e["timestamp"].as_str().unwrap_or(""));
    for e in entries {
        let r = &e["toolUseResult"];
        if let Some(id) = r["backgroundTaskId"]
            .as_str()
            .or(r["agentId"].as_str())
            .or(r["resumedAgentId"].as_str())
        {
            let blocks = e["message"]["content"].as_array();
            let tool = blocks
                .into_iter()
                .flatten()
                .find(|b| b["type"] == "tool_result")
                .and_then(|b| b["tool_use_id"].as_str());
            let origin = tool.and_then(|t| stored.owners.get(t)).cloned();
            // Do not assign ambiguous structured results to unrelated tools.
            if blocks.is_some_and(|b| b.iter().filter(|b| b["type"] == "tool_result").count() != 1)
                || origin.is_none()
            {
                continue;
            }
            let command = tool.and_then(|t| commands.get(t));
            let agent_run = r["agentId"].is_string() || r["resumedAgentId"].is_string();
            let summary = task::Summary {
                id: id.into(),
                owner_agent_id: origin.as_ref().map(|o| o.agent_id.clone()),
                parent_id: None,
                origin,
                kind: if agent_run {
                    task::Kind::AgentRun {
                        agent_id: id.into(),
                    }
                } else {
                    command.map_or(
                        task::Kind::Other {
                            name: "local_bash".into(),
                        },
                        |c| task::Kind::Command { command: c.clone() },
                    )
                },
                description: r["description"].as_str().map(str::to_owned),
                background: if agent_run {
                    r["isAsync"].as_bool()
                } else {
                    Some(true)
                },
                ambient: None,
                status: match r["status"].as_str() {
                    Some("completed") => task::Status::Completed,
                    Some("failed") => task::Status::Failed,
                    Some("stopped") => task::Status::Stopped,
                    _ => task::Status::Unknown,
                },
                output: if agent_run {
                    task::OutputAvailability::Unavailable
                } else {
                    task::OutputAvailability::Unknown
                },
                progress: if agent_run {
                    Some(task::Progress {
                        total_tokens: r["totalTokens"].as_u64(),
                        tool_uses: r["totalToolUseCount"].as_u64(),
                        duration_ms: r["totalDurationMs"].as_u64(),
                        last_tool_name: None,
                        summary: None,
                    })
                } else {
                    None
                },
            };
            let path = r["outputFile"].as_str().map(PathBuf::from).or_else(|| {
                if agent_run {
                    return None;
                }
                blocks
                    .into_iter()
                    .flatten()
                    .filter_map(|b| b["content"].as_str())
                    .flat_map(str::split_whitespace)
                    .map(|s| s.trim_end_matches('.'))
                    .find(|p| {
                        p.starts_with('/')
                            && p.ends_with(&format!("/{}/tasks/{id}.output", stored.root))
                    })
                    .map(PathBuf::from)
            });
            let mut entry = Entry {
                summary,
                tool: tool.map(str::to_owned),
                path,
                terminal: false,
            };
            if let Some(previous) = tasks.get(id) {
                if entry.summary.description.is_none() {
                    entry.summary.description = previous.summary.description.clone();
                }
                if entry.summary.background.is_none() {
                    entry.summary.background = previous.summary.background;
                }
            }
            tasks.insert(id.into(), entry);
        }
        // This is native queue metadata, not model/user prose. A known task and
        // matching invocation are still required before accepting its outcome.
        if e["type"] == "queue-operation"
            && e["operation"] == "enqueue"
            && let Some(content) = e["content"].as_str()
            && content.trim_start().starts_with("<task-notification>")
        {
            let Some(id) = tag(content, "task-id") else {
                continue;
            };
            let Some(entry) = tasks.get_mut(id) else {
                continue;
            };
            if tag(content, "tool-use-id").is_none()
                || entry.tool.as_deref() != tag(content, "tool-use-id")
            {
                continue;
            }
            match tag(content, "status") {
                Some("completed") => entry.summary.status = task::Status::Completed,
                Some("failed") => entry.summary.status = task::Status::Failed,
                Some("stopped") => entry.summary.status = task::Status::Stopped,
                _ => continue,
            }
            entry.terminal = true;
            if let Some(path) = tag(content, "output-file") {
                entry.path = Some(path.into());
            }
            entry.summary.progress.get_or_insert_default().summary =
                tag(content, "summary").map(str::to_owned);
        }
    }
    for entry in tasks.values_mut() {
        entry.summary.output =
            output::read(entry.binding(&stored.root), Default::default())?.availability;
    }
    Ok(tasks)
}
pub(super) fn tag<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    text.split_once(&format!("<{name}>"))?
        .1
        .split_once(&format!("</{name}>"))
        .map(|(value, _)| value)
}

impl Claude {
    pub async fn list_tasks(&self, root: &str, query: task::Query) -> Result<task::Page> {
        let stored = self.stored(root).await?;
        if let Some(owner) = &query.owner_agent_id {
            stored.member(owner)?;
        }
        let values = crate::files::blocking(move || read(&stored))
            .await?
            .into_values()
            .map(|e| e.summary)
            .filter(|t| {
                query
                    .owner_agent_id
                    .as_ref()
                    .is_none_or(|id| t.owner_agent_id.as_ref() == Some(id))
            })
            .collect();
        let (tasks, next_cursor) = agents::paginate(
            values,
            Scope {
                root: root.into(),
                kind: "tasks".into(),
                owner: query.owner_agent_id,
                parent: None,
                ancestor: None,
            },
            query.limit,
            query.cursor.as_deref(),
        )?;
        Ok(task::Page { tasks, next_cursor })
    }
    pub async fn task_output(
        &self,
        root: &str,
        id: &str,
        query: task::OutputQuery,
    ) -> Result<task::OutputPage> {
        let stored = self.stored(root).await?;
        let root = root.to_owned();
        let id = id.to_owned();
        crate::files::blocking(move || {
            let tasks = read(&stored)?;
            let entry = tasks.get(&id).ok_or_else(|| Error::TaskNotFound {
                session_id: root.clone(),
                id,
            })?;
            output::read(entry.binding(&root), query)
        })
        .await
    }
}
