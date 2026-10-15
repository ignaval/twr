//! The trash (`hub::trash`) with real temporary git repositories: deleting a thread moves its
//! worktree, record and branch to the trash (every commit kept, its name free at once); the
//! trash lists what it holds with sizes and losses; only the user empties it or restores from
//! it; a restore brings the thread back with its history, under another name when its own was
//! taken, copying from the item, which stays (listed as restored) until it is purged; items are
//! purged after `[trash] keep_days`, and nothing else is.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::time::timeout;
use twr_adapters::fake::FakeAdapter;
use twr_core::adapter::{Adapter, Answer};
use twr_core::config::Config;
use twr_core::event::*;
use twr_core::proto::codes;
use twr_core::testkit::git::{branch_exists, commit_all, init_repo, run, worktrees};
use twr_hub::{Caller, Hub, HubOptions, NewThread};

const WAIT: Duration = Duration::from_secs(20);

const DAY_MS: u64 = 86_400_000;

fn opts(dir: &Path, keep_days: u64) -> HubOptions {
    let mut config = Config::default();
    config.trash.keep_days = keep_days;
    let mut o = HubOptions::new(dir.join("data").join("twr.db"), config);
    o.adapters = Arc::new(|name: &str| -> Option<Arc<dyn Adapter>> {
        (name == "fake").then(|| Arc::new(FakeAdapter::new()) as Arc<dyn Adapter>)
    });
    o.git_tick = Some(Duration::ZERO);
    o.stall_after = Some(Duration::ZERO);
    o.trash_tick = Some(Duration::ZERO);
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

    fn trash(&self) -> PathBuf {
        self.root.join("data").join("trash")
    }
}

async fn create(hub: &Hub, project: &Path, name: &str) -> String {
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

/// Asks to delete `name` and confirms the card as the user; waits until it is gone.
async fn delete(hub: &Hub, name: &str) {
    let id = hub.thread_view(name).unwrap().id;
    let card = hub
        .delete_threads(&[name.to_string()], &user())
        .await
        .unwrap();
    hub.respond_at(
        card["attn_id"].as_str().unwrap(),
        Answer::new("Delete"),
        card["revision"].as_u64(),
        &user(),
    )
    .unwrap();
    timeout(WAIT, async {
        while hub.thread_view(&id).is_ok() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{name} was not deleted: {:?}", hub.attention_list(true)));
}

/// Every control event so far.
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

/// The items of `trash.list`.
async fn items(hub: &Hub) -> Vec<Value> {
    hub.trash_list().await.unwrap()["items"]
        .as_array()
        .unwrap()
        .clone()
}

/// Waits until thread `id`'s latest turn ended.
async fn turn_done(hub: &Hub, id: &str) {
    timeout(WAIT, async {
        loop {
            let v = hub.thread_view(id).unwrap();
            if v.last_turn.is_some() && v.current_turn.is_none() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the turn ended");
}

/// Deleting a thread moves its worktree (with what it holds), record and branch to the trash:
/// `thread.deleted` names the item, the trash lists it with its size and what the card said it
/// loses, every commit stays in the repository, and the name, path and branch are free at once.
#[tokio::test]
async fn delete_moves_the_thread_to_the_trash_and_frees_its_name() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root, 7)).await.unwrap();
    let id = create(&hub, &r.repo, "api").await;
    let wt = r.worktree("api");
    std::fs::write(wt.join("done.txt"), "done").unwrap();
    commit_all(&wt, "work");
    let commit = run(&wt, &["rev-parse", "HEAD"]);
    std::fs::write(wt.join("wip.txt"), "wip").unwrap();
    std::fs::write(wt.join(".gitignore"), "target/\n*.local\n").unwrap();
    std::fs::write(wt.join("big.local"), vec![1u8; 32 << 10]).unwrap();
    std::fs::create_dir_all(wt.join("target")).unwrap();
    std::fs::write(wt.join("target").join("out.bin"), "o").unwrap();

    let card = hub.delete_threads(&["api".into()], &user()).await.unwrap();
    let summary = card["summary"].as_str().unwrap();
    assert!(summary.contains("to the trash"), "{summary}");
    assert!(
        summary.contains("The trash keeps it for 7 days"),
        "{summary}"
    );
    assert!(card["threads"][0]["worktree"].is_string());
    delete(&hub, "api").await;

    let d = deleted(&hub, &id);
    let item = d.trash.clone().expect("a trash item");
    assert_eq!(d.worktree.as_deref(), Some(wt.to_str().unwrap()));
    assert_eq!(d.branch.as_deref(), Some("twr/api"));
    assert!(d.trash_bytes.unwrap() >= 32 << 10, "{d:?}");
    // Build output is deleted only on Linux and macOS; elsewhere it is ignored files like the
    // others.
    let deletes = cfg!(any(target_os = "linux", target_os = "macos"));
    let ignored = if deletes {
        "big.local"
    } else {
        "big.local, target/"
    };
    assert_eq!(
        d.losses.as_deref(),
        Some(format!("2 uncommitted files, ignored files ({ignored}), 1 unpushed commit").as_str())
    );
    let dir = r.trash().join(&item);
    assert_eq!(d.trash_dir.as_deref(), Some(dir.to_str().unwrap()));
    for f in ["done.txt", "wip.txt", "big.local"] {
        assert!(dir.join("worktree").join(f).exists(), "{f}");
    }
    // Its build output was deleted, not trashed (on Linux and macOS).
    assert_eq!(dir.join("worktree").join("target").exists(), !deletes);
    assert!(dir.join("record").exists() && dir.join("manifest.json").exists());
    assert!(!wt.exists() && !branch_exists(&r.repo, "twr/api"));
    assert!(!worktrees(&r.repo).contains(&wt));
    assert!(!run(
        &r.repo,
        &["for-each-ref", "--contains", &commit, "refs/twr-trash/"]
    )
    .is_empty());

    let list = hub.trash_list().await.unwrap();
    let it = &list["items"][0];
    assert_eq!(list["items"].as_array().unwrap().len(), 1, "{list}");
    assert_eq!(
        (&it["item"], &it["thread"], &it["thread_name"], &it["box"]),
        (
            &Value::from(item.clone()),
            &Value::from(id.clone()),
            &Value::from("api"),
            &Value::from("local")
        )
    );
    assert_eq!(it["project"], "proj", "{it}");
    assert_eq!(it["short"], Value::from(&item[18..]));
    assert_eq!(it["losses"], Value::from(d.losses.clone().unwrap()));
    assert!(it["bytes"].as_u64().unwrap() >= 32 << 10, "{it}");
    assert!(it["size"].as_str().unwrap().ends_with("KB"), "{it}");
    assert_eq!(
        it["purge_at_ms"].as_u64().unwrap() - it["deleted_at_ms"].as_u64().unwrap(),
        7 * DAY_MS
    );
    assert_eq!(list["keep_days"], 7);

    // The name, path and branch are free at once.
    create(&hub, &r.repo, "api").await;
    assert!(wt.exists() && branch_exists(&r.repo, "twr/api"));
    hub.shutdown().await;
}

/// A restore brings the deleted thread back (also after a hub restart): the same thread, with
/// its earlier events, parked with its session, a worktree at its path (dirty files and all)
/// on its branch made again; the item stays, listed as restored, is not restored twice, and is
/// purged as any other (the thread keeps its worktree); the thread resumes.
#[tokio::test]
async fn restore_brings_the_thread_back_with_its_history() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root, 7)).await.unwrap();
    let id = create(&hub, &r.repo, "api").await;
    hub.send("api", "hello", TurnFrom::User, Source::User("test".into()))
        .unwrap();
    turn_done(&hub, &id).await;
    let session = hub.thread_view(&id).unwrap().session_id;
    assert!(session.is_some());
    let wt = r.worktree("api");
    std::fs::write(wt.join("f.txt"), "f").unwrap();
    commit_all(&wt, "work");
    let tip = run(&wt, &["rev-parse", "HEAD"]);
    std::fs::write(wt.join("wip.txt"), "wip").unwrap();
    delete(&hub, "api").await;
    let item = deleted(&hub, &id).trash.unwrap();
    hub.shutdown().await;
    drop(hub);

    let hub = Hub::open(opts(&r.root, 7)).await.unwrap();
    assert!(hub.thread_view("api").is_err());
    assert_eq!(items(&hub).await.len(), 1);
    let out = hub.trash_restore(&item[20..], None, &user()).await.unwrap();
    assert_eq!(out["thread"], Value::from(id.clone()), "{out}");
    assert_eq!(out["thread_name"], "api");
    assert_eq!(out["worktree"], Value::from(wt.to_str().unwrap()));
    assert_eq!(out["branch"], "twr/api");
    assert!(
        out["message"].as_str().unwrap().contains("twr resume api"),
        "{out}"
    );
    let v = hub.thread_view("api").unwrap();
    assert_eq!(v.id, id);
    assert!(v.parked);
    assert_eq!(v.session_id, session);
    assert_eq!(v.worktree.as_deref(), Some(wt.to_str().unwrap()));
    assert!(worktrees(&r.repo).contains(&wt));
    assert_eq!(run(&wt, &["rev-parse", "HEAD"]), tip);
    assert!(run(&wt, &["status", "--porcelain"]).contains("?? wip.txt"));
    let (_, events) = hub.thread_events("api", 0, 10_000).unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e.kind()).collect();
    for k in [
        "thread.created",
        "turn.started",
        "thread.deleted",
        "thread.restored",
    ] {
        assert!(kinds.contains(&k), "{kinds:?}");
    }
    let listed = items(&hub).await;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["restored_as"], "api", "{listed:?}");
    assert!(listed[0]["restored_at"].is_string(), "{listed:?}");
    let text = hub.trash_list().await.unwrap()["message"].clone();
    assert!(text.as_str().unwrap().contains("1 item"), "{text}");
    assert!(r.trash().join(&item).join("worktree/wip.txt").exists());
    assert!(!run(&r.repo, &["for-each-ref", "refs/twr-trash/"]).is_empty());
    let e = hub
        .trash_restore(&item, Some("api-2"), &user())
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::CONFLICT);
    assert!(
        e.message.contains("restored already, as api"),
        "{}",
        e.message
    );
    assert!(hub.thread_view("api-2").is_err() && !branch_exists(&r.repo, "twr/api-2"));
    // Purged as any other, after a restart too; the thread keeps its worktree.
    hub.shutdown().await;
    drop(hub);
    let hub = Hub::open(opts(&r.root, 7)).await.unwrap();
    assert_eq!(items(&hub).await[0]["restored_as"], "api");
    let out = hub
        .trash_empty(std::slice::from_ref(&item), false, &user())
        .await
        .unwrap();
    assert_eq!(out["purged"].as_array().unwrap().len(), 1, "{out}");
    assert!(items(&hub).await.is_empty());
    assert!(!r.trash().join(&item).exists());
    assert!(run(&r.repo, &["for-each-ref", "refs/twr-trash/"]).is_empty());
    assert!(wt.join("wip.txt").exists() && branch_exists(&r.repo, "twr/api"));

    let resumed = hub.resume_thread("api", &user()).await.unwrap();
    assert!(resumed.resumed && !resumed.worktree_recreated);
    hub.send("api", "again", TurnFrom::User, Source::User("test".into()))
        .unwrap();
    turn_done(&hub, &id).await;
    assert_eq!(hub.thread_view(&id).unwrap().turns, 2);
    hub.shutdown().await;
}

/// A thread that took the name meanwhile: the restore is refused until it is given another
/// name, and then lands at a fresh path on its own branch.
#[tokio::test]
async fn a_taken_name_needs_another() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root, 7)).await.unwrap();
    let id = create(&hub, &r.repo, "api").await;
    let wt = r.worktree("api");
    std::fs::write(wt.join("old.txt"), "old").unwrap();
    commit_all(&wt, "old work");
    let tip = run(&wt, &["rev-parse", "HEAD"]);
    delete(&hub, "api").await;
    let item = deleted(&hub, &id).trash.unwrap();
    let new = create(&hub, &r.repo, "api").await;

    let e = hub.trash_restore("api", None, &user()).await.unwrap_err();
    assert_eq!(e.code, codes::CONFLICT);
    assert!(e.message.contains("--as"), "{}", e.message);
    let e = hub
        .trash_restore(&item, Some("bad name"), &user())
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST);
    assert_eq!(items(&hub).await.len(), 1, "nothing changed");

    let out = hub
        .trash_restore(&item, Some("api-old"), &user())
        .await
        .unwrap();
    let fresh = r.worktree("api-old");
    assert_eq!(out["thread_name"], "api-old");
    assert_eq!(out["previous_name"], "api");
    assert_eq!(out["worktree"], Value::from(fresh.to_str().unwrap()));
    assert_eq!(out["branch"], "twr/api-old");
    assert_eq!(hub.thread_view("api-old").unwrap().id, id);
    assert_eq!(hub.thread_view("api").unwrap().id, new);
    assert_eq!(
        run(&fresh, &["symbolic-ref", "HEAD"]),
        "refs/heads/twr/api-old"
    );
    assert_eq!(run(&fresh, &["rev-parse", "HEAD"]), tip);
    assert!(fresh.join("old.txt").exists() && wt.exists());
    run(&fresh, &["status"]);
    assert_eq!(items(&hub).await[0]["restored_as"], "api-old");
    hub.shutdown().await;
}

/// A renamed thread's former names are free once it is deleted. Restored, it takes back those
/// no other thread took meanwhile (they stay reserved for it: its branch may carry one), and
/// leaves the others to the threads that took them; a restart replays the same.
#[tokio::test]
async fn a_restored_thread_keeps_only_the_former_names_still_free() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root, 7)).await.unwrap();
    // A: `api`, renamed `gateway`, deleted and restored as it was.
    let a = create(&hub, &r.repo, "api").await;
    hub.rename_thread("api", "gateway", &user()).unwrap();
    delete(&hub, "gateway").await;
    let item = deleted(&hub, &a).trash.unwrap();
    let out = hub.trash_restore(&item, None, &user()).await.unwrap();
    assert_eq!(out["branch"], "twr/api");
    let v = hub.thread_view(&a).unwrap();
    assert_eq!(
        (v.name.as_str(), v.former_names.clone()),
        ("gateway", vec!["api".to_string()])
    );
    assert_eq!(hub.resolve("api").unwrap(), a);
    assert_eq!(create_err(&hub, &r.repo, "api").await, codes::CONFLICT);

    // Deleted again; B takes `api` meanwhile; A comes back without it.
    delete(&hub, "gateway").await;
    let item = control(&hub)
        .into_iter()
        .filter_map(|e| match e.body {
            EventBody::ThreadDeleted(d) if e.thread == a => d.trash,
            _ => None,
        })
        .next_back()
        .unwrap();
    let b = create(&hub, &r.repo, "api").await;
    let out = hub
        .trash_restore(&item, Some("gateway-restored"), &user())
        .await
        .unwrap();
    assert_eq!(out["thread_name"], "gateway-restored");
    let v = hub.thread_view(&a).unwrap();
    assert!(v.former_names.is_empty(), "{:?}", v.former_names);
    assert_eq!(hub.resolve("api").unwrap(), b);
    // B renamed: `api` is its former name alone, not ambiguous, and no prefix of another.
    create(&hub, &r.repo, "api-client").await;
    hub.rename_thread(&b, "billing", &user()).unwrap();
    assert_eq!(hub.resolve("api").unwrap(), b);
    let restored = control(&hub)
        .into_iter()
        .filter_map(|e| match e.body {
            EventBody::ThreadRestored(p) if e.thread == a => Some(p),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(restored[0].former_names, ["api"]);
    assert!(restored[1].former_names.is_empty());

    hub.shutdown().await;
    drop(hub);
    let hub = Hub::open(opts(&r.root, 7)).await.unwrap();
    assert!(hub.thread_view(&a).unwrap().former_names.is_empty());
    assert_eq!(hub.thread_view(&b).unwrap().former_names, ["api"]);
    assert_eq!(hub.resolve("api").unwrap(), b);
    hub.shutdown().await;
}

/// A thread renamed only in case (`api` to `API`) keeps its earlier spelling through a delete
/// and a restore under the same name: `api` reaches it again, not `api-client` by prefix, live
/// and after a restart.
#[tokio::test]
async fn a_restore_keeps_a_former_name_that_differs_only_in_case() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root, 7)).await.unwrap();
    let a = create(&hub, &r.repo, "api").await;
    hub.rename_thread("api", "API", &user()).unwrap();
    let client = create(&hub, &r.repo, "api-client").await;
    assert_eq!(hub.resolve("api").unwrap(), a);
    delete(&hub, "API").await;
    let item = deleted(&hub, &a).trash.unwrap();
    // While it is deleted, `api` is only a prefix of api-client's name.
    assert_eq!(hub.resolve("api").unwrap(), client);
    let out = hub.trash_restore(&item, None, &user()).await.unwrap();
    assert_eq!(out["thread_name"], "API");
    let v = hub.thread_view(&a).unwrap();
    assert_eq!(
        (v.name.as_str(), v.former_names.clone()),
        ("API", vec!["api".to_string()])
    );
    assert_eq!(hub.resolve("api").unwrap(), a);
    hub.shutdown().await;
    drop(hub);
    let hub = Hub::open(opts(&r.root, 7)).await.unwrap();
    assert_eq!(hub.thread_view(&a).unwrap().former_names, ["api"]);
    assert_eq!(hub.resolve("api").unwrap(), a);
    assert_eq!(hub.resolve("api-c").unwrap(), client);
    hub.shutdown().await;
}

/// The error code of creating thread `name`.
async fn create_err(hub: &Hub, project: &Path, name: &str) -> String {
    hub.new_thread(
        NewThread {
            project: project.to_string_lossy().into_owned(),
            runtime: Some("fake".into()),
            name: Some(name.into()),
            ..Default::default()
        },
        "test",
    )
    .await
    .unwrap_err()
    .code
}

/// A durable spawn whose thread was deleted since stays complete and says so, under the name
/// the thread had (its name may go to a new thread); a retry with its operation id makes no
/// second thread, while a new operation may reuse the name. Once restored, the operation
/// reports the thread again.
#[tokio::test]
async fn a_spawned_thread_deleted_since_is_reported_as_deleted() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root, 7)).await.unwrap();
    let request = || NewThread {
        project: r.repo.to_string_lossy().into_owned(),
        runtime: Some("fake".into()),
        name: Some("api".into()),
        ..Default::default()
    };
    let done = |op: &'static str| {
        let hub = hub.clone();
        async move {
            let mut status = Value::Null;
            until("the spawn completes", || {
                status = hub.spawn_status(op, &user()).unwrap();
                status["status"] != "pending"
            })
            .await;
            status
        }
    };
    hub.begin_spawn("op-1", request(), None, &user()).unwrap();
    let status = done("op-1").await;
    assert_eq!(status["status"], "complete", "{status}");
    let id = status["thread"]["id"].as_str().unwrap().to_string();
    assert_eq!(status["thread"]["name"], "api");

    delete(&hub, "api").await;
    let status = hub.spawn_status("op-1", &user()).unwrap();
    assert_eq!(status["status"], "complete", "{status}");
    assert_eq!(status["thread"]["id"], Value::from(id.clone()));
    assert_eq!(status["thread"]["name"], "api");
    assert_eq!(status["thread"]["deleted"], true, "{status}");
    // A retry is the same operation: nothing new is made.
    let again = hub.begin_spawn("op-1", request(), None, &user()).unwrap();
    assert_eq!(again["thread"]["deleted"], true, "{again}");

    // The name is free: another operation takes it, the first still names its own thread.
    hub.begin_spawn("op-2", request(), None, &user()).unwrap();
    let second = done("op-2").await;
    assert_eq!(second["status"], "complete", "{second}");
    assert_ne!(second["thread"]["id"], Value::from(id.clone()));
    assert_eq!(second["thread"]["name"], "api");
    let first = hub.spawn_status("op-1", &user()).unwrap();
    assert_eq!(first["thread"]["id"], Value::from(id.clone()));
    assert_eq!(first["thread"]["deleted"], true);

    // Restored under another name: the operation reports the thread as it is now.
    let item = deleted(&hub, &id).trash.unwrap();
    hub.trash_restore(&item, Some("api-old"), &user())
        .await
        .unwrap();
    let first = hub.spawn_status("op-1", &user()).unwrap();
    assert_eq!(first["thread"]["id"], Value::from(id));
    assert_eq!(first["thread"]["name"], "api-old");
    assert!(first["thread"].get("deleted").is_none(), "{first}");
    hub.shutdown().await;
}

/// Only the user empties the trash or restores from it; anyone may list it.
#[tokio::test]
async fn only_the_user_empties_or_restores() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root, 7)).await.unwrap();
    create(&hub, &r.repo, "api").await;
    delete(&hub, "api").await;
    let orch = Caller::Orchestrator;
    let e = hub.trash_empty(&[], true, &orch).await.unwrap_err();
    assert_eq!(e.code, codes::UNAUTHORIZED);
    let e = hub.trash_restore("api", None, &orch).await.unwrap_err();
    assert_eq!(e.code, codes::UNAUTHORIZED);
    assert_eq!(items(&hub).await.len(), 1);
    let e = hub.trash_empty(&[], false, &user()).await.unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST);
    hub.shutdown().await;
}

/// Emptying by item (its id, the end of it, or its thread's name) and all at once; an item
/// that is not there is said, and nothing else goes.
#[tokio::test]
async fn empty_by_item_and_all() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root, 7)).await.unwrap();
    for name in ["a", "b", "c"] {
        create(&hub, &r.repo, name).await;
        delete(&hub, name).await;
    }
    let all = items(&hub).await;
    assert_eq!(all.len(), 3);
    let id_of = |name: &str| {
        all.iter().find(|i| i["thread_name"] == name).unwrap()["item"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let (a, b) = (id_of("a"), id_of("b"));
    let e = hub
        .trash_empty(&["nothing".into()], false, &user())
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::NOT_FOUND);
    let out = hub
        .trash_empty(&[a[18..].to_lowercase(), "b".into()], false, &user())
        .await
        .unwrap();
    let purged: Vec<&str> = out["purged"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["item"].as_str().unwrap())
        .collect();
    assert_eq!(purged.len(), 2, "{out}");
    assert!(purged.contains(&a.as_str()) && purged.contains(&b.as_str()));
    assert!(!r.trash().join(&a).exists() && !r.trash().join(&b).exists());
    let left = items(&hub).await;
    assert_eq!(left.len(), 1);
    assert_eq!(left[0]["thread_name"], "c");
    let refs = run(
        &r.repo,
        &["for-each-ref", "--format=%(refname)", "refs/twr-trash/"],
    );
    assert!(
        !refs.contains(&a) && !refs.contains(&b) && !refs.is_empty(),
        "{refs}"
    );
    let out = hub.trash_empty(&[], true, &user()).await.unwrap();
    assert_eq!(out["purged"].as_array().unwrap().len(), 1, "{out}");
    assert!(items(&hub).await.is_empty());
    assert!(run(&r.repo, &["for-each-ref", "refs/twr-trash/"]).is_empty());
    hub.shutdown().await;
}

/// Items are purged `keep_days` after their deletion (the purge given the time), with their
/// refs; another directory in the trash stays.
#[tokio::test]
async fn items_are_purged_after_keep_days() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root, 2)).await.unwrap();
    let id = create(&hub, &r.repo, "api").await;
    let wt = r.worktree("api");
    std::fs::write(wt.join("f.txt"), "f").unwrap();
    commit_all(&wt, "work");
    delete(&hub, "api").await;
    let item = deleted(&hub, &id).trash.unwrap();
    std::fs::create_dir_all(r.trash().join("notes")).unwrap();
    std::fs::write(r.trash().join("notes").join("keep.txt"), "mine").unwrap();
    let deleted_at = items(&hub).await[0]["deleted_at_ms"].as_u64().unwrap();

    let out = hub.purge_trash(deleted_at + DAY_MS).await;
    assert!(out["purged"].as_array().unwrap().is_empty(), "{out}");
    assert_eq!(items(&hub).await.len(), 1);
    let out = hub.purge_trash(deleted_at + 2 * DAY_MS + 1).await;
    assert_eq!(out["purged"], serde_json::json!([item.clone()]), "{out}");
    assert!(items(&hub).await.is_empty());
    assert!(!r.trash().join(&item).exists());
    assert!(r.trash().join("notes").join("keep.txt").exists());
    assert!(run(&r.repo, &["for-each-ref", "refs/twr-trash/"]).is_empty());
    hub.shutdown().await;
}

/// Makes every append of a `kind` event fail (a trigger in the hub's database) until
/// [`allow`]: an injected store failure.
fn refuse(r: &Repo, kind: &str) {
    let db = rusqlite::Connection::open(r.root.join("data").join("twr.db")).unwrap();
    db.busy_timeout(Duration::from_secs(10)).unwrap();
    db.execute_batch(&format!(
        "CREATE TRIGGER refuse_{name} BEFORE INSERT ON events WHEN NEW.kind = '{kind}' \
         BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
        name = kind.replace('.', "_")
    ))
    .unwrap();
}

/// Undoes [`refuse`].
fn allow(r: &Repo, kind: &str) {
    let db = rusqlite::Connection::open(r.root.join("data").join("twr.db")).unwrap();
    db.busy_timeout(Duration::from_secs(10)).unwrap();
    db.execute_batch(&format!("DROP TRIGGER refuse_{};", kind.replace('.', "_")))
        .unwrap();
}

/// The box's git (the local box of a hub in `r`), as a test drives it behind the hub.
fn box_git(r: &Repo) -> twr_runner::local::LocalGit {
    twr_runner::local::LocalGit::default().with_trash_root(r.trash())
}

/// Asks to delete `name` and confirms the card as the user, without waiting.
async fn confirm_delete(hub: &Hub, name: &str) {
    let card = hub
        .delete_threads(&[name.to_string()], &user())
        .await
        .unwrap();
    hub.respond_at(
        card["attn_id"].as_str().unwrap(),
        Answer::new("Delete"),
        card["revision"].as_u64(),
        &user(),
    )
    .unwrap();
}

/// Waits until `cond` holds.
async fn until(what: &str, mut cond: impl FnMut() -> bool) {
    timeout(WAIT, async {
        while !cond() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("never: {what}"));
}

/// The item's directory as it is: every entry's relative path with its contents (a symlink's
/// target) and modification time.
fn snapshot(dir: &Path) -> std::collections::BTreeMap<PathBuf, (Vec<u8>, i64, i64)> {
    use std::os::unix::fs::MetadataExt;
    let mut out = std::collections::BTreeMap::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            let m = std::fs::symlink_metadata(&p).unwrap();
            let body = if m.is_dir() {
                todo.push(p.clone());
                Vec::new()
            } else if m.is_file() {
                std::fs::read(&p).unwrap()
            } else {
                Vec::new()
            };
            out.insert(
                p.strip_prefix(dir).unwrap().to_path_buf(),
                (body, m.mtime(), m.mtime_nsec()),
            );
        }
    }
    out
}

/// A restore the hub cannot record (an append that fails): the box removes the
/// worktree and branch it made, the thread stays deleted, the item is as it was, and a later
/// restore works.
#[tokio::test]
async fn a_restore_that_cannot_be_recorded_is_undone() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root, 7)).await.unwrap();
    let id = create(&hub, &r.repo, "api").await;
    let wt = r.worktree("api");
    std::fs::write(wt.join("wip.txt"), "wip").unwrap();
    delete(&hub, "api").await;
    let item = deleted(&hub, &id).trash.unwrap();
    let dir = r.trash().join(&item);
    let before = snapshot(&dir);
    refuse(&r, "thread.restored");
    let e = hub.trash_restore(&item, None, &user()).await.unwrap_err();
    assert_eq!(e.code, codes::INTERNAL, "{}", e.message);
    assert!(
        e.message
            .contains("removed again, and the item is as it was"),
        "{}",
        e.message
    );
    assert!(hub.thread_view("api").is_err());
    assert!(!wt.exists() && !branch_exists(&r.repo, "twr/api"));
    assert!(!worktrees(&r.repo).contains(&wt));
    assert_eq!(snapshot(&dir), before);
    let listed = items(&hub).await;
    assert_eq!(listed.len(), 1);
    assert!(listed[0].get("restored_as").is_none(), "{listed:?}");
    allow(&r, "thread.restored");
    hub.trash_restore(&item, None, &user()).await.unwrap();
    assert_eq!(hub.thread_view("api").unwrap().id, id);
    assert!(wt.join("wip.txt").exists());
    assert_eq!(snapshot(&dir), before);
    hub.shutdown().await;
}

/// The box restored a thread and the hub stopped before recording it: the hub,
/// starting, removes what the restore made (the log says deleted, so the box agrees), the
/// item untouched; restoring it then works.
#[tokio::test]
async fn a_restore_cut_short_is_settled_with_the_log() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root, 7)).await.unwrap();
    let id = create(&hub, &r.repo, "api").await;
    let wt = r.worktree("api");
    std::fs::write(wt.join("wip.txt"), "wip").unwrap();
    delete(&hub, "api").await;
    let item = deleted(&hub, &id).trash.unwrap();
    hub.shutdown().await;
    drop(hub);
    let before = snapshot(&r.trash().join(&item));
    // The box restores; nothing records it.
    use twr_core::git::GitOps;
    box_git(&r)
        .trash_restore(twr_core::git::RestoreRequest {
            item: item.clone(),
            branch: "twr/api".into(),
            paths: vec![wt.clone()],
            other_branches: Some(Vec::new()),
        })
        .await
        .unwrap();
    assert!(wt.join("wip.txt").exists() && branch_exists(&r.repo, "twr/api"));

    let hub = Hub::open(opts(&r.root, 7)).await.unwrap();
    assert!(hub.thread_view("api").is_err());
    assert!(!wt.exists() && !branch_exists(&r.repo, "twr/api"));
    assert!(!worktrees(&r.repo).contains(&wt));
    assert_eq!(snapshot(&r.trash().join(&item)), before);
    let listed = items(&hub).await;
    assert_eq!(listed.len(), 1);
    assert!(listed[0].get("restored_as").is_none(), "{listed:?}");
    // Nothing left to settle.
    let done = hub.reconcile_trash().await;
    assert_eq!(done["undone"], serde_json::json!([]), "{done}");
    hub.trash_restore(&item, None, &user()).await.unwrap();
    assert_eq!(hub.thread_view("api").unwrap().id, id);
    assert!(wt.join("wip.txt").exists());
    // Recorded: a reconciliation leaves it.
    let done = hub.reconcile_trash().await;
    assert_eq!(done["undone"], serde_json::json!([]), "{done}");
    assert!(wt.join("wip.txt").exists() && branch_exists(&r.repo, "twr/api"));
    hub.shutdown().await;
}

/// A deletion whose `thread.deleted` cannot be recorded after its worktree and
/// branch moved to the trash: the thread stays (nothing resumes it meanwhile) and an error
/// item says so; the next reconciliation records the deletion from the item. One the hub
/// stopped before recording is recorded when it starts.
#[tokio::test]
async fn a_deletion_that_cannot_be_recorded_is_recorded_later() {
    let r = Repo::new();
    let hub = Hub::open(opts(&r.root, 7)).await.unwrap();
    let id = create(&hub, &r.repo, "api").await;
    let b = create(&hub, &r.repo, "b").await;
    refuse(&r, "thread.deleted");
    confirm_delete(&hub, "api").await;
    until("the error item", || {
        hub.attention_list(true).iter().any(|a| {
            a["summary"]
                .as_str()
                .is_some_and(|s| s.contains("could not be recorded"))
        })
    })
    .await;
    let listed = items(&hub).await;
    assert_eq!(listed.len(), 1, "{listed:?}");
    let item = listed[0]["item"].as_str().unwrap().to_string();
    assert!(hub.thread_view("api").is_ok());
    assert!(!r.worktree("api").exists());
    assert!(hub.resume_thread("api", &user()).await.is_err());
    allow(&r, "thread.deleted");
    let done = hub.reconcile_trash().await;
    assert_eq!(
        done["recorded"],
        serde_json::json!([item.clone()]),
        "{done}"
    );
    assert!(hub.thread_view(&id).is_err());
    assert_eq!(deleted(&hub, &id).trash.as_deref(), Some(item.as_str()));
    assert!(hub.attention_list(true).is_empty());

    // The hub stops before it can record it: recorded as it starts.
    refuse(&r, "thread.deleted");
    confirm_delete(&hub, "b").await;
    until("b in the trash", || {
        r.trash().read_dir().unwrap().count() == 2
    })
    .await;
    until("the error item", || !hub.attention_list(true).is_empty()).await;
    hub.shutdown().await;
    drop(hub);
    allow(&r, "thread.deleted");
    let hub = Hub::open(opts(&r.root, 7)).await.unwrap();
    assert!(hub.thread_view(&b).is_err());
    assert!(deleted(&hub, &b).trash.is_some());
    assert_eq!(items(&hub).await.len(), 2);
    // Both come back.
    hub.trash_restore("b", None, &user()).await.unwrap();
    hub.trash_restore("api", None, &user()).await.unwrap();
    assert!(r.worktree("api").exists() && r.worktree("b").exists());
    hub.shutdown().await;
}

/// An item being made (its deletion held at its first ref change) is neither
/// purged by the hub (it holds the item) nor by the box (the item's lock), however old the
/// purge's cutoff; once the deletion is done, the item is whole.
#[tokio::test]
async fn an_item_being_made_is_never_purged() {
    let r = Repo::new();
    let held = twr_core::testkit::git::HeldGit::install(&r.root.join("bin"));
    let mut o = opts(&r.root, 0);
    o.git_bin = Some(held.bin.clone());
    let hub = Hub::open(o).await.unwrap();
    let id = create(&hub, &r.repo, "api").await;
    let wt = r.worktree("api");
    std::fs::write(wt.join("wip.txt"), "wip").unwrap();
    confirm_delete(&hub, "api").await;
    until("the move to the trash held", || held.held()).await;
    let listed = items(&hub).await;
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0]["incomplete"], true);
    let item = listed[0]["item"].as_str().unwrap().to_string();
    let out = hub.purge_trash(u64::MAX).await;
    assert!(out["purged"].as_array().unwrap().is_empty(), "{out}");
    let out = hub.trash_empty(&[], true, &user()).await.unwrap();
    assert!(out["purged"].as_array().unwrap().is_empty(), "{out}");
    use twr_core::git::GitOps;
    let p = box_git(&r)
        .trash_purge(twr_core::git::TrashPurge {
            items: vec![item.clone()],
            before_ms: Some(u64::MAX),
        })
        .await
        .unwrap();
    assert!(p.purged.is_empty(), "{p:?}");
    assert!(p.failed[0].error.contains("busy"), "{p:?}");
    held.release();
    until("api deleted", || hub.thread_view(&id).is_err()).await;
    let dir = r.trash().join(&item);
    assert!(dir.join("worktree/wip.txt").exists() && dir.join("record").exists());
    assert!(items(&hub).await[0].get("incomplete").is_none());
    hub.shutdown().await;
}
