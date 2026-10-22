//! Context compaction: `context.compacting` / `context.compacted` recorded and shown, the
//! stall and hung-turn checks held while a compaction runs, and the user's `thread.compact`
//! (a turn of its own, between turns, user-only, refused where it cannot run). The fake
//! runtime stands in for a real one: `!compact [manual] [ms]` compacts, `!context n` reports a
//! context, and `FakeAdapter::with_compaction` makes it take `AdapterCmd::Compact`.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::timeout;
use twr_adapters::fake::FakeAdapter;
use twr_core::adapter::{Adapter, AdapterCmd, AdapterHandle, AdapterInfo, SpawnConfig, Tier};
use twr_core::config::Config;
use twr_core::event::*;
use twr_core::proto::codes;
use twr_hub::hub::{ThreadSubscription, WAKE_PROMPT_START};
use twr_hub::{Caller, Hub, HubOptions, NewThread};

const WAIT: Duration = Duration::from_secs(10);

/// Runtimes: `fake` (cannot compact on request), `fakec` and `fakec2` (can), `failc` (its
/// compactions fail, though their turns complete).
fn opts(dir: &Path) -> HubOptions {
    let mut o = HubOptions::new(dir.join("data").join("twr.db"), Config::default());
    o.adapters = Arc::new(|name: &str| -> Option<Arc<dyn Adapter>> {
        match name {
            "fake" => Some(Arc::new(FakeAdapter::new())),
            "fakec" | "fakec2" => Some(Arc::new(FakeAdapter::new().with_compaction())),
            "failc" => Some(Arc::new(FailingCompaction)),
            _ => None,
        }
    });
    o.stop_timeout = Duration::from_secs(5);
    o
}

async fn new_thread(hub: &Hub, project: &Path, name: &str, runtime: &str) -> String {
    let v = hub
        .new_thread(
            NewThread {
                project: project.to_string_lossy().into_owned(),
                runtime: Some(runtime.into()),
                name: Some(name.into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap();
    v["id"].as_str().unwrap().to_string()
}

fn send(hub: &Hub, thread: &str, prompt: &str) {
    hub.send(thread, prompt, TurnFrom::User, Source::User("test".into()))
        .unwrap();
}

async fn until(sub: &mut ThreadSubscription, pred: impl Fn(&Event) -> bool) -> Vec<Event> {
    let mut seen = Vec::new();
    timeout(WAIT, async {
        loop {
            let ev = sub.next().await.unwrap().expect("hub stopped");
            let done = pred(&ev);
            seen.push((*ev).clone());
            if done {
                return;
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "timed out; saw {:?}",
            seen.iter()
                .map(|e| e.kind().to_string())
                .collect::<Vec<_>>()
        )
    });
    seen
}

fn ended_turn(n: u64) -> impl Fn(&Event) -> bool {
    move |e| e.kind() == "turn.ended" && e.turn == Some(n)
}

/// A compaction far longer than the stall threshold, with a message waiting behind its turn:
/// neither a stalled item nor a hung-turn card comes while it runs, the activity line says it
/// compacts, and both its records are on the thread and the control stream.
#[tokio::test]
async fn a_long_compaction_raises_no_stall_or_hung_turn() {
    let dir = tempfile::tempdir().unwrap();
    let mut o = opts(dir.path());
    o.stall_after = Some(Duration::from_millis(300));
    let hub = Hub::open(o).await.unwrap();
    let t = new_thread(&hub, dir.path(), "api", "fake").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "!compact 1500\ndone");
    until(&mut sub, |e| e.kind() == "context.compacting").await;
    // Waits behind the turn: without the hold, a hung-turn card after 300 ms.
    send(&hub, &t, "next");
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        hub.thread_activity()[&t]["activity"],
        twr_core::activity::COMPACTING
    );
    let evs = until(&mut sub, ended_turn(1)).await;
    let raised: Vec<_> = evs
        .iter()
        .filter(|e| e.kind() == "attention.raised")
        .collect();
    assert!(raised.is_empty(), "no stall while compacting: {raised:?}");
    let c = evs
        .iter()
        .find_map(|e| match &e.body {
            EventBody::ContextCompacted(c) => Some((e.turn, c.clone())),
            _ => None,
        })
        .expect("the compaction is recorded");
    assert_eq!(c.0, Some(1));
    assert_eq!(c.1.trigger, "auto");
    let (items, _) = hub.control_read(0, 10_000).unwrap();
    let kinds: Vec<&str> = items
        .iter()
        .filter(|(_, e)| e.thread == t)
        .map(|(_, e)| e.kind())
        .collect();
    assert!(kinds.contains(&"context.compacting") && kinds.contains(&"context.compacted"));
    until(&mut sub, ended_turn(2)).await;
    hub.shutdown().await;
}

/// How full the context is folds into the thread (`context`, from its turns' ends and what a
/// compaction left), which `thread.get` returns.
#[tokio::test]
async fn the_thread_says_how_full_its_context_is() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let t = new_thread(&hub, dir.path(), "api", "fake").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    assert!(hub.get_thread(&t).unwrap().get("context").is_none());
    send(&hub, &t, "!context 34000\nhi");
    until(&mut sub, ended_turn(1)).await;
    let j = hub.get_thread(&t).unwrap();
    assert_eq!(j["context"]["tokens"], 34_000);
    assert_eq!(j["context"]["window"], 100_000);
    send(&hub, &t, "!compact");
    until(&mut sub, ended_turn(2)).await;
    assert_eq!(hub.get_thread(&t).unwrap()["context"]["tokens"], 500);
    hub.shutdown().await;
}

/// The user's compaction: a turn of its own (`/compact`, `ext.twr.compact`, from the user),
/// after the turn running and the messages queued before it was asked for. It is maintenance:
/// it ends `completed` with no grade, report or review, and the thread's last turn and report
/// stay those of its work (turn 2's).
#[tokio::test]
async fn the_user_compacts_a_thread_between_turns() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let t = new_thread(&hub, dir.path(), "api", "fakec").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "!sleep 400\none");
    until(&mut sub, |e| e.kind() == "turn.started").await;
    send(&hub, &t, "two");
    let user = Caller::user("test");
    let r = hub.compact_thread(&t, &user).unwrap();
    assert_eq!(r["state"], "queued");
    assert_eq!(hub.compact_thread(&t, &user).unwrap()["already"], true);
    let evs = until(&mut sub, ended_turn(3)).await;
    let starts: Vec<(Option<u64>, TurnStarted)> = evs
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::TurnStarted(p) => Some((e.turn, p.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(starts[0].1.prompt, "two");
    let (turn, compact) = &starts[1];
    assert_eq!(*turn, Some(3));
    assert!(is_compact_turn(compact));
    assert_eq!(compact.prompt, "/compact");
    assert_eq!(compact.from, TurnFrom::User);
    assert!(evs.iter().any(|e| e.turn == Some(3)
        && matches!(&e.body, EventBody::ContextCompacted(c) if c.trigger == "manual")));
    assert!(evs.iter().any(|e| e.turn == Some(3)
        && matches!(&e.body, EventBody::TurnEnded(p) if p.result == TurnResult::Completed)));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (_, all) = hub.thread_events(&t, 0, 10_000).unwrap();
    assert!(
        !all.iter().any(|e| e.turn == Some(3)
            && matches!(
                e.kind(),
                "report.ready" | "decision.made" | "attention.raised"
            )),
        "no grade, report or review"
    );
    let v = hub.thread_view(&t).unwrap();
    assert_eq!(v.last_turn.as_ref().map(|l| l.turn), Some(2));
    assert_eq!(v.last_report.as_ref().and_then(|r| r.turn), Some(2));
    assert!(v.reported && v.current_turn.is_none());
    assert!(hub.attention_list(true).is_empty(), "no review or error");
    // Idle: it starts at once.
    let r = hub.compact_thread(&t, &user).unwrap();
    assert_eq!(
        (r["state"].as_str(), r["turn"].as_u64()),
        (Some("started"), Some(4))
    );
    until(&mut sub, ended_turn(4)).await;
    hub.shutdown().await;
}

/// Only the user compacts a thread, and only where its runtime can: the orchestrator is
/// refused, as is a runtime that cannot compact from twr (named) and a thread with no session.
/// The method is in neither the orchestrator's nor an agent's set (the socket refuses it to
/// both, `agents.rs` and `orchestrator_socket.rs` try every such method).
#[tokio::test]
async fn compaction_is_the_users_and_refused_where_it_cannot_run() {
    assert!(twr_hub::server::METHODS.contains(&"thread.compact"));
    assert!(!twr_hub::server::AGENT_METHODS.contains(&"thread.compact"));
    assert!(!twr_hub::server::ORCHESTRATOR_METHODS.contains(&"thread.compact"));
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let user = Caller::user("test");
    let c = new_thread(&hub, dir.path(), "web", "fakec").await;
    let e = hub.compact_thread(&c, &user).unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST);
    assert!(e.message.contains("no session"), "{}", e.message);
    let mut sub = hub.subscribe_thread(&c, 0).unwrap();
    send(&hub, &c, "hi");
    until(&mut sub, ended_turn(1)).await;
    let e = hub.compact_thread(&c, &Caller::Orchestrator).unwrap_err();
    assert_eq!(e.code, codes::UNAUTHORIZED);
    let t = new_thread(&hub, dir.path(), "api", "fake").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "hi");
    until(&mut sub, ended_turn(1)).await;
    let e = hub.compact_thread(&t, &user).unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST);
    assert!(e.message.contains("\"fake\""), "{}", e.message);
    // Nothing was started for either.
    assert_eq!(hub.get_thread(&c).unwrap()["turns"], 1);
    assert_eq!(hub.get_thread(&t).unwrap()["turns"], 1);
    hub.shutdown().await;
}

/// Runtime `bgc`: each prompt's turn yields with background task `b1` running (an interim
/// report); a compaction ends `b1` while it compacts; the hub's wake prompt is answered.
struct Bg;

impl Adapter for Bg {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "bgc",
            tier: Tier::Headless,
            description: "background work and compactions",
        }
    }

    fn compacts(&self) -> bool {
        true
    }

    fn spawn(&self, _cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let (h, mut end) = AdapterHandle::pair();
        tokio::spawn(async move {
            end.emit(
                EventBody::ThreadStarted(SessionPayload {
                    session_id: Some("s-bg".into()),
                    ..Default::default()
                }),
                None,
            );
            let text = |t: &str| {
                EventBody::AgentText(AgentText {
                    text: t.into(),
                    is_final: true,
                    ..Default::default()
                })
            };
            let task = |running: bool| {
                EventBody::TaskUpdated(TaskUpdated {
                    task_id: "b1".into(),
                    running,
                    description: "build".into(),
                    ..Default::default()
                })
            };
            let done = || EventBody::TurnEnded(TurnEnded::default());
            let mut first = prompt.map(|p| (p, 1));
            loop {
                let cmd = match first.take() {
                    Some((text, turn)) => AdapterCmd::Send { text, turn },
                    None => match end.cmds.recv().await {
                        Some(c) => c,
                        None => break,
                    },
                };
                match cmd {
                    AdapterCmd::Send { text: p, turn } if p.starts_with(WAKE_PROMPT_START) => {
                        end.emit(text("Handled."), Some(turn));
                        end.emit(done(), Some(turn));
                    }
                    AdapterCmd::Send { turn, .. } => {
                        end.emit(task(true), None);
                        end.emit(text("Building in the background."), Some(turn));
                        end.emit(done(), Some(turn));
                    }
                    AdapterCmd::Compact { turn } => {
                        end.emit(
                            EventBody::ContextCompacting(ContextCompacting::default()),
                            Some(turn),
                        );
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        end.emit(task(false), None);
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        end.emit(
                            EventBody::ContextCompacted(ContextCompacted {
                                trigger: "manual".into(),
                                ..Default::default()
                            }),
                            Some(turn),
                        );
                        end.emit(done(), Some(turn));
                    }
                    AdapterCmd::Stop => break,
                    _ => {}
                }
            }
            end.exited(Some(0), "");
        });
        Ok(h)
    }
}

/// Work that ends while the user's compaction runs is still the worker's to handle: the
/// compaction handles none of it, so the hub's wake comes after it, as it would have.
#[tokio::test]
async fn a_compaction_keeps_the_wake_of_the_work_it_did_not_handle() {
    let dir = tempfile::tempdir().unwrap();
    let mut o = opts(dir.path());
    o.wake_after = Some(Duration::from_millis(300));
    let bg: Arc<dyn Adapter> = Arc::new(Bg);
    let fallback = o.adapters.clone();
    o.adapters = Arc::new(move |name: &str| -> Option<Arc<dyn Adapter>> {
        match name {
            "bgc" => Some(Arc::clone(&bg)),
            other => fallback(other),
        }
    });
    let hub = Hub::open(o).await.unwrap();
    let t = new_thread(&hub, dir.path(), "api", "bgc").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "build it");
    until(&mut sub, |e| e.kind() == "report.ready").await;
    assert_eq!(
        hub.compact_thread(&t, &Caller::user("test")).unwrap()["state"],
        "started"
    );
    until(&mut sub, ended_turn(2)).await;
    let evs = until(
        &mut sub,
        |e| matches!(&e.body, EventBody::TurnStarted(p) if p.prompt.starts_with(WAKE_PROMPT_START)),
    )
    .await;
    assert_eq!(evs.last().unwrap().turn, Some(3));
    until(&mut sub, ended_turn(3)).await;
    hub.shutdown().await;
}

/// A compaction asked for while a turn runs, of a thread then moved to another runtime, is
/// dropped: the new runtime's first turn is a prompt with its handoff.
#[tokio::test]
async fn a_queued_compaction_is_dropped_when_the_thread_moves_to_another_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let t = new_thread(&hub, dir.path(), "api", "fakec").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "!sleep 400\none");
    until(&mut sub, |e| e.kind() == "turn.started").await;
    let user = Caller::user("test");
    assert_eq!(hub.compact_thread(&t, &user).unwrap()["state"], "queued");
    hub.set_runtime_model(&t, Some("fake"), None, None, &user)
        .unwrap();
    until(&mut sub, ended_turn(1)).await;
    send(&hub, &t, "next");
    let evs = until(&mut sub, |e| {
        e.kind() == "turn.started" && e.turn == Some(2)
    })
    .await;
    let EventBody::TurnStarted(p) = &evs.last().unwrap().body else {
        unreachable!()
    };
    assert!(!is_compact_turn(p), "dropped");
    assert_eq!(p.prompt, "next");
    assert!(
        p.ext.as_ref().unwrap()["twr"]["runtime_handoff"].is_string(),
        "with its handoff: {p:?}"
    );
    hub.shutdown().await;
}

/// A compaction that fails is not retried (as a work prompt or at all); it raises an error
/// item saying so, and the thread's work turns are as they were.
#[tokio::test]
async fn a_failed_compaction_is_not_retried() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let t = new_thread(&hub, dir.path(), "api", "fakec").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "fine");
    until(&mut sub, ended_turn(1)).await;
    // The session's next turn fails as transient whatever it is (`!transient 2`: this one
    // and the next): turn 2, its retry turn 3, then the compaction, turn 4, fine.
    send(&hub, &t, "!transient 3");
    until(&mut sub, ended_turn(3)).await;
    hub.compact_thread(&t, &Caller::user("test")).unwrap();
    let evs = until(&mut sub, ended_turn(4)).await;
    assert!(matches!(&evs.last().unwrap().body,
        EventBody::TurnEnded(p) if p.result == TurnResult::Error));
    let evs = until(&mut sub, |e| {
        matches!(&e.body, EventBody::AttentionRaised(a)
            if a.summary.contains("compacting its context failed"))
    })
    .await;
    assert_eq!(evs.last().unwrap().turn, Some(4));
    tokio::time::sleep(Duration::from_millis(500)).await;
    let v = hub.thread_view(&t).unwrap();
    assert_eq!(v.turns, 4, "nothing retried");
    assert_eq!(v.last_turn.as_ref().map(|l| l.turn), Some(3));
    hub.shutdown().await;
}

/// A compaction whose start cannot be recorded stays asked for: the RPC says `queued`, and
/// the next dispatch starts it, before the message sent after it.
#[tokio::test]
async fn a_compaction_whose_start_fails_to_record_stays_queued() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let t = new_thread(&hub, dir.path(), "api", "fakec").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "fine");
    until(&mut sub, ended_turn(1)).await;
    let db = dir.path().join("data").join("twr.db");
    let c = rusqlite::Connection::open(&db).unwrap();
    c.busy_timeout(Duration::from_secs(5)).unwrap();
    c.execute_batch(
        "CREATE TRIGGER inject BEFORE INSERT ON events
         WHEN NEW.kind = 'turn.started' AND NEW.data LIKE '%\"compact\":true%'
         BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;",
    )
    .unwrap();
    let r = hub.compact_thread(&t, &Caller::user("test")).unwrap();
    assert_eq!(r["state"], "queued");
    c.execute_batch("DROP TRIGGER inject").unwrap();
    send(&hub, &t, "after it");
    let evs = until(&mut sub, ended_turn(3)).await;
    let starts: Vec<&TurnStarted> = evs
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::TurnStarted(p) => Some(p),
            _ => None,
        })
        .collect();
    assert!(is_compact_turn(starts[0]), "{starts:?}");
    assert_eq!(starts[1].prompt, "after it");
    hub.shutdown().await;
}

/// A prompt from anyone but the user that is a compaction command is refused wherever it is
/// queued: a send, a thread's first prompt, a send held for other threads.
#[tokio::test]
async fn a_compact_command_is_the_users_only() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let t = new_thread(&hub, dir.path(), "api", "fakec").await;
    let orch = Caller::Orchestrator;
    let e = hub.send_by(&t, " /Compact now", &orch).unwrap_err();
    assert_eq!(e.code, codes::UNAUTHORIZED);
    let e = hub
        .send(&t, "/compact", TurnFrom::Runtime, Source::Hub)
        .unwrap_err();
    assert_eq!(e.code, codes::UNAUTHORIZED);
    let e = hub
        .send_after_by(&t, "/compact", &["api".to_string()], &orch)
        .unwrap_err();
    assert_eq!(e.code, codes::UNAUTHORIZED);
    let e = hub
        .new_thread_by(
            NewThread {
                project: dir.path().to_string_lossy().into_owned(),
                runtime: Some("fakec".into()),
                name: Some("web".into()),
                first_prompt: Some("/compact".into()),
                ..Default::default()
            },
            &orch,
        )
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::UNAUTHORIZED);
    // The user's own goes the compaction's way instead: refused, saying which.
    let e = hub
        .send_by(&t, "/compact", &Caller::user("test"))
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST);
    assert!(e.message.contains("twr compact"), "{}", e.message);
    hub.shutdown().await;
}

/// A compaction is no turn of work for a prompt held for other threads (`send --after`): a
/// send held on a card because its thread failed stays held when that thread is compacted.
#[tokio::test]
async fn a_compaction_finishes_no_turn_a_held_send_waits_for() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let api = new_thread(&hub, dir.path(), "api", "fakec").await;
    let web = new_thread(&hub, dir.path(), "web", "fakec").await;
    let mut sub = hub.subscribe_thread(&web, 0).unwrap();
    send(&hub, &web, "!sleep 300\n!error broke");
    until(&mut sub, |e| e.kind() == "turn.started").await;
    let user = Caller::user("test");
    hub.send_after_by(&api, "go", std::slice::from_ref(&web), &user)
        .unwrap();
    until(&mut sub, ended_turn(1)).await;
    // Its card: the thread finished otherwise than well.
    timeout(Duration::from_secs(20), async {
        while !hub
            .attention_list(true)
            .iter()
            .any(|a| a["summary"].as_str().is_some_and(|s| s.contains("go")))
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the held send's card");
    hub.compact_thread(&web, &user).unwrap();
    until(&mut sub, ended_turn(2)).await;
    // Past the gate's grace (5 s of quiet): still held.
    tokio::time::sleep(Duration::from_millis(6500)).await;
    assert_eq!(
        hub.thread_view(&api).unwrap().turns,
        0,
        "the send is still held"
    );
    hub.shutdown().await;
}

/// Runtime `failc`: prompts end at once; a compaction reports it failed (Grok Build's
/// `auto_compact_failed`) and its turn ends `completed`.
struct FailingCompaction;

impl Adapter for FailingCompaction {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "failc",
            tier: Tier::Headless,
            description: "compactions that fail",
        }
    }

    fn compacts(&self) -> bool {
        true
    }

    fn spawn(&self, _cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let (h, mut end) = AdapterHandle::pair();
        tokio::spawn(async move {
            end.emit(
                EventBody::ThreadStarted(SessionPayload {
                    session_id: Some("s-fail".into()),
                    ..Default::default()
                }),
                None,
            );
            let done = || EventBody::TurnEnded(TurnEnded::default());
            if prompt.is_some() {
                end.emit(done(), Some(1));
            }
            while let Some(cmd) = end.cmds.recv().await {
                match cmd {
                    AdapterCmd::Send { turn, .. } => {
                        end.emit(done(), Some(turn));
                    }
                    AdapterCmd::Compact { turn } => {
                        end.emit(
                            EventBody::ContextCompacted(ContextCompacted {
                                trigger: "manual".into(),
                                error: Some("conversation too short".into()),
                                ..Default::default()
                            }),
                            Some(turn),
                        );
                        end.emit(done(), Some(turn));
                    }
                    AdapterCmd::Stop => break,
                    _ => {}
                }
            }
            end.exited(Some(0), "");
        });
        Ok(h)
    }
}

/// A compaction that failed by its own report raises the error card, though the runtime
/// ended its turn `completed`.
#[tokio::test]
async fn a_compaction_that_says_it_failed_raises_its_error_card() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let t = new_thread(&hub, dir.path(), "api", "failc").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "hi");
    until(&mut sub, ended_turn(1)).await;
    hub.compact_thread(&t, &Caller::user("test")).unwrap();
    let evs = until(&mut sub, |e| {
        matches!(&e.body, EventBody::AttentionRaised(a)
            if a.reason == AttentionReason::Error
            && a.summary.contains("compacting its context failed: conversation too short"))
    })
    .await;
    assert_eq!(evs.last().unwrap().turn, Some(2));
    hub.shutdown().await;
}

/// A compaction asked for behind a queued message, of a thread then moved to another runtime
/// that compacts too: the message starts the new runtime's session (with the handoff), and
/// the compaction, asked of the old session, is dropped.
#[tokio::test]
async fn a_queued_compaction_of_a_session_since_replaced_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let t = new_thread(&hub, dir.path(), "api", "fakec").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "!sleep 400\none");
    until(&mut sub, |e| e.kind() == "turn.started").await;
    send(&hub, &t, "two");
    let user = Caller::user("test");
    assert_eq!(hub.compact_thread(&t, &user).unwrap()["state"], "queued");
    hub.set_runtime_model(&t, Some("fakec2"), None, None, &user)
        .unwrap();
    let evs = until(&mut sub, ended_turn(2)).await;
    let two = evs
        .iter()
        .find_map(|e| match &e.body {
            EventBody::TurnStarted(p) if e.turn == Some(2) => Some(p.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(two.prompt, "two");
    assert!(two.ext.as_ref().unwrap()["twr"]["runtime_handoff"].is_string());
    tokio::time::sleep(Duration::from_millis(500)).await;
    let v = hub.thread_view(&t).unwrap();
    assert_eq!(
        (v.turns, v.current_turn),
        (2, None),
        "no compaction after it"
    );
    hub.shutdown().await;
}

/// A compaction has no report, and none can be written for it: `report.write` of its turn is
/// refused, and nothing is recorded.
#[tokio::test]
async fn no_report_is_written_for_a_compaction() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let t = new_thread(&hub, dir.path(), "api", "fakec").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "fine");
    until(&mut sub, ended_turn(1)).await;
    hub.compact_thread(&t, &Caller::user("test")).unwrap();
    until(&mut sub, ended_turn(2)).await;
    for caller in [Caller::Orchestrator, Caller::user("test")] {
        let e = hub
            .write_report(&t, Some(2), "Compacted.", Grade::Success, &caller)
            .unwrap_err();
        assert_eq!(e.code, codes::BAD_REQUEST);
    }
    let (_, evs) = hub.thread_events(&t, 0, 10_000).unwrap();
    assert!(!evs
        .iter()
        .any(|e| e.turn == Some(2) && matches!(e.kind(), "report.ready" | "decision.made")));
    // Its default turn is the work turn's.
    hub.write_report(&t, None, "Fine.", Grade::Success, &Caller::user("test"))
        .unwrap();
    let v = hub.thread_view(&t).unwrap();
    assert_eq!(v.last_report.as_ref().and_then(|r| r.turn), Some(1));
    hub.shutdown().await;
}

/// Runtime `slowc`: prompts end at once; a compaction takes 1.5 s.
struct SlowCompaction;

impl Adapter for SlowCompaction {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "slowc",
            tier: Tier::Headless,
            description: "slow compactions",
        }
    }

    fn compacts(&self) -> bool {
        true
    }

    fn spawn(&self, _cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let (h, mut end) = AdapterHandle::pair();
        tokio::spawn(async move {
            end.emit(
                EventBody::ThreadStarted(SessionPayload {
                    session_id: Some("s-slow".into()),
                    ..Default::default()
                }),
                None,
            );
            let done = || EventBody::TurnEnded(TurnEnded::default());
            if prompt.is_some() {
                end.emit(done(), Some(1));
            }
            while let Some(cmd) = end.cmds.recv().await {
                match cmd {
                    AdapterCmd::Send { turn, .. } => {
                        end.emit(done(), Some(turn));
                    }
                    AdapterCmd::Compact { turn } => {
                        end.emit(
                            EventBody::ContextCompacting(ContextCompacting::default()),
                            Some(turn),
                        );
                        tokio::time::sleep(Duration::from_millis(1500)).await;
                        end.emit(
                            EventBody::ContextCompacted(ContextCompacted {
                                trigger: "manual".into(),
                                ..Default::default()
                            }),
                            Some(turn),
                        );
                        end.emit(done(), Some(turn));
                    }
                    AdapterCmd::Stop => break,
                    _ => {}
                }
            }
            end.exited(Some(0), "");
        });
        Ok(h)
    }
}

/// A report of the work turn recorded while a compaction runs (here the user's, written
/// when the hub's own could not be recorded) covers that turn once the compaction ends: the
/// thread is reported, with that report, live and after a restart.
#[tokio::test]
async fn a_report_recorded_during_a_compaction_covers_the_work_turn() {
    let dir = tempfile::tempdir().unwrap();
    let mut o = opts(dir.path());
    let fallback = o.adapters.clone();
    o.adapters = Arc::new(move |name: &str| -> Option<Arc<dyn Adapter>> {
        match name {
            "slowc" => Some(Arc::new(SlowCompaction)),
            other => fallback(other),
        }
    });
    let hub = Hub::open(o).await.unwrap();
    let t = new_thread(&hub, dir.path(), "api", "slowc").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    let db = dir.path().join("data").join("twr.db");
    let c = rusqlite::Connection::open(&db).unwrap();
    c.busy_timeout(Duration::from_secs(5)).unwrap();
    c.execute_batch(
        "CREATE TRIGGER inject BEFORE INSERT ON events
         WHEN NEW.kind = 'report.ready' AND NEW.turn = 1
         BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;",
    )
    .unwrap();
    send(&hub, &t, "fine");
    until(&mut sub, ended_turn(1)).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!hub.thread_view(&t).unwrap().reported);
    c.execute_batch("DROP TRIGGER inject").unwrap();
    let user = Caller::user("test");
    hub.compact_thread(&t, &user).unwrap();
    until(&mut sub, |e| e.kind() == "context.compacting").await;
    hub.write_report(&t, Some(1), "Built it.", Grade::Success, &user)
        .unwrap();
    until(&mut sub, ended_turn(2)).await;
    let reported = |hub: &Hub| {
        let v = hub.thread_view(&t).unwrap();
        (v.reported, v.last_report.as_ref().and_then(|r| r.turn))
    };
    assert_eq!(reported(&hub), (true, Some(1)), "live");
    hub.shutdown().await;
    drop((sub, hub));
    let hub = timeout(WAIT, async {
        loop {
            match Hub::open(opts(dir.path())).await {
                Ok(h) => return h,
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await
    .expect("reopening the hub");
    assert_eq!(reported(&hub), (true, Some(1)), "replayed");
    hub.shutdown().await;
}
