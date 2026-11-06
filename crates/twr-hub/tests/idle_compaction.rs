//! A worker whose prompt cache expired with a large context (`[threads] idle_compaction`,
//! `hub::idle_compact`): automatic compaction before a user/orchestrator message, the cases
//! it does not apply, old offer cards recovered without a question, and the compaction the
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
use twr_core::adapter::{Adapter, AdapterCmd, AdapterHandle, AdapterInfo, SpawnConfig, Tier};
use twr_core::config::{Config, IdleCompaction};
use twr_core::event::*;
use twr_core::proto::codes;
use twr_hub::hub::{ThreadSubscription, COMPACT_FIRST, SEND_AS_IS};
use twr_hub::{Caller, Hub, HubOptions, NewThread};

const WAIT: Duration = Duration::from_secs(10);

/// An expired cache (written 3700 s ago, living an hour) of a large context.
const EXPIRED: &str = "!context 250000\n!cache 3600 3700\nwarm up";

/// Runtimes: `fakec` (compacts on request), `fake` (cannot), `failc` (its compactions fail;
/// each prompt's turn reports a large context and an hour-long cache that expires in 200 s).
fn opts(dir: &Path, mode: IdleCompaction) -> HubOptions {
    let mut config = Config::default();
    config.threads.idle_compaction = mode;
    let mut o = HubOptions::new(dir.join("data").join("twr.db"), config);
    o.adapters = Arc::new(|name: &str| -> Option<Arc<dyn Adapter>> {
        match name {
            "fake" => Some(Arc::new(FakeAdapter::new())),
            "fakec" => Some(Arc::new(FakeAdapter::new().with_compaction())),
            "failc" => Some(Arc::new(FailingCompaction)),
            "crashc" => Some(Arc::new(CrashingCompaction)),
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

/// Both user and orchestrator messages resume without a question, with maintenance first.
/// `before_expiry` has the same behavior when the cache has already expired.
#[tokio::test]
async fn expired_cache_compacts_and_continues_without_asking() {
    for mode in [IdleCompaction::AfterExpiry, IdleCompaction::BeforeExpiry] {
        for caller in [user(), Caller::Orchestrator] {
            let dir = tempfile::tempdir().unwrap();
            let hub = open(dir.path(), mode).await;
            let (t, mut sub) = warmed(&hub, dir.path(), "api", "fakec", EXPIRED).await;
            hub.send_by(&t, "next", &caller).unwrap();
            send(&hub, &t, "then this");
            until(&mut sub, ended_turn(4)).await;
            let (source, two) = started(&hub, &t, 2);
            assert!(is_compact_turn(&two));
            assert_eq!(source, Source::Hub);
            assert_eq!(compact_by(&two).as_deref(), Some("expired"));
            assert_eq!(started(&hub, &t, 3).1.prompt, "next");
            assert_eq!(started(&hub, &t, 3).0, caller.source());
            assert_eq!(started(&hub, &t, 4).1.prompt, "then this");
            assert!(offers(&hub).is_empty());
            let (events, _) = hub.control_read(0, 100_000).unwrap();
            assert!(!events.iter().any(|(_, e)| matches!(&e.body,
                EventBody::AttentionRaised(a) if a.ext.as_ref().is_some_and(|x| x["twr"].get("idle_offer").is_some()))));
            hub.shutdown().await;
        }
    }
}

/// Existing per-thread opt-outs still work, and can be re-enabled without compacting now.
#[tokio::test]
async fn per_thread_compaction_can_be_disabled_and_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let hub = open(dir.path(), IdleCompaction::AfterExpiry).await;
    let (t, mut sub) = warmed(&hub, dir.path(), "api", "fakec", EXPIRED).await;
    hub.compact_offers(&t, "off", &user()).unwrap();
    send(&hub, &t, "next");
    until(&mut sub, ended_turn(2)).await;
    assert!(!is_compact_turn(&started(&hub, &t, 2).1));
    assert!(hub.thread_view(&t).unwrap().idle_compaction_off);
    assert_eq!(
        hub.compact_offers(&t, "on", &Caller::Orchestrator)
            .unwrap_err()
            .code,
        codes::UNAUTHORIZED
    );
    assert_eq!(
        hub.compact_offers(&t, "maybe", &user()).unwrap_err().code,
        codes::BAD_REQUEST
    );
    hub.compact_offers(&t, "on", &user()).unwrap();
    assert_eq!(hub.thread_view(&t).unwrap().turns, 2);
    send(&hub, &t, "again");
    until(&mut sub, ended_turn(4)).await;
    assert!(is_compact_turn(&started(&hub, &t, 3).1));
    assert_eq!(started(&hub, &t, 4).1.prompt, "again");
    assert!(offers(&hub).is_empty());
    hub.shutdown().await;
}

/// No automatic compaction: for a prompt of the hub's, a context under 200k tokens, a cache not expired, a
/// context not known, a runtime that cannot compact, mode `off`, and a thread whose offers
/// are off. Each message goes at once.
#[tokio::test]
async fn no_offer_where_it_does_not_apply() {
    let dir = tempfile::tempdir().unwrap();
    let hub = open(dir.path(), IdleCompaction::AfterExpiry).await;
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
        assert!(!is_compact_turn(&started(&hub, &t, 2).1), "{name}");
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

/// The orchestrator's own conversation is excluded even when it reports an expired cache.
#[tokio::test]
async fn the_orchestrator_thread_never_auto_compacts_after_expiry() {
    let dir = tempfile::tempdir().unwrap();
    let mut o = opts(dir.path(), IdleCompaction::AfterExpiry);
    o.config.orchestrator.runtime = Some("claude-code".into());
    o.config.orchestrator.cwd = Some(dir.path().to_path_buf());
    let fallback = o.adapters.clone();
    o.adapters = Arc::new(move |name: &str| -> Option<Arc<dyn Adapter>> {
        if name == "claude-code" {
            Some(Arc::new(FakeAdapter::new().with_compaction()))
        } else {
            fallback(name)
        }
    });
    o.claude_policy = twr_adapters::claude_code::guard::PolicySources {
        managed_dir: Some(dir.path().join("managed")),
        config_dir: Some(dir.path().join("claude-config")),
    };
    let hub = Hub::open(o).await.unwrap();
    hub.say_to_orchestrator(EXPIRED, None, &user()).unwrap();
    let t = hub.orchestrator_thread().unwrap();
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    until(&mut sub, ended_turn(1)).await;
    assert!(hub.thread_view(&t).unwrap().is_orchestrator());
    hub.say_to_orchestrator("next", None, &user()).unwrap();
    until(&mut sub, ended_turn(2)).await;
    assert!(!is_compact_turn(&started(&hub, &t, 2).1));
    assert!(started(&hub, &t, 2).1.prompt.ends_with("next"));
    assert!(offers(&hub).is_empty());
    hub.shutdown().await;
}

/// Simulate an old hub's persisted card and queue. On startup the card closes and the
/// queued messages continue automatically (or without compaction when disabled/stale).
#[tokio::test]
async fn legacy_offer_and_queue_resume_without_an_answer_after_restart() {
    for case in ["due", "off", "stale", "empty", "retire_failure"] {
        let dir = tempfile::tempdir().unwrap();
        let hub = open(dir.path(), IdleCompaction::AfterExpiry).await;
        let (t, sub) = warmed(&hub, dir.path(), "api", "fakec", EXPIRED).await;
        let view = hub.get_thread(&t).unwrap();
        let card = new_id();
        hub.append(
            HUB_THREAD,
            None,
            Source::Hub,
            EventBody::AttentionRaised(AttentionRaised {
                attn_id: card.clone(),
                reason: AttentionReason::Question,
                options: vec![
                    COMPACT_FIRST.into(),
                    SEND_AS_IS.into(),
                    "Don't ask for api".into(),
                ],
                ext: Some(
                    serde_json::from_value(serde_json::json!({"twr": {"idle_offer": {
                        "thread": t, "session": view["session_id"],
                        "expires_at": view["cache_expires_at"], "tokens": 250_000
                    }}}))
                    .unwrap(),
                ),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        if case == "stale" {
            hub.append(
                &t,
                Some(1),
                Source::Adapter("fakec".into()),
                EventBody::TurnEnded(TurnEnded {
                    prompt_cache: Some(PromptCache {
                        called_at: chrono::Utc::now(),
                        ttl_s: Some(3600),
                    }),
                    ..Default::default()
                }),
                None,
            )
            .unwrap();
        }
        hub.shutdown().await;
        drop((sub, hub));
        if case != "empty" {
            let mut store = twr_hub::store::Store::open(&dir.path().join("data/twr.db")).unwrap();
            let head = store.control_head().unwrap();
            store
                .enqueue_prompt(
                    &t,
                    "next",
                    TurnFrom::User,
                    None,
                    &Source::User("test".into()),
                    head,
                )
                .unwrap();
        }
        let db = rusqlite::Connection::open(dir.path().join("data/twr.db")).unwrap();
        if case == "retire_failure" {
            db.execute_batch(
                "CREATE TRIGGER inject BEFORE INSERT ON events
                WHEN NEW.kind = 'attention.resolved'
                BEGIN SELECT RAISE(ABORT, 'injected retire failure'); END;",
            )
            .unwrap();
        }
        let hub = open(
            dir.path(),
            if case == "off" {
                IdleCompaction::Off
            } else {
                IdleCompaction::AfterExpiry
            },
        )
        .await;
        let mut sub = hub.subscribe_thread(&t, 0).unwrap();
        if case == "empty" {
            assert_eq!(hub.thread_view(&t).unwrap().turns, 1);
        } else {
            let last = if matches!(case, "due" | "retire_failure") {
                3
            } else {
                2
            };
            until(&mut sub, ended_turn(last)).await;
            assert_eq!(started(&hub, &t, last).1.prompt, "next", "{case}");
            assert_eq!(
                is_compact_turn(&started(&hub, &t, 2).1),
                matches!(case, "due" | "retire_failure"),
                "{case}"
            );
        }
        if case == "retire_failure" {
            // Even though the old card could not be closed, it did not hold the message.
            // Once writes recover the next dispatch retires it normally.
            db.execute_batch("DROP TRIGGER inject").unwrap();
            send(&hub, &t, "cleanup");
            until(&mut sub, ended_turn(4)).await;
        }
        assert!(offers(&hub).is_empty(), "{case}");
        let res = resolutions(&hub, &card);
        assert_eq!(res.len(), 1, "{case}");
        assert_eq!(res[0].0, Source::Hub);
        assert_eq!(res[0].1.by, ResolvedBy::Timeout);
        assert_eq!(res[0].1.answer, None);
        hub.shutdown().await;
    }
}

/// A failed compaction does not loop on the same cache, and the original message still runs.
#[tokio::test]
async fn expired_cache_compaction_failure_is_not_retried() {
    let dir = tempfile::tempdir().unwrap();
    let hub = open(dir.path(), IdleCompaction::AfterExpiry).await;
    let (t, mut sub) = warmed(&hub, dir.path(), "api", "failc", "go").await;
    hub.append(
        &t,
        Some(1),
        Source::Adapter("fakec".into()),
        EventBody::TurnEnded(TurnEnded {
            prompt_cache: Some(PromptCache {
                called_at: chrono::Utc::now() - chrono::Duration::seconds(3700),
                ttl_s: Some(3600),
            }),
            ..Default::default()
        }),
        None,
    )
    .unwrap();
    send(&hub, &t, "next");
    until(&mut sub, ended_turn(3)).await;
    assert_eq!(
        compact_by(&started(&hub, &t, 2).1).as_deref(),
        Some("expired")
    );
    assert_eq!(started(&hub, &t, 3).1.prompt, "next");
    assert!(hub.attention_list(true).iter().any(|a| a["summary"]
        .as_str()
        .is_some_and(|s| s.contains("compacting its context failed"))));
    assert!(offers(&hub).is_empty());
    hub.shutdown().await;
}

/// A crash during automatic maintenance must not strand either sender's queued message.
#[tokio::test]
async fn a_crash_during_expired_compaction_continues_the_original_message() {
    for caller in [user(), Caller::Orchestrator] {
        let dir = tempfile::tempdir().unwrap();
        let hub = open(dir.path(), IdleCompaction::AfterExpiry).await;
        let (t, mut sub) = warmed(&hub, dir.path(), "api", "crashc", "warm up").await;
        hub.send_by(&t, "next", &caller).unwrap();
        until(&mut sub, ended_turn(3)).await;
        assert!(is_compact_turn(&started(&hub, &t, 2).1));
        assert_eq!(started(&hub, &t, 3).1.prompt, "next");
        assert!(!hub.thread_queue(&t, &user()).unwrap()["held"]
            .as_bool()
            .unwrap());
        assert!(offers(&hub).is_empty());
        hub.shutdown().await;
    }
}

/// An unrecorded automatic compaction belongs to its triggering message. Withdrawing it
/// must not leave maintenance behind; explicit send-now must still mean send as-is.
#[tokio::test]
async fn failed_compaction_start_is_cancelled_on_withdraw_or_bypassed_by_send_now() {
    for action in ["withdraw", "send_now"] {
        let dir = tempfile::tempdir().unwrap();
        let hub = open(dir.path(), IdleCompaction::AfterExpiry).await;
        let (t, mut sub) = warmed(&hub, dir.path(), "api", "fakec", EXPIRED).await;
        let c = rusqlite::Connection::open(dir.path().join("data/twr.db")).unwrap();
        c.execute_batch(
            "CREATE TRIGGER inject BEFORE INSERT ON events
            WHEN NEW.kind = 'turn.started' AND NEW.data LIKE '%\"compact\":true%'
            BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;",
        )
        .unwrap();
        let message = send(&hub, &t, "next");
        assert_eq!(hub.thread_view(&t).unwrap().turns, 1);
        c.execute_batch("DROP TRIGGER inject").unwrap();
        if action == "withdraw" {
            assert_eq!(
                hub.withdraw_message(&t, message, &user()).unwrap()["withdrawn"],
                true
            );
            assert_eq!(
                hub.thread_view(&t).unwrap().turns,
                1,
                "no orphaned compaction"
            );
            // No waiver was recorded for an attempt that never started: a later message
            // still compacts first.
            send(&hub, &t, "replacement");
            until(&mut sub, ended_turn(3)).await;
            assert!(is_compact_turn(&started(&hub, &t, 2).1));
            assert_eq!(started(&hub, &t, 3).1.prompt, "replacement");
        } else {
            let r = hub.send_now(&t, message, &user()).unwrap();
            assert_eq!(r["state"], "started", "{r}");
            until(&mut sub, ended_turn(2)).await;
            assert_eq!(started(&hub, &t, 2).1.prompt, "next");
            assert!(!is_compact_turn(&started(&hub, &t, 2).1));
        }
        assert!(offers(&hub).is_empty());
        hub.shutdown().await;
    }
}

/// Already requested manual maintenance wins over automatic compaction, with no duplicate.
#[tokio::test]
async fn a_manual_compaction_already_queued_runs_only_once() {
    let dir = tempfile::tempdir().unwrap();
    let hub = open(dir.path(), IdleCompaction::AfterExpiry).await;
    let (t, mut sub) = warmed(
        &hub,
        dir.path(),
        "api",
        "fakec",
        "!context 250000\n!cache 3600 0\nwarm up",
    )
    .await;
    send(
        &hub,
        &t,
        "!context 250000\n!cache 3600 3700\n!sleep 300\nwork",
    );
    assert_eq!(hub.compact_thread(&t, &user()).unwrap()["state"], "queued");
    assert_eq!(hub.compact_thread(&t, &user()).unwrap()["already"], true);
    send(&hub, &t, "next");
    until(&mut sub, ended_turn(4)).await;
    assert!(is_compact_turn(&started(&hub, &t, 3).1));
    assert_eq!(compact_by(&started(&hub, &t, 3).1), None);
    assert_eq!(started(&hub, &t, 4).1.prompt, "next");
    hub.shutdown().await;
}

/// A hub prompt ahead of a user message refreshes the cache without unnecessary maintenance.
#[tokio::test]
async fn a_hub_prompt_refreshes_the_cache_before_a_queued_user_message() {
    let dir = tempfile::tempdir().unwrap();
    let hub = open(dir.path(), IdleCompaction::AfterExpiry).await;
    let (t, mut sub) = warmed(&hub, dir.path(), "api", "fakec", EXPIRED).await;
    hub.send(
        &t,
        "!context 250000\n!cache 3600 0\n!sleep 300\nwake",
        TurnFrom::Runtime,
        Source::Hub,
    )
    .unwrap();
    send(&hub, &t, "next");
    until(&mut sub, ended_turn(3)).await;
    assert!(!is_compact_turn(&started(&hub, &t, 2).1));
    assert_eq!(started(&hub, &t, 3).1.prompt, "next");
    assert!(offers(&hub).is_empty());
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

/// Mode `after_expiry` does not compact before expiry; and under `before_expiry`, a compaction that failed
/// is not tried again at every tick.
#[tokio::test]
async fn no_compaction_before_expiry_under_after_expiry_nor_again_after_a_failure() {
    let dir = tempfile::tempdir().unwrap();
    let mut o = opts(dir.path(), IdleCompaction::AfterExpiry);
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
            let hub = open(dir.path(), IdleCompaction::AfterExpiry).await;
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

/// Reports an expired cache for work, but exits abnormally when asked to compact.
struct CrashingCompaction;

impl Adapter for CrashingCompaction {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "crashc",
            tier: Tier::Headless,
            description: "crashes during compaction",
        }
    }
    fn compacts(&self) -> bool {
        true
    }
    fn spawn(&self, cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let (h, mut end) = AdapterHandle::pair();
        tokio::spawn(async move {
            end.emit(
                EventBody::ThreadStarted(SessionPayload {
                    session_id: Some("s-crash".into()),
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
                    AdapterCmd::Compact { .. } => {
                        end.exited(Some(1), "compaction crash");
                        return;
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
