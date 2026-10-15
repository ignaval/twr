//! Hub tests with real temporary git repositories: worktrees at `thread.new`,
//! `git.snapshot`, park (clean → removed, dirty → kept, `keep`), resume (recreated at the same
//! path, session resumed, queue dispatched), non-git projects, name and base validation, and
//! hub restarts while parked.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::time::timeout;
use twr_adapters::fake::FakeAdapter;
use twr_core::adapter::Adapter;
use twr_core::config::Config;
use twr_core::event::*;
use twr_core::proto::codes;
use twr_core::reduce::ThreadStatus;
use twr_core::testkit::git::{branch_exists, commit_all, init_repo, run, worktrees};
use twr_hub::hub::ThreadSubscription;
use twr_hub::{Caller, Hub, HubOptions, NewThread};

const WAIT: Duration = Duration::from_secs(10);

fn opts(dir: &Path, tick: Option<Duration>) -> HubOptions {
    let mut o = HubOptions::new(dir.join("data").join("twr.db"), Config::default());
    o.adapters = Arc::new(|name: &str| -> Option<Arc<dyn Adapter>> {
        (name == "fake").then(|| Arc::new(FakeAdapter::new()) as Arc<dyn Adapter>)
    });
    o.git_tick = Some(tick.unwrap_or(Duration::ZERO));
    o.stall_after = Some(Duration::ZERO);
    o
}

fn user() -> Caller {
    Caller::user("test")
}

async fn create(hub: &Hub, project: &Path, name: &str) -> Result<Value, twr_core::proto::RpcError> {
    create_with(hub, project, name, false, None).await
}

async fn create_with(
    hub: &Hub,
    project: &Path,
    name: &str,
    no_worktree: bool,
    base: Option<&str>,
) -> Result<Value, twr_core::proto::RpcError> {
    hub.new_thread(
        NewThread {
            project: project.to_string_lossy().into_owned(),
            runtime: Some("fake".into()),
            name: Some(name.into()),
            no_worktree,
            base: base.map(str::to_string),
            ..Default::default()
        },
        "test",
    )
    .await
}

fn send(hub: &Hub, thread: &str, prompt: &str) -> twr_hub::hub::SendOutcome {
    hub.send(thread, prompt, TurnFrom::User, Source::User("test".into()))
        .unwrap()
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
    .unwrap_or_else(|_| panic!("timed out; saw {:?}", kinds(&seen)));
    seen
}

fn kinds(evs: &[Event]) -> Vec<String> {
    evs.iter().map(|e| e.kind().to_string()).collect()
}

fn snapshot(e: &Event) -> Option<&GitSnapshot> {
    match &e.body {
        EventBody::GitSnapshot(s) => Some(s),
        _ => None,
    }
}

fn twr_ext(ext: &Option<JsonMap>) -> Value {
    ext.as_ref()
        .and_then(|e| e.get("twr"))
        .cloned()
        .unwrap_or(Value::Null)
}

/// Every event of `thread` so far.
async fn log(hub: &Hub, thread: &str) -> Vec<Event> {
    hub.thread_events(thread, 0, 10_000).unwrap().1
}

fn count(evs: &[Event], kind: &str) -> usize {
    evs.iter().filter(|e| e.kind() == kind).count()
}

struct Repo {
    _dir: tempfile::TempDir,
    root: PathBuf,
    repo: PathBuf,
}

impl Repo {
    fn new() -> Repo {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let repo = init_repo(&root.join("proj"));
        Repo {
            _dir: dir,
            root,
            repo,
        }
    }

    fn worktree(&self, name: &str) -> PathBuf {
        self.root
            .join("data")
            .join("worktrees")
            .join("proj")
            .join(name)
    }
}

#[tokio::test]
async fn thread_new_creates_a_worktree_on_its_own_branch() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root, None)).await.unwrap();
    // A subdirectory of the repository: repo is the top level.
    std::fs::create_dir(r.repo.join("sub")).unwrap();
    let t = create(&hub, &r.repo.join("sub"), "api").await.unwrap();
    // The project is named after the directory given.
    let wt = r
        .root
        .join("data")
        .join("worktrees")
        .join("sub")
        .join("api");
    assert_eq!(t["worktree"], json!(wt));
    assert_eq!(t["repo"], json!(r.repo));
    assert!(wt.join("README.md").exists());
    assert!(branch_exists(&r.repo, "twr/api"));
    assert_eq!(run(&wt, &["branch", "--show-current"]), "twr/api");
    assert!(worktrees(&r.repo).contains(&wt));
    let id = t["id"].as_str().unwrap();
    let evs = log(&hub, id).await;
    let EventBody::ThreadCreated(p) = &evs[0].body else {
        panic!()
    };
    let head = run(&r.repo, &["rev-parse", "HEAD"]);
    let on = run(&r.repo, &["symbolic-ref", "HEAD"]);
    assert_eq!(
        twr_ext(&p.ext),
        json!({"worktree": "twr", "branch": "twr/api", "base": "HEAD", "base_commit": head,
               "base_ref": on})
    );

    // The runtime works in the worktree.
    let mut sub = hub.subscribe_thread(id, 0).unwrap();
    send(&hub, id, "!write made-here.txt hi");
    until(&mut sub, |e| e.kind() == "turn.ended").await;
    assert!(wt.join("made-here.txt").exists());
    assert!(!r.repo.join("made-here.txt").exists());
    hub.shutdown().await;
}

#[tokio::test]
async fn names_bases_and_non_git_projects() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root, None)).await.unwrap();
    let first = run(&r.repo, &["rev-parse", "HEAD"]);
    std::fs::write(r.repo.join("later.txt"), "x").unwrap();
    commit_all(&r.repo, "later");

    // Names must be branch components; nothing is created for a refused thread.
    for bad in [".hidden", "x.lock", "trail."] {
        let e = create(&hub, &r.repo, bad).await.unwrap_err();
        assert_eq!(e.code, codes::BAD_REQUEST, "{bad}: {e:?}");
    }
    // An unknown base: refused, no thread, no branch, no worktree.
    let e = create_with(&hub, &r.repo, "b1", false, Some("nope"))
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST, "{e:?}");
    assert!(hub.list_threads().is_empty());
    assert!(!branch_exists(&r.repo, "twr/b1"));
    assert!(!r.worktree("b1").exists());
    // The name is free again.
    let t = create_with(&hub, &r.repo, "b1", false, Some(&first))
        .await
        .unwrap();
    let wt = r.worktree("b1");
    assert_eq!(run(&wt, &["rev-parse", "HEAD"]), first);
    assert!(!wt.join("later.txt").exists());
    let evs = log(&hub, t["id"].as_str().unwrap()).await;
    let EventBody::ThreadCreated(p) = &evs[0].body else {
        panic!()
    };
    assert_eq!(twr_ext(&p.ext)["base"], json!(first));

    // no_worktree: the repository itself.
    let t = create_with(&hub, &r.repo, "inplace", true, None)
        .await
        .unwrap();
    assert_eq!(t["worktree"], json!(r.repo));
    assert!(!branch_exists(&r.repo, "twr/inplace"));
    let e = create_with(&hub, &r.repo, "inplace2", true, Some("main"))
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST);

    // Not a git repository: the directory, with a note.
    let plain = r.root.join("plain");
    std::fs::create_dir(&plain).unwrap();
    let t = create(&hub, &plain, "plain-1").await.unwrap();
    assert_eq!(t["worktree"], json!(plain));
    assert_eq!(t["repo"], json!(plain));
    let evs = log(&hub, t["id"].as_str().unwrap()).await;
    let EventBody::ThreadCreated(p) = &evs[0].body else {
        panic!()
    };
    let ext = twr_ext(&p.ext);
    assert_eq!(
        (ext["worktree"].clone(), ext["git"].clone()),
        (json!("repo"), json!(false))
    );
    assert!(ext["note"]
        .as_str()
        .unwrap()
        .contains("not a git repository"));
    let e = create_with(&hub, &plain, "plain-2", false, Some("main"))
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST);
    // Parking and resuming a non-git thread touch no worktree.
    let id = t["id"].as_str().unwrap();
    let out = hub.park_thread(id, false, false, &user()).await.unwrap();
    assert!(!out.worktree_removed && !out.kept_dirty && out.worktree.is_none());
    assert!(plain.exists());
    let out = hub.resume_thread(id, &user()).await.unwrap();
    assert!(!out.worktree_recreated);
    hub.shutdown().await;
}

/// `[threads] worktree = false` (and `[projects.<name>] worktree`): threads work in the
/// repository checkout unless `thread.new` asks for a worktree (`worktree`, or a `base`);
/// park never removes the checkout, snapshots and resume work as for `no_worktree`.
#[tokio::test]
async fn worktrees_can_be_off_by_default_and_per_project() {
    let r = Repo::new();
    let other = init_repo(&r.root.join("other"));
    let mut o = opts(&r.root, Some(Duration::from_millis(50)));
    o.config.threads.worktree = false;
    o.config.projects.insert(
        "other".into(),
        twr_core::config::ProjectConfig {
            repo: other.clone(),
            worktree: Some(true),
            ..Default::default()
        },
    );
    o.config.projects.insert(
        "plainproj".into(),
        twr_core::config::ProjectConfig {
            repo: r.repo.clone(),
            ..Default::default()
        },
    );
    let hub = Hub::open(o).await.unwrap();
    let new =
        |project: String, name: &str, worktree: bool, no_worktree: bool, base: Option<&str>| {
            hub.new_thread(
                NewThread {
                    project,
                    runtime: Some("fake".into()),
                    name: Some(name.into()),
                    worktree,
                    no_worktree,
                    base: base.map(str::to_string),
                    ..Default::default()
                },
                "test",
            )
        };
    let path = r.repo.to_string_lossy().into_owned();

    // Off by default: the checkout itself, no branch, `worktree: "repo"`.
    let a = new(path.clone(), "a", false, false, None).await.unwrap();
    let b = new("plainproj".into(), "b", false, false, None)
        .await
        .unwrap();
    for (t, name) in [(&a, "a"), (&b, "b")] {
        assert_eq!(t["worktree"], json!(r.repo), "{name}");
        assert!(!branch_exists(&r.repo, &format!("twr/{name}")));
        let evs = log(&hub, t["id"].as_str().unwrap()).await;
        let EventBody::ThreadCreated(p) = &evs[0].body else {
            panic!()
        };
        assert_eq!(twr_ext(&p.ext)["worktree"], "repo", "{name}");
    }
    assert_eq!(worktrees(&r.repo), vec![r.repo.clone()]);
    // Parallel threads of one repository share the checkout.
    let (a_id, b_id) = (a["id"].as_str().unwrap(), b["id"].as_str().unwrap());
    let mut sub = hub.subscribe_thread(a_id, 0).unwrap();
    send(&hub, a_id, "!write from-a.txt hi");
    until(&mut sub, |e| e.kind() == "turn.ended").await;
    assert!(r.repo.join("from-a.txt").exists());
    // Snapshots see the checkout (the untracked file).
    let evs = until(&mut sub, |e| snapshot(e).is_some_and(|s| s.dirty_files > 0)).await;
    assert_eq!(
        snapshot(evs.last().unwrap()).unwrap().branch.as_deref(),
        Some("main")
    );

    // `worktree` forces one; so does a base (it only applies to a new worktree).
    let c = new(path.clone(), "c", true, false, None).await.unwrap();
    assert_eq!(c["worktree"], json!(r.worktree("c")));
    assert!(branch_exists(&r.repo, "twr/c"));
    let d = new(path.clone(), "d", false, false, Some("main"))
        .await
        .unwrap();
    assert_eq!(d["worktree"], json!(r.worktree("d")));
    // Both flags: refused, nothing made.
    let e = new(path.clone(), "e", true, true, None).await.unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST, "{e:?}");
    assert!(!branch_exists(&r.repo, "twr/e"));
    // A project that turns them back on.
    let f = new("other".into(), "f", false, false, None).await.unwrap();
    let f_wt = r
        .root
        .join("data")
        .join("worktrees")
        .join("other")
        .join("f");
    assert_eq!(f["worktree"], json!(f_wt));
    let g = new("other".into(), "g", false, true, None).await.unwrap();
    assert_eq!(g["worktree"], json!(other));

    // Park never removes the checkout; resume works on it again.
    let out = hub.park_thread(b_id, false, false, &user()).await.unwrap();
    assert!(!out.worktree_removed && out.worktree.is_none(), "{out:?}");
    assert!(r.repo.join("README.md").exists());
    let out = hub.resume_thread(b_id, &user()).await.unwrap();
    assert!(!out.worktree_recreated);
    hub.shutdown().await;
}

#[tokio::test]
async fn snapshots_tell_merge_readiness_against_the_threads_base() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root, Some(Duration::from_millis(100))))
        .await
        .unwrap();
    let on = run(&r.repo, &["symbolic-ref", "HEAD"]).trim().to_string();
    let t = create(&hub, &r.repo, "ready").await.unwrap();
    let id = t["id"].as_str().unwrap();
    // The branch the repository was on: its base.
    assert_eq!(
        hub.thread_view(id).unwrap().base.as_deref(),
        Some(on.as_str())
    );
    let mut sub = hub.subscribe_thread(id, 0).unwrap();
    until(&mut sub, |e| snapshot(e).is_some_and(|s| s.base.is_some())).await;
    // Conflicts need a git with `--attr-source`.
    if !twr_core::testkit::git::merge_tree_writes() {
        hub.shutdown().await;
        return;
    }
    // The base moves on, touching what the thread does too: behind, and a conflict.
    let wt = r.worktree("ready");
    std::fs::write(wt.join("x.txt"), "thread\n").unwrap();
    commit_all(&wt, "x on the thread");
    std::fs::write(r.repo.join("x.txt"), "base\n").unwrap();
    commit_all(&r.repo, "x on the base");
    let evs = until(&mut sub, |e| {
        snapshot(e).is_some_and(|s| s.conflicts.as_ref().is_some_and(|c| !c.is_empty()))
    })
    .await;
    let s = snapshot(evs.last().unwrap()).unwrap();
    assert_eq!(s.base.as_deref(), Some(on.as_str()));
    assert_eq!((s.base_ahead, s.base_behind), (Some(1), Some(1)));
    assert_eq!(s.conflicts.as_deref(), Some(&["x.txt".to_string()][..]));
    // `get_thread` says the same.
    let g = hub.get_thread(id).unwrap();
    assert_eq!(g["git"]["conflicts"], serde_json::json!(["x.txt"]));
    hub.shutdown().await;
}

/// A readiness probe that fails (here: the base is gone) leaves readiness unknown: the last
/// one, read at other commits, is not carried over.
#[tokio::test]
async fn a_failed_readiness_probe_never_keeps_the_last_readiness() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root, Some(Duration::from_millis(100))))
        .await
        .unwrap();
    let on = run(&r.repo, &["symbolic-ref", "HEAD"]).trim().to_string();
    let t = create(&hub, &r.repo, "stale").await.unwrap();
    let id = t["id"].as_str().unwrap();
    let mut sub = hub.subscribe_thread(id, 0).unwrap();
    let evs = until(&mut sub, |e| snapshot(e).is_some_and(|s| s.base.is_some())).await;
    let s = snapshot(evs.last().unwrap()).unwrap();
    assert_eq!((s.base_ahead, s.base_behind), (Some(0), Some(0)));
    // The thread moves on, and the base can no longer be read.
    let wt = r.worktree("stale");
    std::fs::write(wt.join("y.txt"), "thread\n").unwrap();
    commit_all(&wt, "y on the thread");
    let branch = on.strip_prefix("refs/heads/").unwrap();
    run(&r.repo, &["branch", "-m", branch, "elsewhere"]);
    std::fs::write(wt.join("untracked.txt"), "z\n").unwrap();
    let evs = until(&mut sub, |e| {
        snapshot(e).is_some_and(|s| s.dirty_files == 1)
    })
    .await;
    let s = snapshot(evs.last().unwrap()).unwrap();
    assert_eq!(
        (&s.base, s.base_ahead, s.base_behind, &s.conflicts),
        (&None, None, None, &None),
        "{s:?}"
    );
    let g = hub.get_thread(id).unwrap();
    assert!(g["git"].get("base_ahead").is_none(), "{g}");
    hub.shutdown().await;
}

#[tokio::test]
async fn snapshots_follow_turns_and_the_tick_only_on_change() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root, Some(Duration::from_millis(100))))
        .await
        .unwrap();
    let t = create(&hub, &r.repo, "snap").await.unwrap();
    let id = t["id"].as_str().unwrap();
    let wt = r.worktree("snap");
    let mut sub = hub.subscribe_thread(id, 0).unwrap();
    // The tick reports the clean worktree once.
    let evs = until(&mut sub, |e| e.kind() == "git.snapshot").await;
    let s = snapshot(evs.last().unwrap()).unwrap();
    assert_eq!(
        (s.branch.as_deref(), s.dirty_files, s.ahead, s.behind),
        (Some("twr/snap"), 0, 0, 0)
    );
    assert_eq!(evs.last().unwrap().source, Source::Hub);
    assert_eq!(evs.last().unwrap().turn, None);

    // A turn that writes a file: dirty after the turn.
    send(&hub, id, "!write a.txt one");
    let evs = until(&mut sub, |e| {
        snapshot(e).is_some_and(|s| s.dirty_files == 1)
    })
    .await;
    assert!(kinds(&evs).contains(&"turn.ended".to_string()));
    // Several ticks later, nothing changed: no new snapshot.
    tokio::time::sleep(Duration::from_millis(450)).await;
    assert_eq!(count(&log(&hub, id).await, "git.snapshot"), 2);
    // A change outside a turn is seen by the tick.
    commit_all(&wt, "commit a");
    let evs = until(&mut sub, |e| {
        snapshot(e).is_some_and(|s| s.dirty_files == 0)
    })
    .await;
    assert_eq!(snapshot(evs.last().unwrap()).unwrap().dirty_files, 0);
    assert_eq!(hub.thread_view(id).unwrap().git.unwrap().dirty_files, 0);
    // Snapshots are control events (a commit seen half-way, its HEAD moved before its index,
    // may add one: merge readiness counts commits).
    let (items, _) = hub.control_read(0, 10_000).unwrap();
    let n = items
        .iter()
        .filter(|(_, e)| e.kind() == "git.snapshot")
        .count();
    assert!((3..=4).contains(&n), "{n}");
    hub.shutdown().await;
}

#[tokio::test]
async fn park_keeps_dirty_worktrees_and_removes_clean_ones() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root, None)).await.unwrap();
    let t = create(&hub, &r.repo, "wip").await.unwrap();
    let id = t["id"].as_str().unwrap().to_string();
    let wt = r.worktree("wip");
    let mut sub = hub.subscribe_thread(&id, 0).unwrap();
    send(&hub, &id, "!write notes.txt draft");
    let evs = until(&mut sub, |e| e.kind() == "report.ready").await;
    let session = evs
        .iter()
        .find_map(|e| match &e.body {
            EventBody::ThreadStarted(p) => p.session_id.clone(),
            _ => None,
        })
        .unwrap();
    assert_eq!(hub.live_runtimes(), 1);

    // Dirty: parked, worktree kept.
    let out = hub.park_thread(&id, false, false, &user()).await.unwrap();
    assert!(out.parked && out.kept_dirty && !out.worktree_removed);
    assert_eq!(out.dirty_files, Some(1));
    assert_eq!(out.worktree.as_deref(), wt.to_str());
    assert!(wt.join("notes.txt").exists());
    assert_eq!(hub.live_runtimes(), 0);
    let evs = until(&mut sub, |e| e.kind() == "thread.parked").await;
    let parked = evs.last().unwrap();
    assert_eq!(parked.source, Source::User("test".into()));
    let EventBody::ThreadParked(p) = &parked.body else {
        panic!()
    };
    assert_eq!(p.session_id.as_deref(), Some(session.as_str()));
    assert_eq!(twr_ext(&p.ext), json!({"keep": false}));
    let all = log(&hub, &id).await;
    assert_eq!(count(&all, "thread.stopped"), 0);
    assert_eq!(count(&all, "thread.crashed"), 0);
    assert_eq!(hub.thread_view(&id).unwrap().status(), ThreadStatus::Parked);
    let e = hub
        .park_thread(&id, false, false, &user())
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::CONFLICT);

    // Resume, commit the work, park again: clean, so removed and pruned; branch kept.
    let out = hub.resume_thread(&id, &user()).await.unwrap();
    assert!(out.resumed && !out.worktree_recreated);
    assert_eq!(out.turn, None);
    commit_all(&wt, "notes");
    let tip = run(&wt, &["rev-parse", "HEAD"]);
    let out = hub.park_thread(&id, false, false, &user()).await.unwrap();
    assert!(out.worktree_removed && !out.kept_dirty && !out.kept);
    assert!(!wt.exists());
    assert!(!worktrees(&r.repo).contains(&wt));
    assert!(branch_exists(&r.repo, "twr/wip"));
    assert_eq!(run(&r.repo, &["rev-parse", "twr/wip"]), tip);
    hub.shutdown().await;
}

#[tokio::test]
async fn park_keeps_a_worktree_whose_checkout_left_the_thread_branch() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root, None)).await.unwrap();
    let t = create(&hub, &r.repo, "det").await.unwrap();
    let id = t["id"].as_str().unwrap().to_string();
    let wt = r.worktree("det");
    // The agent detached HEAD and committed there: only the worktree's reflog has it.
    run(&wt, &["checkout", "-q", "--detach"]);
    std::fs::write(wt.join("d.txt"), "d").unwrap();
    commit_all(&wt, "detached work");
    let detached = run(&wt, &["rev-parse", "--short", "HEAD"]);
    let out = hub.park_thread(&id, false, false, &user()).await.unwrap();
    assert!(out.parked && !out.worktree_removed && !out.kept_dirty);
    assert_eq!(
        out.kept_reason.as_deref(),
        Some(
            format!("checkout differs from thread branch twr/det (detached at {detached})")
                .as_str()
        )
    );
    assert!(wt.join("d.txt").exists());
    // Resume finds it in place.
    let out = hub.resume_thread(&id, &user()).await.unwrap();
    assert!(!out.worktree_recreated);
    hub.shutdown().await;
}

#[tokio::test]
async fn park_keeps_a_worktree_with_ignored_files_it_does_not_know_are_disposable() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root, None)).await.unwrap();
    let t = create(&hub, &r.repo, "sec").await.unwrap();
    let id = t["id"].as_str().unwrap().to_string();
    let wt = r.worktree("sec");
    // `.env` is ignored (so `git status` shows nothing) and nothing regenerates it.
    let exclude = r.repo.join(".git").join("info").join("exclude");
    std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
    std::fs::write(&exclude, ".env\n").unwrap();
    std::fs::write(wt.join(".env"), "TOKEN=secret").unwrap();
    let out = hub.park_thread(&id, false, false, &user()).await.unwrap();
    assert!(out.parked && !out.worktree_removed && !out.kept_dirty);
    let why = out.kept_reason.unwrap_or_default();
    assert!(why.contains(".env"), "{why:?}");
    assert!(wt.join(".env").exists());
    hub.shutdown().await;
}

#[tokio::test]
async fn resume_recreates_the_worktree_and_runs_the_queue_on_the_same_session() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root, None)).await.unwrap();
    let t = create(&hub, &r.repo, "later").await.unwrap();
    let id = t["id"].as_str().unwrap().to_string();
    let wt = r.worktree("later");
    let mut sub = hub.subscribe_thread(&id, 0).unwrap();
    send(&hub, &id, "first");
    let evs = until(&mut sub, |e| e.kind() == "report.ready").await;
    let session = evs
        .iter()
        .find_map(|e| match &e.body {
            EventBody::ThreadStarted(p) => p.session_id.clone(),
            _ => None,
        })
        .unwrap();

    let out = hub.park_thread(&id, false, false, &user()).await.unwrap();
    assert!(out.worktree_removed);
    assert!(!wt.exists());
    // Sends queue while parked and say so.
    let s = send(&hub, &id, "!write back.txt again");
    assert!(s.parked);
    assert_eq!((s.turn, s.pending), (None, 1));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(hub.thread_view(&id).unwrap().turns, 1);
    assert_eq!(hub.live_runtimes(), 0);
    let j = hub.get_thread(&id).unwrap();
    assert_eq!(
        (j["status"].clone(), j["pending"].clone()),
        (json!("parked"), json!(1))
    );

    let out = hub.resume_thread(&id, &user()).await.unwrap();
    assert!(out.resumed && out.worktree_recreated);
    assert_eq!(out.worktree.as_deref(), wt.to_str());
    assert_eq!(out.turn, Some(2));
    assert!(wt.join("README.md").exists());
    let evs = until(&mut sub, |e| e.kind() == "report.ready").await;
    // thread.resumed (the caller's, no process), then the queued turn on the resumed session.
    let resumed = evs.iter().find(|e| e.kind() == "thread.resumed").unwrap();
    assert_eq!(resumed.source, Source::User("test".into()));
    let EventBody::ThreadResumed(p) = &resumed.body else {
        panic!()
    };
    assert_eq!(p.session_id.as_deref(), Some(session.as_str()));
    assert_eq!(twr_ext(&p.ext), json!({"worktree_recreated": true}));
    let started = evs
        .iter()
        .find_map(|e| match &e.body {
            EventBody::ThreadStarted(p) => Some(p.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(started.session_id.as_deref(), Some(session.as_str()));
    let fake = &started.ext.unwrap()["fake"];
    assert_eq!(
        (fake["resumed"].clone(), fake["transcript_len"].clone()),
        (json!(true), json!(1))
    );
    assert!(wt.join("back.txt").exists());
    let e = hub.resume_thread(&id, &user()).await.unwrap_err();
    assert_eq!(e.code, codes::CONFLICT);
    hub.shutdown().await;
}

/// A renamed thread keeps its worktree path and branch (they keep the name they were made
/// with): it goes on working there, park and resume make the worktree again at the same path
/// on the same branch, and its old name stays its own (it still reaches it, and no new thread
/// can take it, whose worktree and branch would collide). It may take the old name back.
#[tokio::test]
async fn a_renamed_thread_keeps_its_worktree_and_branch() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root, None)).await.unwrap();
    let t = create(&hub, &r.repo, "twr-1").await.unwrap();
    let id = t["id"].as_str().unwrap().to_string();
    let wt = r.worktree("twr-1");
    let mut sub = hub.subscribe_thread(&id, 0).unwrap();

    let out = hub
        .rename_thread("twr-1", "notifications-design", &user())
        .unwrap();
    assert_eq!(
        (
            &out["thread"],
            &out["thread_name"],
            &out["previous"],
            &out["changed"],
            &out["branch"],
            &out["worktree"]
        ),
        (
            &json!(id),
            &json!("notifications-design"),
            &json!("twr-1"),
            &json!(true),
            &json!("twr/twr-1"),
            &json!(wt)
        )
    );
    assert!(
        out["message"]
            .as_str()
            .unwrap()
            .contains("keep their names"),
        "{out}"
    );
    let evs = until(&mut sub, |e| e.kind() == "thread.renamed").await;
    let renamed = evs.last().unwrap();
    assert_eq!(renamed.source, Source::User("test".into()));
    let EventBody::ThreadRenamed(p) = &renamed.body else {
        panic!()
    };
    assert_eq!(
        (p.name.as_str(), p.previous.as_str(), p.by),
        ("notifications-design", "twr-1", ChangedBy::User)
    );
    let v = hub.thread_view("notifications-design").unwrap();
    assert_eq!(v.id, id);
    assert_eq!(v.former_names, ["twr-1"]);
    assert_eq!(v.worktree.as_deref(), wt.to_str());
    // The old name still reaches it.
    assert_eq!(
        hub.get_thread("twr-1").unwrap()["name"],
        "notifications-design"
    );
    // The same name again changes nothing.
    let same = hub
        .rename_thread(&id, "notifications-design", &user())
        .unwrap();
    assert_eq!(same["changed"], false);

    // It works where it did, on its branch.
    send(&hub, &id, "!write renamed.txt hi");
    until(&mut sub, |e| e.kind() == "turn.ended").await;
    assert!(wt.join("renamed.txt").exists());
    assert_eq!(run(&wt, &["branch", "--show-current"]), "twr/twr-1");
    std::fs::remove_file(wt.join("renamed.txt")).unwrap();

    // Its old name is no other thread's: neither a new thread's nor another rename's.
    let e = create(&hub, &r.repo, "twr-1").await.unwrap_err();
    assert_eq!(e.code, codes::CONFLICT, "{e:?}");
    let other = create(&hub, &r.repo, "other").await.unwrap();
    let other = other["id"].as_str().unwrap().to_string();
    for taken in ["twr-1", "notifications-design"] {
        let e = hub.rename_thread(&other, taken, &user()).unwrap_err();
        assert_eq!(e.code, codes::CONFLICT, "{taken}: {e:?}");
    }
    // Names follow the rules of new threads.
    for bad in ["has space", ".x", "x.lock", "a..b", "", "-x"] {
        let e = hub.rename_thread(&other, bad, &user()).unwrap_err();
        assert_eq!(e.code, codes::BAD_REQUEST, "{bad:?}: {e:?}");
    }

    // Park and resume: the worktree comes back at the same path, on the same branch.
    let out = hub.park_thread(&id, false, true, &user()).await.unwrap();
    assert!(out.worktree_removed);
    let out = hub.resume_thread(&id, &user()).await.unwrap();
    assert!(out.worktree_recreated);
    assert_eq!(out.worktree.as_deref(), wt.to_str());
    assert_eq!(run(&wt, &["branch", "--show-current"]), "twr/twr-1");

    // It may take its old name back; the name it leaves stays its own.
    let back = hub.rename_thread(&id, "twr-1", &user()).unwrap();
    assert_eq!(back["changed"], true);
    let v = hub.thread_view(&id).unwrap();
    assert_eq!(v.name, "twr-1");
    assert_eq!(v.former_names, ["notifications-design"]);
    let e = hub
        .rename_thread(&other, "notifications-design", &user())
        .unwrap_err();
    assert_eq!(e.code, codes::CONFLICT);

    // A restart replays it the same way.
    hub.shutdown().await;
    drop(sub);
    drop(hub);
    let hub = Hub::open(opts(&r.root, None)).await.unwrap();
    let v = hub.thread_view(&id).unwrap();
    assert_eq!(
        (v.name.as_str(), v.former_names.clone()),
        ("twr-1", vec!["notifications-design".to_string()])
    );
    hub.shutdown().await;
}

/// On the hub's side, a park whose request goes away while git removes the worktree does not
/// stop the removal: it runs on to its end (its git is not killed), holding the path's
/// reservation and the repository's until then. A resume meanwhile waits for it, then makes
/// the worktree again, and what the resumed runtime writes there stays.
#[tokio::test]
async fn a_resume_after_an_abandoned_park_waits_for_the_removal() {
    let r = Repo::new();
    let held = twr_core::testkit::git::HeldGit::install(&r.root.join("bin"));
    let mut o = opts(&r.root, None);
    o.git_bin = Some(held.bin.clone());
    let hub = Hub::open(o).await.unwrap();
    let t = create(&hub, &r.repo, "gone").await.unwrap();
    let id = t["id"].as_str().unwrap().to_string();
    let wt = r.worktree("gone");
    // `.env` is ignored: `git worktree remove` would delete it with the directory.
    let exclude = r.repo.join(".git").join("info").join("exclude");
    std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
    std::fs::write(&exclude, ".env\n").unwrap();

    // The park passes its checks; git is held; the request goes away.
    let park = {
        let (hub, id) = (hub.clone(), id.clone());
        tokio::spawn(async move { hub.park_thread(&id, false, false, &user()).await })
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while !held.held() {
        assert!(tokio::time::Instant::now() < deadline, "git never held");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    park.abort();
    let _ = park.await;
    assert!(send(&hub, &id, "!write .env TOKEN=secret").parked);

    // The removal runs on: the resume waits for it.
    let mut sub = hub.subscribe_thread(&id, 0).unwrap();
    let resume = {
        let (hub, id) = (hub.clone(), id.clone());
        tokio::spawn(async move { hub.resume_thread(&id, &user()).await })
    };
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !resume.is_finished(),
        "the resume ran while the removal did"
    );
    assert!(!held.finished());
    held.release();
    let out = timeout(Duration::from_secs(60), resume)
        .await
        .expect("the resume never finished")
        .unwrap()
        .unwrap();
    assert!(held.finished(), "the removal did not run to its end");
    assert!(out.resumed && out.worktree_recreated, "{out:?}");
    until(&mut sub, |e| e.kind() == "turn.ended").await;
    assert!(
        wt.join(".env").exists(),
        "what the resumed runtime wrote is gone"
    );
    hub.shutdown().await;
}

#[tokio::test]
async fn park_keep_force_and_attach_rules() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root, None)).await.unwrap();
    let t = create(&hub, &r.repo, "busy").await.unwrap();
    let id = t["id"].as_str().unwrap().to_string();
    let wt = r.worktree("busy");
    let mut sub = hub.subscribe_thread(&id, 0).unwrap();
    send(&hub, &id, "!sleep 60000");
    until(&mut sub, |e| e.kind() == "thread.started").await;

    // A running turn: conflict unless forced; forcing interrupts it.
    let e = hub
        .park_thread(&id, false, false, &user())
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::CONFLICT);
    assert!(e.message.contains("force"), "{}", e.message);
    let out = hub.park_thread(&id, true, true, &user()).await.unwrap();
    assert!(out.kept && !out.worktree_removed && !out.kept_dirty);
    assert!(wt.exists());
    let evs = until(&mut sub, |e| e.kind() == "thread.parked").await;
    let ended = evs
        .iter()
        .find_map(|e| match &e.body {
            EventBody::TurnEnded(p) => Some(p.result),
            _ => None,
        })
        .unwrap();
    assert_eq!(ended, TurnResult::Interrupted);
    let EventBody::ThreadParked(p) = &evs.last().unwrap().body else {
        panic!()
    };
    assert_eq!(twr_ext(&p.ext), json!({"keep": true}));
    // Parked threads cannot be attached; resume first.
    let e = hub.attach_begin(&id, false, "cli").await.unwrap_err();
    assert_eq!(e.code, codes::CONFLICT);
    let out = hub.resume_thread(&id, &user()).await.unwrap();
    assert!(!out.worktree_recreated);

    // Attached: park is refused.
    let grant = hub.attach_begin(&id, false, "cli").await.unwrap();
    let e = hub
        .park_thread(&id, false, false, &user())
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::CONFLICT);
    assert!(hub.attach_end(&id, "cli", grant.lease, None).unwrap());
    // Not parked: resume is refused.
    let e = hub.resume_thread(&id, &user()).await.unwrap_err();
    assert_eq!(e.code, codes::CONFLICT);
    // The orchestrator may park and resume workers.
    hub.park_thread(&id, false, false, &Caller::Orchestrator)
        .await
        .unwrap();
    let evs = until(&mut sub, |e| e.kind() == "thread.parked").await;
    assert_eq!(evs.last().unwrap().source, Source::Orchestrator);
    hub.resume_thread(&id, &Caller::Orchestrator).await.unwrap();
    hub.shutdown().await;
}

#[tokio::test]
async fn a_hub_restart_keeps_a_parked_thread_parked() {
    let r = Repo::new();
    let id = {
        let hub = Hub::open(opts(&r.root, None)).await.unwrap();
        let t = create(&hub, &r.repo, "sleepy").await.unwrap();
        let id = t["id"].as_str().unwrap().to_string();
        let mut sub = hub.subscribe_thread(&id, 0).unwrap();
        send(&hub, &id, "hello");
        until(&mut sub, |e| e.kind() == "report.ready").await;
        hub.park_thread(&id, false, false, &user()).await.unwrap();
        let s = send(&hub, &id, "queued while parked");
        assert!(s.parked);
        hub.shutdown().await;
        id
    };
    let before = {
        let hub = Hub::open(opts(&r.root, None)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        let v = hub.thread_view(&id).unwrap();
        assert_eq!(v.status(), ThreadStatus::Parked);
        assert_eq!(v.turns, 1);
        assert_eq!(hub.live_runtimes(), 0);
        assert_eq!(hub.get_thread(&id).unwrap()["pending"], json!(1));
        let evs = log(&hub, &id).await;
        assert_eq!(count(&evs, "thread.crashed"), 0);
        assert_eq!(count(&evs, "thread.stopped"), 0);
        hub.shutdown().await;
        evs.len()
    };
    // Nothing was appended by the restart itself.
    {
        let hub = Hub::open(opts(&r.root, None)).await.unwrap();
        assert_eq!(log(&hub, &id).await.len(), before);
        let out = hub.resume_thread(&id, &user()).await.unwrap();
        assert!(out.worktree_recreated);
        assert_eq!(out.turn, Some(2));
        let mut sub = hub.subscribe_thread(&id, before as u64).unwrap();
        until(&mut sub, |e| e.kind() == "report.ready").await;
        hub.shutdown().await;
    }

    // A resume with nothing queued starts no process: a restart finds nothing to reap.
    let hub = Hub::open(opts(&r.root, None)).await.unwrap();
    hub.park_thread(&id, false, false, &user()).await.unwrap();
    hub.resume_thread(&id, &user()).await.unwrap();
    assert_eq!(hub.live_runtimes(), 0);
    let n = log(&hub, &id).await.len();
    drop(hub);
    let hub = Hub::open(opts(&r.root, None)).await.unwrap();
    let evs = log(&hub, &id).await;
    assert_eq!(evs.len(), n, "{:?}", kinds(&evs[n.min(evs.len())..]));
    assert_eq!(hub.thread_view(&id).unwrap().status(), ThreadStatus::Done);
    hub.shutdown().await;
}

// ---------------------------------------------------------------------------------------------
// Attach leases hold their directory
// ---------------------------------------------------------------------------------------------

/// A thread `name` working in `dir` (of `repo`) without a worktree of its own, recorded the way
/// a hub could before `thread.new` refused another thread's worktree: `thread.created` as it
/// is in such a log.
fn thread_in(hub: &Hub, name: &str, repo: &Path, dir: &Path, box_name: &str) -> String {
    let id = format!("in-{name}");
    let mut twr = JsonMap::new();
    twr.insert("twr".into(), json!({"worktree": "repo"}));
    hub.append(
        &id,
        None,
        Source::User("test".into()),
        EventBody::ThreadCreated(ThreadCreated {
            name: name.into(),
            project: Some("proj".into()),
            repo: Some(repo.to_string_lossy().into_owned()),
            worktree: Some(dir.to_string_lossy().into_owned()),
            box_name: box_name.into(),
            runtime: "fake".into(),
            mode: "manual".into(),
            ext: Some(twr),
            ..Default::default()
        }),
        None,
    )
    .unwrap();
    id
}

/// Thread `b` works in thread `a`'s worktree; attached, its directory is held: parking `a`
/// keeps the worktree (the interactive runtime works in it) until the attach ends. And
/// `thread.new` no longer lets a thread start in another thread's worktree.
#[tokio::test]
async fn an_attach_keeps_its_directory_from_a_park_of_the_worktrees_thread() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root, None)).await.unwrap();
    let a = create(&hub, &r.repo, "owner").await.unwrap();
    let a = a["id"].as_str().unwrap().to_string();
    let wt = r.worktree("owner");
    // Refused now: that worktree is owner's (also below it, and with a worktree of its own).
    for (dir, no_worktree) in [
        (wt.clone(), true),
        (wt.join("sub"), true),
        (wt.clone(), false),
    ] {
        std::fs::create_dir_all(&dir).unwrap();
        let e = create_with(&hub, &dir, "guest", no_worktree, None)
            .await
            .unwrap_err();
        assert_eq!(e.code, codes::CONFLICT, "{dir:?}: {e:?}");
        assert!(e.message.contains("owner"), "{}", e.message);
    }
    std::fs::remove_dir(wt.join("sub")).unwrap();
    assert!(!r.worktree("guest").exists());

    let b = thread_in(&hub, "guest", &r.repo, &wt, "local");
    let mut sub = hub.subscribe_thread(&b, 0).unwrap();
    send(&hub, &b, "hello");
    until(&mut sub, |e| e.kind() == "report.ready").await;
    let grant = hub.attach_begin(&b, false, "cli").await.unwrap();
    assert_eq!(grant.cwd.as_deref(), Some(wt.to_str().unwrap()));
    let out = hub.park_thread(&a, false, false, &user()).await.unwrap();
    assert!(out.parked && !out.worktree_removed, "{out:?}");
    let reason = out.kept_reason.unwrap_or_default();
    assert!(reason.contains(&b), "{reason}");
    assert!(wt.join("README.md").exists());
    // Once the attach is over, the worktree can go.
    assert!(hub.attach_end(&b, "cli", grant.lease, None).unwrap());
    hub.resume_thread(&a, &user()).await.unwrap();
    let out = hub.park_thread(&a, false, false, &user()).await.unwrap();
    assert!(out.worktree_removed);
    hub.shutdown().await;
}

/// An attach whose directory's worktree is being removed (a park whose git is still at work)
/// is refused, not granted a directory about to vanish.
#[tokio::test]
async fn an_attach_is_refused_while_its_directory_is_being_removed() {
    let r = Repo::new();
    let held = twr_core::testkit::git::HeldGit::install(&r.root.join("bin"));
    let mut o = opts(&r.root, None);
    o.git_bin = Some(held.bin.clone());
    let hub = Hub::open(o).await.unwrap();
    let a = create(&hub, &r.repo, "owner").await.unwrap();
    let a = a["id"].as_str().unwrap().to_string();
    let wt = r.worktree("owner");
    let b = thread_in(&hub, "guest", &r.repo, &wt, "local");
    let mut sub = hub.subscribe_thread(&b, 0).unwrap();
    send(&hub, &b, "hello");
    until(&mut sub, |e| e.kind() == "report.ready").await;
    // Its runtime (which holds the directory while it runs) is gone; the session stays.
    hub.stop_thread(&b, "test").unwrap();
    until(&mut sub, |e| e.kind() == "thread.stopped").await;

    let park = {
        let (hub, a) = (hub.clone(), a.clone());
        tokio::spawn(async move { hub.park_thread(&a, false, false, &user()).await })
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while !held.held() {
        assert!(tokio::time::Instant::now() < deadline, "git never held");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let e = hub.attach_begin(&b, false, "cli").await.unwrap_err();
    assert_eq!(e.code, codes::CONFLICT, "{e:?}");
    assert!(
        e.message.contains("being removed") || e.message.contains("being parked"),
        "{}",
        e.message
    );
    assert_eq!(hub.thread_view(&b).unwrap().attached, None);
    held.release();
    let out = timeout(Duration::from_secs(60), park)
        .await
        .expect("the park never finished")
        .unwrap()
        .unwrap();
    assert!(out.worktree_removed);
    assert!(held.finished());
    hub.shutdown().await;
}

/// The first attach of a Claude Code thread to the worktree twr made for it tells the CLI that
/// Claude will ask whether to trust the folder (`trust_prompt`); later attaches, other
/// runtimes and threads without a twr worktree do not.
#[tokio::test]
async fn the_first_claude_attach_to_a_twr_worktree_warns_of_the_trust_question() {
    let r = Repo::new();
    let mut o = opts(&r.root, None);
    // A stand-in for Claude Code (the hint depends on the runtime's name only).
    o.adapters = Arc::new(|name: &str| -> Option<Arc<dyn Adapter>> {
        matches!(name, "fake" | "claude-code")
            .then(|| Arc::new(FakeAdapter::new()) as Arc<dyn Adapter>)
    });
    let hub = Hub::open(o).await.unwrap();
    let new = |name: &'static str, runtime: &'static str, no_worktree: bool| {
        let hub = hub.clone();
        let project = r.repo.to_string_lossy().into_owned();
        async move {
            let t = hub
                .new_thread(
                    NewThread {
                        project,
                        runtime: Some(runtime.into()),
                        name: Some(name.into()),
                        no_worktree,
                        ..Default::default()
                    },
                    "test",
                )
                .await
                .unwrap();
            let id = t["id"].as_str().unwrap().to_string();
            let mut sub = hub.subscribe_thread(&id, 0).unwrap();
            send(&hub, &id, "hello");
            until(&mut sub, |e| e.kind() == "report.ready").await;
            id
        }
    };
    let attach = |id: String| {
        let hub = hub.clone();
        async move {
            let grant = hub.attach_begin(&id, false, "cli").await.unwrap();
            assert!(hub.attach_end(&id, "cli", grant.lease, None).unwrap());
            grant.trust_prompt
        }
    };
    let claude = new("cc", "claude-code", false).await;
    assert!(attach(claude.clone()).await, "first attach");
    assert!(!attach(claude).await, "second attach");
    let fake = new("other", "fake", false).await;
    assert!(!attach(fake).await);
    let shared = new("shared", "claude-code", true).await;
    assert!(!attach(shared).await);
    hub.shutdown().await;
}

/// Two isolated hubs can work on the same repository with the same thread name. Git branch
/// names are isolated too; parking and resuming one never takes over the other's branch.
#[tokio::test]
async fn isolated_hubs_share_a_repo_without_sharing_threads_or_branches() {
    use twr_core::paths::Paths;
    let dir = tempfile::tempdir().unwrap();
    let repo = init_repo(&dir.path().join("repo"));
    let a_paths = Paths::with_home(&dir.path().join("personal"));
    let b_paths = Paths::with_home(&dir.path().join("work"));
    let options = |paths: &Paths| {
        let mut o = opts(paths.config_file.parent().unwrap(), None);
        o.instance_name = paths.instance_name();
        o.worktree_namespace = paths.worktree_namespace();
        o
    };
    let a = Hub::open(options(&a_paths)).await.unwrap();
    let b = Hub::open(options(&b_paths)).await.unwrap();
    assert_eq!(a.info()["instance"], "personal");
    assert_eq!(b.info()["instance"], "work");
    let a_thread = create(&a, &repo, "api").await.unwrap();
    let b_thread = create(&b, &repo, "api").await.unwrap();
    assert_ne!(a_thread["id"], b_thread["id"]);
    let a_branch = format!("twr/{}/api", a_paths.worktree_namespace().unwrap());
    let b_branch = format!("twr/{}/api", b_paths.worktree_namespace().unwrap());
    assert_ne!(a_branch, b_branch);
    assert!(branch_exists(&repo, &a_branch));
    assert!(branch_exists(&repo, &b_branch));
    let a_wt = a.thread_view("api").unwrap().worktree.unwrap();
    let b_wt = b.thread_view("api").unwrap().worktree.unwrap();
    assert_ne!(a_wt, b_wt);
    assert!(Path::new(&a_wt).starts_with(a_paths.worktrees_dir()));
    assert!(Path::new(&b_wt).starts_with(b_paths.worktrees_dir()));
    assert_eq!(a.list_threads().len(), 1);
    assert_eq!(b.list_threads().len(), 1);
    a.park_thread("api", false, false, &user()).await.unwrap();
    assert!(Path::new(&b_wt).exists());
    assert_eq!(
        run(Path::new(&b_wt), &["branch", "--show-current"]),
        b_branch
    );
    let id = a.resolve("api").unwrap();
    a.shutdown().await;
    drop(a);
    let a = Hub::open(options(&a_paths)).await.unwrap();
    assert_eq!(a.resolve("api").unwrap(), id);
    a.resume_thread("api", &user()).await.unwrap();
    assert_eq!(
        run(Path::new(&a_wt), &["branch", "--show-current"]),
        a_branch
    );
    assert_eq!(
        run(Path::new(&b_wt), &["branch", "--show-current"]),
        b_branch
    );
    let card = a.delete_threads(&["api".into()], &user()).await.unwrap();
    a.respond_at(
        card["attn_id"].as_str().unwrap(),
        Answer::new("Delete"),
        card["revision"].as_u64(),
        &user(),
    )
    .unwrap();
    timeout(WAIT, async {
        while a.thread_view("api").is_ok() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let trash = a.trash_list().await.unwrap();
    let item = trash["items"][0]["item"].as_str().unwrap();
    assert!(b.trash_list().await.unwrap()["items"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(Path::new(&b_wt).exists());
    a.trash_restore(item, Some("api-restored"), &user())
        .await
        .unwrap();
    let restored = a.thread_view("api-restored").unwrap().worktree.unwrap();
    assert_eq!(
        run(Path::new(&restored), &["branch", "--show-current"]),
        format!("twr/{}/api-restored", a_paths.worktree_namespace().unwrap())
    );
    assert_eq!(
        run(Path::new(&b_wt), &["branch", "--show-current"]),
        b_branch
    );
    a.shutdown().await;
    b.shutdown().await;
}
