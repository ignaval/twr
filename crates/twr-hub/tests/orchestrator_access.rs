//! The orchestrator's access level changed while the hub runs (`orchestrator.access`,
//! `hub::orch_access`; `config.set` of `orchestrator.access`, directly or by confirming the
//! orchestrator's card): applied at once, the orchestrator's live runtime restarted on its
//! session with the new restrictions (now when idle, when its turn ends when busy), the mode
//! recorded on its thread following, the level kept in `config.toml` (comments and all) when
//! it can be, and every change a `config.changed` line.
//!
//! The orchestrator's runtime is `claude-code` played by a test adapter that hands every
//! session to the test with the `SpawnConfig` the hub built.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::timeout;
use twr_adapters::fake::FakeAdapter;
use twr_core::adapter::{
    Adapter, AdapterCmd, AdapterEvent, AdapterHandle, AdapterInfo, Answer, SpawnConfig, Tier,
};
use twr_core::config::Config;
use twr_core::event::*;
use twr_core::proto::{codes, RpcError};
use twr_hub::hub::ControlSubscription;
use twr_hub::{server, Caller, Hub, HubOptions};

const WAIT: Duration = Duration::from_secs(10);

/// The config file the tests start from: comments and spacing that must stay as they are.
const CONFIG: &str = "\
# my twr config
default_runtime = \"fake\"   # the test runtime

[orchestrator]
runtime = \"claude-code\"   # keep this comment

[tui]
suggest = true            # a small model suggests
";

/// One orchestrator runtime session handed to the test.
struct OrchRt {
    cfg: SpawnConfig,
    events: mpsc::UnboundedSender<AdapterEvent>,
    turns: mpsc::UnboundedReceiver<(String, u64)>,
    stopped: mpsc::UnboundedReceiver<()>,
}

impl OrchRt {
    async fn next_turn(&mut self) -> (String, u64) {
        timeout(WAIT, self.turns.recv())
            .await
            .expect("timed out waiting for an orchestrator turn")
            .expect("the session ended")
    }

    async fn wait_stopped(&mut self) {
        timeout(WAIT, self.stopped.recv())
            .await
            .expect("timed out waiting for the hub to stop the session");
    }

    fn was_stopped(&mut self) -> bool {
        self.stopped.try_recv().is_ok()
    }

    fn end_turn(&self, turn: u64) {
        for body in [
            EventBody::AgentText(AgentText {
                text: "ok".into(),
                is_final: true,
                ..Default::default()
            }),
            EventBody::TurnEnded(TurnEnded {
                result: TurnResult::Completed,
                ..Default::default()
            }),
        ] {
            let _ = self.events.send(AdapterEvent::Emit {
                body,
                raw: None,
                turn: Some(turn),
            });
        }
    }
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

    fn spawn(&self, cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let (h, mut end) = AdapterHandle::pair();
        end.emit(
            EventBody::ThreadStarted(SessionPayload {
                session_id: Some("s-orch".into()),
                ..Default::default()
            }),
            None,
        );
        let (turn_tx, turns) = mpsc::unbounded_channel();
        let (stop_tx, stopped) = mpsc::unbounded_channel();
        if let Some(p) = prompt {
            let _ = turn_tx.send((p, cfg.prompt_turn.unwrap_or(1)));
        }
        let events = end.events.clone();
        tokio::spawn(async move {
            while let Some(cmd) = end.cmds.recv().await {
                match cmd {
                    AdapterCmd::Send { text, turn } => {
                        let _ = turn_tx.send((text, turn));
                    }
                    AdapterCmd::Stop => {
                        end.exited(Some(0), "");
                        let _ = stop_tx.send(());
                        return;
                    }
                    _ => {}
                }
            }
        });
        let _ = self.0.send(OrchRt {
            cfg,
            events,
            turns,
            stopped,
        });
        Ok(h)
    }
}

struct Env {
    dir: tempfile::TempDir,
    spawns: mpsc::UnboundedReceiver<OrchRt>,
    tx: mpsc::UnboundedSender<OrchRt>,
}

impl Env {
    fn new() -> Env {
        Env::with_config(CONFIG)
    }

    fn with_config(text: &str) -> Env {
        let (tx, spawns) = mpsc::unbounded_channel();
        let env = Env {
            dir: tempfile::tempdir().unwrap(),
            spawns,
            tx,
        };
        std::fs::write(env.config_file(), text).unwrap();
        env
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn config_file(&self) -> PathBuf {
        self.path().join("config.toml")
    }

    fn file(&self) -> String {
        std::fs::read_to_string(self.config_file()).unwrap()
    }

    /// The hub on the config file as it is now (`with_file`: it writes there).
    fn opts(&self, with_file: bool) -> HubOptions {
        let mut config = Config::from_toml_str(&self.file()).unwrap();
        config.orchestrator.cwd = Some(self.path().to_path_buf());
        let mut o = HubOptions::new(self.path().join("data").join("twr.db"), config);
        let orch: Arc<dyn Adapter> = Arc::new(OrchAdapter(self.tx.clone()));
        o.adapters = Arc::new(move |name: &str| -> Option<Arc<dyn Adapter>> {
            match name {
                "fake" => Some(Arc::new(FakeAdapter::new())),
                "claude-code" => Some(Arc::clone(&orch)),
                _ => None,
            }
        });
        o.socket = Some(self.path().join("hub.sock"));
        o.twr_exe = Some(PathBuf::from("/opt/twr/bin/twr"));
        o.claude_policy = twr_adapters::claude_code::guard::PolicySources {
            managed_dir: Some(self.path().join("claude-managed")),
            config_dir: Some(self.path().join("claude-config")),
        };
        if with_file {
            o.config_file = Some(self.config_file());
        }
        o.stop_timeout = Duration::from_secs(5);
        o
    }

    async fn hub(&self) -> Hub {
        Hub::open(self.opts(true)).await.unwrap()
    }

    /// Reopens after the previous hub was dropped (the data-dir lock may linger briefly).
    async fn reopen(&self) -> Hub {
        timeout(WAIT, async {
            loop {
                match Hub::open(self.opts(true)).await {
                    Ok(h) => return h,
                    Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
                }
            }
        })
        .await
        .expect("reopening the hub")
    }

    async fn orch(&mut self) -> OrchRt {
        timeout(WAIT, self.spawns.recv())
            .await
            .expect("timed out waiting for the orchestrator to spawn")
            .unwrap()
    }

    fn no_spawn(&mut self) -> bool {
        self.spawns.try_recv().is_err()
    }
}

fn tui() -> Caller {
    Caller::user("tui")
}

async fn set(hub: &Hub, access: &str) -> Value {
    hub.orchestrator_access(Some(access), &tui()).await.unwrap()
}

/// Every `config.changed` on the orchestrator's thread.
fn changes(hub: &Hub) -> Vec<ConfigChanged> {
    let orch = hub.orchestrator_thread().unwrap();
    hub.thread_events(&orch, 0, 10_000)
        .unwrap()
        .1
        .into_iter()
        .filter_map(|e| match e.body {
            EventBody::ConfigChanged(c) => Some(c),
            _ => None,
        })
        .collect()
}

/// The orchestrator's recorded mode.
fn mode(hub: &Hub) -> Value {
    let orch = hub.orchestrator_thread().unwrap();
    hub.list_threads()
        .into_iter()
        .find(|t| t["id"] == orch)
        .unwrap()["mode"]
        .clone()
}

/// The `ext.twr.reason` of each `thread.stopped` of the orchestrator.
fn stops(hub: &Hub) -> Vec<Value> {
    let orch = hub.orchestrator_thread().unwrap();
    hub.thread_events(&orch, 0, 10_000)
        .unwrap()
        .1
        .into_iter()
        .filter_map(|e| match e.body {
            EventBody::ThreadStopped(p) => Some(p.ext.unwrap_or_default()["twr"]["reason"].clone()),
            _ => None,
        })
        .collect()
}

/// The orchestrator, started and idle after its first turn.
async fn idle_orchestrator(env: &mut Env, hub: &Hub) -> OrchRt {
    hub.say_to_orchestrator("hello", None, &tui()).unwrap();
    let mut rt = env.orch().await;
    let (_, t) = rt.next_turn().await;
    rt.end_turn(t);
    wait_idle(hub).await;
    rt
}

async fn wait_idle(hub: &Hub) {
    timeout(WAIT, async {
        loop {
            if hub.orchestrator_info()["status"] == "idle" {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the orchestrator did not go idle");
}

/// A limited orchestrator's restrictions, on Claude Code.
fn assert_limited(cfg: &SpawnConfig) {
    assert_eq!(cfg.mode, "dontAsk");
    assert!(cfg
        .builtin_tools
        .as_ref()
        .is_some_and(|t| t.contains(&"WebSearch".to_string())));
    assert!(!cfg.keep_own_mcp);
    assert!(cfg
        .system_append
        .as_deref()
        .is_some_and(|s| s.contains("You write no code")));
}

/// A full orchestrator's, on Claude Code.
fn assert_full(cfg: &SpawnConfig) {
    assert_eq!(cfg.mode, "bypassPermissions");
    assert_eq!(cfg.builtin_tools, None);
    assert!(cfg.keep_own_mcp);
    assert!(cfg
        .system_append
        .as_deref()
        .is_some_and(|s| s.contains("You have full access")));
}

/// An idle orchestrator is restarted at once on its session with the new level: its tools,
/// mode and system prompt, the mode recorded on its thread, its next turn going to the new
/// runtime with the new prompt's fingerprint. The level goes into config.toml beside its
/// comments, the change is a `config.changed` line (by the user, when it takes effect in
/// words), and the hub reports it. Setting the current level changes nothing.
#[tokio::test]
async fn an_idle_orchestrator_restarts_with_the_new_level_on_its_session() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let mut rt = idle_orchestrator(&mut env, &hub).await;
    assert_limited(&rt.cfg);
    assert_eq!(mode(&hub), "dontAsk");
    let read = hub.orchestrator_access(None, &tui()).await.unwrap();
    assert_eq!(read, json!({ "access": "limited" }));
    let r = set(&hub, "full").await;
    assert_eq!(
        r,
        json!({ "access": "full", "previous": "limited", "persisted": true, "restart": "now" })
    );
    rt.wait_stopped().await;
    let mut new = env.orch().await;
    assert_eq!(new.cfg.resume_session.as_deref(), Some("s-orch"));
    assert_full(&new.cfg);
    assert_eq!(mode(&hub), "bypassPermissions");
    assert_eq!(stops(&hub), [json!(MODE_RESTART_REASON)]);
    assert_eq!(
        env.file(),
        CONFIG.replace("[orchestrator]\n", "[orchestrator]\naccess = \"full\"\n")
    );
    let c = changes(&hub);
    assert_eq!(c.len(), 1);
    assert_eq!(
        (
            c[0].key.as_str(),
            &c[0].value,
            &c[0].before,
            c[0].by.as_str()
        ),
        (
            "orchestrator.access",
            &json!("full"),
            &Some(json!("limited")),
            "user"
        )
    );
    assert!(
        c[0].effect.starts_with("now: the orchestrator restarts"),
        "{c:?}"
    );
    wait_idle(&hub).await;
    let info = hub.info();
    assert_eq!(info["orchestrator"]["access"], "full");
    assert_eq!(info["orchestrator"]["running_access"], "full");
    // The conversation goes on in the restarted runtime.
    let orch = hub.orchestrator_thread().unwrap();
    hub.say_to_orchestrator("go on", None, &tui()).unwrap();
    let (_, t) = new.next_turn().await;
    new.end_turn(t);
    let fingerprints: Vec<Value> = hub
        .thread_events(&orch, 0, 10_000)
        .unwrap()
        .1
        .into_iter()
        .filter_map(|e| match e.body {
            EventBody::TurnStarted(p) => {
                Some(p.ext.unwrap_or_default()["twr"]["prompt_fingerprint"].clone())
            }
            _ => None,
        })
        .collect();
    assert_eq!(fingerprints.len(), 2);
    assert!(fingerprints[0].is_string() && fingerprints[0] != fingerprints[1]);
    wait_idle(&hub).await;
    // The current level again: nothing.
    let file = env.file();
    let r = set(&hub, "full").await;
    assert_eq!(r["restart"], "none");
    assert_eq!(r["persisted"], true);
    assert_eq!(env.file(), file);
    assert_eq!(changes(&hub).len(), 1);
    assert!(!new.was_stopped());
    // And back.
    let r = set(&hub, "limited").await;
    assert_eq!(r["restart"], "now");
    new.wait_stopped().await;
    let back = env.orch().await;
    assert_limited(&back.cfg);
    assert_eq!(mode(&hub), "dontAsk");
    assert!(env.file().contains("access = \"limited\"\n"));
    drop(back);
    hub.shutdown().await;
}

/// A busy orchestrator keeps its running turn: the restart waits for the turn to end, the
/// level reported meanwhile being the one set, its runtime's the other. Changes made
/// meanwhile: the last one wins, and changing back means no restart at all.
#[tokio::test]
async fn a_busy_orchestrator_restarts_after_its_turn_with_the_last_level_set() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let mut rt = idle_orchestrator(&mut env, &hub).await;
    hub.say_to_orchestrator("think", None, &tui()).unwrap();
    let (_, t) = rt.next_turn().await;
    let r = set(&hub, "full").await;
    assert_eq!(r["restart"], "after_turn");
    let o = hub.orchestrator_info();
    assert_eq!(
        (&o["access"], &o["running_access"]),
        (&json!("full"), &json!("limited"))
    );
    assert_eq!(set(&hub, "limited").await["restart"], "none");
    assert_eq!(set(&hub, "full").await["restart"], "after_turn");
    assert_eq!(mode(&hub), "bypassPermissions");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!rt.was_stopped() && env.no_spawn());
    rt.end_turn(t);
    rt.wait_stopped().await;
    let new = env.orch().await;
    assert_full(&new.cfg);
    assert_eq!(new.cfg.resume_session.as_deref(), Some("s-orch"));
    assert_eq!(changes(&hub).len(), 3);
    // Changed back while busy: the turn ends without a restart.
    let mut new = new;
    hub.say_to_orchestrator("again", None, &tui()).unwrap();
    let (_, t) = new.next_turn().await;
    assert_eq!(set(&hub, "limited").await["restart"], "after_turn");
    assert_eq!(set(&hub, "full").await["restart"], "none");
    new.end_turn(t);
    wait_idle(&hub).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!new.was_stopped() && env.no_spawn());
    hub.shutdown().await;
}

/// The level changed in the file while the hub was down: the next hub starts with it, and the
/// mode recorded on the orchestrator's thread follows.
#[tokio::test]
async fn the_recorded_mode_follows_a_level_changed_across_hub_restarts() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let rt = idle_orchestrator(&mut env, &hub).await;
    hub.shutdown().await;
    drop((hub, rt));
    std::fs::write(
        env.config_file(),
        CONFIG.replace("[tui]", "access = \"full\"\n\n[tui]"),
    )
    .unwrap();
    let hub = env.reopen().await;
    assert_eq!(mode(&hub), "bypassPermissions");
    assert_eq!(hub.info()["orchestrator"]["access"], "full");
    hub.say_to_orchestrator("hello again", None, &tui())
        .unwrap();
    let rt = env.orch().await;
    assert_full(&rt.cfg);
    hub.shutdown().await;
    drop((hub, rt));
    // Unchanged, nothing more is recorded.
    let hub = env.reopen().await;
    let orch = hub.orchestrator_thread().unwrap();
    let modes = hub
        .thread_events(&orch, 0, 10_000)
        .unwrap()
        .1
        .into_iter()
        .filter(|e| matches!(e.body, EventBody::ThreadModeChanged(_)))
        .count();
    assert_eq!(modes, 1);
    hub.shutdown().await;
}

/// A level that can't be kept in the file (an inline `[orchestrator]` table config_edit won't
/// rewrite, or a hub without a config file) still applies until the hub stops, and says why.
#[tokio::test]
async fn a_level_that_cant_be_kept_still_applies() {
    let inline = "default_runtime = \"fake\"\norchestrator = { runtime = \"claude-code\" }\n";
    let mut env = Env::with_config(inline);
    let hub = env.hub().await;
    let mut rt = idle_orchestrator(&mut env, &hub).await;
    let r = set(&hub, "full").await;
    assert_eq!(r["persisted"], false, "{r}");
    assert!(
        r["persist_error"].as_str().is_some_and(|e| !e.is_empty()),
        "{r}"
    );
    assert_eq!(r["restart"], "now");
    assert_eq!(env.file(), inline);
    rt.wait_stopped().await;
    assert_full(&env.orch().await.cfg);
    assert_eq!(hub.info()["orchestrator"]["access"], "full");
    hub.shutdown().await;
    drop(hub);

    let mut env = Env::new();
    let hub = Hub::open(env.opts(false)).await.unwrap();
    let mut rt = idle_orchestrator(&mut env, &hub).await;
    let r = set(&hub, "full").await;
    assert_eq!(r["persisted"], false, "{r}");
    assert!(
        r["persist_error"]
            .as_str()
            .unwrap()
            .contains("without a config file"),
        "{r}"
    );
    rt.wait_stopped().await;
    assert_full(&env.orch().await.cfg);
    assert_eq!(env.file(), CONFIG);
    hub.shutdown().await;
}

/// The user sets the level; the orchestrator can't (its `set_config` puts it on a card), nor
/// can an agent (not one of its methods), and a device needs `admin` (its scope). A value that
/// is no level is refused. Without an orchestrator the level is only kept for its first start.
#[tokio::test]
async fn only_the_user_sets_the_level() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let e = hub
        .orchestrator_access(Some("full"), &Caller::Orchestrator)
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::UNAUTHORIZED, "{e:?}");
    assert!(e.message.contains("set_config"), "{e:?}");
    let e: RpcError = hub
        .orchestrator_access(Some("root"), &tui())
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST, "{e:?}");
    assert!(server::METHODS.contains(&"orchestrator.access"));
    assert!(!server::ORCHESTRATOR_METHODS.contains(&"orchestrator.access"));
    assert!(!server::AGENT_METHODS.contains(&"orchestrator.access"));
    assert_eq!(
        twr_core::devices::method_scope("orchestrator.access"),
        Some("admin")
    );
    // No orchestrator yet: nothing restarts; the first one starts with the level.
    let r = set(&hub, "full").await;
    assert_eq!(
        (&r["restart"], &r["persisted"]),
        (&json!("none"), &json!(true))
    );
    hub.say_to_orchestrator("hello", None, &tui()).unwrap();
    let rt = env.orch().await;
    assert_full(&rt.cfg);
    assert_eq!(mode(&hub), "bypassPermissions");
    drop(rt);
    hub.shutdown().await;
}

/// The next confirm card raised.
async fn card(sub: &mut ControlSubscription) -> String {
    timeout(WAIT, async {
        loop {
            let (_, ev) = sub.next().await.unwrap().expect("hub stopped");
            if let EventBody::AttentionRaised(r) = &ev.body {
                if is_confirm_card(&ev.thread, &r.ext) {
                    return r.attn_id.clone();
                }
            }
        }
    })
    .await
    .expect("timed out waiting for a confirm card")
}

/// `config.set` of `orchestrator.access` goes the same way: the user's own call applies it
/// now (its result says when the runtime restarts), and so does their Confirm on the
/// orchestrator's card, which the orchestrator's `set_config` raises for it.
#[tokio::test]
async fn config_set_of_the_level_applies_it_live() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let mut sub = hub.subscribe_control(0);
    let mut rt = idle_orchestrator(&mut env, &hub).await;
    let r = hub
        .config_set("orchestrator.access", json!("full"), &tui())
        .await
        .unwrap();
    assert_eq!(
        (&r["status"], &r["restart"]),
        (&json!("applied"), &json!("now")),
        "{r}"
    );
    assert!(r["effect"].as_str().unwrap().starts_with("now"), "{r}");
    rt.wait_stopped().await;
    let mut rt = env.orch().await;
    assert_full(&rt.cfg);
    wait_idle(&hub).await;
    // The orchestrator asks; the card's Confirm applies it.
    let seen = hub.control_read(0, 10_000).unwrap().1;
    hub.say_to_orchestrator("go back to limited access", Some(seen), &tui())
        .unwrap();
    let (_, t) = rt.next_turn().await;
    let r = hub
        .config_set(
            "orchestrator.access",
            json!("limited"),
            &Caller::Orchestrator,
        )
        .await
        .unwrap();
    assert_eq!(r["status"], "needs_confirmation", "{r}");
    rt.end_turn(t);
    let attn = card(&mut sub).await;
    assert_eq!(hub.info()["orchestrator"]["access"], "full");
    wait_idle(&hub).await;
    hub.respond(&attn, Answer::new("Confirm"), "tui").unwrap();
    rt.wait_stopped().await;
    let back = env.orch().await;
    assert_limited(&back.cfg);
    assert_eq!(mode(&hub), "dontAsk");
    assert_eq!(hub.info()["orchestrator"]["access"], "limited");
    assert!(env.file().contains("access = \"limited\"\n"));
    let c = changes(&hub);
    assert_eq!(c.len(), 2);
    assert_eq!(c[1].value, json!("limited"));
    drop(back);
    hub.shutdown().await;
}
