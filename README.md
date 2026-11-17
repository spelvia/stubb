# Keel

## Overview

Keel is a Rust library for embedding coding agents in your application. It connects
to Codex, Claude Code, and Pi through their native protocols, and to any agent that
speaks the [Agent Client Protocol](https://agentclientprotocol.com) (ACP), and
exposes a shared asynchronous API for sessions, streaming, interactive requests,
and agent controls.

Use Keel to build chat interfaces, development tools, or agent dashboards.
Keel manages transport and session state; your application handles presentation
and user decisions.

## Installation

Requires **Rust 1.99.0 or newer**. Add Keel to your Rust project:

```sh
cargo add keel@0.1.0-rc.1
```

Install and authenticate the CLI for each provider you want to use. For Pi,
configure credentials for its model providers. Keel uses these local executables
and their native configuration.

## Features

| Area | What you can do |
| --- | --- |
| Sessions | Start, resume, and list sessions; read stored conversation history |
| Streaming and recovery | Receive messages, tool activity, and turn events; recover attributed activity through sequenced records and snapshots |
| Interactive requests | Forward tool permission prompts and answer them asynchronously: allow once, allow for the session, deny, or deny and stop (Claude Code; opt in with `Settings::forward_requests`) |
| Permissions and planning | Configure native approval rules and sandbox settings; select plan mode and transition to execution |
| Models and settings | Discover models; select and update model, effort, speed, and permission settings during a session |
| Active input | Steer ongoing work or queue follow-up input through supported native operations |
| Attachments | Send images and supported native file references alongside text |
| Context | Inspect reported context usage and capacity; request native compaction and observe its progress |
| Agents and tasks | Follow subagents and background tasks, read transcripts and paged logs, interrupt turns, and stop individual work |
| Connection and accounts | Inspect CLI availability, version, connection health, and reported authentication status; use native sign-in flows where available; agent processes die with the host, start with the host's environment or only the variables it allows, and the shared Codex app-server closes when idle |
| Skills and commands | Discover and invoke supported native skills, commands, and prompt templates |
| Usage and limits | Read reported token usage, costs, and rate limits with their native units and scope; read Codex and Claude Code account quotas without a model turn |

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
| Any ACP agent | Configured per agent (for example `npx -y @google/gemini-cli --acp`) | ACP v1 (JSON-RPC over stdio) |

ACP agents are configured at run time with `keel::acp::AcpProvider`. The
`keel::registry` module parses the official [ACP registry](https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json)
(a copy is bundled for offline use) and turns an entry into the command that starts
it, through `npx`, `uvx`, or a binary the host has downloaded. Keel performs no
downloads itself. `keel::acp::probe` reports whether an agent is installed and,
optionally, its name, version and sign-in methods from `initialize`.

Codex and Claude expose different permission, sandbox, and planning controls.
Pi supports extension dialogs, steering, and follow-up queues, but has no built-in
permission system, sandbox, or plan mode. ACP agents report their models, efforts
and modes per session, ask for permissions through ACP, and read or write files
only within the session's folder; Keel offers them no terminals. Keel preserves
these native differences.

## Documentation and examples

See the [0.1.0-rc.1 API documentation](https://docs.rs/keel/0.1.0-rc.1/keel/) for provider
setup, session operations, event handling, and capability details.

Guides in [docs](docs): [agent process lifetime](docs/process-lifetime.md),
[agent process environment](docs/agent-environment.md), and
[compatibility](docs/compatibility.md): what may change between versions,
and how to store and read event JSON with `session::EVENT_FORMAT`.

Runnable examples are included in the repository:

| Example | Purpose |
| --- | --- |
| [usage](crates/keel/examples/usage.rs) | Read Codex or Claude Code account quotas without a model turn |
| [models](crates/keel/examples/models.rs) | Discover models and settings |
| [sessions](crates/keel/examples/sessions.rs) | List sessions and read history |
| [chat](crates/keel/examples/chat.rs) | Start or resume a session and stream replies |
| [observe](crates/keel/examples/observe.rs) | Inspect stored agents, tasks, and logs |
| [approve](crates/keel/examples/approve.rs) | Answer an agent's permission requests from the terminal |

From a repository checkout:

```sh
cargo run --example models -- claude
cargo run --example chat -- codex --prompt "Explain this repository"
```

## Development

```sh
sh .githooks/install
RUSTFMT="$(rustup which rustfmt)" "$(rustup which cargo-fmt)" fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --locked
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps
cargo semver-checks -p keel --baseline-rev <latest release tag>
```

The hooks run the formatting and Clippy checks before each commit.
`cargo-semver-checks` (`cargo install cargo-semver-checks --locked`, or
`nix shell nixpkgs#cargo-semver-checks`) compares the public API with a
release; it has a baseline from the first tagged release on. Changes follow
the [compatibility rules](docs/compatibility.md).

[CI](.github/workflows/ci.yml) checks lint, builds/tests, documentation, packaging,
semver compatibility with the latest release, and dependencies. [Dependency scans](.github/workflows/dependencies.yml) and
[Dependabot updates](.github/dependabot.yml) also run weekly.

Real-provider tests are opt-in: `cargo test --test contract -- --ignored --nocapture`.

## License

[MIT](LICENSE)
