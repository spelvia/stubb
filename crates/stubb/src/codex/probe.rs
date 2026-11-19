//! Native Codex authentication probe.

use std::path::Path;

use crate::core::discovery::status::run;
use crate::probe::Auth;

pub(crate) async fn auth(path: &Path) -> (Auth, Option<String>) {
    match run(path, &["login", "status"]).await {
        Ok(out) => parse_codex_auth(out.success, &format!("{}{}", out.stdout, out.stderr)),
        Err(error) => (Auth::Unknown, Some(error)),
    }
}

fn parse_codex_auth(success: bool, output: &str) -> (Auth, Option<String>) {
    let lower = output.to_lowercase();
    if lower.contains("not logged in") {
        (Auth::SignedOut, None)
    } else if success && lower.contains("logged in") {
        (Auth::SignedIn { account: None }, None)
    } else if !success {
        (Auth::SignedOut, None)
    } else {
        (
            Auth::Unknown,
            Some(format!(
                "unexpected answer from `codex login status`: {}",
                output.trim()
            )),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_status_is_read_from_its_sentence() {
        assert_eq!(
            parse_codex_auth(true, "Logged in using ChatGPT\n").0,
            Auth::SignedIn { account: None }
        );
        assert_eq!(
            parse_codex_auth(false, "Not logged in\n").0,
            Auth::SignedOut
        );
        assert_eq!(parse_codex_auth(true, "something else").0, Auth::Unknown);
    }
}
