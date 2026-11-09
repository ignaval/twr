//! Hub tests: what a paired device may do past its methods' scopes (`hub::device_scope`).
//!
//! A device's connection is the user's, as `device:<id>`; the hub reads its grant from the
//! gateway's device records (`mobile/devices.json` beside the config file) where it decides.
//! A command line runtime needs `terminal`; a mode change, a runtime move and `outside_pin`
//! need `admin`, whether the device asks itself, through a short command in `say`, through
//! the orchestrator relaying its message, or by confirming a card. The local user is never
//! limited.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::timeout;
use twr_adapters::fake::FakeAdapter;
use twr_core::adapter::{Adapter, AdapterCmd, AdapterEnd, AdapterHandle, AdapterInfo, Answer};
use twr_core::adapter::{SpawnConfig, Tier};
use twr_core::config::{Config, RuntimePreset};
use twr_core::event::*;
use twr_core::proto::{codes, RpcError};
use twr_hub::hub::{ControlSubscription, ModelRequest};
use twr_hub::{Caller, Hub, HubOptions, NewThread, SwitchOptions};

const WAIT: Duration = Duration::from_secs(10);

/// One orchestrator runtime session handed to the test.
struct OrchRt {
    end: AdapterEnd,
    first: Option<(String, u64)>,
}

impl OrchRt {
    async fn next_turn(&mut self) -> (String, u64) {
        if let Some(f) = self.first.take() {
            return f;
        }
        timeout(WAIT, async {
            loop {
                match self.end.cmds.recv().await {
                    Some(AdapterCmd::Send { text, turn }) => return (text, turn),
                    Some(_) => continue,
                    None => panic!("the hub dropped the orchestrator runtime"),
                }
            }
        })
        .await
        .expect("timed out waiting for an orchestrator turn")
    }

    fn end_turn(&self, turn: u64, reply: &str) {
        self.end.emit(
            EventBody::AgentText(AgentText {
                text: reply.into(),
                is_final: true,
                ..Default::default()
            }),
            Some(turn),
        );
        self.end.emit(
            EventBody::TurnEnded(TurnEnded {
                result: TurnResult::Completed,
                ..Default::default()
            }),
            Some(turn),
        );
    }
}

/// `claude-code` for the tests: every session goes to the test through a channel.
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
        let (h, end) = AdapterHandle::pair();
        end.emit(
            EventBody::ThreadStarted(SessionPayload {
                session_id: Some("s-orch".into()),
                ..Default::default()
            }),
            None,
        );
        let first = prompt.map(|p| (p, cfg.prompt_turn.unwrap_or(1)));
        let _ = self.0.send(OrchRt { end, first });
        Ok(h)
    }
}

struct Env {
    dir: tempfile::TempDir,
    spawns: mpsc::UnboundedReceiver<OrchRt>,
    tx: mpsc::UnboundedSender<OrchRt>,
}

/// The paired devices of a test: their callers (`device:<id>`).
struct Devices {
    operate: Caller,
    terminal: Caller,
    admin: Caller,
}

impl Env {
    fn new() -> Env {
        let (tx, spawns) = mpsc::unbounded_channel();
        Env {
            dir: tempfile::tempdir().unwrap(),
            spawns,
            tx,
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn config_file(&self) -> PathBuf {
        self.path().join("config.toml")
    }

    fn opts(&self) -> HubOptions {
        let mut config = Config {
            default_runtime: "fake".into(),
            ..Default::default()
        };
        config.orchestrator.runtime = Some("claude-code".into());
        config.orchestrator.cwd = Some(self.path().to_path_buf());
        config.runtimes.insert(
            "my-agent".into(),
            RuntimePreset {
                acp: "/bin/true --acp".into(),
                attach: None,
            },
        );
        let mut o = HubOptions::new(self.path().join("data").join("twr.db"), config);
        let orch: Arc<dyn Adapter> = Arc::new(OrchAdapter(self.tx.clone()));
        o.adapters = Arc::new(move |name: &str| -> Option<Arc<dyn Adapter>> {
            match name {
                "fake" => Some(Arc::new(FakeAdapter::new())),
                "claude-code" => Some(Arc::clone(&orch)),
                "pty" | "acp" => twr_adapters::find(name),
                _ => None,
            }
        });
        o.socket = Some(self.path().join("hub.sock"));
        o.twr_exe = Some(PathBuf::from("/opt/twr/bin/twr"));
        o.claude_policy = twr_adapters::claude_code::guard::PolicySources {
            managed_dir: Some(self.path().join("claude-managed")),
            config_dir: Some(self.path().join("claude-config")),
        };
        o.config_file = Some(self.config_file());
        o.stop_timeout = Duration::from_secs(5);
        o
    }

    async fn hub(&self) -> Hub {
        Hub::open(self.opts()).await.unwrap()
    }

    /// Pairs a device with `scopes` through the gateway's records, as `twr-mobile-gateway
    /// --pair` does. An invitation grants `admin` only with `terminal`, so a device with
    /// `admin` alone is one paired before that rule: paired with both, then `terminal` taken
    /// out of its record.
    fn pair(&self, scopes: &[&str]) -> Caller {
        let dir = self.config_file().with_file_name("mobile");
        let auth = twr_core::devices::Auth::open(&dir).unwrap();
        let legacy = scopes.contains(&"admin") && !scopes.contains(&"terminal");
        let mut granted: Vec<String> = scopes.iter().map(|s| s.to_string()).collect();
        if legacy {
            granted.push("terminal".into());
        }
        let code = auth.begin(granted).unwrap();
        let (device, _) = auth.pair(&code, "phone").unwrap();
        if legacy {
            let path = dir.join("devices.json");
            let mut db: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            let record = db["devices"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|d| d["id"] == device.id.as_str())
                .unwrap();
            record["scopes"] = serde_json::json!(scopes);
            std::fs::write(&path, serde_json::to_vec(&db).unwrap()).unwrap();
        }
        Caller::user(format!("device:{}", device.id))
    }

    fn devices(&self) -> Devices {
        Devices {
            operate: self.pair(&["read", "operate"]),
            terminal: self.pair(&["read", "operate", "terminal"]),
            admin: self.pair(&["read", "operate", "admin"]),
        }
    }

    async fn orch(&mut self) -> OrchRt {
        timeout(WAIT, self.spawns.recv())
            .await
            .expect("timed out waiting for the orchestrator to spawn")
            .unwrap()
    }

    fn project(&self) -> String {
        self.path().to_string_lossy().into_owned()
    }

    async fn worker(&self, hub: &Hub, name: &str) -> String {
        let v = hub
            .new_thread(
                NewThread {
                    project: self.project(),
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

    /// `thread.new` in the test's project with `runtime` as `caller`.
    async fn new_on(
        &self,
        hub: &Hub,
        runtime: Option<&str>,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        hub.new_thread_by(
            NewThread {
                project: self.project(),
                runtime: runtime.map(str::to_string),
                no_worktree: true,
                ..Default::default()
            },
            caller,
        )
        .await
    }
}

fn tui() -> Caller {
    Caller::user("tui")
}

fn client(caller: &Caller) -> &str {
    match caller {
        Caller::User(c) => c,
        Caller::Orchestrator => unreachable!(),
    }
}

#[track_caller]
fn unauthorized<T: std::fmt::Debug>(r: Result<T, RpcError>, needs: &str) -> String {
    let e = r.unwrap_err();
    assert_eq!(e.code, codes::UNAUTHORIZED, "{e:?}");
    assert!(e.message.contains(needs), "{e:?}");
    e.message
}

#[track_caller]
fn not_unauthorized<T: std::fmt::Debug>(r: Result<T, RpcError>) {
    if let Err(e) = r {
        assert_ne!(e.code, codes::UNAUTHORIZED, "{e:?}");
        assert_ne!(e.code, codes::RELAY_FORBIDDEN, "{e:?}");
    }
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
    .expect("timed out");
    seen
}

fn raised(e: &Event) -> Option<&AttentionRaised> {
    match &e.body {
        EventBody::AttentionRaised(p) => Some(p),
        _ => None,
    }
}

/// `pty:` and `acp:` runtimes run a command line of the caller's choosing: a device names one
/// only with `terminal` (`admin` is not enough); without it, only an agent runtime or a
/// configured preset, for a new thread (`thread.new`, `thread.spawn`), a move
/// (`thread.set_model`) or the orchestrator (`orchestrator.use`). The local user names any.
#[tokio::test]
async fn a_device_names_a_command_line_runtime_only_with_terminal() {
    let env = Env::new();
    let hub = env.hub().await;
    let d = env.devices();
    for runtime in ["pty:/bin/echo hi", "acp:/bin/true --acp", "pty", "acp"] {
        for device in [&d.operate, &d.admin] {
            unauthorized(env.new_on(&hub, Some(runtime), device).await, "terminal");
        }
    }
    // Spawned in the background: the refusal is the operation's result.
    let status = hub
        .begin_spawn(
            "op-1",
            NewThread {
                project: env.project(),
                runtime: Some("pty:/bin/sh -c id".into()),
                no_worktree: true,
                ..Default::default()
            },
            Some("hi".into()),
            &d.operate,
        )
        .unwrap();
    let status = if status["status"] == "pending" {
        timeout(WAIT, async {
            loop {
                let s = hub.spawn_status("op-1", &d.operate).unwrap();
                if s["status"] != "pending" {
                    return s;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap()
    } else {
        status
    };
    assert_eq!(status["status"], "error", "{status}");
    assert_eq!(status["error"]["code"], codes::UNAUTHORIZED, "{status}");
    // What the orchestrator may choose, a device may.
    env.new_on(&hub, None, &d.operate).await.unwrap();
    env.new_on(&hub, Some("my-agent"), &d.operate)
        .await
        .unwrap();
    // `terminal`, and the local user, name a command line.
    env.new_on(&hub, Some("pty:/bin/echo hi"), &d.terminal)
        .await
        .unwrap();
    env.new_on(&hub, Some("pty:/bin/echo hi"), &tui())
        .await
        .unwrap();
    // A move: to a command line needs terminal (and, as any move, admin).
    let api = env.worker(&hub, "api").await;
    let to = |runtime: &'static str| ModelRequest {
        runtime: Some(runtime),
        ..Default::default()
    };
    for device in [&d.operate, &d.admin] {
        unauthorized(
            hub.set_thread_model(&api, to("acp:/bin/true --acp"), device)
                .await,
            "terminal",
        );
    }
    unauthorized(
        hub.set_thread_model(&api, to("acp:/bin/true --acp"), &d.terminal)
            .await,
        "admin",
    );
    // The orchestrator's runtime: `orchestrator.use` is admin's, a command line terminal's.
    unauthorized(
        hub.use_orchestrator(
            "acp:/bin/true --acp",
            None,
            SwitchOptions::no_summary(),
            &d.admin,
        )
        .await,
        "terminal",
    );
    assert_eq!(hub.thread_view(&api).unwrap().runtime, "fake");
    hub.shutdown().await;
}

/// A device moves a thread to another runtime, starts one in a mode that doesn't ask, or goes
/// outside an account pin only with `admin`; a move between accounts and a restricted mode are
/// operate's. The local user does all of it.
#[tokio::test]
async fn moves_wide_modes_and_outside_pin_need_admin_on_a_device() {
    let env = Env::new();
    let hub = env.hub().await;
    let d = env.devices();
    let api = env.worker(&hub, "api").await;
    let to_claude = ModelRequest {
        runtime: Some("claude-code"),
        ..Default::default()
    };
    unauthorized(
        hub.set_thread_model(&api, to_claude, &d.operate).await,
        "admin",
    );
    unauthorized(
        hub.set_thread_model(&api, to_claude, &d.terminal).await,
        "admin",
    );
    assert_eq!(hub.thread_view(&api).unwrap().runtime, "fake");
    hub.set_thread_model(&api, to_claude, &d.admin)
        .await
        .unwrap();
    assert_eq!(hub.thread_view(&api).unwrap().runtime, "claude-code");
    // A model change alone is no move.
    let web = env.worker(&hub, "web").await;
    hub.set_runtime_model(&web, None, Some("fake-big"), None, &d.operate)
        .unwrap();
    let outside = ModelRequest {
        account: Some("work"),
        outside_pin: true,
        ..Default::default()
    };
    unauthorized(
        hub.set_thread_model(&web, outside, &d.operate).await,
        "admin",
    );
    // Admin gets past the scope (to the account check: this hub has no `work`).
    not_unauthorized(hub.set_thread_model(&web, outside, &d.admin).await);
    let new = |mode: Option<&str>, outside_pin: bool| NewThread {
        project: env.project(),
        mode: mode.map(str::to_string),
        account: outside_pin.then(|| "work".to_string()),
        outside_pin,
        no_worktree: true,
        ..Default::default()
    };
    for mode in ["auto", "Auto", "bypassPermissions"] {
        unauthorized(
            hub.new_thread_by(new(Some(mode), false), &d.operate).await,
            "admin",
        );
    }
    hub.new_thread_by(new(Some("plan"), false), &d.operate)
        .await
        .unwrap();
    hub.new_thread_by(new(Some("auto"), false), &d.admin)
        .await
        .unwrap();
    hub.new_thread_by(new(Some("auto"), false), &tui())
        .await
        .unwrap();
    unauthorized(
        hub.new_thread_by(new(None, true), &d.operate).await,
        "admin",
    );
    not_unauthorized(hub.new_thread_by(new(None, true), &d.admin).await);
    // `thread.set_mode` itself.
    unauthorized(hub.set_mode(&web, "plan", &d.operate), "admin");
    hub.set_mode(&web, "plan", &d.admin).unwrap();
    hub.set_mode(&web, "auto", &tui()).unwrap();
    hub.shutdown().await;
}

/// A short mode command in a device's `say` is refused whole without `admin`: nothing is
/// applied, recorded or sent to the orchestrator. An answer alone is operate's. Admin devices
/// and the local user have it applied.
#[tokio::test]
async fn a_devices_short_mode_command_needs_admin() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let d = env.devices();
    let web = env.worker(&hub, "web").await;
    hub.say_to_orchestrator("watch", None, &tui()).unwrap();
    let mut rt = env.orch().await;
    let (_, t) = rt.next_turn().await;
    let orch = hub.orchestrator_thread().unwrap();
    rt.end_turn(t, "Watching.");
    let mut sub = hub.subscribe_control(0);
    until(&mut sub, |e| e.kind() == "turn.ended" && e.thread == orch).await;
    let seen = hub.control_read(0, 1000).unwrap().1;
    let events = hub.control_read(0, 10_000).unwrap().0.len();
    for device in [&d.operate, &d.terminal] {
        let m = unauthorized(
            hub.say_to_orchestrator("set web to plan mode", Some(seen), device),
            "admin",
        );
        assert!(m.contains("web"), "{m}");
    }
    assert_eq!(hub.thread_view(&web).unwrap().mode, "auto");
    assert_eq!(hub.control_read(0, 10_000).unwrap().0.len(), events);
    assert_eq!(hub.thread_view(&orch).unwrap().turns, 1);
    let r = hub
        .say_to_orchestrator("set web to plan mode", Some(seen), &d.admin)
        .unwrap();
    assert_eq!(r["action"], "command", "{r}");
    assert_eq!(hub.thread_view(&web).unwrap().mode, "plan");
    let seen = hub.control_read(0, 1000).unwrap().1;
    let r = hub
        .say_to_orchestrator("set web to auto mode", Some(seen), &tui())
        .unwrap();
    assert_eq!(r["action"], "command", "{r}");
    assert_eq!(hub.thread_view(&web).unwrap().mode, "auto");
    drop(rt);
    hub.shutdown().await;
}

/// The orchestrator relays no mode change and no runtime move from a turn a device without
/// `admin` started (`relay_forbidden`), directly or as a proposal; a turn of an admin device's
/// or the local user's message is judged by the rest of the gate.
#[tokio::test]
async fn the_orchestrator_relays_no_mode_or_move_from_a_device_without_admin() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let d = env.devices();
    let web = env.worker(&hub, "web").await;
    hub.say_to_orchestrator("watch", None, &tui()).unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    rt.end_turn(t1, "Watching.");
    let ask = "web looks stuck, please put it in bypass mode and move it to claude code";
    for (who, allowed) in [(&d.operate, false), (&d.admin, true), (&tui(), true)] {
        let seen = hub.control_read(0, 1000).unwrap().1;
        let r = hub.say_to_orchestrator(ask, Some(seen), who).unwrap();
        assert_eq!(r["action"], "delegate", "{r}");
        let (_, t) = rt.next_turn().await;
        let mode = hub.set_mode(&web, "bypassPermissions", &Caller::Orchestrator);
        let moved =
            hub.set_runtime_model(&web, Some("claude-code"), None, None, &Caller::Orchestrator);
        if allowed {
            not_unauthorized(mode);
            not_unauthorized(moved);
        } else {
            for r in [mode, moved] {
                let e = r.unwrap_err();
                assert_eq!(e.code, codes::RELAY_FORBIDDEN, "{e:?}");
                assert!(e.message.contains("admin"), "{e:?}");
            }
            assert_eq!(hub.thread_view(&web).unwrap().mode, "auto");
            assert_eq!(hub.thread_view(&web).unwrap().runtime, "fake");
        }
        rt.end_turn(t, "Done.");
    }
    drop(rt);
    hub.shutdown().await;
}

/// A confirm card that changes a mode or moves a thread is confirmed from a device only with
/// `admin`: a device without it is refused, and the card stays open for the local user or an
/// admin device.
#[tokio::test]
async fn a_device_without_admin_cannot_confirm_a_mode_or_move_card() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let d = env.devices();
    let web = env.worker(&hub, "web").await;
    let mut sub = hub.subscribe_control(0);
    hub.say_to_orchestrator("watch", None, &tui()).unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    rt.end_turn(t1, "Watching.");
    let seen = hub.control_read(0, 1000).unwrap().1;
    let r = hub
        .say_to_orchestrator("move web to claude code", Some(seen), &tui())
        .unwrap();
    assert_eq!(r["action"], "delegate", "{r}");
    let (_, t2) = rt.next_turn().await;
    let r = hub
        .set_runtime_model(&web, Some("claude-code"), None, None, &Caller::Orchestrator)
        .unwrap();
    assert_eq!(r["status"], "needs_confirmation", "{r}");
    rt.end_turn(t2, "It's on a card.");
    let evs = until(&mut sub, |e| {
        raised(e).is_some_and(|r| is_confirm_card(&e.thread, &r.ext))
    })
    .await;
    let attn = raised(evs.last().unwrap()).unwrap().attn_id.clone();
    for device in [&d.operate, &d.terminal] {
        unauthorized(
            hub.respond(&attn, Answer::new("Confirm"), client(device)),
            "admin",
        );
    }
    assert!(hub
        .attention_list(false)
        .iter()
        .any(|a| a["attn_id"] == json!(attn)));
    assert_eq!(hub.thread_view(&web).unwrap().runtime, "fake");
    hub.respond(&attn, Answer::new("Confirm"), client(&d.admin))
        .unwrap();
    assert_eq!(hub.thread_view(&web).unwrap().runtime, "claude-code");
    drop(rt);
    hub.shutdown().await;
}
