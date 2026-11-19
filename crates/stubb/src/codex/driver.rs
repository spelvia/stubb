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
    async fn stop(&self, target: crate::control::Target) -> Result<crate::control::Receipt> {
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

    fn stop_agent<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<crate::control::Receipt>> {
        Box::pin(self.stop(crate::control::Target::Agent { id: id.into() }))
    }
    fn stop_task<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<crate::control::Receipt>> {
        Box::pin(self.stop(crate::control::Target::Task { id: id.into() }))
    }

    fn set_permissions(
        &self,
        configuration: crate::permission::Configuration,
    ) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let crate::permission::Configuration::Codex(permissions) = configuration else {
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

    fn send(&self, input: turn::Input) -> BoxFuture<'_, Result<turn::Receipt>> {
        Box::pin(async move {
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
                            .collect(),
                    },
                    self.request_timeout,
                )
                .await?;
            Ok(turn::Receipt {
                turn_id: response.turn.id,
            })
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
