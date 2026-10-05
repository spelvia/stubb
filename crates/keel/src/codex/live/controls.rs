use super::{Attachment, state::State};
use crate::control::{Disposition as D, Receipt, Target};
use crate::{Error, Result, task};
use crate::{observation as obs, session, turn};
use tokio::sync::broadcast;

pub(super) struct Pending {
    pub(super) receipt: Receipt,
    item_id: Option<String>,
    owner: String,
    exit: Option<Option<i32>>,
}

pub(in crate::codex) enum Request {
    Agent {
        id: String,
        turn_id: String,
    },
    Task {
        owner: String,
        item_id: String,
        process_id: String,
    },
}

impl Attachment {
    pub(in crate::codex) fn begin_stop(
        &self,
        target: Target,
    ) -> Result<(Receipt, Option<Request>)> {
        let mut state = self.state.lock().expect("observation lock");
        if state.root.closed {
            return Err(Error::Unsupported {
                operation: "stop_closed_attachment",
            });
        }
        let before = state.aggregate_status();
        let (owner, item_id, turn_id, request, disposition) = match &target {
            Target::Agent { id } => {
                let branch = if id == &state.root.agent.id {
                    &state.root
                } else {
                    state.children.get(id).ok_or_else(|| Error::AgentNotFound {
                        session_id: state.root.agent.id.clone(),
                        id: id.clone(),
                    })?
                };
                let turn = branch
                    .turns
                    .values()
                    .find(|turn| !turn.ended)
                    .map(|turn| turn.turn.id.clone());
                let request = turn.as_ref().map(|turn_id| Request::Agent {
                    id: id.clone(),
                    turn_id: turn_id.clone(),
                });
                let disposition = if request.is_some() {
                    D::Requested
                } else {
                    D::NotRunning
                };
                (id.clone(), None, turn, request, disposition)
            }
            Target::Task { id } => {
                let (owner, item, process_id, status) =
                    state.tasks.target(id).ok_or_else(|| Error::TaskNotFound {
                        session_id: state.root.agent.id.clone(),
                        id: id.clone(),
                    })?;
                let finished = matches!(
                    status,
                    task::Status::Completed | task::Status::Failed | task::Status::Stopped
                );
                let turn = state
                    .tasks
                    .summaries()
                    .into_iter()
                    .find(|task| &task.id == id)
                    .and_then(|task| task.origin.and_then(|origin| origin.turn_id));
                let request = (!finished).then(|| Request::Task {
                    owner: owner.clone(),
                    item_id: item.clone(),
                    process_id,
                });
                (
                    owner,
                    Some(item),
                    turn,
                    request,
                    if finished {
                        D::AlreadyFinished
                    } else {
                        D::Requested
                    },
                )
            }
        };
        if let Some(existing) = state.controls.values().find(|pending| {
            pending.receipt.target == target
                && pending.receipt.turn_id == turn_id
                && matches!(pending.receipt.disposition, D::Requested | D::Accepted)
        }) {
            return Ok((existing.receipt.clone(), None));
        }
        let receipt = Receipt {
            id: uuid::Uuid::new_v4().to_string(),
            target: target.clone(),
            turn_id,
            disposition,
            message: None,
        };
        state.controls.insert(
            receipt.id.clone(),
            Pending {
                receipt: receipt.clone(),
                item_id,
                owner,
                exit: None,
            },
        );
        if let Target::Task { id } = target
            && request.is_some()
            && let Some(task) = state.tasks.status(&id, task::Status::Stopping)
        {
            state.publish(&self.records, None, obs::Event::Task { task });
        }
        state.publish_status_change(before, &self.records);
        state.publish(
            &self.records,
            None,
            obs::Event::Control {
                receipt: receipt.clone(),
            },
        );
        Ok((receipt, request))
    }

    pub(in crate::codex) fn stop_result(
        &self,
        id: &str,
        result: std::result::Result<bool, &Error>,
    ) -> Receipt {
        let mut state = self.state.lock().expect("observation lock");
        if state.root.closed {
            return state.controls[id].receipt.clone();
        }
        let before = state.aggregate_status();
        let pending = state.controls.get_mut(id).expect("registered control");
        if !matches!(
            pending.receipt.disposition,
            D::Confirmed | D::AlreadyFinished
        ) {
            pending.receipt.disposition = match result {
                Ok(true) if matches!(pending.receipt.target, Target::Task { .. }) => {
                    if pending.exit.is_some_and(|exit| exit != Some(-1)) {
                        D::AlreadyFinished
                    } else {
                        D::Confirmed
                    }
                }
                Ok(true) => D::Accepted,
                Ok(false) => D::Unconfirmed,
                Err(Error::Rpc { .. } | Error::Unsupported { .. }) => D::Rejected,
                Err(_) => D::Unconfirmed,
            };
            pending.receipt.message = result.err().map(ToString::to_string);
        }
        let receipt = pending.receipt.clone();
        let exit = pending.exit;
        if let Target::Task { id } = &receipt.target {
            let status = if receipt.disposition == D::Confirmed && exit == Some(Some(-1)) {
                Some(task::Status::Stopped)
            } else if matches!(receipt.disposition, D::Unconfirmed | D::Rejected) {
                Some(task::Status::Unknown)
            } else {
                None
            };
            if let Some(status) = status
                && let Some(task) = state.tasks.status(id, status)
            {
                state.publish(&self.records, None, obs::Event::Task { task });
            }
        }
        state.publish_status_change(before, &self.records);
        state.publish(
            &self.records,
            None,
            obs::Event::Control {
                receipt: receipt.clone(),
            },
        );
        receipt
    }
}

impl State {
    pub(super) fn control_event(
        &mut self,
        owner: &str,
        event: &session::Event,
        records: &broadcast::Sender<obs::Record>,
    ) {
        let mut updates = vec![];
        let mut stopped = vec![];
        for pending in self.controls.values_mut().filter(|pending| {
            pending.owner == owner
                || (owner == self.root.agent.id && matches!(event, session::Event::Closed { .. }))
        }) {
            let previous = pending.receipt.clone();
            match (&pending.receipt.target, event) {
                (Target::Agent { .. }, session::Event::TurnEnd { turn })
                    if pending.receipt.turn_id.as_deref() == Some(&turn.id)
                        && pending.receipt.is_pending() =>
                {
                    pending.receipt.disposition = if turn.status == turn::Status::Interrupted {
                        D::Confirmed
                    } else {
                        D::AlreadyFinished
                    };
                }
                (
                    Target::Task { id },
                    session::Event::ItemEnd {
                        item: turn::Item::ToolCall(tool),
                        ..
                    },
                ) if pending.item_id.as_deref() == Some(&tool.id) => {
                    if let turn::ToolDetail::Command { exit_code, .. } = tool.detail {
                        pending.exit = Some(exit_code);
                        if pending.receipt.disposition == D::Confirmed && exit_code == Some(-1) {
                            stopped.push(id.clone());
                        } else if exit_code != Some(-1) {
                            pending.receipt.disposition = D::AlreadyFinished;
                        }
                    }
                }
                (_, session::Event::Closed { .. }) if pending.receipt.is_pending() => {
                    pending.receipt.disposition = D::Unconfirmed
                }
                _ => {}
            }
            if previous != pending.receipt {
                updates.push(pending.receipt.clone());
            }
        }
        for id in stopped {
            if let Some(task) = self.tasks.status(&id, task::Status::Stopped) {
                self.publish(records, None, obs::Event::Task { task });
            }
        }
        for receipt in updates {
            self.publish(records, None, obs::Event::Control { receipt });
        }
    }
}
