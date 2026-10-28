//! Threads on accounts (`hub::thread_account`): what `thread.new` picks and records (explicit,
//! `<runtime>:<account>`, project pins, `outside_pin`), the account's environment on every
//! spawn, resume, attach and job (its variables last, its stray credentials removed), and the
//! refusals of a start, an attach or a job on an account removed or moved since.
//!
//! The hub's own environment has `ANTHROPIC_API_KEY` and `CODEX_API_KEY` set (once, before any
//! hub runs; std's environment is locked against its own process spawns, the only readers
//! here), so a job shows which of them it inherits.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::oneshot;
use tokio::time::timeout;
use twr_adapters::fake::FakeAdapter;
use twr_core::adapter::{Adapter, AdapterCmd, AdapterHandle, AdapterInfo, SpawnConfig, Tier};
use twr_core::config::{AccountConfig, BoxConfig, Config, ProjectConfig};
use twr_core::event::*;
use twr_core::paths::Paths;
use twr_core::proto::{codes, RpcError};
use twr_hub::hub::{AccountAddParams, AccountRemoveParams, NativeDirFn};
use twr_hub::{Caller, Hub, HubOptions, JobStart, NewThread};

const WAIT: Duration = Duration::from_secs(20);

/// The stray credentials of the hub's environment ([`init`]).
fn init() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        std::env::set_var("ANTHROPIC_API_KEY", "stray");
        std::env::set_var("CODEX_API_KEY", "stray");
    });
}

/// One start of a runtime: spawn or resume, and its configuration.
#[derive(Debug, Clone)]
struct Start {
    resume: bool,
    cfg: SpawnConfig,
}

type Starts = Arc<Mutex<Vec<Start>>>;

/// A runtime (played under `claude-code` and `codex`) that records each start, ends every turn
/// at once, and attaches through `acct-attach <session>` with its configuration's
/// environment as the real adapters apply it (`twr_adapters::process::apply_env_std`).
struct AcctAdapter {
    name: &'static str,
    starts: Starts,
}

impl AcctAdapter {
    fn start(&self, cfg: SpawnConfig, prompt: bool, resume: bool) -> AdapterHandle {
        self.starts.lock().unwrap().push(Start {
            resume,
            cfg: cfg.clone(),
        });
        let (h, mut end) = AdapterHandle::pair();
        let session = cfg.resume_session.unwrap_or_else(|| "s-1".into());
        tokio::spawn(async move {
            end.emit(
                EventBody::ThreadStarted(SessionPayload {
                    session_id: Some(session),
                    ..Default::default()
                }),
                None,
            );
            let ended = || {
                EventBody::TurnEnded(TurnEnded {
                    result: TurnResult::Completed,
                    ..Default::default()
                })
            };
            if prompt {
                end.emit(ended(), None);
            }
            while let Some(cmd) = end.cmds.recv().await {
                match cmd {
                    AdapterCmd::Send { .. } => {
                        end.emit(ended(), None);
                    }
                    AdapterCmd::Stop => break,
                    _ => {}
                }
            }
        });
        h
    }
}

impl Adapter for AcctAdapter {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: self.name,
            tier: Tier::Headless,
            description: "account test runtime",
        }
    }

    fn spawn(&self, cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let resume = cfg.resume_session.is_some();
        Ok(self.start(cfg, prompt.is_some(), resume))
    }

    fn resume(&self, cfg: SpawnConfig, session_id: String) -> anyhow::Result<AdapterHandle> {
        let cfg = SpawnConfig {
            resume_session: Some(session_id),
            ..cfg
        };
        Ok(self.start(cfg, false, true))
    }

    fn attach_command(&self, cfg: &SpawnConfig, session_id: &str) -> Option<std::process::Command> {
        let mut cmd = std::process::Command::new("acct-attach");
        cmd.arg(session_id);
        twr_adapters::process::apply_env_std(&mut cmd, cfg.env.clone(), &cfg.env_remove);
        Some(cmd)
    }
}

/// A scratch home: its project `api` (a plain folder with its own environment) and
/// `acme-api`, the accounts' directories, and every start the runtimes saw.
struct Env {
    home: tempfile::TempDir,
    starts: Starts,
}

impl Env {
    fn new() -> Env {
        init();
        let home = tempfile::tempdir().unwrap();
        for d in ["api", "acme-api", "acc/work", "acc/work2", "acc/team"] {
            std::fs::create_dir_all(home.path().join(d)).unwrap();
        }
        Env {
            home,
            starts: Arc::default(),
        }
    }

    fn path(&self, p: &str) -> PathBuf {
        self.home.path().join(p)
    }

    /// The database's schema version (`PRAGMA user_version`): raised once account data is
    /// recorded (`twr_hub::store`).
    fn db_version(&self) -> i32 {
        twr_hub::store::connect(&Paths::with_home(self.home.path()).db_file())
            .unwrap()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap()
    }

    /// The config: `[accounts.claude-code.work]` in `work_dir` (default `acc/work`),
    /// `[accounts.codex.team]`, project `api` (its env sets `CLAUDE_CONFIG_DIR` and
    /// `ANTHROPIC_API_KEY`) and project `acme-api` (pinned to `work`).
    fn config(&self, work_dir: Option<&Path>) -> Config {
        let mut c = Config {
            default_runtime: "claude-code".into(),
            ..Default::default()
        };
        let account = |c: &mut Config, rt: &str, name: &str, dir: PathBuf| {
            c.accounts.entry(rt.into()).or_default().insert(
                name.into(),
                AccountConfig {
                    dir: Some(dir),
                    ..Default::default()
                },
            );
        };
        let work = work_dir.map_or_else(|| self.path("acc/work"), Path::to_path_buf);
        account(&mut c, "claude-code", "work", work);
        account(&mut c, "codex", "team", self.path("acc/team"));
        c.projects.insert(
            "api".into(),
            ProjectConfig {
                repo: self.path("api"),
                env: [
                    ("CLAUDE_CONFIG_DIR", "/project/claude"),
                    ("ANTHROPIC_API_KEY", "project-key"),
                    ("KEEP", "1"),
                ]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
                ..Default::default()
            },
        );
        c.projects.insert(
            "acme-api".into(),
            ProjectConfig {
                repo: self.path("acme-api"),
                account: Some(twr_core::account::Pin::parse("work")),
                ..Default::default()
            },
        );
        c
    }

    fn opts(&self, config: Config) -> HubOptions {
        let paths = Paths::with_home(self.home.path());
        let mut o = HubOptions::new(paths.db_file(), config);
        o.accounts_root = Some(paths.accounts_dir());
        let native = self.path("native");
        let native: NativeDirFn = Arc::new(move |rt: &str| Some(native.join(rt)));
        o.native_dir = Some(native);
        o.job_gate_exe = Some(env!("CARGO_BIN_EXE_twr-job-gate").into());
        o.git_tick = Some(Duration::ZERO);
        o.disk_tick = Some(Duration::ZERO);
        o.trash_tick = Some(Duration::ZERO);
        o.stall_after = Some(Duration::ZERO);
        let starts = Arc::clone(&self.starts);
        let claude: Arc<dyn Adapter> = Arc::new(AcctAdapter {
            name: "claude-code",
            starts: Arc::clone(&starts),
        });
        let codex: Arc<dyn Adapter> = Arc::new(AcctAdapter {
            name: "codex",
            starts,
        });
        o.adapters = Arc::new(move |name: &str| -> Option<Arc<dyn Adapter>> {
            match name {
                "claude-code" => Some(Arc::clone(&claude)),
                "codex" => Some(Arc::clone(&codex)),
                "fake" => Some(Arc::new(FakeAdapter::new())),
                _ => None,
            }
        });
        o
    }

    async fn open(&self, config: Config) -> Hub {
        Hub::open(self.opts(config)).await.unwrap()
    }

    /// Shuts `hub` down and opens the home again with `config` (the data-directory lock may
    /// linger briefly).
    async fn restart(&self, hub: Hub, config: Config) -> Hub {
        self.restart_with(hub, config, None).await
    }

    /// [`Env::restart`], with `gate`'s hook on the moves' project lookups.
    async fn restart_with(&self, hub: Hub, config: Config, gate: Option<&LookupGate>) -> Hub {
        hub.shutdown().await;
        drop(hub);
        timeout(WAIT, async {
            loop {
                let mut opts = self.opts(config.clone());
                opts.pins_lookup_hook = gate.map(LookupGate::hook);
                match Hub::open(opts).await {
                    Ok(h) => return h,
                    Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
                }
            }
        })
        .await
        .expect("reopening the hub")
    }

    /// The latest start of thread `id`.
    fn last_start(&self, id: &str) -> Start {
        self.starts
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|s| s.cfg.thread == id)
            .cloned()
            .unwrap_or_else(|| panic!("no start of {id}"))
    }
}

/// A hook on the moves' project lookups (`HubOptions::pins_lookup_hook`): it notes each
/// thread looked up, and once armed holds the next lookup until released.
#[derive(Clone, Default)]
struct LookupGate {
    seen: Arc<Mutex<Vec<String>>>,
    #[allow(clippy::type_complexity)]
    armed: Arc<Mutex<Option<(oneshot::Sender<String>, oneshot::Receiver<()>)>>>,
}

impl LookupGate {
    fn hook(&self) -> twr_hub::hub::BoxFilesReadHook {
        let gate = self.clone();
        Arc::new(move |id: &str| {
            gate.seen.lock().unwrap().push(id.to_string());
            let armed = gate.armed.lock().unwrap().take();
            let id = id.to_string();
            Box::pin(async move {
                if let Some((held, release)) = armed {
                    let _ = held.send(id);
                    let _ = release.await;
                }
            })
        })
    }

    /// Holds the next lookup: the receiver gets its thread once it is held, the sender
    /// releases it.
    fn arm(&self) -> (oneshot::Receiver<String>, oneshot::Sender<()>) {
        let (held, on_hold) = oneshot::channel();
        let (release, released) = oneshot::channel();
        *self.armed.lock().unwrap() = Some((held, released));
        (on_hold, release)
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

fn new_thread(project: &str, name: &str) -> NewThread {
    NewThread {
        project: project.into(),
        name: Some(name.into()),
        no_worktree: true,
        ..Default::default()
    }
}

async fn create(hub: &Hub, req: NewThread) -> String {
    let v = hub.new_thread(req, "itest").await.unwrap();
    v["id"].as_str().unwrap().to_string()
}

/// `message` of an RPC error, after checking its code.
fn refused(r: Result<impl std::fmt::Debug, RpcError>, code: &str) -> String {
    let e = r.unwrap_err();
    assert_eq!(e.code, code, "{}", e.message);
    e.message
}

/// Waits until `pred` holds of the thread's events; returns them.
async fn until(hub: &Hub, thread: &str, pred: impl Fn(&[Event]) -> bool) -> Vec<Event> {
    timeout(WAIT, async {
        loop {
            let (_, evs) = hub.thread_events(thread, 0, 10_000).unwrap();
            if pred(&evs) {
                return evs;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("timed out")
}

fn turn_ends(evs: &[Event]) -> Vec<&TurnEnded> {
    evs.iter()
        .filter_map(|e| match &e.body {
            EventBody::TurnEnded(p) => Some(p),
            _ => None,
        })
        .collect()
}

/// Sends `prompt` and waits for its turn's end; returns that end.
async fn turn(hub: &Hub, thread: &str) -> TurnEnded {
    let before = turn_ends(&hub.thread_events(thread, 0, 10_000).unwrap().1).len();
    hub.send(thread, "hi", TurnFrom::User, Source::User("itest".into()))
        .unwrap();
    let evs = until(hub, thread, |evs| turn_ends(evs).len() > before).await;
    turn_ends(&evs)[before].clone()
}

/// `thread.created`'s `ext.twr`.
fn created_twr(hub: &Hub, thread: &str) -> Value {
    let (_, evs) = hub.thread_events(thread, 0, 10).unwrap();
    evs.iter()
        .find_map(|e| match &e.body {
            EventBody::ThreadCreated(p) => p.ext.as_ref().map(|x| x["twr"].clone()),
            _ => None,
        })
        .unwrap()
}

fn get<'a>(env: &'a [(String, String)], k: &str) -> Vec<&'a str> {
    env.iter()
        .filter(|(n, _)| n == k)
        .map(|(_, v)| v.as_str())
        .collect()
}

/// Adds account `claude-code:side` in its default directory, made as `twr account add`
/// makes it.
fn add_side(env: &Env, hub: &Hub, user: &Caller) {
    let r = hub
        .account_add(
            AccountAddParams {
                runtime: "claude-code".into(),
                account: "side".into(),
                ..Default::default()
            },
            user,
        )
        .unwrap();
    let dir = PathBuf::from(r["dir"].as_str().unwrap());
    assert_eq!(
        dir,
        Paths::with_home(env.home.path())
            .accounts_dir()
            .join("claude-code/side")
    );
    std::fs::create_dir_all(dir).unwrap();
}

/// Starts `argv` as a job of `thread` (not waking it); returns `job.start`'s answer.
fn start_job(hub: &Hub, thread: &str, argv: &[&str]) -> Value {
    let req: JobStart = serde_json::from_value(json!({"argv": argv, "wake": false})).unwrap();
    hub.start_job(thread, req, Source::User("itest".into()))
        .unwrap()
}

/// The `task.updated` that ended job `job`: its description and `ext.twr.job`.
fn job_end(evs: &[Event], job: &str) -> Option<(String, Value)> {
    evs.iter().find_map(|e| match &e.body {
        EventBody::TaskUpdated(p) if !p.running && p.task_id == format!("job:{job}") => Some((
            p.description.clone(),
            p.ext.as_ref()?.get("twr")?.get("job")?.clone(),
        )),
        _ => None,
    })
}

/// Runs `script` as a job of `thread` and returns its log once it ended.
async fn job_output(hub: &Hub, thread: &str, script: &str) -> String {
    let req: JobStart = serde_json::from_value(json!({"argv": ["sh", "-c", script]})).unwrap();
    let job = hub
        .start_job(thread, req, Source::User("itest".into()))
        .unwrap()["job"]
        .as_str()
        .unwrap()
        .to_string();
    until(hub, thread, |evs| {
        evs.iter().any(|e| {
            matches!(&e.body, EventBody::TaskUpdated(p)
                if !p.running && p.task_id == format!("job:{job}"))
        })
    })
    .await;
    hub.job_log(&job, None).unwrap()["text"]
        .as_str()
        .unwrap_or_default()
        .to_string()
}

/// An explicit account (`account`, or `<runtime>:<account>`) runs with the account's
/// variables last (the project's `CLAUDE_CONFIG_DIR` loses) and without the stray
/// credentials; the native account's thread keeps the project's environment, with no
/// removals. What each thread got is on its `thread.created`.
#[tokio::test]
async fn explicit_accounts_get_their_environment_and_are_recorded() {
    let env = Env::new();
    let hub = env.open(env.config(None)).await;
    let work = env.path("acc/work");
    let team = env.path("acc/team");
    // Config accounts alone record nothing: a twr before accounts may still open it.
    assert_eq!(env.db_version(), twr_hub::store::SCHEMA_VERSION);
    let a = create(
        &hub,
        NewThread {
            account: Some("work".into()),
            ..new_thread("api", "a")
        },
    )
    .await;
    // A thread on a named account: from it, such a twr refuses the database.
    assert_eq!(env.db_version(), twr_hub::store::ACCOUNTS_SCHEMA_VERSION);
    let b = create(
        &hub,
        NewThread {
            runtime: Some("claude-code:work".into()),
            ..new_thread("api", "b")
        },
    )
    .await;
    let n = create(&hub, new_thread("api", "n")).await;
    let c = create(
        &hub,
        NewThread {
            runtime: Some("codex:team".into()),
            ..new_thread("api", "c")
        },
    )
    .await;
    for t in [&a, &b, &n, &c] {
        assert_eq!(turn(&hub, t).await.result, TurnResult::Completed);
    }
    for t in [&a, &b] {
        let cfg = env.last_start(t).cfg;
        assert_eq!(
            get(&cfg.env, "CLAUDE_CONFIG_DIR"),
            [work.to_str().unwrap()],
            "{t}"
        );
        assert_eq!(
            get(&cfg.env, "ANTHROPIC_CONFIG_DIR"),
            [work.join("anthropic").to_str().unwrap()]
        );
        // The account's variables after the configured ones; the project's stray key gone
        // with the rest.
        let at = |k: &str| cfg.env.iter().position(|(n, _)| n == k).unwrap();
        assert!(at("KEEP") < at("CLAUDE_CONFIG_DIR"), "{:?}", cfg.env);
        assert!(
            get(&cfg.env, "ANTHROPIC_API_KEY").is_empty(),
            "{:?}",
            cfg.env
        );
        assert_eq!(get(&cfg.env, "KEEP"), ["1"]);
        for k in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "CLAUDE_CODE_OAUTH_TOKEN",
            "CLAUDE_CODE_USE_BEDROCK",
        ] {
            assert!(
                cfg.env_remove.iter().any(|r| r == k),
                "{k}: {:?}",
                cfg.env_remove
            );
        }
        assert!(!cfg.env_remove.iter().any(|r| r == "CLAUDE_CONFIG_DIR"));
    }
    let cfg = env.last_start(&n).cfg;
    assert_eq!(get(&cfg.env, "CLAUDE_CONFIG_DIR"), ["/project/claude"]);
    assert_eq!(get(&cfg.env, "ANTHROPIC_API_KEY"), ["project-key"]);
    assert!(get(&cfg.env, "ANTHROPIC_CONFIG_DIR").is_empty());
    assert!(cfg.env_remove.is_empty(), "{:?}", cfg.env_remove);
    let cfg = env.last_start(&c).cfg;
    assert_eq!(get(&cfg.env, "CODEX_HOME"), [team.to_str().unwrap()]);
    for k in ["OPENAI_API_KEY", "CODEX_API_KEY", "CODEX_ACCESS_TOKEN"] {
        assert!(cfg.env_remove.iter().any(|r| r == k), "{k}");
    }

    // Recorded: the named account, its directory, why, and the thread's own pin.
    let twr = created_twr(&hub, &a);
    assert_eq!(twr["account"], "work");
    assert_eq!(twr["account_dir"], json!(work));
    assert_eq!(twr["account_reason"], "explicit");
    assert_eq!(twr["account_pin"], "work");
    assert_eq!(created_twr(&hub, &b)["account_pin"], "work");
    // The native account nothing else chose: nothing recorded (absent is native).
    let twr = created_twr(&hub, &n);
    for k in ["account", "account_dir", "account_pin", "account_reason"] {
        assert!(twr.get(k).is_none(), "{k}: {twr}");
    }
    let v = hub.thread_view(&a).unwrap();
    assert_eq!(
        (v.account.as_deref(), v.account_pin.as_deref()),
        (Some("work"), Some("work"))
    );
    assert_eq!(hub.thread_view(&n).unwrap().account, None);

    // Both forms given, they must agree; an empty account part is refused.
    let e = refused(
        hub.new_thread(
            NewThread {
                runtime: Some("claude-code:work".into()),
                account: Some("other".into()),
                ..new_thread("api", "x")
            },
            "itest",
        )
        .await,
        codes::BAD_REQUEST,
    );
    assert_eq!(
        e,
        "the runtime names account work and account names other: give the account once"
    );
    let e = refused(
        hub.new_thread(
            NewThread {
                runtime: Some("fake:work".into()),
                ..new_thread("api", "x")
            },
            "itest",
        )
        .await,
        codes::BAD_REQUEST,
    );
    assert_eq!(e, "runtime fake takes no accounts");
    let e = refused(
        hub.new_thread(
            NewThread {
                account: Some("nope".into()),
                ..new_thread("api", "x")
            },
            "itest",
        )
        .await,
        codes::BAD_REQUEST,
    );
    assert_eq!(
        e,
        "claude-code has no account \"nope\": twr account add claude-code nope"
    );
    hub.shutdown().await;
}

/// The project's pin picks the account; one outside it needs the user's `outside_pin`
/// (recorded as an override), and the orchestrator never gets one, though it may name the
/// pinned account (`claude-code:work` through its runtime checks).
#[tokio::test]
async fn project_pins_and_who_may_leave_them() {
    let env = Env::new();
    let mut config = env.config(None);
    config
        .accounts
        .entry("claude-code".into())
        .or_default()
        .insert(
            "work2".into(),
            AccountConfig {
                dir: Some(env.path("acc/work2")),
                ..Default::default()
            },
        );
    let hub = env.open(config).await;
    let p = create(&hub, new_thread("acme-api", "p")).await;
    let twr = created_twr(&hub, &p);
    assert_eq!(
        (&twr["account"], &twr["account_reason"]),
        (&json!("work"), &json!("project"))
    );
    assert!(twr.get("account_pin").is_none());
    let outside = || NewThread {
        account: Some("work2".into()),
        ..new_thread("acme-api", "o")
    };
    let e = refused(hub.new_thread(outside(), "itest").await, codes::BAD_REQUEST);
    assert_eq!(
        e,
        "project acme-api pins account work: pass outside_pin (twr thread new --outside-pin) to \
         open the thread on claude-code:work2 anyway"
    );
    let e = refused(
        hub.new_thread_by(
            NewThread {
                outside_pin: true,
                ..outside()
            },
            &Caller::Orchestrator,
        )
        .await,
        codes::BAD_REQUEST,
    );
    assert_eq!(
        e,
        "project acme-api pins account work; only the user can open its threads on another \
         account"
    );
    let o = create(
        &hub,
        NewThread {
            outside_pin: true,
            ..outside()
        },
    )
    .await;
    let twr = created_twr(&hub, &o);
    assert_eq!(
        (&twr["account"], &twr["account_reason"], &twr["account_pin"]),
        (&json!("work2"), &json!("override"), &json!("work2"))
    );
    // The orchestrator, inside the pin, through `<runtime>:<account>`.
    let v = hub
        .new_thread_by(
            NewThread {
                runtime: Some("claude-code:work".into()),
                ..new_thread("acme-api", "w")
            },
            &Caller::Orchestrator,
        )
        .await
        .unwrap();
    let w = v["id"].as_str().unwrap();
    assert_eq!(created_twr(&hub, w)["account"], "work");
    // A pin the runtime has no account for refuses the thread.
    let e = refused(
        hub.new_thread(
            NewThread {
                runtime: Some("codex".into()),
                ..new_thread("acme-api", "x")
            },
            "itest",
        )
        .await,
        codes::BAD_REQUEST,
    );
    assert_eq!(
        e,
        "project acme-api pins account \"work\", which codex doesn't have: add it (twr account \
         add codex work), or change the pin ([projects.acme-api] accounts.codex = \
         \"<account>\")"
    );
    hub.shutdown().await;
}

/// A named account on another box than the hub's own is refused before the box is asked.
#[tokio::test]
async fn accounts_on_remote_boxes_are_refused() {
    let env = Env::new();
    let mut config = env.config(None);
    config.boxes.insert(
        "gpu".into(),
        BoxConfig {
            token: Some("t".repeat(64)),
            ..Default::default()
        },
    );
    let hub = env.open(config).await;
    let remote = "accounts on remote boxes aren't supported yet: use the box's own login (twr \
                  box login <box> <agent>)";
    for req in [
        NewThread {
            account: Some("work".into()),
            box_name: Some("gpu".into()),
            ..new_thread("/srv/api", "r")
        },
        NewThread {
            runtime: Some("claude-code:work".into()),
            box_name: Some("gpu".into()),
            ..new_thread("/srv/api", "r")
        },
    ] {
        let e = refused(hub.new_thread(req, "itest").await, codes::UNSUPPORTED);
        assert_eq!(e, remote);
    }
    hub.shutdown().await;
}

/// Stops thread `id`'s runtime and waits until it closed.
async fn stopped(hub: &Hub, id: &str) {
    let n = |evs: &[Event]| evs.iter().filter(|e| e.kind() == "thread.stopped").count();
    let before = n(&hub.thread_events(id, 0, 10_000).unwrap().1);
    hub.stop_thread(id, "itest").unwrap();
    until(hub, id, |evs| n(evs) > before).await;
}

/// A thread on another box runs on that box's own login, not the registry's native account
/// (the hub's own login): with that removed (`--force`), a new thread there is made and starts,
/// and one made before starts again, while the hub's own box refuses both; `account.list`
/// counts, and `account.remove` lists, only the hub's own box's threads.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_hubs_native_account_says_nothing_of_remote_threads() {
    use tokio::sync::oneshot;
    use twr_core::runner::Secret;
    use twr_hub::listener::RunnerListener;
    use twr_runner::{LocalBox, LocalBoxOptions, RunnerOptions};

    let env = Env::new();
    let token = "t".repeat(64);
    let mut config = env.config(None);
    config.hub.listen = Some("127.0.0.1:0".into());
    config.boxes.insert(
        "gpu".into(),
        BoxConfig {
            token: Some(token.clone()),
            ..Default::default()
        },
    );
    let o = env.opts(config);
    let listener = RunnerListener::bind(&o.config.hub).await.unwrap().unwrap();
    let url = listener.url();
    let hub = Hub::open(o).await.unwrap();
    let _server = listener.serve(hub.clone());
    // The box: an in-process runner whose `claude-code` is the recording runtime.
    let rdir = env.path("runner");
    let mut ro = RunnerOptions::new(&url, Secret::new(&token), "gpu", &rdir);
    ro.backoff_min = Duration::from_millis(20);
    ro.backoff_max = Duration::from_millis(200);
    ro.refused_retry = Duration::from_millis(200);
    let mut box_opts = LocalBoxOptions::new(&rdir, "gpu");
    let claude: Arc<dyn Adapter> = Arc::new(AcctAdapter {
        name: "claude-code",
        starts: Arc::clone(&env.starts),
    });
    box_opts.adapters = Arc::new(move |name: &str| -> Option<Arc<dyn Adapter>> {
        (name == "claude-code").then(|| Arc::clone(&claude))
    });
    if !box_opts.runtimes.iter().any(|r| r == "claude-code") {
        box_opts.runtimes.push("claude-code".into());
    }
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let runner = tokio::spawn(twr_runner::run_with(
        ro,
        LocalBox::open(box_opts).unwrap(),
        async move {
            let _ = stop_rx.await;
        },
    ));
    timeout(WAIT, async {
        while !hub
            .boxes()
            .iter()
            .any(|(n, online, _)| n == "gpu" && *online)
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("box never online");

    let remote = env.path("remote");
    std::fs::create_dir_all(&remote).unwrap();
    let on_box = |name: &str| NewThread {
        box_name: Some("gpu".into()),
        ..new_thread(remote.to_str().unwrap(), name)
    };
    let r = create(&hub, on_box("r")).await;
    let l = create(&hub, new_thread("api", "l")).await;
    for t in [&r, &l] {
        assert_eq!(turn(&hub, t).await.result, TurnResult::Completed);
    }
    let list = hub.account_list(Some("claude-code"), true).unwrap();
    let native = list["runtimes"]["claude-code"]["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["native"] == true)
        .cloned()
        .unwrap();
    assert_eq!(native["threads"], 1, "{list}");
    let user = Caller::user("itest");
    let remove = |force: bool| AccountRemoveParams {
        runtime: "claude-code".into(),
        account: "default".into(),
        force,
        ..Default::default()
    };
    let e = refused(
        hub.account_remove(remove(false), &user).await,
        codes::CONFLICT,
    );
    assert!(
        e.starts_with("claude-code:default has threads on it: l. "),
        "{e}"
    );
    let rm = hub.account_remove(remove(true), &user).await.unwrap();
    assert_eq!(rm["threads"], json!(["l"]));

    // The box's threads: a new one is made and starts, the one from before starts again; the
    // thread JSON does not call their account removed.
    let r2 = create(&hub, on_box("r2")).await;
    assert_eq!(turn(&hub, &r2).await.result, TurnResult::Completed);
    stopped(&hub, &r).await;
    assert_eq!(turn(&hub, &r).await.result, TurnResult::Completed);
    let threads = hub.list_threads();
    for t in [&r, &r2] {
        let j = threads.iter().find(|j| j["id"] == json!(t)).unwrap();
        assert!(j.get("account_removed").is_none(), "{j}");
    }
    // The hub's own box: refused.
    let e = refused(
        hub.new_thread(new_thread("api", "l2"), "itest").await,
        codes::BAD_REQUEST,
    );
    assert!(
        e.starts_with("claude-code's own login (default) was removed"),
        "{e}"
    );
    stopped(&hub, &l).await;
    let end = turn(&hub, &l).await;
    assert_eq!(end.result, TurnResult::Error);
    assert_eq!(
        end.ext.as_ref().unwrap()["twr"]["error"],
        json!(
            "l runs on claude-code:default, which was removed: move it (twr model l --account \
             <name>)"
        )
    );

    let _ = stop_tx.send(());
    let _ = timeout(Duration::from_secs(10), runner).await;
    hub.shutdown().await;
}

/// A recorded account that collides once the hub starts again (the hub's environment now
/// makes its directory a runtime's own: `CLAUDE_CONFIG_DIR` set to it, say) does not stop the
/// hub: it is a warning (`hub.info`), its `account.list` entry says why (`conflict`), and it
/// is refused at use (its thread's start, attach and job; a new thread on it) until
/// `account.remove`, which works as for any account.
#[tokio::test]
async fn a_recorded_account_in_a_runtimes_own_directory_is_refused_at_use() {
    let env = Env::new();
    let hub = env.open(env.config(None)).await;
    let user = Caller::user("itest");
    add_side(&env, &hub, &user);
    let s = create(
        &hub,
        NewThread {
            account: Some("side".into()),
            ..new_thread("api", "s")
        },
    )
    .await;
    assert_eq!(turn(&hub, &s).await.result, TurnResult::Completed);
    hub.shutdown().await;
    drop(hub);
    let side = Paths::with_home(env.home.path())
        .accounts_dir()
        .join("claude-code/side");
    let opts = || {
        let mut o = env.opts(env.config(None));
        let (native, side) = (env.path("native"), side.clone());
        o.native_dir = Some(Arc::new(move |rt: &str| match rt {
            "claude-code" => Some(side.clone()),
            _ => Some(native.join(rt)),
        }));
        o
    };
    let hub = timeout(WAIT, async {
        loop {
            match Hub::open(opts()).await {
                Ok(h) => return h,
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await
    .expect("reopening the hub");
    let why = format!(
        "its directory {} is claude-code's own directory in the hub's environment \
         (CLAUDE_CONFIG_DIR, or its default): set CLAUDE_CONFIG_DIR elsewhere for the hub and \
         restart it, or remove the account (twr account remove claude-code side)",
        side.display()
    );
    let warnings = hub.info()["warnings"].clone();
    assert!(
        warnings
            .as_array()
            .unwrap()
            .contains(&json!(format!("claude-code:side can't be used: {why}"))),
        "{warnings}"
    );
    let list = hub.account_list(Some("claude-code"), true).unwrap();
    let entry = list["runtimes"]["claude-code"]["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "side")
        .cloned()
        .unwrap();
    assert_eq!(entry["conflict"], json!(why));
    let refusal = format!("s runs on claude-code:side, which can't be used: {why}");
    let end = turn(&hub, &s).await;
    assert_eq!(end.result, TurnResult::Error);
    assert_eq!(end.ext.as_ref().unwrap()["twr"]["error"], json!(refusal));
    assert_eq!(
        refused(hub.attach_begin(&s, false, "itest").await, codes::CONFLICT),
        refusal
    );
    let req: JobStart = serde_json::from_value(json!({"argv": ["true"]})).unwrap();
    assert_eq!(
        refused(
            hub.start_job(&s, req, Source::User("itest".into())),
            codes::CONFLICT
        ),
        refusal
    );
    let e = refused(
        hub.new_thread(
            NewThread {
                account: Some("side".into()),
                ..new_thread("api", "s2")
            },
            "itest",
        )
        .await,
        codes::CONFLICT,
    );
    assert_eq!(e, format!("claude-code:side can't be used: {why}"));
    // Its removal works; its thread is on a removed account then.
    hub.account_remove(
        AccountRemoveParams {
            runtime: "claude-code".into(),
            account: "side".into(),
            force: true,
            ..Default::default()
        },
        &user,
    )
    .await
    .unwrap();
    let end = turn(&hub, &s).await;
    assert_eq!(
        end.ext.as_ref().unwrap()["twr"]["error"],
        json!(
            "s runs on claude-code:side, which was removed: move it (twr model s --account <name>)"
        )
    );
    hub.shutdown().await;
}

/// The account's environment on a resume after a hub restart (rebuilt from the log), on the
/// attach command (its variables, its removals as `null`), and on a job's process (the
/// account's variables, without the stray credentials it would inherit from the hub; the
/// native account's job keeps them).
#[tokio::test]
async fn the_account_reaches_resumes_attach_and_jobs() {
    let env = Env::new();
    let work = env.path("acc/work");
    let hub = env.open(env.config(None)).await;
    let a = create(
        &hub,
        NewThread {
            account: Some("work".into()),
            ..new_thread("api", "a")
        },
    )
    .await;
    let n = create(&hub, new_thread("api", "n")).await;
    turn(&hub, &a).await;
    turn(&hub, &n).await;
    let spawned = env.last_start(&a);
    assert!(!spawned.resume);
    let hub = env.restart(hub, env.config(None)).await;
    turn(&hub, &a).await;
    let resumed = env.last_start(&a);
    assert!(resumed.resume, "{resumed:?}");
    assert_eq!(resumed.cfg.resume_session.as_deref(), Some("s-1"));
    let strip = |env: &[(String, String)]| -> Vec<(String, String)> {
        env.iter()
            .filter(|(k, _)| !k.starts_with("TWR_"))
            .cloned()
            .collect()
    };
    assert_eq!(strip(&resumed.cfg.env), strip(&spawned.cfg.env));
    assert_eq!(resumed.cfg.env_remove, spawned.cfg.env_remove);

    // Jobs: the account's variables; the hub's stray Claude key removed, Codex's kept.
    let script = "echo \"dir=$CLAUDE_CONFIG_DIR key=${ANTHROPIC_API_KEY-unset} \
                  codex=${CODEX_API_KEY-unset}\"";
    let out = job_output(&hub, &a, script).await;
    assert!(
        out.contains(&format!("dir={} key=unset codex=stray", work.display())),
        "{out}"
    );
    let out = job_output(&hub, &n, script).await;
    assert!(
        out.contains("dir=/project/claude key=project-key codex=stray"),
        "{out}"
    );

    // Attach: the same environment, the removals as `null`.
    let grant = hub.attach_begin(&a, false, "itest").await.unwrap();
    assert_eq!(grant.argv, ["acct-attach", "s-1"]);
    assert_eq!(grant.env["CLAUDE_CONFIG_DIR"], json!(work));
    assert_eq!(
        grant.env["ANTHROPIC_CONFIG_DIR"],
        json!(work.join("anthropic"))
    );
    assert_eq!(grant.env["ANTHROPIC_API_KEY"], Value::Null);
    assert_eq!(grant.env["CLAUDE_CODE_OAUTH_TOKEN"], Value::Null);
    assert!(hub.attach_end(&a, "itest", grant.lease, None).unwrap());
    let grant = hub.attach_begin(&n, false, "itest").await.unwrap();
    assert_eq!(grant.env["CLAUDE_CONFIG_DIR"], "/project/claude");
    assert!(
        grant.env.get("CLAUDE_CODE_OAUTH_TOKEN").is_none(),
        "{:?}",
        grant.env
    );
    assert!(hub.attach_end(&n, "itest", grant.lease, None).unwrap());
    hub.shutdown().await;
}

/// After `account.remove --force` (a recorded account, and the native one), and after the
/// config moves an account's directory, the thread's start ends its turn with the refusal, and
/// an attach or a job is refused the same way.
#[tokio::test]
async fn removed_or_moved_accounts_refuse_starts_attaches_and_jobs() {
    let env = Env::new();
    let hub = env.open(env.config(None)).await;
    let user = Caller::user("itest");
    add_side(&env, &hub, &user);
    let s = create(
        &hub,
        NewThread {
            account: Some("side".into()),
            ..new_thread("api", "s")
        },
    )
    .await;
    let n = create(
        &hub,
        NewThread {
            runtime: Some("codex".into()),
            ..new_thread("api", "n")
        },
    )
    .await;
    let w = create(
        &hub,
        NewThread {
            account: Some("work".into()),
            ..new_thread("api", "w")
        },
    )
    .await;
    for t in [&s, &n, &w] {
        assert_eq!(turn(&hub, t).await.result, TurnResult::Completed);
    }
    for (rt, name) in [("claude-code", "side"), ("codex", "default")] {
        hub.account_remove(
            AccountRemoveParams {
                runtime: rt.into(),
                account: name.into(),
                force: true,
                ..Default::default()
            },
            &user,
        )
        .await
        .unwrap();
    }
    // (A runtime that runs goes on: only a start is refused.)
    let hub = env.restart(hub, env.config(None)).await;
    let side = "s runs on claude-code:side, which was removed: move it (twr model s --account \
                <name>)";
    let native = "n runs on codex:default, which was removed: move it (twr model n --account \
                  <name>)";
    for (t, why) in [(&s, side), (&n, native)] {
        let end = turn(&hub, t).await;
        assert_eq!(end.result, TurnResult::Error);
        assert_eq!(end.ext.as_ref().unwrap()["twr"]["error"], json!(why), "{t}");
        assert_eq!(
            refused(hub.attach_begin(t, false, "itest").await, codes::CONFLICT),
            why
        );
        let req: JobStart = serde_json::from_value(json!({"argv": ["true"]})).unwrap();
        assert_eq!(
            refused(
                hub.start_job(t, req, Source::User("itest".into())),
                codes::CONFLICT
            ),
            why
        );
    }

    // The config moves `work` elsewhere: its thread's session is in the old directory.
    let (old, new) = (env.path("acc/work"), env.path("acc/work2"));
    let hub = env.restart(hub, env.config(Some(&new))).await;
    let why = format!(
        "claude-code:work's directory changed since w's session began ({} -> {}): twr model w \
         --account work starts a new session there",
        old.display(),
        new.display()
    );
    let end = turn(&hub, &w).await;
    assert_eq!(end.result, TurnResult::Error);
    assert_eq!(end.ext.as_ref().unwrap()["twr"]["error"], json!(why));
    assert_eq!(
        refused(hub.attach_begin(&w, false, "itest").await, codes::CONFLICT),
        why
    );
    let req: JobStart = serde_json::from_value(json!({"argv": ["true"]})).unwrap();
    assert_eq!(
        refused(
            hub.start_job(&w, req, Source::User("itest".into())),
            codes::CONFLICT
        ),
        why
    );
    // Back where it was, it starts again.
    let hub = env.restart(hub, env.config(None)).await;
    assert_eq!(turn(&hub, &w).await.result, TurnResult::Completed);
    hub.shutdown().await;
}

/// The native account removed under the name it had (`codex:home`, with force) stays removed
/// after a restart whose config gives that name to a named account: the native account is
/// `default` again, still removed, and its thread refuses to start; the named `home` is a
/// different login.
#[tokio::test]
async fn a_removed_native_account_stays_removed_when_config_takes_its_name() {
    let env = Env::new();
    let hub = env.open(env.config(None)).await;
    let user = Caller::user("itest");
    hub.account_add(
        AccountAddParams {
            runtime: "codex".into(),
            account: "home".into(),
            native: true,
            ..Default::default()
        },
        &user,
    )
    .unwrap();
    let n = create(
        &hub,
        NewThread {
            runtime: Some("codex".into()),
            ..new_thread("api", "n")
        },
    )
    .await;
    assert_eq!(turn(&hub, &n).await.result, TurnResult::Completed);
    hub.account_remove(
        AccountRemoveParams {
            runtime: "codex".into(),
            account: "home".into(),
            force: true,
            ..Default::default()
        },
        &user,
    )
    .await
    .unwrap();
    let home_dir = env.path("acc/home");
    std::fs::create_dir_all(&home_dir).unwrap();
    let mut config = env.config(None);
    config.accounts.entry("codex".into()).or_default().insert(
        "home".into(),
        AccountConfig {
            dir: Some(home_dir),
            ..Default::default()
        },
    );
    let hub = env.restart(hub, config).await;
    let why = "n runs on codex:default, which was removed: move it (twr model n --account \
               <name>)";
    let end = turn(&hub, &n).await;
    assert_eq!(end.result, TurnResult::Error);
    assert_eq!(end.ext.as_ref().unwrap()["twr"]["error"], json!(why));
    assert_eq!(
        refused(hub.attach_begin(&n, false, "itest").await, codes::CONFLICT),
        why
    );
    hub.shutdown().await;
}

/// A job queued behind a busy slot is checked again right before it starts: its account
/// removed meanwhile (`account.remove --force`), it fails with the start's refusal and never
/// runs; a job queued on a thread moved to another account meanwhile runs on that account.
#[tokio::test]
async fn a_queued_job_is_refused_once_its_account_is_removed() {
    let env = Env::new();
    let mut config = env.config(None);
    config.threads.job_slots = 1;
    let hub = env.open(config).await;
    let user = Caller::user("itest");
    add_side(&env, &hub, &user);
    let s = create(
        &hub,
        NewThread {
            account: Some("side".into()),
            ..new_thread("api", "s")
        },
    )
    .await;
    let m = create(
        &hub,
        NewThread {
            account: Some("side".into()),
            ..new_thread("api", "m")
        },
    )
    .await;
    let flag = env.path("go");
    let ran = env.path("ran");
    let wait = format!("while [ ! -e {} ]; do sleep 0.05; done", flag.display());
    let first = start_job(&hub, &s, &["sh", "-c", &wait]);
    // It runs (it holds the one slot) before anything changes.
    timeout(WAIT, async {
        loop {
            let jobs = hub.list_jobs(Some(&s)).unwrap();
            if jobs[0]["state"] == "running" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the first job running");
    let queued = start_job(&hub, &s, &["touch", ran.to_str().unwrap()]);
    assert_eq!(queued["position"], 0, "{queued}");
    // `m` moves to `work` while its job waits: the job gets `work`'s environment.
    let out = env.path("m-env");
    let script = format!("printf %s \"$CLAUDE_CONFIG_DIR\" > {}", out.display());
    let moved = start_job(&hub, &m, &["sh", "-c", &script]);
    assert_eq!(moved["position"], 1, "{moved}");
    hub.set_thread_model(
        &m,
        twr_hub::hub::ModelRequest {
            account: Some("work"),
            outside_pin: true,
            ..Default::default()
        },
        &user,
    )
    .await
    .unwrap();
    hub.account_remove(
        AccountRemoveParams {
            runtime: "claude-code".into(),
            account: "side".into(),
            force: true,
            ..Default::default()
        },
        &user,
    )
    .await
    .unwrap();
    std::fs::write(&flag, "").unwrap();
    let (a, b) = (
        first["job"].as_str().unwrap(),
        queued["job"].as_str().unwrap(),
    );
    let evs = until(&hub, &s, |evs| {
        job_end(evs, a).is_some() && job_end(evs, b).is_some()
    })
    .await;
    assert_eq!(job_end(&evs, a).unwrap().1["outcome"], "exited");
    let (description, end) = job_end(&evs, b).unwrap();
    assert_eq!(end["outcome"], "failed_to_start", "{end}");
    assert!(
        description.contains(
            "s runs on claude-code:side, which was removed: move it (twr model s --account \
             <name>)"
        ),
        "{description}"
    );
    assert!(!ran.exists(), "it ran");
    let c = moved["job"].as_str().unwrap();
    let evs = until(&hub, &m, |evs| job_end(evs, c).is_some()).await;
    assert_eq!(job_end(&evs, c).unwrap().1["outcome"], "exited");
    assert_eq!(
        std::fs::read_to_string(&out).unwrap(),
        env.path("acc/work").to_str().unwrap()
    );
    hub.shutdown().await;
}

/// Removes `claude-code:side` (`--force`, its threads keep it), records `codex:side` in the
/// same directory, and moves thread `id` there: the same account name and directory, another
/// runtime.
async fn side_to_codex(env: &Env, hub: &Hub, user: &Caller, id: &str) {
    hub.account_remove(
        AccountRemoveParams {
            runtime: "claude-code".into(),
            account: "side".into(),
            force: true,
            ..Default::default()
        },
        user,
    )
    .await
    .unwrap();
    let dir = Paths::with_home(env.home.path())
        .accounts_dir()
        .join("claude-code/side");
    hub.account_add(
        AccountAddParams {
            runtime: "codex".into(),
            account: "side".into(),
            dir: Some(dir),
            ..Default::default()
        },
        user,
    )
    .unwrap();
    hub.set_thread_model(
        id,
        twr_hub::hub::ModelRequest {
            runtime: Some("codex:side"),
            ..Default::default()
        },
        user,
    )
    .await
    .unwrap();
}

/// A job's account is its runtime's too: a job queued on `claude-code:side` whose thread moved
/// to `codex:side` in the same directory meanwhile starts with Codex's environment
/// (`CODEX_HOME`, Codex's removals), not the Claude Code one it was asked with.
#[tokio::test]
async fn a_queued_job_moved_to_another_runtimes_account_in_its_directory_gets_its_env() {
    let env = Env::new();
    let mut config = env.config(None);
    config.threads.job_slots = 1;
    let hub = env.open(config).await;
    let user = Caller::user("itest");
    add_side(&env, &hub, &user);
    let h = create(&hub, new_thread("api", "h")).await;
    let m = create(
        &hub,
        NewThread {
            account: Some("side".into()),
            ..new_thread("api", "m")
        },
    )
    .await;
    let flag = env.path("go");
    let wait = format!("while [ ! -e {} ]; do sleep 0.05; done", flag.display());
    let first = start_job(&hub, &h, &["sh", "-c", &wait]);
    timeout(WAIT, async {
        loop {
            let jobs = hub.list_jobs(Some(&h)).unwrap();
            if jobs[0]["state"] == "running" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the first job running");
    let out = env.path("m-env");
    let script = format!(
        "printf '%s|%s|%s' \"${{CODEX_HOME-unset}}\" \"${{CLAUDE_CONFIG_DIR-unset}}\" \
         \"${{CODEX_API_KEY-unset}}\" > {}",
        out.display()
    );
    let queued = start_job(&hub, &m, &["sh", "-c", &script]);
    assert_eq!(queued["position"], 0, "{queued}");
    side_to_codex(&env, &hub, &user, &m).await;
    std::fs::write(&flag, "").unwrap();
    let a = first["job"].as_str().unwrap();
    until(&hub, &h, |evs| job_end(evs, a).is_some()).await;
    let b = queued["job"].as_str().unwrap();
    let evs = until(&hub, &m, |evs| job_end(evs, b).is_some()).await;
    assert_eq!(job_end(&evs, b).unwrap().1["outcome"], "exited");
    let side = Paths::with_home(env.home.path())
        .accounts_dir()
        .join("claude-code/side");
    // (The project's `CLAUDE_CONFIG_DIR` is the project's own, not an account's.)
    assert_eq!(
        std::fs::read_to_string(&out).unwrap(),
        format!("{}|/project/claude|unset", side.display())
    );
    hub.shutdown().await;
}

/// A job held at its gate whose thread moved to another runtime's account of the same name
/// and directory meanwhile fails to start: its environment is the old runtime's.
#[tokio::test]
async fn a_job_held_at_its_gate_refuses_a_move_to_another_runtimes_account() {
    let env = Env::new();
    let mut o = env.opts(env.config(None));
    let (at, go) = holding_gate(&mut o);
    let hub = Hub::open(o).await.unwrap();
    let user = Caller::user("itest");
    add_side(&env, &hub, &user);
    let m = create(
        &hub,
        NewThread {
            account: Some("side".into()),
            ..new_thread("api", "m")
        },
    )
    .await;
    let ran = env.path("ran");
    let job = start_job(&hub, &m, &["touch", ran.to_str().unwrap()]);
    tokio::task::spawn_blocking(move || at.recv_timeout(WAIT))
        .await
        .unwrap()
        .expect("the child reached its gate");
    side_to_codex(&env, &hub, &user, &m).await;
    go.send(()).unwrap();
    let id = job["job"].as_str().unwrap();
    let evs = until(&hub, &m, |evs| job_end(evs, id).is_some()).await;
    let (description, end) = job_end(&evs, id).unwrap();
    assert_eq!(end["outcome"], "failed_to_start", "{end}");
    assert!(
        description.contains("m moved to codex:side while the job started; run it again"),
        "{description}"
    );
    assert!(!ran.exists(), "it ran");
    hub.shutdown().await;
}

/// A gate hook that holds each job's child at its start gate: the child's pid comes out of the
/// returned receiver, and the child goes on once the returned sender is sent a `()`.
fn holding_gate(
    o: &mut HubOptions,
) -> (std::sync::mpsc::Receiver<u32>, std::sync::mpsc::Sender<()>) {
    let (at_tx, at_rx) = std::sync::mpsc::channel();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let at_tx = Mutex::new(at_tx);
    let go_rx = Mutex::new(go_rx);
    o.job_gate_hook = Some(Arc::new(move |pid| {
        let _ = at_tx.lock().unwrap().send(pid);
        let _ = go_rx.lock().unwrap().recv_timeout(WAIT);
    }));
    (at_rx, go_tx)
}

/// A job held at its start gate (its environment built, its process waiting) is checked again
/// as the gate opens, under the lock a move and a removal take: its thread moved to another
/// account meanwhile, it fails to start saying so (its environment is the old account's); its
/// account removed (`--force`) meanwhile, it fails with the start's refusal. Neither runs its
/// command.
#[tokio::test]
async fn a_job_held_at_its_gate_checks_its_account_before_it_runs() {
    let env = Env::new();
    let mut o = env.opts(env.config(None));
    let (at, go) = holding_gate(&mut o);
    let hub = Hub::open(o).await.unwrap();
    let user = Caller::user("itest");
    add_side(&env, &hub, &user);
    let on_side = |name: &str| NewThread {
        account: Some("side".into()),
        ..new_thread("api", name)
    };
    let s = create(&hub, on_side("s")).await;
    let m = create(&hub, on_side("m")).await;
    let reached = |at: std::sync::mpsc::Receiver<u32>| async move {
        tokio::task::spawn_blocking(move || at.recv_timeout(WAIT).map(|_| at))
            .await
            .unwrap()
            .expect("the child reached its gate")
    };

    // Moved while held.
    let moved_ran = env.path("moved-ran");
    let job = start_job(&hub, &m, &["touch", moved_ran.to_str().unwrap()]);
    let at = reached(at).await;
    hub.set_thread_model(
        &m,
        twr_hub::hub::ModelRequest {
            account: Some("work"),
            outside_pin: true,
            ..Default::default()
        },
        &user,
    )
    .await
    .unwrap();
    go.send(()).unwrap();
    let id = job["job"].as_str().unwrap();
    let evs = until(&hub, &m, |evs| job_end(evs, id).is_some()).await;
    let (description, end) = job_end(&evs, id).unwrap();
    assert_eq!(end["outcome"], "failed_to_start", "{end}");
    assert!(
        description.contains("m moved to claude-code:work while the job started; run it again"),
        "{description}"
    );

    // Its account removed while held.
    let removed_ran = env.path("removed-ran");
    let job = start_job(&hub, &s, &["touch", removed_ran.to_str().unwrap()]);
    let _at = reached(at).await;
    hub.account_remove(
        AccountRemoveParams {
            runtime: "claude-code".into(),
            account: "side".into(),
            force: true,
            ..Default::default()
        },
        &user,
    )
    .await
    .unwrap();
    go.send(()).unwrap();
    let id = job["job"].as_str().unwrap();
    let evs = until(&hub, &s, |evs| job_end(evs, id).is_some()).await;
    let (description, end) = job_end(&evs, id).unwrap();
    assert_eq!(end["outcome"], "failed_to_start", "{end}");
    assert!(
        description.contains(
            "s runs on claude-code:side, which was removed: move it (twr model s --account \
             <name>)"
        ),
        "{description}"
    );
    for f in [&moved_ran, &removed_ran] {
        assert!(!f.exists(), "{} ran", f.display());
    }
    hub.shutdown().await;
}

/// Links `acc/side-a` and `acc/side-b` to `acc/side`, and adds account `claude-code:side`
/// through `acc/side-a`; returns both links.
fn add_side_through_a(env: &Env, hub: &Hub, user: &Caller) -> (PathBuf, PathBuf) {
    let (a, b) = (env.path("acc/side-a"), env.path("acc/side-b"));
    std::fs::create_dir_all(env.path("acc/side")).unwrap();
    std::fs::create_dir_all(env.path("acc/elsewhere")).unwrap();
    for l in [&a, &b] {
        std::os::unix::fs::symlink(env.path("acc/side"), l).unwrap();
    }
    hub.account_add(
        AccountAddParams {
            runtime: "claude-code".into(),
            account: "side".into(),
            dir: Some(a.clone()),
            ..Default::default()
        },
        user,
    )
    .unwrap();
    (a, b)
}

/// Retargets link `a` (thread `id`'s recorded path to `acc/side`) to `acc/elsewhere`, removes
/// `claude-code:side` (`--force`), adds it again through `b` (another link to `acc/side`) and
/// moves thread `id` to it: the same runtime, account and directory, another path.
async fn rebind_side_through_b(hub: &Hub, user: &Caller, id: &str, (a, b): (&Path, &Path)) {
    std::fs::remove_file(a).unwrap();
    std::os::unix::fs::symlink(a.parent().unwrap().join("elsewhere"), a).unwrap();
    hub.account_remove(
        AccountRemoveParams {
            runtime: "claude-code".into(),
            account: "side".into(),
            force: true,
            ..Default::default()
        },
        user,
    )
    .await
    .unwrap();
    hub.account_add(
        AccountAddParams {
            runtime: "claude-code".into(),
            account: "side".into(),
            dir: Some(b.to_path_buf()),
            ..Default::default()
        },
        user,
    )
    .unwrap();
    let r = hub
        .set_thread_model(
            id,
            twr_hub::hub::ModelRequest {
                account: Some("side"),
                ..Default::default()
            },
            user,
        )
        .await
        .unwrap();
    assert_eq!(r["account_changed"], json!(true), "{r}");
}

/// A job's account is the path its environment gives the account by too: a job queued on
/// `claude-code:side` through link `a` whose thread moved meanwhile to the same account through
/// link `b` to the same directory (`a` retargeted elsewhere) starts through `b`, not `a`.
#[cfg(unix)]
#[tokio::test]
async fn a_queued_job_moved_to_its_account_through_another_path_gets_that_path() {
    let env = Env::new();
    let mut config = env.config(None);
    config.threads.job_slots = 1;
    let hub = env.open(config).await;
    let user = Caller::user("itest");
    let (a, b) = add_side_through_a(&env, &hub, &user);
    let h = create(&hub, new_thread("api", "h")).await;
    let m = create(
        &hub,
        NewThread {
            account: Some("side".into()),
            ..new_thread("api", "m")
        },
    )
    .await;
    assert_eq!(created_twr(&hub, &m)["account_env_dir"], json!(a));
    let flag = env.path("go");
    let wait = format!("while [ ! -e {} ]; do sleep 0.05; done", flag.display());
    let first = start_job(&hub, &h, &["sh", "-c", &wait]);
    timeout(WAIT, async {
        loop {
            let jobs = hub.list_jobs(Some(&h)).unwrap();
            if jobs[0]["state"] == "running" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the first job running");
    let out = env.path("m-env");
    let script = format!("printf %s \"$CLAUDE_CONFIG_DIR\" > {}", out.display());
    let queued = start_job(&hub, &m, &["sh", "-c", &script]);
    assert_eq!(queued["position"], 0, "{queued}");
    rebind_side_through_b(&hub, &user, &m, (&a, &b)).await;
    std::fs::write(&flag, "").unwrap();
    let first = first["job"].as_str().unwrap();
    until(&hub, &h, |evs| job_end(evs, first).is_some()).await;
    let id = queued["job"].as_str().unwrap();
    let evs = until(&hub, &m, |evs| job_end(evs, id).is_some()).await;
    assert_eq!(job_end(&evs, id).unwrap().1["outcome"], "exited");
    assert_eq!(std::fs::read_to_string(&out).unwrap(), b.to_str().unwrap());
    hub.shutdown().await;
}

/// A job held at its gate (its environment giving `claude-code:side` through link `a`) whose
/// thread moved meanwhile to the same account through link `b` to the same directory fails to
/// start: its environment names the old path.
#[cfg(unix)]
#[tokio::test]
async fn a_job_held_at_its_gate_refuses_a_move_to_its_account_through_another_path() {
    let env = Env::new();
    let mut o = env.opts(env.config(None));
    let (at, go) = holding_gate(&mut o);
    let hub = Hub::open(o).await.unwrap();
    let user = Caller::user("itest");
    let (a, b) = add_side_through_a(&env, &hub, &user);
    let m = create(
        &hub,
        NewThread {
            account: Some("side".into()),
            ..new_thread("api", "m")
        },
    )
    .await;
    let ran = env.path("ran");
    let job = start_job(&hub, &m, &["touch", ran.to_str().unwrap()]);
    tokio::task::spawn_blocking(move || at.recv_timeout(WAIT))
        .await
        .unwrap()
        .expect("the child reached its gate");
    rebind_side_through_b(&hub, &user, &m, (&a, &b)).await;
    go.send(()).unwrap();
    let id = job["job"].as_str().unwrap();
    let evs = until(&hub, &m, |evs| job_end(evs, id).is_some()).await;
    let (description, end) = job_end(&evs, id).unwrap();
    assert_eq!(end["outcome"], "failed_to_start", "{end}");
    let want = format!(
        "m moved to claude-code:side through {} while the job started; run it again",
        b.display()
    );
    assert!(description.contains(&want), "{description}");
    assert!(!ran.exists(), "it ran");
    hub.shutdown().await;
}

/// A config account whose directory is a link retargeted, after the hub started, to a
/// runtime's own directory is refused at use like a recorded one: a new thread on it, and its
/// thread's start, saying to change config.toml.
#[cfg(unix)]
#[tokio::test]
async fn a_config_account_retargeted_to_a_native_directory_is_refused_at_use() {
    let env = Env::new();
    let link = env.path("acc/link");
    std::os::unix::fs::symlink(env.path("acc/work"), &link).unwrap();
    let native = env.path("native/claude-code");
    std::fs::create_dir_all(&native).unwrap();
    let hub = env.open(env.config(Some(&link))).await;
    let on_work = |name: &str| NewThread {
        account: Some("work".into()),
        ..new_thread("api", name)
    };
    let w = create(&hub, on_work("w")).await;
    assert_eq!(turn(&hub, &w).await.result, TurnResult::Completed);
    // (A runtime that runs goes on: only a start is refused.)
    stopped(&hub, &w).await;
    // The link now leads to claude-code's own directory.
    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(&native, &link).unwrap();
    let why = format!(
        "its directory {} is claude-code's own directory in the hub's environment \
         (CLAUDE_CONFIG_DIR, or its default): set CLAUDE_CONFIG_DIR elsewhere for the hub and \
         restart it, or change [accounts.claude-code.work] dir in config.toml and restart the \
         hub",
        link.display()
    );
    let e = refused(
        hub.new_thread(on_work("w2"), "itest").await,
        codes::CONFLICT,
    );
    assert_eq!(e, format!("claude-code:work can't be used: {why}"));
    let end = turn(&hub, &w).await;
    assert_eq!(end.result, TurnResult::Error);
    assert_eq!(
        end.ext.as_ref().unwrap()["twr"]["error"],
        json!(format!(
            "w runs on claude-code:work, which can't be used: {why}"
        ))
    );
    let list = hub.account_list(Some("claude-code"), true).unwrap();
    let entry = list["runtimes"]["claude-code"]["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "work")
        .cloned()
        .unwrap();
    assert_eq!(entry["conflict"], json!(why));
    hub.shutdown().await;
}

/// A named account whose directory is gone (deleted by hand) refuses its thread's start, an
/// attach and a job, saying how to make it again; back, the thread starts.
#[tokio::test]
async fn a_missing_account_directory_refuses_starts_attaches_and_jobs() {
    let env = Env::new();
    let hub = env.open(env.config(None)).await;
    let w = create(
        &hub,
        NewThread {
            account: Some("work".into()),
            ..new_thread("api", "w")
        },
    )
    .await;
    assert_eq!(turn(&hub, &w).await.result, TurnResult::Completed);
    let dir = env.path("acc/work");
    std::fs::remove_dir(&dir).unwrap();
    let why = format!(
        "claude-code:work's directory {} is missing: twr account add claude-code work makes it \
         again and logs it in",
        dir.display()
    );
    let hub = env.restart(hub, env.config(None)).await;
    let end = turn(&hub, &w).await;
    assert_eq!(end.result, TurnResult::Error);
    assert_eq!(end.ext.as_ref().unwrap()["twr"]["error"], json!(why));
    assert_eq!(
        refused(hub.attach_begin(&w, false, "itest").await, codes::CONFLICT),
        why
    );
    let req: JobStart = serde_json::from_value(json!({"argv": ["true"]})).unwrap();
    assert_eq!(
        refused(
            hub.start_job(&w, req, Source::User("itest".into())),
            codes::CONFLICT
        ),
        why
    );
    std::fs::create_dir_all(&dir).unwrap();
    assert_eq!(turn(&hub, &w).await.result, TurnResult::Completed);
    hub.shutdown().await;
}

/// Runs git in `dir` (as a committer of its own); panics when it fails.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.name=t", "-c", "user.email=t@t"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// A git repository at `dir` with one commit.
fn git_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q"]);
    git(dir, &["commit", "-q", "--allow-empty", "-m", "init"]);
}

/// A thread made for a repository path gets the pins of the configured project of that
/// repository however the path names it (its absolute path, a symlink to it, a subdirectory,
/// another worktree of it): an account outside them is refused for the orchestrator, and
/// needs the user's `outside_pin`. Recorded as `ext.twr.account_project`, the pin keeps the
/// thread's moves inside it too (after a restart as well). Another repository has no pins.
#[tokio::test]
async fn a_repository_path_gets_its_projects_pins() {
    let env = Env::new();
    let repo = env.path("acme");
    git_repo(&repo);
    std::fs::create_dir_all(repo.join("sub")).unwrap();
    let link = env.path("acme-link");
    std::os::unix::fs::symlink(&repo, &link).unwrap();
    let wt = env.path("acme-wt");
    git(&repo, &["worktree", "add", "-q", wt.to_str().unwrap()]);
    let mut config = env.config(None);
    config
        .accounts
        .entry("claude-code".into())
        .or_default()
        .insert(
            "work2".into(),
            AccountConfig {
                dir: Some(env.path("acc/work2")),
                ..Default::default()
            },
        );
    config.projects.insert(
        "acme".into(),
        ProjectConfig {
            repo: repo.clone(),
            account: Some(twr_core::account::Pin::parse("work")),
            ..Default::default()
        },
    );
    let hub = env.open(config.clone()).await;
    for path in [&repo, &link, &repo.join("sub"), &wt] {
        let req = || NewThread {
            account: Some("work2".into()),
            ..new_thread(path.to_str().unwrap(), "o")
        };
        let e = refused(
            hub.new_thread_by(req(), &Caller::Orchestrator).await,
            codes::BAD_REQUEST,
        );
        assert_eq!(
            e,
            "project acme pins account work; only the user can open its threads on another \
             account",
            "{}",
            path.display()
        );
        let e = refused(hub.new_thread(req(), "itest").await, codes::BAD_REQUEST);
        assert_eq!(
            e,
            "project acme pins account work: pass outside_pin (twr thread new --outside-pin) to \
             open the thread on claude-code:work2 anyway",
            "{}",
            path.display()
        );
    }

    // Without an account: the project's pin, and the project recorded for its moves.
    let v = hub
        .new_thread_by(
            new_thread(link.join("sub").to_str().unwrap(), "p"),
            &Caller::Orchestrator,
        )
        .await
        .unwrap();
    let p = v["id"].as_str().unwrap().to_string();
    let twr = created_twr(&hub, &p);
    assert_eq!(
        (
            &twr["account"],
            &twr["account_reason"],
            &twr["account_project"]
        ),
        (&json!("work"), &json!("project"), &json!("acme")),
        "{twr}"
    );
    assert!(twr.get("configured_project").is_none(), "{twr}");
    let view = hub.thread_view(&p).unwrap();
    assert_eq!(
        (view.project.as_deref(), view.pins_project()),
        (Some("sub"), Some("acme"))
    );
    // By name, nothing more is recorded: the project is the thread's own.
    let named = create(&hub, new_thread("acme", "named")).await;
    assert!(created_twr(&hub, &named).get("account_project").is_none());

    let hub = env.restart(hub, config.clone()).await;
    let move_to = |caller: Caller| {
        let (hub, p) = (&hub, &p);
        async move {
            hub.set_thread_model(
                p,
                twr_hub::hub::ModelRequest {
                    account: Some("work2"),
                    ..Default::default()
                },
                &caller,
            )
            .await
        }
    };
    assert_eq!(
        refused(move_to(Caller::Orchestrator).await, codes::BAD_REQUEST),
        "project acme pins account work; only the user can move its threads to another account"
    );
    assert_eq!(
        refused(move_to(Caller::user("itest")).await, codes::BAD_REQUEST),
        "project acme pins account work: pass outside_pin (twr model p --outside-pin) to move it \
         to claude-code:work2 anyway"
    );

    // Another repository: no configured project, no pins.
    let other = env.path("other");
    git_repo(&other);
    let v = hub
        .new_thread_by(
            NewThread {
                account: Some("work2".into()),
                ..new_thread(other.to_str().unwrap(), "x")
            },
            &Caller::Orchestrator,
        )
        .await
        .unwrap();
    let twr = created_twr(&hub, v["id"].as_str().unwrap());
    assert_eq!(twr["account"], "work2");
    assert!(twr.get("account_project").is_none(), "{twr}");
    hub.shutdown().await;
}

/// A git that holds `git worktree add` until released (`<dir>/go`), saying so (`<dir>/held`).
struct HeldAdd {
    bin: PathBuf,
    dir: PathBuf,
}

impl HeldAdd {
    fn install(dir: &Path) -> HeldAdd {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir).unwrap();
        let script = format!(
            "#!/bin/sh\nprev=\nfor a in \"$@\"; do\n  \
             if [ \"$prev\" = worktree ] && [ \"$a\" = add ]; then\n    touch '{held}'\n    \
             while [ ! -e '{go}' ]; do sleep 0.02; done\n  fi\n  prev=\"$a\"\ndone\n\
             exec git \"$@\"\n",
            held = dir.join("held").display(),
            go = dir.join("go").display(),
        );
        // Copied into place by `cp` (an open descriptor of this process would make exec'ing
        // it fail with ETXTBSY).
        let (src, bin) = (dir.join("git-held.src"), dir.join("git-held"));
        std::fs::write(&src, script).unwrap();
        let st = std::process::Command::new("cp")
            .arg(&src)
            .arg(&bin)
            .status()
            .unwrap();
        assert!(st.success());
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        HeldAdd {
            bin,
            dir: dir.to_path_buf(),
        }
    }
}

/// The native account removed while a new thread's worktree is being made refuses the thread
/// under the lock it would be recorded with (as a named account's removal does): nothing is
/// recorded, and the worktree made for it is removed again.
#[tokio::test]
async fn a_native_account_removed_while_the_workspace_is_made_refuses_the_thread() {
    let env = Env::new();
    let repo = env.path("repo");
    git_repo(&repo);
    let held = HeldAdd::install(&env.path("bin"));
    let mut o = env.opts(env.config(None));
    o.git_bin = Some(held.bin.clone());
    let hub = Hub::open(o).await.unwrap();
    let make = {
        let (hub, repo) = (hub.clone(), repo.clone());
        tokio::spawn(async move {
            let req = NewThread {
                no_worktree: false,
                worktree: true,
                ..new_thread(repo.to_str().unwrap(), "n")
            };
            hub.new_thread(req, "itest").await
        })
    };
    timeout(WAIT, async {
        while !held.dir.join("held").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("git worktree add never held");
    hub.account_remove(
        AccountRemoveParams {
            runtime: "claude-code".into(),
            account: "default".into(),
            force: true,
            ..Default::default()
        },
        &Caller::user("itest"),
    )
    .await
    .unwrap();
    std::fs::write(held.dir.join("go"), "").unwrap();
    let e = refused(
        timeout(WAIT, make)
            .await
            .expect("thread.new never ended")
            .unwrap(),
        codes::CONFLICT,
    );
    assert_eq!(
        e,
        "claude-code:default changed while the thread was being made; try again"
    );
    assert!(hub.list_threads().is_empty());
    // Its worktree goes again (in the background).
    timeout(WAIT, async {
        while git(&repo, &["worktree", "list", "--porcelain"])
            .lines()
            .filter(|l| l.starts_with("worktree "))
            .count()
            > 1
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the worktree was not removed");
    hub.shutdown().await;
}

/// A named account's directory deleted while a new thread's worktree is being made refuses the
/// thread under the lock it would be recorded with, as its start would be refused: nothing is
/// recorded.
#[tokio::test]
async fn an_accounts_directory_gone_while_the_workspace_is_made_refuses_the_thread() {
    let env = Env::new();
    let repo = env.path("repo");
    git_repo(&repo);
    let held = HeldAdd::install(&env.path("bin"));
    let mut o = env.opts(env.config(None));
    o.git_bin = Some(held.bin.clone());
    let hub = Hub::open(o).await.unwrap();
    let make = {
        let (hub, repo) = (hub.clone(), repo.clone());
        tokio::spawn(async move {
            let req = NewThread {
                no_worktree: false,
                worktree: true,
                account: Some("work".into()),
                ..new_thread(repo.to_str().unwrap(), "n")
            };
            hub.new_thread(req, "itest").await
        })
    };
    timeout(WAIT, async {
        while !held.dir.join("held").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("git worktree add never held");
    let work = env.path("acc/work");
    std::fs::remove_dir_all(&work).unwrap();
    std::fs::write(held.dir.join("go"), "").unwrap();
    let e = refused(
        timeout(WAIT, make)
            .await
            .expect("thread.new never ended")
            .unwrap(),
        codes::CONFLICT,
    );
    assert_eq!(
        e,
        format!(
            "claude-code:work's directory {} is missing: twr account add claude-code work makes \
             it again and logs it in",
            work.display()
        )
    );
    assert!(hub.list_threads().is_empty());
    hub.shutdown().await;
}

/// A configured project's folder pins the repositories in it, whether the folder is in git
/// or not (a folder of repositories, a repository nested in the project's): the closest
/// project holding the path wins.
#[tokio::test]
async fn a_projects_folder_pins_the_repositories_in_it() {
    let env = Env::new();
    let folder = env.path("folder");
    std::fs::create_dir_all(&folder).unwrap();
    let svc = folder.join("svc");
    git_repo(&svc);
    let outer = folder.join("outer");
    git_repo(&outer);
    let inner = outer.join("inner");
    git_repo(&inner);
    let mut config = env.config(None);
    config
        .accounts
        .entry("claude-code".into())
        .or_default()
        .insert(
            "work2".into(),
            AccountConfig {
                dir: Some(env.path("acc/work2")),
                ..Default::default()
            },
        );
    for (name, repo, pin) in [("folder", &folder, "work"), ("outer", &outer, "work2")] {
        config.projects.insert(
            name.into(),
            ProjectConfig {
                repo: repo.clone(),
                account: Some(twr_core::account::Pin::parse(pin)),
                ..Default::default()
            },
        );
    }
    let hub = env.open(config).await;
    for (path, name, account, project) in [
        (&svc, "s", "work", "folder"),
        (&inner, "i", "work2", "outer"),
    ] {
        let id = create(&hub, new_thread(path.to_str().unwrap(), name)).await;
        let twr = created_twr(&hub, &id);
        assert_eq!(
            (
                &twr["account"],
                &twr["account_reason"],
                &twr["account_project"]
            ),
            (&json!(account), &json!("project"), &json!(project)),
            "{}: {twr}",
            path.display()
        );
    }
    hub.shutdown().await;
}

/// A thread made for a repository path whose log names no project for its pins (its project
/// configured only later) has its repository's looked up when it moves: `thread.set_model`
/// and `account.remove {move_to}` keep it inside the pin (which comes before its own pin's
/// refusal).
#[tokio::test]
async fn a_move_looks_up_the_project_the_log_does_not_name() {
    let env = Env::new();
    let repo = env.path("late");
    git_repo(&repo);
    let side = env.path("acc/side");
    std::fs::create_dir_all(&side).unwrap();
    let mut config = env.config(None);
    config
        .accounts
        .entry("claude-code".into())
        .or_default()
        .insert(
            "work2".into(),
            AccountConfig {
                dir: Some(env.path("acc/work2")),
                ..Default::default()
            },
        );
    let hub = env.open(config.clone()).await;
    let user = Caller::user("itest");
    hub.account_add(
        AccountAddParams {
            runtime: "claude-code".into(),
            account: "side".into(),
            dir: Some(side.clone()),
            ..Default::default()
        },
        &user,
    )
    .unwrap();
    let t = create(
        &hub,
        NewThread {
            account: Some("side".into()),
            ..new_thread(repo.to_str().unwrap(), "t")
        },
    )
    .await;
    assert!(created_twr(&hub, &t).get("account_project").is_none());
    config.projects.insert(
        "late".into(),
        ProjectConfig {
            repo: repo.clone(),
            account: Some(twr_core::account::Pin::parse("work")),
            ..Default::default()
        },
    );
    let hub = env.restart(hub, config).await;
    assert_eq!(hub.thread_view(&t).unwrap().pins_project(), None);
    let set = |account: &'static str| {
        let (hub, t, user) = (&hub, &t, &user);
        async move {
            hub.set_thread_model(
                t,
                twr_hub::hub::ModelRequest {
                    account: Some(account),
                    ..Default::default()
                },
                user,
            )
            .await
        }
    };
    assert_eq!(
        refused(set("work2").await, codes::BAD_REQUEST),
        "project late pins account work: pass outside_pin (twr model t --outside-pin) to move it \
         to claude-code:work2 anyway"
    );
    let e = refused(
        hub.account_remove(
            AccountRemoveParams {
                runtime: "claude-code".into(),
                account: "side".into(),
                move_to: Some("work2".into()),
                ..Default::default()
            },
            &user,
        )
        .await,
        codes::CONFLICT,
    );
    assert!(
        e.contains("t (project late pins account work: pass outside_pin"),
        "{e}"
    );
    hub.shutdown().await;
}

/// A move acts on the thread its project was looked up for: `thread.set_model` resolves its
/// thread once, before the lookup, so another thread renamed meanwhile to the name asked for is
/// not moved, and the one looked up keeps to its project's pin. `account.remove {move_to}`
/// also looks up a thread that came onto the account during its lookups before moving it.
#[tokio::test]
async fn a_move_acts_on_the_thread_it_looked_up() {
    let env = Env::new();
    let repo = env.path("late");
    git_repo(&repo);
    let side = env.path("acc/side");
    std::fs::create_dir_all(&side).unwrap();
    let mut config = env.config(None);
    config
        .accounts
        .entry("claude-code".into())
        .or_default()
        .insert(
            "work2".into(),
            AccountConfig {
                dir: Some(env.path("acc/work2")),
                ..Default::default()
            },
        );
    let hub = env.open(config.clone()).await;
    let user = Caller::user("itest");
    hub.account_add(
        AccountAddParams {
            runtime: "claude-code".into(),
            account: "side".into(),
            dir: Some(side.clone()),
            ..Default::default()
        },
        &user,
    )
    .unwrap();
    // Moved there, not made there: no pin of their own.
    async fn to_side(hub: &Hub, t: String) -> String {
        let req = twr_hub::hub::ModelRequest {
            account: Some("side"),
            ..Default::default()
        };
        hub.set_thread_model(&t, req, &Caller::user("itest"))
            .await
            .unwrap();
        t
    }
    let late = create(&hub, new_thread(repo.to_str().unwrap(), "late-main")).await;
    let late = to_side(&hub, late).await;
    let other = create(&hub, new_thread("api", "other")).await;
    config.projects.insert(
        "late".into(),
        ProjectConfig {
            repo: repo.clone(),
            account: Some(twr_core::account::Pin::parse("work")),
            ..Default::default()
        },
    );
    let gate = LookupGate::default();
    let hub = env.restart_with(hub, config, Some(&gate)).await;
    assert_eq!(hub.thread_view(&late).unwrap().pins_project(), None);
    let account_of = |t: &str| hub.thread_view(t).unwrap().account;
    let other_before = account_of(&other);

    // "lat" names late-main (a prefix) when the move looks it up; then other is renamed "lat".
    let (on_hold, release) = gate.arm();
    let moved = hub.set_thread_model(
        "lat",
        twr_hub::hub::ModelRequest {
            account: Some("work2"),
            ..Default::default()
        },
        &user,
    );
    let rename = async {
        assert_eq!(on_hold.await.unwrap(), late);
        hub.rename_thread(&other, "lat", &user).unwrap();
        release.send(()).unwrap();
    };
    let (moved, ()) = tokio::join!(moved, rename);
    assert_eq!(
        refused(moved, codes::BAD_REQUEST),
        "project late pins account work: pass outside_pin (twr model late-main --outside-pin) to \
         move it to claude-code:work2 anyway"
    );
    assert_eq!(account_of(&other), other_before);
    assert_eq!(account_of(&late).as_deref(), Some("side"));
    assert_eq!(gate.seen(), std::slice::from_ref(&late));

    // A thread moved onto the account while account.remove looks up late-main's project
    // (its own move looks it up once): looked up again, then both move.
    let (on_hold, release) = gate.arm();
    let removal = hub.account_remove(
        AccountRemoveParams {
            runtime: "claude-code".into(),
            account: "side".into(),
            move_to: Some("work".into()),
            ..Default::default()
        },
        &user,
    );
    let joiner = async {
        assert_eq!(on_hold.await.unwrap(), late);
        let v = create(&hub, new_thread("api", "v")).await;
        let v = to_side(&hub, v).await;
        release.send(()).unwrap();
        v
    };
    let (removed, v) = tokio::join!(removal, joiner);
    let removed = removed.unwrap();
    assert_eq!(removed["moved"], json!(["late-main", "v"]), "{removed}");
    assert_eq!(
        gate.seen(),
        [late.clone(), late.clone(), v.clone(), v.clone()]
    );
    for t in [&late, &v] {
        assert_eq!(account_of(t).as_deref(), Some("work"));
    }
    hub.shutdown().await;
}
