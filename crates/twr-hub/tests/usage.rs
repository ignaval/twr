//! Usage tracking in the hub (`hub::usage`): a runtime's `runtime.limits` reports become
//! `account.usage` on the hub thread when they say something new, never events of the thread;
//! `usage.get` answers the merged windows, also after a restart.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use chrono::{TimeDelta, Utc};
use tokio::time::timeout;
use twr_core::adapter::{
    Adapter, AdapterCmd, AdapterEnd, AdapterHandle, AdapterInfo, SpawnConfig, Tier,
};
use twr_core::config::Config;
use twr_core::event::*;
use twr_core::usage::UsageWindow;
use twr_hub::{Hub, HubOptions, NewThread};

const WAIT: Duration = Duration::from_secs(10);

/// Runtime `test`: every session reports `reports` (in order) and ends its turn.
struct Reporter(Vec<RuntimeLimits>);

impl Adapter for Reporter {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "test",
            tier: Tier::Headless,
            description: "usage test adapter",
        }
    }

    fn spawn(&self, _cfg: SpawnConfig, _prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let (h, mut end): (AdapterHandle, AdapterEnd) = AdapterHandle::pair();
        let reports = self.0.clone();
        tokio::spawn(async move {
            end.emit(
                EventBody::ThreadStarted(SessionPayload {
                    session_id: Some("s-1".into()),
                    ..Default::default()
                }),
                None,
            );
            for r in reports {
                end.emit(EventBody::RuntimeLimits(r), None);
            }
            end.emit(
                EventBody::TurnEnded(TurnEnded {
                    result: TurnResult::Completed,
                    ..Default::default()
                }),
                None,
            );
            while let Some(cmd) = end.cmds.recv().await {
                if cmd == AdapterCmd::Stop {
                    break;
                }
            }
            end.exited(Some(0), "");
        });
        Ok(h)
    }
}

fn opts(dir: &Path, reports: Vec<RuntimeLimits>) -> HubOptions {
    let mut o = HubOptions::new(dir.join("data").join("twr.db"), Config::default());
    let test: Arc<dyn Adapter> = Arc::new(Reporter(reports));
    o.adapters = Arc::new(move |n: &str| (n == "test").then(|| Arc::clone(&test)));
    o.stop_timeout = Duration::from_secs(5);
    o
}

fn window(id: &str, pct: f64, resets_in_hours: i64) -> UsageWindow {
    UsageWindow {
        window: id.into(),
        used_pct: pct,
        resets_at: Some(Utc::now() + TimeDelta::hours(resets_in_hours)),
        minutes: None,
        observed_at: None,
    }
}

fn report(windows: Vec<UsageWindow>, source: &str, plan: Option<&str>) -> RuntimeLimits {
    RuntimeLimits {
        windows,
        source: source.into(),
        plan: plan.map(str::to_string),
        ..Default::default()
    }
}

fn usage_events(hub: &Hub) -> Vec<(Event, AccountUsage)> {
    let (items, _) = hub.control_read(0, 1000).unwrap();
    items
        .into_iter()
        .filter_map(|(_, e)| match &e.body {
            EventBody::AccountUsage(u) => {
                let u = u.clone();
                Some((e, u))
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn reports_become_account_usage_and_survive_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let five = window("five_hour", 22.4, 3);
    let week = window("seven_day", 44.0, 100);
    let reports = vec![
        report(vec![five.clone(), week.clone()], "read", Some("max")),
        // The same whole percentages and resets: nothing new.
        report(
            vec![
                UsageWindow {
                    used_pct: 21.6,
                    ..five.clone()
                },
                week.clone(),
            ],
            "event",
            None,
        ),
        // Only the weekly window moved: the 5-hour one is kept.
        report(
            vec![UsageWindow {
                used_pct: 45.0,
                ..week.clone()
            }],
            "event",
            None,
        ),
        // A window that already reset says nothing about now.
        report(vec![window("other", 99.0, -1)], "event", None),
    ];
    let hub = Hub::open(opts(dir.path(), reports.clone())).await.unwrap();
    let v = hub
        .new_thread(
            NewThread {
                project: dir.path().to_string_lossy().into_owned(),
                runtime: Some("test".into()),
                name: Some("w".into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap();
    let id = v["id"].as_str().unwrap().to_string();
    let mut sub = hub.subscribe_thread(&id, 0).unwrap();
    hub.send(&id, "hi", TurnFrom::User, Source::User("test".into()))
        .unwrap();
    let mut thread_events = Vec::new();
    timeout(WAIT, async {
        loop {
            let ev = sub.next().await.unwrap().expect("hub stopped");
            let done = ev.kind() == "turn.ended";
            thread_events.push((*ev).clone());
            if done {
                return;
            }
        }
    })
    .await
    .expect("the turn ends");
    assert!(
        thread_events.iter().all(|e| e.kind() != "runtime.limits"),
        "reports are not the thread's events"
    );

    let recorded = usage_events(&hub);
    assert_eq!(recorded.len(), 2, "{recorded:?}");
    for (e, u) in &recorded {
        assert_eq!(e.thread, HUB_THREAD);
        assert!(e.is_control());
        assert_eq!(u.runtime, "test");
        assert_eq!(u.account, None);
        assert_eq!(u.thread.as_deref(), Some(id.as_str()));
        assert_eq!(u.plan.as_deref(), Some("max"), "the plan is kept");
    }
    // Each window as reported, stamped with when (`observed_at`).
    let bare = |ws: &[UsageWindow]| -> Vec<UsageWindow> {
        ws.iter()
            .map(|w| UsageWindow {
                observed_at: None,
                ..w.clone()
            })
            .collect()
    };
    let seen = |u: &AccountUsage, id: &str| {
        u.windows
            .iter()
            .find(|w| w.window == id)
            .and_then(|w| w.observed_at)
            .expect("stamped")
    };
    assert_eq!(recorded[0].1.source, "read");
    assert_eq!(
        bare(&recorded[0].1.windows),
        vec![five.clone(), week.clone()]
    );
    assert_eq!(recorded[1].1.source, "event");
    let weekly_45 = UsageWindow {
        used_pct: 45.0,
        ..week.clone()
    };
    assert_eq!(
        bare(&recorded[1].1.windows),
        vec![five.clone(), weekly_45.clone()]
    );
    // The five-hour window the weekly report left out keeps its own report's time.
    let (first, second) = (&recorded[0].1, &recorded[1].1);
    assert_eq!(seen(second, "five_hour"), seen(first, "five_hour"));
    assert!(seen(second, "seven_day") >= seen(first, "seven_day"));
    assert_eq!(second.observed_at, Some(seen(second, "seven_day")));

    let check = |u: serde_json::Value| {
        let rts = u["runtimes"].as_array().unwrap();
        assert_eq!(rts.len(), 1, "{u}");
        let r = &rts[0];
        assert_eq!(r["runtime"], "test");
        assert_eq!(r["label"], "test");
        assert_eq!(r["plan"], "max");
        assert_eq!(r["stale"], false);
        assert_eq!(r["account"], serde_json::Value::Null);
        let labels: Vec<&str> = r["windows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| w["label"].as_str().unwrap())
            .collect();
        assert_eq!(labels, ["5h", "wk"]);
        assert_eq!(r["windows"][1]["used_pct"], 45.0);
        assert_eq!(r["windows"][1]["remaining_pct"], 55.0);
        assert_eq!(r["windows"][0]["used_pct"], 22.4);
        assert_eq!(r["windows"][0]["remaining_pct"], 77.6);
        assert_eq!(r["windows"][0]["minutes"], 300);
    };
    check(hub.usage());
    hub.shutdown().await;
    drop((sub, hub));

    // Replayed from the log.
    let hub = timeout(WAIT, async {
        loop {
            match Hub::open(opts(dir.path(), Vec::new())).await {
                Ok(h) => return h,
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await
    .expect("the hub reopens");
    check(hub.usage());
    hub.shutdown().await;
}

/// No report, no entry: a runtime nobody used is not listed.
#[tokio::test]
async fn nothing_known_lists_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path(), Vec::new())).await.unwrap();
    assert_eq!(hub.usage()["runtimes"], serde_json::json!([]));
    hub.shutdown().await;
}

/// `usage.get`'s labels follow the accounts' rule, not the entries: the native account's one
/// report names it while a second account is usable (`Claude:default`), under its current
/// name once named (`Claude:personal`), and names nothing once the second account is removed.
#[tokio::test]
async fn labels_follow_the_accounts() {
    let dir = tempfile::tempdir().unwrap();
    let reports = vec![report(vec![window("seven_day", 44.0, 100)], "read", None)];
    let mut o = opts(dir.path(), reports.clone());
    let claude: Arc<dyn Adapter> = Arc::new(Reporter(reports));
    o.adapters = Arc::new(move |n: &str| (n == "claude-code").then(|| Arc::clone(&claude)));
    o.accounts_root = Some(dir.path().join("accounts"));
    let native = dir.path().join("native");
    o.native_dir = Some(Arc::new(move |rt: &str| Some(native.join(rt))));
    let hub = Hub::open(o).await.unwrap();
    let v = hub
        .new_thread(
            NewThread {
                project: dir.path().to_string_lossy().into_owned(),
                runtime: Some("claude-code".into()),
                name: Some("w".into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap();
    let id = v["id"].as_str().unwrap().to_string();
    hub.send(&id, "hi", TurnFrom::User, Source::User("test".into()))
        .unwrap();
    timeout(WAIT, async {
        while usage_events(&hub).is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the report recorded");
    let label = |hub: &Hub| hub.usage()["runtimes"][0]["label"].clone();
    // One account alone: the runtime.
    assert_eq!(label(&hub), "Claude");
    let user = twr_hub::Caller::user("test");
    hub.account_add(
        twr_hub::hub::AccountAddParams {
            runtime: "claude-code".into(),
            account: "work".into(),
            ..Default::default()
        },
        &user,
    )
    .unwrap();
    // A second usable account, though only the native one reported.
    assert_eq!(label(&hub), "Claude:default");
    hub.account_add(
        twr_hub::hub::AccountAddParams {
            runtime: "claude-code".into(),
            account: "personal".into(),
            native: true,
            ..Default::default()
        },
        &user,
    )
    .unwrap();
    assert_eq!(label(&hub), "Claude:personal");
    hub.account_remove(
        twr_hub::hub::AccountRemoveParams {
            runtime: "claude-code".into(),
            account: "work".into(),
            ..Default::default()
        },
        &user,
    )
    .await
    .unwrap();
    assert_eq!(label(&hub), "Claude");
    hub.shutdown().await;
}

/// Runtime `claude-code`: each turn reports the next of `reports`, then ends.
struct PerTurn(Vec<RuntimeLimits>);

impl Adapter for PerTurn {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "claude-code",
            tier: Tier::Headless,
            description: "usage test adapter, a report a turn",
        }
    }

    fn spawn(&self, _cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let (h, mut end): (AdapterHandle, AdapterEnd) = AdapterHandle::pair();
        let mut reports = self.0.clone().into_iter();
        tokio::spawn(async move {
            end.emit(
                EventBody::ThreadStarted(SessionPayload {
                    session_id: Some("s-1".into()),
                    ..Default::default()
                }),
                None,
            );
            let turn = |end: &AdapterEnd, r: Option<RuntimeLimits>, t: Option<u64>| {
                if let Some(r) = r {
                    end.emit(EventBody::RuntimeLimits(r), t);
                }
                end.emit(
                    EventBody::TurnEnded(TurnEnded {
                        result: TurnResult::Completed,
                        ..Default::default()
                    }),
                    t,
                );
            };
            if prompt.is_some() {
                turn(&end, reports.next(), Some(1));
            }
            while let Some(cmd) = end.cmds.recv().await {
                match cmd {
                    AdapterCmd::Send { turn: t, .. } => turn(&end, reports.next(), Some(t)),
                    AdapterCmd::Stop => break,
                    _ => {}
                }
            }
            end.exited(Some(0), "");
        });
        Ok(h)
    }
}

/// A session's usage is its account directory's: its start is stamped with it
/// (`ext.twr.account_dir`), and once the account's name leads elsewhere (removed with
/// `force`, added again in another directory) what the old session reports is dropped, never
/// recorded as the new directory's.
#[tokio::test]
async fn a_report_of_an_accounts_previous_directory_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let reports = vec![
        report(vec![window("seven_day", 44.0, 100)], "read", None),
        report(vec![window("seven_day", 80.0, 100)], "read", None),
    ];
    let mut o = opts(&root, Vec::new());
    let claude: Arc<dyn Adapter> = Arc::new(PerTurn(reports));
    o.adapters = Arc::new(move |n: &str| (n == "claude-code").then(|| Arc::clone(&claude)));
    o.accounts_root = Some(root.join("accounts"));
    let native = root.join("native");
    o.native_dir = Some(Arc::new(move |rt: &str| Some(native.join(rt))));
    let hub = Hub::open(o).await.unwrap();
    let user = twr_hub::Caller::user("test");
    let (a, b) = (root.join("acc/a"), root.join("acc/b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let add = |dir: &Path| twr_hub::hub::AccountAddParams {
        runtime: "claude-code".into(),
        account: "side".into(),
        dir: Some(dir.to_path_buf()),
        ..Default::default()
    };
    hub.account_add(add(&a), &user).unwrap();
    let v = hub
        .new_thread(
            NewThread {
                project: root.to_string_lossy().into_owned(),
                runtime: Some("claude-code".into()),
                account: Some("side".into()),
                name: Some("w".into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap();
    let id = v["id"].as_str().unwrap().to_string();
    let ends = |hub: &Hub| {
        let (_, evs) = hub.thread_events(&id, 0, 10_000).unwrap();
        evs.iter()
            .filter(|e| matches!(e.body, EventBody::TurnEnded(_)))
            .count()
    };
    let turn = |n: usize| {
        let (hub, ends) = (&hub, &ends);
        hub.send(&id, "hi", TurnFrom::User, Source::User("test".into()))
            .unwrap();
        async move {
            timeout(WAIT, async {
                while ends(hub) < n {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("the turn ended");
        }
    };
    turn(1).await;
    let recorded = usage_events(&hub);
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert_eq!(recorded[0].1.account.as_deref(), Some("side"));
    let (_, evs) = hub.thread_events(&id, 0, 10_000).unwrap();
    let started = evs
        .iter()
        .find_map(|e| match &e.body {
            EventBody::ThreadStarted(p) => p.ext.clone(),
            _ => None,
        })
        .unwrap();
    assert_eq!(started["twr"]["account"], "side", "{started:?}");
    assert_eq!(started["twr"]["account_dir"], a.to_str().unwrap());

    hub.account_remove(
        twr_hub::hub::AccountRemoveParams {
            runtime: "claude-code".into(),
            account: "side".into(),
            force: true,
            ..Default::default()
        },
        &user,
    )
    .await
    .unwrap();
    hub.account_add(add(&b), &user).unwrap();
    // The old session, still running in `a`, reports again: not `side`'s now.
    turn(2).await;
    let (_, evs) = hub.thread_events(&id, 0, 10_000).unwrap();
    let last = evs.iter().rev().find_map(|e| match &e.body {
        EventBody::TurnEnded(t) => Some(t.result),
        _ => None,
    });
    assert_eq!(last, Some(TurnResult::Completed), "the old session ran it");
    // Its one record, and the removal's that cleared it: nothing of `b`.
    let recorded = usage_events(&hub);
    assert_eq!(recorded.len(), 2, "{recorded:?}");
    assert_eq!(recorded[0].1.dir.as_deref(), a.to_str());
    assert!(
        recorded[1].1.windows.is_empty() && recorded[1].1.dir.is_none(),
        "{recorded:?}"
    );
    hub.shutdown().await;
}

/// Runtime `claude-code`: each turn of any of its sessions reports the next of the shared
/// `reports`, then ends.
struct Shared(Arc<std::sync::Mutex<std::collections::VecDeque<RuntimeLimits>>>);

impl Adapter for Shared {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "claude-code",
            tier: Tier::Headless,
            description: "usage test adapter, the next shared report a turn",
        }
    }

    fn spawn(&self, _cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let (h, mut end): (AdapterHandle, AdapterEnd) = AdapterHandle::pair();
        let reports = Arc::clone(&self.0);
        tokio::spawn(async move {
            end.emit(
                EventBody::ThreadStarted(SessionPayload {
                    session_id: Some("s-1".into()),
                    ..Default::default()
                }),
                None,
            );
            let turn = |end: &AdapterEnd, t: Option<u64>| {
                if let Some(r) = reports.lock().unwrap().pop_front() {
                    end.emit(EventBody::RuntimeLimits(r), t);
                }
                end.emit(
                    EventBody::TurnEnded(TurnEnded {
                        result: TurnResult::Completed,
                        ..Default::default()
                    }),
                    t,
                );
            };
            if prompt.is_some() {
                turn(&end, Some(1));
            }
            while let Some(cmd) = end.cmds.recv().await {
                match cmd {
                    AdapterCmd::Send { turn: t, .. } => turn(&end, Some(t)),
                    AdapterCmd::Stop => break,
                    _ => {}
                }
            }
            end.exited(Some(0), "");
        });
        Ok(h)
    }
}

/// Options for a hub in `root` whose `claude-code` sessions report `reports` in turn
/// ([`Shared`]), with `config`.
fn shared_opts(root: &Path, config: Config, reports: Vec<RuntimeLimits>) -> HubOptions {
    let mut o = opts(root, Vec::new());
    o.config = config;
    let claude: Arc<dyn Adapter> =
        Arc::new(Shared(Arc::new(std::sync::Mutex::new(reports.into()))));
    o.adapters = Arc::new(move |n: &str| (n == "claude-code").then(|| Arc::clone(&claude)));
    o.accounts_root = Some(root.join("accounts"));
    let native = root.join("native");
    o.native_dir = Some(Arc::new(move |rt: &str| Some(native.join(rt))));
    o
}

/// A new `claude-code` thread `name` on `account` in `root`, after one turn: its id.
async fn thread_after_a_turn(hub: &Hub, root: &Path, name: &str, account: &str) -> String {
    let v = hub
        .new_thread(
            NewThread {
                project: root.to_string_lossy().into_owned(),
                runtime: Some("claude-code".into()),
                account: Some(account.into()),
                name: Some(name.into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap();
    let id = v["id"].as_str().unwrap().to_string();
    hub.send(&id, "hi", TurnFrom::User, Source::User("test".into()))
        .unwrap();
    timeout(WAIT, async {
        loop {
            let (_, evs) = hub.thread_events(&id, 0, 10_000).unwrap();
            if evs
                .iter()
                .any(|e| matches!(e.body, EventBody::TurnEnded(_)))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the turn ended");
    id
}

/// What `usage.get` says of `claude-code`'s `account`: its windows' ids and used shares.
fn shown_of(hub: &Hub, account: &str) -> Option<Vec<(String, f64)>> {
    let u = hub.usage();
    let e = u["runtimes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["runtime"] == "claude-code" && r["account"] == account)?
        .clone();
    Some(
        e["windows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| {
                (
                    w["window"].as_str().unwrap().to_string(),
                    w["used_pct"].as_f64().unwrap(),
                )
            })
            .collect(),
    )
}

/// Usage follows the account's directory: removed and added again elsewhere, the account
/// shows nothing (every client is told: a record with no windows) until a session in the new
/// directory reports, and that report, though it names one window, keeps none of the old
/// directory's.
#[tokio::test]
async fn usage_follows_the_accounts_directory() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let reports = vec![
        report(
            vec![window("five_hour", 30.0, 3), window("seven_day", 44.0, 100)],
            "read",
            None,
        ),
        report(vec![window("seven_day", 50.0, 100)], "event", None),
    ];
    let hub = Hub::open(shared_opts(&root, Config::default(), reports))
        .await
        .unwrap();
    let user = twr_hub::Caller::user("test");
    let (a, b) = (root.join("acc/a"), root.join("acc/b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let add = |dir: &Path| twr_hub::hub::AccountAddParams {
        runtime: "claude-code".into(),
        account: "work".into(),
        dir: Some(dir.to_path_buf()),
        ..Default::default()
    };
    hub.account_add(add(&a), &user).unwrap();
    thread_after_a_turn(&hub, &root, "w", "work").await;
    assert_eq!(
        shown_of(&hub, "work"),
        Some(vec![("five_hour".into(), 30.0), ("seven_day".into(), 44.0)])
    );
    assert_eq!(usage_events(&hub)[0].1.dir.as_deref(), a.to_str());

    hub.account_remove(
        twr_hub::hub::AccountRemoveParams {
            runtime: "claude-code".into(),
            account: "work".into(),
            force: true,
            ..Default::default()
        },
        &user,
    )
    .await
    .unwrap();
    hub.account_add(add(&b), &user).unwrap();
    assert_eq!(shown_of(&hub, "work"), None, "{}", hub.usage());
    let recorded = usage_events(&hub);
    assert_eq!(recorded.len(), 2, "{recorded:?}");
    let clear = &recorded[1].1;
    assert_eq!(clear.account.as_deref(), Some("work"));
    assert!(clear.windows.is_empty() && clear.dir.is_none(), "{clear:?}");
    // The clearing record came with the removal, not with the add.
    let (items, _) = hub.control_read(0, 1000).unwrap();
    let kinds: Vec<&str> = items
        .iter()
        .map(|(_, e)| e.kind())
        .filter(|k| k.starts_with("account.") && *k != "account.status")
        .collect();
    assert_eq!(
        kinds,
        [
            "account.added",
            "account.usage",
            "account.removed",
            "account.usage",
            "account.added"
        ]
    );

    // A session in `b` reports its weekly window alone: nothing of `a`'s 5-hour one.
    thread_after_a_turn(&hub, &root, "w2", "work").await;
    assert_eq!(
        shown_of(&hub, "work"),
        Some(vec![("seven_day".into(), 50.0)])
    );
    let recorded = usage_events(&hub);
    assert_eq!(recorded.len(), 3, "{recorded:?}");
    assert_eq!(recorded[2].1.dir.as_deref(), b.to_str());
    hub.shutdown().await;
}

/// A config account whose directory config changes while the hub is down: the next start
/// clears what the old directory reported (once: a later start records nothing more).
#[tokio::test]
async fn a_config_directory_change_clears_the_usage_at_start() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let (a, b) = (root.join("acc/a"), root.join("acc/b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let config = |dir: &Path| {
        let mut c = Config::default();
        c.accounts.entry("claude-code".into()).or_default().insert(
            "team".into(),
            twr_core::config::AccountConfig {
                dir: Some(dir.to_path_buf()),
                ..Default::default()
            },
        );
        c
    };
    let reports = vec![report(vec![window("seven_day", 44.0, 100)], "read", None)];
    let hub = Hub::open(shared_opts(&root, config(&a), reports))
        .await
        .unwrap();
    thread_after_a_turn(&hub, &root, "w", "team").await;
    assert_eq!(
        shown_of(&hub, "team"),
        Some(vec![("seven_day".into(), 44.0)])
    );
    hub.shutdown().await;
    drop(hub);

    // The same directory again: nothing changes.
    let hub = Hub::open(shared_opts(&root, config(&a), Vec::new()))
        .await
        .unwrap();
    assert_eq!(usage_events(&hub).len(), 1);
    assert_eq!(
        shown_of(&hub, "team"),
        Some(vec![("seven_day".into(), 44.0)])
    );
    hub.shutdown().await;
    drop(hub);

    for _ in 0..2 {
        let hub = Hub::open(shared_opts(&root, config(&b), Vec::new()))
            .await
            .unwrap();
        assert_eq!(shown_of(&hub, "team"), None, "{}", hub.usage());
        let recorded = usage_events(&hub);
        assert_eq!(recorded.len(), 2, "{recorded:?}");
        assert!(recorded[1].1.windows.is_empty(), "{recorded:?}");
        hub.shutdown().await;
    }
}
