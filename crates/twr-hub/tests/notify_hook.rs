//! The notification hook (`hub::hook`): `[notify] command` runs for what needs the user, with
//! the TUI's rules: held, dropped when answered meanwhile or while a TUI is looked at.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::time::timeout;
use twr_adapters::fake::FakeAdapter;
use twr_core::adapter::Adapter;
use twr_core::config::Config;
use twr_core::event::*;
use twr_hub::{Caller, Hub, HubOptions, NewThread};

const WAIT: Duration = Duration::from_secs(20);

/// A hub whose hook appends its stdin to `<dir>/out.jsonl` and its filled-in argument to
/// `<dir>/args`.
fn opts(dir: &Path, detail: bool) -> HubOptions {
    let mut config = Config::default();
    let out = dir.join("out");
    config.notify.command = vec![
        "sh".into(),
        "-c".into(),
        "cat >> \"$0.jsonl\"; printf '%s\\n' \"$1\" >> \"$0.args\"".into(),
        out.to_string_lossy().into_owned(),
        "{title}|{body}|{workspace}".into(),
    ];
    config.notify.detail = detail;
    let mut o = HubOptions::new(dir.join("data").join("twr.db"), config);
    o.adapters = Arc::new(|name: &str| -> Option<Arc<dyn Adapter>> {
        (name == "fake").then(|| Arc::new(FakeAdapter::new()) as Arc<dyn Adapter>)
    });
    o.git_tick = Some(Duration::ZERO);
    o.stall_after = Some(Duration::ZERO);
    o.disk_tick = Some(Duration::ZERO);
    o
}

async fn thread(hub: &Hub, project: &Path, name: &str) -> String {
    std::fs::create_dir_all(project).unwrap();
    let t = hub
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
    t["id"].as_str().unwrap().to_string()
}

/// Raises a permission item `attn` on `t`.
fn ask(hub: &Hub, t: &str, attn: &str) {
    hub.append(
        t,
        None,
        Source::Adapter("fake".into()),
        EventBody::AttentionRaised(AttentionRaised {
            attn_id: attn.into(),
            reason: AttentionReason::Permission,
            summary: "Bash curl -H 'Authorization: s3cret'".into(),
            options: vec!["allow".into(), "deny".into()],
            ..Default::default()
        }),
        None,
    )
    .unwrap();
}

fn delivered(dir: &Path) -> Vec<Value> {
    std::fs::read_to_string(dir.join("out.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn args(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("out.args"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

async fn until_delivered(dir: &Path, n: usize) -> Vec<Value> {
    timeout(WAIT, async {
        loop {
            // Its arguments are written after its stdin.
            let d = delivered(dir);
            if d.len() >= n && args(dir).len() >= n {
                return d;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the hook never ran")
}

fn out_dir() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_path_buf();
    (dir, path)
}

#[tokio::test]
async fn a_permission_runs_the_command_with_who_and_what_only() {
    let (_tmp, dir) = out_dir();
    let hub = Hub::open(opts(&dir, false)).await.unwrap();
    let t = thread(&hub, &dir.join("proj"), "api").await;
    ask(&hub, &t, "p1");
    let d = until_delivered(&dir, 1).await;
    assert_eq!(d[0]["title"], "twr: api");
    assert_eq!(d[0]["body"], "api needs permission");
    assert_eq!(d[0]["workspace"], "default");
    assert_eq!(d[0]["items"][0]["event"], "permission");
    assert_eq!(d[0]["items"][0]["thread"], t.as_str());
    assert_eq!(d[0]["items"][0]["item"], "p1");
    assert!(!d[0].to_string().contains("s3cret"), "{}", d[0]);
    assert_eq!(args(&dir), ["twr: api|api needs permission|default"]);
    hub.request_shutdown();
}

#[tokio::test]
async fn with_detail_the_request_text_goes_too() {
    let (_tmp, dir) = out_dir();
    let hub = Hub::open(opts(&dir, true)).await.unwrap();
    let t = thread(&hub, &dir.join("proj"), "api").await;
    ask(&hub, &t, "p1");
    let d = until_delivered(&dir, 1).await;
    assert!(
        d[0]["items"][0]["text"]
            .as_str()
            .unwrap()
            .contains("s3cret"),
        "{}",
        d[0]
    );
    hub.request_shutdown();
}

#[tokio::test]
async fn what_is_answered_meanwhile_or_seen_in_a_tui_never_notifies() {
    let (_tmp, dir) = out_dir();
    let hub = Hub::open(opts(&dir, false)).await.unwrap();
    let t = thread(&hub, &dir.join("proj"), "api").await;
    // Answered within its hold.
    ask(&hub, &t, "p1");
    hub.append(
        &t,
        None,
        Source::User("test".into()),
        EventBody::AttentionResolved(AttentionResolved {
            attn_id: "p1".into(),
            ..Default::default()
        }),
        None,
    )
    .unwrap();
    // Raised while a TUI is looked at: one of two, the other one no longer.
    hub.set_looking(1, true);
    hub.set_looking(2, true);
    hub.set_looking(2, false);
    ask(&hub, &t, "p2");
    tokio::time::sleep(Duration::from_millis(3500)).await;
    assert!(delivered(&dir).is_empty(), "{:?}", delivered(&dir));
    // Not looked at any more: the next one notifies, alone.
    hub.set_looking(1, false);
    ask(&hub, &t, "p3");
    let d = until_delivered(&dir, 1).await;
    assert_eq!(d.len(), 1);
    assert_eq!(d[0]["items"].as_array().unwrap().len(), 1);
    assert_eq!(d[0]["items"][0]["item"], "p3");
    hub.request_shutdown();
}

#[tokio::test]
async fn what_the_command_leaves_running_is_ended_with_it() {
    let (_tmp, dir) = out_dir();
    let mut o = opts(&dir, false);
    let pidfile = dir.join("left.pid");
    o.config.notify.command = vec![
        "sh".into(),
        "-c".into(),
        "sleep 300 2>/dev/null >/dev/null </dev/null & echo $! > \"$0\"; cat > /dev/null".into(),
        pidfile.to_string_lossy().into_owned(),
    ];
    let hub = Hub::open(o).await.unwrap();
    let t = thread(&hub, &dir.join("proj"), "api").await;
    ask(&hub, &t, "p1");
    let pid: u32 = timeout(WAIT, async {
        loop {
            if let Some(p) = std::fs::read_to_string(&pidfile)
                .ok()
                .and_then(|s| s.trim().parse().ok())
            {
                return p;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the hook never ran");
    timeout(WAIT, async {
        loop {
            let alive = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .is_ok_and(|s| !s.contains(") Z"));
            if !alive {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("what the hook left running was not ended");
    hub.request_shutdown();
}

#[tokio::test]
async fn what_falls_due_while_looked_at_is_dropped_even_while_a_command_runs() {
    let (_tmp, dir) = out_dir();
    let mut o = opts(&dir, false);
    // A slow command: 8 s after it wrote what it got.
    o.config.notify.command[2] =
        "cat >> \"$0.jsonl\"; printf '%s\\n' \"$1\" >> \"$0.args\"; sleep 8".into();
    let hub = Hub::open(o).await.unwrap();
    let t = thread(&hub, &dir.join("proj"), "api").await;
    ask(&hub, &t, "p1");
    until_delivered(&dir, 1).await;
    // While it runs: looked at, and p2 falls due meanwhile (5 s after p1 went out).
    hub.set_looking(1, true);
    ask(&hub, &t, "p2");
    tokio::time::sleep(Duration::from_millis(6500)).await;
    // Not looked at any more, the command still running: p2 stays dropped.
    hub.set_looking(1, false);
    tokio::time::sleep(Duration::from_millis(5000)).await;
    let d = delivered(&dir);
    assert_eq!(d.len(), 1, "{d:?}");
    assert_eq!(d[0]["items"][0]["item"], "p1");
    hub.request_shutdown();
}

#[tokio::test]
async fn a_shutdown_ends_a_command_still_running() {
    let (_tmp, dir) = out_dir();
    let mut o = opts(&dir, false);
    let pidfile = dir.join("cmd.pid");
    o.config.notify.command = vec![
        "sh".into(),
        "-c".into(),
        "echo $$ > \"$0\"; exec sleep 300".into(),
        pidfile.to_string_lossy().into_owned(),
    ];
    let hub = Hub::open(o).await.unwrap();
    let t = thread(&hub, &dir.join("proj"), "api").await;
    ask(&hub, &t, "p1");
    let pid: u32 = timeout(WAIT, async {
        loop {
            if let Some(p) = std::fs::read_to_string(&pidfile)
                .ok()
                .and_then(|s| s.trim().parse().ok())
            {
                return p;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the hook never ran");
    // Straight to shutdown, without asking for it first.
    timeout(Duration::from_secs(12), hub.shutdown())
        .await
        .expect("the shutdown waited out its bound");
    let alive =
        std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|s| !s.contains(") Z"));
    assert!(!alive, "the command outlived the shutdown");
}

/// A report the hook told of, rewritten after the hub restarted, revises that news and stays
/// quiet, as it does without a restart: the hook's policy recalls the replay.
#[tokio::test]
async fn a_report_rewritten_after_a_restart_does_not_notify_again() {
    let (_tmp, dir) = out_dir();
    let hub = Hub::open(opts(&dir, false)).await.unwrap();
    let t = thread(&hub, &dir.join("proj"), "api").await;
    hub.send(
        &t,
        "Fixed the bug. Tests pass.",
        TurnFrom::User,
        Source::User("test".into()),
    )
    .unwrap();
    let d = until_delivered(&dir, 1).await;
    assert_eq!(d[0]["items"][0]["event"], "report", "{}", d[0]);
    assert_eq!(d[0]["items"][0]["turn"], 1, "{}", d[0]);
    timeout(WAIT, hub.shutdown())
        .await
        .expect("the hub did not stop");
    drop(hub);

    let hub = Hub::open(opts(&dir, false)).await.unwrap();
    hub.write_report(
        &t,
        Some(1),
        "Fixed the bug, partly.",
        Grade::Partial,
        &Caller::user("test"),
    )
    .unwrap();
    // Well past its hold: nothing went.
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(delivered(&dir).len(), 1, "{:?}", delivered(&dir));
    // The hook still runs: what is news after the restart notifies.
    ask(&hub, &t, "p1");
    let d = until_delivered(&dir, 2).await;
    assert_eq!(d.len(), 2, "{d:?}");
    assert_eq!(d[1]["items"][0]["event"], "permission", "{}", d[1]);
    hub.request_shutdown();
}
