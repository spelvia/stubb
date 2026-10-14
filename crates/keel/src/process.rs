//! What every agent process Keel spawns shares: it never outlives the host,
//! and it starts with the environment the host chose.
//!
//! `kill_on_drop` covers an orderly exit. On Linux the child also asks the
//! kernel for `SIGKILL` when its parent dies (`PR_SET_PDEATHSIG`), so a host
//! that crashes or is killed leaves no agent behind. The signal follows the
//! thread that spawned the child: spawn from a runtime's worker threads, not
//! from a short-lived blocking thread.
//!
//! By default an agent process inherits the host's whole environment. A host
//! that holds secrets in its environment, such as a database URL, can start
//! agents with only the variables they need instead: see [`Environment`].

use std::ffi::{OsStr, OsString};

use tokio::process::Command;

/// The environment an agent process starts with, set per provider (for
/// example [`CodexConfig::environment`](crate::codex::CodexConfig::environment)).
///
/// [`Environment::inherit`], the default, passes the host's whole
/// environment. [`Environment::only`] starts from an empty one and passes
/// only the named host variables. Either way, [`Environment::set`] adds
/// variables or replaces them, and the variables Keel itself sets for a
/// provider, such as `CLAUDE_CONFIG_DIR`, come last.
///
/// ```
/// use keel::process::Environment;
///
/// // PATH, HOME, the locale and Codex's own settings; nothing else.
/// let environment = Environment::only(["PATH", "HOME", "LANG", "LC_*", "CODEX_HOME", "OPENAI_*"])
///     .set("TERM", "dumb");
/// assert!(!environment.inherits());
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Environment {
    /// The host variables passed; `None` passes them all.
    pass: Option<Vec<String>>,
    set: Vec<(OsString, OsString)>,
}

impl Environment {
    /// The host's whole environment.
    pub fn inherit() -> Self {
        Self::default()
    }

    /// An empty environment, plus the host variables named in `names`. A
    /// name ending in `*` passes every variable that starts with the rest,
    /// so `LC_*` passes `LC_ALL` and `LC_CTYPE`. Names are matched exactly,
    /// with case. A named variable the host does not have is left out.
    pub fn only<I, S>(names: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            pass: Some(names.into_iter().map(Into::into).collect()),
            set: Vec::new(),
        }
    }

    /// Sets `name` to `value`, over a passed or inherited value.
    #[must_use]
    pub fn set(mut self, name: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        let name = name.into();
        self.set.retain(|(set, _)| *set != name);
        self.set.push((name, value.into()));
        self
    }

    /// Whether the host's whole environment is passed.
    pub fn inherits(&self) -> bool {
        self.pass.is_none()
    }

    /// Whether the host variable `name` is passed.
    pub fn passes(&self, name: &str) -> bool {
        self.pass.as_ref().is_none_or(|pass| {
            pass.iter().any(|pattern| match pattern.strip_suffix('*') {
                Some(prefix) => name.starts_with(prefix),
                None => pattern == name,
            })
        })
    }

    /// Gives `command` this environment, from the host's.
    pub(crate) fn apply(&self, command: &mut Command) {
        self.apply_from(command, std::env::vars_os());
    }

    fn apply_from(
        &self,
        command: &mut Command,
        host: impl IntoIterator<Item = (OsString, OsString)>,
    ) {
        if self.pass.is_some() {
            command.env_clear();
            for (name, value) in host {
                // A name that is not Unicode matches no pattern.
                if name.to_str().is_some_and(|name| self.passes(name)) {
                    command.env(name, value);
                }
            }
        }
        for (name, value) in &self.set {
            command.env(name, value);
        }
    }

    /// The value the process gets for `name` when Keel sets nothing for it.
    pub(crate) fn value(&self, name: &str) -> Option<OsString> {
        match self
            .set
            .iter()
            .rev()
            .find(|(set, _)| set == OsStr::new(name))
        {
            Some((_, value)) => Some(value.clone()),
            None if self.passes(name) => std::env::var_os(name),
            None => None,
        }
    }
}

/// Makes the child die with this process, where the platform allows it.
pub(crate) fn die_with_parent(command: &mut Command) {
    #[cfg(target_os = "linux")]
    {
        let parent = std::process::id();
        // SAFETY: between fork and exec the closure makes only
        // async-signal-safe calls (`prctl`, `getppid`) and allocates nothing,
        // not even its errors.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                // The parent may have died before the request took effect.
                if u32::try_from(libc::getppid()).ok() != Some(parent) {
                    return Err(std::io::Error::from_raw_os_error(libc::ESRCH));
                }
                Ok(())
            });
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = command;
}

/// Writes a stand-in executable for a test. A child of another test that
/// forks while this process holds the file open for writing would keep it
/// open until it execs, and starting the file then fails with "Text file
/// busy". So a short-lived `sh` writes it instead, from standard input.
#[cfg(all(test, unix))]
pub(crate) fn write_executable(path: &std::path::Path, script: &str) {
    use std::io::Write as _;
    let mut writer = std::process::Command::new("sh")
        .args(["-c", "cat > \"$1\" && chmod 700 \"$1\"", "sh"])
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    writer
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    assert!(writer.wait().unwrap().success());
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::time::Duration;

    /// Whether the process with this ID still runs (and is not a zombie).
    fn alive(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .is_ok_and(|stat| !stat.rsplit(") ").next().unwrap_or("").starts_with('Z'))
    }

    /// A child spawned in a thread that then exits is killed with it, as it
    /// would be with its host process.
    #[test]
    fn a_child_dies_with_the_thread_that_spawned_it() {
        let pid = std::thread::spawn(|| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let mut command = tokio::process::Command::new("sleep");
                command.arg("30");
                super::die_with_parent(&mut command);
                let child = command.spawn().unwrap();
                let pid = child.id().unwrap();
                // Not dropped: kill_on_drop is not what ends it.
                std::mem::forget(child);
                pid
            })
        })
        .join()
        .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while alive(pid) && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!alive(pid), "the child outlived its parent");
    }
}

#[cfg(all(test, unix))]
mod environment_tests {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::path::PathBuf;

    use super::Environment;

    /// `env` from the host's `PATH`: the child's own `PATH` may not have it.
    fn env_program() -> PathBuf {
        let path = std::env::var_os("PATH").unwrap();
        std::env::split_paths(&path)
            .map(|dir| dir.join("env"))
            .find(|candidate| candidate.is_file())
            .unwrap()
    }

    /// The variables a process started with `environment` sees, from `host`.
    async fn seen(
        environment: &Environment,
        host: Option<&[(&str, &str)]>,
    ) -> BTreeMap<String, String> {
        let mut command = tokio::process::Command::new(env_program());
        match host {
            Some(host) => environment.apply_from(
                &mut command,
                host.iter()
                    .map(|(name, value)| (OsString::from(name), OsString::from(value))),
            ),
            None => environment.apply(&mut command),
        }
        let output = command.output().await.unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(name, value)| (name.to_owned(), value.to_owned()))
            .collect()
    }

    #[tokio::test]
    async fn a_process_sees_only_the_allowed_environment() {
        let host = [
            ("PATH", "/usr/bin"),
            ("HOME", "/home/someone"),
            ("LC_ALL", "C.UTF-8"),
            ("LC_TIME", "en_GB.UTF-8"),
            ("TERM", "xterm-256color"),
            ("DATABASE_URL", "postgres://secret"),
            ("APP_SECRET_KEY", "secret"),
            ("XLC_ALL", "not a prefix match"),
        ];
        let environment = Environment::only(["PATH", "HOME", "LC_*", "TERM", "UNSET"])
            .set("TERM", "dumb")
            .set("RUN_URL", "http://127.0.0.1:1");

        let seen = seen(&environment, Some(&host)).await;

        let expected: BTreeMap<String, String> = [
            ("PATH", "/usr/bin"),
            ("HOME", "/home/someone"),
            ("LC_ALL", "C.UTF-8"),
            ("LC_TIME", "en_GB.UTF-8"),
            ("TERM", "dumb"),
            ("RUN_URL", "http://127.0.0.1:1"),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect();
        assert_eq!(seen, expected);
    }

    #[tokio::test]
    async fn by_default_a_process_inherits_the_host_environment() {
        let environment = Environment::default().set("KEEL_TEST_EXTRA", "1");
        assert!(environment.inherits());

        let seen = seen(&environment, None).await;

        assert_eq!(
            seen.get("PATH").map(OsString::from),
            std::env::var_os("PATH")
        );
        assert_eq!(seen.get("KEEL_TEST_EXTRA").map(String::as_str), Some("1"));
    }

    #[test]
    fn a_value_comes_from_what_is_set_then_what_is_passed() {
        let environment = Environment::only(["HOME"])
            .set("CLAUDE_CONFIG_DIR", "/a")
            .set("CLAUDE_CONFIG_DIR", "/b");

        assert_eq!(environment.value("CLAUDE_CONFIG_DIR"), Some("/b".into()));
        assert_eq!(environment.value("HOME"), std::env::var_os("HOME"));
        assert_eq!(environment.value("PATH"), None);
        assert!(environment.passes("HOME"));
        assert!(!environment.passes("HOMEDIR"));
    }
}
