# Keel

Keel is an embeddable Rust library for connecting applications to coding agents. It drives each agent through its native protocol (`codex app-server`, Claude Code's stream-json mode, Pi's RPC mode) and exposes them through one provider-neutral interface, `keel::Provider`.

It provides shared model discovery, sessions, streaming, and (soon) permission handling for any application. Codex, Claude Code and Pi are supported; the host only needs their `codex`, `claude` and `pi` executables installed and signed in (for Pi: with its model providers configured).

Pi models are named `<provider>/<model id>` and its thinking levels are the effort levels. Pi has no approval step, so unlike the other two, a Pi session runs its tools (commands, file edits) without asking.

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
cargo run --example chat -- pi --effort low
```

## Development

The library lives in `crates/keel`. Public `agent`, `task`, `control`, and
`observation` modules define the consumer contracts. `observation/capabilities.rs`
contains capability types, reexported through `keel::observation`.

Within the Codex adapter, `agents.rs` handles stored discovery and history;
`live/` contains attachment state, routing, task reconciliation, and stop tracking.
Captured native data and decoding tests live in `codex/fixtures/`.

The unpublished fake-process workspace member lives in
[`tests/codex-harness`](tests/codex-harness). Root Cargo commands default to the
library; use `--workspace` to include the harness tests:

```sh
cargo test --workspace --locked
```

Install the Git hooks in a checkout with:

```sh
sh .githooks/install
```

The hooks check Rust formatting and Clippy before each commit and require Conventional Commit subjects.

The `contract` test checks that every installed provider behaves as the `Provider` and `Driver` docs promise. It starts the real agents and runs short turns, so it is ignored by default:

```sh
cargo test --test contract -- --ignored --nocapture
KEEL_CONTRACT_PROVIDERS=pi cargo test --test contract -- --ignored --nocapture
```
