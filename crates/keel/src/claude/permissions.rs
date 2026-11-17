//! Claude Code's permission modes.

use serde::{Deserialize, Serialize};

/// How much Claude Code may do without asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub enum PermissionMode {
    /// Asks before edits and commands (forwarded as requests when the
    /// session forwards them).
    Default,
    /// Edits files without asking; still asks before commands.
    AcceptEdits,
    /// Reads and plans; makes no changes.
    Plan,
    /// Never asks.
    BypassPermissions,
}

impl PermissionMode {
    /// The name Claude Code uses, on its command line and in control requests.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AcceptEdits => "acceptEdits",
            Self::Plan => "plan",
            Self::BypassPermissions => "bypassPermissions",
        }
    }
}

/// Claude Code's start, resume and live permission settings; an absent mode
/// keeps its native default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Permissions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<PermissionMode>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_use_claude_code_names() {
        for mode in [
            PermissionMode::Default,
            PermissionMode::AcceptEdits,
            PermissionMode::Plan,
            PermissionMode::BypassPermissions,
        ] {
            assert_eq!(serde_json::to_value(mode).unwrap(), mode.as_str());
        }
    }
}
