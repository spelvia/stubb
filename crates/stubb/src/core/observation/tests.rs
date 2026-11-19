use super::records::*;
use crate::session::Driver;
use crate::{Error, Result, provider::BoxFuture, turn};
use crate::{observation::agent, observation::task, session};
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
        pending_controls: vec![],
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
    #[cfg(feature = "pi")]
    pi_defaults_are_unsupported_without_a_launch().await;
}

#[cfg(feature = "pi")]
async fn pi_defaults_are_unsupported_without_a_launch() {
    let provider = crate::providers::pi::Pi::new(crate::providers::pi::PiConfig {
        executable: "/nonexistent/stubb-contract-test".into(),
        ..Default::default()
    });
    let provider: &dyn crate::Provider = &provider;
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
