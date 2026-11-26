//! Maps Codex notifications and server requests onto session events.

use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::{history, types};
use crate::session::{CloseReason, Event};
use crate::turn;

/// Converts one notification. Anything Stubb does not model, or fails to
/// decode, is passed through as [`Event::Raw`].
pub(super) fn from_notification(method: &str, params: Value) -> Event {
    let event = match method {
        "thread/status/changed" => {
            decode::<types::StatusChangedNotification>(&params).map(|n| Event::Status {
                status: n.status.into(),
            })
        }
        "thread/closed" => Some(Event::Closed {
            reason: CloseReason::Agent,
        }),
        // A thread renamed (`thread/name/set`, from any client; Codex does
        // not name threads itself): its title. Clearing the name sends no
        // title; the host keeps the one it has.
        "thread/name/updated" => params["threadName"]
            .as_str()
            .map(str::trim)
            .filter(|title| !title.is_empty())
            .map(|title| Event::Title {
                title: title.to_owned(),
            }),
        "turn/started" => decode::<types::TurnNotification>(&params)
            .map(|n| Event::TurnStart { turn_id: n.turn.id }),
        // Items were already delivered by the item events.
        "turn/completed" => decode::<types::TurnNotification>(&params).map(|n| Event::TurnEnd {
            turn: turn::Turn {
                items: Vec::new(),
                ..history::turn(n.turn)
            },
        }),
        "item/started" => decode::<types::ItemNotification>(&params).map(|n| {
            compaction(&n, turn::CompactionStatus::InProgress).unwrap_or(Event::ItemStart {
                turn_id: n.turn_id,
                item: history::item(n.item),
            })
        }),
        "item/completed" => decode::<types::ItemNotification>(&params).map(|n| {
            compaction(&n, turn::CompactionStatus::Completed).unwrap_or(Event::ItemEnd {
                turn_id: n.turn_id,
                item: history::item(n.item),
            })
        }),
        "item/agentMessage/delta" => {
            decode::<types::DeltaNotification>(&params).map(|n| Event::ItemDelta {
                turn_id: n.turn_id,
                item_id: n.item_id,
                delta: turn::Delta::Text { text: n.delta },
            })
        }
        "item/reasoning/summaryTextDelta" => {
            decode::<types::DeltaNotification>(&params).map(|n| Event::ItemDelta {
                turn_id: n.turn_id,
                item_id: n.item_id,
                delta: turn::Delta::Thinking { text: n.delta },
            })
        }
        // Live text matches the item's final text, whose parts are joined
        // by a blank line.
        "item/reasoning/summaryPartAdded" => decode::<types::SummaryPartNotification>(&params)
            .and_then(|n| {
                (n.summary_index > 0).then(|| Event::ItemDelta {
                    turn_id: n.turn_id,
                    item_id: n.item_id,
                    delta: turn::Delta::Thinking {
                        text: "\n\n".to_owned(),
                    },
                })
            }),
        "item/reasoning/textDelta" => {
            decode::<types::DeltaNotification>(&params).map(|n| Event::ItemDelta {
                turn_id: n.turn_id,
                item_id: n.item_id,
                delta: turn::Delta::Thinking { text: n.delta },
            })
        }
        "item/commandExecution/outputDelta" => {
            decode::<types::DeltaNotification>(&params).map(|n| Event::ItemDelta {
                turn_id: n.turn_id,
                item_id: n.item_id,
                delta: turn::Delta::Output { text: n.delta },
            })
        }
        "thread/tokenUsage/updated" => {
            decode::<types::TokenUsageNotification>(&params).map(|n| Event::Usage {
                turn_id: n.turn_id,
                usage: turn::Usage {
                    last: token_count(&n.token_usage.last),
                    total: token_count(&n.token_usage.total),
                    context_window: n.token_usage.model_context_window,
                },
            })
        }
        "error" => decode::<types::ErrorNotification>(&params).map(|n| Event::Error {
            turn_id: n.turn_id,
            message: n.error.message,
            retrying: n.will_retry,
        }),
        _ => None,
    };
    event.unwrap_or_else(|| Event::Raw {
        name: method.to_owned(),
        payload: params,
    })
}

/// Answers a request from Codex that Stubb cannot handle yet. Approvals are
/// declined so the turn continues without the action; anything else gets a
/// "method not supported" error. Returns the reply and the event to report.
pub(super) fn decline_request(id: &Value, method: &str, params: &Value) -> (Value, Event) {
    let reply = match method {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            json!({ "id": id, "result": { "decision": "decline" } })
        }
        _ => json!({
            "id": id,
            "error": { "code": -32601, "message": format!("{method} is not supported by Stubb yet") },
        }),
    };
    let text = |key: &str| params.get(key).and_then(Value::as_str).map(str::to_owned);
    let event = Event::RequestDeclined {
        method: method.to_owned(),
        turn_id: text("turnId"),
        summary: text("command").or_else(|| text("reason")),
    };
    (reply, event)
}

fn decode<T: DeserializeOwned>(params: &Value) -> Option<T> {
    serde_json::from_value(params.clone()).ok()
}

/// Codex reports a compaction as a `contextCompaction` item that starts and
/// completes. Its deprecated `thread/compacted` notification repeats the end
/// and stays [`Event::Raw`].
fn compaction(n: &types::ItemNotification, status: turn::CompactionStatus) -> Option<Event> {
    if n.item.get("type").and_then(Value::as_str) != Some("contextCompaction") {
        return None;
    }
    let mut compaction = turn::Compaction::new(status);
    compaction.id = n.item.get("id").and_then(Value::as_str).map(str::to_owned);
    Some(Event::Compaction {
        turn_id: Some(n.turn_id.clone()),
        compaction,
    })
}

fn token_count(usage: &types::TokenUsageBreakdown) -> turn::TokenCount {
    turn::TokenCount {
        input: usage.input_tokens,
        cached_input: usage.cached_input_tokens,
        output: usage.output_tokens,
        reasoning_output: usage.reasoning_output_tokens,
        total: usage.total_tokens,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_streaming_notifications() {
        let delta = from_notification(
            "item/agentMessage/delta",
            json!({ "threadId": "t", "turnId": "u", "itemId": "i", "delta": "hel" }),
        );
        assert_eq!(
            delta,
            Event::ItemDelta {
                turn_id: "u".into(),
                item_id: "i".into(),
                delta: turn::Delta::Text { text: "hel".into() }
            }
        );

        let completed = from_notification(
            "turn/completed",
            json!({ "threadId": "t", "turn": { "id": "u", "itemsView": "full", "status": "completed",
                "items": [{ "type": "agentMessage", "id": "i", "text": "hello" }] } }),
        );
        // The items were delivered by item events, so TurnEnd does not repeat them.
        assert!(matches!(
            completed,
            Event::TurnEnd { turn } if turn.id == "u" && turn.status == turn::Status::Completed && turn.items.is_empty()
        ));
    }

    #[test]
    fn reasoning_deltas_build_the_final_text() {
        let params = |extra: Value| {
            let mut params = json!({ "threadId": "t", "turnId": "u", "itemId": "r" });
            params
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            params
        };
        let notifications = [
            (
                "item/reasoning/summaryPartAdded",
                params(json!({ "summaryIndex": 0 })),
            ),
            (
                "item/reasoning/summaryTextDelta",
                params(json!({ "summaryIndex": 0, "delta": "**Plan**" })),
            ),
            (
                "item/reasoning/summaryPartAdded",
                params(json!({ "summaryIndex": 1 })),
            ),
            (
                "item/reasoning/summaryTextDelta",
                params(json!({ "summaryIndex": 1, "delta": "Then test" })),
            ),
        ];
        let mut text = String::new();
        for (method, params) in notifications {
            if let Event::ItemDelta {
                turn_id,
                item_id,
                delta: turn::Delta::Thinking { text: piece },
            } = from_notification(method, params)
            {
                assert_eq!((turn_id.as_str(), item_id.as_str()), ("u", "r"));
                text.push_str(&piece);
            }
        }
        let Event::ItemEnd {
            item: turn::Item::Thinking { text: last, .. },
            ..
        } = from_notification(
            "item/completed",
            json!({ "threadId": "t", "turnId": "u",
                "item": { "type": "reasoning", "id": "r", "summary": ["**Plan**", "Then test"], "content": [] } }),
        )
        else {
            panic!("expected a thinking item");
        };
        assert_eq!(text, last);

        // Models that report raw reasoning stream it as thinking, too.
        assert_eq!(
            from_notification(
                "item/reasoning/textDelta",
                params(json!({ "contentIndex": 0, "delta": "step" }))
            ),
            Event::ItemDelta {
                turn_id: "u".into(),
                item_id: "r".into(),
                delta: turn::Delta::Thinking {
                    text: "step".into()
                }
            }
        );
    }

    #[test]
    fn unknown_notifications_pass_through() {
        let event = from_notification("turn/diff/updated", json!({ "threadId": "t" }));
        assert!(matches!(event, Event::Raw { name, .. } if name == "turn/diff/updated"));
    }

    #[test]
    fn declines_command_approvals() {
        let (reply, event) = decline_request(
            &json!(7),
            "item/commandExecution/requestApproval",
            &json!({ "threadId": "t", "turnId": "u", "command": "rm -rf /" }),
        );
        assert_eq!(
            reply,
            json!({ "id": 7, "result": { "decision": "decline" } })
        );
        assert!(
            matches!(event, Event::RequestDeclined { summary: Some(command), .. } if command == "rm -rf /")
        );
    }

    /// Shapes from Codex 0.170's app-server schema.
    #[test]
    fn compaction_items_become_compaction_events() {
        let started = from_notification(
            "item/started",
            json!({ "threadId": "th", "turnId": "t1", "item": { "type": "contextCompaction", "id": "c1" } }),
        );
        assert!(
            matches!(started, Event::Compaction { turn_id: Some(turn), compaction }
            if turn == "t1" && compaction.id.as_deref() == Some("c1")
                && compaction.status == turn::CompactionStatus::InProgress)
        );
        let completed = from_notification(
            "item/completed",
            json!({ "threadId": "th", "turnId": "t1", "item": { "type": "contextCompaction", "id": "c1" } }),
        );
        assert!(matches!(completed, Event::Compaction { compaction, .. }
            if compaction.status == turn::CompactionStatus::Completed));
    }

    #[test]
    fn a_renamed_thread_gives_the_session_its_title() {
        let renamed = from_notification(
            "thread/name/updated",
            json!({ "threadId": "th", "threadName": "  Fix the login bug " }),
        );
        assert_eq!(
            renamed,
            Event::Title {
                title: "Fix the login bug".to_owned()
            }
        );
        let cleared = from_notification(
            "thread/name/updated",
            json!({ "threadId": "th", "threadName": null }),
        );
        assert!(matches!(cleared, Event::Raw { .. }));
    }
}
