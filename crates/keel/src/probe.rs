//! Whether an agent harness is usable on this computer, without starting it.
//!
//! [`probe`] finds the harness's executable, reads its version and asks it
//! whether it is signed in, using only cheap status commands: it never starts
//! a session or sends a prompt, so probing costs nothing.
//!
//! ```no_run
//! # async fn demo() {
//! let report = keel::probe::probe(keel::probe::Harness::Codex, None).await;
//! println!("{:?}", report.auth);
//! # }
//! ```

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// How long one status command may take.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);

/// A harness Keel can drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum Harness {
    Claude,
    Codex,
    Pi,
}

impl Harness {
    /// Every harness Keel supports, in a stable order. A slice, so that
    /// adding a harness does not change its type.
    pub const ALL: &'static [Harness] = &[Harness::Claude, Harness::Codex, Harness::Pi];

    /// Keel's provider identifier, as [`crate::Provider::id`] reports it.
    pub fn id(self) -> &'static str {
        match self {
            Harness::Claude => crate::claude::PROVIDER_ID,
            Harness::Codex => crate::codex::PROVIDER_ID,
            Harness::Pi => crate::pi::PROVIDER_ID,
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
    /// built with a struct literal outside Keel.
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
        Harness::Claude => claude_auth(&path).await,
        Harness::Codex => codex_auth(&path).await,
        Harness::Pi => pi_auth(&path).await,
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

async fn claude_auth(path: &Path) -> (Auth, Option<String>) {
    match run(path, &["auth", "status"]).await {
        Ok(out) => parse_claude_auth(&out.stdout),
        Err(error) => (Auth::Unknown, Some(error)),
    }
}

async fn codex_auth(path: &Path) -> (Auth, Option<String>) {
    match run(path, &["login", "status"]).await {
        Ok(out) => parse_codex_auth(out.success, &format!("{}{}", out.stdout, out.stderr)),
        Err(error) => (Auth::Unknown, Some(error)),
    }
}

async fn pi_auth(path: &Path) -> (Auth, Option<String>) {
    let Some(provider) = pi_default_provider() else {
        return (
            Auth::Unknown,
            Some("Pi has no default provider configured".to_owned()),
        );
    };
    let args = [
        "auth",
        "check",
        "--provider",
        &provider,
        "--json",
        "--no-refresh",
    ];
    match run(path, &args).await {
        Ok(out) => parse_pi_auth(&out.stdout),
        Err(error) => (Auth::Unknown, Some(error)),
    }
}

/// Pi's default provider, from its settings file.
fn pi_default_provider() -> Option<String> {
    let dir = std::env::var_os("PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .or_else(|| home().map(|home| home.join(".pi").join("agent")))?;
    let text = std::fs::read_to_string(dir.join("settings.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value
        .get("defaultProvider")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
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

fn parse_claude_auth(stdout: &str) -> (Auth, Option<String>) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(stdout.trim()) else {
        return (
            Auth::Unknown,
            Some("`claude auth status` did not answer in JSON".to_owned()),
        );
    };
    match value.get("loggedIn").and_then(serde_json::Value::as_bool) {
        Some(true) => {
            let account = value
                .get("email")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            (Auth::SignedIn { account }, None)
        }
        Some(false) => (Auth::SignedOut, None),
        None => (
            Auth::Unknown,
            Some("`claude auth status` did not say whether it is signed in".to_owned()),
        ),
    }
}

fn parse_codex_auth(success: bool, output: &str) -> (Auth, Option<String>) {
    let lower = output.to_lowercase();
    if lower.contains("not logged in") {
        (Auth::SignedOut, None)
    } else if success && lower.contains("logged in") {
        (Auth::SignedIn { account: None }, None)
    } else if !success {
        (Auth::SignedOut, None)
    } else {
        (
            Auth::Unknown,
            Some(format!(
                "unexpected answer from `codex login status`: {}",
                output.trim()
            )),
        )
    }
}

fn parse_pi_auth(stdout: &str) -> (Auth, Option<String>) {
    let value = serde_json::from_str::<serde_json::Value>(stdout.trim()).ok();
    match value
        .as_ref()
        .and_then(|v| v.get("status"))
        .and_then(serde_json::Value::as_str)
    {
        Some("ready") => (Auth::SignedIn { account: None }, None),
        Some(other) => (
            Auth::SignedOut,
            Some(format!("Pi reports its default provider as `{other}`")),
        ),
        None => (
            Auth::Unknown,
            Some("`pi auth check` did not answer in JSON".to_owned()),
        ),
    }
}

struct Output {
    success: bool,
    stdout: String,
    stderr: String,
}

async fn run(path: &Path, args: &[&str]) -> Result<Output, String> {
    let mut command = Command::new(path);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Version managers put the interpreter next to the script; make sure a
    // script found through the login shell can find it too.
    if let Some(joined) = path_beside(path, std::env::var_os("PATH")) {
        command.env("PATH", joined);
    }
    let shown = format!("`{} {}`", path.display(), args.join(" "));
    match tokio::time::timeout(COMMAND_TIMEOUT, command.output()).await {
        Err(_) => Err(format!(
            "{shown} did not finish within {}s",
            COMMAND_TIMEOUT.as_secs()
        )),
        Ok(Err(error)) => Err(format!("{shown} could not start: {error}")),
        Ok(Ok(out)) => Ok(Output {
            success: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }),
    }
}

/// `current` with the folder of `executable` in front, so a script found
/// through the login shell finds the interpreter installed next to it.
pub(crate) fn path_beside(executable: &Path, current: Option<OsString>) -> Option<OsString> {
    let dir = executable
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())?;
    let mut paths = vec![dir.to_path_buf()];
    if let Some(current) = current {
        paths.extend(std::env::split_paths(&current));
    }
    std::env::join_paths(paths).ok()
}

/// Finds an executable: as given when it contains a path separator,
/// otherwise on `PATH`, then through the login shell.
pub(crate) async fn resolve(program: &Path) -> Option<PathBuf> {
    if program.components().count() > 1 {
        return is_executable(program).then(|| program.to_path_buf());
    }
    if let Some(found) = search_path(program, std::env::var_os("PATH")) {
        return Some(found);
    }
    login_shell_lookup(program).await
}

fn search_path(program: &Path, path: Option<OsString>) -> Option<PathBuf> {
    std::env::split_paths(&path?)
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

async fn login_shell_lookup(program: &Path) -> Option<PathBuf> {
    if cfg!(windows) {
        return None;
    }
    let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into());
    let name = program.to_str()?;
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
    {
        return None;
    }
    let mut command = Command::new(shell);
    command
        .args(["-lic", &format!("command -v {name}")])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .ok()?
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let found = stable(PathBuf::from(
        text.lines().rev().find(|l| l.starts_with('/'))?.trim(),
    ));
    is_executable(&found).then_some(found)
}

/// Resolves the folder (not the file) to its real location: version managers
/// hand out temporary symlinked folders per shell, and the program must stay
/// next to its interpreter. Nix profiles are the exception: their real
/// location is a store path that changes with every rebuild.
fn stable(path: PathBuf) -> PathBuf {
    match (path.parent().map(std::fs::canonicalize), path.file_name()) {
        (Some(Ok(dir)), Some(name)) if !dir.starts_with("/nix/store") => dir.join(name),
        _ => path,
    }
}

pub(crate) fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.is_file() && meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        meta.is_file()
    }
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
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
    fn claude_status_names_the_account() {
        let (auth, detail) = parse_claude_auth(r#"{"loggedIn": true, "email": "a@example.com"}"#);
        assert_eq!(
            auth,
            Auth::SignedIn {
                account: Some("a@example.com".into())
            }
        );
        assert_eq!(detail, None);
        assert_eq!(
            parse_claude_auth(r#"{"loggedIn": false}"#).0,
            Auth::SignedOut
        );
        assert_eq!(parse_claude_auth("Not JSON").0, Auth::Unknown);
    }

    #[test]
    fn codex_status_is_read_from_its_sentence() {
        assert_eq!(
            parse_codex_auth(true, "Logged in using ChatGPT\n").0,
            Auth::SignedIn { account: None }
        );
        assert_eq!(
            parse_codex_auth(false, "Not logged in\n").0,
            Auth::SignedOut
        );
        assert_eq!(parse_codex_auth(true, "something else").0, Auth::Unknown);
    }

    #[test]
    fn pi_readiness_comes_from_its_json() {
        assert_eq!(
            parse_pi_auth(r#"{"status":"ready","provider":"x"}"#).0,
            Auth::SignedIn { account: None }
        );
        assert_eq!(parse_pi_auth(r#"{"status":"missing"}"#).0, Auth::SignedOut);
        assert_eq!(parse_pi_auth("").0, Auth::Unknown);
    }

    #[test]
    fn harness_ids_match_the_providers() {
        assert_eq!(Harness::Claude.id(), "claude");
        assert_eq!(Harness::Codex.id(), "codex");
        assert_eq!(Harness::Pi.id(), "pi");
    }

    #[test]
    fn a_missing_program_is_not_found_on_path() {
        let path = std::env::join_paths(["/nonexistent-keel-dir"]).ok();
        assert_eq!(search_path(Path::new("definitely-not-here"), path), None);
    }
}
