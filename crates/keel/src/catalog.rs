//! Models a provider offers and the settings that depend on them.
//!
//! Use the types through the module path, for example `catalog::Model`.

use serde::{Deserialize, Serialize};

/// Models offered by one provider installation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Catalog {
    pub provider: ProviderInfo,
    pub models: Vec<Model>,
}

/// Identity of the provider installation that produced a catalog.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInfo {
    /// Keel provider identifier, for example `codex`.
    pub id: String,
    /// Harness version reported by the agent, when it could be determined.
    pub version: Option<String>,
    /// Raw user agent string reported during initialization.
    pub user_agent: Option<String>,
}

/// A selectable model and the settings that depend on it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    /// Provider model identifier, passed back verbatim when selecting it.
    pub id: String,
    pub label: String,
    pub description: String,
    pub is_default: bool,
    /// Reasoning effort levels this model accepts, in provider order.
    pub efforts: Vec<Effort>,
    pub default_effort: Option<String>,
    pub fast_mode: FastMode,
}

/// A reasoning effort level. The identifier is provider-specific.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Effort {
    pub id: String,
    pub description: String,
}

/// Whether a model can run in the provider's faster service tier.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum FastMode {
    Unavailable,
    Available {
        /// Provider tier identifier to request, for example `priority`.
        tier_id: String,
        label: String,
        description: String,
        /// Whether the provider enables this tier by default for the model.
        default_on: bool,
    },
}
