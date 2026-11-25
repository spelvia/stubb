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

    fn mcp_prompts(&self) -> BoxFuture<'_, Result<Vec<crate::mcp::Prompt>>> {
        Box::pin(async {
            Ok(self
                .process
                .commands()
                .iter()
                .filter_map(|row| {
                    let label = row["name"].as_str()?.strip_suffix(" (MCP)")?;
                    let (server, name) = label.split_once(':')?;
                    Some(crate::mcp::Prompt {
                        server: server.into(),
                        name: name.into(),
                        description: row["description"].as_str().unwrap_or_default().into(),
                        arguments: None,
                    })
                })
                .collect())
        })
    }

    fn invoke_mcp_prompt<'a>(
        &'a self,
        server: &'a str,
        name: &'a str,
        mut input: turn::Input,
    ) -> BoxFuture<'a, Result<turn::Receipt>> {
        Box::pin(async move {
            if !self
                .mcp_prompts()
                .await?
                .iter()
                .any(|p| p.server == server && p.name == name)
            {
                return Err(Error::InvalidSetting {
                    setting: "prompt",
                    value: name.into(),
                    reason: "MCP prompt unavailable".into(),
                });
            }
            // Claude dispatches this to prompts/get on its existing server connection.
            input.text = format!("/mcp__{server}__{name} {}", input.text)
                .trim_end()
                .to_owned();
            self.send(input).await
        })
    }

    fn skills(&self) -> BoxFuture<'_, Result<Vec<session::Skill>>> {
        Box::pin(async {
            let commands = self.process.commands();
            let skills = commands.iter().filter(|c| {
                let name = c["name"].as_str().unwrap_or_default();
                c["builtin"] != true
                    && !name.contains(char::is_whitespace)
                    && !name.starts_with("mcp__")
                    && !commands
                        .iter()
                        .any(|entry| entry["builtin"] == true && entry["name"] == name)
                    && (commands.iter().any(|entry| entry["builtin"] == true)
                        || ["(user)", "(project)", "(plugin)"]
                            .iter()
                            .any(|s| c["description"].as_str().unwrap_or_default().ends_with(s)))
            });
            Ok(skills
                .filter_map(|c| {
                    let name = c["name"].as_str()?;
                    Some(session::Skill::new(
                        name.into(),
                        name.into(),
                        c["description"].as_str().unwrap_or_default().into(),
                    ))
                })
                .collect())
        })
    }

    fn invoke_skill<'a>(
        &'a self,
        id: &'a str,
        input: turn::Input,
    ) -> BoxFuture<'a, Result<turn::Receipt>> {
        Box::pin(async move {
            let skill = self
                .skills()
                .await?
                .into_iter()
                .find(|s| s.id == id)
                .ok_or_else(|| session::Skill::unavailable(id))?;
            self.send(skill.slash_input(input)).await
        })
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
