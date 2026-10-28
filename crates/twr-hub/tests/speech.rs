//! Reading aloud (`speech.*`, `hub::speech`): what a speaker attached over the socket is sent
//! (new final texts of conversation turns, never history, maintenance, thinking or tools;
//! replies and commentary told apart by the turn or by Codex; capped texts whole; a watermark's
//! catch-up and what it missed; the orchestrator followed across switches), requests (their
//! batches whole and in order, also two at once; a delta's seq), controls and status, one
//! speaker per device, the user only, and nothing written to the log.
//!
//! The orchestrator here is a thread whose `thread.created` says so, its events appended by
//! the test (no runtime runs): what the feed reads is exactly what the test wrote. The
//! refusals use a hub with a real orchestrator's connection (its token) and an agent's (a
//! process of the test carrying the agent marker).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::timeout;
use twr_adapters::fake::FakeAdapter;
use twr_core::adapter::{Adapter, AdapterEnd, AdapterHandle, AdapterInfo, SpawnConfig, Tier};
use twr_core::client::{Client, SpeechLink};
use twr_core::config::Config;
use twr_core::event::*;
use twr_core::paths::Paths;
use twr_core::proto::{codes, RpcError};
use twr_core::speech::*;
use twr_hub::{server, Caller, Hub, HubOptions};

const WAIT: Duration = Duration::from_secs(10);
/// The orchestrator thread.
const ORCH: &str = "o1";

struct TestHub {
    _dir: tempfile::TempDir,
    socket: PathBuf,
    db: PathBuf,
    hub: Hub,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

/// A hub serving its socket, with orchestrator thread [`ORCH`] (stopped: nothing runs).
async fn start() -> TestHub {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::with_home(dir.path());
    let listener = server::bind(&paths.socket).await.unwrap();
    let mut o = HubOptions::new(paths.db_file(), Config::default());
    o.socket = Some(paths.socket.clone());
    o.stop_timeout = Duration::from_secs(5);
    // Nothing the hub does on its own meanwhile: no wake, no stall.
    o.wake_after = Some(Duration::ZERO);
    o.stall_after = Some(Duration::ZERO);
    let hub = Hub::open(o).await.unwrap();
    let task = tokio::spawn(server::serve(
        hub.clone(),
        listener,
        paths.socket.clone(),
        false,
    ));
    orchestrator(&hub, ORCH);
    TestHub {
        _dir: dir,
        db: paths.db_file(),
        socket: paths.socket,
        hub,
        task,
    }
}

impl TestHub {
    async fn client(&self) -> Client {
        let mut c = Client::connect(&self.socket).await.unwrap();
        c.hello(json!({"client": "tui"})).await.unwrap();
        c
    }

    async fn call(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        self.client().await.call(method, params).await
    }

    async fn stop(self) {
        self.call("hub.shutdown", Value::Null).await.unwrap();
        self.task.await.unwrap().unwrap();
    }

    /// Events in the log.
    fn events(&self) -> i64 {
        let c = rusqlite::Connection::open(&self.db).unwrap();
        c.query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap()
    }

    /// Removes blob `r` from the hub's store, as if it had been lost.
    fn lose_blob(&self, r: &str) {
        let c = rusqlite::Connection::open(&self.db).unwrap();
        let n = c.execute("DELETE FROM blobs WHERE hash = ?1", [r]).unwrap();
        assert_eq!(n, 1, "{r}");
    }

    /// Waits until no speaker of `device` is attached.
    async fn detached(&self, device: &str) {
        let deadline = Instant::now() + WAIT;
        loop {
            let s = status(self).await;
            if s.speakers.iter().all(|s| s.device != device) {
                return;
            }
            assert!(Instant::now() < deadline, "{device} stays attached");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// An orchestrator thread `id` (a switch to it, once there was one before).
fn orchestrator(hub: &Hub, id: &str) {
    let mut ext = JsonMap::new();
    ext.insert("twr".into(), json!({"role": "orchestrator"}));
    hub.append(
        id,
        None,
        Source::Hub,
        EventBody::ThreadCreated(ThreadCreated {
            name: id.into(),
            box_name: "local".into(),
            runtime: "fake".into(),
            ext: Some(ext),
            ..Default::default()
        }),
        None,
    )
    .unwrap();
    hub.append(
        id,
        None,
        Source::Hub,
        EventBody::ThreadStopped(Default::default()),
        None,
    )
    .unwrap();
}

fn on(hub: &Hub, thread: &str, turn: u64, body: EventBody) -> Event {
    hub.append(thread, Some(turn), Source::Hub, body, None)
        .unwrap()
}

/// Turn `n` of `thread` starts, from `from`, with `twr` as its `ext.twr`.
fn turn(hub: &Hub, thread: &str, n: u64, from: TurnFrom, twr: Option<Value>) -> Event {
    turn_ext(hub, thread, n, from, twr.map(|t| json!({ "twr": t })))
}

/// [`turn`] with `ext` as its `ext`.
fn turn_ext(hub: &Hub, thread: &str, n: u64, from: TurnFrom, ext: Option<Value>) -> Event {
    let ext = ext.map(|e| e.as_object().unwrap().clone());
    on(
        hub,
        thread,
        n,
        EventBody::TurnStarted(TurnStarted {
            prompt: format!("prompt {n}"),
            from,
            ext,
            ..Default::default()
        }),
    )
}

/// A final text of turn `n`, with Codex's `phase` when given.
fn say(hub: &Hub, thread: &str, n: u64, text: &str, phase: Option<&str>) -> Event {
    let ext = phase.map(|p| {
        let mut ext = JsonMap::new();
        ext.insert("codex".into(), json!({"phase": p}));
        ext
    });
    on(
        hub,
        thread,
        n,
        EventBody::AgentText(AgentText {
            text: text.into(),
            is_final: true,
            ext,
            ..Default::default()
        }),
    )
}

fn delta(hub: &Hub, thread: &str, n: u64, text: &str) -> Event {
    on(
        hub,
        thread,
        n,
        EventBody::AgentText(AgentText {
            text: text.into(),
            delta: true,
            ..Default::default()
        }),
    )
}

/// The seq of `thread`'s last event.
fn head(hub: &Hub, thread: &str) -> u64 {
    let (_, events) = hub.thread_events(thread, 0, 100_000).unwrap();
    events.last().map_or(0, |e| e.seq)
}

fn end(hub: &Hub, thread: &str, n: u64) {
    on(hub, thread, n, EventBody::TurnEnded(TurnEnded::default()));
}

/// A user's turn `n` answered with `text`, ended.
fn exchange(hub: &Hub, thread: &str, n: u64, text: &str) -> Event {
    turn(hub, thread, n, TurnFrom::User, None);
    let e = say(hub, thread, n, text, None);
    end(hub, thread, n);
    e
}

/// A speaker of `device` reading everything, live.
fn speaker(device: &str) -> SpeechAttachParams {
    SpeechAttachParams {
        device: device.into(),
        name: format!("{device} speaker"),
        engine: "text".into(),
        voice: None,
        live: true,
        read: default_read(),
        turn_from: TurnsRead::All,
        watermarks: BTreeMap::new(),
        catchup_secs: 120,
    }
}

async fn attach(
    th: &TestHub,
    p: &SpeechAttachParams,
) -> anyhow::Result<(SpeechAttachAck, SpeechLink)> {
    let mut c = Client::connect(&th.socket).await?;
    c.hello(json!({"client": "speak"})).await?;
    SpeechLink::attach(c, p).await
}

async fn next(link: &mut SpeechLink) -> SpeechItem {
    timeout(WAIT, link.next())
        .await
        .expect("timed out waiting for a speech item")
        .expect("the speech stream ended")
        .unwrap()
}

/// The next item, a message saying `text` as `kind` sent for `origin`.
async fn said(link: &mut SpeechLink, text: &str, kind: MsgKind, origin: Origin) -> SpeechItem {
    let item = next(link).await;
    match &item {
        SpeechItem::Msg {
            text: t,
            kind: k,
            origin: o,
            ..
        } => assert_eq!((t.as_str(), *k, *o), (text, kind, origin), "{item:?}"),
        _ => panic!("expected {text:?}, got {item:?}"),
    }
    item
}

/// The `msg` fields a test looks at: thread, seq, text, kind, origin, batch.
fn msg(item: &SpeechItem) -> (&str, u64, &str, MsgKind, Origin, Option<u64>) {
    match item {
        SpeechItem::Msg {
            thread,
            seq,
            text,
            kind,
            origin,
            batch,
            ..
        } => (thread, *seq, text, *kind, *origin, *batch),
        _ => panic!("not a message: {item:?}"),
    }
}

async fn status(th: &TestHub) -> SpeechStatus {
    serde_json::from_value(th.call("speech.status", Value::Null).await.unwrap()).unwrap()
}

fn code(e: &anyhow::Error) -> String {
    e.downcast_ref::<RpcError>()
        .map(|r| r.code.clone())
        .unwrap_or_else(|| format!("{e:#}"))
}

/// Every blob pointer in `v`.
fn refs(v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Object(m) => {
            for (k, c) in m {
                match c {
                    Value::String(s) if k.ends_with("_ref") => out.push(s.clone()),
                    c => refs(c, out),
                }
            }
        }
        Value::Array(a) => a.iter().for_each(|c| refs(c, out)),
        _ => {}
    }
}

fn blobs_of(ev: &Event) -> Vec<String> {
    let mut out = Vec::new();
    refs(&ev.body.data().unwrap(), &mut out);
    out
}

/// A speaker without a watermark gets nothing said before it attached, then each final text
/// as it is said, at once, as `message` (the default reads both kinds): the commentary before
/// a tool call and the reply after it, never a delta, thinking, a tool call or its result.
#[tokio::test]
async fn a_speaker_gets_what_is_said_from_now_on() {
    let th = start().await;
    let h = &th.hub;
    exchange(h, ORCH, 1, "said before");
    let (ack, mut link) = attach(&th, &speaker("mac")).await.unwrap();
    assert_eq!(ack.orchestrator.as_deref(), Some(ORCH));
    assert_eq!(ack.head, head(h, ORCH));

    turn(h, ORCH, 2, TurnFrom::User, None);
    delta(h, ORCH, 2, "Let me ");
    delta(h, ORCH, 2, "look.");
    let first = say(h, ORCH, 2, "Let me look.", None);
    on(
        h,
        ORCH,
        2,
        EventBody::AgentThinking(AgentThinking {
            text: "thinking SECRET".into(),
            ..Default::default()
        }),
    );
    on(
        h,
        ORCH,
        2,
        EventBody::AgentToolCall(AgentToolCall {
            call_id: "c1".into(),
            tool: "Bash".into(),
            input_summary: "ls TOOL".into(),
            ..Default::default()
        }),
    );
    on(
        h,
        ORCH,
        2,
        EventBody::AgentToolResult(AgentToolResult {
            call_id: "c1".into(),
            ok: true,
            output_summary: "files OUTPUT".into(),
            ..Default::default()
        }),
    );
    let second = say(h, ORCH, 2, "Two files.", None);
    end(h, ORCH, 2);

    let item = said(&mut link, "Let me look.", MsgKind::Message, Origin::Live).await;
    assert_eq!(msg(&item).1, first.seq);
    let SpeechItem::Msg {
        turn,
        truncated,
        ts,
        batch,
        thread,
        ..
    } = &item
    else {
        unreachable!()
    };
    assert_eq!(
        (thread.as_str(), *turn, *truncated, *batch),
        (ORCH, Some(2), false, None)
    );
    assert!(
        ts.starts_with(&first.ts.format("%Y-%m-%dT%H:%M").to_string()),
        "{ts}"
    );
    let item = said(&mut link, "Two files.", MsgKind::Message, Origin::Live).await;
    assert_eq!(msg(&item).1, second.seq);
    // Nothing else came between: the next item is the next thing said.
    exchange(h, ORCH, 3, "Next.");
    said(&mut link, "Next.", MsgKind::Message, Origin::Live).await;
    drop(link);
    th.stop().await;
}

/// Maintenance is never read: a handoff and its summary turn, a handoff prompt, a compaction
/// (the user's, an offer's, the hub's idle one; a marker set to anything but `null` or
/// `false`); nor a turn whose
/// `turn.started` is not in the log, a turn the user took back, or an empty text. A notice
/// turn of the orchestrator's and a runtime's follow-up are; with `turn_from = mine`, only the
/// user's own turns are.
#[tokio::test]
async fn maintenance_and_taken_back_turns_are_silent() {
    let th = start().await;
    let h = &th.hub;
    let (_, mut all) = attach(&th, &speaker("all")).await.unwrap();
    let mine = SpeechAttachParams {
        turn_from: TurnsRead::Mine,
        ..speaker("mine")
    };
    let (_, mut mine) = attach(&th, &mine).await.unwrap();
    let mut n = 0;
    for twr in [
        json!({"handoff": "from o0"}),
        json!({"handoff_prompt": true}),
        json!({"compact": true}),
        json!({"compact": true, "compact_by": "idle"}),
        json!({"compact": true, "compact_by": "offer"}),
    ] {
        n += 1;
        turn(h, ORCH, n, TurnFrom::User, Some(twr.clone()));
        say(h, ORCH, n, &format!("SILENT {twr}"), None);
        end(h, ORCH, n);
    }
    // A marker set to false or null is no marker.
    n += 1;
    turn(
        h,
        ORCH,
        n,
        TurnFrom::User,
        Some(json!({"compact": false, "handoff": null})),
    );
    say(h, ORCH, n, "unmarked", None);
    end(h, ORCH, n);
    // A text in a turn that never started.
    n += 1;
    say(h, ORCH, n, "SILENT no start", None);
    // Taken back: what came before and after the withdrawal.
    n += 1;
    let t = n;
    turn(h, ORCH, t, TurnFrom::User, None);
    on(
        h,
        ORCH,
        t,
        EventBody::TurnWithdrawn(TurnWithdrawn {
            msg_ref: 7,
            turn: t,
            ..Default::default()
        }),
    );
    say(h, ORCH, t, "SILENT withdrawn", None);
    end(h, ORCH, t);
    n += 1;
    turn(h, ORCH, n, TurnFrom::User, None);
    say(h, ORCH, n, "", None);
    end(h, ORCH, n);
    n += 1;
    turn(h, ORCH, n, TurnFrom::Orchestrator, None);
    say(h, ORCH, n, "a notice", None);
    end(h, ORCH, n);
    n += 1;
    turn(h, ORCH, n, TurnFrom::Runtime, None);
    say(h, ORCH, n, "a follow-up", None);
    end(h, ORCH, n);
    // A moved thread's next message (its handoff before its prompt, maybe resuming a
    // session): a message, its reply read.
    n += 1;
    turn(
        h,
        ORCH,
        n,
        TurnFrom::User,
        Some(json!({"runtime_handoff": "<twr-handoff>…</twr-handoff>", "resumes": "s1"})),
    );
    say(h, ORCH, n, "after a move", None);
    end(h, ORCH, n);
    n += 1;
    exchange(h, ORCH, n, "mine");

    for text in [
        "unmarked",
        "a notice",
        "a follow-up",
        "after a move",
        "mine",
    ] {
        said(&mut all, text, MsgKind::Message, Origin::Live).await;
    }
    for text in ["unmarked", "after a move", "mine"] {
        said(&mut mine, text, MsgKind::Message, Origin::Live).await;
    }
    drop((all, mine));
    th.stop().await;
}

/// `read` picks the kinds. Without Codex's phase, a speaker reading replies alone gets a
/// turn's last text when the turn ends, one reading commentary alone gets each text once the
/// next one of its turn comes (never the last); with it, each gets its kind at once. A text
/// held when the user takes its turn back is dropped.
#[tokio::test]
async fn replies_and_commentary_are_told_apart() {
    let th = start().await;
    let h = &th.hub;
    let (_, mut both) = attach(&th, &speaker("both")).await.unwrap();
    let replies = SpeechAttachParams {
        read: vec![ReadKind::Replies],
        ..speaker("replies")
    };
    let (_, mut replies) = attach(&th, &replies).await.unwrap();
    let commentary = SpeechAttachParams {
        read: vec![ReadKind::Commentary],
        ..speaker("commentary")
    };
    let (_, mut commentary) = attach(&th, &commentary).await.unwrap();

    // No phase: told by the turn.
    turn(h, ORCH, 1, TurnFrom::User, None);
    say(h, ORCH, 1, "A first look.", None);
    say(h, ORCH, 1, "A second look.", None);
    say(h, ORCH, 1, "The answer.", None);
    end(h, ORCH, 1);
    // Codex says.
    turn(h, ORCH, 2, TurnFrom::User, None);
    say(h, ORCH, 2, "Codex looks.", Some("commentary"));
    say(h, ORCH, 2, "Codex answers.", Some("final_answer"));
    end(h, ORCH, 2);
    for (text, kind) in [
        ("A first look.", MsgKind::Message),
        ("A second look.", MsgKind::Message),
        ("The answer.", MsgKind::Message),
        ("Codex looks.", MsgKind::Commentary),
        ("Codex answers.", MsgKind::Reply),
    ] {
        said(&mut both, text, kind, Origin::Live).await;
    }
    // Taken back with a text held (sent at once to a speaker reading both, which has it
    // before the turn is taken back).
    turn(h, ORCH, 3, TurnFrom::User, None);
    say(h, ORCH, 3, "Held, then taken back.", None);
    said(
        &mut both,
        "Held, then taken back.",
        MsgKind::Message,
        Origin::Live,
    )
    .await;
    on(
        h,
        ORCH,
        3,
        EventBody::TurnWithdrawn(TurnWithdrawn {
            msg_ref: 3,
            turn: 3,
            ..Default::default()
        }),
    );
    end(h, ORCH, 3);
    // And a turn after it.
    turn(h, ORCH, 4, TurnFrom::User, None);
    say(h, ORCH, 4, "Last look.", None);
    say(h, ORCH, 4, "Last answer.", None);
    end(h, ORCH, 4);

    for text in ["Last look.", "Last answer."] {
        said(&mut both, text, MsgKind::Message, Origin::Live).await;
    }
    for text in ["The answer.", "Codex answers.", "Last answer."] {
        said(&mut replies, text, MsgKind::Reply, Origin::Live).await;
    }
    for text in [
        "A first look.",
        "A second look.",
        "Codex looks.",
        "Last look.",
    ] {
        said(&mut commentary, text, MsgKind::Commentary, Origin::Live).await;
    }
    drop((both, replies, commentary));
    th.stop().await;
}

/// A speaker that comes back with its watermark gets exactly what was said while it was away,
/// as catch-up, then the rest live; one already attached for a device makes another of it a
/// `conflict` until it goes. Params a speaker may not send are refused.
#[tokio::test]
async fn a_watermark_resumes_exactly() {
    let th = start().await;
    let h = &th.hub;
    let (ack, mut link) = attach(&th, &speaker("mac")).await.unwrap();
    let heard = exchange(h, ORCH, 1, "heard");
    said(&mut link, "heard", MsgKind::Message, Origin::Live).await;
    assert!(heard.seq > ack.head);
    // One speaker per device.
    let e = attach(&th, &speaker("mac")).await.unwrap_err();
    assert_eq!(code(&e), codes::CONFLICT);
    let other = SpeechAttachParams {
        name: "mac speaker".into(),
        ..speaker("linux")
    };
    let (_, other) = attach(&th, &other).await.unwrap();
    drop(other);
    drop(link);
    th.detached("mac").await;

    exchange(h, ORCH, 2, "away one");
    exchange(h, ORCH, 3, "away two");
    let p = SpeechAttachParams {
        watermarks: BTreeMap::from([(ORCH.to_string(), heard.seq)]),
        ..speaker("mac")
    };
    let (ack, mut link) = attach(&th, &p).await.unwrap();
    assert_eq!(ack.orchestrator.as_deref(), Some(ORCH));
    said(&mut link, "away one", MsgKind::Message, Origin::Catchup).await;
    said(&mut link, "away two", MsgKind::Message, Origin::Catchup).await;
    exchange(h, ORCH, 4, "back");
    said(&mut link, "back", MsgKind::Message, Origin::Live).await;

    for bad in [
        SpeechAttachParams {
            device: "a b".into(),
            ..speaker("x")
        },
        SpeechAttachParams {
            read: vec![],
            ..speaker("x")
        },
        SpeechAttachParams {
            catchup_secs: CATCHUP_MAX + 1,
            ..speaker("x")
        },
        SpeechAttachParams {
            name: "bell\u{7}".into(),
            ..speaker("x")
        },
    ] {
        let e = attach(&th, &bad).await.unwrap_err();
        assert_eq!(code(&e), codes::BAD_REQUEST, "{bad:?}");
    }
    drop(link);
    th.stop().await;
}

/// After a watermark, what was said longer than `catchup_secs` ago is counted in one
/// `missed` (with the newest one's seq), before what came since, which is read.
#[tokio::test]
async fn what_is_too_old_is_counted_as_missed() {
    let th = start().await;
    let h = &th.hub;
    let (ack, link) = attach(&th, &speaker("mac")).await.unwrap();
    drop(link);
    th.detached("mac").await;
    exchange(h, ORCH, 1, "old one");
    let old = exchange(h, ORCH, 2, "old two");
    tokio::time::sleep(Duration::from_millis(1200)).await;
    exchange(h, ORCH, 3, "recent");
    let p = SpeechAttachParams {
        watermarks: BTreeMap::from([(ORCH.to_string(), ack.head)]),
        catchup_secs: 1,
        ..speaker("mac")
    };
    let (_, mut link) = attach(&th, &p).await.unwrap();
    let item = next(&mut link).await;
    assert_eq!(
        item,
        SpeechItem::Missed {
            thread: ORCH.into(),
            count: 2,
            upto: old.seq,
        }
    );
    said(&mut link, "recent", MsgKind::Message, Origin::Catchup).await;
    // All of it missed: counted once the backlog is read, with nothing to say after it.
    drop(link);
    th.detached("mac").await;
    let p = SpeechAttachParams {
        watermarks: BTreeMap::from([(ORCH.to_string(), ack.head)]),
        catchup_secs: 0,
        ..speaker("mac")
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    let (_, mut link) = attach(&th, &p).await.unwrap();
    let SpeechItem::Missed { count, .. } = next(&mut link).await else {
        panic!("expected missed");
    };
    assert_eq!(count, 3);
    drop(link);
    th.stop().await;
}

/// A text too long for an event (capped: its blob holds the rest) is read whole; past
/// `TEXT_MAX` it is cut on a character boundary and `truncated`; one whose blob is gone is its
/// preview, `truncated`.
#[tokio::test]
async fn a_capped_text_is_read_whole() {
    let th = start().await;
    let h = &th.hub;
    let (_, mut link) = attach(&th, &speaker("mac")).await.unwrap();
    let long: String = (0..2000).map(|i| format!("word{i} ")).collect();
    turn(h, ORCH, 1, TurnFrom::User, None);
    let stored = say(h, ORCH, 1, &long, None);
    assert!(!blobs_of(&stored).is_empty(), "not capped");
    let item = next(&mut link).await;
    let SpeechItem::Msg {
        text, truncated, ..
    } = &item
    else {
        panic!("{item:?}")
    };
    assert_eq!((text.as_str(), *truncated), (long.as_str(), false));

    let huge = "é".repeat(TEXT_MAX); // twice TEXT_MAX bytes
    say(h, ORCH, 1, &huge, None);
    let SpeechItem::Msg {
        text, truncated, ..
    } = next(&mut link).await
    else {
        panic!()
    };
    assert!(truncated);
    assert!(
        text.len() <= TEXT_MAX && text.len() > TEXT_MAX - 2,
        "{}",
        text.len()
    );
    assert!(huge.starts_with(&text));

    // Its blob lost: the preview, truncated (asked for: the feed read it already).
    let lost: String = (0..2000).map(|i| format!("lost{i} ")).collect();
    let stored = say(h, ORCH, 1, &lost, None);
    end(h, ORCH, 1);
    next(&mut link).await;
    for r in blobs_of(&stored) {
        th.lose_blob(&r);
    }
    th.call("speech.request", json!({"seq": stored.seq}))
        .await
        .unwrap();
    assert!(matches!(
        next(&mut link).await,
        SpeechItem::Ctl {
            op: CtlOp::Replace,
            ..
        }
    ));
    let SpeechItem::Msg {
        text, truncated, ..
    } = next(&mut link).await
    else {
        panic!()
    };
    assert!(truncated);
    assert!(lost.starts_with(&text) && text.len() < lost.len(), "{text}");
    drop(link);
    th.stop().await;
}

/// A turn's start too big for an event is read whole before it is judged: a marker moved to
/// the blob store still silences it, and so does a part of `ext` whose blob is gone (it could
/// be a marker); one with nothing of the sort is read, also when its long prompt is lost.
#[tokio::test]
async fn a_capped_turn_start_is_judged_whole() {
    let th = start().await;
    let h = &th.hub;
    let (ack, link) = attach(&th, &speaker("mac")).await.unwrap();
    drop(link);
    th.detached("mac").await;
    let big = |tag: &str| -> String { (0..900).map(|i| format!("{tag}{i} ")).collect() };
    // Many small fields: what holds them moves whole.
    let many = |tag: &str, marker: Option<(&str, Value)>| -> Value {
        let mut m: serde_json::Map<String, Value> = (0..400)
            .map(|i| (format!("k{i}"), json!(format!("{tag}{i}"))))
            .collect();
        if let Some((k, v)) = marker {
            m.insert(k.into(), v);
        }
        Value::Object(m)
    };
    let twr = |t: Value| json!({ "twr": t });
    let handoff = || Some(("handoff", json!("from o0")));
    let starts = [
        ("read: a long note", twr(json!({"note": big("a")})), false),
        (
            "SILENT: a lost long note",
            twr(json!({"note": big("b")})),
            true,
        ),
        (
            "SILENT: a long handoff",
            twr(json!({"handoff": big("c")})),
            false,
        ),
        ("read: many fields", twr(many("d", None)), false),
        (
            "SILENT: many fields, a lost one",
            twr(many("e", None)),
            true,
        ),
        (
            "SILENT: many fields with a handoff",
            twr(many("f", handoff())),
            false,
        ),
    ];
    for (i, (text, ext, lose)) in starts.iter().enumerate() {
        let n = i as u64 + 1;
        let start = turn_ext(h, ORCH, n, TurnFrom::User, Some(ext.clone()));
        let blobs = blobs_of(&start);
        assert!(!blobs.is_empty(), "{text}: not capped");

        if *lose {
            for r in blobs {
                th.lose_blob(&r);
            }
        }
        say(h, ORCH, n, text, None);
        end(h, ORCH, n);
    }
    // A long prompt lost says nothing of the turn (`ext` itself never moves).
    let start = on(
        h,
        ORCH,
        9,
        EventBody::TurnStarted(TurnStarted {
            prompt: big("p"),
            from: TurnFrom::User,
            ..Default::default()
        }),
    );
    let blobs = blobs_of(&start);
    assert!(!blobs.is_empty(), "the prompt is not capped");
    blobs.iter().for_each(|r| th.lose_blob(r));
    say(h, ORCH, 9, "read: a lost long prompt", None);
    end(h, ORCH, 9);
    exchange(h, ORCH, 10, "read: plain");
    let p = SpeechAttachParams {
        watermarks: BTreeMap::from([(ORCH.to_string(), ack.head)]),
        ..speaker("mac")
    };
    let (_, mut link) = attach(&th, &p).await.unwrap();
    for text in [
        "read: a long note",
        "read: many fields",
        "read: a lost long prompt",
        "read: plain",
    ] {
        said(&mut link, text, MsgKind::Message, Origin::Catchup).await;
    }
    // And live, the same.
    let start = turn(
        h,
        ORCH,
        11,
        TurnFrom::User,
        Some(json!({"handoff": big("g")})),
    );
    assert!(!blobs_of(&start).is_empty());
    say(h, ORCH, 11, "SILENT: live handoff", None);
    end(h, ORCH, 11);
    exchange(h, ORCH, 12, "read: live");
    said(&mut link, "read: live", MsgKind::Message, Origin::Live).await;
    // A request skips them too.
    th.call("speech.request", json!({"last": 50}))
        .await
        .unwrap();
    let SpeechItem::Ctl { value, .. } = next(&mut link).await else {
        panic!()
    };
    assert_eq!(value.unwrap()["count"], 5);
    for text in [
        "read: a long note",
        "read: many fields",
        "read: a lost long prompt",
        "read: plain",
        "read: live",
    ] {
        said(&mut link, text, MsgKind::Message, Origin::Request).await;
    }
    drop(link);
    th.stop().await;
}

/// `speech.request last N`: a `replace` naming the batch and its count, then the last N
/// messages, oldest first, of the request's origin and batch, to every speaker attached (or
/// the one asked); `seq` reads one, by its final text's seq or a delta's. Refused: neither or
/// both of `last` and `seq`, `last` out of range, a seq that is no message, one still being
/// written, a speaker not attached. With none attached, it reaches none.
#[tokio::test]
async fn a_request_reads_the_last_messages() {
    let th = start().await;
    let h = &th.hub;
    let r = th.call("speech.request", json!({"last": 3})).await.unwrap();
    assert_eq!(r, json!({"count": 0, "speakers": 0, "thread": ORCH}));
    let mut seqs = Vec::new();
    for n in 1..=4 {
        seqs.push(exchange(h, ORCH, n, &format!("message {n}")).seq);
    }
    // A handoff's is not among them.
    turn(h, ORCH, 5, TurnFrom::User, Some(json!({"compact": true})));
    let compacted = say(h, ORCH, 5, "compacted", None).seq;
    end(h, ORCH, 5);
    let (_, mut a) = attach(&th, &speaker("a")).await.unwrap();
    let (_, mut b) = attach(&th, &speaker("b")).await.unwrap();

    let r = th.call("speech.request", json!({"last": 3})).await.unwrap();
    assert_eq!(r, json!({"count": 3, "speakers": 2, "thread": ORCH}));
    for link in [&mut a, &mut b] {
        let SpeechItem::Ctl {
            op: CtlOp::Replace,
            value: Some(v),
        } = next(link).await
        else {
            panic!()
        };
        assert_eq!(v["count"], 3);
        let batch = v["batch"].as_u64().unwrap();
        for n in 2..=4 {
            let item = said(
                link,
                &format!("message {n}"),
                MsgKind::Message,
                Origin::Request,
            )
            .await;
            assert_eq!(msg(&item).1, seqs[n - 1]);
            assert_eq!(msg(&item).5, Some(batch));
        }
    }

    // One speaker, one message: by its final's seq, or a delta's.
    turn(h, ORCH, 6, TurnFrom::User, None);
    let first_delta = delta(h, ORCH, 6, "Strea");
    delta(h, ORCH, 6, "med.");
    let streamed = say(h, ORCH, 6, "Streamed.", None);
    end(h, ORCH, 6);
    said(&mut a, "Streamed.", MsgKind::Message, Origin::Live).await;
    said(&mut b, "Streamed.", MsgKind::Message, Origin::Live).await;
    for seq in [first_delta.seq, streamed.seq] {
        let r = th
            .call("speech.request", json!({"seq": seq, "speaker": "b"}))
            .await
            .unwrap();
        assert_eq!(r["count"], 1);
        assert_eq!(r["speakers"], 1);
        next(&mut b).await;
        let item = said(&mut b, "Streamed.", MsgKind::Message, Origin::Request).await;
        assert_eq!(msg(&item).1, streamed.seq);
    }
    // By name too, and another thread's by name.
    let r = th
        .call(
            "speech.request",
            json!({"seq": seqs[0], "speaker": "a speaker", "thread": ORCH}),
        )
        .await
        .unwrap();
    assert_eq!(r["speakers"], 1);
    next(&mut a).await;
    said(&mut a, "message 1", MsgKind::Message, Origin::Request).await;

    turn(h, ORCH, 7, TurnFrom::User, None);
    let unfinished = delta(h, ORCH, 7, "still");
    for (p, want) in [
        (json!({}), codes::BAD_REQUEST),
        (json!({"last": 2, "seq": seqs[0]}), codes::BAD_REQUEST),
        (json!({"last": 0}), codes::BAD_REQUEST),
        (json!({"last": REQUEST_MAX + 1}), codes::BAD_REQUEST),
        (json!({"seq": 1}), codes::BAD_REQUEST),
        (json!({"seq": unfinished.seq}), codes::BAD_REQUEST),
        (json!({"seq": 9999}), codes::NOT_FOUND),
        (json!({"last": 1, "speaker": "nobody"}), codes::NOT_FOUND),
        (json!({"last": 1, "thread": "nothing"}), codes::NOT_FOUND),
        (json!({"last": 1, "unknown": 1}), codes::BAD_REQUEST),
    ] {
        let e = th.call("speech.request", p.clone()).await.unwrap_err();
        assert_eq!(code(&e), want, "{p}");
    }
    // The compacted turn's text (whatever its seq) is refused by seq too.
    let e = th
        .call("speech.request", json!({"seq": compacted}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::BAD_REQUEST);
    drop((a, b));
    th.stop().await;
}

/// A request reads back past pages of texts that are not read aloud (here a page and more of
/// compactions' texts after the messages asked for), and no further than it needs.
#[tokio::test]
async fn a_request_pages_past_silent_texts() {
    let th = start().await;
    let h = &th.hub;
    for n in 1..=3 {
        exchange(h, ORCH, n, &format!("message {n}"));
    }
    for n in 4..=80 {
        turn(h, ORCH, n, TurnFrom::User, Some(json!({"compact": true})));
        say(h, ORCH, n, "compacted", None);
        end(h, ORCH, n);
    }
    let mut att = h
        .speech_attach(speaker("mac"), &Caller::user("speak"))
        .unwrap();
    let p = SpeechRequestParams {
        last: Some(2),
        ..Default::default()
    };
    let r = h.speech_request(&p, &Caller::user("tui")).await.unwrap();
    assert_eq!((r.count, r.speakers), (2, 1));
    taken(&mut att).await;
    for n in 2..=3 {
        assert_eq!(msg(&taken(&mut att).await).2, format!("message {n}"));
    }
    drop(att);
    th.stop().await;
}

/// Requests at once never interleave on a speaker, nor with what its feed sends meanwhile:
/// each one's `replace` is followed by its own messages only, whole and in order, also when
/// the speaker's queue is full and they wait their turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requests_at_once_do_not_interleave() {
    let th = start().await;
    let h = &th.hub;
    for n in 1..=50 {
        exchange(h, ORCH, n, &format!("message {n}"));
    }
    // In memory: its queue is not read until every request is under way, so they fill it and
    // wait (more items than it holds).
    let mut att = h
        .speech_attach(speaker("mac"), &Caller::user("speak"))
        .unwrap();
    let asks: Vec<_> = (0..30u32)
        .map(|i| {
            let hub = th.hub.clone();
            tokio::spawn(async move {
                let p = SpeechRequestParams {
                    last: Some(REQUEST_MAX - i % 5),
                    ..Default::default()
                };
                hub.speech_request(&p, &Caller::user("tui")).await.unwrap()
            })
        })
        .collect();
    for n in 51..=60 {
        exchange(h, ORCH, n, &format!("live {n}"));
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut batches = std::collections::HashSet::new();
    let mut live = 0;
    while batches.len() < asks.len() || live < 10 {
        match taken(&mut att).await {
            SpeechItem::Ctl {
                op: CtlOp::Replace,
                value: Some(v),
            } => {
                let batch = v["batch"].as_u64().unwrap();
                let count = v["count"].as_u64().unwrap();
                assert!(count >= u64::from(REQUEST_MAX - 4), "{v}");
                let mut last = 0;
                for _ in 0..count {
                    let item = taken(&mut att).await;
                    let (_, seq, _, _, origin, b) = msg(&item);
                    assert_eq!((origin, b), (Origin::Request, Some(batch)), "{item:?}");
                    assert!(seq > last, "{item:?}");
                    last = seq;
                }
                assert!(batches.insert(batch), "batch {batch} twice");
            }
            SpeechItem::Msg {
                origin: Origin::Live,
                batch: None,
                ..
            } => live += 1,
            item => panic!("{item:?}"),
        }
    }
    for a in asks {
        let r = timeout(WAIT, a).await.unwrap().unwrap();
        assert_eq!(r.speakers, 1, "{r:?}");
    }
    drop(att);
    th.stop().await;
}

/// The next item of an attachment made in memory.
async fn taken(att: &mut twr_hub::hub::SpeechAttachment) -> SpeechItem {
    timeout(WAIT, att.next()).await.unwrap().unwrap()
}

/// `speech.control` sends a control to every speaker attached, or the one named (by name or
/// device); a value out of range, `replace`, or a speaker not attached is refused.
/// `speech.state` on a speaker's connection is what `speech.status` shows of it (as it
/// attached until then), and on another connection a `bad_request`; a speaker that goes
/// leaves the status.
#[tokio::test]
async fn controls_state_and_status() {
    let th = start().await;
    let (_, mut a) = attach(&th, &speaker("a")).await.unwrap();
    let mut b_params = speaker("b");
    b_params.voice = Some("Samantha".into());
    b_params.live = false;
    let (_, mut b) = attach(&th, &b_params).await.unwrap();

    let r = th
        .call("speech.control", json!({"op": "stop"}))
        .await
        .unwrap();
    assert_eq!(r, json!({"speakers": 2}));
    for link in [&mut a, &mut b] {
        assert_eq!(
            next(link).await,
            SpeechItem::Ctl {
                op: CtlOp::Stop,
                value: None
            }
        );
    }
    let r = th
        .call(
            "speech.control",
            json!({"op": "rate", "value": 220, "speaker": "b"}),
        )
        .await
        .unwrap();
    assert_eq!(r, json!({"speakers": 1}));
    assert_eq!(
        next(&mut b).await,
        SpeechItem::Ctl {
            op: CtlOp::Rate,
            value: Some(json!(220))
        }
    );
    let r = th
        .call(
            "speech.control",
            json!({"op": "live", "value": true, "speaker": "a speaker"}),
        )
        .await
        .unwrap();
    assert_eq!(r, json!({"speakers": 1}));
    assert_eq!(
        next(&mut a).await,
        SpeechItem::Ctl {
            op: CtlOp::Live,
            value: Some(json!(true))
        }
    );
    for (p, want) in [
        (json!({"op": "rate", "value": 50}), codes::BAD_REQUEST),
        (json!({"op": "live"}), codes::BAD_REQUEST),
        (json!({"op": "stop", "value": 1}), codes::BAD_REQUEST),
        (
            json!({"op": "replace", "value": {"batch": 1, "count": 0}}),
            codes::BAD_REQUEST,
        ),
        (json!({"op": "louder"}), codes::BAD_REQUEST),
        (json!(null), codes::BAD_REQUEST),
        (json!({"op": "skip", "speaker": "c"}), codes::NOT_FOUND),
    ] {
        let e = th.call("speech.control", p.clone()).await.unwrap_err();
        assert_eq!(code(&e), want, "{p}");
    }

    let s = status(&th).await;
    // The TUI asks with `{}`: the same answer.
    let asked: SpeechStatus =
        serde_json::from_value(th.call("speech.status", json!({})).await.unwrap()).unwrap();
    assert_eq!(asked, s);
    let shown: Vec<_> = s
        .speakers
        .iter()
        .map(|s| {
            (
                s.device.as_str(),
                s.name.as_str(),
                s.engine.as_str(),
                s.state.live,
                s.state.voice.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        shown,
        [
            ("a", "a speaker", "text", true, None),
            ("b", "b speaker", "text", false, Some("Samantha")),
        ]
    );
    let state = SpeechStateParams {
        playing: Some(Playing {
            thread: ORCH.into(),
            seq: 12,
            chunk: 2,
            of: 5,
        }),
        queued: 3,
        paused: true,
        live: true,
        voice: Some("Daniel".into()),
        rate: 200,
        error: Some("spd-say\u{1b}[31m is gone\nfor good".into()),
        ..Default::default()
    };
    b.state(&state).await.unwrap();
    let deadline = Instant::now() + WAIT;
    let got = loop {
        let s = status(&th).await;
        if s.speakers[1].state.queued == 3 {
            break s.speakers[1].state.clone();
        }
        assert!(Instant::now() < deadline, "the state never showed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(
        got,
        SpeechStateParams {
            error: Some("spd-say[31m is gone for good".into()),
            ..state
        }
    );
    // Not a speaker's connection.
    let e = th
        .call("speech.state", json!({"queued": 1}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::BAD_REQUEST);
    drop(b);
    th.detached("b").await;
    let s = status(&th).await;
    assert_eq!(s.speakers.len(), 1);
    assert_eq!(s.speakers[0].device, "a");
    drop(a);
    th.stop().await;
}

/// A new orchestrator (a switch, a reset) is followed: `orchestrator` names it, then what it
/// says is read, and nothing more of the one before.
#[tokio::test]
async fn the_feed_follows_a_new_orchestrator() {
    let th = start().await;
    let h = &th.hub;
    let (_, mut link) = attach(&th, &speaker("mac")).await.unwrap();
    exchange(h, ORCH, 1, "from o1");
    said(&mut link, "from o1", MsgKind::Message, Origin::Live).await;
    orchestrator(h, "o2");
    exchange(h, "o2", 1, "from o2");
    assert_eq!(
        next(&mut link).await,
        SpeechItem::Orchestrator {
            thread: "o2".into()
        }
    );
    let item = next(&mut link).await;
    let (thread, _, text, _, origin, _) = msg(&item);
    assert_eq!((thread, text), ("o2", "from o2"));
    assert!(matches!(origin, Origin::Live | Origin::Catchup), "{item:?}");
    exchange(h, ORCH, 2, "o1 again");
    exchange(h, "o2", 2, "o2 again");
    let item = said(&mut link, "o2 again", MsgKind::Message, Origin::Live).await;
    assert_eq!(msg(&item).0, "o2");
    drop(link);
    th.stop().await;
}

/// A switch right after a speaker attached (before its feed ran) is seen, and so is what the
/// new orchestrator says at once; with no orchestrator at attach, the first one is followed.
#[tokio::test]
async fn a_switch_right_after_attach_is_followed() {
    let th = start().await;
    let h = &th.hub;
    let mut att = h
        .speech_attach(speaker("mac"), &Caller::user("speak"))
        .unwrap();
    assert_eq!(att.ack().orchestrator.as_deref(), Some(ORCH));
    orchestrator(h, "o2");
    exchange(h, "o2", 1, "at once");
    let item = timeout(WAIT, att.next()).await.unwrap().unwrap();
    assert_eq!(
        item,
        SpeechItem::Orchestrator {
            thread: "o2".into()
        }
    );
    let item = timeout(WAIT, att.next()).await.unwrap().unwrap();
    assert_eq!(msg(&item).2, "at once");
    drop(att);
    th.stop().await;

    // No orchestrator yet.
    let dir = tempfile::tempdir().unwrap();
    let mut o = HubOptions::new(dir.path().join("twr.db"), Config::default());
    o.wake_after = Some(Duration::ZERO);
    o.stall_after = Some(Duration::ZERO);
    let hub = Hub::open(o).await.unwrap();
    let mut att = hub
        .speech_attach(speaker("mac"), &Caller::user("speak"))
        .unwrap();
    assert_eq!(att.ack(), &SpeechAttachAck::default());
    orchestrator(&hub, "first");
    exchange(&hub, "first", 1, "hello");
    let item = timeout(WAIT, att.next()).await.unwrap().unwrap();
    assert_eq!(
        item,
        SpeechItem::Orchestrator {
            thread: "first".into()
        }
    );
    let item = timeout(WAIT, att.next()).await.unwrap().unwrap();
    assert_eq!(msg(&item).2, "hello");
    drop(att);
    hub.shutdown().await;
}

/// A new orchestrator is read live, never aged by `catchup_secs` (that rule is the attach's):
/// with `catchup_secs = 0`, a reply logged before the feed saw the switch is a message, not
/// `missed`; a watermark the attach gave for the new thread is still where it starts.
#[tokio::test]
async fn a_new_orchestrator_is_not_aged() {
    let th = start().await;
    let h = &th.hub;
    // o2's thread.created and thread.stopped, then "skipped" (3 to 5): its watermark.
    let p = SpeechAttachParams {
        catchup_secs: 0,
        watermarks: BTreeMap::from([("o2".to_string(), 5)]),
        ..speaker("mac")
    };
    let mut att = h.speech_attach(p, &Caller::user("speak")).unwrap();
    // All of it before the feed runs (one thread of runtime): logged before the switch is seen.
    orchestrator(h, "o2");
    exchange(h, "o2", 1, "skipped");
    assert_eq!(head(h, "o2"), 5);
    exchange(h, "o2", 2, "right after the switch");
    // Older than `catchup_secs` by the time the feed reads it (a blocking sleep: the feed
    // does not run meanwhile).
    std::thread::sleep(Duration::from_millis(20));
    let item = timeout(WAIT, att.next()).await.unwrap().unwrap();
    assert_eq!(
        item,
        SpeechItem::Orchestrator {
            thread: "o2".into()
        }
    );
    let item = timeout(WAIT, att.next()).await.unwrap().unwrap();
    assert_eq!(
        msg(&item).2,
        "right after the switch",
        "{item:?} (aged into missed?)"
    );
    // And a switch the feed sees at once, with no watermark: from the start, live.
    tokio::time::sleep(Duration::from_millis(50)).await;
    orchestrator(h, "o3");
    exchange(h, "o3", 1, "from o3");
    let item = timeout(WAIT, att.next()).await.unwrap().unwrap();
    assert_eq!(
        item,
        SpeechItem::Orchestrator {
            thread: "o3".into()
        }
    );
    let item = timeout(WAIT, att.next()).await.unwrap().unwrap();
    assert_eq!(msg(&item).2, "from o3", "{item:?}");
    drop(att);
    th.stop().await;
}

/// A switch first handles what the thread left logged already, and a text a speaker reading
/// replies alone held is sent as its turn's reply before `orchestrator`: logged with the
/// switch (seen first, the control stream going first), or held well before it.
#[tokio::test]
async fn a_switch_sends_the_held_reply_first() {
    let th = start().await;
    let h = &th.hub;
    let p = SpeechAttachParams {
        read: vec![ReadKind::Replies],
        ..speaker("mac")
    };
    let mut att = h.speech_attach(p, &Caller::user("speak")).unwrap();
    // Before the feed runs: the text, then the switch.
    turn(h, ORCH, 1, TurnFrom::User, None);
    say(h, ORCH, 1, "The last of o1.", None);
    orchestrator(h, "o2");
    let item = timeout(WAIT, att.next()).await.unwrap().unwrap();
    let (thread, _, text, kind, origin, _) = msg(&item);
    assert_eq!(
        (thread, text, kind, origin),
        (ORCH, "The last of o1.", MsgKind::Reply, Origin::Live)
    );
    let item = timeout(WAIT, att.next()).await.unwrap().unwrap();
    assert_eq!(
        item,
        SpeechItem::Orchestrator {
            thread: "o2".into()
        }
    );
    // Held by the feed (it ran meanwhile), then a reset.
    turn(h, "o2", 1, TurnFrom::User, None);
    say(h, "o2", 1, "The last of o2.", None);
    tokio::time::sleep(Duration::from_millis(100)).await;
    orchestrator(h, "o3");
    let item = timeout(WAIT, att.next()).await.unwrap().unwrap();
    let (thread, _, text, kind, _, _) = msg(&item);
    assert_eq!(
        (thread, text, kind),
        ("o2", "The last of o2.", MsgKind::Reply)
    );
    let item = timeout(WAIT, att.next()).await.unwrap().unwrap();
    assert_eq!(
        item,
        SpeechItem::Orchestrator {
            thread: "o3".into()
        }
    );
    // The old turn's end, after the switch: nothing more of o2.
    end(h, "o2", 1);
    exchange(h, "o3", 1, "o3 answers");
    let item = timeout(WAIT, att.next()).await.unwrap().unwrap();
    let (thread, _, text, kind, origin, _) = msg(&item);
    assert_eq!(
        (thread, text, kind, origin),
        ("o3", "o3 answers", MsgKind::Reply, Origin::Live)
    );
    drop(att);
    th.stop().await;
}

/// A store read failing on the orchestrator's subscription does not silence the speaker for
/// good: the feed subscribes again after a pause, after the last event it handled, and goes
/// on (here the catch-up the failed read was for, then live).
#[tokio::test]
async fn a_failed_read_is_retried() {
    let th = start().await;
    let h = &th.hub;
    let (ack, link) = attach(&th, &speaker("mac")).await.unwrap();
    drop(link);
    th.detached("mac").await;
    exchange(h, ORCH, 1, "away");
    // The catch-up after the watermark is read from the store: make that read fail.
    let c = rusqlite::Connection::open(&th.db).unwrap();
    c.busy_timeout(Duration::from_secs(5)).unwrap();
    c.execute_batch("ALTER TABLE events RENAME TO events_hidden")
        .unwrap();
    let p = SpeechAttachParams {
        watermarks: BTreeMap::from([(ORCH.to_string(), ack.head)]),
        ..speaker("mac")
    };
    let mut att = h.speech_attach(p, &Caller::user("speak")).unwrap();
    // The feed runs (and fails its read) while this sleeps.
    assert!(timeout(Duration::from_millis(300), att.next())
        .await
        .is_err());
    c.execute_batch("ALTER TABLE events_hidden RENAME TO events")
        .unwrap();
    let item = timeout(WAIT, att.next()).await.unwrap().unwrap();
    let (_, _, text, _, origin, _) = msg(&item);
    assert_eq!((text, origin), ("away", Origin::Catchup), "{item:?}");
    exchange(h, ORCH, 2, "back");
    let item = timeout(WAIT, att.next()).await.unwrap().unwrap();
    let (_, _, text, _, origin, _) = msg(&item);
    assert_eq!((text, origin), ("back", Origin::Live), "{item:?}");
    drop(att);
    th.stop().await;
}

/// Nothing a speaker or the user does with `speech.*` is recorded: the log has as many events
/// after as before. A speaker's stream ends when the hub stops.
#[tokio::test]
async fn reading_aloud_writes_nothing_to_the_log() {
    let th = start().await;
    for n in 1..=3 {
        exchange(&th.hub, ORCH, n, &format!("message {n}"));
    }
    let before = th.events();
    let (_, mut link) = attach(&th, &speaker("mac")).await.unwrap();
    link.state(&SpeechStateParams {
        queued: 1,
        ..Default::default()
    })
    .await
    .unwrap();
    th.call("speech.request", json!({"last": 2})).await.unwrap();
    th.call("speech.control", json!({"op": "pause"}))
        .await
        .unwrap();
    status(&th).await;
    for _ in 0..4 {
        next(&mut link).await;
    }
    assert_eq!(th.events(), before);
    drop(link);
    th.detached("mac").await;
    assert_eq!(th.events(), before);

    let (_, mut link) = attach(&th, &speaker("mac")).await.unwrap();
    th.stop().await;
    let end = timeout(WAIT, link.next()).await.unwrap();
    assert!(end.is_none(), "{end:?}");
}

// ---- The user only ----

/// The `speech.*` methods.
const SPEECH: [&str; 5] = [
    "speech.attach",
    "speech.state",
    "speech.request",
    "speech.control",
    "speech.status",
];

/// An orchestrator runtime that reports the config it got (its token), and stays up.
struct OrchRt {
    cfg: SpawnConfig,
    _end: AdapterEnd,
}

struct OrchAdapter(mpsc::UnboundedSender<OrchRt>);

impl Adapter for OrchAdapter {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "claude-code",
            tier: Tier::Headless,
            description: "test orchestrator runtime",
        }
    }

    fn spawn(&self, cfg: SpawnConfig, _prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let (h, end) = AdapterHandle::pair();
        end.emit(
            EventBody::ThreadStarted(SessionPayload {
                session_id: Some("s-orch".into()),
                ..Default::default()
            }),
            None,
        );
        let _ = self.0.send(OrchRt { cfg, _end: end });
        Ok(h)
    }
}

/// The orchestrator's token, from the MCP configuration its runtime got.
fn token_of(rt: &OrchRt) -> String {
    let path = rt.cfg.mcp_config_file.as_ref().expect("an MCP config file");
    let mcp: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    mcp["mcpServers"]["twr"]["env"]["TWR_ORCH_TOKEN"]
        .as_str()
        .unwrap()
        .to_string()
}

/// Not a test: the agent's process of [`only_the_user_reads_aloud`] (run with
/// `TWR_TEST_SPEECH_SOCKET`): it connects, greets, calls every `speech.*` method and writes
/// the answers to `TWR_TEST_SPEECH_OUT`, one per line.
#[test]
fn speech_agent_client() {
    use std::io::{BufRead, BufReader, Write};
    let (Some(socket), Some(out)) = (
        std::env::var_os("TWR_TEST_SPEECH_SOCKET"),
        std::env::var_os("TWR_TEST_SPEECH_OUT"),
    ) else {
        return;
    };
    let mut s = std::os::unix::net::UnixStream::connect(socket).unwrap();
    s.set_read_timeout(Some(WAIT)).unwrap();
    let mut r = BufReader::new(s.try_clone().unwrap());
    let mut answers = String::new();
    let calls = std::iter::once(("hello", json!({"client": "agent"}))).chain(SPEECH.iter().map(
        |m| match *m {
            "speech.attach" => (*m, json!(speaker("agent"))),
            _ => (*m, Value::Null),
        },
    ));
    for (i, (method, params)) in calls.enumerate() {
        let line = json!({"id": i + 1, "method": method, "params": params});
        writeln!(s, "{line}").unwrap();
        let mut answer = String::new();
        r.read_line(&mut answer).unwrap();
        answers.push_str(&answer);
    }
    std::fs::write(PathBuf::from(out), answers).unwrap();
}

/// The orchestrator's connection and an agent's get `unauthorized` for every `speech.*`
/// method (none is in their lists), and the hub refuses the orchestrator itself too.
#[tokio::test]
async fn only_the_user_reads_aloud() {
    for m in SPEECH {
        assert!(server::METHODS.contains(&m), "{m}");
        assert!(!server::ORCHESTRATOR_METHODS.contains(&m), "{m}");
        assert!(!server::AGENT_METHODS.contains(&m), "{m}");
    }
    let (tx, mut spawns) = mpsc::unbounded_channel();
    let orch: Arc<dyn Adapter> = Arc::new(OrchAdapter(tx));
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::with_home(dir.path());
    let listener = server::bind(&paths.socket).await.unwrap();
    let mut config = Config {
        default_runtime: "fake".into(),
        ..Default::default()
    };
    config.orchestrator.runtime = Some("claude-code".into());
    let mut opts = HubOptions::new(paths.db_file(), config);
    opts.adapters = Arc::new(move |name: &str| -> Option<Arc<dyn Adapter>> {
        match name {
            "fake" => Some(Arc::new(FakeAdapter::new())),
            "claude-code" => Some(Arc::clone(&orch)),
            _ => None,
        }
    });
    opts.socket = Some(paths.socket.clone());
    // The orchestrator runtime runs in this process: its token is taken from this process's
    // connections.
    opts.orchestrator_token_unverified = true;
    let hub = Hub::open(opts).await.unwrap();
    let task = tokio::spawn(server::serve(
        hub.clone(),
        listener,
        paths.socket.clone(),
        false,
    ));
    let mut c = Client::connect(&paths.socket).await.unwrap();
    c.hello(json!({"client": "cli"})).await.unwrap();
    for name in ["api", "web"] {
        c.call(
            "thread.new",
            json!({"project": dir.path(), "runtime": "fake", "name": name}),
        )
        .await
        .unwrap();
    }
    let r = c
        .call("say", json!({"message": "coordinate the release"}))
        .await
        .unwrap();
    assert_eq!(r["action"], "delegate", "{r}");
    let rt = timeout(WAIT, spawns.recv()).await.unwrap().unwrap();

    let mut o = Client::connect(&paths.socket).await.unwrap();
    o.hello(json!({"client": "orchestrator", "token": token_of(&rt)}))
        .await
        .unwrap();
    for m in SPEECH {
        let params = if m == "speech.attach" {
            json!(speaker("orch"))
        } else {
            Value::Null
        };
        let e = o.call(m, params).await.unwrap_err();
        let e = e.downcast_ref::<RpcError>().unwrap();
        assert_eq!(e.code, codes::UNAUTHORIZED, "{m}: {e:?}");
        assert_eq!(e.message, format!("the orchestrator may not call {m}"));
    }
    // The hub refuses the orchestrator itself, whatever the server lets through.
    let orch = Caller::Orchestrator;
    let e = hub
        .speech_attach(speaker("orch"), &orch)
        .map(|_| ())
        .unwrap_err();
    assert_eq!(e.code, codes::UNAUTHORIZED);
    let e = hub
        .speech_request(
            &SpeechRequestParams {
                last: Some(1),
                ..Default::default()
            },
            &orch,
        )
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::UNAUTHORIZED);
    let p = SpeechControlParams {
        op: CtlOp::Stop,
        value: None,
        speaker: None,
    };
    let e = hub.speech_control(&p, &orch).await.unwrap_err();
    assert_eq!(e.code, codes::UNAUTHORIZED);
    assert_eq!(
        hub.speech_status(&orch).unwrap_err().code,
        codes::UNAUTHORIZED
    );

    // An agent: a process carrying the agent marker.
    let out = dir.path().join("agent-answers");
    let status = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "speech_agent_client", "--test-threads=1"])
        .env("TWR_AGENT", "w1")
        .env("TWR_TEST_SPEECH_SOCKET", &paths.socket)
        .env("TWR_TEST_SPEECH_OUT", &out)
        .stdout(std::process::Stdio::null())
        .status();
    let status = timeout(WAIT * 3, status).await.unwrap().unwrap();
    assert!(status.success());
    let answers: Vec<Value> = std::fs::read_to_string(&out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(answers.len(), 1 + SPEECH.len(), "{answers:?}");
    assert!(
        answers[0]["result"]["identity"]
            .as_str()
            .is_some_and(|i| i.starts_with("agent")),
        "{answers:?}"
    );
    for (a, m) in answers[1..].iter().zip(SPEECH) {
        assert_eq!(a["error"]["code"], codes::UNAUTHORIZED, "{m}: {a}");
        assert!(
            a["error"]["message"]
                .as_str()
                .is_some_and(|e| e.contains("agent")),
            "{m}: {a}"
        );
    }
    assert!(hub
        .speech_status(&Caller::user("cli"))
        .unwrap()
        .speakers
        .is_empty());

    drop(rt);
    c.call("hub.shutdown", Value::Null).await.unwrap();
    task.await.unwrap().unwrap();
}

/// The last messages are the last said, by `seq`: a late text of an older turn (a runtime
/// reopening it) comes where it was said, also across a page of the backwards read.
#[tokio::test]
async fn the_last_messages_are_the_last_said() {
    let th = start().await;
    let h = &th.hub;
    exchange(h, ORCH, 1, "first");
    exchange(h, ORCH, 2, "second");
    // Turn 1's text, said after turn 2's.
    let late = say(h, ORCH, 1, "late in turn one", None).seq;
    let (_, mut a) = attach(&th, &speaker("a")).await.unwrap();
    let r = th.call("speech.request", json!({"last": 1})).await.unwrap();
    assert_eq!(r["count"], 1);
    let SpeechItem::Ctl {
        op: CtlOp::Replace, ..
    } = next(&mut a).await
    else {
        panic!("a replace first")
    };
    let SpeechItem::Msg { seq, text, .. } = next(&mut a).await else {
        panic!("a message")
    };
    assert_eq!((seq, text.as_str()), (late, "late in turn one"));
    let r = th.call("speech.request", json!({"last": 3})).await.unwrap();
    assert_eq!(r["count"], 3);
    let _ = next(&mut a).await;
    let mut texts = Vec::new();
    for _ in 0..3 {
        if let SpeechItem::Msg { text, .. } = next(&mut a).await {
            texts.push(text);
        }
    }
    assert_eq!(texts, ["first", "second", "late in turn one"]);
}

/// A switch reads the thread it leaves through all it said before, however much: a long turn
/// (more streamed chunks than the feed's queue) still gets its last text read.
#[tokio::test]
async fn a_switch_reads_all_the_old_thread_said() {
    let th = start().await;
    let h = &th.hub;
    let mut att = h
        .speech_attach(speaker("mac"), &Caller::user("speak"))
        .unwrap();
    // Before the feed runs: a long streamed turn, its final text, then the switch.
    turn(h, ORCH, 1, TurnFrom::User, None);
    for i in 0..1500 {
        on(
            h,
            ORCH,
            1,
            EventBody::AgentText(AgentText {
                text: format!("chunk {i} "),
                delta: true,
                ..Default::default()
            }),
        );
    }
    say(h, ORCH, 1, "The end of a long turn.", None);
    orchestrator(h, "o2");
    let item = timeout(WAIT, att.next()).await.unwrap().unwrap();
    let (thread, _, text, _, _, _) = msg(&item);
    assert_eq!((thread, text), (ORCH, "The end of a long turn."));
    let item = timeout(WAIT, att.next()).await.unwrap().unwrap();
    assert_eq!(
        item,
        SpeechItem::Orchestrator {
            thread: "o2".into()
        }
    );
}

/// Several switches before the feed gets to them are followed one after the other: what the
/// orchestrator in between said is read too.
#[tokio::test]
async fn switches_are_followed_in_order() {
    let th = start().await;
    let h = &th.hub;
    let mut att = h
        .speech_attach(speaker("mac"), &Caller::user("speak"))
        .unwrap();
    // Before the feed runs: to B, B's reply, to C, C's reply.
    orchestrator(h, "ob");
    exchange(h, "ob", 1, "From B.");
    orchestrator(h, "oc");
    exchange(h, "oc", 1, "From C.");
    let mut seen = Vec::new();
    for _ in 0..4 {
        let item = timeout(WAIT, att.next()).await.unwrap().unwrap();
        seen.push(match item {
            SpeechItem::Orchestrator { thread } => format!("to {thread}"),
            SpeechItem::Msg { text, .. } => text,
            other => panic!("{other:?}"),
        });
    }
    assert_eq!(seen, ["to ob", "From B.", "to oc", "From C."]);
}
