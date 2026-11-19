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

The Git hooks run with [lefthook](https://lefthook.dev) and use a few small
tools: [taplo](https://taplo.tamasfe.dev), [typos](https://github.com/crate-ci/typos),
[cargo-sort](https://github.com/DevinR528/cargo-sort),
[cargo-machete](https://github.com/bnjbvr/cargo-machete) and
[gitleaks](https://github.com/gitleaks/gitleaks). Install them with your package
manager, or for example:

```sh
cargo install --locked taplo-cli typos-cli cargo-sort cargo-machete
brew install lefthook gitleaks          # or: nix profile install nixpkgs#lefthook nixpkgs#gitleaks
```

Then clone, install the hooks and run the tests:

```sh
git clone https://github.com/spelvia/stubb.git
cd stubb
lefthook install
cargo test --workspace
```

Before each commit, the hooks format Rust and TOML files, keep `Cargo.toml`
sorted, check spelling and scan the staged changes for secrets. They check that
commit messages follow [Conventional Commits](#commit-messages), and run Clippy
and the unused-dependency check before each push.

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
[`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny) dependency checks
and repository checks: secrets in every commit, spelling, TOML formatting,
`Cargo.toml` order, unused dependencies, GitHub Actions lint and security
([actionlint](https://github.com/rhysd/actionlint),
[zizmor](https://docs.zizmor.sh)), and links between files.
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
- Follow the [versioning rules](#versioning).

### Commit messages

Commit subjects follow [Conventional Commits](https://www.conventionalcommits.org):

```text
feat: add session restore
fix(acp): handle a closed connection
feat(api)!: replace the session constructor
```

Use `!` for breaking changes. Pull request titles follow the same format,
because a squash merge uses the title as the commit subject on `main`.

## Versioning

Stubb follows [Semantic Versioning](https://semver.org) as Cargo applies it.
Before 1.0, the minor version is the breaking one: `0.1.x` releases only add,
and breaking changes wait for `0.2.0`. Release candidates may still change
before their release.

- Enums that may gain variants, and structs Stubb builds for applications, are
  `#[non_exhaustive]`. Adding a variant or field to them is not breaking.
- Stored event JSON is versioned by `session::EVENT_FORMAT`. Within one format,
  changes only add; removing, renaming or retyping anything bumps the format
  together with a breaking release.
- A public item is deprecated for at least one release before it is removed.
- The minimum supported Rust version is raised only in a breaking release.

On release PRs, CI runs
[`cargo-semver-checks`](https://github.com/obi1kenobi/cargo-semver-checks)
against the previous release tag.

## Releases

Maintainers publish releases by tagging a tested commit on `main`. GitHub
Actions publishes the crate to crates.io and creates the GitHub Release. See the
[release process](docs/releases/README.md).

## License

By contributing, you agree that your contributions are licensed under the
[MIT License](LICENSE).
