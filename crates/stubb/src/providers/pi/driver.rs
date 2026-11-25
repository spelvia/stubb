use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::sync::broadcast;

use super::process::PiProcess;
use crate::{BoxFuture, Error, Result, session, turn};

pub(super) struct PiSession {
    pub(super) process: Arc<PiProcess>,
    pub(super) request_timeout: Duration,
}

impl session::Driver for PiSession {
    fn subscribe(&self) -> broadcast::Receiver<session::Event> {
        self.process.subscribe()
    }

    fn skills(&self) -> BoxFuture<'_, Result<Vec<session::Skill>>> {
        Box::pin(async {
            let catalog = self
                .process
                .command(json!({"type":"get_commands"}), self.request_timeout)
                .await?;
            let commands = catalog["commands"]
                .as_array()
                .ok_or_else(|| Error::Unsupported {
                    operation: "skills",
                })?;
            let skills = commands.iter().filter(|c| c["source"] == "skill");
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
                    stderr: "the Pi process for this session has exited".to_owned(),
                });
            }
            let mut images = Vec::with_capacity(input.images.len());
            for image in &input.images {
                images.push(json!({
                    "type": "image",
                    "data": image.base64().await?,
                    "mimeType": image.mime_type,
                }));
            }
            let turn_id = self
                .process
                .send_user(&input.text, images, self.request_timeout)
                .await?;
            Ok(turn::Receipt { turn_id })
        })
    }

    /// Pi aborts whatever is running; there is only ever one turn.
    fn interrupt<'a>(&'a self, _turn_id: &'a str) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            self.process
                .command(json!({ "type": "abort" }), self.request_timeout)
                .await
                .map(|_| ())
        })
    }

    fn close(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            self.process.close().await;
            Ok(())
        })
    }
}
