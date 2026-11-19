//! Inspect Codex permission configuration without starting a model turn.
//!
//! cargo run --example permissions

use stubb::providers::codex::{ApprovalPolicy, Codex, CodexConfig, Permissions, Sandbox};
use stubb::session;

#[tokio::main]
async fn main() -> stubb::Result<()> {
    let provider = Codex::new(CodexConfig::default());
    let live = provider
        .start_session(session::Spec {
            cwd: std::env::current_dir()?,
            settings: session::Settings {
                permissions: Some(
                    Permissions {
                        approval_policy: Some(ApprovalPolicy::OnRequest),
                        sandbox: Some(Sandbox::ReadOnly),
                        ..Default::default()
                    }
                    .into(),
                ),
                ..Default::default()
            },
        })
        .await?;
    println!(
        "{}",
        serde_json::to_string_pretty(&live.info().permissions).expect("permission info serializes")
    );
    live.close().await?;
    provider.shutdown().await;
    Ok(())
}
