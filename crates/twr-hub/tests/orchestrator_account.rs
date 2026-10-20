//! The orchestrator on an account (`hub::orch`): the first one's from `[orchestrator]
//! account`, a switch's from `orchestrator.use <runtime>:<account>` (a switch with a handoff,
//! the same runtime too), a reset keeping it, refusals as for a new thread; the account's
//! environment on its spawn and attach; Claude Code's managed policy read from the account's
//! directory; and the native account's memory directory shared through `autoMemoryDirectory`
//! and the guard's `--memory-dir` for a named account that shares its configuration.
//!
//! `claude-code` and `codex` are played by a test adapter that hands every session to the test
//! with the `SpawnConfig` the hub built ([`OrchRt`]).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Local, SecondsFormat, TimeDelta, Utc};
use futures::FutureExt;

use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::timeout;
use twr_adapters::claude_code::guard;
use twr_adapters::fake::FakeAdapter;
use twr_core::account::Share;
use twr_core::adapter::{
    Adapter, AdapterCmd, AdapterEvent, AdapterHandle, AdapterInfo, SpawnConfig, Tier,
};
use twr_core::config::{AccountConfig, Config};
use twr_core::event::*;
use twr_core::proto::codes;
use twr_core::runner::{AgentInfo, AgentLogin};
use twr_hub::hub::NativeDirFn;
use twr_hub::{Caller, Hub, HubOptions, SwitchOptions};

const WAIT: Duration = Duration::from_secs(10);

/// The twr binary the hub's guard hook names.
const TWR: &str = "/opt/twr/bin/twr";

/// One orchestrator runtime session handed to the test.
struct OrchRt {
    cfg: SpawnConfig,
    events: mpsc::UnboundedSender<AdapterEvent>,
    turns: mpsc::UnboundedReceiver<(String, u64)>,
}

impl OrchRt {
    async fn next_turn(&mut self) -> (String, u64) {
        timeout(WAIT, self.turns.recv())
            .await
            .expect("timed out waiting for an orchestrator turn")
            .expect("the session ended")
    }

    fn end_turn(&self, turn: u64) {
        let _ = self.events.send(AdapterEvent::Emit {
            body: EventBody::TurnEnded(TurnEnded {
                result: TurnResult::Completed,
                ..Default::default()
            }),
            raw: None,
            turn: Some(turn),
        });
    }

    /// Ends turn `turn` `error`, with `ext` as `turn.ended`'s.
    fn fail_turn(&self, turn: u64, ext: Value) {
        let _ = self.events.send(AdapterEvent::Emit {
            body: EventBody::TurnEnded(TurnEnded {
                result: TurnResult::Error,
                ext: ext.as_object().cloned(),
                ..Default::default()
            }),
            raw: None,
            turn: Some(turn),
        });
    }

    /// The value of `key` in its environment.
    fn env(&self, key: &str) -> Option<&str> {
        self.cfg
            .env
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// A headless runtime that can host the orchestrator, named `name`.
struct OrchAdapter {
    name: &'static str,
    tx: mpsc::UnboundedSender<OrchRt>,
}

impl Adapter for OrchAdapter {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: self.name,
            tier: Tier::Headless,
            description: "test orchestrator runtime",
        }
    }

    fn spawn(&self, cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let (h, mut end) = AdapterHandle::pair();
        end.emit(
            EventBody::ThreadStarted(SessionPayload {
                session_id: Some(format!("s-{}", self.name)),
                ..Default::default()
            }),
            None,
        );
        let (turn_tx, turns) = mpsc::unbounded_channel();
        if let Some(p) = prompt {
            let _ = turn_tx.send((p, cfg.prompt_turn.unwrap_or(1)));
        }
        let events = end.events.clone();
        tokio::spawn(async move {
            let mut active = None;
            while let Some(cmd) = end.cmds.recv().await {
                match cmd {
                    AdapterCmd::Send { text, turn } => {
                        active = Some(turn);
                        let _ = turn_tx.send((text, turn));
                    }
                    AdapterCmd::Interrupt => {
                        if let Some(turn) = active.take() {
                            end.emit(
                                EventBody::TurnEnded(TurnEnded {
                                    result: TurnResult::Interrupted,
                                    ..Default::default()
                                }),
                                Some(turn),
                            );
                        }
                    }
                    AdapterCmd::Stop => {
                        end.exited(Some(0), "");
                        return;
                    }
                    AdapterCmd::Respond { .. } => {}
                }
            }
        });
        let _ = self.tx.send(OrchRt { cfg, events, turns });
        Ok(h)
    }

    /// `orch-attach <session> <what the hub's config asked for, as JSON>`.
    fn attach_command(&self, cfg: &SpawnConfig, session_id: &str) -> Option<std::process::Command> {
        let mut cmd = std::process::Command::new("orch-attach");
        cmd.arg(session_id).arg(
            json!({
                "builtin_tools": cfg.builtin_tools,
                "settings": cfg.settings,
                "env": cfg.env,
                "env_remove": cfg.env_remove,
            })
            .to_string(),
        );
        Some(cmd)
    }
}

/// A scratch home: the orchestrator's directory, the accounts' directories (`acc/work`
/// sharing its configuration, `acc/solo` sharing nothing), the native directories
/// (`native/<runtime>`), and every orchestrator spawn.
struct Env {
    dir: tempfile::TempDir,
    spawns: mpsc::UnboundedReceiver<OrchRt>,
    tx: mpsc::UnboundedSender<OrchRt>,
}

impl Env {
    fn new() -> Env {
        let (tx, spawns) = mpsc::unbounded_channel();
        let dir = tempfile::tempdir().unwrap();
        for d in ["orch", "acc/work", "acc/solo", "native/claude-code"] {
            std::fs::create_dir_all(dir.path().join(d)).unwrap();
        }
        Env { dir, spawns, tx }
    }

    fn path(&self, p: &str) -> PathBuf {
        self.dir.path().canonicalize().unwrap().join(p)
    }

    fn opts(&self, edit: impl FnOnce(&mut Config)) -> HubOptions {
        let mut config = Config {
            default_runtime: "fake".into(),
            ..Default::default()
        };
        config.orchestrator.runtime = Some("claude-code".into());
        config.orchestrator.cwd = Some(self.path("orch"));
        let mut account = |name: &str, share: Share| {
            config
                .accounts
                .entry("claude-code".into())
                .or_default()
                .insert(
                    name.into(),
                    AccountConfig {
                        dir: Some(self.path(&format!("acc/{name}"))),
                        share,
                        ..Default::default()
                    },
                );
        };
        account("work", Share::Config);
        account("solo", Share::None);
        edit(&mut config);
        let mut o = HubOptions::new(self.path("data/twr.db"), config);
        let tx = self.tx.clone();
        o.adapters = Arc::new(move |name: &str| -> Option<Arc<dyn Adapter>> {
            match name {
                "fake" => Some(Arc::new(FakeAdapter::new())),
                "claude-code" => Some(Arc::new(OrchAdapter {
                    name: "claude-code",
                    tx: tx.clone(),
                })),
                "codex" => Some(Arc::new(OrchAdapter {
                    name: "codex",
                    tx: tx.clone(),
                })),
                _ => None,
            }
        });
        o.socket = Some(self.path("hub.sock"));
        o.twr_exe = Some(PathBuf::from(TWR));
        // The managed policy from this test's own (empty) directories, never the host's.
        o.claude_policy = guard::PolicySources {
            managed_dir: Some(self.path("claude-managed")),
            config_dir: Some(self.path("native/claude-code")),
        };
        let native = self.path("native");
        let native: NativeDirFn = Arc::new(move |rt: &str| Some(native.join(rt)));
        o.native_dir = Some(native);
        o.stop_timeout = Duration::from_secs(5);
        o
    }

    async fn orch(&mut self) -> OrchRt {
        timeout(WAIT, self.spawns.recv())
            .await
            .expect("timed out waiting for an orchestrator to spawn")
            .unwrap()
    }

    /// The memory directory the orchestrator shares on a named account: the native
    /// account's for its working directory.
    fn shared_memory(&self) -> PathBuf {
        let key = guard::sanitize(&self.path("orch").to_string_lossy());
        self.path("native/claude-code/projects")
            .join(key)
            .join("memory")
    }
}

fn tui() -> Caller {
    Caller::user("tui")
}

fn quick() -> SwitchOptions {
    SwitchOptions::no_summary()
}

/// `ext.twr` of thread `id`'s `thread.created`.
fn created_twr(hub: &Hub, id: &str) -> Value {
    let events = hub.thread_events(id, 0, 10_000).unwrap().1;
    events
        .into_iter()
        .find_map(|e| match e.body {
            EventBody::ThreadCreated(p) => p.ext.and_then(|x| x.get("twr").cloned()),
            _ => None,
        })
        .expect("thread.created with ext.twr")
}

/// Asserts that `rt` runs on account `work`: its variables set, the stray credentials removed.
fn assert_on_work(rt: &OrchRt, env: &Env) {
    let work = env.path("acc/work");
    assert_eq!(
        rt.env("CLAUDE_CONFIG_DIR"),
        Some(work.to_str().unwrap()),
        "{:?}",
        rt.cfg.env
    );
    assert_eq!(
        rt.env("ANTHROPIC_CONFIG_DIR"),
        Some(work.join("anthropic").to_str().unwrap())
    );
    assert!(
        rt.cfg.env_remove.iter().any(|k| k == "ANTHROPIC_API_KEY"),
        "{:?}",
        rt.cfg.env_remove
    );
}

/// Asserts that `rt` runs on the native account: no account variable, nothing removed.
fn assert_native(rt: &OrchRt) {
    assert_eq!(rt.env("CLAUDE_CONFIG_DIR"), None, "{:?}", rt.cfg.env);
    assert!(rt.cfg.env_remove.is_empty(), "{:?}", rt.cfg.env_remove);
}

/// Asserts that `rt` has its file tools, guarded, with its memory in `memory` (`None`: the
/// one Claude Code derives).
fn assert_guarded(rt: &OrchRt, memory: Option<&Path>) {
    assert_eq!(
        rt.cfg.builtin_tools,
        Some(vec!["Read".into(), "Write".into(), "Edit".into()])
    );
    assert_eq!(
        rt.cfg.settings,
        Some(guard::settings(Path::new(TWR), memory))
    );
}

/// `[orchestrator] account` picks the first orchestrator's account (recorded, its environment
/// on the spawn, the native memory directory shared); a reset keeps it; `orchestrator.use
/// claude-code` without an account is the native account, and `claude-code:work` from there
/// is a switch with a handoff.
#[tokio::test]
async fn the_orchestrator_runs_on_its_account_across_resets_and_switches() {
    let mut env = Env::new();
    let hub = Hub::open(env.opts(|c| c.orchestrator.account = Some("work".into())))
        .await
        .unwrap();
    // Before it exists: what config picks.
    let info = hub.info();
    assert_eq!(info["orchestrator"]["account"], "work", "{info}");
    assert_eq!(info["orchestrator"]["configured"]["account"], "work");
    assert_eq!(info["orchestrator"]["account_shown"], true);

    hub.say("watch the builds", None, &tui()).await.unwrap();
    let mut first = env.orch().await;
    assert_on_work(&first, &env);
    let memory = env.shared_memory();
    assert_guarded(&first, Some(&memory));
    let settings = first.cfg.settings.clone().unwrap();
    assert_eq!(settings["autoMemoryDirectory"], memory.to_str().unwrap());
    let command = settings["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(
        command.contains(&format!("--memory-dir '{}'", memory.display())),
        "{command}"
    );
    let id = hub.orchestrator_thread().unwrap();
    let twr = created_twr(&hub, &id);
    assert_eq!(twr["account"], "work", "{twr}");
    assert_eq!(twr["account_dir"], env.path("acc/work").to_str().unwrap());
    assert_eq!(twr["account_reason"], "config");
    let info = hub.info();
    assert_eq!(info["orchestrator"]["account"], "work");
    let (_, t) = first.next_turn().await;
    first.end_turn(t);

    // A reset keeps the account.
    let r = hub.reset_orchestrator(&tui()).await.unwrap();
    assert_eq!(r["account"], "work", "{r}");
    let reset = r["thread"].as_str().unwrap().to_string();
    assert_ne!(reset, id);
    let twr = created_twr(&hub, &reset);
    assert_eq!(twr["account"], "work", "{twr}");
    assert_eq!(twr["account_dir"], env.path("acc/work").to_str().unwrap());
    assert_eq!(twr["account_reason"], "config");
    let mut second = env.orch().await;
    assert_on_work(&second, &env);
    assert_guarded(&second, Some(&memory));
    hub.send_by(&reset, "hello", &tui()).unwrap();
    let (_, t) = second.next_turn().await;
    second.end_turn(t);

    // A worker on `work`, for the handoff's thread lines: they say its account while
    // claude-code shows account labels.
    let folder = env.path("proj");
    std::fs::create_dir_all(&folder).unwrap();
    hub.new_thread(
        twr_hub::hub::NewThread {
            project: folder.to_string_lossy().into_owned(),
            runtime: Some("claude-code:work".into()),
            name: Some("w1".into()),
            no_worktree: true,
            ..Default::default()
        },
        "test",
    )
    .await
    .unwrap();

    // A switch without an account: the native one (no policy default), the memory Claude
    // Code derives.
    let r = hub
        .use_orchestrator("claude-code", None, quick(), &tui())
        .await
        .unwrap();
    assert_eq!(r["account"], "default", "{r}");
    let native_id = r["thread"].as_str().unwrap().to_string();
    let mut native = env.orch().await;
    assert_native(&native);
    assert_guarded(&native, None);
    assert_eq!(created_twr(&hub, &native_id).get("account_reason"), None);
    let (handoff, t) = native.next_turn().await;
    assert!(handoff.starts_with("[twr handoff]"), "{handoff}");
    assert!(handoff.contains("runtime=claude-code:work"), "{handoff}");
    native.end_turn(t);

    // Only the account of the same runtime: a switch, with a handoff.
    let r = hub
        .use_orchestrator("claude-code:work", None, quick(), &tui())
        .await
        .unwrap();
    assert_eq!(r["account"], "work", "{r}");
    assert_eq!(r["runtime"], "claude-code");
    assert_eq!(r["previous"]["thread"], native_id.as_str());
    let work_id = r["thread"].as_str().unwrap().to_string();
    assert_ne!(work_id, native_id);
    let mut work = env.orch().await;
    assert_on_work(&work, &env);
    assert_guarded(&work, Some(&memory));
    let twr = created_twr(&hub, &work_id);
    assert_eq!(twr["account_reason"], "explicit", "{twr}");
    assert_eq!(twr["account_pin"], "work");
    let (handoff, t) = work.next_turn().await;
    assert!(handoff.starts_with("[twr handoff]"), "{handoff}");
    work.end_turn(t);
    let thread = hub
        .list_threads()
        .into_iter()
        .find(|t| t["id"] == work_id.as_str())
        .unwrap();
    assert_eq!(thread["account"], "work");
    assert_eq!(hub.info()["orchestrator"]["account"], "work");
    hub.shutdown().await;
}

/// An account that cannot be used is refused for `orchestrator.use` before anything happens,
/// and a configured one (`[orchestrator] account`, the account part of `[orchestrator]
/// runtime`, the policy's default) refuses making the first orchestrator: never the native
/// account in its place. The hub still opens (an autostart only logs it); naming a usable
/// account makes it.
#[tokio::test]
async fn unknown_accounts_are_refused() {
    let mut env = Env::new();
    let hub = Hub::open(env.opts(|c| {
        c.orchestrator.account = Some("nope".into());
        c.orchestrator.autostart = true;
    }))
    .await
    .unwrap();
    // What it would run on, though it cannot.
    assert_eq!(hub.info()["orchestrator"]["account"], "nope");
    let e = hub.say("hello", None, &tui()).await.unwrap_err();
    assert!(
        e.message.starts_with(
            "the orchestrator cannot be made: claude-code has no account \"nope\": twr \
             account add claude-code nope"
        ),
        "{e:?}"
    );
    assert_eq!(e.code, codes::BAD_REQUEST, "{e:?}");
    // A reset without an orchestrator makes the first one: refused alike.
    let e = hub.reset_orchestrator(&tui()).await.unwrap_err();
    assert!(e.message.contains("has no account \"nope\""), "{e:?}");
    assert_eq!(hub.orchestrator_thread(), None);
    assert!(env.spawns.try_recv().is_err(), "no orchestrator spawned");
    for (spec, why) in [
        ("claude-code:nope", "claude-code has no account \"nope\""),
        ("claude-code:", "names no account"),
    ] {
        let e = hub
            .use_orchestrator(spec, None, quick(), &tui())
            .await
            .unwrap_err();
        assert_eq!(e.code, codes::BAD_REQUEST, "{spec}: {e:?}");
        assert!(e.message.contains(why), "{spec}: {e:?}");
    }
    assert_eq!(hub.orchestrator_thread(), None);
    // The user picks one that works.
    let r = hub
        .use_orchestrator("claude-code:work", None, quick(), &tui())
        .await
        .unwrap();
    assert_eq!(r["account"], "work", "{r}");
    let rt = env.orch().await;
    assert_on_work(&rt, &env);
    hub.shutdown().await;

    // The account part of `[orchestrator] runtime`, and the policy's default: the same.
    for (edit, why) in [
        (
            Box::new(|c: &mut Config| c.orchestrator.runtime = Some("claude-code:gone".into()))
                as Box<dyn FnOnce(&mut Config)>,
            "claude-code has no account \"gone\"",
        ),
        (
            Box::new(|c: &mut Config| {
                c.account_policy.insert(
                    "claude-code".into(),
                    twr_core::config::AccountPolicy {
                        default: Some("gone".into()),
                    },
                );
            }),
            "[account_policy.claude-code] default is \"gone\", which claude-code doesn't have",
        ),
    ] {
        let mut env = Env::new();
        let hub = Hub::open(env.opts(edit)).await.unwrap();
        let e = hub.say("hello", None, &tui()).await.unwrap_err();
        assert!(
            e.message.starts_with("the orchestrator cannot be made: ") && e.message.contains(why),
            "{e:?}"
        );
        assert_eq!(hub.orchestrator_thread(), None);
        assert!(env.spawns.try_recv().is_err(), "no orchestrator spawned");
        hub.shutdown().await;
    }
}

/// A named account that shares nothing keeps its own memory: no `autoMemoryDirectory`, no
/// `--memory-dir`, still its environment.
#[tokio::test]
async fn an_account_that_shares_nothing_keeps_its_own_memory() {
    let mut env = Env::new();
    let hub = Hub::open(env.opts(|c| c.orchestrator.runtime = Some("claude-code:solo".into())))
        .await
        .unwrap();
    hub.say("hello", None, &tui()).await.unwrap();
    let rt = env.orch().await;
    let solo = env.path("acc/solo");
    assert_eq!(rt.env("CLAUDE_CONFIG_DIR"), Some(solo.to_str().unwrap()));
    assert_guarded(&rt, None);
    let twr = created_twr(&hub, &hub.orchestrator_thread().unwrap());
    assert_eq!(twr["account"], "solo", "{twr}");
    assert_eq!(twr["account_reason"], "config");
    hub.shutdown().await;
}

/// Account settings that set `autoMemoryDirectory` already (the native `settings.json` the
/// account links to, or the account's own) keep it: no memory directory is injected for the
/// named account that shares the configuration. Settings that are not JSON, or no such key,
/// set nothing: the native memory directory is shared. Claude Code reads the account's
/// settings only, so a native `settings.json` the account does not link to counts for nothing.
#[tokio::test]
async fn an_existing_auto_memory_directory_is_kept() {
    let mut env = Env::new();
    let native = env.path("native/claude-code/settings.json");
    let linked = env.path("acc/work/settings.json");
    std::fs::write(&native, "{ not json").unwrap();
    std::os::unix::fs::symlink(&native, &linked).unwrap();
    let hub = Hub::open(env.opts(|c| c.orchestrator.runtime = Some("claude-code:work".into())))
        .await
        .unwrap();
    hub.say("hello", None, &tui()).await.unwrap();
    let mut rt = env.orch().await;
    assert_on_work(&rt, &env);
    let memory = env.shared_memory();
    assert_guarded(&rt, Some(&memory));
    let (_, t) = rt.next_turn().await;
    rt.end_turn(t);
    let steps = [
        // Through the link, the native settings.
        (
            &native,
            r#"{"autoMemoryDirectory": "/home/me/memories"}"#,
            None,
        ),
        (&native, r#"{"model": "opus"}"#, Some(&memory)),
        (&native, r#"{"autoMemoryDirectory": "  "}"#, Some(&memory)),
    ];
    for (file, text, want) in steps {
        std::fs::write(file, text).unwrap();
        hub.reset_orchestrator(&tui()).await.unwrap();
        let mut rt = env.orch().await;
        assert_on_work(&rt, &env);
        assert_guarded(&rt, want.map(PathBuf::as_path));
        if let Some(settings) = &rt.cfg.settings {
            let set = settings.get("autoMemoryDirectory").is_some();
            assert_eq!(set, want.is_some(), "{text}: {settings}");
        }
        let id = hub.orchestrator_thread().unwrap();
        hub.send_by(&id, "hello", &tui()).unwrap();
        let (_, t) = rt.next_turn().await;
        rt.end_turn(t);
    }
    // The account's own settings, no longer a link.
    std::fs::remove_file(&linked).unwrap();
    std::fs::write(&linked, r#"{"autoMemoryDirectory": "~/mem"}"#).unwrap();
    hub.reset_orchestrator(&tui()).await.unwrap();
    let mut rt = env.orch().await;
    assert_guarded(&rt, None);
    let id = hub.orchestrator_thread().unwrap();
    hub.send_by(&id, "hello", &tui()).unwrap();
    let (_, t) = rt.next_turn().await;
    rt.end_turn(t);
    // The native settings set it, the account's own do not: the account's decide.
    std::fs::write(&native, r#"{"autoMemoryDirectory": "/home/me/memories"}"#).unwrap();
    std::fs::write(&linked, r#"{"model": "opus"}"#).unwrap();
    hub.reset_orchestrator(&tui()).await.unwrap();
    let rt = env.orch().await;
    assert_guarded(&rt, Some(&memory));
    hub.shutdown().await;
}

/// A native configuration directory reached through a symbolic link: the memory directory
/// shared is the real one, as `autoMemoryDirectory` and the guard's `--memory-dir` alike
/// (Claude Code's `dontAsk` refuses a memory write through a link).
#[tokio::test]
async fn the_shared_memory_directory_is_the_real_one() {
    let mut env = Env::new();
    let link = env.path("native-link");
    std::os::unix::fs::symlink(env.path("native"), &link).unwrap();
    let mut o = env.opts(|c| c.orchestrator.runtime = Some("claude-code:work".into()));
    let native: NativeDirFn = Arc::new(move |rt: &str| Some(link.join(rt)));
    o.native_dir = Some(native);
    let hub = Hub::open(o).await.unwrap();
    hub.say("hello", None, &tui()).await.unwrap();
    let rt = env.orch().await;
    assert_on_work(&rt, &env);
    let memory = env.shared_memory();
    assert_guarded(&rt, Some(&memory));
    let settings = rt.cfg.settings.clone().unwrap();
    assert_eq!(settings["autoMemoryDirectory"], memory.to_str().unwrap());
    let command = settings["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    assert!(
        command.contains(&format!("--memory-dir '{}'", memory.display())),
        "{command}"
    );
    hub.shutdown().await;
}

/// Server-managed settings (`remote-settings.json`) in a named account's directory turn the
/// file tools off for an orchestrator on that account only: the native one keeps them.
#[tokio::test]
async fn an_accounts_server_managed_settings_turn_its_file_tools_off() {
    let mut env = Env::new();
    std::fs::write(env.path("acc/work/remote-settings.json"), "{}").unwrap();
    let hub = Hub::open(env.opts(|_| {})).await.unwrap();
    hub.say("hello", None, &tui()).await.unwrap();
    let mut native = env.orch().await;
    assert_native(&native);
    assert_guarded(&native, None);
    let (_, t) = native.next_turn().await;
    native.end_turn(t);
    let r = hub
        .use_orchestrator("claude-code:work", None, quick(), &tui())
        .await
        .unwrap();
    assert_eq!(r["account"], "work", "{r}");
    let work = env.orch().await;
    assert_on_work(&work, &env);
    assert_eq!(work.cfg.builtin_tools, Some(Vec::new()));
    assert_eq!(work.cfg.settings, None);
    hub.shutdown().await;
}

/// Attaching the orchestrator on a named account carries its environment, its removals and
/// the shared memory directory, as its spawn did.
#[tokio::test]
async fn attaching_the_orchestrator_carries_its_account() {
    let mut env = Env::new();
    let hub = Hub::open(env.opts(|c| c.orchestrator.account = Some("work".into())))
        .await
        .unwrap();
    hub.say("hello", None, &tui()).await.unwrap();
    let mut rt = env.orch().await;
    let (_, t) = rt.next_turn().await;
    rt.end_turn(t);
    let id = hub.orchestrator_thread().unwrap();
    let grant = timeout(WAIT, async {
        loop {
            match hub.attach_begin(&id, true, "test").await {
                // Its session is not recorded yet.
                Err(e) if e.message.contains("no runtime session") => {
                    tokio::time::sleep(Duration::from_millis(20)).await
                }
                other => return other,
            }
        }
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(grant.argv[0], "orch-attach");
    let asked: Value = serde_json::from_str(&grant.argv[2]).unwrap();
    let work = env.path("acc/work");
    let env_list = asked["env"].as_array().unwrap();
    assert!(
        env_list.contains(&json!(["CLAUDE_CONFIG_DIR", work.to_str().unwrap()])),
        "{asked}"
    );
    assert!(
        asked["env_remove"]
            .as_array()
            .unwrap()
            .contains(&json!("ANTHROPIC_API_KEY")),
        "{asked}"
    );
    let memory = env.shared_memory();
    assert_eq!(
        asked["settings"],
        guard::settings(Path::new(TWR), Some(&memory))
    );
    assert_eq!(asked["builtin_tools"], json!(["Read", "Write", "Edit"]));
    hub.attach_end(&id, "test", grant.lease, None).unwrap();
    hub.shutdown().await;
}

/// The orchestrator's working directory holds a `.git` that is a FIFO (no writer: a blocking
/// read would wait forever, and the hub reads it under its core lock to find the memory
/// directory a named account shares): the orchestrator on a named account that shares its
/// configuration starts at once, its memory the directory's own, and the hub keeps answering
/// (an attach works the memory directory out again). Run on a thread of its own, so a hub
/// that blocks fails the test instead of hanging it.
#[test]
fn a_fifo_for_a_git_file_does_not_stall_the_hub() {
    let (tx, rx) = std::sync::mpsc::channel();
    let run = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut env = Env::new();
            let dot_git = env.path("orch/.git");
            let c = std::ffi::CString::new(dot_git.to_str().unwrap()).unwrap();
            assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
            let hub =
                Hub::open(env.opts(|c| c.orchestrator.runtime = Some("claude-code:work".into())))
                    .await
                    .unwrap();
            hub.say("hello", None, &tui()).await.unwrap();
            let mut rt = env.orch().await;
            assert_on_work(&rt, &env);
            let memory = env.shared_memory();
            assert_guarded(&rt, Some(&memory));
            let (_, t) = rt.next_turn().await;
            rt.end_turn(t);
            let id = hub.orchestrator_thread().unwrap();
            let grant = timeout(WAIT, async {
                loop {
                    match hub.attach_begin(&id, true, "test").await {
                        // Its session is not recorded yet.
                        Err(e) if e.message.contains("no runtime session") => {
                            tokio::time::sleep(Duration::from_millis(20)).await
                        }
                        other => return other,
                    }
                }
            })
            .await
            .unwrap()
            .unwrap();
            let asked: Value = serde_json::from_str(&grant.argv[2]).unwrap();
            assert_eq!(
                asked["settings"],
                guard::settings(Path::new(TWR), Some(&memory))
            );
            hub.attach_end(&id, "test", grant.lease, None).unwrap();
            hub.shutdown().await;
        });
        let _ = tx.send(());
    });
    match rx.recv_timeout(WAIT * 2) {
        Ok(()) => {}
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            std::panic::resume_unwind(run.join().unwrap_err())
        }
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            panic!("the hub stalled on a FIFO .git in the orchestrator's directory")
        }
    }
}

/// A login check that says the accounts whose directory's name (`native` for the native
/// account) is in `out` are logged out, and every other one logged in.
fn logins(out: &'static [&'static str]) -> twr_hub::hub::AccountProbe {
    Arc::new(move |runtime: String, dir: Option<PathBuf>| {
        let name = dir
            .as_deref()
            .and_then(Path::file_name)
            .map_or("native".to_string(), |n| n.to_string_lossy().into_owned());
        let login = match out.contains(&name.as_str()) {
            true => AgentLogin::LoggedOut,
            false => AgentLogin::LoggedIn,
        };
        async move {
            AgentInfo {
                runtime,
                found: true,
                login,
                ..Default::default()
            }
        }
        .boxed()
    })
}

/// Waits until `f` holds of `hub.info()`'s `orchestrator`.
async fn until_orch(hub: &Hub, what: &str, f: impl Fn(&Value) -> bool) -> Value {
    timeout(WAIT, async {
        loop {
            let o = hub.info()["orchestrator"].clone();
            if f(&o) {
                return o;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}: {}", hub.info()))
}

/// `[orchestrator] account` is a pin: a list with a glob picks the first of its accounts
/// that can be used (`solo` is logged out: `work`, which `w*` matches).
#[tokio::test]
async fn the_configured_account_is_a_pin() {
    let mut env = Env::new();
    let mut o = env.opts(|c| {
        c.orchestrator.account = Some(twr_core::account::Pin(vec!["solo".into(), "w*".into()]))
    });
    o.account_probe = Some(logins(&["solo"]));
    let hub = Hub::open(o).await.unwrap();
    let o = until_orch(&hub, "solo's logout", |o| o["account"] == "work").await;
    assert_eq!(o["configured"]["account"], "solo,w*", "{o}");
    hub.say("hello", None, &tui()).await.unwrap();
    let rt = env.orch().await;
    assert_on_work(&rt, &env);
    let twr = created_twr(&hub, &hub.orchestrator_thread().unwrap());
    assert_eq!(twr["account"], "work", "{twr}");
    assert_eq!(twr["account_reason"], "config");
    hub.shutdown().await;
}

/// Before the first orchestrator exists, `runtime` is the runtime alone, its configured
/// account apart (`account`, and `configured.runtime` as written), with or without labels.
#[tokio::test]
async fn a_configured_runtime_account_is_not_part_of_the_runtime() {
    let env = Env::new();
    let mut o = env.opts(|c| c.orchestrator.runtime = Some("claude-code:work".into()));
    o.account_probe = Some(logins(&["native", "solo"]));
    let hub = Hub::open(o).await.unwrap();
    let o = hub.info()["orchestrator"].clone();
    assert_eq!(o["runtime"], "claude-code", "{o}");
    let o = until_orch(&hub, "labels off", |o| o["account_shown"] == false).await;
    for (k, want) in [
        ("runtime", "claude-code"),
        ("account", "work"),
        ("thread", ""),
    ] {
        match want {
            "" => assert!(o[k].is_null(), "{k}: {o}"),
            w => assert_eq!(o[k], w, "{k}: {o}"),
        }
    }
    assert_eq!(o["configured"]["runtime"], "claude-code:work", "{o}");
    assert_eq!(o["configured"]["account"], "work", "{o}");
    hub.shutdown().await;
}

/// With no `[orchestrator] account`, the policy's default is the first orchestrator's, and a
/// switch's without an account part.
#[tokio::test]
async fn the_policys_default_applies_without_a_configured_account() {
    let mut env = Env::new();
    let hub = Hub::open(env.opts(|c| {
        c.account_policy.insert(
            "claude-code".into(),
            twr_core::config::AccountPolicy {
                default: Some("work".into()),
            },
        );
    }))
    .await
    .unwrap();
    assert_eq!(hub.info()["orchestrator"]["account"], "work");
    assert_eq!(
        hub.info()["orchestrator"]["configured"]["account"],
        Value::Null
    );
    hub.say("hello", None, &tui()).await.unwrap();
    let mut rt = env.orch().await;
    assert_on_work(&rt, &env);
    let id = hub.orchestrator_thread().unwrap();
    assert_eq!(created_twr(&hub, &id)["account_reason"], "default");
    let (_, t) = rt.next_turn().await;
    rt.end_turn(t);
    let r = hub
        .use_orchestrator("claude-code", None, quick(), &tui())
        .await
        .unwrap();
    assert_eq!(r["account"], "work", "{r}");
    let rt = env.orch().await;
    assert_on_work(&rt, &env);
    let twr = created_twr(&hub, r["thread"].as_str().unwrap());
    assert_eq!(twr["account_reason"], "default", "{twr}");
    hub.shutdown().await;
}

/// The orchestrator's failed turns say what a worker's do of its account, on its own card
/// (never retried): a sign-in failure on its named account says `twr account login` in place
/// of the adapter's hint and has that account's login checked; a limit names the account and
/// when it resets, with `ext.twr.limit` and `ext.twr.account`.
#[tokio::test]
async fn the_orchestrators_failures_name_its_account() {
    let mut env = Env::new();
    let checks: Arc<Mutex<Vec<Option<PathBuf>>>> = Arc::default();
    let mut o = env.opts(|c| c.orchestrator.account = Some("work".into()));
    let seen = Arc::clone(&checks);
    o.account_probe = Some(Arc::new(move |runtime: String, dir: Option<PathBuf>| {
        seen.lock().unwrap().push(dir);
        async move {
            AgentInfo {
                runtime,
                found: true,
                login: AgentLogin::LoggedIn,
                ..Default::default()
            }
        }
        .boxed()
    }));
    let hub = Hub::open(o).await.unwrap();
    let eventually = |what: &'static str, f: Box<dyn Fn() -> bool>| async move {
        timeout(WAIT, async {
            while !f() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
    };
    // The checks at open (the native account, `work` and `solo`) are done before counting.
    let c = Arc::clone(&checks);
    eventually(
        "the login checks at open",
        Box::new(move || c.lock().unwrap().len() >= 3),
    )
    .await;
    hub.say("watch the builds", None, &tui()).await.unwrap();
    let mut rt = env.orch().await;
    let id = hub.orchestrator_thread().unwrap();
    let name = hub.thread_view(&id).unwrap().name;
    let raised = |turn: u64| {
        let (_, evs) = hub.thread_events(&id, 0, 10_000).unwrap();
        evs.into_iter().find_map(|e| match e.body {
            EventBody::AttentionRaised(a) if e.turn == Some(turn) => Some(a),
            _ => None,
        })
    };
    let item = |turn: u64| {
        let raised = &raised;
        async move {
            timeout(WAIT, async {
                loop {
                    if let Some(a) = raised(turn) {
                        return a;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("the error item")
        }
    };

    // A sign-in failure.
    let hint = twr_adapters::claude_code::translate::AUTH_HINT;
    let (_, t) = rt.next_turn().await;
    let before = checks.lock().unwrap().len();
    rt.fail_turn(
        t,
        json!({"claude": {"auth_failed": true, "transient": false},
               "twr": {"error": format!("Not logged in · Please run /login{hint}")}}),
    );
    let a = item(t).await;
    assert_eq!(a.reason, AttentionReason::Error);
    assert_eq!(
        a.summary,
        format!(
            "{name}: turn {t} failed: Not logged in · Please run /login (sign in again: twr \
             account login claude-code work)"
        )
    );
    let (c, work) = (Arc::clone(&checks), env.path("acc/work"));
    eventually(
        "work's check",
        Box::new(move || c.lock().unwrap()[before..].contains(&Some(work.clone()))),
    )
    .await;

    // A long one: cut before the hint, never the hint.
    hub.send_by(&id, "again", &tui()).unwrap();
    let (_, t) = rt.next_turn().await;
    rt.fail_turn(
        t,
        json!({"claude": {"auth_failed": true, "transient": false},
               "twr": {"error": format!("{}{hint}", "x".repeat(500))}}),
    );
    let a = item(t).await;
    assert_eq!(
        a.summary,
        format!(
            "{name}: turn {t} failed: {}... (sign in again: twr account login claude-code work)",
            "x".repeat(197)
        )
    );

    // A limit.
    hub.send_by(&id, "again", &tui()).unwrap();
    let (_, t2) = rt.next_turn().await;
    let at = Utc::now() + TimeDelta::minutes(13 * 60 + 30);
    let resets = at.to_rfc3339_opts(SecondsFormat::Secs, true);
    let limit = json!({"kind": "usage", "window": "seven_day", "resets_at": resets});
    rt.fail_turn(
        t2,
        json!({"twr": {"error": "You've hit your weekly limit", "limit": limit}}),
    );
    let a = item(t2).await;
    let local = DateTime::parse_from_rfc3339(&resets)
        .unwrap()
        .with_timezone(&Local);
    assert_eq!(
        a.summary,
        format!(
            "{name}: claude-code:work hit its weekly limit · resets {} (in 13h)",
            local.format("%H:%M")
        )
    );
    let twr = &a.ext.as_ref().unwrap()["twr"];
    assert_eq!(twr["limit"], limit);
    assert_eq!(twr["account"], "work");
    // Never retried: the runtime got no other prompt.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(rt.turns.try_recv().is_err(), "a retry");
    hub.shutdown().await;
}

/// A reset keeps the orchestrator's account: one its new session could not start on (here
/// removed with `force`) refuses the reset before anything happens, the current orchestrator
/// left as it was, and says how to switch it (`twr orchestrator use`, never `twr model`). The
/// native account removed refuses a switch onto it the same way.
#[tokio::test]
async fn a_reset_on_an_account_it_cannot_start_on_is_refused_before_anything_stops() {
    let mut env = Env::new();
    std::fs::create_dir_all(env.path("acc/side")).unwrap();
    let hub = Hub::open(env.opts(|_| {})).await.unwrap();
    hub.account_add(
        twr_hub::hub::AccountAddParams {
            runtime: "claude-code".into(),
            account: "side".into(),
            dir: Some(env.path("acc/side")),
            ..Default::default()
        },
        &tui(),
    )
    .unwrap();
    let r = hub
        .use_orchestrator("claude-code:side", None, quick(), &tui())
        .await
        .unwrap();
    assert_eq!(r["account"], "side", "{r}");
    let id = hub.orchestrator_thread().unwrap();
    hub.send_by(&id, "hello", &tui()).unwrap();
    let mut rt = env.orch().await;
    let (_, t) = rt.next_turn().await;
    rt.end_turn(t);
    let o = hub.orchestrator_info();
    assert_eq!(o["account"], "side", "{o}");
    assert!(o.get("account_removed").is_none(), "{o}");
    let removed = hub
        .account_remove(
            twr_hub::hub::AccountRemoveParams {
                runtime: "claude-code".into(),
                account: "side".into(),
                force: true,
                ..Default::default()
            },
            &tui(),
        )
        .await
        .unwrap();
    assert_eq!(removed["dir"], env.path("acc/side").to_str().unwrap());
    // `orchestrator.show` says the account was removed (shown whatever `account_shown` says).
    let o = hub.orchestrator_info();
    assert_eq!(o["account"], "side", "{o}");
    assert_eq!(o["account_removed"], true, "{o}");
    let e = hub.reset_orchestrator(&tui()).await.unwrap_err();
    assert_eq!(e.code, codes::CONFLICT, "{e:?}");
    assert_eq!(
        e.message,
        "orchestrator runs on claude-code:side, which was removed: switch it (twr \
         orchestrator use claude-code:<name>)"
    );
    // Untouched: still the orchestrator, not stopped, its runtime still taking turns.
    assert_eq!(hub.orchestrator_thread().as_deref(), Some(id.as_str()));
    let view = hub.thread_view(&id).unwrap();
    assert!(!view.stopped, "{view:?}");
    let (_, evs) = hub.thread_events(&id, 0, 10_000).unwrap();
    assert!(
        !evs.iter()
            .any(|e| matches!(e.body, EventBody::ThreadStopped(_))),
        "stopped"
    );
    hub.send_by(&id, "still there", &tui()).unwrap();
    let (text, t) = rt.next_turn().await;
    assert_eq!(text, "still there");
    rt.end_turn(t);

    // The native account removed: a switch onto it says how to pick another.
    hub.account_remove(
        twr_hub::hub::AccountRemoveParams {
            runtime: "claude-code".into(),
            account: "default".into(),
            ..Default::default()
        },
        &tui(),
    )
    .await
    .unwrap();
    let e = hub
        .use_orchestrator("claude-code", None, quick(), &tui())
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST, "{e:?}");
    assert_eq!(
        e.message,
        "claude-code's own login (default) was removed: pick an account (twr orchestrator use \
         claude-code:<account>, or [orchestrator] account = \"<account>\") or twr account \
         name-native claude-code default"
    );
    assert_eq!(hub.orchestrator_thread().as_deref(), Some(id.as_str()));
    hub.shutdown().await;
}

/// A switch onto an account whose directory was deleted since the last login check is refused
/// as a new thread's would be, before the current orchestrator is stopped: it stays the
/// orchestrator and keeps taking turns.
#[tokio::test]
async fn a_switch_onto_a_missing_directory_is_refused_before_anything_stops() {
    let mut env = Env::new();
    let hub = Hub::open(env.opts(|_| {})).await.unwrap();
    hub.use_orchestrator("claude-code:work", None, quick(), &tui())
        .await
        .unwrap();
    let id = hub.orchestrator_thread().unwrap();
    hub.send_by(&id, "hello", &tui()).unwrap();
    let mut rt = env.orch().await;
    let (_, t) = rt.next_turn().await;
    rt.end_turn(t);
    let solo = env.path("acc/solo");
    std::fs::remove_dir_all(&solo).unwrap();
    let e = hub
        .use_orchestrator("claude-code:solo", None, quick(), &tui())
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::CONFLICT, "{e:?}");
    assert_eq!(
        e.message,
        format!(
            "claude-code:solo's directory {} is missing: twr account add claude-code solo makes \
             it again and logs it in",
            solo.display()
        )
    );
    assert_eq!(hub.orchestrator_thread().as_deref(), Some(id.as_str()));
    let view = hub.thread_view(&id).unwrap();
    assert!(!view.stopped, "{view:?}");
    let (_, evs) = hub.thread_events(&id, 0, 10_000).unwrap();
    assert!(
        !evs.iter()
            .any(|e| matches!(e.body, EventBody::ThreadStopped(_))),
        "stopped"
    );
    hub.send_by(&id, "still there", &tui()).unwrap();
    let (text, t) = rt.next_turn().await;
    assert_eq!(text, "still there");
    rt.end_turn(t);
    hub.shutdown().await;
}

/// `account.remove {move_to}` of an account the orchestrator runs on, with workers on it too,
/// refuses the whole removal before anything is recorded (the orchestrator moves only by a
/// switch), saying to switch it first; without `move_to` the threads listed say which one is
/// the orchestrator. Once it is switched, the removal moves the workers.
#[tokio::test]
async fn removing_the_orchestrators_account_asks_to_switch_it_first() {
    let env = Env::new();
    for d in ["acc/side", "api"] {
        std::fs::create_dir_all(env.path(d)).unwrap();
    }
    let hub = Hub::open(env.opts(|c| {
        c.projects.insert(
            "api".into(),
            twr_core::config::ProjectConfig {
                repo: env.path("api"),
                ..Default::default()
            },
        );
    }))
    .await
    .unwrap();
    hub.account_add(
        twr_hub::hub::AccountAddParams {
            runtime: "claude-code".into(),
            account: "side".into(),
            dir: Some(env.path("acc/side")),
            ..Default::default()
        },
        &tui(),
    )
    .unwrap();
    let r = hub
        .use_orchestrator("claude-code:side", None, quick(), &tui())
        .await
        .unwrap();
    assert_eq!(r["account"], "side", "{r}");
    let orch = hub.orchestrator_thread().unwrap();
    let mut workers = Vec::new();
    for name in ["s1", "s2"] {
        let v = hub
            .new_thread(
                twr_hub::NewThread {
                    project: "api".into(),
                    runtime: Some("claude-code".into()),
                    name: Some(name.into()),
                    no_worktree: true,
                    ..Default::default()
                },
                "itest",
            )
            .await
            .unwrap();
        let id = v["id"].as_str().unwrap().to_string();
        // Moved there (made on it, its own pin would keep it there).
        let r = hub
            .set_thread_model(
                &id,
                twr_hub::hub::ModelRequest {
                    account: Some("side"),
                    ..Default::default()
                },
                &tui(),
            )
            .await
            .unwrap();
        assert_eq!(r["account_changed"], json!(true), "{r}");
        workers.push(id);
    }
    let remove = |to: Option<&str>| twr_hub::hub::AccountRemoveParams {
        runtime: "claude-code".into(),
        account: "side".into(),
        move_to: to.map(str::to_string),
        ..Default::default()
    };
    let moves = |id: &str| {
        hub.thread_events(id, 0, 10_000)
            .unwrap()
            .1
            .iter()
            .filter(|e| matches!(e.body, EventBody::ThreadModelChanged(_)))
            .count()
    };
    let e = hub
        .account_remove(remove(Some("work")), &tui())
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::CONFLICT, "{e:?}");
    assert_eq!(
        e.message,
        "the orchestrator runs on claude-code:side: switch it first (twr orchestrator use \
         claude-code:work), then remove the account"
    );
    let e = hub.account_remove(remove(None), &tui()).await.unwrap_err();
    assert_eq!(e.code, codes::CONFLICT, "{e:?}");
    assert_eq!(
        e.message,
        "claude-code:side has threads on it: orchestrator (the orchestrator: switch it with twr \
         orchestrator use claude-code:<name>), s1, s2. Move them to another account first \
         (move_to), or remove it with force: they keep it and cannot start until they move"
    );
    // Nothing was recorded: the account is there, nothing moved, the orchestrator the same.
    let list = hub.account_list(Some("claude-code"), false).unwrap();
    let side = list["runtimes"]["claude-code"]["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "side")
        .cloned()
        .unwrap();
    assert_ne!(side["removed"], json!(true), "{side}");
    for id in &workers {
        assert_eq!(moves(id), 1, "{id} moved");
    }
    assert_eq!(hub.orchestrator_thread().as_deref(), Some(orch.as_str()));

    hub.use_orchestrator("claude-code:work", None, quick(), &tui())
        .await
        .unwrap();
    let r = hub
        .account_remove(remove(Some("work")), &tui())
        .await
        .unwrap();
    assert_eq!(r["moved"], json!(["s1", "s2"]), "{r}");
    assert_eq!(r["threads"], json!([]), "{r}");
    for id in &workers {
        assert_eq!(moves(id), 2, "{id} did not move");
        assert_eq!(
            hub.thread_view(id).unwrap().account.as_deref(),
            Some("work")
        );
    }
    hub.shutdown().await;
}
