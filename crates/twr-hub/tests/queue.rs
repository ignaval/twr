//! The queue as the user sees and changes it (`hub::queue`): `thread.queue` lists a thread's
//! queued messages in order; `message.send_now` interrupts the running turn so the next
//! queued message runs now; editing is taking back (`message.withdraw`) and sending again.
//!
//! Workers run the fake adapter, which interrupts a turn and keeps its session.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::time::timeout;
use twr_adapters::fake::FakeAdapter;
use twr_core::adapter::{Adapter, AdapterHandle, AdapterInfo, SpawnConfig, Tier};
use twr_core::config::Config;
use twr_core::event::*;
use twr_hub::hub::ControlSubscription;
use twr_hub::{Caller, Hub, HubOptions, NewThread};

const WAIT: Duration = Duration::from_secs(10);

/// A runtime that cannot interrupt a turn (a PTY's tier), otherwise the fake.
struct PtyLike;

impl Adapter for PtyLike {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "ptylike",
            tier: Tier::Pty,
            description: "test runtime without interrupts",
        }
    }

    fn spawn(&self, cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        FakeAdapter::new().spawn(cfg, prompt)
    }
}

async fn hub(dir: &Path) -> Hub {
    let config = Config {
        default_runtime: "fake".into(),
        ..Default::default()
    };
    let mut o = HubOptions::new(dir.join("data").join("twr.db"), config);
    o.adapters = Arc::new(|name: &str| -> Option<Arc<dyn Adapter>> {
        match name {
            "fake" => Some(Arc::new(FakeAdapter::new())),
            "ptylike" => Some(Arc::new(PtyLike)),
            _ => None,
        }
    });
    o.socket = Some(dir.join("hub.sock"));
    o.stop_timeout = Duration::from_secs(5);
    Hub::open(o).await.unwrap()
}

async fn thread(hub: &Hub, dir: &Path, name: &str, runtime: &str) -> String {
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

fn tui() -> Caller {
    Caller::user("tui")
}

async fn until(sub: &mut ControlSubscription, pred: impl Fn(&Event) -> bool) -> Vec<Event> {
    let mut seen = Vec::new();
    timeout(WAIT, async {
        loop {
            let (_, ev) = sub.next().await.unwrap().expect("hub stopped");
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
            seen.iter().map(|e| e.kind()).collect::<Vec<_>>()
        )
    });
    seen
}

fn is_kind_on(kind: &'static str, thread: &str) -> impl Fn(&Event) -> bool {
    let thread = thread.to_string();
    move |e: &Event| e.kind() == kind && e.thread == thread
}

fn events_of(hub: &Hub, thread: &str) -> Vec<Event> {
    hub.thread_events(thread, 0, 10_000).unwrap().1
}

fn queue(hub: &Hub, thread: &str) -> Value {
    hub.thread_queue(thread, &tui()).unwrap()
}

fn refs(q: &Value) -> Vec<i64> {
    q["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["ref"].as_i64().unwrap())
        .collect()
}

async fn eventually(mut check: impl FnMut() -> bool) {
    timeout(WAIT, async {
        while !check() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timed out waiting for a condition");
}

/// The queue lists the waiting messages in the order they run, the user's own marked
/// `mine` and the orchestrator's not, each cut to a preview; the running one is not queued.
/// The orchestrator can't read it.
#[tokio::test]
async fn the_queue_lists_waiting_messages_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let hub = hub(dir.path()).await;
    let api = thread(&hub, dir.path(), "api", "fake").await;
    let first = hub.send_by(&api, "!sleep 5000", &tui()).unwrap();
    assert_eq!(first.turn, Some(1));
    let q = queue(&hub, "api");
    assert_eq!((q["total"].as_u64(), refs(&q)), (Some(0), vec![]), "{q}");
    let second = hub.send_by(&api, "second", &tui()).unwrap();
    let long = "x".repeat(5000);
    let third = hub.send_by(&api, &long, &tui()).unwrap();
    let orch = hub.send_by(&api, "from the orchestrator", &Caller::Orchestrator);
    let orch = orch.unwrap();
    let q = queue(&hub, "api");
    assert_eq!(q["total"], 3, "{q}");
    assert_eq!(q["thread_name"], "api");
    assert_eq!(q["held"], false);
    assert_eq!(
        refs(&q),
        vec![second.msg_ref, third.msg_ref, orch.msg_ref],
        "{q}"
    );
    let items = q["items"].as_array().unwrap();
    assert_eq!(
        (
            items[0]["prompt"].as_str(),
            items[0]["mine"].as_bool(),
            items[0]["from"].as_str(),
            items[0]["truncated"].as_bool()
        ),
        (Some("second"), Some(true), Some("user"), Some(false))
    );
    assert_eq!(items[1]["truncated"], true);
    assert_eq!(items[1]["prompt"].as_str().unwrap().len(), 2048);
    assert_eq!(
        (items[2]["mine"].as_bool(), items[2]["from"].as_str()),
        (Some(false), Some("orchestrator"))
    );
    // The user's newest, wherever it is in the queue.
    assert_eq!(q["newest_mine"].as_i64(), Some(third.msg_ref), "{q}");
    // Only the user reads it.
    assert!(hub.thread_queue("api", &Caller::Orchestrator).is_err());
    // Taking one back leaves the others in order; the full text comes back.
    let w = hub.withdraw_message("api", third.msg_ref, &tui()).unwrap();
    assert_eq!(w["prompt"].as_str(), Some(long.as_str()));
    let q = queue(&hub, "api");
    assert_eq!(refs(&q), vec![second.msg_ref, orch.msg_ref]);
    assert_eq!(q["newest_mine"].as_i64(), Some(second.msg_ref), "{q}");
    hub.stop_thread("api", "test").unwrap();
    hub.shutdown().await;
}

/// Sending the next queued message now interrupts the running turn, which ends
/// `interrupted` (its later steps never run, no report), and the message is the next turn;
/// only the next one can be sent now, and the turn the message started can't be sent again.
#[tokio::test]
async fn send_now_interrupts_the_turn_and_runs_the_next_message() {
    let dir = tempfile::tempdir().unwrap();
    let hub = hub(dir.path()).await;
    let api = thread(&hub, dir.path(), "api", "fake").await;
    let mut sub = hub.subscribe_control(0);
    hub.send_by(&api, "!sleep 5000\n!tool Bash ls", &tui())
        .unwrap();
    let second = hub.send_by(&api, "second\n!sleep 5000", &tui()).unwrap();
    let third = hub.send_by(&api, "third", &tui()).unwrap();
    let r = hub.send_now("api", third.msg_ref, &tui()).unwrap();
    assert_eq!(
        (r["sent_now"].as_bool(), r["reason"].as_str()),
        (Some(false), Some("not_next")),
        "{r}"
    );
    assert!(
        r["message"]
            .as_str()
            .unwrap()
            .contains("1 queued message is ahead"),
        "{r}"
    );
    let r = hub.send_now("api", second.msg_ref, &tui()).unwrap();
    assert_eq!(
        (
            r["sent_now"].as_bool(),
            r["state"].as_str(),
            r["turn"].as_u64()
        ),
        (Some(true), Some("interrupting"), Some(1)),
        "{r}"
    );
    // Asked again while it is being interrupted: the same.
    let again = hub.send_now("api", second.msg_ref, &tui()).unwrap();
    assert_eq!(again["state"], "interrupting", "{again}");
    let evs = until(&mut sub, |e| {
        e.thread == api
            && matches!(&e.body, EventBody::TurnStarted(p) if p.prompt.starts_with("second"))
    })
    .await;
    let ended = evs
        .iter()
        .find_map(|e| match &e.body {
            EventBody::TurnEnded(p) if e.thread == api => Some(p.result),
            _ => None,
        })
        .unwrap();
    assert_eq!(ended, TurnResult::Interrupted);
    let log = events_of(&hub, &api);
    let at = |kind: &str| log.iter().position(|e| e.kind() == kind);
    assert!(at("turn.interrupted") < at("turn.ended"));
    assert_eq!(at("agent.tool_call"), None);
    let EventBody::TurnInterrupted(p) = &log[at("turn.interrupted").unwrap()].body else {
        panic!()
    };
    assert_eq!((p.msg_ref, p.turn), (second.msg_ref, 1));
    assert_eq!(log[at("turn.interrupted").unwrap()].turn, Some(1));
    assert_eq!(
        log[at("turn.interrupted").unwrap()].source,
        Source::User("tui".into())
    );
    // No report of the interrupted turn (the second message's turn may have one).
    assert!(
        log.iter()
            .all(|e| !matches!(&e.body, EventBody::ReportReady(r) if r.turn == Some(1))),
        "{log:?}"
    );
    // The third waits; the second isn't queued any more.
    assert_eq!(refs(&queue(&hub, "api")), vec![third.msg_ref]);
    let gone = hub.send_now("api", second.msg_ref, &tui()).unwrap();
    assert_eq!(gone["reason"], "unknown", "{gone}");
    hub.stop_thread("api", "test").unwrap();
    hub.shutdown().await;
}

/// A runtime that missed the interrupt (it came before the turn began) is interrupted again
/// once it shows it runs the turn, as a taken-back turn is: what came after never runs.
#[tokio::test]
async fn an_interrupted_turn_its_runtime_runs_anyway_is_interrupted_again() {
    let dir = tempfile::tempdir().unwrap();
    let hub = hub(dir.path()).await;
    let api = thread(&hub, dir.path(), "api", "fake").await;
    let mut sub = hub.subscribe_control(0);
    hub.send_by(
        &api,
        "!deaf\nworking on it\n!sleep 3000\n!tool Bash ls",
        &tui(),
    )
    .unwrap();
    let next = hub.send_by(&api, "next", &tui()).unwrap();
    assert_eq!(
        hub.send_now("api", next.msg_ref, &tui()).unwrap()["state"],
        "interrupting"
    );
    let evs = until(&mut sub, is_kind_on("turn.ended", &api)).await;
    let EventBody::TurnEnded(end) = &evs.last().unwrap().body else {
        panic!()
    };
    assert_eq!(end.result, TurnResult::Interrupted);
    assert!(events_of(&hub, &api)
        .iter()
        .all(|e| e.kind() != "agent.tool_call"));
    hub.stop_thread("api", "test").unwrap();
    hub.shutdown().await;
}

/// A turn on a runtime that can't interrupt one: refused, the message stays next.
#[tokio::test]
async fn send_now_on_an_uninterruptible_turn_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let hub = hub(dir.path()).await;
    let api = thread(&hub, dir.path(), "api", "ptylike").await;
    hub.send_by(&api, "!sleep 5000", &tui()).unwrap();
    let next = hub.send_by(&api, "next", &tui()).unwrap();
    let r = hub.send_now("api", next.msg_ref, &tui()).unwrap();
    assert_eq!(
        (r["sent_now"].as_bool(), r["reason"].as_str()),
        (Some(false), Some("uninterruptible")),
        "{r}"
    );
    assert_eq!(refs(&queue(&hub, "api")), vec![next.msg_ref]);
    assert!(events_of(&hub, &api)
        .iter()
        .all(|e| e.kind() != "turn.interrupted"));
    hub.stop_thread("api", "test").unwrap();
    hub.shutdown().await;
}

/// A crash holds the queue (nothing the user didn't send runs after it); sending its next
/// message now releases it, as the user's own send does.
#[tokio::test]
async fn send_now_releases_a_queue_a_crash_held() {
    let dir = tempfile::tempdir().unwrap();
    let hub = hub(dir.path()).await;
    let api = thread(&hub, dir.path(), "api", "fake").await;
    let mut sub = hub.subscribe_control(0);
    hub.send_by(&api, "!crash", &tui()).unwrap();
    until(&mut sub, is_kind_on("thread.crashed", &api)).await;
    let orch = hub
        .send_by(&api, "from the orchestrator", &Caller::Orchestrator)
        .unwrap();
    assert_eq!(orch.turn, None);
    let q = queue(&hub, "api");
    assert_eq!(
        (q["held"].as_bool(), refs(&q)),
        (Some(true), vec![orch.msg_ref]),
        "{q}"
    );
    let r = hub.send_now("api", orch.msg_ref, &tui()).unwrap();
    assert_eq!(r["sent_now"], true, "{r}");
    until(&mut sub, |e| {
        e.thread == api
            && matches!(&e.body, EventBody::TurnStarted(p) if p.prompt == "from the orchestrator")
    })
    .await;
    eventually(|| queue(&hub, "api")["total"] == 0).await;
    hub.stop_thread("api", "test").unwrap();
    hub.shutdown().await;
}
