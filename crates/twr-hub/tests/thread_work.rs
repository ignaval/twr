//! What a thread has in flight (`hub::work`) and the hung-turn check that judges by it
//! (`hub::stall`): a quiet turn waiting on its own background work gets no hung-turn card
//! until it has been quiet for long; a call that waits on work that already ended gets one card,
//! with the evidence, and no other for the same incident; heartbeats never hold one off; and
//! `thread.get` and `thread.activity` read the same.
//!
//! Workers run the fake adapter, scripted (the first prompt starts the session, which plays its
//! steps at once): tool calls and tasks as Claude Code reports them
//! (a foreground task carries `ext.twr.foreground_call`, a task moved to the background
//! `ext.twr.detached`).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::time::timeout;
use twr_adapters::fake::{FakeAdapter, Step};
use twr_core::adapter::Adapter;
use twr_core::config::Config;
use twr_core::event::*;
use twr_hub::{Caller, Hub, HubOptions, NewThread};

const WAIT: Duration = Duration::from_secs(20);

/// The stall threshold of these hubs: the hung-turn threshold is the same (it is shorter than
/// 5 min), and a turn waiting on its own work waits twice that.
const STALL: Duration = Duration::from_millis(500);

fn opts(dir: &Path, script: Vec<Step>) -> HubOptions {
    let mut o = HubOptions::new(dir.join("data").join("twr.db"), Config::default());
    let fake: Arc<dyn Adapter> = Arc::new(FakeAdapter::scripted(script));
    o.adapters = Arc::new(move |name: &str| -> Option<Arc<dyn Adapter>> {
        (name == "fake").then(|| Arc::clone(&fake))
    });
    o.git_tick = Some(Duration::ZERO);
    o.disk_tick = Some(Duration::ZERO);
    o.stall_after = Some(STALL);
    o
}

async fn thread(hub: &Hub, dir: &Path) -> String {
    let project = dir.join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let t = hub
        .new_thread(
            NewThread {
                project: project.to_string_lossy().into_owned(),
                runtime: Some("fake".into()),
                name: Some("api".into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap();
    t["id"].as_str().unwrap().to_string()
}

fn events(hub: &Hub, thread: &str) -> Vec<Event> {
    hub.thread_events(thread, 0, 100_000).unwrap().1
}

async fn until(hub: &Hub, thread: &str, pred: impl Fn(&[Event]) -> bool) -> Vec<Event> {
    timeout(WAIT, async {
        loop {
            let evs = events(hub, thread);
            if pred(&evs) {
                return evs;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "timed out; saw {:#?}",
            events(hub, thread)
                .iter()
                .map(|e| format!("{} {:?}", e.kind(), e.turn))
                .collect::<Vec<_>>()
        )
    })
}

/// The hung-turn cards among `evs`, with when each was raised.
fn cards(evs: &[Event]) -> Vec<(chrono::DateTime<chrono::Utc>, AttentionRaised)> {
    evs.iter()
        .filter_map(|e| match &e.body {
            EventBody::AttentionRaised(p) if is_hung_card(&p.ext) => Some((e.ts, p.clone())),
            _ => None,
        })
        .collect()
}

/// `ext.twr.hung.<key>` of card `p`.
fn hung(p: &AttentionRaised, key: &str) -> Value {
    p.ext.as_ref().unwrap()["twr"]["hung"][key].clone()
}

fn session() -> Step {
    Step::EmitSession(EventBody::ThreadStarted(SessionPayload {
        session_id: Some("s".into()),
        ..Default::default()
    }))
}

/// A Bash call `id` the agent says is for `purpose`.
fn call(id: &str, purpose: &str) -> Step {
    let mut ext = None;
    set_twr_ext(&mut ext, PURPOSE_KEY, json!(purpose));
    Step::Emit(EventBody::AgentToolCall(AgentToolCall {
        call_id: id.into(),
        tool: "Bash".into(),
        input_summary: format!("sh {id}"),
        ext,
        ..Default::default()
    }))
}

fn result(id: &str) -> Step {
    Step::Emit(EventBody::AgentToolResult(AgentToolResult {
        call_id: id.into(),
        ok: true,
        ..Default::default()
    }))
}

/// Task `id` doing `what`; `placed`: its `ext.twr` key and value, if any (`foreground_call`,
/// `detached`).
fn task(id: &str, what: &str, running: bool, placed: Option<(&str, Value)>) -> Step {
    let mut ext = None;
    if let Some((k, v)) = placed {
        set_twr_ext(&mut ext, k, v);
    }
    Step::EmitSession(EventBody::TaskUpdated(TaskUpdated {
        task_id: id.into(),
        running,
        description: what.into(),
        ext,
        ..Default::default()
    }))
}

/// A turn that starts its review in the background and waits on its answer in the foreground,
/// as the incident's worker did; the review ends after `review_ms`.
fn review_script(review_ms: u64) -> Vec<Step> {
    vec![
        session(),
        call("c1", "Re-review at xhigh"),
        task("review", "Re-review at xhigh", true, None),
        result("c1"),
        call("c2", "Wait for the review's answer"),
        task(
            "waiter",
            "Wait for the review's answer",
            true,
            Some((FOREGROUND_CALL_KEY, json!("c2"))),
        ),
        Step::Sleep(review_ms),
        task("review", "", false, None),
        task("waiter", "", false, None),
        result("c2"),
        Step::Emit(EventBody::AgentText(AgentText {
            text: "The review is clean.".into(),
            is_final: true,
            ..Default::default()
        })),
        Step::Emit(EventBody::TurnEnded(TurnEnded::default())),
        Step::Sleep(60_000),
    ]
}

/// A quiet turn whose review runs in the background, a message of the user's waiting: no
/// hung-turn card at the hung-turn threshold; `thread.get` and `thread.activity` both say it
/// waits on its work, with the call and the review; the card comes only after twice the
/// threshold, saying the work still runs.
#[tokio::test]
async fn a_turn_waiting_on_its_running_review_is_not_hung_at_the_threshold() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path(), review_script(60_000)))
        .await
        .unwrap();
    let api = thread(&hub, dir.path()).await;
    hub.send_by(&api, "review it", &Caller::user("test"))
        .unwrap();
    until(&hub, &api, |e| {
        e.iter()
            .any(|e| matches!(&e.body, EventBody::TaskUpdated(t) if t.task_id == "waiter"))
    })
    .await;
    hub.send_by(&api, "and then this", &Caller::user("test"))
        .unwrap();
    let start = chrono::Utc::now();
    tokio::time::sleep(STALL + STALL / 2).await;
    // Past the hung-turn threshold: no card, no plain stall.
    let evs = events(&hub, &api);
    assert!(cards(&evs).is_empty(), "{:#?}", cards(&evs));
    let t = hub.get_thread("api").unwrap();
    let work = &t["work"];
    // (Quiet for less than a minute: a client reads it as working; the hung-turn check reads
    // what is in flight, with its own thresholds.)
    assert_eq!(work["state"], "working", "{work:#}");
    assert_eq!(work["sight"], "full");
    assert_eq!(work["calls"][0]["what"], "Wait for the review's answer");
    assert_eq!(work["calls"][0]["task"], "waiter");
    let kinds: Vec<(&str, &str)> = work["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| (t["id"].as_str().unwrap(), t["kind"].as_str().unwrap()))
        .collect();
    assert!(kinds.contains(&("review", "background")), "{kinds:?}");
    assert!(kinds.contains(&("waiter", "foreground")), "{kinds:?}");
    assert_eq!(work["queued"]["count"], 1);
    assert_eq!(work["queued"]["from"], "user");
    // `thread.activity` reads the same.
    let a = &hub.thread_activity()[&api]["work"];
    for key in [
        "state",
        "sight",
        "calls",
        "tasks",
        "queued",
        "turn",
        "progress_at",
    ] {
        assert_eq!(a[key], work[key], "{key}");
    }
    // The waiter is the call's, not background work: one `bg`.
    assert_eq!(
        t["background_tasks"]["waiter"]["foreground_call"], "c2",
        "{:#}",
        t["background_tasks"]
    );
    let evs = until(&hub, &api, |e| !cards(e).is_empty()).await;
    let (at, card) = cards(&evs).remove(0);
    assert!(
        at - start
            >= chrono::Duration::from_std(STALL * 2).unwrap() - chrono::Duration::milliseconds(100),
        "raised {} ms in",
        (at - start).num_milliseconds()
    );
    assert_eq!(hung(&card, "state"), "waiting_on_work");
    assert!(
        card.summary
            .contains("Its background work still runs (Bash \"Wait for the review's answer\""),
        "{}",
        card.summary
    );
    assert!(
        card.summary.contains("background: Re-review at xhigh"),
        "{}",
        card.summary
    );
    hub.shutdown().await;
}

/// The tests outlive their Bash call's timeout (moved to the background, detached), and the
/// agent then waits for them with a blocking `TaskOutput` call: they are work it waits on, so
/// no card comes at the hung-turn threshold, and the one that comes later says the work runs.
#[tokio::test]
async fn detached_tests_waited_for_again_are_work_it_waits_on() {
    let dir = tempfile::tempdir().unwrap();
    let mut wait = None;
    set_twr_ext(&mut wait, WAITS_FOR_KEY, json!("tests"));
    let script = vec![
        session(),
        call("c1", "Run the TUI tests"),
        task(
            "tests",
            "Run the TUI tests",
            true,
            Some((FOREGROUND_CALL_KEY, json!("c1"))),
        ),
        task("tests", "", true, Some((DETACHED_KEY, json!(true)))),
        result("c1"),
        Step::Emit(EventBody::AgentToolCall(AgentToolCall {
            call_id: "c2".into(),
            tool: "TaskOutput".into(),
            ext: wait,
            ..Default::default()
        })),
        Step::Sleep(60_000),
    ];
    let hub = Hub::open(opts(dir.path(), script)).await.unwrap();
    let api = thread(&hub, dir.path()).await;
    hub.send_by(&api, "test it", &Caller::user("test")).unwrap();
    until(&hub, &api, |e| {
        e.iter()
            .any(|e| matches!(&e.body, EventBody::AgentToolCall(c) if c.call_id == "c2"))
    })
    .await;
    hub.send_by(&api, "and then this", &Caller::user("test"))
        .unwrap();
    tokio::time::sleep(STALL + STALL / 2).await;
    assert!(cards(&events(&hub, &api)).is_empty());
    let work = &hub.get_thread("api").unwrap()["work"];
    assert_eq!(work["tasks"][0]["kind"], "detached", "{work:#}");
    assert_eq!(work["tasks"][0]["awaited"], true, "{work:#}");
    assert_eq!(work["calls"][0]["task"], "tests", "{work:#}");
    let evs = until(&hub, &api, |e| !cards(e).is_empty()).await;
    let (_, card) = cards(&evs).remove(0);
    assert_eq!(hung(&card, "state"), "waiting_on_work");
    assert!(
        card.summary
            .contains("detached, waited for: Run the TUI tests"),
        "{}",
        card.summary
    );
    hub.shutdown().await;
}

/// The review ends while the turn is quiet: the card that never came is no matter, the turn
/// goes on, and nothing was raised.
#[tokio::test]
async fn a_review_that_ends_in_time_raises_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path(), review_script(800)))
        .await
        .unwrap();
    let api = thread(&hub, dir.path()).await;
    hub.send_by(&api, "review it", &Caller::user("test"))
        .unwrap();
    hub.send_by(&api, "and then this", &Caller::user("test"))
        .unwrap();
    until(&hub, &api, |e| {
        e.iter()
            .any(|e| matches!(&e.body, EventBody::AgentToolResult(r) if r.call_id == "c2"))
    })
    .await;
    // (The waiting message's turn, which this script never answers, is another matter.)
    let turn_1 = |evs: &[Event]| {
        cards(evs)
            .into_iter()
            .filter(|(_, c)| hung(c, "turn") == 1)
            .count()
    };
    tokio::time::sleep(STALL).await;
    assert_eq!(turn_1(&events(&hub, &api)), 0);
    hub.shutdown().await;
}

/// The tests a waiter waits for end; the waiter stays open (a `pgrep -f` loop that matches
/// itself never ends) while an older waiter runs detached: one hung-turn card at the
/// threshold, saying the call waits, no background work runs, what ended last and the leftover.
/// When the leftover ends (progress) the card closes, its runtime gone on, and no other card
/// comes for the same call.
#[tokio::test]
async fn a_waiter_that_outlived_its_tests_gets_one_card_with_the_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let script = vec![
        session(),
        task(
            "old",
            "Wait for TUI tests",
            true,
            Some((DETACHED_KEY, json!(true))),
        ),
        call("c1", "Apply the edit; run the TUI tests"),
        task("tests", "Apply the edit; run the TUI tests", true, None),
        result("c1"),
        call("c2", "Wait for TUI tests"),
        task(
            "w",
            "Wait for TUI tests",
            true,
            Some((FOREGROUND_CALL_KEY, json!("c2"))),
        ),
        Step::Sleep(100),
        task("tests", "", false, None),
        // Long after the card: the leftover is killed (its end is progress).
        Step::Sleep(2_000),
        task("old", "", false, Some((DETACHED_KEY, json!(true)))),
        Step::Sleep(60_000),
    ];
    let hub = Hub::open(opts(dir.path(), script)).await.unwrap();
    let api = thread(&hub, dir.path()).await;
    hub.send_by(&api, "test it", &Caller::user("test")).unwrap();
    hub.send_by(&api, "and then this", &Caller::user("test"))
        .unwrap();
    let evs = until(&hub, &api, |e| !cards(e).is_empty()).await;
    let (_, card) = cards(&evs).remove(0);
    assert_eq!(hung(&card, "state"), "waiting_on_call");
    assert_eq!(hung(&card, "key"), "waiting_on_call:c2");
    assert_eq!(hung(&card, "work")["calls"][0]["id"], "c2");
    for part in [
        "api's turn 1 seems hung: no output from its runtime for",
        "while a message of yours waits to start the next turn.",
        "It waits on Bash \"Wait for TUI tests\"",
        "and no background work of its own runs; the last, \"Apply the edit; run the TUI tests\", ended",
        "1 task moved to the background after its call returned runs unwaited",
        "Interrupt it to run that now, or wait.",
    ] {
        assert!(card.summary.contains(part), "{part:?} in {}", card.summary);
    }
    // The leftover's end closes it; the same call open and quiet again raises no other.
    let evs = until(&hub, &api, |e| {
        e.iter().any(|e| {
            matches!(&e.body,
            EventBody::AttentionResolved(p) if p.attn_id == card.attn_id)
        })
    })
    .await;
    let closed = evs
        .iter()
        .find_map(|e| match &e.body {
            EventBody::AttentionResolved(p) if p.attn_id == card.attn_id => Some(p.clone()),
            _ => None,
        })
        .unwrap();
    assert!(went_on(&closed.ext), "{closed:?}");
    tokio::time::sleep(STALL * 4).await;
    let evs = events(&hub, &api);
    assert_eq!(cards(&evs).len(), 1, "{:#?}", cards(&evs));
    assert_eq!(
        hub.get_thread("api").unwrap()["work"]["calls"][0]["id"],
        "c2",
        "the call is still open"
    );
    hub.shutdown().await;
}

/// A turn with nothing in flight but a detached task whose heartbeats keep coming: they are
/// liveness, not progress, so the card comes at the threshold, saying nothing is in flight
/// and the leftover runs unwaited; `thread.get` keeps the heartbeat apart from progress.
#[tokio::test]
async fn heartbeats_alone_do_not_hold_off_a_hung_card() {
    let dir = tempfile::tempdir().unwrap();
    let mut script = vec![
        session(),
        task(
            "old",
            "Wait for TUI tests",
            true,
            Some((DETACHED_KEY, json!(true))),
        ),
    ];
    for _ in 0..40 {
        script.push(Step::Sleep(100));
        // (As Claude Code's adapter says them: each update of a detached task says so.)
        let mut ext = None;
        set_twr_ext(&mut ext, DETACHED_KEY, json!(true));
        script.push(Step::EmitSession(EventBody::TaskUpdated(TaskUpdated {
            task_id: "old".into(),
            running: true,
            heartbeat: true,
            ext,
            ..Default::default()
        })));
    }
    script.push(Step::Sleep(60_000));
    let mut o = opts(dir.path(), script);
    // Heartbeats recorded every 250 ms (a quarter of the background timeout), not 10 min.
    o.config.background_timeout_secs = 1;
    let hub = Hub::open(o).await.unwrap();
    let api = thread(&hub, dir.path()).await;
    hub.send_by(&api, "go", &Caller::user("test")).unwrap();
    hub.send_by(&api, "and then this", &Caller::user("test"))
        .unwrap();
    let evs = until(&hub, &api, |e| !cards(e).is_empty()).await;
    let (_, card) = cards(&evs).remove(0);
    assert_eq!(hung(&card, "state"), "nothing_in_flight");
    assert!(
        card.summary
            .contains("Nothing of it is in flight that twr can see"),
        "{}",
        card.summary
    );
    assert!(card.summary.contains("runs unwaited"), "{}", card.summary);
    assert!(
        evs.iter()
            .any(|e| matches!(&e.body, EventBody::TaskUpdated(t) if t.heartbeat)),
        "heartbeats were recorded before the card"
    );
    let work = &hub.get_thread("api").unwrap()["work"];
    assert_eq!(work["tasks"][0]["kind"], "detached");
    // Seen alive since its last progress: said apart.
    let at = |k: &str| {
        serde_json::from_value::<chrono::DateTime<chrono::Utc>>(work[k].clone())
            .unwrap_or_else(|e| panic!("{k}: {e}: {work:#}"))
    };
    assert!(at("alive_at") > at("progress_at"), "{work:#}");
    hub.shutdown().await;
}
