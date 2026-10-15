//! Switching the orchestrator (`orchestrator.use`, `twr orchestrator use`): a new orchestrator
//! thread on another runtime / model, the previous one stopped and retired, the choice kept
//! by the log across restarts, and the handoff the new one starts with (`hub::handoff`).
//!
//! Workers run the fake adapter. `claude-code`, `codex` and `grok-build` are played by a test
//! adapter that hands every session to the test ([`OrchRt`]) with the `SpawnConfig` the hub
//! built, and ends the session when the hub stops it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::timeout;
use twr_adapters::fake::FakeAdapter;
use twr_adapters::pty::PtyAdapter;
use twr_core::adapter::{
    Adapter, AdapterCmd, AdapterEvent, AdapterHandle, AdapterInfo, SpawnConfig, Tier,
};
use twr_core::config::Config;
use twr_core::event::*;
use twr_core::proto::codes;
use twr_hub::hub::ControlSubscription;
use twr_hub::{Caller, Hub, HubOptions, NewThread, SwitchOptions};

const WAIT: Duration = Duration::from_secs(10);

/// One orchestrator runtime session handed to the test.
struct OrchRt {
    runtime: &'static str,
    cfg: SpawnConfig,
    events: mpsc::UnboundedSender<AdapterEvent>,
    turns: mpsc::UnboundedReceiver<(String, u64)>,
    stopped: mpsc::UnboundedReceiver<()>,
}

impl OrchRt {
    /// The next turn the hub gives this session: `(prompt, turn)`.
    async fn next_turn(&mut self) -> (String, u64) {
        timeout(WAIT, self.turns.recv())
            .await
            .expect("timed out waiting for an orchestrator turn")
            .expect("the session ended")
    }

    /// Whether the hub gave this session a turn (without waiting).
    fn had_turn(&mut self) -> bool {
        self.turns.try_recv().is_ok()
    }

    /// Waits until the hub stopped this session.
    async fn wait_stopped(&mut self) {
        timeout(WAIT, self.stopped.recv())
            .await
            .expect("timed out waiting for the hub to stop the session");
    }

    fn emit(&self, body: EventBody, turn: Option<u64>) {
        let _ = self.events.send(AdapterEvent::Emit {
            body,
            raw: None,
            turn,
        });
    }

    /// Ends turn `turn` with `text` as its final reply.
    fn reply(&self, turn: u64, text: &str) {
        self.emit(
            EventBody::AgentText(AgentText {
                text: text.into(),
                is_final: true,
                ..Default::default()
            }),
            Some(turn),
        );
        self.end_turn(turn);
    }

    fn end_turn(&self, turn: u64) {
        self.emit(
            EventBody::TurnEnded(TurnEnded {
                result: TurnResult::Completed,
                ..Default::default()
            }),
            Some(turn),
        );
    }

    fn token(&self) -> String {
        let path = self
            .cfg
            .mcp_config_file
            .as_ref()
            .expect("an MCP config file");
        let mcp: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        mcp["mcpServers"]["twr"]["env"]["TWR_ORCH_TOKEN"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

/// A runtime that can host the orchestrator (headless), named `name`.
struct OrchAdapter {
    name: &'static str,
    tx: mpsc::UnboundedSender<OrchRt>,
    /// While set, every start fails as a missing agent program does ([`SPAWN_ERROR`]).
    failing: Arc<AtomicBool>,
}

/// The start error of an [`OrchAdapter`] while it is failing.
const SPAWN_ERROR: &str = "`no-such-agent` not found on PATH (install Codex, or set TWR_CODEX_BIN)";

impl Adapter for OrchAdapter {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: self.name,
            tier: Tier::Headless,
            description: "test orchestrator runtime",
        }
    }

    fn spawn(&self, cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        if self.failing.load(Ordering::SeqCst) {
            anyhow::bail!("{SPAWN_ERROR}");
        }
        let (h, mut end) = AdapterHandle::pair();
        end.emit(
            EventBody::ThreadStarted(SessionPayload {
                session_id: Some(format!("s-{}", self.name)),
                ..Default::default()
            }),
            None,
        );
        let (turn_tx, turns) = mpsc::unbounded_channel();
        let (stop_tx, stopped) = mpsc::unbounded_channel();
        let mut active = prompt.as_ref().map(|_| cfg.prompt_turn.unwrap_or(1));
        if let Some(p) = prompt {
            let _ = turn_tx.send((p, cfg.prompt_turn.unwrap_or(1)));
        }
        let events = end.events.clone();
        tokio::spawn(async move {
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
                        let _ = stop_tx.send(());
                        return;
                    }
                    AdapterCmd::Respond { .. } => {}
                }
            }
        });
        let _ = self.tx.send(OrchRt {
            runtime: self.name,
            cfg,
            events,
            turns,
            stopped,
        });
        Ok(h)
    }

    /// `orch-attach <session> <what the hub's config asked for, as JSON>`.
    fn attach_command(&self, cfg: &SpawnConfig, session_id: &str) -> Option<std::process::Command> {
        let mut cmd = std::process::Command::new("orch-attach");
        cmd.arg(session_id).arg(
            json!({
                "mode": cfg.mode,
                "builtin_tools": cfg.builtin_tools,
                "allowed_tools": cfg.allowed_tools,
                "settings": cfg.settings,
                "mcp_config": cfg.mcp_config,
                "mcp_config_file": cfg.mcp_config_file,
                "env": cfg.env,
            })
            .to_string(),
        );
        Some(cmd)
    }
}

/// The orchestrator runtimes the test adapter plays.
const ORCH_RUNTIMES: [&str; 4] = ["claude-code", "codex", "grok-build", "acp"];

struct Env {
    dir: tempfile::TempDir,
    spawns: mpsc::UnboundedReceiver<OrchRt>,
    tx: mpsc::UnboundedSender<OrchRt>,
    /// [`HubOptions::handoff_reply_timeout`] of the hubs it opens.
    reply_timeout: Duration,
    /// [`OrchAdapter::failing`] of every runtime it plays.
    failing: Arc<AtomicBool>,
}

impl Env {
    fn new() -> Env {
        let (tx, spawns) = mpsc::unbounded_channel();
        Env {
            dir: tempfile::tempdir().unwrap(),
            spawns,
            tx,
            reply_timeout: Duration::from_secs(5),
            failing: Arc::new(AtomicBool::new(false)),
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn opts(&self, edit: impl FnOnce(&mut Config)) -> HubOptions {
        let mut config = Config {
            default_runtime: "fake".into(),
            ..Default::default()
        };
        config.orchestrator.runtime = Some("claude-code".into());
        config.orchestrator.cwd = Some(self.path().to_path_buf());
        config.orchestrator.model = Some("haiku".into());
        edit(&mut config);
        let mut o = HubOptions::new(self.path().join("data").join("twr.db"), config);
        let tx = self.tx.clone();
        let failing = Arc::clone(&self.failing);
        o.adapters = Arc::new(move |name: &str| -> Option<Arc<dyn Adapter>> {
            match name {
                "fake" => Some(Arc::new(FakeAdapter::new())),
                "pty" => Some(Arc::new(PtyAdapter::new())),
                "claude-code" | "codex" | "grok-build" | "acp" => {
                    let name = ORCH_RUNTIMES.into_iter().find(|n| *n == name).unwrap();
                    Some(Arc::new(OrchAdapter {
                        name,
                        tx: tx.clone(),
                        failing: Arc::clone(&failing),
                    }))
                }
                _ => None,
            }
        });
        o.socket = Some(self.path().join("hub.sock"));
        o.twr_exe = Some(PathBuf::from("/opt/twr/bin/twr"));
        // Claude Code's managed policy from this test's own (empty) directories, never the
        // host's.
        o.claude_policy = twr_adapters::claude_code::guard::PolicySources {
            managed_dir: Some(self.path().join("claude-managed")),
            config_dir: Some(self.path().join("claude-config")),
        };
        o.stop_timeout = Duration::from_secs(5);
        o.handoff_reply_timeout = self.reply_timeout;
        o
    }

    async fn hub(&self) -> Hub {
        Hub::open(self.opts(|_| {})).await.unwrap()
    }

    /// Reopens after the previous hub was dropped (the data-dir lock may linger briefly).
    async fn reopen_with(&self, edit: impl Fn(&mut Config)) -> Hub {
        let mut last = None;
        let reopened = timeout(WAIT, async {
            loop {
                match Hub::open(self.opts(&edit)).await {
                    Ok(h) => return h,
                    Err(e) => {
                        last = Some(format!("{e:#}"));
                        tokio::time::sleep(Duration::from_millis(20)).await
                    }
                }
            }
        })
        .await;
        reopened.unwrap_or_else(|_| panic!("reopening the hub: {last:?}"))
    }

    async fn orch(&mut self) -> OrchRt {
        timeout(WAIT, self.spawns.recv())
            .await
            .expect("timed out waiting for an orchestrator to spawn")
            .unwrap()
    }

    fn no_spawn(&mut self) -> bool {
        self.spawns.try_recv().is_err()
    }

    async fn worker(&self, hub: &Hub, name: &str) -> String {
        let v = hub
            .new_thread(
                NewThread {
                    project: self.path().to_string_lossy().into_owned(),
                    runtime: Some("fake".into()),
                    name: Some(name.into()),
                    ..Default::default()
                },
                "test",
            )
            .await
            .unwrap();
        v["id"].as_str().unwrap().to_string()
    }
}

fn tui() -> Caller {
    Caller::user("tui")
}

/// `orchestrator.use` by the TUI.
async fn switch(hub: &Hub, runtime: &str, model: Option<&str>, opts: SwitchOptions) -> Value {
    hub.use_orchestrator(runtime, model, opts, &tui())
        .await
        .unwrap()
}

/// `orchestrator.use` without the summary request.
fn quick() -> SwitchOptions {
    SwitchOptions::no_summary()
}

async fn until(sub: &mut ControlSubscription, pred: impl Fn(&Event) -> bool) -> Vec<Event> {
    let mut seen = Vec::new();
    timeout(WAIT, async {
        loop {
            let (_, ev) = sub.next().await.unwrap().expect("hub stopped");
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
            seen.iter()
                .map(|e| e.kind().to_string())
                .collect::<Vec<_>>()
        )
    });
    seen
}

/// Every event of `thread`.
fn events_of(hub: &Hub, thread: &str) -> Vec<Event> {
    hub.thread_events(thread, 0, 10_000).unwrap().1
}

fn thread_json(hub: &Hub, id: &str) -> Value {
    hub.list_threads()
        .into_iter()
        .find(|t| t["id"] == id)
        .unwrap()
}

/// The spawn config restrictions every orchestrator runtime gets.
fn assert_orchestrator_cfg(rt: &OrchRt, env: &Env) {
    // Only a Claude Code orchestrator has file tools: for its memory directory, guarded by
    // the twr binary's hook.
    if rt.runtime == "claude-code" {
        assert_eq!(
            rt.cfg.builtin_tools,
            Some(vec!["Read".into(), "Write".into(), "Edit".into()])
        );
        assert_eq!(
            rt.cfg.settings,
            Some(twr_adapters::claude_code::guard::settings(Path::new(
                "/opt/twr/bin/twr"
            )))
        );
    } else {
        assert_eq!(rt.cfg.builtin_tools, Some(Vec::new()), "{}", rt.runtime);
        assert_eq!(rt.cfg.settings, None, "{}", rt.runtime);
    }
    assert_eq!(rt.cfg.allowed_tools, vec!["mcp__twr".to_string()]);
    assert!(rt
        .cfg
        .system_append
        .as_deref()
        .is_some_and(|s| s.contains("You are the twr orchestrator")));
    assert_eq!(rt.cfg.cwd, env.path());
    assert!(rt.cfg.mcp_config.is_none());
    let file = rt.cfg.mcp_config_file.as_ref().unwrap();
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(!rt.token().is_empty());
}

/// claude-code → codex → grok-build: each switch stops and retires the running orchestrator
/// (its turn interrupted, its queued prompts dropped) and starts a new orchestrator thread
/// with the new runtime and model and the same restrictions. The old ones get no notice or
/// delegation again, cannot be sent to, resumed or attached, and their tokens are dead.
#[tokio::test]
async fn switching_runtimes_makes_a_new_orchestrator_each_time() {
    let mut env = Env::new();
    let hub = Hub::open(env.opts(|c| c.orchestrator.effort = Some("max".into())))
        .await
        .unwrap();
    let api = env.worker(&hub, "api").await;
    env.worker(&hub, "web").await;
    let mut sub = hub.subscribe_control(0);

    // The configured runtime and model start the first orchestrator.
    let r = hub.say("watch the builds", None, &tui()).await.unwrap();
    assert_eq!(r["action"], "delegate");
    let mut claude = env.orch().await;
    assert_eq!(claude.runtime, "claude-code");
    assert_eq!(claude.cfg.model.as_deref(), Some("haiku"));
    assert_eq!(claude.cfg.effort.as_deref(), Some("max"));
    assert_orchestrator_cfg(&claude, &env);
    let first = hub.orchestrator_thread().unwrap();
    assert_eq!(hub.resolve("orch").unwrap(), first);
    let (_, t1) = claude.next_turn().await;
    let claude_token = claude.token();
    assert!(hub.orchestrator_token_is(&claude_token));
    // Busy: a second delegation queues behind the open turn.
    hub.say("and the deploys", None, &tui()).await.unwrap();

    // Switch while its turn runs (without waiting for its summary).
    let r = switch(&hub, "codex", Some(" o5 "), quick()).await;
    assert_eq!(r["runtime"], "codex");
    assert_eq!(r["model"], "o5");
    assert_eq!(r["name"], "orchestrator");
    assert_eq!(r["started"], true, "the previous one was running: {r}");
    assert_eq!(r["previous"]["thread"], first.as_str());
    assert_eq!(r["previous"]["runtime"], "claude-code");
    assert_eq!(r["previous"]["model"], "haiku");
    assert_eq!(r["previous"]["dropped_prompts"], 1);
    let second = r["thread"].as_str().unwrap().to_string();
    assert_ne!(second, first);
    claude.wait_stopped().await;
    until(&mut sub, |e| {
        e.kind() == "thread.stopped" && e.thread == first
    })
    .await;
    // The normal closing sequence: the open turn ended interrupted.
    let ended = events_of(&hub, &first)
        .into_iter()
        .find_map(|e| match e.body {
            EventBody::TurnEnded(p) if e.turn == Some(t1) => Some(p.result),
            _ => None,
        });
    assert_eq!(ended, Some(TurnResult::Interrupted));
    assert!(!hub.orchestrator_token_is(&claude_token));

    let mut codex = env.orch().await;
    assert_eq!(codex.runtime, "codex");
    assert_eq!(codex.cfg.model.as_deref(), Some("o5"));
    assert_eq!(codex.cfg.effort, None);
    assert_orchestrator_cfg(&codex, &env);
    // It starts with the handoff.
    let (handoff, t) = codex.next_turn().await;
    assert!(handoff.starts_with("[twr handoff]"), "{handoff}");
    assert!(handoff.contains("user: watch the builds"), "{handoff}");
    codex.end_turn(t);
    assert_ne!(codex.token(), claude_token);
    assert!(hub.orchestrator_token_is(&codex.token()));
    assert_eq!(hub.orchestrator_thread().as_deref(), Some(second.as_str()));
    for q in ["orchestrator", "orch"] {
        assert_eq!(hub.resolve(q).unwrap(), second, "{q}");
    }
    let old = thread_json(&hub, &first);
    assert_eq!(old["retired"], true);
    assert_eq!(old["name"], "orchestrator");
    let new = thread_json(&hub, &second);
    assert_eq!(new["retired"], false);
    assert_eq!(new["runtime"], "codex");
    assert_eq!(new["role"], "orchestrator");
    assert_eq!(
        new["mode"], "manual",
        "non-Claude runtimes get threads.default_mode"
    );
    let info = hub.info();
    assert_eq!(info["orchestrator"]["runtime"], "codex");
    assert_eq!(info["orchestrator"]["model"], "o5");
    assert_eq!(info["orchestrator"]["configured"]["runtime"], "claude-code");
    assert_eq!(info["orchestrator"]["retired"], 1);
    assert_eq!(
        info["orchestrator"]["runtimes"],
        json!(["claude-code", "codex", "grok-build"])
    );

    // grok-build, without a model: the runtime's default (the configured model is the first
    // orchestrator's, for the configured runtime).
    let codex_token = codex.token();
    until(&mut sub, |e| e.kind() == "turn.ended" && e.thread == second).await;
    let r = switch(&hub, "grok-build", None, quick()).await;
    assert_eq!(r["model"], Value::Null);
    assert_eq!(r["previous"]["runtime"], "codex");
    let third = r["thread"].as_str().unwrap().to_string();
    codex.wait_stopped().await;
    let mut grok = env.orch().await;
    assert_eq!(grok.runtime, "grok-build");
    assert_eq!(grok.cfg.model, None);
    assert_eq!(
        grok.cfg.effort, None,
        "configured Claude max must not block a Grok switch"
    );
    assert_orchestrator_cfg(&grok, &env);
    let (handoff, t) = grok.next_turn().await;
    assert!(handoff.starts_with("[twr handoff]"), "{handoff}");
    grok.end_turn(t);
    assert!(!hub.orchestrator_token_is(&codex_token));
    assert!(hub.orchestrator_token_is(&grok.token()));
    until(&mut sub, |e| {
        e.kind() == "thread.stopped" && e.thread == second
    })
    .await;
    assert_eq!(thread_json(&hub, &second)["retired"], true);

    // `say` delegates to the new one only.
    let r = hub
        .say("status of everything?", None, &tui())
        .await
        .unwrap();
    assert_eq!(r["action"], "delegate");
    assert_eq!(r["thread"], third.as_str());
    let (prompt, t) = grok.next_turn().await;
    assert!(prompt.contains("status of everything?"), "{prompt}");
    grok.end_turn(t);
    // A worker's question is noticed by the new one only.
    hub.send_by(&api, "!error worker needs help", &tui())
        .unwrap();
    let (notice, t) = grok.next_turn().await;
    assert!(notice.starts_with("[twr notice]"), "{notice}");
    assert!(notice.contains("thread=api reason=error"), "{notice}");
    grok.end_turn(t);
    assert!(!claude.had_turn() && !codex.had_turn());
    for old in [&first, &second] {
        let after_retire = events_of(&hub, old)
            .into_iter()
            .skip_while(|e| e.kind() != "thread.stopped")
            .filter(|e| e.kind() != "thread.stopped")
            .map(|e| e.kind().to_string())
            .collect::<Vec<_>>();
        assert!(after_retire.is_empty(), "{old}: {after_retire:?}");
    }

    // A retired orchestrator never takes a turn, a resume or an attach.
    let e = hub.send_by(&first, "hello?", &tui()).unwrap_err();
    assert_eq!(e.code, codes::CONFLICT);
    assert!(e.message.contains("retired orchestrator"), "{}", e.message);
    let e = hub.resume_thread(&second, &tui()).await.unwrap_err();
    assert_eq!(e.code, codes::CONFLICT);
    let e = hub.attach_begin(&first, true, "cli").await.unwrap_err();
    assert_eq!(e.code, codes::CONFLICT);
    assert!(env.no_spawn());
    hub.shutdown().await;
}

/// The switch lives in the log: a restarted hub keeps the new runtime and model (the config
/// still says claude-code) and autostarts that one.
#[tokio::test]
async fn the_switch_survives_a_restart() {
    let mut env = Env::new();
    let hub = env.hub().await;
    env.worker(&hub, "api").await;
    env.worker(&hub, "web").await;
    // No orchestrator yet: the switch just creates one; it starts with the first message.
    let r = switch(&hub, "codex", Some("o5"), SwitchOptions::default()).await;
    assert_eq!(r["previous"], Value::Null);
    assert_eq!(r["started"], false);
    assert_eq!(r["handoff"], Value::Null, "nothing to hand over");
    let id = r["thread"].as_str().unwrap().to_string();
    assert!(env.no_spawn());
    hub.shutdown().await;
    drop(hub);

    let hub = env.reopen_with(|c| c.orchestrator.autostart = true).await;
    let rt = env.orch().await;
    assert_eq!(rt.runtime, "codex");
    assert_eq!(rt.cfg.model.as_deref(), Some("o5"));
    assert_eq!(hub.orchestrator_thread().as_deref(), Some(id.as_str()));
    assert_eq!(hub.info()["orchestrator"]["runtime"], "codex");
    let r = hub.say("anything new?", None, &tui()).await.unwrap();
    assert_eq!(r["thread"], id.as_str());
    hub.shutdown().await;
    drop(hub);

    // Back to Claude. The codex orchestrator is not running after the restart: no summary
    // (it is not started for one) and nothing starts now. The handoff waits in the log for
    // the new orchestrator's first turn, across a restart, and goes out once, before the
    // first message.
    let hub = env.reopen_with(|_| {}).await;
    let r = switch(&hub, "claude-code", None, SwitchOptions::default()).await;
    let claude = r["thread"].as_str().unwrap().to_string();
    assert_eq!(r["started"], false, "{r}");
    assert_eq!(r["handoff"]["turn"], Value::Null);
    assert_eq!(r["handoff"]["summary"], false);
    assert_eq!(
        r["handoff"]["summary_note"],
        "the previous orchestrator was not running"
    );
    assert!(env.no_spawn());
    hub.shutdown().await;
    drop(hub);
    let hub = env.reopen_with(|_| {}).await;
    assert_eq!(hub.orchestrator_thread().as_deref(), Some(claude.as_str()));
    assert_eq!(thread_json(&hub, &id)["retired"], true);
    assert!(env.no_spawn());
    let mut sub = hub.subscribe_control(0);
    hub.say("hi", None, &tui()).await.unwrap();
    let mut rt = env.orch().await;
    assert_eq!(rt.runtime, "claude-code");
    assert_eq!(rt.cfg.model, None);
    let (handoff, t) = rt.next_turn().await;
    assert_eq!(t, 1);
    assert!(handoff.starts_with("[twr handoff]"), "{handoff}");
    assert!(handoff.contains("user: anything new?"), "{handoff}");
    assert!(handoff.contains("(none: the previous orchestrator was not running)"));
    rt.reply(t, "Got the handoff.");
    let (prompt, t) = rt.next_turn().await;
    assert_eq!((prompt.as_str(), t), ("hi", 2));
    rt.end_turn(t);
    until(&mut sub, |e| {
        e.kind() == "turn.ended" && e.thread == claude && e.turn == Some(2)
    })
    .await;
    hub.shutdown().await;
    drop((sub, hub));
    // Delivered: a restart (autostarting it) never sends it again.
    let hub = env.reopen_with(|c| c.orchestrator.autostart = true).await;
    let mut rt = env.orch().await;
    assert_eq!(rt.runtime, "claude-code");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!rt.had_turn());
    let handoffs = events_of(&hub, &claude)
        .into_iter()
        .filter(|e| matches!(&e.body, EventBody::TurnStarted(p) if twr_hub::hub::is_handoff(p)))
        .count();
    assert_eq!(handoffs, 1);
    hub.shutdown().await;
}

/// Only a user switches, and only to a local runtime that can host the orchestrator.
#[tokio::test]
async fn only_users_switch_and_only_to_runtimes_that_attach_mcp() {
    let env = Env::new();
    let hub = env.hub().await;
    let e = hub
        .use_orchestrator("codex", None, quick(), &Caller::Orchestrator)
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::UNAUTHORIZED);
    for bad in ["pty", "pty:bash", "fake", "nope", " "] {
        let e = hub
            .use_orchestrator(bad, None, quick(), &tui())
            .await
            .unwrap_err();
        assert_eq!(e.code, codes::BAD_REQUEST, "{bad}: {}", e.message);
        if bad != " " {
            assert!(e.message.contains("claude-code"), "{bad}: {}", e.message);
        }
        let why = match bad {
            "nope" => "unknown runtime",
            " " => "runtime is required",
            _ => "cannot host the orchestrator",
        };
        assert!(e.message.contains(why), "{bad}: {}", e.message);
    }
    for model in ["m".repeat(129), "o5\x1b[2J".into(), "o5\u{9b}31m".into()] {
        let e = hub
            .use_orchestrator("codex", Some(&model), quick(), &tui())
            .await
            .unwrap_err();
        assert_eq!(e.code, codes::BAD_REQUEST, "{model:?}");
    }
    assert!(hub.list_threads().is_empty(), "nothing was created");
    assert!(hub.orchestrator_thread().is_none());
    hub.shutdown().await;
}

/// A worker thread that holds the name `orchestrator` keeps it: orchestrators then get
/// `orchestrator-<n>` (and still the free alias `orch`), as before, across switches too.
#[tokio::test]
async fn a_worker_named_orchestrator_keeps_its_name() {
    let env = Env::new();
    let hub = env.hub().await;
    let w = env.worker(&hub, "orchestrator").await;
    let r = switch(&hub, "codex", None, quick()).await;
    assert_eq!(r["name"], "orchestrator-2");
    let r = switch(&hub, "grok-build", None, quick()).await;
    assert_eq!(r["name"], "orchestrator-2");
    let current = r["thread"].as_str().unwrap().to_string();
    assert_eq!(hub.resolve("orchestrator").unwrap(), w);
    assert_eq!(hub.resolve("orchestrator-2").unwrap(), current);
    assert!(thread_json(&hub, &current)["aliases"]
        .as_array()
        .unwrap()
        .contains(&json!("orch")));
    hub.shutdown().await;
}

// --- handoff ---------------------------------------------------------------------------------

/// The orchestrator's `turn.started` of turn `turn`.
fn started_turn(hub: &Hub, thread: &str, turn: u64) -> (Event, TurnStarted) {
    events_of(hub, thread)
        .into_iter()
        .find_map(|e| match &e.body {
            EventBody::TurnStarted(p) if e.turn == Some(turn) => Some((e.clone(), p.clone())),
            _ => None,
        })
        .expect("the turn started")
}

/// The whole prompt of a stored `turn.started`: from the blob store when it was over the
/// payload cap (the log then holds a preview and a `prompt_ref`).
fn full_prompt(hub: &Hub, p: &TurnStarted) -> String {
    match p.extra.get("prompt_ref").and_then(Value::as_str) {
        Some(r) => String::from_utf8(hub.blob(r).unwrap().expect("the blob")).unwrap(),
        None => p.prompt.clone(),
    }
}

/// Whether `p` is a hub notice turn that belongs to a handoff.
fn is_handoff_notice(ev: &Event, p: &TurnStarted) -> bool {
    let twr = p.ext.as_ref().and_then(|e| e.get("twr")).cloned();
    ev.source == Source::Hub
        && p.from == TurnFrom::Orchestrator
        && twr_hub::hub::is_handoff(p)
        && twr.is_some_and(|t| t["notice"] == true)
}

/// A switch away from a live, idle orchestrator asks it for a summary first (a hub notice
/// turn), then starts the new one with the handoff: that summary, the recent conversation
/// with the user, the threads, the open attention items and the reports, as a hub notice turn
/// recorded in the log (a blob past the payload cap). Worker output is not in it.
#[tokio::test]
async fn a_switch_hands_the_summary_and_the_conversation_over() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    let web = env.worker(&hub, "web").await;
    let mut sub = hub.subscribe_control(0);

    hub.say("watch the builds and prefer small commits", None, &tui())
        .await
        .unwrap();
    let mut claude = env.orch().await;
    let old = hub.orchestrator_thread().unwrap();
    let (_, t) = claude.next_turn().await;
    // A watch the orchestrator set (on web, which runs no turn here): a switch keeps it, and
    // the handoff lists it.
    hub.watch_thread(
        &web,
        "send its branch to twr-integrator",
        false,
        &Caller::Orchestrator,
    )
    .unwrap();
    claude.reply(t, "Will do: watching api and web.");
    until(&mut sub, |e| e.kind() == "turn.ended" && e.thread == old).await;
    // A worker turn with output, and a permission still open at the switch.
    hub.send_by(&api, "say SECRET-WORKER-OUTPUT", &tui())
        .unwrap();
    until(&mut sub, |e| e.kind() == "report.ready" && e.thread == api).await;
    // Success and permission updates are visible hub facts, with no extra model turns.
    hub.send_by(&api, "!perm Bash ls", &tui()).unwrap();
    until(&mut sub, |e| {
        e.kind() == "attention.raised" && e.thread == api
    })
    .await;
    let item = hub.attention_list(false)[0].clone();
    let attn = format!(
        "{}@{}",
        item["attn_id"].as_str().unwrap(),
        item["revision"].as_u64().unwrap()
    );

    let switching = switch(&hub, "codex", Some("o5"), SwitchOptions::default());
    let answering = async {
        let (request, t) = claude.next_turn().await;
        assert!(
            request.starts_with("[twr notice] Handoff request"),
            "{request}"
        );
        assert!(request.contains("codex, model o5"), "{request}");
        claude.reply(
            t,
            "SUMMARY: the user wants small commits; api waits on `ls`.",
        );
    };
    let (r, ()) = tokio::join!(switching, answering);
    assert_eq!(r["started"], true, "{r}");
    assert_eq!(r["handoff"]["summary"], true, "{r}");
    assert_eq!(r["handoff"]["summary_note"], Value::Null);
    assert_eq!(r["handoff"]["exchanges"], 1);
    assert_eq!(r["handoff"]["dropped_exchanges"], 0);
    let new = r["thread"].as_str().unwrap().to_string();
    claude.wait_stopped().await;

    // The request turn: a hub notice on the old orchestrator, its summary the final text.
    let (ev, p) = started_turn(&hub, &old, 2);
    assert!(is_handoff_notice(&ev, &p), "{p:?}");

    let mut codex = env.orch().await;
    let (handoff, t) = codex.next_turn().await;
    assert_eq!(t, 1);
    assert!(handoff.starts_with("[twr handoff]"), "{handoff}");
    assert!(handoff.contains("acknowledg"), "{handoff}");
    for part in [
        "<twr-handoff>\n## Summary from the previous orchestrator\nSUMMARY: the user wants small commits",
        "user: watch the builds and prefer small commits\norchestrator: Will do: watching api and web.",
        "- api (",
        "runtime=fake",
        "- web (",
        &format!("- {attn} thread=api reason=permission options=[allow, deny]"),
        "## Watches (1): the hub wakes you when these workers finish",
        &format!("- w1 on web ({web}): \"send its branch to twr-integrator\""),
        "## Recent reports (newest first)",
    ] {
        assert!(handoff.contains(part), "{part:?} missing from:\n{}", &handoff[..handoff.len().min(3000)]);
    }
    assert!(handoff.ends_with("</twr-handoff>\n"), "{handoff}");
    assert_eq!(hub.watches()["watches"][0]["watch"], "w1");
    // Notices and the summary request are not conversation; worker output only as reports.
    assert!(!handoff.contains("Handoff request"), "{handoff}");
    assert!(!handoff.contains("reason=permission attn_id"), "{handoff}");
    let before_threads = handoff.split("## Threads").next().unwrap();
    assert!(
        !before_threads.contains("SECRET-WORKER-OUTPUT"),
        "{before_threads}"
    );
    for line in handoff
        .lines()
        .filter(|l| l.contains("SECRET-WORKER-OUTPUT"))
    {
        let report = line.contains("last report") || line.contains("(by hub)");
        assert!(report, "worker output outside a report summary: {line}");
    }
    // In the log as a hub notice turn, the prompt whole (from the blob store when capped).
    let (ev, p) = started_turn(&hub, &new, 1);
    assert!(is_handoff_notice(&ev, &p), "{p:?}");
    assert!(full_prompt(&hub, &p) == handoff);
    assert_eq!(r["handoff"]["bytes"], handoff.len());
    codex.reply(t, "Got the handoff.");
    // The user's next message goes to the new one.
    hub.say("and the deploys?", None, &tui()).await.unwrap();
    let (prompt, _) = codex.next_turn().await;
    assert_eq!(prompt, "and the deploys?");
    hub.shutdown().await;
}

/// A busy orchestrator is waited for (at most `wait`), then the switch goes on without its
/// summary, with a note. Meanwhile the orchestrator takes no new prompt and no second switch
/// starts.
#[tokio::test]
async fn a_busy_orchestrator_is_waited_for_then_skipped() {
    let mut env = Env::new();
    let hub = env.hub().await;
    env.worker(&hub, "api").await;
    env.worker(&hub, "web").await;
    hub.say("a long task", None, &tui()).await.unwrap();
    let mut claude = env.orch().await;
    let old = hub.orchestrator_thread().unwrap();
    let (_, _busy) = claude.next_turn().await;

    let opts = SwitchOptions {
        summary: true,
        wait: Duration::from_millis(400),
    };
    let started = std::time::Instant::now();
    let user = tui();
    let switching = hub.use_orchestrator("codex", None, opts, &user);
    let meanwhile = async {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let e = hub.send_by(&old, "more", &tui()).unwrap_err();
        assert_eq!(e.code, codes::CONFLICT, "{e:?}");
        assert!(e.message.contains("being switched"), "{}", e.message);
        let e = hub.say("more", None, &tui()).await.unwrap_err();
        assert_eq!(e.code, codes::CONFLICT, "{e:?}");
        let e = hub
            .use_orchestrator("grok-build", None, quick(), &tui())
            .await
            .unwrap_err();
        assert_eq!(e.code, codes::CONFLICT, "{e:?}");
        assert!(e.message.contains("in progress"), "{}", e.message);
        let e = hub.reset_orchestrator(&tui()).await.unwrap_err();
        assert_eq!(e.code, codes::CONFLICT, "{e:?}");
    };
    let (r, ()) = tokio::join!(switching, meanwhile);
    let r = r.unwrap();
    assert!(started.elapsed() >= Duration::from_millis(400));
    assert_eq!(r["handoff"]["summary"], false, "{r}");
    assert_eq!(
        r["handoff"]["summary_note"],
        "the previous orchestrator was still busy after 0.4s"
    );
    claude.wait_stopped().await;
    assert!(!claude.had_turn(), "it was never asked");
    let mut codex = env.orch().await;
    let (handoff, _) = codex.next_turn().await;
    assert!(
        handoff.contains("user: a long task\norchestrator: (no reply recorded)"),
        "{handoff}"
    );
    assert!(handoff.contains("(none: the previous orchestrator was still busy after 0.4s)"));
    // The switch is over: messages reach the new orchestrator again.
    assert_eq!(
        hub.say("hello?", None, &tui()).await.unwrap()["action"],
        "delegate"
    );
    hub.shutdown().await;
}

#[tokio::test]
async fn a_handoff_summary_waits_for_background_work_then_stops_it_on_timeout() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let mut sub = hub.subscribe_control(0);
    hub.say("a long task", None, &tui()).await.unwrap();
    let mut claude = env.orch().await;
    let old = hub.orchestrator_thread().unwrap();
    let (_, turn) = claude.next_turn().await;
    claude.emit(
        EventBody::TaskUpdated(TaskUpdated {
            task_id: "review".into(),
            running: true,
            ..Default::default()
        }),
        None,
    );
    claude.end_turn(turn);
    until(&mut sub, |e| e.thread == old && e.kind() == "turn.ended").await;
    assert!(hub.thread_view(&old).unwrap().current_turn.is_none());
    assert_eq!(hub.thread_view(&old).unwrap().background_tasks.len(), 1);
    let result = hub
        .use_orchestrator(
            "codex",
            None,
            SwitchOptions {
                summary: true,
                wait: Duration::from_millis(100),
            },
            &tui(),
        )
        .await
        .unwrap();
    assert_eq!(result["handoff"]["summary"], false);
    assert_eq!(
        result["handoff"]["summary_note"],
        "the previous orchestrator was still busy after 0.1s"
    );
    claude.wait_stopped().await;
    assert!(
        !claude.had_turn(),
        "a summary must not preempt background work"
    );
    assert!(hub.thread_view(&old).unwrap().background_tasks.is_empty());
    hub.shutdown().await;
}

/// An orchestrator that never answers the summary request is stopped after the reply timeout;
/// `--no-summary` (`summary: false`) never asks.
#[tokio::test]
async fn no_summary_skips_the_request_and_a_silent_orchestrator_times_out() {
    let mut env = Env::new();
    env.reply_timeout = Duration::from_millis(300);
    let hub = env.hub().await;
    env.worker(&hub, "api").await;
    env.worker(&hub, "web").await;
    let mut sub = hub.subscribe_control(0);
    hub.say("first", None, &tui()).await.unwrap();
    let mut claude = env.orch().await;
    let old = hub.orchestrator_thread().unwrap();
    let (_, t) = claude.next_turn().await;
    claude.reply(t, "ok");
    until(&mut sub, |e| e.kind() == "turn.ended" && e.thread == old).await;

    // Live and idle, but not asked.
    let r = switch(&hub, "codex", None, quick()).await;
    assert_eq!(r["handoff"]["summary"], false);
    assert_eq!(
        r["handoff"]["summary_note"],
        "the switch was asked without it"
    );
    claude.wait_stopped().await;
    assert!(!claude.had_turn());
    let mut codex = env.orch().await;
    let second = hub.orchestrator_thread().unwrap();
    let (handoff, t) = codex.next_turn().await;
    assert!(
        handoff.contains("user: first\norchestrator: ok"),
        "{handoff}"
    );
    codex.reply(t, "ack");
    until(&mut sub, |e| e.kind() == "turn.ended" && e.thread == second).await;

    // Asked, silent: the switch goes on after the reply timeout.
    let switching = switch(&hub, "grok-build", None, SwitchOptions::default());
    let silent = async {
        let (request, _) = codex.next_turn().await;
        assert!(request.contains("Handoff request"), "{request}");
    };
    let (r, ()) = tokio::join!(switching, silent);
    assert_eq!(r["handoff"]["summary"], false);
    assert_eq!(
        r["handoff"]["summary_note"],
        "the previous orchestrator did not answer within 0.3s"
    );
    codex.wait_stopped().await;
    let mut grok = env.orch().await;
    let (handoff, _) = grok.next_turn().await;
    // The conversation spans every orchestrator so far; the new one's acknowledgement of a
    // handoff (a notice turn) is not conversation.
    assert!(
        handoff.contains("user: first\norchestrator: ok"),
        "{handoff}"
    );
    assert!(!handoff.contains("orchestrator: ack"), "{handoff}");
    hub.shutdown().await;
}

/// The conversation keeps the newest exchanges within the byte budget and says how many
/// older ones it dropped.
#[tokio::test]
async fn the_conversation_is_trimmed_oldest_first() {
    let mut env = Env::new();
    let hub = env.hub().await;
    // An orchestrator that never ran, with a long history in its log.
    let r = switch(&hub, "claude-code", None, quick()).await;
    let old = r["thread"].as_str().unwrap().to_string();
    let reply = "r".repeat(2000);
    for n in 1..=20u64 {
        let user = Source::User("tui".into());
        hub.append(
            &old,
            Some(n),
            user,
            EventBody::TurnStarted(TurnStarted {
                prompt: format!("message {n}."),
                from: TurnFrom::User,
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        hub.append(
            &old,
            Some(n),
            Source::Adapter("claude-code".into()),
            EventBody::AgentText(AgentText {
                text: format!("reply {n}. {reply}"),
                is_final: true,
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        hub.append(
            &old,
            Some(n),
            Source::Adapter("claude-code".into()),
            EventBody::TurnEnded(TurnEnded {
                result: TurnResult::Completed,
                ..Default::default()
            }),
            None,
        )
        .unwrap();
    }
    let r = switch(&hub, "codex", None, SwitchOptions::default()).await;
    // Not started (the previous one never ran): its first message starts it, handoff first.
    assert_eq!(r["started"], false);
    hub.send_by(r["thread"].as_str().unwrap(), "go", &tui())
        .unwrap();
    let kept = r["handoff"]["exchanges"].as_u64().unwrap();
    let dropped = r["handoff"]["dropped_exchanges"].as_u64().unwrap();
    assert_eq!(kept + dropped, 20);
    assert!((10..=12).contains(&kept), "{r}");
    let mut codex = env.orch().await;
    let (handoff, _) = codex.next_turn().await;
    assert!(handoff.contains(&format!("({dropped} older exchanges dropped")));
    assert!(handoff.contains("user: message 20.\norchestrator: reply 20."));
    assert!(
        !handoff.contains("user: message 1.\n"),
        "the oldest went first"
    );
    let conversation = handoff
        .split("## Recent conversation")
        .nth(1)
        .unwrap()
        .split("## Threads")
        .next()
        .unwrap();
    assert!(conversation.len() <= twr_hub::hub::CONVERSATION_MAX + 200);
    // Recorded with its prompt in the blob store (over the payload cap), whole on read.
    let (_, p) = started_turn(&hub, r["thread"].as_str().unwrap(), 1);
    assert!(p.extra.contains_key("prompt_ref"), "capped");
    assert!(p.prompt.len() < handoff.len());
    assert!(
        full_prompt(&hub, &p) == handoff,
        "the blob holds the whole handoff"
    );
    hub.shutdown().await;
}

/// A handoff is a hub notice: the relay guard never takes it for the user's message, even
/// when it quotes the user's "yes" from the previous conversation, which was said while the
/// permission was waiting (refused, not even a proposal).
#[tokio::test]
async fn a_quoted_yes_in_a_handoff_answers_nothing() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    env.worker(&hub, "web").await;
    let mut sub = hub.subscribe_control(0);
    hub.say("watch api", None, &tui()).await.unwrap();
    let mut claude = env.orch().await;
    let old = hub.orchestrator_thread().unwrap();
    let (_, t) = claude.next_turn().await;
    claude.reply(t, "ok");
    until(&mut sub, |e| e.kind() == "turn.ended" && e.thread == old).await;
    hub.send_by(&api, "!perm Bash ls", &tui()).unwrap();
    until(&mut sub, |e| {
        e.kind() == "attention.raised" && e.thread == api
    })
    .await;
    let attn = hub.attention_list(false)[0]["attn_id"]
        .as_str()
        .unwrap()
        .to_string();
    // The user's "yes" to the orchestrator, after the permission appeared.
    hub.send_by(&old, "yes", &tui()).unwrap();
    let (yes, t) = claude.next_turn().await;
    assert!(yes.ends_with("yes"), "{yes}");
    claude.reply(t, "Noted.");
    until(&mut sub, |e| e.kind() == "turn.ended" && e.thread == old).await;

    switch(&hub, "codex", None, quick()).await;
    let mut codex = env.orch().await;
    let (handoff, _) = codex.next_turn().await;
    assert!(
        handoff.contains("user: yes\norchestrator: Noted."),
        "{handoff}"
    );
    // In its handoff turn the new orchestrator tries to relay that "yes": refused.
    let e = hub
        .respond_relay(&attn, Answer::allow(), None, &Caller::Orchestrator)
        .unwrap_err();
    assert_eq!(e.code, codes::RELAY_FORBIDDEN, "{e:?}");
    assert!(
        e.message.contains("not started by a user message"),
        "{}",
        e.message
    );
    assert_eq!(hub.attention_list(false).len(), 1, "still open");
    hub.shutdown().await;
}

/// A new orchestrator that did not start at the switch gets its handoff first whatever
/// starts it: here a worker's question (a notice), which follows the handoff turn.
#[tokio::test]
async fn a_notice_waits_for_the_handoff() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    env.worker(&hub, "web").await;
    switch(&hub, "codex", None, quick()).await;
    let r = switch(&hub, "grok-build", None, SwitchOptions::default()).await;
    assert_eq!(r["started"], false);
    assert!(env.no_spawn());
    hub.send_by(&api, "!error worker needs help", &tui())
        .unwrap();
    let mut grok = env.orch().await;
    assert_eq!(grok.runtime, "grok-build");
    let (handoff, t) = grok.next_turn().await;
    assert_eq!(t, 1);
    assert!(handoff.starts_with("[twr handoff]"), "{handoff}");
    grok.reply(t, "ok");
    let (notice, t) = grok.next_turn().await;
    assert_eq!(t, 2);
    assert!(notice.starts_with("[twr notice]"), "{notice}");
    assert!(notice.contains("thread=api reason=error"), "{notice}");
    hub.shutdown().await;
}

/// A watch's firing queued while the orchestrator was busy goes over to the new one at a
/// switch (which keeps the watches) and reaches it after its handoff turn.
#[tokio::test]
async fn a_switch_delivers_a_queued_watch_firing_after_the_handoff() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    let mut sub = hub.subscribe_control(0);
    hub.say_to_orchestrator("send api's branch on when it finishes", None, &tui())
        .unwrap();
    let mut claude = env.orch().await;
    let (_, t) = claude.next_turn().await;
    hub.watch_thread(
        &api,
        "send its branch to twr-integrator",
        false,
        &Caller::Orchestrator,
    )
    .unwrap();
    // api finishes while the orchestrator's turn is still open: its line waits.
    hub.send_by(&api, "Committed a1e8080 on twr/api.", &tui())
        .unwrap();
    until(&mut sub, |e| e.kind() == "report.ready" && e.thread == api).await;
    assert!(!claude.had_turn(), "turn {t} is still open");

    let r = switch(&hub, "codex", None, quick()).await;
    assert_eq!(r["started"], true);
    let mut codex = env.orch().await;
    let (handoff, t) = codex.next_turn().await;
    assert!(handoff.starts_with("[twr handoff]"), "{handoff}");
    codex.reply(t, "ok");
    let (notice, _) = codex.next_turn().await;
    assert!(notice.starts_with("[twr notice] 1 update"), "{notice}");
    assert!(
        notice.contains("- thread=api report turn=1 grade=success")
            && notice.contains(" watch=w1 note=\"send its branch to twr-integrator\""),
        "{notice}"
    );
    hub.shutdown().await;
}

/// Whatever `threads.default_mode` says, an orchestrator never runs in a mode wider
/// than asking: Claude Code stays `dontAsk`, every other runtime (codex, grok, a generic ACP
/// agent) gets `manual` for a wider or runtime-specific default (recorded on its thread and
/// in its spawn config), and keeps a narrower one.
#[tokio::test]
async fn no_orchestrator_runs_in_a_mode_wider_than_asking() {
    for (default, want) in [
        ("bypassPermissions", "manual"),
        ("acceptEdits", "manual"),
        ("auto", "manual"),
        ("never", "manual"),
        ("plan", "plan"),
    ] {
        let mut env = Env::new();
        let hub = Hub::open(env.opts(|c| c.threads.default_mode = default.into()))
            .await
            .unwrap();
        hub.say("watch the builds", None, &tui()).await.unwrap();
        let claude = env.orch().await;
        assert_eq!(claude.cfg.mode, "dontAsk");
        for runtime in ["codex", "grok-build", "acp:agent --stdio"] {
            // The previous one is running: the new one starts at once.
            let r = switch(&hub, runtime, None, quick()).await;
            let rt = env.orch().await;
            assert_eq!(rt.cfg.mode, want, "{default} on {runtime}");
            assert_orchestrator_cfg(&rt, &env);
            let id = r["thread"].as_str().unwrap();
            assert_eq!(
                thread_json(&hub, id)["mode"],
                want,
                "{default} on {runtime}"
            );
        }
        hub.shutdown().await;
    }
}

/// Attaching the current orchestrator carries its restrictions (its mode, no
/// built-in tools, only the twr tools, no MCP server of the user's, no attachments) on Claude
/// Code. The other runtimes' attach commands cannot carry them (a generic ACP agent's
/// template; `codex resume`, which loads the MCP servers of the user's Codex config; grok's
/// TUI, which takes no tool profile), so attaching the orchestrator there is refused.
#[tokio::test]
async fn attaching_the_orchestrator_keeps_its_restrictions() {
    let mut env = Env::new();
    let hub = Hub::open(env.opts(|c| c.threads.default_mode = "bypassPermissions".into()))
        .await
        .unwrap();
    let attach = async |id: &str| {
        timeout(WAIT, async {
            loop {
                match hub.attach_begin(id, true, "test").await {
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
    };
    for runtime in ["claude-code", "codex", "grok-build", "acp:agent --stdio"] {
        let r = switch(&hub, runtime, None, quick()).await;
        let id = r["thread"].as_str().unwrap().to_string();
        hub.send_by(&id, "hello", &tui()).unwrap();
        let mut rt = env.orch().await;
        assert_eq!(rt.runtime, runtime.split(':').next().unwrap());
        // The handoff (after the first switch), then the hello: interrupted by the attach.
        let (_, t) = rt.next_turn().await;
        rt.end_turn(t);
        if runtime != "claude-code" {
            let e = attach(&id).await.unwrap_err();
            assert_eq!(e.code, codes::CONFLICT, "{runtime}: {e:?}");
            assert!(
                e.message.contains("cannot carry its restrictions"),
                "{runtime}: {e:?}"
            );
            assert!(thread_json(&hub, &id)["attached"].is_null(), "not attached");
            continue;
        }
        let grant = attach(&id).await.unwrap();
        assert_eq!(grant.argv[0], "orch-attach", "{runtime}");
        let asked: Value = serde_json::from_str(&grant.argv[2]).unwrap();
        assert_eq!(asked["mode"], "dontAsk", "{runtime}");
        assert_eq!(asked["builtin_tools"], json!(["Read", "Write", "Edit"]));
        assert_eq!(
            asked["settings"]["hooks"]["PreToolUse"][0]["hooks"][0]["command"],
            "'/opt/twr/bin/twr' claude-memory-guard || exit 2"
        );
        assert_eq!(asked["allowed_tools"], json!(["mcp__twr"]), "{runtime}");
        assert_eq!(
            asked["mcp_config"],
            json!({ "mcpServers": {} }),
            "{runtime}"
        );
        assert_eq!(asked["mcp_config_file"], Value::Null, "{runtime}");
        assert!(
            asked["env"]
                .as_array()
                .unwrap()
                .contains(&json!(["CLAUDE_CODE_DISABLE_ATTACHMENTS", "1"])),
            "{asked}"
        );
        hub.attach_end(&id, "test", grant.lease, None).unwrap();
    }
    hub.shutdown().await;
}

/// A handoff that cannot be delivered after the switch committed is the new
/// orchestrator's start error, not the switch's: the switch reports success with
/// `start_error`, and the new orchestrator is the current one (it gets the handoff when it
/// first starts).
#[tokio::test]
async fn a_handoff_that_cannot_be_delivered_is_a_start_error() {
    let mut env = Env::new();
    let hub = env.hub().await;
    hub.say("watch the builds", None, &tui()).await.unwrap();
    let _claude = env.orch().await;
    let db = rusqlite::Connection::open(env.path().join("data").join("twr.db")).unwrap();
    db.busy_timeout(Duration::from_secs(5)).unwrap();
    db.execute_batch(
        "CREATE TRIGGER inject BEFORE INSERT ON events WHEN NEW.kind = 'turn.started'
         BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;",
    )
    .unwrap();
    let r = hub
        .use_orchestrator("codex", None, quick(), &tui())
        .await
        .expect("the switch itself succeeded");
    assert_eq!(r["started"], false, "{r}");
    assert!(
        r["start_error"]
            .as_str()
            .is_some_and(|e| e.contains("injected write failure")),
        "{r}"
    );
    let id = r["thread"].as_str().unwrap().to_string();
    assert_eq!(hub.orchestrator_thread().as_deref(), Some(id.as_str()));
    db.execute_batch("DROP TRIGGER inject").unwrap();
    // Its first turn is still the handoff.
    hub.send_by(&id, "hello", &tui()).unwrap();
    let mut codex = env.orch().await;
    let (handoff, _) = codex.next_turn().await;
    assert!(handoff.starts_with("[twr handoff]"), "{handoff}");
    assert!(hub.fatal_error().is_none());
    hub.shutdown().await;
}

/// The open attention items of `thread`, as `attention.list --all` has them.
fn items_of(hub: &Hub, thread: &str) -> Vec<Value> {
    hub.attention_list(true)
        .into_iter()
        .filter(|i| i["thread"] == thread)
        .collect()
}

/// An orchestrator turn that fails (here: its runtime cannot start, so the
/// handoff turn ends `error`) raises an `error` item that quotes the cause. It gets no
/// `report.ready` (whose summary would point at `twr attach orchestrator`) and no `review`.
/// Failing again leaves one open item, the latest failure's; a hub restart replays the same
/// items, and one that died before raising the item raises it at startup the same way.
#[tokio::test]
async fn an_orchestrator_that_cannot_start_raises_its_cause() {
    let mut env = Env::new();
    let hub = env.hub().await;
    hub.say("watch the builds", None, &tui()).await.unwrap();
    let _claude = env.orch().await;
    env.failing.store(true, Ordering::SeqCst);
    let r = switch(&hub, "codex", None, quick()).await;
    assert!(
        r["start_error"]
            .as_str()
            .is_some_and(|e| e.contains(SPAWN_ERROR)),
        "{r}"
    );
    let id = r["thread"].as_str().unwrap().to_string();
    let no_reports_or_reviews = |hub: &Hub| {
        let evs = events_of(hub, &id);
        assert!(
            !evs.iter()
                .any(|e| matches!(e.body, EventBody::ReportReady(_))),
            "an orchestrator turn got a report"
        );
        assert!(
            !evs.iter().any(|e| matches!(
                &e.body,
                EventBody::AttentionRaised(a) if a.reason == AttentionReason::Review
            )),
            "an orchestrator turn got a review"
        );
        assert!(!evs.iter().any(|e| matches!(
            &e.body,
            EventBody::DecisionMade(d) if d.site == DecisionSite::Grade
        )));
    };
    let items = items_of(&hub, &id);
    assert_eq!(items.len(), 1, "{items:?}");
    assert_eq!(items[0]["reason"], "error");
    let summary = items[0]["summary"].as_str().unwrap();
    assert!(summary.contains(SPAWN_ERROR), "{summary}");
    assert!(summary.contains("turn 1 failed"), "{summary}");
    assert!(!summary.contains("twr attach"), "{summary}");
    no_reports_or_reviews(&hub);

    let raised = |hub: &Hub| {
        events_of(hub, &id)
            .into_iter()
            .filter(|e| matches!(e.body, EventBody::AttentionRaised(_)))
            .count()
    };
    let first = items[0].clone();
    hub.shutdown().await;
    drop(hub);

    // Replay: the same open item, nothing raised again.
    let hub = env.reopen_with(|_| {}).await;
    assert_eq!(items_of(&hub, &id), vec![first.clone()]);
    assert_eq!(raised(&hub), 1);
    no_reports_or_reviews(&hub);

    // It fails again (the user's next message), and the hub dies before the item is raised.
    let db = rusqlite::Connection::open(env.path().join("data").join("twr.db")).unwrap();
    db.busy_timeout(Duration::from_secs(5)).unwrap();
    db.execute_batch(
        "CREATE TRIGGER inject BEFORE INSERT ON events WHEN NEW.kind = 'attention.raised'
         BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;",
    )
    .unwrap();
    let _ = hub.send_by(&id, "hello", &tui());
    let ended = events_of(&hub, &id)
        .into_iter()
        .any(|e| e.turn == Some(2) && matches!(e.body, EventBody::TurnEnded(_)));
    assert!(ended);
    assert_eq!(items_of(&hub, &id), vec![first.clone()]);
    hub.shutdown().await;
    drop(hub);
    db.execute_batch("DROP TRIGGER inject").unwrap();

    // Recovery raises it at startup: one open item, the latest failure's (the first one is
    // superseded), as the live hub does.
    let hub = env.reopen_with(|_| {}).await;
    let items = items_of(&hub, &id);
    assert_eq!(items.len(), 1, "{items:?}");
    let summary = items[0]["summary"].as_str().unwrap();
    assert!(
        summary.contains("turn 2 failed") && summary.contains(SPAWN_ERROR),
        "{summary}"
    );
    assert_eq!(raised(&hub), 2);
    no_reports_or_reviews(&hub);
    let recovered = items[0].clone();
    hub.shutdown().await;
    drop(hub);
    let hub = env.reopen_with(|_| {}).await;
    assert_eq!(items_of(&hub, &id), vec![recovered]);
    assert_eq!(raised(&hub), 2);

    // Live: failing once more supersedes it the same way.
    hub.send_by(&id, "again", &tui()).unwrap();
    let items = items_of(&hub, &id);
    assert_eq!(items.len(), 1, "{items:?}");
    let summary = items[0]["summary"].as_str().unwrap();
    assert!(
        summary.contains("turn 3 failed") && summary.contains(SPAWN_ERROR),
        "{summary}"
    );
    let superseded = events_of(&hub, &id)
        .into_iter()
        .filter(
            |e| matches!(&e.body, EventBody::AttentionResolved(p) if p.by == ResolvedBy::Timeout),
        )
        .count();
    assert_eq!(superseded, 2);
    assert_eq!(raised(&hub), 3);
    no_reports_or_reviews(&hub);
    assert!(env.no_spawn());
    assert!(hub.fatal_error().is_none());
    hub.shutdown().await;
}

/// Switching to an ACP agent whose program is not on PATH (a bare name) or
/// not there (a path), as `acp:<command>` or a `[runtimes]` preset, says so in the result's
/// `warning`; the switch still happens. A program that is there gets no warning.
#[tokio::test]
async fn a_switch_to_an_agent_that_is_not_installed_warns() {
    let env = Env::new();
    let opts = env.opts(|c| {
        c.runtimes.insert(
            "ghost".into(),
            twr_core::config::RuntimePreset {
                acp: "twr-test-no-such-agent --acp".into(),
                ..Default::default()
            },
        );
        c.runtimes.insert(
            "shell".into(),
            twr_core::config::RuntimePreset {
                acp: "sh -c 'exit 0'".into(),
                ..Default::default()
            },
        );
    });
    let hub = Hub::open(opts).await.unwrap();
    for (runtime, missing) in [
        (
            "acp:twr-test-no-such-agent --stdio",
            Some("`twr-test-no-such-agent` not found on PATH"),
        ),
        ("ghost", Some("`twr-test-no-such-agent` not found on PATH")),
        (
            "acp:/nonexistent/twr-agent",
            Some("`/nonexistent/twr-agent` not found"),
        ),
        ("acp:sh -c 'exit 0'", None),
        ("shell", None),
        ("codex", None),
    ] {
        let r = switch(&hub, runtime, None, quick()).await;
        assert_eq!(
            hub.orchestrator_thread().as_deref(),
            r["thread"].as_str(),
            "{runtime}: the switch happened"
        );
        match missing {
            Some(why) => {
                let w = r["warning"]
                    .as_str()
                    .unwrap_or_else(|| panic!("{runtime}: {r}"));
                assert!(
                    w.contains(why) && w.contains("cannot start"),
                    "{runtime}: {w}"
                );
            }
            None => assert!(r["warning"].is_null(), "{runtime}: {r}"),
        }
        let reset = hub.reset_orchestrator(&tui()).await.unwrap();
        assert_eq!(
            reset["warning"], r["warning"],
            "reset checks the recorded command: {runtime}"
        );
        assert!(reset["handoff"].is_null());
    }
    hub.shutdown().await;
}

/// The same for the built-in runtimes: the program checked is the one the runtime's adapter
/// would start (`Adapter::program`: its `TWR_*_BIN` override, else its binary name; here set
/// on the adapter, so the test leaves the process environment alone), and the warning carries
/// the adapter's own install hint. The switch still happens; a program that is there gets no
/// warning.
#[tokio::test]
async fn a_switch_to_a_built_in_runtime_whose_cli_is_missing_warns() {
    use std::os::unix::fs::PermissionsExt;
    use twr_adapters::{codex::CodexAdapter, grok_build::GrokBuildAdapter};

    let env = Env::new();
    let codex_bin = env.path().join("bin").join("codex");
    let mut opts = env.opts(|_| {});
    let test_adapters = opts.adapters.clone();
    let bin = codex_bin.clone();
    opts.adapters = Arc::new(move |name: &str| -> Option<Arc<dyn Adapter>> {
        match name {
            "codex" => Some(Arc::new(CodexAdapter::with_bin(&bin))),
            "grok-build" => Some(Arc::new(GrokBuildAdapter::with_bin(
                "twr-test-no-such-grok",
            ))),
            _ => test_adapters(name),
        }
    });
    let hub = Hub::open(opts).await.unwrap();
    let shown = codex_bin.display().to_string();
    for (runtime, missing) in [
        (
            "codex",
            Some(format!(
                "`{shown}` not found: the new orchestrator cannot start until it is there \
                 (install Codex, or set TWR_CODEX_BIN)"
            )),
        ),
        (
            "grok-build",
            Some(
                "`twr-test-no-such-grok` not found on PATH: the new orchestrator cannot start \
                 until it is there (install Grok Build, or set TWR_GROK_BIN)"
                    .to_string(),
            ),
        ),
        ("codex", None),
    ] {
        if missing.is_none() {
            std::fs::create_dir_all(codex_bin.parent().unwrap()).unwrap();
            std::fs::write(&codex_bin, "#!/bin/sh\nexit 0\n").unwrap();
            std::fs::set_permissions(&codex_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let r = switch(&hub, runtime, None, quick()).await;
        assert_eq!(
            hub.orchestrator_thread().as_deref(),
            r["thread"].as_str(),
            "{runtime}: the switch happened"
        );
        assert_eq!(r["warning"].as_str(), missing.as_deref(), "{runtime}: {r}");
    }
    hub.shutdown().await;
}

/// `orchestrator.show` tells a client whether `[orchestrator] runtime` was set
/// (`configured.explicit`; the TUI's first-run choice) and, per runtime that can host the
/// orchestrator, whether its program is installed (`installed`; the same check as the
/// switch's warning). `runtimes` keeps its shape: a list of names.
#[tokio::test]
async fn show_says_whether_the_runtime_was_configured_and_what_is_installed() {
    use std::os::unix::fs::PermissionsExt;
    use twr_adapters::codex::CodexAdapter;

    let env = Env::new();
    let codex_bin = env.path().join("bin").join("codex");
    let opts = |explicit: bool| {
        let mut opts = env.opts(|c| {
            if !explicit {
                c.orchestrator.runtime = None;
                c.default_runtime = "codex".into();
            }
        });
        let test_adapters = opts.adapters.clone();
        let bin = codex_bin.clone();
        opts.adapters = Arc::new(move |name: &str| -> Option<Arc<dyn Adapter>> {
            match name {
                "codex" => Some(Arc::new(CodexAdapter::with_bin(&bin))),
                _ => test_adapters(name),
            }
        });
        opts
    };
    let hub = Hub::open(opts(true)).await.unwrap();
    let o = hub.orchestrator_info();
    assert_eq!(o["configured"]["explicit"], json!(true), "{o}");
    assert_eq!(o["configured"]["runtime"], json!("claude-code"), "{o}");
    assert_eq!(
        o["runtimes"],
        json!(["claude-code", "codex", "grok-build"]),
        "{o}"
    );
    // The test runtimes have no known program: installed. Codex's is missing.
    assert_eq!(
        o["installed"],
        json!({"claude-code": true, "codex": false, "grok-build": true}),
        "{o}"
    );
    assert!(o["thread"].is_null() && o["retired"] == json!(0), "{o}");
    hub.shutdown().await;
    drop(hub);

    std::fs::create_dir_all(codex_bin.parent().unwrap()).unwrap();
    std::fs::write(&codex_bin, "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&codex_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let hub = {
        let mut last = None;
        timeout(WAIT, async {
            loop {
                match Hub::open(opts(false)).await {
                    Ok(h) => return h,
                    Err(e) => {
                        last = Some(format!("{e:#}"));
                        tokio::time::sleep(Duration::from_millis(20)).await
                    }
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("reopening the hub: {last:?}"))
    };
    let o = hub.orchestrator_info();
    // Unset: the default runtime is the orchestrator's, and the config did not choose it.
    assert_eq!(o["configured"]["explicit"], json!(false), "{o}");
    assert_eq!(o["configured"]["runtime"], json!("codex"), "{o}");
    assert_eq!(o["installed"]["codex"], json!(true), "{o}");
    // A choice is recorded in the log: an orchestrator thread exists from then on.
    switch(&hub, "codex", None, quick()).await;
    let o = hub.orchestrator_info();
    assert!(o["thread"].is_string(), "{o}");
    assert_eq!(o["runtime"], json!("codex"), "{o}");
    hub.shutdown().await;
}

/// Reset is a fresh session, even while busy: it drops queued prompts and notices but keeps
/// workers, their transcripts and attention. The archived conversation stays out of later
/// handoffs, including after a hub restart.
#[tokio::test]
async fn reset_keeps_workers_and_archives_the_conversation_without_handoff() {
    let mut env = Env::new();
    let hub = Hub::open(env.opts(|c| c.orchestrator.effort = Some("high".into())))
        .await
        .unwrap();
    let worker = env.worker(&hub, "api").await;
    let mut sub = hub.subscribe_control(0);
    hub.say_to_orchestrator("OLD_PRIVATE_CONVERSATION", None, &tui())
        .unwrap();
    let mut old = env.orch().await;
    let first = hub.orchestrator_thread().unwrap();
    let old_token = old.token();
    let (_, turn) = old.next_turn().await;
    old.reply(turn, "OLD_PRIVATE_REPLY");
    until(&mut sub, |e| e.thread == first && e.kind() == "turn.ended").await;
    hub.say_to_orchestrator("keep working", None, &tui())
        .unwrap();
    old.next_turn().await;
    hub.say_to_orchestrator("QUEUED_OLD_PROMPT", None, &tui())
        .unwrap();
    // Queue an urgent worker notice while the orchestrator is busy.
    hub.append(
        &worker,
        None,
        Source::Hub,
        EventBody::AttentionRaised(AttentionRaised {
            attn_id: "worker-review".into(),
            reason: AttentionReason::Review,
            summary: "Keep this worker review".into(),
            ..Default::default()
        }),
        None,
    )
    .unwrap();
    hub.append(
        &worker,
        None,
        Source::Hub,
        EventBody::ReportReady(ReportReady {
            summary: "UNDELIVERED_WORKER_REPORT".into(),
            grade: Grade::NeedsVerification,
            written_by: "hub".into(),
            ..Default::default()
        }),
        None,
    )
    .unwrap();
    hub.append(
        &first,
        None,
        Source::Hub,
        EventBody::AttentionRaised(AttentionRaised {
            attn_id: "old-orch-question".into(),
            reason: AttentionReason::Question,
            summary: "A stale orchestrator question".into(),
            ..Default::default()
        }),
        None,
    )
    .unwrap();
    let worker_events = events_of(&hub, &worker);
    let worker_view = hub.thread_view(&worker).unwrap();
    let r = hub.reset_orchestrator(&tui()).await.unwrap();
    let fresh = r["thread"].as_str().unwrap().to_string();
    assert_ne!(fresh, first);
    assert!(r["handoff"].is_null(), "{r}");
    assert_eq!(r["previous"]["dropped_prompts"], 1);
    assert_eq!(r["runtime"], "claude-code");
    assert_eq!(r["model"], "haiku");
    assert_eq!(r["started"], true);
    assert!(hub.thread_view(&first).unwrap().retired);
    assert!(!hub.orchestrator_token_is(&old_token));
    old.wait_stopped().await;
    let mut new = env.orch().await;
    assert!(new.cfg.resume_session.is_none());
    assert_eq!(new.cfg.effort.as_deref(), Some("high"));
    let (notice, turn) = new.next_turn().await;
    assert!(notice.starts_with("[twr notice]"), "{notice}");
    assert!(
        notice.contains("worker-review") && notice.contains("UNDELIVERED_WORKER_REPORT"),
        "{notice}"
    );
    assert!(!notice.contains("OLD_PRIVATE") && !notice.contains("QUEUED_OLD_PROMPT"));
    new.end_turn(turn);
    until(&mut sub, |e| e.thread == fresh && e.kind() == "turn.ended").await;
    assert_eq!(events_of(&hub, &worker), worker_events);
    assert_eq!(hub.thread_view(&worker).unwrap(), worker_view);
    let items = hub.attention_list(true);
    assert!(items.iter().any(|i| i["attn_id"] == "worker-review"));
    assert!(!items.iter().any(|i| i["attn_id"] == "old-orch-question"));
    assert!(events_of(&hub, &first)
        .iter()
        .any(|e| matches!(&e.body, EventBody::AgentText(t) if t.text == "OLD_PRIVATE_REPLY")));
    hub.say_to_orchestrator("NEW_CONVERSATION", None, &tui())
        .unwrap();
    let (prompt, turn) = new.next_turn().await;
    assert!(prompt.contains("NEW_CONVERSATION"));
    assert!(!prompt.contains("OLD_PRIVATE"));
    new.reply(turn, "NEW_REPLY");
    until(&mut sub, |e| e.thread == fresh && e.kind() == "turn.ended").await;
    hub.shutdown().await;
    drop(sub);
    drop(hub);
    let hub = env.reopen_with(|_| {}).await;
    assert_eq!(hub.orchestrator_thread().as_deref(), Some(fresh.as_str()));
    let switched = switch(&hub, "codex", None, quick()).await;
    let id = switched["thread"].as_str().unwrap();
    let created = events_of(&hub, id)
        .into_iter()
        .find(|e| e.kind() == "thread.created")
        .unwrap();
    let EventBody::ThreadCreated(p) = created.body else {
        unreachable!()
    };
    let handoff = p.ext.unwrap()["twr"]["handoff_prompt"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(handoff.contains("NEW_CONVERSATION"), "{handoff}");
    assert!(!handoff.contains("OLD_PRIVATE"), "{handoff}");
    assert!(!handoff.contains("QUEUED_OLD_PROMPT"), "{handoff}");
    // Removing a retired reset session from the registry must not erase its boundary.
    hub.append(
        &fresh,
        None,
        Source::Hub,
        EventBody::ThreadDeleted(ThreadDeleted {
            name: "orchestrator".into(),
            ..Default::default()
        }),
        None,
    )
    .unwrap();
    let switched = switch(&hub, "grok-build", None, quick()).await;
    assert_eq!(
        switched["handoff"]["exchanges"], 0,
        "archived history stays excluded"
    );
    hub.shutdown().await;
}

/// Both raised confirm cards and proposals still waiting for a card belong to the old
/// conversation. Reset withdraws them without applying their actions, durably.
#[tokio::test]
async fn reset_supersedes_confirm_cards_and_pending_proposals() {
    for raised in [false, true] {
        let mut env = Env::new();
        let hub = env.hub().await;
        let worker = env.worker(&hub, "api").await;
        let other = env.worker(&hub, "web").await;
        hub.append(
            &worker,
            None,
            Source::Hub,
            EventBody::AttentionRaised(AttentionRaised {
                attn_id: "worker-permission".into(),
                reason: AttentionReason::Permission,
                summary: "May I run this?".into(),
                options: vec!["allow".into(), "deny".into()],
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        let mut sub = hub.subscribe_control(0);
        hub.say_to_orchestrator(
            "allow api and explain the result afterwards",
            hub.info()["control_head"].as_u64(),
            &tui(),
        )
        .unwrap();
        let mut rt = env.orch().await;
        let (_, turn) = rt.next_turn().await;
        assert!(matches!(
            hub.respond_relay(
                "worker-permission",
                Answer::allow(),
                None,
                &Caller::Orchestrator
            )
            .unwrap(),
            twr_hub::Relay::NeedsConfirmation(_)
        ));
        let card = if raised {
            rt.end_turn(turn);
            let evs = until(&mut sub, |e| matches!(&e.body, EventBody::AttentionRaised(p) if is_confirm_card(&e.thread, &p.ext))).await;
            let EventBody::AttentionRaised(p) = &evs.last().unwrap().body else {
                unreachable!()
            };
            Some(p.attn_id.clone())
        } else {
            None
        };
        let orch_delete = hub
            .delete_threads(std::slice::from_ref(&worker), &Caller::Orchestrator)
            .await
            .unwrap();
        let user_delete = hub
            .delete_threads(std::slice::from_ref(&other), &tui())
            .await
            .unwrap();
        hub.reset_orchestrator(&tui()).await.unwrap();
        let items = hub.attention_list(true);
        assert!(!items.iter().any(|i| i["attn_id"] == orch_delete["attn_id"]));
        assert!(items.iter().any(|i| i["attn_id"] == user_delete["attn_id"]));
        assert!(hub.thread_view(&worker).is_ok() && hub.thread_view(&other).is_ok());
        if let Some(card) = card {
            let error = hub
                .respond(&card, Answer::new("Confirm"), "tui")
                .unwrap_err();
            assert_eq!(error.code, codes::CONFLICT);
            assert!(error.message.contains("superseded"), "{error:?}");
            assert!(!error.message.contains("newer message"), "{error:?}");
        }
        assert_eq!(hub.attention_list(true).len(), 2);
        assert!(hub
            .attention_list(true)
            .iter()
            .any(|i| i["attn_id"] == "worker-permission"));
        hub.shutdown().await;
        drop(sub);
        drop(hub);
        let hub = env.reopen_with(|_| {}).await;
        assert_eq!(
            hub.attention_list(true).len(),
            2,
            "no old pending card after replay"
        );
        hub.shutdown().await;
    }
}

#[tokio::test]
async fn reset_requires_detaching_but_can_replace_a_parked_orchestrator() {
    let mut env = Env::new();
    let hub = env.hub().await;
    hub.say_to_orchestrator("before attaching", None, &tui())
        .unwrap();
    let mut rt = env.orch().await;
    let old = hub.orchestrator_thread().unwrap();
    rt.next_turn().await;
    let grant = timeout(WAIT, async {
        loop {
            match hub.attach_begin(&old, true, "test").await {
                Err(e) if e.message.contains("no runtime session") => {
                    tokio::task::yield_now().await
                }
                result => break result.unwrap(),
            }
        }
    })
    .await
    .unwrap();
    let error = hub.reset_orchestrator(&tui()).await.unwrap_err();
    assert_eq!(error.code, codes::CONFLICT);
    assert!(error.message.contains("attached"));
    assert_eq!(hub.orchestrator_thread().as_deref(), Some(old.as_str()));
    hub.attach_end(&old, "test", grant.lease, None).unwrap();
    hub.park_thread(&old, true, true, &tui()).await.unwrap();
    assert!(hub.thread_view(&old).unwrap().parked);
    let reset = hub.reset_orchestrator(&tui()).await.unwrap();
    let fresh = reset["thread"].as_str().unwrap();
    assert_ne!(fresh, old);
    assert_eq!(reset["started"], false);
    assert!(!hub.thread_view(fresh).unwrap().parked);
    assert!(reset["handoff"].is_null());
    hub.send_by(fresh, "after parking", &tui()).unwrap();
    let mut fresh_rt = env.orch().await;
    assert!(fresh_rt.cfg.resume_session.is_none());
    assert_eq!(fresh_rt.next_turn().await.0, "after parking");
    hub.shutdown().await;
}

#[tokio::test]
async fn reset_is_user_only_and_preserves_a_recorded_acp_agent() {
    let mut env = Env::new();
    let hub = env.hub().await;
    assert_eq!(
        hub.reset_orchestrator(&Caller::Orchestrator)
            .await
            .unwrap_err()
            .code,
        codes::UNAUTHORIZED
    );
    assert!(hub.orchestrator_thread().is_none());
    let first = hub.reset_orchestrator(&tui()).await.unwrap();
    assert_eq!(first["started"], false);
    assert!(env.no_spawn());
    let chosen = switch(&hub, "acp:opencode acp", Some("a-model"), quick()).await;
    let old = chosen["thread"].as_str().unwrap();
    let command = hub.thread_view(old).unwrap().acp_command;
    let r = hub.reset_orchestrator(&tui()).await.unwrap();
    let id = r["thread"].as_str().unwrap();
    assert_eq!(r["runtime"], "acp");
    assert_eq!(r["model"], "a-model");
    assert_eq!(hub.thread_view(id).unwrap().acp_command, command);
    assert!(r["handoff"].is_null());
    assert!(!events_of(&hub, id)
        .iter()
        .any(|e| e.kind() == "turn.started"));
    hub.say_to_orchestrator("fresh ACP prompt", None, &tui())
        .unwrap();
    let mut rt = env.orch().await;
    assert!(rt.cfg.resume_session.is_none());
    assert!(rt.next_turn().await.0.contains("fresh ACP prompt"));
    hub.shutdown().await;
}
