//! `thread.new` with a project given by its repository folder name (`hub::find_project`): a
//! git repository at `<root>/<name>` or `<root>/*/<name>` under `project_roots` is used as if
//! its absolute path had been given, and the result says which one (`project_found`). Several
//! are `ambiguous`, none is `not_found` naming the roots; folders that are not git
//! repositories are ignored; an empty `project_roots` turns the lookup off; a configured
//! project wins; a thread on a remote box never searches the hub's disk, and a project the
//! box does not have says to run `twr box project`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::oneshot;
use twr_adapters::FakeAdapter;
use twr_core::adapter::Adapter;
use twr_core::config::{BoxConfig, Config, ProjectConfig};
use twr_core::proto::{codes, RpcError};
use twr_core::runner::Secret;
use twr_core::testkit::git::init_repo;
use twr_hub::listener::RunnerListener;
use twr_hub::{Hub, HubOptions, NewThread};
use twr_runner::{LocalBox, LocalBoxOptions, RunnerOptions};

const TOKEN: &str = "lookup-token-31-0123456789abcdef";
const BOX: &str = "gpu";
const WAIT: Duration = Duration::from_secs(15);

/// A temporary directory (canonical) with two project roots, `a` and `b`.
struct Roots {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Roots {
    fn new() -> Roots {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        Roots { _dir: dir, root }
    }

    fn a(&self) -> PathBuf {
        self.root.join("a")
    }

    fn b(&self) -> PathBuf {
        self.root.join("b")
    }

    /// A config whose `project_roots` are `a` and `b`.
    fn config(&self) -> Config {
        Config {
            default_runtime: "fake".into(),
            project_roots: Some(vec![self.a(), self.b()]),
            ..Default::default()
        }
    }
}

fn opts(dir: &Path, config: Config) -> HubOptions {
    let mut o = HubOptions::new(dir.join("data").join("twr.db"), config);
    o.adapters = Arc::new(|name: &str| -> Option<Arc<dyn Adapter>> {
        (name == "fake").then(|| Arc::new(FakeAdapter::new()) as Arc<dyn Adapter>)
    });
    o.git_tick = Some(Duration::ZERO);
    o.stall_after = Some(Duration::ZERO);
    o
}

async fn create(hub: &Hub, project: &str, name: &str) -> Result<Value, RpcError> {
    hub.new_thread(
        NewThread {
            project: project.into(),
            runtime: Some("fake".into()),
            name: Some(name.into()),
            ..Default::default()
        },
        "test",
    )
    .await
}

#[tokio::test]
async fn one_repository_with_the_name_is_used_and_reported() {
    let r = Roots::new();
    let repo = init_repo(&r.a().join("orbit-webapp"));
    // One folder deeper (`<root>/<org>/<repo>`), and a case-insensitive match.
    let deep = init_repo(&r.b().join("org").join("Billing"));
    let hub = Hub::open(opts(&r.root, r.config())).await.unwrap();

    let t = create(&hub, "orbit-webapp", "orbit").await.unwrap();
    assert_eq!(t["project"], "orbit-webapp");
    assert_eq!(t["repo"], repo.to_str().unwrap());
    assert_eq!(t["project_found"]["name"], "orbit-webapp");
    assert_eq!(t["project_found"]["repo"], repo.to_str().unwrap());
    // Everything an absolute path gets: its own worktree under the data directory.
    let wt = PathBuf::from(t["worktree"].as_str().unwrap());
    assert!(wt.starts_with(r.root.join("data").join("worktrees")), "{t}");
    assert!(wt.is_dir());
    // thread.get shows the repository too.
    let got = hub.get_thread("orbit").unwrap();
    assert_eq!(got["repo"], repo.to_str().unwrap());

    let t = create(&hub, "billing", "bill").await.unwrap();
    assert_eq!(t["project"], "Billing");
    assert_eq!(t["project_found"]["name"], "billing");
    assert_eq!(t["project_found"]["repo"], deep.to_str().unwrap());

    // A path or a configured project is never reported as found.
    let t = create(&hub, repo.to_str().unwrap(), "by-path")
        .await
        .unwrap();
    assert!(t.get("project_found").is_none(), "{t}");
    hub.shutdown().await;
}

#[tokio::test]
async fn several_repositories_with_the_name_are_ambiguous() {
    let r = Roots::new();
    let one = init_repo(&r.a().join("dup"));
    let two = init_repo(&r.b().join("team").join("dup"));
    let hub = Hub::open(opts(&r.root, r.config())).await.unwrap();
    let e = create(&hub, "dup", "dup").await.unwrap_err();
    assert_eq!(e.code, codes::AMBIGUOUS, "{e}");
    assert!(e.message.contains(one.to_str().unwrap()), "{e}");
    assert!(e.message.contains(two.to_str().unwrap()), "{e}");
    assert!(e.message.contains("absolute path"), "{e}");
    assert!(hub.list_threads().is_empty());
    hub.shutdown().await;
}

#[tokio::test]
async fn no_repository_is_not_found_naming_the_roots() {
    let r = Roots::new();
    let hub = Hub::open(opts(&r.root, r.config())).await.unwrap();
    let e = create(&hub, "ghost", "ghost").await.unwrap_err();
    assert_eq!(e.code, codes::NOT_FOUND, "{e}");
    for want in [
        r.a().to_str().unwrap(),
        r.b().to_str().unwrap(),
        "absolute path",
        "[projects.ghost]",
    ] {
        assert!(e.message.contains(want), "{want:?} missing: {e}");
    }
    // A name with a path separator or a leading dot is not looked up.
    for odd in ["a/ghost", "../ghost", ".ghost"] {
        let e = create(&hub, odd, "odd").await.unwrap_err();
        assert_eq!(e.code, codes::NOT_FOUND, "{e}");
        assert!(e.message.contains("neither a configured project"), "{e}");
    }
    assert!(hub.list_threads().is_empty());
    hub.shutdown().await;
}

#[tokio::test]
async fn a_folder_that_is_not_a_git_repository_is_ignored() {
    let r = Roots::new();
    std::fs::create_dir_all(r.a().join("notes")).unwrap();
    let hub = Hub::open(opts(&r.root, r.config())).await.unwrap();
    let e = create(&hub, "notes", "notes").await.unwrap_err();
    assert_eq!(e.code, codes::NOT_FOUND, "{e}");
    assert!(e.message.contains("not git repositories"), "{e}");

    // With a repository of that name elsewhere, the repository is the one.
    let repo = init_repo(&r.b().join("notes"));
    let t = create(&hub, "notes", "notes").await.unwrap();
    assert_eq!(t["project_found"]["repo"], repo.to_str().unwrap());
    hub.shutdown().await;
}

#[tokio::test]
async fn an_empty_project_roots_turns_the_lookup_off_and_configured_projects_win() {
    let r = Roots::new();
    init_repo(&r.a().join("api"));
    let configured = init_repo(&r.root.join("elsewhere").join("api"));
    let mut config = r.config();
    config.project_roots = Some(vec![]);
    let hub = Hub::open(opts(&r.root.join("off"), config)).await.unwrap();
    let e = create(&hub, "api", "api").await.unwrap_err();
    assert_eq!(e.code, codes::NOT_FOUND, "{e}");
    assert!(e.message.contains("project_roots is empty"), "{e}");
    hub.shutdown().await;

    let mut config = r.config();
    config.projects.insert(
        "api".into(),
        ProjectConfig {
            repo: configured.clone(),
            ..Default::default()
        },
    );
    let hub = Hub::open(opts(&r.root.join("on"), config)).await.unwrap();
    let t = create(&hub, "api", "api").await.unwrap();
    assert_eq!(t["repo"], configured.to_str().unwrap());
    assert!(t.get("project_found").is_none(), "{t}");
    hub.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_thread_on_a_remote_box_does_not_search_the_hub() {
    let r = Roots::new();
    // A repository the hub could find, were the thread local.
    let local = init_repo(&r.a().join("orbit"));
    let mut config = r.config();
    config.hub.listen = Some("127.0.0.1:0".into());
    config.boxes.insert(
        BOX.into(),
        BoxConfig {
            token: Some(TOKEN.into()),
            ..Default::default()
        },
    );
    // A project of the hub whose repository the box does not have.
    let ghost = r.root.join("missing").join("ghost");
    config.projects.insert(
        "ghost".into(),
        ProjectConfig {
            repo: ghost.clone(),
            ..Default::default()
        },
    );
    let mut o = opts(&r.root.join("hub"), config);
    o.stop_timeout = Duration::from_secs(5);
    let listener = RunnerListener::bind(&o.config.hub).await.unwrap().unwrap();
    let url = listener.url();
    let hub = Hub::open(o).await.unwrap();
    let _server = listener.serve(hub.clone());
    let rdir = r.root.join("r");
    let mut ro = RunnerOptions::new(&url, Secret::new(TOKEN), BOX, &rdir);
    ro.backoff_min = Duration::from_millis(20);
    ro.backoff_max = Duration::from_millis(200);
    ro.refused_retry = Duration::from_millis(200);
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

    let remote = hub
        .new_thread(
            NewThread {
                project: "orbit".into(),
                runtime: Some("fake".into()),
                name: Some("remote".into()),
                box_name: Some(BOX.into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap_err();
    assert_eq!(remote.code, codes::NOT_FOUND, "{remote}");
    assert!(
        remote.message.contains("only on the hub's own machine"),
        "{remote}"
    );
    assert!(
        !remote.message.contains(local.to_str().unwrap()),
        "{remote}"
    );
    // What to do: find or clone it on the box.
    assert!(
        remote
            .message
            .contains("run `twr box project gpu orbit` to find or clone it there"),
        "{remote}"
    );
    // A configured project whose repository is not on the box says so too.
    let missing = hub
        .new_thread(
            NewThread {
                project: "ghost".into(),
                runtime: Some("fake".into()),
                name: Some("ghost".into()),
                box_name: Some(BOX.into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap_err();
    assert!(
        missing
            .message
            .contains("project \"ghost\" is not on box \"gpu\""),
        "{missing}"
    );
    assert!(
        missing.message.contains(ghost.to_str().unwrap())
            && missing
                .message
                .contains("run `twr box project gpu ghost` to find or clone it there"),
        "{missing}"
    );
    // An absolute path that is not there: no hint (nothing to record).
    let path = hub
        .new_thread(
            NewThread {
                project: ghost.to_string_lossy().into_owned(),
                runtime: Some("fake".into()),
                name: Some("path".into()),
                box_name: Some(BOX.into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap_err();
    assert!(!path.message.contains("twr box project"), "{path}");
    assert!(hub.list_threads().is_empty());

    // The same name on the hub's own box is found.
    let t = create(&hub, "orbit", "here").await.unwrap();
    assert_eq!(t["project_found"]["repo"], local.to_str().unwrap());
    assert_eq!(t["box_name"], "local");

    let _ = stop_tx.send(());
    let _ = tokio::time::timeout(WAIT, runner).await;
    hub.shutdown().await;
}

#[tokio::test]
async fn durable_spawns_keep_project_resolution_after_completion_and_recovery() {
    let r = Roots::new();
    let repo = init_repo(&r.a().join("orbit-webapp"));
    let hub = Hub::open(opts(&r.root, r.config())).await.unwrap();
    let caller = twr_hub::Caller::user("test");
    hub.begin_spawn(
        "lookup-retry",
        NewThread {
            project: "orbit-webapp".into(),
            name: Some("durable-lookup".into()),
            runtime: Some("fake".into()),
            ..Default::default()
        },
        None,
        &caller,
    )
    .unwrap();
    let status = tokio::time::timeout(WAIT, async {
        loop {
            let status = hub.spawn_status("lookup-retry", &caller).unwrap();
            if status["status"] != "pending" {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(status["status"], "complete", "{status}");
    assert_eq!(
        status["thread"]["project_found"]["repo"],
        repo.to_str().unwrap()
    );
    let id = status["thread"]["id"].clone();
    hub.shutdown().await;
    drop(hub);
    let db = rusqlite::Connection::open(r.root.join("data/twr.db")).unwrap();
    db.execute(
        "UPDATE spawn_operations SET result=NULL WHERE operation_id='lookup-retry'",
        [],
    )
    .unwrap();
    drop(db);
    let hub = tokio::time::timeout(WAIT, async {
        loop {
            match Hub::open(opts(&r.root, r.config())).await {
                Ok(hub) => break hub,
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await
    .unwrap();
    let status = hub.spawn_status("lookup-retry", &caller).unwrap();
    assert_eq!(status["thread"]["id"], id);
    assert_eq!(status["thread"]["project_found"]["name"], "orbit-webapp");
    assert_eq!(
        status["thread"]["project_found"]["repo"],
        repo.to_str().unwrap()
    );
    hub.shutdown().await;
}
