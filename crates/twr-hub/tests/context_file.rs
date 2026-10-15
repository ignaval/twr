//! The standing context file's safety: `context.set`
//! with `expect_hash`, logging an unlogged manual edit before overwriting it, refusing
//! non-UTF-8 files and symlinked `.twr/` / `context.md`, keeping permissions, the 1 MiB read
//! cap, and a cached file never masking a rewrite.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::timeout;
use twr_core::adapter::{
    Adapter, AdapterCmd, AdapterEnd, AdapterHandle, AdapterInfo, SpawnConfig, Tier,
};
use twr_core::blob::blob_ref;
use twr_core::config::{Config, ProjectConfig};
use twr_core::event::*;
use twr_core::proto::codes;
use twr_hub::hub::{injection, ThreadSubscription, SET_MAX};
use twr_hub::{Caller, Hub, HubOptions, NewThread};

const WAIT: Duration = Duration::from_secs(10);

type Prompts = Arc<Mutex<Vec<String>>>;

/// Runtime `rec`: records every prompt and completes each turn at once.
struct Recorder(Prompts);

impl Adapter for Recorder {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "rec",
            tier: Tier::Headless,
            description: "records prompts",
        }
    }

    fn spawn(&self, cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let (h, end) = AdapterHandle::pair();
        let first = prompt.map(|p| (p, cfg.prompt_turn.unwrap_or(1)));
        tokio::spawn(serve(end, Arc::clone(&self.0), first));
        Ok(h)
    }
}

async fn serve(mut end: AdapterEnd, prompts: Prompts, first: Option<(String, u64)>) {
    end.emit(
        EventBody::ThreadStarted(SessionPayload {
            session_id: Some("s-rec".into()),
            ..Default::default()
        }),
        None,
    );
    let turn = |text: String, turn: u64, end: &AdapterEnd| {
        prompts.lock().unwrap().push(text);
        end.emit(
            EventBody::TurnEnded(TurnEnded {
                result: TurnResult::Completed,
                ..Default::default()
            }),
            Some(turn),
        );
    };
    if let Some((p, t)) = first {
        turn(p, t, &end);
    }
    while let Some(cmd) = end.cmds.recv().await {
        match cmd {
            AdapterCmd::Send { text, turn: t } => turn(text, t, &end),
            AdapterCmd::Stop | AdapterCmd::Interrupt => break,
            _ => {}
        }
    }
    end.exited(Some(0), "");
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    hub: Hub,
    prompts: Prompts,
    thread: String,
    project: String,
    file: PathBuf,
}

/// A hub in `root` whose runtime `rec` records its prompts in `prompts`.
async fn open_hub(root: &Path, mut config: Config, prompts: &Prompts) -> Hub {
    config.orchestrator.runtime = Some("rec".into());
    let mut o = HubOptions::new(root.join("data").join("twr.db"), config);
    let rec: Arc<dyn Adapter> = Arc::new(Recorder(Arc::clone(prompts)));
    o.adapters = Arc::new(move |name: &str| -> Option<Arc<dyn Adapter>> {
        (name == "rec").then(|| Arc::clone(&rec))
    });
    o.stop_timeout = Duration::from_secs(5);
    Hub::open(o).await.unwrap()
}

/// A hub with one `rec` worker thread on `<tmp>/proj`.
async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let repo = root.join("proj");
    std::fs::create_dir(&repo).unwrap();
    let prompts = Prompts::default();
    let hub = open_hub(&root, Config::default(), &prompts).await;
    let thread = hub
        .new_thread(
            NewThread {
                project: repo.to_string_lossy().into_owned(),
                runtime: Some("rec".into()),
                name: Some("w".into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    Fixture {
        _dir: dir,
        file: repo.join(".twr").join("context.md"),
        root,
        hub,
        prompts,
        thread,
        project: "proj".into(),
    }
}

impl Fixture {
    /// Sends `prompt` and waits for turn `n` to end; returns what the runtime received.
    async fn turn(&self, sub: &mut ThreadSubscription, prompt: &str, n: u64) -> String {
        self.hub
            .send(
                &self.thread,
                prompt,
                TurnFrom::User,
                Source::User("test".into()),
            )
            .unwrap();
        timeout(WAIT, async {
            loop {
                let ev = sub.next().await.unwrap().expect("hub stopped");
                if ev.kind() == "turn.ended" && ev.turn == Some(n) {
                    return;
                }
            }
        })
        .await
        .expect("turn never ended");
        self.prompts.lock().unwrap().last().cloned().unwrap()
    }

    /// `context.updated` events with their gseq.
    fn updates(&self) -> Vec<(u64, ContextUpdated)> {
        let (items, _) = self.hub.control_read(0, 100_000).unwrap();
        items
            .into_iter()
            .filter_map(|(g, e)| match e.body {
                EventBody::ContextUpdated(c) => Some((g, c)),
                _ => None,
            })
            .collect()
    }

    fn write(&self, bytes: &[u8]) {
        std::fs::create_dir_all(self.file.parent().unwrap()).unwrap();
        std::fs::write(&self.file, bytes).unwrap();
    }
}

fn origin(u: &ContextUpdated) -> &str {
    u.ext.as_ref().unwrap()["twr"]["origin"].as_str().unwrap()
}

#[tokio::test]
async fn a_stale_expect_hash_is_a_conflict_and_changes_nothing() {
    let f = fixture().await;
    let user = Caller::user("cli");
    f.write(b"v1\n");
    let read = f.hub.context_read(&f.project).await.unwrap();
    let h1 = read["hash"].as_str().unwrap().to_string();
    assert_eq!(h1, blob_ref(b"v1\n"));
    // Changed by hand after the read.
    f.write(b"v2 by hand\n");
    let e = f
        .hub
        .context_set(&f.project, "mine\n", Some(&h1), &user)
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::CONFLICT, "{e:?}");
    assert_eq!(std::fs::read(&f.file).unwrap(), b"v2 by hand\n");
    assert!(f.updates().is_empty());
    // With the current hash it goes through.
    let h2 = blob_ref(b"v2 by hand\n");
    let r = f
        .hub
        .context_set(&f.project, "mine\n", Some(&h2), &user)
        .await
        .unwrap();
    assert_eq!(r["changed"], true);
    assert_eq!(std::fs::read(&f.file).unwrap(), b"mine\n");
    f.hub.shutdown().await;
}

#[tokio::test]
async fn an_unlogged_manual_edit_is_logged_before_the_set_overwrites_it() {
    let f = fixture().await;
    let user = Caller::user("cli");
    f.hub
        .context_set(&f.project, "A\n", None, &user)
        .await
        .unwrap();
    f.write(b"B by hand\n");
    f.hub
        .context_set(&f.project, "C\n", None, &user)
        .await
        .unwrap();
    let ups = f.updates();
    let got: Vec<(&str, &str)> = ups
        .iter()
        .map(|(_, u)| (origin(u), u.hash.as_str()))
        .collect();
    let (a, b, c) = (blob_ref(b"A\n"), blob_ref(b"B by hand\n"), blob_ref(b"C\n"));
    assert_eq!(got, [("set", a.as_str()), ("file", &b), ("set", &c)]);
    // The capture and the set are one batch: adjacent in the log.
    assert_eq!(ups[2].0, ups[1].0 + 1);
    assert_eq!(f.hub.blob(&b).unwrap().unwrap(), b"B by hand\n");
    assert_eq!(std::fs::read(&f.file).unwrap(), b"C\n");
    f.hub.shutdown().await;
}

#[tokio::test]
async fn a_non_utf8_file_is_never_written_over() {
    let f = fixture().await;
    f.write(b"rules \xff\xfe\n");
    let r = f.hub.context_read(&f.project).await.unwrap();
    assert_eq!(r["utf8"], false);
    let e = f
        .hub
        .context_set(&f.project, "new\n", None, &Caller::user("cli"))
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::CONFLICT);
    assert!(e.message.contains("not UTF-8"), "{e:?}");
    assert_eq!(std::fs::read(&f.file).unwrap(), b"rules \xff\xfe\n");
    assert!(f.updates().is_empty());
    f.hub.shutdown().await;
}

#[tokio::test]
async fn symlinked_context_files_and_dirs_are_refused() {
    let f = fixture().await;
    let user = Caller::user("cli");
    let secret = f.root.join("secret.txt");
    std::fs::write(&secret, "TOP SECRET\n").unwrap();
    std::fs::create_dir_all(f.file.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&secret, &f.file).unwrap();

    // context.md -> a file elsewhere: not read, not written, not injected.
    let e = f.hub.context_read(&f.project).await.unwrap_err();
    assert_eq!(e.code, codes::CONFLICT);
    assert!(e.message.contains("symbolic link"), "{e:?}");
    let e = f
        .hub
        .context_set(&f.project, "x\n", None, &user)
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::CONFLICT);
    assert_eq!(std::fs::read(&secret).unwrap(), b"TOP SECRET\n");
    assert!(std::fs::symlink_metadata(&f.file)
        .unwrap()
        .file_type()
        .is_symlink());
    let mut sub = f.hub.subscribe_thread(&f.thread, 0).unwrap();
    assert_eq!(f.turn(&mut sub, "go", 1).await, "go");
    assert!(f.updates().is_empty());

    // .twr -> a directory elsewhere: the same.
    let elsewhere = f.root.join("elsewhere");
    std::fs::create_dir(&elsewhere).unwrap();
    std::fs::write(elsewhere.join("context.md"), "OTHER\n").unwrap();
    std::fs::remove_dir_all(f.file.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&elsewhere, f.file.parent().unwrap()).unwrap();
    let e = f.hub.context_read(&f.project).await.unwrap_err();
    assert_eq!(e.code, codes::CONFLICT);
    let e = f
        .hub
        .context_set(&f.project, "x\n", None, &user)
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::CONFLICT);
    assert_eq!(
        std::fs::read(elsewhere.join("context.md")).unwrap(),
        b"OTHER\n"
    );
    assert_eq!(std::fs::read_dir(&elsewhere).unwrap().count(), 1);
    assert_eq!(f.turn(&mut sub, "again", 2).await, "again");
    assert!(f.updates().is_empty());
    f.hub.shutdown().await;
}

#[tokio::test]
async fn a_set_keeps_the_file_mode_and_leaves_no_temp_file() {
    let f = fixture().await;
    f.write(b"old\n");
    std::fs::set_permissions(&f.file, std::fs::Permissions::from_mode(0o600)).unwrap();
    f.hub
        .context_set(&f.project, "new\n", None, &Caller::user("cli"))
        .await
        .unwrap();
    let meta = std::fs::symlink_metadata(&f.file).unwrap();
    assert!(meta.is_file());
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    assert_eq!(std::fs::read(&f.file).unwrap(), b"new\n");
    let names: Vec<_> = std::fs::read_dir(f.file.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(names, ["context.md"]);
    f.hub.shutdown().await;
}

#[tokio::test]
async fn a_file_over_the_cap_is_read_logged_and_injected_truncated() {
    let f = fixture().await;
    let size = SET_MAX + 100;
    f.write(&vec![b'a'; size]);
    let r = f.hub.context_read(&f.project).await.unwrap();
    assert_eq!(
        (
            r["truncated"].as_bool(),
            r["bytes"].as_u64(),
            r["file_bytes"].as_u64()
        ),
        (Some(true), Some(SET_MAX as u64), Some(size as u64))
    );
    let mut sub = f.hub.subscribe_thread(&f.thread, 0).unwrap();
    let got = f.turn(&mut sub, "go", 1).await;
    assert!(got.len() < 66 * 1024, "{}", got.len());
    assert!(
        got.contains(&format!("cut to its first 65536 of {size} bytes")),
        "{}",
        &got[got.len() - 300..]
    );
    let ups = f.updates();
    assert_eq!(ups.len(), 1);
    let twr = &ups[0].1.ext.as_ref().unwrap()["twr"];
    assert_eq!(
        (
            &twr["origin"],
            &twr["truncated"],
            &twr["bytes"],
            &twr["file_bytes"]
        ),
        (&"file".into(), &true.into(), &SET_MAX.into(), &size.into())
    );
    assert_eq!(ups[0].1.hash, blob_ref(&vec![b'a'; SET_MAX]));
    // Unchanged: not logged again.
    f.turn(&mut sub, "again", 2).await;
    assert_eq!(f.updates().len(), 1);
    f.hub.shutdown().await;
}

/// A file of exactly [`SET_MAX`] bytes is read whole (`truncated: false`); once
/// it grows past the cap its capped hash is unchanged, so a stale `expect_hash` must not pass
/// and overwrite (and log only the beginning of) the grown file. Any set over a file larger
/// than the cap is refused before any comparison or event.
#[tokio::test]
async fn a_set_never_overwrites_a_file_grown_past_the_cap() {
    let f = fixture().await;
    let user = Caller::user("cli");
    let full = vec![b'a'; SET_MAX];
    f.write(&full);
    let r = f.hub.context_read(&f.project).await.unwrap();
    assert_eq!(r["truncated"], false);
    let stale = r["hash"].as_str().unwrap().to_string();
    assert_eq!(stale, blob_ref(&full));
    // Appended by hand after the read.
    let mut grown = full.clone();
    grown.extend_from_slice(b"appended by hand\n");
    std::fs::OpenOptions::new()
        .append(true)
        .open(&f.file)
        .unwrap()
        .write_all(b"appended by hand\n")
        .unwrap();
    for expect in [Some(stale.as_str()), None] {
        let e = f
            .hub
            .context_set(&f.project, "mine\n", expect, &user)
            .await
            .unwrap_err();
        assert_eq!(e.code, codes::CONFLICT, "{e:?}");
        assert!(e.message.contains("larger than twr manages"), "{e:?}");
        assert_eq!(std::fs::read(&f.file).unwrap(), grown);
        assert!(f.updates().is_empty());
    }
    f.hub.shutdown().await;
}

#[tokio::test]
async fn a_cached_file_never_masks_a_same_size_rewrite() {
    let f = fixture().await;
    let mut sub = f.hub.subscribe_thread(&f.thread, 0).unwrap();
    // Just written ("racy"): read at every dispatch.
    f.write(b"AAAA");
    assert!(f.turn(&mut sub, "1", 1).await.contains("AAAA"));
    f.write(b"BBBB");
    assert!(f.turn(&mut sub, "2", 2).await.contains("BBBB"));
    // Old enough to be cached; then rewritten with the same size.
    tokio::time::sleep(Duration::from_millis(2100)).await;
    assert!(f.turn(&mut sub, "3", 3).await.contains("BBBB"));
    assert!(f.turn(&mut sub, "4", 4).await.contains("BBBB"));
    f.write(b"CCCC");
    assert!(f.turn(&mut sub, "5", 5).await.contains("CCCC"));
    let hashes: Vec<String> = f.updates().into_iter().map(|(_, u)| u.hash).collect();
    assert_eq!(
        hashes,
        [blob_ref(b"AAAA"), blob_ref(b"BBBB"), blob_ref(b"CCCC")]
    );
    f.hub.shutdown().await;
}

/// A project configured at a subdirectory of a git repository keeps its standing
/// context at the repository's top level, where its threads work (`thread.created.repo`):
/// `context.set` before any thread exists, `context.read`, and a worker's prompt all use that
/// one file.
#[tokio::test]
async fn a_project_configured_in_a_subdirectory_shares_the_repositorys_context() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let top = root.join("repo");
    let sub = top.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let init = std::process::Command::new("git")
        .args(["init", "-q"])
        .arg(&top)
        .status()
        .unwrap();
    assert!(init.success());
    let top = std::fs::canonicalize(&top).unwrap();
    let mut config = Config::default();
    config.projects.insert(
        "sub".into(),
        ProjectConfig {
            repo: sub.clone(),
            runtime: Some("rec".into()),
            box_name: None,
            ..Default::default()
        },
    );
    let prompts = Prompts::default();
    let hub = open_hub(&root, config, &prompts).await;
    let user = Caller::user("cli");
    let file = top.join(".twr").join("context.md");

    let set = hub
        .context_set("sub", "Sub rules.\n", None, &user)
        .await
        .unwrap();
    assert_eq!(set["path"], file.to_string_lossy().as_ref());
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "Sub rules.\n");
    assert!(!sub.join(".twr").exists());
    let read = hub.context_read("sub").await.unwrap();
    assert_eq!(read["path"], set["path"]);
    assert_eq!(read["content"], "Sub rules.\n");

    let thread = hub
        .new_thread(
            NewThread {
                project: "sub".into(),
                name: Some("w".into()),
                no_worktree: true,
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let view = hub.thread_view(&thread).unwrap();
    assert_eq!(view.repo.as_deref(), Some(top.to_string_lossy().as_ref()));
    let mut sub_events = hub.subscribe_thread(&thread, 0).unwrap();
    hub.send(&thread, "go", TurnFrom::User, Source::User("test".into()))
        .unwrap();
    timeout(WAIT, async {
        loop {
            let ev = sub_events.next().await.unwrap().expect("hub stopped");
            if ev.kind() == "turn.ended" {
                return;
            }
        }
    })
    .await
    .expect("turn never ended");
    let got = prompts.lock().unwrap().last().cloned().unwrap();
    assert_eq!(got, format!("{}go", injection("sub", "Sub rules.\n", None)));
    // Still one file after the dispatch.
    let read = hub.context_read("sub").await.unwrap();
    assert_eq!(read["path"], set["path"]);
}
