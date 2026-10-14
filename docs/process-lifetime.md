# Agent process lifetime

Keel starts native agent processes (the shared `codex app-server`, a Claude
Code process per session and a Pi process per session) and makes sure none
outlives the host:

- **Orderly exit.** Every child is spawned with `kill_on_drop`, and
  `Provider::shutdown()` closes the shared Codex app-server and lets it exit
  within a short grace period before killing it. Hosts should call
  `shutdown()` on a graceful stop.
- **Crash or kill.** On Linux each child asks the kernel for `SIGKILL` when
  its parent dies (`PR_SET_PDEATHSIG`), and checks that the parent did not
  already exit while it was starting. The signal follows the thread that
  spawned the child, so spawn from a Tokio runtime's worker threads, which
  live as long as the runtime, rather than from a short-lived blocking
  thread. Other platforms rely on `kill_on_drop` and `shutdown()`.
- **Idle Codex.** The shared app-server closes once nothing has used it for
  `CodexConfig::idle_timeout` (10 minutes by default): no live session, no
  request waiting for an answer, and no call in between. The next call
  starts it again; `None` keeps it until `shutdown()`. Close or drop sessions
  when their work is done, or the app-server stays up for them.
