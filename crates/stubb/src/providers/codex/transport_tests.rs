//! The shared app-server against a stand-in `codex` executable: when it
//! starts, when it closes, and what it is asked.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{Codex, CodexConfig};
use crate::process::Environment;
use crate::{Error, Provider, session};

/// A stand-in `codex app-server` that records each start's process ID and
/// each request's method, and answers what Stubb asks here.
fn executable(folder: &Path) -> PathBuf {
    let path = folder.join("codex");
    let script = format!(
        r#"#!/bin/sh
[ "$1" = app-server ] || exit 1
echo $$ >> '{starts}'
env > '{environment}'
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{{"id":\([0-9]*\),.*/\1/p')
  method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  echo "$method" >> '{methods}'
  printf '%s\n' "$line" >> '{requests}'
  [ -n "$id" ] || continue
  case "$method" in
    initialize) printf '{{"id":%s,"result":{{"userAgent":"stubb-test/0.1.0"}}}}\n' "$id" ;;
    model/list) printf '{{"id":%s,"result":{{"data":[]}}}}\n' "$id" ;;
    account/rateLimits/read) printf '{{"id":%s,"result":{{"rateLimits":{{"limitId":"codex","planType":"plus","primary":{{"usedPercent":16,"windowDurationMins":10080}}}}}}}}\n' "$id" ;;
    thread/start) printf '{{"id":%s,"result":{{"thread":{{"id":"thread-1","cwd":"/tmp","status":{{"type":"idle"}}}},"model":"gpt-test","cwd":"/tmp"}}}}\n' "$id" ;;
    turn/start) printf '{{"id":%s,"result":{{"turn":{{"id":"turn-1","status":"inProgress","items":[]}}}}}}\n' "$id" ;;
    thread/backgroundTerminals/list) printf '{{"id":%s,"result":{{"data":[]}}}}\n' "$id" ;;
    thread/fork)
      case "$line" in
        *'"lastTurnId":"missing"'*) printf '{{"id":%s,"error":{{"code":-32600,"message":"turn not found: missing"}}}}\n' "$id" ;;
        *'"threadId":"unknown"'*) printf '{{"id":%s,"error":{{"code":-32600,"message":"no rollout found for thread id unknown"}}}}\n' "$id" ;;
        *) printf '{{"id":%s,"result":{{"thread":{{"id":"fork-1","preview":"hello","cwd":"/tmp","status":{{"type":"idle"}},"createdAt":7,"updatedAt":8}},"model":"gpt-test","cwd":"/tmp"}}}}\n' "$id" ;;
      esac ;;
    thread/read)
      case "$line" in
        *'"threadId":"root"'*) printf '{{"id":%s,"result":{{"thread":{{"id":"root","cwd":"/tmp","status":{{"type":"notLoaded"}},"path":"{folder}/root.jsonl"}}}}}}\n' "$id" ;;
        *'"threadId":"child"'*) printf '{{"id":%s,"result":{{"thread":{{"id":"child","cwd":"/tmp","status":{{"type":"notLoaded"}},"parentThreadId":"root","path":"{folder}/child.jsonl"}}}}}}\n' "$id" ;;
        *) printf '{{"id":%s,"error":{{"code":-32600,"message":"no rollout found for thread id"}}}}\n' "$id" ;;
      esac ;;
    thread/list) printf '{{"id":%s,"result":{{"data":[{{"id":"child","cwd":"/tmp","status":{{"type":"notLoaded"}},"parentThreadId":"root","path":"{folder}/child.jsonl"}}]}}}}\n' "$id" ;;
    thread/unsubscribe) printf '{{"id":%s,"result":{{}}}}\n' "$id" ;;
    *) printf '{{"id":%s,"error":{{"code":-32601,"message":"not in this stand-in"}}}}\n' "$id" ;;
  esac
done
"#,
        starts = folder.join("starts").display(),
        methods = folder.join("methods").display(),
        requests = folder.join("requests").display(),
        environment = folder.join("environment").display(),
        folder = folder.display(),
    );
    crate::process::write_executable(&path, &script);
    path
}

fn lines(folder: &Path, name: &str) -> Vec<String> {
    std::fs::read_to_string(folder.join(name))
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn alive(pid: &str) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .is_ok_and(|stat| !stat.rsplit(") ").next().unwrap_or("").starts_with('Z'))
}

fn folder() -> PathBuf {
    let folder = std::env::temp_dir().join(format!("stubb-codex-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&folder).unwrap();
    folder
}

fn provider(folder: &Path, idle: Option<Duration>) -> Codex {
    Codex::new(CodexConfig {
        executable: executable(folder),
        idle_timeout: idle,
        ..CodexConfig::default()
    })
}

/// Waits until `done` holds, for at most two seconds.
async fn eventually(done: impl Fn() -> bool) -> bool {
    for _ in 0..200 {
        if done() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    done()
}

#[tokio::test]
async fn account_usage_is_read_over_the_shared_connection() {
    let folder = folder();
    let codex = provider(&folder, None);

    let usage = codex.account_usage().await.unwrap();
    codex.catalog().await.unwrap();
    let again = codex.account_usage().await.unwrap();
    codex.shutdown().await;

    assert_eq!(usage.plan.as_deref(), Some("plus"));
    assert_eq!(usage.windows[0].label, "Weekly limit");
    assert_eq!(usage.windows[0].used_percent, Some(16.0));
    assert_eq!(again.windows, usage.windows);
    // One process for everything, and never a thread or a turn.
    assert_eq!(lines(&folder, "starts").len(), 1);
    let methods = lines(&folder, "methods");
    assert!(
        !methods
            .iter()
            .any(|method| method.starts_with("thread/") || method.starts_with("turn/")),
        "{methods:?}"
    );
    std::fs::remove_dir_all(folder).unwrap();
}

#[tokio::test]
async fn the_app_server_starts_with_only_the_allowed_environment() {
    let folder = folder();
    let codex = Codex::new(CodexConfig {
        executable: executable(&folder),
        idle_timeout: None,
        environment: Environment::only(["PATH"]).set("STUBB_RUN", "1"),
        ..CodexConfig::default()
    });

    codex.catalog().await.unwrap();
    codex.shutdown().await;

    let names: Vec<String> = lines(&folder, "environment")
        .iter()
        .filter_map(|line| line.split_once('=').map(|(name, _)| name.to_owned()))
        .collect();
    assert!(names.iter().any(|name| name == "PATH"), "{names:?}");
    assert!(names.iter().any(|name| name == "STUBB_RUN"), "{names:?}");
    // Besides what the shell exports for itself, nothing of the host's.
    let host: Vec<&String> = names
        .iter()
        .filter(|name| {
            !["PATH", "STUBB_RUN", "PWD", "OLDPWD", "SHLVL", "_"].contains(&name.as_str())
        })
        .collect();
    assert!(host.is_empty(), "{host:?}");
    std::fs::remove_dir_all(folder).unwrap();
}

#[tokio::test]
async fn an_idle_connection_closes_and_starts_again_on_the_next_call() {
    let folder = folder();
    let codex = provider(&folder, Some(Duration::from_millis(150)));

    codex.catalog().await.unwrap();
    let first = lines(&folder, "starts")[0].clone();
    assert!(codex.is_connected().await);
    assert!(
        eventually(|| !alive(&first)).await,
        "the idle app-server kept running"
    );
    assert!(!codex.is_connected().await);

    codex.account_usage().await.unwrap();
    assert_eq!(lines(&folder, "starts").len(), 2);
    codex.shutdown().await;
    std::fs::remove_dir_all(folder).unwrap();
}

#[tokio::test]
async fn a_live_session_keeps_the_connection_open() {
    let folder = folder();
    let codex = provider(&folder, Some(Duration::from_millis(100)));

    let live = codex
        .start_session(session::Spec {
            cwd: "/tmp".into(),
            settings: session::Settings::default(),
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(codex.is_connected().await, "closed under a live session");

    // Once the session is closed, the idle time starts.
    live.close().await.unwrap();
    let pid = lines(&folder, "starts")[0].clone();
    assert!(
        eventually(|| !alive(&pid)).await,
        "kept running after the session"
    );
    drop(live);
    assert_eq!(lines(&folder, "starts").len(), 1);
    std::fs::remove_dir_all(folder).unwrap();
}

#[tokio::test]
async fn without_an_idle_timeout_it_stays_until_shutdown() {
    let folder = folder();
    let codex = provider(&folder, None);
    codex.catalog().await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(codex.is_connected().await);
    let pid = lines(&folder, "starts")[0].clone();
    codex.shutdown().await;
    assert!(eventually(|| !alive(&pid)).await);
    std::fs::remove_dir_all(folder).unwrap();
}

/// The app-server dies with the host even when the host does not drop it:
/// here the host is a thread with its own runtime, which ends.
#[cfg(target_os = "linux")]
#[test]
fn the_app_server_dies_with_its_host() {
    let folder = folder();
    let path = folder.clone();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let codex = provider(&path, None);
            codex.catalog().await.unwrap();
            // Neither shut down nor dropped: the host just goes away.
            std::mem::forget(codex);
        });
        std::mem::forget(runtime);
    })
    .join()
    .unwrap();
    let pid = lines(&folder, "starts")[0].clone();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while alive(&pid) && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!alive(&pid), "the app-server outlived its host");
    std::fs::remove_dir_all(folder).unwrap();
}

#[tokio::test]
async fn forks_with_thread_fork_and_leaves_the_fork_stored_not_loaded() {
    let folder = folder();
    let codex = provider(&folder, None);
    assert!(codex.capabilities().fork_session);

    let fork = codex
        .fork_session(
            "thread-1",
            session::Fork {
                at_turn: Some("turn-1".into()),
            },
        )
        .await
        .unwrap();
    assert_eq!(fork.id, "fork-1");
    assert_eq!(fork.preview, "hello");
    assert_eq!(fork.status, session::Status::NotLoaded);
    let whole = codex
        .fork_session("thread-1", session::Fork::default())
        .await
        .unwrap();
    assert_eq!(whole.id, "fork-1");

    let missing_turn = codex
        .fork_session(
            "thread-1",
            session::Fork {
                at_turn: Some("missing".into()),
            },
        )
        .await;
    assert!(
        matches!(&missing_turn, Err(Error::TurnNotFound { session_id, id }) if session_id == "thread-1" && id == "missing"),
        "{missing_turn:?}"
    );
    let unknown = codex
        .fork_session("unknown", session::Fork::default())
        .await;
    assert!(
        matches!(&unknown, Err(Error::SessionNotFound { id }) if id == "unknown"),
        "{unknown:?}"
    );
    codex.shutdown().await;

    let requests: Vec<serde_json::Value> = lines(&folder, "requests")
        .iter()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let forks: Vec<_> = requests
        .iter()
        .filter(|request| request["method"] == "thread/fork")
        .map(|request| request["params"].clone())
        .collect();
    assert_eq!(
        forks[0],
        serde_json::json!({ "threadId": "thread-1", "lastTurnId": "turn-1", "ephemeral": false, "excludeTurns": true })
    );
    assert_eq!(
        forks[1],
        serde_json::json!({ "threadId": "thread-1", "ephemeral": false, "excludeTurns": true })
    );
    // Each successful fork is unsubscribed again, so it is not left loaded.
    let unsubscribed: Vec<_> = requests
        .iter()
        .filter(|request| request["method"] == "thread/unsubscribe")
        .map(|request| request["params"]["threadId"].clone())
        .collect();
    assert_eq!(unsubscribed, ["fork-1", "fork-1"]);
    std::fs::remove_dir_all(folder).unwrap();
}

#[tokio::test]
async fn turns_ask_for_a_reasoning_summary() {
    for (summary, expected) in [
        (Some("auto"), serde_json::json!("auto")),
        (Some("detailed"), serde_json::json!("detailed")),
        (None, serde_json::Value::Null),
    ] {
        let folder = folder();
        let codex = Codex::new(CodexConfig {
            executable: executable(&folder),
            idle_timeout: None,
            reasoning_summary: summary.map(str::to_owned),
            ..CodexConfig::default()
        });
        let live = codex
            .start_session(session::Spec {
                cwd: "/tmp".into(),
                settings: session::Settings::default(),
            })
            .await
            .unwrap();
        live.send(crate::turn::Input::text("hi")).await.unwrap();
        live.close().await.unwrap();
        codex.shutdown().await;

        let turn = lines(&folder, "requests")
            .iter()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .find(|request| request["method"] == "turn/start")
            .unwrap();
        assert_eq!(turn["params"]["summary"], expected, "{turn}");
        std::fs::remove_dir_all(folder).unwrap();
    }
    assert_eq!(
        CodexConfig::default().reasoning_summary.as_deref(),
        Some("auto")
    );
}

#[tokio::test]
async fn stored_tasks_are_read_from_the_rollouts_of_the_session_and_its_descendants() {
    use crate::observation::task;
    let folder = folder();
    std::fs::write(
        folder.join("root.jsonl"),
        include_str!("fixtures/codex-rollout-0.170.0.jsonl"),
    )
    .unwrap();
    let child = include_str!("fixtures/codex-rollout-0.170.0.jsonl")
        .lines()
        .filter(|line| line.contains("task_complete") || line.contains("exec-child"))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(folder.join("child.jsonl"), child).unwrap();
    let codex = provider(&folder, None);

    let all = codex
        .list_tasks("root", task::Query::default())
        .await
        .unwrap();
    let owned: Vec<_> = all
        .tasks
        .iter()
        .map(|task| (task.id.as_str(), task.owner_agent_id.as_deref().unwrap()))
        .collect();
    assert_eq!(
        owned,
        [
            ("exec-server", "root"),
            ("exec-interrupted", "root"),
            ("exec-child", "child")
        ]
    );
    assert!(all.next_cursor.is_none());
    let first = codex
        .list_tasks(
            "root",
            task::Query {
                limit: Some(2),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(first.tasks.len(), 2);
    let rest = codex
        .list_tasks(
            "root",
            task::Query {
                cursor: first.next_cursor.clone(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(rest.tasks[0].id, "exec-child");
    let rescoped = codex
        .list_tasks(
            "root",
            task::Query {
                owner_agent_id: Some("child".into()),
                cursor: first.next_cursor,
                ..Default::default()
            },
        )
        .await;
    assert!(matches!(rescoped, Err(Error::InvalidSetting { .. })));
    let children = codex
        .list_tasks(
            "root",
            task::Query {
                owner_agent_id: Some("child".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(children.tasks.len(), 1);
    let stranger = codex
        .list_tasks(
            "root",
            task::Query {
                owner_agent_id: Some("stranger".into()),
                ..Default::default()
            },
        )
        .await;
    assert!(
        matches!(stranger, Err(Error::AgentNotFound { .. })),
        "{stranger:?}"
    );

    let mut text = String::new();
    let mut cursor = None;
    loop {
        let page = codex
            .task_output(
                "root",
                "exec-server",
                task::OutputQuery {
                    limit: Some(8),
                    cursor,
                },
            )
            .await
            .unwrap();
        assert_eq!(page.availability, task::OutputAvailability::Complete);
        text.push_str(&page.text);
        cursor = page.next_cursor;
        if page.eof {
            assert!(cursor.is_none());
            break;
        }
    }
    assert_eq!(text, "ready on :3000\nGET / 200\n");
    let missing = codex
        .task_output("root", "exec-quick", task::OutputQuery::default())
        .await;
    assert!(
        matches!(missing, Err(Error::TaskNotFound { .. })),
        "{missing:?}"
    );
    let unknown = codex.list_tasks("unknown", task::Query::default()).await;
    assert!(
        matches!(unknown, Err(Error::SessionNotFound { .. })),
        "{unknown:?}"
    );
    codex.shutdown().await;
    std::fs::remove_dir_all(folder).unwrap();
}
