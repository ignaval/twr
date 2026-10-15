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
