//! Prompt discovery and retrieval on a connected Model Context Protocol server.
use serde::{Deserialize, Serialize};

/// An MCP prompt as exposed by the current session. Some providers publish
/// only an invocation name and delegate argument parsing to their runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Prompt {
    pub server: String,
    pub name: String,
    pub description: String,
    pub arguments: Option<Vec<Argument>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Argument {
    pub name: String,
    pub description: Option<String>,
    pub required: bool,
}

#[cfg(feature = "mcp")]
pub use connected::*;

#[cfg(feature = "mcp")]
mod connected {
    use super::{Argument, Prompt};
    use crate::{Error, Result};
    use rmcp::{
        RoleClient, ServiceExt, model::GetPromptRequestParams, service::RunningService,
        transport::TokioChildProcess,
    };
    use std::{collections::BTreeMap, path::Path, time::Duration};

    pub use rmcp::model::PromptMessage;

    /// A host-owned stdio connection. Listing and retrieving use the same initialized peer.
    pub struct PromptClient {
        name: String,
        service: RunningService<RoleClient, ()>,
    }

    async fn bounded<T, E: std::fmt::Display>(
        method: &'static str,
        f: impl Future<Output = std::result::Result<T, E>>,
    ) -> Result<T> {
        let timeout = Duration::from_secs(10);
        tokio::time::timeout(timeout, f)
            .await
            .map_err(|_| Error::Timeout(timeout))?
            .map_err(|err| Error::Io(std::io::Error::other(format!("{method}: {err}"))))
    }

    impl PromptClient {
        /// Connect to a host-approved executable. Arguments are passed directly to it.
        pub async fn stdio(
            name: String,
            executable: &Path,
            args: &[String],
            cwd: &Path,
        ) -> Result<Self> {
            let mut command = tokio::process::Command::new(executable);
            command.args(args).current_dir(cwd);
            let transport = TokioChildProcess::new(command)?;
            let service = bounded("initialize", ().serve(transport)).await?;
            Ok(Self { name, service })
        }

        pub async fn prompts(&self) -> Result<Vec<Prompt>> {
            if self
                .service
                .peer_info()
                .is_none_or(|info| info.capabilities.prompts.is_none())
            {
                return Ok(Vec::new());
            }
            Ok(bounded("prompts/list", self.service.list_all_prompts())
                .await?
                .into_iter()
                .map(|p| Prompt {
                    server: self.name.clone(),
                    name: p.name,
                    description: p.description.unwrap_or_default(),
                    arguments: Some(
                        p.arguments
                            .unwrap_or_default()
                            .into_iter()
                            .map(|a| Argument {
                                name: a.name,
                                description: a.description,
                                required: a.required.unwrap_or(false),
                            })
                            .collect(),
                    ),
                })
                .collect())
        }

        /// Return the original ordered messages and content blocks. The host controls delivery.
        pub async fn get(
            &self,
            name: &str,
            arguments: &BTreeMap<String, String>,
        ) -> Result<Vec<PromptMessage>> {
            let params = GetPromptRequestParams::new(name).with_arguments(
                arguments
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone().into()))
                    .collect(),
            );
            Ok(bounded("prompts/get", self.service.get_prompt(params))
                .await?
                .messages)
        }
    }
}

#[cfg(all(test, unix, feature = "mcp"))]
mod tests {
    use super::PromptClient;
    use std::{collections::BTreeMap, path::Path};

    #[tokio::test]
    async fn two_servers_can_serve_the_same_name_with_different_arguments() {
        let fixture = r#"
import json,sys
for line in sys.stdin:
    req=json.loads(line)
    if "id" not in req: continue
    method=req["method"]
    if method=="initialize":
        result={"protocolVersion":req["params"]["protocolVersion"],"capabilities":{"prompts":{}},"serverInfo":{"name":sys.argv[1],"version":"1"}}
    elif method=="prompts/list":
        result={"prompts":[{"name":"audit","arguments":[{"name":"issue","required":True}]}]}
    elif method=="prompts/get":
        result={"messages":[{"role":"user","content":{"type":"text","text":sys.argv[1]+" "+req["params"]["arguments"]["issue"]}}]}
    else: result={}
    print(json.dumps({"jsonrpc":"2.0","id":req["id"],"result":result}),flush=True)
"#;
        for server in ["one", "two"] {
            let client = PromptClient::stdio(
                server.into(),
                Path::new("python3"),
                &["-c".into(), fixture.into(), server.into()],
                Path::new("."),
            )
            .await
            .unwrap();
            let prompts = client.prompts().await.unwrap();
            assert_eq!(prompts[0].server, server);
            assert_eq!(prompts[0].name, "audit");
            assert!(prompts[0].arguments.as_ref().unwrap()[0].required);
            let result = client
                .get(
                    "audit",
                    &BTreeMap::from([("issue".into(), "hello world".into())]),
                )
                .await
                .unwrap();
            assert_eq!(
                serde_json::to_value(result).unwrap()[0]["content"]["text"],
                format!("{server} hello world")
            );
        }
    }
}
