//! Account-wide quotas reported by a native provider. These are separate
//! from a turn's token counts; missing values remain unknown.

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Account {
    pub provider: String,
    pub plan: Option<String>,
    pub limits_available: bool,
    pub windows: Vec<Window>,
    /// Native account response, including spend and usage breakdowns. Hosts
    /// should select the fields they need rather than expose it indiscriminately.
    pub raw: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Window {
    pub id: String,
    pub label: String,
    /// Percentage used, in the native 0–100 unit. Missing is not zero.
    pub used_percent: Option<f64>,
    pub duration_minutes: Option<u32>,
    /// Native RFC3339 reset time; absent if not reported.
    pub resets_at: Option<String>,
}
