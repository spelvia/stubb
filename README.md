# Keel

Keel is an embeddable Rust library for connecting applications to coding agents. It drives each agent through its native protocol (`codex app-server`, Claude Code's stream-json mode) and exposes them through one provider-neutral interface, `keel::Provider`.

It provides shared model discovery, sessions, streaming, and (soon) permission handling for Ahab, Yomi, and other applications. Codex and Claude Code are supported; the host only needs their `codex` and `claude` executables installed and signed in.

## Examples

Runnable examples live in [`crates/keel/examples`](crates/keel/examples). Each one is self-contained, so it doubles as a starting point:

| Example | Shows |
| --- | --- |
| [`models`](crates/keel/examples/models.rs) | Models with their effort levels and fast-mode support, for a model picker |
| [`sessions`](crates/keel/examples/sessions.rs) | Stored sessions of a directory, and one session's history (read-only) |
| [`chat`](crates/keel/examples/chat.rs) | Start or resume a session, send messages, and stream the reply and the commands the agent runs |

```sh
cargo run --example models -- claude
cargo run --example sessions -- codex --cwd /path/to/repo
cargo run --example chat -- codex --model gpt-6-luna --effort medium
cargo run --example chat -- claude --resume <session-id>
```

## Development

Install the Git hooks in a checkout with:

```sh
sh .githooks/install
```

The hooks check Rust formatting and Clippy before each commit and require Conventional Commit subjects.
