//! The Pi command-line agent, for discovery.

use std::path::Path;

use crate::BoxFuture;
use crate::discovery::profile::{Logo, LogoSource, Profile};
use crate::discovery::update::Package;
use crate::discovery::{Auth, Cli};

/// The `pi` executable this provider drives.
pub const CLI: Cli = Cli {
    id: super::PROVIDER_ID,
    program: "pi",
    profile,
    package,
    update_args: &["update", "--self"],
    native_install: None,
    auth,
};

fn auth(path: &Path) -> BoxFuture<'_, (Auth, Option<String>)> {
    Box::pin(super::probe::auth(path))
}

fn profile() -> Profile {
    Profile {
        id: super::PROVIDER_ID.to_owned(),
        name: "Pi".to_owned(),
        maker: Some("Earendil".to_owned()),
        description: "A minimal, extensible coding agent.".to_owned(),
        homepage: None,
        repository: Some("https://github.com/earendil-works/pi".to_owned()),
        logo: Logo {
            source: LogoSource::Svg(include_str!("logo.svg").to_owned()),
            monochrome: false,
        },
    }
}

fn package() -> Package {
    Package {
        npm: "@earendil-works/pi-coding-agent".to_owned(),
        tag: "latest".to_owned(),
    }
}
