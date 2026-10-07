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

## Observing delegated work

`Session::subscribe_records()` provides sequenced activity with agent ownership;
`subscribe()` retains the root event stream. Subscribe first, fetch `snapshot()`,
then apply records from the same stream whose sequence exceeds the snapshot's
watermark. Check `observation_capabilities()` for effective adapter support.

Claude Code supports live root, child and nested agent discovery, transcripts,
native execution outcomes, and atomic snapshots. It joins native task IDs to
forwarded conversations through spawning tool IDs. Delegated turn IDs are those
native tool IDs; they are scoped to their owning agent. Unresolved parents remain
unknown, and bounded routing gaps are reported explicitly. Work still running
when the attachment closes becomes unknown.

Claude tracks native task lifecycle, background membership, ownership and reported
progress. `Session::task_output()` reads command logs in bounded pages with scoped
cursors that can follow growing files. Missing or expired output is reported
separately from task status; agent-run output uses agent transcripts.

`Provider::list_agents()` discovers stored descendants with parent/ancestor
filters, and `list_agent_turns()` returns explicitly owned turns. Claude joins
creation and `SendMessage` continuation history using native invocation IDs.
`list_tasks()` and provider-level `task_output()` inspect stored tasks and logs
without opening the session. Cursors validate session, resource, and filters.
Resume bootstraps stored summaries into a new observation stream; files alone
never establish that work is currently running. Retention and compaction can
remove history, and unreported stored outcomes remain unknown.

Native root interrupts and
individual agent/task stops return receipts that distinguish acknowledgement
from matching terminal evidence. Stop requests capture the current execution;
Keel does not simulate subtree termination. Unknown native task
fields remain available as raw observations.
Snapshots restore agent state and active items; completed transcripts are not
replayed. Keel forwards native observation and controls; delegation and commands
are initiated by the coding agent.

See [Claude monitoring contracts and native limits](docs/claude-observation.md)
for consumer recovery, stops, history, and log semantics. The read-only
`observe` example inspects stored agents/tasks without opening their session.

## Development

The library lives in `crates/keel`. Public `agent`, `task`, `control`, and
`observation` modules define the consumer contracts. `observation/capabilities.rs`
contains capability types, reexported through `keel::observation`.

Within the Codex adapter, `agents.rs` handles stored discovery and history;
`live/` contains attachment state, routing, task reconciliation, and stop tracking.
Captured native data and decoding tests live in `codex/fixtures/`.

Claude's `live/` contains its attachment, native identity routing and active-turn
materialization. `claude/fixtures/` retains selected native stream-json traffic
and regression tests. These adapters share the public contracts while keeping
their protocol-specific state private.

Run the library tests with:

```sh
cargo test --workspace --locked
```

Install the Git hooks in a checkout with:

```sh
sh .githooks/install
```

The hooks check Rust formatting and Clippy before each commit and require Conventional Commit subjects.

GitHub Actions runs on pushes to `main` and pull requests targeting `main`;
it can also be started manually for `main`.
It runs lint first, then Linux builds and tests on Rust 1.99.0
(the declared minimum) and current stable. Once both pass, rustdoc/doctests,
package verification, and dependency checks run in parallel. `CI passed` is the
combined check to require in branch protection; it fails if any stage fails or
is skipped. The workflows use the lockfile and do not run ignored provider tests.

Dependency checks use `cargo-deny` and `deny.toml` for advisories, licenses, and
sources; duplicate versions produce warnings. The same check runs independently
on `main` on Mondays at 06:23 UTC and can be started manually for `main` from Actions. Dependabot opens
weekly Cargo and GitHub Actions update PRs, grouping compatible Cargo updates.
For a local scan, install `cargo-deny` and run:

```sh
cargo deny --all-features --locked check
```

The `contract` test checks that every installed provider behaves as the `Provider` and `Driver` docs promise. It starts the real agents and runs short turns, so it is ignored by default:

```sh
cargo test --test contract -- --ignored --nocapture
KEEL_CONTRACT_PROVIDERS=pi cargo test --test contract -- --ignored --nocapture
```
