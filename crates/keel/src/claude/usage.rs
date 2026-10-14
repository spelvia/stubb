//! Claude Code's read-only `get_usage` control response. No user input or
//! inference is needed; older CLIs may reject the experimental request.

use serde_json::Value;

use crate::usage::{Account, Window};

pub(super) fn account(raw: Value) -> Account {
    let limits = raw.get("rate_limits").unwrap_or(&Value::Null);
    let mut windows = Vec::new();
    for (id, label, duration) in [
        ("five_hour", "5-hour limit", 300),
        ("seven_day", "Weekly limit", 10080),
        ("seven_day_opus", "Opus weekly limit", 10080),
        ("seven_day_sonnet", "Sonnet weekly limit", 10080),
        ("seven_day_oauth_apps", "OAuth apps weekly limit", 10080),
        ("seven_day_cowork", "Cowork weekly limit", 10080),
    ] {
        let Some(value) = limits.get(id).filter(|value| value.is_object()) else {
            continue;
        };
        windows.push(Window {
            id: id.into(),
            label: label.into(),
            duration_minutes: Some(duration),
            used_percent: percent(value.get("utilization")),
            resets_at: value
                .get("resets_at")
                .and_then(Value::as_str)
                .map(str::to_owned),
        });
    }
    // Newer CLIs can also report named model quotas, beyond Opus/Sonnet.
    for value in limits
        .get("model_scoped")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let Some(name) = value.get("display_name").and_then(Value::as_str) else {
            continue;
        };
        let label = format!("{name} weekly limit");
        if windows
            .iter()
            .any(|window| window.label.eq_ignore_ascii_case(&label))
        {
            continue;
        }
        windows.push(Window {
            id: format!("model:{name}"),
            label,
            duration_minutes: Some(10080),
            used_percent: percent(value.get("utilization")),
            resets_at: value
                .get("resets_at")
                .and_then(Value::as_str)
                .map(str::to_owned),
        });
    }
    Account {
        provider: "claude".into(),
        plan: raw
            .get("subscription_type")
            .and_then(Value::as_str)
            .map(str::to_owned),
        limits_available: raw
            .get("rate_limits_available")
            .and_then(Value::as_bool)
            .unwrap_or(!windows.is_empty()),
        windows,
        raw,
    }
}

fn percent(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && (0.0..=100.0).contains(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn native_percentages_are_not_fractions_and_missing_is_unknown() {
        let usage = account(
            json!({"subscription_type":"max", "rate_limits_available":true,
            "rate_limits": {
                "five_hour":{"utilization":100,"resets_at":"2026-10-12T16:09:59Z"},
                "seven_day":{"utilization":62}, "seven_day_sonnet": null,
                "model_scoped":[{"display_name":"Fable","utilization":0},{"display_name":"Other"}]
            }}),
        );
        assert_eq!(usage.plan.as_deref(), Some("max"));
        assert_eq!(usage.windows.len(), 4);
        assert_eq!(usage.windows[0].used_percent, Some(100.0));
        assert_eq!(usage.windows[1].duration_minutes, Some(10080));
        assert_eq!(usage.windows[2].used_percent, Some(0.0));
        assert_eq!(usage.windows[3].used_percent, None);
        assert_eq!(
            usage.windows[0].resets_at.as_deref(),
            Some("2026-10-12T16:09:59Z")
        );
    }

    #[test]
    fn no_subscription_is_not_an_empty_quota() {
        let usage = account(json!({"rate_limits_available":false, "rate_limits":null}));
        assert!(!usage.limits_available);
        assert!(usage.windows.is_empty());
        assert_eq!(usage.plan, None);
    }
}

#[cfg(all(test, unix))]
mod transport_tests {
    use crate::claude::{Claude, ClaudeConfig};

    #[tokio::test]
    async fn account_query_sends_only_initialize_and_get_usage() {
        let folder = std::env::temp_dir().join(format!("keel-usage-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&folder).unwrap();
        let executable = folder.join("claude");
        crate::process::write_executable(
            &executable,
            r#"#!/bin/sh
IFS= read -r request
case "$request" in *'"subtype":"initialize"'*) ;; *) exit 1;; esac
printf '%s\n' '{"type":"control_response","response":{"subtype":"success","request_id":"keel_0","response":{}}}'
IFS= read -r request
case "$request" in *'"subtype":"get_usage"'*) ;; *) exit 1;; esac
printf '%s\n' '{"type":"control_response","response":{"subtype":"success","request_id":"keel_1","response":{"subscription_type":"max","rate_limits_available":true,"rate_limits":{"five_hour":{"utilization":42}}}}}'
# Wait for the host to close instead of launching any inference.
IFS= read -r unexpected
exit 1
"#,
        );
        let provider = Claude::new(ClaudeConfig {
            executable,
            ..ClaudeConfig::default()
        });
        let result = provider.account_usage().await;
        std::fs::remove_dir_all(folder).unwrap();
        let result = result.unwrap();
        assert_eq!(result.windows[0].used_percent, Some(42.0));
    }
}
