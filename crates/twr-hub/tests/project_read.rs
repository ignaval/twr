//! The orchestrator's project reads (`project.files`, `project.read`, `project.search`,
//! `hub::project_read`) against real git repositories: committed content only, secrets
//! refused, sizes capped, a budget per turn, a `project.read` event for every call, and the
//! config switch.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::timeout;
use twr_adapters::fake::FakeAdapter;
use twr_core::adapter::{Adapter, AdapterEnd, AdapterHandle, AdapterInfo, SpawnConfig, Tier};
use twr_core::client::Client;
use twr_core::config::{BoxConfig, Config, ProjectConfig};
use twr_core::event::{EventBody, SessionPayload};
use twr_core::paths::Paths;
use twr_core::proto::{codes, RpcError};
use twr_hub::{server, Hub, HubOptions};

const WAIT: Duration = Duration::from_secs(10);

/// A private key line, split so this file holds none.
fn key_line() -> String {
    format!("-----BEGIN OPENSSH {} KEY-----", "PRIVATE")
}

struct OrchRt {
    cfg: SpawnConfig,
    _end: AdapterEnd,
}

struct OrchAdapter(mpsc::UnboundedSender<OrchRt>);

impl Adapter for OrchAdapter {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "claude-code",
            tier: Tier::Headless,
            description: "test orchestrator runtime",
        }
    }

    fn spawn(&self, cfg: SpawnConfig, _prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let (h, end) = AdapterHandle::pair();
        end.emit(
            EventBody::ThreadStarted(SessionPayload {
                session_id: Some("s-orch".into()),
                ..Default::default()
            }),
            None,
        );
        let _ = self.0.send(OrchRt { cfg, _end: end });
        Ok(h)
    }
}

struct TestHub {
    dir: tempfile::TempDir,
    socket: PathBuf,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
    spawns: mpsc::UnboundedReceiver<OrchRt>,
    /// The orchestrator's runtime, kept alive.
    rt: Option<OrchRt>,
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Creates the executable `path` (mode 755, its directories made) holding `content`, without
/// this process ever holding it open for writing: the bytes go to a side file, `cp` writes
/// `path`, and the side file is removed. A child another test thread forks while this process
/// writes the file would inherit the descriptor, and exec'ing the file would fail with
/// ETXTBSY until that child execs or exits.
fn install(path: &Path, content: impl AsRef<[u8]>) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut src = path.as_os_str().to_owned();
    src.push(".src");
    std::fs::write(&src, content).unwrap();
    let st = std::process::Command::new("cp")
        .arg(&src)
        .arg(path)
        .status()
        .unwrap();
    assert!(st.success(), "cp failed");
    std::fs::remove_file(&src).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn write(dir: &Path, path: &str, data: &[u8]) {
    let p = dir.join(path);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, data).unwrap();
}

/// A repository with committed docs, secrets, big and binary files, a symlink out, a
/// submodule entry, and files that are not committed.
fn make_repo(repo: &Path) {
    std::fs::create_dir_all(repo).unwrap();
    git(repo, &["init", "-q"]);
    write(repo, "README.md", b"# api\n\nRun it with `make run`.\n");
    write(repo, "docs/guide.md", b"needle in the guide\n");
    write(repo, ".env", b"API_KEY=hunter2\n");
    write(repo, ".env.local.example", b"API_KEY=changeme\n");
    write(repo, ".env.example", b"API_KEY=changeme\n");
    write(repo, "deploy/id_ed25519", b"not really a key\n");
    write(repo, "deploy/id_ed25519.pub", b"ssh-ed25519 AAAA t@t\n");
    write(repo, "certs/prod.pem", b"cert\n");
    write(
        repo,
        "notes.txt",
        format!("{}\nb3Bl\nneedle\n", key_line()).as_bytes(),
    );
    write(repo, "big.txt", &vec![b'x'; 2 << 20]);
    write(repo, "logo.bin", &[0u8, 1, 2, 3, 0, 9]);
    let mut long = String::new();
    for i in 0..4000 {
        long.push_str(&format!("line {i:05} of a long file\n"));
    }
    write(repo, "long.txt", long.as_bytes());
    for i in 0..30 {
        write(
            repo,
            &format!("many/f{i:02}.txt"),
            b"needle one\nneedle two\nneedle three\n",
        );
    }
    std::os::unix::fs::symlink("/etc/passwd", repo.join("passwd-link")).unwrap();
    // A link whose target is a token, and a file whose name is one.
    std::os::unix::fs::symlink(format!("ghp_{}", "a".repeat(36)), repo.join("current")).unwrap();
    write(
        repo,
        &format!("ghp_{}.txt", "b".repeat(36)),
        b"harmless words\n",
    );
    // Secret names git lists as they are: a leading `-`, a newline.
    write(repo, "-prod.env", b"needle=1\n");
    write(repo, "odd\nname.env", b"needle=2\n");
    git(repo, &["add", "-A"]);
    // A submodule entry (a gitlink) without a submodule repository.
    git(
        repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{},vendor", "1".repeat(40)),
        ],
    );
    git(repo, &["commit", "-qm", "init"]);
    // Not committed: an untracked file and an ignored one.
    write(repo, ".gitignore.local", b"x\n");
    write(repo, "untracked.md", b"needle untracked\n");
    write(repo, ".git/info/exclude", b"ignored.md\n");
    write(repo, "ignored.md", b"needle ignored\n");
}

async fn start(edit: impl FnOnce(&mut Config, &Path)) -> TestHub {
    start_git(edit, None).await
}

/// [`start`] with the hub running the git at `git_bin`.
async fn start_git(edit: impl FnOnce(&mut Config, &Path), git_bin: Option<PathBuf>) -> TestHub {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::with_home(dir.path());
    let listener = server::bind(&paths.socket).await.unwrap();
    let repo = dir.path().join("code/api");
    make_repo(&repo);
    let mut config = Config {
        default_runtime: "fake".into(),
        ..Default::default()
    };
    config.orchestrator.runtime = Some("claude-code".into());
    config.projects.insert(
        "api".into(),
        ProjectConfig {
            repo: repo.clone(),
            ..Default::default()
        },
    );
    edit(&mut config, dir.path());
    let (tx, spawns) = mpsc::unbounded_channel();
    let orch: Arc<dyn Adapter> = Arc::new(OrchAdapter(tx));
    let mut opts = HubOptions::new(paths.db_file(), config);
    opts.adapters = Arc::new(move |name: &str| -> Option<Arc<dyn Adapter>> {
        match name {
            "fake" => Some(Arc::new(FakeAdapter::new())),
            "claude-code" => Some(Arc::clone(&orch)),
            _ => None,
        }
    });
    opts.socket = Some(paths.socket.clone());
    opts.orchestrator_token_unverified = true;
    opts.git_bin = git_bin;
    let hub = Hub::open(opts).await.unwrap();
    let task = tokio::spawn(server::serve(
        hub.clone(),
        listener,
        paths.socket.clone(),
        false,
    ));
    TestHub {
        dir,
        socket: paths.socket,
        task,
        spawns,
        rt: None,
    }
}

impl TestHub {
    async fn client(&self, name: &str) -> Client {
        let mut c = Client::connect(&self.socket).await.unwrap();
        c.hello(json!({ "client": name })).await.unwrap();
        c
    }

    /// Starts the orchestrator (its first turn stays open) and connects as it.
    async fn orchestrator(&mut self) -> Client {
        let mut c = self.client("cli").await;
        c.call("say", json!({"message": "hello", "to": "orchestrator"}))
            .await
            .unwrap();
        let rt = timeout(WAIT, self.spawns.recv()).await.unwrap().unwrap();
        let path = rt.cfg.mcp_config_file.clone().unwrap();
        let mcp: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let token = mcp["mcpServers"]["twr"]["env"]["TWR_ORCH_TOKEN"]
            .as_str()
            .unwrap()
            .to_string();
        self.rt = Some(rt);
        let mut o = Client::connect(&self.socket).await.unwrap();
        // The token is armed once the runtime started.
        timeout(WAIT, async {
            loop {
                if o.hello(json!({"client": "orchestrator", "token": token}))
                    .await
                    .is_ok()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
                o = Client::connect(&self.socket).await.unwrap();
            }
        })
        .await
        .unwrap();
        o
    }

    /// The `project.read` events of the orchestrator's thread, oldest first.
    async fn reads(&self) -> Vec<Value> {
        let mut c = self.client("events").await;
        let r = c
            .call(
                "thread.events",
                json!({"thread": "orchestrator", "limit": 10000}),
            )
            .await
            .unwrap();
        r["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["kind"] == "project.read")
            .map(|e| e["data"].clone())
            .collect()
    }

    async fn stop(self) {
        let mut c = self.client("end").await;
        c.call("hub.shutdown", Value::Null).await.unwrap();
        self.task.await.unwrap().unwrap();
    }
}

fn err(e: anyhow::Error) -> RpcError {
    e.downcast_ref::<RpcError>()
        .cloned()
        .unwrap_or_else(|| panic!("not an RPC error: {e:#}"))
}

async fn refused(o: &mut Client, method: &str, p: Value) -> RpcError {
    err(o
        .call(method, p.clone())
        .await
        .expect_err(&format!("{method} {p}")))
}

#[tokio::test]
async fn reads_committed_files_and_never_the_work_tree() {
    let mut th = start(|_, _| {}).await;
    let mut o = th.orchestrator().await;
    let r = o
        .call(
            "project.read",
            json!({"project": "api", "path": "README.md"}),
        )
        .await
        .unwrap();
    assert_eq!(r["content"], "# api\n\nRun it with `make run`.\n");
    assert_eq!(r["project"], "api");
    assert_eq!(r["commit"].as_str().unwrap().len(), 40);
    // The top: committed entries only, secrets marked, a submodule and a symlink as such.
    let r = o
        .call("project.files", json!({"project": "api"}))
        .await
        .unwrap();
    let entries = r["entries"].as_array().unwrap();
    let names: Vec<&str> = entries
        .iter()
        .map(|e| e["name"].as_str().unwrap())
        .collect();
    assert!(
        names.contains(&"README.md") && names.contains(&"docs"),
        "{names:?}"
    );
    for absent in ["untracked.md", "ignored.md", ".gitignore.local"] {
        assert!(
            !names.contains(&absent),
            "{absent} is not committed: {names:?}"
        );
    }
    let entry = |n: &str| entries.iter().find(|e| e["name"] == n).unwrap().clone();
    assert!(entry(".env")["secret"].is_string());
    assert!(entry(".env.example").get("secret").is_none());
    assert_eq!(entry("vendor")["kind"], "submodule");
    assert_eq!(entry("passwd-link")["kind"], "symlink");
    assert_eq!(entry("docs")["kind"], "dir");
    // Untracked and ignored files do not exist for it.
    for path in ["untracked.md", "ignored.md"] {
        let e = refused(
            &mut o,
            "project.read",
            json!({"project": "api", "path": path}),
        )
        .await;
        assert_eq!(e.code, codes::NOT_FOUND, "{path}: {e:?}");
    }
    // A symlink is its target's name; nothing is followed.
    let r = o
        .call(
            "project.read",
            json!({"project": "api", "path": "passwd-link"}),
        )
        .await
        .unwrap();
    assert_eq!(r["content"], "symlink -> /etc/passwd");
    assert!(!r.to_string().contains("root:"));
    let e = refused(
        &mut o,
        "project.read",
        json!({"project": "api", "path": "vendor"}),
    )
    .await;
    assert!(e.message.contains("submodule"), "{e:?}");
    let e = refused(
        &mut o,
        "project.read",
        json!({"project": "api", "path": "docs"}),
    )
    .await;
    assert!(e.message.contains("directory"), "{e:?}");
    let r = o
        .call("project.files", json!({"project": "api", "path": "docs/"}))
        .await
        .unwrap();
    assert_eq!(r["entries"][0]["name"], "guide.md");
    th.stop().await;
}

#[tokio::test]
async fn secrets_are_refused_whole_and_never_searched() {
    let mut th = start(|c, _| c.orchestrator.read_deny = vec!["long.*".into()]).await;
    let mut o = th.orchestrator().await;
    for path in [
        ".env",
        "deploy/id_ed25519",
        "certs/prod.pem",
        "notes.txt",
        "long.txt",
    ] {
        let e = refused(
            &mut o,
            "project.read",
            json!({"project": "api", "path": path}),
        )
        .await;
        assert_eq!(e.code, codes::BAD_REQUEST, "{path}");
        assert!(
            e.message.contains("never shows") || e.message.contains("read_deny"),
            "{path}: {e:?}"
        );
        assert!(!e.message.contains("hunter2"));
    }
    for path in [".env.example", "deploy/id_ed25519.pub"] {
        o.call("project.read", json!({"project": "api", "path": path}))
            .await
            .unwrap();
    }
    // notes.txt has `needle` but also a key: its lines never come back.
    let r = o
        .call(
            "project.search",
            json!({"project": "api", "query": "needle"}),
        )
        .await
        .unwrap();
    let paths: Vec<&str> = r["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["path"].as_str().unwrap())
        .collect();
    assert!(!paths.contains(&"notes.txt"), "{paths:?}");
    assert!(!paths.contains(&"untracked.md") && !paths.contains(&"ignored.md"));
    let r = o
        .call(
            "project.search",
            json!({"project": "api", "query": "API_KEY"}),
        )
        .await
        .unwrap();
    let paths: Vec<&str> = r["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["path"].as_str().unwrap())
        .collect();
    assert!(paths.contains(&".env.example"), "{paths:?}");
    assert!(!paths.contains(&".env"), "{paths:?}");
    assert!(r.get("hidden").is_none(), "{r}");
    assert!(!r.to_string().contains("hunter2"));
    let r = o
        .call(
            "project.search",
            json!({"project": "api", "query": "OPENSSH"}),
        )
        .await
        .unwrap();
    assert_eq!(r["hits"], json!([]), "{r}");
    th.stop().await;
}

#[tokio::test]
async fn sizes_paths_and_queries_are_bounded() {
    let mut th = start(|_, _| {}).await;
    let mut o = th.orchestrator().await;
    let e = refused(
        &mut o,
        "project.read",
        json!({"project": "api", "path": "big.txt"}),
    )
    .await;
    assert!(e.message.contains("over the"), "{e:?}");
    let e = refused(
        &mut o,
        "project.read",
        json!({"project": "api", "path": "logo.bin"}),
    )
    .await;
    assert!(e.message.contains("binary"), "{e:?}");
    // A long file pages in 16 KB reads that add up to it.
    let mut offset = 0u64;
    let mut whole = String::new();
    loop {
        let r = o
            .call(
                "project.read",
                json!({"project": "api", "path": "long.txt", "offset": offset}),
            )
            .await
            .unwrap();
        let content = r["content"].as_str().unwrap();
        assert!(content.len() <= 16 * 1024);
        whole.push_str(content);
        offset = r["end"].as_u64().unwrap();
        if offset == r["total"].as_u64().unwrap() {
            break;
        }
    }
    assert_eq!(whole.lines().count(), 4000);
    for path in [
        "../x",
        "/etc/passwd",
        "-n",
        "a//b",
        "docs/../.env",
        "a\u{0}b",
    ] {
        let e = refused(
            &mut o,
            "project.read",
            json!({"project": "api", "path": path}),
        )
        .await;
        assert_eq!(e.code, codes::BAD_REQUEST, "{path:?}: {e:?}");
    }
    // A pathspec's magic is a name like any other.
    let e = refused(
        &mut o,
        "project.read",
        json!({"project": "api", "path": ":(glob)*"}),
    )
    .await;
    assert_eq!(e.code, codes::NOT_FOUND, "{e:?}");
    // Searches stop at 50 lines, 5 a file.
    let r = o
        .call(
            "project.search",
            json!({"project": "api", "query": "needle"}),
        )
        .await
        .unwrap();
    assert_eq!(r["hits"].as_array().unwrap().len(), 50, "{r}");
    assert_eq!(r["more"], true);
    for q in ["", "a\nb", &"q".repeat(201)] {
        let e = refused(
            &mut o,
            "project.search",
            json!({"project": "api", "query": q}),
        )
        .await;
        assert_eq!(e.code, codes::BAD_REQUEST, "{q:?}");
    }
    // Only the projects twr knows.
    let e = refused(
        &mut o,
        "project.read",
        json!({"project": "nope", "path": "README.md"}),
    )
    .await;
    assert_eq!(e.code, codes::NOT_FOUND, "{e:?}");
    th.stop().await;
}

#[tokio::test]
async fn a_worker_branch_is_read_by_its_thread() {
    let mut th = start(|_, _| {}).await;
    let mut c = th.client("cli").await;
    let w = c
        .call(
            "thread.new",
            json!({"project": "api", "runtime": "fake", "name": "w1"}),
        )
        .await
        .unwrap();
    let wt = PathBuf::from(w["worktree"].as_str().unwrap());
    write(&wt, "CHANGES.md", b"what w1 did\n");
    git(&wt, &["add", "CHANGES.md"]);
    git(&wt, &["commit", "-qm", "w1"]);
    let mut o = th.orchestrator().await;
    let r = o
        .call(
            "project.read",
            json!({"project": "api", "path": "CHANGES.md", "thread": "w1"}),
        )
        .await
        .unwrap();
    assert_eq!(r["content"], "what w1 did\n");
    assert_eq!(r["branch_of"], "w1");
    let e = refused(
        &mut o,
        "project.read",
        json!({"project": "api", "path": "CHANGES.md"}),
    )
    .await;
    assert_eq!(e.code, codes::NOT_FOUND, "HEAD does not have it: {e:?}");
    // Not a worker of this project.
    let e = refused(
        &mut o,
        "project.read",
        json!({"project": "api", "path": "README.md", "thread": "orchestrator"}),
    )
    .await;
    assert!(e.message.contains("not a worker thread"), "{e:?}");
    // Every call is on the orchestrator's thread, served or refused.
    let reads = th.reads().await;
    assert_eq!(reads.len(), 3, "{reads:?}");
    assert_eq!(reads[0]["tool"], "read");
    assert_eq!(reads[0]["path"], "CHANGES.md");
    assert_eq!(reads[0]["branch_of"], "w1");
    assert_eq!(reads[0]["bytes"], 12);
    assert!(reads[0]["commit"].is_string() && reads[0].get("refused").is_none());
    assert!(reads[1]["refused"].is_string());
    assert!(reads[2]["refused"]
        .as_str()
        .unwrap()
        .contains("not a worker thread"));
    th.stop().await;
}

#[tokio::test]
async fn a_turn_has_a_budget_and_users_none() {
    let mut th = start(|_, _| {}).await;
    let mut o = th.orchestrator().await;
    // 256 KB: the 16 KB pages of long.txt (about 106 KB) twice, then a third time passes it.
    let mut served = 0;
    let mut stop = None;
    for i in 0..40 {
        let offset = (i % 7) * 16 * 1024;
        match o
            .call(
                "project.read",
                json!({"project": "api", "path": "long.txt", "offset": offset}),
            )
            .await
        {
            Ok(_) => served += 1,
            Err(e) => {
                stop = Some(err(e));
                break;
            }
        }
    }
    let e = stop.expect("the byte budget ends the turn's reads");
    assert!(e.message.contains("spawn a worker"), "{e:?}");
    assert!((15..=17).contains(&served), "{served}");
    // The call budget: 40 calls a turn, refused ones included.
    let calls = th.reads().await.len();
    for _ in calls..40 {
        let _ = o
            .call("project.read", json!({"project": "api", "path": ".env"}))
            .await;
    }
    let e = refused(&mut o, "project.files", json!({"project": "api"})).await;
    assert!(e.message.contains("40 project reads"), "{e:?}");
    // Calls past the budget are refused, and only the first refusal is recorded.
    for _ in 0..10 {
        refused(&mut o, "project.files", json!({"project": "api"})).await;
    }
    let reads = th.reads().await;
    assert_eq!(reads.len(), 41, "{}", reads.len());
    assert!(reads[40]["refused"]
        .as_str()
        .unwrap()
        .contains("40 project reads"));
    // A user's reads are neither counted nor recorded.
    let before = reads.len();
    let mut c = th.client("cli").await;
    for _ in 0..45 {
        c.call("project.files", json!({"project": "api"}))
            .await
            .unwrap();
    }
    assert_eq!(th.reads().await.len(), before);
    th.stop().await;
}

#[tokio::test]
async fn reads_can_be_turned_off_and_dangerous_repositories_are_refused() {
    let mut th = start(|c, _| c.orchestrator.project_reads = false).await;
    let mut o = th.orchestrator().await;
    let e = refused(
        &mut o,
        "project.read",
        json!({"project": "api", "path": "README.md"}),
    )
    .await;
    assert_eq!(e.code, codes::UNAUTHORIZED);
    assert!(e.message.contains("project_reads"), "{e:?}");
    assert_eq!(th.reads().await.len(), 1, "a refusal is shown too");
    th.stop().await;

    // A repository that holds twr's data directory, and a project on a remote box.
    let mut th = start(|c, home| {
        git(home, &["init", "-q"]);
        write(home, "x.md", b"x\n");
        git(home, &["add", "x.md"]);
        git(home, &["commit", "-qm", "x"]);
        c.projects.insert(
            "home".into(),
            ProjectConfig {
                repo: home.to_path_buf(),
                ..Default::default()
            },
        );
        c.boxes.insert(
            "gpu".into(),
            BoxConfig {
                token: Some("t".repeat(32)),
                ..Default::default()
            },
        );
        c.projects.insert(
            "far".into(),
            ProjectConfig {
                repo: "/srv/far".into(),
                box_name: Some("gpu".into()),
                ..Default::default()
            },
        );
    })
    .await;
    let mut o = th.orchestrator().await;
    let e = refused(
        &mut o,
        "project.read",
        json!({"project": "home", "path": "x.md"}),
    )
    .await;
    assert!(e.message.contains("does not read it"), "{e:?}");
    let e = refused(&mut o, "project.files", json!({"project": "far"})).await;
    assert!(e.message.contains("remote box"), "{e:?}");
    let _ = &th.dir;
    th.stop().await;
}

#[tokio::test]
async fn a_key_files_lines_never_come_back_from_a_search() {
    let mut th = start(|_, _| {}).await;
    let mut o = th.orchestrator().await;
    // notes.txt holds a private key: its base64 lines look like nothing, yet stay hidden.
    let r = o
        .call(
            "project.search",
            json!({"project": "api", "query": "b3Bl", "path": "notes.txt"}),
        )
        .await
        .unwrap();
    assert_eq!(r["hits"], json!([]), "{r}");
    // And the answer is the same as for a string the file does not hold: no count, no flag
    // says a file twr never shows matched.
    let miss = o
        .call(
            "project.search",
            json!({"project": "api", "query": "zzzz-nowhere", "path": "notes.txt"}),
        )
        .await
        .unwrap();
    let strip = |mut v: Value| {
        v.as_object_mut().unwrap().remove("query");
        v
    };
    assert_eq!(strip(r), strip(miss));
    for (hit, miss) in [("API_KEY=h", "API_KEY=z")] {
        let a = o
            .call(
                "project.search",
                json!({"project": "api", "query": hit, "path": ".env"}),
            )
            .await
            .unwrap();
        let b = o
            .call(
                "project.search",
                json!({"project": "api", "query": miss, "path": ".env"}),
            )
            .await
            .unwrap();
        assert_eq!(
            strip(a),
            strip(b),
            "a guess at .env's content is not confirmed"
        );
    }
    // So do a big file's and a link's.
    let r = o
        .call(
            "project.search",
            json!({"project": "api", "query": "xxxx", "path": "big.txt"}),
        )
        .await
        .unwrap();
    assert_eq!(r["hits"], json!([]), "{r}");
    // A link's target is content: a token there is refused like one in a file.
    let e = refused(
        &mut o,
        "project.read",
        json!({"project": "api", "path": "current"}),
    )
    .await;
    assert!(e.message.contains("GitHub token"), "{e:?}");
    assert!(!e.message.contains("ghp_"), "{e:?}");
    th.stop().await;
}

#[tokio::test]
async fn files_a_read_refuses_never_count_toward_a_searchs_lines() {
    // Files with ordinary names whose content a read refuses, more of them than the lines
    // git returns for one search, each with as many matching lines as a file shows.
    let keys = |repo: &Path, n: usize, extra: &[(&str, &str)]| {
        std::fs::create_dir_all(repo).unwrap();
        git(repo, &["init", "-q"]);
        for i in 0..n {
            let body = format!("{}\n{}", key_line(), "needle\n".repeat(5));
            write(repo, &format!("k{i:03}.txt"), body.as_bytes());
        }
        for (path, text) in extra {
            write(repo, path, text.as_bytes());
        }
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "-qm", "keys"]);
    };
    let mut th = start(|c, home| {
        let alone = home.join("code/alone");
        keys(&alone, 16, &[]);
        project(c, "alone", &alone);
        // And a file a read serves, found after them.
        let after = home.join("code/after");
        keys(&after, 16, &[("zz.txt", "the needle here\n")]);
        project(c, "after", &after);
        // More than the searches twr runs to leave them out.
        let flood = home.join("code/flood");
        keys(&flood, 200, &[]);
        project(c, "flood", &flood);
    })
    .await;
    let mut o = th.orchestrator().await;
    let strip = |mut v: Value| {
        v.as_object_mut().unwrap().remove("query");
        v
    };
    let mut search = async |project: &str, query: &str| {
        o.call(
            "project.search",
            json!({"project": project, "query": query}),
        )
        .await
        .unwrap()
    };
    let hit = search("alone", "needle").await;
    let miss = search("alone", "zzzz-nowhere").await;
    assert_eq!(hit["more"], json!(false), "{hit}");
    assert_eq!(strip(hit), strip(miss));
    let r = search("after", "needle").await;
    assert_eq!(
        r["hits"],
        json!([{"path": "zz.txt", "line": 1, "text": "the needle here"}]),
        "{r}"
    );
    assert_eq!(r["more"], json!(false), "{r}");
    // Past the searches it runs, twr refuses rather than answer from a window they fill.
    let e = refused(
        &mut o,
        "project.search",
        json!({"project": "flood", "query": "needle"}),
    )
    .await;
    assert!(e.message.contains("narrow the search"), "{e:?}");
    th.stop().await;
}

#[tokio::test]
async fn git_output_never_reaches_a_refusal() {
    // A git whose grep prints a secret and fails without a word on stderr (as a killed one
    // can): the refusal and its event carry none of what it printed.
    let bin_dir = tempfile::tempdir().unwrap();
    let bin = bin_dir.path().join("git");
    install(
        &bin,
        "#!/bin/sh\nfor a in \"$@\"; do\n  if [ \"$a\" = grep ]; then\n    printf 'x:.env\\0001\\0API_KEY=hunter2\\n'\n    exit 2\n  fi\ndone\nexec git \"$@\"\n",
    );
    let mut th = start_git(|_, _| {}, Some(bin)).await;
    let mut o = th.orchestrator().await;
    let e = refused(
        &mut o,
        "project.search",
        json!({"project": "api", "query": "API_KEY"}),
    )
    .await;
    assert!(!e.message.contains("hunter2"), "{e:?}");
    assert!(e.message.contains("git could not search"), "{e:?}");
    let reads = th.reads().await;
    assert!(
        !serde_json::to_string(&reads).unwrap().contains("hunter2"),
        "{reads:?}"
    );
    th.stop().await;
}

#[tokio::test]
async fn a_repository_whose_git_directory_is_elsewhere_is_refused() {
    let mut th = start(|c, home| {
        // `redir`'s .git file points at another repository's git directory.
        let other = home.join("other");
        std::fs::create_dir_all(&other).unwrap();
        git(&other, &["init", "-q"]);
        write(&other, "secret-notes.md", b"other repository\n");
        git(&other, &["add", "-A"]);
        git(&other, &["commit", "-qm", "o"]);
        let redir = home.join("code/redir");
        std::fs::create_dir_all(&redir).unwrap();
        std::fs::write(
            redir.join(".git"),
            format!("gitdir: {}\n", other.join(".git").display()),
        )
        .unwrap();
        c.projects.insert(
            "redir".into(),
            ProjectConfig {
                repo: redir,
                ..Default::default()
            },
        );
    })
    .await;
    let mut o = th.orchestrator().await;
    let e = refused(&mut o, "project.files", json!({"project": "redir"})).await;
    assert!(e.message.contains("git directory"), "{e:?}");
    th.stop().await;
}

#[tokio::test]
async fn long_names_and_refusals_are_bounded() {
    let mut th = start(|_, _| {}).await;
    let mut o = th.orchestrator().await;
    let long = "p".repeat(100_000);
    let e = refused(
        &mut o,
        "project.read",
        json!({"project": long, "path": "README.md"}),
    )
    .await;
    assert!(e.message.len() <= 1024, "{}", e.message.len());
    let e = refused(
        &mut o,
        "project.read",
        json!({"project": "api", "path": "README.md", "thread": long}),
    )
    .await;
    assert!(e.message.len() <= 1024, "{}", e.message.len());
    for r in th.reads().await {
        assert!(r.to_string().len() < 4096, "{r}");
    }
    th.stop().await;
}

/// A git that logs every command line to `log` and runs the real git, failing `rev-parse
/// --show-toplevel` with a message holding a secret when `fail_toplevel`.
fn logging_git(dir: &Path, log: &Path, fail_toplevel: bool) -> PathBuf {
    let bin = dir.join("git");
    let fail = if fail_toplevel {
        "for a in \"$@\"; do\n  if [ \"$a\" = --show-toplevel ]; then\n    echo \"fatal: bad boolean config value 'hunter2' for 'core.bare'\" >&2\n    exit 128\n  fi\ndone\n"
    } else {
        ""
    };
    install(
        &bin,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n{fail}exec git \"$@\"\n",
            log.display()
        ),
    );
    bin
}

#[tokio::test]
async fn files_twr_never_shows_never_reach_git_grep() {
    let bin_dir = tempfile::tempdir().unwrap();
    let log = bin_dir.path().join("log");
    let bin = logging_git(bin_dir.path(), &log, false);
    let mut th = start_git(|_, _| {}, Some(bin)).await;
    let mut o = th.orchestrator().await;
    o.call(
        "project.search",
        json!({"project": "api", "query": "needle"}),
    )
    .await
    .unwrap();
    let log = std::fs::read_to_string(&log).unwrap();
    let grep = log
        .lines()
        .find(|l| l.contains(" grep "))
        .expect("a git grep");
    for path in ["big.txt", ".env", "deploy/id_ed25519", "certs/prod.pem"] {
        assert!(
            grep.contains(&format!(":(exclude,literal){path}")),
            "{path}: {grep}"
        );
    }
    // Every read runs in the git directory, never finding `.git` in the work tree again.
    for l in log
        .lines()
        .filter(|l| l.contains("cat-file") || l.contains(" grep "))
    {
        assert!(l.contains("--git-dir="), "{l}");
    }
    assert!(grep.contains(":(exclude,literal)-prod.env"), "{grep}");
    th.stop().await;
}

#[tokio::test]
async fn repository_lookup_errors_never_carry_gits_output() {
    let bin_dir = tempfile::tempdir().unwrap();
    let log = bin_dir.path().join("log");
    let bin = logging_git(bin_dir.path(), &log, true);
    let mut th = start_git(|_, _| {}, Some(bin)).await;
    let mut o = th.orchestrator().await;
    let e = refused(
        &mut o,
        "project.read",
        json!({"project": "api", "path": "README.md"}),
    )
    .await;
    assert!(!e.message.contains("hunter2"), "{e:?}");
    assert!(!serde_json::to_string(&th.reads().await)
        .unwrap()
        .contains("hunter2"));
    th.stop().await;
}

#[tokio::test]
async fn names_holding_tokens_are_never_shown_or_recorded() {
    let mut th = start(|c, home| {
        let link = home.join("code/linked-api");
        std::os::unix::fs::symlink(home.join("code/api"), &link).unwrap();
        c.projects.insert(
            "linked".into(),
            ProjectConfig {
                repo: link,
                ..Default::default()
            },
        );
    })
    .await;
    let mut o = th.orchestrator().await;
    let token = format!("ghp_{}", "b".repeat(36));
    let r = o
        .call("project.files", json!({"project": "api"}))
        .await
        .unwrap();
    assert!(!r.to_string().contains(&token), "{r}");
    assert!(
        r.to_string().contains("[redacted: holds a GitHub token]"),
        "{r}"
    );
    let r = o
        .call(
            "project.search",
            json!({"project": "api", "query": "harmless"}),
        )
        .await
        .unwrap();
    assert_eq!(r["hits"], json!([]), "{r}");
    let e = refused(
        &mut o,
        "project.read",
        json!({"project": "api", "path": format!("{token}.txt")}),
    )
    .await;
    assert!(!e.message.contains(&token), "{e:?}");
    let e = refused(
        &mut o,
        "project.search",
        json!({"project": "api", "query": token}),
    )
    .await;
    assert!(e.message.contains("neither looks for"), "{e:?}");
    // A configured repository that is a symbolic link could be pointed anywhere.
    let e = refused(&mut o, "project.files", json!({"project": "linked"})).await;
    assert!(e.message.contains("symbolic link"), "{e:?}");
    let reads = serde_json::to_string(&th.reads().await).unwrap();
    assert!(!reads.contains(&token), "{reads}");
    th.stop().await;
}

#[tokio::test]
async fn git_never_finds_another_repository_from_the_git_directory() {
    // A `.git` inside api's git directory pointing at another repository, and a git that
    // only runs bare repositories it is told about.
    let bin_dir = tempfile::tempdir().unwrap();
    let bin = bin_dir.path().join("git");
    install(
        &bin,
        "#!/bin/sh\nexec git -c safe.bareRepository=explicit \"$@\"\n",
    );
    let mut th = start_git(
        |_, home| {
            let other = home.join("other");
            std::fs::create_dir_all(&other).unwrap();
            git(&other, &["init", "-q"]);
            write(&other, "README.md", b"the other repository\n");
            git(&other, &["add", "-A"]);
            git(&other, &["commit", "-qm", "o"]);
            std::fs::write(
                home.join("code/api/.git/.git"),
                format!("gitdir: {}\n", other.join(".git").display()),
            )
            .unwrap();
        },
        Some(bin),
    )
    .await;
    let mut o = th.orchestrator().await;
    let r = o
        .call(
            "project.read",
            json!({"project": "api", "path": "README.md"}),
        )
        .await
        .unwrap();
    assert_eq!(r["content"], "# api\n\nRun it with `make run`.\n");
    let r = o
        .call(
            "project.search",
            json!({"project": "api", "query": "other repository"}),
        )
        .await
        .unwrap();
    assert_eq!(r["hits"], json!([]), "{r}");
    th.stop().await;
}

#[tokio::test]
async fn a_token_cut_short_is_still_redacted() {
    let mut th = start(|_, _| {}).await;
    let mut o = th.orchestrator().await;
    // The token starts near the 512 bytes an event keeps of a path.
    let path = format!(
        "{}/{}/ghp_{}.txt",
        "a".repeat(240),
        "b".repeat(231),
        "c".repeat(36)
    );
    refused(
        &mut o,
        "project.read",
        json!({"project": "api", "path": path}),
    )
    .await;
    let reads = serde_json::to_string(&th.reads().await).unwrap();
    assert!(!reads.contains("ghp_"), "{reads}");
    assert!(!reads.contains("cccccccc"), "{reads}");
    th.stop().await;
}

/// A repository at `repo` with files named by raw bytes (`name`, `content`), committed.
fn byte_named_repo(repo: &Path, files: &[(&[u8], String)]) {
    use std::os::unix::ffi::OsStrExt;
    std::fs::create_dir_all(repo).unwrap();
    git(repo, &["init", "-q"]);
    for (name, content) in files {
        std::fs::write(repo.join(std::ffi::OsStr::from_bytes(name)), content).unwrap();
    }
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-qm", "bytes"]);
}

#[cfg_attr(
    target_os = "macos",
    ignore = "APFS refuses file names that are not UTF-8"
)]
#[tokio::test]
async fn names_that_are_not_utf8_never_hide_a_secret() {
    let mut th = start(|c, home| {
        // An innocent file, and a key file whose name reads the same once its invalid byte is
        // replaced; the key's body lines look like nothing.
        let alike = home.join("code/alike");
        byte_named_repo(
            &alike,
            &[
                ("notes\u{FFFD}.txt".as_bytes(), "MIIinnocent\n".into()),
                (b"notes\xff.txt", format!("{}\nMIIkeybody\n", key_line())),
            ],
        );
        // Files with secret names twr cannot name in a pathspec.
        let envs = home.join("code/envs");
        byte_named_repo(
            &envs,
            &[
                (b".env.\xff1", "API_KEY=guess\n".into()),
                (b"readme.md", "API_KEY docs\n".into()),
            ],
        );
        for (name, repo) in [("alike", alike), ("envs", envs)] {
            c.projects.insert(
                name.into(),
                ProjectConfig {
                    repo,
                    ..Default::default()
                },
            );
        }
    })
    .await;
    let mut o = th.orchestrator().await;
    // The key file cannot be left out by name, nor its lines told apart from the innocent
    // file's: the search is refused, whatever it looks for (its answer says nothing about
    // what the key file holds).
    for query in ["MII", "zzzz-nowhere"] {
        let e = refused(
            &mut o,
            "project.search",
            json!({"project": "alike", "query": query}),
        )
        .await;
        assert!(e.message.contains("not UTF-8"), "{query}: {e:?}");
    }
    // The innocent file reads as itself.
    let r = o
        .call(
            "project.read",
            json!({"project": "alike", "path": "notes\u{FFFD}.txt"}),
        )
        .await
        .unwrap();
    assert_eq!(r["content"], "MIIinnocent\n", "{r}");
    let e = refused(
        &mut o,
        "project.search",
        json!({"project": "envs", "query": "API_KEY"}),
    )
    .await;
    assert!(e.message.contains("not UTF-8"), "{e:?}");
    th.stop().await;
}

/// Lines of files whose names are not UTF-8 are never shown: when they fill most of the lines
/// git returns, `more` still says only whether a line twr would show matched, never whether a
/// file a read refuses (found past those lines, unchecked) did.
#[cfg_attr(
    target_os = "macos",
    ignore = "APFS refuses file names that are not UTF-8"
)]
#[tokio::test]
async fn unshown_lines_never_leave_more_to_an_unchecked_file() {
    let repo = |dir: &Path, last: String| {
        let mut files: Vec<(Vec<u8>, String)> = Vec::new();
        // 25 lines first that are never shown, then 50 that are: the lines git returns for
        // one search, and nothing after them but `c.txt`.
        for i in 0..5u8 {
            files.push((vec![b'a', 0xff, b'0' + i], "needle\n".repeat(5)));
        }
        for i in 0..10 {
            files.push((format!("b{i:02}.txt").into_bytes(), "needle\n".repeat(5)));
        }
        files.push((b"c.txt".to_vec(), last));
        let files: Vec<(&[u8], String)> = files
            .iter()
            .map(|(n, c)| (n.as_slice(), c.clone()))
            .collect();
        byte_named_repo(dir, &files);
    };
    let mut th = start(|c, home| {
        let keyed = home.join("code/keyed");
        repo(&keyed, format!("{}\nneedle\n", key_line()));
        project(c, "keyed", &keyed);
        let plain = home.join("code/plain");
        repo(&plain, "needle\n".into());
        project(c, "plain", &plain);
    })
    .await;
    let mut o = th.orchestrator().await;
    let mut search = async |project: &str| {
        o.call(
            "project.search",
            json!({"project": project, "query": "needle"}),
        )
        .await
        .unwrap()
    };
    let keyed = search("keyed").await;
    assert_eq!(keyed["hits"].as_array().unwrap().len(), 50, "{keyed}");
    assert_eq!(keyed["more"], json!(false), "{keyed}");
    let plain = search("plain").await;
    assert_eq!(plain["hits"].as_array().unwrap().len(), 50, "{plain}");
    assert_eq!(plain["more"], json!(true), "{plain}");
    th.stop().await;
}

/// A configured project `name` at `repo`.
fn project(c: &mut Config, name: &str, repo: &Path) {
    c.projects.insert(
        name.into(),
        ProjectConfig {
            repo: repo.to_path_buf(),
            ..Default::default()
        },
    );
}

/// A repository at `dir` with one commit of `README.md` = `text`.
fn one_file_repo(dir: &Path, text: &str) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q"]);
    write(dir, "README.md", text.as_bytes());
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-qm", "one"]);
}

#[tokio::test]
async fn a_git_directory_that_sends_git_elsewhere_is_refused() {
    let mut th = start(|c, home| {
        let other = home.join("other");
        one_file_repo(&other, "the other repository\n");
        // A commondir inside the git directory whose own commondir is the other repository.
        let nested = home.join("code/nested");
        one_file_repo(&nested, "nested\n");
        let common = nested.join(".git/common");
        std::fs::create_dir_all(common.join("objects")).unwrap();
        std::fs::create_dir_all(common.join("refs")).unwrap();
        let branch = git(&other, &["symbolic-ref", "HEAD"]);
        std::fs::write(common.join("HEAD"), format!("ref: {branch}\n")).unwrap();
        std::fs::write(
            common.join("commondir"),
            format!("{}\n", other.join(".git").display()),
        )
        .unwrap();
        std::fs::write(nested.join(".git/commondir"), "common\n").unwrap();
        project(c, "nested", &nested);
        // Alternates borrowing the other repository's objects.
        let borrow = home.join("code/borrow");
        one_file_repo(&borrow, "borrow\n");
        std::fs::write(
            borrow.join(".git/objects/info/alternates"),
            format!("{}\n", other.join(".git/objects").display()),
        )
        .unwrap();
        project(c, "borrow", &borrow);
    })
    .await;
    let mut o = th.orchestrator().await;
    for p in ["nested", "borrow"] {
        let e = refused(
            &mut o,
            "project.read",
            json!({"project": p, "path": "README.md"}),
        )
        .await;
        assert!(e.message.contains("takes git elsewhere"), "{p}: {e:?}");
    }
    th.stop().await;
}

#[tokio::test]
async fn object_storage_linked_to_another_repository_is_refused() {
    let mut th = start(|c, home| {
        // The repository the links lead to: loose objects, and a copy of it packed.
        let other = home.join("other");
        one_file_repo(&other, "the other repository\n");
        let commit = git(&other, &["rev-parse", "HEAD"]);
        let packed = home.join("packed");
        one_file_repo(&packed, "the packed repository\n");
        git(&packed, &["repack", "-qad"]);
        let packed_commit = git(&packed, &["rev-parse", "HEAD"]);
        let objects = |r: &Path| r.join(".git/objects");
        // The whole objects folder a link to the other's, the branch at the other's commit.
        let whole = home.join("code/whole");
        one_file_repo(&whole, "whole\n");
        std::fs::remove_dir_all(objects(&whole)).unwrap();
        std::os::unix::fs::symlink(objects(&other), objects(&whole)).unwrap();
        git(&whole, &["update-ref", "refs/heads/main", &commit]);
        project(c, "whole", &whole);
        // Each of the other's loose objects linked in one by one.
        let loose = home.join("code/loose");
        one_file_repo(&loose, "loose\n");
        for d in std::fs::read_dir(objects(&other)).unwrap() {
            let d = d.unwrap();
            let name = d.file_name();
            if name.len() != 2 {
                continue;
            }
            let mine = objects(&loose).join(&name);
            std::fs::create_dir_all(&mine).unwrap();
            for f in std::fs::read_dir(d.path()).unwrap() {
                let f = f.unwrap();
                std::os::unix::fs::symlink(f.path(), mine.join(f.file_name())).unwrap();
            }
        }
        git(&loose, &["update-ref", "refs/heads/main", &commit]);
        project(c, "loose", &loose);
        // The packed repository's pack and index linked into the pack folder.
        let pack = home.join("code/pack");
        one_file_repo(&pack, "pack\n");
        let packs = objects(&packed).join("pack");
        for f in std::fs::read_dir(&packs).unwrap() {
            let f = f.unwrap();
            let to = objects(&pack).join("pack").join(f.file_name());
            std::os::unix::fs::symlink(f.path(), to).unwrap();
        }
        git(&pack, &["update-ref", "refs/heads/main", &packed_commit]);
        project(c, "pack", &pack);
    })
    .await;
    let mut o = th.orchestrator().await;
    for p in ["whole", "loose", "pack"] {
        for (method, params) in [
            ("project.read", json!({"project": p, "path": "README.md"})),
            ("project.files", json!({"project": p})),
            (
                "project.search",
                json!({"project": p, "query": "repository"}),
            ),
        ] {
            let e = refused(&mut o, method, params).await;
            assert!(
                e.message.contains("symbolic link") && e.message.contains("elsewhere"),
                "{p} {method}: {e:?}"
            );
            assert!(!e.message.contains("other repository"), "{e:?}");
        }
    }
    th.stop().await;
}

/// Alternates added after the checks before a read (a regular file, which stays) are found
/// by the checks after it: the read fails, and nothing of the other repository comes back.
#[tokio::test]
async fn alternates_added_while_a_read_runs_fail_it() {
    let bin_dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let other = home.path().join("other");
    one_file_repo(&other, "the other repository\n");
    let commit = git(&other, &["rev-parse", "HEAD"]);
    let late = home.path().join("late");
    one_file_repo(&late, "late\n");
    // Its branch at the other's commit, which it does not have (yet).
    std::fs::write(late.join(".git/refs/heads/main"), format!("{commit}\n")).unwrap();
    // A git that adds alternates to the other's objects as the commit is looked up: after
    // every check before the read.
    let alternates = late.join(".git/objects/info/alternates");
    let bin = bin_dir.path().join("git");
    install(
        &bin,
        format!(
            "#!/bin/sh\nfor a in \"$@\"; do\n  if [ \"$a\" = --verify ]; then\n    \
             printf '%s\\n' '{}' > '{}'\n  fi\ndone\nexec git \"$@\"\n",
            other.join(".git/objects").display(),
            alternates.display()
        ),
    );
    let mut th = start_git(|c, _| project(c, "late", &late), Some(bin)).await;
    let mut o = th.orchestrator().await;
    let e = refused(
        &mut o,
        "project.read",
        json!({"project": "late", "path": "README.md"}),
    )
    .await;
    assert!(alternates.exists(), "the git wrapper ran");
    assert!(e.message.contains("takes git elsewhere"), "{e:?}");
    assert!(!e.message.contains("other repository"), "{e:?}");
    th.stop().await;
}

#[cfg_attr(
    target_os = "macos",
    ignore = "APFS refuses file names that are not UTF-8"
)]
#[tokio::test]
async fn a_binary_file_whose_name_is_not_utf8_refuses_a_search() {
    let mut th = start(|c, home| {
        // Searched as text whatever it holds (`diff`), so its matches would count.
        let bin = home.join("code/bin");
        byte_named_repo(
            &bin,
            &[
                (b".gitattributes", "* diff\n".into()),
                (b"img\xff.bin", "\0needle\nneedle\n".into()),
                (b"readme.md", "nothing here\n".into()),
            ],
        );
        project(c, "bin", &bin);
    })
    .await;
    let mut o = th.orchestrator().await;
    for query in ["needle", "zzzz-nowhere"] {
        let e = refused(
            &mut o,
            "project.search",
            json!({"project": "bin", "query": query}),
        )
        .await;
        assert!(e.message.contains("not UTF-8"), "{query}: {e:?}");
    }
    th.stop().await;
}

#[tokio::test]
async fn a_path_the_commit_holds_twice_is_neither_read_nor_searched() {
    let mut th = start(|c, home| {
        let dup = home.join("code/dup");
        std::fs::create_dir_all(&dup).unwrap();
        git(&dup, &["init", "-q"]);
        let blob = |text: &str| {
            let out = Command::new("git")
                .args(["hash-object", "-w", "--stdin"])
                .current_dir(&dup)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .and_then(|mut child| {
                    use std::io::Write;
                    child.stdin.take().unwrap().write_all(text.as_bytes())?;
                    child.wait_with_output()
                })
                .unwrap();
            String::from_utf8(out.stdout).unwrap().trim().to_string()
        };
        let harmless = blob("MIIharmless\n");
        let key = blob(&format!("{}\nMIIkeybody\n", key_line()));
        // A tree holding notes.txt twice, which `git mktree` would refuse.
        let mut tree = Vec::new();
        for oid in [&harmless, &key] {
            tree.extend_from_slice(b"100644 notes.txt\0");
            tree.extend((0..20).map(|i| u8::from_str_radix(&oid[2 * i..2 * i + 2], 16).unwrap()));
        }
        std::fs::write(dup.join("tree.bin"), &tree).unwrap();
        let tree = git(
            &dup,
            &["hash-object", "-t", "tree", "-w", "--literally", "tree.bin"],
        );
        std::fs::remove_file(dup.join("tree.bin")).unwrap();
        let commit = git(&dup, &["commit-tree", &tree, "-m", "dup"]);
        let branch = git(&dup, &["symbolic-ref", "HEAD"]);
        git(&dup, &["update-ref", &branch, &commit]);
        project(c, "dup", &dup);
    })
    .await;
    let mut o = th.orchestrator().await;
    let e = refused(
        &mut o,
        "project.search",
        json!({"project": "dup", "query": "MII"}),
    )
    .await;
    assert!(e.message.contains("one path twice"), "{e:?}");
    let e = refused(
        &mut o,
        "project.read",
        json!({"project": "dup", "path": "notes.txt"}),
    )
    .await;
    assert!(!e.message.contains("MII"), "{e:?}");
    th.stop().await;
}

#[tokio::test]
async fn search_ignores_git_config_that_changes_what_it_reads() {
    // A user's config that recurses into submodules and prints match columns.
    let bin_dir = tempfile::tempdir().unwrap();
    let bin = bin_dir.path().join("git");
    let log = bin_dir.path().join("log");
    install(
        &bin,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexec git -c submodule.recurse=true -c \
             grep.column=true \"$@\"\n",
            log.display()
        ),
    );
    let mut th = start_git(
        |c, home| {
            let sub = home.join("sub");
            one_file_repo(&sub, "subneedle\n");
            let outer = home.join("code/outer");
            one_file_repo(&outer, "outer\n");
            git(
                &outer,
                &[
                    "-c",
                    "protocol.file.allow=always",
                    "submodule",
                    "add",
                    "-q",
                    sub.to_str().unwrap(),
                    "sub",
                ],
            );
            git(&outer, &["commit", "-qm", "sub"]);
            project(c, "outer", &outer);
        },
        Some(bin),
    )
    .await;
    let mut o = th.orchestrator().await;
    let r = o
        .call(
            "project.search",
            json!({"project": "api", "query": "make run"}),
        )
        .await
        .unwrap();
    assert_eq!(r["hits"][0]["text"], "Run it with `make run`.", "{r}");
    let r = o
        .call(
            "project.search",
            json!({"project": "outer", "query": "subneedle"}),
        )
        .await
        .unwrap();
    assert_eq!(r["hits"], json!([]), "{r}");
    // A submodule's lines would be dropped anyway (it is no file of the commit), but its
    // matches would count toward `more`: git must not look there at all.
    let log = std::fs::read_to_string(&log).unwrap();
    for l in log.lines().filter(|l| l.contains(" grep ")) {
        assert!(l.contains("--no-recurse-submodules"), "{l}");
        // Nor start the threads a repository's grep.threads asks for.
        assert!(l.contains("--threads=2"), "{l}");
    }
    th.stop().await;
}

/// Writes `text` as an object of `kind` into `repo` (bypassing git's checks), its id.
fn raw_object(repo: &Path, kind: &str, data: &[u8]) -> String {
    let tmp = repo.join(".obj.tmp");
    std::fs::write(&tmp, data).unwrap();
    let oid = git(
        repo,
        &["hash-object", "-t", kind, "-w", "--literally", ".obj.tmp"],
    );
    std::fs::remove_file(&tmp).unwrap();
    oid
}

/// A tree object of `(mode, name, oid)` entries, as raw bytes.
fn tree_bytes(entries: &[(&str, &str, &str)]) -> Vec<u8> {
    let mut tree = Vec::new();
    for (mode, name, oid) in entries {
        tree.extend_from_slice(format!("{mode} {name}\0").as_bytes());
        tree.extend((0..20).map(|i| u8::from_str_radix(&oid[2 * i..2 * i + 2], 16).unwrap()));
    }
    tree
}

/// Points `repo`'s current branch at a new commit of `tree`.
fn commit_tree(repo: &Path, tree: &str) {
    let commit = git(repo, &["commit-tree", tree, "-m", "crafted"]);
    let branch = git(repo, &["symbolic-ref", "HEAD"]);
    git(repo, &["update-ref", &branch, &commit]);
}

#[tokio::test]
async fn a_repository_cannot_make_a_read_fetch_or_run_anything() {
    let marker = tempfile::tempdir().unwrap();
    let ran = marker.path().join("RAN");
    let ran_s = ran.display().to_string();
    let mut th = start(move |c, home| {
        // A partial clone whose promisor remote is a command, allowed by the repository's own
        // config, and a commit naming a blob it lacks: git would fetch it lazily.
        let lazy = home.join("code/lazy");
        std::fs::create_dir_all(&lazy).unwrap();
        git(&lazy, &["init", "-q"]);
        let missing = "0123456789abcdef0123456789abcdef01234567";
        let tree = raw_object(
            &lazy,
            "tree",
            &tree_bytes(&[("100644", "README.md", missing)]),
        );
        commit_tree(&lazy, &tree);
        for (k, v) in [
            ("extensions.partialClone", "origin".to_string()),
            ("remote.origin.url", format!("ext::sh -c touch% {ran_s}")),
            ("remote.origin.promisor", "true".to_string()),
            ("protocol.ext.allow", "always".to_string()),
        ] {
            git(&lazy, &["config", k, &v]);
        }
        project(c, "lazy", &lazy);
        // A remote with a filter alone is a promisor remote to git too.
        let filtered = home.join("code/filtered");
        std::fs::create_dir_all(&filtered).unwrap();
        git(&filtered, &["init", "-q"]);
        let tree = raw_object(
            &filtered,
            "tree",
            &tree_bytes(&[("100644", "README.md", missing)]),
        );
        commit_tree(&filtered, &tree);
        for (k, v) in [
            ("remote.origin.url", format!("ext::sh -c touch% {ran_s}")),
            ("remote.origin.partialclonefilter", "blob:none".to_string()),
            ("remote.origin.promisor", "false".to_string()),
        ] {
            git(&filtered, &["config", k, &v]);
        }
        project(c, "filtered", &filtered);
    })
    .await;
    let mut o = th.orchestrator().await;
    let config = th.dir.path().join("code/lazy/.git/config");
    let before = std::fs::read(&config).unwrap();
    for name in ["lazy", "filtered"] {
        for (method, p) in [
            (
                "project.read",
                json!({"project": name, "path": "README.md"}),
            ),
            ("project.files", json!({"project": name})),
            ("project.search", json!({"project": name, "query": "x"})),
        ] {
            let e = refused(&mut o, method, p).await;
            assert!(
                e.message.contains("partial clone"),
                "{name} {method}: {e:?}"
            );
        }
    }
    assert!(
        !ran.exists(),
        "a read ran the repository's transport command"
    );
    // Nor did a fetch start: it writes to the config first (on a git without
    // GIT_NO_LAZY_FETCH), so a partial clone is refused before any object is read.
    assert_eq!(std::fs::read(&config).unwrap(), before);
    th.stop().await;
}

#[tokio::test]
async fn crafted_objects_never_pass_for_checked_files() {
    let mut th = start(|c, home| {
        let craft = home.join("code/craft");
        std::fs::create_dir_all(&craft).unwrap();
        git(&craft, &["init", "-q"]);
        // A file entry naming a tag whose own text holds a key, pointing at a clean blob.
        let clean = raw_object(&craft, "blob", b"MIIclean\n");
        let tag = raw_object(
            &craft,
            "tag",
            format!(
                "object {clean}\ntype blob\ntag v1\ntagger t <t@t> 0 +0000\n\n{}\nMIIkeybody\n",
                key_line()
            )
            .as_bytes(),
        );
        // And a token-named path twice.
        let token = format!("ghp_{}", "d".repeat(36));
        let tree = raw_object(
            &craft,
            "tree",
            &tree_bytes(&[("100644", &token, &clean), ("100644", &token, &clean)]),
        );
        let top = raw_object(
            &craft,
            "tree",
            &tree_bytes(&[("40000", "dup", &tree), ("100644", "notes.txt", &tag)]),
        );
        commit_tree(&craft, &top);
        project(c, "craft", &craft);
    })
    .await;
    let mut o = th.orchestrator().await;
    let r = o
        .call(
            "project.search",
            json!({"project": "craft", "query": "MIIkey", "path": "notes.txt"}),
        )
        .await
        .unwrap();
    assert_eq!(r["hits"], json!([]), "{r}");
    let e = refused(
        &mut o,
        "project.read",
        json!({"project": "craft", "path": "notes.txt"}),
    )
    .await;
    assert!(!e.message.contains("MII"), "{e:?}");
    let e = refused(
        &mut o,
        "project.search",
        json!({"project": "craft", "query": "MII"}),
    )
    .await;
    assert!(!e.message.contains("ghp_"), "{e:?}");
    let reads = serde_json::to_string(&th.reads().await).unwrap();
    assert!(!reads.contains("ghp_"), "{reads}");
    th.stop().await;
}
