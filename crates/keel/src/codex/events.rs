//! Maps Codex notifications and server requests onto session events.

use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use super::{history, types};
use crate::session::{CloseReason, Event};
use crate::turn;

/// Converts one notification. Anything Keel does not model, or fails to
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
        "turn/started" => decode::<types::TurnNotification>(&params)
            .map(|n| Event::TurnStart { turn_id: n.turn.id }),
        "turn/completed" => decode::<types::TurnNotification>(&params).map(|n| Event::TurnEnd {
            turn: history::turn(n.turn),
        }),
        "item/started" => decode::<types::ItemNotification>(&params).map(|n| Event::ItemStart {
            turn_id: n.turn_id,
            item: history::item(n.item),
        }),
        "item/completed" => decode::<types::ItemNotification>(&params).map(|n| Event::ItemEnd {
            turn_id: n.turn_id,
            item: history::item(n.item),
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
                delta: turn::Delta::Reasoning { text: n.delta },
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

/// Answers a request from Codex that Keel cannot handle yet. Approvals are
/// declined so the turn continues without the action; anything else gets a
/// "method not supported" error. Returns the reply and the event to report.
pub(super) fn decline_request(id: &Value, method: &str, params: &Value) -> (Value, Event) {
    let reply = match method {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            json!({ "id": id, "result": { "decision": "decline" } })
        }
        _ => json!({
            "id": id,
            "error": { "code": -32601, "message": format!("{method} is not supported by Keel yet") },
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
            json!({ "threadId": "t", "turn": { "id": "u", "items": [], "itemsView": "notLoaded", "status": "completed" } }),
        );
        assert!(matches!(
            completed,
            Event::TurnEnd { turn } if turn.id == "u" && turn.status == turn::Status::Completed
        ));
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
}
