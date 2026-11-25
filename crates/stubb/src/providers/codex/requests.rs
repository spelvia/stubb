//! Codex's server requests that a session forwards to the host: approvals,
//! `request_user_input` questions and MCP elicitations.
//!
//! Each becomes a [`request::Request`]; the host's decision becomes the
//! JSON-RPC result Codex expects for its method.

use serde_json::{Map, Value, json};

use crate::Result;
use crate::request::{self, Decision};

const COMMAND_APPROVAL: &str = "item/commandExecution/requestApproval";
const FILE_CHANGE_APPROVAL: &str = "item/fileChange/requestApproval";
const PERMISSIONS_APPROVAL: &str = "item/permissions/requestApproval";
const USER_INPUT: &str = "item/tool/requestUserInput";
const ELICITATION: &str = "mcpServer/elicitation/request";

/// A forwarded request waiting for the host.
pub(super) struct Pending {
    /// The JSON-RPC id to answer.
    pub native_id: Value,
    pub method: String,
    pub thread_id: String,
    pub turn_id: Option<String>,
    pub params: Value,
    pub kind: request::Kind,
}

/// The id hosts answer a native request by.
pub(super) fn key(native_id: &Value) -> String {
    native_id
        .as_str()
        .map_or_else(|| native_id.to_string(), str::to_owned)
}

/// The request to forward for a server request, or `None` for a method Stubb
/// does not forward (it is then declined as before).
pub(super) fn forward(
    native_id: &Value,
    method: &str,
    params: &Value,
) -> Option<(request::Request, Pending)> {
    let text = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    let thread_id = text(params, "threadId")?;
    let turn_id = text(params, "turnId");
    let item_id = text(params, "itemId");
    let kind = match method {
        COMMAND_APPROVAL => {
            let human = params.get("kind").and_then(Value::as_str) == Some("humanCommand");
            request::Kind::Tool {
                name: "commandExecution".to_owned(),
                display_name: None,
                description: text(params, "reason"),
                input: pick(params, &["command", "cwd", "commandActions", "kind"]),
                tool_call_id: item_id,
                // Codex's prompt for a managed checkpoint takes a fresh answer
                // each time.
                can_allow_for_session: !human,
            }
        }
        FILE_CHANGE_APPROVAL => request::Kind::Tool {
            name: "fileChange".to_owned(),
            display_name: None,
            description: text(params, "reason"),
            input: pick(params, &["grantRoot"]),
            tool_call_id: item_id,
            can_allow_for_session: true,
        },
        PERMISSIONS_APPROVAL => request::Kind::Tool {
            name: "permissions".to_owned(),
            display_name: None,
            description: text(params, "reason"),
            input: pick(params, &["permissions", "cwd"]),
            tool_call_id: item_id,
            can_allow_for_session: true,
        },
        USER_INPUT => {
            let questions = params
                .get("questions")?
                .as_array()?
                .iter()
                .map(|question| {
                    Some(request::Question {
                        id: text(question, "id")?,
                        header: text(question, "header").filter(|header| !header.is_empty()),
                        question: text(question, "question")?,
                        options: question
                            .get("options")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten()
                            .filter_map(|option| {
                                Some(request::QuestionOption {
                                    label: text(option, "label")?,
                                    description: text(option, "description"),
                                    preview: None,
                                })
                            })
                            .collect(),
                        multi_select: false,
                        allow_other: question.get("isOther").and_then(Value::as_bool) == Some(true),
                        secret: question.get("isSecret").and_then(Value::as_bool) == Some(true),
                    })
                })
                .collect::<Option<Vec<_>>>()?;
            request::Kind::Question(request::Questions {
                questions,
                tool_call_id: item_id,
            })
        }
        ELICITATION => {
            let message = text(params, "message").unwrap_or_default();
            let mut form = match params.get("mode").and_then(Value::as_str) {
                Some("url") => {
                    let mut form = request::Form::from_schema(message, Value::Null);
                    form.url = Some(text(params, "url")?);
                    form
                }
                _ => request::Form::from_schema(
                    message,
                    params.get("requestedSchema").cloned().unwrap_or_default(),
                ),
            };
            form.source = text(params, "serverName");
            request::Kind::Form(form)
        }
        _ => return None,
    };
    let id = key(native_id);
    Some((
        request::Request {
            id,
            turn_id: turn_id.clone(),
            kind: kind.clone(),
            native: params.clone(),
        },
        Pending {
            native_id: native_id.clone(),
            method: method.to_owned(),
            thread_id,
            turn_id,
            params: params.clone(),
            kind,
        },
    ))
}

/// The JSON-RPC result for the host's decision.
pub(super) fn result(id: &str, pending: &Pending, decision: &Decision) -> Result<Value> {
    let unanswerable = || {
        request::invalid(
            id,
            format!("{} does not answer {}", decision.name(), pending.method),
        )
    };
    match pending.method.as_str() {
        COMMAND_APPROVAL | FILE_CHANGE_APPROVAL => {
            let decision = match decision {
                Decision::Allow => "accept",
                Decision::AllowForSession => {
                    if let request::Kind::Tool {
                        can_allow_for_session: false,
                        ..
                    } = pending.kind
                    {
                        return Err(request::invalid(id, "this approval is for this call only"));
                    }
                    "acceptForSession"
                }
                Decision::Deny { .. } => "decline",
                Decision::Cancel => "cancel",
                _ => return Err(unanswerable()),
            };
            Ok(json!({ "decision": decision }))
        }
        PERMISSIONS_APPROVAL => {
            let (permissions, scope) = match decision {
                Decision::Allow => (granted(&pending.params), "turn"),
                Decision::AllowForSession => (granted(&pending.params), "session"),
                Decision::Deny { .. } | Decision::Cancel => (json!({}), "turn"),
                _ => return Err(unanswerable()),
            };
            Ok(json!({ "permissions": permissions, "scope": scope }))
        }
        USER_INPUT => {
            let request::Kind::Question(questions) = &pending.kind else {
                return Err(unanswerable());
            };
            let mut map = Map::new();
            match decision {
                Decision::Answer { answers } => {
                    for answer in answers {
                        let Some(question) = questions
                            .questions
                            .iter()
                            .find(|question| question.id == answer.question_id)
                        else {
                            return Err(request::invalid(
                                id,
                                format!("no question {:?}", answer.question_id),
                            ));
                        };
                        if let Some(label) = answer.selected.iter().find(|label| {
                            !question
                                .options
                                .iter()
                                .any(|option| &option.label == *label)
                        }) {
                            return Err(request::invalid(
                                id,
                                format!("{label:?} is not an option of {:?}", question.id),
                            ));
                        }
                        map.insert(question.id.clone(), json!({ "answers": answer.values() }));
                    }
                }
                // Codex has no way to decline: the questions go unanswered.
                Decision::Allow | Decision::Deny { .. } | Decision::Cancel => {}
                _ => return Err(unanswerable()),
            }
            Ok(json!({ "answers": map }))
        }
        ELICITATION => {
            let url = matches!(&pending.kind, request::Kind::Form(form) if form.url.is_some());
            let (action, content) = match decision {
                Decision::Submit { values } if !url => ("accept", Value::Object(values.clone())),
                Decision::Submit { .. } | Decision::Allow => {
                    ("accept", if url { Value::Null } else { json!({}) })
                }
                Decision::Deny { .. } => ("decline", Value::Null),
                Decision::Cancel => ("cancel", Value::Null),
                _ => return Err(unanswerable()),
            };
            Ok(json!({ "action": action, "content": content, "_meta": null }))
        }
        _ => Err(unanswerable()),
    }
}

/// Whether the decision stops the turn as well: Codex's own `cancel`
/// already does so for approvals, questions and forms need an interrupt.
pub(super) fn interrupts(pending: &Pending, decision: &Decision) -> bool {
    matches!(decision, Decision::Cancel)
        && matches!(pending.method.as_str(), USER_INPUT | ELICITATION)
}

/// The requested permissions, without the parts not asked for.
fn granted(params: &Value) -> Value {
    let mut granted = Map::new();
    if let Some(requested) = params.get("permissions").and_then(Value::as_object) {
        for (key, value) in requested {
            if !value.is_null() {
                granted.insert(key.clone(), value.clone());
            }
        }
    }
    Value::Object(granted)
}

/// The fields of `params` named in `keys` that are present.
fn pick(params: &Value, keys: &[&str]) -> Value {
    Value::Object(
        keys.iter()
            .filter_map(|key| {
                let value = params.get(*key).filter(|value| !value.is_null())?;
                Some(((*key).to_owned(), value.clone()))
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forwarded(method: &str, params: Value) -> (request::Request, Pending) {
        forward(&json!(7), method, &params).expect("a forwarded request")
    }

    #[test]
    fn user_input_becomes_questions_and_answers_go_by_id() {
        let (request, pending) = forwarded(
            USER_INPUT,
            json!({
                "threadId": "t", "turnId": "u", "itemId": "i", "isBlocking": true,
                "questions": [{
                    "id": "lib", "header": "Library", "question": "Which one?",
                    "isOther": true, "isSecret": false,
                    "options": [{ "label": "chrono", "description": "Dates" }]
                }, {
                    "id": "token", "header": "", "question": "Token?", "isOther": false,
                    "isSecret": true, "options": null
                }]
            }),
        );
        assert_eq!(request.id, "7");
        assert_eq!(request.turn_id.as_deref(), Some("u"));
        let request::Kind::Question(questions) = &request.kind else {
            panic!("expected questions, got {:?}", request.kind);
        };
        assert_eq!(questions.tool_call_id.as_deref(), Some("i"));
        assert!(questions.questions[0].allow_other);
        assert_eq!(questions.questions[0].header.as_deref(), Some("Library"));
        assert_eq!(questions.questions[1].header, None);
        assert!(questions.questions[1].secret);
        assert!(questions.questions[1].options.is_empty());

        let answer = Decision::Answer {
            answers: vec![
                request::Answer::selected("lib", ["chrono"]).with_other("or time"),
                request::Answer::other("token", "s3cret"),
            ],
        };
        assert_eq!(
            result("7", &pending, &answer).unwrap(),
            json!({ "answers": {
                "lib": { "answers": ["chrono", "or time"] },
                "token": { "answers": ["s3cret"] }
            } })
        );
        assert_eq!(
            result("7", &pending, &Decision::Cancel).unwrap(),
            json!({ "answers": {} })
        );
        assert!(interrupts(&pending, &Decision::Cancel));
        let unknown = Decision::Answer {
            answers: vec![request::Answer::selected("lib", ["moment"])],
        };
        assert!(matches!(
            result("7", &pending, &unknown),
            Err(crate::Error::InvalidDecision { .. })
        ));
        assert!(result("7", &pending, &Decision::AllowForSession).is_err());
    }

    #[test]
    fn approvals_map_to_codex_decisions() {
        let (request, pending) = forwarded(
            COMMAND_APPROVAL,
            json!({ "threadId": "t", "turnId": "u", "itemId": "c", "kind": "command",
                "command": "cargo test", "cwd": "/w", "reason": null }),
        );
        assert!(matches!(
            &request.kind,
            request::Kind::Tool { name, tool_call_id: Some(id), can_allow_for_session: true, .. }
                if name == "commandExecution" && id == "c"
        ));
        let decide = |decision| result("7", &pending, &decision).unwrap()["decision"].clone();
        assert_eq!(decide(Decision::Allow), "accept");
        assert_eq!(decide(Decision::AllowForSession), "acceptForSession");
        assert_eq!(decide(Decision::Deny { message: None }), "decline");
        assert_eq!(decide(Decision::Cancel), "cancel");
        assert!(!interrupts(&pending, &Decision::Cancel));

        let (_, human) = forwarded(
            COMMAND_APPROVAL,
            json!({ "threadId": "t", "turnId": "u", "itemId": "c", "kind": "humanCommand", "command": "rm" }),
        );
        assert!(result("7", &human, &Decision::AllowForSession).is_err());

        let (_, permissions) = forwarded(
            PERMISSIONS_APPROVAL,
            json!({ "threadId": "t", "turnId": "u", "itemId": "p", "cwd": "/w", "reason": "net",
                "permissions": { "network": { "enabled": true }, "fileSystem": null } }),
        );
        assert_eq!(
            result("7", &permissions, &Decision::AllowForSession).unwrap(),
            json!({ "permissions": { "network": { "enabled": true } }, "scope": "session" })
        );
        assert_eq!(
            result("7", &permissions, &Decision::Deny { message: None }).unwrap(),
            json!({ "permissions": {}, "scope": "turn" })
        );
    }

    #[test]
    fn elicitations_become_forms() {
        let (request, pending) = forwarded(
            ELICITATION,
            json!({ "threadId": "t", "turnId": null, "serverName": "github", "mode": "form",
                "_meta": null, "message": "Which repo?",
                "requestedSchema": { "type": "object", "required": ["repo"],
                    "properties": { "repo": { "type": "string" } } } }),
        );
        let request::Kind::Form(form) = &request.kind else {
            panic!("expected a form, got {:?}", request.kind);
        };
        assert_eq!(form.source.as_deref(), Some("github"));
        assert_eq!(form.fields[0].name, "repo");
        assert!(form.fields[0].required);
        let mut values = Map::new();
        values.insert("repo".into(), json!("spelvia/stubb"));
        assert_eq!(
            result("7", &pending, &Decision::Submit { values }).unwrap(),
            json!({ "action": "accept", "content": { "repo": "spelvia/stubb" }, "_meta": null })
        );
        assert_eq!(
            result("7", &pending, &Decision::Deny { message: None }).unwrap()["action"],
            "decline"
        );

        let (request, pending) = forwarded(
            ELICITATION,
            json!({ "threadId": "t", "turnId": "u", "serverName": "auth", "mode": "url",
                "_meta": null, "message": "Sign in", "url": "https://example.com", "elicitationId": "e" }),
        );
        assert!(
            matches!(&request.kind, request::Kind::Form(form) if form.url.as_deref() == Some("https://example.com"))
        );
        assert_eq!(
            result("7", &pending, &Decision::Allow).unwrap(),
            json!({ "action": "accept", "content": null, "_meta": null })
        );
    }

    #[test]
    fn other_methods_are_not_forwarded() {
        assert!(forward(&json!(1), "item/tool/call", &json!({ "threadId": "t" })).is_none());
        assert!(
            forward(
                &json!(1),
                "execCommandApproval",
                &json!({ "threadId": "t" })
            )
            .is_none()
        );
    }
}
