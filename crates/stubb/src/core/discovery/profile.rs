//! Who an agent is, for hosts to show: its name, maker, description, links
//! and logo, the same way for built-in agents and for ACP registry agents.
//!
//! Built-in agents ship their logos (Claude Code shows Claude's mark, Codex
//! OpenAI's, Pi its own) through [`Cli::profile`](crate::discovery::Cli::profile).
//! Registry agents name a logo by URL; Stubb does not fetch it, so a host
//! fetches it and passes it through [`clean_svg`] before serving it.
//!
//! ```
//! # #[cfg(feature = "claude")] {
//! let claude = stubb::providers::claude::CLI.profile();
//! assert_eq!(claude.name, "Claude Code");
//! assert!(matches!(claude.logo.source, stubb::discovery::profile::LogoSource::Svg(_)));
//! # }
//! ```

use serde::{Deserialize, Serialize};

/// An agent as a host presents it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Profile {
    /// Stubb's provider identifier for its own harnesses (`claude`), the
    /// registry's for registry agents (`gemini`).
    pub id: String,
    pub name: String,
    /// Who makes it, such as "Anthropic".
    pub maker: Option<String>,
    pub description: String,
    pub homepage: Option<String>,
    pub repository: Option<String>,
    pub logo: Logo,
}

/// An agent's logo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct Logo {
    pub source: LogoSource,
    /// Drawn in one colour (`currentColor`): hosts show it in the text's
    /// colour so it reads on light and dark grounds. Others keep their own
    /// colours.
    pub monochrome: bool,
}

/// Where a logo is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "camelCase")]
#[non_exhaustive]
pub enum LogoSource {
    /// The SVG itself, shipped with Stubb.
    Svg(String),
    /// A URL the host fetches (then cleans with [`clean_svg`]).
    Url(String),
}

/// The largest logo Stubb accepts, in bytes.
pub const MAX_LOGO: usize = 64 * 1024;

/// An SVG fetched from elsewhere, if it is safe to serve as a logo: small,
/// an `<svg>` element, and without scripts, event handlers, embedded
/// documents or references outside itself. Rejected rather than repaired.
pub fn clean_svg(svg: &str) -> Option<String> {
    if svg.len() > MAX_LOGO {
        return None;
    }
    let lower = svg.to_ascii_lowercase();
    let start = lower.find("<svg")?;
    // Before the `<svg>` only an XML declaration and comments may come.
    let mut before = lower[..start].trim();
    while !before.is_empty() {
        let end = if before.starts_with("<?xml") {
            before.find("?>").map(|at| at + 2)
        } else if before.starts_with("<!--") {
            before.find("-->").map(|at| at + 3)
        } else {
            None
        }?;
        before = before[end..].trim_start();
    }
    const FORBIDDEN: [&str; 9] = [
        "<script",
        "<foreignobject",
        "<iframe",
        "<embed",
        "<object",
        "javascript:",
        "<!entity",
        "<!doctype",
        "@import",
    ];
    if FORBIDDEN.iter().any(|word| lower.contains(word)) {
        return None;
    }
    // Event handlers (onload=, onclick=…), anywhere in a tag.
    let handler = lower
        .split(|c: char| c.is_whitespace() || c == '<' || c == '/')
        .filter_map(|word| word.strip_prefix("on"))
        .filter_map(|rest| rest.split_once('='))
        .any(|(name, _)| !name.is_empty() && name.chars().all(|c| c.is_ascii_alphabetic()));
    if handler {
        return None;
    }
    // References may only point inside the document (`#id`).
    for attribute in ["href=", "src="] {
        let mut rest = lower.as_str();
        while let Some(at) = rest.find(attribute) {
            let value = rest[at + attribute.len()..].trim_start_matches(['"', '\'']);
            if !value.starts_with('#') {
                return None;
            }
            rest = &rest[at + attribute.len()..];
        }
    }
    if lower
        .split("url(")
        .skip(1)
        .any(|value| !value.trim_start_matches(['"', '\'']).starts_with('#'))
    {
        return None;
    }
    Some(svg.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_agents_ship_clean_logos() {
        for cli in crate::discovery::clis() {
            let profile = cli.profile();
            assert_eq!(profile.id, cli.id());
            let LogoSource::Svg(svg) = &profile.logo.source else {
                panic!("{cli:?} has no logo");
            };
            assert!(clean_svg(svg).is_some(), "{cli:?}'s logo is not clean");
        }
    }

    #[test]
    fn unsafe_svgs_are_refused() {
        let ok = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><defs><linearGradient id="g"/></defs><path fill="url(#g)" d="M0 0h24v24H0z"/><use href="#g"/></svg>"##;
        assert!(clean_svg(ok).is_some());
        assert!(clean_svg(&format!("<?xml version=\"1.0\"?>\n{ok}")).is_some());
        for bad in [
            r#"<svg><script>alert(1)</script></svg>"#,
            r#"<svg onload="alert(1)"></svg>"#,
            r#"<svg><a href="javascript:alert(1)"><path/></a></svg>"#,
            r#"<svg><image href="https://example.com/x.png"/></svg>"#,
            r#"<svg><path style="fill:url(https://example.com/x)"/></svg>"#,
            r#"<svg><foreignObject><div/></foreignObject></svg>"#,
            r#"<!DOCTYPE svg [<!ENTITY x "y">]><svg></svg>"#,
            r#"<html><svg></svg></html>"#,
            "not an svg",
        ] {
            assert!(clean_svg(bad).is_none(), "{bad}");
        }
        assert!(clean_svg(&format!("<svg>{}</svg>", "a".repeat(MAX_LOGO))).is_none());
    }
}
