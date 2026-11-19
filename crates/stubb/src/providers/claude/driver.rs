use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::sync::broadcast;

use super::{live, process::ClaudeProcess};
use crate::{BoxFuture, Error, Result, session, turn};

pub(super) struct ClaudeSession {
    pub(super) process: Arc<ClaudeProcess>,
    pub(super) session_id: String,
    pub(super) request_timeout: Duration,
}

impl session::Driver for ClaudeSession {
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
            let Some(mode) = permissions.mode else {
                return Ok(());
            };
            self.process
                .control(
                    json!({ "subtype": "set_permission_mode", "mode": mode.as_str() }),
                    self.request_timeout,
                )
                .await
                .map(|_| ())
        })
    }

    fn subscribe(&self) -> broadcast::Receiver<session::Event> {
        self.process.subscribe()
    }

    fn subscribe_records(&self) -> Result<broadcast::Receiver<crate::observation::Record>> {
        Ok(self.process.subscribe_records())
    }

    fn observation_capabilities(&self) -> Result<crate::observation::Capabilities> {
        Ok(live::capabilities())
    }

    fn snapshot(&self) -> BoxFuture<'_, Result<crate::observation::Snapshot>> {
        Box::pin(async { Ok(self.process.snapshot()) })
    }

    fn stop_agent<'a>(
        &'a self,
        id: &'a str,
    ) -> BoxFuture<'a, Result<crate::observation::control::Receipt>> {
        Box::pin(async move {
            self.process
                .stop(
                    crate::observation::control::Target::Agent { id: id.into() },
                    self.request_timeout,
                )
                .await
        })
    }
    fn stop_task<'a>(
        &'a self,
        id: &'a str,
    ) -> BoxFuture<'a, Result<crate::observation::control::Receipt>> {
        Box::pin(async move {
            self.process
                .stop(
                    crate::observation::control::Target::Task { id: id.into() },
                    self.request_timeout,
                )
                .await
        })
    }

    fn task_output<'a>(
        &'a self,
        id: &'a str,
        query: crate::observation::task::OutputQuery,
    ) -> BoxFuture<'a, Result<crate::observation::task::OutputPage>> {
        Box::pin(async move { self.process.task_output(id, query).await })
    }

    fn send(&self, input: turn::Input) -> BoxFuture<'_, Result<turn::Receipt>> {
        Box::pin(async move {
            if !self.process.is_alive() {
                return Err(Error::ProcessExited {
                    stderr: "the Claude Code process for this session has exited".to_owned(),
                });
            }
            let turn_id = uuid::Uuid::new_v4().to_string();
            self.process
                .send_user(&self.session_id, &turn_id, &input)
                .await?;
            Ok(turn::Receipt { turn_id })
        })
    }

    fn interrupt<'a>(&'a self, turn_id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.process
                .interrupt_turn(turn_id, self.request_timeout)
                .await
        })
    }

    fn respond<'a>(
        &'a self,
        request_id: &'a str,
        decision: crate::request::Decision,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.process
                .respond(&self.session_id, request_id, decision)
                .await
        })
    }

    fn close(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.process.close().await;
            Ok(())
        })
    }
}
