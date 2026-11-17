//! The ACP provider against a fake agent, built with the SDK's agent side
//! and connected through an in-memory stream, so Keel's real connection,
//! JSON encoding and mapping run.
//!
//! The fake agent answers prompts by their text:
//!
//! - `hello`: two message chunks, a command tool call, then `end_turn`.
//! - `permission`: an edit that asks for permission; the edit completes when
//!   allowed and fails when refused.
//! - `wait`: a chunk, then waits for `session/cancel` and stops `cancelled`.
//! - `read <path>`: reads the file through the client and repeats it.
//! - `write <path>`: writes `written` to the file through the client.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_client_protocol::v1::schema as wire;
use agent_client_protocol::{self as sdk, Agent};
use serde_json::{Value, json};
use tokio::sync::{Notify, broadcast};

use super::connection::{Connector, lines};
use super::{AcpConfig, AcpProvider, Launch};
use crate::request::{self, Decision};
use crate::session::{self, Event};
use crate::turn::{self, Item, ToolDetail, ToolStatus};
use crate::{Error, Provider};

const SESSION: &str = "fake-session";

/// What the fake agent saw, for assertions.
#[derive(Default)]
struct Seen {
    prompts: Vec<Value>,
    config: Vec<(String, String)>,
    modes: Vec<String>,
    cancels: usize,
    closes: usize,
    resumes: Vec<(String, String)>,
}

#[derive(Default)]
struct Fake {
    seen: Mutex<Seen>,
    cancelled: Notify,
}

fn options(model: &str, effort: &str) -> Vec<wire::SessionConfigOption> {
    serde_json::from_value(json!([
        {
            "id": "model", "name": "Model", "category": "model", "type": "select",
            "currentValue": model,
            "options": [
                { "value": "fast", "name": "Fast", "description": "Quick" },
                { "value": "smart", "name": "Smart" }
            ]
        },
        {
            "id": "thinking", "name": "Thinking", "category": "thought_level", "type": "select",
            "currentValue": effort,
            "options": [ { "value": "low", "name": "Low" }, { "value": "high", "name": "High" } ]
        }
    ]))
    .unwrap()
}

fn modes() -> wire::SessionModeState {
    serde_json::from_value(json!({
        "currentModeId": "ask",
        "availableModes": [
            { "id": "ask", "name": "Ask", "description": "Asks before changes" },
            { "id": "code", "name": "Code" }
        ]
    }))
    .unwrap()
}

fn chunk(kind: &str, text: &str) -> wire::SessionUpdate {
    serde_json::from_value(json!({
        "sessionUpdate": kind,
        "content": { "type": "text", "text": text }
    }))
    .unwrap()
}

fn update(value: Value) -> wire::SessionUpdate {
    serde_json::from_value(value).unwrap()
}

type AgentCx = sdk::v1::ConnectionTo<sdk::Client>;

fn notify(cx: &AgentCx, update: wire::SessionUpdate) {
    cx.send_notification(wire::SessionNotification::new(SESSION, update))
        .unwrap();
}

/// Runs one scenario and returns the stop reason.
async fn scenario(fake: Arc<Fake>, cx: AgentCx, text: String) -> wire::StopReason {
    let words: Vec<&str> = text.split_whitespace().collect();
    match words.as_slice() {
        ["hello"] => {
            notify(&cx, chunk("agent_thought_chunk", "Thinking"));
            notify(&cx, chunk("agent_message_chunk", "Hel"));
            notify(&cx, chunk("agent_message_chunk", "lo"));
            notify(
                &cx,
                update(json!({
                    "sessionUpdate": "tool_call", "toolCallId": "call-1", "title": "List files",
                    "kind": "execute", "status": "in_progress", "rawInput": { "command": "ls" }
                })),
            );
            notify(
                &cx,
                update(json!({
                    "sessionUpdate": "tool_call_update", "toolCallId": "call-1", "status": "completed",
                    "content": [ { "type": "content", "content": { "type": "text", "text": "a.txt" } } ]
                })),
            );
            notify(
                &cx,
                update(json!({ "sessionUpdate": "usage_update", "used": 42, "size": 1000 })),
            );
            notify(
                &cx,
                update(json!({ "sessionUpdate": "current_mode_update", "currentModeId": "code" })),
            );
            wire::StopReason::EndTurn
        }
        ["permission"] => {
            let tool = json!({
                "sessionUpdate": "tool_call", "toolCallId": "edit-1", "title": "Edit a.txt",
                "kind": "edit", "status": "pending", "rawInput": { "path": "a.txt" }
            });
            notify(&cx, update(tool));
            let request: wire::RequestPermissionRequest = serde_json::from_value(json!({
                "sessionId": SESSION,
                "toolCall": { "toolCallId": "edit-1", "title": "Edit a.txt", "kind": "edit", "rawInput": { "path": "a.txt" } },
                "options": [
                    { "optionId": "once", "name": "Allow once", "kind": "allow_once" },
                    { "optionId": "always", "name": "Always allow", "kind": "allow_always" },
                    { "optionId": "no", "name": "Reject", "kind": "reject_once" }
                ]
            }))
            .unwrap();
            let response = cx.send_request(request).block_task().await;
            let chosen = match response.map(|response| response.outcome) {
                Ok(wire::RequestPermissionOutcome::Selected(selected)) => {
                    selected.option_id.0.to_string()
                }
                Ok(_) => "cancelled".to_owned(),
                Err(error) => format!("error: {}", error.message),
            };
            let allowed = chosen == "once" || chosen == "always";
            notify(
                &cx,
                update(json!({
                    "sessionUpdate": "tool_call_update", "toolCallId": "edit-1",
                    "status": if allowed { "completed" } else { "failed" },
                    "content": [ { "type": "diff", "path": "/w/a.txt", "oldText": "a\n", "newText": "b\n" } ]
                })),
            );
            notify(&cx, chunk("agent_message_chunk", &chosen));
            if chosen == "cancelled" {
                wire::StopReason::Cancelled
            } else {
                wire::StopReason::EndTurn
            }
        }
        ["wait"] => {
            let cancelled = fake.cancelled.notified();
            notify(&cx, chunk("agent_message_chunk", "Working"));
            cancelled.await;
            wire::StopReason::Cancelled
        }
        ["read", path] => {
            let request = wire::ReadTextFileRequest::new(SESSION, *path);
            let text = match cx.send_request(request).block_task().await {
                Ok(response) => response.content,
                Err(error) => format!("error {}", error.code.value()),
            };
            notify(&cx, chunk("agent_message_chunk", &text));
            wire::StopReason::EndTurn
        }
        ["write", path] => {
            let request = wire::WriteTextFileRequest::new(SESSION, *path, "written");
            let text = match cx.send_request(request).block_task().await {
                Ok(_) => "ok".to_owned(),
                Err(error) => format!("error {}", error.code.value()),
            };
            notify(&cx, chunk("agent_message_chunk", &text));
            wire::StopReason::EndTurn
        }
        _ => wire::StopReason::Refusal,
    }
}

/// Serves one connection as the fake agent.
async fn serve(fake: Arc<Fake>, stream: tokio::io::DuplexStream) {
    let (read, write) = tokio::io::split(stream);
    let state = Arc::new(Mutex::new(options("fast", "low")));
    let (f1, f2, f3, f4, f5, f6, f7) = (
        Arc::clone(&fake),
        Arc::clone(&fake),
        Arc::clone(&fake),
        Arc::clone(&fake),
        Arc::clone(&fake),
        Arc::clone(&fake),
        Arc::clone(&fake),
    );
    let (s1, s2, s3) = (Arc::clone(&state), Arc::clone(&state), Arc::clone(&state));
    let _ = Agent
        .v1()
        .name("fake-agent")
        .on_receive_request(
            async |request: wire::InitializeRequest, responder, _cx| {
                assert!(request.client_capabilities.fs.read_text_file);
                assert!(!request.client_capabilities.terminal);
                responder.respond(
                    serde_json::from_value(json!({
                        "protocolVersion": 1,
                        "agentInfo": { "name": "fake-agent", "title": "Fake", "version": "1.2.3" },
                        "authMethods": [ { "id": "login", "name": "Log in", "description": "Opens a browser" } ],
                        "agentCapabilities": {
                            "loadSession": true,
                            "promptCapabilities": { "image": true },
                            "sessionCapabilities": { "resume": {}, "close": {}, "list": {} }
                        }
                    }))
                    .unwrap(),
                )
            },
            sdk::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: wire::NewSessionRequest, responder, _cx| {
                assert!(request.mcp_servers.is_empty());
                let options = s1.lock().unwrap().clone();
                responder.respond(
                    wire::NewSessionResponse::new(SESSION)
                        .modes(modes())
                        .config_options(options),
                )
            },
            sdk::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: wire::ResumeSessionRequest, responder, _cx| {
                if &*request.session_id.0 != SESSION {
                    return responder.respond_with_error(sdk::Error::new(
                        sdk::ErrorCode::RESOURCE_NOT_FOUND,
                        "no such session",
                    ));
                }
                f1.seen.lock().unwrap().resumes.push((
                    request.session_id.0.to_string(),
                    request.cwd.display().to_string(),
                ));
                let options = s2.lock().unwrap().clone();
                responder.respond(
                    wire::ResumeSessionResponse::new()
                        .modes(modes())
                        .config_options(options),
                )
            },
            sdk::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: wire::ListSessionsRequest, responder, _cx| {
                responder.respond(wire::ListSessionsResponse::new(vec![
                    wire::SessionInfo::new(SESSION, "/work/project")
                        .title("Fake work".to_owned())
                        .updated_at("2026-11-16T12:00:00Z".to_owned()),
                ]))
            },
            sdk::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: wire::SetSessionConfigOptionRequest, responder, _cx| {
                let value = match &request.value {
                    wire::SessionConfigOptionValue::ValueId { value } => value.0.to_string(),
                    other => format!("{other:?}"),
                };
                f2.seen
                    .lock()
                    .unwrap()
                    .config
                    .push((request.config_id.0.to_string(), value.clone()));
                let mut options = s3.lock().unwrap();
                let (model, effort) = {
                    let current = |id: &str| {
                        options
                            .iter()
                            .find(|o| &*o.id.0 == id)
                            .and_then(|o| match &o.kind {
                                wire::SessionConfigKind::Select(s) => {
                                    Some(s.current_value.0.to_string())
                                }
                                _ => None,
                            })
                            .unwrap()
                    };
                    (current("model"), current("thinking"))
                };
                *options = match &*request.config_id.0 {
                    "model" => super::tests::options(&value, &effort),
                    _ => super::tests::options(&model, &value),
                };
                responder.respond(wire::SetSessionConfigOptionResponse::new(options.clone()))
            },
            sdk::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: wire::SetSessionModeRequest, responder, _cx| {
                f3.seen
                    .lock()
                    .unwrap()
                    .modes
                    .push(request.mode_id.0.to_string());
                responder.respond(wire::SetSessionModeResponse::new())
            },
            sdk::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: wire::CloseSessionRequest, responder, _cx| {
                f4.seen.lock().unwrap().closes += 1;
                responder.respond(wire::CloseSessionResponse::new())
            },
            sdk::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: wire::PromptRequest, responder, cx: AgentCx| {
                let prompt = serde_json::to_value(&request.prompt).unwrap();
                f5.seen.lock().unwrap().prompts.push(prompt);
                let text = request
                    .prompt
                    .iter()
                    .find_map(|block| match block {
                        wire::ContentBlock::Text(text) => Some(text.text.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                let fake = Arc::clone(&f6);
                tokio::spawn(async move {
                    let stop = scenario(fake, cx, text).await;
                    let _ = responder.respond(wire::PromptResponse::new(stop));
                });
                Ok(())
            },
            sdk::on_receive_request!(),
        )
        .on_receive_notification(
            async move |_cancel: wire::CancelNotification, _cx| {
                f7.seen.lock().unwrap().cancels += 1;
                f7.cancelled.notify_waiters();
                Ok(())
            },
            sdk::on_receive_notification!(),
        )
        .connect_to(lines(read, write))
        .await;
}

fn provider(fake: &Arc<Fake>) -> AcpProvider {
    let fake = Arc::clone(fake);
    let connector = Connector::Memory(Arc::new(move || {
        let (client, agent) = tokio::io::duplex(64 * 1024);
        tokio::spawn(serve(Arc::clone(&fake), agent));
        client
    }));
    let mut config = AcpConfig::new("fake-acp", "Fake", Launch::new("fake-agent"));
    config.startup_timeout = Duration::from_secs(10);
    config.request_timeout = Duration::from_secs(10);
    AcpProvider::with_connector(config, connector)
}

fn spec(settings: session::Settings) -> session::Spec {
    session::Spec {
        cwd: std::env::temp_dir(),
        settings,
    }
}

async fn next(events: &mut broadcast::Receiver<Event>) -> Event {
    tokio::time::timeout(Duration::from_secs(10), events.recv())
        .await
        .expect("an event in time")
        .expect("the stream is open")
}

/// Events up to and including the turn's end.
async fn turn_events(events: &mut broadcast::Receiver<Event>) -> Vec<Event> {
    let mut seen = Vec::new();
    loop {
        let event = next(events).await;
        let done = matches!(event, Event::TurnEnd { .. });
        seen.push(event);
        if done {
            // The session goes idle right after.
            seen.push(next(events).await);
            return seen;
        }
    }
}

fn ended(events: &[Event]) -> Vec<&Item> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::ItemEnd { item, .. } => Some(item),
            _ => None,
        })
        .collect()
}

fn turn_end(events: &[Event]) -> &turn::Turn {
    events
        .iter()
        .find_map(|event| match event {
            Event::TurnEnd { turn } => Some(turn),
            _ => None,
        })
        .expect("a turn end")
}

#[tokio::test]
async fn the_provider_reports_a_dynamic_id() {
    let fake = Arc::new(Fake::default());
    let provider = provider(&fake);
    assert_eq!(Provider::id(&provider), "fake-acp");
    assert_eq!(provider.label(), "Fake");
    assert!(std::ptr::eq(
        super::intern("fake-acp"),
        super::intern("fake-acp")
    ));
}

#[tokio::test]
async fn initialize_reports_the_agent() {
    let fake = Arc::new(Fake::default());
    let provider = provider(&fake);
    let connection = provider.connect(&std::env::temp_dir()).await.unwrap();
    let report = super::probe::report_from(&connection.init);
    connection.shutdown().await;
    assert!(report.initialized);
    assert_eq!(report.protocol_version, Some(1));
    let agent = report.agent.unwrap();
    assert_eq!(
        (agent.name.as_str(), agent.version.as_str()),
        ("fake-agent", "1.2.3")
    );
    assert_eq!(report.auth_methods.len(), 1);
    assert_eq!(report.auth_methods[0].id, "login");
    let capabilities = report.capabilities.unwrap();
    assert!(capabilities.load_session && capabilities.resume_session && capabilities.image_prompts);
}

#[tokio::test]
async fn the_catalog_comes_from_the_session_config_options() {
    let fake = Arc::new(Fake::default());
    let catalog = provider(&fake).catalog().await.unwrap();
    assert_eq!(catalog.provider.id, "fake-acp");
    assert_eq!(catalog.provider.version.as_deref(), Some("1.2.3"));
    let ids: Vec<&str> = catalog.models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["fast", "smart"]);
    assert!(catalog.models[0].is_default);
    assert_eq!(catalog.models[0].efforts.len(), 2);
    assert_eq!(
        fake.seen.lock().unwrap().closes,
        1,
        "the probe session is closed"
    );
}

#[tokio::test]
async fn a_new_session_applies_model_and_effort() {
    let fake = Arc::new(Fake::default());
    let provider = provider(&fake);
    let session = provider
        .start_session(spec(session::Settings {
            model: Some("smart".into()),
            effort: Some("high".into()),
            ..Default::default()
        }))
        .await
        .unwrap();
    assert_eq!(session.id(), SESSION);
    assert_eq!(session.info().model, "smart");
    assert_eq!(session.info().effort.as_deref(), Some("high"));
    assert_eq!(session.info().summary.provider, "fake-acp");
    assert_eq!(
        fake.seen.lock().unwrap().config,
        [
            ("model".into(), "smart".into()),
            ("thinking".into(), "high".into())
        ]
    );
    let invalid = provider
        .start_session(spec(session::Settings {
            model: Some("missing".into()),
            ..Default::default()
        }))
        .await;
    assert!(matches!(
        invalid,
        Err(Error::InvalidSetting {
            setting: "model",
            ..
        })
    ));
    session.close().await.unwrap();
}

#[tokio::test]
async fn a_prompt_turn_streams_messages_and_tool_calls() {
    let fake = Arc::new(Fake::default());
    let session = provider(&fake)
        .start_session(spec(session::Settings::default()))
        .await
        .unwrap();
    let mut events = session.subscribe();
    let receipt = session.send(turn::Input::text("hello")).await.unwrap();
    let events = turn_events(&mut events).await;
    assert_eq!(
        events[0],
        Event::Status {
            status: session::Status::Active
        }
    );
    assert_eq!(
        events[1],
        Event::TurnStart {
            turn_id: receipt.turn_id.clone()
        }
    );
    assert!(events.iter().any(|event| matches!(
        event,
        Event::ItemDelta { delta: turn::Delta::Text { text }, .. } if text == "lo"
    )));
    let items = ended(&events);
    assert!(matches!(items[0], Item::UserMessage { id, .. } if *id == receipt.turn_id));
    assert!(matches!(items[1], Item::Thinking { text, .. } if text == "Thinking"));
    assert!(matches!(items[2], Item::AgentMessage { text, .. } if text == "Hello"));
    let Item::ToolCall(call) = items[3] else {
        panic!("expected a tool call: {:?}", items[3]);
    };
    assert_eq!(call.id, "call-1");
    assert_eq!(call.status, ToolStatus::Completed);
    assert!(matches!(
        &call.detail,
        ToolDetail::Command { command, output: Some(output), .. } if command == "ls" && output == "a.txt"
    ));
    assert!(events.iter().any(|event| matches!(
        event,
        Event::Usage { usage, .. } if usage.context_window == Some(1000)
    )));
    let end = turn_end(&events);
    assert_eq!(end.id, receipt.turn_id);
    assert_eq!(end.status, turn::Status::Completed);
    assert_eq!(
        events.last(),
        Some(&Event::Status {
            status: session::Status::Idle
        })
    );
    // The agent switched modes during the turn.
    assert_eq!(session.modes().unwrap().current, "code");
    let prompt = fake.seen.lock().unwrap().prompts[0].clone();
    assert_eq!(prompt, json!([{ "type": "text", "text": "hello" }]));
    session.close().await.unwrap();
}

#[tokio::test]
async fn images_are_sent_before_the_text() {
    let fake = Arc::new(Fake::default());
    let session = provider(&fake)
        .start_session(spec(session::Settings::default()))
        .await
        .unwrap();
    let path = std::env::temp_dir().join(format!("keel-acp-image-{}.png", std::process::id()));
    std::fs::write(&path, b"hi").unwrap();
    let mut events = session.subscribe();
    session
        .send(turn::Input::text("hello").with_images(vec![turn::Image::new(&path, "image/png")]))
        .await
        .unwrap();
    turn_events(&mut events).await;
    std::fs::remove_file(&path).unwrap();
    let prompt = fake.seen.lock().unwrap().prompts[0].clone();
    assert_eq!(prompt[0]["type"], "image");
    assert_eq!(prompt[0]["data"], "aGk=");
    assert_eq!(prompt[0]["mimeType"], "image/png");
    assert_eq!(prompt[1]["text"], "hello");
    session.close().await.unwrap();
}

#[tokio::test]
async fn forwarded_permission_requests_round_trip() {
    let fake = Arc::new(Fake::default());
    let session = provider(&fake)
        .start_session(spec(session::Settings {
            forward_requests: true,
            ..Default::default()
        }))
        .await
        .unwrap();
    for (decision, expected, status) in [
        (Decision::AllowForSession, "always", ToolStatus::Completed),
        (Decision::Deny { message: None }, "no", ToolStatus::Declined),
    ] {
        let mut events = session.subscribe();
        let receipt = session.send(turn::Input::text("permission")).await.unwrap();
        let request = loop {
            if let Event::Request { request } = next(&mut events).await {
                break request;
            }
        };
        assert_eq!(request.turn_id.as_deref(), Some(receipt.turn_id.as_str()));
        let request::Kind::Tool {
            name,
            display_name,
            tool_call_id,
            can_allow_for_session,
            input,
            ..
        } = &request.kind
        else {
            panic!("expected a tool request, got {:?}", request.kind);
        };
        assert_eq!(name, "edit");
        assert_eq!(display_name.as_deref(), Some("Edit a.txt"));
        assert_eq!(tool_call_id.as_deref(), Some("edit-1"));
        assert!(*can_allow_for_session);
        assert_eq!(input, &json!({ "path": "a.txt" }));
        session
            .respond(&request.id, decision.clone())
            .await
            .unwrap();
        let events = turn_events(&mut events).await;
        assert!(events.contains(&Event::RequestResolved {
            id: request.id.clone(),
            outcome: request::Outcome::Answered { decision },
        }));
        let items = ended(&events);
        assert!(items.iter().any(|item| matches!(
            item,
            Item::ToolCall(call) if call.status == status
                && matches!(&call.detail, ToolDetail::FileChange { changes } if changes.len() == 1)
        )));
        assert!(items.iter().any(|item| matches!(
            item,
            Item::AgentMessage { text, .. } if text == expected
        )));
        let again = session.respond(&request.id, Decision::Allow).await;
        assert!(matches!(again, Err(Error::RequestNotFound { .. })));
    }
    session.close().await.unwrap();
}

#[tokio::test]
async fn without_forwarding_permission_requests_are_declined() {
    let fake = Arc::new(Fake::default());
    let session = provider(&fake)
        .start_session(spec(session::Settings::default()))
        .await
        .unwrap();
    let mut events = session.subscribe();
    session.send(turn::Input::text("permission")).await.unwrap();
    let events = turn_events(&mut events).await;
    assert!(events.iter().any(|event| matches!(
        event,
        Event::RequestDeclined { method, summary, .. }
            if method == "session/request_permission" && summary.as_deref() == Some("Edit a.txt")
    )));
    assert!(ended(&events).iter().any(|item| matches!(
        item,
        Item::AgentMessage { text, .. } if text == "no"
    )));
    session.close().await.unwrap();
}

#[tokio::test]
async fn interrupting_cancels_the_turn() {
    let fake = Arc::new(Fake::default());
    let session = provider(&fake)
        .start_session(spec(session::Settings::default()))
        .await
        .unwrap();
    let mut events = session.subscribe();
    let receipt = session.send(turn::Input::text("wait")).await.unwrap();
    // Wait until the agent is working, then interrupt.
    loop {
        if let Event::ItemDelta { .. } = next(&mut events).await {
            break;
        }
    }
    let busy = session.send(turn::Input::text("hello")).await;
    assert!(matches!(busy, Err(Error::Rpc { .. })), "one turn at a time");
    session.interrupt(&receipt.turn_id).await.unwrap();
    let events = turn_events(&mut events).await;
    assert_eq!(turn_end(&events).status, turn::Status::Interrupted);
    assert!(ended(&events).iter().any(|item| matches!(
        item,
        Item::AgentMessage { text, .. } if text == "Working"
    )));
    assert_eq!(fake.seen.lock().unwrap().cancels, 1);
    session.close().await.unwrap();
}

#[tokio::test]
async fn cancelling_a_permission_request_withdraws_it() {
    let fake = Arc::new(Fake::default());
    let session = provider(&fake)
        .start_session(spec(session::Settings {
            forward_requests: true,
            ..Default::default()
        }))
        .await
        .unwrap();
    let mut events = session.subscribe();
    let receipt = session.send(turn::Input::text("permission")).await.unwrap();
    let request = loop {
        if let Event::Request { request } = next(&mut events).await {
            break request;
        }
    };
    session.interrupt(&receipt.turn_id).await.unwrap();
    let events = turn_events(&mut events).await;
    assert!(events.contains(&Event::RequestResolved {
        id: request.id,
        outcome: request::Outcome::Withdrawn,
    }));
    assert_eq!(turn_end(&events).status, turn::Status::Interrupted);
    session.close().await.unwrap();
}

#[tokio::test]
async fn modes_are_listed_and_switched() {
    let fake = Arc::new(Fake::default());
    let session = provider(&fake)
        .start_session(spec(session::Settings::default()))
        .await
        .unwrap();
    let modes = session.modes().unwrap();
    assert_eq!(modes.current, "ask");
    let ids: Vec<&str> = modes.available.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["ask", "code"]);
    assert_eq!(
        modes.available[0].description.as_deref(),
        Some("Asks before changes")
    );
    session.set_mode("code").await.unwrap();
    assert_eq!(session.modes().unwrap().current, "code");
    assert_eq!(fake.seen.lock().unwrap().modes, ["code"]);
    let invalid = session.set_mode("yolo").await;
    assert!(matches!(
        invalid,
        Err(Error::InvalidSetting {
            setting: "mode",
            ..
        })
    ));
    session.close().await.unwrap();
}

#[tokio::test]
async fn files_are_served_within_the_session_folder() {
    let fake = Arc::new(Fake::default());
    let dir = std::env::temp_dir().join(format!("keel-acp-files-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("notes.txt"), "remember").unwrap();
    let session = provider(&fake)
        .start_session(session::Spec {
            cwd: dir.clone(),
            settings: Default::default(),
        })
        .await
        .unwrap();
    let mut events = session.subscribe();
    let mut say = async |text: String| -> String {
        session.send(turn::Input::text(text)).await.unwrap();
        let events = turn_events(&mut events).await;
        ended(&events)
            .into_iter()
            .find_map(|item| match item {
                Item::AgentMessage { text, .. } => Some(text.clone()),
                _ => None,
            })
            .unwrap()
    };
    assert_eq!(
        say(format!("read {}", dir.join("notes.txt").display())).await,
        "remember"
    );
    assert_eq!(say("read /etc/hostname".to_owned()).await, "error -32602");
    assert_eq!(
        say(format!("write {}", dir.join("new/out.txt").display())).await,
        "ok"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("new/out.txt")).unwrap(),
        "written"
    );
    session.close().await.unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn stored_sessions_are_listed_and_resumed() {
    let fake = Arc::new(Fake::default());
    let provider = provider(&fake);
    let page = provider
        .list_sessions(session::Query::default())
        .await
        .unwrap();
    assert_eq!(page.sessions.len(), 1);
    let summary = &page.sessions[0];
    assert_eq!(summary.id, SESSION);
    assert_eq!(summary.title.as_deref(), Some("Fake work"));
    assert_eq!(summary.updated_at, 1_794_830_400);
    assert_eq!(
        provider.read_session(SESSION).await.unwrap().cwd,
        summary.cwd
    );
    assert!(matches!(
        provider.read_session("missing").await,
        Err(Error::SessionNotFound { .. })
    ));
    let session = provider
        .resume_session(SESSION, session::Settings::default())
        .await
        .unwrap();
    assert!(session.info().restored);
    assert_eq!(
        fake.seen.lock().unwrap().resumes,
        [(SESSION.to_owned(), "/work/project".to_owned())]
    );
    session.close().await.unwrap();
    let missing = provider
        .resume_session_in(
            "missing",
            &std::env::temp_dir(),
            session::Settings::default(),
        )
        .await;
    assert!(matches!(missing, Err(Error::SessionNotFound { .. })));
    assert!(matches!(
        provider.list_turns(SESSION, turn::Query::default()).await,
        Err(Error::Unsupported {
            operation: "list_turns"
        })
    ));
}

#[tokio::test]
async fn closing_reports_the_client_and_stops_the_connection() {
    let fake = Arc::new(Fake::default());
    let session = provider(&fake)
        .start_session(spec(session::Settings::default()))
        .await
        .unwrap();
    let mut events = session.subscribe();
    session.close().await.unwrap();
    assert_eq!(
        next(&mut events).await,
        Event::Closed {
            reason: session::CloseReason::Client
        }
    );
    assert_eq!(fake.seen.lock().unwrap().closes, 1);
    let late = session.send(turn::Input::text("hello")).await;
    assert!(matches!(late, Err(Error::ProcessExited { .. })));
}

#[tokio::test]
async fn unsupported_settings_are_refused() {
    let fake = Arc::new(Fake::default());
    let provider = provider(&fake);
    let fast = provider
        .start_session(spec(session::Settings {
            fast_mode: Some(true),
            ..Default::default()
        }))
        .await;
    assert!(matches!(
        fast,
        Err(Error::InvalidSetting {
            setting: "fast_mode",
            ..
        })
    ));
    let permissions = provider
        .start_session(spec(session::Settings {
            permissions: Some(crate::permission::Configuration::Codex(Default::default())),
            ..Default::default()
        }))
        .await;
    assert!(matches!(permissions, Err(Error::Unsupported { .. })));
}

#[tokio::test]
async fn a_missing_program_is_reported() {
    let provider = AcpProvider::new(AcpConfig::new(
        "missing",
        "Missing",
        Launch::new("/nonexistent/keel-acp-agent"),
    ));
    let result = provider
        .start_session(spec(session::Settings::default()))
        .await;
    assert!(matches!(result, Err(Error::ExecutableNotFound { .. })));
    let report = super::probe(
        &Launch::new("/nonexistent/keel-acp-agent"),
        &crate::process::Environment::inherit(),
        Some(Duration::from_secs(1)),
    )
    .await;
    assert!(!report.installed());
    assert!(!report.initialized);
}

/// A real process: a shell script that answers `initialize`, so spawning,
/// the standard stream transport and shutdown run.
#[cfg(unix)]
#[tokio::test]
async fn a_process_agent_is_probed_over_its_standard_streams() {
    let dir = std::env::temp_dir().join(format!("keel-acp-process-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("agent");
    crate::process::write_executable(
        &script,
        r#"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\("[^"]*"\|[0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":1,"agentInfo":{"name":"sh-agent","version":"0.1.0"},"authMethods":[{"id":"key","name":"API key"}],"agentCapabilities":{"loadSession":true}}}\n' "$id" ;;
  esac
done
"#,
    );
    let launch = Launch::new(&script).env("KEEL_TEST", "1");
    let environment = crate::process::Environment::inherit();
    let found = super::probe(&launch, &environment, None).await;
    assert_eq!(found.path.as_deref(), Some(script.as_path()));
    assert!(!found.initialized);
    let report = super::probe(&launch, &environment, Some(Duration::from_secs(10))).await;
    assert!(report.initialized, "{report:?}");
    assert_eq!(report.agent.unwrap().name, "sh-agent");
    assert_eq!(report.auth_methods[0].id, "key");
    assert!(report.capabilities.unwrap().load_session);
    std::fs::remove_dir_all(&dir).unwrap();
}
