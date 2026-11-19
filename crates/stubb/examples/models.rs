//! List the models a provider offers, with their reasoning effort levels and
//! fast-mode support: everything a model picker needs.
//!
//! ```sh
//! cargo run --example models -- codex
//! cargo run --example models -- claude
//! cargo run --example models -- pi
//! ```

use stubb::Provider;
use stubb::catalog::FastMode;
use stubb::claude::{Claude, ClaudeConfig};
use stubb::codex::{Codex, CodexConfig};
use stubb::pi::{Pi, PiConfig};

#[tokio::main]
async fn main() -> stubb::Result<()> {
    let name = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "codex".to_owned());
    let provider: Box<dyn Provider> = match name.as_str() {
        "codex" => Box::new(Codex::new(CodexConfig::default())),
        "claude" => Box::new(Claude::new(ClaudeConfig::default())),
        "pi" => Box::new(Pi::new(PiConfig::default())),
        other => {
            eprintln!("unknown provider {other:?}; use codex, claude or pi");
            std::process::exit(2);
        }
    };

    let catalog = provider.catalog().await?;
    println!(
        "{} {}",
        catalog.provider.id,
        catalog
            .provider
            .version
            .as_deref()
            .unwrap_or("(unknown version)")
    );

    for model in &catalog.models {
        let default = if model.is_default { " (default)" } else { "" };
        println!("\n{}{default}  {}", model.id, model.label);
        if !model.description.is_empty() {
            println!("  {}", model.description);
        }

        // Effort ids are provider-specific; pass them back verbatim.
        let efforts: Vec<String> = model
            .efforts
            .iter()
            .map(|effort| {
                if model.default_effort.as_deref() == Some(&effort.id) {
                    format!("{}*", effort.id)
                } else {
                    effort.id.clone()
                }
            })
            .collect();
        if !efforts.is_empty() {
            println!("  effort: {}", efforts.join(", "));
        }

        match &model.fast_mode {
            FastMode::Available {
                tier_id,
                description,
                default_on,
                ..
            } => println!(
                "  fast mode: available (tier {tier_id}{}){}",
                if *default_on { ", on by default" } else { "" },
                if description.is_empty() {
                    String::new()
                } else {
                    format!(" — {description}")
                }
            ),
            FastMode::Unavailable => println!("  fast mode: unavailable"),
            // Fast mode states added by later versions of Stubb.
            _ => println!("  fast mode: unknown"),
        }
    }

    provider.shutdown().await;
    Ok(())
}
