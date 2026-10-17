//! SSH boxes, the hub side: a box daemon of the twr-runner library runs in this process and
//! stands in for the box; the hub reaches it as its ssh supervisor would (the daemon's socket,
//! the sealed channel with a hub key, a WebSocket inside it) and serves the connection with
//! `Hub::serve_sealed_runner`. Covered: a sealed box goes online and runs a fake runtime's
//! turn, and a restarted hub takes its session over through the same daemon; the sealed
//! handshake's refusals; the listener treating an SSH or a command box as unknown; boxes added
//! while the hub runs (`box.add`, `box.install`), with their refusals; `hub.info`'s box fields;
//! a command box added, dialed (through the test dialer), used and restarted.
//!
//! Every hub here dials its SSH boxes with `TWR_SSH=false` (set once, first thing): its
//! supervisor's probe fails at once, so the supervisor stays out of the way (and shows it).
//! Its ssh part is covered end to end by the CLI's tests, with a fake ssh.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::protocol::Role;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;
use twr_adapters::FakeAdapter;
use twr_core::adapter::Adapter;
use twr_core::config::{BoxConfig, BoxTransport, Config};
use twr_core::event::*;
use twr_core::proto::codes;
use twr_core::runner::{
    auth_proof, hub_errors, new_nonce, AgentInfo, AgentLogin, CmdResult, Hello, HubMsg, Inventory,
    Op, Prover, RunnerMsg, Secret, PROTOCOL_VERSION, SEALED_AUTH_PROOF,
};
use twr_core::sealed::{hub_side, HubKey};
use twr_hub::hub::{AgentsPoll, BoxView, SealedEnd};
use twr_hub::listener::{pre_auth_ws_config, RunnerListener};
use twr_hub::{Caller, Hub, HubOptions, NewThread};
use twr_runner::daemon::{request, Request};
use twr_runner::relay::DaemonPaths;
use twr_runner::{DaemonOptions, LocalBox, RunnerOptions};

const BOX: &str = "gpu";
const WAIT: Duration = Duration::from_secs(20);

// ------------------------------------------------------------------------------------------
// Harness
// ------------------------------------------------------------------------------------------

/// Logs, and `TWR_SSH=false` before any hub runs (see the module docs). Std's environment is
/// locked against its own process spawns, which are the only readers here.
fn init() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        std::env::set_var("TWR_SSH", "false");
        let _ = tracing_subscriber::fmt()
            .with_test_writer()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
            )
            .try_init();
    });
}

/// The built-in adapters plus the fake runtime.
fn with_fake() -> twr_hub::host::AdapterLookup {
    Arc::new(|name: &str| -> Option<Arc<dyn Adapter>> {
        match name {
            "fake" => Some(Arc::new(FakeAdapter::new())),
            other => twr_adapters::find(other),
        }
    })
}

fn ssh_box() -> BoxConfig {
    BoxConfig {
        transport: Some(BoxTransport::Ssh),
        ssh: Some("me@gpu.invalid".into()),
        ..Default::default()
    }
}

/// A command box whose program does not exist (its supervisor's probe fails at once), a
/// secret among its words.
fn command_box() -> BoxConfig {
    BoxConfig {
        transport: Some(BoxTransport::Command),
        command: Some(
            ["/nonexistent/twr-test-exec", "-i", "-e", "K=s3cret", "box"]
                .map(String::from)
                .to_vec(),
        ),
        ..Default::default()
    }
}

fn config() -> Config {
    let mut c = Config {
        default_runtime: "fake".into(),
        ..Default::default()
    };
    c.boxes.insert(BOX.into(), ssh_box());
    c
}

fn hub_opts(dir: &Path, cfg: Config) -> HubOptions {
    init();
    let mut o = HubOptions::new(dir.join("hub").join("twr.db"), cfg);
    o.adapters = with_fake();
    o.stop_timeout = Duration::from_secs(5);
    o.git_tick = Some(Duration::ZERO);
    o.stall_after = Some(Duration::ZERO);
    o
}

async fn wait_for(what: &str, cond: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !cond() {
        if tokio::time::Instant::now() > deadline {
            panic!("timed out waiting for {what}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Box `name`'s entry of `hub.info`.
fn box_info(hub: &Hub, name: &str) -> Value {
    hub.info()["boxes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["name"] == name)
        .cloned()
        .unwrap_or(Value::Null)
}

fn online(hub: &Hub, name: &str) -> bool {
    hub.boxes().iter().any(|(n, on, _)| n == name && *on)
}

/// A box daemon running in this process, as `twr runner --daemon` runs on a box.
struct Daemon {
    socket: PathBuf,
    lbox: LocalBox,
    stop: Option<oneshot::Sender<()>>,
}

impl Daemon {
    async fn start(dir: &Path, key: &HubKey, name: &str) -> Daemon {
        init();
        let paths = DaemonPaths {
            // In a directory of its own (the daemon makes it 0700, and refuses one others can
            // write).
            socket: dir.join("st/box-daemons/d.sock"),
            data_dir: dir.join("data"),
            lock: dir.join("st/locks/box.lock"),
            log: dir.join("data/daemon.log"),
        };
        let mut opts = RunnerOptions::for_daemon(
            name,
            &paths.data_dir,
            DaemonOptions {
                socket: paths.socket.clone(),
                hub_key: key.public(),
                lock: paths.lock.clone(),
                build: twr_core::build_id().map(str::to_string),
                launch: Some("detached".into()),
            },
        );
        opts.ping_interval = Duration::from_millis(500);
        opts.idle_timeout = Duration::from_secs(10);
        opts.welcome_timeout = Duration::from_secs(10);
        opts.shutdown_grace = Duration::from_secs(3);
        let mut o = opts.box_options();
        // No real login shell or agent CLI under a test.
        o.env = twr_runner::BoxEnv::none();
        o.adapters = with_fake();
        if !o.runtimes.iter().any(|r| r == "fake") {
            o.runtimes.push("fake".into());
        }
        twr_core::paths::ensure_private_dir(&paths.data_dir).unwrap();
        let lbox = LocalBox::open(o).unwrap();
        let (tx, rx) = oneshot::channel::<()>();
        let task = tokio::spawn(twr_runner::run_with(opts, lbox.clone(), async move {
            let _ = rx.await;
        }));
        let deadline = tokio::time::Instant::now() + WAIT;
        while request(&paths.socket, Request::Status).await.is_err() {
            assert!(!task.is_finished(), "the daemon ended: {:?}", task.await);
            assert!(
                tokio::time::Instant::now() < deadline,
                "the daemon's socket"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Daemon {
            socket: paths.socket,
            lbox,
            stop: Some(tx),
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
    }
}

/// Connects `hub` to the daemon on `socket` as its ssh supervisor does (the `connect`
/// request, the sealed channel opened with `key`, a WebSocket accepted inside it with the
/// small pre-authentication limits), and serves it in the background.
async fn connect(
    hub: &Hub,
    socket: &Path,
    key: &HubKey,
    name: &str,
) -> tokio::task::JoinHandle<SealedEnd> {
    let (stream, _) = request(socket, Request::Connect).await.unwrap();
    let (sealed, hello) = tokio::time::timeout(WAIT, hub_side(stream, key, name))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(hello.box_name, name);
    let ws = tokio_tungstenite::accept_async_with_config(sealed, Some(pre_auth_ws_config()))
        .await
        .unwrap();
    let (hub, name) = (hub.clone(), name.to_string());
    tokio::spawn(async move { hub.serve_sealed_runner(ws, &name).await })
}

/// Waits for `thread`'s event of `kind` in turn `turn`.
async fn until_turn_event(hub: &Hub, thread: &str, kind: &str, turn: u64) {
    let mut sub = hub.subscribe_thread(thread, 0).unwrap();
    tokio::time::timeout(WAIT, async {
        loop {
            let ev = sub.next().await.unwrap().expect("hub stopped");
            if ev.kind() == kind && ev.turn == Some(turn) {
                return;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {kind} of turn {turn}"));
}

fn count(hub: &Hub, thread: &str, kind: &str) -> usize {
    hub.thread_events(thread, 0, 100_000)
        .unwrap()
        .1
        .iter()
        .filter(|e| e.kind() == kind)
        .count()
}

/// A raw runner's end of a sealed connection served by `hub` for `name` (no channel: the
/// WebSocket over an in-memory pipe), and the hub's serving task.
async fn raw_sealed(
    hub: &Hub,
    name: &str,
) -> (
    WebSocketStream<tokio::io::DuplexStream>,
    impl std::future::Future<Output = SealedEnd>,
) {
    let (hub_end, runner_end) = tokio::io::duplex(1 << 20);
    let (hub, name) = (hub.clone(), name.to_string());
    let served = tokio::spawn(async move {
        let ws =
            WebSocketStream::from_raw_socket(hub_end, Role::Server, Some(pre_auth_ws_config()))
                .await;
        hub.serve_sealed_runner(ws, &name).await
    });
    let runner = WebSocketStream::from_raw_socket(runner_end, Role::Client, None).await;
    (runner, async move { served.await.unwrap() })
}

async fn send<S>(ws: &mut WebSocketStream<S>, m: &RunnerMsg)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    ws.send(Message::Text(serde_json::to_string(m).unwrap().into()))
        .await
        .unwrap();
}

/// The hub's next message, `None` once it closed.
async fn next<S>(ws: &mut WebSocketStream<S>) -> Option<HubMsg>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        match tokio::time::timeout(WAIT, ws.next())
            .await
            .expect("no message")
        {
            Some(Ok(Message::Text(t))) => match serde_json::from_str(t.as_str()).unwrap() {
                HubMsg::Ping | HubMsg::Pong => continue,
                m => return Some(m),
            },
            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return None,
            Some(Ok(_)) => continue,
        }
    }
}

fn hello(name: &str, protocol: u32) -> RunnerMsg {
    RunnerMsg::Hello(Hello {
        box_name: name.into(),
        protocol,
        nonce: new_nonce().unwrap(),
    })
}

fn assert_error(m: Option<HubMsg>, code: &str) -> String {
    match m {
        Some(HubMsg::Error { code: c, message }) => {
            assert_eq!(c, code, "{message}");
            message
        }
        other => panic!("expected error {code}, got {other:?}"),
    }
}

fn user() -> Caller {
    Caller::user("test")
}

// ------------------------------------------------------------------------------------------
// Tests
// ------------------------------------------------------------------------------------------

/// A sealed box goes online and a fake runtime's turn runs on it; `hub.info` says so. The hub
/// restarts: the runtime keeps running on the box, and the new hub, connecting to the same
/// daemon, takes its session over (no new runtime, no crash) and runs the next turn on it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sealed_box_runs_turns_and_a_restarted_hub_takes_its_session_over() {
    let dir = tempfile::tempdir().unwrap();
    let key = HubKey::generate().unwrap();
    let daemon = Daemon::start(&dir.path().join("box"), &key, BOX).await;
    let hub = Hub::open(hub_opts(dir.path(), config())).await.unwrap();
    // The supervisor's ssh fails here (`TWR_SSH=false`): the box is offline, and says why.
    let h = hub.clone();
    wait_for("the supervisor's failed probe", move || {
        let b = box_info(&h, BOX);
        b["state"] == "offline"
            && b["last_error"]
                .as_str()
                .is_some_and(|e| e.contains("probing on box gpu failed (exit 1)"))
    })
    .await;
    let b = box_info(&hub, BOX);
    assert_eq!(b["transport"], "ssh");
    assert_eq!(b["online"], false);
    assert_eq!(b["switch_pending"], false);
    assert_eq!(b["notes"], json!([]));

    let served = connect(&hub, &daemon.socket, &key, BOX).await;
    let h = hub.clone();
    wait_for("the box online", move || online(&h, BOX)).await;
    let b = box_info(&hub, BOX);
    assert_eq!(
        (&b["state"], &b["online"], &b["transport"]),
        (&json!("online"), &json!(true), &json!("ssh")),
        "{b}"
    );
    assert_eq!(b["version"], twr_runner::client::version());
    // How the daemon says it was started (for users only).
    assert_eq!(b["launch"], "detached", "{b}");
    let project = dir.path().join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let req = NewThread {
        project: project.to_string_lossy().into_owned(),
        runtime: Some("fake".into()),
        name: Some("remote1".into()),
        box_name: Some(BOX.into()),
        ..Default::default()
    };
    let t = hub.new_thread(req, "test").await.unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    hub.send(
        &t,
        "alpha beta",
        TurnFrom::User,
        Source::User("test".into()),
    )
    .unwrap();
    until_turn_event(&hub, &t, "turn.ended", 1).await;
    assert_eq!(daemon.lbox.live_count(), 1);

    // The hub restarts; the runtime runs on.
    hub.shutdown().await;
    let end = tokio::time::timeout(WAIT, served).await.unwrap().unwrap();
    assert!(end.welcomed && end.protocol_refused.is_none(), "{end:?}");
    assert_eq!(end.build.as_deref(), twr_core::build_id());
    drop(hub);
    assert_eq!(daemon.lbox.live_count(), 1);
    let hub = Hub::open(hub_opts(dir.path(), config())).await.unwrap();
    let _served = connect(&hub, &daemon.socket, &key, BOX).await;
    let h = hub.clone();
    wait_for("the box online again", move || online(&h, BOX)).await;
    assert_eq!(hub.get_thread(&t).unwrap()["live"], true);
    hub.send(&t, "gamma", TurnFrom::User, Source::User("test".into()))
        .unwrap();
    until_turn_event(&hub, &t, "turn.ended", 2).await;
    assert_eq!(count(&hub, &t, "thread.started"), 1);
    assert_eq!(count(&hub, &t, "thread.resumed"), 0);
    assert_eq!(count(&hub, &t, "thread.crashed"), 0);
    assert_eq!(daemon.lbox.live_count(), 1);
    hub.shutdown().await;
}

/// A sealed connection is bound to its box: a hello for another box and an `auth` with a
/// proof are `unauthorized`; a runner of another protocol is refused and the supervisor told;
/// the empty proof is authenticated, and the session ends welcomed with its inventory's
/// build.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sealed_handshake_is_bound_to_its_box() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(hub_opts(dir.path(), config())).await.unwrap();

    let (mut ws, served) = raw_sealed(&hub, BOX).await;
    send(&mut ws, &hello("other", PROTOCOL_VERSION)).await;
    let m = assert_error(next(&mut ws).await, hub_errors::UNAUTHORIZED);
    assert!(m.contains("box gpu's"), "{m}");
    assert_eq!(served.await, SealedEnd::default());

    let (mut ws, served) = raw_sealed(&hub, BOX).await;
    send(&mut ws, &hello(BOX, PROTOCOL_VERSION)).await;
    assert!(matches!(
        next(&mut ws).await,
        Some(HubMsg::Challenge { .. })
    ));
    let token = Secret::new("a-token-0123456789abcdef");
    let proof = auth_proof(&token, Prover::Runner, BOX, &new_nonce().unwrap(), "x");
    send(&mut ws, &RunnerMsg::Auth { proof }).await;
    assert_error(next(&mut ws).await, hub_errors::UNAUTHORIZED);
    assert_eq!(served.await, SealedEnd::default());
    let r = hub.box_refusal(BOX).unwrap();
    assert!(
        r.reason.contains("sent a proof on a sealed connection"),
        "{}",
        r.reason
    );

    let (mut ws, served) = raw_sealed(&hub, BOX).await;
    send(&mut ws, &hello(BOX, PROTOCOL_VERSION + 1)).await;
    assert_error(next(&mut ws).await, hub_errors::PROTOCOL);
    assert_eq!(served.await.protocol_refused, Some(PROTOCOL_VERSION + 1));

    let (mut ws, served) = raw_sealed(&hub, BOX).await;
    send(&mut ws, &hello(BOX, PROTOCOL_VERSION)).await;
    assert!(matches!(
        next(&mut ws).await,
        Some(HubMsg::Challenge { .. })
    ));
    send(
        &mut ws,
        &RunnerMsg::Auth {
            proof: SEALED_AUTH_PROOF.into(),
        },
    )
    .await;
    assert!(matches!(next(&mut ws).await, Some(HubMsg::Authenticated)));
    let inv = Inventory {
        version: "9.9.9-raw".into(),
        runtimes: vec!["fake".into()],
        worktrees: "/nonexistent".into(),
        build: Some("0123456789abcdef".into()),
        ..Default::default()
    };
    // Large once authenticated: past the pre-authentication limit.
    let mut big = inv.clone();
    big.runtimes
        .extend((0..2000).map(|i| format!("runtime-{i:04}")));
    send(&mut ws, &RunnerMsg::Inventory(big)).await;
    assert!(matches!(next(&mut ws).await, Some(HubMsg::Welcome { .. })));
    assert!(
        hub.box_refusal(BOX).is_none(),
        "an authenticated connection clears it"
    );
    drop(ws);
    let end = served.await;
    assert_eq!(
        end,
        SealedEnd {
            welcomed: true,
            protocol_refused: None,
            build: Some("0123456789abcdef".into()),
        }
    );
    hub.shutdown().await;
}

/// On the listener an SSH box, and a command box, is a box the hub does not know: a random
/// proof, then `unauthorized`, and no refusal recorded for the box.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_listener_refuses_an_ssh_box() {
    for dialed in [ssh_box(), command_box()] {
        listener_refuses(dialed).await;
    }
}

async fn listener_refuses(dialed: BoxConfig) {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config();
    cfg.boxes.insert(BOX.into(), dialed);
    cfg.hub.listen = Some("127.0.0.1:0".into());
    let l = RunnerListener::bind(&cfg.hub).await.unwrap().unwrap();
    let url = l.url();
    let hub = Hub::open(hub_opts(dir.path(), cfg)).await.unwrap();
    let _server = l.serve(hub.clone());
    let (mut ws, _) = tokio_tungstenite::connect_async(url.as_str())
        .await
        .unwrap();
    let nonce = new_nonce().unwrap();
    send(
        &mut ws,
        &RunnerMsg::Hello(Hello {
            box_name: BOX.into(),
            protocol: PROTOCOL_VERSION,
            nonce: nonce.clone(),
        }),
    )
    .await;
    let Some(HubMsg::Challenge {
        nonce: hub_nonce,
        proof,
    }) = next(&mut ws).await
    else {
        panic!("no challenge")
    };
    assert_eq!(proof.len(), 64);
    // Not a proof over an empty token either.
    let empty = Secret::new("");
    assert_ne!(
        proof,
        auth_proof(&empty, Prover::Hub, BOX, &nonce, &hub_nonce)
    );
    let mine = auth_proof(&empty, Prover::Runner, BOX, &nonce, &hub_nonce);
    send(&mut ws, &RunnerMsg::Auth { proof: mine }).await;
    let m = assert_error(next(&mut ws).await, hub_errors::UNAUTHORIZED);
    assert_eq!(m, "unknown box or bad token");
    assert!(hub.box_refusal(BOX).is_none());
    assert!(!online(&hub, BOX));
    hub.shutdown().await;
}

/// `box.add` reads config.toml and boxes.toml again and adds a token box (its short token a
/// warning; its runner can authenticate on the listener at once) and an SSH box (its
/// configured project used by `thread.new` once its daemon is connected); `config.show`
/// lists both with their transports. Refused: a box the hub has (nothing replaced), one in
/// neither file, files that do not parse, a short token, the orchestrator (agents cannot call
/// it at all), a named workspace. `box.install` wakes an SSH box only.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn boxes_are_added_while_the_hub_runs() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_dir = dir.path().join("cfg");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let config_file = cfg_dir.join("config.toml");
    let boxes_file = cfg_dir.join("boxes.toml");
    std::fs::write(&config_file, "default_runtime = \"fake\"\n").unwrap();
    let mut cfg = Config::load_with_boxes(&config_file).unwrap();
    cfg.hub.listen = Some("127.0.0.1:0".into());
    let l = RunnerListener::bind(&cfg.hub).await.unwrap().unwrap();
    let url = l.url();
    let mut opts = hub_opts(dir.path(), cfg);
    opts.config_file = Some(config_file.clone());
    let hub = Hub::open(opts).await.unwrap();
    let _server = l.serve(hub.clone());
    let lab_token = "lab-token-0123456789";
    let project = dir.path().join("api");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        &boxes_file,
        format!(
            "[boxes.lab]\ntoken = \"{lab_token}\"\n\n[boxes.far]\ntransport = \"ssh\"\nssh = \
             \"me@far.invalid\"\n\n[boxes.far.projects.api]\nrepo = \"{}\"\n",
            project.display()
        ),
    )
    .unwrap();

    let v = hub.box_add("lab", &user()).await.unwrap();
    assert_eq!(
        v,
        json!({"box": "lab", "transport": "token", "listening": true, "already": false})
    );
    let warnings = hub.info()["warnings"].to_string();
    assert!(warnings.contains("boxes.lab.token is short"), "{warnings}");
    assert!(!warnings.contains(lab_token), "{warnings}");
    // Its runner authenticates on the listener now.
    let (mut ws, _) = tokio_tungstenite::connect_async(url.as_str())
        .await
        .unwrap();
    let nonce = new_nonce().unwrap();
    send(
        &mut ws,
        &RunnerMsg::Hello(Hello {
            box_name: "lab".into(),
            protocol: PROTOCOL_VERSION,
            nonce: nonce.clone(),
        }),
    )
    .await;
    let Some(HubMsg::Challenge { nonce: hn, .. }) = next(&mut ws).await else {
        panic!("no challenge")
    };
    let proof = auth_proof(&Secret::new(lab_token), Prover::Runner, "lab", &nonce, &hn);
    send(&mut ws, &RunnerMsg::Auth { proof }).await;
    assert!(matches!(next(&mut ws).await, Some(HubMsg::Authenticated)));
    drop(ws);

    let v = hub.box_add("far", &user()).await.unwrap();
    assert_eq!(v["transport"], "ssh");
    let b = box_info(&hub, "far");
    assert_eq!(
        (&b["transport"], &b["online"]),
        (&json!("ssh"), &json!(false))
    );
    assert_eq!(box_info(&hub, "lab")["transport"], "token");
    assert_eq!(box_info(&hub, "lab")["switch_pending"], Value::Null);
    let shown = hub.config_show();
    assert_eq!(shown["boxes"]["lab"]["transport"], "token");
    assert_eq!(shown["boxes"]["far"]["transport"], "ssh");
    assert!(!shown.to_string().contains("far.invalid"), "{shown}");

    // Its daemon connected, `thread.new` finds the project boxes.toml gives it.
    let key = HubKey::generate().unwrap();
    let daemon = Daemon::start(&dir.path().join("far"), &key, "far").await;
    let _served = connect(&hub, &daemon.socket, &key, "far").await;
    let h = hub.clone();
    wait_for("far online", move || online(&h, "far")).await;
    let req = NewThread {
        project: "api".into(),
        runtime: Some("fake".into()),
        name: Some("on-far".into()),
        box_name: Some("far".into()),
        ..Default::default()
    };
    let t = hub.new_thread(req, "test").await.unwrap();
    assert_eq!(t["box_name"], "far");
    assert_eq!(t["repo"], project.to_string_lossy().as_ref(), "{t}");

    // A box the hub has: conflict, and nothing replaced.
    std::fs::write(
        &boxes_file,
        "[boxes.lab]\ntransport = \"ssh\"\nssh = \"me@lab.invalid\"\n",
    )
    .unwrap();
    let e = hub.box_add("lab", &user()).await.unwrap_err();
    assert_eq!(e.code, codes::CONFLICT, "{}", e.message);
    assert_eq!(box_info(&hub, "lab")["transport"], "token");
    // As the files say it (its build aside): nothing to do, and it says so.
    std::fs::write(
        &boxes_file,
        format!(
            "[boxes.far]\ntransport = \"ssh\"\nssh = \"me@far.invalid\"\nbuild = \
             \"0123456789abcdef\"\n\n[boxes.far.projects.api]\nrepo = \"{}\"\n",
            project.display()
        ),
    )
    .unwrap();
    assert_eq!(hub.box_add("far", &user()).await.unwrap()["already"], true);
    // In neither file; files that do not parse; a short token.
    let e = hub.box_add("nope", &user()).await.unwrap_err();
    assert_eq!(e.code, codes::NOT_FOUND, "{}", e.message);
    std::fs::write(&boxes_file, "[boxes.x\n").unwrap();
    let e = hub.box_add("x", &user()).await.unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST, "{}", e.message);
    assert!(e.message.contains("boxes.toml"), "{}", e.message);
    std::fs::write(&boxes_file, "[boxes.tiny]\ntoken = \"0123456789\"\n").unwrap();
    let e = hub.box_add("tiny", &user()).await.unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST, "{}", e.message);
    assert!(
        e.message.contains("boxes.tiny.token is too short"),
        "{}",
        e.message
    );
    assert!(!e.message.contains("0123456789"), "{}", e.message);
    assert_eq!(box_info(&hub, "tiny"), Value::Null);
    // `ssh` words that would forward something: an SSH box's, and a token box's (`twr
    // attach` runs them).
    for (name, file, says) in [
        (
            "fwd",
            "[boxes.fwd]\ntransport = \"ssh\"\nssh = \"-qoForwardAgent=yes me@fwd\"\n".to_string(),
            "`-o ForwardAgent=yes`: it forwards your ssh agent",
        ),
        (
            "x11",
            format!("[boxes.x11]\ntoken = \"{lab_token}\"\nssh = \"-X me@x11\"\n"),
            "boxes.x11.ssh = \"-X me@x11\": `-X`",
        ),
    ] {
        std::fs::write(&boxes_file, file).unwrap();
        let e = hub.box_add(name, &user()).await.unwrap_err();
        assert_eq!(e.code, codes::BAD_REQUEST, "{}", e.message);
        assert!(e.message.contains(says), "{}", e.message);
        assert_eq!(box_info(&hub, name), Value::Null);
    }
    // Users only.
    let e = hub
        .box_add("tiny", &Caller::Orchestrator)
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::UNAUTHORIZED);
    let e = hub
        .box_install("far", &Caller::Orchestrator)
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::UNAUTHORIZED);
    for m in ["box.add", "box.install"] {
        assert!(twr_hub::server::METHODS.contains(&m));
        assert!(!twr_hub::server::AGENT_METHODS.contains(&m));
        assert!(!twr_hub::server::ORCHESTRATOR_METHODS.contains(&m));
    }
    // `box.install`: SSH boxes only.
    assert_eq!(
        hub.box_install("far", &user()).await.unwrap(),
        json!({"box": "far"})
    );
    // It takes the box's new build, and nothing else: a box whose settings changed since
    // the hub added it (the CLI installed at another destination than the one the hub dials)
    // is a conflict until the hub restarts.
    let far = |ssh: &str| {
        format!(
            "[boxes.far]\ntransport = \"ssh\"\nssh = \"{ssh}\"\nbuild = \"0123456789abcdef\"\n\n\
             [boxes.far.projects.api]\nrepo = \"{}\"\n",
            project.display()
        )
    };
    std::fs::write(&boxes_file, far("me@elsewhere.invalid")).unwrap();
    let e = hub.box_install("far", &user()).await.unwrap_err();
    assert_eq!(e.code, codes::CONFLICT, "{}", e.message);
    assert!(
        e.message.contains("`twr hub stop`, then `twr`"),
        "{}",
        e.message
    );
    std::fs::write(&boxes_file, far("me@far.invalid")).unwrap();
    assert_eq!(
        hub.box_install("far", &user()).await.unwrap(),
        json!({"box": "far"})
    );
    let e = hub.box_install("lab", &user()).await.unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST);
    let e = hub.box_install("nope", &user()).await.unwrap_err();
    assert_eq!(e.code, codes::NOT_FOUND);
    hub.shutdown().await;

    // A named workspace's hub adds none.
    std::fs::write(
        &boxes_file,
        "[boxes.w]\ntransport = \"ssh\"\nssh = \"me@w\"\n",
    )
    .unwrap();
    let mut opts = hub_opts(&dir.path().join("ws"), Config::default());
    opts.config_file = Some(config_file);
    opts.workspace = Some("client-x".into());
    let hub = Hub::open(opts).await.unwrap();
    let e = hub.box_add("w", &user()).await.unwrap_err();
    assert_eq!(e.code, codes::UNSUPPORTED, "{}", e.message);
    hub.shutdown().await;

    // A hub does not start with a box whose `ssh` would forward something.
    let mut cfg = Config::default();
    cfg.boxes.insert(
        "lab".into(),
        BoxConfig {
            token: Some(lab_token.into()),
            ssh: Some("-A me@lab".into()),
            ..BoxConfig::default()
        },
    );
    let e = Hub::open(hub_opts(&dir.path().join("fwd"), cfg))
        .await
        .err()
        .expect("refused");
    assert!(format!("{e:#}").contains("boxes.lab.ssh"), "{e:#}");
}

/// `box.projects` reads a box's projects again from config.toml and boxes.toml (`twr box
/// project` recorded one there) and replaces those of that box only: a new thread on it uses
/// them at once, and the result says what each one's repo is now. A box whose other settings
/// changed there is a conflict, nothing taken (and
/// projects alone no longer make `box.install` one). Reloads run one at a time from their read
/// to their change: one whose read is slow is applied before the next one reads, so the older
/// file is never applied last. Users only; a box the hub or the files do not have, files that
/// do not parse, a hub without a config file are refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn box_projects_are_read_again_while_the_hub_runs() {
    let dir = tempfile::tempdir().unwrap();
    let cfg_dir = dir.path().join("cfg");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let config_file = cfg_dir.join("config.toml");
    let boxes_file = cfg_dir.join("boxes.toml");
    std::fs::write(&config_file, "default_runtime = \"fake\"\n").unwrap();
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let lab_token = "lab-token-0123456789abcdef0123456789";
    // Box `far` at `ssh` with `projects`, and box `lab` with project `lab-only` at `lab`.
    let files = |ssh: &str, projects: &[(&str, &Path)], lab: &Path| {
        let mut f = format!("[boxes.far]\ntransport = \"ssh\"\nssh = \"{ssh}\"\n");
        for (name, repo) in projects {
            f.push_str(&format!(
                "\n[boxes.far.projects.{name}]\nrepo = \"{}\"\n",
                repo.display()
            ));
        }
        f.push_str(&format!(
            "\n[boxes.lab]\ntoken = \"{lab_token}\"\n\n[boxes.lab.projects.lab-only]\nrepo = \
             \"{}\"\n",
            lab.display()
        ));
        std::fs::write(&boxes_file, f).unwrap();
    };
    let ssh = "me@far.invalid";
    files(ssh, &[("api", &a)], &a);
    // The slow read: armed, the next `box.projects` says it has read the files, then waits.
    type Gate = (oneshot::Sender<()>, oneshot::Receiver<()>);
    let gate: Arc<std::sync::Mutex<Option<Gate>>> = Arc::default();
    let mut opts = hub_opts(dir.path(), Config::load_with_boxes(&config_file).unwrap());
    opts.config_file = Some(config_file.clone());
    let g = Arc::clone(&gate);
    opts.box_files_read_hook = Some(Arc::new(move |_name: &str| {
        let armed = g.lock().unwrap().take();
        Box::pin(async move {
            if let Some((read, release)) = armed {
                let _ = read.send(());
                let _ = release.await;
            }
        })
    }));
    let hub = Hub::open(opts).await.unwrap();
    let repo_of =
        |hub: &Hub, b: &str, p: &str| hub.config_show()["boxes"][b]["projects"][p]["repo"].clone();
    let shown = |p: &Path| json!(p.to_string_lossy());
    assert_eq!(repo_of(&hub, "far", "api"), shown(&a));

    // Applied: that box's projects, nothing of the other's.
    files(ssh, &[("api", &b), ("web", &a)], &b);
    assert_eq!(
        hub.box_projects("far", &user()).await.unwrap(),
        json!({
            "box": "far",
            "projects": ["api", "web"],
            "repos": {"api": shown(&b), "web": shown(&a)}
        })
    );
    assert_eq!(repo_of(&hub, "far", "api"), shown(&b));
    assert_eq!(repo_of(&hub, "far", "web"), shown(&a));
    assert_eq!(repo_of(&hub, "lab", "lab-only"), shown(&a));
    // A new thread on the box uses them.
    let key = HubKey::generate().unwrap();
    let daemon = Daemon::start(&dir.path().join("far"), &key, "far").await;
    let _served = connect(&hub, &daemon.socket, &key, "far").await;
    let h = hub.clone();
    wait_for("far online", move || online(&h, "far")).await;
    let t = hub
        .new_thread(
            NewThread {
                project: "web".into(),
                runtime: Some("fake".into()),
                name: Some("on-far".into()),
                box_name: Some("far".into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap();
    assert_eq!(t["repo"], shown(&a), "{t}");

    // The box's connection changed in the files: a conflict, nothing taken.
    files("me@elsewhere.invalid", &[("api", &a)], &a);
    let e = hub.box_projects("far", &user()).await.unwrap_err();
    assert_eq!(e.code, codes::CONFLICT, "{}", e.message);
    assert!(
        e.message.contains("`twr hub stop`, then `twr`"),
        "{}",
        e.message
    );
    assert_eq!(repo_of(&hub, "far", "api"), shown(&b));
    // Projects that differ alone do not make `box.install` a conflict.
    files(ssh, &[("api", &a)], &a);
    assert_eq!(
        hub.box_install("far", &user()).await.unwrap(),
        json!({"box": "far"})
    );

    // A slow reload of an older file, then a reload of a newer one: the newer one is last.
    let (read_tx, read_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    *gate.lock().unwrap() = Some((read_tx, release_rx));
    files(ssh, &[("old", &a)], &a);
    let h = hub.clone();
    let older = tokio::spawn(async move { h.box_projects("far", &user()).await });
    tokio::time::timeout(WAIT, read_rx).await.unwrap().unwrap();
    files(ssh, &[("new", &b)], &a);
    let h = hub.clone();
    let newer = tokio::spawn(async move { h.box_projects("far", &user()).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !newer.is_finished(),
        "the newer reload ran while the older one was reading"
    );
    release_tx.send(()).unwrap();
    let older = older.await.unwrap().unwrap();
    let newer = newer.await.unwrap().unwrap();
    assert_eq!(older["projects"], json!(["old"]));
    assert_eq!(newer["projects"], json!(["new"]));
    assert_eq!(older["repos"], json!({"old": shown(&a)}));
    assert_eq!(newer["repos"], json!({"new": shown(&b)}));
    let far_projects = hub.config_show()["boxes"]["far"]["projects"].clone();
    assert_eq!(
        far_projects,
        json!({"new": {"repo": shown(&b), "runtime": null}})
    );

    // Refusals.
    let e = hub
        .box_projects("far", &Caller::Orchestrator)
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::UNAUTHORIZED);
    assert!(twr_hub::server::METHODS.contains(&"box.projects"));
    assert!(!twr_hub::server::AGENT_METHODS.contains(&"box.projects"));
    assert!(!twr_hub::server::ORCHESTRATOR_METHODS.contains(&"box.projects"));
    let e = hub.box_projects("nope", &user()).await.unwrap_err();
    assert_eq!(e.code, codes::NOT_FOUND, "{}", e.message);
    let e = hub.box_projects("local", &user()).await.unwrap_err();
    assert_eq!(e.code, codes::NOT_FOUND, "{}", e.message);
    std::fs::write(&boxes_file, "[boxes.far\n").unwrap();
    let e = hub.box_projects("far", &user()).await.unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST, "{}", e.message);
    std::fs::write(&boxes_file, "").unwrap();
    let e = hub.box_projects("far", &user()).await.unwrap_err();
    assert_eq!(e.code, codes::NOT_FOUND, "{}", e.message);
    assert!(
        e.message.contains("no longer have box far"),
        "{}",
        e.message
    );
    assert_eq!(repo_of(&hub, "far", "new"), shown(&b));
    drop(daemon);
    hub.shutdown().await;

    // A hub without a config file reads none.
    let hub = Hub::open(hub_opts(&dir.path().join("nofile"), config()))
        .await
        .unwrap();
    let e = hub.box_projects(BOX, &user()).await.unwrap_err();
    assert_eq!(e.code, codes::UNSUPPORTED, "{}", e.message);
    hub.shutdown().await;
}

/// Concurrent adds of one box: one adds it, the others find it there as the files say
/// (`already`), and the hub has one such box (with one supervisor).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_adds_of_one_box_add_it_once() {
    let dir = tempfile::tempdir().unwrap();
    let config_file = dir.path().join("config.toml");
    std::fs::write(&config_file, "").unwrap();
    std::fs::write(
        dir.path().join("boxes.toml"),
        "[boxes.c2]\ntransport = \"ssh\"\nssh = \"me@c2.invalid\"\n",
    )
    .unwrap();
    let mut opts = hub_opts(dir.path(), Config::default());
    opts.config_file = Some(config_file);
    let hub = Hub::open(opts).await.unwrap();
    let adds: Vec<_> = (0..8)
        .map(|_| {
            let hub = hub.clone();
            tokio::spawn(async move { hub.box_add("c2", &user()).await })
        })
        .collect();
    let mut added = 0;
    for a in adds {
        let v = a.await.unwrap().unwrap();
        if v["already"] == false {
            added += 1;
        }
    }
    assert_eq!(added, 1);
    let boxes = hub.info()["boxes"].clone();
    assert_eq!(boxes.as_array().unwrap().len(), 1, "{boxes}");
    // Its one supervisor runs (and fails to probe here).
    let h = hub.clone();
    wait_for("the supervisor's failed probe", move || {
        box_info(&h, "c2")["last_error"]
            .as_str()
            .is_some_and(|e| e.contains("probing on box c2"))
    })
    .await;
    hub.shutdown().await;
}

/// A delete card confirmed after one of its threads was deleted and restored is replaced by a
/// card for the others; when the box of every other one is offline by then, the user's
/// confirmed intent is not dropped silently: an error item on each says the deletion did not
/// happen and why.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_confirmed_card_whose_box_went_offline_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let key = HubKey::generate().unwrap();
    let daemon = Daemon::start(&dir.path().join("box"), &key, BOX).await;
    let hub = Hub::open(hub_opts(dir.path(), config())).await.unwrap();
    let _served = connect(&hub, &daemon.socket, &key, BOX).await;
    let h = hub.clone();
    wait_for("the box online", move || online(&h, BOX)).await;
    let repo = twr_core::testkit::git::init_repo(&dir.path().join("repo"));
    let mut ids = Vec::new();
    for name in ["a", "b"] {
        let req = NewThread {
            project: repo.to_string_lossy().into_owned(),
            runtime: Some("fake".into()),
            name: Some(name.into()),
            box_name: Some(BOX.into()),
            ..Default::default()
        };
        ids.push(
            hub.new_thread(req, "test").await.unwrap()["id"]
                .as_str()
                .unwrap()
                .to_string(),
        );
    }
    let (a, b) = (ids[0].clone(), ids[1].clone());
    let ask = |threads: &[&str]| {
        let threads: Vec<String> = threads.iter().map(|t| t.to_string()).collect();
        let hub = hub.clone();
        async move { hub.delete_threads(&threads, &user()).await.unwrap() }
    };
    let answer = |card: &Value| {
        hub.respond_at(
            card["attn_id"].as_str().unwrap(),
            twr_core::adapter::Answer::new("Delete"),
            card["revision"].as_u64(),
            &user(),
        )
    };
    let ab = ask(&["a", "b"]).await;
    let only_a = ask(&["a"]).await;
    answer(&only_a).unwrap();
    let h = hub.clone();
    let id = a.clone();
    wait_for("a deleted", move || h.thread_view(&id).is_err()).await;
    let item = hub
        .control_read(0, 100_000)
        .unwrap()
        .0
        .into_iter()
        .find_map(|(_, e)| match e.body {
            EventBody::ThreadDeleted(d) if e.thread == a => d.trash,
            _ => None,
        })
        .expect("a's trash item");
    hub.trash_restore(&item, None, &user()).await.unwrap();
    assert!(hub.thread_view(&a).is_ok());

    // The box goes offline; then the card from before is confirmed.
    drop(daemon);
    let h = hub.clone();
    wait_for("the box offline", move || !online(&h, BOX)).await;
    answer(&ab).unwrap();
    let h = hub.clone();
    let id = b.clone();
    wait_for("an item on b saying why", move || {
        h.attention_list(true).iter().any(|i| {
            i["thread"] == json!(id)
                && i["summary"].as_str().is_some_and(|s| {
                    s.contains(&format!(
                        "card {} was confirmed, but",
                        ab["attn_id"].as_str().unwrap()
                    )) && s.contains("offline")
                })
        })
    })
    .await;
    assert!(hub.thread_view(&b).is_ok(), "b is still there");
    hub.shutdown().await;
}

/// An agent of a box's report.
fn agent(runtime: &str, found: bool, login: AgentLogin) -> AgentInfo {
    AgentInfo {
        runtime: runtime.into(),
        found,
        path: found.then(|| format!("/home/u/.local/bin/{runtime}")),
        version: found.then(|| "1.0.0".into()),
        login,
        method: (login == AgentLogin::LoggedIn).then(|| "claude.ai".into()),
        note: (!found).then(|| "not found on PATH".into()),
    }
}

/// The hub's next command on `ws`, its id and op, past the hub's own rounds (the trash's
/// listing, the disk's reading), which are answered with a failure.
async fn next_cmd<S>(ws: &mut WebSocketStream<S>) -> (String, Op)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        match next(ws).await {
            Some(HubMsg::Cmd { cmd_id, op }) => match op {
                Op::TrashList { .. } | Op::DiskUsage { .. } => {
                    let failed = twr_core::runner::CmdError::new("failed", "not here");
                    send(
                        ws,
                        &RunnerMsg::CmdResult(CmdResult::new(cmd_id, Err(failed))),
                    )
                    .await;
                }
                op => return (cmd_id, op),
            },
            other => panic!("expected a command, got {other:?}"),
        }
    }
}

async fn answer<S>(ws: &mut WebSocketStream<S>, cmd_id: &str, result: Value)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    send(
        ws,
        &RunnerMsg::CmdResult(CmdResult::new(cmd_id, Ok(result))),
    )
    .await;
}

/// A box's agents reach the hub: its inventory's first, then its answer to the hub's poll
/// (shortly after the welcome) and to `box.agents refresh` (users only). A user sees all of
/// them, the orchestrator only each box's state and which agents it has, signed in or not.
/// A thread on a runtime the box reports missing is still made, or moved there, with a
/// warning. Once the box is offline, a refresh is `box_offline` and the last report stays.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_boxs_agents_reach_the_hub_and_warn_its_threads() {
    let dir = tempfile::tempdir().unwrap();
    let mut opts = hub_opts(dir.path(), config());
    opts.agents_poll = AgentsPoll {
        first: Duration::from_millis(300),
        every: Duration::from_secs(3600),
    };
    let hub = Hub::open(opts).await.unwrap();
    let (mut ws, served) = raw_sealed(&hub, BOX).await;
    send(&mut ws, &hello(BOX, PROTOCOL_VERSION)).await;
    assert!(matches!(
        next(&mut ws).await,
        Some(HubMsg::Challenge { .. })
    ));
    send(
        &mut ws,
        &RunnerMsg::Auth {
            proof: SEALED_AUTH_PROOF.into(),
        },
    )
    .await;
    assert!(matches!(next(&mut ws).await, Some(HubMsg::Authenticated)));
    let inv = Inventory {
        version: "9.9.9-raw".into(),
        runtimes: ["fake", "claude-code", "codex", "grok-build"]
            .map(String::from)
            .to_vec(),
        worktrees: "/nonexistent".into(),
        build: Some("0123456789abcdef".into()),
        agents: vec![
            agent("claude-code", true, AgentLogin::LoggedIn),
            agent("codex", false, AgentLogin::Unknown),
        ],
        launch: Some("launchd tools.twr.box.0123456789abcdef.0a1b2c3d".into()),
        ..Default::default()
    };
    send(&mut ws, &RunnerMsg::Inventory(inv)).await;
    assert!(matches!(next(&mut ws).await, Some(HubMsg::Welcome { .. })));
    send(&mut ws, &RunnerMsg::Synced).await;
    let h = hub.clone();
    wait_for("the box online", move || online(&h, BOX)).await;

    // The inventory's: all of it for the user, state and agents for the orchestrator.
    let b = box_info(&hub, BOX);
    assert_eq!(
        b["agents"][0]["path"], "/home/u/.local/bin/claude-code",
        "{b}"
    );
    assert_eq!(b["agents"][0]["method"], "claude.ai", "{b}");
    assert_eq!(b["agents"][1]["note"], "not found on PATH", "{b}");
    assert_eq!(
        b["launch"], "launchd tools.twr.box.0123456789abcdef.0a1b2c3d",
        "{b}"
    );
    let limited = hub.info_for(BoxView::Limited);
    let lb = limited["boxes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["name"] == BOX)
        .unwrap()
        .clone();
    assert_eq!(
        lb,
        json!({
            "name": BOX, "state": "online", "online": true, "version": "9.9.9-raw",
            "agents": [
                {"runtime": "claude-code", "found": true, "login": "logged_in"},
                {"runtime": "codex", "found": false, "login": "unknown"},
            ],
        })
    );
    assert_eq!(limited["agents"], json!([]), "the hub probes none here");

    // Shortly after the welcome the hub asks again (the runner's last probe): its answer is
    // the box's report from then on, filtered as the probe filters (a box may say anything):
    // an email as the sign-in method, a note of its own, runtimes twr has no CLI probe for,
    // a runtime twice.
    let (id, op) = next_cmd(&mut ws).await;
    assert_eq!(op, Op::Agents { refresh: false });
    let mut leaky = agent("claude-code", true, AgentLogin::LoggedIn);
    leaky.method = Some("alice@example.com".into());
    leaky.note = Some("signed in as alice@example.com (Acme Corp)".into());
    let mut junk = agent("evil\u{1b}]0;owned\u{7}", true, AgentLogin::LoggedIn);
    junk.method = Some("bob@example.com".into());
    let mut codex_method = agent("codex", false, AgentLogin::Unknown);
    codex_method.method = Some("claude.ai".into());
    answer(
        &mut ws,
        &id,
        json!([
            leaky,
            junk,
            agent("pty", true, AgentLogin::LoggedIn),
            agent("claude-code", false, AgentLogin::LoggedOut),
            codex_method,
            agent("grok-build", false, AgentLogin::Unknown),
        ]),
    )
    .await;
    let h = hub.clone();
    wait_for("the poll's answer", move || {
        box_info(&h, BOX)["agents"].as_array().map(Vec::len) == Some(3)
    })
    .await;
    let b = box_info(&hub, BOX);
    let runtimes: Vec<&str> = b["agents"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|a| a["runtime"].as_str())
        .collect();
    assert_eq!(runtimes, ["claude-code", "codex", "grok-build"], "{b}");
    assert_eq!(
        b["agents"][0]["found"], true,
        "the first entry of a runtime: {b}"
    );
    assert_eq!(b["agents"][0]["method"], "other", "{b}");
    assert_eq!(
        b["agents"][0]["note"], "the box reported something else",
        "{b}"
    );
    assert!(b["agents"][1]["method"].is_null(), "{b}");
    assert_eq!(b["agents"][1]["note"], "not found on PATH", "{b}");
    assert!(!b.to_string().contains("example.com"), "{b}");

    // A thread on a runtime the box reports missing is made, with a warning.
    let new = |name: &str, runtime: &str| {
        let hub = hub.clone();
        let req = NewThread {
            project: "/srv/api".into(),
            runtime: Some(runtime.into()),
            name: Some(name.into()),
            box_name: Some(BOX.into()),
            no_worktree: true,
            ..Default::default()
        };
        tokio::spawn(async move { hub.new_thread(req, "test").await })
    };
    let made = new("on-codex", "codex");
    let (id, op) = next_cmd(&mut ws).await;
    assert!(matches!(op, Op::RepoRoot { .. }), "{op:?}");
    answer(&mut ws, &id, Value::Null).await;
    let t = made.await.unwrap().unwrap();
    assert_eq!(t["box_name"], BOX);
    assert_eq!(
        t["warning"],
        "box gpu reports no `codex` on its PATH; the thread may fail to start: run `twr box \
         doctor gpu`"
    );
    let made = new("on-claude", "claude-code");
    let (id, _) = next_cmd(&mut ws).await;
    answer(&mut ws, &id, Value::Null).await;
    let t = made.await.unwrap().unwrap();
    assert!(t.get("warning").is_none(), "{t}");
    // Moved to one the box reports missing: moved, with the warning; to one it has, without.
    let moved = hub
        .set_runtime_model("on-claude", Some("grok-build"), None, None, &user())
        .unwrap();
    assert_eq!(moved["runtime_changed"], true, "{moved}");
    assert!(
        moved["warning"]
            .as_str()
            .is_some_and(|w| w.starts_with("box gpu reports no `grok` on its PATH")),
        "{moved}"
    );
    let moved = hub
        .set_runtime_model("on-codex", Some("claude-code"), None, None, &user())
        .unwrap();
    assert!(moved.get("warning").is_none(), "{moved}");
    // A new model or effort on the same runtime restarts its CLI: the thread's own runtime is
    // checked (allowed either way).
    let grok = "box gpu reports no `grok` on its PATH";
    let changed = hub
        .set_runtime_model("on-claude", None, Some("grok-4.6"), None, &user())
        .unwrap();
    assert!(
        changed["warning"]
            .as_str()
            .is_some_and(|w| w.starts_with(grok)),
        "{changed}"
    );
    let changed = hub
        .set_runtime_model("on-claude", None, None, Some("high"), &user())
        .unwrap();
    assert!(
        changed["warning"]
            .as_str()
            .is_some_and(|w| w.starts_with(grok)),
        "{changed}"
    );
    let changed = hub
        .set_runtime_model("on-codex", None, Some("sonnet"), None, &user())
        .unwrap();
    assert!(changed.get("warning").is_none(), "{changed}");
    // A new permission mode (re)starts the runtime too: the same warning.
    let mode = hub.set_mode("on-claude", "read-only", &user()).unwrap();
    assert_eq!(mode["changed"], true, "{mode}");
    assert!(
        mode["warning"]
            .as_str()
            .is_some_and(|w| w.starts_with(grok)),
        "{mode}"
    );
    let mode = hub.set_mode("on-codex", "read-only", &user()).unwrap();
    assert!(mode.get("warning").is_none(), "{mode}");

    // A prompt that starts a thread's runtime: the same warning; one to a thread whose
    // runtime runs already starts nothing, so none.
    let sent = hub
        .send(
            "on-claude",
            "hi",
            TurnFrom::User,
            Source::User("test".into()),
        )
        .unwrap();
    let sent = serde_json::to_value(sent).unwrap();
    assert!(
        sent["warning"]
            .as_str()
            .is_some_and(|w| w.starts_with(grok)),
        "{sent}"
    );
    let again = hub
        .send(
            "on-claude",
            "more",
            TurnFrom::User,
            Source::User("test".into()),
        )
        .unwrap();
    assert_eq!(again.warning, None, "{again:?}");
    let fine = hub
        .send(
            "on-codex",
            "hi",
            TurnFrom::User,
            Source::User("test".into()),
        )
        .unwrap();
    assert_eq!(fine.warning, None, "{fine:?}");

    // `box.agents`: what is known, as each caller sees it; a fresh probe for users only.
    let known = hub.box_agents(BOX, false, BoxView::Limited).await.unwrap();
    assert_eq!(known["box"], BOX);
    assert_eq!(known["online"], true);
    assert_eq!(
        known["agents"][0],
        json!({"runtime": "claude-code", "found": true, "login": "logged_in"})
    );
    let e = hub
        .box_agents(BOX, true, BoxView::Limited)
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::UNAUTHORIZED);
    let h = hub.clone();
    let fresh = tokio::spawn(async move { h.box_agents(BOX, true, BoxView::Full).await });
    // (Past what the prompts above asked of the box: their runtimes' commands.)
    let (id, op) = loop {
        let (id, op) = next_cmd(&mut ws).await;
        if matches!(op, Op::Agents { .. }) {
            break (id, op);
        }
    };
    assert_eq!(op, Op::Agents { refresh: true });
    answer(
        &mut ws,
        &id,
        json!([
            agent("claude-code", true, AgentLogin::LoggedIn),
            agent("codex", true, AgentLogin::LoggedOut),
        ]),
    )
    .await;
    let fresh = fresh.await.unwrap().unwrap();
    assert_eq!(fresh["agents"][1]["found"], true, "{fresh}");
    assert_eq!(fresh["agents"][1]["path"], "/home/u/.local/bin/codex");
    assert_eq!(box_info(&hub, BOX)["agents"][1]["login"], "logged_out");
    let e = hub
        .box_agents("nope", false, BoxView::Full)
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::NOT_FOUND);

    // Offline: no fresh probe, the last report stays.
    drop(ws);
    served.await;
    let h = hub.clone();
    wait_for("the box offline", move || !online(&h, BOX)).await;
    let e = hub.box_agents(BOX, true, BoxView::Full).await.unwrap_err();
    assert_eq!(e.code, codes::BOX_OFFLINE);
    let last = hub.box_agents(BOX, false, BoxView::Full).await.unwrap();
    assert_eq!(last["online"], false);
    assert_eq!(last["agents"][1]["login"], "logged_out");
    hub.shutdown().await;
}

/// A box of another OS or CPU runs its configured build; after a twr upgrade that build speaks
/// an older protocol, so the hub refuses its daemon. The hub does not launch that build again
/// and again: the box needs an install (`twr box install`), said in its `last_error`, and is
/// looked at again only when `box.install` wakes it (or after 5 minutes).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cross_target_build_of_an_older_protocol_needs_an_install() {
    use futures::FutureExt;
    use twr_hub::ssh_box::{BoxDialer, BoxIo, Dial};
    const CONFIGURED: &str = "00000000000000c6";
    let dir = tempfile::tempdir().unwrap();
    // The hub's box key, made now: the stand-in daemon checks its holder.
    let data = dir.path().join("hub");
    std::fs::create_dir_all(&data).unwrap();
    let key = HubKey::load_or_create(&data.join(twr_hub::ssh_box::BOX_KEY_FILE)).unwrap();
    let public = key.public();
    let dials = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = Arc::clone(&dials);
    // A daemon of the configured build, speaking the protocol before the hub's.
    let dialer: BoxDialer = Arc::new(move |name: String, _dial: Dial| {
        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        async move {
            let (hub_end, box_end) = tokio::io::duplex(1 << 16);
            tokio::spawn(async move {
                let Ok(sealed) =
                    twr_core::sealed::daemon_side(box_end, &public, &name, Some(CONFIGURED)).await
                else {
                    return;
                };
                let Ok((mut ws, _)) =
                    tokio_tungstenite::client_async("ws://twr-box/", sealed).await
                else {
                    return;
                };
                send(&mut ws, &hello(&name, PROTOCOL_VERSION - 1)).await;
                let _ = next(&mut ws).await;
            });
            let io: Box<dyn BoxIo> = Box::new(hub_end);
            Ok((io, CONFIGURED.to_string()))
        }
        .boxed()
    });
    let mut cfg = config();
    cfg.boxes.get_mut(BOX).unwrap().build = Some(CONFIGURED.into());
    let mut opts = hub_opts(dir.path(), cfg);
    opts.box_dialer = Some(dialer);
    let hub = Hub::open(opts).await.unwrap();
    let want = format!(
        "box gpu runs a twr build that speaks protocol {}, the hub {PROTOCOL_VERSION}: install \
         this hub's build there: `twr box install gpu`",
        PROTOCOL_VERSION - 1
    );
    let h = hub.clone();
    let w = want.clone();
    wait_for("needs_install", move || {
        let b = box_info(&h, BOX);
        b["state"] == "needs_install" && b["last_error"] == json!(w)
    })
    .await;
    // Not launched again meanwhile.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(dials.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(box_info(&hub, BOX)["state"], "needs_install");
    // `box.install` wakes it: one more attempt (refused again here).
    hub.box_install(BOX, &user()).await.unwrap();
    let d = Arc::clone(&dials);
    wait_for("another attempt", move || {
        d.load(std::sync::atomic::Ordering::SeqCst) == 2
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(dials.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(box_info(&hub, BOX)["state"], "needs_install");
    hub.shutdown().await;
}

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "user.email=t@t", "-c", "user.name=t"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

/// A command box, added with `box.add` from boxes.toml, is dialed by its supervisor (the test
/// dialer standing in for its command, the daemon in this process): it goes online with the
/// fields of a box the hub dials in `hub.info` and transport `command`, its threads' branches
/// take the hub's namespace, and a fake runtime's turn runs on it. Neither `hub.info` nor
/// `config.show` shows its command. `box.install` and `box.restart` take it: the restart
/// dials again with `--restart` and the box comes back on a larger epoch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_box_is_added_dialed_and_restarted() {
    use futures::FutureExt;
    use twr_hub::ssh_box::{BoxDialer, BoxIo, Dial};
    let dir = tempfile::tempdir().unwrap();
    // The hub's box key, made now: the daemon checks its holder.
    let data = dir.path().join("hub");
    std::fs::create_dir_all(&data).unwrap();
    let key = HubKey::load_or_create(&data.join(twr_hub::ssh_box::BOX_KEY_FILE)).unwrap();
    let daemon = Daemon::start(&dir.path().join("sb"), &key, "sb").await;
    let dials: Arc<std::sync::Mutex<Vec<Dial>>> = Arc::default();
    let dialer: BoxDialer = {
        let (socket, dials) = (daemon.socket.clone(), Arc::clone(&dials));
        Arc::new(move |name: String, dial: Dial| {
            let (socket, dials) = (socket.clone(), Arc::clone(&dials));
            async move {
                assert_eq!(name, "sb");
                dials.lock().unwrap().push(dial);
                let (stream, _) = request(&socket, Request::Connect)
                    .await
                    .map_err(|e| std::io::Error::other(format!("{e:#}")))?;
                let io: Box<dyn BoxIo> = Box::new(stream);
                let build = twr_core::build_id().unwrap_or("0123456789abcdef");
                Ok((io, build.to_string()))
            }
            .boxed()
        })
    };
    let config_file = dir.path().join("config.toml");
    std::fs::write(&config_file, "default_runtime = \"fake\"\n").unwrap();
    let mut opts = hub_opts(dir.path(), Config::load_with_boxes(&config_file).unwrap());
    opts.config_file = Some(config_file);
    opts.box_dialer = Some(dialer);
    opts.worktree_namespace = Some("hubns".into());
    let hub = Hub::open(opts).await.unwrap();
    std::fs::write(
        dir.path().join("boxes.toml"),
        "[boxes.sb]\ntransport = \"command\"\ncommand = [\"docker\", \"exec\", \"-i\", \"-e\", \
         \"K=s3cret\", \"twr-s3cret-ctr\"]\nterminal_command = [\"docker\", \"exec\", \"-i\", \
         \"-t\", \"twr-s3cret-ctr\"]\n",
    )
    .unwrap();
    let v = hub.box_add("sb", &user()).await.unwrap();
    assert_eq!(
        (&v["transport"], &v["already"]),
        (&json!("command"), &json!(false))
    );
    let h = hub.clone();
    wait_for("sb online", move || online(&h, "sb")).await;
    let b = box_info(&hub, "sb");
    assert_eq!(
        (&b["transport"], &b["state"], &b["online"]),
        (&json!("command"), &json!("online"), &json!(true)),
        "{b}"
    );
    // The fields of a box the hub dials.
    assert!(b["notes"].is_array(), "{b}");
    assert!(b["switch_pending"].is_boolean(), "{b}");
    assert_eq!(b["restart_pending"], false, "{b}");
    assert_eq!(b["launch"], "detached", "{b}");
    let epoch = b["epoch"].as_u64().unwrap();
    for shown in [hub.info().to_string(), hub.config_show().to_string()] {
        assert!(
            !shown.contains("s3cret") && !shown.contains("docker"),
            "{shown}"
        );
    }
    assert_eq!(hub.config_show()["boxes"]["sb"]["transport"], "command");
    assert_eq!(hub.config_show()["boxes"]["sb"]["has_token"], false);
    assert_eq!(dials.lock().unwrap().as_slice(), [Dial::default()]);

    // A thread on it: its branch in the hub's namespace, and a turn.
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
    let t = hub
        .new_thread(
            NewThread {
                project: repo.to_string_lossy().into_owned(),
                runtime: Some("fake".into()),
                name: Some("on-sb".into()),
                box_name: Some("sb".into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    git(
        &repo,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "refs/heads/twr/hubns/on-sb",
        ],
    );
    hub.send(&t, "alpha", TurnFrom::User, Source::User("test".into()))
        .unwrap();
    until_turn_event(&hub, &t, "turn.ended", 1).await;
    assert_eq!(daemon.lbox.live_count(), 1);

    // `box.install` and `box.restart` take a command box.
    assert_eq!(
        hub.box_install("sb", &user()).await.unwrap(),
        json!({"box": "sb"})
    );
    assert_eq!(
        hub.box_restart("sb", &user()).unwrap(),
        json!({"box": "sb", "epoch": epoch})
    );
    let h = hub.clone();
    wait_for("sb online again, restarted", move || {
        let b = box_info(&h, "sb");
        b["online"] == true
            && b["epoch"].as_u64().is_some_and(|e| e > epoch)
            && b["restart_pending"] == false
    })
    .await;
    assert!(
        dials.lock().unwrap().last().is_some_and(|d| d.restart),
        "{:?}",
        dials.lock().unwrap()
    );
    // The orchestrator may not.
    let e = hub.box_restart("sb", &Caller::Orchestrator).unwrap_err();
    assert_eq!(e.code, codes::UNAUTHORIZED);
    hub.shutdown().await;
}
