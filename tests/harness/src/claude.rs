//! Scripted Claude stream-json peer; no model calls or command execution.
use super::emit;
use serde_json::{Value, json};
use std::io::{self, BufRead};

fn ack(id: &Value, result: Value) {
    emit(
        json!({"type":"control_response","response":{"subtype":"success","request_id":id,"response":result}}),
    );
}
fn system(subtype: &str, payload: Value) {
    let mut value = payload;
    value["type"] = json!("system");
    value["subtype"] = json!(subtype);
    emit(value);
}
fn start(id: &str, tool: &str, kind: &str) {
    system(
        "task_started",
        json!({"task_id":id,"tool_use_id":tool,"task_type":kind,"subagent_type":if kind=="local_agent"{Some("general-purpose")}else{None},"is_backgrounded":true,"description":id}),
    );
}
fn terminal(id: &str, tool: &str, status: &str) {
    system(
        "task_notification",
        json!({"task_id":id,"tool_use_id":tool,"status":status,"summary":format!("{id} {status}")}),
    );
}
fn assistant(parent: Option<&str>, id: &str, content: Value) {
    emit(
        json!({"type":"assistant","parent_tool_use_id":parent,"message":{"id":id,"model":"claude-sonnet-4-6","content":content}}),
    );
}
fn tool(parent: Option<&str>, id: &str, name: &str) {
    assistant(
        parent,
        id,
        json!([{"type":"tool_use","id":id,"name":name,"input":{"command":"fixture command"}}]),
    );
}
fn delta(parent: &str, text: &str) {
    emit(
        json!({"type":"stream_event","parent_tool_use_id":parent,"event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}}),
    );
}
fn stream(parent: &str) {
    for event in [
        json!({"type":"message_start","message":{"id":format!("message-{parent}")}}),
        json!({"type":"content_block_start","index":0,"content_block":{"type":"text"}}),
    ] {
        emit(json!({"type":"stream_event","parent_tool_use_id":parent,"event":event}));
    }
}
fn result(interrupted: bool) {
    emit(
        json!({"type":"result","subtype":if interrupted{"error_during_execution"}else{"success"},"is_error":interrupted,"terminal_reason":if interrupted {Some("aborted_by_user")}else{None},"errors":if interrupted{json!(["Request interrupted by user"])}else{json!([])},"usage":{}}),
    );
}

pub(super) fn run() {
    let args: Vec<_> = std::env::args().collect();
    let root = args
        .iter()
        .find_map(|a| {
            a.strip_prefix("--session-id=")
                .or_else(|| a.strip_prefix("--resume="))
        })
        .unwrap_or("");
    let scenario = std::fs::read_to_string("scenario").unwrap_or_default();
    let mut pending: Option<(Value, String, String)> = None;
    let mut stops = 0;
    for line in io::stdin().lock().lines() {
        let m: Value = serde_json::from_str(&line.unwrap()).unwrap();
        match m["type"].as_str() {
            Some("control_request") => {
                let r = &m["request"];
                let id = &m["request_id"];
                match r["subtype"].as_str() {
                    Some("initialize") => {
                        assert!(root.is_empty() || r["forwardSubagentText"] == true);
                        if !root.is_empty() {
                            assert_eq!(
                                std::env::var_os("CLAUDE_CONFIG_DIR").map(std::path::PathBuf::from),
                                Some(std::env::current_dir().unwrap().join("config"))
                            );
                        }
                        ack(
                            id,
                            json!({"models":[{"value":"claude-sonnet-4-6","displayName":"Sonnet 4.6","supportedEffortLevels":["medium"]}],"current_permission_mode":"default"}),
                        );
                        if !root.is_empty() {
                            system(
                                "fixture",
                                json!({"config_dir":std::env::var("CLAUDE_CONFIG_DIR").ok()}),
                            );
                        }
                    }
                    Some("interrupt") => {
                        result(true);
                        ack(id, json!({}));
                    }
                    Some("stop_task") => {
                        stops += 1;
                        let target = r["task_id"].as_str().unwrap();
                        let tool = if target == "command" {
                            "bash"
                        } else {
                            "spawn-child"
                        };
                        system("fixture_stop", json!({"count":stops,"target":target}));
                        match scenario.as_str() {
                            "rejected" => emit(
                                json!({"type":"control_response","response":{"subtype":"error","request_id":id,"error":"fixture refusal"}}),
                            ),
                            "exit" => std::process::exit(3),
                            "before-ack" | "natural" => {
                                terminal(
                                    target,
                                    tool,
                                    if scenario == "natural" {
                                        "completed"
                                    } else {
                                        "stopped"
                                    },
                                );
                                tool_resume(target);
                                ack(id, json!({}));
                            }
                            "pending" | "detach" => {
                                pending = Some((id.clone(), target.into(), tool.into()))
                            }
                            "lost-ack" => pending = Some((Value::Null, target.into(), tool.into())),
                            _ => {
                                ack(id, json!({}));
                                pending = Some((Value::Null, target.into(), tool.into()));
                            }
                        }
                    }
                    _ => ack(id, json!({})),
                }
            }
            Some("user") => {
                match m["message"]["content"]
                    .as_str()
                    .or_else(|| m["message"]["content"][0]["text"].as_str())
                    .unwrap_or("")
                {
                    "seed" => {
                        // Forwarded grandchildren can arrive before both owner and task metadata.
                        assistant(
                            Some("spawn-grand"),
                            "early",
                            json!([{"type":"text","text":"nested fixture"}]),
                        );
                        start("grand", "spawn-grand", "local_agent");
                        tool(None, "spawn-child", "Agent");
                        start("child", "spawn-child", "local_agent");
                        tool(Some("spawn-child"), "spawn-grand", "Agent");
                        tool(None, "spawn-sibling", "Agent");
                        start("sibling", "spawn-sibling", "local_agent");
                        tool(Some("spawn-child"), "bash", "Bash");
                        start("command", "bash", "local_bash");
                        system(
                            "background_tasks_changed",
                            json!({"tasks":[{"task_id":"child","task_type":"local_agent"},{"task_id":"command","task_type":"local_bash"},{"task_id":"sibling","task_type":"local_agent"}]}),
                        );
                        system(
                            "task_progress",
                            json!({"task_id":"command","tool_use_id":"bash","usage":{"duration_ms":10},"last_tool_name":"Bash","summary":"waiting"}),
                        );
                        let path = std::env::current_dir()
                            .unwrap()
                            .join(root)
                            .join("tasks/command.output");
                        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                        std::fs::write(&path, "fixture log").unwrap();
                        emit(
                            json!({"type":"user","parent_tool_use_id":"spawn-child","message":{"content":[{"type":"tool_result","tool_use_id":"bash","content":format!("Background output available at {}",path.display())}]}}),
                        );
                        stream("spawn-child");
                        delta("spawn-child", "half");
                        result(false);
                    }
                    "resume" => {
                        tool_resume("child");
                        result(false);
                    }
                    "wait" => {
                        tool(None, "root-command", "Bash");
                    }
                    "confirm" => {
                        if let Some((id, target, tool)) = pending.take() {
                            terminal(&target, &tool, "stopped");
                            if !id.is_null() {
                                ack(&id, json!({}));
                            }
                        }
                        result(false);
                    }
                    "natural" => {
                        if let Some((id, target, tool)) = pending.take() {
                            terminal(&target, &tool, "completed");
                            tool_resume(&target);
                            if !id.is_null() {
                                ack(&id, json!({}));
                            }
                        }
                        result(false);
                    }
                    "flood" => {
                        for _ in 0..1200 {
                            delta("spawn-child", "x");
                        }
                        system("fixture_flood_done", json!({}));
                        result(false);
                    }
                    "permissions" => {
                        tool(Some("spawn-child"), "child-denied", "Bash");
                        emit(
                            json!({"type":"control_request","request_id":"permission","request":{"subtype":"can_use_tool","tool_name":"Bash","tool_use_id":"child-denied","input":{"command":"fixture command"}}}),
                        );
                    }
                    _ => result(false),
                }
            }
            Some("control_response") => {
                system("fixture_permission", json!({"response":m["response"]}));
                result(false);
            }
            _ => {}
        }
    }
}
fn tool_resume(target: &str) {
    if target == "child" {
        tool(None, "resume-child", "SendMessage");
        start(target, "resume-child", "local_agent");
    }
}
