//! The ACP agent registry: which agents speak ACP, and how to start them.
//!
//! The [official registry](https://agentclientprotocol.com) is a JSON file
//! at [`INDEX_URL`]. Each [`Entry`] names an agent and how it is
//! distributed: as a per-platform binary archive, as an npm package run
//! with `npx`, or as a Python package run with `uvx`. Keel parses the file
//! and turns an entry into an [`acp::Launch`](crate::acp::Launch); it does
//! not download anything. A host fetches the registry and any archive
//! itself, checks the archive's SHA-256 and unpacks it into an install
//! folder (for example [`Entry::install_dir`]). [`Registry::bundled`] is a
//! copy that ships with Keel, for hosts that are offline.
//!
//! ```
//! use keel::registry::{Method, Registry};
//!
//! let registry = Registry::bundled();
//! let gemini = registry.get("gemini").expect("bundled");
//! let launch = gemini.launch(Method::Npx, None).unwrap();
//! assert_eq!(launch.program.to_str(), Some("npx"));
//! ```
//!
//! Registry data comes from outside: Keel refuses binary commands that
//! would leave the install folder.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::acp::Launch;

/// Where the official registry is published.
pub const INDEX_URL: &str = "https://cdn.agentclientprotocol.com/registry/v1/latest/registry.json";

/// The copy of the registry bundled with Keel, without icons.
const BUNDLED: &str = include_str!("registry.json");

/// A registry file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Registry {
    /// The registry format's version, such as `1.0.0`.
    pub version: String,
    pub agents: Vec<Entry>,
}

impl Registry {
    /// Parses a registry file, such as the one at [`INDEX_URL`]. Unknown
    /// fields are ignored.
    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// The registry as bundled with this version of Keel.
    pub fn bundled() -> &'static Registry {
        static REGISTRY: OnceLock<Registry> = OnceLock::new();
        REGISTRY.get_or_init(|| Registry::parse(BUNDLED).expect("the bundled registry parses"))
    }

    /// The entry with this id.
    pub fn get(&self, id: &str) -> Option<&Entry> {
        self.agents.iter().find(|entry| entry.id == id)
    }
}

/// One agent in the registry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Entry {
    /// A stable identifier, such as `gemini`.
    pub id: String,
    pub name: String,
    /// The version the distributions start.
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub website: Option<String>,
    #[serde(default)]
    pub authors: Vec<String>,
    /// An SPDX license expression.
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default, rename = "license_url")]
    pub license_url: Option<String>,
    /// An icon's URL.
    #[serde(default)]
    pub icon: Option<String>,
    pub distribution: Distribution,
}

/// The ways an agent is distributed. At least one is set.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Distribution {
    /// Archives by platform, such as `linux-x86_64` (see [`platform`]).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub binary: BTreeMap<String, Binary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npx: Option<Package>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uvx: Option<Package>,
}

/// A binary archive for one platform.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Binary {
    /// Where to download the archive (`.tar.gz`, `.tar.bz2`, `.zip`, or a
    /// bare executable).
    pub archive: String,
    /// The archive's SHA-256, in hex, when the registry lists one.
    #[serde(default)]
    pub sha256: Option<String>,
    /// The program, relative to the folder the archive was unpacked into.
    pub cmd: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// A package run by `npx` or `uvx`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Package {
    /// The package, usually with its version, such as
    /// `@google/gemini-cli@0.68.0`.
    pub package: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// A way to start an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum Method {
    /// The binary for this platform, unpacked into an install folder.
    Binary,
    /// `npx -y <package>`; needs Node.js.
    Npx,
    /// `uvx <package>`; needs uv.
    Uvx,
}

/// Why an entry cannot be started a given way.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum LaunchError {
    #[error("{agent} is not distributed through {method:?}")]
    NotDistributed { agent: String, method: Method },
    #[error("{agent} has no binary for this platform ({platform})")]
    NoBinaryForPlatform { agent: String, platform: String },
    #[error("{agent}'s binary needs an install folder")]
    NoInstallDir { agent: String },
    #[error("{agent}'s command {cmd:?} would leave its install folder")]
    UnsafeCommand { agent: String, cmd: String },
    /// None of the entry's methods is ready: no installed binary, and no
    /// `npx` or `uvx` found.
    #[error("{agent} cannot be started here: {detail}")]
    Unavailable { agent: String, detail: String },
}

/// Whether one way of starting an agent works here, as
/// [`Entry::availability`] found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Availability {
    pub method: Method,
    /// Whether the agent can be started this way now.
    pub ready: bool,
    /// The program that was found: `npx`, `uvx`, or the installed binary.
    pub path: Option<PathBuf>,
    /// Why it is not ready, in a sentence for people.
    pub detail: Option<String>,
}

/// The registry's name for this platform, such as `linux-x86_64`,
/// `linux-aarch64-musl` or `darwin-aarch64`, or `None` on a platform the
/// registry does not cover.
pub fn platform() -> Option<&'static str> {
    let os = match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "darwin",
        "windows" => "windows",
        _ => return None,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        _ => return None,
    };
    Some(match (os, arch, cfg!(target_env = "musl")) {
        ("linux", "x86_64", true) => "linux-x86_64-musl",
        ("linux", "aarch64", true) => "linux-aarch64-musl",
        ("linux", "x86_64", false) => "linux-x86_64",
        ("linux", "aarch64", false) => "linux-aarch64",
        ("darwin", "x86_64", _) => "darwin-x86_64",
        ("darwin", "aarch64", _) => "darwin-aarch64",
        ("windows", "x86_64", _) => "windows-x86_64",
        _ => "windows-aarch64",
    })
}

impl Entry {
    /// The methods this entry is distributed through on `platform`, in the
    /// order Keel prefers them: the binary, then `npx`, then `uvx`.
    pub fn methods(&self, platform: Option<&str>) -> Vec<Method> {
        let mut methods = Vec::new();
        if platform.is_some_and(|platform| self.binary_for(platform).is_some()) {
            methods.push(Method::Binary);
        }
        if self.distribution.npx.is_some() {
            methods.push(Method::Npx);
        }
        if self.distribution.uvx.is_some() {
            methods.push(Method::Uvx);
        }
        methods
    }

    /// The archive for `platform`. A glibc Linux platform falls back to
    /// nothing else; a musl one falls back to the glibc build, as the
    /// registry lists musl builds only for some agents.
    pub fn binary_for(&self, platform: &str) -> Option<&Binary> {
        let binaries = &self.distribution.binary;
        binaries.get(platform).or_else(|| {
            platform
                .strip_suffix("-musl")
                .and_then(|glibc| binaries.get(glibc))
        })
    }

    /// A folder to unpack this version's binary into, under `root`:
    /// `<root>/<id>/<version>`.
    pub fn install_dir(&self, root: &Path) -> PathBuf {
        root.join(sanitize(&self.id)).join(sanitize(&self.version))
    }

    /// The command that starts this agent `method`'s way on this platform.
    /// [`Method::Binary`] needs the folder the archive was unpacked into.
    /// Nothing is checked on disk; see [`availability`](Self::availability).
    pub fn launch(
        &self,
        method: Method,
        install_dir: Option<&Path>,
    ) -> Result<Launch, LaunchError> {
        self.launch_on(method, platform(), install_dir)
    }

    fn launch_on(
        &self,
        method: Method,
        platform: Option<&str>,
        install_dir: Option<&Path>,
    ) -> Result<Launch, LaunchError> {
        let not_distributed = || LaunchError::NotDistributed {
            agent: self.id.clone(),
            method,
        };
        let package = |package: &Option<Package>, program: &str, prefix: &[&str]| {
            let package = package.as_ref().ok_or_else(not_distributed)?;
            let mut launch = Launch::new(program)
                .args(prefix.iter().copied())
                .args([package.package.as_str()])
                .args(package.args.iter().cloned());
            for (name, value) in &package.env {
                launch = launch.env(name, value);
            }
            Ok(launch)
        };
        match method {
            Method::Npx => package(&self.distribution.npx, "npx", &["-y"]),
            Method::Uvx => package(&self.distribution.uvx, "uvx", &[]),
            Method::Binary => {
                if self.distribution.binary.is_empty() {
                    return Err(not_distributed());
                }
                let platform = platform.unwrap_or("unknown");
                let binary =
                    self.binary_for(platform)
                        .ok_or_else(|| LaunchError::NoBinaryForPlatform {
                            agent: self.id.clone(),
                            platform: platform.to_owned(),
                        })?;
                let dir = install_dir.ok_or_else(|| LaunchError::NoInstallDir {
                    agent: self.id.clone(),
                })?;
                let program =
                    binary_path(dir, &binary.cmd).ok_or_else(|| LaunchError::UnsafeCommand {
                        agent: self.id.clone(),
                        cmd: binary.cmd.clone(),
                    })?;
                let mut launch = Launch::new(program).args(binary.args.iter().cloned());
                for (name, value) in &binary.env {
                    launch = launch.env(name, value);
                }
                Ok(launch)
            }
        }
    }

    /// Checks cheaply whether each of the entry's methods works here: that
    /// `npx` or `uvx` can be found (on `PATH`, then through the login
    /// shell), or that the binary is unpacked in `install_dir`. Nothing is
    /// started or downloaded.
    pub async fn availability(&self, install_dir: Option<&Path>) -> Vec<Availability> {
        let mut found = Vec::new();
        for method in self.methods(platform()) {
            found.push(self.check(method, install_dir).await);
        }
        found
    }

    async fn check(&self, method: Method, install_dir: Option<&Path>) -> Availability {
        let launch = match self.launch(method, install_dir) {
            Ok(launch) => launch,
            Err(error) => {
                return Availability {
                    method,
                    ready: false,
                    path: None,
                    detail: Some(error.to_string()),
                };
            }
        };
        let path = match method {
            Method::Binary => {
                crate::probe::is_executable(&launch.program).then(|| launch.program.clone())
            }
            Method::Npx | Method::Uvx => crate::probe::resolve(&launch.program).await,
        };
        let detail = path.is_none().then(|| match method {
            Method::Binary => format!("{} is not installed", launch.program.display()),
            _ => format!("`{}` was not found", launch.program.display()),
        });
        Availability {
            method,
            ready: path.is_some(),
            path,
            detail,
        }
    }

    /// The command for the first method that works here, in the order of
    /// [`methods`](Self::methods): an installed binary, then `npx`, then
    /// `uvx`. `npx` and `uvx` are given by the path they were found at.
    pub async fn resolve(&self, install_dir: Option<&Path>) -> Result<Launch, LaunchError> {
        let checks = self.availability(install_dir).await;
        if let Some(ready) = checks.iter().find(|check| check.ready) {
            let mut launch = self.launch(ready.method, install_dir)?;
            if let Some(path) = &ready.path {
                launch.program.clone_from(path);
            }
            return Ok(launch);
        }
        let detail = if checks.is_empty() {
            "no distribution for this platform".to_owned()
        } else {
            checks
                .iter()
                .filter_map(|check| check.detail.clone())
                .collect::<Vec<_>>()
                .join("; ")
        };
        Err(LaunchError::Unavailable {
            agent: self.id.clone(),
            detail,
        })
    }
}

/// `cmd` inside `dir`, unless it is absolute or climbs out of it.
fn binary_path(dir: &Path, cmd: &str) -> Option<PathBuf> {
    let relative = Path::new(cmd);
    let mut path = dir.to_path_buf();
    for component in relative.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => path.push(part),
            _ => return None,
        }
    }
    (path != dir).then_some(path)
}

/// A registry value as one path component.
fn sanitize(text: &str) -> String {
    let clean: String = text
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.+@".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    if clean.is_empty() || clean.chars().all(|c| c == '.') {
        "_".to_owned()
    } else {
        clean
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "version": "1.0.0",
      "agents": [
        {
          "id": "both", "name": "Both", "version": "1.2.3",
          "description": "Binary and npx", "repository": "https://example.com/both",
          "authors": ["A"], "license": "MIT", "icon": "https://example.com/icon.svg",
          "distribution": {
            "binary": {
              "linux-x86_64": { "archive": "https://example.com/b.tar.gz", "sha256": "ab", "cmd": "./bin/both", "args": ["acp"], "env": { "MODE": "acp" } },
              "darwin-aarch64": { "archive": "https://example.com/b.zip", "cmd": "both" }
            },
            "npx": { "package": "@example/both@1.2.3", "args": ["--acp"], "env": { "NO_UPDATE": "1" } }
          },
          "unknownField": true
        },
        {
          "id": "py", "name": "Py", "version": "0.1.0",
          "distribution": { "uvx": { "package": "py-agent==0.1.0", "args": ["acp"] } }
        },
        {
          "id": "evil", "name": "Evil", "version": "1",
          "distribution": { "binary": { "linux-x86_64": { "archive": "x", "cmd": "../../bin/sh" } } }
        }
      ],
      "extensions": []
    }"#;

    #[test]
    fn parses_entries_and_ignores_unknown_fields() {
        let registry = Registry::parse(SAMPLE).unwrap();
        assert_eq!(registry.version, "1.0.0");
        let both = registry.get("both").unwrap();
        assert_eq!(both.license.as_deref(), Some("MIT"));
        let binary = both.binary_for("linux-x86_64").unwrap();
        assert_eq!(binary.sha256.as_deref(), Some("ab"));
        assert_eq!(binary.env["MODE"], "acp");
        assert_eq!(
            both.binary_for("linux-x86_64-musl"),
            Some(binary),
            "musl falls back to glibc"
        );
        assert_eq!(both.binary_for("windows-x86_64"), None);
        let py = registry.get("py").unwrap();
        assert_eq!(py.description, "");
        assert!(py.distribution.binary.is_empty());
        assert!(Registry::parse("{}").is_err());
    }

    #[test]
    fn package_launches() {
        let registry = Registry::parse(SAMPLE).unwrap();
        let npx = registry
            .get("both")
            .unwrap()
            .launch(Method::Npx, None)
            .unwrap();
        assert_eq!(npx.program, PathBuf::from("npx"));
        assert_eq!(npx.args, ["-y", "@example/both@1.2.3", "--acp"]);
        assert_eq!(npx.env, [("NO_UPDATE".to_owned(), "1".to_owned())]);
        let uvx = registry
            .get("py")
            .unwrap()
            .launch(Method::Uvx, None)
            .unwrap();
        assert_eq!(uvx.program, PathBuf::from("uvx"));
        assert_eq!(uvx.args, ["py-agent==0.1.0", "acp"]);
        assert_eq!(
            registry.get("py").unwrap().launch(Method::Npx, None),
            Err(LaunchError::NotDistributed {
                agent: "py".into(),
                method: Method::Npx
            })
        );
    }

    #[test]
    fn binary_launches_stay_in_their_install_folder() {
        let registry = Registry::parse(SAMPLE).unwrap();
        let both = registry.get("both").unwrap();
        let dir = Path::new("/opt/agents/both/1.2.3");
        let launch = both
            .launch_on(Method::Binary, Some("linux-x86_64"), Some(dir))
            .unwrap();
        assert_eq!(launch.program, dir.join("bin/both"));
        assert_eq!(launch.args, ["acp"]);
        assert_eq!(
            both.launch_on(Method::Binary, Some("linux-x86_64"), None),
            Err(LaunchError::NoInstallDir {
                agent: "both".into()
            })
        );
        assert!(matches!(
            both.launch_on(Method::Binary, Some("windows-x86_64"), Some(dir)),
            Err(LaunchError::NoBinaryForPlatform { .. })
        ));
        let evil = registry.get("evil").unwrap();
        assert!(matches!(
            evil.launch_on(Method::Binary, Some("linux-x86_64"), Some(dir)),
            Err(LaunchError::UnsafeCommand { .. })
        ));
        assert_eq!(binary_path(dir, "/bin/sh"), None);
        assert_eq!(binary_path(dir, "."), None);
        assert_eq!(
            both.install_dir(Path::new("/opt/agents")),
            Path::new("/opt/agents/both/1.2.3")
        );
        assert_eq!(sanitize(".."), "_");
        assert_eq!(sanitize("a/b"), "a_b");
    }

    #[test]
    fn methods_follow_the_preferred_order() {
        let registry = Registry::parse(SAMPLE).unwrap();
        let both = registry.get("both").unwrap();
        assert_eq!(
            both.methods(Some("linux-x86_64")),
            [Method::Binary, Method::Npx]
        );
        assert_eq!(both.methods(Some("windows-x86_64")), [Method::Npx]);
        assert_eq!(both.methods(None), [Method::Npx]);
    }

    #[tokio::test]
    async fn an_installed_binary_is_available() {
        let registry = Registry::parse(SAMPLE).unwrap();
        let both = registry.get("both").unwrap();
        let root = std::env::temp_dir().join(format!("keel-registry-{}", std::process::id()));
        let dir = both.install_dir(&root);
        let binary = both.check(Method::Binary, Some(&dir)).await;
        if platform() == Some("linux-x86_64") {
            assert!(!binary.ready);
            std::fs::create_dir_all(dir.join("bin")).unwrap();
            #[cfg(unix)]
            crate::process::write_executable(&dir.join("bin/both"), "#!/bin/sh\n");
            let binary = both.check(Method::Binary, Some(&dir)).await;
            assert!(binary.ready, "{binary:?}");
            let launch = both.resolve(Some(&dir)).await.unwrap();
            assert_eq!(launch.program, dir.join("bin/both"));
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_bundled_registry_parses() {
        let registry = Registry::bundled();
        assert!(registry.agents.len() >= 40);
        assert!(registry.agents.iter().all(|entry| {
            !entry.methods(Some("linux-x86_64")).is_empty()
                || !entry.methods(Some("darwin-aarch64")).is_empty()
        }));
        let claude = registry.get("claude-acp").unwrap();
        assert!(claude.distribution.npx.is_some());
        assert!(registry.agents.iter().all(|entry| entry.icon.is_none()));
    }
}
