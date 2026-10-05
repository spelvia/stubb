# Scripted Codex process

This unpublished workspace member tests Keel through a real subprocess and its
native JSON-RPC transport. The executable accepts `app-server`; scenarios are
selected by the resumed root ID, without shared environment variables. It never
runs commands, invokes a model, or requires an installed Codex CLI.

Run `cargo test -p keel-test-harness --test observation` or
`cargo test --workspace --locked`. Root Cargo commands default to the library.
The peer scripts the protocol subset needed for these assertions; it is
not a general Codex, Claude, or Pi simulator. Real Luna checks through keel-tester
remain necessary to validate the native CLI behavior and consumer interface.

Coverage includes captured-turn stops, concurrent duplicate requests, natural
completion and interruption before acknowledgement, lost/rejected acknowledgements,
process exit, detach and resume, late responses, stale process preflight, native
terminal final events and delayed running lists, descendant/history pagination,
missing ancestry, interleaved items with identical native IDs, terminal cursor
loops, broadcast lag and atomic snapshot recovery. Request trace notifications
allow tests to check native target parameters and that history reads do not start
or resume descendants. Stop controls act only on their explicit target.
