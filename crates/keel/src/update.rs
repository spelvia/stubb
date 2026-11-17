//! Updating an agent harness: how it was installed, whether Keel can update
//! it, and doing so with the harness's own updater.
//!
//! Every harness Keel drives updates itself (`claude update`, `codex update`,
//! `pi update --self`), following the way it was installed (its native
//! installer, npm, Homebrew). A harness installed by Nix is the system's to
//! update, so Keel only says how. Which version is newest comes from the
//! harness's npm package, [`Package`]; Keel does not fetch it, so hosts use
//! their own HTTP client.
//!
//! ```no_run
//! # async fn demo() {
//! use keel::probe::{Harness, probe};
//! let report = probe(Harness::Codex, None).await;
//! let plan = keel::update::plan(&report);
//! if plan.can_update {
//!     let outcome = keel::update::update(&report).await;
//!     println!("{:?}", outcome);
//! }
//! # }
//! ```

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

use crate::probe::{Harness, Report, probe};

/// How long an update may take: it downloads and installs.
const UPDATE_TIMEOUT: Duration = Duration::from_secs(300);
/// How much of an updater's output is kept, from its end.
const OUTPUT_TAIL: usize = 4_000;

/// Where a harness's releases are published: its npm package, and the
/// dist-tag of the channel it follows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Package {
    /// Such as `@openai/codex`.
    pub npm: String,
    /// Such as `latest`, or `stable` for Claude Code set to that channel.
    pub tag: String,
}

impl Package {
    /// The npm registry document listing the package's dist-tags.
    pub fn registry_url(&self) -> String {
        format!("https://registry.npmjs.org/{}", self.npm)
    }
}

/// How a harness was installed, told from where its executable really is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
#[non_exhaustive]
pub enum InstallMethod {
    /// Claude Code's own installer (`~/.local/share/claude/versions`).
    Native,
    /// A global npm install under `prefix` (fnm, nvm and the like included).
    Npm {
        prefix: PathBuf,
    },
    Homebrew,
    /// The Nix store: the system's configuration owns it.
    Nix,
    /// Somewhere else; the harness's own updater is tried.
    Unknown,
}

/// What updating a harness would do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Plan {
    pub harness: Harness,
    pub method: InstallMethod,
    pub package: Package,
    /// Whether [`update`] may run here.
    pub can_update: bool,
    /// The command that updates it: the one [`update`] runs, or what a person
    /// runs (or changes) when Keel cannot.
    pub command: String,
    /// Why Keel cannot update it, when it cannot.
    pub reason: Option<String>,
}

/// What [`update`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    pub harness: Harness,
    pub before: Option<String>,
    /// The version after updating, probed again.
    pub after: Option<String>,
    /// The updater succeeded (exit code 0).
    pub success: bool,
    /// The end of what the updater printed, both streams.
    pub output: String,
}

/// Why [`update`] could not run.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum UpdateError {
    #[error("{0} is not installed")]
    NotInstalled(&'static str),
    #[error("{0}")]
    NotUpdatable(String),
    #[error("the updater could not start: {0}")]
    Start(String),
    #[error("the updater did not finish within {} minutes", UPDATE_TIMEOUT.as_secs() / 60)]
    Timeout,
}

/// The npm package a harness is published as, and the channel it follows.
pub fn package(harness: Harness) -> Package {
    match harness {
        Harness::Claude => Package {
            npm: "@anthropic-ai/claude-code".to_owned(),
            tag: claude_channel().unwrap_or_else(|| "latest".to_owned()),
        },
        Harness::Codex => Package {
            npm: "@openai/codex".to_owned(),
            tag: "latest".to_owned(),
        },
        Harness::Pi => Package {
            npm: "@earendil-works/pi-coding-agent".to_owned(),
            tag: "latest".to_owned(),
        },
    }
}

/// How `executable` (as probed) was installed.
pub fn install_method(harness: Harness, executable: &Path) -> InstallMethod {
    let real = std::fs::canonicalize(executable).unwrap_or_else(|_| executable.to_path_buf());
    method_of(harness, &real)
}

/// What updating the harness in `report` would do.
pub fn plan(report: &Report) -> Plan {
    let harness = report.harness;
    let package = package(harness);
    let own = own_updater(harness);
    let Some(path) = &report.path else {
        return Plan {
            harness,
            method: InstallMethod::Unknown,
            package,
            can_update: false,
            command: own.join(" "),
            reason: Some(format!("{} is not installed", harness.program())),
        };
    };
    let method = install_method(harness, path);
    match method {
        InstallMethod::Nix => Plan {
            harness,
            method,
            package,
            can_update: false,
            command: "nix flake update, then rebuild (or update the package where it is pinned)"
                .to_owned(),
            reason: Some("installed by Nix: the system's configuration updates it".to_owned()),
        },
        method => Plan {
            harness,
            method,
            package,
            can_update: true,
            command: own.join(" "),
            reason: None,
        },
    }
}

/// Updates the harness in `report` with its own updater, without input, then
/// probes it again for the new version.
pub async fn update(report: &Report) -> Result<Outcome, UpdateError> {
    let harness = report.harness;
    let Some(path) = &report.path else {
        return Err(UpdateError::NotInstalled(harness.program()));
    };
    let plan = plan(report);
    if !plan.can_update {
        return Err(UpdateError::NotUpdatable(
            plan.reason
                .unwrap_or_else(|| "it cannot be updated here".to_owned()),
        ));
    }
    let args = &own_updater(harness)[1..];
    let mut command = Command::new(path);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Updaters that ask before installing take silence as no; these say
        // yes where they can, and nothing waits on a terminal.
        .env("CI", "1")
        .kill_on_drop(true);
    if let Some(dir) = path.parent() {
        let mut paths = vec![dir.to_path_buf()];
        if let Some(current) = std::env::var_os("PATH") {
            paths.extend(std::env::split_paths(&current));
        }
        if let Ok(joined) = std::env::join_paths(paths) {
            command.env("PATH", joined);
        }
    }
    let mut child = command
        .spawn()
        .map_err(|error| UpdateError::Start(error.to_string()))?;
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let read = async {
        let mut out = String::new();
        let mut err = String::new();
        // Both at once: a full stderr pipe would stall an updater whose
        // stdout is still being read.
        tokio::join!(
            async {
                if let Some(stdout) = stdout.as_mut() {
                    let _ = stdout.read_to_string(&mut out).await;
                }
            },
            async {
                if let Some(stderr) = stderr.as_mut() {
                    let _ = stderr.read_to_string(&mut err).await;
                }
            },
        );
        let status = child.wait().await;
        (status, out, err)
    };
    let (status, out, err) = tokio::time::timeout(UPDATE_TIMEOUT, read)
        .await
        .map_err(|_| UpdateError::Timeout)?;
    let after = probe(harness, Some(path)).await;
    Ok(Outcome {
        harness,
        before: report.version.clone(),
        after: after.version,
        success: status.is_ok_and(|status| status.success()),
        output: tail(&format!("{out}{err}"), OUTPUT_TAIL),
    })
}

/// Whether `latest` is newer than `installed`, comparing dotted numbers
/// (a pre-release suffix after `-` counts as older than its release).
pub fn is_newer(latest: &str, installed: &str) -> bool {
    fn parts(version: &str) -> (Vec<u64>, bool) {
        let version = version.trim().trim_start_matches('v');
        let (numbers, pre) = match version.split_once('-') {
            Some((numbers, _)) => (numbers, true),
            None => (version, false),
        };
        (
            numbers
                .split('.')
                .map(|part| part.parse().unwrap_or(0))
                .collect(),
            pre,
        )
    }
    let (new, new_pre) = parts(latest);
    let (old, old_pre) = parts(installed);
    match new.cmp(&old) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        std::cmp::Ordering::Equal => old_pre && !new_pre,
    }
}

/// The harness's own updater, program first.
fn own_updater(harness: Harness) -> Vec<&'static str> {
    match harness {
        Harness::Claude => vec!["claude", "update"],
        Harness::Codex => vec!["codex", "update"],
        Harness::Pi => vec!["pi", "update", "--self"],
    }
}

fn method_of(harness: Harness, real: &Path) -> InstallMethod {
    let text = real.to_string_lossy();
    if text.starts_with("/nix/store/") {
        return InstallMethod::Nix;
    }
    if text.contains("/Cellar/") || text.contains("/homebrew/") || text.contains("/linuxbrew/") {
        return InstallMethod::Homebrew;
    }
    if harness == Harness::Claude && text.contains("/claude/versions/") {
        return InstallMethod::Native;
    }
    if let Some((prefix, _)) = text.split_once("/lib/node_modules/") {
        return InstallMethod::Npm {
            prefix: PathBuf::from(prefix),
        };
    }
    if let Some((prefix, _)) = text.split_once("/node_modules/") {
        return InstallMethod::Npm {
            prefix: PathBuf::from(prefix),
        };
    }
    InstallMethod::Unknown
}

/// Claude Code's update channel (`stable` or `latest`), from its settings.
fn claude_channel() -> Option<String> {
    let dir = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")))?;
    let text = std::fs::read_to_string(dir.join("settings.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value
        .get("autoUpdatesChannel")?
        .as_str()
        .filter(|channel| matches!(*channel, "stable" | "latest"))
        .map(str::to_owned)
}

fn tail(text: &str, keep: usize) -> String {
    let trimmed = text.trim();
    if trimmed.len() <= keep {
        return trimmed.to_owned();
    }
    let mut start = trimmed.len() - keep;
    while !trimmed.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &trimmed[start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_methods_are_told_from_the_real_path() {
        assert_eq!(
            method_of(
                Harness::Claude,
                Path::new("/home/me/.local/share/claude/versions/2.1.322")
            ),
            InstallMethod::Native
        );
        assert_eq!(
            method_of(
                Harness::Codex,
                Path::new(
                    "/home/me/.local/share/fnm/node-versions/v24/installation/lib/node_modules/@openai/codex/bin/codex.js"
                )
            ),
            InstallMethod::Npm {
                prefix: PathBuf::from("/home/me/.local/share/fnm/node-versions/v24/installation")
            }
        );
        assert_eq!(
            method_of(
                Harness::Pi,
                Path::new("/nix/store/abc-pi-coding-agent-1.5.2/bin/pi")
            ),
            InstallMethod::Nix
        );
        assert_eq!(
            method_of(
                Harness::Codex,
                Path::new("/opt/homebrew/Cellar/codex/0.1/bin/codex")
            ),
            InstallMethod::Homebrew
        );
        assert_eq!(
            method_of(Harness::Codex, Path::new("/usr/local/bin/codex")),
            InstallMethod::Unknown
        );
    }

    #[test]
    fn nix_installs_are_left_to_the_system() {
        let report = Report {
            harness: Harness::Pi,
            path: Some(PathBuf::from("/nix/store/abc-pi-coding-agent-1.5.2/bin/pi")),
            version: Some("1.5.2".to_owned()),
            auth: crate::probe::Auth::SignedOut,
            detail: None,
        };
        let plan = plan(&report);
        assert!(!plan.can_update);
        assert_eq!(plan.method, InstallMethod::Nix);
        assert_eq!(plan.package.npm, "@earendil-works/pi-coding-agent");
    }

    #[test]
    fn newer_versions_are_told_apart() {
        assert!(is_newer("2.1.325", "2.1.322"));
        assert!(is_newer("0.169.0", "0.167.1"));
        assert!(is_newer("1.10.0", "1.9.9"));
        assert!(!is_newer("1.5.2", "1.5.2"));
        assert!(!is_newer("2.1.311", "2.1.322"));
        assert!(is_newer("1.0.0", "1.0.0-beta.1"));
        assert!(!is_newer("1.0.0-beta.2", "1.0.0"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_update_runs_the_harness_updater_and_probes_again() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("keel-update-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let program = dir.join("codex");
        let marker = dir.join("updated");
        // A stand-in Codex: `update` installs "0.2.0", `--version` tells it.
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\nif [ \"$1\" = update ]; then echo installing; echo warn >&2; touch {m}; exit 0; fi\n\
                 if [ \"$1\" = --version ]; then if [ -e {m} ]; then echo 0.2.0; else echo 0.1.0; fi; exit 0; fi\nexit 1\n",
                m = marker.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let report = Report {
            harness: Harness::Codex,
            path: Some(program.clone()),
            version: Some("0.1.0".to_owned()),
            auth: crate::probe::Auth::SignedOut,
            detail: None,
        };
        let outcome = update(&report).await.unwrap();
        assert!(outcome.success);
        assert_eq!(outcome.before.as_deref(), Some("0.1.0"));
        assert_eq!(outcome.after.as_deref(), Some("0.2.0"));
        assert!(outcome.output.contains("installing"));
        assert!(outcome.output.contains("warn"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn nothing_runs_for_an_agent_that_is_not_installed() {
        let report = Report {
            harness: Harness::Claude,
            path: None,
            version: None,
            auth: crate::probe::Auth::Unknown,
            detail: None,
        };
        assert_eq!(
            update(&report).await,
            Err(UpdateError::NotInstalled("claude"))
        );
    }

    #[test]
    fn long_output_keeps_its_end() {
        assert_eq!(tail("short", 10), "short");
        assert_eq!(tail("0123456789abc", 3), "…abc");
    }
}
