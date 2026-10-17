//! The switch of an SSH box to the build the hub launched there (`twr_hub::ssh_box`,
//! "Switching builds"). A box daemon of the twr-runner library runs in this process and
//! stands in for the box. A first hub, connected to it as W3's tests connect (`ssh_boxes.rs`),
//! leaves threads with runtimes on it; a second hub dials it through its ssh supervisor with a
//! test dialer (`HubOptions::box_dialer`), which takes the place of ssh and the relay: it says
//! the hub launched another build than the daemon's (so the box is `switch_pending`), and a
//! dial that replaces the daemon (`--replace`) stops it and starts a new one on the same data
//! directory, of the build the hub launched. The bytes the hub sends the box go through a pump
//! the test can hold (a slow link), so what happens while the switch's stops are on their way
//! is deterministic.
//!
//! Covered: a quiet box with idle runtimes switches (their threads end `box_upgrade`, not
//! stopped, no crash or error item; the supervisor replaces the daemon; the next prompt resumes
//! the session), a prompt sent during the drain is queued and runs once after the switch, a
//! `thread.stop` during the drain leaves its thread stopped, and a new thread, a park or a
//! deletion on the box during the drain is refused (`box_offline`, the box is switching); a
//! busy box (a running turn, an open permission) is not switched until it is quiet.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{oneshot, watch};
use twr_adapters::FakeAdapter;
use twr_core::adapter::{Adapter, Answer};
use twr_core::config::{BoxConfig, BoxTransport, Config};
use twr_core::event::*;
use twr_core::proto::{codes, RpcError};
use twr_core::sealed::{hub_side, HubKey};
use twr_hub::hub::SealedEnd;
use twr_hub::listener::pre_auth_ws_config;
use twr_hub::ssh_box::{BoxDialer, BoxIo, SwitchTiming};
use twr_hub::{Caller, Hub, HubOptions, NewThread};
use twr_runner::daemon::{request, Request};
use twr_runner::relay::DaemonPaths;
use twr_runner::{DaemonOptions, LocalBox, RunnerOptions};

const BOX: &str = "gpu";
const WAIT: Duration = Duration::from_secs(30);
/// The build the dialer says the hub launched on the box first (the daemon runs another: this
/// test binary's).
const LAUNCHED: &str = "00000000000000a1";

// ------------------------------------------------------------------------------------------
// Harness
// ------------------------------------------------------------------------------------------

/// Logs, and `TWR_SSH=false` before any hub runs (the first hub's supervisor fails to probe
/// at once and stays out of the way, as in `ssh_boxes.rs`).
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

fn with_fake() -> twr_hub::host::AdapterLookup {
    Arc::new(|name: &str| -> Option<Arc<dyn Adapter>> {
        match name {
            "fake" => Some(Arc::new(FakeAdapter::new())),
            other => twr_adapters::find(other),
        }
    })
}

fn config() -> Config {
    let mut c = Config {
        default_runtime: "fake".into(),
        ..Default::default()
    };
    c.boxes.insert(
        BOX.into(),
        BoxConfig {
            transport: Some(BoxTransport::Ssh),
            ssh: Some("me@gpu.invalid".into()),
            ..Default::default()
        },
    );
    c
}

fn hub_opts(dir: &Path) -> HubOptions {
    init();
    let mut o = HubOptions::new(dir.join("hub").join("twr.db"), config());
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

fn box_info(hub: &Hub) -> Value {
    hub.info()["boxes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["name"] == BOX)
        .cloned()
        .unwrap_or(Value::Null)
}

fn online(hub: &Hub) -> bool {
    hub.boxes().iter().any(|(n, on, _)| n == BOX && *on)
}

fn has_note(hub: &Hub, note: &str) -> bool {
    box_info(hub)["notes"]
        .as_array()
        .is_some_and(|n| n.iter().any(|l| l == note))
}

/// The build of this test binary, which every daemon here runs.
fn own_build() -> &'static str {
    twr_core::build_id().expect("the test binary's build id")
}

/// A box daemon running in this process, as `twr runner --daemon` runs on a box.
struct Daemon {
    socket: PathBuf,
    lbox: Option<LocalBox>,
    stop: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<anyhow::Result<()>>>,
}

impl Daemon {
    async fn start(dir: &Path, hub_key: [u8; 32]) -> Daemon {
        Daemon::start_as(dir, hub_key, own_build()).await
    }

    /// [`Daemon::start`] of a daemon that says it runs build `build`.
    async fn start_as(dir: &Path, hub_key: [u8; 32], build: &str) -> Daemon {
        init();
        let paths = DaemonPaths {
            socket: dir.join("st/box-daemons/d.sock"),
            data_dir: dir.join("data"),
            lock: dir.join("st/locks/box.lock"),
            log: dir.join("data/daemon.log"),
        };
        let mut opts = RunnerOptions::for_daemon(
            BOX,
            &paths.data_dir,
            DaemonOptions {
                socket: paths.socket.clone(),
                hub_key,
                lock: paths.lock.clone(),
                build: Some(build.to_string()),
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
        // (It waits for the locks a stopped daemon may still hold for a moment.)
        let lbox = tokio::task::spawn_blocking(move || LocalBox::open(o))
            .await
            .unwrap()
            .unwrap();
        let (tx, rx) = oneshot::channel::<()>();
        let task = tokio::spawn(twr_runner::run_with(opts, lbox.clone(), async move {
            let _ = rx.await;
        }));
        let deadline = tokio::time::Instant::now() + WAIT;
        while request(&paths.socket, Request::Status).await.is_err() {
            assert!(!task.is_finished(), "the daemon ended");
            assert!(
                tokio::time::Instant::now() < deadline,
                "the daemon's socket"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        Daemon {
            socket: paths.socket,
            lbox: Some(lbox),
            stop: Some(tx),
            task: Some(task),
        }
    }

    fn live_count(&self) -> usize {
        self.lbox.as_ref().unwrap().live_count()
    }

    /// Stops the daemon as the relay's `--replace` does (a `stop` request on its socket) and
    /// waits until it has ended.
    async fn stop(mut self) {
        let _ = request(&self.socket, Request::Stop).await;
        self.lbox = None;
        if let Some(task) = self.task.take() {
            tokio::time::timeout(WAIT, task)
                .await
                .expect("the daemon stops")
                .unwrap()
                .unwrap();
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

/// Connects `hub` to `daemon` as W3's tests do (no supervisor), served in the background.
async fn connect(hub: &Hub, daemon: &Daemon, key: &HubKey) -> tokio::task::JoinHandle<SealedEnd> {
    let (stream, _) = request(&daemon.socket, Request::Connect).await.unwrap();
    let (sealed, _) = tokio::time::timeout(WAIT, hub_side(stream, key, BOX))
        .await
        .unwrap()
        .unwrap();
    let ws = tokio_tungstenite::accept_async_with_config(sealed, Some(pre_auth_ws_config()))
        .await
        .unwrap();
    let hub = hub.clone();
    tokio::spawn(async move { hub.serve_sealed_runner(ws, BOX).await })
}

/// The bytes between the hub and a daemon's connection: copied both ways, those the hub sends
/// held back while `hold` is set (a slow link).
fn pump(daemon: UnixStream, hold: watch::Receiver<bool>) -> tokio::io::DuplexStream {
    let (hub_end, mine) = tokio::io::duplex(1 << 16);
    let (mut dr, mut dw) = daemon.into_split();
    let (mut mr, mut mw) = tokio::io::split(mine);
    tokio::spawn(async move {
        let _ = tokio::io::copy(&mut dr, &mut mw).await;
        let _ = mw.shutdown().await;
    });
    tokio::spawn(async move {
        let mut hold = hold;
        let mut buf = vec![0u8; 16 << 10];
        loop {
            let n = match mr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            let _ = hold.wait_for(|h| !*h).await;
            if dw.write_all(&buf[..n]).await.is_err() {
                break;
            }
        }
        let _ = dw.shutdown().await;
    });
    hub_end
}

/// The box as the test dialer sees it.
struct BoxSide {
    dir: PathBuf,
    hub_key: [u8; 32],
    daemon: Option<Daemon>,
    /// The daemon runs the build the hub launched (it replaced the first one).
    replaced: bool,
    /// Whether each dial was to replace the daemon.
    dials: Vec<bool>,
}

/// The second hub's dialer: the first daemon is of another build than the one the hub takes
/// itself to have launched ([`LAUNCHED`]); a dial that replaces it stops it and starts the
/// launched build's (this binary's) on the same data directory.
fn dialer(side: Arc<tokio::sync::Mutex<BoxSide>>, hold: watch::Receiver<bool>) -> BoxDialer {
    Arc::new(move |name: String, replace: bool| {
        let side = Arc::clone(&side);
        let hold = hold.clone();
        async move {
            assert_eq!(name, BOX);
            let mut side = side.lock().await;
            side.dials.push(replace);
            if replace && !side.replaced {
                if let Some(old) = side.daemon.take() {
                    old.stop().await;
                }
                let d = Daemon::start(&side.dir, side.hub_key).await;
                side.daemon = Some(d);
                side.replaced = true;
            }
            let launched = if side.replaced { own_build() } else { LAUNCHED };
            let socket = side.daemon.as_ref().expect("a daemon").socket.clone();
            let (stream, _) = request(&socket, Request::Connect)
                .await
                .map_err(|e| std::io::Error::other(format!("{e:#}")))?;
            let io: Box<dyn BoxIo> = Box::new(pump(stream, hold));
            Ok((io, launched.to_string()))
        }
        .boxed()
    })
}

/// The first hub: connected to `daemon` without a supervisor, it makes threads `names` on the
/// box and runs a turn on each (their runtimes stay up, idle); with `ask`, the first thread's
/// next turn then waits on a permission. It shuts down (the runtimes keep running on the
/// box). Returns the threads' ids.
async fn first_hub(
    dir: &Path,
    daemon: &Daemon,
    key: &HubKey,
    names: &[&str],
    ask: bool,
) -> Vec<String> {
    let hub = Hub::open(hub_opts(dir)).await.unwrap();
    let served = connect(&hub, daemon, key).await;
    let h = hub.clone();
    wait_for("the box online", move || online(&h)).await;
    let project = dir.join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let mut ids = Vec::new();
    for name in names {
        let req = NewThread {
            project: project.to_string_lossy().into_owned(),
            runtime: Some("fake".into()),
            name: Some(name.to_string()),
            box_name: Some(BOX.into()),
            ..Default::default()
        };
        let t = hub.new_thread(req, "test").await.unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        hub.send(&t, "alpha beta", TurnFrom::User, user()).unwrap();
        until_turn_event(&hub, &t, "turn.ended", 1).await;
        ids.push(t);
    }
    if ask {
        hub.send(&ids[0], "!perm Bash rm -rf build", TurnFrom::User, user())
            .unwrap();
        until_turn_event(&hub, &ids[0], "attention.raised", 2).await;
    }
    hub.shutdown().await;
    let _ = tokio::time::timeout(WAIT, served).await;
    assert_eq!(daemon.live_count(), names.len());
    ids
}

/// The second hub, dialing the box through `side` with the switch's timing `timing`.
async fn second_hub(
    dir: &Path,
    side: &Arc<tokio::sync::Mutex<BoxSide>>,
    hold: watch::Receiver<bool>,
    timing: SwitchTiming,
) -> Hub {
    let mut opts = hub_opts(dir);
    opts.box_switch = timing;
    opts.box_dialer = Some(dialer(Arc::clone(side), hold));
    Hub::open(opts).await.unwrap()
}

/// The hubs' box key (`<data dir>/box-key`), made now: the daemon serves its holder.
fn hub_key(dir: &Path) -> HubKey {
    let data = dir.join("hub");
    std::fs::create_dir_all(&data).unwrap();
    HubKey::load_or_create(&data.join(twr_hub::ssh_box::BOX_KEY_FILE)).unwrap()
}

fn user() -> Source {
    Source::User("test".into())
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

fn events(hub: &Hub, thread: &str) -> Vec<Event> {
    hub.thread_events(thread, 0, 100_000).unwrap().1
}

fn count(hub: &Hub, thread: &str, kind: &str) -> usize {
    events(hub, thread)
        .iter()
        .filter(|e| e.kind() == kind)
        .count()
}

/// The reasons (`ext.twr.reason`, `None` for none) of `thread`'s `thread.stopped` events.
fn stop_reasons(hub: &Hub, thread: &str) -> Vec<Option<String>> {
    events(hub, thread)
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::ThreadStopped(p) => Some(
                p.ext
                    .as_ref()
                    .and_then(|x| x.get("twr")?.get("reason")?.as_str())
                    .map(str::to_string),
            ),
            _ => None,
        })
        .collect()
}

/// `thread`'s error items (raised by the hub or its runtime).
fn error_items(hub: &Hub, thread: &str) -> usize {
    events(hub, thread)
        .iter()
        .filter(|e| matches!(&e.body, EventBody::AttentionRaised(a) if a.reason == AttentionReason::Error))
        .count()
}

fn upgrade() -> Option<String> {
    Some(BOX_UPGRADE_REASON.to_string())
}

// ------------------------------------------------------------------------------------------
// Tests
// ------------------------------------------------------------------------------------------

/// A box whose daemon runs another build, quiet with idle runtimes, is switched: while the
/// stops are on their way (the link held), the box shows `switching`, a prompt to one thread
/// is queued, a `thread.stop` of another wins, and a new thread, a park or a deletion on the
/// box is refused; then the runtimes end `box_upgrade` (their
/// threads not stopped, no crash, no error item), the supervisor replaces the daemon, the
/// queued prompt runs once on the new daemon resuming its session, and the next prompt of the
/// third thread resumes its session too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_quiet_box_switches_to_the_launched_build() {
    let dir = tempfile::tempdir().unwrap();
    let key = hub_key(dir.path());
    let box_dir = dir.path().join("box");
    let daemon = Daemon::start(&box_dir, key.public()).await;
    let ids = first_hub(
        dir.path(),
        &daemon,
        &key,
        &["queued", "stopped", "idle"],
        false,
    )
    .await;
    let (queued, stopped, idle) = (&ids[0], &ids[1], &ids[2]);

    let (hold_tx, hold) = watch::channel(false);
    let side = Arc::new(tokio::sync::Mutex::new(BoxSide {
        dir: box_dir.clone(),
        hub_key: key.public(),
        daemon: Some(daemon),
        replaced: false,
        dials: Vec::new(),
    }));
    let timing = SwitchTiming {
        quiet: Duration::from_millis(1500),
        check: Duration::from_millis(50),
        drain: Duration::from_secs(20),
    };
    let hub = second_hub(dir.path(), &side, hold, timing).await;
    let h = hub.clone();
    wait_for("the box online", move || online(&h)).await;
    // What the hub sends the box waits from here on.
    hold_tx.send(true).unwrap();
    let b = box_info(&hub);
    assert_eq!(
        (&b["state"], &b["switch_pending"], &b["build"]),
        (
            &Value::from("online"),
            &Value::from(true),
            &Value::from(LAUNCHED)
        ),
        "{b}"
    );
    let session = |t: &str| hub.get_thread(t).unwrap()["session_id"].clone();
    let sessions: Vec<Value> = ids.iter().map(|t| session(t)).collect();
    assert!(sessions.iter().all(Value::is_string), "{sessions:?}");

    let h = hub.clone();
    wait_for("the switch", move || box_info(&h)["state"] == "switching").await;
    assert!(has_note(&hub, &format!("switching to build {LAUNCHED}")));
    // During the drain: a prompt is queued, never refused, and a stop wins.
    let sent = hub
        .send(queued, "after the switch", TurnFrom::User, user())
        .unwrap();
    assert_eq!((sent.turn, sent.pending), (None, 1), "{sent:?}");
    let stop = hub.stop_thread(stopped, "test").unwrap();
    assert_eq!(stop["stopping"], true, "{stop}");
    // Repository requests on the box are refused meanwhile (the switch would cut them off),
    // saying why: a new thread, a park, a deletion.
    let switching = |e: RpcError| {
        assert_eq!(e.code, codes::BOX_OFFLINE, "{}", e.message);
        assert!(
            e.message
                .contains("is switching to the hub's twr build: try again in a minute"),
            "{}",
            e.message
        );
    };
    let late = NewThread {
        project: dir.path().join("proj").to_string_lossy().into_owned(),
        runtime: Some("fake".into()),
        name: Some("late".into()),
        box_name: Some(BOX.into()),
        ..Default::default()
    };
    switching(hub.new_thread(late, "test").await.unwrap_err());
    match hub
        .park_thread(idle, false, false, &Caller::user("test"))
        .await
    {
        Err(e) => switching(e),
        Ok(_) => panic!("a park during the switch"),
    }
    switching(
        hub.delete_threads(std::slice::from_ref(idle), &Caller::user("test"))
            .await
            .unwrap_err(),
    );
    hold_tx.send(false).unwrap();

    // The daemon is replaced and the box comes back on the launched build.
    let h = hub.clone();
    wait_for("the box back on the launched build", move || {
        let b = box_info(&h);
        b["state"] == "online" && b["switch_pending"] == false
    })
    .await;
    let b = box_info(&hub);
    assert_eq!(b["build"], own_build(), "{b}");
    assert!(
        has_note(&hub, &format!("now on build {}", own_build())),
        "{b}"
    );
    assert_eq!(side.lock().await.dials, [false, true]);
    // The queued prompt runs once, on its session.
    until_turn_event(&hub, queued, "turn.ended", 2).await;
    let prompts: Vec<String> = events(&hub, queued)
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::TurnStarted(t) => Some(t.prompt.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(prompts, ["alpha beta", "after the switch"]);
    assert_eq!(stop_reasons(&hub, queued), [upgrade()]);
    assert_eq!(session(queued), sessions[0]);
    // The stopped one stays stopped: a plain stop, nothing more run.
    assert_eq!(stop_reasons(&hub, stopped), [None]);
    assert_eq!(hub.get_thread(stopped).unwrap()["stopped"], true);
    assert_eq!(count(&hub, stopped, "turn.started"), 1);
    // The idle one ended `box_upgrade`, is not stopped, and its next prompt resumes it.
    assert_eq!(stop_reasons(&hub, idle), [upgrade()]);
    assert_eq!(hub.get_thread(idle).unwrap()["stopped"], false);
    hub.send(idle, "gamma", TurnFrom::User, user()).unwrap();
    until_turn_event(&hub, idle, "turn.ended", 2).await;
    assert_eq!(session(idle), sessions[2]);
    for t in &ids {
        assert_eq!(count(&hub, t, "thread.crashed"), 0, "{t}");
        assert_eq!(error_items(&hub, t), 0, "{t}");
    }
    assert_eq!(
        side.lock().await.daemon.as_ref().unwrap().live_count(),
        2,
        "the resumed sessions run on the new daemon"
    );
    hub.shutdown().await;
}

/// A box whose daemon runs another build is not switched while it is busy: a turn waiting on a
/// permission (an open item) and another running. Once both are over and it has been quiet
/// long enough, it is switched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_busy_box_is_switched_once_quiet() {
    let dir = tempfile::tempdir().unwrap();
    let key = hub_key(dir.path());
    let box_dir = dir.path().join("box");
    let daemon = Daemon::start(&box_dir, key.public()).await;
    let ids = first_hub(dir.path(), &daemon, &key, &["asks", "works"], true).await;
    let (asks, works) = (&ids[0], &ids[1]);

    let (_hold_tx, hold) = watch::channel(false);
    let side = Arc::new(tokio::sync::Mutex::new(BoxSide {
        dir: box_dir.clone(),
        hub_key: key.public(),
        daemon: Some(daemon),
        replaced: false,
        dials: Vec::new(),
    }));
    let timing = SwitchTiming {
        quiet: Duration::from_millis(300),
        check: Duration::from_millis(50),
        drain: Duration::from_secs(20),
    };
    let hub = second_hub(dir.path(), &side, hold, timing).await;
    let h = hub.clone();
    wait_for("the box online", move || online(&h)).await;
    hub.send(works, "!sleep 1500\nslow", TurnFrom::User, user())
        .unwrap();
    let not_switched = |hub: &Hub| {
        let b = box_info(hub);
        assert_eq!(
            (&b["state"], &b["switch_pending"]),
            (&Value::from("online"), &Value::from(true)),
            "{b}"
        );
        assert!(!has_note(hub, &format!("switching to build {LAUNCHED}")));
        for t in [asks, works] {
            assert!(stop_reasons(hub, t).is_empty(), "{t}");
        }
    };
    tokio::time::sleep(Duration::from_secs(1)).await;
    not_switched(&hub);
    // The running turn is over; the permission is still open.
    until_turn_event(&hub, works, "turn.ended", 2).await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    not_switched(&hub);
    assert_eq!(side.lock().await.dials, [false]);
    let attn = hub
        .attention_list(false)
        .into_iter()
        .find(|i| i["thread"] == asks.as_str())
        .expect("the permission")["attn_id"]
        .as_str()
        .unwrap()
        .to_string();
    hub.respond(&attn, Answer::allow(), "test").unwrap();
    until_turn_event(&hub, asks, "turn.ended", 2).await;

    let h = hub.clone();
    wait_for("the box back on the launched build", move || {
        let b = box_info(&h);
        b["state"] == "online" && b["switch_pending"] == false
    })
    .await;
    assert_eq!(side.lock().await.dials, [false, true]);
    for t in [asks, works] {
        assert_eq!(stop_reasons(&hub, t), [upgrade()], "{t}");
        assert_eq!(hub.get_thread(t).unwrap()["stopped"], false, "{t}");
        assert_eq!(count(&hub, t, "thread.crashed"), 0, "{t}");
    }
    hub.send(asks, "again", TurnFrom::User, user()).unwrap();
    until_turn_event(&hub, asks, "turn.ended", 3).await;
    hub.shutdown().await;
}

/// A box of another OS or CPU runs its configured build (`[boxes.<b>] build`). `twr box
/// install` records another one and `box.install` hands it to the hub while the box is online
/// on the first: the box becomes `switch_pending` (its `running_build` the first), is switched
/// once quiet like any box, and the next dial replaces the daemon (`--replace`) with the newly
/// configured build, which it then runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_installed_build_replaces_the_running_one() {
    const FIRST: &str = "00000000000000b1";
    const SECOND: &str = "00000000000000b2";
    let dir = tempfile::tempdir().unwrap();
    let key = hub_key(dir.path());
    let box_dir = dir.path().join("box");
    let config_file = dir.path().join("config.toml");
    let boxes_file = dir.path().join("boxes.toml");
    std::fs::write(&config_file, "default_runtime = \"fake\"\n").unwrap();
    let boxes = |build: &str| {
        std::fs::write(
            &boxes_file,
            format!(
                "[boxes.{BOX}]\ntransport = \"ssh\"\nssh = \"me@gpu.invalid\"\nbuild = \
                 \"{build}\"\n"
            ),
        )
        .unwrap()
    };
    boxes(FIRST);
    let daemon = Daemon::start_as(&box_dir, key.public(), FIRST).await;
    // The dialer launches the configured build: the first, then (replacing) the second.
    let dials = Arc::new(std::sync::Mutex::new(Vec::new()));
    let side = Arc::new(tokio::sync::Mutex::new(Some(daemon)));
    let dialer: BoxDialer = {
        let (dials, side, box_dir, hub_key) = (
            Arc::clone(&dials),
            Arc::clone(&side),
            box_dir.clone(),
            key.public(),
        );
        Arc::new(move |name: String, replace: bool| {
            let (dials, side, box_dir) = (Arc::clone(&dials), Arc::clone(&side), box_dir.clone());
            async move {
                assert_eq!(name, BOX);
                dials.lock().unwrap().push(replace);
                let mut side = side.lock().await;
                let launched = if replace { SECOND } else { FIRST };
                if replace {
                    if let Some(old) = side.take() {
                        old.stop().await;
                    }
                    *side = Some(Daemon::start_as(&box_dir, hub_key, SECOND).await);
                }
                let socket = side.as_ref().expect("a daemon").socket.clone();
                let (stream, _) = request(&socket, Request::Connect)
                    .await
                    .map_err(|e| std::io::Error::other(format!("{e:#}")))?;
                let io: Box<dyn BoxIo> = Box::new(stream);
                Ok((io, launched.to_string()))
            }
            .boxed()
        })
    };
    let mut opts = hub_opts(dir.path());
    opts.config = Config::load_with_boxes(&config_file).unwrap();
    opts.config_file = Some(config_file.clone());
    opts.box_switch = SwitchTiming {
        quiet: Duration::from_millis(300),
        check: Duration::from_millis(50),
        drain: Duration::from_secs(20),
    };
    opts.box_dialer = Some(dialer);
    let hub = Hub::open(opts).await.unwrap();
    let h = hub.clone();
    wait_for("the box online", move || online(&h)).await;
    let b = box_info(&hub);
    assert_eq!(
        (&b["build"], &b["running_build"], &b["switch_pending"]),
        (
            &Value::from(FIRST),
            &Value::from(FIRST),
            &Value::from(false)
        ),
        "{b}"
    );
    // Nothing to switch to: the box stays as it is.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(*dials.lock().unwrap(), [false]);

    // `twr box install` recorded the second build.
    boxes(SECOND);
    hub.box_install(BOX, &Caller::user("test")).await.unwrap();
    let h = hub.clone();
    wait_for("the switch to the installed build", move || {
        has_note(&h, &format!("switching to build {SECOND}"))
    })
    .await;
    let h = hub.clone();
    wait_for("the box online on the installed build", move || {
        let b = box_info(&h);
        online(&h) && b["running_build"] == SECOND
    })
    .await;
    let b = box_info(&hub);
    assert_eq!(
        (&b["build"], &b["switch_pending"], &b["state"]),
        (
            &Value::from(SECOND),
            &Value::from(false),
            &Value::from("online")
        ),
        "{b}"
    );
    assert!(has_note(&hub, &format!("now on build {SECOND}")), "{b}");
    assert_eq!(*dials.lock().unwrap(), [false, true]);
    hub.shutdown().await;
}

/// A new thread whose workspace is being made on the box (its requests held on the link)
/// keeps the box from being switched, however long the box is otherwise quiet: it passed the
/// gate and holds the box's operation (`Core::reserve_box`). Once it is recorded (and has run
/// a turn) the switch begins, and a new thread asked for then (the gate set, the stop of the
/// thread's runtime held on the link) is refused, saying the box is switching.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_thread_being_made_keeps_the_box_from_switching() {
    let dir = tempfile::tempdir().unwrap();
    let key = hub_key(dir.path());
    let box_dir = dir.path().join("box");
    let daemon = Daemon::start(&box_dir, key.public()).await;
    let (hold_tx, hold) = watch::channel(false);
    let side = Arc::new(tokio::sync::Mutex::new(BoxSide {
        dir: box_dir.clone(),
        hub_key: key.public(),
        daemon: Some(daemon),
        replaced: false,
        dials: Vec::new(),
    }));
    let quiet = Duration::from_millis(500);
    let timing = SwitchTiming {
        quiet,
        check: Duration::from_millis(50),
        drain: Duration::from_secs(20),
    };
    let hub = second_hub(dir.path(), &side, hold, timing).await;
    let h = hub.clone();
    wait_for("the box online", move || online(&h)).await;
    assert_eq!(box_info(&hub)["switch_pending"], true);
    let project = dir.path().join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let thread = |name: &str| NewThread {
        project: project.to_string_lossy().into_owned(),
        runtime: Some("fake".into()),
        name: Some(name.into()),
        box_name: Some(BOX.into()),
        ..Default::default()
    };

    // The thread's requests to the box wait on the link: it is being made.
    hold_tx.send(true).unwrap();
    let making = {
        let (hub, req) = (hub.clone(), thread("made"));
        tokio::spawn(async move { hub.new_thread(req, "test").await })
    };
    tokio::time::sleep(quiet * 4).await;
    let switch_note = format!("switching to build {LAUNCHED}");
    assert!(!has_note(&hub, &switch_note), "{}", box_info(&hub));
    assert_eq!(box_info(&hub)["state"], "online");
    assert!(!making.is_finished());
    hold_tx.send(false).unwrap();
    let made = making.await.unwrap().unwrap();
    assert_eq!(made["box_name"], BOX, "{made}");
    let made = made["id"].as_str().unwrap().to_string();
    hub.send(&made, "alpha beta", TurnFrom::User, user())
        .unwrap();
    until_turn_event(&hub, &made, "turn.ended", 1).await;

    // Quiet now: the switch begins. Its stop of the thread's runtime held on the link, the
    // gate stays set while the box is online.
    hold_tx.send(true).unwrap();
    let h = hub.clone();
    wait_for("the switch", move || box_info(&h)["state"] == "switching").await;
    assert!(has_note(&hub, &switch_note));
    let e = hub.new_thread(thread("late"), "test").await.unwrap_err();
    assert_eq!(e.code, codes::BOX_OFFLINE, "{}", e.message);
    assert!(
        e.message
            .contains("is switching to the hub's twr build: try again in a minute"),
        "{}",
        e.message
    );
    hold_tx.send(false).unwrap();
    let h = hub.clone();
    wait_for("the box back on the launched build", move || {
        online(&h) && box_info(&h)["state"] == "online" && box_info(&h)["switch_pending"] == false
    })
    .await;
    assert_eq!(side.lock().await.dials, [false, true]);
    hub.shutdown().await;
}

/// A switch that does not take (the dial that was to replace the daemon reaches the same old
/// one: a relay that could not replace it) is not tried again in a loop: the box says it
/// still runs its build after a switch to the launched one, and stays as it is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_switch_that_does_not_take_is_not_tried_again() {
    let dir = tempfile::tempdir().unwrap();
    let key = hub_key(dir.path());
    let daemon = Daemon::start(&dir.path().join("box"), key.public()).await;
    let socket = daemon.socket.clone();
    let dials = Arc::new(std::sync::Mutex::new(Vec::new()));
    let dialer: BoxDialer = {
        let dials = Arc::clone(&dials);
        Arc::new(move |_name: String, replace: bool| {
            let (dials, socket) = (Arc::clone(&dials), socket.clone());
            async move {
                dials.lock().unwrap().push(replace);
                let (stream, _) = request(&socket, Request::Connect)
                    .await
                    .map_err(|e| std::io::Error::other(format!("{e:#}")))?;
                let io: Box<dyn BoxIo> = Box::new(stream);
                // Always "launched" the other build; the daemon is never replaced.
                Ok((io, LAUNCHED.to_string()))
            }
            .boxed()
        })
    };
    let quiet = Duration::from_millis(300);
    let mut opts = hub_opts(dir.path());
    opts.box_switch = SwitchTiming {
        quiet,
        check: Duration::from_millis(50),
        drain: Duration::from_secs(20),
    };
    opts.box_dialer = Some(dialer);
    let hub = Hub::open(opts).await.unwrap();
    let h = hub.clone();
    wait_for("the switch that does not take", move || {
        box_info(&h)["last_error"].as_str().is_some_and(|e| {
            e.contains(&format!(
                "still runs build {} after a switch to build {LAUNCHED}",
                own_build()
            ))
        })
    })
    .await;
    let h = hub.clone();
    wait_for("the box online", move || online(&h)).await;
    // Quiet for many times its quiet period: no further switch.
    tokio::time::sleep(quiet * 8).await;
    assert_eq!(*dials.lock().unwrap(), [false, true]);
    let b = box_info(&hub);
    assert_eq!(b["state"], "online", "{b}");
    assert_eq!(b["switch_pending"], true, "{b}");
    hub.shutdown().await;
    drop(daemon);
}
