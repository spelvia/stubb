//! Executable lookup on PATH and through the user's login shell.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

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
    let path = login_shell_path().await?;
    search_path(program, Some(path)).map(stable)
}

/// How long the login shell's `PATH` is reused: long enough that probing
/// several agents starts one shell, short enough that a newly installed
/// version manager is picked up without a restart.
const LOGIN_PATH_FRESH: Duration = Duration::from_secs(300);

/// How long a login shell may take. Interactive shell startup files can be
/// slow, and slower still on a busy machine.
const LOGIN_SHELL_TIMEOUT: Duration = Duration::from_secs(30);

/// The `PATH` the user's interactive login shell sets up, where version
/// managers (nvm, fnm, asdf, mise) and `~/.local/bin` usually come from.
/// Asked once and shared by every concurrent lookup; a failure is not
/// remembered, so the next lookup asks again.
async fn login_shell_path() -> Option<OsString> {
    static CACHED: tokio::sync::Mutex<Option<(std::time::Instant, OsString)>> =
        tokio::sync::Mutex::const_new(None);
    if cfg!(windows) {
        return None;
    }
    let mut cached = CACHED.lock().await;
    if let Some((at, path)) = cached.as_ref()
        && at.elapsed() < LOGIN_PATH_FRESH
    {
        return Some(path.clone());
    }
    let path = ask_login_shell().await?;
    *cached = Some((std::time::Instant::now(), path.clone()));
    Some(path)
}

/// Marks the line with the answer, since startup files may print too.
const PATH_MARKER: &str = "__stubb_path__=";

async fn ask_login_shell() -> Option<OsString> {
    let shell = std::env::var_os("SHELL").unwrap_or_else(|| "/bin/sh".into());
    let mut command = Command::new(shell);
    command
        .args(["-lic", &format!("printf '\\n{PATH_MARKER}%s\\n' \"$PATH\"")])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    let out = tokio::time::timeout(LOGIN_SHELL_TIMEOUT, command.output())
        .await
        .ok()?
        .ok()?;
    parse_login_path(&String::from_utf8_lossy(&out.stdout))
}

fn parse_login_path(output: &str) -> Option<OsString> {
    output
        .lines()
        .rev()
        .find_map(|line| line.trim_end().strip_prefix(PATH_MARKER))
        .filter(|path| !path.is_empty())
        .map(OsString::from)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_login_path_is_read_past_what_startup_files_print() {
        let output = "Welcome back!\n/not/this\n\n__stubb_path__=/home/me/.local/bin:/usr/bin\n";
        assert_eq!(
            parse_login_path(output),
            Some(OsString::from("/home/me/.local/bin:/usr/bin"))
        );
        assert_eq!(parse_login_path("no marker\n"), None);
        assert_eq!(parse_login_path("__stubb_path__=\n"), None);
    }

    #[test]
    fn a_missing_program_is_not_found_on_path() {
        let path = std::env::join_paths(["/nonexistent-stubb-dir"]).ok();
        assert_eq!(search_path(Path::new("definitely-not-here"), path), None);
    }
}
