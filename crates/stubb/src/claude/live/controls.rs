//! Native stop receipts, scoped to the captured execution rather than a context.

use serde_json::{Value, json};

use super::state::State;
use crate::{Error, Result, control, observation as obs, session, task, turn};
use tokio::sync::broadcast;

type Records = broadcast::Sender<obs::Record>;

#[derive(Clone, PartialEq, Eq)]
pub(super) enum Scope {
    Root {
        turn: String,
    },
    Task {
        id: String,
        generation: u64,
        tool: Option<String>,
    },
}

pub(super) struct Pending {
    pub(super) receipt: control::Receipt,
    scope: Option<Scope>,
    acknowledged: bool,
    outcome: Option<control::Disposition>,
}

pub(in crate::claude) struct Request {
    pub(in crate::claude) id: String,
    pub(in crate::claude) native: Option<Value>,
}

impl State {
    pub(super) fn begin_root_stop(&mut self, turn: &str, records: &Records) -> Result<Request> {
        if self.root.current.as_deref() != Some(turn) || self.closed {
            return Err(Error::Unsupported {
                operation: "interrupt_stale_turn",
            });
        }
        self.begin_stop(
            control::Target::Agent {
                id: self.root.agent.id.clone(),
            },
            records,
        )
    }

    pub(super) fn begin_stop(
        &mut self,
        target: control::Target,
        records: &Records,
    ) -> Result<Request> {
        let (scope, finished) = match &target {
            control::Target::Agent { id } if id == &self.root.agent.id => (
                self.root
                    .current
                    .as_ref()
                    .filter(|_| !self.closed)
                    .map(|turn| Scope::Root { turn: turn.clone() }),
                matches!(
                    self.root.agent.status,
                    crate::agent::Status::Completed
                        | crate::agent::Status::Failed
                        | crate::agent::Status::Interrupted
                ),
            ),
            control::Target::Agent { id } => {
                if !self.children.contains_key(id) {
                    return Err(Error::AgentNotFound {
                        session_id: self.root.agent.id.clone(),
                        id: id.clone(),
                    });
                }
                self.task_scope(id)
            }
            control::Target::Task { id } => {
                if !self.tasks.entries.contains_key(id) {
                    return Err(Error::TaskNotFound {
                        session_id: self.root.agent.id.clone(),
                        id: id.clone(),
                    });
                }
                self.task_scope(id)
            }
        };
        if let Some(existing) = self.controls.values().find(|p| {
            p.receipt.target == target
                && p.scope == scope
                && p.receipt.is_pending()
                && scope.is_some()
        }) {
            return Ok(Request {
                id: existing.receipt.id.clone(),
                native: None,
            });
        }
        let turn_id = scope.as_ref().and_then(|s| match s {
            Scope::Root { turn } => Some(turn.clone()),
            Scope::Task { tool, .. } => tool.clone(),
        });
        let mut receipt = control::Receipt {
            id: uuid::Uuid::new_v4().to_string(),
            target,
            turn_id,
            disposition: if scope.is_some() {
                control::Disposition::Requested
            } else if finished {
                control::Disposition::AlreadyFinished
            } else {
                control::Disposition::NotRunning
            },
            message: None,
        };
        let shared = scope.as_ref().and_then(|scope| {
            self.controls
                .values()
                .find(|p| p.scope.as_ref() == Some(scope) && p.receipt.is_pending())
        });
        let acknowledged = shared.is_some_and(|p| p.acknowledged);
        let duplicate = shared.is_some();
        if acknowledged {
            receipt.disposition = control::Disposition::Accepted;
        }
        let native = if duplicate {
            None
        } else {
            scope.as_ref().map(|s| match s {
                Scope::Root { .. } => json!({"subtype":"interrupt"}),
                Scope::Task { id, .. } => json!({"subtype":"stop_task","task_id":id}),
            })
        };
        let id = receipt.id.clone();
        self.publish(
            records,
            None,
            obs::Event::Control {
                receipt: receipt.clone(),
            },
        );
        self.controls.insert(
            id.clone(),
            Pending {
                receipt,
                scope,
                acknowledged,
                outcome: None,
            },
        );
        Ok(Request { id, native })
    }

    fn task_scope(&self, id: &str) -> (Option<Scope>, bool) {
        let Some(e) = self.tasks.entries.get(id) else {
            return (None, false);
        };
        let finished = super::tasks::is_terminal(e.summary.status);
        let active = !self.closed
            && e.generation > 0
            && e.tool.is_some()
            && !finished
            && matches!(
                e.summary.status,
                task::Status::Pending
                    | task::Status::Running
                    | task::Status::Paused
                    | task::Status::Stopping
            );
        (
            active.then(|| Scope::Task {
                id: id.into(),
                generation: e.generation,
                tool: e.tool.clone(),
            }),
            finished,
        )
    }

    pub(super) fn stop_is_current(&self, id: &str) -> bool {
        let Some(p) = self.controls.get(id) else {
            return false;
        };
        if self.closed || p.outcome.is_some() {
            return false;
        }
        match &p.scope {
            Some(Scope::Root { turn }) => self.root.current.as_ref() == Some(turn),
            Some(Scope::Task {
                id,
                generation,
                tool,
            }) => self.tasks.entries.get(id).is_some_and(|e| {
                e.generation == *generation
                    && e.tool == *tool
                    && !super::tasks::is_terminal(e.summary.status)
            }),
            None => false,
        }
    }

    pub(super) fn stop_receipt(&self, id: &str) -> control::Receipt {
        self.controls[id].receipt.clone()
    }

    pub(super) fn finish_stop(
        &mut self,
        id: &str,
        result: &Result<()>,
        records: &Records,
    ) -> control::Receipt {
        let scope = self.controls[id].scope.clone();
        let ids = self
            .controls
            .iter()
            .filter(|(_, p)| p.scope == scope && p.receipt.is_pending())
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in ids {
            let current = self.stop_is_current(&id);
            let p = self.controls.get_mut(&id).unwrap();
            let previous = p.receipt.clone();
            if let Some(outcome) = p.outcome {
                p.receipt.disposition = outcome;
            } else {
                match result {
                    Ok(()) if current => {
                        p.acknowledged = true;
                        p.receipt.disposition = control::Disposition::Accepted;
                    }
                    Ok(()) => {
                        p.receipt.disposition = control::Disposition::Unconfirmed;
                        p.receipt.message = Some(
                            "The captured execution changed or detached before acknowledgement"
                                .into(),
                        );
                    }
                    Err(Error::Rpc { message, .. }) => {
                        p.receipt.disposition = control::Disposition::Rejected;
                        p.receipt.message = Some(message.clone());
                    }
                    Err(error) => {
                        p.receipt.disposition = control::Disposition::Unconfirmed;
                        p.receipt.message = Some(error.to_string());
                    }
                }
            }
            let replacement = (previous != p.receipt).then(|| p.receipt.clone());
            if let Some(receipt) = replacement {
                self.publish(records, None, obs::Event::Control { receipt });
            }
        }
        // An accepted stop is intent only; do not overwrite a newer generation.
        if self.stop_receipt(id).disposition == control::Disposition::Accepted
            && !self.closed
            && let Some(Scope::Task {
                id: task_id,
                generation,
                tool,
            }) = scope
        {
            let before = self.status();
            if let Some(e) = self.tasks.entries.get_mut(&task_id)
                && e.generation == generation
                && e.tool == tool
                && !super::tasks::is_terminal(e.summary.status)
            {
                e.summary.status = task::Status::Stopping;
                let task = e.summary.clone();
                self.publish(records, None, obs::Event::Task { task });
            }
            self.publish_status(before, records);
        }
        self.stop_receipt(id)
    }

    pub(super) fn root_outcome(&mut self, event: &session::Event, records: &Records) {
        let session::Event::TurnEnd { turn } = event else {
            return;
        };
        let outcome = if turn.status == turn::Status::Interrupted {
            control::Disposition::Confirmed
        } else {
            control::Disposition::AlreadyFinished
        };
        self.resolve_stops(
            |scope| matches!(scope,Scope::Root {turn:id} if id==&turn.id),
            outcome,
            records,
        );
    }

    pub(super) fn task_outcome(&mut self, m: &Value, records: &Records) {
        if m["type"] != "system" || m["subtype"] != "task_notification" {
            return;
        }
        let Some(id) = m["task_id"].as_str() else {
            return;
        };
        let Some(e) = self.tasks.entries.get(id) else {
            return;
        };
        let Some(status) = m["status"].as_str() else {
            return;
        };
        if !matches!(status, "stopped" | "completed" | "failed") {
            return;
        }
        let scope = Scope::Task {
            id: id.into(),
            generation: e.generation,
            tool: e.tool.clone(),
        };
        if m["tool_use_id"]
            .as_str()
            .is_some_and(|tool| e.tool.as_deref() != Some(tool))
        {
            return;
        }
        if e.generation > 1 && !m["tool_use_id"].is_string() {
            return;
        }
        let outcome = if status == "stopped" {
            control::Disposition::Confirmed
        } else {
            control::Disposition::AlreadyFinished
        };
        self.resolve_stops(|candidate| candidate == &scope, outcome, records);
    }

    fn resolve_stops(
        &mut self,
        matches: impl Fn(&Scope) -> bool,
        outcome: control::Disposition,
        records: &Records,
    ) {
        let receipts = self
            .controls
            .values_mut()
            .filter_map(|p| {
                if p.receipt.is_pending() && p.scope.as_ref().is_some_and(&matches) {
                    p.outcome = Some(outcome);
                    p.receipt.disposition = outcome;
                    p.receipt.message = None;
                    Some(p.receipt.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        for receipt in receipts {
            self.publish(records, None, obs::Event::Control { receipt });
        }
    }

    pub(super) fn controls_lost(&mut self, records: &Records) {
        let receipts = self
            .controls
            .values_mut()
            .filter_map(|p| {
                if p.receipt.is_pending() {
                    p.receipt.disposition = control::Disposition::Unconfirmed;
                    p.receipt.message =
                        Some("Attachment closed before native termination evidence".into());
                    Some(p.receipt.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        for receipt in receipts {
            self.publish(records, None, obs::Event::Control { receipt });
        }
    }
}
