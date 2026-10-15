//! `thread new --runtime 'pty:<command line>'` through the hub: the command is split into
//! argv, recorded in `thread.created` `ext.twr.pty_command`, and handed to the pty adapter as
//! `TWR_PTY_COMMAND` on spawn and on resume. The program is a small `sh -c` loop under a real
//! pseudo-terminal (the adapter's timing is shortened so turns end fast).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::time::timeout;
use twr_adapters::pty::{PtyAdapter, Timing};
use twr_core::adapter::Adapter;
use twr_core::config::Config;
use twr_core::event::*;
use twr_core::proto::codes;
use twr_hub::hub::ThreadSubscription;
use twr_hub::{Hub, HubOptions, NewThread};

const WAIT: Duration = Duration::from_secs(20);

fn opts(dir: &Path) -> HubOptions {
    let config = Config {
        default_runtime: "fake".into(),
        ..Default::default()
    };
    let mut o = HubOptions::new(dir.join("data").join("twr.db"), config);
    // The built-in pty adapter (the command comes from the spawn env), with fast heuristics.
    o.adapters = Arc::new(|name: &str| -> Option<Arc<dyn Adapter>> {
        (name == "pty").then(|| {
            Arc::new(PtyAdapter::new().timing(Timing {
                idle: Duration::from_millis(400),
                settle: Duration::from_millis(100),
                startup_max: Duration::from_millis(1000),
                flush: Duration::from_millis(50),
            })) as Arc<dyn Adapter>
        })
    });
    o.stop_timeout = Duration::from_secs(5);
    o.git_tick = Some(Duration::ZERO);
    o.stall_after = Some(Duration::ZERO);
    o
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
            seen.iter().map(|e| e.kind()).collect::<Vec<_>>()
        )
    });
    seen
}

fn text(evs: &[Event]) -> String {
    evs.iter()
        .filter_map(|e| match &e.body {
            EventBody::AgentText(t) if t.delta => Some(t.text.as_str()),
            _ => None,
        })
        .collect()
}

fn new(project: &Path, runtime: &str) -> NewThread {
    NewThread {
        project: project.to_string_lossy().into_owned(),
        runtime: Some(runtime.into()),
        name: Some("aider".into()),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_pty_thread_runs_its_command_on_spawn_and_resume() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    // Quoting as a shell would; `$l` is for the inner shell, not expanded by twr. The program
    // shows an input prompt (`> `): the adapter types a turn only at one.
    let runtime = r#"pty:sh -c 'printf "> "; while read l; do echo "got [$l]"; printf "> "; done' ignored-arg0"#;
    let t = hub
        .new_thread(new(dir.path(), runtime), "test")
        .await
        .unwrap();
    assert_eq!(t["runtime"], "pty");
    let argv = json!([
        "sh",
        "-c",
        r#"printf "> "; while read l; do echo "got [$l]"; printf "> "; done"#,
        "ignored-arg0"
    ]);
    assert_eq!(t["pty_command"], argv);
    let id = t["id"].as_str().unwrap().to_string();
    let log = hub.thread_events(&id, 0, 10).unwrap().1;
    let EventBody::ThreadCreated(c) = &log[0].body else {
        panic!("{:?}", log[0])
    };
    assert_eq!(c.runtime, "pty");
    assert_eq!(c.ext.as_ref().unwrap()["twr"]["pty_command"], argv);

    let mut sub = hub.subscribe_thread(&id, 0).unwrap();
    hub.send(
        &id,
        "hello world",
        TurnFrom::User,
        Source::User("test".into()),
    )
    .unwrap();
    let evs = until(&mut sub, |e| e.kind() == "turn.ended").await;
    assert!(text(&evs).contains("got [hello world]"), "{:?}", text(&evs));
    let started = evs.iter().find(|e| e.kind() == "thread.started").unwrap();
    let EventBody::ThreadStarted(s) = &started.body else {
        panic!()
    };
    assert_eq!(s.ext.as_ref().unwrap()["pty"]["degraded"], true);
    let EventBody::TurnEnded(e) = &evs.last().unwrap().body else {
        panic!()
    };
    assert_eq!(e.result, TurnResult::Completed);
    until(&mut sub, |e| e.kind() == "report.ready").await;

    // Stopped, then sent again: the hub resumes the thread, which runs the command afresh
    // with the same TWR_PTY_COMMAND.
    hub.stop_thread(&id, "test").unwrap();
    until(&mut sub, |e| e.kind() == "thread.stopped").await;
    hub.send(&id, "again", TurnFrom::User, Source::User("test".into()))
        .unwrap();
    let evs = until(&mut sub, |e| e.kind() == "turn.ended").await;
    assert!(text(&evs).contains("got [again]"), "{:?}", text(&evs));
    let EventBody::TurnEnded(e) = &evs.last().unwrap().body else {
        panic!()
    };
    assert_eq!(e.result, TurnResult::Completed);
    hub.shutdown().await;
}

#[tokio::test]
async fn bad_pty_runtimes_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    for (runtime, says) in [
        ("pty", "needs the command"),
        ("pty:", "names no program"),
        ("pty:   ", "names no program"),
        ("pty:aider 'unterminated", "unterminated"),
    ] {
        let e = hub
            .new_thread(new(dir.path(), runtime), "test")
            .await
            .unwrap_err();
        assert_eq!(e.code, codes::BAD_REQUEST, "{runtime}: {e:?}");
        assert!(e.message.contains(says), "{runtime}: {}", e.message);
    }
    let long = format!("pty:{}", "x".repeat(5000));
    let e = hub
        .new_thread(new(dir.path(), &long), "test")
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST);
    assert!(hub.list_threads().is_empty());
    hub.shutdown().await;
}
