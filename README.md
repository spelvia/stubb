# Stubb

[![crates.io](https://img.shields.io/crates/v/stubb.svg)](https://crates.io/crates/stubb)
[![docs.rs](https://img.shields.io/docsrs/stubb)](https://docs.rs/stubb)
[![CI](https://github.com/spelvia/stubb/actions/workflows/ci.yml/badge.svg)](https://github.com/spelvia/stubb/actions/workflows/ci.yml)
[![MSRV 1.99](https://img.shields.io/badge/rustc-1.99%2B-blue.svg)](#installation)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

**A Rust library for embedding coding agents in your application.**

Stubb drives Codex, Claude Code, Pi, and any [Agent Client Protocol](https://agentclientprotocol.com)
(ACP) agent through their native protocols, and gives you one asynchronous API
for sessions, streaming, permission requests, and agent controls. Use it to
build chat interfaces, developer tools, or agent dashboards: Stubb handles the
agent processes and session state, and your application handles presentation
and user decisions.

## Installation

Stubb requires **Rust 1.99 or newer** and a [Tokio](https://tokio.rs) runtime.

```sh
cargo add stubb
cargo add tokio --features macros,rt-multi-thread
```

Stubb talks to the agent CLIs already installed on the machine. Install and sign
in to each one you want to use (`codex`, `claude`, or `pi`). Stubb uses their
existing configuration and credentials.

## Quick start

Start a Claude Code session in the current directory, send a message, and print
the reply as it streams:

```rust
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
```

Swap in `stubb::codex::Codex`, `stubb::pi::Pi`, or `stubb::acp::AcpProvider` to
use another agent. Every provider implements the same `stubb::Provider` trait.

## Supported agents

| Agent | Executable | Protocol |
| --- | --- | --- |
| OpenAI Codex | `codex` | App-server JSON-RPC |
| Claude Code | `claude` | Stream JSON |
| Pi | `pi` | RPC |
| Any ACP agent | Configured per agent, for example `npx -y @google/gemini-cli --acp` | ACP v1 (JSON-RPC over stdio) |

ACP agents are configured at run time with `stubb::acp::AcpProvider`. The
`stubb::registry` module reads the official [ACP registry](https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json)
(a copy is bundled for offline use) and turns an entry into the command that
starts it. Stubb never downloads anything itself.

Agents differ, and Stubb keeps those differences visible rather than hiding them.
Codex and Claude Code have their own permission, sandbox, and planning controls.
Pi supports steering and follow-up queues but has no permission system or
sandbox. Each provider reports which operations it supports. Anything
unsupported returns an explicit error.

## Features

| Area | What you can do |
| --- | --- |
| Sessions | Start, resume, fork, and list sessions; read stored conversation history |
| Streaming | Receive messages, tool activity, and turn events; recover missed activity from sequenced records and snapshots |
| Permission requests | Forward tool permission prompts to your users and answer them asynchronously |
| Permissions and planning | Configure native approval rules and sandbox settings; use plan mode and switch to execution |
| Models and settings | Discover models; change model, effort, speed, and permissions mid-session |
| Active input | Steer a running turn or queue follow-up messages |
| Attachments | Send images and native file references with text |
| Context | Inspect context usage and request native compaction |
| Agents and tasks | Follow subagents and background tasks, read their logs, interrupt turns, and stop individual work |
| Connection and accounts | Check CLI availability, version, and sign-in status; control the environment agent processes start with |
| Skills and commands | Discover and invoke native skills, commands, and prompt templates |
| Usage and limits | Read token usage, costs, rate limits, and Codex or Claude Code account quotas |

## Documentation

- **[API reference on docs.rs](https://docs.rs/stubb)**: every type, provider,
  and capability.
- **Guides**: [agent process lifetime](docs/process-lifetime.md),
  [agent process environment](docs/agent-environment.md), and
  [compatibility and versioning](docs/compatibility.md), including how to store
  event JSON across versions.
- **[Changelog](https://github.com/spelvia/stubb/releases)**: notes for every release.

### Examples

Run any example from a checkout with `cargo run --example <name> -- <args>`:

| Example | What it shows |
| --- | --- |
| [quickstart](crates/stubb/examples/quickstart.rs) | The program above |
| [chat](crates/stubb/examples/chat.rs) | An interactive terminal chat with any provider, including tool activity |
| [approve](crates/stubb/examples/approve.rs) | Answering an agent's permission requests |
| [models](crates/stubb/examples/models.rs) | Discovering models and their settings |
| [sessions](crates/stubb/examples/sessions.rs) | Listing sessions and reading history |
| [observe](crates/stubb/examples/observe.rs) | Inspecting stored subagents, tasks, and logs |
| [usage](crates/stubb/examples/usage.rs) | Reading account quotas without a model turn |
| [permissions](crates/stubb/examples/permissions.rs) | Inspecting Codex permission configuration |
| [probe](crates/stubb/examples/probe.rs) | Detecting which agents are installed |

```sh
cargo run --example chat -- codex --prompt "Explain this repository"
```

## Contributing

Bug reports, feature requests, and pull requests are welcome. See
[CONTRIBUTING.md](CONTRIBUTING.md) to get set up. Report security issues
privately as described in [SECURITY.md](SECURITY.md).

## License

Stubb is licensed under the [MIT License](LICENSE). Bundled third-party assets
are listed in [THIRD_PARTY.md](THIRD_PARTY.md).
