//! The shared app-server against a stand-in `codex` executable: when it
//! starts, when it closes, and what it is asked.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{Codex, CodexConfig};
use crate::session;

/// A stand-in `codex app-server` that records each start's process ID and
/// each request's method, and answers what Keel asks here.
fn executable(folder: &Path) -> PathBuf {
    let path = folder.join("codex");
    let script = format!(
        r#"#!/bin/sh
[ "$1" = app-server ] || exit 1
echo $$ >> '{starts}'
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/^{{"id":\([0-9]*\),.*/\1/p')
  method=$(printf '%s' "$line" | sed -n 's/.*"method":"\([^"]*\)".*/\1/p')
  echo "$method" >> '{methods}'
  [ -n "$id" ] || continue
  case "$method" in
    initialize) printf '{{"id":%s,"result":{{"userAgent":"keel-test/0.1.0"}}}}\n' "$id" ;;
    model/list) printf '{{"id":%s,"result":{{"data":[]}}}}\n' "$id" ;;
    thread/start) printf '{{"id":%s,"result":{{"thread":{{"id":"thread-1","cwd":"/tmp","status":{{"type":"idle"}}}},"model":"gpt-test","cwd":"/tmp"}}}}\n' "$id" ;;
    thread/backgroundTerminals/list) printf '{{"id":%s,"result":{{"data":[]}}}}\n' "$id" ;;
    thread/unsubscribe) printf '{{"id":%s,"result":{{}}}}\n' "$id" ;;
    *) printf '{{"id":%s,"error":{{"code":-32601,"message":"not in this stand-in"}}}}\n' "$id" ;;
  esac
done
"#,
        starts = folder.join("starts").display(),
        methods = folder.join("methods").display(),
    );
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
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
    let folder = std::env::temp_dir().join(format!("keel-codex-{}", uuid::Uuid::new_v4()));
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

    codex.catalog().await.unwrap();
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
