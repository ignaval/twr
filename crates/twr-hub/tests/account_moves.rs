//! Moving a thread to another account (`thread.set_model {account}`, `runtime`
//! `<runtime>:<account>`; `hub::switch`, `hub::thread_account`): the old runtime stops with
//! `account_change` (`runtime_change` when the runtime changes too), the next turn spawns a new
//! session on the new account (its environment) with the handoff naming the previous account,
//! the start is stamped with the account, usage is recorded on the account the reporting
//! session ran on, pins are kept, and `account.remove {move_to}` moves every thread on the
//! account, or none.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::Notify;
use tokio::time::timeout;
use twr_core::adapter::{Adapter, AdapterCmd, AdapterHandle, AdapterInfo, SpawnConfig, Tier};
use twr_core::config::{AccountConfig, BoxConfig, Config, ProjectConfig};
use twr_core::event::*;
use twr_core::paths::Paths;
use twr_core::proto::{codes, RpcError};
use twr_core::usage::UsageWindow;
use twr_hub::hub::{AccountAddParams, AccountRemoveParams, ModelRequest, NativeDirFn};
use twr_hub::{Caller, Hub, HubOptions, JobStart, NewThread};

const WAIT: Duration = Duration::from_secs(20);

/// One start of a runtime: which one, its configuration and its first prompt.
#[derive(Debug, Clone)]
struct Start {
    runtime: &'static str,
    cfg: SpawnConfig,
    prompt: Option<String>,
}

#[derive(Default)]
struct Shared {
    starts: Mutex<Vec<Start>>,
    /// Released once per held turn (a prompt that ends with `HOLD`).
    hold: Notify,
    sessions: AtomicUsize,
}

/// A runtime (played under `claude-code` and `codex`) that records each start, reports a new
/// session `<runtime>-<n>` (or the one it resumes), ends every turn with `done` (a prompt that
/// ends with `HOLD` waits for [`Shared::hold`] first; one that ends with `USAGE` reports usage
/// first: a handoff quotes earlier prompts before the message).
struct MoveAdapter {
    name: &'static str,
    shared: Arc<Shared>,
}

impl MoveAdapter {
    fn start(&self, cfg: SpawnConfig, prompt: Option<String>) -> AdapterHandle {
        self.shared.starts.lock().unwrap().push(Start {
            runtime: self.name,
            cfg: cfg.clone(),
            prompt: prompt.clone(),
        });
        let n = self.shared.sessions.fetch_add(1, Ordering::SeqCst) + 1;
        let session = cfg
            .resume_session
            .clone()
            .unwrap_or_else(|| format!("{}-{n}", self.name));
        let shared = Arc::clone(&self.shared);
        let (h, mut end) = AdapterHandle::pair();
        tokio::spawn(async move {
            end.emit(
                EventBody::ThreadStarted(SessionPayload {
                    session_id: Some(session.clone()),
                    ..Default::default()
                }),
                None,
            );
            let mut pending = prompt.map(|p| (p, cfg.prompt_turn.unwrap_or(1)));
            loop {
                if let Some((text, turn)) = pending.take() {
                    if text.ends_with("HOLD") {
                        shared.hold.notified().await;
                    }
                    if text.ends_with("USAGE") {
                        end.emit(
                            EventBody::RuntimeLimits(RuntimeLimits {
                                windows: vec![UsageWindow {
                                    window: "five_hour".into(),
                                    used_pct: 40.0,
                                    resets_at: None,
                                    minutes: None,
                                    observed_at: None,
                                }],
                                source: "test".into(),
                                ..Default::default()
                            }),
                            None,
                        );
                    }
                    end.emit(
                        EventBody::AgentText(AgentText {
                            text: "done".into(),
                            is_final: true,
                            ..Default::default()
                        }),
                        Some(turn),
                    );
                    end.emit(
                        EventBody::TurnEnded(TurnEnded {
                            result: TurnResult::Completed,
                            session_id: Some(session.clone()),
                            ..Default::default()
                        }),
                        Some(turn),
                    );
                }
                match end.cmds.recv().await {
                    Some(AdapterCmd::Send { text, turn }) => pending = Some((text, turn)),
                    Some(AdapterCmd::Stop | AdapterCmd::Interrupt) | None => break,
                    Some(_) => {}
                }
            }
            end.exited(Some(0), "");
        });
        h
    }
}

impl Adapter for MoveAdapter {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: self.name,
            tier: Tier::Headless,
            description: "account move test runtime",
        }
    }

    fn spawn(&self, cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        Ok(self.start(cfg, prompt))
    }

    fn resume(&self, cfg: SpawnConfig, session_id: String) -> anyhow::Result<AdapterHandle> {
        let cfg = SpawnConfig {
            resume_session: Some(session_id),
            ..cfg
        };
        Ok(self.start(cfg, None))
    }
}

/// A scratch home: projects `api` (no pin) and `acme-api` (pinned to `work`), the accounts'
/// directories, and every start the runtimes saw.
struct Env {
    home: tempfile::TempDir,
    shared: Arc<Shared>,
}

impl Env {
    fn new() -> Env {
        let home = tempfile::tempdir().unwrap();
        for d in [
            "api",
            "acme-api",
            "acc/work",
            "acc/work2",
            "acc/work-moved",
            "acc/team",
        ] {
            std::fs::create_dir_all(home.path().join(d)).unwrap();
        }
        Env {
            home,
            shared: Arc::default(),
        }
    }

    fn path(&self, p: &str) -> PathBuf {
        self.home.path().join(p)
    }

    fn dir(&self, p: &str) -> String {
        self.path(p).to_string_lossy().into_owned()
    }

    /// The database's schema version (`PRAGMA user_version`): raised once account data is
    /// recorded (`twr_hub::store`).
    fn db_version(&self) -> i32 {
        twr_hub::store::connect(&Paths::with_home(self.home.path()).db_file())
            .unwrap()
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap()
    }

    /// `[accounts.claude-code.work]` in `work_dir` (default `acc/work`), `work2`,
    /// `[accounts.codex.team]`; projects `api` and `acme-api` (`account = "work"`).
    fn config(&self, work_dir: Option<&Path>) -> Config {
        let mut c = Config {
            default_runtime: "claude-code".into(),
            ..Default::default()
        };
        let mut account = |rt: &str, name: &str, dir: PathBuf| {
            c.accounts.entry(rt.into()).or_default().insert(
                name.into(),
                AccountConfig {
                    dir: Some(dir),
                    ..Default::default()
                },
            );
        };
        let work = work_dir.map_or_else(|| self.path("acc/work"), Path::to_path_buf);
        account("claude-code", "work", work);
        account("claude-code", "work2", self.path("acc/work2"));
        account("codex", "team", self.path("acc/team"));
        c.projects.insert(
            "api".into(),
            ProjectConfig {
                repo: self.path("api"),
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
        o.git_tick = Some(Duration::ZERO);
        o.disk_tick = Some(Duration::ZERO);
        o.trash_tick = Some(Duration::ZERO);
        o.stall_after = Some(Duration::ZERO);
        o.stop_timeout = Duration::from_secs(5);
        let claude: Arc<dyn Adapter> = Arc::new(MoveAdapter {
            name: "claude-code",
            shared: Arc::clone(&self.shared),
        });
        let codex: Arc<dyn Adapter> = Arc::new(MoveAdapter {
            name: "codex",
            shared: Arc::clone(&self.shared),
        });
        o.adapters = Arc::new(move |name: &str| -> Option<Arc<dyn Adapter>> {
            match name {
                "claude-code" => Some(Arc::clone(&claude)),
                "codex" => Some(Arc::clone(&codex)),
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
        hub.shutdown().await;
        drop(hub);
        timeout(WAIT, async {
            loop {
                match Hub::open(self.opts(config.clone())).await {
                    Ok(h) => return h,
                    Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
                }
            }
        })
        .await
        .expect("reopening the hub")
    }

    fn starts(&self, id: &str) -> Vec<Start> {
        self.shared
            .starts
            .lock()
            .unwrap()
            .iter()
            .filter(|s| s.cfg.thread == id)
            .cloned()
            .collect()
    }

    fn last_start(&self, id: &str) -> Start {
        self.starts(id)
            .pop()
            .unwrap_or_else(|| panic!("no start of {id}"))
    }
}

fn user() -> Caller {
    Caller::user("itest")
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

/// `thread.set_model {account}` by `caller`.
async fn move_to(
    hub: &Hub,
    thread: &str,
    account: &str,
    outside_pin: bool,
    caller: &Caller,
) -> Result<Value, RpcError> {
    hub.set_thread_model(
        thread,
        ModelRequest {
            account: Some(account),
            outside_pin,
            ..Default::default()
        },
        caller,
    )
    .await
}

/// `message` of an RPC error, after checking its code.
fn refused(r: Result<impl std::fmt::Debug, RpcError>, code: &str) -> String {
    let e = r.unwrap_err();
    assert_eq!(e.code, code, "{}", e.message);
    e.message
}

fn events(hub: &Hub, thread: &str) -> Vec<Event> {
    hub.thread_events(thread, 0, 10_000).unwrap().1
}

/// Waits until `pred` holds of the thread's events; returns them.
async fn until(hub: &Hub, thread: &str, pred: impl Fn(&[Event]) -> bool) -> Vec<Event> {
    timeout(WAIT, async {
        loop {
            let evs = events(hub, thread);
            if pred(&evs) {
                return evs;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        let evs = events(hub, thread);
        let kinds: Vec<&str> = evs.iter().map(|e| e.kind()).collect();
        panic!("timed out; saw {kinds:?}")
    })
}

fn turn_ends(evs: &[Event]) -> usize {
    evs.iter()
        .filter(|e| matches!(e.body, EventBody::TurnEnded(_)))
        .count()
}

/// Sends `prompt` and waits for its turn's end.
async fn turn(hub: &Hub, thread: &str, prompt: &str) {
    let before = turn_ends(&events(hub, thread));
    hub.send(thread, prompt, TurnFrom::User, Source::User("itest".into()))
        .unwrap();
    until(hub, thread, |evs| turn_ends(evs) > before).await;
}

/// The `ext.twr.reason`s of the thread's settings-restart stops.
fn restart_reasons(evs: &[Event]) -> Vec<String> {
    evs.iter()
        .filter_map(|e| match &e.body {
            EventBody::ThreadStopped(p) if is_settings_restart(p) => p.ext.as_ref()?["twr"]
                ["reason"]
                .as_str()
                .map(str::to_string),
            _ => None,
        })
        .collect()
}

/// Waits for a settings-restart stop of the thread; returns every reason so far.
async fn stopped_for(hub: &Hub, thread: &str, n: usize) -> Vec<String> {
    restart_reasons(&until(hub, thread, |evs| restart_reasons(evs).len() >= n).await)
}

fn moves(evs: &[Event]) -> Vec<ThreadModelChanged> {
    evs.iter()
        .filter_map(|e| match &e.body {
            EventBody::ThreadModelChanged(p) => Some(p.clone()),
            _ => None,
        })
        .collect()
}

/// The `ext.twr` of each `thread.started`.
fn start_stamps(evs: &[Event]) -> Vec<Value> {
    evs.iter()
        .filter_map(|e| match &e.body {
            EventBody::ThreadStarted(p) => p.ext.as_ref().map(|x| x["twr"].clone()),
            _ => None,
        })
        .collect()
}

fn get<'a>(env: &'a [(String, String)], k: &str) -> Vec<&'a str> {
    env.iter()
        .filter(|(n, _)| n == k)
        .map(|(_, v)| v.as_str())
        .collect()
}

fn view(hub: &Hub, thread: &str) -> Value {
    hub.get_thread(thread).unwrap()
}

/// The handoff the thread's latest turn carried (`turn.started` `ext.twr.runtime_handoff`).
fn last_handoff(hub: &Hub, thread: &str) -> Option<String> {
    events(hub, thread)
        .iter()
        .rev()
        .find_map(|e| match &e.body {
            EventBody::TurnStarted(p) => Some(
                p.ext.as_ref()?["twr"]
                    .get("runtime_handoff")?
                    .as_str()?
                    .to_string(),
            ),
            _ => None,
        })
}

/// The session the thread's view has now.
fn session(hub: &Hub, thread: &str) -> String {
    view(hub, thread)["session_id"]
        .as_str()
        .expect("a session")
        .to_string()
}

/// An idle thread on the native account moves to `work`: its runtime stops (`account_change`),
/// the next turn spawns a new session (not a resume) with the account's environment and the
/// handoff naming the previous account, and that start is stamped `ext.twr.account`. Usage is
/// recorded on the account each session ran on. After a hub restart, the thread resumes its
/// session on `work`. Back to the native account by name resumes the session it left there,
/// told what it did on `work` since; then the same account again (no move).
#[tokio::test]
async fn an_idle_thread_moves_to_another_account_with_a_handoff() {
    let env = Env::new();
    let hub = env.open(env.config(None)).await;
    let a = create(&hub, new_thread("api", "a")).await;
    turn(&hub, &a, "USAGE").await;
    let first = env.last_start(&a);
    assert!(get(&first.cfg.env, "CLAUDE_CONFIG_DIR").is_empty());
    let old_session = view(&hub, &a)["session_id"].as_str().unwrap().to_string();
    assert!(start_stamps(&events(&hub, &a))[0].get("account").is_none());
    // Nothing on an account yet: a twr before accounts may still open the database.
    assert_eq!(env.db_version(), twr_hub::store::SCHEMA_VERSION);

    let r = move_to(&hub, &a, "work", false, &user()).await.unwrap();
    // The move is account data: from it, such a twr refuses the database.
    assert_eq!(env.db_version(), twr_hub::store::ACCOUNTS_SCHEMA_VERSION);
    for (k, want) in [
        ("changed", json!(true)),
        ("account_changed", json!(true)),
        ("account", json!("work")),
        ("previous_account", json!("default")),
        ("runtime", json!("claude-code")),
        ("previous_runtime", json!("claude-code")),
        ("runtime_changed", json!(false)),
        ("applies", json!("now")),
        (
            "message",
            json!(
                "a moves to claude-code:work; its next turn starts a new session there with \
                 a handoff"
            ),
        ),
    ] {
        assert_eq!(r[k], want, "{k}: {r}");
    }
    assert_eq!(stopped_for(&hub, &a, 1).await, ["account_change"]);
    let mv = moves(&events(&hub, &a)).pop().unwrap();
    assert!(mv.account_changed);
    assert_eq!(
        (mv.account.as_deref(), mv.previous_account.as_deref()),
        (Some("work"), None)
    );
    assert_eq!(
        (mv.runtime.as_deref(), mv.previous_runtime.as_deref()),
        (Some("claude-code"), Some("claude-code"))
    );
    let twr = mv.ext.as_ref().unwrap()["twr"].clone();
    assert_eq!(twr["account_dir"], json!(env.dir("acc/work")));
    assert_eq!(twr["account_reason"], json!("explicit"));
    assert_eq!(twr["previous_session"], json!(old_session));
    assert_eq!(mv.by, ChangedBy::User);
    let v = view(&hub, &a);
    assert_eq!(v["account"], json!("work"));
    assert_eq!(v["handoff_pending"], json!(true), "{v}");

    // The next turn: a new session on `work`, with the handoff.
    turn(&hub, &a, "USAGE").await;
    let s = env.last_start(&a);
    assert!(s.cfg.resume_session.is_none(), "a spawn, not a resume");
    assert_eq!(
        get(&s.cfg.env, "CLAUDE_CONFIG_DIR"),
        [env.dir("acc/work").as_str()]
    );
    assert!(s.cfg.env_remove.iter().any(|k| k == "ANTHROPIC_API_KEY"));
    let prompt = s.prompt.unwrap();
    assert!(
        prompt.starts_with("[twr handoff] This thread (a) moved to another account:"),
        "{prompt}"
    );
    for line in [
        format!(
            "- previous runtime: claude-code:default (model default, effort default, session \
             {old_session})"
        ),
        "- previous account: default (claude-code)".into(),
        "- now: claude-code:work (model default, effort default)".into(),
    ] {
        assert!(prompt.contains(&line), "{line}\n{prompt}");
    }
    assert!(prompt.ends_with("USAGE"), "{prompt}");
    let stamps = start_stamps(&events(&hub, &a));
    assert_eq!(stamps.last().unwrap()["account"], json!("work"));
    assert_eq!(view(&hub, &a)["handoff_pending"], json!(false));
    // Usage: the native session's report, then `work`'s.
    let (items, _) = hub.control_read(0, 1000).unwrap();
    let usage: Vec<Option<String>> = items
        .iter()
        .filter_map(|(_, e)| match &e.body {
            EventBody::AccountUsage(u) => Some(u.account.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(usage, [None, Some("work".to_string())]);

    // A restart: the thread resumes its `work` session in `work`'s environment.
    let new_session = view(&hub, &a)["session_id"].as_str().unwrap().to_string();
    let hub = env.restart(hub, env.config(None)).await;
    assert_eq!(view(&hub, &a)["account"], json!("work"));
    turn(&hub, &a, "hi").await;
    let s = env.last_start(&a);
    assert_eq!(s.cfg.resume_session.as_deref(), Some(new_session.as_str()));
    assert_eq!(
        get(&s.cfg.env, "CLAUDE_CONFIG_DIR"),
        [env.dir("acc/work").as_str()]
    );

    // Back to the native account, by its name: the session it left there resumes (its own
    // account's), told what the thread did on `work` since.
    let r = move_to(&hub, &a, "default", false, &user()).await.unwrap();
    assert_eq!(
        (&r["account"], &r["previous_account"]),
        (&json!("default"), &json!("work"))
    );
    assert_eq!(r["resumes"], json!(&old_session), "{r}");
    assert_eq!(
        r["message"],
        json!(format!(
            "a moves from claude-code:work back to claude-code:default: its earlier \
             claude-code:default session ({old_session}) resumes with its next turn, told what \
             the thread did since it left it"
        ))
    );
    let mv = moves(&events(&hub, &a)).pop().unwrap();
    assert_eq!(
        (mv.account.as_deref(), mv.previous_account.as_deref()),
        (None, Some("work"))
    );
    let twr = &mv.ext.as_ref().unwrap()["twr"];
    assert!(twr.get("account_dir").is_none());
    assert_eq!(twr["resume_session"], json!(&old_session));
    turn(&hub, &a, "hi").await;
    let s = env.last_start(&a);
    assert_eq!(s.cfg.resume_session.as_deref(), Some(old_session.as_str()));
    assert!(get(&s.cfg.env, "CLAUDE_CONFIG_DIR").is_empty());
    assert!(s.cfg.env_remove.is_empty());
    let handoff = last_handoff(&hub, &a).expect("the resume's handoff");
    assert!(
        handoff.starts_with(
            "[twr handoff] This thread (a) comes back to you: you are its earlier \
             claude-code:default session, resumed. Since you left it, it ran on \
             claude-code:work for 2 turns"
        ),
        "{handoff}"
    );
    assert!(start_stamps(&events(&hub, &a))
        .last()
        .unwrap()
        .get("account")
        .is_none());

    // The account it is on already: nothing changes.
    let count = moves(&events(&hub, &a)).len();
    let r = move_to(&hub, &a, "default", false, &user()).await.unwrap();
    assert_eq!(r["changed"], json!(false), "{r}");
    assert_eq!(r["account_changed"], json!(false));
    assert_eq!(
        r["message"],
        json!(
            "a already runs on claude-code:default with the runtime's default model and its \
             default effort"
        )
    );
    assert_eq!(moves(&events(&hub, &a)).len(), count);
    hub.shutdown().await;
}

/// A move between accounts of one runtime keeps the thread's model and effort; `--runtime
/// <runtime>:<account>` on the thread's own runtime is such a move. A move to another runtime
/// on a named account (`codex:team` -> `claude-code:work`) changes both: `runtime_change`, the
/// new runtime's defaults, a handoff naming both accounts.
#[tokio::test]
async fn runtime_account_specs_move_and_account_only_moves_keep_the_model() {
    let env = Env::new();
    let hub = env.open(env.config(None)).await;
    let c = create(
        &hub,
        NewThread {
            runtime: Some("codex".into()),
            model: Some("gpt-x".into()),
            effort: Some("high".into()),
            ..new_thread("api", "c")
        },
    )
    .await;
    turn(&hub, &c, "hi").await;
    let r = hub
        .set_runtime_model(&c, Some("codex:team"), None, None, &user())
        .unwrap();
    assert_eq!(r["account_changed"], json!(true), "{r}");
    assert_eq!(r["runtime_changed"], json!(false));
    assert_eq!(
        (&r["model"], &r["effort"]),
        (&json!("gpt-x"), &json!("high"))
    );
    let mv = moves(&events(&hub, &c)).pop().unwrap();
    assert_eq!(
        (mv.model.as_deref(), mv.effort.as_deref()),
        (Some("gpt-x"), Some("high"))
    );
    assert_eq!(stopped_for(&hub, &c, 1).await, ["account_change"]);
    turn(&hub, &c, "hi").await;
    let s = env.last_start(&c);
    assert_eq!(s.runtime, "codex");
    assert!(s.cfg.resume_session.is_none());
    assert_eq!(
        (s.cfg.model.as_deref(), s.cfg.effort.as_deref()),
        (Some("gpt-x"), Some("high"))
    );
    assert_eq!(
        get(&s.cfg.env, "CODEX_HOME"),
        [env.dir("acc/team").as_str()]
    );

    // codex:team -> claude-code:work: the runtime and the account.
    let r = hub
        .set_runtime_model(&c, Some("claude-code:work"), None, None, &user())
        .unwrap();
    for (k, want) in [
        ("runtime_changed", json!(true)),
        ("account_changed", json!(true)),
        ("account", json!("work")),
        ("previous_account", json!("team")),
        ("model", Value::Null),
        ("effort", Value::Null),
    ] {
        assert_eq!(r[k], want, "{k}: {r}");
    }
    assert_eq!(
        stopped_for(&hub, &c, 2).await,
        ["account_change", "runtime_change"]
    );
    turn(&hub, &c, "hi").await;
    let s = env.last_start(&c);
    assert_eq!(s.runtime, "claude-code");
    assert_eq!(
        get(&s.cfg.env, "CLAUDE_CONFIG_DIR"),
        [env.dir("acc/work").as_str()]
    );
    let prompt = s.prompt.unwrap();
    assert!(
        prompt.starts_with("[twr handoff] This thread (c) moved to another runtime:"),
        "{prompt}"
    );
    assert!(prompt.contains("- previous runtime: codex:team (model gpt-x, effort high"));
    assert!(!prompt.contains("- previous account:"), "{prompt}");
    assert!(prompt.contains("- now: claude-code:work (model default, effort default)"));

    // `claude-code:work2` on a claude-code thread: an account move.
    let r = hub
        .set_runtime_model(&c, Some("claude-code:work2"), None, None, &user())
        .unwrap();
    assert_eq!(
        (&r["account_changed"], &r["runtime_changed"], &r["account"]),
        (&json!(true), &json!(false), &json!("work2"))
    );
    // Both given, they must agree.
    let e = hub
        .set_thread_model(
            &c,
            ModelRequest {
                runtime: Some("claude-code:work"),
                account: Some("work2"),
                ..Default::default()
            },
            &user(),
        )
        .await;
    assert_eq!(
        refused(e, codes::BAD_REQUEST),
        "the runtime names account work and account names work2: give the account once"
    );
    hub.shutdown().await;
}

/// A running thread moves once its turn ended: the change applies `after_turn`, the old
/// runtime finishes the turn, then stops (`account_change`).
#[tokio::test]
async fn a_running_thread_moves_after_its_turn() {
    let env = Env::new();
    let hub = env.open(env.config(None)).await;
    let t = create(&hub, new_thread("api", "t")).await;
    turn(&hub, &t, "hi").await;
    hub.send(&t, "HOLD", TurnFrom::User, Source::User("itest".into()))
        .unwrap();
    until(&hub, &t, |evs| {
        evs.iter()
            .filter(|e| matches!(e.body, EventBody::TurnStarted(_)))
            .count()
            == 2
    })
    .await;
    let r = move_to(&hub, &t, "work", false, &user()).await.unwrap();
    assert_eq!(r["applies"], json!("after_turn"), "{r}");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(restart_reasons(&events(&hub, &t)).is_empty());
    env.shared.hold.notify_one();
    assert_eq!(stopped_for(&hub, &t, 1).await, ["account_change"]);
    let evs = events(&hub, &t);
    let stop = evs
        .iter()
        .position(|e| matches!(e.body, EventBody::ThreadStopped(_)))
        .unwrap();
    assert!(turn_ends(&evs[..stop]) == 2, "the held turn ended first");
    turn(&hub, &t, "hi").await;
    let s = env.last_start(&t);
    assert!(s.cfg.resume_session.is_none());
    assert_eq!(
        get(&s.cfg.env, "CLAUDE_CONFIG_DIR"),
        [env.dir("acc/work").as_str()]
    );
    hub.shutdown().await;
}

/// Pins: the project's pin and the thread's own keep a move inside them: refused for the
/// orchestrator always, for the user unless `outside_pin` (recorded as `override`). Unknown
/// accounts and remote boxes are refused like a new thread's.
#[tokio::test]
async fn moves_keep_to_the_pins() {
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
    let p = create(&hub, new_thread("acme-api", "p")).await;
    assert_eq!(view(&hub, &p)["account"], json!("work"));
    assert_eq!(
        refused(
            move_to(&hub, &p, "work2", false, &user()).await,
            codes::BAD_REQUEST
        ),
        "project acme-api pins account work: pass outside_pin (twr model p --outside-pin) to \
         move it to claude-code:work2 anyway"
    );
    for outside in [false, true] {
        assert_eq!(
            refused(
                move_to(&hub, &p, "work2", outside, &Caller::Orchestrator).await,
                codes::BAD_REQUEST
            ),
            "project acme-api pins account work; only the user can move its threads to another \
             account"
        );
    }
    let r = move_to(&hub, &p, "work2", true, &user()).await.unwrap();
    assert_eq!(r["account"], json!("work2"), "{r}");
    let mv = moves(&events(&hub, &p)).pop().unwrap();
    assert_eq!(mv.ext.unwrap()["twr"]["account_reason"], json!("override"));
    // Back inside the pin, the orchestrator may move it (no confirm card: same runtime).
    let r = move_to(&hub, &p, "work", false, &Caller::Orchestrator)
        .await
        .unwrap();
    assert_eq!(
        (&r["account"], &r["changed"]),
        (&json!("work"), &json!(true)),
        "{r}"
    );
    assert_eq!(
        moves(&events(&hub, &p)).pop().unwrap().by,
        ChangedBy::Orchestrator
    );

    // The thread's own pin.
    let o = create(
        &hub,
        NewThread {
            account: Some("work".into()),
            ..new_thread("api", "o")
        },
    )
    .await;
    assert_eq!(
        refused(
            move_to(&hub, &o, "work2", false, &user()).await,
            codes::BAD_REQUEST
        ),
        "o is pinned to account work: pass outside_pin (twr model o --outside-pin) to move it \
         to claude-code:work2 anyway"
    );
    // Its own pin is for the runtime it was made on: another runtime's accounts are free.
    let r = hub
        .set_runtime_model(&o, Some("codex:team"), None, None, &user())
        .unwrap();
    assert_eq!(r["account"], json!("team"), "{r}");

    // Unknown accounts, as for a new thread.
    assert_eq!(
        refused(
            move_to(&hub, &p, "nope", false, &user()).await,
            codes::BAD_REQUEST
        ),
        "claude-code has no account \"nope\": twr account add claude-code nope"
    );

    // A thread on a remote box.
    hub.append(
        "remote-1",
        None,
        Source::User("itest".into()),
        EventBody::ThreadCreated(ThreadCreated {
            name: "r".into(),
            box_name: "gpu".into(),
            runtime: "claude-code".into(),
            mode: "manual".into(),
            ..Default::default()
        }),
        None,
    )
    .unwrap();
    assert_eq!(
        refused(
            move_to(&hub, "r", "work", false, &user()).await,
            codes::UNSUPPORTED
        ),
        "accounts on remote boxes aren't supported yet: use the box's own login (twr box login \
         <box> <agent>)"
    );
    hub.shutdown().await;
}

/// A list or a glob is chosen from as for a new thread (its first usable account) before it is
/// compared with the account the thread runs on: a thread on a later candidate moves to the
/// first one, and is left alone only when the first usable one is its own.
#[tokio::test]
async fn a_pool_moves_to_its_first_usable_account() {
    let env = Env::new();
    let hub = env.open(env.config(None)).await;
    // On work2 by a move: no pin of its own.
    let t = create(&hub, new_thread("api", "t")).await;
    move_to(&hub, &t, "work2", false, &user()).await.unwrap();
    assert_eq!(view(&hub, &t)["account"], json!("work2"));
    assert!(view(&hub, &t).get("account_pin").is_none());
    let moved = |r: &Value| (r["account_changed"].clone(), r["account"].clone());

    let r = move_to(&hub, &t, "work,work2", false, &user())
        .await
        .unwrap();
    assert_eq!(moved(&r), (json!(true), json!("work")), "{r}");
    let r = move_to(&hub, &t, "work,work2", false, &user())
        .await
        .unwrap();
    assert_ne!(r["account_changed"], json!(true), "{r}");
    move_to(&hub, &t, "work2", false, &user()).await.unwrap();
    let before = moves(&events(&hub, &t)).len();
    // Its own account first: no move.
    let r = move_to(&hub, &t, "work2,work", false, &user())
        .await
        .unwrap();
    assert_ne!(r["account_changed"], json!(true), "{r}");
    assert_eq!(view(&hub, &t)["account"], json!("work2"));
    // A glob: its matches in name order, work first.
    let r = move_to(&hub, &t, "work*", false, &user()).await.unwrap();
    assert_eq!(moved(&r), (json!(true), json!("work")), "{r}");
    let r = move_to(&hub, &t, "work*", false, &user()).await.unwrap();
    assert_ne!(r["account_changed"], json!(true), "{r}");
    assert_eq!(view(&hub, &t)["account"], json!("work"));
    // Since the move back to work2, only the glob's move was recorded.
    let account_moves = moves(&events(&hub, &t))[before..]
        .iter()
        .filter(|m| m.account_changed)
        .count();
    assert_eq!(account_moves, 1);
    hub.shutdown().await;
}

/// A config change that moves an account's directory makes `--account <same name>` a move (a
/// new session in the new directory), recorded with the new directory.
#[tokio::test]
async fn rebinding_after_a_directory_change_is_a_move() {
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
    turn(&hub, &w, "hi").await;
    // The same account in the same directory: no move.
    let r = move_to(&hub, &w, "work", false, &user()).await.unwrap();
    assert_eq!(r["changed"], json!(false), "{r}");

    let moved = env.path("acc/work-moved");
    let hub = env.restart(hub, env.config(Some(&moved))).await;
    let r = move_to(&hub, &w, "work", false, &user()).await.unwrap();
    for (k, want) in [
        ("changed", json!(true)),
        ("account_changed", json!(true)),
        ("account", json!("work")),
        ("previous_account", json!("work")),
    ] {
        assert_eq!(r[k], want, "{k}: {r}");
    }
    let mv = moves(&events(&hub, &w)).pop().unwrap();
    assert_eq!(
        (mv.account.as_deref(), mv.previous_account.as_deref()),
        (Some("work"), Some("work"))
    );
    assert_eq!(
        mv.ext.unwrap()["twr"]["account_dir"],
        json!(env.dir("acc/work-moved"))
    );
    turn(&hub, &w, "hi").await;
    let s = env.last_start(&w);
    assert!(s.cfg.resume_session.is_none());
    assert_eq!(
        get(&s.cfg.env, "CLAUDE_CONFIG_DIR"),
        [env.dir("acc/work-moved").as_str()]
    );
    hub.shutdown().await;
}

/// An account whose configured directory is a link: the thread records where it leads, and
/// its runtimes get the link as configured. The link retargeted (across a hub restart) is a
/// directory change: the thread's start is refused, saying where the link leads now, until
/// `--account <same name>` moves it there (a new session), recorded with the new directory.
#[tokio::test]
async fn a_retargeted_account_link_is_a_directory_change() {
    let env = Env::new();
    let link = env.path("acc/work-link");
    std::os::unix::fs::symlink(env.path("acc/work"), &link).unwrap();
    let (work, moved) = (
        env.path("acc/work").canonicalize().unwrap(),
        env.path("acc/work-moved").canonicalize().unwrap(),
    );
    let config = env.config(Some(&link));
    let hub = env.open(config.clone()).await;
    let w = create(
        &hub,
        NewThread {
            account: Some("work".into()),
            ..new_thread("api", "w")
        },
    )
    .await;
    turn(&hub, &w, "hi").await;
    let created = events(&hub, &w)
        .into_iter()
        .find_map(|e| match e.body {
            EventBody::ThreadCreated(p) => p.ext,
            _ => None,
        })
        .unwrap();
    assert_eq!(created["twr"]["account_dir"], json!(work));
    let link_s = link.to_str().unwrap();
    assert_eq!(
        get(&env.last_start(&w).cfg.env, "CLAUDE_CONFIG_DIR"),
        [link_s]
    );
    // Where it led: no move.
    let r = move_to(&hub, &w, "work", false, &user()).await.unwrap();
    assert_eq!(r["changed"], json!(false), "{r}");

    std::fs::remove_file(&link).unwrap();
    std::os::unix::fs::symlink(env.path("acc/work-moved"), &link).unwrap();
    let hub = env.restart(hub, config.clone()).await;
    turn(&hub, &w, "hi").await;
    let end = events(&hub, &w)
        .into_iter()
        .filter_map(|e| match e.body {
            EventBody::TurnEnded(p) => Some(p),
            _ => None,
        })
        .next_back()
        .unwrap();
    assert_eq!(end.result, TurnResult::Error);
    assert_eq!(
        end.ext.unwrap()["twr"]["error"],
        json!(format!(
            "claude-code:work's directory changed since w's session began ({} -> {link_s}, now \
             {}): twr model w --account work starts a new session there",
            work.display(),
            moved.display()
        ))
    );
    let r = move_to(&hub, &w, "work", false, &user()).await.unwrap();
    for (k, want) in [
        ("changed", json!(true)),
        ("account_changed", json!(true)),
        ("account", json!("work")),
        ("previous_account", json!("work")),
    ] {
        assert_eq!(r[k], want, "{k}: {r}");
    }
    let mv = moves(&events(&hub, &w)).pop().unwrap();
    assert_eq!(mv.ext.unwrap()["twr"]["account_dir"], json!(moved));
    turn(&hub, &w, "hi").await;
    let s = env.last_start(&w);
    assert!(s.cfg.resume_session.is_none());
    assert_eq!(get(&s.cfg.env, "CLAUDE_CONFIG_DIR"), [link_s]);
    hub.shutdown().await;
}

/// A config that names an account's directory by another path (a link to the same place)
/// changes nothing for a thread already on it: its runtimes keep the path recorded with the
/// account (a runtime may key what it keeps on it, as Claude Code's macOS Keychain does),
/// and `account.list` (a user's) names it in the account's `retained_paths`, since a login
/// signs the configured path in. Asking for the same account explicitly rebinds it: a new
/// session through the configured path, recorded with it; then it is no move. A model
/// change alone keeps the path. A new thread gets the new path.
#[tokio::test]
async fn another_path_to_the_same_account_directory_keeps_the_recorded_one() {
    let env = Env::new();
    let alias = env.path("acc/work-alias");
    std::os::unix::fs::symlink(env.path("acc/work"), &alias).unwrap();
    let plain = env.path("acc/work");
    let hub = env.open(env.config(Some(&plain))).await;
    let w = create(
        &hub,
        NewThread {
            account: Some("work".into()),
            ..new_thread("api", "w")
        },
    )
    .await;
    turn(&hub, &w, "hi").await;
    let plain_s = plain.to_str().unwrap();
    assert_eq!(
        get(&env.last_start(&w).cfg.env, "CLAUDE_CONFIG_DIR"),
        [plain_s]
    );
    // The config now names the same directory through the link.
    let hub = env.restart(hub, env.config(Some(&alias))).await;
    turn(&hub, &w, "hi").await;
    let s = env.last_start(&w);
    assert!(s.cfg.resume_session.is_some(), "the same session resumes");
    assert_eq!(get(&s.cfg.env, "CLAUDE_CONFIG_DIR"), [plain_s]);
    let retained = |hub: &Hub, with_dirs: bool| {
        let list = hub.account_list(Some("claude-code"), with_dirs).unwrap();
        list["runtimes"]["claude-code"]["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["name"] == "work")
            .unwrap()["retained_paths"]
            .clone()
    };
    assert_eq!(
        retained(&hub, true),
        json!([{"thread": "w", "path": plain_s}])
    );
    assert_eq!(retained(&hub, false), Value::Null, "paths are a user's");
    // A model change is no rebind.
    let r = hub
        .set_thread_model(
            &w,
            ModelRequest {
                model: Some("m2"),
                ..Default::default()
            },
            &user(),
        )
        .await
        .unwrap();
    assert_eq!(r["account_changed"], json!(false), "{r}");
    turn(&hub, &w, "hi").await;
    assert_eq!(
        get(&env.last_start(&w).cfg.env, "CLAUDE_CONFIG_DIR"),
        [plain_s]
    );
    let r = move_to(&hub, &w, "work", false, &user()).await.unwrap();
    for (k, want) in [
        ("changed", json!(true)),
        ("account_changed", json!(true)),
        ("account", json!("work")),
        ("previous_account", json!("work")),
    ] {
        assert_eq!(r[k], want, "{k}: {r}");
    }
    let alias_s = alias.to_str().unwrap();
    let mv = moves(&events(&hub, &w)).pop().unwrap();
    assert_eq!(mv.ext.unwrap()["twr"]["account_env_dir"], json!(alias_s));
    turn(&hub, &w, "hi").await;
    let s = env.last_start(&w);
    assert!(s.cfg.resume_session.is_none(), "a new session");
    assert_eq!(get(&s.cfg.env, "CLAUDE_CONFIG_DIR"), [alias_s]);
    assert_eq!(retained(&hub, true), Value::Null);
    let r = move_to(&hub, &w, "work", false, &user()).await.unwrap();
    assert_eq!(r["changed"], json!(false), "{r}");
    let n = create(
        &hub,
        NewThread {
            account: Some("work".into()),
            ..new_thread("api", "n")
        },
    )
    .await;
    turn(&hub, &n, "hi").await;
    assert_eq!(
        get(&env.last_start(&n).cfg.env, "CLAUDE_CONFIG_DIR"),
        [alias.to_str().unwrap()]
    );
    hub.shutdown().await;
}

/// A move records the path the account's runtimes get, as `thread.created` does: a config
/// that names the moved-to directory by another path afterwards changes nothing for the
/// thread (the same session resumes with the path it started with).
#[tokio::test]
async fn a_move_keeps_the_path_it_was_recorded_with() {
    let env = Env::new();
    let alias = env.path("acc/work-alias");
    std::os::unix::fs::symlink(env.path("acc/work"), &alias).unwrap();
    let plain = env.path("acc/work");
    let plain_s = plain.to_str().unwrap();
    let hub = env.open(env.config(Some(&plain))).await;
    let w = create(&hub, new_thread("api", "w")).await;
    turn(&hub, &w, "hi").await;
    assert!(get(&env.last_start(&w).cfg.env, "CLAUDE_CONFIG_DIR").is_empty());
    let r = move_to(&hub, &w, "work", false, &user()).await.unwrap();
    assert_eq!(r["account_changed"], json!(true), "{r}");
    let mv = moves(&events(&hub, &w)).pop().unwrap();
    assert_eq!(mv.ext.unwrap()["twr"]["account_env_dir"], json!(plain_s));
    turn(&hub, &w, "hi").await;
    assert_eq!(
        get(&env.last_start(&w).cfg.env, "CLAUDE_CONFIG_DIR"),
        [plain_s]
    );
    let hub = env.restart(hub, env.config(Some(&alias))).await;
    turn(&hub, &w, "hi").await;
    let s = env.last_start(&w);
    assert!(s.cfg.resume_session.is_some(), "the same session resumes");
    assert_eq!(get(&s.cfg.env, "CLAUDE_CONFIG_DIR"), [plain_s]);
    hub.shutdown().await;
}

/// A path recorded with the account that no longer leads to its directory (the link it was
/// given by removed, the config naming the directory through another link now) refuses the
/// thread's start, attach and job, rather than going on through the other path (a runtime
/// may key a login on the path: Claude Code's macOS Keychain entry). Asking for the same
/// account moves it: a new session through the configured path, recorded with it.
#[tokio::test]
async fn a_recorded_path_that_no_longer_leads_there_refuses_until_a_move() {
    let env = Env::new();
    let (a, b) = (env.path("acc/work-a"), env.path("acc/work-b"));
    for l in [&a, &b] {
        std::os::unix::fs::symlink(env.path("acc/work"), l).unwrap();
    }
    let work = env.path("acc/work").canonicalize().unwrap();
    let hub = env.open(env.config(Some(&a))).await;
    let w = create(
        &hub,
        NewThread {
            account: Some("work".into()),
            ..new_thread("api", "w")
        },
    )
    .await;
    turn(&hub, &w, "hi").await;
    let (a_s, b_s) = (a.to_str().unwrap(), b.to_str().unwrap());
    assert_eq!(get(&env.last_start(&w).cfg.env, "CLAUDE_CONFIG_DIR"), [a_s]);

    std::fs::remove_file(&a).unwrap();
    let hub = env.restart(hub, env.config(Some(&b))).await;
    let starts = env.starts(&w).len();
    turn(&hub, &w, "hi").await;
    let end = events(&hub, &w)
        .into_iter()
        .filter_map(|e| match e.body {
            EventBody::TurnEnded(p) => Some(p),
            _ => None,
        })
        .next_back()
        .unwrap();
    assert_eq!(end.result, TurnResult::Error);
    let refusal = format!(
        "claude-code:work's path {a_s} no longer leads to {}: twr model w --account work starts \
         a new session through the configured path",
        work.display()
    );
    assert_eq!(end.ext.unwrap()["twr"]["error"], json!(refusal));
    assert_eq!(env.starts(&w).len(), starts, "nothing started");
    assert_eq!(
        refused(hub.attach_begin(&w, false, "itest").await, codes::CONFLICT),
        refusal
    );
    let req: JobStart = serde_json::from_value(json!({"argv": ["true"]})).unwrap();
    assert_eq!(
        refused(
            hub.start_job(&w, req, Source::User("itest".into())),
            codes::CONFLICT
        ),
        refusal
    );

    let r = move_to(&hub, &w, "work", false, &user()).await.unwrap();
    for (k, want) in [
        ("changed", json!(true)),
        ("account_changed", json!(true)),
        ("account", json!("work")),
        ("previous_account", json!("work")),
    ] {
        assert_eq!(r[k], want, "{k}: {r}");
    }
    let mv = moves(&events(&hub, &w)).pop().unwrap();
    assert_eq!(mv.ext.unwrap()["twr"]["account_env_dir"], json!(b_s));
    turn(&hub, &w, "hi").await;
    let s = env.last_start(&w);
    assert!(s.cfg.resume_session.is_none(), "a new session");
    assert_eq!(get(&s.cfg.env, "CLAUDE_CONFIG_DIR"), [b_s]);
    // Where it leads now: no move.
    let r = move_to(&hub, &w, "work", false, &user()).await.unwrap();
    assert_eq!(r["changed"], json!(false), "{r}");
    hub.shutdown().await;
}

/// An account whose settings file is the native account's (a link to it) while that selects
/// credentials (`apiKeyHelper`, `env.ANTHROPIC_API_KEY`; Codex's model provider key) would
/// sign in as the native account: it is refused for new threads, moves and starts, saying
/// why, and `account.list` says it apart from a directory collision. An account's own file
/// that sets them is allowed.
#[tokio::test]
async fn settings_shared_with_the_native_account_that_pick_credentials_are_refused() {
    let env = Env::new();
    let native = env.path("native/claude-code");
    std::fs::create_dir_all(&native).unwrap();
    std::fs::create_dir_all(env.path("native/codex")).unwrap();
    std::fs::write(
        native.join("settings.json"),
        r#"{"apiKeyHelper": "/bin/key", "env": {"ANTHROPIC_API_KEY": "sk-native"}}"#,
    )
    .unwrap();
    std::fs::write(
        env.path("native/codex/config.toml"),
        "model_provider = \"corp\"\n[model_providers.corp]\nenv_key = \"CORP_KEY\"\n",
    )
    .unwrap();
    // work2's own file with a helper of its own: allowed.
    std::fs::write(
        env.path("acc/work2/settings.json"),
        r#"{"apiKeyHelper": "/bin/work2-key"}"#,
    )
    .unwrap();
    let hub = env.open(env.config(None)).await;
    // On work before its file leads to the native one.
    let w = create(
        &hub,
        NewThread {
            account: Some("work".into()),
            ..new_thread("api", "w")
        },
    )
    .await;
    turn(&hub, &w, "hi").await;
    std::os::unix::fs::symlink(
        native.join("settings.json"),
        env.path("acc/work/settings.json"),
    )
    .unwrap();
    std::os::unix::fs::symlink(
        env.path("native/codex/config.toml"),
        env.path("acc/team/config.toml"),
    )
    .unwrap();
    let why = format!(
        "its settings.json is the native account's ({}), which sets apiKeyHelper, \
         env.ANTHROPIC_API_KEY, so it would sign in as the native account: replace {} with a \
         copy without those keys",
        native.join("settings.json").display(),
        env.path("acc/work/settings.json").display()
    );
    let refusal = format!("claude-code:work can't be used: {why}");
    let e = hub
        .new_thread(
            NewThread {
                account: Some("work".into()),
                ..new_thread("api", "n")
            },
            "itest",
        )
        .await;
    assert_eq!(refused(e, codes::CONFLICT), refusal);
    let o = create(&hub, new_thread("api", "o")).await;
    assert_eq!(
        refused(
            move_to(&hub, &o, "work", false, &user()).await,
            codes::CONFLICT
        ),
        refusal
    );
    // Its thread's starts, attaches and jobs.
    let starts = env.starts(&w).len();
    assert_eq!(
        refused(hub.attach_begin(&w, false, "itest").await, codes::CONFLICT),
        format!("w runs on claude-code:work, which can't be used: {why}")
    );
    let req: JobStart = serde_json::from_value(json!({"argv": ["true"]})).unwrap();
    assert_eq!(
        refused(
            hub.start_job(&w, req, Source::User("itest".into())),
            codes::CONFLICT
        ),
        format!("w runs on claude-code:work, which can't be used: {why}")
    );
    assert_eq!(env.starts(&w).len(), starts, "nothing started");
    // Codex: a model provider's key in the shared config.toml.
    let e = hub
        .new_thread(
            NewThread {
                runtime: Some("codex:team".into()),
                ..new_thread("api", "c")
            },
            "itest",
        )
        .await;
    let msg = refused(e, codes::CONFLICT);
    assert!(
        msg.starts_with("codex:team can't be used: its config.toml is the native account's (")
            && msg.contains("which sets model_providers.corp.env_key"),
        "{msg}"
    );
    // Listed apart from a directory collision.
    let list = hub.account_list(Some("claude-code"), true).unwrap();
    let work = list["runtimes"]["claude-code"]["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "work")
        .unwrap()
        .clone();
    assert_eq!(work["shared_credentials"], json!(why), "{work}");
    assert!(work.get("conflict").is_none(), "{work}");
    // Without directories (the orchestrator's view): not said there.
    let list = hub.account_list(Some("claude-code"), false).unwrap();
    assert!(!list.to_string().contains("shared_credentials"), "{list}");
    // A native settings file twr can't read safely (a FIFO no one writes, which a blocking
    // read would wait on forever under the core lock; one over the cap) is not read: listed
    // and refused at once, as one twr can't check.
    let settings = native.join("settings.json");
    let kept = std::fs::read(&settings).unwrap();
    let fifo = |p: &std::path::Path| {
        let c = std::ffi::CString::new(p.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    };
    let big = |p: &std::path::Path| {
        std::fs::File::create(p)
            .unwrap()
            .set_len(twr_core::fsread::SETTINGS_MAX + 1)
            .unwrap();
    };
    for (make, what) in [
        (
            &fifo as &dyn Fn(&std::path::Path),
            "it is a FIFO, not a regular file",
        ),
        (&big, "it is larger than 1024 KiB"),
    ] {
        std::fs::remove_file(&settings).unwrap();
        make(&settings);
        let why = format!(
            "its settings.json is the native account's ({}), which twr can't check: it can't \
             be read ({what}), so it may sign in as the native account: replace {} with a copy \
             of its own",
            settings.display(),
            env.path("acc/work/settings.json").display()
        );
        // (On a thread of its own, so a read that blocks fails the test instead of hanging
        // it.)
        let (tx, rx) = std::sync::mpsc::channel();
        let h = hub.clone();
        std::thread::spawn(move || {
            let _ = tx.send(h.account_list(Some("claude-code"), true));
        });
        let list = rx
            .recv_timeout(WAIT)
            .expect("account.list does not wait on the settings file")
            .unwrap();
        let work = list["runtimes"]["claude-code"]["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["name"] == "work")
            .unwrap()
            .clone();
        assert_eq!(work["shared_credentials"], json!(why), "{work}");
        let e = hub
            .new_thread(
                NewThread {
                    account: Some("work".into()),
                    ..new_thread("api", "n")
                },
                "itest",
            )
            .await;
        assert_eq!(
            refused(e, codes::CONFLICT),
            format!("claude-code:work can't be used: {why}")
        );
    }
    std::fs::remove_file(&settings).unwrap();
    std::fs::write(&settings, kept).unwrap();

    // An account's own settings that set a helper: allowed.
    let r = move_to(&hub, &o, "work2", false, &user()).await.unwrap();
    assert_eq!(r["account"], json!("work2"), "{r}");
    turn(&hub, &o, "hi").await;
    assert_eq!(
        get(&env.last_start(&o).cfg.env, "CLAUDE_CONFIG_DIR"),
        [env.dir("acc/work2").as_str()]
    );
    // A copy of its own without the keys: usable again.
    std::fs::remove_file(env.path("acc/work/settings.json")).unwrap();
    std::fs::write(env.path("acc/work/settings.json"), "{}").unwrap();
    let n = create(
        &hub,
        NewThread {
            account: Some("work".into()),
            ..new_thread("api", "n")
        },
    )
    .await;
    assert_eq!(view(&hub, &n)["account"], json!("work"));
    let req: JobStart = serde_json::from_value(json!({"argv": ["true"]})).unwrap();
    hub.start_job(&w, req, Source::User("itest".into()))
        .unwrap();
    hub.shutdown().await;
}

/// `account.remove {move_to}` moves every thread on the account, then removes it, in one
/// commit; a thread that cannot move (its own pin) refuses the whole removal, listed, before
/// anything is recorded. The moves and the removal survive a restart.
#[tokio::test]
async fn removing_an_account_moves_its_threads_or_none() {
    let env = Env::new();
    let hub = env.open(env.config(None)).await;
    let added = hub
        .account_add(
            AccountAddParams {
                runtime: "claude-code".into(),
                account: "side".into(),
                ..Default::default()
            },
            &user(),
        )
        .unwrap();
    // (Made as `twr account add` makes it: a thread is not made on a missing directory.)
    std::fs::create_dir_all(added["dir"].as_str().unwrap()).unwrap();
    let s1 = create(&hub, new_thread("api", "s1")).await;
    let s2 = create(&hub, new_thread("api", "s2")).await;
    for t in [&s1, &s2] {
        move_to(&hub, t, "side", false, &user()).await.unwrap();
    }
    let s3 = create(
        &hub,
        NewThread {
            account: Some("side".into()),
            ..new_thread("api", "s3")
        },
    )
    .await;
    let remove = |to: Option<&str>| AccountRemoveParams {
        runtime: "claude-code".into(),
        account: "side".into(),
        move_to: to.map(str::to_string),
        ..Default::default()
    };
    let control_kinds = |hub: &Hub| {
        let (items, _) = hub.control_read(0, 1000).unwrap();
        items
            .iter()
            .filter(|(_, e)| matches!(e.body, EventBody::AccountRemoved(_)))
            .count()
    };
    let before: Vec<usize> = [&s1, &s2, &s3]
        .iter()
        .map(|t| moves(&events(&hub, t)).len())
        .collect();
    assert_eq!(
        refused(
            hub.account_remove(remove(Some("work")), &user()).await,
            codes::CONFLICT
        ),
        "cannot move every thread on claude-code:side to claude-code:work, so nothing was \
         removed: s3 (s3 is pinned to account side: pass outside_pin (twr model s3 \
         --outside-pin) to move it to claude-code:work anyway)"
    );
    let after: Vec<usize> = [&s1, &s2, &s3]
        .iter()
        .map(|t| moves(&events(&hub, t)).len())
        .collect();
    assert_eq!(before, after, "nothing recorded");
    assert_eq!(control_kinds(&hub), 0);
    assert_eq!(view(&hub, &s1)["account"], json!("side"));
    // Without move_to (or force): refused, as before.
    refused(
        hub.account_remove(remove(None), &user()).await,
        codes::CONFLICT,
    );
    // move_to must be another account of the runtime.
    for bad in ["side", "nope"] {
        refused(
            hub.account_remove(remove(Some(bad)), &user()).await,
            codes::BAD_REQUEST,
        );
    }

    move_to(&hub, &s3, "work", true, &user()).await.unwrap();
    let r = hub
        .account_remove(remove(Some("work")), &user())
        .await
        .unwrap();
    assert_eq!(r["moved"], json!(["s1", "s2"]), "{r}");
    assert_eq!(r["threads"], json!([]));
    assert_eq!(
        r["message"],
        json!(
            "removed claude-code:side; s1, s2 moved to claude-code:work: their next turns start \
             new sessions there with a handoff"
        )
    );
    assert_eq!(control_kinds(&hub), 1);
    for t in [&s1, &s2] {
        assert_eq!(view(&hub, t)["account"], json!("work"));
        let mv = moves(&events(&hub, t)).pop().unwrap();
        assert_eq!(
            (mv.account.as_deref(), mv.previous_account.as_deref()),
            (Some("work"), Some("side"))
        );
    }
    let hub = env.restart(hub, env.config(None)).await;
    for t in [&s1, &s2] {
        assert_eq!(view(&hub, t)["account"], json!("work"));
    }
    assert_eq!(control_kinds(&hub), 1);
    turn(&hub, &s1, "hi").await;
    let s = env.last_start(&s1);
    assert_eq!(
        get(&s.cfg.env, "CLAUDE_CONFIG_DIR"),
        [env.dir("acc/work").as_str()]
    );
    hub.shutdown().await;
}

/// A destination whose directory was deleted since the last login check is refused as a new
/// thread's would be, before anything is recorded: `thread.set_model {account}` records no
/// move, and `account.remove {move_to}` neither moves its threads nor removes the account.
#[tokio::test]
async fn a_destination_whose_directory_is_gone_is_refused_before_anything_is_recorded() {
    let env = Env::new();
    let hub = env.open(env.config(None)).await;
    let added = hub
        .account_add(
            AccountAddParams {
                runtime: "claude-code".into(),
                account: "side".into(),
                ..Default::default()
            },
            &user(),
        )
        .unwrap();
    std::fs::create_dir_all(added["dir"].as_str().unwrap()).unwrap();
    let s = create(&hub, new_thread("api", "s")).await;
    move_to(&hub, &s, "side", false, &user()).await.unwrap();
    turn(&hub, &s, "hi").await;
    let moved = moves(&events(&hub, &s)).len();
    let work2 = env.path("acc/work2");
    std::fs::remove_dir_all(&work2).unwrap();
    let why = format!(
        "claude-code:work2's directory {} is missing: twr account add claude-code work2 makes \
         it again and logs it in",
        work2.display()
    );
    assert_eq!(
        refused(
            move_to(&hub, &s, "work2", false, &user()).await,
            codes::CONFLICT
        ),
        why
    );
    // (Also as the runtime part of a spec.)
    assert_eq!(
        refused(
            hub.set_thread_model(
                &s,
                ModelRequest {
                    runtime: Some("claude-code:work2"),
                    ..Default::default()
                },
                &user(),
            )
            .await,
            codes::CONFLICT
        ),
        why
    );
    assert_eq!(moves(&events(&hub, &s)).len(), moved, "no move recorded");
    let removal = hub
        .account_remove(
            AccountRemoveParams {
                runtime: "claude-code".into(),
                account: "side".into(),
                move_to: Some("work2".into()),
                ..Default::default()
            },
            &user(),
        )
        .await;
    assert_eq!(
        refused(removal, codes::CONFLICT),
        format!(
            "cannot move every thread on claude-code:side to claude-code:work2, so nothing was \
             removed: s ({why})"
        )
    );
    assert_eq!(moves(&events(&hub, &s)).len(), moved, "no move recorded");
    let (items, _) = hub.control_read(0, 1000).unwrap();
    assert!(
        !items
            .iter()
            .any(|(_, e)| matches!(e.body, EventBody::AccountRemoved(_))),
        "nothing removed"
    );
    assert_eq!(view(&hub, &s)["account"], json!("side"));
    // Still working on its account.
    turn(&hub, &s, "still").await;
    // Back: the move goes ahead.
    std::fs::create_dir_all(&work2).unwrap();
    move_to(&hub, &s, "work2", false, &user()).await.unwrap();
    assert_eq!(view(&hub, &s)["account"], json!("work2"));
    hub.shutdown().await;
}

/// `account.remove` names the threads whose runtime process still runs on the login it
/// removed (`running`), and each one's thread object says so (`runtime_account_removed`)
/// until it no longer does: a busy thread moved with `move_to` until its held turn ends and
/// its runtime stops, an idle one until its runtime stopped, one left on it with `force`
/// until it is stopped.
#[tokio::test]
async fn removing_an_account_names_the_runtimes_still_on_it() {
    let env = Env::new();
    let hub = env.open(env.config(None)).await;
    let add_in = |name: &str, dir: Option<PathBuf>| {
        let added = hub
            .account_add(
                AccountAddParams {
                    runtime: "claude-code".into(),
                    account: name.into(),
                    dir,
                    ..Default::default()
                },
                &user(),
            )
            .unwrap();
        let dir = PathBuf::from(added["dir"].as_str().unwrap());
        std::fs::create_dir_all(&dir).unwrap();
        // (As the runtime was started in it: links followed.)
        std::fs::canonicalize(dir).unwrap()
    };
    let add = |name: &str| add_in(name, None);
    let side_dir = add("side");
    // (Moved there rather than made there: a thread made on it is pinned to it.)
    async fn on(hub: &Hub, name: &str, account: &str) -> String {
        let t = create(hub, new_thread("api", name)).await;
        move_to(hub, &t, account, false, &user()).await.unwrap();
        t
    }
    let busy = on(&hub, "busy", "side").await;
    let idle = on(&hub, "idle", "side").await;
    for t in [&busy, &idle] {
        turn(&hub, t, "hi").await;
    }
    hub.send(&busy, "HOLD", TurnFrom::User, Source::User("itest".into()))
        .unwrap();
    until(&hub, &busy, |evs| {
        evs.iter()
            .filter(|e| matches!(e.body, EventBody::TurnStarted(_)))
            .count()
            == 2
    })
    .await;
    let still = |t: &str| view(&hub, t).get("runtime_account_removed").cloned();
    assert_eq!(still(&busy), None, "not removed yet");
    let remove = |account: &str, move_to: Option<&str>, force: bool| AccountRemoveParams {
        runtime: "claude-code".into(),
        account: account.into(),
        move_to: move_to.map(str::to_string),
        force,
    };
    let r = hub
        .account_remove(remove("side", Some("work"), false), &user())
        .await
        .unwrap();
    assert_eq!(r["moved"], json!(["busy", "idle"]), "{r}");
    assert_eq!(
        r["running"],
        json!([
            {"thread": busy, "thread_name": "busy"},
            {"thread": idle, "thread_name": "idle"},
        ]),
        "{r}"
    );
    let side = json!({"runtime": "claude-code", "account": "side", "dir": side_dir});
    assert_eq!(still(&busy), Some(side.clone()));
    // The idle one's runtime stops now; the busy one's after its turn.
    stopped_for(&hub, &idle, 1).await;
    assert_eq!(still(&idle), None);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(still(&busy), Some(side));
    env.shared.hold.notify_one();
    assert_eq!(stopped_for(&hub, &busy, 1).await, ["account_change"]);
    assert_eq!(still(&busy), None);

    // Left on it with force: its idle runtime runs on until it is stopped.
    let other_dir = add("other");
    let left = on(&hub, "left", "other").await;
    turn(&hub, &left, "hi").await;
    let r = hub
        .account_remove(remove("other", None, true), &user())
        .await
        .unwrap();
    assert_eq!(r["threads"], json!(["left"]), "{r}");
    assert_eq!(
        r["running"],
        json!([{"thread": left, "thread_name": "left"}]),
        "{r}"
    );
    let on_other = json!({"runtime": "claude-code", "account": "other", "dir": other_dir});
    assert_eq!(still(&left), Some(on_other.clone()));
    // Added again elsewhere while that runtime lives: it still says the directory it runs
    // in, and an account later made in that directory is waited for by it when removed.
    add_in("other", Some(env.path("acc/work-moved")));
    assert_eq!(still(&left), Some(on_other));
    add_in("third", Some(other_dir.clone()));
    let r = hub
        .account_remove(remove("third", None, false), &user())
        .await
        .unwrap();
    assert_eq!(
        r["running"],
        json!([{"thread": left, "thread_name": "left"}]),
        "{r}"
    );
    // So is another runtime's account made in that directory: the Claude Code process is
    // what reads and writes it, whatever runtime the removed account is of.
    hub.account_add(
        AccountAddParams {
            runtime: "codex".into(),
            account: "cx".into(),
            dir: Some(other_dir.clone()),
            ..Default::default()
        },
        &user(),
    )
    .unwrap();
    let r = hub
        .account_remove(
            AccountRemoveParams {
                runtime: "codex".into(),
                ..remove("cx", None, false)
            },
            &user(),
        )
        .await
        .unwrap();
    assert_eq!(
        r["running"],
        json!([{"thread": left, "thread_name": "left"}]),
        "{r}"
    );
    // Not the new directory's removal: nothing runs there.
    let r = hub
        .account_remove(remove("other", None, true), &user())
        .await
        .unwrap();
    assert_eq!(r["running"], json!([]), "{r}");
    hub.stop_thread(&left, "itest").unwrap();
    until(&hub, &left, |evs| {
        evs.iter()
            .any(|e| matches!(e.body, EventBody::ThreadStopped(_)))
    })
    .await;
    assert_eq!(still(&left), None);
    // Nothing runs on a removed login: nothing is named.
    add("idle-free");
    let r = hub
        .account_remove(remove("idle-free", None, false), &user())
        .await
        .unwrap();
    assert_eq!(r["running"], json!([]), "{r}");
    hub.shutdown().await;
}

/// A removed native account keeps its name, so it does not take that of a named account
/// called `default`: the native account named `personal` (`mine` on codex), a named account
/// `default`, then `personal` removed with `move_to: default` (its threads move there and run
/// on it) and `mine` with `force` (its thread stays on `codex:mine`, removed, and cannot
/// start; the thread on `codex:default` runs). A restart keeps it all.
#[tokio::test]
async fn a_removed_native_account_keeps_its_name() {
    let env = Env::new();
    let hub = env.open(env.config(None)).await;
    for (rt, native, dir) in [
        ("claude-code", "personal", "acc/cdef"),
        ("codex", "mine", "acc/xdef"),
    ] {
        std::fs::create_dir_all(env.path(dir)).unwrap();
        hub.account_add(
            AccountAddParams {
                runtime: rt.into(),
                account: native.into(),
                native: true,
                ..Default::default()
            },
            &user(),
        )
        .unwrap();
        hub.account_add(
            AccountAddParams {
                runtime: rt.into(),
                account: "default".into(),
                dir: Some(env.path(dir)),
                ..Default::default()
            },
            &user(),
        )
        .unwrap();
    }
    let a = create(&hub, new_thread("api", "a")).await;
    let b = create(
        &hub,
        NewThread {
            account: Some("default".into()),
            ..new_thread("api", "b")
        },
    )
    .await;
    let codex = |name: &str, account: Option<&str>| NewThread {
        runtime: Some("codex".into()),
        account: account.map(str::to_string),
        ..new_thread("api", name)
    };
    let c = create(&hub, codex("c", None)).await;
    let d = create(&hub, codex("d", Some("default"))).await;
    assert_eq!(view(&hub, &a)["account"], json!("personal"));
    assert_eq!(view(&hub, &c)["account"], json!("mine"));

    let remove = |rt: &str, name: &str, to: Option<&str>| AccountRemoveParams {
        runtime: rt.into(),
        account: name.into(),
        move_to: to.map(str::to_string),
        force: to.is_none(),
    };
    let r = hub
        .account_remove(remove("claude-code", "personal", Some("default")), &user())
        .await
        .unwrap();
    assert_eq!(r["moved"], json!(["a"]), "{r}");
    let r = hub
        .account_remove(remove("codex", "mine", None), &user())
        .await
        .unwrap();
    assert_eq!(r["threads"], json!(["c"]), "{r}");

    let check = |hub: &Hub| {
        for t in [&a, &b, &d] {
            let v = view(hub, t);
            assert_eq!(v["account"], json!("default"), "{v}");
            assert!(v.get("account_removed").is_none(), "{v}");
        }
        let v = view(hub, &c);
        assert_eq!(
            (&v["account"], &v["account_removed"]),
            (&json!("mine"), &json!(true)),
            "{v}"
        );
        let list = hub.account_list(None, false).unwrap();
        for (rt, native) in [("claude-code", "personal"), ("codex", "mine")] {
            let rt = &list["runtimes"][rt];
            assert_eq!(rt["native"], json!(native), "{rt}");
            let got: Vec<(Value, Value, Value)> = rt["accounts"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| (a["name"].clone(), a["native"].clone(), a["removed"].clone()))
                .collect();
            // (Then config's `work`, `work2`, `team`.)
            assert_eq!(
                got[..2],
                [
                    (json!(native), json!(true), json!(true)),
                    (json!("default"), json!(false), json!(false)),
                ],
                "{rt}"
            );
        }
        let accounts = &list["runtimes"]["codex"]["accounts"];
        assert_eq!(
            (&accounts[0]["threads"], &accounts[1]["threads"]),
            (&json!(1), &json!(1)),
            "{accounts}"
        );
        // Named `personal` is the removed native account's name still: `name-native` adds it
        // back.
        let why = refused(
            hub.account_add(
                AccountAddParams {
                    runtime: "claude-code".into(),
                    account: "personal".into(),
                    dir: Some(env.path("acc/work-moved")),
                    ..Default::default()
                },
                &user(),
            ),
            codes::CONFLICT,
        );
        assert!(
            why.contains("removed: twr account name-native claude-code personal adds it back"),
            "{why}"
        );
    };
    check(&hub);
    for (t, dir) in [(&a, "acc/cdef"), (&b, "acc/cdef")] {
        turn(&hub, t, "hi").await;
        let s = env.last_start(t);
        assert_eq!(
            get(&s.cfg.env, "CLAUDE_CONFIG_DIR"),
            [env.dir(dir).as_str()]
        );
    }
    turn(&hub, &d, "hi").await;
    assert_eq!(
        get(&env.last_start(&d).cfg.env, "CODEX_HOME"),
        [env.dir("acc/xdef").as_str()]
    );

    let hub = env.restart(hub, env.config(None)).await;
    check(&hub);
    for t in [&a, &b, &d] {
        let before = env.starts(t).len();
        turn(&hub, t, "again").await;
        assert_eq!(env.starts(t).len(), before + 1, "{t} starts again");
    }
    let before = env.starts(&c).len();
    turn(&hub, &c, "hi").await;
    assert_eq!(env.starts(&c).len(), before, "c cannot start");
    let end = events(&hub, &c)
        .into_iter()
        .filter_map(|e| match e.body {
            EventBody::TurnEnded(p) => Some(p),
            _ => None,
        })
        .next_back()
        .unwrap();
    assert_eq!(end.result, TurnResult::Error);
    assert_eq!(
        end.ext.as_ref().unwrap()["twr"]["error"],
        json!("c runs on codex:mine, which was removed: move it (twr model c --account <name>)")
    );
    hub.shutdown().await;
}

/// A recorded native `personal` and a recorded named `default`, then config declares
/// `personal` named (a restart): the native account falls back to `default`, next to the
/// named one. `default` is the named account: a new thread asking for it gets it, and a thread
/// on the native account asking for it moves to it (not "already on it").
#[tokio::test]
async fn a_name_the_native_account_shares_is_the_named_accounts() {
    let env = Env::new();
    for d in ["acc/xdef", "acc/personal"] {
        std::fs::create_dir_all(env.path(d)).unwrap();
    }
    let hub = env.open(env.config(None)).await;
    for (name, dir, native) in [
        ("personal", None, true),
        ("default", Some("acc/xdef"), false),
    ] {
        hub.account_add(
            AccountAddParams {
                runtime: "codex".into(),
                account: name.into(),
                dir: dir.map(|d| env.path(d)),
                native,
                ..Default::default()
            },
            &user(),
        )
        .unwrap();
    }
    let codex = |name: &str, account: Option<&str>| NewThread {
        runtime: Some("codex".into()),
        account: account.map(str::to_string),
        ..new_thread("api", name)
    };
    let a = create(&hub, codex("a", None)).await;
    assert_eq!(view(&hub, &a)["account"], json!("personal"));
    let mut config = env.config(None);
    config.accounts.entry("codex".into()).or_default().insert(
        "personal".into(),
        AccountConfig {
            dir: Some(env.path("acc/personal")),
            ..Default::default()
        },
    );
    let hub = env.restart(hub, config).await;
    let list = hub.account_list(Some("codex"), false).unwrap();
    assert_eq!(list["runtimes"]["codex"]["native"], json!("default"));
    // A new thread asking for `default` gets the named account.
    let b = create(&hub, codex("b", Some("default"))).await;
    turn(&hub, &b, "hi").await;
    assert_eq!(
        get(&env.last_start(&b).cfg.env, "CODEX_HOME"),
        [env.dir("acc/xdef").as_str()]
    );
    // The thread on the native account moves to it.
    turn(&hub, &a, "hi").await;
    assert!(get(&env.last_start(&a).cfg.env, "CODEX_HOME").is_empty());
    let r = move_to(&hub, &a, "default", false, &user()).await.unwrap();
    assert_eq!(r["account"], json!("default"), "{r}");
    let m = moves(&events(&hub, &a));
    assert_eq!(m.len(), 1, "{m:?}");
    turn(&hub, &a, "again").await;
    assert_eq!(
        get(&env.last_start(&a).cfg.env, "CODEX_HOME"),
        [env.dir("acc/xdef").as_str()]
    );
    hub.shutdown().await;
}

/// `thread.set_model {runtime}` by the user, past the thread's pin (`outside_pin`).
async fn move_runtime(hub: &Hub, thread: &str, runtime: &str) -> Value {
    hub.set_thread_model(
        thread,
        ModelRequest {
            runtime: Some(runtime),
            outside_pin: true,
            ..Default::default()
        },
        &user(),
    )
    .await
    .unwrap()
}

/// Switch-back resume keyed by account: a move back to the account a thread left (on the same
/// runtime) resumes the session it left there, in that account's environment; a move to another
/// account of the runtime, where the thread never had a session, starts a new one, never
/// another account's session.
#[tokio::test]
async fn a_move_back_resumes_the_session_of_that_account_only() {
    let env = Env::new();
    let hub = env.open(env.config(None)).await;
    let t = create(
        &hub,
        NewThread {
            account: Some("work".into()),
            ..new_thread("api", "t")
        },
    )
    .await;
    turn(&hub, &t, "hi").await;
    let on_work = session(&hub, &t);

    // To work2: a new session there.
    let r = move_to(&hub, &t, "work2", true, &user()).await.unwrap();
    assert_eq!(r["resumes"], Value::Null, "{r}");
    turn(&hub, &t, "hi").await;
    let s = env.last_start(&t);
    assert!(s.cfg.resume_session.is_none(), "another account's session");
    let on_work2 = session(&hub, &t);
    assert_ne!(on_work2, on_work);

    // Back to work: its session there, in work's environment.
    let r = move_to(&hub, &t, "work", false, &user()).await.unwrap();
    assert_eq!(r["resumes"], json!(&on_work), "{r}");
    assert!(
        r["message"]
            .as_str()
            .unwrap()
            .starts_with("t moves from claude-code:work2 back to claude-code:work: its earlier"),
        "{r}"
    );
    turn(&hub, &t, "hi").await;
    let s = env.last_start(&t);
    assert_eq!(s.cfg.resume_session.as_deref(), Some(on_work.as_str()));
    assert_eq!(
        get(&s.cfg.env, "CLAUDE_CONFIG_DIR"),
        [env.dir("acc/work").as_str()]
    );
    let handoff = last_handoff(&hub, &t).expect("the resume's handoff");
    assert!(
        handoff.contains("it ran on claude-code:work2 for 1 turn"),
        "{handoff}"
    );

    // And to work2 again: its own session there.
    let r = move_to(&hub, &t, "work2", true, &user()).await.unwrap();
    assert_eq!(r["resumes"], json!(&on_work2), "{r}");
    turn(&hub, &t, "hi").await;
    assert_eq!(
        env.last_start(&t).cfg.resume_session.as_deref(),
        Some(on_work2.as_str())
    );

    // The native account, where the thread never ran: a new session with the full handoff.
    let r = move_to(&hub, &t, "default", true, &user()).await.unwrap();
    assert_eq!(r["resumes"], Value::Null, "{r}");
    assert!(
        r["message"]
            .as_str()
            .unwrap()
            .contains("its next turn starts a new session there with a handoff"),
        "{r}"
    );
    turn(&hub, &t, "hi").await;
    let s = env.last_start(&t);
    assert!(s.cfg.resume_session.is_none());
    assert!(s
        .prompt
        .unwrap()
        .starts_with("[twr handoff] This thread (t) moved to another account:"));
    hub.shutdown().await;
}

/// A move to another runtime and back resumes the session it left only on the account it ran
/// on: back on the runtime with another account, a new session; on the account it left, its
/// session there.
#[tokio::test]
async fn a_runtime_move_back_resumes_only_on_the_same_account() {
    let env = Env::new();
    let hub = env.open(env.config(None)).await;
    let t = create(
        &hub,
        NewThread {
            account: Some("work".into()),
            ..new_thread("api", "t")
        },
    )
    .await;
    turn(&hub, &t, "hi").await;
    let on_work = session(&hub, &t);
    let r = move_runtime(&hub, &t, "codex:team").await;
    assert_eq!(r["resumes"], Value::Null, "{r}");
    turn(&hub, &t, "hi").await;
    let on_team = session(&hub, &t);

    // Claude Code, on another account: a new session.
    let r = move_runtime(&hub, &t, "claude-code:work2").await;
    assert_eq!(r["resumes"], Value::Null, "{r}");
    turn(&hub, &t, "hi").await;
    let s = env.last_start(&t);
    assert_eq!(s.runtime, "claude-code");
    assert!(s.cfg.resume_session.is_none());

    // Codex on team again: its session there.
    let r = move_runtime(&hub, &t, "codex:team").await;
    assert_eq!(r["resumes"], json!(&on_team), "{r}");
    turn(&hub, &t, "hi").await;
    let s = env.last_start(&t);
    assert_eq!(
        (s.runtime, s.cfg.resume_session.as_deref()),
        ("codex", Some(on_team.as_str()))
    );
    assert_eq!(
        get(&s.cfg.env, "CODEX_HOME"),
        [env.dir("acc/team").as_str()]
    );

    // Claude Code on work, the account it left first: that session.
    let r = move_runtime(&hub, &t, "claude-code:work").await;
    assert_eq!(r["resumes"], json!(&on_work), "{r}");
    turn(&hub, &t, "hi").await;
    let s = env.last_start(&t);
    assert_eq!(
        (s.runtime, s.cfg.resume_session.as_deref()),
        ("claude-code", Some(on_work.as_str()))
    );
    assert_eq!(
        get(&s.cfg.env, "CLAUDE_CONFIG_DIR"),
        [env.dir("acc/work").as_str()]
    );
    hub.shutdown().await;
}

/// The session a move left lives in the directory it began in: once the account's name leads
/// to another directory (config changed), or the account was removed and added again (in the
/// same directory too: another login there), a move back starts a new session with the full
/// handoff.
#[tokio::test]
async fn a_move_back_after_a_directory_change_or_a_re_add_starts_a_new_session() {
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
    turn(&hub, &w, "hi").await;
    move_to(&hub, &w, "work2", true, &user()).await.unwrap();
    turn(&hub, &w, "hi").await;
    let moved = env.path("acc/work-moved");
    let hub = env.restart(hub, env.config(Some(&moved))).await;
    let r = move_to(&hub, &w, "work", false, &user()).await.unwrap();
    assert_eq!(r["resumes"], Value::Null, "{r}");
    turn(&hub, &w, "hi").await;
    let s = env.last_start(&w);
    assert!(
        s.cfg.resume_session.is_none(),
        "a new session in the new directory"
    );
    assert_eq!(
        get(&s.cfg.env, "CLAUDE_CONFIG_DIR"),
        [env.dir("acc/work-moved").as_str()]
    );
    assert!(s
        .prompt
        .unwrap()
        .starts_with("[twr handoff] This thread (w) moved to another account:"));

    // A recorded account removed and added again, in the same directory.
    let add = |hub: &Hub| {
        let added = hub
            .account_add(
                AccountAddParams {
                    runtime: "claude-code".into(),
                    account: "side".into(),
                    ..Default::default()
                },
                &user(),
            )
            .unwrap();
        let dir = added["dir"].as_str().unwrap().to_string();
        std::fs::create_dir_all(&dir).unwrap();
        dir
    };
    let side_dir = add(&hub);
    let s = create(
        &hub,
        NewThread {
            account: Some("side".into()),
            ..new_thread("api", "s")
        },
    )
    .await;
    turn(&hub, &s, "hi").await;
    move_to(&hub, &s, "work2", true, &user()).await.unwrap();
    turn(&hub, &s, "hi").await;
    hub.account_remove(
        AccountRemoveParams {
            runtime: "claude-code".into(),
            account: "side".into(),
            ..Default::default()
        },
        &user(),
    )
    .await
    .unwrap();
    assert_eq!(add(&hub), side_dir);
    let r = move_to(&hub, &s, "side", false, &user()).await.unwrap();
    assert_eq!(r["resumes"], Value::Null, "{r}");
    turn(&hub, &s, "hi").await;
    let st = env.last_start(&s);
    assert!(
        st.cfg.resume_session.is_none(),
        "another login in that directory"
    );
    assert_eq!(get(&st.cfg.env, "CLAUDE_CONFIG_DIR"), [side_dir.as_str()]);
    hub.shutdown().await;
}

/// The native account is the same account under another name: a thread that left it resumes
/// its session there after `twr account name-native` (an `account.added` of the native
/// account's name), moved back by the new name.
#[tokio::test]
async fn a_renamed_native_account_still_resumes_its_session() {
    let env = Env::new();
    let hub = env.open(env.config(None)).await;
    let a = create(&hub, new_thread("api", "a")).await;
    turn(&hub, &a, "hi").await;
    let native = session(&hub, &a);
    move_to(&hub, &a, "work", false, &user()).await.unwrap();
    turn(&hub, &a, "hi").await;
    hub.account_add(
        AccountAddParams {
            runtime: "claude-code".into(),
            account: "personal".into(),
            native: true,
            ..Default::default()
        },
        &user(),
    )
    .unwrap();
    let r = move_to(&hub, &a, "personal", false, &user()).await.unwrap();
    assert_eq!(r["account"], json!("personal"), "{r}");
    assert_eq!(r["resumes"], json!(&native), "{r}");
    turn(&hub, &a, "hi").await;
    let s = env.last_start(&a);
    assert_eq!(s.cfg.resume_session.as_deref(), Some(native.as_str()));
    assert!(get(&s.cfg.env, "CLAUDE_CONFIG_DIR").is_empty());
    hub.shutdown().await;
}
