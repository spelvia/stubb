//! Native Pi authentication probe.

use std::path::{Path, PathBuf};

use crate::core::discovery::status::run;
use crate::probe::Auth;

pub(crate) async fn auth(path: &Path) -> (Auth, Option<String>) {
    let Some(provider) = pi_default_provider() else {
        return (
            Auth::Unknown,
            Some("Pi has no default provider configured".to_owned()),
        );
    };
    let args = [
        "auth",
        "check",
        "--provider",
        &provider,
        "--json",
        "--no-refresh",
    ];
    match run(path, &args).await {
        Ok(out) => parse_pi_auth(&out.stdout),
        Err(error) => (Auth::Unknown, Some(error)),
    }
}

/// Pi's default provider, from its settings file.
fn pi_default_provider() -> Option<String> {
    let dir = std::env::var_os("PI_CODING_AGENT_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".pi").join("agent"))
        })?;
    let text = std::fs::read_to_string(dir.join("settings.json")).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    value
        .get("defaultProvider")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn parse_pi_auth(stdout: &str) -> (Auth, Option<String>) {
    let value = serde_json::from_str::<serde_json::Value>(stdout.trim()).ok();
    match value
        .as_ref()
        .and_then(|v| v.get("status"))
        .and_then(serde_json::Value::as_str)
    {
        Some("ready") => (Auth::SignedIn { account: None }, None),
        Some(other) => (
            Auth::SignedOut,
            Some(format!("Pi reports its default provider as `{other}`")),
        ),
        None => (
            Auth::Unknown,
            Some("`pi auth check` did not answer in JSON".to_owned()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pi_readiness_comes_from_its_json() {
        assert_eq!(
            parse_pi_auth(r#"{"status":"ready","provider":"x"}"#).0,
            Auth::SignedIn { account: None }
        );
        assert_eq!(parse_pi_auth(r#"{"status":"missing"}"#).0, Auth::SignedOut);
        assert_eq!(parse_pi_auth("").0, Auth::Unknown);
    }
}
