//! Running-only native terminal reconciliation. Absence never establishes exit.
use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::{agent, session, task, turn};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ListParams<'a> {
    pub thread_id: &'a str,
    pub cursor: Option<String>,
    pub limit: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Page {
    pub data: Vec<Terminal>,
    pub next_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Terminal {
    pub item_id: String,
    pub process_id: String,
    pub command: String,
}

#[derive(Default)]
pub(super) struct Tasks {
    entries: BTreeMap<(String, String), Entry>,
    commands: BTreeMap<(String, String), Command>,
}
struct Entry {
    summary: task::Summary,
    // Native references stay private; public IDs are attachment-scoped UUIDs.
    process_id: String,
}
#[derive(Default)]
struct Command {
    origin: Option<agent::Origin>,
    outcome: Option<task::Status>,
    output: Option<task::OutputAvailability>,
}

impl Tasks {
    pub(super) fn target(&self, id: &str) -> Option<(String, String, String, task::Status)> {
        self.entries
            .iter()
            .find(|(_, entry)| entry.summary.id == id)
            .map(|((owner, item), entry)| {
                (
                    owner.clone(),
                    item.clone(),
                    entry.process_id.clone(),
                    entry.summary.status,
                )
            })
    }
    pub(super) fn status(&mut self, id: &str, status: task::Status) -> Option<task::Summary> {
        let entry = self
            .entries
            .values_mut()
            .find(|entry| entry.summary.id == id)?;
        if entry.summary.status == status {
            return None;
        }
        entry.summary.status = status;
        Some(entry.summary.clone())
    }
    pub(super) fn summaries(&self) -> Vec<task::Summary> {
        self.entries
            .values()
            .map(|entry| entry.summary.clone())
            .collect()
    }

    pub(super) fn running(&self) -> bool {
        self.entries.values().any(|entry| {
            matches!(
                entry.summary.status,
                task::Status::Running | task::Status::Stopping
            )
        })
    }

    pub(super) fn list(&mut self, owner: &str, terminals: Vec<Terminal>) -> Vec<task::Summary> {
        let present: HashSet<_> = terminals
            .iter()
            .map(|terminal| terminal.item_id.clone())
            .collect();
        let mut changed = vec![];
        for terminal in terminals {
            let key = (owner.into(), terminal.item_id.clone());
            let command = self.commands.get(&key);
            let previous_status = self.entries.get(&key).map(|entry| entry.summary.status);
            let summary = task::Summary {
                id: self
                    .entries
                    .get(&key)
                    .map(|entry| entry.summary.id.clone())
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
                owner_agent_id: Some(owner.into()),
                parent_id: None,
                origin: command
                    .and_then(|command| command.origin.clone())
                    .or_else(|| {
                        Some(agent::Origin {
                            agent_id: owner.into(),
                            turn_id: None,
                            item_id: Some(terminal.item_id.clone()),
                        })
                    }),
                kind: task::Kind::Command {
                    command: terminal.command,
                },
                description: None,
                background: Some(true),
                ambient: None,
                // A delayed list cannot resurrect an item that has already ended.
                status: command
                    .and_then(|command| command.outcome)
                    .map(|outcome| {
                        if previous_status == Some(task::Status::Stopped) {
                            task::Status::Stopped
                        } else {
                            outcome
                        }
                    })
                    .unwrap_or_else(|| {
                        if previous_status == Some(task::Status::Stopping) {
                            task::Status::Stopping
                        } else {
                            task::Status::Running
                        }
                    }),
                output: command
                    .and_then(|command| command.output)
                    .unwrap_or(task::OutputAvailability::Unknown),
            };
            if self
                .entries
                .get(&key)
                .is_none_or(|entry| entry.summary != summary)
            {
                changed.push(summary.clone());
            }
            self.entries.insert(
                key,
                Entry {
                    summary,
                    process_id: terminal.process_id,
                },
            );
        }
        for ((agent, item), entry) in &mut self.entries {
            if agent == owner
                && !present.contains(item)
                && matches!(
                    entry.summary.status,
                    task::Status::Running | task::Status::Stopping
                )
            {
                entry.summary.status = task::Status::Unknown;
                changed.push(entry.summary.clone());
            }
        }
        changed
    }

    pub(super) fn event(&mut self, owner: &str, event: &session::Event) -> Vec<task::Summary> {
        let (turn_id, item_id, outcome, output) = match event {
            session::Event::ItemStart {
                turn_id,
                item: turn::Item::ToolCall(tool),
            }
            | session::Event::ItemEnd {
                turn_id,
                item: turn::Item::ToolCall(tool),
            } if matches!(tool.detail, turn::ToolDetail::Command { .. }) => {
                let ended = matches!(event, session::Event::ItemEnd { .. });
                let outcome = ended.then_some(match tool.status {
                    turn::ToolStatus::Completed => task::Status::Completed,
                    turn::ToolStatus::Failed | turn::ToolStatus::Declined => task::Status::Failed,
                    turn::ToolStatus::InProgress => task::Status::Unknown,
                });
                let turn::ToolDetail::Command { output, .. } = &tool.detail else {
                    unreachable!()
                };
                let availability = if ended {
                    Some(if output.is_some() {
                        task::OutputAvailability::Complete
                    } else {
                        task::OutputAvailability::Unavailable
                    })
                } else {
                    output.as_ref().map(|_| task::OutputAvailability::Partial)
                };
                (turn_id, &tool.id, outcome, availability)
            }
            session::Event::ItemDelta {
                turn_id,
                item_id,
                delta: turn::Delta::Output { .. },
            } => (
                turn_id,
                item_id,
                None,
                Some(task::OutputAvailability::Partial),
            ),
            _ => return vec![],
        };
        let key = (owner.into(), item_id.clone());
        let command = self.commands.entry(key.clone()).or_default();
        command.origin = Some(agent::Origin {
            agent_id: owner.into(),
            turn_id: Some(turn_id.clone()),
            item_id: Some(item_id.clone()),
        });
        if outcome.is_some() {
            command.outcome = outcome;
        }
        if output.is_some()
            && command.output != Some(task::OutputAvailability::Complete)
            && !(output == Some(task::OutputAvailability::Unavailable)
                && command.output == Some(task::OutputAvailability::Partial))
        {
            command.output = output;
        }
        let Some(entry) = self.entries.get_mut(&key) else {
            return vec![];
        };
        let previous = entry.summary.clone();
        entry.summary.origin = command.origin.clone();
        if let Some(outcome) = command.outcome
            && entry.summary.status != task::Status::Stopped
        {
            entry.summary.status = outcome;
        }
        if let Some(output) = command.output {
            entry.summary.output = output;
        }
        if entry.summary == previous {
            vec![]
        } else {
            vec![entry.summary.clone()]
        }
    }

    pub(super) fn lost(&mut self, owner: Option<&str>) -> Vec<task::Summary> {
        let mut changed = vec![];
        for ((agent, _), entry) in &mut self.entries {
            if owner.is_none_or(|owner| owner == agent)
                && matches!(
                    entry.summary.status,
                    task::Status::Running | task::Status::Stopping
                )
            {
                entry.summary.status = task::Status::Unknown;
                changed.push(entry.summary.clone());
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn terminal() -> Terminal {
        Terminal {
            item_id: "item".into(),
            process_id: "process".into(),
            command: "sleep 60".into(),
        }
    }
    fn end() -> session::Event {
        session::Event::ItemEnd {
            turn_id: "turn".into(),
            item: turn::Item::ToolCall(turn::ToolCall {
                id: "item".into(),
                status: turn::ToolStatus::Completed,
                duration_ms: None,
                detail: turn::ToolDetail::Command {
                    command: "sleep 60".into(),
                    cwd: None,
                    output: Some("done".into()),
                    exit_code: Some(0),
                },
            }),
        }
    }
    #[test]
    fn identity_ownership_absence_and_late_lists() {
        let mut tasks = Tasks::default();
        let root = tasks.list("root", vec![terminal()])[0].clone();
        let child = tasks.list("child", vec![terminal()])[0].clone();
        assert_ne!(root.id, child.id);
        assert_eq!(tasks.list("root", vec![])[0].status, task::Status::Unknown);
        assert_eq!(
            tasks.event("root", &end())[0].status,
            task::Status::Completed
        );
        assert!(tasks.list("root", vec![terminal()]).is_empty());
        assert_eq!(tasks.summaries()[1].id, root.id);
        assert_eq!(
            tasks.summaries()[1].output,
            task::OutputAvailability::Complete
        );
        assert!(tasks.running());
        assert_eq!(tasks.lost(None)[0].owner_agent_id.as_deref(), Some("child"));
        assert!(!tasks.running());
    }
    #[test]
    fn output_chunks_remain_available_when_final_capture_is_missing() {
        let mut tasks = Tasks::default();
        tasks.list("root", vec![terminal()]);
        let delta = session::Event::ItemDelta {
            turn_id: "turn".into(),
            item_id: "item".into(),
            delta: turn::Delta::Output {
                text: "partial".into(),
            },
        };
        assert_eq!(
            tasks.event("root", &delta)[0].output,
            task::OutputAvailability::Partial
        );
        let mut final_event = end();
        if let session::Event::ItemEnd {
            item: turn::Item::ToolCall(tool),
            ..
        } = &mut final_event
            && let turn::ToolDetail::Command { output, .. } = &mut tool.detail
        {
            *output = None;
        }
        let final_task = tasks.event("root", &final_event)[0].clone();
        assert_eq!(final_task.status, task::Status::Completed);
        assert_eq!(final_task.output, task::OutputAvailability::Partial);
        assert!(tasks.event("root", &delta).is_empty());
    }

    #[test]
    fn final_event_before_discovery_is_preserved() {
        let mut tasks = Tasks::default();
        tasks.event("root", &end());
        let summary = tasks.list("root", vec![terminal()])[0].clone();
        assert_eq!(summary.status, task::Status::Completed);
        assert_eq!(summary.origin.unwrap().turn_id.as_deref(), Some("turn"));
    }
}
