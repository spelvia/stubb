//! Answer an agent's requests from the terminal: run one prompt in a session
//! that forwards requests, and ask before each tool call, answer its
//! questions, approve its plan and fill in its forms.
//!
//! ```sh
//! cargo run --example approve -- claude --prompt "Create hello.txt containing hi"
//! cargo run --example approve -- claude --cwd /tmp/demo --yes --prompt "Create hello.txt containing hi"
//! cargo run --example approve -- claude --prompt "Ask me which language to greet in, then do it"
//! cargo run --example approve -- codex --prompt "Create hello.txt containing hi"
//! ```
//!
//! For a tool call, answer `y` to allow once, `a` to allow similar calls for
//! the rest of the session (when the agent offers it), `n` to deny, or `c` to
//! deny and stop the turn. Questions take option numbers or your own words,
//! plans `y`, an option number or feedback to keep planning. `--yes` allows
//! every tool call and plan without asking.

use std::io::Write;
use std::path::PathBuf;

use stubb::providers::claude::{Claude, ClaudeConfig};
use stubb::providers::codex::{Codex, CodexConfig};
use stubb::request::{Answer, Decision, FieldKind, Kind, Request};
use stubb::{Provider, Session, session, turn};
use tokio::io::{AsyncBufReadExt, BufReader, Lines, Stdin};

#[tokio::main]
async fn main() -> stubb::Result<()> {
    let mut args = std::env::args().skip(1);
    let provider: Box<dyn Provider> = match args.next().as_deref() {
        Some("claude") => Box::new(Claude::new(ClaudeConfig::default())),
        Some("codex") => Box::new(Codex::new(CodexConfig::default())),
        other => {
            eprintln!("unknown provider {other:?}; use claude or codex");
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
                let decision = if yes && !matches!(request.kind, Kind::Question(_) | Kind::Form(_))
                {
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
    let mut read = async |prompt: &str| -> stubb::Result<String> {
        print!("{prompt}");
        std::io::stdout().flush()?;
        Ok(answers
            .next_line()
            .await?
            .unwrap_or_default()
            .trim()
            .to_owned())
    };
    match &request.kind {
        Kind::Question(questions) => {
            let mut chosen = Vec::new();
            for question in &questions.questions {
                println!("\n{}", question.question);
                for (number, option) in question.options.iter().enumerate() {
                    println!("  {}. {}", number + 1, option.label);
                }
                let line = read("Option numbers, or your own answer: ").await?;
                let picked: Option<Vec<String>> = line
                    .split([',', ' '])
                    .filter(|part| !part.is_empty())
                    .map(|part| {
                        let number: usize = part.parse().ok()?;
                        Some(question.options.get(number.checked_sub(1)?)?.label.clone())
                    })
                    .collect();
                chosen.push(match picked {
                    Some(labels) if !labels.is_empty() => Answer::selected(&question.id, labels),
                    _ => Answer::other(&question.id, line),
                });
            }
            Ok(Decision::Answer { answers: chosen })
        }
        Kind::PlanApproval(plan) => {
            println!("\n{}\n", plan.plan);
            for (number, option) in plan.options.iter().enumerate() {
                println!("  {}. {}", number + 1, option.label);
            }
            let line = read("Approve? [y, an option number, or feedback] ").await?;
            let option = line
                .parse::<usize>()
                .ok()
                .and_then(|number| plan.options.get(number.checked_sub(1)?));
            Ok(match (line.as_str(), option) {
                (_, Some(option)) => Decision::Select {
                    option_id: option.id.clone(),
                },
                ("y", _) => Decision::Allow,
                (_, None) => Decision::Deny {
                    message: Some(line),
                },
            })
        }
        Kind::Permission(permission) => {
            println!(
                "\n{}",
                permission
                    .display_name
                    .clone()
                    .unwrap_or_else(|| permission.name.clone())
            );
            for (number, option) in permission.options.iter().enumerate() {
                println!("  {}. {}", number + 1, option.label);
            }
            loop {
                let line = read("Option number: ").await?;
                if let Some(option) = line
                    .parse::<usize>()
                    .ok()
                    .and_then(|number| permission.options.get(number.checked_sub(1)?))
                {
                    return Ok(Decision::Select {
                        option_id: option.id.clone(),
                    });
                }
            }
        }
        Kind::Form(form) => {
            println!("\n{}", form.message);
            if let Some(url) = &form.url {
                read(&format!("Open {url}, then press enter ")).await?;
            }
            let mut values = serde_json::Map::new();
            for field in &form.fields {
                let line = read(&format!(
                    "{}: ",
                    field.title.as_ref().unwrap_or(&field.name)
                ))
                .await?;
                if line.is_empty() {
                    continue;
                }
                let value = match field.kind {
                    FieldKind::Boolean => serde_json::Value::Bool(line == "y" || line == "true"),
                    FieldKind::Number | FieldKind::Integer => {
                        serde_json::from_str(&line).unwrap_or(serde_json::Value::String(line))
                    }
                    FieldKind::MultiSelect => line.split(',').map(str::trim).collect(),
                    _ => serde_json::Value::String(line),
                };
                values.insert(field.name.clone(), value);
            }
            Ok(Decision::Submit { values })
        }
        kind => {
            let (title, offers_session) = match kind {
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
                match read(&format!("\nAllow {title}? [{choices}] "))
                    .await?
                    .as_str()
                {
                    "y" => return Ok(Decision::Allow),
                    "a" if offers_session => return Ok(Decision::AllowForSession),
                    "n" => return Ok(Decision::Deny { message: None }),
                    "c" => return Ok(Decision::Cancel),
                    _ => {}
                }
            }
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
