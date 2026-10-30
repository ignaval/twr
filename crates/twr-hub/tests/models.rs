//! `thread.set_model` and `thread.new` `effort`: `thread.model_changed` and the restart that
//! applies it (`hub::model`, `hub::restart`). The fake runtime reports the model and effort it
//! was started with (`thread.started` `ext.fake.model` / `ext.fake.effort`).

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::time::timeout;
use twr_adapters::fake::FakeAdapter;
use twr_core::adapter::{
    Adapter, AdapterCmd, AdapterHandle, AdapterInfo, Answer, CodexAuto, SpawnConfig, Tier,
};
use twr_core::config::Config;
use twr_core::event::*;
use twr_core::proto::codes;
use twr_hub::hub::ThreadSubscription;
use twr_hub::{Caller, Hub, HubOptions, NewThread};

const WAIT: Duration = Duration::from_secs(10);

/// Runtime `late`: every session reports its effort (`ext.fake.effort`) and ends each turn at
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
            ext.insert("fake".into(), json!({ "effort": cfg.effort }));
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
            // Never spawned here: threads of these only check what they take.
            "codex" => Some(Arc::new(twr_adapters::codex::CodexAdapter::new())),
            "claude-code" => Some(Arc::new(twr_adapters::ClaudeCodeAdapter::new())),
            "pty" => Some(Arc::new(twr_adapters::PtyAdapter::new())),
            _ => None,
        }
    });
    o.stop_timeout = Duration::from_secs(5);
    o
}

fn user() -> Caller {
    Caller::user("test")
}

fn new_req(dir: &Path, name: &str, runtime: &str) -> NewThread {
    NewThread {
        project: dir.to_string_lossy().into_owned(),
        runtime: Some(runtime.into()),
        name: Some(name.into()),
        ..Default::default()
    }
}

async fn new_thread(hub: &Hub, dir: &Path, name: &str, runtime: &str) -> String {
    let v = hub
        .new_thread(new_req(dir, name, runtime), "test")
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

/// What a fake `thread.started` says its process runs with: `(model, effort)`.
fn started_with(ev: &Event) -> Option<(Value, Value)> {
    match &ev.body {
        EventBody::ThreadStarted(p) => {
            let f = &p.ext.as_ref()?["fake"];
            Some((f["model"].clone(), f["effort"].clone()))
        }
        _ => None,
    }
}

/// `[codex] auto` reaches every worker's runtime (`SpawnConfig::codex_auto`, reported by the
/// fake as `ext.fake.codex_auto`), and a configured project's `codex_auto` wins for the
/// threads made for that project; a thread of a folder that is no configured project gets
/// the global one.
#[tokio::test]
async fn threads_start_with_their_codex_auto_setting() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("api");
    std::fs::create_dir(&repo).unwrap();
    let mut o = opts(dir.path());
    o.config.codex.auto = CodexAuto::Never;
    o.config.projects.insert(
        "api".into(),
        twr_core::config::ProjectConfig {
            repo: repo.clone(),
            codex_auto: Some(CodexAuto::AutoReview),
            ..Default::default()
        },
    );
    let hub = Hub::open(o).await.unwrap();
    let plain = new_thread(&hub, dir.path(), "plain", "fake").await;
    let api = hub
        .new_thread(
            NewThread {
                project: "api".into(),
                runtime: Some("fake".into()),
                name: Some("api".into()),
                no_worktree: true,
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    for (t, want) in [(plain, "never"), (api, "auto_review")] {
        let mut sub = hub.subscribe_thread(&t, 0).unwrap();
        send(&hub, &t, "hi");
        let evs = until(&mut sub, is_report).await;
        let got = evs.iter().find_map(|e| match &e.body {
            EventBody::ThreadStarted(p) => Some(p.ext.as_ref()?["fake"]["codex_auto"].clone()),
            _ => None,
        });
        assert_eq!(got, Some(json!(want)), "{t}");
    }
}

/// With nothing in `[codex]`, a worker's runtime gets `auto_review` (Codex's reviewer
/// answers what its `auto` sandbox does not allow), and a configured project's `codex_auto =
/// "ask"` brings the user's cards back for that project's threads.
#[tokio::test]
async fn threads_start_with_auto_review_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("api");
    std::fs::create_dir(&repo).unwrap();
    let mut o = opts(dir.path());
    assert_eq!(o.config.codex, twr_core::config::CodexConfig::default());
    o.config.projects.insert(
        "api".into(),
        twr_core::config::ProjectConfig {
            repo: repo.clone(),
            codex_auto: Some(CodexAuto::Ask),
            ..Default::default()
        },
    );
    let hub = Hub::open(o).await.unwrap();
    let plain = new_thread(&hub, dir.path(), "plain", "fake").await;
    let api = hub
        .new_thread(
            NewThread {
                project: "api".into(),
                runtime: Some("fake".into()),
                name: Some("api".into()),
                no_worktree: true,
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    for (t, want) in [(plain, "auto_review"), (api, "ask")] {
        let mut sub = hub.subscribe_thread(&t, 0).unwrap();
        send(&hub, &t, "hi");
        let evs = until(&mut sub, is_report).await;
        let got = evs.iter().find_map(|e| match &e.body {
            EventBody::ThreadStarted(p) => Some(p.ext.as_ref()?["fake"]["codex_auto"].clone()),
            _ => None,
        });
        assert_eq!(got, Some(json!(want)), "{t}");
    }
}

fn is_report(ev: &Event) -> bool {
    matches!(ev.body, EventBody::ReportReady(_))
}

/// The `ext.twr.reason` of a settings restart's `thread.stopped`.
fn restart_reason(ev: &Event) -> Option<String> {
    match &ev.body {
        EventBody::ThreadStopped(p) if is_settings_restart(p) => p.ext.as_ref()?["twr"]["reason"]
            .as_str()
            .map(str::to_string),
        _ => None,
    }
}

/// An idle live runtime restarts at once on its session with the new model and effort (a
/// `model_change` restart: the thread is not stopped); setting them again records nothing;
/// the orchestrator's change is applied directly and recorded as its own.
#[tokio::test]
async fn an_idle_runtime_restarts_with_the_new_model_and_effort() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let t = new_thread(&hub, dir.path(), "web", "fake").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "hi");
    let evs = until(&mut sub, is_report).await;
    assert_eq!(
        evs.iter().find_map(started_with),
        Some((json!(twr_adapters::fake::DEFAULT_MODEL), Value::Null))
    );
    let before = hub.thread_view(&t).unwrap();
    assert_eq!(
        (before.model.as_deref(), before.effort.as_deref()),
        (None, None)
    );
    let r = hub
        .set_model("web", Some("fake-big"), Some(" deep "), &user())
        .unwrap();
    assert_eq!(r["model"], "fake-big", "{r}");
    assert_eq!(r["effort"], "deep", "{r}");
    assert!(
        r["previous_model"].is_null() && r["previous_effort"].is_null(),
        "{r}"
    );
    assert_eq!(
        (r["changed"].clone(), r["applies"].clone()),
        (json!(true), json!("now"))
    );
    let evs = until(&mut sub, |e| started_with(e).is_some()).await;
    let kinds: Vec<&str> = evs.iter().map(|e| e.kind()).collect();
    assert_eq!(
        kinds,
        ["thread.model_changed", "thread.stopped", "thread.started"],
        "{evs:?}"
    );
    let EventBody::ThreadModelChanged(m) = &evs[0].body else {
        panic!()
    };
    assert_eq!(
        (m.model.as_deref(), m.effort.as_deref(), m.by),
        (Some("fake-big"), Some("deep"), ChangedBy::User)
    );
    assert_eq!(
        (m.previous_model.as_deref(), m.previous_effort.as_deref()),
        (None, None)
    );
    assert_eq!(evs[0].source, Source::User("test".into()));
    assert_eq!(
        restart_reason(&evs[1]).as_deref(),
        Some(MODEL_RESTART_REASON)
    );
    assert_eq!(
        started_with(&evs[2]),
        Some((json!("fake-big"), json!("deep")))
    );
    let EventBody::ThreadStarted(p) = &evs[2].body else {
        panic!()
    };
    assert_eq!(p.session_id, before.session_id, "the same session");
    let twr = &p.ext.as_ref().unwrap()["twr"];
    assert_eq!(
        (&twr["model"], &twr["effort"]),
        (&json!("fake-big"), &json!("deep"))
    );
    let after = hub.thread_view(&t).unwrap();
    assert_eq!(
        (after.model.as_deref(), after.effort.as_deref()),
        (Some("fake-big"), Some("deep"))
    );
    assert!(!after.stopped);
    assert_eq!(after.status(), before.status());
    assert!(hub.attention_list(true).is_empty());

    // The same values again: nothing recorded, nothing restarted.
    let head = hub.control_read(0, 1).unwrap().1;
    let r = hub
        .set_model("web", Some("fake-big"), Some("deep"), &user())
        .unwrap();
    assert_eq!(r["changed"], false, "{r}");
    assert!(r["message"].as_str().unwrap().contains("already"), "{r}");
    assert_eq!(hub.control_read(0, 1).unwrap().1, head);
    let r = hub.set_model("web", None, Some("deep"), &Caller::Orchestrator);
    assert_eq!(r.unwrap()["changed"], false);
    assert_eq!(hub.control_read(0, 1).unwrap().1, head);

    // The orchestrator clears the effort: applied at once, by the orchestrator.
    let r = hub
        .set_model("web", None, Some("default"), &Caller::Orchestrator)
        .unwrap();
    assert_eq!(
        (r["model"].clone(), r["effort"].clone()),
        (json!("fake-big"), Value::Null)
    );
    assert_eq!(r["applies"], "now", "{r}");
    let evs = until(&mut sub, |e| started_with(e).is_some()).await;
    let EventBody::ThreadModelChanged(m) = &evs[0].body else {
        panic!("{evs:?}")
    };
    assert_eq!((m.effort.as_deref(), m.by), (None, ChangedBy::Orchestrator));
    assert_eq!(m.previous_effort.as_deref(), Some("deep"));
    assert_eq!(evs[0].source, Source::Orchestrator);
    assert_eq!(
        evs.last().and_then(started_with),
        Some((json!("fake-big"), Value::Null))
    );
    // The next prompt runs in the restarted process.
    send(&hub, &t, "again");
    let evs = until(&mut sub, is_report).await;
    assert!(
        evs.iter().all(|e| started_with(e).is_none()),
        "no new start"
    );
}

/// Mid-turn: the turn (and the permission it waits on) stays with the old process; once it
/// ends, the runtime restarts before the queued prompt. A mode change made meanwhile goes with
/// the same restart (one restart, reason `mode_change`).
#[tokio::test]
async fn a_running_turn_finishes_before_the_restart() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let t = new_thread(&hub, dir.path(), "web", "fake").await;
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
    let r = hub
        .set_model("web", Some("fake-big"), Some("high"), &user())
        .unwrap();
    assert_eq!(r["applies"], "after_turn", "{r}");
    let listed = hub.get_thread("web").unwrap();
    assert_eq!(
        (listed["model"].clone(), listed["effort"].clone()),
        (json!("fake-big"), json!("high"))
    );
    assert!(
        listed["runtime_model"].is_null() && listed.get("runtime_model").is_some(),
        "{listed}"
    );
    assert!(
        listed["runtime_effort"].is_null() && listed.get("runtime_effort").is_some(),
        "{listed}"
    );
    assert!(listed.get("runtime_mode").is_none(), "{listed}");
    let r = hub.set_mode("web", "plan", &user()).unwrap();
    assert_eq!(r["applies"], "after_turn", "{r}");
    let sent = send(&hub, &t, "second");
    assert_eq!(sent.turn, None, "it waits for the restart");
    hub.respond(&attn, Answer::allow(), "test").unwrap();
    let evs = until(
        &mut sub,
        |e| matches!(&e.body, EventBody::TurnStarted(p) if p.prompt == "second"),
    )
    .await;
    let pos = |pred: &dyn Fn(&Event) -> bool| evs.iter().position(pred).unwrap();
    let turn1_end = pos(&|e| matches!(e.body, EventBody::TurnEnded(_)) && e.turn == Some(1));
    let stop = pos(&|e| restart_reason(e).is_some());
    assert!(turn1_end < stop, "{evs:?}");
    assert_eq!(
        restart_reason(&evs[stop]).as_deref(),
        Some(MODE_RESTART_REASON)
    );
    assert_eq!(
        evs.iter().filter(|e| restart_reason(e).is_some()).count(),
        1
    );
    let evs = until(&mut sub, |e| started_with(e).is_some()).await;
    assert_eq!(
        evs.iter().find_map(started_with),
        Some((json!("fake-big"), json!("high")))
    );
    until(&mut sub, is_report).await;
    let v = hub.thread_view(&t).unwrap();
    assert_eq!((v.mode.as_str(), v.turns, v.stopped), ("plan", 2, false));
    let listed = hub.get_thread("web").unwrap();
    assert!(listed.get("runtime_model").is_none(), "{listed}");
}

/// A parked thread takes them at its next start; a stop during a pending restart wins; the
/// log keeps them across a hub restart (replay equals live).
#[tokio::test]
async fn parked_stopped_and_reopened_threads() {
    let dir = tempfile::tempdir().unwrap();
    let t = {
        let hub = Hub::open(opts(dir.path())).await.unwrap();
        let t = new_thread(&hub, dir.path(), "web", "fake").await;
        let mut sub = hub.subscribe_thread(&t, 0).unwrap();
        send(&hub, &t, "hi");
        until(&mut sub, is_report).await;
        hub.park_thread("web", false, false, &user()).await.unwrap();
        let r = hub
            .set_model("web", Some("fake-big"), None, &user())
            .unwrap();
        assert_eq!(r["applies"], "next_start", "{r}");
        hub.resume_thread("web", &user()).await.unwrap();
        send(&hub, &t, "after resume");
        let evs = until(&mut sub, |e| started_with(e).is_some()).await;
        assert_eq!(
            evs.iter().find_map(started_with),
            Some((json!("fake-big"), Value::Null))
        );
        until(&mut sub, is_report).await;

        // A restart waiting for the turn to end, then a stop: the thread stays stopped.
        send(&hub, &t, "!sleep 300");
        until(&mut sub, |e| matches!(e.body, EventBody::TurnStarted(_))).await;
        let r = hub.set_model("web", None, Some("high"), &user()).unwrap();
        assert_eq!(r["applies"], "after_turn", "{r}");
        hub.stop_thread_by("web", &user()).unwrap();
        let evs = until(&mut sub, |e| matches!(e.body, EventBody::ThreadStopped(_))).await;
        assert_eq!(restart_reason(evs.last().unwrap()), None);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let v = hub.thread_view(&t).unwrap();
        assert!(v.stopped && v.effort.as_deref() == Some("high"), "{v:?}");
        assert!(!hub.get_thread("web").unwrap()["live"].as_bool().unwrap());
        hub.shutdown().await;
        t
    };
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let v = hub.thread_view(&t).unwrap();
    assert_eq!(
        (v.model.as_deref(), v.effort.as_deref()),
        (Some("fake-big"), Some("high"))
    );
    let mut sub = hub.subscribe_thread(&t, v.last_seq).unwrap();
    send(&hub, &t, "hi again");
    let evs = until(&mut sub, |e| started_with(e).is_some()).await;
    assert_eq!(
        evs.iter().find_map(started_with),
        Some((json!("fake-big"), json!("high")))
    );
}

/// An item the runtime raised between turns defers the restart until it is answered;
/// nothing runs with the old settings meanwhile.
#[tokio::test]
async fn an_item_raised_between_turns_defers_the_restart() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let t = new_thread(&hub, dir.path(), "web", "late").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "hi");
    until(
        &mut sub,
        |e| matches!(&e.body, EventBody::AttentionRaised(a) if a.reason == AttentionReason::Error),
    )
    .await;
    let attn = hub.attention_list(true)[0]["attn_id"]
        .as_str()
        .unwrap()
        .to_string();
    let r = hub.set_model("web", None, Some("high"), &user()).unwrap();
    assert_eq!(r["applies"], "after_turn", "{r}");
    let sent = send(&hub, &t, "second");
    assert_eq!(sent.turn, None);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (_, log) = hub.thread_events(&t, 0, 1000).unwrap();
    assert!(
        !log.iter().any(|e| restart_reason(e).is_some()),
        "restarted too early"
    );
    hub.respond(&attn, Answer::new("ok"), "test").unwrap();
    let evs = until(
        &mut sub,
        |e| matches!(&e.body, EventBody::TurnStarted(p) if p.prompt == "second"),
    )
    .await;
    assert!(evs.iter().any(|e| restart_reason(e).is_some()), "{evs:?}");
    // The queued prompt starts the new process (its start event follows the turn's).
    let evs = until(&mut sub, |e| e.kind() == "thread.started").await;
    let EventBody::ThreadStarted(p) = &evs.last().unwrap().body else {
        panic!()
    };
    assert_eq!(p.ext.as_ref().unwrap()["fake"]["effort"], "high", "{p:?}");
}

/// Refusals: nothing to set, a pty thread, an effort the runtime does not take (the allowed
/// values listed), a model that cannot be shown, an unknown thread. A runtime's own values
/// are canonicalized. None of them records anything.
#[tokio::test]
async fn bad_model_requests() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    new_thread(&hub, dir.path(), "web", "fake").await;
    new_thread(&hub, dir.path(), "term", "pty:sh -c 'sleep 1'").await;
    new_thread(&hub, dir.path(), "cx", "codex").await;
    new_thread(&hub, dir.path(), "cc", "claude-code").await;
    let head = hub.control_read(0, 1).unwrap().1;
    let e = hub.set_model("web", None, None, &user()).unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST, "{e}");
    let e = hub
        .set_model("term", Some("opus"), None, &Caller::Orchestrator)
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST, "{e}");
    assert!(e.message.contains("pty"), "{e}");
    let e = hub
        .set_model("cx", None, Some("turbo"), &user())
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST, "{e}");
    assert!(
        e.message
            .contains("use one of minimal, low, medium, high, xhigh, max, ultra"),
        "{e}"
    );
    let e = hub
        .set_model("cc", None, Some("ultra"), &user())
        .unwrap_err();
    assert!(e.message.contains("low, medium, high, xhigh, max"), "{e}");
    let e = hub
        .set_model("web", Some(&"m".repeat(200)), None, &user())
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST, "{e}");
    let e = hub.set_model("nope", Some("x"), None, &user()).unwrap_err();
    assert_eq!(e.code, codes::NOT_FOUND, "{e}");
    assert_eq!(hub.control_read(0, 1).unwrap().1, head, "nothing recorded");
    // Canonical spelling; no runtime yet, so the next start takes it.
    let r = hub
        .set_model("cc", Some("claude-fable-5-1"), Some("HIGH"), &user())
        .unwrap();
    assert_eq!(
        (r["effort"].clone(), r["applies"].clone()),
        (json!("high"), json!("next_start"))
    );
    let v: Value = hub.get_thread("cc").unwrap();
    assert_eq!(
        (v["model"].clone(), v["effort"].clone()),
        (json!("claude-fable-5-1"), json!("high"))
    );
    // `default` clears the model too.
    let r = hub.set_model("cc", Some("Default"), None, &user()).unwrap();
    assert!(r["model"].is_null() && r["effort"] == "high", "{r}");
}

/// `thread.new` `effort`: checked and canonicalized for the runtime, recorded as
/// `ext.twr.effort`, and passed to the runtime.
#[tokio::test]
async fn threads_start_with_their_effort() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let mut req = new_req(dir.path(), "web", "fake");
    req.effort = Some("deep".into());
    let v = hub.new_thread(req, "test").await.unwrap();
    assert_eq!(v["effort"], "deep", "{v}");
    let t = v["id"].as_str().unwrap().to_string();
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    let created = until(&mut sub, |e| e.kind() == "thread.created").await;
    let EventBody::ThreadCreated(c) = &created[0].body else {
        panic!()
    };
    assert_eq!(c.ext.as_ref().unwrap()["twr"]["effort"], "deep");
    send(&hub, &t, "hi");
    let evs = until(&mut sub, |e| started_with(e).is_some()).await;
    assert_eq!(
        evs.iter().find_map(started_with).map(|(_, e)| e),
        Some(json!("deep"))
    );
    let mut req = new_req(dir.path(), "cx", "codex");
    req.effort = Some("Max".into());
    let v = hub.new_thread(req, "test").await.unwrap();
    assert_eq!(v["effort"], "max", "{v}");
    let mut req = new_req(dir.path(), "cc", "claude-code");
    req.effort = Some("default".into());
    let v = hub.new_thread(req, "test").await.unwrap();
    assert!(v["effort"].is_null(), "{v}");
    let mut req = new_req(dir.path(), "gb", "codex");
    req.effort = Some("extreme".into());
    let e = hub.new_thread(req, "test").await.unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST, "{e}");
    assert!(e.message.contains("use one of"), "{e}");
}

/// A runtime that keeps a session's model (and, with `keeps_effort`, its reasoning effort)
/// when it resumes the session without one, as the real ones do (`twr_adapters::resume_keeps`:
/// Codex, Grok Build and ACP agents keep both, Claude Code the model). Each session reports
/// what it runs with (`ext.fake.model` / `ext.fake.effort`) and ends each turn at once.
struct Resuming {
    name: &'static str,
    keeps_effort: bool,
    /// Session id → the model and effort it last ran with.
    sessions: Arc<std::sync::Mutex<std::collections::HashMap<String, (Value, Value)>>>,
    /// When set, a new session (not a resumed one) reports its `thread.started` only once this
    /// is notified: a runtime that is live but has not named its session yet.
    hold: Option<Arc<tokio::sync::Notify>>,
}

impl Adapter for Resuming {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: self.name,
            tier: Tier::Headless,
            description: "keeps a session's settings on resume",
        }
    }

    fn spawn(&self, cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let (h, mut end) = AdapterHandle::pair();
        let sid = cfg
            .resume_session
            .clone()
            .unwrap_or_else(|| format!("s-{}", self.name));
        let mut sessions = self.sessions.lock().unwrap();
        let (kept_model, kept_effort) = match &cfg.resume_session {
            Some(id) => sessions.get(id).cloned().unwrap_or_default(),
            None => (Value::Null, Value::Null),
        };
        let model = cfg.model.clone().map_or(kept_model, Value::from);
        let effort = match cfg.effort.clone() {
            Some(e) => json!(e),
            None if self.keeps_effort => kept_effort,
            None => Value::Null,
        };
        sessions.insert(sid.clone(), (model.clone(), effort.clone()));
        drop(sessions);
        let hold = self.hold.clone().filter(|_| cfg.resume_session.is_none());
        tokio::spawn(async move {
            if let Some(hold) = hold {
                hold.notified().await;
            }
            let mut ext = JsonMap::new();
            ext.insert("fake".into(), json!({ "model": model, "effort": effort }));
            end.emit(
                EventBody::ThreadStarted(SessionPayload {
                    session_id: Some(sid),
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

/// Clearing a model or effort back to the runtime's default once the thread has a session:
/// refused for a runtime that keeps that setting on resume (its session would go on with the
/// old value while the thread, and the restart's `ext.twr` stamp, said the default), and
/// nothing is recorded. Before the first session the clear is fine. Claude Code resumes on
/// the default effort when none is passed, so its effort clears: the restarted session reports
/// the default, as the stamp says.
#[tokio::test]
async fn clears_a_resumed_session_would_ignore_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut o = opts(dir.path());
    let sessions = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let resuming = |name: &'static str, keeps_effort: bool| -> Arc<dyn Adapter> {
        Arc::new(Resuming {
            name,
            keeps_effort,
            sessions: Arc::clone(&sessions),
            hold: None,
        })
    };
    let adapters = [
        resuming("codex", true),
        resuming("grok-build", true),
        resuming("acp", true),
        resuming("claude-code", false),
    ];
    o.adapters = Arc::new(move |name: &str| -> Option<Arc<dyn Adapter>> {
        adapters.iter().find(|a| a.info().name == name).cloned()
    });
    let hub = Hub::open(o).await.unwrap();
    for (name, spec, keeps_effort) in [
        ("cx", "codex", true),
        ("gb", "grok-build", true),
        ("ag", "acp:agent --stdio", true),
        ("cc", "claude-code", false),
    ] {
        let mut req = new_req(dir.path(), name, spec);
        req.model = Some("m1".into());
        req.effort = Some("low".into());
        let t = hub.new_thread(req, "test").await.unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        // No session yet: the next start is a new session, so a clear is fine (and undone).
        let r = hub.set_model(name, Some("default"), Some("default"), &user());
        let r = r.unwrap_or_else(|e| panic!("{name}: {e}"));
        assert!(r["model"].is_null() && r["effort"].is_null(), "{name}: {r}");
        hub.set_model(name, Some("m1"), Some("low"), &user())
            .unwrap();
        let mut sub = hub.subscribe_thread(&t, 0).unwrap();
        send(&hub, &t, "hi");
        let evs = until(&mut sub, |e| e.kind() == "turn.ended").await;
        assert_eq!(
            evs.iter().find_map(started_with),
            Some((json!("m1"), json!("low"))),
            "{name}"
        );

        let head = hub.control_read(0, 1).unwrap().1;
        let e = hub
            .set_model(name, Some("default"), None, &user())
            .unwrap_err();
        assert_eq!(e.code, codes::BAD_REQUEST, "{name}: {e}");
        assert!(
            e.message.contains("keeps a session's model on resume")
                && e.message.contains("would go on with m1")
                && e.message.contains("pick a model instead"),
            "{name}: {e}"
        );
        let e = hub.set_model(name, Some(""), Some("default"), &Caller::Orchestrator);
        assert_eq!(e.unwrap_err().code, codes::BAD_REQUEST, "{name}");
        let r = hub.set_model(name, None, Some("default"), &user());
        if keeps_effort {
            let e = r.unwrap_err();
            assert!(
                e.message.contains("keeps a session's effort on resume")
                    && e.message.contains("would go on with low")
                    && e.message.contains("pick an effort instead"),
                "{name}: {e}"
            );
            assert_eq!(hub.control_read(0, 1).unwrap().1, head, "{name}");
            let v = hub.thread_view(&t).unwrap();
            assert_eq!(
                (v.model.as_deref(), v.effort.as_deref()),
                (Some("m1"), Some("low")),
                "{name}"
            );
            // A value is fine: the session takes it.
            let r = hub.set_model(name, None, Some("high"), &user()).unwrap();
            assert_eq!(r["applies"], "now", "{name}: {r}");
            let evs = until(&mut sub, |e| started_with(e).is_some()).await;
            assert_eq!(
                evs.last().and_then(started_with),
                Some((json!("m1"), json!("high"))),
                "{name}"
            );
        } else {
            let r = r.unwrap();
            assert_eq!(r["applies"], "now", "{name}: {r}");
            let evs = until(&mut sub, |e| started_with(e).is_some()).await;
            let start = evs.last().unwrap();
            assert_eq!(
                started_with(start),
                Some((json!("m1"), Value::Null)),
                "{name}: the session runs on the default effort"
            );
            let EventBody::ThreadStarted(p) = &start.body else {
                panic!()
            };
            assert!(p.ext.as_ref().unwrap()["twr"]["effort"].is_null(), "{p:?}");
        }
    }
}

/// A runtime that keeps a session's settings, started with a model and effort, is live but has
/// not reported its session yet (its `thread.started` is held back): a clear is refused all the
/// same, since the settings restart that would apply it resumes the session the runtime is
/// about to name, which goes on with the old values. A setting the live runtime started on
/// the default can still be cleared after a value was set meanwhile: the runtime runs on the
/// default already, and nothing restarts.
#[tokio::test]
async fn clears_are_refused_while_the_first_session_is_unnamed() {
    let dir = tempfile::tempdir().unwrap();
    let mut o = opts(dir.path());
    let hold = Arc::new(tokio::sync::Notify::new());
    let codex: Arc<dyn Adapter> = Arc::new(Resuming {
        name: "codex",
        keeps_effort: true,
        sessions: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        hold: Some(Arc::clone(&hold)),
    });
    o.adapters = Arc::new(move |name: &str| -> Option<Arc<dyn Adapter>> {
        (name == "codex").then(|| Arc::clone(&codex))
    });
    let hub = Hub::open(o).await.unwrap();
    let mut req = new_req(dir.path(), "cx", "codex");
    req.model = Some("m1".into());
    let t = hub.new_thread(req, "test").await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "hi");
    let v = hub.thread_view(&t).unwrap();
    assert!(v.session_id.is_none(), "{:?}", v.session_id);

    // The model the runtime started with cannot be cleared: nothing is recorded.
    let head = hub.control_read(0, 1).unwrap().1;
    let e = hub
        .set_model("cx", Some("default"), None, &user())
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST, "{e}");
    assert!(
        e.message.contains("keeps a session's model on resume")
            && e.message.contains("would go on with m1"),
        "{e}"
    );
    assert_eq!(hub.control_read(0, 1).unwrap().1, head);
    // The effort it started on the default: set, then cleared again before anything restarts.
    let r = hub.set_model("cx", None, Some("high"), &user()).unwrap();
    assert_eq!(r["applies"], "after_turn", "{r}");
    let r = hub.set_model("cx", None, Some("default"), &user()).unwrap();
    assert_eq!(r["applies"], "now", "{r}");
    assert!(r["effort"].is_null(), "{r}");

    // The session is named and the turn ends; no restart was due, and the clear is still
    // refused now that the thread has a session.
    hold.notify_one();
    let evs = until(&mut sub, |e| e.kind() == "turn.ended").await;
    assert_eq!(
        evs.iter().find_map(started_with),
        Some((json!("m1"), Value::Null))
    );
    assert!(!evs.iter().any(|e| restart_reason(e).is_some()));
    let v = hub.thread_view(&t).unwrap();
    assert_eq!(v.session_id.as_deref(), Some("s-codex"));
    assert_eq!(
        (v.model.as_deref(), v.effort.as_deref()),
        (Some("m1"), None)
    );
    let e = hub
        .set_model("cx", Some("default"), None, &user())
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST, "{e}");
}

/// A move to another runtime starts a new session, so going back to the runtime's default is
/// fine with it, though the thread's current runtime keeps a session's settings on resume.
/// Nor is a later clear refused because of the live runtime the thread moved away from (here
/// still running its turn, started with a model and not even named yet): it is replaced, never
/// resumed. Once the new runtime has a session, its own rule applies.
#[tokio::test]
async fn clears_are_fine_with_a_move_to_another_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let mut o = opts(dir.path());
    let sessions = Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let hold = Arc::new(tokio::sync::Notify::new());
    let codex: Arc<dyn Adapter> = Arc::new(Resuming {
        name: "codex",
        keeps_effort: true,
        sessions: Arc::clone(&sessions),
        hold: Some(Arc::clone(&hold)),
    });
    let grok: Arc<dyn Adapter> = Arc::new(Resuming {
        name: "grok-build",
        keeps_effort: true,
        sessions,
        hold: None,
    });
    o.adapters = Arc::new(move |name: &str| -> Option<Arc<dyn Adapter>> {
        match name {
            "codex" => Some(Arc::clone(&codex)),
            "grok-build" => Some(Arc::clone(&grok)),
            _ => None,
        }
    });
    let hub = Hub::open(o).await.unwrap();

    // A thread with a codex session on m1 / low.
    let mut req = new_req(dir.path(), "cx", "codex");
    req.model = Some("m1".into());
    req.effort = Some("low".into());
    let t = hub.new_thread(req, "test").await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "hi");
    hold.notify_one();
    until(&mut sub, |e| e.kind() == "turn.ended").await;
    assert!(hub.thread_view(&t).unwrap().session_id.is_some());
    let e = hub.set_model("cx", Some("default"), None, &user());
    assert_eq!(e.unwrap_err().code, codes::BAD_REQUEST);

    // Moving to grok-build with both cleared: fine, nothing of codex's session goes along.
    let r = hub
        .set_runtime_model(
            "cx",
            Some("grok-build"),
            Some("default"),
            Some("default"),
            &user(),
        )
        .unwrap();
    assert_eq!(r["runtime_changed"], true, "{r}");
    assert!(r["model"].is_null() && r["effort"].is_null(), "{r}");
    until(&mut sub, |e| restart_reason(e).is_some()).await;
    // And back to codex on m1 / low: a codex runtime runs its turn (live, unnamed, on m1).
    let r = hub
        .set_runtime_model("cx", Some("codex"), Some("m1"), Some("low"), &user())
        .unwrap();
    assert_eq!(r["runtime_changed"], true, "{r}");
    send(&hub, &t, "work");
    // Codex's runtime refuses a clear while it runs unnamed on m1 (the restart would resume
    // the session it is about to name).
    let e = hub.set_model("cx", Some("default"), None, &user());
    assert!(
        e.unwrap_err().message.contains("keeps a session's model"),
        "a live runtime of the thread's own runtime counts"
    );
    // The move away from it: that live runtime counts no more, so the clear is fine.
    let r = hub
        .set_runtime_model("cx", Some("grok-build"), Some("g1"), None, &user())
        .unwrap();
    assert_eq!(r["applies"], "after_turn", "{r}");
    let r = hub
        .set_model("cx", Some("default"), Some("default"), &user())
        .unwrap_or_else(|e| panic!("a clear for the new runtime: {e}"));
    assert_eq!(r["runtime"], "grok-build", "{r}");
    assert!(r["model"].is_null() && r["effort"].is_null(), "{r}");

    // The codex turn ends and its runtime goes; grok-build's first session runs on its defaults.
    hold.notify_one();
    until(&mut sub, |e| {
        restart_reason(e).as_deref() == Some(RUNTIME_RESTART_REASON)
    })
    .await;
    send(&hub, &t, "go on");
    let evs = until(&mut sub, |e| e.kind() == "turn.ended").await;
    assert_eq!(
        evs.iter().find_map(started_with),
        Some((Value::Null, Value::Null))
    );
    let v = hub.thread_view(&t).unwrap();
    assert_eq!(v.session_id.as_deref(), Some("s-grok-build"));
    // Now grok-build's session would keep a model set on it: its rule applies.
    hub.set_model("cx", Some("g2"), None, &user()).unwrap();
    let e = hub
        .set_model("cx", Some("default"), None, &user())
        .unwrap_err();
    assert!(
        e.message
            .contains("grok-build keeps a session's model on resume"),
        "{e}"
    );
    hub.shutdown().await;
}

/// The effort is checked against the `claude` the thread's spawn would run: a project whose
/// `env` `PATH` names another `claude` (known not to list `--effort`) is refused at
/// `thread.new` and `thread.set_model`; a thread without that `PATH` is not.
#[tokio::test]
async fn the_effort_is_checked_against_the_threads_own_claude() {
    use twr_adapters::claude_code::cli::{lookup, Lookup};
    if std::env::var_os("TWR_CLAUDE_BIN").is_some_and(|v| !v.is_empty()) {
        eprintln!("skipped: TWR_CLAUDE_BIN names the binary, the PATH does not choose it");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    // An older `claude` in the project's own `PATH`: options listed, `--effort` not.
    let bin = root.join("old-claude");
    std::fs::create_dir(&bin).unwrap();
    let src = root.join("claude.src");
    std::fs::write(
        &src,
        "#!/bin/sh\ncase \"$1\" in\n--help) printf 'Options:\\n  --model <model>  Model\\n  \
         -p, --print  Print\\n';;\n--version) echo '2.1.100 (Claude Code)';;\nesac\n",
    )
    .unwrap();
    // Copied, not written in place: a fork elsewhere holding it open would make exec'ing it
    // fail (ETXTBSY).
    let st = std::process::Command::new("install")
        .args(["-m", "755"])
        .arg(&src)
        .arg(bin.join("claude"))
        .status()
        .unwrap();
    assert!(st.success());
    let repo = root.join("proj");
    std::fs::create_dir(&repo).unwrap();
    let path = format!("{}:/usr/bin:/bin", bin.display());
    // What an earlier spawn of that thread read of it (the hub never probes for this).
    let env = vec![("PATH".to_string(), path.clone())];
    let resolved = twr_adapters::resolve::resolve(Path::new("claude"), &env, None, &repo);
    if let Lookup::Probe(p) = lookup(&resolved, &repo, WAIT) {
        p.wait().await;
    }
    let mut o = opts(&root);
    o.config.projects.insert(
        "old".into(),
        twr_core::config::ProjectConfig {
            repo: repo.clone(),
            env: [("PATH".to_string(), path)].into_iter().collect(),
            ..Default::default()
        },
    );
    let hub = Hub::open(o).await.unwrap();
    let req = |effort: Option<&str>| NewThread {
        project: "old".into(),
        runtime: Some("claude-code".into()),
        name: Some(format!("old-{}", effort.unwrap_or("none"))),
        effort: effort.map(str::to_string),
        no_worktree: true,
        ..Default::default()
    };
    let e = hub.new_thread(req(Some("high")), "test").await.unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST, "{e}");
    assert!(e.message.contains("does not support --effort"), "{e}");
    assert!(e.message.contains("2.1.100"), "{e}");
    // Nothing was recorded: the name is free.
    assert!(hub.thread_view("old-high").is_err());
    hub.new_thread(req(None), "test").await.unwrap();
    let e = hub
        .set_model("old-none", None, Some("high"), &user())
        .unwrap_err();
    assert!(e.message.contains("does not support --effort"), "{e}");
    // A thread of another directory, without that `PATH`: its `claude` is not known.
    let mut other = new_req(&root, "elsewhere", "claude-code");
    other.effort = Some("high".into());
    let v = hub.new_thread(other, "test").await.unwrap();
    assert_eq!(v["effort"], "high", "{v}");
    hub.shutdown().await;
}
