//! Native task bookends and live membership; absence never means success.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;

use serde_json::Value;

use crate::{agent, task};

#[derive(Clone)]
pub(super) struct Entry {
    pub(super) summary: task::Summary,
    pub(super) tool: Option<String>,
    pub(super) output_file: Option<PathBuf>,
    pub(super) generation: u64,
    pub(super) output_final: bool,
    started: bool,
    terminal: bool,
}

#[derive(Default)]
pub(super) struct Tasks {
    pub(super) entries: BTreeMap<String, Entry>,
    live: HashSet<String>,
}

impl Tasks {
    pub(super) fn running(&self) -> bool {
        self.entries.values().any(|e| {
            matches!(
                e.summary.status,
                task::Status::Pending
                    | task::Status::Running
                    | task::Status::Paused
                    | task::Status::Stopping
            )
        })
    }

    pub(super) fn summaries(&self) -> Vec<task::Summary> {
        self.entries.values().map(|e| e.summary.clone()).collect()
    }

    fn entry(&mut self, id: &str, kind: &str) -> &mut Entry {
        self.entries.entry(id.into()).or_insert_with(|| Entry {
            summary: task::Summary {
                id: id.into(),
                owner_agent_id: None,
                parent_id: None,
                origin: None,
                kind: if kind == "local_agent" {
                    task::Kind::AgentRun {
                        agent_id: id.into(),
                    }
                } else {
                    task::Kind::Other { name: kind.into() }
                },
                description: None,
                background: None,
                ambient: None,
                status: task::Status::Unknown,
                output: task::OutputAvailability::Unknown,
                progress: None,
            },
            tool: None,
            output_file: None,
            generation: 0,
            output_final: false,
            started: false,
            terminal: false,
        })
    }

    pub(super) fn message(&mut self, m: &Value) {
        match m["subtype"].as_str() {
            Some("task_started") => {
                let Some(id) = text(m, "task_id") else {
                    return;
                };
                let kind = text(m, "task_type").unwrap_or_else(|| {
                    if m["subagent_type"].is_string() {
                        "local_agent".into()
                    } else {
                        "unknown".into()
                    }
                });
                let tool = text(m, "tool_use_id");
                let e = self.entry(&id, &kind);
                if e.started && e.tool == tool {
                    return;
                }
                let reset = !e.terminal || (e.started && e.tool != tool);
                if reset {
                    e.generation += 1;
                    e.terminal = false;
                    e.output_file = None;
                    e.output_final = false;
                    e.summary.status = task::Status::Running;
                    e.summary.output = task::OutputAvailability::Unknown;
                    e.summary.progress = None;
                }
                e.started = true;
                e.tool = tool;
                if kind == "local_agent" {
                    e.summary.kind = task::Kind::AgentRun { agent_id: id };
                }
                e.summary.description = text(m, "description");
                e.summary.background = m.get("is_backgrounded").and_then(Value::as_bool);
                e.summary.ambient = m.get("skip_transcript").and_then(Value::as_bool);
            }
            Some("task_updated") => {
                let Some(id) = text(m, "task_id") else {
                    return;
                };
                let patch = &m["patch"];
                let e = self.entry(&id, "unknown");
                if let Some(value) = text(patch, "description") {
                    e.summary.description = Some(value);
                }
                if let Some(value) = patch.get("is_backgrounded").and_then(Value::as_bool) {
                    e.summary.background = Some(value);
                }
                if !e.terminal
                    && let Some(status) = status(patch["status"].as_str())
                {
                    // Agent contexts can be reused; their terminal notification
                    // carries the execution ID.
                    if !matches!(e.summary.kind, task::Kind::AgentRun { .. })
                        || !is_terminal(status)
                    {
                        e.summary.status = status;
                        e.terminal = is_terminal(status);
                    }
                }
            }
            Some("task_progress") => {
                let Some(id) = text(m, "task_id") else {
                    return;
                };
                let e = self.entry(&id, "unknown");
                if !e.terminal && e.summary.status == task::Status::Unknown {
                    e.summary.status = task::Status::Running;
                }
                if let Some(value) = text(m, "description") {
                    e.summary.description = Some(value);
                }
                if e.tool.is_none() {
                    e.tool = text(m, "tool_use_id");
                }
                update_progress(e, m);
            }
            Some("task_notification") => {
                let Some(id) = text(m, "task_id") else {
                    return;
                };
                let e = self.entry(&id, "unknown");
                if let (Some(expected), Some(actual)) =
                    (e.tool.as_deref(), m["tool_use_id"].as_str())
                {
                    if expected != actual {
                        return;
                    }
                } else if e.generation > 1 {
                    return;
                }
                if let Some(status) = status(m["status"].as_str())
                    && !(e.output_final && e.terminal)
                {
                    e.summary.status = status;
                    e.terminal = is_terminal(status);
                    e.output_final = e.terminal;
                }
                if e.tool.is_none() {
                    e.tool = text(m, "tool_use_id");
                }
                if e.generation == 0 {
                    e.generation = 1;
                }
                update_progress(e, m);
                if let Some(path) = text(m, "output_file") {
                    e.output_file = Some(path.into());
                }
            }
            Some("background_tasks_changed") => {
                let Some(values) = m["tasks"].as_array() else {
                    return;
                };
                let mut current = HashSet::new();
                for value in values {
                    let id = value
                        .as_str()
                        .map(str::to_owned)
                        .or_else(|| text(value, "task_id"));
                    let Some(id) = id else {
                        continue;
                    };
                    current.insert(id.clone());
                    let e = self.entry(&id, value["task_type"].as_str().unwrap_or("unknown"));
                    e.summary.background = Some(true);
                    if let Some(description) = text(value, "description") {
                        e.summary.description = Some(description);
                    }
                    // The level set establishes existence, not an outcome or owner.
                }
                for id in self.live.difference(&current) {
                    if let Some(e) = self.entries.get_mut(id)
                        && !e.terminal
                    {
                        e.summary.status = task::Status::Unknown;
                    }
                }
                self.live = current;
            }
            _ => {}
        }
    }

    pub(super) fn repair(
        &mut self,
        owners: &HashMap<String, agent::Origin>,
        commands: &HashMap<String, String>,
    ) {
        for e in self.entries.values_mut() {
            if let Some(tool) = &e.tool {
                if let Some(origin) = owners.get(tool) {
                    e.summary.origin = Some(origin.clone());
                    e.summary.owner_agent_id = Some(origin.agent_id.clone());
                }
                if let Some(command) = commands.get(tool) {
                    e.summary.kind = task::Kind::Command {
                        command: command.clone(),
                    };
                }
            }
            if matches!(e.summary.kind, task::Kind::AgentRun { .. }) {
                e.summary.output = task::OutputAvailability::Unavailable;
            }
        }
    }

    pub(super) fn result(&mut self, m: &Value, root: &str) {
        let r = &m["tool_use_result"];
        let id = text(r, "backgroundTaskId").or_else(|| text(r, "agentId"));
        let Some(id) = id else {
            return;
        };
        let Some(e) = self.entries.get_mut(&id) else {
            return;
        };
        if let Some(path) = text(r, "outputFile") {
            e.output_file = Some(path.into());
        }
        // Bash's structured result supplies the ID; its native text supplies
        // only the log path. No task identity is inferred from prose.
        if e.output_file.is_none() && r["backgroundTaskId"].is_string() {
            for block in m["message"]["content"].as_array().into_iter().flatten() {
                let Some(content) = block["content"].as_str() else {
                    continue;
                };
                for word in content.split_whitespace() {
                    let path = word.trim_end_matches('.');
                    if path.starts_with('/')
                        && path.ends_with(&format!("/{root}/tasks/{id}.output"))
                    {
                        e.output_file = Some(path.into());
                    }
                }
            }
        }
    }

    pub(super) fn lost(&mut self) {
        for e in self.entries.values_mut() {
            if !e.terminal {
                e.summary.status = task::Status::Unknown;
            }
        }
        self.live.clear();
    }
}

pub(super) fn is_terminal(status: task::Status) -> bool {
    matches!(
        status,
        task::Status::Completed | task::Status::Failed | task::Status::Stopped
    )
}

fn status(value: Option<&str>) -> Option<task::Status> {
    match value {
        Some("pending") => Some(task::Status::Pending),
        Some("running") => Some(task::Status::Running),
        Some("paused") => Some(task::Status::Paused),
        Some("completed") => Some(task::Status::Completed),
        Some("failed") => Some(task::Status::Failed),
        Some("killed" | "stopped") => Some(task::Status::Stopped),
        _ => None,
    }
}
fn text(m: &Value, key: &str) -> Option<String> {
    m.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn update_progress(e: &mut Entry, m: &Value) {
    let progress = e.summary.progress.get_or_insert_default();
    for (field, key) in [
        (&mut progress.total_tokens, "total_tokens"),
        (&mut progress.tool_uses, "tool_uses"),
        (&mut progress.duration_ms, "duration_ms"),
    ] {
        if let Some(value) = m["usage"][key].as_u64() {
            *field = Some(value);
        }
    }
    if let Some(name) = text(m, "last_tool_name") {
        progress.last_tool_name = Some(name);
    }
    if let Some(summary) = text(m, "summary") {
        progress.summary = Some(summary);
    }
}
