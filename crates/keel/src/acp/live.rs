//! A live ACP session: prompt turns, cancellation, permission answers and
//! mode switching over the session's [`Connection`].

use std::sync::Arc;
use std::time::Duration;

use agent_client_protocol::v1::schema as wire;
use tokio::sync::broadcast;

use super::config;
use super::connection::Connection;
use crate::files::now;
use crate::provider::BoxFuture;
use crate::session::{self, CloseReason};
use crate::{Error, Result, request, turn};

/// How long `session/close` may take before Keel stops the agent anyway.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) struct AcpSession {
    pub(super) connection: Arc<Connection>,
    pub(super) session_id: wire::SessionId,
    pub(super) request_timeout: Duration,
}

/// A finished turn as [`session::Event::TurnEnd`] reports it.
pub(super) fn ended_turn(
    id: String,
    status: turn::Status,
    started_at: i64,
    error: Option<String>,
) -> turn::Turn {
    let completed_at = now();
    turn::Turn {
        id,
        status,
        items: Vec::new(),
        started_at: Some(started_at),
        completed_at: Some(completed_at),
        duration_ms: Some((completed_at - started_at) * 1000),
        error,
    }
}

impl AcpSession {
    fn capabilities(&self) -> &wire::AgentCapabilities {
        &self.connection.init.agent_capabilities
    }

    async fn prompt(&self, input: turn::Input) -> Result<turn::Receipt> {
        if !self.connection.is_alive() {
            return Err(Error::ProcessExited {
                stderr: self.connection.state.closed_reason().unwrap_or_default(),
            });
        }
        if !input.images.is_empty() && !self.capabilities().prompt_capabilities.image {
            return Err(Error::Unsupported {
                operation: "image_input",
            });
        }
        let mut prompt = Vec::with_capacity(input.images.len() + 1);
        for image in &input.images {
            prompt.push(wire::ContentBlock::Image(wire::ImageContent::new(
                image.base64().await?,
                image.mime_type.clone(),
            )));
        }
        prompt.push(wire::ContentBlock::Text(wire::TextContent::new(
            input.text.clone(),
        )));

        let state = &self.connection.state;
        let turn_id = uuid::Uuid::new_v4().to_string();
        if !state.begin_turn(&turn_id) {
            return Err(Error::Rpc {
                method: "session/prompt".to_owned(),
                code: -32600,
                message: "a turn is already running in this session".to_owned(),
            });
        }
        state.emit(session::Event::Status {
            status: session::Status::Active,
        });
        state.emit(session::Event::TurnStart {
            turn_id: turn_id.clone(),
        });
        state.emit(session::Event::ItemEnd {
            turn_id: turn_id.clone(),
            item: turn::Item::UserMessage {
                id: turn_id.clone(),
                content: turn::user_content(&input),
            },
        });

        let connection = Arc::clone(&self.connection);
        let request = wire::PromptRequest::new(self.session_id.clone(), prompt);
        tokio::spawn(async move {
            let result = connection.request_untimed("session/prompt", request).await;
            let state = &connection.state;
            let interrupted = state.is_interrupted();
            let (status, error) = match result {
                Ok(response) => stop_status(&response.stop_reason, interrupted),
                Err(_) if interrupted => (turn::Status::Interrupted, None),
                Err(error) => {
                    let message = error.to_string();
                    if connection.is_alive() {
                        state.emit(session::Event::Error {
                            turn_id: state.current_turn(),
                            message: message.clone(),
                            retrying: false,
                        });
                    }
                    (turn::Status::Failed, Some(message))
                }
            };
            // A connection that died has already ended the turn.
            let Some((turn, events)) = state.end_turn(status == turn::Status::Completed) else {
                return;
            };
            state.withdraw_requests();
            for event in events {
                state.emit(event);
            }
            state.emit(session::Event::TurnEnd {
                turn: ended_turn(turn.id, status, turn.started_at, error),
            });
            state.emit(session::Event::Status {
                status: session::Status::Idle,
            });
        });
        Ok(turn::Receipt { turn_id })
    }

    async fn cancel(&self, turn_id: &str) -> Result<()> {
        let state = &self.connection.state;
        if !state.interrupt(turn_id) {
            return Ok(());
        }
        // ACP: the client answers pending permission requests as cancelled.
        state.withdraw_requests();
        self.connection
            .notify(wire::CancelNotification::new(self.session_id.clone()))
    }

    async fn switch_mode(&self, mode_id: &str) -> Result<()> {
        let state = &self.connection.state;
        let options = state.config_options();
        let invalid = |reason: &str| Error::InvalidSetting {
            setting: "mode",
            value: mode_id.to_owned(),
            reason: reason.to_owned(),
        };
        if let Some((option, _, choices)) =
            config::select(&options, &wire::SessionConfigOptionCategory::Mode)
        {
            if !choices.iter().any(|choice| choice.value == mode_id) {
                return Err(invalid("the agent does not offer this mode"));
            }
            let request = wire::SetSessionConfigOptionRequest::new(
                self.session_id.clone(),
                option.id.clone(),
                wire::SessionConfigValueId::new(mode_id.to_owned()),
            );
            let response = self
                .connection
                .request("session/set_config_option", request, self.request_timeout)
                .await?;
            state.set_config(None, Some(response.config_options));
            return Ok(());
        }
        let Some(modes) = state.modes() else {
            return Err(Error::Unsupported {
                operation: "set_mode",
            });
        };
        if !modes
            .available_modes
            .iter()
            .any(|mode| *mode.id.0 == *mode_id)
        {
            return Err(invalid("the agent does not offer this mode"));
        }
        let request = wire::SetSessionModeRequest::new(
            self.session_id.clone(),
            wire::SessionModeId::new(mode_id.to_owned()),
        );
        self.connection
            .request("session/set_mode", request, self.request_timeout)
            .await?;
        state.set_current_mode(mode_id);
        Ok(())
    }

    async fn detach(&self) -> Result<()> {
        if !self.connection.is_alive() {
            return Ok(());
        }
        let state = &self.connection.state;
        state.withdraw_requests();
        if self.capabilities().session_capabilities.close.is_some() {
            let close = wire::CloseSessionRequest::new(self.session_id.clone());
            let _ = self
                .connection
                .request("session/close", close, CLOSE_TIMEOUT)
                .await;
        }
        if let Some((turn, events)) = state.end_turn(false) {
            for event in events {
                state.emit(event);
            }
            state.emit(session::Event::TurnEnd {
                turn: ended_turn(turn.id, turn::Status::Interrupted, turn.started_at, None),
            });
        }
        self.connection.shutdown().await;
        state.emit(session::Event::Closed {
            reason: CloseReason::Client,
        });
        Ok(())
    }
}

/// How a prompt's stop reason ends its turn.
fn stop_status(reason: &wire::StopReason, interrupted: bool) -> (turn::Status, Option<String>) {
    match reason {
        wire::StopReason::EndTurn => (turn::Status::Completed, None),
        wire::StopReason::Cancelled => (turn::Status::Interrupted, None),
        wire::StopReason::MaxTokens => (
            turn::Status::Failed,
            Some("the model reached its output token limit".to_owned()),
        ),
        wire::StopReason::MaxTurnRequests => (
            turn::Status::Failed,
            Some("the agent reached its limit of model requests for one turn".to_owned()),
        ),
        wire::StopReason::Refusal => (
            turn::Status::Failed,
            Some("the agent refused to continue".to_owned()),
        ),
        _ if interrupted => (turn::Status::Interrupted, None),
        _ => (turn::Status::Completed, None),
    }
}

impl session::Driver for AcpSession {
    fn subscribe(&self) -> broadcast::Receiver<session::Event> {
        self.connection.state.subscribe()
    }

    fn send(&self, input: turn::Input) -> BoxFuture<'_, Result<turn::Receipt>> {
        Box::pin(self.prompt(input))
    }

    /// Sends `session/cancel`; the turn ends once the agent answers the
    /// prompt, normally with the `cancelled` stop reason.
    fn interrupt<'a>(&'a self, turn_id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(self.cancel(turn_id))
    }

    fn respond<'a>(
        &'a self,
        request_id: &'a str,
        decision: request::Decision,
    ) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let cancel = self.connection.state.respond(request_id, &decision)?;
            if cancel && let Some(turn_id) = self.connection.state.current_turn() {
                self.cancel(&turn_id).await?;
            }
            Ok(())
        })
    }

    fn modes(&self) -> Result<session::Modes> {
        let state = &self.connection.state;
        config::modes(&state.config_options(), state.modes().as_ref())
            .ok_or(Error::Unsupported { operation: "modes" })
    }

    fn set_mode<'a>(&'a self, mode_id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(self.switch_mode(mode_id))
    }

    fn close(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(self.detach())
    }
}
