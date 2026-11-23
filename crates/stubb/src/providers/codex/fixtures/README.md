# Codex protocol fixtures

`codex-model-list-0.159.3.json` records a native model catalog response.

`codex-observation-0.159.0.json` contains selected native app-server traffic
captured on 2026-10-05 through a live test harness with `gpt-6-luna`, medium effort,
and standard speed. Session/tool identifiers and the scratch working directory
were replaced. It is a collection of observations, not a complete replay script.

The capture covers:

- Initialization with the experimental protocol capability negotiated.
- Stored metadata for a root, child, and grandchild; the descendants were read
  after their original app-server exited, so their status is `notLoaded`.
- Nested turn/text events and coordination/activity items.
- A managed `sleep 60` command that remained running after its root turn ended.
- A native terminal list, direct native termination response, and empty list
  after termination. These are protocol probes, not public Stubb task methods.
- The later command completion notification: Codex reported `failed` with exit
  code `-1` after termination. A future task adapter must correlate the explicit
  stop request to distinguish stopping from an ordinary command failure.

The simple managed-command prompt was:

> Run sleep 60 with a short tool yield time. Leave its tool session running and return.

Asking only for a background `sleep` initially produced a shell command with `&`.
That shell exited and was not a running managed terminal. Retain this distinction
when testing task discovery; a model's claim that work is backgrounded is not
evidence that the harness manages it.

`mod.rs` tests native decoding and mapping against this capture. Recursive
routing, task state, and stop controls are tested alongside `codex/live/`.

`codex-rollout-0.170.0.jsonl` is a small native rollout file in the format
Codex 0.170.0 writes to `~/.codex/sessions`. It was written by hand after
real rollouts, keeping their record shapes but not their content. A command
item recorded after its turn's `task_complete` or `turn_aborted` outlived
that turn: Stubb lists those as stored tasks. Commands that completed within
their turn, items of another thread, and a malformed line are left out.
