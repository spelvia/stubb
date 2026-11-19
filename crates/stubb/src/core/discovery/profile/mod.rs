//! Who an agent is, for hosts to show: its name, maker, description, links
//! and logo, the same way for Stubb's own harnesses and for ACP registry
//! agents.
//!
//! Stubb ships the logos of its own harnesses (Claude Code shows Claude's
//! mark, Codex OpenAI's, Pi its own). Registry agents name a logo by URL;
//! Stubb does not fetch it, so a host fetches it and passes it through
//! [`clean_svg`] before serving it.
//!
//! ```
//! use stubb::probe::Harness;
//! let claude = stubb::profile::of(Harness::Claude);
//! assert_eq!(claude.name, "Claude Code");
//! assert!(matches!(claude.logo.source, stubb::profile::LogoSource::Svg(_)));
//! ```

use serde::{Deserialize, Serialize};

use crate::probe::Harness;
use crate::registry::Entry;

const CLAUDE: &str = include_str!("claude.svg");
const OPENAI: &str = include_str!("openai.svg");
const PI: &str = include_str!("pi.svg");

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

/// The profile of one of Stubb's own harnesses.
pub fn of(harness: Harness) -> Profile {
    match harness {
        Harness::Claude => Profile {
            id: harness.id().to_owned(),
            name: "Claude Code".to_owned(),
            maker: Some("Anthropic".to_owned()),
            description: "Anthropic's agentic coding tool, in the terminal.".to_owned(),
            homepage: Some("https://claude.com/product/claude-code".to_owned()),
            repository: Some("https://github.com/anthropics/claude-code".to_owned()),
            logo: Logo {
                source: LogoSource::Svg(CLAUDE.to_owned()),
                monochrome: false,
            },
        },
        Harness::Codex => Profile {
            id: harness.id().to_owned(),
            name: "Codex".to_owned(),
            maker: Some("OpenAI".to_owned()),
            description: "OpenAI's coding agent, in the terminal.".to_owned(),
            homepage: Some("https://openai.com/codex".to_owned()),
            repository: Some("https://github.com/openai/codex".to_owned()),
            logo: Logo {
                source: LogoSource::Svg(OPENAI.to_owned()),
                monochrome: true,
            },
        },
        Harness::Pi => Profile {
            id: harness.id().to_owned(),
            name: "Pi".to_owned(),
            maker: Some("Earendil".to_owned()),
            description: "A minimal, extensible coding agent.".to_owned(),
            homepage: None,
            repository: Some("https://github.com/earendil-works/pi".to_owned()),
            logo: Logo {
                source: LogoSource::Svg(PI.to_owned()),
                monochrome: false,
            },
        },
    }
}

impl Entry {
    /// The registry agent's profile. The registry's logos are drawn in
    /// `currentColor`, so they are monochrome.
    pub fn profile(&self) -> Profile {
        Profile {
            id: self.id.clone(),
            name: self.name.clone(),
            maker: (!self.authors.is_empty()).then(|| self.authors.join(", ")),
            description: self.description.clone(),
            homepage: self.website.clone(),
            repository: self.repository.clone(),
            logo: Logo {
                source: match &self.icon {
                    Some(url) => LogoSource::Url(url.clone()),
                    None => LogoSource::Svg(String::new()),
                },
                monochrome: true,
            },
        }
    }
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
    fn own_harnesses_ship_their_logos() {
        for &harness in Harness::ALL {
            let profile = of(harness);
            let LogoSource::Svg(svg) = &profile.logo.source else {
                panic!("{harness:?} has no logo");
            };
            assert!(clean_svg(svg).is_some(), "{harness:?}'s logo is not clean");
        }
        assert!(of(Harness::Codex).logo.monochrome);
        assert!(!of(Harness::Claude).logo.monochrome);
    }

    #[test]
    fn registry_agents_name_their_logo() {
        let gemini = crate::registry::Registry::bundled()
            .get("gemini")
            .expect("gemini is in the bundled registry")
            .profile();
        assert_eq!(gemini.name, "Gemini CLI");
        assert!(gemini.logo.monochrome);
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
