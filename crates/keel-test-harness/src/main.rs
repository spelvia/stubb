//! Scripted native stdio peer. It executes no commands and calls no model.
//! Scenarios are selected by the resumed root ID, so tests need no global env.
use std::io::{self, BufRead, Write};

use serde_json::{Value, json};

fn emit(value: Value) {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    writeln!(out, "{value}").unwrap();
    out.flush().unwrap();
}
fn reply(id: &Value, result: Value) {
    emit(json!({"id":id,"result":result}));
}
fn notify(method: &str, params: Value) {
    emit(json!({"method":method,"params":params}));
}
fn native_turn(id: &str, status: &str) -> Value {
    json!({"id":id,"status":status,"items":[]})
}
fn turn(thread: &str, id: &str, status: &str) {
    notify(
        if status == "inProgress" {
            "turn/started"
        } else {
            "turn/completed"
        },
        json!({"threadId":thread,"turn":native_turn(id,status)}),
    );
}
fn thread(id: &str, root: &str) -> Value {
    let parent = if id.ends_with("/grandchild") {
        Some(format!("{root}/child"))
    } else if id.ends_with("/child") || id.ends_with("/sibling") {
        Some(root.to_owned())
    } else {
        None
    };
    json!({"id":id,"cwd":"/fixture","model":"gpt-6-luna","reasoningEffort":"medium",
        "status":{"type":if parent.is_some(){"notLoaded"}else{"idle"}},
        "parentThreadId":parent,"source":if parent.is_some(){json!({"subAgent":{}})}else{json!("appServer")}})
}
fn command(root: &str, ended: bool) {
    notify(
        if ended {
            "item/completed"
        } else {
            "item/started"
        },
        json!({"threadId":root,"turnId":"owner-turn",
        "item":{"type":"commandExecution","id":"command","command":"fake sleep","cwd":"/fixture",
        "status":if ended{"failed"}else{"inProgress"},"exitCode":if ended{json!(-1)}else{Value::Null},
        "aggregatedOutput":if ended{json!("final-log")}else{Value::Null},"durationMs":if ended{json!(8)}else{Value::Null}}}),
    );
}

fn main() {
    assert_eq!(std::env::args().nth(1).as_deref(), Some("app-server"));
    let mut root = String::new();
    let mut seeded = false;
    let mut pending = None;
    let mut terminal_lists = 0;
    for line in io::stdin().lock().lines() {
        let request: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let method = request["method"].as_str().unwrap();
        let id = &request["id"];
        let params = &request["params"];
        if method == "initialized" {
            continue;
        }
        if !root.is_empty() {
            notify(
                "fake/request",
                json!({"threadId":root,"method":method,"params":params}),
            );
        }
        match method {
            "initialize" => reply(id, json!({"userAgent":"fake-codex/0.159.0"})),
            "thread/resume" => {
                root = params["threadId"].as_str().unwrap().into();
                reply(
                    id,
                    json!({"thread":thread(&root,&root),"model":"gpt-6-luna","cwd":"/fixture","reasoningEffort":"medium"}),
                );
            }
            "thread/read" => {
                let target = params["threadId"].as_str().unwrap();
                if target == root || target.starts_with(&format!("{root}/")) || target == "foreign"
                {
                    reply(id, json!({"thread":thread(target,&root)}));
                } else {
                    emit(json!({"id":id,"error":{"code":-32600,"message":"no rollout found"}}));
                }
            }
            "thread/list" => {
                let child = format!("{root}/child");
                let grandchild = format!("{root}/grandchild");
                let sibling = format!("{root}/sibling");
                if !seeded {
                    seeded = true;
                    if (root.starts_with("task") && root != "task-polled-lost")
                        || root == "terminal-loop"
                    {
                        turn(&root, "owner-turn", "inProgress");
                        command(&root, false);
                        turn(&root, "owner-turn", "completed");
                    } else if root != "history" {
                        turn(&grandchild, "grand-turn", "inProgress");
                        notify(
                            "item/agentMessage/delta",
                            json!({"threadId":grandchild,"turnId":"grand-turn","itemId":"shared","delta":"grand"}),
                        );
                        turn(&child, "old-turn", "inProgress");
                        turn(&sibling, "sibling-turn", "inProgress");
                        notify(
                            "item/agentMessage/delta",
                            json!({"threadId":child,"turnId":"old-turn","itemId":"shared","delta":"child"}),
                        );
                    }
                }
                if root.starts_with("task") || root == "terminal-loop" {
                    reply(id, json!({"data":[],"nextCursor":null}));
                } else if params["cursor"].is_null() {
                    reply(
                        id,
                        json!({"data":[thread(&grandchild,&root)],"nextCursor":"page2"}),
                    );
                } else {
                    reply(
                        id,
                        json!({"data":[thread(&child,&root),thread(&sibling,&root)],"nextCursor":null}),
                    );
                }
            }
            "thread/turns/list" => {
                let target = params["threadId"].as_str().unwrap();
                let mut stored = native_turn(
                    if params["cursor"].is_null() {
                        "stored1"
                    } else {
                        "stored2"
                    },
                    "completed",
                );
                stored["items"] = json!([{"type":"agentMessage","id":"message","text":target}]);
                reply(
                    id,
                    json!({"data":[stored],"nextCursor":if params["cursor"].is_null(){json!("turn2")}else{Value::Null}}),
                );
            }
            "turn/interrupt" => {
                assert_eq!(params["turnId"], "old-turn");
                assert_eq!(params["threadId"], format!("{root}/child"));
                match root.as_str() {
                    "accepted" => reply(id, json!({})),
                    "exit" => return,
                    "lost-ack" => {}
                    "rejected" => {
                        emit(json!({"id":id,"error":{"code":-32600,"message":"cannot interrupt"}}))
                    }
                    _ => pending = Some(id.clone()),
                }
            }
            "turn/start" => {
                let text = params["input"][0]["text"].as_str().unwrap();
                if text == "burst" {
                    turn(&root, "burst", "inProgress");
                    for _ in 0..2200 {
                        notify(
                            "item/agentMessage/delta",
                            json!({"threadId":root,"turnId":"burst","itemId":"message","delta":"x"}),
                        );
                    }
                } else if text == "missing" {
                    turn("missing", "unowned", "inProgress");
                    notify(
                        "item/started",
                        json!({"threadId":root,"turnId":"root-request","item":{
                        "type":"collabAgentToolCall","id":"wait","tool":"wait","receiverThreadIds":["missing"]}}),
                    );
                } else {
                    turn(
                        &format!("{root}/child"),
                        "old-turn",
                        if text == "confirm" {
                            "interrupted"
                        } else {
                            "completed"
                        },
                    );
                    turn(&format!("{root}/child"), "new-turn", "inProgress");
                    if let Some(ack) = pending.take() {
                        reply(&ack, json!({}));
                    }
                }
                reply(id, json!({"turn":native_turn("root-request","inProgress")}));
            }
            "thread/backgroundTerminals/list" => {
                terminal_lists += 1;
                let data = if root.starts_with("task") || root == "terminal-loop" {
                    json!([{"itemId":"command","processId":if root=="task-reused" && terminal_lists>1{"replacement"}else{"process"},"command":"fake sleep"}])
                } else {
                    json!([])
                };
                reply(
                    id,
                    json!({"data":data,"nextCursor":if root=="terminal-loop"{json!("repeat")}else{Value::Null}}),
                );
            }
            "thread/backgroundTerminals/terminate" => {
                assert_eq!(params["processId"], "process");
                assert_eq!(params["threadId"], root);
                if root == "task-lost-ack" || root == "task-polled-lost" {
                    continue;
                }
                command(&root, true);
                reply(id, json!({"terminated":true}));
            }
            "thread/unsubscribe" => reply(id, json!({})),
            _ => emit(json!({"id":id,"error":{"code":-32601,"message":"unexpected method"}})),
        }
    }
}
