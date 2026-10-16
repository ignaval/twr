//! An SSH box's daemon driven over its socket as the hub drives it (through the relay): the
//! requests, the sealed channel with the hub's key, the runner protocol inside it, sessions
//! replaced by newer connections, and the relay that starts the daemon.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use twr_core::runner::{
    new_nonce, CmdResult, HubMsg, Inventory, Op, RunnerMsg, WireSpawnConfig, PROTOCOL_VERSION,
    SEALED_AUTH_PROOF,
};
use twr_core::sealed::{hub_side, HubKey};
use twr_runner::daemon::{request, Request};
use twr_runner::relay::{DaemonPaths, Launcher, RelayOptions};
use twr_runner::{DaemonOptions, LocalBox, LocalBoxOptions, RunnerOptions};

const BOX: &str = "gpu";

/// What the fake hub saw, over every connection.
#[derive(Default)]
struct HubState {
    watermarks: BTreeMap<String, u64>,
    epoch: u64,
    inventories: Vec<Inventory>,
    /// Frames received past a gap (never committed).
    gaps: u64,
    /// Committed frames: (thread, rseq, kind), `exited` for exits.
    committed: Vec<(String, u64, String)>,
    results: Vec<CmdResult>,
    synced: u64,
}

#[derive(Clone, Default)]
struct Hub {
    state: Arc<Mutex<HubState>>,
}

/// One connection of the fake hub, past the welcome.
struct HubConn {
    cmds: mpsc::UnboundedSender<HubMsg>,
    /// Set when the connection ended.
    ended: Arc<Mutex<bool>>,
}

impl HubConn {
    fn cmd(&self, cmd_id: &str, op: Op) {
        self.cmds
            .send(HubMsg::Cmd {
                cmd_id: cmd_id.into(),
                op,
            })
            .unwrap();
    }

    fn ended(&self) -> bool {
        *self.ended.lock().unwrap()
    }
}

impl Hub {
    fn state(&self) -> std::sync::MutexGuard<'_, HubState> {
        self.state.lock().unwrap()
    }

    /// Connects to the daemon on `socket` as the hub with `key`: the `connect` request, the
    /// sealed channel, the WebSocket inside it and the handshake, up to the welcome. Then
    /// serves the connection in the background (commits and acks frames, records results).
    async fn connect(&self, socket: &Path, key: &HubKey) -> HubConn {
        let (stream, status) = request(socket, Request::Connect).await.unwrap();
        assert_eq!(status.hub_id, key.id());
        assert_eq!(status.protocol, PROTOCOL_VERSION);
        assert_eq!(status.build.as_deref(), twr_core::build_id());
        self.over(stream, key).await
    }

    /// [`Hub::connect`] over a stream to the daemon whose request is made (a relay's).
    async fn over<S>(&self, stream: S, key: &HubKey) -> HubConn
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (sealed, hello) =
            tokio::time::timeout(Duration::from_secs(15), hub_side(stream, key, BOX))
                .await
                .expect("the daemon's hello")
                .unwrap();
        assert_eq!(hello.box_name, BOX);
        assert_eq!(hello.build.as_deref(), twr_core::build_id());
        let ws = tokio_tungstenite::accept_async(sealed).await.unwrap();
        let (mut sink, mut stream) = ws.split();
        let text = |m: &HubMsg| Message::Text(serde_json::to_string(m).unwrap().into());
        let mut next = async || -> RunnerMsg {
            loop {
                match stream.next().await {
                    Some(Ok(Message::Text(t))) => return serde_json::from_str(t.as_str()).unwrap(),
                    Some(Ok(_)) => continue,
                    other => panic!("the runner closed the connection: {other:?}"),
                }
            }
        };
        let RunnerMsg::Hello(h) = next().await else {
            panic!("no hello")
        };
        assert_eq!((h.box_name.as_str(), h.protocol), (BOX, PROTOCOL_VERSION));
        // Any proof: a daemon does not check it (the channel proved the hub).
        sink.send(text(&HubMsg::Challenge {
            nonce: new_nonce().unwrap(),
            proof: new_nonce().unwrap(),
        }))
        .await
        .unwrap();
        let RunnerMsg::Auth { proof } = next().await else {
            panic!("no auth")
        };
        assert_eq!(proof, SEALED_AUTH_PROOF);
        sink.send(text(&HubMsg::Authenticated)).await.unwrap();
        let RunnerMsg::Inventory(inv) = next().await else {
            panic!("no inventory")
        };
        assert_eq!(inv.build.as_deref(), twr_core::build_id());
        assert!(inv.build.is_some());
        let welcome = {
            let mut s = self.state();
            s.epoch += 1;
            s.inventories.push(inv);
            HubMsg::Welcome {
                epoch: s.epoch,
                watermarks: s.watermarks.clone(),
            }
        };
        sink.send(text(&welcome)).await.unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel::<HubMsg>();
        let ended = Arc::new(Mutex::new(false));
        let hub = self.clone();
        let done = ended.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    m = rx.recv() => {
                        let Some(m) = m else { break };
                        if sink.send(text(&m)).await.is_err() { break; }
                    }
                    f = stream.next() => {
                        let Some(Ok(f)) = f else { break };
                        let Message::Text(t) = f else {
                            if matches!(f, Message::Close(_)) { break; }
                            continue;
                        };
                        let (thread, rseq, kind) = match serde_json::from_str(t.as_str()).unwrap() {
                            RunnerMsg::Event { thread, rseq, body, .. } => {
                                (thread, rseq, body.kind().to_string())
                            }
                            RunnerMsg::Exited { thread, rseq, .. } => (thread, rseq, "exited".into()),
                            RunnerMsg::CmdResult(r) => { hub.state().results.push(r); continue; }
                            RunnerMsg::Synced => { hub.state().synced += 1; continue; }
                            RunnerMsg::Ping => {
                                if sink.send(text(&HubMsg::Pong)).await.is_err() { break; }
                                continue;
                            }
                            _ => continue,
                        };
                        {
                            let mut s = hub.state();
                            let wm = s.watermarks.get(&thread).copied().unwrap_or(0);
                            if rseq > wm + 1 {
                                s.gaps += 1;
                            }
                            if rseq == wm + 1 {
                                s.watermarks.insert(thread.clone(), rseq);
                                s.committed.push((thread.clone(), rseq, kind));
                            }
                        }
                        if sink.send(text(&HubMsg::Ack { thread, rseq })).await.is_err() { break; }
                    }
                }
            }
            *done.lock().unwrap() = true;
        });
        HubConn { cmds: tx, ended }
    }

    async fn wait(&self, what: &str, cond: impl Fn(&HubState) -> bool) {
        wait_for(what, || cond(&self.state())).await;
    }

    /// The committed frames of `thread`: (rseq, kind).
    fn committed(&self, thread: &str) -> Vec<(u64, String)> {
        self.state()
            .committed
            .iter()
            .filter(|c| c.0 == thread)
            .map(|c| (c.1, c.2.clone()))
            .collect()
    }
}

async fn wait_for(what: &str, cond: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !cond() {
        if tokio::time::Instant::now() > deadline {
            panic!("timed out waiting for {what}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn init_logs() {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();
}

/// The built-in adapters with the fake one (production registries leave it out).
fn with_fake(mut o: LocalBoxOptions) -> LocalBoxOptions {
    o.adapters = Arc::new(|n: &str| {
        twr_adapters::registry_with(true)
            .into_iter()
            .find(|a| a.info().name == n)
    });
    o.runtimes = twr_adapters::registry_with(true)
        .iter()
        .map(|a| a.info().name.to_string())
        .collect();
    o
}

fn spawn_op(thread: &str, prompt: &str) -> Op {
    Op::Spawn {
        thread: thread.into(),
        runtime: "fake".into(),
        cfg: WireSpawnConfig {
            cwd: std::env::temp_dir(),
            ..Default::default()
        },
        prompt: Some(prompt.into()),
        run: None,
    }
}

fn assert_contiguous(frames: &[(u64, String)]) {
    let rseqs: Vec<u64> = frames.iter().map(|f| f.0).collect();
    let want: Vec<u64> = (1..=rseqs.len() as u64).collect();
    assert_eq!(rseqs, want, "{frames:?}");
}

/// Paths of a daemon under `dir` (short: the socket's path must fit).
fn paths(dir: &Path) -> DaemonPaths {
    DaemonPaths {
        socket: dir.join("st/box-daemons/d.sock"),
        data_dir: dir.join("data"),
        lock: dir.join("st/locks/gpu.lock"),
        log: dir.join("data/daemon.log"),
    }
}

/// The daemon's options for `paths` and the hub key `key`, with short timeouts.
fn daemon_opts(paths: &DaemonPaths, key: &[u8; 32]) -> RunnerOptions {
    let mut o = RunnerOptions::for_daemon(
        BOX,
        &paths.data_dir,
        DaemonOptions {
            socket: paths.socket.clone(),
            hub_key: *key,
            lock: paths.lock.clone(),
            build: twr_core::build_id().map(str::to_string),
        },
    );
    o.ping_interval = Duration::from_millis(200);
    o.idle_timeout = Duration::from_secs(5);
    o.welcome_timeout = Duration::from_secs(5);
    o.shutdown_grace = Duration::from_secs(3);
    o
}

/// A daemon running in this process.
struct Daemon {
    paths: DaemonPaths,
    lbox: LocalBox,
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Daemon {
    async fn start(dir: &Path, key: &HubKey) -> Daemon {
        init_logs();
        let paths = paths(dir);
        let opts = daemon_opts(&paths, &key.public());
        assert!(opts.validate().is_ok(), "a daemon needs no token or URL");
        let lbox = LocalBox::open(with_fake(opts.box_options())).unwrap();
        let (tx, rx) = oneshot::channel::<()>();
        let task = tokio::spawn(twr_runner::run_with(opts, lbox.clone(), async move {
            let _ = rx.await;
        }));
        let socket = paths.socket.clone();
        wait_for_async("the daemon's socket", || {
            let socket = socket.clone();
            async move { request(&socket, Request::Status).await.is_ok() }
        })
        .await;
        Daemon {
            paths,
            lbox,
            stop: Some(tx),
            task,
        }
    }

    /// Waits for the daemon to end on its own (a `stop` request): its result.
    async fn ended(&mut self) -> anyhow::Result<()> {
        tokio::time::timeout(Duration::from_secs(20), &mut self.task)
            .await
            .expect("the daemon did not end")
            .unwrap()
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(tx) = self.stop.take() {
            let _ = tx.send(());
        }
    }
}

async fn wait_for_async<F: std::future::Future<Output = bool>>(what: &str, cond: impl Fn() -> F) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
    while !cond().await {
        if tokio::time::Instant::now() > deadline {
            panic!("timed out waiting for {what}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Everything the daemon sends on a connection that says `line` first, until it closes.
async fn answer_to(socket: &Path, line: &[u8]) -> Vec<u8> {
    let mut s = UnixStream::connect(socket).await.unwrap();
    s.write_all(line).await.unwrap();
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), s.read_to_end(&mut out))
        .await
        .expect("the daemon kept the connection open")
        .unwrap_or_default();
    out
}

/// The hub drives the daemon through its socket: a sealed connection with the hub's key, the
/// runner protocol inside (the empty proof, the build in the inventory), a runtime's events.
/// `status` answers and closes; `stop` answers, stops the runtime, delivers its exit and ends
/// the daemon, its socket gone.
#[tokio::test]
async fn the_hub_drives_the_daemon_over_its_socket_and_stop_ends_it() {
    let dir = tempfile::tempdir().unwrap();
    let key = HubKey::generate().unwrap();
    let mut d = Daemon::start(dir.path(), &key).await;
    let hub = Hub::default();
    let conn = hub.connect(&d.paths.socket, &key).await;
    hub.wait("synced", |s| s.synced == 1).await;
    {
        let s = hub.state();
        let inv = &s.inventories[0];
        assert!(inv.runtimes.contains(&"fake".to_string()));
        assert_eq!(inv.worktrees, d.paths.data_dir.join("worktrees"));
    }
    conn.cmd("c1", spawn_op("t1", "hello over the socket"));
    hub.wait("turn.ended", |s| {
        s.committed.iter().any(|c| c.2 == "turn.ended")
    })
    .await;
    let frames = hub.committed("t1");
    assert_contiguous(&frames);
    assert_eq!(frames[0].1, "thread.started");
    hub.wait("the spawn's result", |s| {
        s.results.iter().any(|r| r.cmd_id == "c1" && r.ok)
    })
    .await;

    // `status`: the status line, then the daemon closes.
    let out = answer_to(&d.paths.socket, b"twr-box 1 status\n").await;
    let want = twr_runner::daemon::Status::of(&key.public()).line();
    assert_eq!(String::from_utf8(out).unwrap(), want);
    assert_eq!(d.lbox.live_count(), 1);

    // `stop`: the status line, then a graceful shutdown that delivers the exit.
    let out = answer_to(&d.paths.socket, b"twr-box 1 stop\n").await;
    assert_eq!(String::from_utf8(out).unwrap(), want);
    d.ended().await.unwrap();
    let frames = hub.committed("t1");
    assert_eq!(frames.last().unwrap().1, "exited");
    assert_contiguous(&frames);
    assert_eq!(d.lbox.live_count(), 0);
    assert!(d.lbox.spool().is_empty().unwrap());
    assert!(!d.paths.socket.exists(), "the socket outlived the daemon");
    wait_for("the hub's connection to end", || conn.ended()).await;
}

/// A hub that signs with another key than the daemon's gets the channel's hello line and
/// nothing more: no record, no runner protocol. The daemon runs on and serves its own hub.
#[tokio::test]
async fn a_hub_with_another_key_gets_only_the_hello_line() {
    let dir = tempfile::tempdir().unwrap();
    let key = HubKey::generate().unwrap();
    let d = Daemon::start(dir.path(), &key).await;
    let other = HubKey::generate().unwrap();
    let (stream, status) = request(&d.paths.socket, Request::Connect).await.unwrap();
    assert_eq!(status.hub_id, key.id());
    let (mut sealed, hello) =
        tokio::time::timeout(Duration::from_secs(15), hub_side(stream, &other, BOX))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(hello.box_name, BOX);
    // The daemon closes without a record: a clean end of the sealed stream, nothing in it.
    let mut buf = Vec::new();
    let n = tokio::time::timeout(Duration::from_secs(15), sealed.read_to_end(&mut buf))
        .await
        .expect("the daemon kept the connection open")
        .unwrap();
    assert_eq!(n, 0);
    assert!(d.lbox.in_flight().is_empty());
    assert!(d.lbox.spool().is_empty().unwrap());
    // The genuine hub still gets in.
    let hub = Hub::default();
    let _conn = hub.connect(&d.paths.socket, &key).await;
    hub.wait("synced", |s| s.synced == 1).await;
}

/// A second `connect` replaces the connection served: the old one ends as a lost one does,
/// the runtime runs on, and its events continue on the new one in `rseq` order, without a
/// gap or a duplicate committed.
#[tokio::test]
async fn a_newer_connection_replaces_the_served_one_and_events_continue() {
    let dir = tempfile::tempdir().unwrap();
    let key = HubKey::generate().unwrap();
    let d = Daemon::start(dir.path(), &key).await;
    let hub = Hub::default();
    let first = hub.connect(&d.paths.socket, &key).await;
    first.cmd("c1", spawn_op("t1", "the first turn"));
    hub.wait("the first turn", |s| {
        s.committed.iter().any(|c| c.2 == "turn.ended")
    })
    .await;
    let before = hub.committed("t1").len();

    let second = hub.connect(&d.paths.socket, &key).await;
    wait_for("the first connection to end", || first.ended()).await;
    assert!(!second.ended());
    assert_eq!(
        d.lbox.live_count(),
        1,
        "the runtime ended with the connection"
    );
    {
        let s = hub.state();
        let inv = s.inventories.last().unwrap();
        assert_eq!(inv.running.len(), 1);
        assert_eq!(inv.running[0].thread, "t1");
        assert_eq!(inv.epoch_hint, Some(1), "the first welcome's epoch");
    }
    second.cmd(
        "c2",
        Op::Send {
            thread: "t1".into(),
            turn: 2,
            text: "the second turn".into(),
            run: None,
        },
    );
    hub.wait("the second turn", |s| {
        s.committed.iter().filter(|c| c.2 == "turn.ended").count() == 2
    })
    .await;
    let frames = hub.committed("t1");
    assert!(frames.len() > before);
    assert_contiguous(&frames);
    assert_eq!(hub.state().gaps, 0);
}

/// A newer connection waits for the reader of the one it replaces to end before it answers
/// the hub: a repository change the old reader is in the middle of taking (held there, its
/// thread blocked, where aborting its task cannot stop it) is counted and finished before the
/// new connection's hello and inventory, never begun after them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_replaced_connections_change_finishes_before_the_new_inventory() {
    let dir = tempfile::tempdir().unwrap();
    let key = Arc::new(HubKey::generate().unwrap());
    let d = Daemon::start(dir.path(), &key).await;
    let repo = twr_core::testkit::git::init_repo(&dir.path().join("repo"));
    let wt = dir.path().join("wt");
    let hub = Hub::default();
    let first = hub.connect(&d.paths.socket, &key).await;
    let pauses = d.lbox.hold_change_receipts();
    first.cmd(
        "c1",
        Op::WorktreeCreate {
            repo: repo.clone(),
            path: wt.clone(),
            branch: "twr/t1".into(),
            base: None,
        },
    );
    let pause = tokio::task::spawn_blocking(move || pauses.recv_timeout(Duration::from_secs(20)))
        .await
        .unwrap()
        .expect("the reader takes the change");

    // A newer connection, while the old reader is held in the middle of taking the change.
    let second = {
        let (hub, socket, key) = (hub.clone(), d.paths.socket.clone(), Arc::clone(&key));
        tokio::spawn(async move { hub.connect(&socket, &key).await })
    };
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        hub.state().inventories.len(),
        1,
        "the new connection answered while the old reader could still begin a change"
    );
    assert!(!wt.exists());
    pause.go.send(()).unwrap();
    let _second = tokio::time::timeout(Duration::from_secs(20), second)
        .await
        .expect("the new connection is served once the old reader has ended")
        .unwrap();
    // The change was counted, ran and finished before the new inventory.
    let inv = hub.state().inventories.last().cloned().unwrap();
    assert!(!inv.in_flight.contains(&"c1".to_string()), "{inv:?}");
    assert!(wt.join(".git").exists(), "the worktree was made first");
    assert_eq!(d.lbox.changes_running(), 0);
    wait_for("the first connection to end", || first.ended()).await;
}

/// A daemon whose box fails ends the session it serves and joins its reader before it ends:
/// a repository change the reader is in the middle of taking (held there, its thread
/// blocked) is counted and waited for, never begun after the daemon is done.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_box_joins_the_reader_before_the_daemon_ends() {
    let dir = tempfile::tempdir().unwrap();
    let key = HubKey::generate().unwrap();
    let mut d = Daemon::start(dir.path(), &key).await;
    let repo = twr_core::testkit::git::init_repo(&dir.path().join("repo"));
    let hub = Hub::default();
    let first = hub.connect(&d.paths.socket, &key).await;
    let pauses = d.lbox.hold_change_receipts();
    // A runtime that writes again in a moment, then a change the reader is held taking.
    first.cmd("c0", spawn_op("t1", "!sleep 500\nalpha beta"));
    first.cmd(
        "c1",
        Op::WorktreeCreate {
            repo: repo.clone(),
            path: dir.path().join("wt"),
            branch: "twr/t1".into(),
            base: None,
        },
    );
    let pause = tokio::task::spawn_blocking(move || pauses.recv_timeout(Duration::from_secs(20)))
        .await
        .unwrap()
        .expect("the reader takes the change");
    // The runtime's next event cannot be recorded: the box fails.
    d.lbox.spool().inject_failures(1000, 0);
    wait_for("the box failed", || d.lbox.fatal_error().is_some()).await;
    // (Longer than its shutdown grace: what else it waits for is over by then.)
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(
        !d.task.is_finished(),
        "the daemon ended while its reader could still begin a change"
    );
    pause.go.send(()).unwrap();
    let r = d.ended().await;
    assert!(r.is_err(), "{r:?}");
    assert_eq!(d.lbox.changes_running(), 0);
    d.lbox.spool().inject_failures(0, 0);
}

/// A peer whose first line is not a request (or too long a line) is dropped without an
/// answer; the daemon serves on.
#[tokio::test]
async fn a_peer_without_a_valid_request_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let key = HubKey::generate().unwrap();
    let d = Daemon::start(dir.path(), &key).await;
    for line in [
        &b"hello\n"[..],
        b"twr-box 2 connect\n",
        b"twr-box 1 connect now\n",
        b"TWR-BOX-KEY {}\n",
        &[b'x'; 300][..],
    ] {
        let out = answer_to(&d.paths.socket, line).await;
        assert!(
            out.is_empty(),
            "{:?}: {:?}",
            String::from_utf8_lossy(line),
            out
        );
    }
    let (_, status) = request(&d.paths.socket, Request::Status).await.unwrap();
    assert_eq!(status.hub_id, key.id());
}

/// A socket file left by a daemon that died (nobody listens on it) is replaced at start.
#[tokio::test]
async fn a_stale_socket_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let key = HubKey::generate().unwrap();
    let p = paths(dir.path());
    std::fs::create_dir_all(p.socket.parent().unwrap()).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            p.socket.parent().unwrap(),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
    }
    drop(std::os::unix::net::UnixListener::bind(&p.socket).unwrap());
    // A process another test forks may hold the dropped listener until it execs.
    wait_for_async("the stale socket to stop answering", || {
        let socket = p.socket.clone();
        async move { UnixStream::connect(&socket).await.is_err() }
    })
    .await;
    let d = Daemon::start(dir.path(), &key).await;
    let hub = Hub::default();
    let _conn = hub.connect(&d.paths.socket, &key).await;
    hub.wait("synced", |s| s.synced == 1).await;
}

/// The variable that makes [`daemon_child_process`] a daemon: its arguments, as the relay
/// passed them (space-separated; the test's paths have no spaces).
const CHILD_ARGS_ENV: &str = "TWR_TEST_DAEMON_ARGS";

/// The box daemon a relay starts in [`the_relay_starts_the_daemon_and_relays_to_it`] (does
/// nothing unless started so): `twr runner --daemon` with the fake adapter, until a `stop`.
#[tokio::test]
async fn daemon_child_process() {
    let Ok(args) = std::env::var(CHILD_ARGS_ENV) else {
        return;
    };
    let args: Vec<&str> = args.split(' ').collect();
    let arg = |flag: &str| {
        let i = args.iter().position(|a| *a == flag).unwrap();
        args[i + 1].to_string()
    };
    assert_eq!(&args[..2], ["runner", "--daemon"]);
    assert!(args.contains(&format!("--box={BOX}").as_str()), "{args:?}");
    let key = twr_core::sealed::parse_hub_public(&arg("--hub-key")).unwrap();
    let paths = DaemonPaths {
        socket: arg("--daemon-socket").into(),
        data_dir: arg("--data-dir").into(),
        lock: arg("--lock").into(),
        log: arg("--log").into(),
    };
    let log = twr_core::paths::open_private_append(&paths.log).unwrap();
    let _ = tracing_subscriber::fmt()
        .with_writer(Mutex::new(log))
        .with_ansi(false)
        .try_init();
    let opts = daemon_opts(&paths, &key);
    let lbox = LocalBox::open(with_fake(opts.box_options())).unwrap();
    twr_runner::run_with(opts, lbox, twr_runner::client::shutdown_signal())
        .await
        .unwrap();
}

/// Stops the daemon at `socket` when dropped (a failed test leaves no daemon behind).
struct StopOnDrop(PathBuf);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        use std::io::Write;
        if let Ok(mut s) = std::os::unix::net::UnixStream::connect(&self.0) {
            let _ = s.write_all(b"twr-box 1 stop\n");
        }
    }
}

/// The relay starts a daemon (in a session of its own) when none runs, says so on its notes,
/// and relays the hub's bytes to it; when the hub's side ends, the relay ends and the daemon
/// and its runtime run on, and the next relay uses that daemon.
#[tokio::test]
async fn the_relay_starts_the_daemon_and_relays_to_it() {
    init_logs();
    let dir = tempfile::tempdir().unwrap();
    let key = HubKey::generate().unwrap();
    // The "twr" the relay starts: this test binary, running only `daemon_child_process`.
    let program = dir.path().join("twr");
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\n{CHILD_ARGS_ENV}=\"$*\" exec '{}' --exact daemon_child_process \
             --nocapture --test-threads=1\n",
            std::env::current_exe().unwrap().display()
        ),
    )
    .unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let paths = paths(dir.path());
    let _stop = StopOnDrop(paths.socket.clone());
    let opts = RelayOptions {
        box_name: BOX.into(),
        hub_key: key.public(),
        replace: true,
        paths: paths.clone(),
        program,
        launcher: Launcher::Setsid,
        start_timeout: Duration::from_secs(60),
        stop_timeout: Duration::from_secs(60),
        build: None,
    };
    let notes: Arc<Mutex<Vec<String>>> = Arc::default();
    let relay = |io: tokio::io::DuplexStream| {
        let opts = opts.clone();
        let notes = notes.clone();
        tokio::spawn(async move {
            twr_runner::relay::relay_with(opts, io, &mut |l: &str| {
                notes.lock().unwrap().push(l.to_string())
            })
            .await
        })
    };

    let (hub_end, relay_end) = tokio::io::duplex(64 << 10);
    let first = relay(relay_end);
    let hub = Hub::default();
    let conn = hub.over(hub_end, &key).await;
    let build = twr_core::build_id().unwrap();
    assert_eq!(
        *notes.lock().unwrap(),
        [format!("twr-box: started the box daemon {build}")]
    );
    let mode = |p: &Path| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    };
    assert_eq!(mode(&paths.data_dir), 0o700);
    assert_eq!(mode(&paths.log), 0o600);
    conn.cmd("c1", spawn_op("t1", "through the relay"));
    hub.wait("turn.ended", |s| {
        s.committed.iter().any(|c| c.2 == "turn.ended")
    })
    .await;

    // The hub's side ends (ssh went away): the relay ends, the daemon runs on.
    drop(conn);
    tokio::time::timeout(Duration::from_secs(20), first)
        .await
        .expect("the first relay did not end")
        .unwrap()
        .unwrap();
    assert_eq!(hub.state().gaps, 0);
    let (hub_end, relay_end) = tokio::io::duplex(64 << 10);
    let second = relay(relay_end);
    let conn = hub.over(hub_end, &key).await;
    {
        let s = hub.state();
        let inv = s.inventories.last().unwrap();
        assert_eq!(
            inv.running.len(),
            1,
            "the runtime did not survive the relay"
        );
    }
    assert_eq!(
        notes.lock().unwrap().len(),
        1,
        "a second daemon was started"
    );

    // Asked to stop (`twr runner --stdio --stop`), the daemon delivers the runtime's exit, lets
    // go of its socket and its lock, and the relay serving it ends.
    let mut stop_notes = Vec::new();
    twr_runner::relay::stop(&opts, &mut |l: &str| stop_notes.push(l.to_string()))
        .await
        .unwrap();
    assert_eq!(
        stop_notes,
        [format!("twr-box: stopped the box daemon {build}")]
    );
    hub.wait("the exit", |s| s.committed.iter().any(|c| c.2 == "exited"))
        .await;
    tokio::time::timeout(Duration::from_secs(20), second)
        .await
        .expect("the second relay did not end")
        .unwrap()
        .unwrap();
    wait_for("the daemon to let go of its lock", || {
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&paths.lock)
            .unwrap();
        f.try_lock().is_ok()
    })
    .await;
    assert!(!paths.socket.exists());
    let log = std::fs::read_to_string(&paths.log).unwrap();
    assert!(log.contains("the box daemon is listening"), "{log}");
    drop(conn);
    // No daemon left: stopping again says so, and is no error.
    let mut stop_notes = Vec::new();
    twr_runner::relay::stop(&opts, &mut |l: &str| stop_notes.push(l.to_string()))
        .await
        .unwrap();
    assert_eq!(
        stop_notes,
        [format!(
            "twr-box: no box daemon runs for box {BOX} of hub key {}",
            key.id()
        )]
    );
}
