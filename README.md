# Keel

## Overview

Keel is a Rust library for embedding coding agents in your application. It connects
to Codex, Claude Code, and Pi through their native protocols and exposes a shared
asynchronous API for sessions, streaming, interactive requests, and agent controls.

Use Keel to build chat interfaces, development tools, or agent dashboards.
Keel manages transport and session state; your application handles presentation
and user decisions.

## Installation

Requires **Rust 1.99.0 or newer**. Add Keel to your Rust project:

```sh
cargo add keel@0.1.0
```

Install and authenticate the CLI for each provider you want to use. For Pi,
configure credentials for its model providers. Keel uses these local executables
and their native configuration.

## Features

| Area | What you can do |
| --- | --- |
| Sessions | Start, resume, and list sessions; read stored conversation history |
| Streaming and recovery | Receive messages, tool activity, and turn events; recover attributed activity through sequenced records and snapshots |
| Interactive requests | Answer action approvals, scoped permission grants, structured questions, and Pi extension dialogs asynchronously |
| Permissions and planning | Configure native approval rules and sandbox settings; select plan mode and transition to execution |
| Models and settings | Discover models; select and update model, effort, speed, and permission settings during a session |
| Active input | Steer ongoing work or queue follow-up input through supported native operations |
| Attachments | Send images and supported native file references alongside text |
| Context | Inspect reported context usage and capacity; request native compaction and observe its progress |
| Agents and tasks | Follow subagents and background tasks, read transcripts and paged logs, interrupt turns, and stop individual work |
| Connection and accounts | Inspect CLI availability, version, connection health, and reported authentication status; use native sign-in flows where available |
| Skills and commands | Discover and invoke supported native skills, commands, and prompt templates |
| Usage and limits | Read reported token usage, costs, and rate limits with their native units and scope |

Support depends on the provider, CLI version, model, and session configuration.
Capability reporting identifies supported operations and partial coverage;
unsupported operations return explicit errors. Unreported usage or account data
remains unknown.

## Supported providers

| Provider | Executable | Native protocol |
| --- | --- | --- |
| OpenAI Codex | `codex` | App-server JSON-RPC |
| Claude Code | `claude` | Stream JSON |
| Pi | `pi` | RPC |

Codex and Claude expose different permission, sandbox, and planning controls.
Pi supports extension dialogs, steering, and follow-up queues, but has no built-in
permission system, sandbox, or plan mode. Keel preserves these native differences.

## Documentation and examples

See the [0.1.0 API documentation](https://docs.rs/keel/0.1.0/keel/) for provider
setup, session operations, event handling, and capability details.

Runnable examples are included in the repository:

| Example | Purpose |
| --- | --- |
| [usage](crates/keel/examples/usage.rs) | Read Claude Code account quotas without a model turn |
| [models](crates/keel/examples/models.rs) | Discover models and settings |
| [sessions](crates/keel/examples/sessions.rs) | List sessions and read history |
| [chat](crates/keel/examples/chat.rs) | Start or resume a session and stream replies |
| [observe](crates/keel/examples/observe.rs) | Inspect stored agents, tasks, and logs |

From a repository checkout:

```sh
cargo run --example models -- claude
cargo run --example chat -- codex --prompt "Explain this repository"
```

## Development

```sh
cargo test --workspace --locked
sh .githooks/install
```

[CI](.github/workflows/ci.yml) checks lint, builds/tests, documentation, packaging,
and dependencies. [Dependency scans](.github/workflows/dependencies.yml) and
[Dependabot updates](.github/dependabot.yml) also run weekly.

Real-provider tests are opt-in: `cargo test --test contract -- --ignored --nocapture`.

## License

[MIT](LICENSE)
