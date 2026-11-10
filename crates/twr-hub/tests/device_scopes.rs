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
        let dir = tempfile::tempdir().unwrap();
        // The test's project: a repository of its own, beside the hub's data (a device without
        // admin starts threads only where the orchestrator may).
        init_repo(&dir.path().join("repo"));
        Env { dir, spawns, tx }
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
        self.path().join("repo").to_string_lossy().into_owned()
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

/// Makes `dir` a repository with one commit.
fn init_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    for args in [
        &["init", "-q"][..],
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "init",
        ][..],
    ] {
        let st = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap();
        assert!(st.success(), "git {args:?}");
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

/// Device records the hub cannot read (damaged, or a file twr will not trust) grant a device
/// nothing, its admin and terminal included, and say so as an error to try again, never as a
/// scope it lacks; the local user is not affected, and once they read again the device's
/// scopes hold as before. A hub with no device records at all serves the local user.
#[tokio::test]
async fn unreadable_device_records_refuse_a_device_and_never_grant() {
    let env = Env::new();
    let hub = env.hub().await;
    // No records yet: the local user names anything.
    env.new_on(&hub, Some("pty:/bin/echo hi"), &tui())
        .await
        .unwrap();
    let d = env.devices();
    let api = env.worker(&hub, "api").await;
    let records = env
        .config_file()
        .with_file_name("mobile")
        .join("devices.json");
    let good = std::fs::read(&records).unwrap();
    let refused = |r: Result<_, RpcError>| {
        let e: RpcError = r.map(|_: Value| ()).unwrap_err();
        assert_eq!(e.code, codes::INTERNAL, "{e:?}");
        assert!(e.message.contains("Try again"), "{e:?}");
    };
    let damaged = |path: &Path| std::fs::write(path, b"{\"devices\": [").unwrap();
    let readable_by_others = |path: &Path| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
    };
    for spoil in [damaged, readable_by_others] {
        std::fs::write(&records, &good).unwrap();
        spoil(&records);
        for device in [&d.terminal, &d.admin] {
            refused(env.new_on(&hub, Some("pty:/bin/echo hi"), device).await);
        }
        let to = ModelRequest {
            runtime: Some("claude-code"),
            ..Default::default()
        };
        refused(
            hub.set_thread_model(&api, to, &d.admin)
                .await
                .map(|_| Value::Null),
        );
        assert_eq!(hub.thread_view(&api).unwrap().runtime, "fake");
        env.new_on(&hub, Some("pty:/bin/echo hi"), &tui())
            .await
            .unwrap();
        std::fs::remove_file(&records).unwrap();
    }
    std::fs::write(&records, &good).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&records, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    env.new_on(&hub, Some("pty:/bin/echo hi"), &d.terminal)
        .await
        .unwrap();
    unauthorized(
        env.new_on(&hub, Some("pty:/bin/echo hi"), &d.operate).await,
        "terminal",
    );
    hub.shutdown().await;
}

/// A thread in `dir` with no worktree of its own.
fn in_dir(dir: &Path) -> NewThread {
    NewThread {
        project: dir.to_string_lossy().into_owned(),
        // The default runtime (`fake`): one a device may name.
        no_worktree: true,
        ..Default::default()
    }
}

/// `thread.spawn` of `req` by `caller`, waited for: its result, or its error.
async fn spawned(hub: &Hub, id: &str, req: NewThread, caller: &Caller) -> Result<Value, RpcError> {
    let status = hub.begin_spawn(id, req, None, caller)?;
    let status = if status["status"] == "pending" {
        timeout(WAIT, async {
            loop {
                let s = hub.spawn_status(id, caller).unwrap();
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
    if status["status"] == "error" {
        let e = &status["error"];
        return Err(RpcError::new(
            e["code"].as_str().unwrap_or(""),
            e["message"].as_str().unwrap_or(""),
        ));
    }
    Ok(status)
}

/// A paired device without `admin` starts a thread in a configured project, or in a folder or
/// git repository that is not, and does not hold, the home folder, and is outside its private
/// folders and twr's own data. Directly (`thread.new`), in the background (`thread.spawn`) and
/// through a typed `new <dir>: ...` in `say`, the home folder, a folder that holds it, a
/// repository or a plain folder in the hub's data are refused (`unauthorized`, saying what to
/// do instead), operate's and terminal's alike; a plain folder elsewhere is not. A device
/// with `admin`, and the local user, start them all. The orchestrator, acting on any message,
/// is refused a folder outside git as before.
#[tokio::test]
async fn a_device_without_admin_starts_threads_only_outside_the_home_and_data_folders() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let d = env.devices();
    let plain = env.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let in_data = env.path().join("data").join("inner");
    init_repo(&in_data);
    let plain_in_data = env.path().join("data").join("plain");
    std::fs::create_dir_all(&plain_in_data).unwrap();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut barred = vec![in_data.clone(), plain_in_data.clone()];
    barred.extend(
        home.as_deref()
            .and_then(Path::parent)
            .map(Path::to_path_buf),
    );
    barred.extend(home.clone());
    let refused = |r: Result<Value, RpcError>, dir: &Path| {
        let e = r.unwrap_err();
        assert_eq!(e.code, codes::UNAUTHORIZED, "{}: {e:?}", dir.display());
        assert!(
            e.message.contains("without the admin permission")
                && e.message.contains("--allow-admin"),
            "{e:?}"
        );
    };
    let mut n = 0;
    for device in [&d.operate, &d.terminal] {
        for dir in &barred {
            refused(hub.new_thread_by(in_dir(dir), device).await, dir);
            n += 1;
            refused(
                spawned(&hub, &format!("op-{n}"), in_dir(dir), device).await,
                dir,
            );
            let said = format!("new {}: hello", dir.display());
            refused(hub.say(&said, None, device).await, dir);
        }
        // Where it may: the test's repository, and a plain folder clear of those.
        hub.new_thread_by(in_dir(Path::new(&env.project())), device)
            .await
            .unwrap();
        hub.new_thread_by(in_dir(&plain), device).await.unwrap();
        n += 1;
        spawned(&hub, &format!("op-{n}"), in_dir(&plain), device)
            .await
            .unwrap();
        let said = format!("new {}: hello", plain.display());
        let r = hub.say(&said, None, device).await.unwrap();
        assert_eq!(r["action"], "new_thread", "{r}");
    }
    // A path that names no folder is no thread's, whatever it would resolve to.
    let missing = env.path().join("missing").join("..").join("plain");
    hub.new_thread_by(in_dir(&missing), &d.operate)
        .await
        .unwrap_err();
    // Nothing started in a barred folder.
    for dir in &barred {
        assert!(
            hub.list_threads()
                .iter()
                .all(|t| t["worktree"].as_str() != Some(&*dir.to_string_lossy())),
            "{}",
            dir.display()
        );
    }
    // `admin`, and the local user, are not held to it (not in the home folder here: the
    // thread would be real).
    for caller in [&d.admin, &tui()] {
        for dir in [&plain, &in_data] {
            hub.new_thread_by(in_dir(dir), caller).await.unwrap();
            n += 1;
            spawned(&hub, &format!("op-{n}"), in_dir(dir), caller)
                .await
                .unwrap();
        }
        let said = format!("new {}: hello", plain.display());
        let r = hub.say(&said, None, caller).await.unwrap();
        assert_eq!(r["action"], "new_thread", "{r}");
    }
    // The orchestrator, acting on a device's message or the user's, as before.
    hub.say_to_orchestrator("watch", None, &tui()).unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    rt.end_turn(t1, "Watching.");
    for who in [&d.operate, &d.admin] {
        let seen = hub.control_read(0, 1000).unwrap().1;
        let r = hub
            .say_to_orchestrator(
                &format!("start a thread in {}", plain.display()),
                Some(seen),
                who,
            )
            .unwrap();
        assert_eq!(r["action"], "delegate", "{r}");
        let (_, t) = rt.next_turn().await;
        let e = hub
            .new_thread_by(
                NewThread {
                    // (Not `fake`, which the orchestrator never starts.)
                    runtime: Some("my-agent".into()),
                    ..in_dir(&plain)
                },
                &Caller::Orchestrator,
            )
            .await
            .unwrap_err();
        assert_eq!(e.code, codes::BAD_REQUEST, "{e:?}");
        assert!(
            e.message.contains("the orchestrator starts threads only"),
            "{e:?}"
        );
        rt.end_turn(t, "Done.");
    }
    drop(rt);
    hub.shutdown().await;
}
