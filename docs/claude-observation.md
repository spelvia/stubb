# Claude delegated work and task monitoring

Keel bridges Claude Code's native stream-json process. Claude launches the work;
consumers observe it and may forward native stops. The adapter was exercised
with Claude Code 2.1.289 and `claude-sonnet-4-6`, medium effort, standard speed.
Older native CLIs can omit fields needed for parts of this contract. Check
`Session::observation_capabilities()` or the snapshot's capabilities.

## Consumer API

| Surface | Claude behavior |
| --- | --- |
| `Session::subscribe()` | Existing root event stream |
| `Session::subscribe_records()` | Sequenced, attributed root/child/nested activity and replacement summaries |
| `Session::snapshot()` | Atomic tree, retained tasks, active owned items, pending receipts and watermark |
| `Provider::list_agents(root, query)` | Stored native descendants, root first; direct-parent or ancestor filters |
| `Provider::list_agent_turns(root, agent, query)` | Session-scoped, explicitly owned native history |
| `Provider::list_tasks(root, query)` | Retained native results/notifications, optional owner filter |
| `Session::task_output(task, query)` | Bounded live command-file pages |
| `Provider::task_output(root, task, query)` | Bounded stored command-file pages |
| `Session::stop_agent(id)` | Root interrupt or delegated execution's native `stop_task` |
| `Session::stop_task(id)` | Native `stop_task` for the captured execution |
| `Session::interrupt(turn)` | Legacy root control, now guarded against an obsolete turn ID |
| `Capabilities::stop_subtree` | Unsupported: no atomic native subtree stop |

Agent summaries retain native IDs, creation ancestry and effective models when
reported. Their status describes the latest work, so a completed agent can run
again under the same ID. `SendMessage` continuations have distinct invocation
IDs. Delegated turn IDs are native invocation tool IDs, scoped to their agent.
Unknown parents remain unknown until native tool ownership resolves them; depth,
arrival order and reply text do not establish ancestry.

Task summaries retain status, owner/origin, kind, background/ambient flags, output
availability, and native usage/progress. Agent-run tasks reference the agent node.
Other native task kinds are retained as `Kind::Other`. Native metadata not mapped
to a common field, including patch timing/error details, tool heartbeats and retry
information, remains available in raw records. Agent/task creation failures that
provide no native resource ID remain tool failures rather than invented nodes.

## Subscription and recovery

Subscribe before taking a snapshot. Replace consumer state with the snapshot,
then apply buffered records from its `stream_id` whose sequence exceeds its
watermark. Summaries and receipts are complete replacements keyed by native ID
or receipt ID. Active turns contain materialized items; completed transcript
replay uses owned history. A fresh native process has a fresh stream ID.

On broadcast lag or a sequence gap, take another snapshot. It restores current
known state and active items, not every missed transcript/log byte. Unresolved
parent traffic is bounded at 256 frames/1 MiB; overflow emits
`keel/observationGap`. Activity whose owner cannot be established remains raw.
Child usage never silently contributes to root usage.

The current background membership set is a native level signal; missing a task
from it does not establish success or cancellation. The adapter accepts both
native task-object arrays and older SDK ID arrays. Root completion can coexist
with running children or commands, and the aggregate session remains active.
Pausing/running changes are monitored when native patches report them; Keel does
not add consumer task-launch, pause, continuation or scheduling APIs.

## Stops and outcomes

A stop receipt captures an execution. Concurrent requests for the same target
reuse the receipt; agent/task targets for the same native execution share one
native request while keeping distinct target receipts. The stdin writer checks
that the captured execution is still current before sending.

`Accepted` means native acknowledgement. Only matching native terminal evidence
produces `Confirmed`. Natural completion produces `AlreadyFinished`; no known
active execution produces `NotRunning`. Native rejection is `Rejected` and lost
acknowledgement, process loss or detach is `Unconfirmed`. Later matching evidence
can resolve a lost acknowledgement. A late acknowledgement cannot overwrite a
terminal outcome or set a newer execution to stopping.

Claude's `stop_task` has no atomic native generation guard. A reused task needs
its invocation ID to correlate outcomes after the first execution. An outcome
without sufficient identity remains inconclusive. Stops follow native ownership
semantics; stopping a parent does not promise every child or owned task stopped.
Keel does not kill individual OS processes or traverse a subtree to imitate a
native operation. Process loss marks nonterminal work unknown, not interrupted.

## History, resume and logs

Stored discovery reads the selected root's native sidechains and structured tool
results. History cursors validate root, operation, selected resource and filters;
output cursors also validate native file identity and offset. Unknown roots,
unrelated agents and unknown tasks return typed not-found errors. No consumer
supplies a filesystem path. An explicit `ClaudeConfig::config_dir` configures both
stored reads and the native process through `CLAUDE_CONFIG_DIR`.

Resume seeds retained summaries into the new attachment. Stored files establish
existence and retained outcomes, not a live process or stoppable execution. Native
bookends establish new active work. Missing or expired child transcripts may
leave a discoverable agent with empty history. Compaction and native retention
can remove history. An unreported delegated outcome stays `inProgress` in owned
history with agent status unknown; this does not assert that it is still running.
Native user cancellations may not persist a terminal notification, so a task that
was stopped live can correctly become unknown in a later stored read.

Forwarded child results may omit structured result metadata. Keel joins the
actual result tool ID to an already-known native task bookend before extracting
its log path; the text never supplies task identity or execution status.

Command log reads use 4–65,536-byte pages (16,384 by default). A running file can
be at EOF now and still have a continuation cursor. `Partial`/`Complete` describes
native output finality independently of execution status. The completion patch
can precede final file writes; only a native terminal notification establishes
finality. Missing or expired files are unavailable. Detected replacement or
truncation invalidates the cursor. Trailing partial UTF-8 is held for append;
invalid terminal bytes are emitted lossily so a final read can reach EOF.
Agent-run `.output` files contain full native JSONL, including system attachments;
use owned agent transcripts instead of exposing them as command logs.

Read-only inspection example:

```sh
cargo run --example observe -- claude <root-session-id> --agent <native-agent-id>
cargo run --example observe -- claude <root-session-id> --task <native-command-task-id>
```

## Validation

Deterministic checks cover routing, task/outcome ordering, native identity reuse,
receipt races, rejection/timeout, crash/detach, permissions, snapshots/lag, scoped
history, file rotation/truncation/UTF-8, and resume. The real subprocess peers live
in `tests/harness`, with separate Claude stream-json and Codex JSON-RPC modules.
Real keel-tester checks separately exercise Sonnet 4.6 trees, siblings, nested
agents, background work, progress/logs, native stops, selectable history and
resume/continuation. These checks do not imply support for external Claude
sessions, experimental agent teams or native features that provide no observable
identity on this process's stream.
