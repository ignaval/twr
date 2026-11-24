//! The hub's internet connection (`hub::net`): the notice while it is down, stalls and hung
//! turns judged with it, retries that wait for it, and what closes when it is back. The check
//! is a fake whose answer the test sets.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::time::timeout;
use twr_adapters::fake::FakeAdapter;
use twr_core::adapter::Adapter;
use twr_core::config::Config;
use twr_core::event::*;
use twr_hub::hub::{NetProbe, NetTiming, Probed, ThreadSubscription};
use twr_hub::{Hub, HubOptions, NewThread};

const WAIT: Duration = Duration::from_secs(10);

/// A check that connects while `up` is set.
fn probe(up: &Arc<AtomicBool>) -> NetProbe {
    let up = Arc::clone(up);
    NetProbe {
        targets: vec!["api.example.com:443".into()],
        check: Arc::new(move || {
            let ok = up.load(Ordering::SeqCst);
            Box::pin(async move {
                Probed {
                    ok,
                    detail: if ok {
                        "api.example.com:443".into()
                    } else {
                        "api.example.com:443: Network is unreachable".into()
                    },
                }
            })
        }),
        timing: NetTiming {
            every: Some(Duration::from_millis(20)),
            suspect_every: Duration::from_millis(10),
            offline_every: Duration::from_millis(10),
            offline_after: Duration::from_millis(40),
            nudge_gap: Duration::from_millis(5),
        },
    }
}

fn opts(dir: &Path, up: &Arc<AtomicBool>, stall_after: Option<Duration>) -> HubOptions {
    let mut o = HubOptions::new(dir.join("data").join("twr.db"), Config::default());
    o.adapters = Arc::new(|name: &str| -> Option<Arc<dyn Adapter>> {
        (name == "fake").then(|| Arc::new(FakeAdapter::new()) as Arc<dyn Adapter>)
    });
    o.stop_timeout = Duration::from_secs(5);
    o.stall_after = stall_after;
    o.net_probe = Some(probe(up));
    o
}

async fn new_thread(hub: &Hub, project: &Path, name: &str) -> String {
    let v = hub
        .new_thread(
            NewThread {
                project: project.to_string_lossy().into_owned(),
                runtime: Some("fake".into()),
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

async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    timeout(WAIT, async {
        while !f() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

fn state(hub: &Hub) -> String {
    hub.info()["connectivity"]["state"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// The open items of `thread`, as `attention.list` gives them.
fn items_of(hub: &Hub, thread: &str) -> Vec<serde_json::Value> {
    hub.attention_list(true)
        .into_iter()
        .filter(|a| a["thread"] == thread)
        .collect()
}

fn raised(evs: &[Event]) -> Vec<AttentionRaised> {
    evs.iter()
        .filter_map(|e| match &e.body {
            EventBody::AttentionRaised(a) => Some(a.clone()),
            _ => None,
        })
        .collect()
}

fn retries(evs: &[Event]) -> Vec<(u64, u64)> {
    evs.iter()
        .filter_map(|e| match &e.body {
            EventBody::TurnStarted(p) => {
                let of = p.ext.as_ref()?.get("twr")?.get("retry_of")?.as_u64()?;
                Some((e.turn?, of))
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn the_hub_says_when_its_connection_is_down_and_closes_it_when_back() {
    let dir = tempfile::tempdir().unwrap();
    let up = Arc::new(AtomicBool::new(true));
    let hub = Hub::open(opts(dir.path(), &up, None)).await.unwrap();
    eventually("a first check", || state(&hub) == "online").await;
    assert!(items_of(&hub, HUB_THREAD).is_empty());

    up.store(false, Ordering::SeqCst);
    eventually("the notice", || !items_of(&hub, HUB_THREAD).is_empty()).await;
    assert_eq!(state(&hub), "offline");
    let items = items_of(&hub, HUB_THREAD);
    assert_eq!(items.len(), 1, "{items:?}");
    let card = &items[0];
    assert_eq!(card["reason"], "stalled");
    assert!(card["offline_since"].is_string(), "{card}");
    let summary = card["summary"].as_str().unwrap();
    assert!(
        summary.starts_with("No internet connection since "),
        "{summary}"
    );
    assert!(
        summary.contains("cannot reach api.example.com:443"),
        "{summary}"
    );
    let attn_id = card["attn_id"].as_str().unwrap().to_string();
    // Still down: one notice, however many checks fail.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(items_of(&hub, HUB_THREAD).len(), 1);

    up.store(true, Ordering::SeqCst);
    eventually("the notice to close", || {
        items_of(&hub, HUB_THREAD).is_empty()
    })
    .await;
    assert_eq!(state(&hub), "online");
    assert!(hub.info()["connectivity"]["back_at"].is_string());
    let (items, _) = hub.control_read(0, 10_000).unwrap();
    let raise = items
        .iter()
        .find_map(|(_, e)| match &e.body {
            EventBody::AttentionRaised(p) if p.attn_id == attn_id => Some(p.clone()),
            _ => None,
        })
        .unwrap();
    let mark = &raise.ext.as_ref().unwrap()["twr"][OFFLINE_KEY];
    assert!(mark["since"].is_string(), "{mark}");
    assert_eq!(mark["targets"][0], "api.example.com:443");
    assert!(mark["detail"]
        .as_str()
        .unwrap()
        .contains("Network is unreachable"));
    let closed = items
        .iter()
        .find_map(|(_, e)| match &e.body {
            EventBody::AttentionResolved(p) if p.attn_id == attn_id => Some(p.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(closed.by, ResolvedBy::Timeout);
    assert!(closed.ext.as_ref().unwrap()["twr"][BACK_ONLINE_KEY].is_string());
    hub.shutdown().await;
}

#[tokio::test]
async fn one_failed_check_is_no_outage() {
    let dir = tempfile::tempdir().unwrap();
    let up = Arc::new(AtomicBool::new(true));
    let mut o = opts(dir.path(), &up, None);
    // A failure must span a second before it counts: a blip of a few checks does not.
    o.net_probe.as_mut().unwrap().timing.offline_after = Duration::from_secs(1);
    let hub = Hub::open(o).await.unwrap();
    eventually("a first check", || state(&hub) == "online").await;
    up.store(false, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(150)).await;
    up.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(state(&hub), "online");
    let (items, _) = hub.control_read(0, 10_000).unwrap();
    assert!(
        !items
            .iter()
            .any(|(_, e)| e.kind() == "attention.raised" && e.thread == HUB_THREAD),
        "no notice for a blip"
    );
    hub.shutdown().await;
}

#[tokio::test]
async fn a_silent_turn_while_offline_says_so_and_its_silence_counts_from_the_return() {
    let dir = tempfile::tempdir().unwrap();
    let up = Arc::new(AtomicBool::new(false));
    let stall = Duration::from_millis(300);
    let hub = Hub::open(opts(dir.path(), &up, Some(stall))).await.unwrap();
    eventually("the outage", || state(&hub) == "offline").await;
    let t = new_thread(&hub, dir.path(), "api").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "!sleep 60000\ndone");
    // A message waits behind the silent turn: no hung-turn card while the connection is down
    // (interrupting would not help), but its stall, which says why.
    send(&hub, &t, "next");
    let evs = until(&mut sub, |e| e.kind() == "attention.raised").await;
    let a = raised(&evs).pop().unwrap();
    assert_eq!(a.reason, AttentionReason::Stalled);
    assert!(!is_hung_card(&a.ext), "{a:?}");
    assert!(
        a.summary.starts_with("No internet connection since "),
        "{}",
        a.summary
    );
    assert!(
        a.summary.contains(": no activity for 1 s; nothing in flight that twr can see. Its runtime cannot reach its model API"),
        "{}",
        a.summary
    );
    assert!(offline_since(&Source::Hub, &a.ext).is_some());
    let item = items_of(&hub, &t);
    assert_eq!(item.len(), 1);
    assert!(item[0]["offline_since"].is_string());
    // No other card while it stays down.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(items_of(&hub, &t).len(), 1);

    // Back: its stall closes, and its silence counts from now.
    up.store(true, Ordering::SeqCst);
    let evs = until(
        &mut sub,
        |e| matches!(&e.body, EventBody::AttentionResolved(r) if r.attn_id == a.attn_id),
    )
    .await;
    let back = evs.last().unwrap().ts;
    let EventBody::AttentionResolved(r) = &evs.last().unwrap().body else {
        unreachable!()
    };
    assert_eq!(r.by, ResolvedBy::Timeout);
    assert!(r.ext.as_ref().unwrap()["twr"][BACK_ONLINE_KEY].is_string());
    assert!(!went_on(&r.ext));
    // The turn is still silent with a message behind it: a hung-turn card, but only once the
    // hung threshold has passed since the return.
    let evs = until(&mut sub, |e| e.kind() == "attention.raised").await;
    let next = evs.last().unwrap();
    let EventBody::AttentionRaised(h) = &next.body else {
        unreachable!()
    };
    assert!(is_hung_card(&h.ext), "{h:?}");
    assert!(
        next.ts - back >= chrono::Duration::from_std(stall).unwrap(),
        "raised {} after the return",
        next.ts - back
    );
    hub.shutdown().await;
}

#[tokio::test]
async fn a_transient_failure_waits_for_the_connection_to_be_retried() {
    let dir = tempfile::tempdir().unwrap();
    let up = Arc::new(AtomicBool::new(false));
    let hub = Hub::open(opts(dir.path(), &up, None)).await.unwrap();
    eventually("the outage", || state(&hub) == "offline").await;
    let t = new_thread(&hub, dir.path(), "api").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "working\n!transient");
    until(&mut sub, |e| {
        e.kind() == "decision.made" && e.turn == Some(1)
    })
    .await;
    // Not retried while the connection is down, and nothing else of its queue runs.
    send(&hub, &t, "queued after");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (_, all) = hub.thread_events(&t, 0, 1000).unwrap();
    assert!(retries(&all).is_empty(), "{:?}", retries(&all));
    assert_eq!(hub.thread_view(&t).unwrap().turns, 1);
    assert_eq!(hub.info()["connectivity"]["held_retries"], 1);
    // No error card for it either: its retry is pending.
    assert!(items_of(&hub, &t).is_empty());

    up.store(true, Ordering::SeqCst);
    until(&mut sub, |e| e.kind() == "turn.ended" && e.turn == Some(2)).await;
    let (_, all) = hub.thread_events(&t, 0, 1000).unwrap();
    assert_eq!(retries(&all), [(2, 1)]);
    assert_eq!(hub.info()["connectivity"]["held_retries"], 0);
    hub.shutdown().await;
}

#[tokio::test]
async fn with_the_connection_up_a_retry_starts_after_one_check() {
    let dir = tempfile::tempdir().unwrap();
    let up = Arc::new(AtomicBool::new(true));
    let mut o = opts(dir.path(), &up, None);
    // A long period: only the check the failure asks for can release the retry soon.
    o.net_probe.as_mut().unwrap().timing.every = Some(Duration::from_secs(3600));
    let hub = Hub::open(o).await.unwrap();
    eventually("a first check", || state(&hub) == "online").await;
    let t = new_thread(&hub, dir.path(), "api").await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "working\n!transient");
    until(&mut sub, |e| e.kind() == "turn.ended" && e.turn == Some(2)).await;
    let (_, all) = hub.thread_events(&t, 0, 1000).unwrap();
    assert_eq!(retries(&all), [(2, 1)]);
    hub.shutdown().await;
}

#[tokio::test]
async fn a_hub_without_a_check_reports_it_off() {
    let dir = tempfile::tempdir().unwrap();
    let up = Arc::new(AtomicBool::new(false));
    let mut o = opts(dir.path(), &up, None);
    o.net_probe = None;
    let hub = Hub::open(o).await.unwrap();
    assert_eq!(state(&hub), "off");
    // `connectivity_check_secs = 0` turns a hub's check off too.
    hub.shutdown().await;
    let dir = tempfile::tempdir().unwrap();
    let mut o = opts(dir.path(), &up, None);
    o.net_probe.as_mut().unwrap().timing.every = None;
    o.config.hub.connectivity_check_secs = Some(0);
    let hub = Hub::open(o).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(state(&hub), "off");
    assert!(items_of(&hub, HUB_THREAD).is_empty());
    hub.shutdown().await;
}

#[tokio::test]
async fn the_notice_can_be_dismissed_while_the_connection_is_down() {
    let dir = tempfile::tempdir().unwrap();
    let up = Arc::new(AtomicBool::new(false));
    let hub = Hub::open(opts(dir.path(), &up, None)).await.unwrap();
    eventually("the notice", || !items_of(&hub, HUB_THREAD).is_empty()).await;
    let attn_id = items_of(&hub, HUB_THREAD)[0]["attn_id"]
        .as_str()
        .unwrap()
        .to_string();
    hub.attention_dismiss(&attn_id, None, &twr_hub::Caller::user("test"))
        .unwrap();
    // Gone, and not raised again while the same outage lasts.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(items_of(&hub, HUB_THREAD).is_empty());
    assert_eq!(state(&hub), "offline");
    up.store(true, Ordering::SeqCst);
    eventually("the return", || state(&hub) == "online").await;
    assert!(items_of(&hub, HUB_THREAD).is_empty());
    hub.shutdown().await;
}
