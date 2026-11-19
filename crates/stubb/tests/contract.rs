//! Checks that every provider keeps the behaviour the [`stubb::Provider`] and
//! [`stubb::session::Driver`] documentation promises, against the real agents.
//!
//! The checks start agents and run short turns, which can cost tokens, so
//! they are ignored by default. Run them with:
//!
//! ```sh
//! cargo test --test contract -- --ignored --nocapture
//! STUBB_CONTRACT_PROVIDERS=pi cargo test --test contract -- --ignored --nocapture
//! ```
//!
//! `STUBB_CONTRACT_PROVIDERS` is a comma-separated list of `codex`, `claude`
//! and `pi`; it defaults to all three. A provider whose executable is not
//! installed is skipped.

use std::path::{Path, PathBuf};
use std::time::Duration;

use stubb::catalog::FastMode;
use stubb::providers::claude::{Claude, ClaudeConfig};
use stubb::providers::codex::{Codex, CodexConfig};
use stubb::providers::pi::{Pi, PiConfig};
use stubb::session::{self, CloseReason, Event};
use stubb::turn::{self, Item};
use stubb::{Error, Provider, Session};
use tokio::sync::broadcast;

const TURN_TIMEOUT: Duration = Duration::from_secs(180);
const UNKNOWN_ID: &str = "00000000-0000-4000-8000-000000000000";

#[tokio::test(flavor = "multi_thread")]
#[ignore = "starts the installed agents and runs short turns"]
async fn providers_keep_the_contract() {
    let wanted = std::env::var("STUBB_CONTRACT_PROVIDERS")
        .or_else(|_| std::env::var("KEEL_CONTRACT_PROVIDERS"))
        .unwrap_or_else(|_| "codex,claude,pi".to_owned());
    let mut checked = 0;
    for name in wanted.split(',').map(str::trim) {
        let (executable, provider): (&str, Box<dyn Provider>) = match name {
            "codex" => ("codex", Box::new(Codex::new(CodexConfig::default()))),
            "claude" => ("claude", Box::new(Claude::new(ClaudeConfig::default()))),
            "pi" => ("pi", Box::new(Pi::new(PiConfig::default()))),
            other => panic!("unknown provider {other:?}"),
        };
        if !on_path(executable) {
            eprintln!("skipping {name}: `{executable}` is not installed");
            continue;
        }
        eprintln!("checking {name}");
        check(name, provider.as_ref()).await;
        provider.shutdown().await;
        checked += 1;
    }
    assert!(checked > 0, "no provider was installed");
}

async fn check(name: &str, provider: &dyn Provider) {
    assert_eq!(provider.id(), name);
    let catalog = provider.catalog().await.expect("catalog");
    check_catalog(&catalog);
    check_unknown_session(provider).await;
    check_invalid_settings(provider).await;

    let cwd = scratch_dir(name);
    let live = provider
        .start_session(session::Spec {
            cwd: cwd.clone(),
            settings: session::Settings::default(),
        })
        .await
        .expect("start a session");
    assert_eq!(live.info().cwd, cwd);
    assert!(!live.info().restored);

    let turn_id = check_turn(&live).await;
    check_close(&live).await;
    check_history(provider, live.id(), &turn_id, &cwd).await;

    let resumed = provider
        .resume_session(live.id(), session::Settings::default())
        .await
        .expect("resume the session");
    assert!(resumed.info().restored);
    assert_eq!(resumed.id(), live.id());
    check_interrupt(&resumed).await;
    resumed.close().await.expect("close the resumed session");
}

fn check_catalog(catalog: &stubb::Catalog) {
    assert!(!catalog.models.is_empty(), "the catalog lists no models");
    let defaults = catalog
        .models
        .iter()
        .filter(|model| model.is_default)
        .count();
    assert!(defaults <= 1, "{defaults} default models");
    for model in &catalog.models {
        if let Some(effort) = &model.default_effort {
            assert!(
                model.efforts.iter().any(|option| &option.id == effort),
                "{}: default effort {effort} is not one of its efforts",
                model.id
            );
        }
        if let FastMode::Available { tier_id, .. } = &model.fast_mode {
            assert!(
                !tier_id.is_empty(),
                "{}: fast mode without a tier",
                model.id
            );
        }
    }
}

/// Unknown ids are `SessionNotFound` everywhere, so hosts can handle them
/// the same way for every provider.
async fn check_unknown_session(provider: &dyn Provider) {
    let not_found = |result: Result<(), Error>, call: &str| match result {
        Err(Error::SessionNotFound { id }) => assert_eq!(id, UNKNOWN_ID),
        other => panic!("{call} of an unknown session: expected SessionNotFound, got {other:?}"),
    };
    not_found(
        provider.read_session(UNKNOWN_ID).await.map(drop),
        "read_session",
    );
    not_found(
        provider
            .list_turns(UNKNOWN_ID, turn::Query::default())
            .await
            .map(drop),
        "list_turns",
    );
    not_found(
        provider
            .resume_session(UNKNOWN_ID, session::Settings::default())
            .await
            .map(drop),
        "resume_session",
    );
}

/// Settings a provider cannot honour are rejected before a session starts.
async fn check_invalid_settings(provider: &dyn Provider) {
    let settings = session::Settings {
        effort: Some("not-an-effort".to_owned()),
        ..session::Settings::default()
    };
    let result = provider
        .start_session(session::Spec {
            cwd: std::env::temp_dir(),
            settings,
        })
        .await;
    assert!(
        matches!(
            result,
            Err(Error::InvalidSetting {
                setting: "effort",
                ..
            })
        ),
        "an unknown effort: expected InvalidSetting, got {:?}",
        result.map(|live| live.id().to_owned())
    );
}

/// One turn: the receipt names the turn its events carry, which runs from
/// `TurnStart` to `TurnEnd` between `Active` and `Idle`.
async fn check_turn(live: &Session) -> String {
    let mut events = live.subscribe();
    let receipt = live
        .send(turn::Input::text("Reply with exactly: ok"))
        .await
        .expect("send a message");
    let events = until_turn_end(&mut events).await;

    let position = |find: &dyn Fn(&Event) -> bool| events.iter().position(find);
    let active = position(&|event| {
        matches!(
            event,
            Event::Status {
                status: session::Status::Active
            }
        )
    });
    let start = position(
        &|event| matches!(event, Event::TurnStart { turn_id } if *turn_id == receipt.turn_id),
    );
    let user = position(
        &|event| matches!(event, Event::ItemEnd { turn_id, item: Item::UserMessage { .. } } if *turn_id == receipt.turn_id),
    );
    let reply = position(&|event| {
        matches!(event, Event::ItemEnd { turn_id, item: Item::AgentMessage { text, .. } }
            if *turn_id == receipt.turn_id && text.to_lowercase().contains("ok"))
    });
    let idle = position(&|event| {
        matches!(
            event,
            Event::Status {
                status: session::Status::Idle
            }
        )
    });
    let Some(Event::TurnEnd { turn }) = events.last() else {
        unreachable!("until_turn_end stops at TurnEnd");
    };

    assert!(
        start.is_some(),
        "no TurnStart for the receipt's turn: {events:#?}"
    );
    assert!(
        active < start,
        "Active must come before TurnStart: {events:#?}"
    );
    assert!(
        user.is_some_and(|user| Some(user) > start),
        "no user message item: {events:#?}"
    );
    assert!(reply.is_some(), "no reply: {events:#?}");
    assert!(idle.is_some(), "the session never went idle: {events:#?}");
    assert_eq!(turn.id, receipt.turn_id);
    assert_eq!(turn.status, turn::Status::Completed, "{turn:#?}");
    assert!(turn.items.is_empty(), "TurnEnd repeats its items");
    for event in &events {
        if let Event::ItemStart { turn_id, .. }
        | Event::ItemDelta { turn_id, .. }
        | Event::ItemEnd { turn_id, .. } = event
        {
            assert_eq!(
                *turn_id, receipt.turn_id,
                "an item of another turn: {event:?}"
            );
        }
    }
    receipt.turn_id
}

/// Closing announces `Closed { Client }` to subscribers.
async fn check_close(live: &Session) {
    let mut events = live.subscribe();
    live.close().await.expect("close the session");
    let closed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match events.recv().await {
                Ok(Event::Closed { reason }) => return Some(reason),
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return None,
            }
        }
    })
    .await;
    assert!(
        matches!(closed, Ok(Some(CloseReason::Client))),
        "close did not announce Closed {{ Client }}: {closed:?}"
    );
}

/// The finished turn is stored under the id it had live, and the session is
/// listed for its working directory.
async fn check_history(provider: &dyn Provider, id: &str, turn_id: &str, cwd: &Path) {
    let summary = provider.read_session(id).await.expect("read the session");
    assert_eq!(summary.id, id);
    assert_eq!(summary.cwd, cwd);

    let turns = provider
        .list_turns(id, turn::Query::default())
        .await
        .expect("list the turns");
    let stored = turns.turns.last().expect("a stored turn");
    assert_eq!(
        stored.id, turn_id,
        "the stored turn id differs from the live one"
    );
    assert!(
        matches!(stored.items.first(), Some(Item::UserMessage { .. })),
        "a stored turn starts with its user message"
    );

    let page = provider
        .list_sessions(session::Query {
            cwd: Some(cwd.to_owned()),
            ..session::Query::default()
        })
        .await
        .expect("list sessions");
    assert!(
        page.sessions.iter().any(|session| session.id == id),
        "the session is not listed for its directory"
    );
    assert!(page.sessions.iter().all(|session| session.cwd == cwd));
}

/// An interrupted turn ends with status `Interrupted`.
async fn check_interrupt(live: &Session) {
    let mut events = live.subscribe();
    let receipt = live
        .send(turn::Input::text(
            "Write the numbers from 1 to 300, one per line, and nothing else.",
        ))
        .await
        .expect("send a message");
    // Let the turn get going before stopping it.
    tokio::time::sleep(Duration::from_secs(2)).await;
    live.interrupt(&receipt.turn_id)
        .await
        .expect("interrupt the turn");
    let events = until_turn_end(&mut events).await;
    let Some(Event::TurnEnd { turn }) = events.last() else {
        unreachable!("until_turn_end stops at TurnEnd");
    };
    assert_eq!(turn.id, receipt.turn_id);
    assert_eq!(turn.status, turn::Status::Interrupted, "{turn:#?}");
}

/// Events up to and including the next `TurnEnd`.
async fn until_turn_end(events: &mut broadcast::Receiver<Event>) -> Vec<Event> {
    let mut seen = Vec::new();
    let wait = async {
        loop {
            match events.recv().await {
                Ok(event) => {
                    let end = matches!(event, Event::TurnEnd { .. });
                    seen.push(event);
                    if end {
                        return;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => panic!("the event stream closed"),
            }
        }
    };
    if tokio::time::timeout(TURN_TIMEOUT, wait).await.is_err() {
        panic!("no TurnEnd within {TURN_TIMEOUT:?}");
    }
    seen
}

/// A fresh working directory, so the session list holds only this test's
/// session. Canonical, since agents report resolved paths.
fn scratch_dir(provider: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("stubb-contract-{provider}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("create a scratch directory");
    dir.canonicalize().expect("canonical scratch directory")
}

fn on_path(executable: &str) -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join(executable).is_file()))
}
