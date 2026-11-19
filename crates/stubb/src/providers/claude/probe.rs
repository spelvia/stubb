//! Native Claude authentication probe.

use std::path::Path;

use crate::core::discovery::status::run;
use crate::discovery::Auth;

pub(crate) async fn auth(path: &Path) -> (Auth, Option<String>) {
    match run(path, &["auth", "status"]).await {
        Ok(out) => parse_claude_auth(&out.stdout),
        Err(error) => (Auth::Unknown, Some(error)),
    }
}

fn parse_claude_auth(stdout: &str) -> (Auth, Option<String>) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(stdout.trim()) else {
        return (
            Auth::Unknown,
            Some("`claude auth status` did not answer in JSON".to_owned()),
        );
    };
    match value.get("loggedIn").and_then(serde_json::Value::as_bool) {
        Some(true) => {
            let account = value
                .get("email")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            (Auth::SignedIn { account }, None)
        }
        Some(false) => (Auth::SignedOut, None),
        None => (
            Auth::Unknown,
            Some("`claude auth status` did not say whether it is signed in".to_owned()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_status_names_the_account() {
        let (auth, detail) = parse_claude_auth(r#"{"loggedIn": true, "email": "a@example.com"}"#);
        assert_eq!(
            auth,
            Auth::SignedIn {
                account: Some("a@example.com".into())
            }
        );
        assert_eq!(detail, None);
        assert_eq!(
            parse_claude_auth(r#"{"loggedIn": false}"#).0,
            Auth::SignedOut
        );
        assert_eq!(parse_claude_auth("Not JSON").0, Auth::Unknown);
    }
}
