//! Inspect stored agents, owned history and command logs without opening a session.
//! cargo run --example observe -- claude <session-id> [--agent <id>] [--task <id>]
use stubb::{Provider, observation::agent, observation::task, turn};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let provider: Box<dyn Provider> = match args.next().as_deref() {
        Some("claude") => Box::new(stubb::providers::claude::Claude::new(Default::default())),
        Some("codex") => Box::new(stubb::providers::codex::Codex::new(Default::default())),
        _ => {
            return Err(
                "usage: observe <claude|codex> <session-id> [--agent <id>] [--task <id>]".into(),
            );
        }
    };
    let root = args.next().ok_or("session ID required")?;
    let mut selected = None;
    let mut output = None;
    while let Some(option) = args.next() {
        match option.as_str() {
            "--agent" => selected = Some(args.next().ok_or("agent ID required")?),
            "--task" => output = Some(args.next().ok_or("task ID required")?),
            _ => return Err(format!("unknown option {option}").into()),
        }
    }
    let mut cursor = None;
    loop {
        let page = provider
            .list_agents(
                &root,
                agent::Query {
                    cursor,
                    ..Default::default()
                },
            )
            .await?;
        println!("{}", serde_json::to_string_pretty(&page)?);
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    if let Some(id) = selected {
        let mut cursor = None;
        loop {
            let page = provider
                .list_agent_turns(
                    &root,
                    &id,
                    turn::Query {
                        cursor,
                        ..Default::default()
                    },
                )
                .await?;
            println!("{}", serde_json::to_string_pretty(&page)?);
            cursor = page.next_cursor;
            if cursor.is_none() {
                break;
            }
        }
    }
    match provider.list_tasks(&root, task::Query::default()).await {
        Ok(mut page) => loop {
            println!("{}", serde_json::to_string_pretty(&page)?);
            let Some(cursor) = page.next_cursor else {
                break;
            };
            page = provider
                .list_tasks(
                    &root,
                    task::Query {
                        cursor: Some(cursor),
                        ..Default::default()
                    },
                )
                .await?;
        },
        Err(stubb::Error::Unsupported { operation }) => {
            eprintln!("{operation} is unsupported by {}", provider.id())
        }
        Err(error) => return Err(error.into()),
    }
    if let Some(id) = output {
        // One bounded page. The caller can follow nextCursor; EOF only means
        // no more bytes are currently available, and availability is independent.
        let page = provider
            .task_output(&root, &id, task::OutputQuery::default())
            .await?;
        println!("{}", serde_json::to_string_pretty(&page)?);
    }
    provider.shutdown().await;
    Ok(())
}
