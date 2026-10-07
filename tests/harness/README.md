# Scripted native processes

This unpublished workspace member tests Keel through a real subprocess and its
native transports. The executable accepts Codex `app-server` and Claude
stream-json arguments. Codex scenarios use the resumed root ID; Claude scenarios
use a file in an isolated working directory, without shared environment variables. It never
runs commands, invokes a model, or requires an installed agent CLI.

Run `cargo test -p keel-test-harness --test claude_observation` or
`cargo test --workspace --locked`. Root Cargo commands default to the library.
The peer scripts the protocol subset needed for these assertions; it is
not a general Codex, Claude, or Pi simulator. Real Luna and Sonnet 4.6 checks through keel-tester
remain necessary to validate the native CLI behavior and consumer interface.

Coverage includes captured-turn stops, concurrent duplicate requests, natural
completion and interruption before acknowledgement, lost/rejected acknowledgements,
process exit, detach and resume, late responses, stale process preflight, native
terminal final events and delayed running lists, descendant/history pagination,
missing ancestry, interleaved items with identical native IDs, terminal cursor
loops, broadcast lag and atomic snapshot recovery. Request trace notifications
allow tests to check native target parameters and that history reads do not start
or resume descendants. Stop controls act only on their explicit target.


Claude coverage also includes initialize.forwardSubagentText, config-directory
forwarding, early recursive identity repair, child-owned commands and bounded
logs, legacy root projection, cross-target stop deduplication, native outcomes
before acknowledgement and continuation, lost acknowledgement recovery, rejection,
crash/detach uncertainty, root turn guards, fail-closed child permissions, stored
membership/scoped cursors, resume bootstrap and broadcast lag recovery. Native
CLI checks remain separate from this deterministic peer.
