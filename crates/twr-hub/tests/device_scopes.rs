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

use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::timeout;
use twr_adapters::fake::FakeAdapter;
use twr_core::adapter::{Adapter, AdapterEnd, AdapterHandle, AdapterInfo};
use twr_core::adapter::{SpawnConfig, Tier};
use twr_core::config::{Config, RuntimePreset};
use twr_core::event::*;
use twr_core::proto::{codes, RpcError};
use twr_hub::hub::ModelRequest;
use twr_hub::{Caller, Hub, HubOptions, NewThread, SwitchOptions};

const WAIT: Duration = Duration::from_secs(10);

/// One orchestrator runtime session handed to the test (kept open while the hub runs).
struct OrchRt {
    _end: AdapterEnd,
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
        let _ = (cfg, prompt);
        let _ = self.0.send(OrchRt { _end: end });
        Ok(h)
    }
}

struct Env {
    dir: tempfile::TempDir,
    _spawns: mpsc::UnboundedReceiver<OrchRt>,
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
            _spawns: spawns,
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
    /// --pair` does.
    fn pair(&self, scopes: &[&str]) -> Caller {
        let auth =
            twr_core::devices::Auth::open(&self.config_file().with_file_name("mobile")).unwrap();
        let code = auth
            .begin(scopes.iter().map(|s| s.to_string()).collect())
            .unwrap();
        let (device, _) = auth.pair(&code, "phone").unwrap();
        Caller::user(format!("device:{}", device.id))
    }

    fn devices(&self) -> Devices {
        Devices {
            operate: self.pair(&["read", "operate"]),
            terminal: self.pair(&["read", "operate", "terminal"]),
            admin: self.pair(&["read", "operate", "admin"]),
        }
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

#[track_caller]
fn unauthorized<T: std::fmt::Debug>(r: Result<T, RpcError>, needs: &str) -> String {
    let e = r.unwrap_err();
    assert_eq!(e.code, codes::UNAUTHORIZED, "{e:?}");
    assert!(e.message.contains(needs), "{e:?}");
    e.message
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
