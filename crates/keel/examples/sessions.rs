//! List the stored sessions of a working directory, or print one session's
//! history. Nothing is loaded into the agent, so this is read-only and safe
//! to run on sessions another client has open.
//!
//! ```sh
//! cargo run --example sessions -- codex                 # sessions in the current directory
//! cargo run --example sessions -- claude --cwd /some/repo
//! cargo run --example sessions -- pi --cwd /some/repo
//! cargo run --example sessions -- codex <session-id>    # print that session's turns
//! ```

use std::path::PathBuf;

use keel::claude::{Claude, ClaudeConfig};
use keel::codex::{Codex, CodexConfig};
use keel::pi::{Pi, PiConfig};
use keel::turn::{Item, ToolDetail, UserContent};
use keel::{Provider, session, turn};

#[tokio::main]
async fn main() -> keel::Result<()> {
    let mut args = std::env::args().skip(1);
    let name = args.next().unwrap_or_else(|| "codex".to_owned());
    let mut cwd = std::env::current_dir()?;
    let mut session_id = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--cwd" => cwd = PathBuf::from(args.next().expect("--cwd needs a path")),
            id => session_id = Some(id.to_owned()),
        }
    }

    let provider: Box<dyn Provider> = match name.as_str() {
        "codex" => Box::new(Codex::new(CodexConfig::default())),
        "claude" => Box::new(Claude::new(ClaudeConfig::default())),
        "pi" => Box::new(Pi::new(PiConfig::default())),
        other => {
            eprintln!("unknown provider {other:?}; use codex, claude or pi");
            std::process::exit(2);
        }
    };

    match session_id {
        None => list(provider.as_ref(), cwd).await?,
        Some(id) => show(provider.as_ref(), &id).await?,
    }
    provider.shutdown().await;
    Ok(())
}

async fn list(provider: &dyn Provider, cwd: PathBuf) -> keel::Result<()> {
    println!("{} sessions in {}", provider.id(), cwd.display());
    let mut cursor = None;
    loop {
        let page = provider
            .list_sessions(session::Query {
                cwd: Some(cwd.clone()),
                limit: Some(20),
                cursor,
            })
            .await?;
        for summary in &page.sessions {
            let name = summary.title.as_deref().unwrap_or(&summary.preview);
            println!(
                "\n{}\n  {}\n  model: {}  updated: {} (unix)",
                summary.id,
                one_line(name, 100),
                summary.model.as_deref().unwrap_or("unknown"),
                summary.updated_at
            );
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(())
}

async fn show(provider: &dyn Provider, id: &str) -> keel::Result<()> {
    let summary = provider.read_session(id).await?;
    println!(
        "{}\n{}\n",
        summary.title.as_deref().unwrap_or(&summary.preview),
        summary.cwd.display()
    );

    // History comes in pages of turns, oldest first.
    let mut cursor = None;
    let mut number = 0;
    loop {
        let page = provider
            .list_turns(
                id,
                turn::Query {
                    limit: Some(20),
                    cursor,
                },
            )
            .await?;
        for turn in &page.turns {
            number += 1;
            println!("── turn {number} ({:?})", turn.status);
            for item in &turn.items {
                print_item(item);
            }
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(())
}

fn print_item(item: &Item) {
    match item {
        Item::UserMessage { content, .. } => {
            for part in content {
                if let UserContent::Text { text } = part {
                    println!("you: {}", one_line(text, 200));
                }
            }
        }
        Item::AgentMessage { text, .. } => println!("agent: {}", one_line(text, 200)),
        Item::Thinking { summarized, .. } => println!(
            "  ({})",
            if *summarized {
                "thinking summary"
            } else {
                "thinking"
            }
        ),
        Item::Plan { text, .. } => println!("  plan: {}", one_line(text, 120)),
        Item::ToolCall(call) => {
            let what = match &call.detail {
                ToolDetail::Command {
                    command, exit_code, ..
                } => match exit_code {
                    Some(code) => format!("$ {} → exit {code}", one_line(command, 100)),
                    None => format!("$ {}", one_line(command, 100)),
                },
                ToolDetail::FileChange { changes } => changes
                    .iter()
                    .map(|change| change.path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
                ToolDetail::Mcp { server, tool, .. } => format!("mcp {server}.{tool}"),
                ToolDetail::Other { name, .. } => name.clone(),
                // Tool kinds added by later versions of Keel.
                _ => "tool".to_owned(),
            };
            println!("  [{:?}] {what}", call.status);
        }
        Item::Other { kind, .. } => println!("  ({kind})"),
        // Item types added by later versions of Keel.
        _ => println!("  (unknown item)"),
    }
}

fn one_line(text: &str, max: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.char_indices().nth(max) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line,
    }
}
