//! Read Claude Code account quotas without running a model turn.
use keel::claude::{Claude, ClaudeConfig};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let executable = std::env::args().nth(1).unwrap_or_else(|| "claude".into());
    let provider = Claude::new(ClaudeConfig {
        executable: executable.into(),
        ..ClaudeConfig::default()
    });
    let usage = provider.account_usage().await?;
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
