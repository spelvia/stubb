# Contributing to Stubb

Thanks for helping improve Stubb. Bug reports, feature requests, documentation
fixes, and pull requests are all welcome.

## Reporting issues

Use the [issue forms](https://github.com/spelvia/stubb/issues/new/choose) for
bugs, feature requests, and maintenance work. For bugs, include the Stubb
version, the agent and its CLI version, your operating system, and a small
reproduction. Remove secrets from any logs you paste.

Report security vulnerabilities privately as described in
[SECURITY.md](SECURITY.md), not in a public issue.

## Setting up

You need [rustup](https://rustup.rs). The repository's `rust-toolchain.toml`
selects Rust 1.99.0, the minimum version Stubb supports, so local builds use it
automatically.

```sh
git clone https://github.com/spelvia/stubb.git
cd stubb
sh .githooks/install
cargo test --workspace
```

The Git hooks check formatting and Clippy before each commit and check that
commit messages follow [Conventional Commits](#commit-messages).

## Before opening a pull request

Run the same checks as CI:

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --all-features --no-deps --locked
cargo test --workspace --all-features --doc --locked
```

CI runs these on stable Rust for every pull request, along with
[`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny) dependency checks.
Releases are also tested on Rust 1.99.0 before publishing.

### Tests against real agents

The default test suite runs without any agent installed. An opt-in contract
test starts the agent CLIs on your machine and runs short real turns, which
count against your accounts' usage:

```sh
cargo test --test contract -- --ignored --nocapture
STUBB_CONTRACT_PROVIDERS=codex,claude cargo test --test contract -- --ignored --nocapture
```

Run it when you change a provider adapter, and mention in your pull request
which agents and CLI versions you tested.

## Pull requests

- Keep each pull request focused on one change, and link the issue it addresses.
- Add or update tests for behavior changes.
- Update the rustdoc, README, or [guides](docs) when you change public behavior.
- Follow the [compatibility rules](docs/compatibility.md). Breaking changes to
  the public API or stored event JSON wait for the next minor version before 1.0.

### Commit messages

Commit subjects follow [Conventional Commits](https://www.conventionalcommits.org):

```text
feat: add session restore
fix(acp): handle a closed connection
feat(api)!: replace the session constructor
```

Use `!` for breaking changes. Pull request titles follow the same format,
because a squash merge uses the title as the commit subject on `main`.

## Releases

Maintainers publish releases by tagging a tested commit on `main`. GitHub
Actions publishes the crate to crates.io and creates the GitHub Release. See the
[release process](docs/releases/README.md).

## License

By contributing, you agree that your contributions are licensed under the
[MIT License](LICENSE).
