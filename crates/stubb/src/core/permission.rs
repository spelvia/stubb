//! Provider-specific permission configuration for session start and resume.
//!
//! Approval rules and sandbox isolation are separate native controls. Omitted
//! settings retain native defaults or stored values; Stubb never invents a common
//! permission mode across providers.
//!
//! Use [`crate::providers::codex::Permissions`] through [`Configuration::Codex`] and
//! [`crate::providers::claude::Permissions`] through [`Configuration::Claude`].
//! [`crate::session::Info::permissions`] separates requested settings from native
//! reported configuration. Each provider rejects the others' settings; Pi
//! has none.

use serde::{Deserialize, Serialize};

/// Requested permission configuration. Other providers reject a variant
/// that is not theirs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "camelCase")]
#[non_exhaustive]
pub enum Configuration {
    Codex(crate::providers::codex::Permissions),
    Claude(crate::providers::claude::Permissions),
}

/// Native configuration reported at session attachment time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "camelCase")]
#[non_exhaustive]
pub enum EffectiveConfiguration {
    Codex(crate::providers::codex::EffectivePermissions),
}

/// Requested configuration is distinct from provider-confirmed configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Info {
    pub requested: Option<Configuration>,
    pub effective: EffectiveConfiguration,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Error, Provider, session};

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
            permissions: Some(Configuration::Codex(
                crate::providers::codex::Permissions::default(),
            )),
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
