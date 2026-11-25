use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::broadcast;

use super::{
    Permissions, live,
    transport::{
        connection::{self, AppServer},
        rpc::EventSink,
        types,
    },
};
use crate::{BoxFuture, Error, Result, session, turn};

pub(super) struct CodexSession {
    pub(super) cwd: std::path::PathBuf,
    pub(super) connection: Arc<AppServer>,
    pub(super) thread_id: String,
    pub(super) attachment: Arc<live::Attachment>,
    pub(super) request_timeout: Duration,
    /// Permissions changed during the session, sent with the next turn
    /// (Codex keeps them for the turns after it).
    pub(super) overrides: std::sync::Mutex<Option<Permissions>>,
    /// Sent with every turn; see [`super::CodexConfig::reasoning_summary`].
    pub(super) reasoning_summary: Option<String>,
}

impl CodexSession {
    async fn list_skills(&self) -> Result<Vec<session::Skill>> {
        #[derive(serde::Deserialize)]
        struct Response {
            data: Vec<Catalog>,
        }
        #[derive(serde::Deserialize)]
        struct Catalog {
            cwd: std::path::PathBuf,
            skills: Vec<Entry>,
        }
        #[derive(serde::Deserialize)]
        struct Entry {
            path: String,
            name: String,
            description: String,
            enabled: bool,
        }

        let response: Response = self
            .connection
            .request(
                "skills/list",
                &serde_json::json!({"cwds": [&self.cwd], "forceReload": true}),
                self.request_timeout,
            )
            .await?;
        Ok(response
            .data
            .into_iter()
            .filter(|c| c.cwd == self.cwd)
            .flat_map(|c| c.skills)
            .filter(|s| s.enabled)
            .map(|s| session::Skill::new(s.path, s.name, s.description))
            .collect())
    }

    async fn start_turn(
        &self,
        input: turn::Input,
        skill: Option<&session::Skill>,
    ) -> Result<turn::Receipt> {
        let overrides = self.overrides.lock().expect("overrides lock").take();
        let response: types::TurnStartResponse = self
            .connection
            .request(
                "turn/start",
                &types::TurnStartParams {
                    approval_policy: overrides.as_ref().and_then(|p| p.approval_policy.clone()),
                    sandbox_policy: overrides.as_ref().and_then(Permissions::turn_sandbox),
                    summary: self.reasoning_summary.as_deref(),
                    thread_id: &self.thread_id,
                    input: input
                        .images
                        .iter()
                        .map(|image| types::UserInput::LocalImage { path: &image.path })
                        .chain([types::UserInput::Text { text: &input.text }])
                        .chain(skill.map(|s| types::UserInput::Skill {
                            name: &s.name,
                            path: &s.id,
                        }))
                        .collect(),
                },
                self.request_timeout,
            )
            .await?;
        Ok(turn::Receipt {
            turn_id: response.turn.id,
        })
    }

    async fn stop(
        &self,
        target: crate::observation::control::Target,
    ) -> Result<crate::observation::control::Receipt> {
        let (receipt, request) = self.attachment.begin_stop(target)?;
        let Some(request) = request else {
            return Ok(receipt);
        };
        let result: Result<bool> = match request {
            live::StopRequest::Agent { id, turn_id } => self
                .connection
                .request::<_, Value>(
                    "turn/interrupt",
                    &types::TurnInterruptParams {
                        thread_id: &id,
                        turn_id: &turn_id,
                    },
                    self.request_timeout,
                )
                .await
                .map(|_| true),
            live::StopRequest::Task {
                owner,
                item_id,
                process_id,
            } => {
                // Check native item/process membership before sending the private
                // reference. The native protocol has no atomic item guard.
                match connection::terminal_list(&self.connection, &owner, self.request_timeout)
                    .await
                {
                    Ok(terminals)
                        if terminals.iter().any(|terminal| {
                            terminal.item_id == item_id && terminal.process_id == process_id
                        }) =>
                    {
                        #[derive(serde::Deserialize)]
                        struct Response {
                            terminated: bool,
                        }
                        self.connection
                            .request::<_, Response>(
                                "thread/backgroundTerminals/terminate",
                                &serde_json::json!({"threadId":owner,"processId":process_id}),
                                self.request_timeout,
                            )
                            .await
                            .map(|response| response.terminated)
                    }
                    Ok(_) => Ok(false),
                    Err(error) => Err(error),
                }
            }
        };
        let receipt = self
            .attachment
            .stop_result(&receipt.id, result.as_ref().copied());
        result.map(|_| receipt)
    }
}

impl session::Driver for CodexSession {
    fn subscribe(&self) -> broadcast::Receiver<session::Event> {
        self.attachment.subscribe()
    }

    fn subscribe_records(&self) -> Result<broadcast::Receiver<crate::observation::Record>> {
        Ok(self.attachment.subscribe_records())
    }

    fn observation_capabilities(&self) -> Result<crate::observation::Capabilities> {
        Ok(self.attachment.capabilities())
    }

    fn snapshot(&self) -> BoxFuture<'_, Result<crate::observation::Snapshot>> {
        Box::pin(async { Ok(self.attachment.snapshot()) })
    }

    fn stop_agent<'a>(
        &'a self,
        id: &'a str,
    ) -> BoxFuture<'a, Result<crate::observation::control::Receipt>> {
        Box::pin(self.stop(crate::observation::control::Target::Agent { id: id.into() }))
    }
    fn stop_task<'a>(
        &'a self,
        id: &'a str,
    ) -> BoxFuture<'a, Result<crate::observation::control::Receipt>> {
        Box::pin(self.stop(crate::observation::control::Target::Task { id: id.into() }))
    }

    fn task_output<'a>(
        &'a self,
        id: &'a str,
        query: crate::observation::task::OutputQuery,
    ) -> BoxFuture<'a, Result<crate::observation::task::OutputPage>> {
        Box::pin(async move { self.attachment.task_output(id, query) })
    }

    fn set_permissions(
        &self,
        configuration: crate::permission::Configuration,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let Some(permissions) = configuration.get::<super::Permissions>()? else {
                return Err(Error::Unsupported {
                    operation: "permission_configuration",
                });
            };
            permissions.validate()?;
            if permissions.profile.is_some() {
                return Err(Error::Unsupported {
                    operation: "permission_profile_during_session",
                });
            }
            *self.overrides.lock().expect("overrides lock") = Some(permissions);
            Ok(())
        })
    }

    fn skills(&self) -> BoxFuture<'_, Result<Vec<session::Skill>>> {
        Box::pin(self.list_skills())
    }

    fn invoke_skill<'a>(
        &'a self,
        id: &'a str,
        input: turn::Input,
    ) -> BoxFuture<'a, Result<turn::Receipt>> {
        Box::pin(async move {
            let skill = self
                .list_skills()
                .await?
                .into_iter()
                .find(|s| s.id == id)
                .ok_or_else(|| Error::InvalidSetting {
                    setting: "skill",
                    value: id.to_owned(),
                    reason: "skill unavailable in this session".into(),
                })?;
            self.start_turn(input, Some(&skill)).await
        })
    }

    fn send(&self, input: turn::Input) -> BoxFuture<'_, Result<turn::Receipt>> {
        Box::pin(self.start_turn(input, None))
    }

    fn respond<'a>(
        &'a self,
        request_id: &'a str,
        decision: crate::request::Decision,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let (pending, result) = self.attachment.answer(request_id, |pending| {
                super::requests::result(request_id, pending, &decision)
            })?;
            self.connection.reply(&pending.native_id, result).await?;
            self.attachment.emit(session::Event::RequestResolved {
                id: request_id.to_owned(),
                outcome: crate::request::Outcome::Answered {
                    decision: decision.clone(),
                },
            });
            if super::requests::interrupts(&pending, &decision)
                && let Some(turn_id) = &pending.turn_id
            {
                self.connection
                    .request::<_, Value>(
                        "turn/interrupt",
                        &types::TurnInterruptParams {
                            thread_id: &pending.thread_id,
                            turn_id,
                        },
                        self.request_timeout,
                    )
                    .await?;
            }
            Ok(())
        })
    }

    fn interrupt<'a>(&'a self, turn_id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.connection
                .request::<_, Value>(
                    "turn/interrupt",
                    &types::TurnInterruptParams {
                        thread_id: &self.thread_id,
                        turn_id,
                    },
                    self.request_timeout,
                )
                .await
                .map(|_| ())
        })
    }

    fn close(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            // A dead connection already announced `Closed { Exited }`.
            if !self.connection.is_alive() {
                self.connection.detach(&self.thread_id, &self.attachment);
                return Ok(());
            }
            self.attachment.emit(session::Event::Closed {
                reason: session::CloseReason::Client,
            });
            if !self.connection.detach(&self.thread_id, &self.attachment) {
                return Ok(());
            }
            self.connection
                .request::<_, Value>(
                    "thread/unsubscribe",
                    &types::ThreadIdParams {
                        thread_id: &self.thread_id,
                    },
                    self.request_timeout,
                )
                .await
                .map(|_| ())
        })
    }
}
