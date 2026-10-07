//! Session-scoped native sidechain discovery and owned history.
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{Claude, history, store};
use crate::{Error, Result, agent, files, turn};

pub(super) struct Stored {
    pub(super) root: String,
    pub(super) agents: Vec<agent::Summary>,
    pub(super) entries: BTreeMap<String, Vec<Value>>,
    pub(super) turns: BTreeMap<String, Vec<turn::Turn>>,
    pub(super) owners: HashMap<String, agent::Origin>,
}

impl Stored {
    pub(super) fn read(file: &store::SessionFile, entries: Vec<Value>) -> Result<Self> {
        let summary = store::summary(file, &entries);
        let root = file.id.clone();
        let mut transcripts = BTreeMap::from([(root.clone(), entries)]);
        let mut metadata = BTreeMap::new();
        let directory = file.path.with_extension("").join("subagents");
        let session_directory = file.path.with_extension("");
        let project = file.path.parent().expect("native project directory");
        if directory.is_dir()
            && session_directory.canonicalize()?.parent() == Some(project.canonicalize()?.as_path())
            && directory.canonicalize()?.parent()
                == Some(session_directory.canonicalize()?.as_path())
        {
            let canonical = directory.canonicalize()?;
            // Only direct native children of this root's sidechain directory.
            for entry in fs::read_dir(&directory)? {
                let path = entry?.path();
                let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                    continue;
                };
                let Some(id) = name
                    .strip_prefix("agent-")
                    .and_then(|n| n.strip_suffix(".jsonl"))
                else {
                    continue;
                };
                if !store::safe_id(id)
                    || id == root
                    || path.canonicalize()?.parent() != Some(canonical.as_path())
                {
                    continue;
                }
                let values = files::read_entries(&path)?;
                // The filename is not sufficient to establish native membership.
                let values: Vec<_> = values
                    .into_iter()
                    .filter(|e| e["sessionId"] == root && e["agentId"] == id)
                    .collect();
                if values.is_empty() {
                    continue;
                }
                let meta_path = directory.join(format!("agent-{id}.meta.json"));
                let meta = if meta_path.is_file()
                    && meta_path.canonicalize()?.parent() == Some(canonical.as_path())
                {
                    serde_json::from_slice(&fs::read(meta_path)?).unwrap_or(Value::Null)
                } else {
                    Value::Null
                };
                metadata.insert(id.to_owned(), meta);
                transcripts.insert(id.to_owned(), values);
            }
        }
        // Structured native results retain discovery even after transcript cleanup.
        let references: Vec<_> = transcripts
            .values()
            .flatten()
            .filter_map(|e| {
                let r = &e["toolUseResult"];
                let id = r["agentId"].as_str()?;
                if !store::safe_id(id) || id == root {
                    return None;
                }
                let blocks = e["message"]["content"].as_array()?;
                let tools: Vec<_> = blocks
                    .iter()
                    .filter(|b| b["type"] == "tool_result")
                    .filter_map(|b| b["tool_use_id"].as_str())
                    .collect();
                (tools.len() == 1).then(|| (id.to_owned(), tools[0].to_owned()))
            })
            .collect();
        for (id, tool) in references {
            transcripts.entry(id.clone()).or_default();
            metadata
                .entry(id)
                .or_insert_with(|| serde_json::json!({"toolUseId":tool}));
        }
        let mut turns = BTreeMap::new();
        let mut owners = HashMap::new();
        for (id, entries) in &transcripts {
            let mut history = if id == &root {
                history::turns(entries)
            } else {
                history::sidechain_turns(entries)
            };
            if id != &root {
                // Live delegated turns use their native invocation tool IDs.
                // Only the first invocation is identified by creation metadata.
                if let Some(first) = history.first_mut()
                    && let Some(tool) = metadata[id]["toolUseId"].as_str()
                {
                    first.id = tool.into();
                }
                for t in &mut history {
                    t.status = turn::Status::InProgress;
                    t.completed_at = None;
                    t.duration_ms = None;
                }
            }
            for t in &history {
                for item in &t.items {
                    if let turn::Item::ToolCall(call) = item {
                        owners.insert(
                            call.id.clone(),
                            agent::Origin {
                                agent_id: id.clone(),
                                turn_id: Some(t.id.clone()),
                                item_id: Some(call.id.clone()),
                            },
                        );
                    }
                }
            }
            turns.insert(id.clone(), history);
        }
        let mut agents = vec![agent::Summary {
            id: root.clone(),
            parent: agent::Parent::Root,
            name: summary.title,
            role: None,
            model: summary.model,
            description: Some(summary.preview),
            background: None,
            status: agent::Status::Unknown,
            origin: None,
            created_at: Some(summary.created_at),
            updated_at: Some(summary.updated_at),
        }];
        for (id, meta) in metadata {
            let entries = &transcripts[&id];
            let origin = meta["toolUseId"]
                .as_str()
                .and_then(|t| owners.get(t))
                .cloned();
            let parent = meta["parentAgentId"]
                .as_str()
                .filter(|p| *p != id && transcripts.contains_key(*p))
                .map(str::to_owned)
                .or_else(|| origin.as_ref().map(|o| o.agent_id.clone()));
            let model = entries.iter().rev().find_map(|e| {
                e["message"]["model"]
                    .as_str()
                    .filter(|m| !m.starts_with('<'))
                    .map(str::to_owned)
            });
            let at = |e: &Value| {
                e["timestamp"]
                    .as_str()
                    .and_then(files::timestamp_millis)
                    .map(|t| t / 1000)
            };
            agents.push(agent::Summary {
                id: id.clone(),
                parent: parent.map_or(agent::Parent::Unknown, |id| agent::Parent::Agent { id }),
                name: None,
                role: meta["agentType"].as_str().map(str::to_owned),
                model,
                description: meta["description"].as_str().map(str::to_owned),
                background: match meta["requestShape"].as_str() {
                    Some("background") => Some(true),
                    Some("foreground") => Some(false),
                    _ => None,
                },
                status: agent::Status::Unknown,
                origin,
                created_at: entries.iter().find_map(at),
                updated_at: entries.iter().rev().find_map(at),
            });
        }
        agents[1..].sort_by_key(|a| {
            (
                transcripts[&a.id]
                    .iter()
                    .find_map(|e| e["timestamp"].as_str().and_then(files::timestamp_millis))
                    .unwrap_or(i64::MAX),
                a.id.clone(),
            )
        });
        let mut stored = Self {
            root,
            agents,
            entries: transcripts,
            turns,
            owners,
        };
        stored.native_outcomes();
        // Rebuild ownership after invocation IDs have been joined.
        for (id, turns) in &stored.turns {
            for t in turns {
                for item in &t.items {
                    if let turn::Item::ToolCall(call) = item
                        && let Some(origin) = stored.owners.get_mut(&call.id)
                    {
                        origin.agent_id = id.clone();
                        origin.turn_id = Some(t.id.clone());
                    }
                }
            }
        }
        for a in &mut stored.agents {
            if let Some(origin) = &a.origin
                && let Some(tool) = &origin.item_id
            {
                a.origin = stored.owners.get(tool).cloned();
            }
        }
        Ok(stored)
    }

    fn native_outcomes(&mut self) {
        let mut entries: Vec<_> = self.entries.values().flatten().collect();
        entries.sort_by_key(|e| e["timestamp"].as_str().unwrap_or(""));
        let mut latest_tools = HashMap::new();
        // Native structured results join a child input's promptId to the
        // invocation tool. Prompt text and message wording establish nothing.
        for entry in &entries {
            let r = &entry["toolUseResult"];
            let Some(id) = r["agentId"].as_str().or(r["resumedAgentId"].as_str()) else {
                continue;
            };
            let blocks = entry["message"]["content"].as_array();
            let tools: Vec<_> = blocks
                .into_iter()
                .flatten()
                .filter(|b| b["type"] == "tool_result")
                .filter_map(|b| b["tool_use_id"].as_str())
                .collect();
            if tools.len() != 1 || !self.owners.contains_key(tools[0]) {
                continue;
            }
            let tool = tools[0];
            latest_tools.insert(id.to_owned(), tool.to_owned());
            if let Some(turns) = self.turns.get_mut(id) {
                let prompt = entry["promptId"].as_str();
                let matching: Vec<_> = self.entries[id]
                    .iter()
                    .filter(|e| {
                        e["type"] == "user" && prompt.is_some() && e["promptId"].as_str() == prompt
                    })
                    .filter_map(|e| e["uuid"].as_str())
                    .collect();
                for t in turns {
                    if matching.contains(&t.id.as_str()) || (t.id == tool) {
                        t.id = tool.into();
                        if let Some(status) = native_status(r["status"].as_str()) {
                            set_outcome(t, status, entry);
                        }
                    }
                }
            }
            if let Some(a) = self.agents.iter_mut().find(|a| a.id == id) {
                a.status = native_status(r["status"].as_str()).unwrap_or(agent::Status::Unknown);
                if a.model.is_none()
                    && let Some(model) = r["resolvedModel"].as_str()
                {
                    a.model = Some(model.into());
                }
            }
        }
        // Only trusted native queue entries can finalize background invocations.
        for entry in entries {
            if entry["type"] != "queue-operation" || entry["operation"] != "enqueue" {
                continue;
            }
            let Some(content) = entry["content"]
                .as_str()
                .filter(|c| c.trim_start().starts_with("<task-notification>"))
            else {
                continue;
            };
            let Some(id) = super::stored_tasks::tag(content, "task-id") else {
                continue;
            };
            let Some(tool) = super::stored_tasks::tag(content, "tool-use-id") else {
                continue;
            };
            let Some(status) = native_status(super::stored_tasks::tag(content, "status")) else {
                continue;
            };
            if let Some(turns) = self.turns.get_mut(id) {
                if let Some(t) = turns.iter_mut().find(|t| t.id == tool) {
                    set_outcome(t, status, entry);
                }
                let current = turns.last().map_or_else(
                    || latest_tools.get(id).is_some_and(|latest| latest == tool),
                    |t| t.id == tool,
                );
                if current && let Some(a) = self.agents.iter_mut().find(|a| a.id == id) {
                    a.status = status;
                }
            }
        }
    }

    pub(super) fn member(&self, id: &str) -> Result<()> {
        if self.entries.contains_key(id) {
            Ok(())
        } else {
            Err(Error::AgentNotFound {
                session_id: self.root.clone(),
                id: id.into(),
            })
        }
    }

    fn descendant(&self, id: &str, ancestor: &str) -> bool {
        let mut current = id;
        let mut visited = HashSet::new();
        while visited.insert(current) {
            let Some(a) = self.agents.iter().find(|a| a.id == current) else {
                return false;
            };
            let agent::Parent::Agent { id: parent } = &a.parent else {
                return false;
            };
            if parent == ancestor {
                return true;
            }
            current = parent;
        }
        false
    }
}

fn native_status(value: Option<&str>) -> Option<agent::Status> {
    match value {
        Some("completed") => Some(agent::Status::Completed),
        Some("failed") => Some(agent::Status::Failed),
        Some("stopped") => Some(agent::Status::Interrupted),
        _ => None,
    }
}
fn set_outcome(t: &mut turn::Turn, status: agent::Status, entry: &Value) {
    t.status = match status {
        agent::Status::Completed => turn::Status::Completed,
        agent::Status::Failed => turn::Status::Failed,
        agent::Status::Interrupted => turn::Status::Interrupted,
        _ => turn::Status::InProgress,
    };
    t.completed_at = entry["timestamp"]
        .as_str()
        .and_then(files::timestamp_millis)
        .map(|at| at / 1000);
    t.duration_ms = None;
}

#[derive(Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct Scope {
    pub(super) root: String,
    pub(super) kind: String,
    pub(super) owner: Option<String>,
    pub(super) parent: Option<String>,
    pub(super) ancestor: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct Cursor {
    scope: Scope,
    offset: usize,
}
fn invalid(value: &str) -> Error {
    Error::InvalidSetting {
        setting: "cursor",
        value: value.into(),
        reason: "Invalid or differently scoped Claude observation cursor".into(),
    }
}
pub(super) fn paginate<T>(
    values: Vec<T>,
    scope: Scope,
    limit: Option<u32>,
    cursor: Option<&str>,
) -> Result<(Vec<T>, Option<String>)> {
    if limit == Some(0) {
        return Err(Error::InvalidSetting {
            setting: "limit",
            value: "0".into(),
            reason: "Must be positive".into(),
        });
    }
    let start = if let Some(value) = cursor {
        let c: Cursor = serde_json::from_str(
            value
                .strip_prefix("keel:claude-history:v1:")
                .ok_or_else(|| invalid(value))?,
        )
        .map_err(|_| invalid(value))?;
        if c.scope != scope || c.offset > values.len() {
            return Err(invalid(value));
        }
        c.offset
    } else {
        0
    };
    let end = start
        .saturating_add(limit.unwrap_or(50).min(500) as usize)
        .min(values.len());
    let next = (end < values.len()).then(|| {
        format!(
            "keel:claude-history:v1:{}",
            serde_json::to_string(&Cursor { scope, offset: end }).expect("cursor serialization")
        )
    });
    Ok((
        values.into_iter().skip(start).take(end - start).collect(),
        next,
    ))
}

impl Claude {
    pub(super) async fn stored(&self, root: &str) -> Result<Stored> {
        let (file, entries) = self.load(root).await?;
        files::blocking(move || Stored::read(&file, entries)).await
    }
    pub async fn list_agents(&self, root: &str, query: agent::Query) -> Result<agent::Page> {
        query.validate()?;
        let stored = self.stored(root).await?;
        if let Some(id) = query.parent_id.as_deref().or(query.ancestor_id.as_deref()) {
            stored.member(id)?;
        }
        let values = stored
            .agents
            .iter()
            .filter(|a| {
                query
                    .parent_id
                    .as_ref()
                    .is_none_or(|p| a.parent == agent::Parent::Agent { id: p.clone() })
                    && query
                        .ancestor_id
                        .as_ref()
                        .is_none_or(|p| stored.descendant(&a.id, p))
            })
            .cloned()
            .collect();
        let (agents, next_cursor) = paginate(
            values,
            Scope {
                root: root.into(),
                kind: "agents".into(),
                owner: None,
                parent: query.parent_id,
                ancestor: query.ancestor_id,
            },
            query.limit,
            query.cursor.as_deref(),
        )?;
        Ok(agent::Page {
            agents,
            next_cursor,
        })
    }
    pub async fn list_agent_turns(
        &self,
        root: &str,
        id: &str,
        query: turn::Query,
    ) -> Result<agent::TurnPage> {
        let mut stored = self.stored(root).await?;
        stored.member(id)?;
        let (turns, next_cursor) = paginate(
            stored.turns.remove(id).unwrap_or_default(),
            Scope {
                root: root.into(),
                kind: "turns".into(),
                owner: Some(id.into()),
                parent: None,
                ancestor: None,
            },
            query.limit,
            query.cursor.as_deref(),
        )?;
        Ok(agent::TurnPage {
            turns: turns
                .into_iter()
                .map(|turn| agent::OwnedTurn {
                    agent_id: id.into(),
                    turn,
                })
                .collect(),
            next_cursor,
        })
    }
}

#[cfg(test)]
mod tests;
