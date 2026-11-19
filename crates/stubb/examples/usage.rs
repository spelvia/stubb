//! Read Codex or Claude Code account quotas without running a model turn:
//! `cargo run --example usage -- codex [executable]` or `-- claude [executable]`.
use stubb::Provider;
use stubb::claude::{Claude, ClaudeConfig};
use stubb::codex::{Codex, CodexConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let provider_id = args.next().unwrap_or_else(|| "claude".into());
    let executable = args.next().unwrap_or_else(|| provider_id.clone());
    let provider: Box<dyn Provider> = match provider_id.as_str() {
        "codex" => Box::new(Codex::new(CodexConfig {
            executable: executable.into(),
            ..CodexConfig::default()
        })),
        _ => Box::new(Claude::new(ClaudeConfig {
            executable: executable.into(),
            ..ClaudeConfig::default()
        })),
    };
    let usage = provider.account_usage().await;
    provider.shutdown().await;
    let usage = usage?;
    println!(
        "Provider: {} · plan: {}",
        usage.provider,
        usage.plan.as_deref().unwrap_or("unknown")
    );
    for window in usage.windows {
        println!(
            "{}: {:?}% used · resets {}",
            window.label,
            window.used_percent,
            window.resets_at.as_deref().unwrap_or("unknown")
        );
    }
    Ok(())
}
