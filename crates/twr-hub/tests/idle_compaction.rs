//! A worker whose prompt cache expired with a large context (`[threads] idle_compaction`,
//! `hub::idle_compact`): the offer on the hub's thread before a message of the user's or the
//! orchestrator's goes to it, its answers, the cases it is not made in, and the compaction the
//! hub starts itself before the cache expires under `before_expiry`. The fake runtime stands
//! in for Claude Code: `!context n` reports a context of `n` tokens, `!cache <ttl> <ago>` a
//! prompt cache written `ago` seconds ago that lives `ttl` seconds, and
//! `FakeAdapter::with_compaction` makes it compact on request.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::time::timeout;
use twr_adapters::fake::FakeAdapter;
use twr_core::adapter::{
    Adapter, AdapterCmd, AdapterHandle, AdapterInfo, Answer, SpawnConfig, Tier,
};
use twr_core::config::{Config, IdleCompaction};
use twr_core::event::*;
use twr_core::proto::codes;
use twr_hub::hub::{ThreadSubscription, COMPACT_FIRST, SEND_AS_IS};
use twr_hub::{Caller, Hub, HubOptions, NewThread};

const WAIT: Duration = Duration::from_secs(10);

/// An expired cache (written 3700 s ago, living an hour) of a large context.
const EXPIRED: &str = "!context 250000\n!cache 3600 3700\nwarm up";

/// Runtimes: `fakec` (compacts on request), `fake` (cannot), `failc` (its compactions fail;
/// each prompt's turn reports a large context and an hour-long cache that expires in 200 s),
/// `fresh` (a new session at every start, each turn a large context whose cache expired).
fn opts(dir: &Path, mode: IdleCompaction) -> HubOptions {
    let mut config = Config::default();
    config.threads.idle_compaction = mode;
    let mut o = HubOptions::new(dir.join("data").join("twr.db"), config);
    o.adapters = Arc::new(|name: &str| -> Option<Arc<dyn Adapter>> {
        match name {
            "fake" => Some(Arc::new(FakeAdapter::new())),
            "fakec" => Some(Arc::new(FakeAdapter::new().with_compaction())),
            "failc" => Some(Arc::new(FailingCompaction)),
            "fresh" => Some(Arc::new(FreshSessions)),
            _ => None,
        }
    });
    o.stop_timeout = Duration::from_secs(5);
    o
}

async fn open(dir: &Path, mode: IdleCompaction) -> Hub {
    Hub::open(opts(dir, mode)).await.unwrap()
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

fn user() -> Caller {
    Caller::user("test")
}

/// Sends `prompt` to `thread` as the user; returns the message's ref.
fn send(hub: &Hub, thread: &str, prompt: &str) -> i64 {
    hub.send_by(thread, prompt, &user()).unwrap().msg_ref
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

/// The open offer cards (on the hub's thread, with the offer's options).
fn offers(hub: &Hub) -> Vec<Value> {
    hub.attention_list(true)
        .into_iter()
        .filter(|i| {
            i["thread"] == HUB_THREAD
                && i["options"]
                    .as_array()
                    .is_some_and(|o| o.iter().any(|x| x == COMPACT_FIRST))
        })
        .collect()
}

/// Waits for the one open offer card; returns it.
async fn offer(hub: &Hub) -> Value {
    timeout(WAIT, async {
        loop {
            if let [card] = offers(hub).as_slice() {
                return card.clone();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("an offer card")
}

/// Waits until no offer card is open.
async fn no_offer(hub: &Hub) {
    timeout(WAIT, async {
        while !offers(hub).is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the offer card closed");
}

/// The `attention.raised` of `attn_id`.
fn raised(hub: &Hub, attn_id: &str) -> AttentionRaised {
    let (items, _) = hub.control_read(0, 100_000).unwrap();
    items
        .iter()
        .find_map(|(_, e)| match &e.body {
            EventBody::AttentionRaised(p) if p.attn_id == attn_id => Some(p.clone()),
            _ => None,
        })
        .expect("the card's event")
}

/// The `attention.resolved` events of `attn_id`, with their source.
fn resolutions(hub: &Hub, attn_id: &str) -> Vec<(Source, AttentionResolved)> {
    let (items, _) = hub.control_read(0, 100_000).unwrap();
    items
        .iter()
        .filter_map(|(_, e)| match &e.body {
            EventBody::AttentionResolved(p) if p.attn_id == attn_id => {
                Some((e.source.clone(), p.clone()))
            }
            _ => None,
        })
        .collect()
}

fn attn(card: &Value) -> String {
    card["attn_id"].as_str().unwrap().to_string()
}

fn answer(hub: &Hub, card: &Value, decision: &str, caller: &Caller) -> Result<Event, String> {
    hub.respond_by(&attn(card), Answer::new(decision), caller)
        .map_err(|e| format!("{}: {}", e.code, e.message))
}

/// The `turn.started` of `thread`'s turn `n`, with its source.
fn started(hub: &Hub, thread: &str, n: u64) -> (Source, TurnStarted) {
    let (_, evs) = hub.thread_events(thread, 0, 100_000).unwrap();
    evs.iter()
        .find_map(|e| match &e.body {
            EventBody::TurnStarted(p) if e.turn == Some(n) => Some((e.source.clone(), p.clone())),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no turn {n}"))
}

fn compact_by(p: &TurnStarted) -> Option<String> {
    p.ext.as_ref()?["twr"]
        .get("compact_by")?
        .as_str()
        .map(str::to_string)
}

/// A thread `name` on `runtime` whose first turn ran `first`, ended.
async fn warmed(
    hub: &Hub,
    dir: &Path,
    name: &str,
    runtime: &str,
    first: &str,
) -> (String, ThreadSubscription) {
    let t = new_thread(hub, dir, name, runtime).await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(hub, &t, first);
    until(&mut sub, ended_turn(1)).await;
    (t, sub)
}

/// A message of the user's to a worker whose hour-long cache of a 250k-token context expired
/// waits on a card on the hub's thread, which says what it is about and quotes nothing of the
/// worker's. The thread says when its cache expires. The orchestrator's answer is refused, an
/// answer that is no option too; the user's `Compact first` runs a compaction (the user's,
/// `compact_by: offer`) before the held message, which then goes.
#[tokio::test]
async fn compact_first_runs_a_compaction_before_the_held_message() {
    let dir = tempfile::tempdir().unwrap();
    let hub = open(dir.path(), IdleCompaction::Ask).await;
    let (t, mut sub) = warmed(&hub, dir.path(), "api", "fakec", EXPIRED).await;
    let j = hub.get_thread(&t).unwrap();
    assert_eq!(j["cache_ttl_s"], 3600);
    let expires: chrono::DateTime<chrono::Utc> =
        serde_json::from_value(j["cache_expires_at"].clone()).unwrap();
    assert!(expires < chrono::Utc::now());
    assert!(j.get("idle_compaction_off").is_none());
    send(&hub, &t, "next");
    let card = offer(&hub).await;
    assert_eq!(
        card["options"],
        serde_json::json!([COMPACT_FIRST, SEND_AS_IS, "Don't ask for api"])
    );
    let summary = card["summary"].as_str().unwrap();
    assert!(
        summary.starts_with("api: its context (250k tokens) is no longer cached (idle 1h 1m; the cache expired 1m ago)"),
        "{summary}"
    );
    assert!(!summary.contains("warm up"), "{summary}");
    let p = raised(&hub, &attn(&card));
    let o = &p.ext.as_ref().unwrap()["twr"]["idle_offer"];
    assert_eq!(o["thread"], t.as_str());
    assert_eq!(o["tokens"], 250_000);
    assert!(
        o["session"].is_string() && o["expires_at"].is_string(),
        "{o}"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(hub.thread_view(&t).unwrap().turns, 1, "held");
    let e = answer(&hub, &card, COMPACT_FIRST, &Caller::Orchestrator).unwrap_err();
    assert!(e.starts_with(codes::RELAY_FORBIDDEN), "{e}");
    let e = answer(&hub, &card, "maybe", &user()).unwrap_err();
    assert!(
        e.starts_with(codes::BAD_REQUEST) && e.contains(SEND_AS_IS),
        "{e}"
    );
    assert_eq!(offers(&hub).len(), 1, "still open");
    answer(&hub, &card, COMPACT_FIRST, &user()).unwrap();
    until(&mut sub, ended_turn(3)).await;
    let (source, two) = started(&hub, &t, 2);
    assert!(is_compact_turn(&two), "{two:?}");
    assert_eq!(source, Source::User("test".into()));
    assert_eq!(compact_by(&two).as_deref(), Some("offer"));
    assert_eq!(started(&hub, &t, 3).1.prompt, "next");
    let r = resolutions(&hub, &attn(&card));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].1.by, ResolvedBy::User);
    assert!(offers(&hub).is_empty());
    // Compacted: a small context, the cache forgotten; the next message goes.
    send(&hub, &t, "after");
    until(&mut sub, ended_turn(4)).await;
    assert!(offers(&hub).is_empty());
    hub.shutdown().await;
}

/// The orchestrator's message is held too; `Send as is` lets it go, and a later message to the
/// same cache gets no second card.
#[tokio::test]
async fn send_as_is_lets_the_held_message_go_and_offers_no_more() {
    let dir = tempfile::tempdir().unwrap();
    let hub = open(dir.path(), IdleCompaction::Ask).await;
    let (t, mut sub) = warmed(&hub, dir.path(), "api", "fakec", EXPIRED).await;
    hub.send_by(&t, "from the orchestrator", &Caller::Orchestrator)
        .unwrap();
    let card = offer(&hub).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(hub.thread_view(&t).unwrap().turns, 1, "held");
    answer(&hub, &card, SEND_AS_IS, &user()).unwrap();
    until(&mut sub, ended_turn(2)).await;
    let (_, two) = started(&hub, &t, 2);
    assert_eq!(two.prompt, "from the orchestrator");
    send(&hub, &t, "again");
    until(&mut sub, ended_turn(3)).await;
    assert!(offers(&hub).is_empty(), "not asked again");
    hub.shutdown().await;
}

/// `Don't ask for <name>` lets the message go and turns the thread's offers off (recorded,
/// the user's): later messages to an expired cache are not held. `thread.compact {offers:
/// "on"}` (the user's only) turns them back on.
#[tokio::test]
async fn dont_ask_turns_the_threads_offers_off_until_turned_on() {
    let dir = tempfile::tempdir().unwrap();
    let hub = open(dir.path(), IdleCompaction::Ask).await;
    let (t, mut sub) = warmed(&hub, dir.path(), "api", "fakec", EXPIRED).await;
    send(&hub, &t, "next");
    let card = offer(&hub).await;
    answer(&hub, &card, "don't ask for API", &user()).unwrap();
    until(&mut sub, ended_turn(2)).await;
    assert!(hub.thread_view(&t).unwrap().idle_compaction_off);
    assert_eq!(hub.get_thread(&t).unwrap()["idle_compaction_off"], true);
    let (_, evs) = hub.thread_events(&t, 0, 100_000).unwrap();
    let update = evs
        .iter()
        .find(|e| {
            matches!(&e.body, EventBody::ConversationUpdate(p)
            if p.ext.as_ref().is_some_and(|x| x["twr"]["idle_compaction"] == "off"))
        })
        .expect("the offers turned off");
    assert_eq!(update.source, Source::User("test".into()));
    // Another expired cache: not held.
    send(&hub, &t, "!context 250000\n!cache 3600 4000\nmore");
    until(&mut sub, ended_turn(3)).await;
    send(&hub, &t, "again");
    until(&mut sub, ended_turn(4)).await;
    assert!(offers(&hub).is_empty());
    let e = hub
        .compact_offers(&t, "on", &Caller::Orchestrator)
        .unwrap_err();
    assert_eq!(e.code, codes::UNAUTHORIZED);
    let e = hub.compact_offers(&t, "maybe", &user()).unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST);
    let r = hub.compact_offers(&t, "on", &user()).unwrap();
    assert_eq!(r["offers"], "on");
    assert_eq!(r["thread_name"], "api");
    assert!(!hub.thread_view(&t).unwrap().idle_compaction_off);
    assert_eq!(hub.thread_view(&t).unwrap().turns, 4, "nothing compacted");
    send(&hub, &t, "and again");
    offer(&hub).await;
    hub.shutdown().await;
}

/// No offer: for a prompt of the hub's, a context under 200k tokens, a cache not expired, a
/// context not known, a runtime that cannot compact, mode `off`, and a thread whose offers
/// are off. Each message goes at once.
#[tokio::test]
async fn no_offer_where_it_does_not_apply() {
    let dir = tempfile::tempdir().unwrap();
    let hub = open(dir.path(), IdleCompaction::Ask).await;
    let cases = [
        ("hubsent", "fakec", EXPIRED),
        (
            "small",
            "fakec",
            "!context 150000\n!cache 3600 3700\nwarm up",
        ),
        ("warm", "fakec", "!context 250000\n!cache 3600 0\nwarm up"),
        ("unknown", "fakec", "!cache 3600 3700\nwarm up"),
        ("plain", "fake", EXPIRED),
        ("off", "fakec", EXPIRED),
    ];
    for (name, runtime, first) in cases {
        let (t, mut sub) = warmed(&hub, dir.path(), name, runtime, first).await;
        match name {
            "hubsent" => {
                hub.send(&t, "a job ended", TurnFrom::Runtime, Source::Hub)
                    .unwrap();
            }
            "off" => {
                hub.compact_offers(&t, "off", &user()).unwrap();
                send(&hub, &t, "next");
            }
            _ => {
                send(&hub, &t, "next");
            }
        }
        until(&mut sub, ended_turn(2)).await;
        assert!(offers(&hub).is_empty(), "{name}");
    }
    hub.shutdown().await;
    // Mode `off`: none for anyone.
    let dir = tempfile::tempdir().unwrap();
    let hub = open(dir.path(), IdleCompaction::Off).await;
    let (t, mut sub) = warmed(&hub, dir.path(), "api", "fakec", EXPIRED).await;
    send(&hub, &t, "next");
    until(&mut sub, ended_turn(2)).await;
    assert!(offers(&hub).is_empty());
    hub.shutdown().await;
}

/// An open card outlives a hub restart: it is still tracked (answering it works) and the
/// message is still held.
#[tokio::test]
async fn an_open_offer_and_its_held_message_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let hub = open(dir.path(), IdleCompaction::Ask).await;
    let (t, sub) = warmed(&hub, dir.path(), "api", "fakec", EXPIRED).await;
    send(&hub, &t, "next");
    let card = offer(&hub).await;
    hub.shutdown().await;
    drop((sub, hub));
    let hub = timeout(WAIT, async {
        loop {
            match Hub::open(opts(dir.path(), IdleCompaction::Ask)).await {
                Ok(h) => return h,
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await
    .expect("reopening the hub");
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    assert_eq!(offers(&hub), vec![card.clone()]);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(hub.thread_view(&t).unwrap().turns, 1, "still held");
    let e = answer(&hub, &card, SEND_AS_IS, &Caller::Orchestrator).unwrap_err();
    assert!(e.starts_with(codes::RELAY_FORBIDDEN), "tracked: {e}");
    answer(&hub, &card, SEND_AS_IS, &user()).unwrap();
    until(&mut sub, ended_turn(2)).await;
    assert_eq!(started(&hub, &t, 2).1.prompt, "next");
    hub.shutdown().await;
}

/// A card that stops applying while no runtime runs (after a hub restart, a move to another
/// runtime) is closed when the move is recorded, and the held message goes to the new runtime.
#[tokio::test]
async fn a_move_with_no_runtime_closes_an_open_offer() {
    let dir = tempfile::tempdir().unwrap();
    let hub = open(dir.path(), IdleCompaction::Ask).await;
    let (t, sub) = warmed(&hub, dir.path(), "api", "fakec", EXPIRED).await;
    send(&hub, &t, "next");
    let card = offer(&hub).await;
    hub.shutdown().await;
    drop((sub, hub));
    let hub = timeout(WAIT, async {
        loop {
            match Hub::open(opts(dir.path(), IdleCompaction::Ask)).await {
                Ok(h) => return h,
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await
    .expect("reopening the hub");
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    assert_eq!(offers(&hub), vec![card.clone()]);
    hub.set_runtime_model(&t, Some("fake"), None, None, &user())
        .unwrap();
    until(&mut sub, ended_turn(2)).await;
    assert!(started(&hub, &t, 2).1.prompt.ends_with("next"));
    no_offer(&hub).await;
    let res = resolutions(&hub, &attn(&card));
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].1.answer, None);
    hub.shutdown().await;
}

/// The user's own `thread.compact` while a card holds a message answers it `Compact first`
/// (recorded as theirs): the compaction (the user's ask, no `compact_by`) runs before the held
/// message, not after it.
#[tokio::test]
async fn the_users_compact_answers_an_open_offer_and_runs_first() {
    let dir = tempfile::tempdir().unwrap();
    let hub = open(dir.path(), IdleCompaction::Ask).await;
    let (t, mut sub) = warmed(&hub, dir.path(), "api", "fakec", EXPIRED).await;
    send(&hub, &t, "next");
    let card = offer(&hub).await;
    let r = hub.compact_thread(&t, &user()).unwrap();
    assert_eq!(
        (&r["state"], &r["turn"]),
        (&Value::from("started"), &Value::from(2)),
        "{r}"
    );
    until(&mut sub, ended_turn(3)).await;
    let (source, two) = started(&hub, &t, 2);
    assert!(is_compact_turn(&two), "{two:?}");
    assert_eq!(source, Source::User("test".into()));
    assert_eq!(compact_by(&two), None);
    assert_eq!(started(&hub, &t, 3).1.prompt, "next");
    let r = resolutions(&hub, &attn(&card));
    assert_eq!(r.len(), 1);
    assert_eq!(
        (&r[0].0, r[0].1.by),
        (&Source::User("test".into()), ResolvedBy::User)
    );
    assert_eq!(
        r[0].1.answer.as_ref().map(|a| a.decision.as_str()),
        Some(COMPACT_FIRST)
    );
    assert!(offers(&hub).is_empty());
    hub.shutdown().await;
}

/// The user's `message.send_now` of the held message answers the card `Send as is` (recorded
/// as theirs) and starts it; the orchestrator may not send now at all.
#[tokio::test]
async fn send_now_of_the_held_message_answers_the_offer_send_as_is() {
    let dir = tempfile::tempdir().unwrap();
    let hub = open(dir.path(), IdleCompaction::Ask).await;
    let (t, mut sub) = warmed(&hub, dir.path(), "api", "fakec", EXPIRED).await;
    let m = send(&hub, &t, "next");
    let card = offer(&hub).await;
    let e = hub.send_now(&t, m, &Caller::Orchestrator).unwrap_err();
    assert_eq!(e.code, codes::UNAUTHORIZED);
    assert_eq!(offers(&hub).len(), 1, "still open");
    let r = hub.send_now(&t, m, &user()).unwrap();
    assert_eq!(
        (&r["sent_now"], &r["state"], &r["turn"]),
        (&Value::from(true), &Value::from("started"), &Value::from(2)),
        "{r}"
    );
    until(&mut sub, ended_turn(2)).await;
    assert_eq!(started(&hub, &t, 2).1.prompt, "next");
    let r = resolutions(&hub, &attn(&card));
    assert_eq!(r.len(), 1);
    assert_eq!(
        (&r[0].0, r[0].1.by),
        (&Source::User("test".into()), ResolvedBy::User)
    );
    assert_eq!(
        r[0].1.answer.as_ref().map(|a| a.decision.as_str()),
        Some(SEND_AS_IS)
    );
    assert!(offers(&hub).is_empty());
    hub.shutdown().await;
}

/// A runtime restarted with no turn (a settings change) that reports another session: the
/// card, about the previous session, is closed once that start is recorded, and the held
/// message goes.
#[tokio::test]
async fn a_restart_onto_another_session_closes_an_open_offer() {
    let dir = tempfile::tempdir().unwrap();
    let hub = open(dir.path(), IdleCompaction::Ask).await;
    let (t, mut sub) = warmed(&hub, dir.path(), "api", "fresh", "warm up").await;
    let first = hub.thread_view(&t).unwrap().session_id.unwrap();
    send(&hub, &t, "next");
    let card = offer(&hub).await;
    hub.set_mode(&t, "plan", &user()).unwrap();
    until(&mut sub, ended_turn(2)).await;
    assert_eq!(started(&hub, &t, 2).1.prompt, "next");
    assert_ne!(hub.thread_view(&t).unwrap().session_id.unwrap(), first);
    no_offer(&hub).await;
    let r = resolutions(&hub, &attn(&card));
    assert_eq!(r.len(), 1);
    assert_eq!(
        (r[0].1.by, r[0].1.answer.clone()),
        (ResolvedBy::Timeout, None)
    );
    hub.shutdown().await;
}

/// Taking the held message back closes the card (nothing of the user's is left to hold); so
/// does deleting the thread.
#[tokio::test]
async fn withdrawing_the_message_or_deleting_the_thread_closes_the_offer() {
    let dir = tempfile::tempdir().unwrap();
    let hub = open(dir.path(), IdleCompaction::Ask).await;
    let (t, _sub) = warmed(&hub, dir.path(), "api", "fakec", EXPIRED).await;
    let r = send(&hub, &t, "next");
    let card = offer(&hub).await;
    let w = hub.withdraw_message(&t, r, &user()).unwrap();
    assert_eq!(w["withdrawn"], true);
    no_offer(&hub).await;
    let res = resolutions(&hub, &attn(&card));
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].0, Source::Hub);
    assert_eq!(res[0].1.answer, None);
    assert_eq!(hub.thread_view(&t).unwrap().turns, 1);
    // Another message: a card again; then the thread is deleted.
    send(&hub, &t, "again");
    let card = offer(&hub).await;
    let d = hub
        .delete_threads(std::slice::from_ref(&t), &user())
        .await
        .unwrap();
    hub.respond_at(
        d["attn_id"].as_str().unwrap(),
        Answer::new("Delete"),
        d["revision"].as_u64(),
        &user(),
    )
    .unwrap();
    timeout(WAIT, async {
        while hub.thread_view(&t).is_ok() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("deleted");
    no_offer(&hub).await;
    let res = resolutions(&hub, &attn(&card));
    assert_eq!(res.len(), 1);
    assert_eq!(res[0].1.by, ResolvedBy::ThreadStopped);
    hub.shutdown().await;
}

/// `before_expiry`, its tick short: an idle worker whose hour-long cache of a large context
/// expires within 5 minutes (not within 30 s) is compacted by the hub (`source: hub`,
/// `compact_by: idle`), once. Not one whose cache expires later, nor one whose cache lives 5
/// minutes, nor while it works or has messages queued (after which it is).
#[tokio::test]
async fn before_expiry_compacts_an_idle_worker_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut o = opts(dir.path(), IdleCompaction::BeforeExpiry);
    o.idle_compaction_tick = Some(Duration::from_millis(100));
    let hub = Hub::open(o).await.unwrap();
    let soon = "!context 250000\n!cache 3600 3400\nwarm up";
    let (t, mut sub) = warmed(&hub, dir.path(), "api", "fakec", soon).await;
    until(&mut sub, ended_turn(2)).await;
    let (source, two) = started(&hub, &t, 2);
    assert!(is_compact_turn(&two));
    assert_eq!(source, Source::Hub);
    assert_eq!(compact_by(&two).as_deref(), Some("idle"));
    assert_eq!(two.from, TurnFrom::User);
    // Not again.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(hub.thread_view(&t).unwrap().turns, 2);
    // Expiring in an hour, in 10 s (past the margin), or a 5-minute cache: none.
    let cases = [
        ("later", "!context 250000\n!cache 3600 0\nwarm up"),
        ("margin", "!context 250000\n!cache 3600 3590\nwarm up"),
        ("short", "!context 250000\n!cache 300 100\nwarm up"),
    ];
    let mut quiet = Vec::new();
    for (name, first) in cases {
        quiet.push(warmed(&hub, dir.path(), name, "fakec", first).await);
    }
    tokio::time::sleep(Duration::from_millis(600)).await;
    for (t, _) in &quiet {
        assert_eq!(hub.thread_view(t).unwrap().turns, 1);
    }
    // Working, a message queued behind: no compaction until both ran.
    let (w, mut wsub) = warmed(
        &hub,
        dir.path(),
        "busy",
        "fakec",
        "!context 250000\n!cache 3600 0\nwarm up",
    )
    .await;
    send(
        &hub,
        &w,
        "!context 250000\n!cache 3600 3400\n!sleep 600\nlong",
    );
    send(&hub, &w, "queued");
    until(&mut wsub, ended_turn(3)).await;
    assert!(!is_compact_turn(&started(&hub, &w, 2).1));
    assert_eq!(started(&hub, &w, 3).1.prompt, "queued");
    until(&mut wsub, ended_turn(4)).await;
    assert_eq!(compact_by(&started(&hub, &w, 4).1).as_deref(), Some("idle"));
    hub.shutdown().await;
}

/// Mode `ask` compacts nothing on its own; and under `before_expiry`, a compaction that failed
/// is not tried again at every tick.
#[tokio::test]
async fn no_compaction_before_expiry_under_ask_nor_again_after_a_failure() {
    let dir = tempfile::tempdir().unwrap();
    let mut o = opts(dir.path(), IdleCompaction::Ask);
    o.idle_compaction_tick = Some(Duration::from_millis(100));
    let hub = Hub::open(o).await.unwrap();
    let soon = "!context 250000\n!cache 3600 3400\nwarm up";
    let (t, _sub) = warmed(&hub, dir.path(), "api", "fakec", soon).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(hub.thread_view(&t).unwrap().turns, 1);
    hub.shutdown().await;

    let dir = tempfile::tempdir().unwrap();
    let mut o = opts(dir.path(), IdleCompaction::BeforeExpiry);
    o.idle_compaction_tick = Some(Duration::from_millis(100));
    let hub = Hub::open(o).await.unwrap();
    let (t, mut sub) = warmed(&hub, dir.path(), "api", "failc", "go").await;
    until(&mut sub, |e| {
        matches!(&e.body, EventBody::AttentionRaised(a)
            if a.summary.contains("compacting its context failed"))
    })
    .await;
    assert_eq!(compact_by(&started(&hub, &t, 2).1).as_deref(), Some("idle"));
    tokio::time::sleep(Duration::from_millis(800)).await;
    let v = hub.thread_view(&t).unwrap();
    assert_eq!(v.turns, 2, "not tried again");
    assert!(v.cache_expires_at.is_some() && v.context.is_some());
    hub.shutdown().await;
}

/// A hub that died with an idle worker whose hour-long cache expires soon: the next hub
/// compacts nothing before its startup recovery closed that thread (its runtime died with the
/// previous hub: a compaction started earlier would resume its session beside the old process,
/// and the recovery would close the new runtime), and nothing after (a crashed thread is not
/// compacted). The previous hub left the runtime's spawn intent, whose processes the startup
/// looks for off the async runtime before the recovery: on a single-threaded runtime, a ticker
/// started before then runs in that wait.
#[test]
fn before_expiry_waits_for_the_startup_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let soon = "!context 250000\n!cache 3600 3400\nwarm up";
    let t = {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let t = rt.block_on(async {
            let hub = open(dir.path(), IdleCompaction::Ask).await;
            warmed(&hub, dir.path(), "api", "fakec", soon).await.0
        });
        // The hub dies without stopping anything.
        drop(rt);
        t
    };
    // The fake runtime reports no process: its spawn intent stays.
    let db = rusqlite::Connection::open(dir.path().join("data").join("twr.db")).unwrap();
    let intents: i64 = db
        .query_row(
            "SELECT count(*) FROM spawn_intents WHERE thread = ?1",
            [&t],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(intents, 1);
    drop(db);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut o = opts(dir.path(), IdleCompaction::BeforeExpiry);
        o.idle_compaction_tick = Some(Duration::from_millis(50));
        let hub = Hub::open(o).await.unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        let (_, evs) = hub.thread_events(&t, 0, 100_000).unwrap();
        let after: Vec<&str> = evs
            .iter()
            .skip_while(|e| !(e.kind() == "turn.ended" && e.turn == Some(1)))
            .map(|e| e.kind())
            .collect();
        assert!(after.contains(&"thread.crashed"), "{after:?}");
        assert!(!after.contains(&"turn.started"), "{after:?}");
        assert_eq!(hub.thread_view(&t).unwrap().turns, 1);
        hub.shutdown().await;
    });
}

/// Runtime `failc`: each prompt's turn ends at once with a context of 250 000 tokens and an
/// hour-long prompt cache written 3400 s ago (it expires in 200 s); a compaction reports it
/// failed and its turn ends `completed`.
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
            let worked = || {
                EventBody::TurnEnded(TurnEnded {
                    context: Some(ContextSize {
                        tokens: 250_000,
                        window: Some(1_000_000),
                    }),
                    prompt_cache: Some(PromptCache {
                        called_at: chrono::Utc::now() - chrono::Duration::seconds(3400),
                        ttl_s: Some(3600),
                    }),
                    ..Default::default()
                })
            };
            if prompt.is_some() {
                end.emit(worked(), Some(1));
            }
            while let Some(cmd) = end.cmds.recv().await {
                match cmd {
                    AdapterCmd::Send { turn, .. } => {
                        end.emit(worked(), Some(turn));
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
                        end.emit(EventBody::TurnEnded(TurnEnded::default()), Some(turn));
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

/// Runtime `fresh`: every start is a new session (`fresh-<n>`, whatever it was asked to
/// resume); each prompt's turn ends at once with a context of 250 000 tokens and an hour-long
/// prompt cache written 3700 s ago (expired); a compaction ends its turn at once.
struct FreshSessions;

static FRESH_STARTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl Adapter for FreshSessions {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "fresh",
            tier: Tier::Headless,
            description: "a new session at every start",
        }
    }

    fn compacts(&self) -> bool {
        true
    }

    fn spawn(&self, cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let (h, mut end) = AdapterHandle::pair();
        let n = FRESH_STARTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        tokio::spawn(async move {
            end.emit(
                EventBody::ThreadStarted(SessionPayload {
                    session_id: Some(format!("fresh-{n}")),
                    ..Default::default()
                }),
                None,
            );
            let worked = || {
                EventBody::TurnEnded(TurnEnded {
                    context: Some(ContextSize {
                        tokens: 250_000,
                        window: Some(1_000_000),
                    }),
                    prompt_cache: Some(PromptCache {
                        called_at: chrono::Utc::now() - chrono::Duration::seconds(3700),
                        ttl_s: Some(3600),
                    }),
                    ..Default::default()
                })
            };
            if prompt.is_some() {
                end.emit(worked(), Some(cfg.prompt_turn.unwrap_or(1)));
            }
            while let Some(cmd) = end.cmds.recv().await {
                match cmd {
                    AdapterCmd::Send { turn, .. } => {
                        end.emit(worked(), Some(turn));
                    }
                    AdapterCmd::Compact { turn } => {
                        end.emit(EventBody::TurnEnded(TurnEnded::default()), Some(turn));
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
