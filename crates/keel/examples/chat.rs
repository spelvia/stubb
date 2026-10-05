//! Chat with an agent from the terminal: start (or resume) a session, send
//! messages, and stream the reply, including the commands the agent runs.
//!
//! ```sh
//! cargo run --example chat -- codex --model gpt-6-luna --effort medium
//! cargo run --example chat -- claude --model claude-sonnet-4-6 --effort medium
//! cargo run --example chat -- pi --model homelab-vllm/swift-1.5-27b-heretic --effort low
//! cargo run --example chat -- codex --resume <session-id>
//! cargo run --example chat -- claude --prompt "Reply with exactly: hello world"   # one turn, then exit
//! ```
//!
//! Keel cannot approve tool use yet, so commands Codex or Claude Code want to
//! run are declined and shown as such. Pi has no approvals and runs them.

use std::io::Write;
use std::path::PathBuf;

use keel::claude::{Claude, ClaudeConfig};
use keel::codex::{Codex, CodexConfig};
use keel::pi::{Pi, PiConfig};
use keel::turn::{Item, ToolDetail};
use keel::{Provider, Session, session, turn};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::broadcast;

#[derive(Default)]
struct Options {
    provider: String,
    cwd: Option<PathBuf>,
    resume: Option<String>,
    prompt: Option<String>,
    settings: session::Settings,
}

#[tokio::main]
async fn main() -> keel::Result<()> {
    let options = parse_args();
    let provider: Box<dyn Provider> = match options.provider.as_str() {
        "codex" => Box::new(Codex::new(CodexConfig::default())),
        "claude" => Box::new(Claude::new(ClaudeConfig::default())),
        "pi" => Box::new(Pi::new(PiConfig::default())),
        other => {
            eprintln!("unknown provider {other:?}; use codex, claude or pi");
            std::process::exit(2);
        }
    };

    // Starting or resuming loads the session into the agent and returns a
    // live handle.
    let live = match &options.resume {
        Some(id) => {
            provider
                .resume_session(id, options.settings.clone())
                .await?
        }
        None => {
            let cwd = match options.cwd.clone() {
                Some(cwd) => cwd,
                None => std::env::current_dir()?,
            };
            provider
                .start_session(session::Spec {
                    cwd,
                    settings: options.settings.clone(),
                })
                .await?
        }
    };
    let info = live.info();
    println!(
        "{} session {} — model {}, effort {}, fast mode {}, in {}",
        provider.id(),
        live.id(),
        info.model,
        info.effort.as_deref().unwrap_or("default"),
        if info.fast_mode { "on" } else { "off" },
        info.cwd.display()
    );

    // Subscribe before sending: events emitted earlier are not replayed.
    let mut events = live.subscribe();

    match &options.prompt {
        Some(prompt) => {
            run_turn(&live, &mut events, prompt).await?;
        }
        None => {
            println!("Type a message and press Enter. /quit to exit.\n");
            let mut lines = BufReader::new(tokio::io::stdin()).lines();
            loop {
                print!("you> ");
                std::io::stdout().flush()?;
                let Some(line) = lines.next_line().await? else {
                    break;
                };
                let line = line.trim();
                if line == "/quit" {
                    break;
                }
                if !line.is_empty() {
                    run_turn(&live, &mut events, line).await?;
                }
            }
        }
    }

    // Closing detaches from the session; its history stays on disk.
    live.close().await?;
    provider.shutdown().await;
    Ok(())
}

/// Sends one message and prints events until that turn completes.
async fn run_turn(
    live: &Session,
    events: &mut broadcast::Receiver<session::Event>,
    text: &str,
) -> keel::Result<()> {
    // The receipt only confirms that the turn started; progress and the
    // outcome arrive as events.
    let receipt = live.send(turn::Input::text(text)).await?;
    let mut usage = None;
    loop {
        let event = match events.recv().await {
            Ok(event) => event,
            // A slow reader can fall behind the buffer and miss events.
            Err(broadcast::error::RecvError::Lagged(missed)) => {
                eprintln!("\n(missed {missed} events)");
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        };
        match event {
            session::Event::ItemDelta {
                delta: turn::Delta::Text { text },
                ..
            } => {
                print!("{text}");
                std::io::stdout().flush()?;
            }
            session::Event::ItemEnd {
                item: Item::AgentMessage { .. },
                ..
            } => println!(),
            session::Event::ItemStart {
                item: Item::ToolCall(call),
                ..
            } => println!("\n▸ {}", describe(&call.detail)),
            session::Event::ItemEnd {
                item: Item::ToolCall(call),
                ..
            } => {
                let exit = match &call.detail {
                    ToolDetail::Command {
                        exit_code: Some(code),
                        ..
                    } => format!(", exit {code}"),
                    _ => String::new(),
                };
                println!("  {:?}{exit}", call.status);
            }
            session::Event::RequestDeclined { summary, .. } => {
                println!("  ✗ declined: {}", summary.unwrap_or_default());
            }
            session::Event::Usage {
                usage: reported, ..
            } => usage = Some(reported),
            session::Event::Error {
                message, retrying, ..
            } => eprintln!(
                "\nerror{}: {message}",
                if retrying { " (retrying)" } else { "" }
            ),
            session::Event::Closed { reason } => {
                eprintln!("\nthe session closed: {reason:?}");
                break;
            }
            session::Event::TurnEnd { turn } if turn.id == receipt.turn_id => {
                let tokens = usage
                    .map(|usage| format!(", {} tokens in the session", usage.total.total))
                    .unwrap_or_default();
                println!("[turn {:?}{tokens}]\n", turn.status);
                if let Some(error) = turn.error {
                    eprintln!("{error}");
                }
                break;
            }
            _ => {}
        }
    }
    Ok(())
}

fn describe(detail: &ToolDetail) -> String {
    match detail {
        ToolDetail::Command { command, .. } => format!("$ {command}"),
        ToolDetail::FileChange { changes } => {
            let paths: Vec<String> = changes
                .iter()
                .map(|change| change.path.display().to_string())
                .collect();
            format!("edit {}", paths.join(", "))
        }
        ToolDetail::Mcp { server, tool, .. } => format!("mcp {server}.{tool}"),
        ToolDetail::Other { name, .. } => name.clone(),
    }
}

fn parse_args() -> Options {
    let mut args = std::env::args().skip(1);
    let mut options = Options {
        provider: args.next().unwrap_or_else(|| "codex".to_owned()),
        ..Options::default()
    };
    while let Some(flag) = args.next() {
        let mut value = || {
            args.next()
                .unwrap_or_else(|| panic!("{flag} needs a value"))
        };
        match flag.as_str() {
            "--model" => options.settings.model = Some(value()),
            "--effort" => options.settings.effort = Some(value()),
            "--fast" => options.settings.fast_mode = Some(true),
            "--cwd" => options.cwd = Some(PathBuf::from(value())),
            "--resume" => options.resume = Some(value()),
            "--prompt" => options.prompt = Some(value()),
            other => {
                eprintln!("unknown option {other}");
                std::process::exit(2);
            }
        }
    }
    options
}
