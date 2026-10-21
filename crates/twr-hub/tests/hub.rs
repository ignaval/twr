//! Hub core tests: append/seq/replay, store triggers, lagging subscribers, the durable prompt
//! queue across restarts, the closing sequence, fallback reports and the relay rule.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::time::timeout;
use twr_adapters::fake::{FakeAdapter, Step};
use twr_core::adapter::{
    Adapter, AdapterCmd, AdapterEnd, AdapterEvent, AdapterHandle, AdapterInfo, Answer,
    AttachApplies, SpawnConfig, Tier,
};
use twr_core::config::Config;
use twr_core::event::*;
use twr_core::proto::codes;
use twr_core::reduce::ThreadStatus;
use twr_hub::hub::ThreadSubscription;
use twr_hub::{Caller, Hub, HubOptions, NewThread};

const WAIT: Duration = Duration::from_secs(10);

fn crashy() -> FakeAdapter {
    FakeAdapter::scripted(vec![
        Step::Emit(EventBody::ThreadStarted(SessionPayload {
            session_id: Some("s-crashy".into()),
            ..Default::default()
        })),
        Step::Emit(EventBody::AttentionRaised(AttentionRaised {
            attn_id: "a-crashy".into(),
            reason: AttentionReason::Permission,
            summary: "Allow Bash?".into(),
            options: vec!["allow".into(), "deny".into()],
            ..Default::default()
        })),
        Step::Exit {
            code: Some(3),
            stderr_tail: "warming up\nsegfault in widget".into(),
        },
    ])
}

/// A runtime that numbers its requests per process, like grok's `perm-<rpc id>`: every session
/// raises `perm-1` and finishes its turn once that is answered.
fn numbered() -> FakeAdapter {
    FakeAdapter::scripted(vec![
        Step::Emit(EventBody::ThreadStarted(SessionPayload {
            session_id: Some("s-numbered".into()),
            ..Default::default()
        })),
        Step::Emit(permission("perm-1")),
        Step::AwaitRespond("perm-1".into()),
        Step::Emit(text("answered")),
        Step::Emit(EventBody::TurnEnded(TurnEnded {
            result: TurnResult::Completed,
            ..Default::default()
        })),
    ])
}

fn opts(dir: &Path) -> HubOptions {
    let mut o = HubOptions::new(dir.join("data").join("twr.db"), Config::default());
    o.adapters = Arc::new(|name: &str| -> Option<Arc<dyn Adapter>> {
        match name {
            "fake" => Some(Arc::new(FakeAdapter::new())),
            "crashy" => Some(Arc::new(crashy())),
            "numbered" => Some(Arc::new(numbered())),
            _ => None,
        }
    });
    o.stop_timeout = Duration::from_secs(5);
    o
}

/// What a [`TestAdapter`] does with a new session.
type SpawnFn = dyn Fn(AdapterEnd, SpawnConfig, Option<String>) + Send + Sync;

/// An adapter whose sessions are written inline in each test (runtime name `test`).
struct TestAdapter(Arc<SpawnFn>);

impl Adapter for TestAdapter {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "test",
            tier: Tier::Headless,
            description: "hub test adapter",
        }
    }

    fn spawn(&self, cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let (h, end) = AdapterHandle::pair();
        (self.0)(end, cfg, prompt);
        Ok(h)
    }
}

/// `opts` plus runtime `test` played by `f`.
fn opts_with(
    dir: &Path,
    f: impl Fn(AdapterEnd, SpawnConfig, Option<String>) + Send + Sync + 'static,
) -> HubOptions {
    let mut o = opts(dir);
    let base = o.adapters.clone();
    let test: Arc<dyn Adapter> = Arc::new(TestAdapter(Arc::new(f)));
    o.adapters = Arc::new(move |n: &str| {
        if n == "test" {
            Some(Arc::clone(&test))
        } else {
            base(n)
        }
    });
    o
}

/// Serves commands until `stop` (or the hub drops the handle), then exits cleanly.
async fn until_stopped(mut end: AdapterEnd) {
    while let Some(cmd) = end.cmds.recv().await {
        if cmd == AdapterCmd::Stop {
            break;
        }
    }
    end.exited(Some(0), "");
}

fn session(id: &str) -> EventBody {
    EventBody::ThreadStarted(SessionPayload {
        session_id: Some(id.into()),
        ..Default::default()
    })
}

fn ended(result: TurnResult) -> EventBody {
    EventBody::TurnEnded(TurnEnded {
        result,
        ..Default::default()
    })
}

/// A non-final `agent.text` the tests wait for to know everything before it was processed.
fn sync_mark() -> EventBody {
    EventBody::AgentText(AgentText {
        text: "sync".into(),
        ..Default::default()
    })
}

fn is_sync_mark(e: &Event) -> bool {
    matches!(&e.body, EventBody::AgentText(t) if t.text == "sync")
}

async fn new_thread(hub: &Hub, dir: &Path, name: &str, runtime: &str) -> String {
    let v = hub
        .new_thread(
            NewThread {
                project: dir.to_string_lossy().into_owned(),
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
    .unwrap_or_else(|_| panic!("timed out; saw {:?}", kinds(&seen)));
    seen
}

fn kinds(evs: &[Event]) -> Vec<String> {
    evs.iter().map(|e| e.kind().to_string()).collect()
}

fn text(t: &str) -> EventBody {
    EventBody::AgentText(AgentText {
        text: t.into(),
        is_final: true,
        ..Default::default()
    })
}

#[tokio::test]
async fn append_assigns_seq_and_state_replays() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let t1 = new_thread(&hub, dir.path(), "one", "fake").await;
    let t2 = new_thread(&hub, dir.path(), "two", "fake").await;
    let src = Source::Adapter("fake".into());
    let e = hub
        .append(
            &t1,
            Some(1),
            Source::Hub,
            EventBody::TurnStarted(TurnStarted {
                prompt: "p".into(),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
    assert_eq!(e.seq, 2);
    let e = hub
        .append(
            &t1,
            Some(1),
            src.clone(),
            text("hi"),
            Some("{\"raw\":1}".into()),
        )
        .unwrap();
    assert_eq!(e.seq, 3);
    let raw = e.raw.clone().expect("raw pointer");
    assert_eq!(hub.blob(&raw).unwrap().unwrap(), b"{\"raw\":1}");
    // Oversized payloads are capped and the full text lands in the blob store.
    let big = "y".repeat(10_000);
    let e = hub
        .append(&t1, Some(1), src.clone(), text(&big), None)
        .unwrap();
    let EventBody::AgentText(t) = &e.body else {
        panic!()
    };
    assert!(t.text.len() <= 512);
    let r = t.extra["text_ref"].as_str().unwrap();
    assert_eq!(hub.blob(r).unwrap().unwrap(), big.as_bytes());
    assert_eq!(
        hub.append(&t2, None, Source::Hub, text("x"), None)
            .unwrap()
            .seq,
        2
    );

    // agent.* stays off the control stream; gseq counts only control events.
    let (items, head) = hub.control_read(0, 100).unwrap();
    assert_eq!(head, 3);
    assert_eq!(
        items.iter().map(|(g, _)| *g).collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert!(items.iter().all(|(_, e)| !e.body.is_agent()));
    let before = hub.thread_view(&t1).unwrap();
    assert_eq!(before.current_turn, Some(1));
    drop(hub);

    // Replay rebuilds the same view (plus the closing of the turn that was left open).
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let (_, evs) = hub.thread_events("one", 0, 100).unwrap();
    assert_eq!(
        kinds(&evs),
        vec![
            "thread.created",
            "turn.started",
            "agent.text",
            "agent.text",
            "turn.ended",
            "thread.crashed",
            "attention.raised",
            // The crash's block_kind (the code rule finds nothing transient: user_blocked).
            "decision.made"
        ]
    );
    assert!(evs.windows(2).all(|w| w[1].seq == w[0].seq + 1));
    let after = hub.thread_view("one").unwrap();
    assert_eq!(after.name, before.name);
    assert_eq!(after.current_turn, None);
    assert_eq!(after.status(), ThreadStatus::Error);
    let e = hub.append(&t2, None, Source::Hub, text("y"), None).unwrap();
    assert_eq!(e.seq, 3);
    let (_, head2) = hub.control_read(0, 1).unwrap();
    assert_eq!(
        head2,
        head + 4,
        "turn.ended, crashed, raised, decision; agent.text is not control"
    );
}

#[tokio::test]
async fn store_triggers_reject_update_and_delete() {
    let dir = tempfile::tempdir().unwrap();
    let o = opts(dir.path());
    let db = o.db_path.clone();
    let hub = Hub::open(o).await.unwrap();
    new_thread(&hub, dir.path(), "one", "fake").await;
    let conn = rusqlite::Connection::open(&db).unwrap();
    for sql in [
        "UPDATE events SET kind = 'x'",
        "DELETE FROM events",
        "UPDATE control SET gseq = 99",
        "DELETE FROM control",
    ] {
        let err = conn.execute(sql, []).unwrap_err().to_string();
        assert!(err.contains("append-only"), "{sql}: {err}");
    }
    assert_eq!(hub.thread_events("one", 0, 10).unwrap().1.len(), 1);
}

#[tokio::test]
async fn lagging_subscribers_catch_up_from_the_store() {
    let dir = tempfile::tempdir().unwrap();
    let mut o = opts(dir.path());
    o.broadcast_capacity = 2;
    let hub = Hub::open(o).await.unwrap();
    let t = new_thread(&hub, dir.path(), "one", "fake").await;
    let mut control = hub.subscribe_control(0);
    let mut thread = hub.subscribe_thread("one", 0).unwrap();
    let body = || {
        EventBody::GitSnapshot(GitSnapshot {
            dirty_files: 1,
            ..Default::default()
        })
    };
    // Far more than the channel holds, while nobody reads.
    for _ in 0..60 {
        hub.append(&t, None, Source::Hub, body(), None).unwrap();
    }
    // Then keep appending concurrently while the subscribers read slowly.
    let writer = {
        let hub = hub.clone();
        let t = t.clone();
        tokio::spawn(async move {
            for i in 0..140 {
                let b = if i % 3 == 0 { text("noise") } else { body() };
                hub.append(&t, None, Source::Hub, b, None).unwrap();
                tokio::task::yield_now().await;
            }
        })
    };
    let total_control = 1 + 60 + 140 - 47; // created + snapshots (i % 3 == 0 are agent.text)
    let mut gseqs = Vec::new();
    while gseqs.len() < total_control {
        let (g, _) = timeout(WAIT, control.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        gseqs.push(g);
        if gseqs.len() % 7 == 0 {
            tokio::task::yield_now().await;
        }
    }
    let mut seqs = Vec::new();
    while seqs.len() < 201 {
        let ev = timeout(WAIT, thread.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        seqs.push(ev.seq);
    }
    writer.await.unwrap();
    assert_eq!(gseqs, (1..=total_control as u64).collect::<Vec<_>>());
    assert_eq!(seqs, (1..=201).collect::<Vec<_>>());
    // A subscription from the middle starts right after `since`.
    let mut mid = hub.subscribe_control(100);
    assert_eq!(mid.next().await.unwrap().unwrap().0, 101);
}

#[test]
fn pending_prompts_survive_a_hub_crash() {
    let dir = tempfile::tempdir().unwrap();
    let thread;
    let session;
    {
        // First hub: a turn waits on a permission, two more prompts queue behind it. Then the
        // runtime (and every task) disappears without any shutdown.
        let rt = tokio::runtime::Runtime::new().unwrap();
        (thread, session) = rt.block_on(async {
            let hub = Hub::open(opts(dir.path())).await.unwrap();
            let t = new_thread(&hub, dir.path(), "one", "fake").await;
            let mut sub = hub.subscribe_thread(&t, 0).unwrap();
            send(&hub, &t, "!perm Bash rm -rf build");
            send(&hub, &t, "second");
            send(&hub, &t, "third");
            let seen = until(&mut sub, |e| e.kind() == "attention.raised").await;
            let session = seen
                .iter()
                .find_map(|e| match &e.body {
                    EventBody::ThreadStarted(p) => p.session_id.clone(),
                    _ => None,
                })
                .unwrap();
            let v = hub.get_thread(&t).unwrap();
            assert_eq!(v["pending"], 2);
            assert_eq!(v["status"], "waiting");
            (t, session)
        });
        drop(rt);
    }
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let hub = Hub::open(opts(dir.path())).await.unwrap();
        let mut sub = hub.subscribe_thread(&thread, 0).unwrap();
        let evs = until(&mut sub, |e| {
            e.turn == Some(3) && e.kind() == "report.ready"
        })
        .await;
        let ks = kinds(&evs);
        // The dead turn is closed first, then the queue drains in order on a resumed session.
        let crash = ks.iter().position(|k| k == "thread.crashed").unwrap();
        assert_eq!(
            &ks[crash - 2..crash + 2],
            [
                "turn.ended",
                "attention.resolved",
                "thread.crashed",
                "attention.raised"
            ]
        );
        let prompts: Vec<(u64, String)> = evs
            .iter()
            .filter_map(|e| match &e.body {
                EventBody::TurnStarted(p) => Some((e.turn.unwrap(), p.prompt.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            prompts,
            vec![
                (1, "!perm Bash rm -rf build".to_string()),
                (2, "second".to_string()),
                (3, "third".to_string())
            ]
        );
        let resumed = evs
            .iter()
            .filter_map(|e| match &e.body {
                EventBody::ThreadStarted(p) => Some(p.clone()),
                _ => None,
            })
            .next_back()
            .unwrap();
        assert_eq!(resumed.session_id.as_deref(), Some(session.as_str()));
        assert_eq!(resumed.ext.unwrap()["fake"]["resumed"], json!(true));
        assert_eq!(hub.get_thread(&thread).unwrap()["pending"], 0);
        hub.shutdown().await;
    });
}

#[tokio::test]
async fn crash_closes_turn_resolves_items_and_raises_error() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let t = new_thread(&hub, dir.path(), "boom", "crashy").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "go");
    let evs = until(
        &mut sub,
        |e| matches!(&e.body, EventBody::AttentionRaised(a) if a.reason == AttentionReason::Error),
    )
    .await;
    assert_eq!(
        kinds(&evs),
        vec![
            "thread.created",
            "turn.started",
            "thread.started",
            "attention.raised",
            "turn.ended",
            "attention.resolved",
            "thread.crashed",
            "attention.raised"
        ]
    );
    let EventBody::TurnEnded(ended) = &evs[4].body else {
        panic!()
    };
    assert_eq!(ended.result, TurnResult::Interrupted);
    assert_eq!(evs[4].turn, Some(1));
    let EventBody::AttentionResolved(r) = &evs[5].body else {
        panic!()
    };
    assert_eq!(
        (r.attn_id.clone(), r.by, r.answer.clone()),
        (format!("{t}:1:a-crashy"), ResolvedBy::ThreadStopped, None)
    );
    let EventBody::ThreadCrashed(c) = &evs[6].body else {
        panic!()
    };
    assert_eq!(c.exit_code, Some(3));
    assert!(c.stderr_tail.ends_with("segfault in widget"));
    let EventBody::AttentionRaised(a) = &evs[7].body else {
        panic!()
    };
    assert!(a.summary.contains("segfault in widget"), "{}", a.summary);
    assert_eq!(evs[7].source, Source::Hub);
    let v = hub.thread_view(&t).unwrap();
    assert_eq!(v.status(), ThreadStatus::Error);
    assert_eq!(hub.live_runtimes(), 0);
    // Hub-raised items resolve without any runtime to forward to.
    let ev = hub
        .respond(&a.attn_id, Answer::new("dismiss"), "test")
        .unwrap();
    assert_eq!(ev.source, Source::User("test".into()));
    assert!(hub.thread_view(&t).unwrap().open_attention.is_empty());
}

#[tokio::test]
async fn stop_closes_the_turn_and_appends_thread_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let t = new_thread(&hub, dir.path(), "one", "fake").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "!perm Bash");
    send(&hub, &t, "queued");
    until(&mut sub, |e| e.kind() == "attention.raised").await;
    let r = hub.stop_thread("one", "test").unwrap();
    assert_eq!(r["stopping"], true);
    assert_eq!(r["dropped_prompts"], 1);
    let evs = until(&mut sub, |e| e.kind() == "thread.stopped").await;
    assert_eq!(
        kinds(&evs),
        vec!["turn.ended", "attention.resolved", "thread.stopped"]
    );
    let v = hub.thread_view(&t).unwrap();
    assert_eq!(v.status(), ThreadStatus::Done);
    assert!(v.open_attention.is_empty());
    assert_eq!(hub.live_runtimes(), 0);
    // The user's stop carries no reason.
    let EventBody::ThreadStopped(p) = &evs.last().unwrap().body else {
        panic!()
    };
    assert!(p.ext.is_none(), "{:?}", p.ext);
    // A new prompt resumes the session lazily.
    send(&hub, &t, "back again");
    let evs = until(&mut sub, |e| e.kind() == "report.ready").await;
    let started = evs
        .iter()
        .find_map(|e| match &e.body {
            EventBody::ThreadStarted(p) => Some(p.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(started.ext.unwrap()["fake"]["resumed"], json!(true));
    hub.shutdown().await;
    assert_eq!(hub.live_runtimes(), 0);
    let (_, evs) = hub.thread_events(&t, 0, 1000).unwrap();
    // The hub's shutdown stops the idle runtime too (the log needs it), and says why.
    let EventBody::ThreadStopped(p) = &evs.last().unwrap().body else {
        panic!("{:?}", evs.last().unwrap().kind())
    };
    assert_eq!(p.ext.as_ref().unwrap()["twr"]["reason"], "hub_shutdown");
}

#[tokio::test]
async fn completed_turns_get_graded_fallback_reports() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let t = new_thread(&hub, dir.path(), "api-1", "fake").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(
        &hub,
        &t,
        "Fixed the bug. Tests pass. Deployed it. Also cleaned up.",
    );
    let evs = until(&mut sub, |e| e.kind() == "report.ready").await;
    let ks = kinds(&evs);
    assert_eq!(
        &ks[ks.len() - 3..],
        ["turn.ended", "decision.made", "report.ready"]
    );
    let EventBody::DecisionMade(d) = &evs[evs.len() - 2].body else {
        panic!()
    };
    assert_eq!(
        (d.site, d.provider, d.chosen.as_str()),
        (DecisionSite::Grade, DecisionProvider::Code, "success")
    );
    assert_eq!(evs[evs.len() - 2].source, Source::Decision("code".into()));
    let EventBody::ReportReady(r) = &evs.last().unwrap().body else {
        panic!()
    };
    assert_eq!(r.summary, "Fixed the bug. Tests pass. Deployed it.");
    assert_eq!(
        (r.grade, r.turn, r.written_by.as_str()),
        (Grade::Success, Some(1), "hub")
    );
    assert!(r.session_id.is_some());
    assert_eq!(hub.thread_view(&t).unwrap().status(), ThreadStatus::Done);

    send(&hub, &t, "!giveup");
    let evs = until(&mut sub, |e| e.kind() == "attention.raised").await;
    let r = evs
        .iter()
        .find_map(|e| match &e.body {
            EventBody::ReportReady(r) => Some(r.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(r.grade, Grade::GaveUp);
    let EventBody::AttentionRaised(a) = &evs.last().unwrap().body else {
        panic!()
    };
    assert_eq!(a.reason, AttentionReason::Review);

    send(&hub, &t, "!error db down");
    let evs = until(&mut sub, |e| e.kind() == "attention.raised").await;
    let r = evs
        .iter()
        .find_map(|e| match &e.body {
            EventBody::ReportReady(r) => Some(r.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(r.grade, Grade::GaveUp);
    assert_eq!(r.summary, "Turn 3 failed.");
    hub.shutdown().await;
}

/// A long reply with a `## Summary` section: the hub's report quotes that section (not the
/// reply's first sentences) and records the reply's seq (`ext.twr.reply_seq`).
#[tokio::test]
async fn fallback_reports_quote_the_replys_summary_section() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let t = new_thread(&hub, dir.path(), "web", "fake").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    let mut prompt = String::from("I started with the login code. It was long. Very long.\n");
    for i in 0..70 {
        prompt.push_str(&format!(
            "- step {i}: read module {i} of the login flow, found nothing wrong there\n"
        ));
    }
    prompt.push_str("## Summary\n- Fixed the login redirect\n- All tests pass\n");
    assert!(prompt.len() > twr_core::blob::PAYLOAD_CAP);
    send(&hub, &t, &prompt);
    let evs = until(&mut sub, |e| e.kind() == "report.ready").await;
    let reply = evs
        .iter()
        .rev()
        .find(|e| matches!(&e.body, EventBody::AgentText(a) if a.is_final))
        .unwrap();
    // The reply is stored capped; the report still quotes its end.
    let stored = hub.thread_events(&t, reply.seq - 1, 1).unwrap().1;
    assert!(twr_core::blob::has_capped(&stored[0].body.data().unwrap()));
    let EventBody::ReportReady(r) = &evs.last().unwrap().body else {
        panic!()
    };
    assert_eq!(r.summary, "Fixed the login redirect; All tests pass");
    assert_eq!(
        r.ext.as_ref().unwrap()["twr"]["reply_seq"],
        json!(reply.seq)
    );

    // No section: the first sentences, as before. No reply: no reply_seq.
    send(&hub, &t, "Done. Tests pass. Shipped. More.");
    let evs = until(&mut sub, |e| e.kind() == "report.ready").await;
    let EventBody::ReportReady(r) = &evs.last().unwrap().body else {
        panic!()
    };
    assert_eq!(r.summary, "Done. Tests pass. Shipped.");
    assert!(r.ext.is_some());
    send(&hub, &t, "!error db down");
    let evs = until(&mut sub, |e| e.kind() == "report.ready").await;
    let EventBody::ReportReady(r) = &evs.last().unwrap().body else {
        panic!()
    };
    assert_eq!(r.ext, None);
    hub.shutdown().await;
}

#[tokio::test]
async fn only_attention_respond_answers_the_agent() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let t1 = new_thread(&hub, dir.path(), "one", "fake").await;
    let t2 = new_thread(&hub, dir.path(), "two", "fake").await;
    let mut s1 = hub.subscribe_thread(&t1, 0).unwrap();
    let mut s2 = hub.subscribe_thread(&t2, 0).unwrap();
    send(&hub, &t1, "!perm Bash make deploy");
    let evs = until(&mut s1, |e| e.kind() == "attention.raised").await;
    let EventBody::AttentionRaised(a) = &evs.last().unwrap().body else {
        panic!()
    };
    let attn = a.attn_id.clone();
    // Lots of unrelated hub activity: another thread's full turn with its report, a queued
    // prompt on the waiting thread, another subscriber.
    send(&hub, &t2, "hello");
    until(&mut s2, |e| e.kind() == "report.ready").await;
    send(&hub, &t1, "next");
    send(&hub, &t2, "!giveup");
    until(&mut s2, |e| e.kind() == "attention.raised").await;
    let (_, evs) = hub.thread_events(&t1, 0, 1000).unwrap();
    assert!(
        !evs.iter().any(|e| matches!(
            e.kind(),
            "attention.resolved" | "agent.tool_result" | "turn.ended"
        )),
        "{:?}",
        kinds(&evs)
    );
    assert_eq!(
        hub.thread_view(&t1).unwrap().status(),
        ThreadStatus::Waiting
    );

    // Wrong answers and unknown ids are rejected without touching the item.
    let e = hub
        .respond(&attn, Answer::new("maybe"), "test")
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST);
    let e = hub.respond("nope", Answer::allow(), "test").unwrap_err();
    assert_eq!(e.code, codes::NOT_FOUND);

    let ev = hub.respond(&attn, Answer::allow(), "tui").unwrap();
    assert_eq!(ev.source, Source::User("tui".into()));
    let evs = until(&mut s1, |e| e.turn == Some(2) && e.kind() == "report.ready").await;
    let ks = kinds(&evs);
    let resolved = ks.iter().position(|k| k == "attention.resolved").unwrap();
    assert_eq!(ks[resolved + 1], "agent.tool_result");
    let EventBody::AttentionResolved(r) = &evs[resolved].body else {
        panic!()
    };
    assert_eq!(r.by, ResolvedBy::User);
    assert_eq!(r.answer, Some(Answer::allow()));
    let EventBody::AgentToolResult(tr) = &evs[resolved + 1].body else {
        panic!()
    };
    assert!(tr.ok);
    let e = hub.respond(&attn, Answer::allow(), "test").unwrap_err();
    assert_eq!(e.code, codes::CONFLICT);
    hub.shutdown().await;
}

/// A turn whose only tool call the user denied (its result `ok: false`) is graded `partial`
/// by the code rule, not the default success, and raises no review.
#[tokio::test]
async fn a_turn_whose_only_tool_call_was_denied_is_partial() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let t = new_thread(&hub, dir.path(), "api", "fake").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(
        &hub,
        &t,
        "!perm Bash rm -rf build\nI left the build directory alone.",
    );
    let evs = until(&mut sub, |e| e.kind() == "attention.raised").await;
    let EventBody::AttentionRaised(a) = &evs.last().unwrap().body else {
        panic!()
    };
    hub.respond(&a.attn_id, Answer::deny(), "tui").unwrap();
    let evs = until(&mut sub, |e| e.kind() == "report.ready").await;
    let grade = evs
        .iter()
        .find_map(|e| match &e.body {
            EventBody::DecisionMade(d) if d.site == DecisionSite::Grade => Some(d.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        (grade.provider, grade.chosen.as_str(), grade.confidence),
        (DecisionProvider::Code, "partial", 0.8)
    );
    let EventBody::ReportReady(r) = &evs.last().unwrap().body else {
        panic!()
    };
    assert_eq!(r.grade, Grade::Partial);
    assert!(hub.attention_list(true).is_empty(), "no review for partial");
    hub.shutdown().await;
}

/// Items the hub raised can be dismissed (`attention.dismiss`, or `cancel` on a `say`
/// question): resolved `by: user` with answer `cancel`, nothing routed. A runtime's permission
/// is refused (the runtime waits for its answer) and stays open; the orchestrator may not
/// dismiss anything.
#[tokio::test]
async fn hub_items_can_be_dismissed_but_runtime_requests_cannot() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let tui = twr_hub::Caller::user("tui");
    let api = new_thread(&hub, dir.path(), "api", "fake").await;
    let web = new_thread(&hub, dir.path(), "web", "fake").await;
    let mut s_api = hub.subscribe_thread(&api, 0).unwrap();
    let mut s_web = hub.subscribe_thread(&web, 0).unwrap();

    // A runtime's permission: refused, still open, still answerable.
    send(&hub, &api, "!perm Bash make deploy");
    let evs = until(&mut s_api, |e| e.kind() == "attention.raised").await;
    let EventBody::AttentionRaised(perm) = &evs.last().unwrap().body else {
        panic!()
    };
    let e = hub
        .attention_dismiss(&perm.attn_id, None, &tui)
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST, "{}", e.message);
    assert!(e.message.contains("stop the thread"), "{}", e.message);
    assert!(hub
        .attention_list(false)
        .iter()
        .any(|i| i["attn_id"] == perm.attn_id.as_str()));
    hub.respond(&perm.attn_id, Answer::allow(), "tui").unwrap();
    until(&mut s_api, |e| e.kind() == "report.ready").await;

    // A review: dismissed.
    send(&hub, &web, "!giveup");
    let evs = until(&mut s_web, |e| e.kind() == "attention.raised").await;
    let EventBody::AttentionRaised(review) = &evs.last().unwrap().body else {
        panic!()
    };
    assert_eq!(review.reason, AttentionReason::Review);
    let e = hub
        .attention_dismiss(&review.attn_id, None, &twr_hub::Caller::Orchestrator)
        .unwrap_err();
    assert_eq!(e.code, codes::RELAY_FORBIDDEN);
    let ev = hub.attention_dismiss(&review.attn_id, None, &tui).unwrap();
    let EventBody::AttentionResolved(r) = &ev.body else {
        panic!()
    };
    assert_eq!(
        (r.by, r.answer.as_ref().map(|a| a.decision.as_str())),
        (ResolvedBy::User, Some("cancel"))
    );
    let e = hub
        .attention_dismiss(&review.attn_id, None, &tui)
        .unwrap_err();
    assert_eq!(e.code, codes::CONFLICT);

    // `say` questions offer `cancel`; answering it (or dismissing) routes nothing.
    let turns = |hub: &Hub| {
        [&api, &web].map(|t| {
            let v = hub.thread_view(t).unwrap();
            (v.turns, hub.get_thread(t).unwrap()["pending"].clone())
        })
    };
    let before = turns(&hub);
    let q = hub.say("deploy the thing", None, &tui).await.unwrap();
    assert_eq!(q["action"], "ask", "{q}");
    assert_eq!(
        q["options"],
        json!(["api", "web", "new thread", "cancel (don't send)"])
    );
    let ev = hub
        .respond(
            q["attn_id"].as_str().unwrap(),
            Answer::new("cancel (don't send)"),
            "tui",
        )
        .unwrap();
    let EventBody::AttentionResolved(r) = &ev.body else {
        panic!()
    };
    assert_eq!(r.answer.as_ref().unwrap().decision, "cancel (don't send)");
    let q = hub.say("deploy the other thing", None, &tui).await.unwrap();
    let id = q["attn_id"].as_str().unwrap();
    let revision = hub
        .attention_list(true)
        .iter()
        .find(|i| i["attn_id"] == id)
        .unwrap()["revision"]
        .as_u64();
    hub.attention_dismiss(id, revision, &tui).unwrap();
    assert!(hub.attention_list(true).is_empty());
    assert_eq!(
        turns(&hub),
        before,
        "a cancelled question routed its message"
    );
    hub.shutdown().await;
}

/// Workers named like the hub question's actions (`cancel`, `new_thread`) stay routable, and
/// the actions keep working: their labels can't be thread names.
#[tokio::test]
async fn hub_question_actions_never_collide_with_thread_names() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let tui = twr_hub::Caller::user("tui");
    let cancel = new_thread(&hub, dir.path(), "cancel", "fake").await;
    let new = new_thread(&hub, dir.path(), "new_thread", "fake").await;
    let turns = |hub: &Hub| {
        [&cancel, &new].map(|t| {
            let v = hub.thread_view(t).unwrap();
            (v.turns, hub.get_thread(t).unwrap()["pending"].clone())
        })
    };
    let ask = |q: &serde_json::Value| -> (String, Vec<String>) {
        assert_eq!(q["action"], "ask", "{q}");
        let options = q["options"]
            .as_array()
            .unwrap()
            .iter()
            .map(|o| o.as_str().unwrap().to_string())
            .collect();
        (q["attn_id"].as_str().unwrap().to_string(), options)
    };
    let index = |options: &[String], label: &str| options.iter().position(|o| o == label).unwrap();
    let mut s_cancel = hub.subscribe_thread(&cancel, 0).unwrap();
    let mut s_new = hub.subscribe_thread(&new, 0).unwrap();

    // The worker named `cancel`, picked by its option index, gets the message.
    let (id, options) = ask(&hub.say("deploy one", None, &tui).await.unwrap());
    assert_eq!(
        options,
        ["cancel", "new_thread", "new thread", "cancel (don't send)"]
    );
    hub.respond_option(&id, index(&options, "cancel"), None, "tui")
        .unwrap();
    let evs = until(&mut s_cancel, |e| e.kind() == "turn.started").await;
    let EventBody::TurnStarted(ts) = &evs.last().unwrap().body else {
        panic!()
    };
    assert_eq!(ts.prompt, "deploy one");
    until(&mut s_cancel, |e| e.kind() == "turn.ended").await;

    // The worker named `new_thread`, by name, gets the message (not the orchestrator).
    let (id, _) = ask(&hub.say("deploy two", None, &tui).await.unwrap());
    hub.respond(&id, Answer::new("new_thread"), "tui").unwrap();
    let evs = until(&mut s_new, |e| e.kind() == "turn.started").await;
    let EventBody::TurnStarted(ts) = &evs.last().unwrap().body else {
        panic!()
    };
    assert_eq!(ts.prompt, "deploy two");
    until(&mut s_new, |e| e.kind() == "turn.ended").await;

    // The cancel action, by index or label, routes nothing.
    let before = turns(&hub);
    let (id, options) = ask(&hub.say("deploy three", None, &tui).await.unwrap());
    let ev = hub
        .respond_option(&id, index(&options, "cancel (don't send)"), None, "tui")
        .unwrap();
    let EventBody::AttentionResolved(r) = &ev.body else {
        panic!()
    };
    assert_eq!(r.answer.as_ref().unwrap().decision, "cancel (don't send)");
    let (id, _) = ask(&hub.say("deploy four", None, &tui).await.unwrap());
    hub.respond(&id, Answer::new("cancel (don't send)"), "tui")
        .unwrap();
    assert!(hub.attention_list(true).is_empty());
    assert_eq!(
        turns(&hub),
        before,
        "a cancelled question routed its message"
    );

    // A question raised before the actions had their own labels keeps its `cancel`.
    hub.append(
        HUB_THREAD,
        None,
        Source::Hub,
        EventBody::AttentionRaised(AttentionRaised {
            attn_id: "legacy-q".into(),
            reason: AttentionReason::Question,
            summary: "Which thread should get: deploy five?".into(),
            options: vec!["api".into(), "new_thread".into(), "cancel".into()],
            ext: Some(
                json!({"twr": {"pending_message": "deploy five", "asked_by": "tui"}})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
            ..Default::default()
        }),
        None,
    )
    .unwrap();
    hub.respond("legacy-q", Answer::new("cancel"), "tui")
        .unwrap();
    assert!(hub.attention_list(true).is_empty());
    assert_eq!(
        turns(&hub),
        before,
        "a cancelled question routed its message"
    );
    hub.shutdown().await;
}

/// A worker thread's runtime starts with `[threads] env`, and `[projects.<name>] env` over it
/// for that project's threads, besides twr's own markers. A thread made from another
/// repository whose folder has the project's name is not that project's.
#[tokio::test]
async fn runtimes_get_the_configured_environment() {
    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(std::sync::Mutex::new(Vec::<Vec<(String, String)>>::new()));
    let s = Arc::clone(&seen);
    let mut o = opts_with(dir.path(), move |end, cfg, _| {
        s.lock().unwrap().push(cfg.env.clone());
        tokio::spawn(async move {
            end.emit(session("s-1"), None);
            end.emit(ended(TurnResult::Completed), None);
            until_stopped(end).await;
        });
    });
    let (api, web) = (dir.path().join("api"), dir.path().join("web"));
    let other = dir.path().join("elsewhere").join("api");
    for d in [&api, &web, &other] {
        std::fs::create_dir_all(d).unwrap();
    }
    o.config.threads.env = [("CARGO_INCREMENTAL", "0"), ("SHARED", "threads")]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    o.config.projects.insert(
        "api".into(),
        twr_core::config::ProjectConfig {
            repo: api.clone(),
            env: [("CARGO_TARGET_DIR", "/cache/api"), ("SHARED", "api")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Default::default()
        },
    );
    let hub = Hub::open(o).await.unwrap();
    let get = |env: &[(String, String)], k: &str| {
        env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
    };
    let mut ids = Vec::new();
    for (project, name) in [
        ("api", "a"),
        (web.to_str().unwrap(), "w"),
        (other.to_str().unwrap(), "o"),
    ] {
        let v = hub
            .new_thread(
                NewThread {
                    project: project.into(),
                    runtime: Some("test".into()),
                    name: Some(name.into()),
                    ..Default::default()
                },
                "test",
            )
            .await
            .unwrap();
        let id = v["id"].as_str().unwrap().to_string();
        let mut sub = hub.subscribe_thread(&id, 0).unwrap();
        send(&hub, &id, "hi");
        until(&mut sub, |e| e.kind() == "turn.ended").await;
        ids.push(id);
    }
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 3);
    let (a, w, o) = (&seen[0], &seen[1], &seen[2]);
    // Recorded under the same project name all the same.
    assert_eq!(
        hub.thread_view(&ids[2]).unwrap().project.as_deref(),
        Some("api")
    );
    assert_eq!(get(o, "CARGO_TARGET_DIR"), None);
    assert_eq!(get(o, "SHARED").as_deref(), Some("threads"));
    assert_eq!(get(a, "CARGO_TARGET_DIR").as_deref(), Some("/cache/api"));
    assert_eq!(get(a, "SHARED").as_deref(), Some("api"));
    assert_eq!(get(a, "CARGO_INCREMENTAL").as_deref(), Some("0"));
    assert_eq!(get(w, "CARGO_TARGET_DIR"), None);
    assert_eq!(get(w, "SHARED").as_deref(), Some("threads"));
    for env in [a, w, o] {
        assert!(get(env, "TWR_AGENT").is_some(), "{env:?}");
    }
    hub.shutdown().await;
}

/// A thread made for a configured project before `thread.created` recorded it
/// (`ext.twr.configured_project`) is replayed as no project's: it gets `[threads] env`, not
/// the project's (it does once made again).
#[tokio::test]
async fn a_thread_from_before_the_marker_gets_no_project_env() {
    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(std::sync::Mutex::new(Vec::<Vec<(String, String)>>::new()));
    let make = |dir: &Path| {
        let s = Arc::clone(&seen);
        let mut o = opts_with(dir, move |end, cfg, _| {
            s.lock().unwrap().push(cfg.env.clone());
            tokio::spawn(async move {
                end.emit(session("s-1"), None);
                end.emit(ended(TurnResult::Completed), None);
                until_stopped(end).await;
            });
        });
        o.config.threads.env = [("SHARED".to_string(), "threads".to_string())].into();
        o.config.projects.insert(
            "api".into(),
            twr_core::config::ProjectConfig {
                repo: dir.join("api"),
                env: [("SHARED".to_string(), "api".to_string())].into(),
                ..Default::default()
            },
        );
        o
    };
    std::fs::create_dir(dir.path().join("api")).unwrap();
    let o = make(dir.path());
    let db = o.db_path.clone();
    let hub = Hub::open(o).await.unwrap();
    let v = hub
        .new_thread(
            NewThread {
                project: "api".into(),
                runtime: Some("test".into()),
                name: Some("a".into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap();
    let id = v["id"].as_str().unwrap().to_string();
    assert!(hub.thread_view(&id).unwrap().configured_project);
    hub.shutdown().await;
    drop(hub);
    // The log as a hub before the marker wrote it.
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch("DROP TRIGGER events_no_update").unwrap();
    let changed = conn
        .execute(
            "UPDATE events SET data = json_remove(data, '$.ext.twr.configured_project') \
             WHERE kind = 'thread.created' \
             AND json_extract(data, '$.ext.twr.configured_project') = 1",
            [],
        )
        .unwrap();
    assert_eq!(changed, 1);
    drop(conn);

    let hub = Hub::open(make(dir.path())).await.unwrap();
    let view = hub.thread_view(&id).unwrap();
    assert_eq!(
        (view.project.as_deref(), view.configured_project),
        (Some("api"), false)
    );
    let mut sub = hub.subscribe_thread(&id, 0).unwrap();
    send(&hub, &id, "hi");
    until(&mut sub, |e| e.kind() == "turn.ended").await;
    let seen = seen.lock().unwrap().clone();
    let shared = |env: &[(String, String)]| {
        env.iter()
            .find(|(k, _)| k == "SHARED")
            .map(|(_, v)| v.clone())
    };
    assert_eq!(
        seen.last().and_then(|e| shared(e)).as_deref(),
        Some("threads")
    );
    hub.shutdown().await;
}

#[tokio::test]
async fn thread_new_validates_and_generates_names() {
    let dir = tempfile::tempdir().unwrap();
    let mut o = opts(dir.path());
    let proj = dir.path().join("My Proj");
    std::fs::create_dir(&proj).unwrap();
    o.config.projects.insert(
        "api".into(),
        twr_core::config::ProjectConfig {
            repo: proj.clone(),
            runtime: Some("fake".into()),
            box_name: None,
            ..Default::default()
        },
    );
    o.config.default_runtime = "fake".into();
    let hub = Hub::open(o).await.unwrap();
    let mk = |project: &str, name: Option<&str>, aliases: &[&str]| {
        hub.new_thread(
            NewThread {
                project: project.into(),
                name: name.map(str::to_string),
                aliases: aliases.iter().map(|a| a.to_string()).collect(),
                ..Default::default()
            },
            "test",
        )
    };
    let a = mk("api", None, &["x"]).await.unwrap();
    assert_eq!(a["name"], "api-1");
    assert_eq!(a["project"], "api");
    assert_eq!(a["runtime"], "fake");
    assert_eq!(a["mode"], "auto");
    assert_eq!(a["box_name"], "local");
    assert_eq!(a["status"], "idle");
    let p = proj.to_string_lossy();
    let b = mk(&p, None, &[]).await.unwrap();
    assert_eq!(b["name"], "my-proj-1");
    assert_eq!(b["project"], "My Proj");
    assert_eq!(b["worktree"], json!(std::fs::canonicalize(&proj).unwrap()));
    assert_eq!(mk("api", None, &[]).await.unwrap()["name"], "api-2");
    assert_eq!(
        mk("api", Some("x"), &[]).await.unwrap_err().code,
        codes::CONFLICT
    );
    assert_eq!(
        mk("api", Some("api-1"), &[]).await.unwrap_err().code,
        codes::CONFLICT
    );
    assert_eq!(
        mk("api", Some("bad name"), &[]).await.unwrap_err().code,
        codes::BAD_REQUEST
    );
    assert_eq!(
        mk("nowhere", None, &[]).await.unwrap_err().code,
        codes::NOT_FOUND
    );
    assert_eq!(
        mk("relative/dir", None, &[]).await.unwrap_err().code,
        codes::NOT_FOUND
    );
    let e = hub
        .new_thread(
            NewThread {
                project: "api".into(),
                box_name: Some("gpu".into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::BOX_OFFLINE);
    let e = hub
        .new_thread(
            NewThread {
                project: "api".into(),
                runtime: Some("nope".into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST);
    assert_eq!(hub.list_threads().len(), 3);
    // Resolution by alias and prefix.
    assert_eq!(hub.resolve("x").unwrap(), a["id"].as_str().unwrap());
    assert_eq!(hub.resolve("my").unwrap(), b["id"].as_str().unwrap());
    assert_eq!(hub.resolve("api").unwrap_err().code, codes::AMBIGUOUS);
}

#[tokio::test]
async fn concurrent_creations_cannot_share_a_name_or_alias() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let mk = |name: &str, aliases: &[&str]| {
        hub.new_thread(
            NewThread {
                project: dir.path().to_string_lossy().into_owned(),
                runtime: Some("fake".into()),
                name: Some(name.into()),
                aliases: aliases.iter().map(|a| a.to_string()).collect(),
                ..Default::default()
            },
            "test",
        )
    };
    // The first request reserves its name and alias before it awaits its workspace (git);
    // the second one runs meanwhile.
    let (a, b) = tokio::join!(mk("first", &["shared"]), mk("shared", &[]));
    assert!(a.is_ok(), "{a:?}");
    assert_eq!(b.unwrap_err().code, codes::CONFLICT);
    let (c, d) = tokio::join!(mk("second", &["x2"]), mk("third", &["second"]));
    assert!(c.is_ok(), "{c:?}");
    assert_eq!(d.unwrap_err().code, codes::CONFLICT);
    // Released on failure: the refused names are free again.
    assert!(mk("third", &["y3"]).await.is_ok());
    assert_eq!(
        hub.resolve("shared").unwrap(),
        a.unwrap()["id"].as_str().unwrap()
    );
    assert_eq!(hub.list_threads().len(), 3);
}

#[tokio::test]
async fn a_second_hub_on_the_same_data_dir_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let t = new_thread(&hub, dir.path(), "one", "fake").await;
    let err = match Hub::open(opts(dir.path())).await {
        Ok(_) => panic!("a second hub opened the same data dir"),
        Err(e) => e.to_string(),
    };
    assert!(err.contains("another hub is using"), "{err}");
    assert!(
        err.contains(&dir.path().join("data").display().to_string()),
        "{err}"
    );
    // The refused hub wrote nothing.
    assert_eq!(hub.thread_events(&t, 0, 100).unwrap().1.len(), 1);
    drop(hub);
    // Dropping the hub releases the lock. (Retried briefly: a process another test spawns at
    // this very moment holds a copy of every open file until its exec closes them.)
    let hub = timeout(WAIT, async {
        loop {
            match Hub::open(opts(dir.path())).await {
                Ok(h) => return h,
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .expect("the lock is released when the hub is dropped");
    assert_eq!(hub.list_threads().len(), 1);
}

#[tokio::test]
async fn adapter_turn_ended_only_closes_the_open_turn() {
    let dir = tempfile::tempdir().unwrap();
    let o = opts_with(dir.path(), |end, _cfg, _prompt| {
        tokio::spawn(async move {
            end.emit(session("s-1"), None);
            // No turn: in-turn kinds get the open one.
            end.emit(text("partial"), None);
            // A turn that is not open.
            end.emit(ended(TurnResult::Completed), Some(7));
            // No turn: closes the open turn 1.
            end.emit(ended(TurnResult::Completed), None);
            // Duplicates once nothing is open.
            end.emit(ended(TurnResult::Completed), Some(1));
            end.emit(ended(TurnResult::Error), None);
            end.emit(sync_mark(), None);
            until_stopped(end).await;
        });
    });
    let hub = Hub::open(o).await.unwrap();
    let t = new_thread(&hub, dir.path(), "one", "test").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "go");
    let evs = until(&mut sub, is_sync_mark).await;
    assert_eq!(
        kinds(&evs),
        vec![
            "thread.created",
            "turn.started",
            "thread.started",
            "agent.text",
            "turn.ended",
            "decision.made",
            "report.ready",
            "agent.text"
        ]
    );
    assert_eq!(evs[3].turn, Some(1), "stamped with the open turn");
    assert_eq!(evs[4].turn, Some(1));
    let EventBody::TurnEnded(e) = &evs[4].body else {
        panic!()
    };
    assert_eq!(e.result, TurnResult::Completed);
    let EventBody::ReportReady(r) = &evs[6].body else {
        panic!()
    };
    assert_eq!((r.turn, r.grade), (Some(1), Grade::Success));
    assert_eq!(evs[7].turn, None, "nothing open to stamp");
    let v = hub.thread_view(&t).unwrap();
    assert_eq!((v.turns, v.current_turn), (1, None));
    hub.shutdown().await;
}

#[tokio::test]
async fn adapters_cannot_emit_hub_kinds() {
    let dir = tempfile::tempdir().unwrap();
    let o = opts_with(dir.path(), |end, _cfg, _prompt| {
        tokio::spawn(async move {
            end.emit(session("s-1"), None);
            end.emit(
                EventBody::AttentionRaised(AttentionRaised {
                    attn_id: "a1".into(),
                    reason: AttentionReason::Permission,
                    summary: "Allow Bash?".into(),
                    options: vec!["allow".into(), "deny".into()],
                    ..Default::default()
                }),
                None,
            );
            // Only the user resolves attention; reports and decisions are the hub's.
            end.emit(
                EventBody::AttentionResolved(AttentionResolved {
                    attn_id: "a1".into(),
                    by: ResolvedBy::User,
                    answer: Some(Answer::allow()),
                    ..Default::default()
                }),
                None,
            );
            end.emit(
                EventBody::ReportReady(ReportReady {
                    summary: "all good".into(),
                    grade: Grade::Success,
                    ..Default::default()
                }),
                None,
            );
            end.emit(
                EventBody::TurnStarted(TurnStarted {
                    prompt: "self-started".into(),
                    ..Default::default()
                }),
                Some(2),
            );
            end.emit(sync_mark(), None);
            until_stopped(end).await;
        });
    });
    let hub = Hub::open(o).await.unwrap();
    let t = new_thread(&hub, dir.path(), "one", "test").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "go");
    let evs = until(&mut sub, is_sync_mark).await;
    assert_eq!(
        kinds(&evs),
        vec![
            "thread.created",
            "turn.started",
            "thread.started",
            "attention.raised",
            "agent.text"
        ]
    );
    assert_eq!(evs[3].turn, Some(1));
    let v = hub.thread_view(&t).unwrap();
    assert!(
        v.open_attention.contains_key(&format!("{t}:1:a1")),
        "{:?}",
        v.open_attention
    );
    assert_eq!(v.status(), ThreadStatus::Waiting);
    assert_eq!((v.turns, v.current_turn), (1, Some(1)));
    hub.shutdown().await;
}

#[tokio::test]
async fn prompts_sent_while_stopping_run_after_the_stop() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let t = new_thread(&hub, dir.path(), "one", "fake").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();

    // Stop while a turn waits on a permission, then send before the runtime has exited.
    send(&hub, &t, "!perm Bash");
    until(&mut sub, |e| e.kind() == "attention.raised").await;
    hub.stop_thread(&t, "test").unwrap();
    let out = hub
        .send(
            &t,
            "after stop",
            TurnFrom::User,
            Source::User("test".into()),
        )
        .unwrap();
    assert_eq!((out.turn, out.pending), (None, 1));
    let evs = until(&mut sub, |e| e.kind() == "report.ready").await;
    let ks = kinds(&evs);
    let stopped = ks.iter().position(|k| k == "thread.stopped").unwrap();
    let started = ks.iter().position(|k| k == "turn.started").unwrap();
    assert!(stopped < started, "{ks:?}");
    let EventBody::TurnStarted(p) = &evs[started].body else {
        panic!()
    };
    assert_eq!(p.prompt, "after stop");
    assert_eq!(evs[started].turn, Some(2));

    // Stop while idle, then send: the prompt must not go to the runtime that is stopping.
    hub.stop_thread(&t, "test").unwrap();
    let out = hub
        .send(&t, "again", TurnFrom::User, Source::User("test".into()))
        .unwrap();
    assert_eq!((out.turn, out.pending), (None, 1));
    let evs = until(&mut sub, |e| e.kind() == "report.ready").await;
    assert_eq!(
        kinds(&evs)[..3],
        ["thread.stopped", "turn.started", "thread.started"]
    );
    let ended = evs
        .iter()
        .find_map(|e| match &e.body {
            EventBody::TurnEnded(p) => Some(p.result),
            _ => None,
        })
        .unwrap();
    assert_eq!(ended, TurnResult::Completed);
    assert_eq!(hub.get_thread(&t).unwrap()["pending"], 0);
    hub.shutdown().await;
}

#[tokio::test]
async fn a_turn_sent_to_a_hung_up_runtime_ends_with_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let o = opts_with(dir.path(), |end, _cfg, _prompt| {
        tokio::spawn(async move {
            let AdapterEnd { cmds, events, .. } = end;
            // Stops reading commands but has not reported its exit yet.
            drop(cmds);
            let emit = |body, turn| {
                let _ = events.send(twr_core::adapter::AdapterEvent::Emit {
                    body,
                    raw: None,
                    turn,
                });
            };
            emit(session("s-1"), None);
            emit(text("done"), Some(1));
            emit(ended(TurnResult::Completed), Some(1));
            tokio::time::sleep(Duration::from_secs(60)).await;
            drop(events);
        });
    });
    let hub = Hub::open(o).await.unwrap();
    let t = new_thread(&hub, dir.path(), "one", "test").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "first");
    until(&mut sub, |e| e.kind() == "report.ready").await;
    let out = hub
        .send(&t, "second", TurnFrom::User, Source::User("test".into()))
        .unwrap();
    assert_eq!(out.turn, Some(2));
    let evs = until(&mut sub, |e| e.kind() == "report.ready").await;
    assert_eq!(
        kinds(&evs)[..4],
        [
            "turn.started",
            "turn.ended",
            "decision.made",
            "report.ready"
        ]
    );
    let EventBody::TurnEnded(e) = &evs[1].body else {
        panic!()
    };
    assert_eq!(e.result, TurnResult::Error);
    assert_eq!(evs[1].turn, Some(2));
    assert_eq!(evs[1].source, Source::Hub);
    let msg = e.ext.as_ref().unwrap()["twr"]["error"].as_str().unwrap();
    assert!(msg.contains("exited"), "{msg}");
    let EventBody::ReportReady(r) = &evs[3].body else {
        panic!()
    };
    assert_eq!(r.turn, Some(2));
    assert_eq!(hub.thread_view(&t).unwrap().current_turn, None);
}

#[tokio::test]
async fn send_reports_a_dispatch_failure_after_queueing() {
    let dir = tempfile::tempdir().unwrap();
    let o = opts(dir.path());
    let db = o.db_path.clone();
    let hub = Hub::open(o).await.unwrap();
    let t = new_thread(&hub, dir.path(), "one", "fake").await;
    let conn = rusqlite::Connection::open(&db).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER block_dispatch BEFORE DELETE ON pending_prompts
         BEGIN SELECT RAISE(ABORT, 'dispatch blocked'); END;",
    )
    .unwrap();
    let out = hub
        .send(&t, "first", TurnFrom::User, Source::User("test".into()))
        .unwrap();
    assert_eq!((out.turn, out.pending), (None, 1));
    let err = out.dispatch_error.clone().expect("dispatch error reported");
    assert!(err.contains("dispatch blocked"), "{err}");
    let j = serde_json::to_value(&out).unwrap();
    assert_eq!(j["dispatch_error"], json!(err));
    assert!(j["queued"].as_i64().unwrap() > 0);
    assert_eq!(hub.thread_view(&t).unwrap().current_turn, None);

    // Once dispatch works again the queued prompt runs first, in order.
    conn.execute_batch("DROP TRIGGER block_dispatch").unwrap();
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    let out = hub
        .send(&t, "second", TurnFrom::User, Source::User("test".into()))
        .unwrap();
    assert_eq!(out.dispatch_error, None);
    assert!(serde_json::to_value(&out)
        .unwrap()
        .get("dispatch_error")
        .is_none());
    let evs = until(&mut sub, |e| {
        e.turn == Some(2) && e.kind() == "report.ready"
    })
    .await;
    let prompts: Vec<String> = evs
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::TurnStarted(p) => Some(p.prompt.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(prompts, ["first", "second"]);
    hub.shutdown().await;
}

/// Runtime `test` that runs a real `sleep 30` in its own process group with the env the hub
/// passes, and reports its pid.
fn sleeper(dir: &Path, children: Arc<std::sync::Mutex<Vec<std::process::Child>>>) -> HubOptions {
    use std::os::unix::process::CommandExt;
    opts_with(dir, move |end, cfg, _prompt| {
        let child = std::process::Command::new("sleep")
            .arg("30")
            .envs(cfg.env.iter().cloned())
            .process_group(0)
            .spawn()
            .unwrap();
        let pid = child.id();
        children.lock().unwrap().push(child);
        end.emit(
            EventBody::ThreadStarted(SessionPayload {
                session_id: Some("s-sleep".into()),
                // An adapter-supplied start_time is never trusted.
                ext: json!({"twr": {"pid": pid, "start_time": 1}})
                    .as_object()
                    .cloned(),
                ..Default::default()
            }),
            None,
        );
        // Never exits nor closes its channel, like a runtime whose hub crashed: dropping the
        // test's tokio runtime must not let the hub see an exit and close the thread.
        std::mem::forget(end);
    })
}

#[test]
fn a_runtime_that_outlived_its_hub_is_killed_before_the_thread_closes() {
    use std::os::unix::process::ExitStatusExt;

    let dir = tempfile::tempdir().unwrap();
    let children = Arc::new(std::sync::Mutex::new(Vec::new()));
    let thread = {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let t = rt.block_on(async {
            let hub = Hub::open(sleeper(dir.path(), Arc::clone(&children)))
                .await
                .unwrap();
            let t = new_thread(&hub, dir.path(), "one", "test").await;
            let mut sub = hub.subscribe_thread(&t, 0).unwrap();
            send(&hub, &t, "go");
            let evs = until(&mut sub, |e| e.kind() == "thread.started").await;
            let EventBody::ThreadStarted(p) = &evs.last().unwrap().body else {
                panic!()
            };
            let twr = &p.ext.as_ref().unwrap()["twr"];
            let pid = children.lock().unwrap()[0].id();
            assert_eq!(twr["pid"], json!(pid));
            let start = twr_runner::reap::process_start_time(pid).unwrap();
            assert_eq!(twr["start_time"], json!(start), "the hub's reading wins");
            let environ = exec_done(pid, "sleep").await;
            assert!(twr_hub::host::environ_has(&environ, "TWR_THREAD", &t));
            t
        });
        // The hub dies without stopping anything; the runtime process survives.
        drop(rt);
        assert_running(&children);
        t
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let hub = Hub::open(opts(dir.path())).await.unwrap();
        // Open returned only after the survivor exited.
        let status = children.lock().unwrap()[0]
            .try_wait()
            .unwrap()
            .expect("the stale runtime was killed");
        assert_eq!(status.signal(), Some(15));
        let (_, evs) = hub.thread_events(&thread, 0, 100).unwrap();
        let ks = kinds(&evs);
        assert_eq!(
            ks[ks.len() - 4..],
            [
                "turn.ended",
                "thread.crashed",
                "attention.raised",
                "decision.made"
            ]
        );
    });
}

/// Waits until process `pid` runs `program` with its environment in place, and returns that
/// environment.
///
/// `spawn` can return while the child is still inside `execve`: the kernel wakes a vfork
/// parent before it switches the child to the new address space, so for a moment
/// `/proc/<pid>/{cmdline,environ}` (or `ps`) still show the parent's. Later, the new image's environment
/// is empty until the kernel has copied it. Checking `cmdline` first (exec only moves forward)
/// and then a non-empty `environ` avoids both windows.
async fn exec_done(pid: u32, program: &str) -> Vec<u8> {
    timeout(WAIT, async {
        loop {
            let cmdline = twr_runner::reap::process_cmdline(pid).unwrap_or_default();
            if twr_hub::host::argv0_matches(&cmdline, program) {
                let env = twr_runner::reap::process_environ(pid).unwrap_or_default();
                if !env.is_empty() {
                    return env;
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("process {pid} never finished exec'ing {program}"))
}

fn assert_running(children: &std::sync::Mutex<Vec<std::process::Child>>) {
    let mut c = children.lock().unwrap();
    assert!(c[0].try_wait().unwrap().is_none(), "sleep exited early");
}

// --- bounds, caps, start failures, crash recovery ------------------------------------------------

/// Eight 600-byte options: the raised payload is over the cap, so `options` is moved whole.
fn long_options() -> Vec<String> {
    (0..8).map(|i| format!("{i}{}", "o".repeat(599))).collect()
}

#[tokio::test]
async fn thread_new_bounds_names_and_aliases() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let mk = |name: &str, aliases: Vec<String>| {
        hub.new_thread(
            NewThread {
                project: dir.path().to_string_lossy().into_owned(),
                runtime: Some("fake".into()),
                name: Some(name.into()),
                aliases,
                ..Default::default()
            },
            "test",
        )
    };
    let many: Vec<String> = (0..80).map(|i| format!("a{i:0>59}")).collect();
    assert_eq!(
        mk("many", many.clone()).await.unwrap_err().code,
        codes::BAD_REQUEST
    );
    assert_eq!(
        mk("seventeen", many[..17].to_vec()).await.unwrap_err().code,
        codes::BAD_REQUEST
    );
    assert_eq!(
        mk("long", vec!["x".repeat(65)]).await.unwrap_err().code,
        codes::BAD_REQUEST
    );
    assert_eq!(
        mk("bad", vec!["has space".into()]).await.unwrap_err().code,
        codes::BAD_REQUEST
    );
    assert_eq!(
        mk(&"n".repeat(65), vec![]).await.unwrap_err().code,
        codes::BAD_REQUEST
    );
    let v = mk("sixteen", many[..16].to_vec()).await.unwrap();
    assert_eq!(v["aliases"].as_array().unwrap().len(), 16);
    // Every alias resolves, before and after a restart.
    let id = v["id"].as_str().unwrap().to_string();
    assert_eq!(hub.resolve(&many[15]).unwrap(), id);
    drop(hub);
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    for a in &many[..16] {
        assert_eq!(hub.resolve(a).unwrap(), id);
    }
}

#[tokio::test]
async fn capped_question_options_stay_answerable() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let t = new_thread(&hub, dir.path(), "one", "fake").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    let options = long_options();
    send(&hub, &t, &format!("!ask Pick one?|{}", options.join("|")));
    let evs = until(&mut sub, |e| e.kind() == "attention.raised").await;
    let EventBody::AttentionRaised(a) = &evs.last().unwrap().body else {
        panic!()
    };
    // The log and the wire carry the capped payload...
    assert!(a.options.is_empty());
    assert!(a.extra.contains_key("options_ref"));
    // ...but the hub's state holds the full options.
    let got = hub.get_thread(&t).unwrap();
    assert_eq!(got["attention"][0]["options"], json!(options));
    assert_eq!(got["status"], "waiting");

    let e = hub.respond_option(&a.attn_id, 8, None, "test").unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST);
    let ev = hub.respond_option(&a.attn_id, 5, None, "tui").unwrap();
    let EventBody::AttentionResolved(r) = &ev.body else {
        panic!()
    };
    assert_eq!(r.answer.as_ref().unwrap().decision, options[5]);
    let evs = until(&mut sub, |e| e.kind() == "report.ready").await;
    let want = format!("answer: {}", options[5]);
    assert!(evs
        .iter()
        .any(|e| matches!(&e.body, EventBody::AgentText(t) if t.is_final && t.text == want)));
    hub.shutdown().await;
}

#[tokio::test]
async fn capped_control_events_replay_hydrated() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let t = new_thread(&hub, dir.path(), "one", "fake").await;
    let options = long_options();
    let stored = hub
        .append(
            &t,
            None,
            Source::Hub,
            EventBody::AttentionRaised(AttentionRaised {
                attn_id: "r1".into(),
                reason: AttentionReason::Review,
                summary: "s".repeat(2000),
                options: options.clone(),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
    let EventBody::AttentionRaised(a) = &stored.body else {
        panic!()
    };
    assert!(a.options.is_empty(), "stored capped");
    drop(hub);
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let got = hub.get_thread(&t).unwrap();
    assert_eq!(got["attention"][0]["options"], json!(options));
    assert_eq!(got["attention"][0]["summary"], json!("s".repeat(2000)));
    let ev = hub.respond_option("r1", 7, None, "test").unwrap();
    let EventBody::AttentionResolved(r) = &ev.body else {
        panic!()
    };
    assert_eq!(r.answer.as_ref().unwrap().decision, options[7]);
}

/// A `thread.started` that claims the hub capped its `session_id` (`_capped` + a
/// `session_id_ref` to the blob its raw line creates), and a question whose `summary` claims the
/// same: before the fix, live state used the literal values and replay hydrated the others.
fn spoofed(body: EventBody, field: &str, other: &str) -> EventBody {
    let mut data = body.data().unwrap();
    data["_capped"] = json!([field]);
    data[format!("{field}_ref")] = json!(twr_core::blob::blob_ref(other.as_bytes()));
    EventBody::from_kind_data(body.kind(), data).unwrap()
}

/// Capping metadata is the hub's alone. An adapter's `_capped` marker and
/// `<field>_ref` siblings are stripped on ingress, so replay (hydrating the stored events)
/// gives exactly the live state: same session id, same attention item.
#[tokio::test]
async fn adapter_supplied_capping_metadata_cannot_split_replay_from_live() {
    let dir = tempfile::tempdir().unwrap();
    let o = opts_with(dir.path(), |end, _cfg, _prompt| {
        tokio::spawn(async move {
            let emit = |body, raw: &str, turn| {
                end.events
                    .send(AdapterEvent::Emit {
                        body,
                        raw: Some(raw.to_string()),
                        turn,
                    })
                    .unwrap();
            };
            emit(
                spoofed(session("session-"), "session_id", "session-other"),
                "session-other",
                None,
            );
            emit(
                spoofed(permission("p1"), "summary", "Allow Bash? rm -rf /"),
                "Allow Bash? rm -rf /",
                Some(1),
            );
            end.emit(sync_mark(), Some(1));
            until_stopped(end).await;
        });
    });
    let hub = Hub::open(o).await.unwrap();
    let t = new_thread(&hub, dir.path(), "one", "test").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "go");
    until(&mut sub, is_sync_mark).await;
    let live_session = hub.thread_view(&t).unwrap().session_id;
    assert_eq!(live_session.as_deref(), Some("session-"));
    let live_items = hub.attention_list(true);
    assert_eq!(live_items.len(), 1);
    assert_eq!(live_items[0]["summary"], "Allow Bash?");
    assert_eq!(hub.info()["sanitized_events"], 2);
    // What replay folds: every stored event hydrated from the hub's blobs.
    for ev in hub.thread_events(&t, 0, 1000).unwrap().1 {
        let mut data = ev.body.data().unwrap();
        twr_core::blob::hydrate(&mut data, &BlobsOf(&hub)).unwrap();
        match EventBody::from_kind_data(ev.kind(), data).unwrap() {
            EventBody::ThreadStarted(p) => assert_eq!(p.session_id, live_session),
            EventBody::AttentionRaised(a) => {
                assert_eq!(json!(a.summary), live_items[0]["summary"])
            }
            _ => {}
        }
    }
    drop(sub);
    hub.shutdown().await;
    drop(hub);
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    assert_eq!(hub.thread_view(&t).unwrap().session_id, live_session);
    hub.shutdown().await;
}

/// The hub's blobs as a [`twr_core::blob::BlobStore`] (read-only).
struct BlobsOf<'a>(&'a Hub);

impl twr_core::blob::BlobStore for BlobsOf<'_> {
    fn put(&self, _bytes: &[u8]) -> anyhow::Result<String> {
        anyhow::bail!("read-only")
    }
    fn get(&self, blob_ref: &str) -> anyhow::Result<Option<Vec<u8>>> {
        self.0.blob(blob_ref)
    }
}

/// A runtime whose binary goes missing: once `missing` is set, every spawn fails (as a spawn of
/// a missing executable does). Its live sessions can be crashed from the test.
#[derive(Clone, Default)]
struct Vanishing {
    missing: Arc<std::sync::atomic::AtomicBool>,
    sessions: Arc<std::sync::Mutex<Vec<AdapterEnd>>>,
}

impl Adapter for Vanishing {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "vanishing",
            tier: Tier::Headless,
            description: "hub test adapter whose binary goes missing",
        }
    }

    fn spawn(&self, _cfg: SpawnConfig, _prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        if self.missing.load(std::sync::atomic::Ordering::SeqCst) {
            anyhow::bail!("spawning vanishing-cli: No such file or directory (os error 2)");
        }
        let (h, end) = AdapterHandle::pair();
        self.sessions.lock().unwrap().push(end);
        Ok(h)
    }
}

/// A runtime that cannot start fails the turn it was given and stops there. The
/// queue used to be dispatched recursively after each failure (deliver -> try_dispatch ->
/// start_turn -> deliver), overflowing the stack with a few hundred queued prompts; now the
/// rest stays queued behind the failed turn's error item until the user acts.
#[tokio::test]
async fn a_runtime_that_cannot_start_does_not_burn_through_the_queue() {
    let dir = tempfile::tempdir().unwrap();
    let rt = Vanishing::default();
    let mut o = opts(dir.path());
    let base = o.adapters.clone();
    let adapter: Arc<dyn Adapter> = Arc::new(rt.clone());
    o.adapters = Arc::new(move |n: &str| {
        if n == "vanishing" {
            Some(Arc::clone(&adapter))
        } else {
            base(n)
        }
    });
    let hub = Hub::open(o).await.unwrap();
    let t = new_thread(&hub, dir.path(), "one", "vanishing").await;
    send(&hub, &t, "first");
    assert_eq!(hub.thread_view(&t).unwrap().current_turn, Some(1));
    const QUEUED: u64 = 400;
    for i in 0..QUEUED {
        send(&hub, &t, &format!("queued {i}"));
    }
    assert_eq!(hub.get_thread(&t).unwrap()["pending"], QUEUED);
    // The binary goes away and the runtime crashes: nothing is dispatched after a crash
    // except a due automatic retry, which cannot start either.
    rt.missing.store(true, std::sync::atomic::Ordering::SeqCst);
    let end = rt.sessions.lock().unwrap().pop().unwrap();
    end.exited(Some(1), "segfault");
    let h = hub.clone();
    let t2 = t.clone();
    timeout(WAIT, async move {
        while h.get_thread(&t2).unwrap()["live"] == true
            || h.thread_view(&t2).unwrap().current_turn.is_some()
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the crash is closed");
    // The user sends again: the oldest queued prompt gets the attempt, fails, and the rest
    // (with the new one) stays queued.
    let before = hub.thread_view(&t).unwrap().turns;
    let out = hub
        .send(&t, "again", TurnFrom::User, Source::User("test".into()))
        .unwrap();
    // (`turn` names only a turn of the prompt just sent; the queue's head went instead.)
    assert_eq!(out.pending, QUEUED, "{out:?}");
    let view = hub.thread_view(&t).unwrap();
    assert_eq!(view.turns, before + 1);
    assert_eq!(view.current_turn, None);
    let evs = hub.thread_events(&t, 0, 100_000).unwrap().1;
    let failed = evs
        .iter()
        .filter(|e| matches!(&e.body, EventBody::TurnEnded(p) if p.result == TurnResult::Error))
        .count() as u64;
    assert!(failed <= 3, "{failed} turns failed");
    let started = evs.iter().filter(|e| e.kind() == "turn.started").count();
    assert!(started <= 3, "{started} turns started");
    let last = evs
        .iter()
        .rev()
        .find_map(|e| match &e.body {
            EventBody::TurnStarted(p) => Some(p.prompt.clone()),
            _ => None,
        })
        .unwrap();
    assert!(last.starts_with("queued 0") || last == "first", "{last}");
    // The failure is the user's to see.
    let items = hub.get_thread(&t).unwrap()["attention"].clone();
    assert!(
        items
            .as_array()
            .unwrap()
            .iter()
            .any(|i| i["reason"] == "error"),
        "{items}"
    );
    hub.shutdown().await;
}

fn count(evs: &[Event], kind: &str) -> usize {
    evs.iter().filter(|e| e.kind() == kind).count()
}

#[tokio::test]
async fn turn_reactions_lost_in_a_crash_are_recovered_once() {
    let dir = tempfile::tempdir().unwrap();
    let (one, two);
    {
        let hub = Hub::open(opts(dir.path())).await.unwrap();
        one = new_thread(&hub, dir.path(), "one", "fake").await;
        two = new_thread(&hub, dir.path(), "two", "fake").await;
        let fake = Source::Adapter("fake".into());
        // Thread one: the hub died right after recording turn.ended. The give-up phrase is
        // past the capped preview, so only the hydrated final text grades it.
        let final_text = format!("{} {}", "x".repeat(5000), twr_adapters::fake::GIVE_UP_TEXT);
        let started = EventBody::TurnStarted(TurnStarted {
            prompt: "go".into(),
            ..Default::default()
        });
        hub.append(
            &one,
            Some(1),
            Source::User("test".into()),
            started.clone(),
            None,
        )
        .unwrap();
        hub.append(&one, Some(1), fake.clone(), text(&final_text), None)
            .unwrap();
        hub.append(
            &one,
            Some(1),
            fake.clone(),
            ended(TurnResult::Completed),
            None,
        )
        .unwrap();
        // Thread two: it died after the grade and the report, before the review attention.
        hub.append(&two, Some(1), Source::User("test".into()), started, None)
            .unwrap();
        hub.append(&two, Some(1), fake, ended(TurnResult::Error), None)
            .unwrap();
        hub.append(
            &two,
            Some(1),
            Source::Decision("code".into()),
            EventBody::DecisionMade(DecisionMade {
                site: DecisionSite::Grade,
                provider: DecisionProvider::Code,
                chosen: "gave_up".into(),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        hub.append(
            &two,
            Some(1),
            Source::Hub,
            EventBody::ReportReady(ReportReady {
                summary: "Turn 1 failed.".into(),
                grade: Grade::GaveUp,
                turn: Some(1),
                written_by: "hub".into(),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
    }
    for round in 0..2 {
        let hub = Hub::open(opts(dir.path())).await.unwrap();
        let (_, evs) = hub.thread_events(&one, 0, 100).unwrap();
        assert_eq!(count(&evs, "decision.made"), 1, "round {round}");
        assert_eq!(count(&evs, "report.ready"), 1, "round {round}");
        assert_eq!(count(&evs, "attention.raised"), 1, "round {round}");
        let report = evs
            .iter()
            .find_map(|e| match &e.body {
                EventBody::ReportReady(r) => Some(r.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!((report.grade, report.turn), (Grade::GaveUp, Some(1)));
        let review = evs.iter().find(|e| e.kind() == "attention.raised").unwrap();
        assert_eq!(review.turn, Some(1));
        let v = hub.thread_view(&one).unwrap();
        assert!(v.reported);
        assert_eq!(v.open_attention.len(), 1);

        // A failed turn: grade + block_kind decisions, and an error item (not a review).
        let (_, evs) = hub.thread_events(&two, 0, 100).unwrap();
        assert_eq!(count(&evs, "decision.made"), 2, "round {round}");
        assert_eq!(count(&evs, "report.ready"), 1, "round {round}");
        assert_eq!(count(&evs, "attention.raised"), 1, "round {round}");
        let last = evs.last().unwrap();
        let EventBody::AttentionRaised(a) = &last.body else {
            panic!("{last:?}")
        };
        assert_eq!((a.reason, last.turn), (AttentionReason::Error, Some(1)));
    }
}

#[tokio::test]
async fn adapter_thread_stopped_is_dropped_and_exit_closes_the_thread() {
    let dir = tempfile::tempdir().unwrap();
    let o = opts_with(dir.path(), |end, _cfg, _prompt| {
        tokio::spawn(async move {
            end.emit(session("s-1"), None);
            end.emit(
                EventBody::AttentionRaised(AttentionRaised {
                    attn_id: "a1".into(),
                    reason: AttentionReason::Permission,
                    summary: "Allow Bash?".into(),
                    options: vec!["allow".into(), "deny".into()],
                    ..Default::default()
                }),
                None,
            );
            // Would clear the open turn and attention before the closing sequence could.
            end.emit(
                EventBody::ThreadStopped(SessionPayload {
                    session_id: Some("s-1".into()),
                    ..Default::default()
                }),
                None,
            );
            end.emit(sync_mark(), None);
            end.exited(Some(0), "");
        });
    });
    let hub = Hub::open(o).await.unwrap();
    let t = new_thread(&hub, dir.path(), "one", "test").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "go");
    let evs = until(&mut sub, |e| e.kind() == "thread.crashed").await;
    assert_eq!(
        kinds(&evs),
        vec![
            "thread.created",
            "turn.started",
            "thread.started",
            "attention.raised",
            "agent.text",
            "turn.ended",
            "attention.resolved",
            "thread.crashed"
        ]
    );
    let EventBody::TurnEnded(e) = &evs[5].body else {
        panic!()
    };
    assert_eq!(e.result, TurnResult::Interrupted);
    let EventBody::AttentionResolved(r) = &evs[6].body else {
        panic!()
    };
    assert_eq!(
        (r.attn_id.clone(), r.by),
        (format!("{t}:1:a1"), ResolvedBy::ThreadStopped)
    );
    assert!(evs
        .iter()
        .all(|e| e.source != Source::Adapter("test".into()) || e.kind() != "thread.stopped"));
    assert!(!twr_hub::hub::adapter_may_emit(&EventBody::ThreadStopped(
        SessionPayload::default()
    )));
}

#[tokio::test]
async fn oversized_identifiers_and_payloads_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let many_keys = || -> JsonMap {
        (0..400)
            .map(|i| (format!("k{i}"), json!("v")))
            .collect::<serde_json::Map<_, _>>()
    };
    let o = opts_with(dir.path(), move |end, _cfg, _prompt| {
        let ext = many_keys();
        tokio::spawn(async move {
            end.emit(session("s-1"), None);
            // Identifier over 256 bytes.
            end.emit(
                EventBody::AttentionRaised(AttentionRaised {
                    attn_id: "a".repeat(300),
                    reason: AttentionReason::Permission,
                    summary: "Allow?".into(),
                    ..Default::default()
                }),
                None,
            );
            // Hundreds of tiny fields: nothing to move, still over the cap.
            end.emit(
                EventBody::AgentText(AgentText {
                    text: "hi".into(),
                    ext: Some(ext),
                    ..Default::default()
                }),
                None,
            );
            // A refused turn.ended still closes the turn (bare).
            end.emit(
                EventBody::TurnEnded(TurnEnded {
                    result: TurnResult::Completed,
                    session_id: Some("s".repeat(300)),
                    ..Default::default()
                }),
                None,
            );
            end.emit(
                EventBody::AttentionRaised(AttentionRaised {
                    attn_id: "ok1".into(),
                    reason: AttentionReason::Permission,
                    summary: "Allow?".into(),
                    options: vec!["allow".into(), "deny".into()],
                    ..Default::default()
                }),
                None,
            );
            end.emit(sync_mark(), None);
            until_stopped(end).await;
        });
    });
    let hub = Hub::open(o).await.unwrap();
    let t = new_thread(&hub, dir.path(), "one", "test").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "go");
    let evs = until(&mut sub, is_sync_mark).await;
    assert_eq!(
        kinds(&evs),
        vec![
            "thread.created",
            "turn.started",
            "thread.started",
            "turn.ended",
            "decision.made",
            "report.ready",
            "attention.raised",
            "agent.text"
        ]
    );
    let EventBody::TurnEnded(e) = &evs[3].body else {
        panic!()
    };
    assert_eq!(
        (e.result, e.session_id.as_deref()),
        (TurnResult::Completed, None)
    );
    assert_eq!(hub.info()["dropped_events"], 3);
    assert!(evs
        .iter()
        .all(|e| serde_json::to_vec(&e.body.data().unwrap()).unwrap().len() <= 4096));

    // Client requests get bad_request and change nothing.
    let ok1 = format!("{t}:1:ok1");
    let e = hub
        .respond(
            &ok1,
            Answer {
                decision: "allow".into(),
                extra: many_keys(),
                ..Default::default()
            },
            "test",
        )
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST);
    let e = hub
        .respond(&"a".repeat(300), Answer::allow(), "test")
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST);
    assert!(hub
        .thread_view(&t)
        .unwrap()
        .open_attention
        .contains_key(&ok1));
    let err = hub
        .append(
            &t,
            None,
            Source::Hub,
            EventBody::AttentionResolved(AttentionResolved {
                attn_id: "x".repeat(300),
                by: ResolvedBy::User,
                ..Default::default()
            }),
            None,
        )
        .unwrap_err();
    assert!(twr_hub::hub::rejection(&err).is_some(), "{err}");
    hub.shutdown().await;
}

// --- attach lease, per-thread model, runtime-withdrawn attention -------------------------------

fn session_of(evs: &[Event]) -> Option<String> {
    evs.iter().rev().find_map(|e| match &e.body {
        EventBody::ThreadStarted(p) => p.session_id.clone(),
        _ => None,
    })
}

fn after<'a>(evs: &'a [Event], kind: &str) -> &'a [Event] {
    let i = evs.iter().rposition(|e| e.kind() == kind).unwrap();
    &evs[i + 1..]
}

#[tokio::test]
async fn attach_while_idle_quiesces_and_detach_runs_the_queue() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let t = new_thread(&hub, dir.path(), "one", "fake").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();

    // No session yet: nothing to attach to.
    let e = hub.attach_begin("one", false, "cli").await.unwrap_err();
    assert_eq!(e.code, codes::UNSUPPORTED);

    send(&hub, &t, "hello");
    let evs = until(&mut sub, |e| e.kind() == "report.ready").await;
    let session = session_of(&evs).unwrap();
    assert_eq!(hub.live_runtimes(), 1);

    let grant = hub.attach_begin("one", false, "cli").await.unwrap();
    assert_eq!(grant.thread, t);
    assert_eq!(grant.session_id, session);
    assert_eq!(grant.runtime, "fake");
    assert_eq!(grant.argv[0], "sh");
    assert_eq!(grant.argv.last(), Some(&session));
    let project = std::fs::canonicalize(dir.path()).unwrap();
    assert_eq!(grant.cwd.as_deref(), project.to_str());
    // Nothing is set for the user's process but the attach and agent markers (no
    // TWR_THREAD); twr's secrets are removed.
    assert_eq!(
        grant.env["TWR_ATTACH"],
        serde_json::json!(format!("{t}:{}", grant.lease))
    );
    assert_eq!(
        grant.env["TWR_AGENT"],
        serde_json::json!(format!("{t}:attach.{}", grant.lease))
    );
    assert!(
        grant
            .env
            .iter()
            .all(|(k, v)| v.is_null() || k == "TWR_ATTACH" || k == "TWR_AGENT")
            && !grant.env.contains_key("TWR_THREAD"),
        "{:?}",
        grant.env
    );
    // The runtime exited before attach.begin returned; its exit appended thread.attached, not
    // thread.stopped.
    assert_eq!(hub.live_runtimes(), 0);
    let evs = until(&mut sub, |e| e.kind() == "thread.attached").await;
    assert_eq!(kinds(&evs), vec!["thread.attached"]);
    assert_eq!(evs[0].source, Source::User("cli".into()));
    let EventBody::ThreadAttached(p) = &evs[0].body else {
        panic!()
    };
    assert_eq!(p.client, "cli");
    let v = hub.thread_view(&t).unwrap();
    assert_eq!(v.status(), ThreadStatus::Attached);
    assert!(!v.stopped);

    // One lease at a time; stop waits for the attach to end.
    let e = hub.attach_begin("one", true, "other").await.unwrap_err();
    assert_eq!(e.code, codes::CONFLICT);
    assert_eq!(
        hub.stop_thread("one", "test").unwrap_err().code,
        codes::CONFLICT
    );

    // Prompts queue while attached.
    let out = hub
        .send(
            &t,
            "while attached",
            TurnFrom::User,
            Source::User("test".into()),
        )
        .unwrap();
    assert_eq!((out.turn, out.pending), (None, 1));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(hub.thread_view(&t).unwrap().turns, 1);
    assert_eq!(hub.live_runtimes(), 0);

    // A lease that is not the current one ends nothing.
    assert!(!hub
        .attach_end(&t, "cli", grant.lease + 1, Some("x"))
        .unwrap());
    assert!(hub.attach_end(&t, "cli", grant.lease, None).unwrap());
    let evs = until(&mut sub, |e| e.kind() == "report.ready").await;
    assert_eq!(evs[0].kind(), "thread.detached");
    let EventBody::ThreadDetached(p) = &evs[0].body else {
        panic!()
    };
    assert_eq!((p.client.as_str(), p.ext.is_none()), ("cli", true));
    // The queued prompt ran on the same session, resumed lazily.
    let started = evs
        .iter()
        .find_map(|e| match &e.body {
            EventBody::ThreadStarted(p) => Some(p.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(started.session_id.as_deref(), Some(session.as_str()));
    assert_eq!(started.ext.unwrap()["fake"]["resumed"], json!(true));
    let turn2 = evs
        .iter()
        .find_map(|e| match &e.body {
            EventBody::TurnStarted(p) => Some((e.turn, p.prompt.clone())),
            _ => None,
        })
        .unwrap();
    assert_eq!(turn2, (Some(2), "while attached".to_string()));
    // Ending twice ends nothing; neither does the old lease closing later.
    assert!(!hub.attach_end(&t, "cli", grant.lease, None).unwrap());
    assert!(!hub
        .attach_end(&t, "cli", grant.lease, Some("connection_closed"))
        .unwrap());
    let (_, all) = hub.thread_events(&t, 0, 1000).unwrap();
    assert_eq!(count(&all, "thread.stopped"), 0);
    assert_eq!(count(&all, "thread.detached"), 1);
    hub.shutdown().await;
}

#[tokio::test]
async fn attach_during_a_turn_needs_force_and_interrupts_it() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let t = new_thread(&hub, dir.path(), "one", "fake").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "!perm Bash rm -rf build");
    until(&mut sub, |e| e.kind() == "attention.raised").await;

    let e = hub.attach_begin("one", false, "cli").await.unwrap_err();
    assert_eq!(e.code, codes::CONFLICT);
    assert!(e.message.contains("turn 1"), "{}", e.message);
    // Refused without side effects: the turn still waits on the user.
    let v = hub.thread_view(&t).unwrap();
    assert_eq!(v.status(), ThreadStatus::Waiting);
    assert_eq!(hub.live_runtimes(), 1);

    hub.attach_begin("one", true, "cli").await.unwrap();
    let evs = until(&mut sub, |e| e.kind() == "thread.attached").await;
    assert_eq!(
        kinds(&evs),
        vec!["turn.ended", "attention.resolved", "thread.attached"]
    );
    let EventBody::TurnEnded(e) = &evs[0].body else {
        panic!()
    };
    assert_eq!(e.result, TurnResult::Interrupted);
    let EventBody::AttentionResolved(r) = &evs[1].body else {
        panic!()
    };
    assert_eq!(r.by, ResolvedBy::ThreadStopped);
    let v = hub.thread_view(&t).unwrap();
    assert_eq!(v.status(), ThreadStatus::Attached);
    assert!(v.open_attention.is_empty());
    assert_eq!(hub.live_runtimes(), 0);
    hub.shutdown().await;
}

#[tokio::test]
async fn attach_needs_a_runtime_with_an_interactive_cli() {
    let dir = tempfile::tempdir().unwrap();
    let o = opts_with(dir.path(), |end, _cfg, _prompt| {
        tokio::spawn(async move {
            end.emit(session("s-1"), None);
            end.emit(ended(TurnResult::Completed), None);
            until_stopped(end).await;
        });
    });
    let hub = Hub::open(o).await.unwrap();
    let t = new_thread(&hub, dir.path(), "one", "test").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "go");
    until(&mut sub, |e| e.kind() == "report.ready").await;
    let e = hub.attach_begin(&t, false, "cli").await.unwrap_err();
    assert_eq!(e.code, codes::UNSUPPORTED);
    // Refused before anything was stopped.
    assert_eq!(hub.live_runtimes(), 1);
    assert!(hub.thread_view(&t).unwrap().attached.is_none());
    hub.shutdown().await;
}

/// Runtime `slowstop`: every session reports `s-slow` (or the one it resumes) and ends each
/// turn at once, but on `stop` it tells `stopping` and exits only once `release` is notified.
/// Its attach command carries the model and mode it was made from (and says so,
/// `Adapter::attach_applies`).
struct SlowStop {
    stopping: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

impl Adapter for SlowStop {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "slowstop",
            tier: Tier::Headless,
            description: "exits on stop only when the test lets it",
        }
    }

    fn spawn(&self, cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let (h, mut end) = AdapterHandle::pair();
        let (stopping, release) = (self.stopping.clone(), self.release.clone());
        tokio::spawn(async move {
            end.emit(
                session(cfg.resume_session.as_deref().unwrap_or("s-slow")),
                None,
            );
            if prompt.is_some() {
                end.emit(
                    ended(TurnResult::Completed),
                    Some(cfg.prompt_turn.unwrap_or(1)),
                );
            }
            while let Some(cmd) = end.cmds.recv().await {
                match cmd {
                    AdapterCmd::Send { turn, .. } => {
                        end.emit(ended(TurnResult::Completed), Some(turn));
                    }
                    AdapterCmd::Stop | AdapterCmd::Interrupt => {
                        stopping.notify_one();
                        release.notified().await;
                        break;
                    }
                    _ => {}
                }
            }
            end.exited(Some(0), "");
        });
        Ok(h)
    }

    fn attach_command(&self, cfg: &SpawnConfig, session_id: &str) -> Option<std::process::Command> {
        let mut cmd = std::process::Command::new("slow-cli");
        cmd.args([
            "--model",
            cfg.model.as_deref().unwrap_or("default"),
            "--permission-mode",
            &cfg.mode,
            "--resume",
            session_id,
        ]);
        Some(cmd)
    }

    /// The model it passes (`default` for none); no effort.
    fn attach_applies(&self, _cfg: &SpawnConfig) -> AttachApplies {
        AttachApplies {
            model: true,
            effort: false,
        }
    }
}

/// A settings change accepted while `attach.begin` waits for the runtime to exit (after the
/// command was made with the old settings) refuses that attach when it would be granted: the
/// attach recorded is ended again and the retry gets the new settings. `change` makes the
/// change (it must be accepted for after the turn); `expect` is in the retry's command.
async fn a_change_while_the_runtime_stops_refuses_the_attach(
    change: impl FnOnce(&Hub, &str),
    expect: [&str; 2],
) {
    let dir = tempfile::tempdir().unwrap();
    let stopping = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let mut o = opts(dir.path());
    let base = o.adapters.clone();
    let slow: Arc<dyn Adapter> = Arc::new(SlowStop {
        stopping: stopping.clone(),
        release: release.clone(),
    });
    o.adapters = Arc::new(move |n: &str| {
        if n == "slowstop" {
            Some(Arc::clone(&slow))
        } else {
            base(n)
        }
    });
    let hub = Hub::open(o).await.unwrap();
    let t = new_thread(&hub, dir.path(), "one", "slowstop").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "hello");
    until(&mut sub, |e| e.kind() == "report.ready").await;

    let (h, tt) = (hub.clone(), t.clone());
    let attach = tokio::spawn(async move { h.attach_begin(&tt, false, "cli").await });
    // The command is made and the runtime is being stopped: the change is accepted.
    timeout(WAIT, stopping.notified()).await.unwrap();
    change(&hub, &t);
    release.notify_one();
    let e = attach.await.unwrap().unwrap_err();
    assert_eq!(e.code, codes::CONFLICT, "{e:?}");
    assert!(
        e.message.contains("one changed while attaching; try again"),
        "{e:?}"
    );
    // The attach recorded on the runtime's exit is ended again: nothing holds the thread.
    let evs = until(&mut sub, |e| e.kind() == "thread.detached").await;
    let attached = after(&evs, "thread.attached");
    assert_eq!(kinds(attached), vec!["thread.detached"]);
    let EventBody::ThreadDetached(p) = &attached[0].body else {
        panic!()
    };
    assert_eq!(attached[0].source, Source::Hub);
    assert_eq!(
        p.ext.as_ref().unwrap()["twr"]["reason"],
        json!("changed_while_attaching")
    );
    assert!(hub.thread_view(&t).unwrap().attached.is_none());
    assert_eq!(hub.live_runtimes(), 0);

    // The retry's command has the new settings.
    let grant = hub.attach_begin(&t, false, "cli").await.unwrap();
    let argv = grant.argv.join(" ");
    for want in expect {
        assert!(argv.contains(want), "{want} not in {argv}");
    }
    assert!(hub.attach_end(&t, "cli", grant.lease, None).unwrap());
    let (_, all) = hub.thread_events(&t, 0, 1000).unwrap();
    assert_eq!(count(&all, "thread.stopped"), 0);
    hub.shutdown().await;
}

#[tokio::test]
async fn a_model_change_while_the_runtime_stops_refuses_the_attach() {
    a_change_while_the_runtime_stops_refuses_the_attach(
        |hub, t| {
            let r = hub
                .set_model(t, Some("big-model"), None, &Caller::user("test"))
                .unwrap();
            assert_eq!(
                (&r["changed"], &r["applies"]),
                (&json!(true), &json!("after_turn")),
                "{r}"
            );
        },
        ["--model big-model", "--permission-mode auto"],
    )
    .await;
}

#[tokio::test]
async fn a_mode_change_while_the_runtime_stops_refuses_the_attach() {
    a_change_while_the_runtime_stops_refuses_the_attach(
        |hub, t| {
            let r = hub.set_mode(t, "plan", &Caller::user("test")).unwrap();
            assert_eq!(
                (&r["changed"], &r["applies"]),
                (&json!(true), &json!("after_turn")),
                "{r}"
            );
        },
        ["--model default", "--permission-mode plan"],
    )
    .await;
}

#[tokio::test]
async fn a_hub_restart_ends_the_attach_and_runs_the_queue() {
    let dir = tempfile::tempdir().unwrap();
    let t;
    {
        let hub = Hub::open(opts(dir.path())).await.unwrap();
        t = new_thread(&hub, dir.path(), "one", "fake").await;
        let mut sub = hub.subscribe_thread(&t, 0).unwrap();
        send(&hub, &t, "hello");
        until(&mut sub, |e| e.kind() == "report.ready").await;
        hub.attach_begin(&t, false, "cli").await.unwrap();
        send(&hub, &t, "later");
        hub.shutdown().await;
        let (_, evs) = hub.thread_events(&t, 0, 1000).unwrap();
        assert_eq!(evs.last().unwrap().kind(), "thread.attached");
    }
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    let evs = until(&mut sub, |e| {
        e.turn == Some(2) && e.kind() == "report.ready"
    })
    .await;
    let tail = after(&evs, "thread.attached");
    assert_eq!(tail[0].kind(), "thread.detached");
    assert_eq!(tail[0].source, Source::Hub);
    let EventBody::ThreadDetached(p) = &tail[0].body else {
        panic!()
    };
    assert_eq!(p.client, "cli");
    assert_eq!(p.ext.as_ref().unwrap()["twr"]["reason"], "hub_restart");
    // The attach had already closed the runtime: nothing crashed.
    assert_eq!(count(&evs, "thread.crashed"), 0);
    assert_eq!(tail[1].kind(), "turn.started");
    hub.shutdown().await;
}

/// Runtime `test` that records each session's config and answers every turn.
fn recording(dir: &Path, seen: Arc<std::sync::Mutex<Vec<SpawnConfig>>>) -> HubOptions {
    opts_with(dir, move |mut end, cfg, prompt| {
        seen.lock().unwrap().push(cfg.clone());
        let first = prompt.map(|_| cfg.prompt_turn.unwrap_or(1));
        tokio::spawn(async move {
            end.emit(session("s-rec"), None);
            if let Some(t) = first {
                end.emit(ended(TurnResult::Completed), Some(t));
            }
            while let Some(cmd) = end.cmds.recv().await {
                match cmd {
                    AdapterCmd::Send { turn, .. } => {
                        end.emit(ended(TurnResult::Completed), Some(turn));
                    }
                    AdapterCmd::Stop | AdapterCmd::Interrupt => break,
                    AdapterCmd::Respond { .. } => {}
                }
            }
            end.exited(Some(0), "");
        });
    })
}

#[tokio::test]
async fn the_thread_model_and_mode_reach_the_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut o = recording(dir.path(), Arc::clone(&seen));
    o.config.threads.model = Some("cfg-model".into());
    let hub = Hub::open(o).await.unwrap();
    let project = dir.path().to_string_lossy().into_owned();
    let explicit = hub
        .new_thread(
            NewThread {
                project: project.clone(),
                runtime: Some("test".into()),
                name: Some("explicit".into()),
                mode: Some("plan".into()),
                model: Some("opus-x".into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap();
    assert_eq!(explicit["model"], "opus-x");
    let defaulted = hub
        .new_thread(
            NewThread {
                project,
                runtime: Some("test".into()),
                name: Some("defaulted".into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap();
    assert_eq!(defaulted["model"], "cfg-model");
    let (_, evs) = hub.thread_events("explicit", 0, 10).unwrap();
    let EventBody::ThreadCreated(c) = &evs[0].body else {
        panic!()
    };
    assert_eq!(c.ext.as_ref().unwrap()["twr"]["model"], "opus-x");

    for name in ["explicit", "defaulted"] {
        let id = hub.resolve(name).unwrap();
        let mut sub = hub.subscribe_thread(&id, 0).unwrap();
        send(&hub, name, "go");
        until(&mut sub, |e| e.kind() == "report.ready").await;
    }
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen[0].model.as_deref(), Some("opus-x"));
        assert_eq!(seen[0].mode, "plan");
        assert_eq!(seen[1].model.as_deref(), Some("cfg-model"));
        assert_eq!(seen[1].mode, "auto");
    }
    // A resumed session gets the same model.
    hub.stop_thread("explicit", "test").unwrap();
    let id = hub.resolve("explicit").unwrap();
    let mut sub = hub.subscribe_thread(&id, 0).unwrap();
    until(&mut sub, |e| e.kind() == "thread.stopped").await;
    send(&hub, "explicit", "again");
    until(&mut sub, |e| {
        e.turn == Some(2) && e.kind() == "report.ready"
    })
    .await;
    {
        let seen = seen.lock().unwrap();
        assert_eq!(seen[2].resume_session.as_deref(), Some("s-rec"));
        assert_eq!(seen[2].model.as_deref(), Some("opus-x"));
    }
    hub.shutdown().await;
}

fn permission(id: &str) -> EventBody {
    EventBody::AttentionRaised(AttentionRaised {
        attn_id: id.into(),
        reason: AttentionReason::Permission,
        summary: "Allow Bash?".into(),
        options: vec!["allow".into(), "deny".into()],
        ..Default::default()
    })
}

fn withdrawn(id: &str, by: ResolvedBy, answer: Option<Answer>) -> EventBody {
    EventBody::AttentionResolved(AttentionResolved {
        attn_id: id.into(),
        by,
        answer,
        ..Default::default()
    })
}

#[tokio::test]
async fn a_runtime_may_withdraw_only_its_own_open_requests() {
    let dir = tempfile::tempdir().unwrap();
    let o = opts_with(dir.path(), |end, _cfg, _prompt| {
        tokio::spawn(async move {
            end.emit(session("s-1"), None);
            end.emit(permission("a1"), None);
            end.emit(permission("a2"), None);
            // Dropped: an answer, a non-timeout resolution, an unknown item.
            end.emit(
                withdrawn("a1", ResolvedBy::Timeout, Some(Answer::allow())),
                None,
            );
            end.emit(withdrawn("a1", ResolvedBy::ThreadStopped, None), None);
            end.emit(withdrawn("nope", ResolvedBy::Timeout, None), None);
            // Accepted: the runtime cancelled its own request.
            end.emit(withdrawn("a1", ResolvedBy::Timeout, None), None);
            // Dropped: already resolved.
            end.emit(withdrawn("a1", ResolvedBy::Timeout, None), None);
            end.emit(sync_mark(), None);
            until_stopped(end).await;
        });
    });
    let hub = Hub::open(o).await.unwrap();
    let t = new_thread(&hub, dir.path(), "one", "test").await;
    let other = new_thread(&hub, dir.path(), "two", "fake").await;
    // An item of another thread cannot be withdrawn by this runtime either.
    hub.append(
        &other,
        None,
        Source::Adapter("fake".into()),
        permission("b1"),
        None,
    )
    .unwrap();
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "go");
    let evs = until(&mut sub, is_sync_mark).await;
    assert_eq!(
        kinds(&evs),
        vec![
            "thread.created",
            "turn.started",
            "thread.started",
            "attention.raised",
            "attention.raised",
            "attention.resolved",
            "agent.text"
        ]
    );
    let r = &evs[5];
    assert_eq!(r.source, Source::Adapter("test".into()));
    assert_eq!(r.turn, Some(1));
    let EventBody::AttentionResolved(p) = &r.body else {
        panic!()
    };
    assert_eq!(
        (p.attn_id.clone(), p.by, p.answer.is_none()),
        (format!("{t}:1:a1"), ResolvedBy::Timeout, true)
    );
    let v = hub.thread_view(&t).unwrap();
    assert_eq!(
        v.open_attention.keys().cloned().collect::<Vec<_>>(),
        vec![format!("{t}:1:a2")]
    );
    assert!(hub
        .thread_view(&other)
        .unwrap()
        .open_attention
        .contains_key("b1"));
    assert!(twr_hub::hub::adapter_may_emit(&withdrawn(
        "x",
        ResolvedBy::Timeout,
        None
    )));
    assert!(!twr_hub::hub::adapter_may_emit(&withdrawn(
        "x",
        ResolvedBy::User,
        None
    )));
    hub.shutdown().await;
}

#[tokio::test]
async fn runtimes_reusing_an_attention_id_each_get_their_own_item_and_answer() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let a = new_thread(&hub, dir.path(), "one", "numbered").await;
    let b = new_thread(&hub, dir.path(), "two", "numbered").await;
    let mut sub_a = hub.subscribe_thread(&a, 0).unwrap();
    let mut sub_b = hub.subscribe_thread(&b, 0).unwrap();
    send(&hub, &a, "go");
    until(&mut sub_a, |e| e.kind() == "attention.raised").await;
    send(&hub, &b, "go");
    until(&mut sub_b, |e| e.kind() == "attention.raised").await;
    // Both raised `perm-1`: each is its own item, in its thread's namespace.
    let (ia, ib) = (format!("{a}:1:perm-1"), format!("{b}:1:perm-1"));
    let listed: Vec<(String, String)> = hub
        .attention_list(true)
        .iter()
        .map(|i| {
            (
                i["attn_id"].as_str().unwrap().into(),
                i["thread"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert_eq!(listed.len(), 2, "{listed:?}");
    assert!(listed.contains(&(ia.clone(), a.clone())), "{listed:?}");
    assert!(listed.contains(&(ib.clone(), b.clone())), "{listed:?}");
    assert_eq!(hub.info()["dropped_events"], 0);
    // The adapter's own id is not a hub id.
    let e = hub.respond("perm-1", Answer::allow(), "test").unwrap_err();
    assert_eq!(e.code, codes::NOT_FOUND);
    // Each answer reaches its own runtime (as `perm-1`): b finishes, a still waits.
    hub.respond(&ib, Answer::allow(), "test").unwrap();
    until(&mut sub_b, |e| e.kind() == "turn.ended").await;
    assert_eq!(hub.thread_view(&a).unwrap().open_attention.len(), 1);
    assert_eq!(hub.thread_view(&a).unwrap().current_turn, Some(1));
    hub.respond(&ia, Answer::allow(), "test").unwrap();
    until(&mut sub_a, |e| e.kind() == "turn.ended").await;
    for t in [&a, &b] {
        let (_, evs) = hub.thread_events(t, 0, 100).unwrap();
        let answered = evs.iter().any(
            |e| matches!(&e.body, EventBody::AgentText(x) if x.text == "answered" && e.turn == Some(1)),
        );
        assert!(answered, "{:?}", kinds(&evs));
        assert!(hub.thread_view(t).unwrap().open_attention.is_empty());
    }
    hub.shutdown().await;
}

#[tokio::test]
async fn a_withdrawal_resolves_only_its_own_threads_item_of_that_id() {
    let dir = tempfile::tempdir().unwrap();
    let o = opts_with(dir.path(), |mut end, _cfg, prompt| {
        tokio::spawn(async move {
            end.emit(session("s-1"), None);
            end.emit(permission("perm-1"), None);
            end.emit(permission("perm-2"), None);
            if prompt.as_deref() == Some("withdraw") {
                end.emit(withdrawn("perm-2", ResolvedBy::Timeout, None), None);
            }
            end.emit(sync_mark(), None);
            while let Some(cmd) = end.cmds.recv().await {
                match cmd {
                    AdapterCmd::Respond { attn_id, answer } => {
                        end.emit(text(&format!("{attn_id}={}", answer.decision)), None);
                        end.emit(ended(TurnResult::Completed), None);
                    }
                    AdapterCmd::Stop | AdapterCmd::Interrupt => break,
                    AdapterCmd::Send { .. } => {}
                }
            }
            end.exited(Some(0), "");
        });
    });
    let hub = Hub::open(o).await.unwrap();
    let a = new_thread(&hub, dir.path(), "one", "test").await;
    let b = new_thread(&hub, dir.path(), "two", "test").await;
    let mut sub_a = hub.subscribe_thread(&a, 0).unwrap();
    let mut sub_b = hub.subscribe_thread(&b, 0).unwrap();
    send(&hub, &b, "keep");
    until(&mut sub_b, is_sync_mark).await;
    send(&hub, &a, "withdraw");
    let evs = until(&mut sub_a, is_sync_mark).await;
    let open = |t: &str| {
        let mut v: Vec<String> = hub
            .thread_view(t)
            .unwrap()
            .open_attention
            .keys()
            .cloned()
            .collect();
        v.sort();
        v
    };
    assert_eq!(open(&a), vec![format!("{a}:1:perm-1")]);
    assert_eq!(
        open(&b),
        vec![format!("{b}:1:perm-1"), format!("{b}:1:perm-2")]
    );
    let r = evs
        .iter()
        .find(|e| e.kind() == "attention.resolved")
        .unwrap();
    assert_eq!(r.source, Source::Adapter("test".into()));
    let EventBody::AttentionResolved(p) = &r.body else {
        panic!()
    };
    assert_eq!(p.attn_id, format!("{a}:1:perm-2"));
    assert_eq!(hub.info()["dropped_events"], 0);
    // The answers carry the runtime's own id to the right runtime.
    hub.respond(&format!("{b}:1:perm-1"), Answer::deny(), "test")
        .unwrap();
    let evs_b = until(&mut sub_b, |e| e.kind() == "turn.ended").await;
    hub.respond(&format!("{a}:1:perm-1"), Answer::allow(), "test")
        .unwrap();
    let evs_a = until(&mut sub_a, |e| e.kind() == "turn.ended").await;
    let texts = |evs: &[Event]| -> Vec<String> {
        evs.iter()
            .filter_map(|e| match &e.body {
                EventBody::AgentText(x) => Some(x.text.clone()),
                _ => None,
            })
            .collect()
    };
    assert_eq!(texts(&evs_a), vec!["perm-1=allow"]);
    assert_eq!(texts(&evs_b), vec!["perm-1=deny"]);
    hub.shutdown().await;
}

#[tokio::test]
async fn a_finished_turn_closes_the_requests_its_runtime_left_open() {
    let dir = tempfile::tempdir().unwrap();
    let o = opts_with(dir.path(), |end, _cfg, _prompt| {
        tokio::spawn(async move {
            end.emit(session("s-1"), None);
            end.emit(permission("a1"), None);
            end.emit(ended(TurnResult::Completed), None);
            until_stopped(end).await;
        });
    });
    let hub = Hub::open(o).await.unwrap();
    let t = new_thread(&hub, dir.path(), "one", "test").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "go");
    let evs = until(&mut sub, |e| e.kind() == "report.ready").await;
    assert_eq!(
        kinds(&evs)[3..],
        [
            "attention.raised",
            "turn.ended",
            "attention.resolved",
            "decision.made",
            "report.ready"
        ]
    );
    let r = &evs[5];
    assert_eq!((r.source.clone(), r.turn), (Source::Hub, Some(1)));
    let EventBody::AttentionResolved(p) = &r.body else {
        panic!()
    };
    assert_eq!(
        (p.attn_id.clone(), p.by),
        (format!("{t}:1:a1"), ResolvedBy::Timeout)
    );
    let v = hub.thread_view(&t).unwrap();
    assert!(v.open_attention.is_empty());
    assert_eq!(v.status(), ThreadStatus::Done);
    hub.shutdown().await;
}

#[tokio::test]
async fn requests_left_open_by_a_turn_that_ended_before_a_crash_are_closed_once() {
    let dir = tempfile::tempdir().unwrap();
    let t;
    {
        // The log as a hub that died right after recording turn.ended leaves it.
        let hub = Hub::open(opts(dir.path())).await.unwrap();
        t = new_thread(&hub, dir.path(), "one", "fake").await;
        let adapter = Source::Adapter("fake".into());
        hub.append(
            &t,
            Some(1),
            Source::User("test".into()),
            EventBody::TurnStarted(TurnStarted {
                prompt: "go".into(),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        hub.append(&t, Some(1), adapter.clone(), permission("a1"), None)
            .unwrap();
        hub.append(&t, Some(1), adapter, ended(TurnResult::Completed), None)
            .unwrap();
    }
    for round in 0..2 {
        let hub = Hub::open(opts(dir.path())).await.unwrap();
        let (_, evs) = hub.thread_events(&t, 0, 100).unwrap();
        assert_eq!(count(&evs, "attention.resolved"), 1, "round {round}");
        let i = evs
            .iter()
            .position(|e| e.kind() == "attention.resolved")
            .unwrap();
        // Before the recovered reactions.
        assert_eq!(evs[i - 1].kind(), "turn.ended");
        assert_eq!(evs[i + 1].kind(), "decision.made");
        let v = hub.thread_view(&t).unwrap();
        assert!(v.open_attention.is_empty());
        assert_eq!(v.status(), ThreadStatus::Done);
    }
}

/// A second connection to the hub's database, for injecting store failures with triggers.
fn db(dir: &Path) -> rusqlite::Connection {
    let c = rusqlite::Connection::open(dir.join("data").join("twr.db")).unwrap();
    c.busy_timeout(Duration::from_secs(5)).unwrap();
    c
}

/// Answers every prompt sent to it with a final text and `turn.ended`.
async fn answer_prompts(mut end: AdapterEnd) {
    while let Some(cmd) = end.cmds.recv().await {
        match cmd {
            AdapterCmd::Send { turn, .. } => {
                end.emit(text("next answer"), Some(turn));
                end.emit(ended(TurnResult::Completed), Some(turn));
            }
            AdapterCmd::Stop | AdapterCmd::Interrupt => break,
            AdapterCmd::Respond { .. } => {}
        }
    }
    end.exited(Some(0), "");
}

#[tokio::test]
async fn a_transient_store_failure_is_retried_before_the_next_event() {
    let dir = tempfile::tempdir().unwrap();
    let o = opts_with(dir.path(), |end, _cfg, _prompt| {
        tokio::spawn(async move {
            end.emit(session("s-1"), None);
            end.emit(text("first answer"), None);
            end.emit(ended(TurnResult::Completed), None);
            answer_prompts(end).await;
        });
    });
    let hub = Hub::open(o).await.unwrap();
    let t = new_thread(&hub, dir.path(), "one", "test").await;
    let db = db(dir.path());
    db.execute_batch(
        "CREATE TRIGGER inject BEFORE INSERT ON events WHEN NEW.kind = 'turn.ended'
         BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;",
    )
    .unwrap();
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "go");
    send(&hub, &t, "again");
    timeout(WAIT, async {
        while hub.info()["persist_retries"].as_u64().unwrap() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the hub never retried the failed append");
    let v = hub.thread_view(&t).unwrap();
    assert_eq!(v.current_turn, Some(1), "turn.ended is not recorded yet");
    db.execute_batch("DROP TRIGGER inject").unwrap();
    let evs = until(&mut sub, |e| {
        e.kind() == "report.ready" && e.turn == Some(2)
    })
    .await;
    assert_eq!(
        kinds(&evs),
        vec![
            "thread.created",
            "turn.started",
            "thread.started",
            "agent.text",
            "turn.ended",
            "decision.made",
            "report.ready",
            "turn.started",
            "agent.text",
            "turn.ended",
            "decision.made",
            "report.ready",
        ]
    );
    let EventBody::ReportReady(r) = &evs[6].body else {
        panic!()
    };
    assert!(r.summary.contains("first answer"), "{}", r.summary);
    let v = hub.thread_view(&t).unwrap();
    assert_eq!((v.turns, v.current_turn), (2, None));
    assert_eq!(hub.get_thread(&t).unwrap()["pending"], 0);
    assert!(hub.fatal_error().is_none());
    hub.shutdown().await;
}

#[tokio::test]
async fn a_store_that_stays_unwritable_fails_the_hub() {
    let dir = tempfile::tempdir().unwrap();
    let gate = Arc::new(tokio::sync::Notify::new());
    let g = Arc::clone(&gate);
    let o = opts_with(dir.path(), move |end, _cfg, _prompt| {
        let g = Arc::clone(&g);
        tokio::spawn(async move {
            end.emit(session("s-1"), None);
            end.emit(sync_mark(), None);
            g.notified().await;
            end.emit(ended(TurnResult::Completed), None);
            until_stopped(end).await;
        });
    });
    let hub = Hub::open(o).await.unwrap();
    let socket = dir.path().join("hub.sock");
    let listener = twr_hub::server::bind(&socket).await.unwrap();
    let served = tokio::spawn(twr_hub::server::serve(hub.clone(), listener, socket, false));
    let t = new_thread(&hub, dir.path(), "one", "test").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "go");
    until(&mut sub, is_sync_mark).await;
    let db = db(dir.path());
    db.execute_batch(
        "CREATE TRIGGER inject BEFORE INSERT ON events
         BEGIN SELECT RAISE(ABORT, 'injected: disk is gone'); END;",
    )
    .unwrap();
    gate.notify_one();
    let err = timeout(Duration::from_secs(30), served)
        .await
        .expect("the hub kept serving")
        .unwrap()
        .expect_err("serve must fail");
    assert!(format!("{err:#}").contains("disk is gone"), "{err:#}");
    assert!(hub.fatal_error().unwrap().contains("disk is gone"));
    assert_eq!(*hub.state().borrow(), twr_hub::HubState::Stopped);
    assert_eq!(hub.live_runtimes(), 0);
    // Nothing after the failure is in the log; the next hub closes the thread.
    db.execute_batch("DROP TRIGGER inject").unwrap();
    drop((sub, hub));
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let (_, evs) = hub.thread_events(&t, 0, 100).unwrap();
    let k = kinds(&evs);
    assert_eq!(
        k[k.len() - 4..],
        [
            "turn.ended",
            "thread.crashed",
            "attention.raised",
            "decision.made"
        ]
    );
    hub.shutdown().await;
}

fn file_mode(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .unwrap_or_else(|e| panic!("{}: {e}", p.display()))
        .permissions()
        .mode()
        & 0o777
}

#[tokio::test]
async fn the_log_and_its_directory_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let chmod =
        |p: &Path, m: u32| std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap();
    let dir = tempfile::tempdir().unwrap();
    chmod(dir.path(), 0o755);
    let data = dir.path().join("data");
    let files = ["twr.db", "twr.db-wal", "twr.db-shm", "twr.lock"];
    {
        let hub = Hub::open(opts(dir.path())).await.unwrap();
        let t = new_thread(&hub, dir.path(), "one", "fake").await;
        hub.append(&t, None, Source::Hub, text("hello"), None)
            .unwrap();
        assert_eq!(file_mode(&data), 0o700);
        for f in files {
            assert_eq!(file_mode(&data.join(f)), 0o600, "{f}");
        }
        // An older twr left everything public.
        chmod(&data, 0o755);
        for f in files {
            chmod(&data.join(f), 0o644);
        }
        hub.shutdown().await;
    }
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    assert_eq!(file_mode(dir.path()), 0o755, "the parent is left alone");
    assert_eq!(file_mode(&data), 0o700);
    for f in files {
        if data.join(f).exists() {
            assert_eq!(file_mode(&data.join(f)), 0o600, "{f}");
        }
    }
    hub.shutdown().await;
}

// --- recording failures, stops, stalls and runtime kills --------------------------------------

/// A failure injected into every insert of an event of `kind` until the trigger is dropped.
fn inject(db: &rusqlite::Connection, kind: &str) {
    db.execute_batch(&format!(
        "CREATE TRIGGER inject BEFORE INSERT ON events WHEN NEW.kind = '{kind}'
         BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;"
    ))
    .unwrap();
}

async fn until_retried(hub: &Hub) {
    timeout(WAIT, async {
        while hub.info()["persist_retries"].as_u64().unwrap() == 0 {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .expect("the hub never retried the failed append");
}

#[tokio::test]
async fn a_detach_that_cannot_be_recorded_keeps_the_lease() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let t = new_thread(&hub, dir.path(), "one", "fake").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "hello");
    until(&mut sub, |e| e.kind() == "report.ready").await;
    let grant = hub.attach_begin(&t, false, "cli").await.unwrap();
    let db = db(dir.path());
    inject(&db, "thread.detached");
    assert!(hub.attach_end(&t, "cli", grant.lease, None).is_err());
    // Still attached under the same lease: the thread takes nothing, and the lease holder can
    // end the attach once the log can record it.
    assert!(hub.thread_view(&t).unwrap().attached.is_some());
    let out = hub
        .send(&t, "queued", TurnFrom::User, Source::User("test".into()))
        .unwrap();
    assert_eq!(out.turn, None);
    db.execute_batch("DROP TRIGGER inject").unwrap();
    assert!(hub.attach_end(&t, "cli", grant.lease, None).unwrap());
    let evs = until(&mut sub, |e| e.kind() == "turn.started").await;
    assert_eq!(count(&evs, "thread.detached"), 1);
    hub.shutdown().await;
}

#[tokio::test]
async fn nothing_starts_or_closes_the_thread_while_an_exit_is_being_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let spawns = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let gate = Arc::new(tokio::sync::Notify::new());
    let (s, g) = (Arc::clone(&spawns), Arc::clone(&gate));
    let o = opts_with(dir.path(), move |end, _cfg, _prompt| {
        let first = s.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
        let g = Arc::clone(&g);
        tokio::spawn(async move {
            end.emit(session("s-1"), None);
            end.emit(ended(TurnResult::Completed), None);
            if first {
                // Crashes while idle, once told to.
                g.notified().await;
                end.exited(Some(1), "boom");
            } else {
                until_stopped(end).await;
            }
        });
    });
    let hub = Hub::open(o).await.unwrap();
    let t = new_thread(&hub, dir.path(), "one", "test").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "go");
    until(&mut sub, |e| e.kind() == "report.ready").await;
    let db = db(dir.path());
    inject(&db, "thread.crashed");
    gate.notify_one();
    // The crash's closing sequence is retried, the core lock released between attempts.
    until_retried(&hub).await;
    // A prompt meanwhile waits: a runtime started now would be closed by that sequence.
    let out = hub
        .send(&t, "next", TurnFrom::User, Source::User("test".into()))
        .unwrap();
    assert_eq!((out.turn, out.pending), (None, 1));
    assert_eq!(spawns.load(std::sync::atomic::Ordering::SeqCst), 1);
    // A stop meanwhile is not a second closing sequence: the exit's closes the thread as
    // stopped (no crash, no error item).
    let r = hub.stop_thread(&t, "test").unwrap();
    assert_eq!(r["stopping"], json!(true));
    assert_eq!(r["dropped_prompts"], json!(1));
    let evs = until(&mut sub, |e| e.kind() == "thread.stopped").await;
    assert_eq!(count(&evs, "thread.crashed"), 0, "{:?}", kinds(&evs));
    assert_eq!(count(&evs, "attention.raised"), 0, "{:?}", kinds(&evs));
    db.execute_batch("DROP TRIGGER inject").unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(spawns.load(std::sync::atomic::Ordering::SeqCst), 1);
    let v = hub.thread_view(&t).unwrap();
    assert_eq!((v.turns, v.stopped), (1, true));
    assert_eq!(hub.live_runtimes(), 0);
    let (_, all) = hub.thread_events(&t, 0, 1000).unwrap();
    assert_eq!(count(&all, "thread.stopped"), 1);
    hub.shutdown().await;
}

/// Runtime `test` that starts its session and then neither takes turns nor stops.
fn deaf(dir: &Path) -> HubOptions {
    opts_with(dir, |end, _cfg, _prompt| {
        end.emit(session("s-deaf"), None);
        std::mem::forget(end);
    })
}

#[test]
fn a_stop_the_hub_asked_for_survives_a_hub_crash() {
    let dir = tempfile::tempdir().unwrap();
    let thread = {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let t = rt.block_on(async {
            let hub = Hub::open(deaf(dir.path())).await.unwrap();
            let t = new_thread(&hub, dir.path(), "one", "test").await;
            let mut sub = hub.subscribe_thread(&t, 0).unwrap();
            send(&hub, &t, "go");
            until(&mut sub, |e| e.kind() == "thread.started").await;
            let r = hub.stop_thread(&t, "test").unwrap();
            assert_eq!(r["stopping"], json!(true));
            t
        });
        // The hub dies before the runtime's exit is recorded.
        drop(rt);
        t
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let hub = Hub::open(opts(dir.path())).await.unwrap();
        let (_, evs) = hub.thread_events(&thread, 0, 100).unwrap();
        let ks = kinds(&evs);
        // Closed as the stop the user asked for: no crash, error item or retry decision.
        assert_eq!(
            ks[ks.len() - 2..],
            ["turn.ended", "thread.stopped"],
            "{ks:?}"
        );
        assert_eq!(count(&evs, "thread.crashed"), 0);
        assert_eq!(count(&evs, "decision.made"), 0);
        assert!(hub.attention_list(true).is_empty());
        let rows: i64 = db(dir.path())
            .query_row("SELECT COUNT(*) FROM stop_requests", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0);
        hub.shutdown().await;
    });
}

#[tokio::test]
async fn every_ended_turn_without_a_report_gets_one_at_startup() {
    let dir = tempfile::tempdir().unwrap();
    let t;
    {
        let hub = Hub::open(opts(dir.path())).await.unwrap();
        t = new_thread(&hub, dir.path(), "one", "fake").await;
        // Turn 1 ended but its reactions were lost (the hub stopped before recording them)
        // while turn 2 ended and was reported.
        let src = Source::Adapter("fake".into());
        let user = Source::User("test".into());
        let started = |p: &str| {
            EventBody::TurnStarted(TurnStarted {
                prompt: p.into(),
                ..Default::default()
            })
        };
        hub.append(&t, Some(1), user.clone(), started("one"), None)
            .unwrap();
        hub.append(&t, Some(1), src.clone(), text("first answer"), None)
            .unwrap();
        hub.append(&t, Some(1), src.clone(), ended(TurnResult::Completed), None)
            .unwrap();
        hub.append(&t, Some(2), user, started("two"), None).unwrap();
        hub.append(&t, Some(2), src, ended(TurnResult::Completed), None)
            .unwrap();
        let report = EventBody::ReportReady(ReportReady {
            summary: "second".into(),
            grade: Grade::Success,
            turn: Some(2),
            written_by: "hub".into(),
            ..Default::default()
        });
        hub.append(&t, Some(2), Source::Hub, report, None).unwrap();
        hub.shutdown().await;
    }
    let reports = |hub: &Hub| -> Vec<(u64, String)> {
        let (_, evs) = hub.thread_events(&t, 0, 1000).unwrap();
        evs.iter()
            .filter_map(|e| match &e.body {
                EventBody::ReportReady(r) => Some((e.turn.unwrap(), r.summary.clone())),
                _ => None,
            })
            .collect()
    };
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let r = reports(&hub);
    assert_eq!(r.len(), 2, "{r:?}");
    assert_eq!(r[1].0, 1);
    assert!(r[1].1.contains("first answer"), "{}", r[1].1);
    hub.shutdown().await;
    drop(hub);
    // Once: the next start adds nothing.
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    assert_eq!(reports(&hub).len(), 2);
    hub.shutdown().await;
}

#[tokio::test]
async fn a_stalled_item_resolves_in_the_batch_of_the_activity() {
    let dir = tempfile::tempdir().unwrap();
    let mut o = opts(dir.path());
    o.stall_after = Some(Duration::from_millis(200));
    let hub = Hub::open(o).await.unwrap();
    let t = new_thread(&hub, dir.path(), "one", "fake").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "!sleep 1500\ndone");
    let evs = until(&mut sub, |e| e.kind() == "attention.raised").await;
    let EventBody::AttentionRaised(stalled) = &evs.last().unwrap().body else {
        panic!()
    };
    assert_eq!(stalled.reason, AttentionReason::Stalled);
    // The resolution cannot be recorded for a while: the activity waits with it (it is
    // retried as a whole), so the log never has the activity without the resolution.
    let db = db(dir.path());
    inject(&db, "attention.resolved");
    until_retried(&hub).await;
    db.execute_batch("DROP TRIGGER inject").unwrap();
    let evs = until(&mut sub, |e| e.kind() == "attention.resolved").await;
    let ks = kinds(&evs);
    assert_eq!(ks.len(), 2, "the activity, then its resolution: {ks:?}");
    let EventBody::AttentionResolved(r) = &evs[1].body else {
        panic!()
    };
    assert_eq!(
        (r.attn_id.as_str(), r.by),
        (stalled.attn_id.as_str(), ResolvedBy::Timeout)
    );
    assert!(went_on(&r.ext), "its runtime's output: {r:?}");
    assert_eq!(evs[1].seq, evs[0].seq + 1);
    hub.shutdown().await;
}

#[tokio::test]
async fn a_stalled_item_left_open_behind_a_closed_turn_is_resolved_at_startup() {
    let dir = tempfile::tempdir().unwrap();
    let t;
    {
        let hub = Hub::open(opts(dir.path())).await.unwrap();
        t = new_thread(&hub, dir.path(), "one", "fake").await;
        // A log from before the resolution was part of the activity's batch.
        let stalled = EventBody::AttentionRaised(AttentionRaised {
            attn_id: "stall-1".into(),
            reason: AttentionReason::Stalled,
            summary: "No activity for 10 min".into(),
            options: vec!["dismiss".into()],
            ..Default::default()
        });
        hub.append(&t, Some(1), Source::Hub, stalled, None).unwrap();
        hub.shutdown().await;
    }
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    assert!(hub.attention_list(true).is_empty());
    let (_, evs) = hub.thread_events(&t, 0, 100).unwrap();
    let EventBody::AttentionResolved(r) = &evs.last().unwrap().body else {
        panic!("{:?}", kinds(&evs))
    };
    assert_eq!((r.attn_id.as_str(), r.by), ("stall-1", ResolvedBy::Timeout));
    // Its thread's work is gone, not going on.
    assert!(!went_on(&r.ext), "{r:?}");
    hub.shutdown().await;
}

#[tokio::test]
async fn an_answer_to_an_earlier_runtime_never_reaches_the_next_one() {
    let dir = tempfile::tempdir().unwrap();
    let o = opts_with(dir.path(), |mut end, _cfg, prompt| {
        tokio::spawn(async move {
            end.emit(session("s-gen"), None);
            // Every process numbers its requests from 1.
            if prompt.is_some() {
                end.emit(permission("perm-1"), None);
            }
            while let Some(cmd) = end.cmds.recv().await {
                match cmd {
                    AdapterCmd::Send { turn, .. } => {
                        end.emit(permission("perm-1"), Some(turn));
                    }
                    AdapterCmd::Respond { attn_id, answer } => {
                        end.emit(text(&format!("{attn_id}={}", answer.decision)), None);
                        end.emit(ended(TurnResult::Completed), None);
                    }
                    AdapterCmd::Stop | AdapterCmd::Interrupt => break,
                }
            }
            end.exited(Some(0), "");
        });
    });
    let hub = Hub::open(o).await.unwrap();
    let t = new_thread(&hub, dir.path(), "one", "test").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "go");
    let evs = until(&mut sub, |e| e.kind() == "attention.raised").await;
    let first = raised_ids(&evs);
    assert_eq!(first, [format!("{t}:1:perm-1")]);
    hub.stop_thread(&t, "test").unwrap();
    until(&mut sub, |e| e.kind() == "thread.stopped").await;
    send(&hub, &t, "again");
    let evs = until(&mut sub, |e| e.kind() == "attention.raised").await;
    let second = raised_ids(&evs);
    assert_eq!(second, [format!("{t}:2:perm-1")]);
    // The user answers what they saw: the first runtime's request, which is gone.
    let e = hub.respond(&first[0], Answer::allow(), "test").unwrap_err();
    assert_eq!(e.code, codes::CONFLICT);
    assert_eq!(hub.thread_view(&t).unwrap().open_attention.len(), 1);
    hub.respond(&second[0], Answer::deny(), "test").unwrap();
    let evs = until(&mut sub, |e| e.kind() == "turn.ended").await;
    let answered: Vec<&str> = evs
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::AgentText(x) => Some(x.text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(answered, ["perm-1=deny"]);
    hub.shutdown().await;
}

fn raised_ids(evs: &[Event]) -> Vec<String> {
    evs.iter()
        .filter_map(|e| match &e.body {
            EventBody::AttentionRaised(a) => Some(a.attn_id.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn shutdown_kills_a_runtime_that_ignores_its_stop_before_recording_it() {
    let dir = tempfile::tempdir().unwrap();
    let children = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut o = sleeper(dir.path(), Arc::clone(&children));
    o.stop_timeout = Duration::from_millis(300);
    let hub = Hub::open(o).await.unwrap();
    let t = new_thread(&hub, dir.path(), "one", "test").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "go");
    until(&mut sub, |e| e.kind() == "thread.started").await;
    let pid = children.lock().unwrap()[0].id();
    exec_done(pid, "sleep").await;
    hub.shutdown().await;
    // By the time the log says stopped, the process is gone.
    let status = children.lock().unwrap()[0].try_wait().unwrap();
    assert!(status.is_some(), "the stuck runtime was left running");
    let (_, evs) = hub.thread_events(&t, 0, 100).unwrap();
    let ks = kinds(&evs);
    assert_eq!(ks[ks.len() - 2..], ["turn.ended", "thread.stopped"]);
}

#[test]
fn a_runtime_whose_hub_died_before_it_reported_its_process_is_killed() {
    use std::os::unix::process::CommandExt;
    let dir = tempfile::tempdir().unwrap();
    let children = Arc::new(std::sync::Mutex::new(Vec::<std::process::Child>::new()));
    let c = Arc::clone(&children);
    let o = opts_with(dir.path(), move |end, cfg, _prompt| {
        let child = std::process::Command::new("sleep")
            .arg("30")
            .envs(cfg.env.iter().cloned())
            .process_group(0)
            .spawn()
            .unwrap();
        c.lock().unwrap().push(child);
        // Still initializing: it reports nothing, not even its pid.
        std::mem::forget(end);
    });
    {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let hub = Hub::open(o).await.unwrap();
            let t = new_thread(&hub, dir.path(), "one", "test").await;
            send(&hub, &t, "go");
            timeout(WAIT, async {
                while children.lock().unwrap().is_empty() {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            let pid = children.lock().unwrap()[0].id();
            let environ = exec_done(pid, "sleep").await;
            assert!(twr_hub::host::environ_has(&environ, "TWR_THREAD", &t));
        });
        drop(rt);
        assert_running(&children);
    }
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let hub = Hub::open(opts(dir.path())).await.unwrap();
        let status = children.lock().unwrap()[0].try_wait().unwrap();
        assert!(
            status.is_some(),
            "the runtime of the previous hub was left running"
        );
        let rows: i64 = db(dir.path())
            .query_row("SELECT COUNT(*) FROM spawn_intents", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0);
        hub.shutdown().await;
    });
}

/// A runtime whose events the hub cannot record is stopped, but its crash is
/// recorded only once it has exited. Until then the thread takes no new runtime (no two
/// processes in one session and worktree) and is neither attached nor parked.
#[tokio::test]
async fn a_runtime_the_hub_gives_up_on_keeps_its_slot_until_it_exits() {
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    let dir = tempfile::tempdir().unwrap();
    let spawns = Arc::new(AtomicUsize::new(0));
    let alive = Arc::new(AtomicUsize::new(0));
    let most_alive = Arc::new(AtomicUsize::new(0));
    let stop_seen = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let (s, a, m, seen, rel) = (
        Arc::clone(&spawns),
        Arc::clone(&alive),
        Arc::clone(&most_alive),
        Arc::clone(&stop_seen),
        Arc::clone(&release),
    );
    let o = opts_with(dir.path(), move |mut end, _cfg, prompt| {
        let first = s.fetch_add(1, SeqCst) == 0;
        let now = a.fetch_add(1, SeqCst) + 1;
        m.fetch_max(now, SeqCst);
        let (a, seen, rel) = (Arc::clone(&a), Arc::clone(&seen), Arc::clone(&rel));
        tokio::spawn(async move {
            end.emit(session("s-1"), None);
            if prompt.is_some() {
                end.emit(text("first answer"), None);
                end.emit(ended(TurnResult::Completed), None);
            }
            while let Some(cmd) = end.cmds.recv().await {
                match cmd {
                    AdapterCmd::Send { turn, .. } => {
                        end.emit(text("next answer"), Some(turn));
                        end.emit(ended(TurnResult::Completed), Some(turn));
                    }
                    AdapterCmd::Stop if first => {
                        // Slow to stop: still running when the hub has given up on it.
                        seen.notify_one();
                        rel.notified().await;
                        break;
                    }
                    AdapterCmd::Stop | AdapterCmd::Interrupt => break,
                    AdapterCmd::Respond { .. } => {}
                }
            }
            a.fetch_sub(1, SeqCst);
            end.exited(Some(0), "");
        });
    });
    let hub = Hub::open(o).await.unwrap();
    let t = new_thread(&hub, dir.path(), "one", "test").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    // The first turn's reactions can never be recorded: the hub gives the runtime up.
    let db = db(dir.path());
    inject(&db, "report.ready");
    send(&hub, &t, "go");
    timeout(Duration::from_secs(20), stop_seen.notified())
        .await
        .expect("the hub never stopped the runtime");
    db.execute_batch("DROP TRIGGER inject").unwrap();

    // The runtime still runs: nothing starts next to it, and nothing closed it.
    let out = hub
        .send(&t, "next", TurnFrom::User, Source::User("test".into()))
        .unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        most_alive.load(SeqCst),
        1,
        "two runtimes of the thread ran at once"
    );
    assert_eq!((out.turn, out.pending), (None, 1));
    let (_, evs) = hub.thread_events(&t, 0, 1000).unwrap();
    assert_eq!(count(&evs, "thread.crashed"), 0, "{:?}", kinds(&evs));
    assert_eq!(hub.get_thread(&t).unwrap()["live"], json!(true));
    let e = hub.attach_begin(&t, false, "cli").await.unwrap_err();
    assert_eq!(e.code, codes::CONFLICT, "{e:?}");
    let e = hub
        .park_thread(&t, true, false, &twr_hub::Caller::user("test"))
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::CONFLICT, "{e:?}");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(spawns.load(SeqCst), 1);

    // Its exit records the crash (why the hub stopped it) and frees the thread.
    release.notify_one();
    let evs = until(&mut sub, |e| e.kind() == "thread.crashed").await;
    let EventBody::ThreadCrashed(c) = &evs.last().unwrap().body else {
        panic!()
    };
    assert!(c.stderr_tail.contains("could not record"), "{c:?}");
    assert_eq!(hub.live_runtimes(), 0);
    send(&hub, &t, "again");
    until(&mut sub, |e| {
        e.kind() == "report.ready" && e.turn == Some(3)
    })
    .await;
    assert_eq!(spawns.load(SeqCst), 2);
    assert_eq!(
        most_alive.load(SeqCst),
        1,
        "two runtimes of the thread ran at once"
    );
    hub.shutdown().await;
}

/// A real process standing in for a runtime: `sleep 60` with the runtime's environment (its
/// `TWR_THREAD` marker), in the test's process group.
fn marked_sleeper(cfg: &SpawnConfig) -> std::process::Child {
    std::process::Command::new("sleep")
        .arg("60")
        .envs(cfg.env.iter().map(|(k, v)| (k, v)))
        .spawn()
        .unwrap()
}

/// An adapter that goes away without reporting an exit may leave its
/// process running. The hub kills it (identity-checked) before it records the crash.
#[tokio::test]
async fn an_adapter_that_vanishes_has_its_process_killed_before_the_crash_is_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let child: Arc<std::sync::Mutex<Option<std::process::Child>>> = Arc::default();
    let c = Arc::clone(&child);
    let o = opts_with(dir.path(), move |end, cfg, _prompt| {
        *c.lock().unwrap() = Some(marked_sleeper(&cfg));
        tokio::spawn(async move {
            end.emit(session("s-1"), None);
            end.emit(ended(TurnResult::Completed), None);
            tokio::time::sleep(Duration::from_millis(300)).await;
            // Gone without an exit, its process left behind.
            drop(end);
        });
    });
    let hub = Hub::open(o).await.unwrap();
    let t = new_thread(&hub, dir.path(), "one", "test").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "go");
    until(&mut sub, |e| e.kind() == "thread.crashed").await;
    let mut child = child.lock().unwrap().take().unwrap();
    let exited = child.try_wait().unwrap();
    if exited.is_none() {
        let _ = child.kill();
        let _ = child.wait();
    }
    assert!(
        exited.is_some(),
        "the crash was recorded while the runtime's process still ran"
    );
    hub.shutdown().await;
}

/// A runtime the hub gave up on that ignores the stop is killed after the stop
/// timeout; its exit then records the crash.
#[tokio::test]
async fn a_runtime_the_hub_gave_up_on_that_ignores_the_stop_is_killed() {
    let dir = tempfile::tempdir().unwrap();
    let mut o = opts_with(dir.path(), move |mut end, cfg, _prompt| {
        let mut child = marked_sleeper(&cfg);
        tokio::spawn(async move {
            end.emit(session("s-1"), None);
            end.emit(ended(TurnResult::Completed), None);
            // Stops are ignored; the adapter reports its process's exit, whenever it comes.
            let status = loop {
                if let Some(s) = child.try_wait().unwrap() {
                    break s;
                }
                let _ = tokio::time::timeout(Duration::from_millis(50), end.cmds.recv()).await;
            };
            end.exited(status.code(), "killed");
        });
    });
    o.stop_timeout = Duration::from_millis(500);
    let hub = Hub::open(o).await.unwrap();
    let t = new_thread(&hub, dir.path(), "one", "test").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    let db = db(dir.path());
    inject(&db, "report.ready");
    send(&hub, &t, "go");
    let evs = until(&mut sub, |e| e.kind() == "thread.crashed").await;
    db.execute_batch("DROP TRIGGER inject").unwrap();
    let EventBody::ThreadCrashed(c) = &evs.last().unwrap().body else {
        panic!()
    };
    assert!(c.stderr_tail.contains("could not record"), "{c:?}");
    assert_eq!(hub.live_runtimes(), 0);
    hub.shutdown().await;
}

/// The kill of a runtime the hub gave up on is decided (its generation still
/// live) but runs later on the blocking pool. When that runtime exits meanwhile and the next
/// send starts a replacement, the kill must not hit the replacement: it is scoped to the
/// runtime's generation (`TWR_RUNTIME_GEN`), and the thread takes no new runtime until the
/// kill is done.
#[test]
fn the_kill_of_a_given_up_runtime_never_hits_its_replacement() {
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    // One blocking thread, which the test occupies while the hub's kill is queued.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let children = Arc::new(std::sync::Mutex::new(Vec::<std::process::Child>::new()));
    let spawns = Arc::new(AtomicUsize::new(0));
    let stop_seen = Arc::new(tokio::sync::Notify::new());
    let exit_now = Arc::new(tokio::sync::Notify::new());
    let (c, s, seen, exit) = (
        Arc::clone(&children),
        Arc::clone(&spawns),
        Arc::clone(&stop_seen),
        Arc::clone(&exit_now),
    );
    let mut o = opts_with(dir.path(), move |mut end, cfg, prompt| {
        let first = s.fetch_add(1, SeqCst) == 0;
        let gen = cfg
            .env
            .iter()
            .find(|(k, _)| k == "TWR_RUNTIME_GEN")
            .map(|(_, v)| v.clone());
        assert!(gen.is_some(), "no generation marker: {:?}", cfg.env);
        c.lock().unwrap().push(marked_sleeper(&cfg));
        let (seen, exit) = (Arc::clone(&seen), Arc::clone(&exit));
        tokio::spawn(async move {
            end.emit(session("s-1"), None);
            if prompt.is_some() {
                end.emit(ended(TurnResult::Completed), None);
            }
            if first {
                // Ignores the stop; exits (its process gone) when the test says so.
                loop {
                    tokio::select! {
                        cmd = end.cmds.recv() => match cmd {
                            Some(AdapterCmd::Stop) => seen.notify_one(),
                            Some(_) => {}
                            None => break,
                        },
                        _ = exit.notified() => break,
                    }
                }
                end.exited(None, "killed");
                return;
            }
            while let Some(cmd) = end.cmds.recv().await {
                match cmd {
                    AdapterCmd::Send { turn, .. } => {
                        end.emit(text("replacement answer"), Some(turn));
                        end.emit(ended(TurnResult::Completed), Some(turn));
                    }
                    AdapterCmd::Stop | AdapterCmd::Interrupt => break,
                    AdapterCmd::Respond { .. } => {}
                }
            }
            end.exited(Some(0), "");
        });
    });
    o.stop_timeout = Duration::from_millis(300);
    let outcome = rt.block_on(async {
        let hub = Hub::open(o).await.unwrap();
        let t = new_thread(&hub, dir.path(), "one", "test").await;
        let mut sub = hub.subscribe_thread(&t, 0).unwrap();
        let db = db(dir.path());
        inject(&db, "report.ready");
        send(&hub, &t, "go");
        timeout(Duration::from_secs(20), stop_seen.notified())
            .await
            .expect("the hub never stopped the runtime");
        db.execute_batch("DROP TRIGGER inject").unwrap();
        // The blocking pool is busy: the kill decided after the stop timeout waits in line.
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        let blocker = tokio::task::spawn_blocking(move || {
            let _ = blocked.recv();
        });
        tokio::time::sleep(Duration::from_millis(900)).await;
        // Meanwhile the runtime exits on its own: its crash is recorded.
        {
            let mut c = children.lock().unwrap();
            let _ = c[0].kill();
            let _ = c[0].wait();
        }
        exit_now.notify_one();
        until(&mut sub, |e| e.kind() == "thread.crashed").await;
        // The next send, while the kill still waits.
        let out = hub
            .send(&t, "again", TurnFrom::User, Source::User("test".into()))
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let started_early = spawns.load(SeqCst) > 1;
        // The kill runs now; the queued send then starts the replacement.
        release.send(()).unwrap();
        blocker.await.unwrap();
        until(&mut sub, |e| {
            e.kind() == "report.ready" && e.turn == out.turn.or(Some(2))
        })
        .await;
        assert_eq!(spawns.load(SeqCst), 2);
        // Give a kill that would hit the replacement ample time to do so.
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let alive = children.lock().unwrap()[1].try_wait().unwrap().is_none();
        hub.shutdown().await;
        (alive, started_early, out)
    });
    for c in children.lock().unwrap().iter_mut() {
        let _ = c.kill();
        let _ = c.wait();
    }
    let (alive, started_early, out) = outcome;
    assert!(
        alive,
        "the kill of the given-up runtime killed its replacement"
    );
    assert!(
        !started_early,
        "a runtime started while its predecessor's kill was still pending (sent: {out:?})"
    );
}

/// Text an agent controls reaches clients without terminal control characters:
/// attention summaries (newlines kept) and option labels, notes, and report summaries (the
/// hub's fallback from the final text, and a written one) are stored sanitized; an option is
/// answered as stored.
#[tokio::test]
async fn agent_text_shown_to_clients_has_no_control_characters() {
    let dir = tempfile::tempdir().unwrap();
    let o = opts_with(dir.path(), |mut end, _cfg, _prompt| {
        tokio::spawn(async move {
            end.emit(session("s-1"), None);
            end.emit(
                EventBody::AttentionRaised(AttentionRaised {
                    attn_id: "a1".into(),
                    reason: AttentionReason::Question,
                    summary: "\x1b[2JWhich db?\r\nreally\x07\u{9b}".into(),
                    options: vec!["post\x1b[31mgres".into(), "sq\nlite".into()],
                    ..Default::default()
                }),
                None,
            );
            while let Some(cmd) = end.cmds.recv().await {
                match cmd {
                    AdapterCmd::Respond { answer, .. } => {
                        end.emit(
                            text(&format!("\x1b]0;pwned\x07Chose {}.", answer.decision)),
                            None,
                        );
                        end.emit(ended(TurnResult::Completed), None);
                    }
                    AdapterCmd::Stop | AdapterCmd::Interrupt => break,
                    _ => {}
                }
            }
            end.exited(Some(0), "");
        });
    });
    let hub = Hub::open(o).await.unwrap();
    let t = new_thread(&hub, dir.path(), "one", "test").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "go");
    let evs = until(&mut sub, |e| e.kind() == "attention.raised").await;
    let EventBody::AttentionRaised(a) = &evs.last().unwrap().body else {
        unreachable!()
    };
    assert_eq!(a.summary, "[2JWhich db? \nreally");
    assert_eq!(a.options, ["post[31mgres", "sq lite"]);
    let listed = &hub.attention_list(true)[0];
    assert_eq!(listed["summary"], "[2JWhich db? \nreally");
    let note = hub
        .annotate(&a.attn_id, "see\x1b[5m this", &twr_hub::Caller::user("tui"))
        .unwrap();
    let EventBody::AttentionAnnotated(n) = &note.body else {
        unreachable!()
    };
    assert_eq!(n.note, "see[5m this");
    hub.respond(&a.attn_id, Answer::new("post[31mgres"), "test")
        .unwrap();
    let evs = until(&mut sub, |e| e.kind() == "report.ready").await;
    let EventBody::ReportReady(r) = &evs.last().unwrap().body else {
        unreachable!()
    };
    assert_eq!(r.summary, "]0;pwnedChose post[31mgres.");
    let w = hub
        .write_report(
            &t,
            Some(1),
            "\x1b[1mbold\x1b[0m claim\u{85}",
            Grade::Success,
            &twr_hub::Caller::user("tui"),
        )
        .unwrap();
    assert_eq!(w["summary"], "[1mbold[0m claim");
    for e in hub.thread_events(&t, 0, 1000).unwrap().1 {
        let data = e.body.data().unwrap().to_string();
        if matches!(
            e.kind(),
            "attention.raised" | "attention.annotated" | "report.ready"
        ) {
            assert!(
                !data.contains("\\u001b") && !data.contains("\\u0007"),
                "{data}"
            );
        }
    }
    hub.shutdown().await;
}

/// A runtime an adapter ends for a fatal reason (Claude Code's wider mode): it ends the
/// waiting turn with an error, then exits, possibly with code 0, with `twr: <why>` in its
/// stderr tail. Nobody asked it to stop, so the hub records a crash carrying the reason, never
/// a clean stop.
#[tokio::test]
async fn a_fatal_end_with_exit_code_zero_is_a_crash() {
    let dir = tempfile::tempdir().unwrap();
    let o = opts_with(dir.path(), |end, _cfg, _prompt| {
        tokio::spawn(async move {
            end.emit(session("s-1"), None);
            end.emit(
                EventBody::TurnEnded(TurnEnded {
                    result: TurnResult::Error,
                    ext: Some(
                        json!({"twr": {"error": "the session runs in bypassPermissions"}})
                            .as_object()
                            .unwrap()
                            .clone(),
                    ),
                    ..Default::default()
                }),
                None,
            );
            end.exited(
                Some(0),
                "twr: the session runs in bypassPermissions, wider than manual",
            );
        });
    });
    let hub = Hub::open(o).await.unwrap();
    let t = new_thread(&hub, dir.path(), "one", "test").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "go");
    let evs = until(&mut sub, |e| e.kind() == "thread.crashed").await;
    let crash = evs
        .iter()
        .find_map(|e| match &e.body {
            EventBody::ThreadCrashed(c) => Some(c.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(crash.exit_code, Some(0));
    assert!(
        crash.stderr_tail.contains("twr: the session runs in"),
        "{crash:?}"
    );
    assert!(
        !kinds(&evs).contains(&"thread.stopped".to_string()),
        "{:?}",
        kinds(&evs)
    );
    assert_eq!(hub.get_thread(&t).unwrap()["status"], "error");
    hub.shutdown().await;
}
