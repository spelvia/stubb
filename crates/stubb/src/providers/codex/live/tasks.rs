//! Running-only native terminal reconciliation. Absence never establishes exit.
use std::collections::{BTreeMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use super::Attachment;
use crate::providers::codex::output;
use crate::{observation as obs, observation::agent, observation::task, session, turn};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::providers::codex) struct ListParams<'a> {
    pub thread_id: &'a str,
    pub cursor: Option<String>,
    pub limit: u32,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::providers::codex) struct Page {
    pub data: Vec<Terminal>,
    pub next_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(in crate::providers::codex) struct Terminal {
    pub item_id: String,
    pub process_id: String,
    pub command: String,
}

/// Ended commands that are not (yet) known tasks keep their output for a
/// late terminal list; only this many of them do.
const UNTRACKED_OUTPUTS: usize = 32;

#[derive(Default)]
pub(super) struct Tasks {
    entries: BTreeMap<(String, String), Entry>,
    commands: BTreeMap<(String, String), Command>,
    untracked: VecDeque<(String, String)>,
}
struct Entry {
    summary: task::Summary,
    // Native references stay private; public IDs are attachment-scoped UUIDs.
    process_id: String,
}
struct Command {
    origin: Option<agent::Origin>,
    outcome: Option<task::Status>,
    output: Option<task::OutputAvailability>,
    ended: bool,
    /// `None` once evicted.
    buffer: Option<output::Buffer>,
}

impl Default for Command {
    fn default() -> Self {
        Self {
            origin: None,
            outcome: None,
            output: None,
            ended: false,
            // Until its start is seen, earlier output may have been missed.
            buffer: Some(output::Buffer::new(true)),
        }
    }
}

impl Command {
    fn availability(&self) -> task::OutputAvailability {
        let Some(buffer) = &self.buffer else {
            return task::OutputAvailability::Unavailable;
        };
        match self.output {
            Some(task::OutputAvailability::Complete) if buffer.truncated() => {
                task::OutputAvailability::Partial
            }
            Some(output) => output,
            None if !self.ended && !buffer.head_missing => task::OutputAvailability::Partial,
            None => task::OutputAvailability::Unknown,
        }
    }

    /// Adds live output, or reconciles an item's aggregated output with what
    /// was streamed. Offsets already handed out stay valid.
    /// Returns whether a reported snapshot agrees with the retained output.
    fn record(&mut self, started: bool, delta: Option<&str>, last: Option<&str>) -> bool {
        let Some(buffer) = &mut self.buffer else {
            return true;
        };
        if started && buffer.is_empty() {
            buffer.head_missing = false;
        }
        if self.ended {
            return true;
        }
        if let Some(delta) = delta {
            buffer.push(delta);
        }
        if let Some(last) = last {
            if buffer.is_empty() {
                buffer.push(last);
                buffer.head_missing = false;
            } else if !buffer.truncated() && last.starts_with(buffer.text()) {
                let rest = last[buffer.text().len()..].to_owned();
                buffer.push(&rest);
            } else if last.len() > buffer.text().len() || !buffer.text().starts_with(last) {
                // The capture disagrees with the streamed output, which is kept
                // so that cursors stay valid.
                return false;
            }
        }
        true
    }
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
                progress: None,
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
                    .map(Command::availability)
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
        let (turn_id, item_id, outcome, output, delta, snapshot) = match event {
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
                    turn::ToolStatus::InProgress | turn::ToolStatus::Unknown => {
                        task::Status::Unknown
                    }
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
                (
                    turn_id,
                    &tool.id,
                    outcome,
                    availability,
                    None,
                    output.as_deref(),
                )
            }
            session::Event::ItemDelta {
                turn_id,
                item_id,
                delta: turn::Delta::Output { text },
            } => (
                turn_id,
                item_id,
                None,
                Some(task::OutputAvailability::Partial),
                Some(text.as_str()),
                None,
            ),
            _ => return vec![],
        };
        let started = matches!(event, session::Event::ItemStart { .. });
        let ended = matches!(event, session::Event::ItemEnd { .. });
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
        let consistent = command.record(started, delta, snapshot);
        if output.is_some()
            && command.output != Some(task::OutputAvailability::Complete)
            && !(output == Some(task::OutputAvailability::Unavailable)
                && command.output == Some(task::OutputAvailability::Partial))
        {
            command.output = output;
        }
        if ended && !consistent {
            command.output = Some(task::OutputAvailability::Partial);
        }
        command.ended |= ended;
        let availability = command.availability();
        let origin = command.origin.clone();
        let outcome = command.outcome;
        let Some(entry) = self.entries.get_mut(&key) else {
            if ended {
                self.untrack(key);
            }
            return vec![];
        };
        let previous = entry.summary.clone();
        entry.summary.origin = origin;
        if let Some(outcome) = outcome
            && entry.summary.status != task::Status::Stopped
        {
            entry.summary.status = outcome;
        }
        entry.summary.output = availability;
        if entry.summary == previous {
            vec![]
        } else {
            vec![entry.summary.clone()]
        }
    }

    /// Bounds the output kept for ended commands that no list has reported.
    fn untrack(&mut self, key: (String, String)) {
        if self.untracked.contains(&key) {
            return;
        }
        self.untracked.push_back(key);
        while self.untracked.len() > UNTRACKED_OUTPUTS {
            let Some(old) = self.untracked.pop_front() else {
                break;
            };
            if !self.entries.contains_key(&old)
                && let Some(command) = self.commands.get_mut(&old)
            {
                command.buffer = None;
            }
        }
    }

    /// Output of a known task, as the session-scoped page for `query`.
    pub(super) fn output(
        &mut self,
        session: &str,
        id: &str,
        query: task::OutputQuery,
    ) -> Option<crate::Result<task::OutputPage>> {
        let key = self
            .entries
            .iter()
            .find(|(_, entry)| entry.summary.id == id)
            .map(|(key, _)| key.clone())?;
        let command = self.commands.entry(key).or_default();
        let availability = command.availability();
        Some(match &command.buffer {
            Some(buffer) => {
                output::page(buffer.view(session, id, command.ended, availability), query)
            }
            None => Ok(task::OutputPage {
                task_id: id.into(),
                text: String::new(),
                next_cursor: None,
                availability,
                eof: true,
            }),
        })
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

impl Attachment {
    /// Retained output of an observed task. Unknown IDs are `TaskNotFound`.
    pub(in crate::providers::codex) fn task_output(
        &self,
        id: &str,
        query: task::OutputQuery,
    ) -> crate::Result<task::OutputPage> {
        let mut state = self.state.lock().expect("observation lock");
        let session = state.root.agent.id.clone();
        state.tasks.output(&session, id, query).unwrap_or_else(|| {
            Err(crate::Error::TaskNotFound {
                session_id: session.clone(),
                id: id.into(),
            })
        })
    }

    pub(in crate::providers::codex) fn terminal_failure(&self, owner: &str, message: &str) {
        let mut state = self.state.lock().expect("observation lock");
        if state.root.closed {
            return;
        }
        let before = state.aggregate_status();
        for task in state.tasks.lost(Some(owner)) {
            state.publish(&self.records, None, obs::Event::Task { task });
        }
        state.publish_status_change(before, &self.records);
        state.publish(
            &self.records,
            None,
            obs::Event::Activity {
                event: session::Event::Raw {
                    name: "keel/taskMonitoringGap".into(),
                    payload: serde_json::json!({"agentId": owner, "message": message}),
                },
                usage_scope: None,
            },
        );
    }

    pub(in crate::providers::codex) fn set_loaded(&self, id: &str, loaded: bool) {
        if let Some(branch) = self
            .state
            .lock()
            .expect("observation lock")
            .children
            .get_mut(id)
        {
            branch.loaded = loaded;
        }
    }

    pub(in crate::providers::codex) fn terminal_threads(&self) -> Vec<String> {
        let state = self.state.lock().expect("observation lock");
        if state.root.closed {
            return vec![];
        }
        std::iter::once(state.root.agent.id.clone())
            .chain(
                state
                    .children
                    .values()
                    .filter(|branch| !branch.closed && branch.loaded)
                    .map(|branch| branch.agent.id.clone()),
            )
            .collect()
    }

    pub(in crate::providers::codex) fn terminals(&self, owner: &str, terminals: Vec<Terminal>) {
        let mut state = self.state.lock().expect("observation lock");
        if state.root.closed
            || (owner != state.root.agent.id
                && state.children.get(owner).is_none_or(|branch| branch.closed))
        {
            return;
        }
        if !state.announced {
            state.announce(&self.records);
        }
        let before = state.aggregate_status();
        for task in state.tasks.list(owner, terminals) {
            state.publish(&self.records, None, obs::Event::Task { task });
        }
        state.publish_status_change(before, &self.records);
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

    fn start() -> session::Event {
        let session::Event::ItemEnd { turn_id, item } = end() else {
            unreachable!()
        };
        let turn::Item::ToolCall(mut tool) = item else {
            unreachable!()
        };
        tool.status = turn::ToolStatus::InProgress;
        if let turn::ToolDetail::Command {
            output, exit_code, ..
        } = &mut tool.detail
        {
            *output = None;
            *exit_code = None;
        }
        session::Event::ItemStart {
            turn_id,
            item: turn::Item::ToolCall(tool),
        }
    }
    fn delta(text: &str) -> session::Event {
        session::Event::ItemDelta {
            turn_id: "turn".into(),
            item_id: "item".into(),
            delta: turn::Delta::Output { text: text.into() },
        }
    }
    fn end_with(text: Option<&str>) -> session::Event {
        let mut event = end();
        if let session::Event::ItemEnd {
            item: turn::Item::ToolCall(tool),
            ..
        } = &mut event
            && let turn::ToolDetail::Command { output, .. } = &mut tool.detail
        {
            *output = text.map(str::to_owned);
        }
        event
    }
    fn read(tasks: &mut Tasks, id: &str, cursor: Option<String>) -> task::OutputPage {
        tasks
            .output(
                "root",
                id,
                task::OutputQuery {
                    limit: Some(6),
                    cursor,
                },
            )
            .unwrap()
            .unwrap()
    }

    #[test]
    fn streamed_output_is_paged_and_followed_until_the_command_ends() {
        let mut tasks = Tasks::default();
        tasks.event("root", &start());
        let id = tasks.list("root", vec![terminal()])[0].id.clone();
        assert!(
            tasks
                .output("root", "unknown", Default::default())
                .is_none()
        );
        let empty = read(&mut tasks, &id, None);
        assert_eq!((empty.text.as_str(), empty.eof), ("", true));
        assert_eq!(empty.availability, task::OutputAvailability::Partial);
        tasks.event("root", &delta("hello "));
        tasks.event("root", &delta("world"));
        let first = read(&mut tasks, &id, empty.next_cursor);
        assert_eq!((first.text.as_str(), first.eof), ("hello ", false));
        let second = read(&mut tasks, &id, first.next_cursor);
        assert_eq!((second.text.as_str(), second.eof), ("world", true));
        assert!(second.next_cursor.is_some());
        // The final aggregated output extends what was streamed.
        let summary = tasks.event("root", &end_with(Some("hello world\n")))[0].clone();
        assert_eq!(summary.output, task::OutputAvailability::Complete);
        let last = read(&mut tasks, &id, second.next_cursor);
        assert_eq!(last.text, "\n");
        assert!(last.eof && last.next_cursor.is_none());
        assert_eq!(last.availability, task::OutputAvailability::Complete);
        tasks.event("root", &delta("late"));
        assert_eq!(read(&mut tasks, &id, None).text, "hello ");
    }

    #[test]
    fn output_seen_only_from_the_middle_or_disagreeing_with_the_capture_is_partial() {
        let mut tasks = Tasks::default();
        let id = tasks.list("root", vec![terminal()])[0].id.clone();
        assert_eq!(
            read(&mut tasks, &id, None).availability,
            task::OutputAvailability::Unknown
        );
        tasks.event("root", &delta("tail"));
        let summary = tasks.event("root", &end_with(Some("whole tail")))[0].clone();
        assert_eq!(summary.output, task::OutputAvailability::Partial);
        let page = read(&mut tasks, &id, None);
        assert_eq!(page.text, "tail");
        assert_eq!(page.availability, task::OutputAvailability::Partial);

        let mut tasks = Tasks::default();
        tasks.event("root", &start());
        let id = tasks.list("root", vec![terminal()])[0].id.clone();
        tasks.event("root", &delta("streamed"));
        tasks.event("root", &end_with(Some("reformatted")));
        assert_eq!(
            read(&mut tasks, &id, None).availability,
            task::OutputAvailability::Partial
        );
    }

    #[test]
    fn only_recent_untracked_commands_keep_their_output() {
        let mut tasks = Tasks::default();
        tasks.event("root", &end_with(Some("first")));
        for index in 0..UNTRACKED_OUTPUTS {
            let mut event = end_with(Some("other"));
            if let session::Event::ItemEnd {
                item: turn::Item::ToolCall(tool),
                ..
            } = &mut event
            {
                tool.id = format!("other-{index}");
            }
            tasks.event("root", &event);
        }
        let summary = tasks.list("root", vec![terminal()])[0].clone();
        assert_eq!(summary.output, task::OutputAvailability::Unavailable);
        let page = read(&mut tasks, &summary.id, None);
        assert!(page.text.is_empty() && page.eof && page.next_cursor.is_none());
        let mut recent = terminal();
        recent.item_id = "other-0".into();
        let id = tasks.list("root", vec![recent])[0].id.clone();
        assert_eq!(read(&mut tasks, &id, None).text, "other");
    }
}
