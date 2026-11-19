//! Answer an agent's permission requests from the terminal: run one prompt in
//! a session that forwards requests, and ask before each tool call.
//!
//! ```sh
//! cargo run --example approve -- claude --prompt "Create hello.txt containing hi"
//! cargo run --example approve -- claude --cwd /tmp/demo --yes --prompt "Create hello.txt containing hi"
//! ```
//!
//! Answer `y` to allow once, `a` to allow similar calls for the rest of the
//! session (when the agent offers it), `n` to deny, or `c` to deny and stop
//! the turn. `--yes` allows every request without asking.

use std::io::Write;
use std::path::PathBuf;

use stubb::providers::claude::{Claude, ClaudeConfig};
use stubb::request::{Decision, Kind, Request};
use stubb::{Provider, Session, session, turn};
use tokio::io::{AsyncBufReadExt, BufReader, Lines, Stdin};

#[tokio::main]
async fn main() -> stubb::Result<()> {
    let mut args = std::env::args().skip(1);
    let provider: Box<dyn Provider> = match args.next().as_deref() {
        Some("claude") => Box::new(Claude::new(ClaudeConfig::default())),
        other => {
            eprintln!("unknown provider {other:?}; requests are forwarded for claude");
            std::process::exit(2);
        }
    };
    let (mut prompt, mut cwd, mut yes) = (None, None, false);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--prompt" => prompt = args.next(),
            "--cwd" => cwd = args.next().map(PathBuf::from),
            "--yes" => yes = true,
            other => {
                eprintln!("unknown option {other}");
                std::process::exit(2);
            }
        }
    }
    let Some(prompt) = prompt else {
        eprintln!("--prompt is required");
        std::process::exit(2);
    };
    let cwd = match cwd {
        Some(cwd) => cwd,
        None => std::env::current_dir()?,
    };

    // Forwarding is opt-in: without it, Stubb declines requests itself.
    let live = provider
        .start_session(session::Spec {
            cwd,
            settings: session::Settings {
                forward_requests: true,
                ..session::Settings::default()
            },
        })
        .await?;
    let mut events = live.subscribe();
    let mut answers = BufReader::new(tokio::io::stdin()).lines();
    let receipt = live.send(turn::Input::text(&prompt)).await?;

    loop {
        let Ok(event) = events.recv().await else {
            break;
        };
        match event {
            session::Event::ItemDelta {
                delta: turn::Delta::Text { text },
                ..
            } => {
                print!("{text}");
                std::io::stdout().flush()?;
            }
            // The agent waits until the request is answered or withdrawn.
            session::Event::Request { request } => {
                let decision = if yes {
                    Decision::Allow
                } else {
                    ask(&request, &mut answers).await?
                };
                println!("  → {decision:?}");
                answer(&live, &request, decision).await?;
            }
            session::Event::RequestResolved { id, outcome } => {
                println!("  request {id}: {outcome:?}");
            }
            session::Event::TurnEnd { turn } if turn.id == receipt.turn_id => {
                println!("\n[turn {:?}]", turn.status);
                break;
            }
            session::Event::Closed { reason } => {
                eprintln!("\nthe session closed: {reason:?}");
                break;
            }
            _ => {}
        }
    }

    live.close().await?;
    provider.shutdown().await;
    Ok(())
}

async fn ask(request: &Request, answers: &mut Lines<BufReader<Stdin>>) -> stubb::Result<Decision> {
    let (title, offers_session) = match &request.kind {
        Kind::Tool {
            name,
            description,
            input,
            can_allow_for_session,
            ..
        } => (
            format!(
                "{name} {}",
                description.clone().unwrap_or_else(|| input.to_string())
            ),
            *can_allow_for_session,
        ),
        _ => ("an unknown request".to_owned(), false),
    };
    let choices = if offers_session { "y/a/n/c" } else { "y/n/c" };
    loop {
        print!("\nAllow {title}? [{choices}] ");
        std::io::stdout().flush()?;
        let line = answers.next_line().await?.unwrap_or_default();
        match line.trim() {
            "y" => return Ok(Decision::Allow),
            "a" if offers_session => return Ok(Decision::AllowForSession),
            "n" => return Ok(Decision::Deny { message: None }),
            "c" => return Ok(Decision::Cancel),
            _ => {}
        }
    }
}

/// A request can be withdrawn while the person decides; that is not an error.
async fn answer(live: &Session, request: &Request, decision: Decision) -> stubb::Result<()> {
    match live.respond(&request.id, decision).await {
        Err(stubb::Error::RequestNotFound { .. }) => {
            println!("  (the agent withdrew this request)");
            Ok(())
        }
        result => result,
    }
}
