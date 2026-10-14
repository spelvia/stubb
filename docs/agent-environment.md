# Agent process environment

Keel starts native agent processes (the shared `codex app-server`, a Claude
Code process per session, a Pi process per session, and the short-lived
processes that read catalogs, versions and account usage). Each provider's
configuration says which environment those processes start with:
`CodexConfig::environment`, `ClaudeConfig::environment` and
`PiConfig::environment`, all a `keel::process::Environment`.

- **`Environment::inherit()`**, the default, passes the host's whole
  environment, as before this option existed.
- **`Environment::only(names)`** starts from an empty environment and passes
  only the host variables named. A name ending in `*` passes every variable
  that starts with the rest (`LC_*`, `OPENAI_*`). Names match exactly, with
  case; a named variable the host does not have is left out.
- **`.set(name, value)`** adds a variable or replaces a passed or inherited
  one, with either starting point.
- **Keel's own variables come last.** `ClaudeConfig::config_dir` and
  `PiConfig::agent_dir`, when set, are still passed as `CLAUDE_CONFIG_DIR`
  and `PI_CODING_AGENT_DIR`. When they are not set, Keel finds stored
  sessions where the process will look, with the environment it gets: for
  example `CLAUDE_CONFIG_DIR` only if it is passed or set, else
  `$HOME/.claude`.

```rust
use keel::codex::{Codex, CodexConfig};
use keel::process::Environment;

let codex = Codex::new(CodexConfig {
    environment: Environment::only(["PATH", "HOME", "USER", "LANG", "LC_*", "CODEX_HOME", "OPENAI_*"])
        .set("TERM", "dumb"),
    ..CodexConfig::default()
});
```

Use `only` when the host keeps secrets in its environment, such as a
database URL, that agents must not see. The agents run commands, and those
commands inherit the agent's environment.

What to pass depends on the agent and how it is signed in. Each needs `PATH`
(Codex and Pi are often Node scripts, so `node` must be on it) and `HOME`,
where its sign-in is stored (`~/.codex`, `~/.claude`, `~/.pi/agent`), or its
own variable for that folder (`CODEX_HOME`, `CLAUDE_CONFIG_DIR`,
`PI_CODING_AGENT_DIR`). Pass API keys (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`
and the other model providers' keys for Pi), proxy and certificate variables
when the host uses them. Claude Code picks the shell for its commands from
`SHELL`. Codex runs commands in a login shell, which reads the system and
user profile files; on NixOS `/etc/profile` resets `PATH` unless
`__NIXOS_SET_ENVIRONMENT_DONE` is passed.
