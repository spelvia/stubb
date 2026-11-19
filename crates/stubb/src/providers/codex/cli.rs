//! The Codex command-line agent, for discovery.

use std::path::Path;

use crate::BoxFuture;
use crate::discovery::profile::{Logo, LogoSource, Profile};
use crate::discovery::update::Package;
use crate::discovery::{Auth, Cli};

/// The `codex` executable this provider drives.
pub const CLI: Cli = Cli {
    id: super::PROVIDER_ID,
    program: "codex",
    profile,
    package,
    update_args: &["update"],
    native_install: None,
    auth,
};

fn auth(path: &Path) -> BoxFuture<'_, (Auth, Option<String>)> {
    Box::pin(super::probe::auth(path))
}

fn profile() -> Profile {
    Profile {
        id: super::PROVIDER_ID.to_owned(),
        name: "Codex".to_owned(),
        maker: Some("OpenAI".to_owned()),
        description: "OpenAI's coding agent, in the terminal.".to_owned(),
        homepage: Some("https://openai.com/codex".to_owned()),
        repository: Some("https://github.com/openai/codex".to_owned()),
        logo: Logo {
            source: LogoSource::Svg(include_str!("logo.svg").to_owned()),
            monochrome: true,
        },
    }
}

fn package() -> Package {
    Package {
        npm: "@openai/codex".to_owned(),
        tag: "latest".to_owned(),
    }
}
