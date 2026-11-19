//! Codex-native approval and sandbox settings.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Error, Result};

/// Optional start/resume overrides; absent fields retain native configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Permissions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<ApprovalPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<Sandbox>,
    /// Native named permission profile, mutually exclusive with `sandbox`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

/// Codex's native approval policy, including granular prompt categories.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum ApprovalPolicy {
    Untrusted,
    OnRequest,
    Never,
    Granular(GranularApproval),
}

/// Native keys intentionally use snake_case inside the granular policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GranularApproval {
    pub sandbox_approval: bool,
    pub rules: bool,
    pub mcp_elicitations: bool,
    #[serde(default)]
    pub request_permissions: bool,
    #[serde(default)]
    pub skill_approval: bool,
}

/// Native sandbox selection. This does not change the approval policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "camelCase", deny_unknown_fields)]
#[non_exhaustive]
pub enum Sandbox {
    ReadOnly,
    WorkspaceWrite {
        /// Additional absolute writable directories, beyond the working directory.
        #[serde(
            default,
            rename = "writableRoots",
            skip_serializing_if = "Option::is_none"
        )]
        writable_roots: Option<Vec<PathBuf>>,
        #[serde(
            default,
            rename = "networkAccess",
            skip_serializing_if = "Option::is_none"
        )]
        network_access: Option<bool>,
        #[serde(
            default,
            rename = "excludeTmpdirEnvVar",
            skip_serializing_if = "Option::is_none"
        )]
        exclude_tmpdir_env_var: Option<bool>,
        #[serde(
            default,
            rename = "excludeSlashTmp",
            skip_serializing_if = "Option::is_none"
        )]
        exclude_slash_tmp: Option<bool>,
    },
    DangerFullAccess,
}

/// Confirmed native sandbox policy. A selected mode alone is not this report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
#[non_exhaustive]
pub enum SandboxPolicy {
    DangerFullAccess,
    ReadOnly {
        #[serde(default, rename = "networkAccess")]
        network_access: bool,
    },
    WorkspaceWrite {
        #[serde(default, rename = "writableRoots")]
        writable_roots: Vec<PathBuf>,
        #[serde(default, rename = "networkAccess")]
        network_access: bool,
        #[serde(default, rename = "excludeTmpdirEnvVar")]
        exclude_tmpdir_env_var: bool,
        #[serde(default, rename = "excludeSlashTmp")]
        exclude_slash_tmp: bool,
    },
    ExternalSandbox {
        #[serde(default = "restricted", rename = "networkAccess")]
        network_access: String,
    },
}

fn restricted() -> String {
    "restricted".into()
}

/// Typed native fields, with the full native report retained for newer schemas.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectivePermissions {
    /// `None` means the provider omitted the field or returned an unknown shape.
    pub approval_policy: Option<ApprovalPolicy>,
    pub sandbox: Option<SandboxPolicy>,
    /// Includes native policy and experimental profile provenance when reported.
    pub native: Value,
}

impl EffectivePermissions {
    pub(crate) fn from_native(approval: &Value, sandbox: &Value, profile: &Value) -> Self {
        Self {
            approval_policy: serde_json::from_value(approval.clone()).ok(),
            sandbox: serde_json::from_value(sandbox.clone()).ok(),
            native: json!({"approvalPolicy": approval, "sandbox": sandbox, "activePermissionProfile": profile}),
        }
    }
}

impl Permissions {
    pub(crate) fn validate(&self) -> Result<()> {
        let invalid = |reason: &str| Error::InvalidSetting {
            setting: "permissions",
            value: format!("{self:?}"),
            reason: reason.into(),
        };
        if self.profile.is_some() && self.sandbox.is_some() {
            return Err(invalid(
                "Codex permission profile and sandbox cannot be combined",
            ));
        }
        if self.profile.as_ref().is_some_and(|s| s.trim().is_empty()) {
            return Err(invalid("permission profile must not be empty"));
        }
        if let Some(Sandbox::WorkspaceWrite {
            writable_roots: Some(roots),
            ..
        }) = &self.sandbox
        {
            for path in roots {
                if !path.is_absolute() || path.to_str().is_none() {
                    return Err(invalid("writable roots must be absolute UTF-8 paths"));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn mode(&self) -> Option<&'static str> {
        self.sandbox.as_ref().map(|sandbox| match sandbox {
            Sandbox::ReadOnly => "read-only",
            Sandbox::WorkspaceWrite { .. } => "workspace-write",
            Sandbox::DangerFullAccess => "danger-full-access",
        })
    }

    /// The sandbox as `turn/start` takes it (a policy, not a mode name).
    pub(crate) fn turn_sandbox(&self) -> Option<Value> {
        self.sandbox.as_ref().map(|sandbox| match sandbox {
            Sandbox::ReadOnly => json!({ "type": "readOnly" }),
            Sandbox::DangerFullAccess => json!({ "type": "dangerFullAccess" }),
            Sandbox::WorkspaceWrite {
                writable_roots,
                network_access,
                exclude_tmpdir_env_var,
                exclude_slash_tmp,
            } => json!({
                "type": "workspaceWrite",
                "writableRoots": writable_roots.clone().unwrap_or_default(),
                "networkAccess": network_access.unwrap_or(false),
                "excludeTmpdirEnvVar": exclude_tmpdir_env_var.unwrap_or(false),
                "excludeSlashTmp": exclude_slash_tmp.unwrap_or(false),
            }),
        })
    }

    pub(crate) fn apply_config(&self, config: &mut HashMap<String, Value>) {
        if let Some(Sandbox::WorkspaceWrite {
            writable_roots,
            network_access,
            exclude_tmpdir_env_var,
            exclude_slash_tmp,
        }) = &self.sandbox
        {
            if let Some(roots) = writable_roots {
                config.insert(
                    "sandbox_workspace_write.writable_roots".into(),
                    json!(roots),
                );
            }
            for (key, value) in [
                ("network_access", network_access),
                ("exclude_tmpdir_env_var", exclude_tmpdir_env_var),
                ("exclude_slash_tmp", exclude_slash_tmp),
            ] {
                if let Some(value) = value {
                    config.insert(format!("sandbox_workspace_write.{key}"), json!(value));
                }
            }
        }
    }
}

impl crate::permission::Native for Permissions {
    const PROVIDER: &'static str = super::PROVIDER_ID;
}

impl crate::permission::Native for EffectivePermissions {
    const PROVIDER: &'static str = super::PROVIDER_ID;
}

impl From<Permissions> for crate::permission::Configuration {
    fn from(permissions: Permissions) -> Self {
        Self::new(&permissions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turns_take_the_sandbox_as_a_policy() {
        let permissions = Permissions {
            sandbox: Some(Sandbox::WorkspaceWrite {
                writable_roots: Some(vec!["/work".into()]),
                network_access: None,
                exclude_tmpdir_env_var: None,
                exclude_slash_tmp: None,
            }),
            ..Permissions::default()
        };
        assert_eq!(
            permissions.turn_sandbox().unwrap(),
            json!({"type": "workspaceWrite", "writableRoots": ["/work"], "networkAccess": false,
                   "excludeTmpdirEnvVar": false, "excludeSlashTmp": false})
        );
        let full = Permissions {
            sandbox: Some(Sandbox::DangerFullAccess),
            ..Permissions::default()
        };
        assert_eq!(
            full.turn_sandbox().unwrap(),
            json!({"type": "dangerFullAccess"})
        );
    }

    #[test]
    fn granular_policy_preserves_each_native_prompt_category() {
        let policy = ApprovalPolicy::Granular(GranularApproval {
            sandbox_approval: true,
            rules: false,
            mcp_elicitations: false,
            request_permissions: true,
            skill_approval: false,
        });
        let native = json!({"granular": {"sandbox_approval": true, "rules": false,
            "mcp_elicitations": false, "request_permissions": true, "skill_approval": false}});
        assert_eq!(serde_json::to_value(&policy).unwrap(), native);
        assert_eq!(
            serde_json::from_value::<ApprovalPolicy>(native).unwrap(),
            policy
        );
    }

    #[test]
    fn rejects_ambiguous_profile_and_relative_roots() {
        let mut settings = Permissions {
            profile: Some("restricted".into()),
            sandbox: Some(Sandbox::ReadOnly),
            ..Default::default()
        };
        assert!(matches!(
            settings.validate(),
            Err(Error::InvalidSetting { .. })
        ));
        settings.profile = None;
        settings.sandbox = Some(Sandbox::WorkspaceWrite {
            writable_roots: Some(vec!["relative".into()]),
            network_access: None,
            exclude_tmpdir_env_var: None,
            exclude_slash_tmp: None,
        });
        assert!(settings.validate().is_err());
        settings.sandbox = None;
        settings.profile = Some(" ".into());
        assert!(settings.validate().is_err());
    }

    #[test]
    fn unknown_native_policy_remains_unknown_and_retains_native_data() {
        let sandbox = json!({"type":"futurePolicy", "restriction":"native"});
        let info =
            EffectivePermissions::from_native(&Value::Null, &sandbox, &json!({"id":"managed"}));
        assert!(info.approval_policy.is_none());
        assert!(info.sandbox.is_none());
        assert_eq!(info.native["sandbox"], sandbox);
        assert_eq!(info.native["activePermissionProfile"]["id"], "managed");
    }

    #[test]
    fn effective_policy_preserves_roots_network_and_temporary_directory_restrictions() {
        let native = json!({"type":"workspaceWrite", "writableRoots":["/work", "/extra"], "networkAccess":false, "excludeSlashTmp":true, "excludeTmpdirEnvVar":true});
        let info = EffectivePermissions::from_native(&json!("never"), &native, &Value::Null);
        assert_eq!(info.approval_policy, Some(ApprovalPolicy::Never));
        assert_eq!(serde_json::to_value(info.sandbox.unwrap()).unwrap(), native);
    }
}
