//! Whether an agent harness is usable on this computer, without starting it.
//!
//! [`probe`] finds the harness's executable, reads its version and asks it
//! whether it is signed in, using only cheap status commands: it never starts
//! a session or sends a prompt, so probing costs nothing.
//!
//! ```no_run
//! # async fn demo() {
//! let report = stubb::probe::probe(stubb::probe::Harness::Codex, None).await;
//! println!("{:?}", report.auth);
//! # }
//! ```

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{executable::resolve, status::run};

/// A harness Stubb can drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Harness {
    Claude,
    Codex,
    Pi,
}

impl Harness {
    /// Every harness Stubb supports, in a stable order. A slice, so that
    /// adding a harness does not change its type.
    pub const ALL: &'static [Harness] = &[Harness::Claude, Harness::Codex, Harness::Pi];

    /// Stubb's provider identifier, as [`crate::Provider::id`] reports it.
    pub fn id(self) -> &'static str {
        match self {
            Harness::Claude => crate::providers::claude::PROVIDER_ID,
            Harness::Codex => crate::providers::codex::PROVIDER_ID,
            Harness::Pi => crate::providers::pi::PROVIDER_ID,
        }
    }

    /// The executable's usual name.
    pub fn program(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
            Harness::Pi => "pi",
        }
    }
}

/// Whether the harness has credentials it can use.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
#[non_exhaustive]
pub enum Auth {
    /// Signed in; `account` when the harness names it (an email, for
    /// example).
    SignedIn { account: Option<String> },
    /// Installed, but without usable credentials.
    SignedOut,
    /// The harness could not tell; `Report::detail` says why.
    Unknown,
}

/// What [`probe`] found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Report {
    pub harness: Harness,
    /// The executable that was found, or `None` when it is not installed.
    pub path: Option<PathBuf>,
    /// The harness's own version string, such as `2.1.322`.
    pub version: Option<String>,
    pub auth: Auth,
    /// Why something could not be determined, in a sentence for people.
    pub detail: Option<String>,
}

impl Report {
    /// A report of `harness` that found nothing yet: not installed, no
    /// version, auth unknown. Set the public fields to describe more, for
    /// example in tests; the struct is `#[non_exhaustive]`, so it cannot be
    /// built with a struct literal outside Stubb.
    pub fn new(harness: Harness) -> Self {
        Self {
            harness,
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

/// Probes one harness. `executable` overrides where to look; a bare name or
/// `None` is looked up on `PATH`, then through the user's login shell, which
/// finds version managers (fnm, nvm, asdf) that services do not see.
pub async fn probe(harness: Harness, executable: Option<&Path>) -> Report {
    let program = executable
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from(harness.program()));
    let Some(path) = resolve(&program).await else {
        return Report {
            detail: Some(format!("`{}` was not found", program.display())),
            ..Report::new(harness)
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
    let auth = match harness {
        Harness::Claude => crate::providers::claude::probe::auth(&path).await,
        Harness::Codex => crate::providers::codex::probe::auth(&path).await,
        Harness::Pi => crate::providers::pi::probe::auth(&path).await,
    };
    let (auth, auth_detail) = auth;
    Report {
        harness,
        path: Some(path),
        version,
        auth,
        detail: detail.or(auth_detail),
    }
}

/// Probes every supported harness concurrently.
pub async fn probe_all() -> Vec<Report> {
    let (claude, codex, pi) = tokio::join!(
        probe(Harness::Claude, None),
        probe(Harness::Codex, None),
        probe(Harness::Pi, None)
    );
    vec![claude, codex, pi]
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

    #[test]
    fn harness_ids_match_the_providers() {
        assert_eq!(Harness::Claude.id(), "claude");
        assert_eq!(Harness::Codex.id(), "codex");
        assert_eq!(Harness::Pi.id(), "pi");
    }
}
