//! Session-wide observation contracts, separate from the legacy root event stream.
//!
//! These types describe a negotiated harness surface, not Keel orchestration.
//! Adapters must explicitly implement observation; empty results cannot stand in
//! for unsupported discovery. Check effective capabilities: adapters can cover
//! only part of the surface, such as live Codex activity without stored descendant discovery.

use serde::{Deserialize, Serialize};

use crate::{agent, session, task};

/// One observation in a live attachment. Sequences begin at one and increase
/// under the same boundary that updates snapshot state. A new attachment gets a
/// new stream ID; sequences are not durable replay cursors.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Record {
    pub stream_id: String,
    pub seq: u64,
    /// Keel observation time in Unix milliseconds, not a native event timestamp.
    pub at: i64,
    /// Activity owner, including the root. Turn/item/usage activity uses `Some`;
    /// aggregate status, root attachment closure and agent/task replacements
    /// use `None`. Unresolved raw traffic may also use `None` while retaining
    /// native IDs in its payload. Agent/task updates identify the affected
    /// resource in payload, rather than naming the emitter here.
    pub agent_id: Option<String>,
    pub event: Event,
}

/// Full state replacements and attributed legacy activity. Announce each agent
/// before its activity, using an unknown-parent placeholder if necessary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Event {
    Agent {
        agent: agent::Summary,
    },
    Task {
        task: task::Summary,
    },
    /// Turn/item IDs belong to the record's agent. Root turn completion does not
    /// end observation of descendants or background commands. `Closed` denotes
    /// root attachment closure; child closure is an agent summary update.
    Activity {
        event: session::Event,
        /// Present for usage activity; missing information is `Unknown`, never
        /// an implicit promise that totals exclude descendants.
        usage_scope: Option<UsageScope>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum UsageScope {
    AgentOnly,
    IncludesDescendants,
    Unknown,
}

/// Atomic state at `seq` (zero before any records). Subscribe first, buffer,
/// fetch this snapshot, then apply records from the same stream with greater
/// sequences. Repeat after receiver lag or attachment changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub stream_id: String,
    pub seq: u64,
    pub status: session::Status,
    pub root_agent_id: String,
    pub agents: Vec<agent::Summary>,
    pub tasks: Vec<task::Summary>,
    /// Materialized turns/items still receiving activity, including commands
    /// that continue after their owning turn has ended.
    pub active_turns: Vec<agent::OwnedTurn>,
    pub capabilities: Capabilities,
}

/// Support and observed coverage are distinct. Complete coverage is relative to
/// the negotiated native surface and does not promise access to invisible work.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Feature {
    pub support: Support,
    pub coverage: Coverage,
    pub reason: Option<String>,
    pub gaps: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Support {
    Supported,
    Unsupported,
    #[default]
    Unknown,
    Denied,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Coverage {
    Complete,
    Partial,
    #[default]
    Unknown,
}

/// Effective support depends on adapter implementation, executable version,
/// configuration and extensions, rather than the selected model alone.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    pub records: Feature,
    pub snapshots: Feature,
    pub agent_discovery: Feature,
    pub parent_links: Feature,
    pub nested_discovery: Feature,
    pub agent_lifecycle: Feature,
    pub agent_transcript: Feature,
    pub agent_history: Feature,
    pub task_discovery: Feature,
    pub task_lifecycle: Feature,
    pub task_output: Feature,
    pub task_history: Feature,
    pub stop_agent: Feature,
    /// Only native subtree controls may implement this; Keel never simulates it.
    pub stop_subtree: Feature,
    pub stop_task: Feature,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Driver;
    use crate::{Error, Provider, Result, provider::BoxFuture, turn};
    use serde_json::json;
    use tokio::sync::broadcast;

    #[test]
    fn recursive_records_preserve_ancestry_and_activity_ownership() {
        let node = json!({
            "streamId": "live-1", "seq": 21, "at": 1791204000000i64,
            "agentId": null,
            "event": {"type": "agent", "agent": {
                "id": "grandchild", "parent": {"type": "agent", "id": "child"},
                "name": null, "role": null, "model": "gpt-6-luna",
                "description": null, "background": null, "status": "running",
                "origin": {"agentId": "child", "turnId": "u1", "itemId": "spawn"},
                "createdAt": null, "updatedAt": null
            }}
        });
        let record: Record = serde_json::from_value(node.clone()).unwrap();
        assert_eq!(serde_json::to_value(record).unwrap(), node);

        let activity = Record {
            stream_id: "live-1".into(),
            seq: 22,
            at: 1791204000001,
            agent_id: Some("grandchild".into()),
            event: Event::Activity {
                event: session::Event::ItemDelta {
                    turn_id: "u2".into(),
                    item_id: "i2".into(),
                    delta: turn::Delta::Text {
                        text: "nested-ok".into(),
                    },
                },
                usage_scope: None,
            },
        };
        assert_eq!(
            serde_json::to_value(activity).unwrap(),
            json!({
                "streamId": "live-1", "seq": 22, "at": 1791204000001i64,
                "agentId": "grandchild", "event": {"type": "activity",
                    "event": {"type": "itemDelta", "turnId": "u2", "itemId": "i2",
                        "delta": {"type": "text", "text": "nested-ok"}},
                    "usageScope": null}
            })
        );
    }

    #[test]
    fn task_state_does_not_imply_ownership_output_or_stop_confirmation() {
        let value = json!({"id": "task-1", "ownerAgentId": null, "parentId": null,
            "origin": null, "kind": {"type": "agentRun", "agentId": "child"},
            "description": null, "background": true, "ambient": null,
            "status": "stopping", "output": "unknown"});
        let task: task::Summary = serde_json::from_value(value.clone()).unwrap();
        assert_eq!(task.status, task::Status::Stopping);
        assert_eq!(task.owner_agent_id, None);
        assert_eq!(task.output, task::OutputAvailability::Unknown);
        assert_eq!(serde_json::to_value(task).unwrap(), value);
        let feature = Feature::default();
        assert_eq!(
            serde_json::to_value(feature).unwrap(),
            json!({
                "support": "unknown", "coverage": "unknown", "reason": null, "gaps": []
            })
        );
    }

    #[test]
    fn snapshot_retains_owned_materialized_items_and_watermark() {
        let owned = agent::OwnedTurn {
            agent_id: "child".into(),
            turn: turn::Turn {
                id: "u1".into(),
                status: turn::Status::InProgress,
                items: vec![turn::Item::AgentMessage {
                    id: "i1".into(),
                    text: "partial".into(),
                }],
                started_at: None,
                completed_at: None,
                duration_ms: None,
                error: None,
            },
        };
        let snapshot = Snapshot {
            stream_id: "live-1".into(),
            seq: 22,
            status: session::Status::Active,
            root_agent_id: "root".into(),
            agents: vec![],
            tasks: vec![],
            active_turns: vec![owned],
            capabilities: Capabilities::default(),
        };
        let value = serde_json::to_value(&snapshot).unwrap();
        assert_eq!(value["activeTurns"][0]["agentId"], "child");
        assert_eq!(
            value["activeTurns"][0]["turn"]["items"][0]["text"],
            "partial"
        );
        assert_eq!(value["seq"], 22);
        assert_eq!(serde_json::from_value::<Snapshot>(value).unwrap(), snapshot);
    }

    // An existing custom driver implements only the original required methods.
    struct LegacyDriver;
    impl Driver for LegacyDriver {
        fn subscribe(&self) -> broadcast::Receiver<session::Event> {
            broadcast::channel(1).1
        }
        fn send(&self, _: turn::Input) -> BoxFuture<'_, Result<turn::Receipt>> {
            unreachable!()
        }
        fn interrupt<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<()>> {
            unreachable!()
        }
        fn close(&self) -> BoxFuture<'_, Result<()>> {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn unimplemented_observation_returns_typed_errors_without_execution() {
        let driver: &dyn Driver = &LegacyDriver;
        assert_eq!(
            driver.subscribe_records().unwrap_err().code(),
            "unsupported"
        );
        assert_eq!(
            driver.observation_capabilities().unwrap_err().code(),
            "unsupported"
        );
        assert!(matches!(
            driver.snapshot().await,
            Err(Error::Unsupported {
                operation: "snapshot"
            })
        ));

        // No executable exists; these defaults must not attempt a CLI launch.
        let provider = crate::codex::Codex::new(crate::codex::CodexConfig {
            executable: "/nonexistent/keel-contract-test".into(),
            ..Default::default()
        });
        let provider: &dyn Provider = &provider;
        assert!(matches!(
            provider.list_agents("root", agent::Query::default()).await,
            Err(Error::Unsupported {
                operation: "list_agents"
            })
        ));
        assert!(matches!(
            provider
                .list_agent_turns("root", "child", turn::Query::default())
                .await,
            Err(Error::Unsupported {
                operation: "list_agent_turns"
            })
        ));
        let query = agent::Query {
            parent_id: Some("root".into()),
            ancestor_id: Some("root".into()),
            ..Default::default()
        };
        assert_eq!(
            provider
                .list_agents("root", query)
                .await
                .unwrap_err()
                .code(),
            "invalid_setting"
        );
        assert_eq!(
            Error::AgentNotFound {
                session_id: "root".into(),
                id: "child".into()
            }
            .code(),
            "agent_not_found"
        );
        assert_eq!(
            Error::TaskNotFound {
                session_id: "root".into(),
                id: "task".into()
            }
            .code(),
            "task_not_found"
        );
    }
}
