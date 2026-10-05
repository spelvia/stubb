# Keel

Keel is an embeddable Rust library for connecting applications to coding agents through the Agent Client Protocol (ACP).

It provides shared session, streaming, authentication, and permission handling for Ahab, Yomi, and other applications. Initial integrations focus on Codex and Claude Code.

## Development

Install the Git hooks in a checkout with:

```sh
sh .githooks/install
```

The hooks check Rust formatting and Clippy before each commit and require Conventional Commit subjects.
