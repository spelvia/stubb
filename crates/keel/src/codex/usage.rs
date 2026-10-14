//! Codex's read-only `account/rateLimits/read`, over the shared app-server
//! connection: no thread is started and no prompt is sent.

use serde_json::Value;

use crate::usage::{Account, Window};

/// The account quotas in a `account/rateLimits/read` result: every limit in
/// `rateLimitsByLimitId`, or the single `rateLimits` of older versions.
pub(super) fn account(raw: Value) -> Account {
    let mut buckets: Vec<(String, &Value)> = raw
        .get("rateLimitsByLimitId")
        .and_then(Value::as_object)
        .map(|limits| {
            limits
                .iter()
                .filter(|(_, limit)| limit.is_object())
                .map(|(id, limit)| (id.clone(), limit))
                .collect()
        })
        .unwrap_or_default();
    if buckets.is_empty()
        && let Some(limit) = raw.get("rateLimits").filter(|limit| limit.is_object())
    {
        let id = limit
            .get("limitId")
            .and_then(Value::as_str)
            .unwrap_or("codex")
            .to_owned();
        buckets.push((id, limit));
    }
    // Codex's own limit first, then the others by name.
    buckets.sort_by(|(a, _), (b, _)| (a != "codex", a).cmp(&(b != "codex", b)));
    let mut windows = Vec::new();
    let mut plan = None;
    for (id, limit) in &buckets {
        plan = plan.or_else(|| {
            limit
                .get("planType")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
        let name = limit
            .get("limitName")
            .and_then(Value::as_str)
            .filter(|_| id != "codex");
        for slot in ["primary", "secondary"] {
            let Some(window) = limit.get(slot).filter(|window| window.is_object()) else {
                continue;
            };
            let duration = window
                .get("windowDurationMins")
                .and_then(Value::as_u64)
                .and_then(|minutes| u32::try_from(minutes).ok());
            let label = duration_label(duration);
            windows.push(Window {
                id: format!("{id}:{slot}"),
                label: match name {
                    Some(name) => format!("{name} {}", lowercase_first(&label)),
                    None => label,
                },
                used_percent: window
                    .get("usedPercent")
                    .and_then(Value::as_f64)
                    .filter(|value| value.is_finite() && (0.0..=100.0).contains(value)),
                duration_minutes: duration,
                resets_at: window.get("resetsAt").and_then(Value::as_i64).map(rfc3339),
            });
        }
    }
    Account {
        provider: super::PROVIDER_ID.into(),
        plan,
        limits_available: !windows.is_empty(),
        windows,
        raw,
    }
}

fn duration_label(minutes: Option<u32>) -> String {
    match minutes {
        Some(300) => "5-hour limit".into(),
        Some(10080) => "Weekly limit".into(),
        Some(minutes) if minutes % 1440 == 0 => format!("{}-day limit", minutes / 1440),
        Some(minutes) if minutes % 60 == 0 => format!("{}-hour limit", minutes / 60),
        Some(minutes) => format!("{minutes}-minute limit"),
        None => "Limit".into(),
    }
}

fn lowercase_first(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_lowercase().chain(chars).collect()
    })
}

/// Unix seconds as an RFC 3339 UTC time, such as `2026-10-12T16:09:59Z`.
fn rfc3339(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let time = seconds.rem_euclid(86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time / 3600,
        time % 3600 / 60,
        time % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_every_limit_with_codex_first() {
        let usage = account(json!({"rateLimitsByLimitId": {
            "other": {"limitName": "GPT-6 Spark", "primary": {"usedPercent": 80, "windowDurationMins": 300, "resetsAt": 1792482389}},
            "codex": {"planType": "pro", "primary": {"usedPercent": 16, "windowDurationMins": 10080}, "secondary": null}
        }}));
        assert_eq!(usage.provider, "codex");
        assert_eq!(usage.plan.as_deref(), Some("pro"));
        assert!(usage.limits_available);
        assert_eq!(usage.windows.len(), 2);
        assert_eq!(usage.windows[0].id, "codex:primary");
        assert_eq!(usage.windows[0].label, "Weekly limit");
        assert_eq!(usage.windows[0].used_percent, Some(16.0));
        assert_eq!(usage.windows[0].resets_at, None);
        assert_eq!(usage.windows[1].label, "GPT-6 Spark 5-hour limit");
        assert_eq!(
            usage.windows[1].resets_at.as_deref(),
            Some("2026-10-20T07:46:29Z")
        );
    }

    #[test]
    fn reads_the_single_limit_of_older_versions_and_keeps_unknowns_unknown() {
        let usage = account(json!({"rateLimits": {"limitId": "codex",
            "primary": {"usedPercent": 31, "windowDurationMins": 300}, "secondary": {"usedPercent": 140}}}));
        assert_eq!(usage.windows[0].label, "5-hour limit");
        assert_eq!(usage.windows[1].label, "Limit");
        assert_eq!(usage.windows[1].used_percent, None);
        let none = account(json!({"rateLimits": null}));
        assert!(!none.limits_available);
        assert!(none.windows.is_empty());
    }

    #[test]
    fn formats_reset_times_in_utc() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_792_482_389), "2026-10-20T07:46:29Z");
    }
}
