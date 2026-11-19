//! Permission configuration for session start and resume.
//!
//! Approval rules and sandbox isolation are native controls, and they differ
//! between providers. Stubb never invents a common permission mode: each
//! provider defines its own typed settings (for example
//! `stubb::providers::codex::Permissions`) and implements [`Native`] for them.
//! A [`Configuration`] carries one provider's settings through the
//! provider-neutral [`crate::session::Settings`].
//!
//! ```
//! # #[cfg(feature = "codex")] {
//! use stubb::permission::Configuration;
//! use stubb::providers::codex::{Permissions, Sandbox};
//!
//! let configuration = Configuration::new(&Permissions {
//!     sandbox: Some(Sandbox::ReadOnly),
//!     ..Default::default()
//! });
//! assert_eq!(configuration.provider(), "codex");
//! let permissions: Option<Permissions> = configuration.get().unwrap();
//! assert_eq!(permissions.unwrap().sandbox, Some(Sandbox::ReadOnly));
//! # }
//! ```
//!
//! [`crate::session::Info::permissions`] separates requested settings from the
//! configuration the provider reports. Providers reject settings that belong to
//! another provider; Pi has none.

use serde::de::{DeserializeOwned, Error as _};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

use crate::{Error, Result};

/// A provider's typed permission settings.
pub trait Native: Serialize + DeserializeOwned {
    /// The [`crate::Provider::id`] these settings belong to.
    const PROVIDER: &'static str;
}

/// One provider's permission settings, in a provider-neutral form.
///
/// Serializes as the provider's own fields plus a `provider` tag, for example
/// `{"provider": "codex", "sandbox": "read-only"}`.
#[derive(Debug, Clone, PartialEq)]
pub struct Configuration {
    provider: String,
    settings: Map<String, Value>,
}

impl Configuration {
    /// Wraps typed settings.
    ///
    /// # Panics
    ///
    /// When `settings` does not serialize to a JSON object. Every [`Native`]
    /// type in Stubb is a struct, which always does.
    pub fn new<P: Native>(settings: &P) -> Self {
        let settings = match serde_json::to_value(settings) {
            Ok(Value::Object(map)) => map,
            Ok(Value::Null) => Map::new(),
            Ok(other) => panic!("permission settings must serialize as an object, not {other}"),
            Err(error) => panic!("permission settings must serialize: {error}"),
        };
        Self {
            provider: P::PROVIDER.to_owned(),
            settings,
        }
    }

    /// The provider these settings belong to.
    pub fn provider(&self) -> &str {
        &self.provider
    }

    /// Whether the settings belong to `P`'s provider.
    pub fn is<P: Native>(&self) -> bool {
        self.provider == P::PROVIDER
    }

    /// The typed settings, or `None` when they belong to another provider.
    /// Settings for `P`'s provider that do not match `P` are an
    /// [`Error::InvalidSetting`].
    pub fn get<P: Native>(&self) -> Result<Option<P>> {
        if !self.is::<P>() {
            return Ok(None);
        }
        serde_json::from_value(Value::Object(self.settings.clone()))
            .map(Some)
            .map_err(|error| Error::InvalidSetting {
                setting: "permissions",
                value: self.provider.clone(),
                reason: error.to_string(),
            })
    }
}

impl Serialize for Configuration {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        let mut map = Map::with_capacity(self.settings.len() + 1);
        map.insert("provider".to_owned(), Value::String(self.provider.clone()));
        map.extend(self.settings.clone());
        map.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Configuration {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let mut settings = Map::deserialize(deserializer)?;
        match settings.remove("provider") {
            Some(Value::String(provider)) => Ok(Self { provider, settings }),
            _ => Err(D::Error::missing_field("provider")),
        }
    }
}

/// Requested configuration is distinct from provider-confirmed configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Info {
    pub requested: Option<Configuration>,
    /// The configuration the provider reports, in its own typed form (for
    /// Codex, `stubb::providers::codex::EffectivePermissions`).
    pub effective: Configuration,
}

#[cfg(all(test, feature = "codex", feature = "claude", feature = "pi"))]
mod tests {
    use super::*;
    use crate::providers::{claude, codex};
    use crate::{Error, Provider, session};
    use serde_json::json;

    #[test]
    fn stored_json_keeps_the_provider_tag_and_native_fields() {
        let configuration = Configuration::new(&codex::Permissions {
            sandbox: Some(codex::Sandbox::ReadOnly),
            ..Default::default()
        });
        let value = serde_json::to_value(&configuration).unwrap();
        // Byte-for-byte what 0.1.0-rc.1 stored, so saved settings keep loading.
        assert_eq!(
            value,
            json!({"provider": "codex", "sandbox": {"mode": "readOnly"}})
        );
        let read: Configuration = serde_json::from_value(value).unwrap();
        assert_eq!(read, configuration);
        assert_eq!(
            read.get::<codex::Permissions>().unwrap().unwrap().sandbox,
            Some(codex::Sandbox::ReadOnly)
        );
    }

    #[test]
    fn settings_for_another_provider_are_not_decoded() {
        let configuration: Configuration = claude::Permissions {
            mode: Some(claude::PermissionMode::Plan),
        }
        .into();
        assert!(!configuration.is::<codex::Permissions>());
        assert_eq!(configuration.get::<codex::Permissions>().unwrap(), None);
    }

    #[test]
    fn malformed_settings_are_an_invalid_setting() {
        let configuration: Configuration =
            serde_json::from_value(json!({"provider": "claude", "mode": "no-such-mode"})).unwrap();
        assert!(matches!(
            configuration.get::<claude::Permissions>(),
            Err(Error::InvalidSetting {
                setting: "permissions",
                ..
            })
        ));
        assert!(serde_json::from_value::<Configuration>(json!({"mode": "plan"})).is_err());
    }

    #[tokio::test]
    async fn other_providers_reject_codex_configuration_before_native_io() {
        let providers: Vec<Box<dyn Provider>> = vec![
            Box::new(crate::providers::pi::Pi::new(
                crate::providers::pi::PiConfig {
                    executable: "/missing/pi".into(),
                    ..Default::default()
                },
            )),
            Box::new(crate::providers::claude::Claude::new(
                crate::providers::claude::ClaudeConfig {
                    executable: "/missing/claude".into(),
                    ..Default::default()
                },
            )),
        ];
        let settings = session::Settings {
            permissions: Some(crate::providers::codex::Permissions::default().into()),
            ..Default::default()
        };
        for provider in providers {
            assert!(matches!(
                provider
                    .start_session(session::Spec {
                        cwd: "/missing/work".into(),
                        settings: settings.clone()
                    })
                    .await,
                Err(Error::Unsupported {
                    operation: "permission_configuration"
                })
            ));
            assert!(matches!(
                provider.resume_session("missing", settings.clone()).await,
                Err(Error::Unsupported {
                    operation: "permission_configuration"
                })
            ));
        }
    }
}
