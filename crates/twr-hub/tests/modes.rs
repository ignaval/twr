//! `thread.set_mode`: `thread.mode_changed` and the restart that applies it (`hub::mode`).
//! The fake runtime reports the mode it was started with (`thread.started` `ext.fake.mode`).

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::time::timeout;
use twr_adapters::fake::FakeAdapter;
use twr_core::adapter::{
    Adapter, AdapterCmd, AdapterHandle, AdapterInfo, Answer, SpawnConfig, Tier,
};
use twr_core::config::Config;
use twr_core::event::*;
use twr_core::proto::codes;
use twr_hub::hub::ThreadSubscription;
use twr_hub::{Caller, Hub, HubOptions, NewThread};

const WAIT: Duration = Duration::from_secs(10);

/// Runtime `late`: every session reports its mode (`ext.fake.mode`) and ends each turn at
/// once; its first session then raises an error item between turns (`e1`, no turn).
struct LateError(Arc<AtomicUsize>);

impl Adapter for LateError {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "late",
            tier: Tier::Headless,
            description: "raises an error between turns",
        }
    }

    fn spawn(&self, cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let (h, mut end) = AdapterHandle::pair();
        let first = self.0.fetch_add(1, Ordering::SeqCst) == 0;
        tokio::spawn(async move {
            let mut ext = JsonMap::new();
            ext.insert("fake".into(), serde_json::json!({ "mode": cfg.mode }));
            end.emit(
                EventBody::ThreadStarted(SessionPayload {
                    session_id: Some(cfg.resume_session.clone().unwrap_or("s-late".into())),
                    ext: Some(ext),
                    ..Default::default()
                }),
                None,
            );
            let ended = || {
                EventBody::TurnEnded(TurnEnded {
                    result: TurnResult::Completed,
                    ..Default::default()
                })
            };
            if prompt.is_some() {
                end.emit(ended(), Some(cfg.prompt_turn.unwrap_or(1)));
            }
            if first {
                end.emit(
                    EventBody::AttentionRaised(AttentionRaised {
                        attn_id: "e1".into(),
                        reason: AttentionReason::Error,
                        summary: "rate limited".into(),
                        ..Default::default()
                    }),
                    None,
                );
            }
            while let Some(cmd) = end.cmds.recv().await {
                match cmd {
                    AdapterCmd::Send { turn, .. } => {
                        end.emit(ended(), Some(turn));
                    }
                    AdapterCmd::Stop | AdapterCmd::Interrupt => break,
                    _ => {}
                }
            }
            end.exited(Some(0), "");
        });
        Ok(h)
    }
}

fn opts(dir: &Path) -> HubOptions {
    let mut o = HubOptions::new(dir.join("data").join("twr.db"), Config::default());
    let late: Arc<dyn Adapter> = Arc::new(LateError(Arc::new(AtomicUsize::new(0))));
    o.adapters = Arc::new(move |name: &str| -> Option<Arc<dyn Adapter>> {
        match name {
            "fake" => Some(Arc::new(FakeAdapter::new()) as Arc<dyn Adapter>),
            "late" => Some(Arc::clone(&late)),
            _ => None,
        }
    });
    o.stop_timeout = Duration::from_secs(5);
    o
}

fn user() -> Caller {
    Caller::user("test")
}

async fn new_thread(hub: &Hub, dir: &Path, name: &str, mode: Option<&str>) -> String {
    new_thread_on(hub, dir, name, mode, "fake").await
}

async fn new_thread_on(
    hub: &Hub,
    dir: &Path,
    name: &str,
    mode: Option<&str>,
    runtime: &str,
) -> String {
    let v = hub
        .new_thread(
            NewThread {
                project: dir.to_string_lossy().into_owned(),
                runtime: Some(runtime.into()),
                name: Some(name.into()),
                mode: mode.map(str::to_string),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap();
    v["id"].as_str().unwrap().to_string()
}

fn send(hub: &Hub, thread: &str, prompt: &str) -> twr_hub::SendOutcome {
    hub.send(thread, prompt, TurnFrom::User, Source::User("test".into()))
        .unwrap()
}

/// Reads the thread's events until one matches `pred`; returns everything read.
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
        let kinds: Vec<&str> = seen.iter().map(|e: &Event| e.kind()).collect();
        panic!("timed out; saw {kinds:?}")
    });
    seen
}

/// The mode a fake `thread.started` says its process runs in.
fn started_in(ev: &Event) -> Option<String> {
    match &ev.body {
        EventBody::ThreadStarted(p) => p.ext.as_ref()?["fake"]["mode"].as_str().map(str::to_string),
        _ => None,
    }
}

fn is_report(ev: &Event) -> bool {
    matches!(ev.body, EventBody::ReportReady(_))
}

fn is_mode_restart_stop(ev: &Event) -> bool {
    matches!(&ev.body, EventBody::ThreadStopped(p) if is_settings_restart(p))
}

/// `auto` is twr's default mode: a thread made without `--mode` runs in it.
#[tokio::test]
async fn threads_default_to_auto() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let t = new_thread(&hub, dir.path(), "api", None).await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "hi");
    let evs = until(&mut sub, is_report).await;
    let started = evs.iter().find_map(started_in);
    assert_eq!(started.as_deref(), Some("auto"));
    assert_eq!(hub.thread_view(&t).unwrap().mode, "auto");
    // The hub records the mode it started the runtime in.
    let stamped = evs.iter().find_map(|e| match &e.body {
        EventBody::ThreadStarted(p) => p.ext.as_ref()?["twr"]["mode"].as_str().map(String::from),
        _ => None,
    });
    assert_eq!(stamped.as_deref(), Some("auto"));
}

/// An idle live runtime restarts at once on its session, in the new mode; its status and
/// session are unchanged.
#[tokio::test]
async fn an_idle_runtime_restarts_in_the_new_mode() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let t = new_thread(&hub, dir.path(), "api", Some("manual")).await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "hi");
    let evs = until(&mut sub, is_report).await;
    assert_eq!(evs.iter().find_map(started_in).as_deref(), Some("manual"));
    let before = hub.thread_view(&t).unwrap();
    let r = hub.set_mode("api", "Read_Only", &user()).unwrap();
    assert_eq!(r["mode"], "read-only", "{r}");
    assert_eq!(r["previous"], "manual");
    assert_eq!(r["changed"], true);
    assert_eq!(r["applies"], "now");
    let evs = until(&mut sub, |e| started_in(e).is_some()).await;
    let kinds: Vec<&str> = evs.iter().map(|e| e.kind()).collect();
    assert_eq!(
        kinds,
        ["thread.mode_changed", "thread.stopped", "thread.started"],
        "{evs:?}"
    );
    let EventBody::ThreadModeChanged(m) = &evs[0].body else {
        panic!()
    };
    assert_eq!(
        (m.mode.as_str(), m.previous.as_str(), m.by),
        ("read-only", "manual", ChangedBy::User)
    );
    assert_eq!(evs[0].source, Source::User("test".into()));
    assert!(is_mode_restart_stop(&evs[1]), "{:?}", evs[1]);
    assert_eq!(started_in(&evs[2]).as_deref(), Some("read-only"));
    let EventBody::ThreadStarted(p) = &evs[2].body else {
        panic!()
    };
    assert_eq!(p.session_id, before.session_id, "the same session");
    assert_eq!(p.ext.as_ref().unwrap()["fake"]["resumed"], true);
    let after = hub.thread_view(&t).unwrap();
    assert_eq!(after.mode, "read-only");
    assert!(!after.stopped);
    assert_eq!(after.status(), before.status());
    assert_eq!(after.session_id, before.session_id);
    // Nothing was raised for the user.
    assert!(hub.attention_list(true).is_empty());
    // The next prompt runs in the new process.
    send(&hub, &t, "again");
    let evs = until(&mut sub, is_report).await;
    assert!(evs.iter().all(|e| started_in(e).is_none()), "no new start");
    // Setting the same mode again changes nothing and says so; the user's explicit choice is
    // recorded (`mode == previous`, it ends an earlier message's authority over the mode,
    // `hub::orch`), and the runtime is not restarted for it.
    let head = hub.control_read(0, 1).unwrap().1;
    let r = hub.set_mode("api", "read-only", &user()).unwrap();
    assert_eq!(r["changed"], false, "{r}");
    assert!(r["message"].as_str().unwrap().contains("already"), "{r}");
    assert_eq!(hub.control_read(0, 1).unwrap().1, head + 1);
    let evs = until(&mut sub, |e| e.kind() == "thread.mode_changed").await;
    let EventBody::ThreadModeChanged(m) = &evs.last().unwrap().body else {
        panic!()
    };
    assert_eq!(
        (m.mode.as_str(), m.previous.as_str(), m.by),
        ("read-only", "read-only", ChangedBy::User)
    );
    send(&hub, &t, "once more");
    let evs = until(&mut sub, is_report).await;
    assert!(
        evs.iter()
            .all(|e| started_in(e).is_none() && !is_mode_restart_stop(e)),
        "no restart: {evs:?}"
    );
    assert_eq!(hub.thread_view(&t).unwrap().mode, "read-only");
}

/// Mid-turn: the turn (and the permission it waits on) stays with the old process; once it
/// ends, the runtime restarts before the queued prompt, which runs in the new mode.
#[tokio::test]
async fn a_running_turn_finishes_before_the_restart() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let t = new_thread(&hub, dir.path(), "api", Some("manual")).await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "!perm Bash ls");
    let evs = until(&mut sub, |e| {
        matches!(e.body, EventBody::AttentionRaised(_))
    })
    .await;
    let attn = evs
        .iter()
        .find_map(|e| match &e.body {
            EventBody::AttentionRaised(p) => Some(p.attn_id.clone()),
            _ => None,
        })
        .unwrap();
    let r = hub.set_mode("api", "auto", &user()).unwrap();
    assert_eq!(r["applies"], "after_turn", "{r}");
    let listed = hub.get_thread("api").unwrap();
    assert_eq!(listed["mode"], "auto");
    assert_eq!(listed["runtime_mode"], "manual", "{listed}");
    // A prompt sent meanwhile waits for the restart.
    let sent = send(&hub, &t, "second");
    assert_eq!(sent.turn, None);
    // The pending permission is still the old process's: answering it finishes turn 1 there.
    hub.respond(&attn, Answer::allow(), "test").unwrap();
    let evs = until(
        &mut sub,
        |e| matches!(&e.body, EventBody::TurnStarted(p) if p.prompt == "second"),
    )
    .await;
    let pos = |pred: &dyn Fn(&Event) -> bool| evs.iter().position(pred).unwrap();
    let tool_ok = pos(&|e| matches!(&e.body, EventBody::AgentToolResult(r) if r.ok));
    let turn1_end = pos(&|e| matches!(e.body, EventBody::TurnEnded(_)) && e.turn == Some(1));
    let stop = pos(&is_mode_restart_stop);
    let second = evs.len() - 1;
    assert!(
        tool_ok < turn1_end && turn1_end < stop && stop < second,
        "{evs:?}"
    );
    // The queued prompt starts the new process, in the new mode.
    let evs = until(&mut sub, |e| started_in(e).is_some()).await;
    assert_eq!(evs.iter().find_map(started_in).as_deref(), Some("auto"));
    until(&mut sub, is_report).await;
    let v = hub.thread_view(&t).unwrap();
    assert_eq!((v.mode.as_str(), v.turns, v.stopped), ("auto", 2, false));
    assert!(hub.get_thread("api").unwrap().get("runtime_mode").is_none());
}

/// A parked thread keeps the new mode for its resume; a stop during a pending restart wins.
#[tokio::test]
async fn parked_and_stopped_threads() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let t = new_thread(&hub, dir.path(), "api", Some("manual")).await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "hi");
    until(&mut sub, is_report).await;
    hub.park_thread("api", false, false, &user()).await.unwrap();
    let r = hub.set_mode("api", "plan", &user()).unwrap();
    assert_eq!(r["applies"], "next_start", "{r}");
    hub.resume_thread("api", &user()).await.unwrap();
    send(&hub, &t, "after resume");
    let evs = until(&mut sub, |e| started_in(e).is_some()).await;
    assert_eq!(evs.iter().find_map(started_in).as_deref(), Some("plan"));
    until(&mut sub, is_report).await;

    // A restart waiting for the turn to end, then a stop: the thread stays stopped.
    send(&hub, &t, "!sleep 300");
    until(&mut sub, |e| matches!(e.body, EventBody::TurnStarted(_))).await;
    let r = hub.set_mode("api", "auto", &user()).unwrap();
    assert_eq!(r["applies"], "after_turn", "{r}");
    hub.stop_thread_by("api", &user()).unwrap();
    let evs = until(&mut sub, |e| matches!(e.body, EventBody::ThreadStopped(_))).await;
    assert!(!is_mode_restart_stop(evs.last().unwrap()));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let v = hub.thread_view(&t).unwrap();
    assert!(v.stopped && v.mode == "auto", "{v:?}");
    assert!(!hub.get_thread("api").unwrap()["live"].as_bool().unwrap());
}

/// The mode is in the log: a reopened hub starts the runtime in it.
#[tokio::test]
async fn the_mode_survives_a_hub_restart() {
    let dir = tempfile::tempdir().unwrap();
    let t = {
        let hub = Hub::open(opts(dir.path())).await.unwrap();
        let t = new_thread(&hub, dir.path(), "api", Some("manual")).await;
        let r = hub.set_mode("api", "acceptEdits", &user()).unwrap();
        assert_eq!(r["applies"], "next_start", "{r}");
        hub.shutdown().await;
        t
    };
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    assert_eq!(hub.thread_view(&t).unwrap().mode, "acceptEdits");
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "hi");
    let evs = until(&mut sub, |e| started_in(e).is_some()).await;
    assert_eq!(
        evs.iter().find_map(started_in).as_deref(),
        Some("acceptEdits")
    );
}

/// Bad requests: an empty mode, an unknown thread.
#[tokio::test]
async fn bad_mode_requests() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    new_thread(&hub, dir.path(), "api", None).await;
    let e = hub.set_mode("api", "  ", &user()).unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST, "{e}");
    let e = hub.set_mode("nope", "auto", &user()).unwrap_err();
    assert_eq!(e.code, codes::NOT_FOUND, "{e}");
    let v: Value = hub.get_thread("api").unwrap();
    assert_eq!(v["mode"], "auto");
}

/// An item the runtime raised between turns (an error here) is the old process's: a restart
/// that widens the mode waits until it is answered (it is never resolved `thread_stopped` by the restart), and
/// nothing runs in the old mode meanwhile; answering it restarts the runtime and the queued
/// prompt runs in the new mode. (A narrowing one does not wait: `hub::restart`.)
#[tokio::test]
async fn an_item_raised_between_turns_defers_the_restart() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let t = new_thread_on(&hub, dir.path(), "api", Some("plan"), "late").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "hi");
    let evs = until(
        &mut sub,
        |e| matches!(&e.body, EventBody::AttentionRaised(a) if a.reason == AttentionReason::Error),
    )
    .await;
    assert_eq!(evs.iter().find_map(started_in).as_deref(), Some("plan"));
    let attn = hub.attention_list(true)[0]["attn_id"]
        .as_str()
        .unwrap()
        .to_string();
    let r = hub.set_mode("api", "auto", &user()).unwrap();
    assert_eq!(r["applies"], "after_turn", "{r}");
    // A prompt waits: it would run in the old mode.
    let sent = send(&hub, &t, "second");
    assert_eq!(sent.turn, None);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let v = hub.thread_view(&t).unwrap();
    assert_eq!(v.turns, 1, "{v:?}");
    assert!(hub
        .attention_list(true)
        .iter()
        .any(|i| i["attn_id"] == attn.as_str()));
    let (_, log) = hub.thread_events(&t, 0, 1000).unwrap();
    assert!(!log.iter().any(is_mode_restart_stop), "restarted too early");
    // Answering it lets the restart go ahead; the queued prompt runs in the new process.
    hub.respond(&attn, Answer::new("ok"), "test").unwrap();
    let evs = until(
        &mut sub,
        |e| matches!(&e.body, EventBody::TurnStarted(p) if p.prompt == "second"),
    )
    .await;
    let resolved = evs.iter().find_map(|e| match &e.body {
        EventBody::AttentionResolved(p) if p.attn_id == attn => Some(p.by),
        _ => None,
    });
    assert_eq!(resolved, Some(ResolvedBy::User));
    assert!(evs.iter().any(is_mode_restart_stop), "{evs:?}");
    let evs = until(&mut sub, |e| started_in(e).is_some()).await;
    assert_eq!(evs.iter().find_map(started_in).as_deref(), Some("auto"));
    let v = hub.thread_view(&t).unwrap();
    assert_eq!((v.mode.as_str(), v.stopped), ("auto", false));
}
