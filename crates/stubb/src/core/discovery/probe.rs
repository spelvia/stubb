use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::cli::{Cli, clis};
use super::{executable::resolve, status::run};

/// Whether the agent has credentials it can use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
#[non_exhaustive]
pub enum Auth {
    /// Signed in; `account` when the agent names it (an email, for
    /// example).
    SignedIn { account: Option<String> },
    /// Installed, but without usable credentials.
    SignedOut,
    /// The agent could not tell; `Report::detail` says why.
    Unknown,
}

/// What [`probe`] found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Report {
    pub cli: Cli,
    /// The executable that was found, or `None` when it is not installed.
    pub path: Option<PathBuf>,
    /// The agent's own version string, such as `2.1.322`.
    pub version: Option<String>,
    pub auth: Auth,
    /// Why something could not be determined, in a sentence for people.
    pub detail: Option<String>,
}

impl Report {
    /// A report of `cli` that found nothing yet: not installed, no
    /// version, auth unknown. Set the public fields to describe more, for
    /// example in tests; the struct is `#[non_exhaustive]`, so it cannot be
    /// built with a struct literal outside Stubb.
    pub fn new(cli: Cli) -> Self {
        Self {
            cli,
            path: None,
            version: None,
            auth: Auth::Unknown,
            detail: None,
        }
    }

    /// Whether the executable was found.
    pub fn installed(&self) -> bool {
        self.path.is_some()
    }
}

/// Probes one built-in agent. `executable` overrides where to look; a bare name or
/// `None` is looked up on `PATH`, then through the user's login shell, which
/// finds version managers (fnm, nvm, asdf) that services do not see.
pub async fn probe(cli: Cli, executable: Option<&Path>) -> Report {
    let program = executable
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(cli.program()));
    let Some(path) = resolve(&program).await else {
        return Report {
            detail: Some(format!("`{}` was not found", program.display())),
            ..Report::new(cli)
        };
    };
    let mut detail = None;
    let version = match run(&path, &["--version"]).await {
        Ok(out) => parse_version(&out.stdout),
        Err(error) => {
            detail = Some(error);
            None
        }
    };
    let (auth, auth_detail) = (cli.auth)(&path).await;
    Report {
        cli,
        path: Some(path),
        version,
        auth,
        detail: detail.or(auth_detail),
    }
}

/// Probes every built-in agent compiled into Stubb, concurrently.
pub async fn probe_all() -> Vec<Report> {
    futures::future::join_all(clis().iter().map(|&cli| probe(cli, None))).await
}

fn parse_version(stdout: &str) -> Option<String> {
    stdout
        .split_whitespace()
        .map(|word| word.trim_start_matches('v'))
        .find(|word| {
            let mut parts = word.split('.');
            parts
                .next()
                .is_some_and(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
                && parts
                    .next()
                    .is_some_and(|p| p.chars().next().is_some_and(|c| c.is_ascii_digit()))
        })
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_are_read_from_each_harness() {
        assert_eq!(
            parse_version("2.1.322 (Claude Code)\n").as_deref(),
            Some("2.1.322")
        );
        assert_eq!(
            parse_version("codex-cli 0.167.1\n").as_deref(),
            Some("0.167.1")
        );
        assert_eq!(parse_version("1.0.2\n").as_deref(), Some("1.0.2"));
        assert_eq!(parse_version("v3.4.0").as_deref(), Some("3.4.0"));
        assert_eq!(parse_version("no version here"), None);
    }
}
