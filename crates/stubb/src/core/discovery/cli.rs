//! The command-line agents behind Stubb's built-in providers.

use std::fmt;
use std::hash::{Hash, Hasher};
use std::path::Path;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::probe::Auth;
use super::profile::Profile;
use super::update::Package;
use crate::BoxFuture;

/// Asks an agent's executable whether it is signed in: the state, and why it
/// could not tell when it could not.
pub(crate) type AuthProbe = fn(&Path) -> BoxFuture<'_, (Auth, Option<String>)>;

/// The command-line agent a built-in provider drives, such as
/// [`providers::codex::CLI`](crate::providers::codex::CLI): its executable,
/// how to check its sign-in, who makes it, and how it updates.
///
/// [`clis`] lists the agents of the providers compiled into Stubb. Two `Cli`s
/// are equal when their [`id`](Cli::id)s are; one serializes as its id.
#[derive(Clone, Copy)]
pub struct Cli {
    pub(crate) id: &'static str,
    pub(crate) program: &'static str,
    pub(crate) profile: fn() -> Profile,
    pub(crate) package: fn() -> Package,
    /// The arguments that make the agent update itself.
    pub(crate) update_args: &'static [&'static str],
    /// A path fragment that marks the agent's own installer.
    pub(crate) native_install: Option<&'static str>,
    pub(crate) auth: AuthProbe,
}

impl Cli {
    /// The provider id, as [`crate::Provider::id`] reports it (`"codex"`).
    pub fn id(&self) -> &'static str {
        self.id
    }

    /// The executable's usual name (`"codex"`).
    pub fn program(&self) -> &'static str {
        self.program
    }

    /// Who the agent is, for hosts to show, with its bundled logo.
    pub fn profile(&self) -> Profile {
        (self.profile)()
    }

    /// The npm package the agent is published as, and the channel it follows.
    pub fn package(&self) -> Package {
        (self.package)()
    }
}

/// The agents of every built-in provider compiled into Stubb, in a stable
/// order.
pub fn clis() -> &'static [Cli] {
    static CLIS: &[Cli] = &[
        #[cfg(feature = "claude")]
        crate::providers::claude::CLI,
        #[cfg(feature = "codex")]
        crate::providers::codex::CLI,
        #[cfg(feature = "pi")]
        crate::providers::pi::CLI,
    ];
    CLIS
}

/// The built-in agent with provider id `id`, if it is compiled in.
pub fn cli(id: &str) -> Option<Cli> {
    clis().iter().copied().find(|cli| cli.id == id)
}

impl fmt::Debug for Cli {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Cli").field(&self.id).finish()
    }
}

impl PartialEq for Cli {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for Cli {}

impl Hash for Cli {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

impl Serialize for Cli {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.id)
    }
}

impl<'de> Deserialize<'de> for Cli {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let id = String::deserialize(deserializer)?;
        cli(&id).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "`{id}` is not a built-in agent compiled into Stubb"
            ))
        })
    }
}
