//! `thread.delete` with real temporary git repositories (`hub::delete`): the delete card says
//! what goes (uncommitted files, edits `git status` hides, ignored files, unpushed commits, also
//! those only threads on the card share, queued prompts), only the user answers it, Cancel
//! keeps everything, Delete removes the worktree (dirty or already gone; what git cannot remove
//! is left and named), the branch, the thread and its queue; more to lose by the time the card
//! is confirmed (or asked for again) brings a new card instead; one card covers several
//! threads; cards whose threads are all gone close; and what is refused.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::time::timeout;
use twr_adapters::fake::FakeAdapter;
use twr_core::adapter::{Adapter, Answer};
use twr_core::config::Config;
use twr_core::event::*;
use twr_core::proto::codes;
use twr_core::testkit::git::{branch_exists, commit_all, init_repo, run};
use twr_hub::hub::ThreadSubscription;
use twr_hub::{Caller, Hub, HubOptions, NewThread};

const WAIT: Duration = Duration::from_secs(20);

fn opts(dir: &Path) -> HubOptions {
    let mut o = HubOptions::new(dir.join("data").join("twr.db"), Config::default());
    o.adapters = Arc::new(|name: &str| -> Option<Arc<dyn Adapter>> {
        (name == "fake").then(|| Arc::new(FakeAdapter::new()) as Arc<dyn Adapter>)
    });
    o.git_tick = Some(Duration::ZERO);
    o.stall_after = Some(Duration::ZERO);
    o
}

fn user() -> Caller {
    Caller::user("test")
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

/// A `fake` thread `name` on the repository (its own worktree unless `no_worktree`); its id.
async fn create(hub: &Hub, project: &Path, name: &str, no_worktree: bool) -> String {
    let t = hub
        .new_thread(
            NewThread {
                project: project.to_string_lossy().into_owned(),
                runtime: Some("fake".into()),
                name: Some(name.into()),
                no_worktree,
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap();
    t["id"].as_str().unwrap().to_string()
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
    .unwrap_or_else(|_| {
        panic!(
            "timed out; saw {:?}",
            seen.iter().map(|e| e.kind()).collect::<Vec<_>>()
        )
    });
    seen
}

/// `thread.delete` for `threads` by the user.
async fn ask(hub: &Hub, threads: &[&str]) -> Value {
    let threads: Vec<String> = threads.iter().map(|t| t.to_string()).collect();
    hub.delete_threads(&threads, &user()).await.unwrap()
}

/// Answers delete card `card` (a `thread.delete` result) with `decision`, as `caller`.
fn answer(hub: &Hub, card: &Value, decision: &str, caller: &Caller) -> Result<Event, String> {
    hub.respond_at(
        card["attn_id"].as_str().unwrap(),
        Answer::new(decision),
        card["revision"].as_u64(),
        caller,
    )
    .map_err(|e| format!("{}: {}", e.code, e.message))
}

/// Whether the attention item `attn_id` is open.
fn open(hub: &Hub, attn_id: &Value) -> bool {
    hub.attention_list(false)
        .iter()
        .any(|i| i["attn_id"] == *attn_id)
}

/// Waits until thread `id` is gone from the registry.
async fn gone(hub: &Hub, id: &str) {
    timeout(WAIT, async {
        while hub.thread_view(id).is_ok() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "{id} was not deleted; items: {:?}",
            hub.attention_list(true)
        )
    });
}

/// Every control event recorded so far.
fn control(hub: &Hub) -> Vec<Event> {
    hub.control_read(0, 100_000)
        .unwrap()
        .0
        .into_iter()
        .map(|(_, e)| e)
        .collect()
}

/// The `thread.deleted` of thread `id`.
fn deleted(hub: &Hub, id: &str) -> ThreadDeleted {
    control(hub)
        .into_iter()
        .find_map(|e| match e.body {
            EventBody::ThreadDeleted(d) if e.thread == id => Some(d),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no thread.deleted of {id}"))
}

/// Which life of thread `id` this is (the seq of its latest `thread.created` or
/// `thread.restored`), as a card records it.
fn life(hub: &Hub, id: &str) -> u64 {
    let v = hub.thread_view(id).unwrap();
    assert!(v.lifecycle_seq > 0, "{v:?}");
    v.lifecycle_seq
}

/// The `ext.twr.delete` of delete card `attn_id`.
fn card_ext(hub: &Hub, attn_id: &Value) -> Value {
    control(hub)
        .into_iter()
        .find_map(|e| match e.body {
            EventBody::AttentionRaised(p) if p.attn_id == *attn_id => {
                Some(json!(p.ext)["twr"][DELETE_KEY].clone())
            }
            _ => None,
        })
        .expect("the card")
}

/// A thread with a commit only its branch has and an uncommitted file: the card says so, and
/// what it records names the thread. Only the user answers it; asking again returns it;
/// Cancel keeps everything.
#[tokio::test]
async fn the_card_says_what_goes_and_only_the_user_answers_it() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root)).await.unwrap();
    let id = create(&hub, &r.repo, "api", false).await;
    let wt = r.worktree("api");
    std::fs::write(wt.join("done.txt"), "done").unwrap();
    commit_all(&wt, "work");
    std::fs::write(wt.join("wip.txt"), "wip").unwrap();

    let card = ask(&hub, &["api"]).await;
    assert_eq!(card["status"], "needs_confirmation");
    assert_eq!(
        (&card["thread"], &card["thread_name"]),
        (&json!(id), &json!("api"))
    );
    assert_eq!(card["existing"], false);
    assert_eq!(card["options"], json!(["Delete", "Cancel"]));
    let t = &card["threads"][0];
    assert_eq!(card["threads"].as_array().unwrap().len(), 1);
    assert_eq!(
        (
            &t["thread"],
            &t["dirty_files"],
            &t["branch_only_commits"],
            &t["pending"]
        ),
        (&json!(id), &json!(1), &json!(1), &json!(0)),
        "{card}"
    );
    assert_eq!(t["branch"], "twr/api");
    assert_eq!(t["worktree"], json!(wt.to_string_lossy()));
    let summary = card["summary"].as_str().unwrap();
    for part in [
        "Delete thread api",
        "with 1 uncommitted file it has",
        "moves its branch twr/api to the trash, with 1 commit on no other branch, tag or remote (not pushed)",
        "The trash keeps it for 7 days, then purges it",
    ] {
        assert!(summary.contains(part), "{part:?} missing from {summary:?}");
    }
    let ext = card_ext(&hub, &card["attn_id"]);
    assert_eq!(
        ext["threads"],
        json!([{"thread": id, "name": "api", "lifecycle": life(&hub, &id)}])
    );
    assert_eq!(ext["asked_by"], "user:test");

    // The orchestrator neither answers nor dismisses it.
    let e = answer(&hub, &card, "Delete", &Caller::Orchestrator).unwrap_err();
    assert!(e.starts_with(codes::RELAY_FORBIDDEN), "{e}");
    let e = hub
        .attention_dismiss(
            card["attn_id"].as_str().unwrap(),
            None,
            &Caller::Orchestrator,
        )
        .unwrap_err();
    assert_eq!(e.code, codes::RELAY_FORBIDDEN, "{e:?}");
    // Nor does any other answer than Delete or Cancel.
    let e = answer(&hub, &card, "yes", &user()).unwrap_err();
    assert!(e.starts_with(codes::BAD_REQUEST), "{e}");
    let again = ask(&hub, &[&id]).await;
    assert_eq!(
        (&again["attn_id"], &again["existing"]),
        (&card["attn_id"], &json!(true))
    );
    assert!(open(&hub, &card["attn_id"]));

    answer(&hub, &card, "cancel", &user()).unwrap();
    assert!(!open(&hub, &card["attn_id"]));
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(hub.thread_view(&id).is_ok());
    assert!(wt.join("wip.txt").exists());
    assert!(branch_exists(&r.repo, "twr/api"));
    // A new card after the cancel.
    let fresh = ask(&hub, &["api"]).await;
    assert_ne!(fresh["attn_id"], card["attn_id"]);
    hub.shutdown().await;
}

/// Delete stops a running turn, removes a dirty worktree and the branch, drops the queue and
/// the thread; its name is free again.
#[tokio::test]
async fn delete_removes_the_worktree_the_branch_the_queue_and_the_thread() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root)).await.unwrap();
    let id = create(&hub, &r.repo, "api", false).await;
    let wt = r.worktree("api");
    let mut sub = hub.subscribe_thread(&id, 0).unwrap();
    send(&hub, &id, "!sleep 60000\nworking");
    until(&mut sub, |e| e.kind() == "turn.started").await;
    let queued = send(&hub, &id, "next");
    assert_eq!(queued.turn, None);
    std::fs::write(wt.join("wip.txt"), "wip").unwrap();

    let card = ask(&hub, &["api"]).await;
    assert_eq!(card["threads"][0]["pending"], 1, "{card}");
    let summary = card["summary"].as_str().unwrap();
    assert!(summary.contains("drops 1 queued prompt"), "{summary}");
    assert!(summary.contains("with 1 uncommitted file"), "{summary}");
    answer(&hub, &card, "Delete", &user()).unwrap();
    gone(&hub, &id).await;

    assert!(!wt.exists(), "the worktree is gone");
    assert!(!branch_exists(&r.repo, "twr/api"));
    assert!(!run(&r.repo, &["worktree", "list"]).contains("api"));
    let d = deleted(&hub, &id);
    assert_eq!(d.name, "api");
    assert_eq!(d.worktree, Some(wt.to_string_lossy().into_owned()));
    assert_eq!(d.branch.as_deref(), Some("twr/api"));
    assert_eq!((d.dirty_files, d.dropped_prompts), (Some(1), 1));
    assert!(!open(&hub, &card["attn_id"]));
    assert!(hub.list_threads().is_empty());
    assert_eq!(hub.thread_view("api").unwrap_err().code, codes::NOT_FOUND);
    assert_eq!(hub.live_runtimes(), 0);
    // Nothing of it stays open, and nothing more goes to it.
    assert!(hub
        .attention_list(true)
        .iter()
        .all(|i| i["thread"] != json!(id)));
    assert!(hub
        .send(&id, "hi", TurnFrom::User, Source::User("test".into()))
        .is_err());

    // The name is free: a new thread `api`, with a new worktree and branch.
    let again = create(&hub, &r.repo, "api", false).await;
    assert_ne!(again, id);
    assert!(wt.exists() && branch_exists(&r.repo, "twr/api"));
    hub.shutdown().await;
}

/// One card for three threads (one named twice): a running one, a parked one whose worktree a
/// park removed and whose branch has a commit only it has, and one working in the repository.
/// Asking for the same threads in another order returns the card; Delete deletes each.
#[tokio::test]
async fn one_card_deletes_several_threads_also_a_parked_one() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root)).await.unwrap();
    let a = create(&hub, &r.repo, "a", false).await;
    let b = create(&hub, &r.repo, "b", false).await;
    let c = create(&hub, &r.repo, "c", true).await;
    let wt_b = r.worktree("b");
    std::fs::write(wt_b.join("b.txt"), "b").unwrap();
    commit_all(&wt_b, "b's work");
    let parked = hub.park_thread(&b, false, false, &user()).await.unwrap();
    assert!(parked.worktree_removed, "{parked:?}");
    assert!(!wt_b.exists() && branch_exists(&r.repo, "twr/b"));

    let card = ask(&hub, &["a", "b", "c", &a]).await;
    let threads = card["threads"].as_array().unwrap();
    let ids: Vec<&Value> = threads.iter().map(|t| &t["thread"]).collect();
    assert_eq!(
        ids,
        [&json!(a), &json!(b), &json!(c)],
        "in order, once each"
    );
    assert_eq!(
        (&card["thread"], &card["thread_name"]),
        (&json!(a), &json!("a"))
    );
    assert_eq!(threads[1]["worktree_missing"], true, "{card}");
    assert_eq!(threads[1]["branch_only_commits"], 1, "{card}");
    assert!(threads[2].get("worktree").is_none(), "{card}");
    let summary = card["summary"].as_str().unwrap();
    let lines: Vec<&str> = summary.lines().collect();
    assert_eq!(lines[0], "Delete 3 threads?");
    assert_eq!(lines.len(), 5, "{summary}");
    assert!(
        lines[4].starts_with("The trash keeps them for 7 days"),
        "{summary}"
    );
    assert!(
        lines[1].starts_with("- a (project proj): stops its runtime, moves its worktree"),
        "{summary}"
    );
    assert!(
        lines[2].contains("moves the record of its worktree")
            && lines[2].contains("(already gone)"),
        "{summary}"
    );
    assert!(
        lines[2].contains("with 1 commit on no other branch"),
        "{summary}"
    );
    assert!(
        lines[3].contains(&format!("it works in {}", r.repo.display())),
        "{summary}"
    );
    assert_eq!(
        card_ext(&hub, &card["attn_id"])["threads"],
        json!([
            {"thread": a, "name": "a", "lifecycle": life(&hub, &a)},
            {"thread": b, "name": "b", "lifecycle": life(&hub, &b)},
            {"thread": c, "name": "c", "lifecycle": life(&hub, &c)}
        ])
    );
    let same = ask(&hub, &["c", "b", "a"]).await;
    assert_eq!(
        (&same["attn_id"], &same["existing"]),
        (&card["attn_id"], &json!(true))
    );
    assert_eq!(same["threads"].as_array().unwrap().len(), 3);
    // A subset is another card.
    let other = ask(&hub, &["a", "b"]).await;
    assert_ne!(other["attn_id"], card["attn_id"]);

    answer(&hub, &card, "Delete", &user()).unwrap();
    for id in [&a, &b, &c] {
        gone(&hub, id).await;
    }
    let d = deleted(&hub, &b);
    assert_eq!(d.worktree, None, "its worktree was gone already");
    assert_eq!(d.branch.as_deref(), Some("twr/b"));
    assert_eq!(d.branch_only_commits, Some(1));
    assert!(!branch_exists(&r.repo, "twr/b") && !branch_exists(&r.repo, "twr/a"));
    assert!(!r.worktree("a").exists());
    assert!(r.repo.join("README.md").exists(), "c's directory stays");
    assert_eq!(deleted(&hub, &c).branch, None);
    // The subset's card is closed with its threads.
    assert!(!open(&hub, &other["attn_id"]));
    assert!(hub.attention_list(false).is_empty());
    hub.shutdown().await;
}

/// A card is closed once all its threads were deleted by other cards; one still naming a
/// thread stays open, and confirming it skips the threads deleted meanwhile. (`z` is stopped:
/// deleted all the same.)
#[tokio::test]
async fn cards_follow_threads_deleted_elsewhere() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root)).await.unwrap();
    let x = create(&hub, &r.repo, "x", true).await;
    let y = create(&hub, &r.repo, "y", true).await;
    let z = create(&hub, &r.repo, "z", true).await;
    let mut sub = hub.subscribe_thread(&z, 0).unwrap();
    send(&hub, &z, "hello");
    until(&mut sub, |e| e.kind() == "report.ready").await;
    hub.stop_thread(&z, "test").unwrap();
    until(&mut sub, |e| e.kind() == "thread.stopped").await;
    assert!(hub.thread_view(&z).unwrap().stopped);
    let xy = ask(&hub, &["x", "y"]).await;
    let only_x = ask(&hub, &["x"]).await;
    let only_y = ask(&hub, &["y"]).await;
    let xz = ask(&hub, &["x", "z"]).await;

    answer(&hub, &only_x, "Delete", &user()).unwrap();
    gone(&hub, &x).await;
    assert!(open(&hub, &xy["attn_id"]) && open(&hub, &xz["attn_id"]));
    // Asking for a deleted thread: not found.
    let e = hub
        .delete_threads(std::slice::from_ref(&x), &user())
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::NOT_FOUND, "{e:?}");

    answer(&hub, &only_y, "Delete", &user()).unwrap();
    gone(&hub, &y).await;
    timeout(WAIT, async {
        while open(&hub, &xy["attn_id"]) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the card of x and y closes");
    let closed = control(&hub).into_iter().find_map(|e| match e.body {
        EventBody::AttentionResolved(p) if p.attn_id == xy["attn_id"] => Some(p.by),
        _ => None,
    });
    assert_eq!(closed, Some(ResolvedBy::ThreadStopped));
    assert!(open(&hub, &xz["attn_id"]), "z is still there");

    answer(&hub, &xz, "Delete", &user()).unwrap();
    gone(&hub, &z).await;
    assert!(hub.list_threads().is_empty());
    assert!(hub.attention_list(true).is_empty(), "no failure item");
    hub.shutdown().await;
}

/// A thread whose deletion fails stays, with an error item saying why; the card's other
/// threads are deleted all the same, and the failed one can be asked for again.
#[tokio::test]
async fn a_failed_deletion_leaves_that_thread_and_the_rest_go() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root)).await.unwrap();
    let p = create(&hub, &r.repo, "p", false).await;
    let q = create(&hub, &r.repo, "q", false).await;
    // Another repository is at p's worktree's path: twr does not delete such a directory.
    let wt_p = r.worktree("p");
    std::fs::remove_dir_all(&wt_p).unwrap();
    init_repo(&wt_p);
    std::fs::write(wt_p.join("mine.txt"), "not twr's").unwrap();
    let card = ask(&hub, &["p", "q"]).await;
    answer(&hub, &card, "Delete", &user()).unwrap();
    gone(&hub, &q).await;
    let failure = timeout(WAIT, async {
        loop {
            let items = hub.attention_list(true);
            if let Some(i) = items.iter().find(|i| i["thread"] == json!(p)) {
                return i.clone();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("an error item on p");
    assert_eq!(failure["reason"], "error");
    let summary = failure["summary"].as_str().unwrap();
    assert!(
        summary.starts_with("Deleting p failed: moving its worktree and branch to the trash"),
        "{summary}"
    );
    assert!(summary.contains("`twr delete p`"), "{summary}");
    assert!(wt_p.join("mine.txt").exists() && branch_exists(&r.repo, "twr/p"));
    let v = hub.thread_view(&p).unwrap();
    assert!(v.parked, "stopped like a park first");
    assert!(!r.worktree("q").exists() && !branch_exists(&r.repo, "twr/q"));
    // Not being deleted any more: asked again, a new card.
    let again = ask(&hub, &["p"]).await;
    assert_ne!(again["attn_id"], card["attn_id"]);
    hub.shutdown().await;
}

/// Makes a directory read-only (`0o500`) until dropped, so the tempdir can be cleaned up.
struct ReadOnly(PathBuf);

impl ReadOnly {
    /// `dir` read-only, with a file in it; `None` when this user can write it all the same
    /// (root): what git cannot remove does not reproduce.
    fn new(dir: &Path) -> Option<ReadOnly> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("f"), "f").unwrap();
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        let ro = ReadOnly(dir.to_path_buf());
        if std::fs::write(dir.join("probe"), "").is_ok() {
            eprintln!(
                "skipped: {} is writable all the same (root?)",
                dir.display()
            );
            return None;
        }
        Some(ro)
    }
}

impl Drop for ReadOnly {
    fn drop(&mut self) {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
    }
}

/// A worktree with a read-only directory in it moves to the trash whole (a rename needs no
/// right to change what is inside), branch too; emptying the trash removes it all the same.
#[tokio::test]
async fn a_worktree_with_a_read_only_directory_moves_whole() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root)).await.unwrap();
    let id = create(&hub, &r.repo, "api", false).await;
    let wt = r.worktree("api");
    let Some(ro) = ReadOnly::new(&wt.join("ro")) else {
        hub.shutdown().await;
        return;
    };
    let card = ask(&hub, &["api"]).await;
    answer(&hub, &card, "Delete", &user()).unwrap();
    gone(&hub, &id).await;
    // Its guard names the old path: the copy in the trash stays read-only.
    drop(ro);
    let d = deleted(&hub, &id);
    assert_eq!(d.worktree, Some(wt.to_string_lossy().into_owned()));
    assert_eq!(d.worktree_left, None);
    assert_eq!(d.branch.as_deref(), Some("twr/api"));
    let item = d.trash.unwrap();
    let moved = r.root.join("data/trash").join(&item).join("worktree/ro/f");
    assert!(moved.exists() && !wt.exists());
    assert!(!branch_exists(&r.repo, "twr/api"));
    assert!(!run(&r.repo, &["worktree", "list"]).contains("api"));
    assert!(hub.attention_list(true).is_empty(), "no failure item");
    let out = hub
        .trash_empty(std::slice::from_ref(&item), false, &user())
        .await
        .unwrap();
    assert_eq!(out["purged"][0]["item"], json!(item), "{out}");
    assert!(!moved.exists());
    hub.shutdown().await;
}

/// The card counts the commits removing the worktree loses (made on a detached HEAD, on no
/// branch, tag or remote), and says the branch existed before the thread; the record counts
/// them too.
#[tokio::test]
async fn the_card_counts_detached_commits_and_names_a_branch_from_before() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root)).await.unwrap();
    // Made without checking it out (the main checkout's HEAD reflog, which stays, would
    // keep it).
    let old = run(
        &r.repo,
        &["commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", "old work"],
    );
    run(&r.repo, &["branch", "twr/old", old.trim()]);
    let id = create(&hub, &r.repo, "old", false).await;
    let wt = r.worktree("old");
    run(&wt, &["checkout", "-q", "--detach"]);
    std::fs::write(wt.join("x.txt"), "x").unwrap();
    commit_all(&wt, "detached experiment");

    let card = ask(&hub, &["old"]).await;
    let t = &card["threads"][0];
    assert_eq!(
        (
            &t["head_only_commits"],
            &t["branch_only_commits"],
            &t["existing_branch"]
        ),
        (&json!(1), &json!(1), &json!(true)),
        "{card}"
    );
    let summary = card["summary"].as_str().unwrap();
    for part in [
        "moves 1 commit made on a detached HEAD in its worktree to the trash (on no branch, \
         tag or remote; not pushed)",
        "moves its branch twr/old, which existed before the thread, to the trash, with 1 commit \
         on no other branch, tag or remote (not pushed)",
    ] {
        assert!(summary.contains(part), "{part:?} missing from {summary:?}");
    }
    // A thread on a new branch: nothing about either.
    create(&hub, &r.repo, "new", false).await;
    let fresh = ask(&hub, &["new"]).await;
    let summary = fresh["summary"].as_str().unwrap();
    assert!(
        !summary.contains("existed before") && !summary.contains("detached"),
        "{summary}"
    );
    assert!(
        fresh["threads"][0].get("existing_branch").is_none(),
        "{fresh}"
    );

    answer(&hub, &card, "Delete", &user()).unwrap();
    gone(&hub, &id).await;
    let d = deleted(&hub, &id);
    assert_eq!(
        (d.head_only_commits, d.branch_only_commits),
        (Some(1), Some(1))
    );
    assert!(!wt.exists() && !branch_exists(&r.repo, "twr/old"));
    hub.shutdown().await;
}

/// A thread `name` working in `dir` without a worktree of its own (`thread.created` as a log
/// from before `thread.new` refused another thread's worktree has it).
fn thread_in(hub: &Hub, name: &str, repo: &Path, dir: &Path) -> String {
    let id = format!("in-{name}");
    hub.append(
        &id,
        None,
        Source::User("test".into()),
        EventBody::ThreadCreated(ThreadCreated {
            name: name.into(),
            project: Some("proj".into()),
            repo: Some(repo.to_string_lossy().into_owned()),
            worktree: Some(dir.to_string_lossy().into_owned()),
            box_name: "local".into(),
            runtime: "fake".into(),
            mode: "manual".into(),
            ext: json!({"twr": {"worktree": "repo"}}).as_object().cloned(),
            ..Default::default()
        }),
        None,
    )
    .unwrap();
    id
}

/// What is refused, for the whole request: no thread, too many, the orchestrator, an attached
/// thread, and a thread another thread is attached inside the worktree of (also when the card
/// is confirmed: it stays open until the attach ends).
#[tokio::test]
async fn deletions_are_refused() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root)).await.unwrap();
    let e = hub.delete_threads(&[], &user()).await.unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST);
    let many: Vec<String> = (0..33).map(|i| format!("t{i}")).collect();
    let e = hub.delete_threads(&many, &user()).await.unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST);
    assert!(e.message.contains("at most 32"), "{e:?}");
    let e = hub
        .delete_threads(&["nope".into()], &user())
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::NOT_FOUND);

    let owner = create(&hub, &r.repo, "owner", false).await;
    hub.append(
        "orch",
        None,
        Source::User("test".into()),
        EventBody::ThreadCreated(ThreadCreated {
            name: "orchestrator".into(),
            box_name: "local".into(),
            runtime: "fake".into(),
            mode: "manual".into(),
            ext: json!({"twr": {"role": "orchestrator"}})
                .as_object()
                .cloned(),
            ..Default::default()
        }),
        None,
    )
    .unwrap();
    let e = hub
        .delete_threads(&["owner".into(), "orchestrator".into()], &user())
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST);
    assert!(
        e.message.contains("is the orchestrator thread")
            && e.message.contains("twr orchestrator use"),
        "{e:?}"
    );
    assert!(hub.attention_list(false).is_empty(), "no card for the rest");
    // A later orchestrator retires it: still refused, and not told to switch.
    hub.append(
        "orch2",
        None,
        Source::User("test".into()),
        EventBody::ThreadCreated(ThreadCreated {
            name: "orchestrator-2".into(),
            box_name: "local".into(),
            runtime: "fake".into(),
            mode: "manual".into(),
            ext: json!({"twr": {"role": "orchestrator"}})
                .as_object()
                .cloned(),
            ..Default::default()
        }),
        None,
    )
    .unwrap();
    assert!(hub.thread_view("orch").unwrap().retired);
    let e = hub
        .delete_threads(&["orch".into()], &user())
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST);
    assert!(
        e.message.contains("is a retired orchestrator thread")
            && !e.message.contains("orchestrator use"),
        "{e:?}"
    );

    // A thread that works in owner's worktree, attached.
    let wt = r.worktree("owner");
    let guest = thread_in(&hub, "guest", &r.repo, &wt);
    let mut sub = hub.subscribe_thread(&guest, 0).unwrap();
    send(&hub, &guest, "hello");
    until(&mut sub, |e| e.kind() == "report.ready").await;
    let grant = hub.attach_begin(&guest, false, "cli").await.unwrap();
    let e = hub
        .delete_threads(&["guest".into()], &user())
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::CONFLICT);
    assert!(e.message.contains("attached by cli"), "{e:?}");
    let e = hub
        .delete_threads(&["owner".into()], &user())
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::CONFLICT);
    assert!(
        e.message.contains("thread guest is attached (by cli)"),
        "{e:?}"
    );
    assert!(hub.attach_end(&guest, "cli", grant.lease, None).unwrap());

    // Attached between the card and its confirmation: refused, the card stays.
    let card = ask(&hub, &["owner"]).await;
    let grant = hub.attach_begin(&guest, false, "cli").await.unwrap();
    let e = answer(&hub, &card, "Delete", &user()).unwrap_err();
    assert!(e.starts_with(codes::CONFLICT) && e.contains("guest"), "{e}");
    assert!(open(&hub, &card["attn_id"]));
    assert!(hub.thread_view(&owner).is_ok() && wt.exists());
    assert!(hub.attach_end(&guest, "cli", grant.lease, None).unwrap());
    answer(&hub, &card, "Delete", &user()).unwrap();
    // Confirmed: no attach starts inside the worktree while it is being deleted (on a local
    // box too: its reservation only starts with git).
    match hub.attach_begin(&guest, false, "cli").await {
        Err(e) => assert_eq!(e.code, codes::CONFLICT, "{e:?}"),
        Ok(g) => panic!("attached inside a worktree being deleted: {g:?}"),
    }
    gone(&hub, &owner).await;
    assert!(!wt.exists());
    hub.shutdown().await;
}

/// The open delete card other than `not`, as an attention item (with its `attn_id` and
/// `revision`), once there is one.
async fn new_card(hub: &Hub, not: &Value) -> Value {
    timeout(WAIT, async {
        loop {
            if let Some(i) = hub.attention_list(false).into_iter().find(|i| {
                i["attn_id"] != *not
                    && i["thread"] == HUB_THREAD
                    && i["options"] == json!(["Delete", "Cancel"])
            }) {
                return i;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no new card; items: {:?}", hub.attention_list(true)))
}

/// Threads keep working while their card is open: a commit and an uncommitted file that
/// appear after a clean card are not deleted under it. Confirmed, the thread is stopped and
/// checked again; nothing is removed, and a new card lists them and says its runtime stays
/// stopped. Confirming that one deletes the thread.
#[tokio::test]
async fn more_to_lose_after_the_card_brings_a_new_card() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root)).await.unwrap();
    let id = create(&hub, &r.repo, "api", false).await;
    let wt = r.worktree("api");
    let mut sub = hub.subscribe_thread(&id, 0).unwrap();
    send(&hub, &id, "hello");
    until(&mut sub, |e| e.kind() == "report.ready").await;

    let card = ask(&hub, &["api"]).await;
    let summary = card["summary"].as_str().unwrap();
    assert!(summary.contains("(no uncommitted changes)"), "{summary}");
    assert!(
        summary.contains("each of its commits is on another branch"),
        "{summary}"
    );
    // Nothing is stopped before the user confirms.
    assert!(!hub.thread_view(&id).unwrap().parked);
    // Asked again with nothing new: the same card, with what it says.
    let same = ask(&hub, &["api"]).await;
    assert_eq!(
        (&same["attn_id"], &same["existing"]),
        (&card["attn_id"], &json!(true))
    );
    assert_eq!(same["threads"][0]["dirty_files"], 0, "{same}");
    assert_eq!(same["threads"][0]["branch_only_commits"], 0, "{same}");
    assert!(
        same["message"]
            .as_str()
            .unwrap()
            .contains("nothing more to lose than it says"),
        "{same}"
    );

    std::fs::write(wt.join("done.txt"), "done").unwrap();
    commit_all(&wt, "work");
    std::fs::write(wt.join("wip.txt"), "wip").unwrap();
    answer(&hub, &card, "Delete", &user()).unwrap();
    let fresh = new_card(&hub, &card["attn_id"]).await;
    let summary = fresh["summary"].as_str().unwrap();
    for part in [
        format!(
            "Nothing was deleted: since card {} was confirmed, there is more to lose (api: \
             uncommitted files, commits on no branch that stays). Its runtime was stopped and \
             stays stopped.",
            card["attn_id"].as_str().unwrap()
        ),
        "with 1 uncommitted file it has".to_string(),
        "with 1 commit on no other branch, tag or remote (not pushed)".to_string(),
    ] {
        assert!(summary.contains(&part), "{part:?} missing from {summary:?}");
    }
    assert_eq!(
        card_ext(&hub, &fresh["attn_id"])["replaces"],
        card["attn_id"]
    );
    assert!(!open(&hub, &card["attn_id"]));
    // Nothing went; the runtime stays stopped.
    let v = hub.thread_view(&id).unwrap();
    assert!(v.parked, "{v:?}");
    assert_eq!(hub.live_runtimes(), 0);
    assert!(wt.join("wip.txt").exists() && wt.join("done.txt").exists());
    assert!(branch_exists(&r.repo, "twr/api"));
    // Not being deleted any more: asking again returns the new card.
    let again = ask(&hub, &["api"]).await;
    assert_eq!(again["attn_id"], fresh["attn_id"]);

    answer(&hub, &fresh, "Delete", &user()).unwrap();
    gone(&hub, &id).await;
    assert!(!wt.exists() && !branch_exists(&r.repo, "twr/api"));
    let d = deleted(&hub, &id);
    assert_eq!((d.dirty_files, d.branch_only_commits), (Some(1), Some(1)));
    hub.shutdown().await;
}

/// Asking again while a card is open, once there is more to lose than it says: a new card
/// replaces it (the open one is resolved `superseded`) and says why.
#[tokio::test]
async fn asking_again_after_more_work_replaces_the_card() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root)).await.unwrap();
    create(&hub, &r.repo, "api", false).await;
    let card = ask(&hub, &["api"]).await;
    std::fs::write(r.worktree("api").join("wip.txt"), "wip").unwrap();
    let again = ask(&hub, &["api"]).await;
    assert_ne!(again["attn_id"], card["attn_id"]);
    assert_eq!(
        (&again["existing"], &again["replaces"]),
        (&json!(false), &card["attn_id"])
    );
    let summary = again["summary"].as_str().unwrap();
    assert!(
        summary.starts_with(&format!(
            "This card replaces {}: since it was raised, there is more to lose (api: \
             uncommitted files).\nDelete thread api",
            card["attn_id"].as_str().unwrap()
        )),
        "{summary}"
    );
    assert!(!open(&hub, &card["attn_id"]) && open(&hub, &again["attn_id"]));
    let superseded = control(&hub).into_iter().find_map(|e| match e.body {
        EventBody::AttentionResolved(p) if p.attn_id == card["attn_id"] => p.answer,
        _ => None,
    });
    assert_eq!(
        superseded.map(|a| a.decision).as_deref(),
        Some("superseded")
    );
    hub.shutdown().await;
}

/// The card lists what `git status` does not show and the force removal deletes all the
/// same: a tracked file edited under assume-unchanged, an ignored `.env`, and an ignored
/// directory summarized with its file count; past a few entries, the rest is counted. Build
/// output (`target/`) is said apart, with its size: it is deleted, not trashed (on Linux and
/// macOS; elsewhere it is ignored files like the others).
#[tokio::test]
async fn the_card_lists_hidden_edits_and_ignored_files() {
    let r = Repo::new();
    std::fs::write(r.repo.join(".gitignore"), "target/\n.env\n*.log\nlocal/\n").unwrap();
    std::fs::write(r.repo.join("config.local"), "a=1\n").unwrap();
    commit_all(&r.repo, "ignores and config");
    let hub = Hub::open(opts(&r.root)).await.unwrap();
    let id = create(&hub, &r.repo, "api", false).await;
    let wt = r.worktree("api");
    run(&wt, &["update-index", "--assume-unchanged", "config.local"]);
    std::fs::write(wt.join("config.local"), "a=2 local edit\n").unwrap();
    std::fs::write(wt.join(".env"), "KEY=secret").unwrap();
    std::fs::create_dir_all(wt.join("target/debug")).unwrap();
    for f in ["target/debug/a", "target/debug/b", "target/c"] {
        std::fs::write(wt.join(f), "x").unwrap();
    }
    std::fs::create_dir_all(wt.join("local")).unwrap();
    for f in ["local/a", "local/b"] {
        std::fs::write(wt.join(f), "x").unwrap();
    }

    let card = ask(&hub, &["api"]).await;
    let t = &card["threads"][0];
    let deletes = cfg!(any(target_os = "linux", target_os = "macos"));
    assert_eq!(
        (
            &t["dirty_files"],
            &t["hidden_changes"],
            &t["ignored_entries"]
        ),
        (&json!(0), &json!(1), &json!(if deletes { 2 } else { 3 })),
        "{card}"
    );
    let summary = card["summary"].as_str().unwrap();
    assert!(!summary.contains("no uncommitted changes"), "{summary}");
    let mut parts = vec![
        format!("moves its worktree {} (", wt.display()),
        ") to the trash, with it 1 tracked file with edits git status does not show \
         (assume-unchanged or skip-worktree: config.local)"
            .to_string(),
    ];
    if deletes {
        assert_eq!(t["ignored"], json!([".env", "local/ (2 files)"]), "{card}");
        assert_eq!(t["build_output"], json!(["target/"]), "{card}");
        assert!(t["build_output_bytes"].as_u64().unwrap() > 0, "{card}");
        parts.push(
            "with it its ignored files (.env, local/ (2 files)), deletes its build output \
             (target/, "
                .to_string(),
        );
        parts.push(") instead of moving it to the trash".to_string());
    } else {
        let ignored = json!([".env", "local/ (2 files)", "target/ (3 files)"]);
        assert_eq!(t["ignored"], ignored, "{card}");
        assert_eq!(t["build_output"], Value::Null, "{card}");
        assert!(!summary.contains("build output"), "{summary}");
    }
    for part in parts {
        assert!(summary.contains(&part), "{part:?} missing from {summary:?}");
    }
    answer(&hub, &card, "Cancel", &user()).unwrap();

    // Many ignored entries: a few named, the rest counted.
    for f in ["a", "b", "c", "d", "e"] {
        std::fs::write(wt.join(format!("{f}.log")), "x").unwrap();
    }
    let card = ask(&hub, &["api"]).await;
    let summary = card["summary"].as_str().unwrap();
    let more = if cfg!(any(target_os = "linux", target_os = "macos")) {
        2
    } else {
        3
    };
    assert!(
        summary.contains(&format!(
            "with it its ignored files (.env, a.log, b.log, c.log, d.log, +{more} more)"
        )),
        "{summary}"
    );
    answer(&hub, &card, "Delete", &user()).unwrap();
    gone(&hub, &id).await;
    assert!(!wt.exists());
    hub.shutdown().await;
}

/// An ignored directory a card said goes to the trash (it held a repository) is not deleted
/// under that card once it is build output (the repository moved out): asking again, or
/// confirming, brings a new card that says it is deleted. (Build output is deleted only on
/// Linux and macOS.)
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[tokio::test]
async fn build_output_a_card_said_was_trashed_brings_a_new_card() {
    let r = Repo::new();
    std::fs::write(r.repo.join(".gitignore"), "target/\n").unwrap();
    commit_all(&r.repo, "ignores");
    let hub = Hub::open(opts(&r.root)).await.unwrap();
    let mut ids = Vec::new();
    for name in ["api", "web"] {
        ids.push(create(&hub, &r.repo, name, false).await);
        let wt = r.worktree(name);
        std::fs::create_dir_all(wt.join("target")).unwrap();
        std::fs::write(wt.join("target/out"), "o").unwrap();
        init_repo(&wt.join("target/dep"));
    }
    let moved_out = |name: &str| {
        std::fs::rename(
            r.worktree(name).join("target/dep"),
            r.root.join(format!("{name}-dep")),
        )
        .unwrap();
    };
    let said = |card: &Value, name: &str| {
        let summary = card["summary"].as_str().unwrap();
        let part = format!("there is more to lose ({name}: build output deleted, not trashed)");
        assert!(summary.contains(&part), "{summary}");
    };

    // Asked again.
    let card = ask(&hub, &["api"]).await;
    assert_eq!(card["threads"][0]["build_output"], Value::Null, "{card}");
    moved_out("api");
    let again = ask(&hub, &["api"]).await;
    assert_ne!(again["attn_id"], card["attn_id"]);
    assert_eq!(again["replaces"], card["attn_id"]);
    assert_eq!(again["threads"][0]["build_output"], json!(["target/"]));
    said(&again, "api");
    answer(&hub, &again, "Cancel", &user()).unwrap();

    // Confirmed: nothing goes, and a new card says it.
    let card = ask(&hub, &["web"]).await;
    assert_eq!(card["threads"][0]["build_output"], Value::Null, "{card}");
    moved_out("web");
    answer(&hub, &card, "Delete", &user()).unwrap();
    let fresh = new_card(&hub, &card["attn_id"]).await;
    said(&fresh, "web");
    assert!(r.worktree("web").join("target/out").exists());
    answer(&hub, &fresh, "Delete", &user()).unwrap();
    gone(&hub, &ids[1]).await;
    assert!(!r.worktree("web").exists());
    hub.shutdown().await;
}

/// A worktree git cannot inspect is said to be so, never clean.
#[tokio::test]
async fn a_worktree_that_cannot_be_inspected_is_not_called_clean() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root)).await.unwrap();
    create(&hub, &r.repo, "api", false).await;
    let wt = r.worktree("api");
    std::fs::write(wt.join(".git"), "gitdir: /nowhere/at/all\n").unwrap();
    let card = ask(&hub, &["api"]).await;
    let t = &card["threads"][0];
    assert_eq!(t["worktree_unchecked"], true, "{card}");
    assert!(t.get("dirty_files").is_none(), "{card}");
    let summary = card["summary"].as_str().unwrap();
    assert!(
        summary.contains(&format!(
            "moves its worktree {} to the trash, which could not be inspected (whatever it holds \
             goes with it)",
            wt.display()
        )),
        "{summary}"
    );
    assert!(
        summary.contains("(Not checked: what its worktree holds"),
        "{summary}"
    );
    assert!(!summary.contains("no uncommitted changes"), "{summary}");
    hub.shutdown().await;
}

/// Threads deleted together keep none of each other's commits: an unpushed commit only `a`'s
/// and `b`'s branches have is lost, and the card says they share it; alone, `a`'s card counts
/// `b`'s branch as a copy.
#[tokio::test]
async fn a_commit_only_threads_on_the_card_have_is_lost_and_shared() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root)).await.unwrap();
    let a = create(&hub, &r.repo, "a", false).await;
    let b = create(&hub, &r.repo, "b", false).await;
    let wt_a = r.worktree("a");
    std::fs::write(wt_a.join("a.txt"), "a").unwrap();
    commit_all(&wt_a, "unpushed");
    run(&r.worktree("b"), &["reset", "-q", "--hard", "twr/a"]);

    let alone = ask(&hub, &["a"]).await;
    assert_eq!(alone["threads"][0]["branch_only_commits"], 0, "{alone}");
    let card = ask(&hub, &["a", "b"]).await;
    let threads = card["threads"].as_array().unwrap();
    for (t, other) in threads.iter().zip(["b", "a"]) {
        assert_eq!(t["branch_only_commits"], 1, "{card}");
        assert_eq!(
            t["shared"],
            json!([{"thread_name": other, "commits": 1}]),
            "{card}"
        );
    }
    let summary = card["summary"].as_str().unwrap();
    let lines: Vec<&str> = summary.lines().collect();
    assert!(
        lines[1].contains("moves its branch twr/a to the trash, with 1 commit on no other branch")
            && lines[1].contains("of its lost commits, shared with b: 1 commit (deleted too)"),
        "{summary}"
    );
    assert!(
        lines[2].contains("of its lost commits, shared with a: 1 commit (deleted too)"),
        "{summary}"
    );
    answer(&hub, &alone, "Cancel", &user()).unwrap();
    answer(&hub, &card, "Delete", &user()).unwrap();
    gone(&hub, &a).await;
    gone(&hub, &b).await;
    assert_eq!(deleted(&hub, &a).branch_only_commits, Some(1));
    hub.shutdown().await;
}

/// Two cards, one for each of two threads sharing an unpushed commit, each say
/// the other's branch keeps it. Confirmed together, their deletions run one after the other in
/// the repository: the second checks again once the first is done, finds the commit lost now,
/// deletes nothing and raises a new card that says so.
#[tokio::test]
async fn deletions_in_one_repository_run_one_after_another() {
    let r = Repo::new();
    let mut options = opts(&r.root);
    options.adapters = Arc::new(|name| match name {
        "fake" => Some(Arc::new(FakeAdapter::new()) as Arc<dyn Adapter>),
        "claude-code" => Some(Arc::new(twr_adapters::ClaudeCodeAdapter::with_bin(
            "/nonexistent/twr-test-claude",
        )) as Arc<dyn Adapter>),
        _ => None,
    });
    let hub = Hub::open(options).await.unwrap();
    let a = create(&hub, &r.repo, "a", false).await;
    let b = create(&hub, &r.repo, "b", false).await;
    let wt_a = r.worktree("a");
    std::fs::write(wt_a.join("a.txt"), "a").unwrap();
    commit_all(&wt_a, "unpushed");
    run(&r.worktree("b"), &["reset", "-q", "--hard", "twr/a"]);

    let card_a = hub
        .delete_threads(std::slice::from_ref(&a), &Caller::Orchestrator)
        .await
        .unwrap();
    let card_b = hub
        .delete_threads(std::slice::from_ref(&b), &Caller::Orchestrator)
        .await
        .unwrap();
    for card in [&card_a, &card_b] {
        assert_eq!(card["threads"][0]["branch_only_commits"], 0, "{card}");
    }
    answer(&hub, &card_a, "Delete", &user()).unwrap();
    answer(&hub, &card_b, "Delete", &user()).unwrap();
    // One goes; the other gets a new card (replacing its own) and stays.
    let fresh = timeout(WAIT, async {
        loop {
            let fresh = hub.attention_list(false).into_iter().find(|i| {
                i["thread"] == HUB_THREAD
                    && i["options"] == json!(["Delete", "Cancel"])
                    && i["attn_id"] != card_a["attn_id"]
                    && i["attn_id"] != card_b["attn_id"]
            });
            let left: Vec<&String> = [&a, &b]
                .into_iter()
                .filter(|id| hub.thread_view(id).is_ok())
                .collect();
            if let (Some(fresh), [_]) = (fresh, left.as_slice()) {
                return fresh;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("items: {:?}", hub.attention_list(true)));
    let (kept, kept_name, gone_id, confirmed) = if hub.thread_view(&a).is_ok() {
        (&a, "a", &b, &card_a)
    } else {
        (&b, "b", &a, &card_b)
    };
    assert!(hub.thread_view(gone_id).is_err());
    let summary = fresh["summary"].as_str().unwrap();
    for part in [
        format!(
            "Nothing was deleted: since card {} was confirmed, there is more to lose \
             ({kept_name}: commits on no branch that stays).",
            confirmed["attn_id"].as_str().unwrap()
        ),
        format!("moves its branch twr/{kept_name} to the trash, with 1 commit on no other branch"),
    ] {
        assert!(summary.contains(&part), "{part:?} missing from {summary:?}");
    }
    assert_eq!(
        card_ext(&hub, &fresh["attn_id"])["replaces"],
        confirmed["attn_id"]
    );
    assert!(branch_exists(&r.repo, &format!("twr/{kept_name}")));
    assert!(hub.thread_view(kept).unwrap().parked);
    assert_eq!(
        card_ext(&hub, &fresh["attn_id"])["asked_by"],
        user().source().to_string()
    );
    hub.reset_orchestrator(&user()).await.unwrap();
    assert!(
        hub.attention_list(true)
            .iter()
            .any(|i| i["attn_id"] == fresh["attn_id"]),
        "a user-confirmed reconfirmation survives reset"
    );
    hub.shutdown().await;
}

/// Threads `a` and `b` of one repository on a hub whose git holds every `git worktree
/// remove`: a commit only `a`'s branch has, which `b`'s worktree `HEAD` reflog also has (its
/// worktree was at it a moment, then back on its branch). The hub, `a`, `b`, the commit, and
/// the held git.
async fn shared_by_branch_and_reflog(
    r: &Repo,
) -> (Hub, String, String, String, twr_core::testkit::git::HeldGit) {
    let held = twr_core::testkit::git::HeldGit::install(&r.root.join("bin"));
    let mut o = opts(&r.root);
    o.git_bin = Some(held.bin.clone());
    let hub = Hub::open(o).await.unwrap();
    let a = create(&hub, &r.repo, "a", false).await;
    let b = create(&hub, &r.repo, "b", false).await;
    let wt_a = r.worktree("a");
    std::fs::write(wt_a.join("a.txt"), "a").unwrap();
    commit_all(&wt_a, "only on a");
    let c = run(&wt_a, &["rev-parse", "HEAD"]);
    let wt_b = r.worktree("b");
    run(&wt_b, &["checkout", "-q", "--detach", &c]);
    run(&wt_b, &["checkout", "-q", "twr/b"]);
    (hub, a, b, c, held)
}

/// Waits until `held` holds a `git worktree remove` (a park) or `git update-ref` (a move to
/// the trash).
async fn until_held(held: &twr_core::testkit::git::HeldGit) {
    timeout(WAIT, async {
        while !held.held() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("git never held");
}

/// A park removes worktrees of the repository a deletion holds only once that
/// deletion is done, and checks then: `a`'s card loses nothing (`b`'s reflog keeps the
/// commit), and once `a` is deleted the commit is only in `b`'s reflog, so `b`'s park keeps
/// its worktree. Without the hold, the park's checks (the commit still on `a`'s branch) and
/// the deletion ran together and the commit was lost.
#[tokio::test]
async fn a_park_waits_for_a_deletion_in_its_repository() {
    let r = Repo::new();
    let (hub, a, b, c, held) = shared_by_branch_and_reflog(&r).await;
    let card = ask(&hub, &["a"]).await;
    assert_eq!(card["threads"][0]["branch_only_commits"], 0, "{card}");
    answer(&hub, &card, "Delete", &user()).unwrap();
    // The deletion passed its check and moves `a` to the trash, holding the repository.
    until_held(&held).await;
    let park = {
        let (hub, b) = (hub.clone(), b.clone());
        tokio::spawn(async move { hub.park_thread(&b, false, false, &user()).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!park.is_finished(), "parked while the deletion was at work");
    held.release();
    gone(&hub, &a).await;
    let out = timeout(WAIT, park)
        .await
        .expect("the park never finished")
        .unwrap()
        .unwrap();
    assert!(out.parked && !out.worktree_removed, "{out:?}");
    let reason = out.kept_reason.clone().unwrap_or_default();
    assert!(
        reason.contains("commits only the worktree's own HEAD, reflogs or refs remember")
            && reason.contains(&c[..12]),
        "{out:?}"
    );
    assert!(!branch_exists(&r.repo, "twr/a"));
    let wt_b = r.worktree("b");
    assert!(run(&wt_b, &["log", "-g", "--format=%H", "HEAD"]).contains(&c));
    hub.shutdown().await;
}

/// The other order: a deletion confirmed while a park removes a worktree of its
/// repository checks again once the park is done. `b`'s park passes its checks (the commit is
/// on `a`'s branch) and removes `b`'s worktree with its reflog; `a`'s deletion, whose check
/// ran meanwhile, waits and checks again: the commit is lost now, so nothing is deleted and a
/// new card says so.
#[tokio::test]
async fn a_deletion_waits_for_a_park_in_its_repository_and_checks_again() {
    let r = Repo::new();
    let (hub, a, b, _c, held) = shared_by_branch_and_reflog(&r).await;
    let card = ask(&hub, &["a"]).await;
    assert_eq!(card["threads"][0]["branch_only_commits"], 0, "{card}");
    let park = {
        let (hub, b) = (hub.clone(), b.clone());
        tokio::spawn(async move { hub.park_thread(&b, false, false, &user()).await })
    };
    // The park passed its checks and removes `b`'s worktree, holding the repository.
    until_held(&held).await;
    answer(&hub, &card, "Delete", &user()).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(hub.thread_view(&a).is_ok());
    held.release();
    let out = timeout(WAIT, park)
        .await
        .expect("the park never finished")
        .unwrap()
        .unwrap();
    assert!(out.parked && out.worktree_removed, "{out:?}");
    let fresh = new_card(&hub, &card["attn_id"]).await;
    let summary = fresh["summary"].as_str().unwrap();
    for part in [
        format!(
            "Nothing was deleted: since card {} was confirmed, there is more to lose (a: \
             commits on no branch that stays).",
            card["attn_id"].as_str().unwrap()
        ),
        "moves its branch twr/a to the trash, with 1 commit on no other branch".to_string(),
    ] {
        assert!(summary.contains(&part), "{part:?} missing from {summary:?}");
    }
    assert!(branch_exists(&r.repo, "twr/a"));
    assert!(hub.thread_view(&a).unwrap().parked);
    hub.shutdown().await;
}

/// A worktree whose directory is gone still has its record, which the deletion
/// clears with every repository in it: a submodule's repository there, and a bare one put
/// there, are on the card with their commits on no remote, and a commit more in one after
/// the card brings a new card.
#[tokio::test]
async fn a_gone_worktree_directory_still_counts_the_repositories_of_its_record() {
    let r = Repo::new();
    let lib = init_repo(&r.root.join("lib"));
    git_in(
        &r.repo,
        &["submodule", "add", "-q", lib.to_str().unwrap(), "libs/x"],
    );
    commit_all(&r.repo, "a submodule");
    let hub = Hub::open(opts(&r.root)).await.unwrap();
    let id = create(&hub, &r.repo, "api", false).await;
    let wt = r.worktree("api");
    git_in(&wt, &["submodule", "update", "--init", "-q"]);
    let x = wt.join("libs/x");
    git_in(&x, &["checkout", "-q", "-b", "mine"]);
    git_in(&x, &["commit", "-q", "--allow-empty", "-m", "secret"]);
    let admin = PathBuf::from(run(&wt, &["rev-parse", "--absolute-git-dir"]));
    let vault = admin.join("vault.git");
    run(
        &r.root,
        &[
            "clone",
            "-q",
            "--bare",
            lib.to_str().unwrap(),
            vault.to_str().unwrap(),
        ],
    );
    std::fs::remove_dir_all(&wt).unwrap();

    let card = ask(&hub, &["api"]).await;
    let t = &card["threads"][0];
    let module = admin.join("modules/libs/x");
    assert_eq!(t["worktree_missing"], true, "{card}");
    assert_eq!(
        t["repositories"],
        json!([
            {"path": module.to_str().unwrap(), "commits": 1},
            {"path": vault.to_str().unwrap(), "commits": 1},
        ]),
        "{card}"
    );
    let summary = card["summary"].as_str().unwrap();
    assert!(
        summary.contains(&format!(
            "moves the record of its worktree {} (already gone) to the trash, with it the \
             repository of its submodule {} with 1 commit on none of its remote-tracking \
             branches (not pushed)",
            wt.display(),
            module.display()
        )),
        "{summary}"
    );
    // One more commit in the record's repository after the card (its work tree is gone): a
    // new card.
    let (git_dir, work_tree) = (
        format!("--git-dir={}", module.display()),
        format!("--work-tree={}", module.display()),
    );
    let in_module = |args: &[&str]| {
        let mut all = vec![git_dir.as_str(), work_tree.as_str()];
        all.extend_from_slice(args);
        git_in(&r.root, &all)
    };
    let more = in_module(&["commit-tree", "HEAD^{tree}", "-p", "HEAD", "-m", "more"]);
    in_module(&["update-ref", "refs/heads/mine", &more]);
    answer(&hub, &card, "Delete", &user()).unwrap();
    let fresh = new_card(&hub, &card["attn_id"]).await;
    let summary = fresh["summary"].as_str().unwrap();
    assert!(
        summary.contains("there is more to lose (api: commits of repositories in the worktree)")
            && summary.contains("with 2 commits on none of its remote-tracking branches"),
        "{summary}"
    );
    assert!(module.join("HEAD").is_file());
    answer(&hub, &fresh, "Delete", &user()).unwrap();
    gone(&hub, &id).await;
    assert!(!admin.exists());
    hub.shutdown().await;
}

/// A bare repository the worktree holds goes with it: an ignored one (deep in an
/// ignored directory) and an untracked one are on the card with their commits on no remote.
#[tokio::test]
async fn bare_repositories_in_the_worktree_are_on_the_card() {
    let r = Repo::new();
    std::fs::write(r.repo.join(".gitignore"), "vendor/\n").unwrap();
    commit_all(&r.repo, "ignores");
    let src = init_repo(&r.root.join("src"));
    let hub = Hub::open(opts(&r.root)).await.unwrap();
    let id = create(&hub, &r.repo, "api", false).await;
    let wt = r.worktree("api");
    std::fs::create_dir_all(wt.join("vendor/old")).unwrap();
    for to in ["vendor/old/backup.git", "spare.git"] {
        run(&wt, &["clone", "-q", "--bare", src.to_str().unwrap(), to]);
    }

    let card = ask(&hub, &["api"]).await;
    assert_eq!(
        card["threads"][0]["repositories"],
        json!([
            {"path": "spare.git", "nested": true, "commits": 1},
            {"path": "vendor/old/backup.git", "nested": true, "commits": 1},
        ]),
        "{card}"
    );
    let summary = card["summary"].as_str().unwrap();
    assert!(
        summary.contains(
            "with it the git repository nested at vendor/old/backup.git with 1 commit on none \
             of its remote-tracking branches (not pushed)"
        ),
        "{summary}"
    );
    answer(&hub, &card, "Delete", &user()).unwrap();
    gone(&hub, &id).await;
    assert!(!wt.exists());
    hub.shutdown().await;
}

/// A worktree whose directory is gone still has its record, which the deletion clears: a
/// commit only its `HEAD` reflog keeps (made on a detached `HEAD`, then the branch checked out
/// again) is counted on the card and by the check after Confirm.
#[tokio::test]
async fn a_gone_worktree_directory_still_counts_its_head_reflog() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root)).await.unwrap();
    let id = create(&hub, &r.repo, "lost", false).await;
    let wt = r.worktree("lost");
    run(&wt, &["checkout", "-q", "--detach"]);
    std::fs::write(wt.join("x.txt"), "x").unwrap();
    commit_all(&wt, "detached");
    run(&wt, &["checkout", "-q", "twr/lost"]);
    std::fs::remove_dir_all(&wt).unwrap();

    let card = ask(&hub, &["lost"]).await;
    let t = &card["threads"][0];
    assert_eq!(
        (
            &t["worktree_missing"],
            &t["head_only_commits"],
            &t["branch_only_commits"]
        ),
        (&json!(true), &json!(1), &json!(0)),
        "{card}"
    );
    let summary = card["summary"].as_str().unwrap();
    assert!(
        summary.contains("moves 1 commit made on a detached HEAD in its worktree to the trash"),
        "{summary}"
    );
    answer(&hub, &card, "Delete", &user()).unwrap();
    gone(&hub, &id).await;
    assert_eq!(deleted(&hub, &id).head_only_commits, Some(1));
    assert!(!branch_exists(&r.repo, "twr/lost"));
    hub.shutdown().await;
}

/// Threads created through two checkouts of one repository (the project opened through its
/// main checkout and through a linked one) are one deletion set: an unpushed commit only
/// their two branches have is lost and shared.
#[tokio::test]
async fn threads_of_two_checkouts_of_one_repository_are_one_set() {
    let r = Repo::new();
    let alt = r.root.join("alt");
    run(
        &r.repo,
        &["worktree", "add", "-q", "-b", "alt", alt.to_str().unwrap()],
    );
    let hub = Hub::open(opts(&r.root)).await.unwrap();
    let a = create(&hub, &r.repo, "a", false).await;
    let b = create(&hub, &alt, "b", false).await;
    let wt = |id: &str| PathBuf::from(hub.thread_view(id).unwrap().worktree.unwrap());
    std::fs::write(wt(&a).join("a.txt"), "a").unwrap();
    commit_all(&wt(&a), "unpushed");
    run(&wt(&b), &["reset", "-q", "--hard", "twr/a"]);

    let card = ask(&hub, &["a", "b"]).await;
    let threads = card["threads"].as_array().unwrap();
    for (t, other) in threads.iter().zip(["b", "a"]) {
        assert_eq!(t["branch_only_commits"], 1, "{card}");
        assert_eq!(
            t["shared"],
            json!([{"thread_name": other, "commits": 1}]),
            "{card}"
        );
    }
    answer(&hub, &card, "Delete", &user()).unwrap();
    gone(&hub, &a).await;
    gone(&hub, &b).await;
    assert_eq!(deleted(&hub, &b).branch_only_commits, Some(1));
    hub.shutdown().await;
}

/// `git branch -D` deletes the branch's reflog too: a commit only that reflog keeps (the
/// thread was parked, then its branch moved back) is lost, and the card says so; a reflog
/// that cannot be read leaves the commits not checked, never "nothing lost".
#[tokio::test]
async fn a_commit_only_the_branch_reflog_keeps_is_counted() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root)).await.unwrap();
    let id = create(&hub, &r.repo, "x", false).await;
    let wt = r.worktree("x");
    std::fs::write(wt.join("x.txt"), "x").unwrap();
    commit_all(&wt, "unpushed");
    let parked = hub.park_thread(&id, false, false, &user()).await.unwrap();
    assert!(parked.worktree_removed, "{parked:?}");
    run(&r.repo, &["branch", "-f", "twr/x", "twr/x~1"]);

    let card = ask(&hub, &["x"]).await;
    assert_eq!(card["threads"][0]["branch_only_commits"], 1, "{card}");
    let summary = card["summary"].as_str().unwrap();
    assert!(
        summary.contains("moves its branch twr/x to the trash, with 1 commit on no other branch"),
        "{summary}"
    );
    answer(&hub, &card, "Cancel", &user()).unwrap();

    let log = r.repo.join(".git/logs/refs/heads/twr/x");
    std::fs::write(&log, "garbage\n").unwrap();
    let card = ask(&hub, &["x"]).await;
    let t = &card["threads"][0];
    assert!(t.get("branch_only_commits").is_none(), "{card}");
    let summary = card["summary"].as_str().unwrap();
    assert!(
        summary.contains("(Not checked: unpushed commits (the reflog")
            && !summary.contains("each of its commits"),
        "{summary}"
    );
    hub.shutdown().await;
}

/// Runs git in `dir` with a committer, no hooks and local file transport allowed (a submodule
/// cloned by `git submodule` has no config of its own).
fn git_in(dir: &Path, args: &[&str]) -> String {
    let mut all = vec![
        "-c",
        "user.name=twr test",
        "-c",
        "user.email=twr-test@example.invalid",
        "-c",
        "commit.gpgsign=false",
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "protocol.file.allow=always",
    ];
    all.extend_from_slice(args);
    run(dir, &all)
}

/// A submodule's repository lives under the worktree's own git directory and goes with it: a
/// commit on a local branch of it (the superproject clean, the gitlink checked out back) is on
/// the card. One more after the card brings a new card and nothing is deleted; confirming that
/// one deletes the thread, the submodule's repository with it.
#[tokio::test]
async fn a_submodule_repository_goes_with_the_worktree_and_is_on_the_card() {
    let r = Repo::new();
    let lib = init_repo(&r.root.join("lib"));
    git_in(
        &r.repo,
        &["submodule", "add", "-q", lib.to_str().unwrap(), "libs/x"],
    );
    commit_all(&r.repo, "a submodule");
    let hub = Hub::open(opts(&r.root)).await.unwrap();
    let id = create(&hub, &r.repo, "api", false).await;
    let wt = r.worktree("api");
    git_in(&wt, &["submodule", "update", "--init", "-q"]);
    let x = wt.join("libs/x");
    let admin = PathBuf::from(run(&wt, &["rev-parse", "--absolute-git-dir"]));
    assert!(admin.join("modules/libs/x/HEAD").is_file());
    git_in(&x, &["checkout", "-q", "-b", "mine"]);
    git_in(&x, &["commit", "-q", "--allow-empty", "-m", "secret"]);
    git_in(&x, &["checkout", "-q", "--detach", "HEAD~1"]);
    assert_eq!(run(&wt, &["status", "--porcelain"]), "");

    let card = ask(&hub, &["api"]).await;
    let t = &card["threads"][0];
    assert_eq!(
        t["repositories"],
        json!([{"path": "libs/x", "commits": 1}]),
        "{card}"
    );
    let summary = card["summary"].as_str().unwrap();
    assert!(
        summary.contains(
            "with it the repository of its submodule libs/x with 1 commit on none of its \
             remote-tracking branches (not pushed)"
        ),
        "{summary}"
    );
    assert!(summary.contains("(no uncommitted changes)"), "{summary}");

    git_in(&x, &["commit", "-q", "--allow-empty", "-m", "more"]);
    git_in(&x, &["checkout", "-q", "--detach", "HEAD~1"]);
    answer(&hub, &card, "Delete", &user()).unwrap();
    let fresh = new_card(&hub, &card["attn_id"]).await;
    let summary = fresh["summary"].as_str().unwrap();
    assert!(
        summary.contains("there is more to lose (api: commits of repositories in the worktree)"),
        "{summary}"
    );
    assert!(
        summary.contains("with it the repository of its submodule libs/x with 2 commits"),
        "{summary}"
    );
    assert!(admin.join("modules/libs/x/HEAD").is_file() && wt.exists());

    answer(&hub, &fresh, "Delete", &user()).unwrap();
    gone(&hub, &id).await;
    assert!(!wt.exists() && !admin.exists());
    hub.shutdown().await;
}

/// File names that are not UTF-8 are told apart: a second one (and one with a literal
/// backslash spelled like an escaped byte) appearing after the card is new, so Confirm brings
/// a new card and deletes nothing.
#[tokio::test]
async fn names_that_are_not_utf8_are_told_apart() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root)).await.unwrap();
    let id = create(&hub, &r.repo, "api", false).await;
    let wt = r.worktree("api");
    let name = |b: &[u8]| wt.join(OsStr::from_bytes(b));
    if let Err(e) = std::fs::write(name(b"notes-\x80"), "a") {
        eprintln!("skipped: this file system refuses non-UTF-8 names ({e})");
        hub.shutdown().await;
        return;
    }
    let card = ask(&hub, &["api"]).await;
    assert_eq!(card["threads"][0]["dirty_files"], 1, "{card}");
    let said = &card_ext(&hub, &card["attn_id"])["losses"][0]["loss"]["inventory"]["dirty"];
    assert_eq!(said["items"], json!(["notes-\\x80"]), "{said}");

    let mut last = card;
    for (raw, files) in [(b"notes-\x81".as_slice(), 2), (b"notes-\\x80", 3)] {
        std::fs::write(name(raw), "b").unwrap();
        answer(&hub, &last, "Delete", &user()).unwrap();
        let fresh = new_card(&hub, &last["attn_id"]).await;
        let summary = fresh["summary"].as_str().unwrap();
        assert!(
            summary.contains("there is more to lose (api: uncommitted files)")
                && summary.contains(&format!("with {files} uncommitted files it has")),
            "{summary}"
        );
        assert!(name(b"notes-\x80").exists() && name(raw).exists());
        last = fresh;
    }
    answer(&hub, &last, "Delete", &user()).unwrap();
    gone(&hub, &id).await;
    assert!(!wt.exists());
    hub.shutdown().await;
}

/// An untracked repository nested in the worktree goes with it: its commits (on no remote) are
/// on the card, and one more after the card brings a new card.
#[tokio::test]
async fn a_nested_repository_in_the_worktree_is_on_the_card() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root)).await.unwrap();
    let id = create(&hub, &r.repo, "api", false).await;
    let wt = r.worktree("api");
    let tools = init_repo(&wt.join("tools/x"));

    let card = ask(&hub, &["api"]).await;
    let t = &card["threads"][0];
    assert_eq!(
        t["repositories"],
        json!([{"path": "tools/x", "nested": true, "commits": 1}]),
        "{card}"
    );
    let summary = card["summary"].as_str().unwrap();
    assert!(
        summary.contains(
            "with it the git repository nested at tools/x with 1 commit on none of its \
             remote-tracking branches (not pushed)"
        ),
        "{summary}"
    );
    git_in(&tools, &["commit", "-q", "--allow-empty", "-m", "more"]);
    answer(&hub, &card, "Delete", &user()).unwrap();
    let fresh = new_card(&hub, &card["attn_id"]).await;
    let summary = fresh["summary"].as_str().unwrap();
    assert!(
        summary.contains("there is more to lose (api: commits of repositories in the worktree)")
            && summary.contains("nested at tools/x with 2 commits"),
        "{summary}"
    );
    assert!(tools.exists());
    answer(&hub, &fresh, "Delete", &user()).unwrap();
    gone(&hub, &id).await;
    assert!(!wt.exists());
    hub.shutdown().await;
}

/// A linked worktree of another repository checked out inside the worktree
/// refuses the deletion: the card says so (with what to do), `Delete` moves nothing (an error
/// item says why; the thread stays, parked) and the other repository's worktree list stays
/// valid. Once it is moved away with git, the deletion goes through.
#[tokio::test]
async fn a_linked_worktree_of_another_repository_refuses_the_delete() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root)).await.unwrap();
    let id = create(&hub, &r.repo, "api", false).await;
    let wt = r.worktree("api");
    let other = init_repo(&r.root.join("other"));
    let side = wt.join("tools/side");
    std::fs::create_dir_all(wt.join("tools")).unwrap();
    run(
        &other,
        &["worktree", "add", "-q", "--detach", &side.to_string_lossy()],
    );
    std::fs::write(side.join("side.txt"), "side").unwrap();
    commit_all(&side, "side");
    let listed = |repo: &Path| run(repo, &["worktree", "list", "--porcelain"]);
    let before = listed(&other);

    let card = ask(&hub, &["api"]).await;
    let t = &card["threads"][0];
    assert_eq!(
        t["linked"][0]["repo"],
        json!(other.display().to_string()),
        "{card}"
    );
    let summary = card["summary"].as_str().unwrap();
    assert!(
        summary.contains(&format!(
            "It cannot be deleted now: api's worktree contains a linked worktree of another \
             repository at {}",
            side.display()
        )) && summary.contains("worktree move"),
        "{summary}"
    );
    answer(&hub, &card, "Delete", &user()).unwrap();
    let failure = timeout(WAIT, async {
        loop {
            let items = hub.attention_list(true);
            if let Some(i) = items.iter().find(|i| i["thread"] == json!(id)) {
                return i.clone();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("an error item on api");
    let summary = failure["summary"].as_str().unwrap();
    assert!(
        summary.starts_with(
            "Deleting api failed: api's worktree contains a linked worktree of another repository"
        ),
        "{summary}"
    );
    assert!(hub.thread_view(&id).unwrap().parked);
    assert!(side.join("side.txt").exists() && branch_exists(&r.repo, "twr/api"));
    assert_eq!(listed(&other), before);
    let trash = hub.trash_list().await.unwrap();
    assert!(trash["items"].as_array().unwrap().is_empty(), "{trash}");

    // Moved away with git: the deletion goes through.
    let away = r.root.join("side-moved");
    run(
        &other,
        &[
            "worktree",
            "move",
            &side.to_string_lossy(),
            &away.to_string_lossy(),
        ],
    );
    let card = ask(&hub, &["api"]).await;
    assert!(card["threads"][0].get("linked").is_none(), "{card}");
    answer(&hub, &card, "Delete", &user()).unwrap();
    gone(&hub, &id).await;
    assert!(!wt.exists());
    run(&other, &["worktree", "prune"]);
    assert!(listed(&other).contains(&away.display().to_string()));
    assert!(away.join("side.txt").exists());
    hub.shutdown().await;
}

/// A card deletes the threads it names only in the lives it was raised for: `a`, deleted by
/// another card and restored as `restored` (a new branch and worktree), is not deleted by
/// confirming the card from before, and nothing is stopped. The answer says what changed, and
/// a new card replacing it asks for `b` alone; confirming that one deletes `b`, and
/// `restored` with its worktree and branch stays.
#[tokio::test]
async fn a_card_from_before_a_restore_does_not_delete_the_restored_thread() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root)).await.unwrap();
    let a = create(&hub, &r.repo, "a", false).await;
    let b = create(&hub, &r.repo, "b", false).await;
    let ab = ask(&hub, &["a", "b"]).await;
    let only_a = ask(&hub, &["a"]).await;
    answer(&hub, &only_a, "Delete", &user()).unwrap();
    gone(&hub, &a).await;
    assert!(open(&hub, &ab["attn_id"]), "b is still there");
    let item = deleted(&hub, &a).trash.expect("a trash item");
    let back = hub
        .trash_restore(&item, Some("restored"), &user())
        .await
        .unwrap();
    let worktree = PathBuf::from(back["worktree"].as_str().unwrap());
    let branch = back["branch"].as_str().unwrap().to_string();
    assert_eq!(hub.thread_view(&a).unwrap().name, "restored");
    assert!(worktree.join(".git").exists());

    let ev = answer(&hub, &ab, "Delete", &user()).unwrap();
    let EventBody::AttentionResolved(resolved) = &ev.body else {
        panic!("{ev:?}")
    };
    assert_eq!(
        json!(resolved.ext)["twr"]["changed"],
        json!(["a was deleted and restored since (now restored)"]),
        "{resolved:?}"
    );
    let fresh = new_card(&hub, &ab["attn_id"]).await;
    let ext = card_ext(&hub, &fresh["attn_id"]);
    assert_eq!(ext["replaces"], ab["attn_id"]);
    assert_eq!(ext["threads"].as_array().unwrap().len(), 1, "{ext}");
    assert_eq!(ext["threads"][0]["thread"], json!(b));
    let summary = fresh["summary"].as_str().unwrap();
    assert!(
        summary.starts_with(&format!(
            "Nothing was deleted: since card {} was raised, a was deleted and restored since \
             (now restored). This card leaves it out (ask again to delete it) and asks for the \
             other thread of that card.",
            ab["attn_id"].as_str().unwrap()
        )),
        "{summary}"
    );
    assert!(!open(&hub, &ab["attn_id"]));
    // Nothing was stopped or taken.
    assert!(!hub.thread_view(&b).unwrap().parked);
    assert!(worktree.join(".git").exists());
    assert!(branch_exists(&r.repo, &branch));

    answer(&hub, &fresh, "Delete", &user()).unwrap();
    gone(&hub, &b).await;
    let v = hub.thread_view(&a).expect("restored stays");
    assert_eq!(v.name, "restored");
    assert!(worktree.join(".git").exists());
    assert!(branch_exists(&r.repo, &branch));
    assert!(hub.attention_list(true).is_empty(), "no failure item");
    hub.shutdown().await;
}

/// Asking again for the threads of an open card one of which was deleted and restored since
/// raises a new card replacing it (the open one is not returned); a card left naming only a
/// restored thread and deleted ones closes when the last of those is deleted.
#[tokio::test]
async fn a_restore_makes_the_cards_from_before_stale() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root)).await.unwrap();
    let a = create(&hub, &r.repo, "a", false).await;
    let b = create(&hub, &r.repo, "b", false).await;
    let c = create(&hub, &r.repo, "c", false).await;
    let ab = ask(&hub, &["a", "b"]).await;
    let ac = ask(&hub, &["a", "c"]).await;
    let only_a = ask(&hub, &["a"]).await;
    let only_c = ask(&hub, &["c"]).await;
    answer(&hub, &only_a, "Delete", &user()).unwrap();
    gone(&hub, &a).await;
    let item = deleted(&hub, &a).trash.expect("a trash item");
    hub.trash_restore(&item, Some("a2"), &user()).await.unwrap();

    let again = ask(&hub, &[a.as_str(), b.as_str()]).await;
    assert_ne!(again["attn_id"], ab["attn_id"]);
    assert_ne!(again["existing"], json!(true));
    assert_eq!(again["replaces"], ab["attn_id"]);
    let summary = again["summary"].as_str().unwrap();
    assert!(
        summary.starts_with(&format!(
            "This card replaces {}: since it was raised, a was deleted and restored since \
             (now a2).",
            ab["attn_id"].as_str().unwrap()
        )),
        "{summary}"
    );
    assert!(!open(&hub, &ab["attn_id"]));
    // The new card is for a2's life: asked again, it is returned.
    let same = ask(&hub, &[a.as_str(), b.as_str()]).await;
    assert_eq!(
        (&same["attn_id"], &same["existing"]),
        (&again["attn_id"], &json!(true))
    );

    assert!(open(&hub, &ac["attn_id"]));
    answer(&hub, &only_c, "Delete", &user()).unwrap();
    gone(&hub, &c).await;
    timeout(WAIT, async {
        while open(&hub, &ac["attn_id"]) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the card of a and c closes");
    assert_eq!(hub.thread_view(&a).unwrap().name, "a2");
    hub.shutdown().await;
}

/// A thread that switched its own worktree to new branches (as one integrating its own work
/// does) is its own all the same: a park keeps the worktree and says which branch it is on,
/// and the delete card names that branch with its commits on no other branch, says it stays,
/// and counts the uncommitted file. Delete moves the worktree and the thread's recorded branch
/// to the trash and leaves the branches it switched to; a restore brings the worktree back on
/// the branch it was on, its uncommitted file with it. A worktree on another thread's recorded
/// branch is still refused, and nothing of it is touched.
#[tokio::test]
async fn a_thread_whose_worktree_switched_branches_is_deleted_and_restored() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root)).await.unwrap();
    let id = create(&hub, &r.repo, "sw", false).await;
    let wt = r.worktree("sw");
    std::fs::write(wt.join("a.txt"), "a").unwrap();
    commit_all(&wt, "on its recorded branch");
    run(&wt, &["checkout", "-q", "-b", "twr/sw-step"]);
    std::fs::write(wt.join("b.txt"), "b").unwrap();
    commit_all(&wt, "on the first branch it switched to");
    run(&wt, &["checkout", "-q", "-b", "twr/sw-final"]);
    std::fs::write(wt.join("c.txt"), "c").unwrap();
    commit_all(&wt, "only on the branch it is on");
    let tip = run(&wt, &["rev-parse", "HEAD"]);

    let park = hub.park_thread("sw", false, false, &user()).await.unwrap();
    let kept = park.kept_reason.unwrap_or_default();
    assert!(
        !park.worktree_removed && kept.contains("branch twr/sw-final"),
        "{kept}"
    );
    assert!(!kept.contains("another"), "{kept}");
    std::fs::write(wt.join("wip.txt"), "wip").unwrap();

    let card = ask(&hub, &["sw"]).await;
    let t = &card["threads"][0];
    assert_eq!(
        (
            &t["checkout"],
            &t["checkout_only_commits"],
            &t["branch_only_commits"],
            &t["dirty_files"]
        ),
        (&json!("twr/sw-final"), &json!(1), &json!(0), &json!(1)),
        "{card}"
    );
    let summary = card["summary"].as_str().unwrap();
    for part in [
        "with 1 uncommitted file it has",
        "moves its branch twr/sw to the trash (each of its commits is on another branch",
        "leaves the branch twr/sw-final its worktree is on in the repository, with 1 commit on \
         no other branch, tag or remote (not pushed)",
    ] {
        assert!(summary.contains(part), "{part:?} missing from {summary:?}");
    }
    assert!(!summary.contains("Not checked"), "{summary}");
    answer(&hub, &card, "Delete", &user()).unwrap();
    gone(&hub, &id).await;
    assert!(!wt.exists() && !branch_exists(&r.repo, "twr/sw"));
    assert_eq!(run(&r.repo, &["rev-parse", "twr/sw-final"]), tip);
    assert!(branch_exists(&r.repo, "twr/sw-step"));

    let item = deleted(&hub, &id).trash.unwrap();
    let out = hub.trash_restore(&item, None, &user()).await.unwrap();
    assert_eq!(
        (&out["branch"], &out["checkout"]),
        (&json!("twr/sw"), &json!("twr/sw-final")),
        "{out}"
    );
    let message = out["message"].as_str().unwrap();
    assert!(
        message.contains("on branch twr/sw-final as it was when it was deleted (its own branch twr/sw is back too)"),
        "{message}"
    );
    assert_eq!(
        run(&wt, &["symbolic-ref", "HEAD"]),
        "refs/heads/twr/sw-final"
    );
    assert_eq!(run(&wt, &["rev-parse", "HEAD"]), tip);
    assert_eq!(run(&wt, &["status", "--porcelain"]), "?? wip.txt");
    assert!(branch_exists(&r.repo, "twr/sw"));

    // Another thread's recorded branch checked out in a thread's worktree (here that of `other`,
    // parked, its worktree removed): refused, nothing touched.
    let other = create(&hub, &r.repo, "other", false).await;
    let park = hub
        .park_thread("other", false, false, &user())
        .await
        .unwrap();
    assert!(park.worktree_removed, "{park:?}");
    let f = create(&hub, &r.repo, "f", false).await;
    let wt_f = r.worktree("f");
    run(&wt_f, &["checkout", "-q", "twr/other"]);
    let card = ask(&hub, &["f"]).await;
    let summary = card["summary"].as_str().unwrap();
    assert!(
        summary.contains("another thread's checkout") && summary.contains("twr/other"),
        "{summary}"
    );
    answer(&hub, &card, "Delete", &user()).unwrap();
    let failure = timeout(WAIT, async {
        loop {
            let items = hub.attention_list(true);
            if let Some(i) = items.iter().find(|i| i["thread"] == json!(f)) {
                return i.clone();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("an error item on f");
    let why = failure["summary"].as_str().unwrap();
    assert!(why.contains("another thread's checkout"), "{why}");
    assert!(wt_f.join(".git").exists() && branch_exists(&r.repo, "twr/f"));
    assert!(branch_exists(&r.repo, "twr/other") && hub.thread_view(&other).is_ok());
    hub.shutdown().await;
}

/// A restore takes the other threads' branches as they are once it holds the repository: a
/// thread recording the branch a deleted thread's worktree was switched to, registered while the
/// restore waited for the repository (held by a park), keeps the restored worktree off it.
#[tokio::test]
async fn a_restore_waiting_for_its_repository_sees_threads_made_meanwhile() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root)).await.unwrap();
    let id = create(&hub, &r.repo, "sw", false).await;
    create(&hub, &r.repo, "x", false).await;
    let wt = r.worktree("sw");
    run(&wt, &["checkout", "-q", "-b", "twr/sw-final"]);
    std::fs::write(wt.join("c.txt"), "c").unwrap();
    commit_all(&wt, "on the branch it switched to");
    let card = ask(&hub, &["sw"]).await;
    answer(&hub, &card, "Delete", &user()).unwrap();
    gone(&hub, &id).await;
    let item = deleted(&hub, &id).trash.unwrap();
    hub.shutdown().await;
    drop(hub);

    let held = twr_core::testkit::git::HeldGit::install(&r.root.join("bin"));
    let mut o = opts(&r.root);
    o.git_bin = Some(held.bin.clone());
    let hub = Hub::open(o).await.unwrap();
    // x's park holds the repository at its `git worktree remove`.
    let park = {
        let hub = hub.clone();
        tokio::spawn(async move { hub.park_thread("x", false, false, &user()).await })
    };
    until_held(&held).await;
    let restore = {
        let (hub, item) = (hub.clone(), item.clone());
        tokio::spawn(async move { hub.trash_restore(&item, None, &user()).await })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !restore.is_finished(),
        "restored while the park held the repository"
    );
    // Meanwhile a thread recording twr/sw-final comes (parked: its worktree is gone).
    hub.append(
        "adopter",
        None,
        Source::User("test".into()),
        EventBody::ThreadCreated(ThreadCreated {
            name: "adopter".into(),
            project: Some("proj".into()),
            repo: Some(r.repo.to_string_lossy().into_owned()),
            worktree: Some(r.worktree("adopter").to_string_lossy().into_owned()),
            box_name: "local".into(),
            runtime: "fake".into(),
            mode: "manual".into(),
            ext: json!({"twr": {"worktree": "twr", "branch": "twr/sw-final"}})
                .as_object()
                .cloned(),
            ..Default::default()
        }),
        None,
    )
    .unwrap();
    held.release();
    timeout(WAIT, park).await.unwrap().unwrap().unwrap();
    let out = timeout(WAIT, restore).await.unwrap().unwrap().unwrap();
    assert!(out.get("checkout").is_none_or(Value::is_null), "{out}");
    let notes = out["notes"].to_string();
    assert!(notes.contains("another thread's now"), "{notes}");
    assert_eq!(run(&wt, &["symbolic-ref", "HEAD"]), "refs/heads/twr/sw");
    hub.shutdown().await;
}
