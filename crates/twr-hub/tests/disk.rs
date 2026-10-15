//! Disk: worktree measurements, `thread.trim`, and the disk card (`hub::disk`), with real
//! temporary git repositories and the fake runtime. Trimming is Linux and macOS only.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::time::timeout;
use twr_adapters::fake::FakeAdapter;
use twr_core::adapter::Adapter;
use twr_core::config::Config;
use twr_core::event::*;
use twr_core::testkit::git::{commit_all, init_repo};
use twr_hub::{Caller, Hub, HubOptions, NewThread};

const WAIT: Duration = Duration::from_secs(20);

fn opts(dir: &Path, warn_gb: u64) -> HubOptions {
    let mut config = Config::default();
    config.threads.disk_warn_gb = warn_gb;
    let mut o = HubOptions::new(dir.join("data").join("twr.db"), config);
    o.adapters = Arc::new(|name: &str| -> Option<Arc<dyn Adapter>> {
        (name == "fake").then(|| Arc::new(FakeAdapter::new()) as Arc<dyn Adapter>)
    });
    o.git_tick = Some(Duration::ZERO);
    o.stall_after = Some(Duration::ZERO);
    o.disk_tick = Some(Duration::ZERO);
    o
}

fn user() -> Caller {
    Caller::user("test")
}

/// A thread on a new repository, its worktree with `build` bytes of ignored `target/` and an
/// ignored `.env`. Its name and worktree.
async fn thread_with_build(hub: &Hub, root: &Path, name: &str, build: usize) -> (String, PathBuf) {
    let repo = init_repo(&root.join(format!("{name}-repo")));
    let t = hub
        .new_thread(
            NewThread {
                project: repo.to_string_lossy().into_owned(),
                runtime: Some("fake".into()),
                name: Some(name.into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap();
    let wt = PathBuf::from(t["worktree"].as_str().unwrap());
    std::fs::write(wt.join(".gitignore"), "target/\n.env\n").unwrap();
    commit_all(&wt, "ignore");
    std::fs::create_dir_all(wt.join("target/debug")).unwrap();
    std::fs::write(wt.join("target/debug/big"), vec![7u8; build]).unwrap();
    std::fs::write(wt.join(".env"), "KEY=1").unwrap();
    (name.to_string(), wt)
}

fn disk_cards(hub: &Hub) -> Vec<Value> {
    hub.attention_list(true)
        .into_iter()
        .filter(|i| {
            i["thread"] == HUB_THREAD && i["summary"].as_str().unwrap_or("").starts_with("Disk low")
        })
        .collect()
}

#[tokio::test]
async fn trim_deletes_an_idle_threads_build_output_and_records_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let hub = Hub::open(opts(&root, 0)).await.unwrap();
    let (name, wt) = thread_with_build(&hub, &root, "api", 300 * 1024).await;

    // A turn first: its runtime stays up, idle, until the trim stops it.
    hub.send(&name, "hello", TurnFrom::User, Source::User("test".into()))
        .unwrap();
    timeout(WAIT, async {
        loop {
            let v = hub.thread_view(&name).unwrap();
            if v.turns >= 1 && v.current_turn.is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();

    // Measured, then recorded on the thread.
    hub.disk_round().await;
    let view = hub.thread_view(&name).unwrap();
    let d = view.disk.clone().expect("measured");
    assert!(d.build_bytes >= 300 * 1024, "{d:?}");

    let out = hub.trim_thread(&name, &user()).await.unwrap();
    assert_eq!(out["thread_name"], "api");
    assert_eq!(out["deleted"], serde_json::json!(["target/"]));
    assert!(out["freed"].as_u64().unwrap() >= 300 * 1024, "{out}");
    assert!(!wt.join("target").exists());
    assert!(wt.join(".env").exists());
    let view = hub.thread_view(&name).unwrap();
    // Its idle runtime was stopped for the trim, which does not count as stopping the thread.
    assert!(!view.stopped, "{view:?}");
    let (_, freed) = view.trimmed.expect("trimmed");
    assert!(freed >= 300 * 1024);
    // The view takes what the trim freed off its last measurement.
    assert!(view.disk.unwrap().build_bytes < 300 * 1024);
    let (_, events) = hub.thread_events(&name, 0, 1000).unwrap();
    assert!(events
        .iter()
        .any(|e| matches!(&e.body, EventBody::WorktreeTrimmed(p) if p.deleted == ["target/"])));
    assert!(events.iter().any(|e| matches!(&e.body,
        EventBody::ThreadStopped(p) if p.ext.as_ref().and_then(|x| x.get("twr"))
            .and_then(|t| t.get("reason")).and_then(|r| r.as_str()) == Some(TRIM_RESTART_REASON))));

    // A thread whose turn runs is not trimmed.
    hub.send(
        &name,
        "!sleep 3000\nhi",
        TurnFrom::User,
        Source::User("test".into()),
    )
    .unwrap();
    timeout(WAIT, async {
        while hub.thread_view(&name).unwrap().current_turn.is_none() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let busy = hub.trim_thread(&name, &user()).await.unwrap_err();
    assert!(busy.message.contains("is running"), "{}", busy.message);
    hub.request_shutdown();
}

#[tokio::test]
async fn a_low_disk_raises_one_card_and_trim_trims_the_idle_threads() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    // A threshold no disk has: the box is always low.
    let hub = Hub::open(opts(&root, 1 << 40)).await.unwrap();
    let (big, big_wt) = thread_with_build(&hub, &root, "big", 101 << 20).await;
    let (small, _) = thread_with_build(&hub, &root, "small", 4096).await;

    hub.disk_round().await;
    let cards = disk_cards(&hub);
    assert_eq!(cards.len(), 1, "{cards:?}");
    let card = &cards[0];
    assert_eq!(card["thread"], HUB_THREAD);
    assert_eq!(card["options"], serde_json::json!(["Trim", "Not now"]));
    let summary = card["summary"].as_str().unwrap();
    assert!(summary.starts_with("Disk low on local:"), "{summary}");
    assert!(summary.contains("1 idle thread(s): big"), "{summary}");
    // Small build output is not worth naming.
    assert!(!summary.contains("small"), "{summary}");
    // Another round raises no second card.
    hub.disk_round().await;
    assert_eq!(disk_cards(&hub).len(), 1);
    // `hub.info` says the box is low.
    assert_eq!(hub.info()["disk"]["local"]["low"], true);

    // Only the user answers it.
    let attn = card["attn_id"].as_str().unwrap();
    hub.respond_at(
        attn,
        Answer::new("Trim"),
        card["revision"].as_u64(),
        &user(),
    )
    .unwrap();
    timeout(WAIT, async {
        while hub.thread_view(&big).unwrap().trimmed.is_none() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the card's Trim trims big");
    assert!(!big_wt.join("target").exists());
    assert!(disk_cards(&hub).is_empty());
    assert!(hub.thread_view(&small).unwrap().trimmed.is_none());
    // Answered: no new card while the disk stays low.
    hub.disk_round().await;
    assert!(disk_cards(&hub).is_empty());
    hub.request_shutdown();
}

/// `[projects.<name>] trim_idle_after`: the disk round trims the build output of that
/// project's threads idle that long, once until they work again, by the hub; another
/// project's threads are left alone.
#[tokio::test]
async fn a_project_trims_its_idle_threads_build_output() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let mut o = opts(&root, 0);
    let api = init_repo(&root.join("api-repo"));
    let web = init_repo(&root.join("web-repo"));
    for (name, repo, after) in [("api", &api, Some("1s")), ("web", &web, None)] {
        o.config.projects.insert(
            name.into(),
            twr_core::config::ProjectConfig {
                repo: repo.clone(),
                trim_idle_after: after.map(str::to_string),
                ..Default::default()
            },
        );
    }
    let hub = Hub::open(o).await.unwrap();
    let mut wts = Vec::new();
    for name in ["api", "web"] {
        let t = hub
            .new_thread(
                NewThread {
                    project: name.into(),
                    runtime: Some("fake".into()),
                    name: Some(format!("{name}-t")),
                    ..Default::default()
                },
                "test",
            )
            .await
            .unwrap();
        let wt = PathBuf::from(t["worktree"].as_str().unwrap());
        std::fs::write(wt.join(".gitignore"), "target/\n").unwrap();
        commit_all(&wt, "ignore");
        std::fs::create_dir_all(wt.join("target/debug")).unwrap();
        std::fs::write(wt.join("target/debug/big"), vec![7u8; 200 * 1024]).unwrap();
        wts.push(wt);
    }
    tokio::time::sleep(Duration::from_millis(1200)).await;
    hub.disk_round().await;
    assert!(
        !wts[0].join("target").exists(),
        "api's idle thread is trimmed"
    );
    assert!(
        wts[1].join("target").exists(),
        "web trims nothing by itself"
    );
    let (_, events) = hub.thread_events("api-t", 0, 1000).unwrap();
    assert!(events.iter().any(|e| e.source == Source::Hub
        && matches!(&e.body, EventBody::WorktreeTrimmed(p) if p.deleted == ["target/"])));
    // Not again until it works again: a new build is left for now.
    std::fs::create_dir_all(wts[0].join("target/debug")).unwrap();
    std::fs::write(wts[0].join("target/debug/big"), vec![7u8; 200 * 1024]).unwrap();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    hub.disk_round().await;
    assert!(wts[0].join("target").exists());
    hub.shutdown().await;
}

/// An idle trim deletes only the build output `[threads] idle_trim_disposable` names too
/// (by default what a build always makes again): an ignored `target/` goes, `build/` and
/// `dist/` stay, though `park_disposable` names them; `twr trim` then deletes those as well.
/// A project whose list is empty has no idle trims.
#[tokio::test]
async fn an_idle_trim_deletes_only_what_a_build_makes_again() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let mut o = opts(&root, 0);
    for (name, only) in [("api", None), ("web", Some(Vec::new()))] {
        o.config.projects.insert(
            name.into(),
            twr_core::config::ProjectConfig {
                repo: init_repo(&root.join(format!("{name}-repo"))),
                trim_idle_after: Some("1s".into()),
                idle_trim_disposable: only,
                ..Default::default()
            },
        );
    }
    let hub = Hub::open(o).await.unwrap();
    let mut wts = Vec::new();
    for name in ["api", "web"] {
        let t = hub
            .new_thread(
                NewThread {
                    project: name.into(),
                    runtime: Some("fake".into()),
                    name: Some(format!("{name}-t")),
                    ..Default::default()
                },
                "test",
            )
            .await
            .unwrap();
        let wt = PathBuf::from(t["worktree"].as_str().unwrap());
        std::fs::write(wt.join(".gitignore"), "target/\nbuild/\ndist/\n").unwrap();
        commit_all(&wt, "ignore");
        for d in ["target/debug", "build", "dist"] {
            std::fs::create_dir_all(wt.join(d)).unwrap();
            std::fs::write(wt.join(d).join("out"), vec![7u8; 64 * 1024]).unwrap();
        }
        wts.push(wt);
    }
    let wt = wts[0].clone();
    tokio::time::sleep(Duration::from_millis(1200)).await;
    hub.disk_round().await;
    assert!(!wt.join("target").exists(), "the idle trim deletes target/");
    assert!(wt.join("build/out").exists() && wt.join("dist/out").exists());
    let (_, events) = hub.thread_events("api-t", 0, 1000).unwrap();
    assert!(events.iter().any(|e| e.source == Source::Hub
        && matches!(&e.body, EventBody::WorktreeTrimmed(p) if p.deleted == ["target/"])));
    // web's list is empty: nothing trimmed, nothing recorded.
    assert!(wts[1].join("target/debug/out").exists());
    assert!(hub.thread_view("web-t").unwrap().trimmed.is_none());

    // `twr trim` goes by park_disposable alone.
    let out = hub.trim_thread("api-t", &user()).await.unwrap();
    let mut deleted: Vec<String> = serde_json::from_value(out["deleted"].clone()).unwrap();
    deleted.sort();
    assert_eq!(deleted, ["build/", "dist/"], "{out}");
    assert!(!wt.join("build").exists() && !wt.join("dist").exists());
    hub.shutdown().await;
}
