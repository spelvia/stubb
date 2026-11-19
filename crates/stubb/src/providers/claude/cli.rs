//! The Claude Code command-line agent, for discovery.

use std::path::{Path, PathBuf};

use crate::BoxFuture;
use crate::discovery::profile::{Logo, LogoSource, Profile};
use crate::discovery::update::Package;
use crate::discovery::{Auth, Cli};

/// The `claude` executable this provider drives.
pub const CLI: Cli = Cli {
    id: super::PROVIDER_ID,
    program: "claude",
    profile,
    package,
    update_args: &["update"],
    native_install: Some("/claude/versions/"),
    auth,
};

fn auth(path: &Path) -> BoxFuture<'_, (Auth, Option<String>)> {
    Box::pin(super::probe::auth(path))
}

fn profile() -> Profile {
    Profile {
        id: super::PROVIDER_ID.to_owned(),
        name: "Claude Code".to_owned(),
        maker: Some("Anthropic".to_owned()),
        description: "Anthropic's agentic coding tool, in the terminal.".to_owned(),
        homepage: Some("https://claude.com/product/claude-code".to_owned()),
        repository: Some("https://github.com/anthropics/claude-code".to_owned()),
        logo: Logo {
            source: LogoSource::Svg(include_str!("logo.svg").to_owned()),
            monochrome: false,
        },
    }
}

fn package() -> Package {
    Package {
        npm: "@anthropic-ai/claude-code".to_owned(),
        tag: claude_channel().unwrap_or_else(|| "latest".to_owned()),
    }
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
