//! The smallest complete program: start a Claude Code session in the current
//! directory, send one message, and print the reply as it streams.
//!
//! ```sh
//! cargo run --example quickstart
//! ```

use stubb::claude::{Claude, ClaudeConfig};
use stubb::{session, turn};
use tokio::sync::broadcast::error::RecvError;

#[tokio::main]
async fn main() -> stubb::Result<()> {
    let provider = Claude::new(ClaudeConfig::default());
    let live = provider
        .start_session(session::Spec {
            cwd: std::env::current_dir()?,
            settings: session::Settings::default(),
        })
        .await?;

    // Subscribe before sending: events emitted earlier are not replayed.
    let mut events = live.subscribe();
    let receipt = live
        .send(turn::Input::text(
            "Summarize this repository in one paragraph.",
        ))
        .await?;

    loop {
        let event = match events.recv().await {
            Ok(event) => event,
            Err(RecvError::Lagged(_)) => continue,
            Err(RecvError::Closed) => break,
        };
        match event {
            session::Event::ItemDelta {
                delta: turn::Delta::Text { text },
                ..
            } => print!("{text}"),
            session::Event::TurnEnd { turn } if turn.id == receipt.turn_id => break,
            _ => {}
        }
    }
    println!();

    live.close().await?;
    provider.shutdown().await;
    Ok(())
}
