//! Provider-specific permission configuration for session start and resume.
//!
//! Approval rules and sandbox isolation are separate native controls. Omitted
//! settings retain native defaults or stored values; Keel never invents a common
//! permission mode across providers.
//!
//! Use [`crate::codex::Permissions`] through [`Configuration::Codex`].
//! [`crate::session::Info::permissions`] separates requested settings from native
//! reported configuration. Claude and Pi reject Codex settings.

use serde::{Deserialize, Serialize};

/// Requested permission configuration. Other providers reject a Codex variant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "camelCase")]
pub enum Configuration {
    Codex(crate::codex::Permissions),
}

/// Native configuration reported at session attachment time.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "camelCase")]
pub enum EffectiveConfiguration {
    Codex(crate::codex::EffectivePermissions),
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
            Box::new(crate::pi::Pi::new(crate::pi::PiConfig {
                executable: "/missing/pi".into(),
                ..Default::default()
            })),
            Box::new(crate::claude::Claude::new(crate::claude::ClaudeConfig {
                executable: "/missing/claude".into(),
                ..Default::default()
            })),
        ];
        let settings = session::Settings {
            permissions: Some(Configuration::Codex(crate::codex::Permissions::default())),
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
