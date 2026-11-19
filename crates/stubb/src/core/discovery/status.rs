//! Bounded, noninteractive status commands for native harness probes.

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

use super::executable::path_beside;

/// How long one status command may take.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(20);

pub(crate) struct Output {
    pub(crate) success: bool,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
}

pub(crate) async fn run(path: &Path, args: &[&str]) -> Result<Output, String> {
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
