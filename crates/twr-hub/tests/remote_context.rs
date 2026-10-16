//! Standing context for threads on a remote box comes from the hub's log (the
//! latest `context.updated` of the project), never from a file on the hub. The runner here is
//! in-process, so the "remote" repository paths exist on this machine too: files planted there
//! must never be read or written by the hub. Dispatch, `context.read` and `context.set` are
//! covered, for a project known from remote threads, for a configured project whose `box`
//! is remote, and for a configured project with no `box` whose threads all run on the remote
//! box (`--box`), including one whose repository is absent from the hub's filesystem.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;
use twr_adapters::FakeAdapter;
use twr_core::adapter::Adapter;
use twr_core::blob::blob_ref;
use twr_core::config::{BoxConfig, Config, ProjectConfig};
use twr_core::event::*;
use twr_core::runner::Secret;
use twr_hub::hub::ThreadSubscription;
use twr_hub::listener::RunnerListener;
use twr_hub::{Caller, Hub, HubOptions, NewThread};
use twr_runner::{LocalBox, LocalBoxOptions, RunnerOptions};

const TOKEN: &str = "ctx-token-77-0123456789abcdef";
const BOX: &str = "gpu";
const WAIT: Duration = Duration::from_secs(15);
const PLANTED: &str = "PLANTED FILE, never read by the hub\n";

async fn until(sub: &mut ThreadSubscription, pred: impl Fn(&Event) -> bool) -> Vec<Event> {
    let mut seen = Vec::new();
    tokio::time::timeout(WAIT, async {
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
    .unwrap_or_else(|_| panic!("timed out; saw {} events", seen.len()));
    seen
}

fn plant(repo: &Path) -> std::path::PathBuf {
    let file = repo.join(".twr").join("context.md");
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, PLANTED).unwrap();
    file
}

fn words(evs: &[Event]) -> String {
    evs.iter()
        .filter_map(|e| match &e.body {
            EventBody::AgentText(t) if t.delta => Some(t.text.trim().to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn context_updates(hub: &Hub) -> Vec<ContextUpdated> {
    let (items, _) = hub.control_read(0, 100_000).unwrap();
    items
        .into_iter()
        .filter_map(|(_, e)| match e.body {
            EventBody::ContextUpdated(c) => Some(c),
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_threads_get_the_logged_context_and_no_hub_file_is_touched() {
    let dir = tempfile::tempdir().unwrap();
    let remote_repo = dir.path().join("proj");
    let configured_repo = dir.path().join("api");
    let boxless_repo = dir.path().join("web");
    // Configured with no box, all its threads remote, its repository only on the runner (the
    // directory is removed from this machine once its thread exists).
    let runner_only_repo = dir.path().join("edge");
    std::fs::create_dir_all(&remote_repo).unwrap();
    std::fs::create_dir_all(&configured_repo).unwrap();
    std::fs::create_dir_all(&boxless_repo).unwrap();
    std::fs::create_dir_all(&runner_only_repo).unwrap();
    let planted = plant(&remote_repo);
    let planted_api = plant(&configured_repo);
    let planted_web = plant(&boxless_repo);

    let mut config = Config {
        default_runtime: "fake".into(),
        ..Default::default()
    };
    config.hub.listen = Some("127.0.0.1:0".into());
    config.boxes.insert(
        BOX.into(),
        BoxConfig {
            token: Some(TOKEN.into()),
            ..Default::default()
        },
    );
    config.projects.insert(
        "api".into(),
        ProjectConfig {
            repo: configured_repo.clone(),
            runtime: None,
            box_name: Some(BOX.into()),
            ..Default::default()
        },
    );
    // Configured with no box: its threads here run on the remote box (`--box`).
    config.projects.insert(
        "web".into(),
        ProjectConfig {
            repo: boxless_repo.clone(),
            runtime: None,
            box_name: None,
            ..Default::default()
        },
    );
    config.projects.insert(
        "edge".into(),
        ProjectConfig {
            repo: runner_only_repo.clone(),
            runtime: None,
            box_name: None,
            ..Default::default()
        },
    );
    let mut o = HubOptions::new(dir.path().join("hub").join("twr.db"), config);
    o.stop_timeout = Duration::from_secs(5);
    o.git_tick = Some(Duration::ZERO);
    o.stall_after = Some(Duration::ZERO);
    let listener = RunnerListener::bind(&o.config.hub).await.unwrap().unwrap();
    let url = listener.url();
    let hub = Hub::open(o).await.unwrap();
    let _server = listener.serve(hub.clone());
    let rdir = dir.path().join("r");
    let mut ro = RunnerOptions::new(&url, Secret::new(TOKEN), BOX, &rdir);
    ro.backoff_min = Duration::from_millis(20);
    ro.backoff_max = Duration::from_millis(200);
    ro.refused_retry = Duration::from_millis(200);
    // The fake runtime on the box, whatever the environment enables.
    let mut box_opts = LocalBoxOptions::new(&rdir, BOX);
    box_opts.adapters = Arc::new(|name: &str| -> Option<Arc<dyn Adapter>> {
        match name {
            "fake" => Some(Arc::new(FakeAdapter::new())),
            other => twr_adapters::find(other),
        }
    });
    if !box_opts.runtimes.iter().any(|r| r == "fake") {
        box_opts.runtimes.push("fake".into());
    }
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let runner = tokio::spawn(twr_runner::run_with(
        ro,
        LocalBox::open(box_opts).unwrap(),
        async move {
            let _ = stop_rx.await;
        },
    ));
    let deadline = tokio::time::Instant::now() + WAIT;
    while !hub.boxes().iter().any(|(n, online, _)| n == BOX && *online) {
        assert!(tokio::time::Instant::now() < deadline, "box never online");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    // A thread of project `proj` on the box (its repo path is the runner's).
    let t = hub
        .new_thread(
            NewThread {
                project: remote_repo.to_string_lossy().into_owned(),
                runtime: Some("fake".into()),
                name: Some("remote".into()),
                box_name: Some(BOX.into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let user = Caller::user("test");

    // context.read: the log (nothing yet), not the planted file.
    let r = hub.context_read("proj").await.unwrap();
    assert_eq!(
        (r["remote"].as_bool(), r["content"].as_str()),
        (Some(true), Some(""))
    );

    // Dispatch without a logged context: nothing injected, nothing read, nothing logged.
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    hub.send(&t, "first", TurnFrom::User, Source::User("test".into()))
        .unwrap();
    let evs = until(&mut sub, |e| e.kind() == "turn.ended").await;
    assert_eq!(words(&evs), "first");
    assert!(context_updates(&hub).is_empty());

    // context.set records the context and writes no file.
    let r = hub
        .context_set("proj", "Use the log.\n", None, &user)
        .await
        .unwrap();
    assert_eq!(
        (r["changed"].as_bool(), r["remote"].as_bool()),
        (Some(true), Some(true))
    );
    assert_eq!(std::fs::read_to_string(&planted).unwrap(), PLANTED);
    let r = hub
        .context_set("proj", "Use the log.\n", None, &user)
        .await
        .unwrap();
    assert_eq!(r["changed"], false);
    let r = hub.context_read("proj").await.unwrap();
    assert_eq!(r["content"], "Use the log.\n");

    // Dispatch injects the logged context (the fake runtime echoes its prompt).
    hub.send(&t, "second", TurnFrom::User, Source::User("test".into()))
        .unwrap();
    let evs = until(&mut sub, |e| e.kind() == "turn.ended" && e.turn == Some(2)).await;
    let said = words(&evs);
    assert!(said.contains("Use the log."), "{said}");
    assert!(!said.contains("PLANTED"), "{said}");
    let started = evs.iter().find(|e| e.kind() == "turn.started").unwrap();
    let EventBody::TurnStarted(p) = &started.body else {
        panic!()
    };
    assert_eq!(p.prompt, "second");
    assert_eq!(
        p.ext.as_ref().unwrap()["twr"]["context_hash"],
        blob_ref(b"Use the log.\n")
    );
    // Only the set was logged: no `origin: file` capture of the planted file.
    let updates = context_updates(&hub);
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].ext.as_ref().unwrap()["twr"]["origin"], "set");

    // A configured project whose box is remote: the same, and its repo file is untouched.
    let r = hub.context_read("api").await.unwrap();
    assert_eq!(
        (r["remote"].as_bool(), r["content"].as_str()),
        (Some(true), Some(""))
    );
    let r = hub
        .context_set("api", "API rules.\n", None, &user)
        .await
        .unwrap();
    assert_eq!(r["changed"], true);
    assert_eq!(std::fs::read_to_string(&planted_api).unwrap(), PLANTED);
    assert_eq!(
        hub.context_read("api").await.unwrap()["content"],
        "API rules.\n"
    );

    // A configured project with no box: its repo here while it has no thread (nothing says
    // where it lives yet)...
    let r = hub.context_read("web").await.unwrap();
    assert_eq!(
        (r["remote"].as_bool(), r["content"].as_str()),
        (Some(false), Some(PLANTED))
    );
    // ...but once all its threads run on the remote box, the log alone, like `api`.
    let w = hub
        .new_thread(
            NewThread {
                project: "web".into(),
                runtime: Some("fake".into()),
                name: Some("web-remote".into()),
                box_name: Some(BOX.into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let r = hub.context_read("web").await.unwrap();
    assert_eq!(
        (r["remote"].as_bool(), r["content"].as_str()),
        (Some(true), Some(""))
    );
    let r = hub
        .context_set("web", "Web rules.\n", None, &user)
        .await
        .unwrap();
    assert_eq!(
        (r["changed"].as_bool(), r["remote"].as_bool()),
        (Some(true), Some(true))
    );
    assert_eq!(std::fs::read_to_string(&planted_web).unwrap(), PLANTED);
    assert_eq!(
        hub.context_read("web").await.unwrap()["content"],
        "Web rules.\n"
    );
    let mut wsub = hub.subscribe_thread(&w, 0).unwrap();
    hub.send(&w, "web turn", TurnFrom::User, Source::User("test".into()))
        .unwrap();
    let said = words(&until(&mut wsub, |e| e.kind() == "turn.ended").await);
    assert!(said.contains("Web rules."), "{said}");
    assert!(!said.contains("PLANTED"), "{said}");
    assert!(context_updates(&hub)
        .iter()
        .all(|u| u.ext.as_ref().unwrap()["twr"]["origin"] == "set"));
    assert_eq!(std::fs::read_to_string(&planted_web).unwrap(), PLANTED);

    // A configured project with no box whose threads all run remotely and whose repository
    // is not on the hub's machine: read and set never resolve a local repository (they did,
    // and failed with `bad_request`); the context is log-only.
    hub.new_thread(
        NewThread {
            project: "edge".into(),
            runtime: Some("fake".into()),
            name: Some("edge-remote".into()),
            box_name: Some(BOX.into()),
            ..Default::default()
        },
        "test",
    )
    .await
    .unwrap();
    std::fs::remove_dir_all(&runner_only_repo).unwrap();
    let r = hub.context_read("edge").await.unwrap();
    assert_eq!(
        (r["remote"].as_bool(), r["content"].as_str()),
        (Some(true), Some(""))
    );
    let r = hub
        .context_set("edge", "Edge rules.\n", None, &user)
        .await
        .unwrap();
    assert_eq!(
        (r["changed"].as_bool(), r["remote"].as_bool()),
        (Some(true), Some(true))
    );
    assert_eq!(
        hub.context_read("edge").await.unwrap()["content"],
        "Edge rules.\n"
    );
    assert!(
        !runner_only_repo.exists(),
        "a log-only set wrote on the hub"
    );

    let _ = stop_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(10), runner).await;
    hub.shutdown().await;
}
