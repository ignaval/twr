//! Remote boxes, the hub side: an in-process hub with its runner listener on 127.0.0.1 and real
//! runners (`twr_runner::run_with`, fake adapter) — spawn/send/permission/respond through a
//! runner, connections killed mid-stream (a counting WebSocket proxy between runner and hub),
//! a hub crash between commit and ack, command resends, fencing, authentication, the bind
//! check, TLS, worktrees and snapshots on the box, remote attach, and reconciliation after
//! a runner restart.
//!
//! The TLS fixtures in `tests/fixtures/tls` were made once with the `openssl` CLI: a test CA
//! (`ca.pem`, its key discarded) and a leaf for `localhost` / `127.0.0.1` signed by it
//! (`hub.pem`, key `hub.key`), valid for 100 years.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio_tungstenite::tungstenite::Message;
use twr_adapters::FakeAdapter;
use twr_core::adapter::{Adapter, Answer};
use twr_core::config::{BoxConfig, Config, HubConfig, TlsConfig};
use twr_core::event::*;
use twr_core::proto::codes;
use twr_core::runner::{
    auth_proof, check_auth_proof, cmd_errors, hub_errors, new_nonce, CmdError, CmdResult, HubMsg,
    Inventory, Op, Prover, RunnerMsg, RunningSession, Secret, PROTOCOL_VERSION,
};
use twr_hub::hub::ThreadSubscription;
use twr_hub::listener::{RunnerListener, RunnerServer};
use twr_hub::{Caller, Hub, HubOptions, NewThread};
use twr_runner::{LocalBox, LocalBoxOptions, RunnerOptions};

const TOKEN: &str = "gpu-token-3f9a1c";
const BOX: &str = "gpu";
const WAIT: Duration = Duration::from_secs(15);

// ------------------------------------------------------------------------------------------
// Harness
// ------------------------------------------------------------------------------------------

/// The `ssh` arguments of a remote attach to `dest`, up to its remote command: a terminal, no
/// forwarding of any kind (before and after the box's words), `--`.
fn attach_ssh(dest: &str) -> Vec<String> {
    std::iter::once("ssh")
        .chain(twr_hub::remote::ATTACH_SSH_OPTIONS.iter().copied())
        .chain([dest])
        .chain(twr_hub::remote::SSH_NO_FORWARDING.iter().copied())
        .chain(["--"])
        .map(String::from)
        .collect()
}

fn config(ssh: Option<&str>) -> Config {
    let mut c = Config {
        default_runtime: "fake".into(),
        ..Default::default()
    };
    c.hub.listen = Some("127.0.0.1:0".into());
    c.boxes.insert(
        BOX.into(),
        BoxConfig {
            token: Some(TOKEN.into()),
            ssh: ssh.map(str::to_string),
            ..Default::default()
        },
    );
    c
}

fn hub_opts(dir: &Path, cfg: Config) -> HubOptions {
    init_logs();
    let mut o = HubOptions::new(dir.join("hub").join("twr.db"), cfg);
    o.adapters = with_fake();
    o.stop_timeout = Duration::from_secs(5);
    o.git_tick = Some(Duration::ZERO);
    o.stall_after = Some(Duration::ZERO);
    o
}

/// A hub in its own runtime (so that a test can crash it: every task gone at once) with its
/// runner listener.
struct TestHub {
    rt: Option<tokio::runtime::Runtime>,
    hub: Option<Hub>,
    server: Option<RunnerServer>,
    url: String,
}

impl TestHub {
    async fn start(dir: &Path, cfg: Config) -> TestHub {
        TestHub::start_opts(hub_opts(dir, cfg)).await
    }

    async fn start_opts(opts: HubOptions) -> TestHub {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (hub, server, url) = rt
            .spawn(async move {
                let l = RunnerListener::bind(&opts.config.hub)
                    .await
                    .unwrap()
                    .unwrap();
                let url = l.url();
                let hub = Hub::open(opts).await.unwrap();
                let server = l.serve(hub.clone());
                (hub, server, url)
            })
            .await
            .unwrap();
        TestHub {
            rt: Some(rt),
            hub: Some(hub),
            server: Some(server),
            url,
        }
    }

    fn hub(&self) -> &Hub {
        self.hub.as_ref().unwrap()
    }

    /// Runs `f` in the hub's runtime (what it spawns lives there).
    fn sync<T>(&self, f: impl FnOnce(&Hub) -> T) -> T {
        let _g = self.rt.as_ref().unwrap().enter();
        f(self.hub())
    }

    /// Runs the future `f` makes in the hub's runtime.
    async fn run<T, F>(&self, f: impl FnOnce(Hub) -> F) -> T
    where
        F: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let fut = f(self.hub().clone());
        self.rt.as_ref().unwrap().spawn(fut).await.unwrap()
    }

    async fn online(&self) {
        let h = self.hub().clone();
        wait_for("the box online", move || {
            h.boxes().iter().any(|(n, online, _)| n == BOX && *online)
        })
        .await;
    }

    async fn new_thread(&self, project: &Path, name: &str) -> String {
        let req = NewThread {
            project: project.to_string_lossy().into_owned(),
            runtime: Some("fake".into()),
            name: Some(name.into()),
            box_name: Some(BOX.into()),
            ..Default::default()
        };
        let v = self
            .run(|h| async move { h.new_thread(req, "test").await })
            .await
            .unwrap();
        v["id"].as_str().unwrap().to_string()
    }

    fn send(&self, thread: &str, prompt: &str) {
        self.sync(|h| {
            h.send(thread, prompt, TurnFrom::User, Source::User("test".into()))
                .unwrap()
        });
    }

    async fn shutdown(mut self) {
        let hub = self.hub.take().unwrap();
        self.rt
            .as_ref()
            .unwrap()
            .spawn(async move { hub.shutdown().await })
            .await
            .unwrap();
        self.server.take();
        self.rt.take().unwrap().shutdown_background();
    }

    /// A crash: every task of the hub is gone at once, nothing graceful happens.
    fn crash(mut self) {
        self.server.take();
        self.hub.take();
        self.rt.take().unwrap().shutdown_background();
    }
}

impl Drop for TestHub {
    fn drop(&mut self) {
        self.server.take();
        self.hub.take();
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

fn runner_opts(url: &str, dir: &Path) -> RunnerOptions {
    let mut o = RunnerOptions::new(url, Secret::new(TOKEN), BOX, dir);
    o.backoff_min = Duration::from_millis(20);
    o.backoff_max = Duration::from_millis(200);
    o.refused_retry = Duration::from_millis(200);
    o.ping_interval = Duration::from_millis(500);
    o.idle_timeout = Duration::from_secs(10);
    o.welcome_timeout = Duration::from_secs(5);
    o.shutdown_grace = Duration::from_secs(3);
    o
}

/// The built-in adapters plus the fake runtime (offered by the registry only on request).
fn with_fake() -> twr_hub::host::AdapterLookup {
    Arc::new(|name: &str| -> Option<Arc<dyn Adapter>> {
        match name {
            "fake" => Some(Arc::new(FakeAdapter::new())),
            other => twr_adapters::find(other),
        }
    })
}

fn open_box(dir: &Path) -> LocalBox {
    let mut o = LocalBoxOptions::new(dir, BOX);
    o.adapters = with_fake();
    if !o.runtimes.iter().any(|r| r == "fake") {
        o.runtimes.push("fake".into());
    }
    LocalBox::open(o).unwrap()
}

struct Runner {
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Runner {
    fn start(o: RunnerOptions, lbox: LocalBox) -> Runner {
        let (tx, rx) = oneshot::channel::<()>();
        let task = tokio::spawn(twr_runner::run_with(o, lbox, async move {
            let _ = rx.await;
        }));
        Runner {
            stop: Some(tx),
            task,
        }
    }

    async fn shutdown(mut self) {
        let _ = self.stop.take().unwrap().send(());
        let _ = tokio::time::timeout(Duration::from_secs(10), &mut self.task).await;
    }

    /// The runner process dies: no graceful stop, nothing more is spooled.
    async fn crash(self) {
        self.task.abort();
        let _ = self.task.await;
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn wait_for(what: &str, cond: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + WAIT;
    while !cond() {
        if tokio::time::Instant::now() > deadline {
            panic!("timed out waiting for {what}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn until(sub: &mut ThreadSubscription, pred: impl Fn(&Event) -> bool) -> Vec<Event> {
    let mut seen = Vec::new();
    tokio::time::timeout(WAIT, async {
        loop {
            let ev = sub.next().await.unwrap().expect("hub stopped");
            let done = pred(&ev);
            seen.push((*ev).clone());
            if done {
                return;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out; saw {:?}", kinds(&seen)));
    seen
}

fn kinds(evs: &[Event]) -> Vec<String> {
    evs.iter().map(|e| e.kind().to_string()).collect()
}

fn count(evs: &[Event], kind: &str) -> usize {
    evs.iter().filter(|e| e.kind() == kind).count()
}

/// The whole log of a thread (the `hub` thread: its control events).
fn log(hub: &Hub, thread: &str) -> Vec<Event> {
    if thread == HUB_THREAD {
        let (items, _) = hub.control_read(0, 100_000).unwrap();
        return items
            .into_iter()
            .map(|(_, e)| e)
            .filter(|e| e.thread == HUB_THREAD)
            .collect();
    }
    hub.thread_events(thread, 0, 100_000).unwrap().1
}

/// The `agent.text` deltas of a thread, in order.
fn deltas(evs: &[Event]) -> Vec<String> {
    evs.iter()
        .filter_map(|e| match &e.body {
            EventBody::AgentText(t) if t.delta => Some(t.text.trim().to_string()),
            _ => None,
        })
        .collect()
}

fn project(dir: &Path) -> PathBuf {
    let p = dir.join("proj");
    std::fs::create_dir_all(&p).unwrap();
    p
}

// ------------------------------------------------------------------------------------------
// A WebSocket proxy between runner and hub that counts and can cut
// ------------------------------------------------------------------------------------------

#[derive(Default)]
struct ProxyState {
    upstream: String,
    /// Cut the next connection after this many runner `event`/`exited` frames (the last one
    /// is not forwarded).
    cut_after_frames: Option<usize>,
    /// Swallow the hub's acks.
    swallow_acks: bool,
    /// Swallow the runner's next command result and cut the connection.
    cut_on_result: bool,
    /// Swallow the hub's next command with this op name (it is lost in transport).
    drop_cmd: Option<&'static str>,
    /// Commands swallowed so far.
    dropped_cmds: usize,
    /// Cut the current connection at its next message, either way.
    cut_now: bool,
    connections: usize,
    cuts: usize,
}

#[derive(Clone)]
struct Proxy {
    url: String,
    state: Arc<Mutex<ProxyState>>,
}

impl Proxy {
    async fn start(upstream: &str) -> Proxy {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}", l.local_addr().unwrap());
        let p = Proxy {
            url,
            state: Arc::new(Mutex::new(ProxyState {
                upstream: upstream.into(),
                ..Default::default()
            })),
        };
        let pp = p.clone();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = l.accept().await {
                let pp = pp.clone();
                tokio::spawn(async move { pp.relay(tcp).await });
            }
        });
        p
    }

    fn st(&self) -> std::sync::MutexGuard<'_, ProxyState> {
        self.state.lock().unwrap()
    }

    async fn relay(&self, tcp: tokio::net::TcpStream) {
        let Ok(down) = tokio_tungstenite::accept_async(tcp).await else {
            return;
        };
        let upstream = self.st().upstream.clone();
        let Ok((up, _)) = tokio_tungstenite::connect_async(upstream.as_str()).await else {
            return;
        };
        let mut cut = {
            let mut s = self.st();
            s.connections += 1;
            s.cut_after_frames.take()
        };
        let (mut dsink, mut dstream) = down.split();
        let (mut usink, mut ustream) = up.split();
        loop {
            let m = tokio::select! {
                m = dstream.next() => Ok(m),
                m = ustream.next() => Err(m),
            };
            {
                let mut s = self.st();
                if s.cut_now {
                    s.cut_now = false;
                    s.cuts += 1;
                    return;
                }
            }
            match m {
                Ok(m) => {
                    let Some(Ok(m)) = m else { return };
                    if let Message::Text(t) = &m {
                        match serde_json::from_str::<RunnerMsg>(t.as_str()) {
                            Ok(RunnerMsg::Event { .. } | RunnerMsg::Exited { .. }) => {
                                if let Some(n) = cut.as_mut() {
                                    *n -= 1;
                                    if *n == 0 {
                                        self.st().cuts += 1;
                                        return;
                                    }
                                }
                            }
                            Ok(RunnerMsg::CmdResult(_)) => {
                                let mut s = self.st();
                                if s.cut_on_result {
                                    s.cut_on_result = false;
                                    s.cuts += 1;
                                    return;
                                }
                            }
                            _ => {}
                        }
                    }
                    if usink.send(m).await.is_err() {
                        return;
                    }
                }
                Err(m) => {
                    let Some(Ok(m)) = m else { return };
                    if let Message::Text(t) = &m {
                        match serde_json::from_str::<HubMsg>(t.as_str()) {
                            Ok(HubMsg::Ack { .. }) if self.st().swallow_acks => continue,
                            Ok(HubMsg::Cmd { op, .. }) => {
                                let mut s = self.st();
                                if s.drop_cmd == Some(op.name()) {
                                    s.drop_cmd = None;
                                    s.dropped_cmds += 1;
                                    continue;
                                }
                            }
                            _ => {}
                        }
                    }
                    if dsink.send(m).await.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

// ------------------------------------------------------------------------------------------
// A raw runner (the protocol by hand)
// ------------------------------------------------------------------------------------------

type RawWs =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn raw(url: &str) -> RawWs {
    tokio_tungstenite::connect_async(url).await.unwrap().0
}

async fn raw_send(ws: &mut RawWs, m: &RunnerMsg) {
    ws.send(Message::Text(serde_json::to_string(m).unwrap().into()))
        .await
        .unwrap();
}

/// The runs the hub named for each thread's sessions (`spawn` / `resume`), in order.
static RUNS: std::sync::LazyLock<Mutex<std::collections::HashMap<String, Vec<String>>>> =
    std::sync::LazyLock::new(Default::default);

/// Notes the run a `spawn` / `resume` names.
fn note_run(m: &HubMsg) {
    if let HubMsg::Cmd {
        op:
            Op::Spawn {
                thread,
                run: Some(r),
                ..
            }
            | Op::Resume {
                thread,
                run: Some(r),
                ..
            },
        ..
    } = m
    {
        let mut runs = RUNS.lock().unwrap();
        let v = runs.entry(thread.clone()).or_default();
        if !v.contains(r) {
            v.push(r.clone());
        }
    }
}

/// The run a raw runner reports for `label`: `r<n>` is the run of the `n`th session the hub
/// started for `thread` (it names them), any other label (or one with no such session) is a
/// run of the runner's own.
fn run_id(thread: &str, label: &str) -> String {
    label
        .strip_prefix('r')
        .and_then(|n| n.parse::<usize>().ok())
        .and_then(|n| {
            RUNS.lock()
                .unwrap()
                .get(thread)?
                .get(n.checked_sub(1)?)
                .cloned()
        })
        .unwrap_or_else(|| label.to_string())
}

/// The next hub message; `None` once the hub closed.
async fn raw_next(ws: &mut RawWs) -> Option<HubMsg> {
    loop {
        let m = tokio::time::timeout(WAIT, ws.next())
            .await
            .expect("no message from the hub");
        match m {
            Some(Ok(Message::Text(t))) => {
                let m = serde_json::from_str(t.as_str()).unwrap();
                note_run(&m);
                return Some(m);
            }
            Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return None,
            Some(Ok(_)) => continue,
        }
    }
}

/// The next hub message that is not a ping/pong.
async fn raw_expect(ws: &mut RawWs) -> Option<HubMsg> {
    loop {
        match raw_next(ws).await {
            Some(HubMsg::Ping | HubMsg::Pong) => continue,
            // The hub reconciles a box's trash once it is online: an empty trash. Whenever
            // it asked, the answer may find the connection closed by now (fenced, or closed
            // after a gap): the next read says so.
            Some(HubMsg::Cmd {
                cmd_id,
                op: Op::TrashList {},
            }) => {
                let empty =
                    RunnerMsg::CmdResult(CmdResult::new(cmd_id, Ok(Value::Array(Vec::new()))));
                let _ = ws
                    .send(Message::Text(serde_json::to_string(&empty).unwrap().into()))
                    .await;
            }
            other => return other,
        }
    }
}

/// What a raw runner says in its handshake: its box and token ([`raw_hello`]) and its
/// inventory.
#[derive(Debug, Clone, Default)]
struct Hello {
    box_name: String,
    token: String,
    version: String,
    epoch_hint: Option<u64>,
    running: Vec<RunningSession>,
    in_flight: Vec<String>,
    acks_needed: std::collections::BTreeMap<String, u64>,
    runtimes: Vec<String>,
    worktrees: std::path::PathBuf,
}

fn hello(token: &str) -> Hello {
    Hello {
        box_name: BOX.into(),
        token: token.into(),
        version: "0.0.0-raw".into(),
        runtimes: vec!["fake".into()],
        worktrees: "/nonexistent/worktrees".into(),
        ..Default::default()
    }
}

/// A raw runner's handshake (as [`twr_runner::client`] does it) up to its inventory: hello,
/// the hub's challenge (its proof checked), auth, authenticated, inventory. The hub's next
/// message is the welcome (or a refusal).
async fn raw_hello(ws: &mut RawWs, h: Hello) {
    let token = Secret::new(h.token.clone());
    let nonce = new_nonce().unwrap();
    let hello = twr_core::runner::Hello {
        box_name: h.box_name.clone(),
        protocol: PROTOCOL_VERSION,
        nonce: nonce.clone(),
    };
    raw_send(ws, &RunnerMsg::Hello(hello)).await;
    let Some(HubMsg::Challenge { nonce: hub, proof }) = raw_expect(ws).await else {
        panic!("no challenge")
    };
    assert!(
        check_auth_proof(&token, Prover::Hub, &h.box_name, &nonce, &hub, &proof),
        "the hub's proof"
    );
    let proof = auth_proof(&token, Prover::Runner, &h.box_name, &nonce, &hub);
    raw_send(ws, &RunnerMsg::Auth { proof }).await;
    let reply = raw_expect(ws).await;
    assert!(matches!(reply, Some(HubMsg::Authenticated)), "{reply:?}");
    let inv = Inventory {
        version: h.version,
        epoch_hint: h.epoch_hint,
        running: h.running,
        in_flight: h.in_flight,
        acks_needed: h.acks_needed,
        runtimes: h.runtimes,
        worktrees: h.worktrees,
        build: None,
    };
    raw_send(ws, &RunnerMsg::Inventory(inv)).await;
}

// ------------------------------------------------------------------------------------------
// Tests
// ------------------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_send_permission_and_respond_through_a_runner() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let runner = Runner::start(
        runner_opts(&h.url, &dir.path().join("r")),
        open_box(&dir.path().join("r")),
    );
    h.online().await;
    let hub_log = log(h.hub(), HUB_THREAD);
    let connected: Vec<&Event> = hub_log
        .iter()
        .filter(|e| e.kind() == "runner.connected")
        .collect();
    assert_eq!(connected.len(), 1);
    let EventBody::RunnerConnected(p) = &connected[0].body else {
        panic!()
    };
    assert_eq!(p.box_name, BOX);
    assert_eq!(p.version.as_deref(), Some(twr_runner::client::version()));
    let info = h.hub().info();
    assert_eq!(info["boxes"][0]["online"], json!(true));

    let t = h.new_thread(&project(dir.path()), "remote1").await;
    assert_eq!(h.hub().get_thread(&t).unwrap()["box_name"], BOX);
    let mut sub = h.hub().subscribe_thread(&t, 0).unwrap();
    h.send(&t, "!perm Bash rm -rf build");
    let evs = until(&mut sub, |e| e.kind() == "attention.raised").await;
    assert_eq!(evs[0].kind(), "thread.created");
    let started = evs.iter().find(|e| e.kind() == "thread.started").unwrap();
    assert_eq!(started.source, Source::Adapter("fake".into()));
    let attn = evs
        .iter()
        .find_map(|e| match &e.body {
            EventBody::AttentionRaised(a) => Some(a.attn_id.clone()),
            _ => None,
        })
        .unwrap();
    h.sync(|hub| hub.respond(&attn, Answer::allow(), "test").unwrap());
    until(&mut sub, |e| e.kind() == "report.ready").await;
    h.send(&t, "alpha beta gamma");
    until(&mut sub, |e| {
        e.kind() == "report.ready" && e.turn == Some(2)
    })
    .await;
    let all = log(h.hub(), &t);
    assert_eq!(count(&all, "thread.started"), 1, "{:?}", kinds(&all));
    assert_eq!(count(&all, "turn.ended"), 2);
    let tool = all
        .iter()
        .find_map(|e| match &e.body {
            EventBody::AgentToolResult(r) => Some(r.ok),
            _ => None,
        })
        .unwrap();
    assert!(tool, "the answer reached the runtime");
    assert_eq!(deltas(&all), ["alpha", "beta", "gamma"]);
    // Every frame is committed exactly once: one event per frame.
    let adapter_events = all
        .iter()
        .filter(|e| matches!(e.source, Source::Adapter(_)))
        .count();
    let wm = rusqlite::Connection::open(dir.path().join("hub/twr.db"))
        .unwrap()
        .query_row(
            "SELECT rseq FROM runner_acks WHERE box = ?1 AND thread = ?2",
            [BOX, t.as_str()],
            |r| r.get::<_, i64>(0),
        )
        .unwrap();
    assert_eq!(wm as usize, adapter_events);

    // The runner goes away: the box is offline, commands say so, prompts wait.
    runner.shutdown().await;
    let hub = h.hub().clone();
    wait_for("offline", move || {
        hub.boxes().iter().any(|(_, online, _)| !online)
    })
    .await;
    // The graceful runner stop ended the session: the exit was delivered first.
    let all = log(h.hub(), &t);
    assert_eq!(
        count(&all, "thread.crashed") + count(&all, "thread.stopped"),
        1
    );
    assert_eq!(count(&log(h.hub(), HUB_THREAD), "runner.disconnected"), 1);
    let s = h.sync(|hub| {
        hub.send(&t, "later", TurnFrom::User, Source::User("test".into()))
            .unwrap()
    });
    assert_eq!((s.turn, s.pending), (None, 1));
    let e = h
        .run(|hub| async move {
            hub.new_thread(
                NewThread {
                    project: "/x".into(),
                    box_name: Some(BOX.into()),
                    ..Default::default()
                },
                "test",
            )
            .await
        })
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::BOX_OFFLINE);
    // Back online: the queued prompt runs.
    let runner = Runner::start(
        runner_opts(&h.url, &dir.path().join("r")),
        open_box(&dir.path().join("r")),
    );
    until(&mut sub, |e| {
        e.kind() == "report.ready" && e.turn == Some(3)
    })
    .await;
    runner.shutdown().await;
    h.shutdown().await;
}

/// `thread.set_mode` on a remote thread: its idle runtime is stopped through the runner and
/// started again on the same session, in the new mode (`hub::mode`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_runtime_restarts_in_a_new_mode() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let runner = Runner::start(
        runner_opts(&h.url, &dir.path().join("r")),
        open_box(&dir.path().join("r")),
    );
    h.online().await;
    let t = h.new_thread(&project(dir.path()), "remote1").await;
    let mut sub = h.hub().subscribe_thread(&t, 0).unwrap();
    h.send(&t, "hi");
    let evs = until(&mut sub, |e| e.kind() == "report.ready").await;
    let mode_of = |e: &Event| match &e.body {
        EventBody::ThreadStarted(p) => p.ext.as_ref()?["fake"]["mode"].as_str().map(str::to_string),
        _ => None,
    };
    assert_eq!(evs.iter().find_map(mode_of).as_deref(), Some("auto"));
    let session = h.hub().thread_view(&t).unwrap().session_id;
    let tt = t.clone();
    let r = h.sync(move |hub| hub.set_mode(&tt, "plan", &Caller::user("test")).unwrap());
    assert_eq!(r["applies"], "now", "{r}");
    let evs = until(&mut sub, |e| e.kind() == "thread.started").await;
    assert!(
        evs.iter().any(|e| matches!(&e.body,
            EventBody::ThreadStopped(p) if is_settings_restart(p))),
        "{:?}",
        kinds(&evs)
    );
    assert_eq!(evs.iter().find_map(mode_of).as_deref(), Some("plan"));
    let v = h.hub().thread_view(&t).unwrap();
    assert_eq!((v.session_id, v.stopped), (session, false));
    h.send(&t, "again");
    until(&mut sub, |e| {
        e.kind() == "report.ready" && e.turn == Some(2)
    })
    .await;
    runner.shutdown().await;
    h.shutdown().await;
}

/// A remote box's session takes no interrupt: a queued message to its thread can be taken
/// back (it never reaches the box), one whose turn started cannot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_thread_gives_back_only_queued_messages() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let runner = Runner::start(
        runner_opts(&h.url, &dir.path().join("r")),
        open_box(&dir.path().join("r")),
    );
    h.online().await;
    let t = h.new_thread(&project(dir.path()), "remote1").await;
    let mut sub = h.hub().subscribe_thread(&t, 0).unwrap();
    let user = Caller::user("test");
    let first = h.sync(|hub| hub.send_by(&t, "!sleep 60000", &user).unwrap());
    let later = h.sync(|hub| hub.send_by(&t, "later", &user).unwrap());
    assert_eq!((first.turn, later.turn), (Some(1), None));
    until(&mut sub, |e| e.kind() == "thread.started").await;
    let w = h.sync(|hub| hub.withdraw_message(&t, later.msg_ref, &user).unwrap());
    assert_eq!(w["state"], "queued", "{w}");
    let w = h.sync(|hub| hub.withdraw_message(&t, first.msg_ref, &user).unwrap());
    assert_eq!(
        (w["withdrawn"].as_bool(), w["reason"].as_str()),
        (Some(false), Some("uninterruptible")),
        "{w}"
    );
    assert!(h.hub().thread_view(&t).unwrap().current_turn.is_some());
    h.sync(|hub| hub.stop_thread(&t, "test").unwrap());
    until(&mut sub, |e| e.kind() == "thread.stopped").await;
    runner.shutdown().await;
    h.shutdown().await;
}

/// `thread.set_model` on a remote thread: the runner gets the new model and effort with the
/// restart (`WireSpawnConfig`: the fake runtime reports both), and the start event says what
/// it started with (`ext.twr.model` / `ext.twr.effort`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_runtime_restarts_with_a_new_model_and_effort() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let runner = Runner::start(
        runner_opts(&h.url, &dir.path().join("r")),
        open_box(&dir.path().join("r")),
    );
    h.online().await;
    let t = h.new_thread(&project(dir.path()), "remote1").await;
    let mut sub = h.hub().subscribe_thread(&t, 0).unwrap();
    h.send(&t, "hi");
    until(&mut sub, |e| e.kind() == "report.ready").await;
    let session = h.hub().thread_view(&t).unwrap().session_id;
    let tt = t.clone();
    let r = h.sync(move |hub| {
        hub.set_model(&tt, Some("fake-big"), Some("deep"), &Caller::user("test"))
            .unwrap()
    });
    assert_eq!(r["applies"], "now", "{r}");
    let evs = until(&mut sub, |e| e.kind() == "thread.started").await;
    assert!(
        evs.iter().any(|e| matches!(&e.body,
            EventBody::ThreadStopped(p) if is_settings_restart(p))),
        "{:?}",
        kinds(&evs)
    );
    let EventBody::ThreadStarted(p) = &evs.last().unwrap().body else {
        panic!()
    };
    let ext = p.ext.as_ref().unwrap();
    assert_eq!(
        (&ext["fake"]["model"], &ext["fake"]["effort"]),
        (&serde_json::json!("fake-big"), &serde_json::json!("deep")),
        "{ext:?}"
    );
    assert_eq!(ext["twr"]["model"], "fake-big", "{ext:?}");
    assert_eq!(ext["twr"]["effort"], "deep", "{ext:?}");
    let v = h.hub().thread_view(&t).unwrap();
    assert_eq!((v.session_id, v.stopped), (session, false));
    runner.shutdown().await;
    h.shutdown().await;
}

/// A remote runtime's start event says the mode its runner started it in (`ext.twr.mode`):
/// a hub that takes the session over after a restart, with a mode change recorded before that
/// event arrived, restarts the runtime in the new mode rather than taking the old process for
/// one started in it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_start_event_keeps_the_mode_the_runtime_started_in() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let proxy = Proxy::start(&h.url).await;
    // The first connection is cut at the session's first frame (its `thread.started`), and
    // later ones reach nobody until the new hub is up.
    proxy.st().cut_after_frames = Some(1);
    let rdir = dir.path().join("r");
    let runner = Runner::start(runner_opts(&proxy.url, &rdir), open_box(&rdir));
    h.online().await;
    let t = h.new_thread(&project(dir.path()), "late").await;
    proxy.st().upstream = "ws://127.0.0.1:1".into();
    h.send(&t, "!sleep 1500\nhello");
    let p2 = proxy.clone();
    wait_for("the cut", move || p2.st().cuts == 1).await;
    // The thread's mode changes before its runtime's start is recorded.
    let tt = t.clone();
    h.sync(move |hub| hub.set_mode(&tt, "plan", &Caller::user("test")).unwrap());
    assert_eq!(count(&log(h.hub(), &t), "thread.started"), 0);
    h.crash();

    let h = TestHub::start(dir.path(), config(None)).await;
    proxy.st().upstream = h.url.clone();
    let mut sub = h.hub().subscribe_thread(&t, 0).unwrap();
    let fake_mode = |e: &Event| match &e.body {
        EventBody::ThreadStarted(p) => p.ext.as_ref()?["fake"]["mode"].as_str().map(str::to_string),
        _ => None,
    };
    let evs = until(&mut sub, |e| fake_mode(e).as_deref() == Some("plan")).await;
    // The first start says what the process runs in; its turn ends, then it restarts.
    let first = evs
        .iter()
        .find(|e| e.kind() == "thread.started")
        .expect("the late start event");
    assert_eq!(fake_mode(first).as_deref(), Some("auto"));
    let EventBody::ThreadStarted(p) = &first.body else {
        panic!()
    };
    assert_eq!(p.ext.as_ref().unwrap()["twr"]["mode"], "auto", "{p:?}");
    // The runner stamped the model and effort it started with too (none: the runtime's).
    let twr = &p.ext.as_ref().unwrap()["twr"];
    assert!(twr["model"].is_null() && twr["effort"].is_null(), "{p:?}");
    assert!(
        twr.get("model").is_some() && twr.get("effort").is_some(),
        "{p:?}"
    );
    assert!(
        evs.iter().any(|e| matches!(&e.body,
            EventBody::ThreadStopped(p) if is_settings_restart(p))),
        "{:?}",
        kinds(&evs)
    );
    runner.shutdown().await;
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_cut_mid_stream_loses_and_duplicates_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let proxy = Proxy::start(&h.url).await;
    // The runner's first connection is cut in the middle of the turn below, after 5 frames
    // (the 5th is lost in flight); the second one after 7 more.
    proxy.st().cut_after_frames = Some(5);
    let rdir = dir.path().join("r");
    let runner = Runner::start(runner_opts(&proxy.url, &rdir), open_box(&rdir));
    h.online().await;
    let t = h.new_thread(&project(dir.path()), "cut").await;
    let mut sub = h.hub().subscribe_thread(&t, 0).unwrap();
    let words: Vec<String> = (0..40).map(|i| format!("w{i}")).collect();
    h.send(&t, &words.join(" "));
    let proxy2 = proxy.clone();
    wait_for("the first cut", move || proxy2.st().cuts == 1).await;
    proxy.st().cut_after_frames = Some(7);
    until(&mut sub, |e| e.kind() == "report.ready").await;
    assert_eq!(proxy.st().cuts, 2);
    assert!(proxy.st().connections >= 3);
    let all = log(h.hub(), &t);
    assert_eq!(deltas(&all), words);
    assert_eq!(count(&all, "thread.started"), 1);
    assert_eq!(count(&all, "turn.ended"), 1);
    assert_eq!(count(&all, "thread.crashed"), 0);
    // The box came back after each cut, and is online (a connection cut during its replay
    // never got online, so it has neither; one whose successor said hello before the hub saw
    // it close was superseded, not disconnected: the box never went offline then, which under
    // load can happen to both cuts).
    let hub_log = log(h.hub(), HUB_THREAD);
    let (up, down) = (
        count(&hub_log, "runner.connected"),
        count(&hub_log, "runner.disconnected"),
    );
    assert!(up >= 2 && up > down, "{up} {down}");
    runner.shutdown().await;
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hub_crash_between_commit_and_ack_is_deduplicated_and_the_session_taken_over() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let proxy = Proxy::start(&h.url).await;
    let rdir = dir.path().join("r");
    let lbox = open_box(&rdir);
    let runner = Runner::start(runner_opts(&proxy.url, &rdir), lbox.clone());
    h.online().await;
    let t = h.new_thread(&project(dir.path()), "crashy").await;
    let mut sub = h.hub().subscribe_thread(&t, 0).unwrap();
    // The hub commits but its acks never arrive: then it dies.
    proxy.st().swallow_acks = true;
    h.send(&t, "!sleep 800\none two three four");
    until(&mut sub, |e| e.kind() == "thread.started").await;
    let committed = log(h.hub(), &t).len();
    assert!(lbox.spool().len().unwrap() >= 1, "frames are still unacked");
    drop(sub);
    h.crash();

    // A new hub on the same data: the runner reconnects (it still runs the session and still
    // spools the unacked frames), is told what is committed, and the turn finishes.
    let h = TestHub::start(dir.path(), config(None)).await;
    {
        let mut s = proxy.st();
        s.upstream = h.url.clone();
        s.swallow_acks = false;
    }
    let mut sub = h.hub().subscribe_thread(&t, 0).unwrap();
    until(&mut sub, |e| e.kind() == "report.ready").await;
    let all = log(h.hub(), &t);
    assert!(all.len() > committed);
    assert_eq!(count(&all, "thread.started"), 1, "{:?}", kinds(&all));
    assert_eq!(deltas(&all), ["one", "two", "three", "four"]);
    assert_eq!(count(&all, "turn.ended"), 1);
    assert_eq!(count(&all, "thread.crashed"), 0, "{:?}", kinds(&all));
    wait_for("acks applied", || lbox.spool().is_empty().unwrap()).await;
    // The adopted session takes new turns.
    h.send(&t, "again");
    until(&mut sub, |e| {
        e.kind() == "report.ready" && e.turn == Some(2)
    })
    .await;
    assert_eq!(count(&log(h.hub(), &t), "thread.started"), 1);
    runner.shutdown().await;
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_command_sent_again_after_a_lost_result_spawns_once() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let proxy = Proxy::start(&h.url).await;
    let rdir = dir.path().join("r");
    let runner = Runner::start(runner_opts(&proxy.url, &rdir), open_box(&rdir));
    h.online().await;
    let t = h.new_thread(&project(dir.path()), "resend").await;
    let mut sub = h.hub().subscribe_thread(&t, 0).unwrap();
    // The spawn runs on the box, but its result is lost with the connection: the hub sends the
    // same command id again on the next one, and the runner answers without spawning again.
    proxy.st().cut_on_result = true;
    h.send(&t, "!sleep 300\nhello there");
    until(&mut sub, |e| e.kind() == "report.ready").await;
    assert!(proxy.st().cuts >= 1);
    let all = log(h.hub(), &t);
    assert_eq!(count(&all, "thread.started"), 1, "{:?}", kinds(&all));
    assert_eq!(deltas(&all), ["hello", "there"]);
    assert_eq!(count(&all, "thread.crashed"), 0);
    runner.shutdown().await;
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_connection_of_a_box_fences_the_first() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let rdir = dir.path().join("r");
    let mut o = runner_opts(&h.url, &rdir);
    o.backoff_min = Duration::from_millis(500);
    o.backoff_max = Duration::from_millis(500);
    o.refused_retry = Duration::from_millis(500);
    let runner = Runner::start(o, open_box(&rdir));
    h.online().await;
    let first = {
        let l = log(h.hub(), HUB_THREAD);
        count(&l, "runner.connected")
    };
    assert_eq!(first, 1);
    // Another runner of the same box: welcomed with a later epoch; the first is fenced.
    let mut ws = raw(&h.url).await;
    raw_hello(&mut ws, hello(TOKEN)).await;
    let Some(HubMsg::Welcome { epoch, .. }) = raw_expect(&mut ws).await else {
        panic!("no welcome")
    };
    assert!(epoch >= 2, "epoch {epoch}");
    raw_send(&mut ws, &RunnerMsg::Synced).await;
    // The fenced runner comes back (after its backoff) and fences this one in turn.
    match raw_expect(&mut ws).await {
        Some(HubMsg::Error { code, .. }) => assert_eq!(code, hub_errors::FENCED),
        other => panic!("expected fenced, got {other:?}"),
    }
    assert!(raw_expect(&mut ws).await.is_none(), "closed after fencing");
    h.online().await;
    let hub = h.hub().clone();
    wait_for("three connected", move || {
        count(&log(&hub, HUB_THREAD), "runner.connected") >= 3
    })
    .await;
    // A fenced connection never took the box offline in between.
    assert_eq!(count(&log(h.hub(), HUB_THREAD), "runner.disconnected"), 0);
    runner.shutdown().await;
    h.shutdown().await;
}

/// A raw hello of box `box_name` with `protocol` (and a nonce), and the hub's answer.
async fn raw_hello_only(ws: &mut RawWs, box_name: &str, protocol: u32) -> (String, Option<HubMsg>) {
    let nonce = new_nonce().unwrap();
    let hello = twr_core::runner::Hello {
        box_name: box_name.into(),
        protocol,
        nonce: nonce.clone(),
    };
    raw_send(ws, &RunnerMsg::Hello(hello)).await;
    (nonce, raw_expect(ws).await)
}

/// The handshake's refusals: a wrong token and an unknown box get a challenge whose proof the
/// runner's token does not check (an unknown box's is random, so a peer cannot tell which
/// boxes exist), and `unauthorized` for their own proof; another protocol (an older runner's
/// hello with its token included), a malformed hello or nonce, and anything but `hello`
/// first or `auth` second are refused as `protocol`. Neither token is ever logged, and no
/// refused connection gets the box online.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_tokens_unknown_boxes_and_other_protocols_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let expect_error = |m: Option<HubMsg>, code: &str| match m {
        Some(HubMsg::Error { code: c, message }) => {
            assert_eq!(c, code, "{message}");
            assert!(!message.contains(TOKEN));
            message
        }
        other => panic!("expected error {code}, got {other:?}"),
    };
    // A wrong token, an unknown box.
    for (box_name, token) in [(BOX, "wrong-token-8d1e"), ("nope", TOKEN)] {
        let mut ws = raw(&h.url).await;
        let (nonce, m) = raw_hello_only(&mut ws, box_name, PROTOCOL_VERSION).await;
        let Some(HubMsg::Challenge { nonce: hub, proof }) = m else {
            panic!("no challenge: {m:?}")
        };
        let token = Secret::new(token);
        assert!(!check_auth_proof(
            &token,
            Prover::Hub,
            box_name,
            &nonce,
            &hub,
            &proof
        ));
        let mine = auth_proof(&token, Prover::Runner, box_name, &nonce, &hub);
        raw_send(&mut ws, &RunnerMsg::Auth { proof: mine }).await;
        expect_error(raw_expect(&mut ws).await, hub_errors::UNAUTHORIZED);
        assert!(raw_expect(&mut ws).await.is_none());
    }
    // The configured box's refusal is recorded for `twr info` (the unknown box's is not).
    let r = h.hub().box_refusal(BOX).expect("a refusal of the box");
    assert!(r.reason.contains("does not match"), "{}", r.reason);
    assert!(h.hub().box_refusal("nope").is_none());
    // A runner that leaves after the challenge (it did not accept the hub's proof).
    let mut ws = raw(&h.url).await;
    let (_, m) = raw_hello_only(&mut ws, BOX, PROTOCOL_VERSION).await;
    assert!(matches!(m, Some(HubMsg::Challenge { .. })), "{m:?}");
    drop(ws);
    wait_for("the refusal of a runner that left", || {
        h.hub()
            .box_refusal(BOX)
            .is_some_and(|r| r.reason.contains("did not accept the hub's proof"))
    })
    .await;
    // The hub's own proof sent back (a replay), or garbage.
    for replay in [true, false] {
        let mut ws = raw(&h.url).await;
        let (_, m) = raw_hello_only(&mut ws, BOX, PROTOCOL_VERSION).await;
        let Some(HubMsg::Challenge { proof, .. }) = m else {
            panic!("no challenge: {m:?}")
        };
        let proof = if replay { proof } else { "zz".repeat(32) };
        raw_send(&mut ws, &RunnerMsg::Auth { proof }).await;
        expect_error(raw_expect(&mut ws).await, hub_errors::UNAUTHORIZED);
    }
    // Other protocols are told which side to update, before any authentication.
    for (protocol, update) in [
        (PROTOCOL_VERSION + 1, "update twr on the hub"),
        (PROTOCOL_VERSION - 1, "update twr on box `gpu`"),
    ] {
        let mut ws = raw(&h.url).await;
        let (_, m) = raw_hello_only(&mut ws, BOX, protocol).await;
        let message = expect_error(m, hub_errors::PROTOCOL);
        assert!(message.contains(update), "{message}");
        assert!(raw_expect(&mut ws).await.is_none());
        let r = h.hub().box_refusal(BOX).expect("a refusal of the box");
        assert!(
            r.reason.contains(&format!("protocol {protocol}")) && r.reason.contains(update),
            "{}",
            r.reason
        );
    }
    // An older runner's hello, token and all: refused as another protocol.
    let mut ws = raw(&h.url).await;
    let old = json!({"type": "hello", "box": BOX, "token": TOKEN, "version": "0.1.0",
        "protocol": 5, "running": [], "runtimes": ["fake"], "worktrees": "/w"});
    ws.send(Message::Text(old.to_string().into()))
        .await
        .unwrap();
    let message = expect_error(raw_expect(&mut ws).await, hub_errors::PROTOCOL);
    assert!(message.contains("update twr on box `gpu`"), "{message}");
    // A nonce that is not one.
    let mut ws = raw(&h.url).await;
    let hello = twr_core::runner::Hello {
        box_name: BOX.into(),
        protocol: PROTOCOL_VERSION,
        nonce: String::new(),
    };
    raw_send(&mut ws, &RunnerMsg::Hello(hello)).await;
    expect_error(raw_expect(&mut ws).await, hub_errors::PROTOCOL);
    // Not a hello first; not auth second.
    let mut ws = raw(&h.url).await;
    raw_send(&mut ws, &RunnerMsg::Synced).await;
    expect_error(raw_expect(&mut ws).await, hub_errors::PROTOCOL);
    let mut ws = raw(&h.url).await;
    let (_, m) = raw_hello_only(&mut ws, BOX, PROTOCOL_VERSION).await;
    assert!(matches!(m, Some(HubMsg::Challenge { .. })), "{m:?}");
    raw_send(&mut ws, &RunnerMsg::Inventory(Inventory::default())).await;
    expect_error(raw_expect(&mut ws).await, hub_errors::PROTOCOL);

    assert!(!h.hub().boxes()[0].1);
    assert_eq!(count(&log(h.hub(), HUB_THREAD), "runner.connected"), 0);
    let logs = String::from_utf8(LOGS.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("runner refused"), "{logs}");
    assert!(!logs.contains(TOKEN) && !logs.contains("wrong-token-8d1e"));
    // `hub.info` shows the latest refusal while the box is offline.
    let info = h.hub().info();
    let refused = &info["boxes"][0]["refused"];
    assert!(refused["at"].is_string(), "{info}");
    assert!(refused["reason"].is_string(), "{info}");
    // A connection that authenticates clears it.
    let mut ws = raw(&h.url).await;
    raw_hello(&mut ws, self::hello(TOKEN)).await;
    wait_for("the refusal cleared", || h.hub().box_refusal(BOX).is_none()).await;
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicates_are_acked_gaps_close_and_strays_are_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw(&h.url).await;
    let mut hi = hello(TOKEN);
    // A session the hub knows nothing about.
    hi.running = vec![RunningSession {
        thread: "01STRAY".into(),
        runtime: "fake".into(),
        run: Some("r-stray".into()),
        ..Default::default()
    }];
    raw_hello(&mut ws, hi).await;
    let Some(HubMsg::Welcome { watermarks, .. }) = raw_expect(&mut ws).await else {
        panic!("no welcome")
    };
    assert!(watermarks.is_empty());
    raw_send(&mut ws, &RunnerMsg::Synced).await;
    // Reconciliation: the stray is told to stop.
    let Some(HubMsg::Cmd { cmd_id, op }) = raw_expect(&mut ws).await else {
        panic!("no stop")
    };
    // The stop names the run the hello reported.
    assert_eq!(
        op,
        Op::Stop {
            thread: "01STRAY".into(),
            run: Some("r-stray".into()),
        }
    );
    raw_send(
        &mut ws,
        &RunnerMsg::CmdResult(CmdResult::new(cmd_id, Ok(Value::Null))),
    )
    .await;
    h.online().await;

    // A thread on the box: the hub spawns it through us.
    let t = h.new_thread_raw(&mut ws, dir.path()).await;
    h.send(&t, "hi");
    let (cmd_id, op) = loop {
        match raw_expect(&mut ws).await {
            Some(HubMsg::Cmd { cmd_id, op }) => break (cmd_id, op),
            Some(_) => continue,
            None => panic!("closed"),
        }
    };
    assert!(
        matches!(op, Op::Spawn { ref thread, .. } if thread == &t),
        "{op:?}"
    );
    raw_send(
        &mut ws,
        &RunnerMsg::CmdResult(CmdResult::new(cmd_id, Ok(Value::Null))),
    )
    .await;
    let started = RunnerMsg::Event {
        thread: t.clone(),
        rseq: 1,
        turn: None,
        body: EventBody::ThreadStarted(SessionPayload {
            session_id: Some("s-raw".into()),
            ..Default::default()
        }),
        raw: None,
        run: None,
        at_ms: None,
    };
    raw_send(&mut ws, &started).await;
    assert_eq!(
        raw_expect(&mut ws).await,
        Some(HubMsg::Ack {
            thread: t.clone(),
            rseq: 1
        })
    );
    // The same frame again: acked, not appended.
    raw_send(&mut ws, &started).await;
    assert_eq!(
        raw_expect(&mut ws).await,
        Some(HubMsg::Ack {
            thread: t.clone(),
            rseq: 1
        })
    );
    assert_eq!(count(&log(h.hub(), &t), "thread.started"), 1);
    // A gap: the connection closes (a runner would replay from its spool).
    raw_send(
        &mut ws,
        &RunnerMsg::Event {
            thread: t.clone(),
            rseq: 3,
            turn: Some(1),
            body: EventBody::AgentText(AgentText {
                text: "x".into(),
                ..Default::default()
            }),
            raw: None,
            run: None,
            at_ms: None,
        },
    )
    .await;
    assert!(
        raw_expect(&mut ws).await.is_none(),
        "a gap closes the connection"
    );
    assert_eq!(count(&log(h.hub(), &t), "agent.text"), 0);
    let logs = String::from_utf8(LOGS.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("runner frame gap"), "{logs}");
    // Offline now; the live session stays (its runtime may still run on the box).
    let hub = h.hub().clone();
    wait_for("offline", move || !hub.boxes()[0].1).await;
    assert_eq!(h.hub().get_thread(&t).unwrap()["status"], json!("running"));
    h.shutdown().await;
}

/// A frame committed in an open ack batch is durable only once the batch is synced. A
/// replacement connection that fences the first before that sync is told the frame's
/// watermark in its welcome (and the runner then deletes the frame), so the welcome waits
/// for a sync of the store.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_welcome_reports_only_synced_watermarks() {
    let dir = tempfile::tempdir().unwrap();
    let mut o = hub_opts(dir.path(), config(None));
    // The first connection's ack batch stays open for the whole test.
    o.ack_batch_delay = Duration::from_secs(600);
    let h = TestHub::start_opts(o).await;
    let mut ws = raw(&h.url).await;
    raw_hello(&mut ws, hello(TOKEN)).await;
    let Some(HubMsg::Welcome { watermarks, .. }) = raw_expect(&mut ws).await else {
        panic!("no welcome")
    };
    assert!(watermarks.is_empty());
    raw_send(&mut ws, &RunnerMsg::Synced).await;
    h.online().await;
    let t = h.new_thread_raw(&mut ws, dir.path()).await;
    h.send(&t, "hi");
    let cmd_id = loop {
        match raw_expect(&mut ws).await {
            Some(HubMsg::Cmd {
                cmd_id,
                op: Op::Spawn { .. },
            }) => break cmd_id,
            Some(_) => continue,
            None => panic!("closed"),
        }
    };
    raw_send(
        &mut ws,
        &RunnerMsg::CmdResult(CmdResult::new(cmd_id, Ok(Value::Null))),
    )
    .await;
    raw_send(
        &mut ws,
        &RunnerMsg::Event {
            thread: t.clone(),
            rseq: 1,
            turn: None,
            body: EventBody::ThreadStarted(SessionPayload {
                session_id: Some("s-raw".into()),
                ..Default::default()
            }),
            raw: None,
            run: None,
            at_ms: None,
        },
    )
    .await;
    // Committed, in the open batch: neither synced nor acked.
    let hub = h.hub().clone();
    let t2 = t.clone();
    wait_for("the frame committed", move || {
        count(&log(&hub, &t2), "thread.started") == 1
    })
    .await;
    let syncs = h.hub().store_syncs();

    // A replacement connection fences the first before its batch closes.
    let mut ws2 = raw(&h.url).await;
    raw_hello(&mut ws2, hello(TOKEN)).await;
    let Some(HubMsg::Welcome { watermarks, .. }) = raw_expect(&mut ws2).await else {
        panic!("no welcome")
    };
    assert_eq!(watermarks.get(&t), Some(&1), "{watermarks:?}");
    assert!(
        h.hub().store_syncs() > syncs,
        "the welcome reported the watermark of a frame that was never synced"
    );
    // The first connection never got an ack for it: it was fenced with its batch open.
    loop {
        match raw_expect(&mut ws).await {
            Some(HubMsg::Ack { .. }) => panic!("the fenced connection got an ack"),
            Some(HubMsg::Error { code, .. }) => {
                assert_eq!(code, hub_errors::FENCED);
                break;
            }
            Some(_) => continue,
            None => break,
        }
    }
    h.shutdown().await;
}

impl TestHub {
    /// `thread.new` on the box while a raw connection plays the runner (it answers the git
    /// requests: not a repository).
    async fn new_thread_raw(&self, ws: &mut RawWs, dir: &Path) -> String {
        self.new_thread_raw_named(ws, dir, "raw").await
    }

    async fn new_thread_raw_named(&self, ws: &mut RawWs, dir: &Path, name: &str) -> String {
        let req = NewThread {
            project: dir.join("rawproj").to_string_lossy().into_owned(),
            runtime: Some("fake".into()),
            name: Some(name.into()),
            box_name: Some(BOX.into()),
            ..Default::default()
        };
        let hub = self.hub().clone();
        let created = self
            .rt
            .as_ref()
            .unwrap()
            .spawn(async move { hub.new_thread(req, "test").await });
        let Some(HubMsg::Cmd { cmd_id, op }) = raw_expect(ws).await else {
            panic!("no repo_root")
        };
        assert!(matches!(op, Op::RepoRoot { .. }), "{op:?}");
        raw_send(
            ws,
            &RunnerMsg::CmdResult(CmdResult::new(cmd_id, Ok(Value::Null))),
        )
        .await;
        let v = created.await.unwrap().unwrap();
        v["id"].as_str().unwrap().to_string()
    }
}

#[tokio::test]
async fn a_non_loopback_listener_needs_tls_or_the_opt_in() {
    let open = |tls: bool, insecure: bool| HubConfig {
        listen: Some("0.0.0.0:0".into()),
        tls: tls.then(|| TlsConfig {
            cert: fixture("hub.pem"),
            key: fixture("hub.key"),
        }),
        allow_insecure_remote: insecure,
        ..Default::default()
    };
    let e = RunnerListener::bind(&open(false, false)).await.unwrap_err();
    assert!(e.to_string().contains("not a loopback address"), "{e:#}");
    assert!(!RunnerListener::bind(&open(false, true))
        .await
        .unwrap()
        .unwrap()
        .is_tls());
    assert!(RunnerListener::bind(&open(true, false))
        .await
        .unwrap()
        .unwrap()
        .is_tls());
    // Unset: no listener.
    assert!(RunnerListener::bind(&HubConfig::default())
        .await
        .unwrap()
        .is_none());
    // `twr hub` itself refuses to start.
    let dir = tempfile::tempdir().unwrap();
    let cfg = Config {
        hub: open(false, false),
        ..Default::default()
    };
    let e = twr_hub::run(hub_opts(dir.path(), cfg), dir.path().join("s.sock"))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("not a loopback address"), "{e:#}");
    assert!(!dir.path().join("hub").join("twr.db").exists());
}

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/tls")
        .join(name)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runners_connect_over_tls() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(None);
    cfg.hub.tls = Some(TlsConfig {
        cert: fixture("hub.pem"),
        key: fixture("hub.key"),
    });
    let h = TestHub::start(dir.path(), cfg).await;
    assert!(h.url.starts_with("wss://127.0.0.1:"), "{}", h.url);
    let rdir = dir.path().join("r");
    // Without the CA the certificate is not trusted: never online.
    let untrusted = Runner::start(runner_opts(&h.url, &rdir), open_box(&rdir));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!h.hub().boxes()[0].1);
    untrusted.shutdown().await;
    let mut o = runner_opts(&h.url, &rdir);
    o.ca_file = Some(fixture("ca.pem"));
    let runner = Runner::start(o, open_box(&rdir));
    h.online().await;
    let t = h.new_thread(&project(dir.path()), "tls").await;
    let mut sub = h.hub().subscribe_thread(&t, 0).unwrap();
    h.send(&t, "over tls");
    until(&mut sub, |e| e.kind() == "report.ready").await;
    assert_eq!(deltas(&log(h.hub(), &t)), ["over", "tls"]);
    runner.shutdown().await;
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worktrees_and_snapshots_live_on_the_box() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let rdir = dir.path().join("r");
    let runner = Runner::start(runner_opts(&h.url, &rdir), open_box(&rdir));
    h.online().await;
    let repo = twr_core::testkit::git::init_repo(&dir.path().join("box-repo"));
    let t = h.new_thread(&repo, "wt").await;
    let v = h.hub().get_thread(&t).unwrap();
    // The worktree is where the box keeps them (the runner's data dir), on the thread branch.
    let wt = rdir.join("worktrees").join("box-repo").join("wt");
    assert_eq!(v["worktree"], json!(wt));
    assert!(wt.join("README.md").exists());
    assert!(twr_core::testkit::git::branch_exists(&repo, "twr/wt"));
    let mut sub = h.hub().subscribe_thread(&t, 0).unwrap();
    h.send(&t, "!write notes.txt remote work");
    let evs = until(&mut sub, |e| e.kind() == "git.snapshot").await;
    assert!(evs.iter().any(|e| e.kind() == "turn.ended"));
    let snap = match &evs.last().unwrap().body {
        EventBody::GitSnapshot(s) => s.clone(),
        _ => unreachable!(),
    };
    assert_eq!(snap.branch.as_deref(), Some("twr/wt"));
    assert_eq!(snap.dirty_files, 1);
    assert!(wt.join("notes.txt").exists());
    // Park on the box: dirty, so kept.
    let out = h
        .run(|hub| {
            let t = t.clone();
            async move {
                hub.park_thread(&t, false, false, &twr_hub::Caller::user("test"))
                    .await
            }
        })
        .await
        .unwrap();
    assert!(out.parked && out.kept_dirty && !out.worktree_removed);
    runner.shutdown().await;
    h.shutdown().await;
}

/// A park on a remote box deletes the ignored paths the hub's `[threads] park_disposable`
/// lists (sent with the removal), not the runner's default list.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_park_uses_the_hubs_disposable_list() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(None);
    cfg.threads.park_disposable = vec!["scratch/".into()];
    let h = TestHub::start(dir.path(), cfg).await;
    let rdir = dir.path().join("r");
    let runner = Runner::start(runner_opts(&h.url, &rdir), open_box(&rdir));
    h.online().await;
    let repo = twr_core::testkit::git::init_repo(&dir.path().join("box-repo"));
    let exclude = repo.join(".git").join("info").join("exclude");
    std::fs::create_dir_all(exclude.parent().unwrap()).unwrap();
    std::fs::write(&exclude, "scratch/\n").unwrap();
    let t = h.new_thread(&repo, "disp").await;
    let wt = rdir.join("worktrees").join("box-repo").join("disp");
    std::fs::create_dir_all(wt.join("scratch")).unwrap();
    std::fs::write(wt.join("scratch").join("x"), "regenerated").unwrap();
    let out = h
        .run(|hub| {
            let t = t.clone();
            async move {
                hub.park_thread(&t, false, false, &twr_hub::Caller::user("test"))
                    .await
            }
        })
        .await
        .unwrap();
    assert!(out.parked && out.worktree_removed, "{out:?}");
    assert!(!wt.exists());
    runner.shutdown().await;
    h.shutdown().await;
}

/// A box whose git holds every `git worktree remove` until `held` is released.
fn held_box(dir: &Path, held: &twr_core::testkit::git::HeldGit) -> LocalBox {
    let mut o = LocalBoxOptions::new(dir, BOX);
    o.adapters = with_fake();
    if !o.runtimes.iter().any(|r| r == "fake") {
        o.runtimes.push("fake".into());
    }
    o.git = twr_runner::local::LocalGit::with_bin(&held.bin);
    LocalBox::open(o).unwrap()
}

/// Parks `thread` (a task of the hub's runtime).
fn park_task(
    h: &TestHub,
    thread: &str,
) -> tokio::task::JoinHandle<Result<twr_hub::hub::ParkOutcome, twr_core::proto::RpcError>> {
    let (hub, t) = (h.hub().clone(), thread.to_string());
    h.rt.as_ref().unwrap().spawn(async move {
        hub.park_thread(&t, false, false, &twr_hub::Caller::user("test"))
            .await
    })
}

/// Creates thread `name` in `project` on the box (a task of the hub's runtime).
fn new_thread_task(
    h: &TestHub,
    project: &Path,
    name: &str,
) -> tokio::task::JoinHandle<Result<Value, twr_core::proto::RpcError>> {
    let req = NewThread {
        project: project.to_string_lossy().into_owned(),
        runtime: Some("fake".into()),
        name: Some(name.into()),
        box_name: Some(BOX.into()),
        ..Default::default()
    };
    let hub = h.hub().clone();
    h.rt.as_ref()
        .unwrap()
        .spawn(async move { hub.new_thread(req, "test").await })
}

/// A repository change on a remote box the hub stopped waiting for (past its
/// timeout) has an unknown outcome, and holds the box: the next change in that repository
/// waits until the runner answers the first one (once its git ran and was reaped), and only
/// then runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unanswered_remote_change_holds_the_box_until_the_runner_answers() {
    let dir = tempfile::tempdir().unwrap();
    let mut o = hub_opts(dir.path(), config(None));
    o.change_request_timeout = Duration::from_secs(2);
    let h = TestHub::start_opts(o).await;
    let rdir = dir.path().join("r");
    let held = twr_core::testkit::git::HeldGit::install(&dir.path().join("bin"));
    let runner = Runner::start(runner_opts(&h.url, &rdir), held_box(&rdir, &held));
    h.online().await;
    let repo = twr_core::testkit::git::init_repo(&dir.path().join("box-repo"));
    let t = h.new_thread(&repo, "held").await;
    let wt = rdir.join("worktrees").join("box-repo").join("held");
    // The park's removal is held on the box; the hub stops waiting: outcome unknown.
    let out = tokio::time::timeout(WAIT, park_task(&h, &t))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(out.parked && !out.worktree_removed, "{out:?}");
    let e = out.worktree_error.unwrap_or_default();
    assert!(e.contains("outcome is unknown"), "{e}");
    assert!(held.held() && !held.finished() && wt.exists());
    // The next change in the repository waits for the box to answer the first.
    let next = new_thread_task(&h, &repo, "next");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !next.is_finished(),
        "a change ran while another may still run"
    );
    assert!(!rdir
        .join("worktrees")
        .join("box-repo")
        .join("next")
        .exists());
    held.release();
    let v = tokio::time::timeout(WAIT, next)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(v["id"].is_string(), "{v}");
    assert!(held.finished());
    assert!(!wt.exists(), "the first removal ran on the box");
    assert!(rdir
        .join("worktrees")
        .join("box-repo")
        .join("next")
        .exists());
    runner.shutdown().await;
    h.shutdown().await;
}

/// A repository change whose connection drops while its git runs on the box:
/// the runner says hello again only once that git has run (and was reaped), so the box comes
/// back online only then, and the hub then takes the change as over.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runner_reconnects_only_once_its_repository_changes_are_over() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let proxy = Proxy::start(&h.url).await;
    let rdir = dir.path().join("r");
    let held = twr_core::testkit::git::HeldGit::install(&dir.path().join("bin"));
    let lbox = held_box(&rdir, &held);
    let runner = Runner::start(runner_opts(&proxy.url, &rdir), lbox.clone());
    h.online().await;
    let repo = twr_core::testkit::git::init_repo(&dir.path().join("box-repo"));
    let t = h.new_thread(&repo, "cut").await;
    let wt = rdir.join("worktrees").join("box-repo").join("cut");
    let park = park_task(&h, &t);
    wait_for("git holds the removal", || held.held()).await;
    // The connection drops while the removal's git runs: the park fails (box offline).
    proxy.st().cut_now = true;
    let out = tokio::time::timeout(WAIT, park)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(out.parked && !out.worktree_removed, "{out:?}");
    assert!(out.worktree_error.is_some(), "{out:?}");
    let hub = h.hub().clone();
    wait_for("the box offline", move || {
        hub.boxes()
            .iter()
            .any(|(n, online, _)| n == BOX && !*online)
    })
    .await;
    // The runner does not reconnect while the change runs.
    assert_eq!(lbox.changes_running(), 1);
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        h.hub()
            .boxes()
            .iter()
            .any(|(n, online, _)| n == BOX && !*online),
        "the runner reconnected with a repository change still running"
    );
    assert_eq!(proxy.st().connections, 1);
    held.release();
    h.online().await;
    assert!(held.finished() && !wt.exists());
    assert_eq!(lbox.changes_running(), 0);
    // The box takes changes again.
    let v = tokio::time::timeout(WAIT, new_thread_task(&h, &repo, "after"))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(v["id"].is_string(), "{v}");
    runner.shutdown().await;
    h.shutdown().await;
}

/// Deleting threads on a remote box: the runner's inventory shows an edit `git status` hides
/// and an ignored `.env`, and its commit count takes the whole card as gone (a commit only
/// both threads' branches have is lost and shared). A file that appears after the card is
/// found once the threads are stopped: nothing is removed and a new card lists it;
/// confirming that one deletes both on the box.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_delete_card_lists_everything_and_is_raised_again() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let rdir = dir.path().join("r");
    let runner = Runner::start(runner_opts(&h.url, &rdir), open_box(&rdir));
    h.online().await;
    let repo = twr_core::testkit::git::init_repo(&dir.path().join("box-repo"));
    std::fs::write(repo.join(".gitignore"), ".env\n").unwrap();
    std::fs::write(repo.join("config.local"), "a=1\n").unwrap();
    twr_core::testkit::git::commit_all(&repo, "ignores and config");
    let a = h.new_thread(&repo, "ra").await;
    let b = h.new_thread(&repo, "rb").await;
    let wt = |n: &str| rdir.join("worktrees").join("box-repo").join(n);
    let git = twr_core::testkit::git::run;
    git(
        &wt("ra"),
        &["update-index", "--assume-unchanged", "config.local"],
    );
    std::fs::write(wt("ra").join("config.local"), "a=2 local edit\n").unwrap();
    std::fs::write(wt("ra").join(".env"), "KEY=secret").unwrap();
    std::fs::write(wt("ra").join("a.txt"), "a").unwrap();
    twr_core::testkit::git::commit_all(&wt("ra"), "unpushed");
    git(&wt("rb"), &["reset", "-q", "--hard", "twr/ra"]);
    let user = twr_hub::Caller::user("test");
    let ask = |threads: Vec<String>| {
        let user = user.clone();
        h.run(move |hub| async move { hub.delete_threads(&threads, &user).await })
    };
    let card = ask(vec!["ra".into(), "rb".into()]).await.unwrap();
    let t = &card["threads"];
    assert_eq!(
        (
            &t[0]["hidden_changes"],
            &t[0]["ignored"],
            &t[0]["dirty_files"]
        ),
        (&json!(1), &json!([".env"]), &json!(0)),
        "{card}"
    );
    for (t, other) in t.as_array().unwrap().iter().zip(["rb", "ra"]) {
        assert_eq!(t["branch_only_commits"], 1, "{card}");
        assert_eq!(
            t["shared"],
            json!([{"thread_name": other, "commits": 1}]),
            "{card}"
        );
    }
    let summary = card["summary"].as_str().unwrap();
    assert!(
        summary.contains("with it its ignored files (.env)")
            && summary.contains("skip-worktree: config.local)"),
        "{summary}"
    );

    std::fs::write(wt("rb").join("late.txt"), "late").unwrap();
    let respond = |item: &Value| {
        let (attn, rev) = (
            item["attn_id"].as_str().unwrap().to_string(),
            item["revision"].as_u64(),
        );
        h.sync(|hub| hub.respond_at(&attn, Answer::new("Delete"), rev, &user))
    };
    respond(&card).unwrap();
    let hub = h.hub().clone();
    let first = card["attn_id"].clone();
    wait_for("a new delete card", || {
        hub.attention_list(false)
            .iter()
            .any(|i| i["attn_id"] != first && i["thread"] == HUB_THREAD)
    })
    .await;
    let fresh = hub
        .attention_list(false)
        .into_iter()
        .find(|i| i["attn_id"] != first && i["thread"] == HUB_THREAD)
        .unwrap();
    let summary = fresh["summary"].as_str().unwrap();
    assert!(
        summary.contains("there is more to lose (rb: uncommitted files)")
            && summary.contains("Their runtimes were stopped and stay stopped."),
        "{summary}"
    );
    assert!(wt("ra").exists() && wt("rb").join("late.txt").exists());
    assert!(hub.thread_view(&a).unwrap().parked && hub.thread_view(&b).unwrap().parked);
    respond(&fresh).unwrap();
    wait_for("both deleted", || {
        hub.thread_view(&a).is_err() && hub.thread_view(&b).is_err()
    })
    .await;
    assert!(!wt("ra").exists() && !wt("rb").exists());
    assert!(!twr_core::testkit::git::branch_exists(&repo, "twr/ra"));

    // Both are in the box's own trash: listed through the runner, one restored on the box
    // (copied: its item stays, listed as restored), then both purged there.
    let list = h
        .run(|hub| async move { hub.trash_list().await })
        .await
        .unwrap();
    let items = list["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "{list}");
    for it in items {
        assert_eq!(it["box"], json!(BOX), "{it}");
        assert!(
            it["dir"]
                .as_str()
                .unwrap()
                .starts_with(rdir.join("trash").to_str().unwrap()),
            "{it}"
        );
    }
    let restored = {
        let user = user.clone();
        h.run(move |hub| async move { hub.trash_restore("ra", None, &user).await })
            .await
            .unwrap()
    };
    assert_eq!(restored["worktree"], json!(wt("ra")), "{restored}");
    assert!(wt("ra").join(".env").exists() && wt("ra").join("a.txt").exists());
    assert!(twr_core::testkit::git::branch_exists(&repo, "twr/ra"));
    assert_eq!(
        git(&wt("ra"), &["rev-parse", "--abbrev-ref", "HEAD"]),
        "twr/ra"
    );
    assert!(hub.thread_view(&a).unwrap().parked);
    let list = h
        .run(|hub| async move { hub.trash_list().await })
        .await
        .unwrap();
    let ra = list["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["thread_name"] == "ra")
        .cloned()
        .unwrap();
    assert_eq!(ra["restored_as"], json!("ra"), "{list}");
    let emptied = {
        let user = user.clone();
        h.run(move |hub| async move { hub.trash_empty(&[], true, &user).await })
            .await
            .unwrap()
    };
    // The restored item stayed until now, listed as restored; it goes with the rest.
    let mut purged: Vec<&str> = emptied["purged"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["thread_name"].as_str().unwrap())
        .collect();
    purged.sort();
    assert_eq!(purged, ["ra", "rb"], "{emptied}");
    assert!(wt("ra").join("a.txt").exists());
    let left: Vec<_> = std::fs::read_dir(rdir.join("trash")).unwrap().collect();
    assert!(left.is_empty(), "{left:?}");
    runner.shutdown().await;
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_attach_quiesces_through_the_runner_and_returns_an_ssh_command() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(Some("me@gpu.lan"))).await;
    let rdir = dir.path().join("r");
    let runner = Runner::start(runner_opts(&h.url, &rdir), open_box(&rdir));
    h.online().await;
    let proj = project(dir.path());
    let t = h.new_thread(&proj, "att").await;
    let mut sub = h.hub().subscribe_thread(&t, 0).unwrap();
    h.send(&t, "first");
    let evs = until(&mut sub, |e| e.kind() == "report.ready").await;
    let session = evs
        .iter()
        .find_map(|e| match &e.body {
            EventBody::ThreadStarted(p) => p.session_id.clone(),
            _ => None,
        })
        .unwrap();
    let grant = h
        .run(|hub| {
            let t = t.clone();
            async move { hub.attach_begin(&t, false, "cli-1").await }
        })
        .await
        .unwrap();
    assert_eq!(grant.box_name.as_deref(), Some(BOX));
    assert_eq!(grant.cwd, None);
    // Only the attach and agent markers, for the local `ssh`.
    assert_eq!(
        grant.env.keys().collect::<Vec<_>>(),
        ["TWR_AGENT", "TWR_ATTACH"],
        "{:?}",
        grant.env
    );
    assert_eq!(grant.argv[..grant.argv.len() - 1], attach_ssh("me@gpu.lan"));
    let remote = &grant.argv[grant.argv.len() - 1];
    assert!(
        remote.starts_with(&format!("cd '{}' && exec ", proj.display())),
        "{remote}"
    );
    assert!(remote.contains(&format!("'{session}'")), "{remote}");
    // The runtime was stopped on the box, and its exit (through the runner) attached the
    // thread.
    let evs = until(&mut sub, |e| e.kind() == "thread.attached").await;
    assert_eq!(
        count(&evs, "thread.stopped") + count(&evs, "thread.crashed"),
        0
    );
    assert_eq!(h.hub().get_thread(&t).unwrap()["status"], json!("attached"));
    let ended = h.sync(|hub| hub.attach_end(&t, "cli-1", grant.lease, None).unwrap());
    assert!(ended);
    // Detached: the next prompt resumes the session on the box.
    h.send(&t, "second");
    until(&mut sub, |e| {
        e.kind() == "report.ready" && e.turn == Some(2)
    })
    .await;
    // Without ssh configured: unsupported.
    runner.shutdown().await;
    h.shutdown().await;

    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let rdir = dir.path().join("r");
    let runner = Runner::start(runner_opts(&h.url, &rdir), open_box(&rdir));
    h.online().await;
    let t = h.new_thread(&project(dir.path()), "nossh").await;
    let mut sub = h.hub().subscribe_thread(&t, 0).unwrap();
    h.send(&t, "first");
    until(&mut sub, |e| e.kind() == "report.ready").await;
    let e = h
        .run(|hub| async move { hub.attach_begin(&t, false, "cli-1").await })
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::UNSUPPORTED);
    assert!(e.message.contains("ssh"), "{}", e.message);
    runner.shutdown().await;
    h.shutdown().await;
}

/// An `acp` preset with an attach template, on a box: the runner's spawn and resume run the
/// preset's agent (its command reaches the adapter on the box), and `twr attach` runs the
/// template over ssh exactly as a local attach runs it (the runner's adapter needs the
/// template to say the runtime can attach at all).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_acp_preset_attaches_with_its_template() {
    let dir = tempfile::tempdir().unwrap();
    let stub = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../twr-adapters/tests/fixtures/acp/stub_agent.py")
        .canonicalize()
        .unwrap();
    let agent = dir.path().join("agent");
    std::fs::create_dir_all(&agent).unwrap();
    let mut cfg = config(Some("me@gpu.lan"));
    cfg.runtimes.insert(
        "stub".into(),
        twr_core::config::RuntimePreset {
            acp: format!("python3 {} {}", stub.display(), agent.display()),
            attach: Some("stub-cli --resume {session}".into()),
        },
    );
    let h = TestHub::start(dir.path(), cfg).await;
    let rdir = dir.path().join("r");
    let runner = Runner::start(runner_opts(&h.url, &rdir), open_box(&rdir));
    h.online().await;
    let proj = project(dir.path());
    let req = NewThread {
        project: proj.to_string_lossy().into_owned(),
        runtime: Some("stub".into()),
        name: Some("acp".into()),
        box_name: Some(BOX.into()),
        ..Default::default()
    };
    let v = h
        .run(|hub| async move { hub.new_thread(req, "test").await })
        .await
        .unwrap();
    assert_eq!(v["runtime"], "acp");
    let t = v["id"].as_str().unwrap().to_string();
    let mut sub = h.hub().subscribe_thread(&t, 0).unwrap();
    h.send(&t, "hello");
    let evs = until(&mut sub, |e| e.kind() == "turn.ended").await;
    let session = evs
        .iter()
        .find_map(|e| match &e.body {
            EventBody::ThreadStarted(p) => p.session_id.clone(),
            _ => None,
        })
        .unwrap();
    let grant = h
        .run(|hub| {
            let t = t.clone();
            async move { hub.attach_begin(&t, false, "cli-1").await }
        })
        .await
        .unwrap();
    assert_eq!(grant.runtime, "acp");
    assert_eq!(grant.argv[..grant.argv.len() - 1], attach_ssh("me@gpu.lan"));
    let remote = &grant.argv[grant.argv.len() - 1];
    assert!(
        remote.starts_with(&format!("cd '{}' && exec ", proj.display())),
        "{remote}"
    );
    assert!(
        remote.ends_with(&format!(" 'stub-cli' '--resume' '{session}'")),
        "{remote}"
    );
    until(&mut sub, |e| e.kind() == "thread.attached").await;
    assert!(h.sync(|hub| hub.attach_end(&t, "cli-1", grant.lease, None).unwrap()));
    // Detached: resumed on the box with the preset's agent; the session remembers.
    h.send(&t, "recall");
    let evs = until(&mut sub, |e| e.kind() == "turn.ended" && e.turn == Some(2)).await;
    let text: String = evs
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::AgentText(t) if t.is_final => Some(t.text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(text, "before: hello", "{evs:?}");
    let stdin = std::fs::read_to_string(agent.join("stdin.jsonl")).unwrap();
    assert!(stdin.contains("\"session/resume\""), "{stdin}");
    runner.shutdown().await;
    h.shutdown().await;
}

/// Thread `b` of the box works in thread `a`'s worktree there (recorded the way a hub could
/// before `thread.new` refused that). The box cannot see an attach (it runs over ssh, not
/// through the runner), so the hub keeps a worktree an attached thread works in from a park,
/// and refuses an attach into a worktree being parked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_attach_keeps_its_directory_from_a_park_of_the_worktrees_thread() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(Some("me@gpu.lan"))).await;
    let rdir = dir.path().join("r");
    let held = twr_core::testkit::git::HeldGit::install(&dir.path().join("bin"));
    let mut bo = LocalBoxOptions::new(&rdir, BOX);
    bo.adapters = with_fake();
    bo.runtimes.push("fake".into());
    bo.git = twr_runner::local::LocalGit::with_bin(held.bin.clone());
    let runner = Runner::start(runner_opts(&h.url, &rdir), LocalBox::open(bo).unwrap());
    h.online().await;
    let repo = twr_core::testkit::git::init_repo(&dir.path().join("box-repo"));
    let a = h.new_thread(&repo, "owner").await;
    let wt = rdir.join("worktrees").join("box-repo").join("owner");
    assert!(wt.join("README.md").exists());
    let b = "in-guest".to_string();
    let mut twr = JsonMap::new();
    twr.insert("twr".into(), json!({"worktree": "repo"}));
    h.sync(|hub| {
        hub.append(
            &b,
            None,
            Source::User("test".into()),
            EventBody::ThreadCreated(ThreadCreated {
                name: "guest".into(),
                project: Some("box-repo".into()),
                repo: Some(repo.to_string_lossy().into_owned()),
                worktree: Some(wt.to_string_lossy().into_owned()),
                box_name: BOX.into(),
                runtime: "fake".into(),
                mode: "manual".into(),
                ext: Some(twr),
                ..Default::default()
            }),
            None,
        )
        .unwrap()
    });
    let mut sub = h.hub().subscribe_thread(&b, 0).unwrap();
    h.send(&b, "hello");
    until(&mut sub, |e| e.kind() == "report.ready").await;
    let attach = || {
        let b = b.clone();
        h.run(|hub| async move { hub.attach_begin(&b, false, "cli").await })
    };
    let grant = attach().await.unwrap();
    assert!(
        grant.argv[grant.argv.len() - 1].contains(&wt.to_string_lossy().into_owned()),
        "{grant:?}"
    );
    until(&mut sub, |e| e.kind() == "thread.attached").await;
    // Parked while b is attached in its worktree: kept.
    let out = h
        .run({
            let a = a.clone();
            |hub| async move {
                hub.park_thread(&a, false, false, &twr_hub::Caller::user("test"))
                    .await
            }
        })
        .await
        .unwrap();
    assert!(out.parked && !out.worktree_removed, "{out:?}");
    let reason = out.kept_reason.unwrap_or_default();
    assert!(
        reason.contains("guest") && reason.contains("cli"),
        "{reason}"
    );
    assert!(wt.join("README.md").exists());
    assert!(!held.held() && !held.finished());
    assert!(h.sync(|hub| hub.attach_end(&b, "cli", grant.lease, None).unwrap()));

    // While a park of the worktree is under way (git held on the box), b cannot attach.
    h.run({
        let a = a.clone();
        |hub| async move { hub.resume_thread(&a, &twr_hub::Caller::user("test")).await }
    })
    .await
    .unwrap();
    let parking = {
        let a = a.clone();
        let hub = h.hub().clone();
        h.rt.as_ref().unwrap().spawn(async move {
            hub.park_thread(&a, false, false, &twr_hub::Caller::user("test"))
                .await
        })
    };
    wait_for("git held on the box", || held.held()).await;
    let e = attach().await.unwrap_err();
    assert_eq!(e.code, codes::CONFLICT, "{e:?}");
    assert!(e.message.contains("being parked"), "{}", e.message);
    held.release();
    let out = tokio::time::timeout(Duration::from_secs(120), parking)
        .await
        .expect("the park never finished")
        .unwrap()
        .unwrap();
    assert!(out.worktree_removed, "{out:?}");
    runner.shutdown().await;
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runner_restart_that_lost_the_process_closes_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let rdir = dir.path().join("r");
    let lbox = open_box(&rdir);
    let runner = Runner::start(runner_opts(&h.url, &rdir), lbox.clone());
    h.online().await;
    let t = h.new_thread(&project(dir.path()), "lost").await;
    let mut sub = h.hub().subscribe_thread(&t, 0).unwrap();
    h.send(&t, "!perm Bash make deploy");
    until(&mut sub, |e| e.kind() == "attention.raised").await;
    // The runner process dies (its fake runtime with it, nothing spooled).
    runner.crash().await;
    drop(lbox);
    let hub = h.hub().clone();
    wait_for("offline", move || !hub.boxes()[0].1).await;
    // Offline: the thread keeps its state.
    assert_eq!(h.hub().get_thread(&t).unwrap()["status"], json!("waiting"));
    let lbox = open_box(&rdir);
    let runner = Runner::start(runner_opts(&h.url, &rdir), lbox.clone());
    let evs = until(&mut sub, |e| e.kind() == "thread.crashed").await;
    let ks = kinds(&evs);
    let crash = ks.iter().position(|k| k == "thread.crashed").unwrap();
    assert_eq!(
        &ks[crash - 2..=crash],
        ["turn.ended", "attention.resolved", "thread.crashed"]
    );
    let EventBody::ThreadCrashed(c) = &evs[crash].body else {
        panic!()
    };
    // The new runner records the exit of the session its predecessor left (its start
    // intent); without it the reconciliation would close the thread.
    assert!(
        c.stderr_tail.contains("died without recording its exit")
            || c.stderr_tail.contains("does not run this runtime"),
        "{}",
        c.stderr_tail
    );
    h.online().await;
    // The thread works again: a fresh (resumed) session on the new runner.
    h.send(&t, "after");
    until(&mut sub, |e| {
        e.kind() == "report.ready" && e.turn == Some(2)
    })
    .await;
    runner.shutdown().await;
    h.shutdown().await;
}

// ------------------------------------------------------------------------------------------
// Box ownership, attention ids, recorded commands, fencing during retries, the replay
// boundary, backpressure
// ------------------------------------------------------------------------------------------

/// A raw runner, connected, synced and online, that reports `running`.
async fn raw_online(h: &TestHub, running: Vec<RunningSession>) -> RawWs {
    raw_online_on(h, raw(&h.url).await, running).await
}

/// [`raw_online`] on connection `ws`.
async fn raw_online_on(h: &TestHub, mut ws: RawWs, running: Vec<RunningSession>) -> RawWs {
    let mut hi = hello(TOKEN);
    hi.running = running;
    raw_hello(&mut ws, hi).await;
    let Some(HubMsg::Welcome { .. }) = raw_expect(&mut ws).await else {
        panic!("no welcome")
    };
    raw_send(&mut ws, &RunnerMsg::Synced).await;
    h.online().await;
    ws
}

/// The next command from the hub (acks skipped).
async fn raw_cmd(ws: &mut RawWs) -> (String, Op) {
    loop {
        match raw_expect(ws).await {
            Some(HubMsg::Cmd { cmd_id, op }) => return (cmd_id, op),
            Some(_) => continue,
            None => panic!("closed"),
        }
    }
}

async fn raw_ok(ws: &mut RawWs, cmd_id: String) {
    raw_send(
        ws,
        &RunnerMsg::CmdResult(CmdResult::new(cmd_id, Ok(Value::Null))),
    )
    .await;
}

/// Sends frame `m` and waits for its ack; returns the commands that came meanwhile.
async fn raw_frame(ws: &mut RawWs, m: RunnerMsg) -> Vec<(String, Op)> {
    let (thread, rseq) = match &m {
        RunnerMsg::Event { thread, rseq, .. } | RunnerMsg::Exited { thread, rseq, .. } => {
            (thread.clone(), *rseq)
        }
        other => panic!("not a frame: {other:?}"),
    };
    raw_send(ws, &m).await;
    let mut cmds = Vec::new();
    loop {
        match raw_expect(ws).await {
            Some(HubMsg::Ack { thread: t, rseq: r }) if t == thread && r == rseq => return cmds,
            Some(HubMsg::Cmd { cmd_id, op }) => cmds.push((cmd_id, op)),
            Some(_) => continue,
            None => panic!("closed before the ack of {thread}/{rseq}"),
        }
    }
}

/// An `event` frame of run `r1` ([`run_id`]).
fn frame(thread: &str, rseq: u64, turn: Option<u64>, body: EventBody) -> RunnerMsg {
    RunnerMsg::Event {
        thread: thread.into(),
        rseq,
        turn,
        body,
        raw: None,
        run: Some(run_id(thread, "r1")),
        at_ms: None,
    }
}

fn started(session: &str) -> EventBody {
    EventBody::ThreadStarted(SessionPayload {
        session_id: Some(session.into()),
        ..Default::default()
    })
}

fn question(attn_id: &str) -> EventBody {
    EventBody::AttentionRaised(AttentionRaised {
        attn_id: attn_id.into(),
        reason: AttentionReason::Question,
        summary: "which one?".into(),
        ..Default::default()
    })
}

/// Open attention items as `(attn_id, thread)`.
fn items(hub: &Hub) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = hub
        .attention_list(true)
        .iter()
        .map(|i| {
            (
                i["attn_id"].as_str().unwrap().to_string(),
                i["thread"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    v.sort();
    v
}

impl TestHub {
    /// A thread on the hub's own box.
    async fn new_local_thread(&self, project: &Path, name: &str) -> String {
        let req = NewThread {
            project: project.to_string_lossy().into_owned(),
            runtime: Some("fake".into()),
            name: Some(name.into()),
            ..Default::default()
        };
        let v = self
            .run(|h| async move { h.new_thread(req, "test").await })
            .await
            .unwrap();
        v["id"].as_str().unwrap().to_string()
    }

    /// A thread on the box whose session a raw runner has spawned (`started`, rseq 1).
    async fn raw_session(&self, ws: &mut RawWs, dir: &Path, name: &str) -> String {
        let t = self.new_thread_raw_named(ws, dir, name).await;
        self.send(&t, "hi");
        let (cmd_id, op) = raw_cmd(ws).await;
        assert!(matches!(op, Op::Spawn { .. }), "{op:?}");
        raw_ok(ws, cmd_id).await;
        raw_frame(ws, frame(&t, 1, None, started(&format!("s-{name}")))).await;
        t
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runner_cannot_touch_the_threads_of_other_boxes() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let local = h.new_local_thread(&project(dir.path()), "home").await;
    let mut sub = h.hub().subscribe_thread(&local, 0).unwrap();
    h.send(&local, "!perm Bash ls");
    let evs = until(&mut sub, |e| e.kind() == "attention.raised").await;
    let attn = evs
        .iter()
        .find_map(|e| match &e.body {
            EventBody::AttentionRaised(a) => Some(a.attn_id.clone()),
            _ => None,
        })
        .unwrap();
    let before = log(h.hub(), &local).len();
    let mut ws = raw_online(&h, vec![]).await;
    // The gpu runner forges an event and an exit of the local thread: acked (its spool
    // drains), counted, and nothing reaches the thread.
    let forged = RunnerMsg::Event {
        thread: local.clone(),
        rseq: 1,
        turn: Some(1),
        body: EventBody::AgentText(AgentText {
            text: "forged".into(),
            ..Default::default()
        }),
        raw: None,
        run: Some("r-evil".into()),
        at_ms: None,
    };
    let mut cmds = raw_frame(&mut ws, forged).await;
    let exit = RunnerMsg::Exited {
        thread: local.clone(),
        rseq: 2,
        code: Some(0),
        stderr_tail: String::new(),
        run: Some("r-evil".into()),
        at_ms: None,
    };
    cmds.extend(raw_frame(&mut ws, exit).await);
    assert_eq!(h.hub().box_rejected_frames(BOX), 2);
    assert_eq!(
        log(h.hub(), &local).len(),
        before,
        "{:?}",
        kinds(&log(h.hub(), &local))
    );
    assert_eq!(
        h.hub().get_thread(&local).unwrap()["status"],
        json!("waiting")
    );
    // The runner is told to stop its own session of that id, and only that run.
    let stop = match cmds.into_iter().next() {
        Some(c) => c,
        None => raw_cmd(&mut ws).await,
    };
    assert_eq!(
        stop.1,
        Op::Stop {
            thread: local.clone(),
            run: Some("r-evil".into())
        }
    );
    // The local runtime never noticed.
    h.sync(|hub| hub.respond(&attn, Answer::allow(), "test").unwrap());
    until(&mut sub, |e| e.kind() == "report.ready").await;
    let all = log(h.hub(), &local);
    assert_eq!(
        count(&all, "thread.crashed") + count(&all, "thread.stopped"),
        0
    );
    assert!(!deltas(&all).contains(&"forged".to_string()));
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_attention_ids_are_namespaced_by_thread_and_never_move_items() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let local = h.new_local_thread(&project(dir.path()), "home").await;
    let mut sub = h.hub().subscribe_thread(&local, 0).unwrap();
    h.send(&local, "!perm Bash ls");
    let evs = until(&mut sub, |e| e.kind() == "attention.raised").await;
    let a_local = evs
        .iter()
        .find_map(|e| match &e.body {
            EventBody::AttentionRaised(a) => Some(a.attn_id.clone()),
            _ => None,
        })
        .unwrap();
    assert!(a_local.starts_with(&format!("{local}:")), "{a_local}");
    let mut ws = raw_online(&h, vec![]).await;
    let t1 = h.raw_session(&mut ws, dir.path(), "r1").await;
    let t2 = h.raw_session(&mut ws, dir.path(), "r2").await;
    // A runner raising the local item's id raises its own item: the local one stays put.
    raw_frame(&mut ws, frame(&t1, 2, Some(1), question(&a_local))).await;
    let t1_local = format!("{t1}:1:{a_local}");
    assert_eq!(items(h.hub()), {
        let mut v = vec![(a_local.clone(), local.clone()), (t1_local, t1.clone())];
        v.sort();
        v
    });
    // Two threads of the box raising the same id (runtimes number their requests per
    // process) each get their own item; a re-raise of an open id updates it.
    let dropped = h.hub().info()["dropped_events"].as_u64().unwrap();
    raw_frame(&mut ws, frame(&t1, 3, Some(1), question("q1"))).await;
    raw_frame(&mut ws, frame(&t2, 2, Some(1), question("q1"))).await;
    raw_frame(&mut ws, frame(&t1, 4, Some(1), question("q1"))).await;
    let (q1, q2) = (format!("{t1}:1:q1"), format!("{t2}:1:q1"));
    let all = items(h.hub());
    assert!(all.contains(&(q1.clone(), t1.clone())), "{all:?}");
    assert!(all.contains(&(q2.clone(), t2.clone())), "{all:?}");
    assert_eq!(all.len(), 4, "{all:?}");
    assert_eq!(h.hub().info()["dropped_events"].as_u64().unwrap(), dropped);
    // A withdrawal resolves its own thread's item of that id only.
    let withdraw = EventBody::AttentionResolved(AttentionResolved {
        attn_id: "q1".into(),
        by: ResolvedBy::Timeout,
        ..Default::default()
    });
    raw_frame(&mut ws, frame(&t2, 3, Some(1), withdraw)).await;
    assert!(items(h.hub()).contains(&(q1.clone(), t1.clone())));
    assert!(!items(h.hub()).iter().any(|(_, t)| t == &t2));
    // Once resolved, the id is free again (a resumed runtime reuses ids).
    raw_frame(&mut ws, frame(&t2, 4, Some(1), question("q1"))).await;
    assert!(items(h.hub()).contains(&(q2.clone(), t2.clone())));
    // Each answer goes to its thread's session under the runner's own id, as a recorded
    // command.
    // q1 was raised again: a bare id no longer answers it (the question may have changed);
    // the current revision does.
    h.sync(|hub| {
        let e = hub
            .respond(&q1, Answer::new("this one"), "test")
            .unwrap_err();
        assert_eq!(e.code, codes::CONFLICT, "{e:?}");
        let rev = hub
            .attention_list(true)
            .iter()
            .find(|i| i["attn_id"] == q1.as_str())
            .and_then(|i| i["revision"].as_u64());
        hub.respond_at(
            &q1,
            Answer::new("this one"),
            rev,
            &twr_hub::Caller::user("test"),
        )
        .unwrap()
    });
    let (cmd_id, op) = raw_cmd(&mut ws).await;
    assert!(cmd_id.starts_with("respond:"), "{cmd_id}");
    let db = rusqlite::Connection::open(dir.path().join("hub/twr.db")).unwrap();
    let recorded: String = db
        .query_row("SELECT cmd_id FROM remote_outbox", [], |r| r.get(0))
        .unwrap();
    assert_eq!(recorded, cmd_id);
    assert!(
        matches!(&op, Op::Respond { thread, attn_id, answer, .. }
            if thread == &t1 && attn_id == "q1" && answer.decision == "this one"),
        "{op:?}"
    );
    raw_ok(&mut ws, cmd_id).await;
    h.sync(|hub| hub.respond(&q2, Answer::new("that one"), "test").unwrap());
    let (cmd_id, op) = raw_cmd(&mut ws).await;
    assert!(
        matches!(&op, Op::Respond { thread, attn_id, answer, .. }
            if thread == &t2 && attn_id == "q1" && answer.decision == "that one"),
        "{op:?}"
    );
    raw_ok(&mut ws, cmd_id).await;
    assert_eq!(h.hub().info()["dropped_events"].as_u64().unwrap(), dropped);
    // The local item was never touched.
    assert!(items(h.hub()).contains(&(a_local.clone(), local.clone())));
    h.shutdown().await;
}

/// Rows of the hub's `remote_outbox`.
fn outbox_rows(dir: &Path) -> i64 {
    rusqlite::Connection::open(dir.join("hub/twr.db"))
        .unwrap()
        .query_row("SELECT COUNT(*) FROM remote_outbox", [], |r| r.get(0))
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_or_respond_lost_with_the_hub_is_sent_again_by_the_next_one() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let proxy = Proxy::start(&h.url).await;
    let rdir = dir.path().join("r");
    let runner = Runner::start(runner_opts(&proxy.url, &rdir), open_box(&rdir));
    h.online().await;
    let t = h.new_thread(&project(dir.path()), "durable").await;
    let mut sub = h.hub().subscribe_thread(&t, 0).unwrap();
    h.send(&t, "!perm Bash make");
    let evs = until(&mut sub, |e| e.kind() == "attention.raised").await;
    let attn = evs
        .iter()
        .find_map(|e| match &e.body {
            EventBody::AttentionRaised(a) => Some(a.attn_id.clone()),
            _ => None,
        })
        .unwrap();
    // The answer is recorded, its `respond` is lost in transport, then the hub dies.
    proxy.st().drop_cmd = Some("respond");
    h.sync(|hub| hub.respond(&attn, Answer::allow(), "test").unwrap());
    let p = proxy.clone();
    wait_for("the respond lost", move || p.st().dropped_cmds == 1).await;
    assert_eq!(outbox_rows(dir.path()), 1);
    drop(sub);
    h.crash();

    // The next hub sends it again (same id) once the runner is back: the turn completes.
    let h = TestHub::start(dir.path(), config(None)).await;
    proxy.st().upstream = h.url.clone();
    let mut sub = h.hub().subscribe_thread(&t, 0).unwrap();
    until(&mut sub, |e| e.kind() == "report.ready").await;
    let d = dir.path().to_path_buf();
    wait_for("the respond forgotten", move || outbox_rows(&d) == 0).await;
    let tool_ok = log(h.hub(), &t)
        .iter()
        .any(|e| matches!(&e.body, EventBody::AgentToolResult(r) if r.ok));
    assert!(tool_ok, "the answer reached the runtime");

    // Same for a prompt: `turn.started` is recorded, its `send` lost, the hub dies.
    proxy.st().drop_cmd = Some("send");
    h.send(&t, "second prompt words");
    let p = proxy.clone();
    wait_for("the send lost", move || p.st().dropped_cmds == 2).await;
    assert_eq!(outbox_rows(dir.path()), 1);
    drop(sub);
    h.crash();
    let h = TestHub::start(dir.path(), config(None)).await;
    proxy.st().upstream = h.url.clone();
    let mut sub = h.hub().subscribe_thread(&t, 0).unwrap();
    until(&mut sub, |e| {
        e.kind() == "report.ready" && e.turn == Some(2)
    })
    .await;
    let all = log(h.hub(), &t);
    assert_eq!(deltas(&all), ["second", "prompt", "words"]);
    assert_eq!(count(&all, "thread.started"), 1, "{:?}", kinds(&all));
    assert_eq!(count(&all, "thread.crashed"), 0, "{:?}", kinds(&all));
    let d = dir.path().to_path_buf();
    wait_for("the send forgotten", move || outbox_rows(&d) == 0).await;
    runner.shutdown().await;
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_fenced_while_its_frame_is_retried_commits_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws1 = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws1, dir.path(), "fence").await;
    // The store refuses agent.text: the frame's commit is retried...
    let db = rusqlite::Connection::open(dir.path().join("hub/twr.db")).unwrap();
    db.busy_timeout(Duration::from_secs(5)).unwrap();
    db.execute_batch(
        "CREATE TRIGGER inject BEFORE INSERT ON events WHEN NEW.kind = 'agent.text'
         BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    )
    .unwrap();
    let text = || {
        frame(
            &t,
            2,
            Some(1),
            EventBody::AgentText(AgentText {
                text: "once".into(),
                delta: true,
                ..Default::default()
            }),
        )
    };
    raw_send(&mut ws1, &text()).await;
    let hub = h.hub().clone();
    wait_for("a retry", move || {
        hub.info()["persist_retries"].as_u64().unwrap() > 0
    })
    .await;
    // ...and a newer connection of the box fences this one meanwhile.
    let mut ws2 = raw(&h.url).await;
    let mut hi = hello(TOKEN);
    hi.running = vec![RunningSession {
        thread: t.clone(),
        runtime: "fake".into(),
        run: Some("r1".into()),
        ..Default::default()
    }];
    raw_hello(&mut ws2, hi).await;
    let Some(HubMsg::Welcome { watermarks, .. }) = raw_expect(&mut ws2).await else {
        panic!("no welcome")
    };
    assert_eq!(watermarks.get(&t), Some(&1));
    db.execute_batch("DROP TRIGGER inject").unwrap();
    // The fenced connection's retry commits nothing and acks nothing.
    loop {
        match raw_expect(&mut ws1).await {
            Some(HubMsg::Ack { rseq, .. }) => panic!("the fenced connection got ack {rseq}"),
            Some(HubMsg::Error { code, .. }) => assert_eq!(code, hub_errors::FENCED),
            Some(_) => continue,
            None => break,
        }
    }
    assert_eq!(count(&log(h.hub(), &t), "agent.text"), 0);
    // The new connection replays it: recorded once.
    raw_frame(&mut ws2, text()).await;
    raw_send(&mut ws2, &RunnerMsg::Synced).await;
    h.online().await;
    assert_eq!(deltas(&log(h.hub(), &t)), ["once"]);
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_exit_in_the_replay_is_no_stray_and_its_replacement_is_never_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "replay").await;
    let ended = EventBody::TurnEnded(TurnEnded {
        result: TurnResult::Completed,
        ..Default::default()
    });
    raw_frame(&mut ws, frame(&t, 2, Some(1), ended)).await;
    // The connection drops; a prompt waits for the box.
    drop(ws);
    let hub = h.hub().clone();
    wait_for("offline", move || !hub.boxes()[0].1).await;
    h.send(&t, "next");
    // The runner comes back: its hello still lists run r1, whose exit is in the replay.
    let mut ws = raw(&h.url).await;
    let mut hi = hello(TOKEN);
    hi.running = vec![RunningSession {
        thread: t.clone(),
        runtime: "fake".into(),
        run: Some(run_id(&t, "r1")),
        ..Default::default()
    }];
    raw_hello(&mut ws, hi).await;
    let Some(HubMsg::Welcome { .. }) = raw_expect(&mut ws).await else {
        panic!("no welcome")
    };
    let exited = RunnerMsg::Exited {
        thread: t.clone(),
        rseq: 3,
        code: Some(0),
        stderr_tail: String::new(),
        run: Some(run_id(&t, "r1")),
        at_ms: None,
    };
    raw_frame(&mut ws, exited).await;
    raw_send(&mut ws, &RunnerMsg::Synced).await;
    // The replacement is resumed and gets the prompt; nothing is stopped.
    let (cmd_id, op) = raw_cmd(&mut ws).await;
    assert!(
        matches!(&op, Op::Resume { thread, .. } if thread == &t),
        "{op:?}"
    );
    raw_ok(&mut ws, cmd_id).await;
    let (cmd_id, op) = raw_cmd(&mut ws).await;
    assert!(matches!(&op, Op::Send { turn: 2, .. }), "{op:?}");
    raw_ok(&mut ws, cmd_id).await;
    let more = tokio::time::timeout(Duration::from_millis(700), raw_cmd(&mut ws)).await;
    assert!(more.is_err(), "unexpected command {:?}", more.map(|c| c.1));
    let all = log(h.hub(), &t);
    assert_eq!(count(&all, "turn.started"), 2, "{:?}", kinds(&all));
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runner_that_does_not_read_is_disconnected() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let ws = raw_online(&h, vec![]).await;
    // Pings as fast as possible, never reading a pong.
    let (mut sink, _stream) = ws.split();
    let ping = Message::Text(serde_json::to_string(&RunnerMsg::Ping).unwrap().into());
    let flood = async {
        loop {
            for _ in 0..256 {
                if sink.feed(ping.clone()).await.is_err() {
                    return;
                }
            }
            if sink.flush().await.is_err() {
                return;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(30), flood)
        .await
        .expect("the hub kept a non-reading runner connected");
    let hub = h.hub().clone();
    wait_for("offline", move || !hub.boxes()[0].1).await;
    let logs = String::from_utf8(LOGS.lock().unwrap().clone()).unwrap();
    assert!(
        logs.contains("send queue is full") || logs.contains("did not take a message"),
        "{}",
        &logs[logs.len().saturating_sub(4000)..]
    );
    h.shutdown().await;
}

/// Everything any test here logs.
static LOGS: Mutex<Vec<u8>> = Mutex::new(Vec::new());

// ------------------------------------------------------------------------------------------
// Lost runtimes, durable stops, expired commands, attach, admission, late exits
// ------------------------------------------------------------------------------------------

/// Numbers a runner supplies at their limits. An `epoch_hint` of `u64::MAX`
/// (twice) neither picks the epoch nor leaves the hub unable to number the next connection,
/// and replacements never share an epoch; an out-of-range `turn` is dropped (not a store
/// failure the replay would repeat forever); an `rseq` of `u64::MAX` is a gap.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_supplied_numbers_at_their_limits_break_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut hi = hello(TOKEN);
    hi.epoch_hint = Some(u64::MAX);
    let mut a = raw(&h.url).await;
    raw_hello(&mut a, hi.clone()).await;
    let Some(HubMsg::Welcome { epoch: e1, .. }) = raw_expect(&mut a).await else {
        panic!("no welcome")
    };
    assert!(e1 < u64::MAX, "the hint picked the epoch: {e1}");
    raw_send(&mut a, &RunnerMsg::Synced).await;
    h.online().await;
    // The same hint again: welcomed with a later epoch, the first connection fenced.
    let mut b = raw(&h.url).await;
    raw_hello(&mut b, hi).await;
    let Some(HubMsg::Welcome { epoch: e2, .. }) = raw_expect(&mut b).await else {
        panic!("no welcome for the replacement")
    };
    assert!(e2 > e1, "{e1} then {e2}");
    match raw_expect(&mut a).await {
        Some(HubMsg::Error { code, .. }) => assert_eq!(code, hub_errors::FENCED),
        other => panic!("expected fenced, got {other:?}"),
    }
    assert!(raw_expect(&mut a).await.is_none(), "closed after fencing");
    raw_send(&mut b, &RunnerMsg::Synced).await;
    h.online().await;
    let t = h.raw_session(&mut b, dir.path(), "lim").await;
    // The old connection's end never took its replacement offline.
    assert_eq!(count(&log(h.hub(), HUB_THREAD), "runner.disconnected"), 0);
    let text = |s: &str| {
        EventBody::AgentText(AgentText {
            text: s.into(),
            ..Default::default()
        })
    };
    // A turn SQLite cannot hold: acked and dropped.
    raw_frame(&mut b, frame(&t, 2, Some(u64::MAX), text("far"))).await;
    assert_eq!(count(&log(h.hub(), &t), "agent.text"), 0);
    // The largest rseq: a gap, the connection closes.
    raw_send(&mut b, &frame(&t, u64::MAX, None, text("max"))).await;
    while let Some(m) = raw_expect(&mut b).await {
        assert!(!matches!(m, HubMsg::Ack { .. }), "{m:?}");
    }
    // A normal reconnect: a later epoch, the watermark where the frames left it.
    let mut c = raw(&h.url).await;
    let mut hi = hello(TOKEN);
    hi.running = vec![running(&t, "r1")];
    raw_hello(&mut c, hi).await;
    let Some(HubMsg::Welcome {
        epoch: e3,
        watermarks,
    }) = raw_expect(&mut c).await
    else {
        panic!("no welcome after the gap")
    };
    assert!(e3 > e2, "{e2} then {e3}");
    assert_eq!(watermarks.get(&t), Some(&2));
    raw_send(&mut c, &RunnerMsg::Synced).await;
    h.online().await;
    raw_frame(&mut c, frame(&t, 3, None, text("ok"))).await;
    assert_eq!(count(&log(h.hub(), &t), "agent.text"), 1);
    h.shutdown().await;
}

fn running(thread: &str, run: &str) -> RunningSession {
    RunningSession {
        thread: thread.into(),
        runtime: "fake".into(),
        run: Some(run_id(thread, run)),
        ..Default::default()
    }
}

fn exited(thread: &str, rseq: u64, code: Option<i32>, run: Option<&str>) -> RunnerMsg {
    RunnerMsg::Exited {
        thread: thread.into(),
        rseq,
        code,
        stderr_tail: "gone".into(),
        run: run.map(|r| run_id(thread, r)),
        at_ms: None,
    }
}

fn turn_ended() -> EventBody {
    EventBody::TurnEnded(TurnEnded {
        result: TurnResult::Completed,
        ..Default::default()
    })
}

fn db_exec(dir: &Path, sql: &str) {
    let c = rusqlite::Connection::open(dir.join("hub/twr.db")).unwrap();
    c.busy_timeout(Duration::from_secs(5)).unwrap();
    c.execute_batch(sql).unwrap();
}

/// No command from the hub within `ms`.
async fn no_cmd(ws: &mut RawWs, ms: u64) {
    let more = tokio::time::timeout(Duration::from_millis(ms), raw_cmd(ws)).await;
    assert!(more.is_err(), "unexpected command {:?}", more.map(|c| c.1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runtime_the_runner_lost_is_a_crash_and_its_queue_waits() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "lost").await;
    h.send(&t, "queued");
    assert_eq!(h.hub().get_thread(&t).unwrap()["pending"], 1);
    drop(ws);
    h.crash();
    // The next hub finds the runner restarted: it runs nothing.
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let all = log(h.hub(), &t);
    let ks = kinds(&all);
    assert_eq!(count(&all, "thread.crashed"), 1, "{ks:?}");
    // Classified like any crash (its error item stays for the user) ...
    let classified = all.iter().any(
        |e| matches!(&e.body, EventBody::DecisionMade(d) if d.site == DecisionSite::BlockKind),
    );
    assert!(classified, "{ks:?}");
    assert_eq!(h.hub().attention_list(true).len(), 1);
    // ... and nothing is dispatched after it, not by the background ticker either (it ticks
    // at most a second apart).
    no_cmd(&mut ws, 2500).await;
    assert_eq!(count(&log(h.hub(), &t), "turn.started"), 1);
    assert_eq!(h.hub().get_thread(&t).unwrap()["pending"], 1);
    h.shutdown().await;
}

/// A duplicate frame of another box's thread is refused (and counted) like a new frame: it
/// never reaches the thread's turn-end recovery. A frame with `rseq` 0 closes the connection.
/// (That a replayed turn end never reacts to one the hub recorded itself is
/// `hub::runners::tests::a_replayed_turn_end_reacts_only_to_ends_the_runtime_reported`.)
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_frames_of_other_boxes_threads_are_refused_and_rseq_0_closes() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(None);
    cfg.boxes.insert(
        "cpu".into(),
        BoxConfig {
            token: Some("cpu-token-77e1b2c3".into()),
            ..Default::default()
        },
    );
    let h = TestHub::start(dir.path(), cfg).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "mine").await;
    let before = log(h.hub(), &t).len();
    // Another box's runner sends a frame of the thread, then the same again.
    let mut cpu = raw(&h.url).await;
    raw_hello(
        &mut cpu,
        Hello {
            box_name: "cpu".into(),
            ..hello("cpu-token-77e1b2c3")
        },
    )
    .await;
    assert!(matches!(
        raw_expect(&mut cpu).await,
        Some(HubMsg::Welcome { .. })
    ));
    raw_send(&mut cpu, &RunnerMsg::Synced).await;
    let forged = |rseq| RunnerMsg::Event {
        thread: t.clone(),
        rseq,
        turn: Some(1),
        body: turn_ended(),
        raw: None,
        run: Some("r-evil".into()),
        at_ms: None,
    };
    raw_frame(&mut cpu, forged(1)).await;
    raw_frame(&mut cpu, forged(1)).await;
    assert_eq!(
        h.hub().box_rejected_frames("cpu"),
        2,
        "the duplicate is refused too"
    );
    assert_eq!(log(h.hub(), &t).len(), before);
    // `rseq` 0 is no frame: the connection closes.
    raw_send(&mut cpu, &forged(0)).await;
    while let Some(m) = raw_expect(&mut cpu).await {
        assert!(!matches!(m, HubMsg::Ack { .. }), "{m:?}");
    }
    h.shutdown().await;
}

/// A crash on a connected box holds the queue, and the runner's reconnection (which
/// dispatches the queues that waited for the box) does not release it: only the user does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_crash_holds_the_queue_across_a_runner_reconnect() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "held").await;
    h.send(&t, "queued");
    raw_frame(&mut ws, exited(&t, 2, Some(1), Some("r1"))).await;
    assert_eq!(count(&log(h.hub(), &t), "thread.crashed"), 1);
    no_cmd(&mut ws, 2500).await;
    drop(ws);
    let hub = h.hub().clone();
    wait_for("offline", move || !hub.boxes()[0].1).await;
    let mut ws = raw_online(&h, vec![]).await;
    no_cmd(&mut ws, 2500).await;
    assert_eq!(count(&log(h.hub(), &t), "turn.started"), 1);
    assert_eq!(h.hub().get_thread(&t).unwrap()["pending"], 1);
    // The user's next prompt releases it: the runtime resumes for the queued prompt.
    h.send(&t, "again");
    let (id, op) = raw_cmd(&mut ws).await;
    assert!(matches!(&op, Op::Resume { .. }), "{op:?}");
    raw_ok(&mut ws, id).await;
    h.shutdown().await;
}

/// A prompt the user sends after a crash releases the hold, also across a hub restart that
/// comes before the box reconnects: the release is the queue row's (queued after the crash),
/// not only the old hub's memory.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_after_a_crash_releases_the_queue_across_a_hub_restart() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "released").await;
    raw_frame(&mut ws, exited(&t, 2, Some(1), Some("r1"))).await;
    assert_eq!(count(&log(h.hub(), &t), "thread.crashed"), 1);
    drop(ws);
    let hub = h.hub().clone();
    wait_for("offline", move || !hub.boxes()[0].1).await;
    // The box is offline: the user's prompt waits for it.
    h.send(&t, "after the crash");
    assert_eq!(h.hub().get_thread(&t).unwrap()["pending"], 1);
    h.crash();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let (id, op) = raw_cmd(&mut ws).await;
    assert!(matches!(&op, Op::Resume { .. }), "{op:?}");
    raw_ok(&mut ws, id).await;
    let (id, op) = raw_cmd(&mut ws).await;
    assert!(matches!(&op, Op::Send { turn: 2, .. }), "{op:?}");
    raw_ok(&mut ws, id).await;
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_is_asked_again_and_honored_after_a_hub_restart() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "halt").await;
    let r = h.sync(|hub| hub.stop_thread(&t, "test").unwrap());
    assert_eq!(r["stopping"], json!(true));
    let (stop_id, op) = raw_cmd(&mut ws).await;
    assert!(
        matches!(&op, Op::Stop { thread, .. } if thread == &t),
        "{op:?}"
    );
    assert!(stop_id.starts_with("stop:"), "{stop_id}");
    assert_eq!(outbox_rows(dir.path()), 1);
    // The hub dies before the runtime's exit.
    drop(ws);
    h.crash();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![running(&t, "r1")]).await;
    // The recorded stop goes out again, same id.
    loop {
        let (id, op) = raw_cmd(&mut ws).await;
        assert!(
            matches!(&op, Op::Stop { thread, .. } if thread == &t),
            "{op:?}"
        );
        raw_ok(&mut ws, id.clone()).await;
        if id == stop_id {
            break;
        }
    }
    // The runtime ends as killed: still the stop the user asked for.
    raw_frame(&mut ws, exited(&t, 2, Some(137), Some("r1"))).await;
    let all = log(h.hub(), &t);
    let ks = kinds(&all);
    assert_eq!(
        ks[ks.len() - 2..],
        ["turn.ended", "thread.stopped"],
        "{ks:?}"
    );
    assert_eq!(count(&all, "thread.crashed"), 0);
    assert_eq!(count(&all, "decision.made"), 0);
    assert!(h.hub().attention_list(true).is_empty());
    let d = dir.path().to_path_buf();
    wait_for("the stop forgotten", move || outbox_rows(&d) == 0).await;
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recorded_command_past_the_resend_horizon_is_not_sent_again() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "old").await;
    raw_frame(&mut ws, frame(&t, 2, Some(1), turn_ended())).await;
    h.send(&t, "second");
    let (_, op) = raw_cmd(&mut ws).await;
    assert!(matches!(&op, Op::Send { turn: 2, .. }), "{op:?}");
    assert_eq!(outbox_rows(dir.path()), 1);
    drop(ws);
    h.crash();
    // The box stays away longer than the runner remembers command results.
    db_exec(
        dir.path(),
        "UPDATE remote_outbox SET created = '2000-01-01T00:00:00.000Z'",
    );
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![running(&t, "r1")]).await;
    no_cmd(&mut ws, 700).await;
    assert_eq!(outbox_rows(dir.path()), 0);
    // The turn it was for ends with an error saying so.
    let all = log(h.hub(), &t);
    let ended = all
        .iter()
        .find_map(|e| match &e.body {
            EventBody::TurnEnded(p) if e.turn == Some(2) => Some(p.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{:?}", kinds(&all)));
    assert_eq!(ended.result, TurnResult::Error);
    let why = ended.ext.unwrap()["twr"]["error"].to_string();
    assert!(why.contains("never reached box"), "{why}");
    assert_eq!(h.hub().attention_list(true).len(), 1);
    h.shutdown().await;
}

/// The same without a hub restart. The unanswered `send` also waits in the
/// hub's memory for the box; when its record expires while the box is away, the reconnection
/// must not transmit it from there either.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_command_waiting_in_memory_is_not_sent_again() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "mem").await;
    raw_frame(&mut ws, frame(&t, 2, Some(1), turn_ended())).await;
    h.send(&t, "second");
    let (_, op) = raw_cmd(&mut ws).await;
    assert!(matches!(&op, Op::Send { turn: 2, .. }), "{op:?}");
    assert_eq!(outbox_rows(dir.path()), 1);
    // No result: the connection drops, the box stays away past the resend horizon.
    drop(ws);
    let hub = h.hub().clone();
    wait_for("offline", move || !hub.boxes()[0].1).await;
    db_exec(
        dir.path(),
        "UPDATE remote_outbox SET created = '2000-01-01T00:00:00.000Z'",
    );
    let mut ws = raw_online(&h, vec![running(&t, "r1")]).await;
    no_cmd(&mut ws, 700).await;
    assert_eq!(outbox_rows(dir.path()), 0);
    let all = log(h.hub(), &t);
    let ended = all
        .iter()
        .find_map(|e| match &e.body {
            EventBody::TurnEnded(p) if e.turn == Some(2) => Some(p.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{:?}", kinds(&all)));
    assert_eq!(ended.result, TurnResult::Error);
    // A later turn still goes out.
    h.send(&t, "third");
    let (_, op) = raw_cmd(&mut ws).await;
    assert!(matches!(&op, Op::Send { turn: 3, .. }), "{op:?}");
    h.shutdown().await;
}

/// Storage refuses every `turn.ended` (a SQLite trigger) until [`STORAGE_OK`] drops it.
const STORAGE_REFUSES_TURN_ENDS: &str = "CREATE TRIGGER refuse_turn_ended BEFORE INSERT ON events
     WHEN NEW.kind = 'turn.ended' BEGIN SELECT RAISE(ABORT, 'storage refuses turn.ended'); END;";
const STORAGE_OK: &str = "DROP TRIGGER refuse_turn_ended;";

/// The `turn.ended` events of turn `turn` of `thread`.
fn turn_ends(hub: &Hub, thread: &str, turn: u64) -> Vec<TurnEnded> {
    log(hub, thread)
        .into_iter()
        .filter_map(|e| match e.body {
            EventBody::TurnEnded(p) if e.turn == Some(turn) => Some(p),
            _ => None,
        })
        .collect()
}

/// Setup for the two tests below: a remote session whose turn 2 `send` expired while the box
/// was away, and a reconnection at which storage refuses the `turn.ended` that expiry means.
/// The expired command must stay recorded (or nothing would end the turn) and never go out.
async fn expired_command_with_storage_refusing(dir: &Path) -> (TestHub, String) {
    let h = TestHub::start(dir, config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir, "exp").await;
    raw_frame(&mut ws, frame(&t, 2, Some(1), turn_ended())).await;
    h.send(&t, "second");
    let (_, op) = raw_cmd(&mut ws).await;
    assert!(matches!(&op, Op::Send { turn: 2, .. }), "{op:?}");
    drop(ws);
    let hub = h.hub().clone();
    wait_for("offline", move || !hub.boxes()[0].1).await;
    db_exec(
        dir,
        "UPDATE remote_outbox SET created = '2000-01-01T00:00:00.000Z'",
    );
    db_exec(dir, STORAGE_REFUSES_TURN_ENDS);
    let mut ws = raw_online(&h, vec![running(&t, "r1")]).await;
    no_cmd(&mut ws, 700).await;
    assert_eq!(
        outbox_rows(dir),
        1,
        "the expired command was dropped unrecorded"
    );
    assert!(turn_ends(h.hub(), &t, 2).is_empty());
    assert_eq!(h.hub().thread_view(&t).unwrap().current_turn, Some(2));
    drop(ws);
    let hub = h.hub().clone();
    wait_for("offline", move || !hub.boxes()[0].1).await;
    (h, t)
}

/// After storage recovers, the next reconnection ends the turn `error` exactly once and
/// retires the command with it; nothing is sent, then or at a later reconnection.
async fn expired_command_retired_once(h: &TestHub, dir: &Path, t: &str) {
    for _ in 0..2 {
        let mut ws = raw_online(h, vec![running(t, "r1")]).await;
        no_cmd(&mut ws, 700).await;
        assert_eq!(outbox_rows(dir), 0);
        let ends = turn_ends(h.hub(), t, 2);
        assert_eq!(ends.len(), 1, "{ends:?}");
        assert_eq!(ends[0].result, TurnResult::Error);
        let why = ends[0].ext.as_ref().unwrap()["twr"]["error"].to_string();
        assert!(why.contains("never reached box"), "{why}");
        drop(ws);
        let hub = h.hub().clone();
        wait_for("offline", move || !hub.boxes()[0].1).await;
    }
}

/// An expired command is retired in the same transaction as the `turn.ended`
/// `error` it means. Storage refusing that event keeps the command (unsent), and the next
/// reconnection after storage recovers completes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_command_is_retired_only_with_its_turn_end() {
    let dir = tempfile::tempdir().unwrap();
    let (h, t) = expired_command_with_storage_refusing(dir.path()).await;
    db_exec(dir.path(), STORAGE_OK);
    expired_command_retired_once(&h, dir.path(), &t).await;
    h.shutdown().await;
}

/// The same, with a hub restart between the refused write and the recovery.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_command_kept_by_a_failed_write_is_retired_after_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (h, t) = expired_command_with_storage_refusing(dir.path()).await;
    h.crash();
    db_exec(dir.path(), STORAGE_OK);
    let h = TestHub::start(dir.path(), config(None)).await;
    expired_command_retired_once(&h, dir.path(), &t).await;
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_attach_runs_the_hubs_command_whatever_the_runner_answers() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(Some("me@gpu.lan"))).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "att").await;
    raw_frame(&mut ws, frame(&t, 2, Some(1), turn_ended())).await;
    let evil = twr_core::runner::AttachInfo {
        program: "rm".into(),
        args: vec!["-rf".into(), "/home".into()],
        cwd: Some("/etc".into()),
        env: vec![("LD_PRELOAD".into(), Some("/tmp/evil.so".into()))],
    };
    let hub = h.hub().clone();
    let tt = t.clone();
    let attach =
        h.rt.as_ref()
            .unwrap()
            .spawn(async move { hub.attach_begin(&tt, false, "cli").await });
    let (cmd_id, op) = raw_cmd(&mut ws).await;
    assert!(matches!(op, Op::AttachInfo { .. }), "{op:?}");
    let v = serde_json::to_value(Some(evil)).unwrap();
    raw_send(
        &mut ws,
        &RunnerMsg::CmdResult(CmdResult::new(cmd_id, Ok(v))),
    )
    .await;
    let (id, op) = raw_cmd(&mut ws).await;
    assert!(matches!(&op, Op::Stop { .. }), "{op:?}");
    raw_ok(&mut ws, id).await;
    raw_frame(&mut ws, exited(&t, 3, Some(0), Some("r1"))).await;
    let grant = attach.await.unwrap().unwrap();
    assert_eq!(grant.argv[..grant.argv.len() - 1], attach_ssh("me@gpu.lan"));
    let remote = &grant.argv[grant.argv.len() - 1];
    let dir_on_box = dir.path().join("rawproj");
    assert!(
        remote.starts_with(&format!("cd '{}' && exec ", dir_on_box.display())),
        "{remote}"
    );
    assert!(remote.contains("'twr-fake-attach' 's-att'"), "{remote}");
    for bad in ["'rm'", "-rf", "/home", "/etc", "LD_PRELOAD", "evil"] {
        assert!(!remote.contains(bad), "{bad} in {remote}");
    }
    h.shutdown().await;
}

/// On a box, a thread made for configured project `rawproj` runs with `[threads] env` and
/// that project's `env`; one made from another repository whose folder has its name, with
/// `[threads] env` alone. `twr attach` runs the interactive command over ssh with the same
/// variables (one whose name starts with `-` too), and none of twr's own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_threads_run_and_attach_with_the_configured_environment() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = config(Some("me@gpu.lan"));
    cfg.threads.env = [
        ("CARGO_INCREMENTAL".to_string(), "0".to_string()),
        ("-ODD".to_string(), "x".to_string()),
    ]
    .into();
    cfg.projects.insert(
        "rawproj".into(),
        twr_core::config::ProjectConfig {
            repo: "/srv/rawproj".into(),
            env: [("CARGO_TARGET_DIR".to_string(), "/cache/rawproj".to_string())].into(),
            ..Default::default()
        },
    );
    let h = TestHub::start(dir.path(), cfg).await;
    let mut ws = raw_online(&h, vec![]).await;
    let hub = h.hub().clone();
    let req = NewThread {
        project: "rawproj".into(),
        runtime: Some("fake".into()),
        name: Some("cfg".into()),
        box_name: Some(BOX.into()),
        ..Default::default()
    };
    let created =
        h.rt.as_ref()
            .unwrap()
            .spawn(async move { hub.new_thread(req, "test").await });
    let (cmd_id, op) = raw_cmd(&mut ws).await;
    assert!(matches!(op, Op::RepoRoot { .. }), "{op:?}");
    raw_ok(&mut ws, cmd_id).await;
    let configured = created.await.unwrap().unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let named = h.new_thread_raw_named(&mut ws, dir.path(), "path").await;
    assert_eq!(
        h.hub().thread_view(&named).unwrap().project.as_deref(),
        Some("rawproj")
    );
    let mut envs = Vec::new();
    for t in [&configured, &named] {
        h.send(t, "hi");
        let (cmd_id, op) = raw_cmd(&mut ws).await;
        let Op::Spawn { cfg, .. } = op else {
            panic!("{op:?}")
        };
        envs.push(cfg.env);
        raw_ok(&mut ws, cmd_id).await;
        raw_frame(&mut ws, frame(t, 1, None, started(&format!("s-{t}")))).await;
        raw_frame(&mut ws, frame(t, 2, Some(1), turn_ended())).await;
    }
    let get = |env: &[(String, String)], k: &str| {
        env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
    };
    assert_eq!(
        get(&envs[0], "CARGO_TARGET_DIR").as_deref(),
        Some("/cache/rawproj")
    );
    assert_eq!(get(&envs[1], "CARGO_TARGET_DIR"), None);
    for env in &envs {
        assert_eq!(
            get(env, "CARGO_INCREMENTAL").as_deref(),
            Some("0"),
            "{env:?}"
        );
    }

    let hub = h.hub().clone();
    let t = configured.clone();
    let attach =
        h.rt.as_ref()
            .unwrap()
            .spawn(async move { hub.attach_begin(&t, false, "cli").await });
    let (cmd_id, op) = raw_cmd(&mut ws).await;
    assert!(matches!(op, Op::AttachInfo { .. }), "{op:?}");
    let info = twr_core::runner::AttachInfo {
        program: "sh".into(),
        args: Vec::new(),
        cwd: None,
        env: Vec::new(),
    };
    raw_send(
        &mut ws,
        &RunnerMsg::CmdResult(CmdResult::new(
            cmd_id,
            Ok(serde_json::to_value(Some(info)).unwrap()),
        )),
    )
    .await;
    let (id, op) = raw_cmd(&mut ws).await;
    assert!(matches!(&op, Op::Stop { .. }), "{op:?}");
    raw_ok(&mut ws, id).await;
    raw_frame(&mut ws, exited(&configured, 3, Some(0), Some("r1"))).await;
    let grant = attach.await.unwrap().unwrap();
    assert_eq!(grant.argv[..grant.argv.len() - 1], attach_ssh("me@gpu.lan"));
    let remote = &grant.argv[grant.argv.len() - 1];
    for set in ["'CARGO_INCREMENTAL=0'", "'CARGO_TARGET_DIR=/cache/rawproj'"] {
        assert!(remote.contains(set), "{set} not in {remote}");
    }
    // After `--`: a name that starts with `-` is no option of `env`.
    let assignments = remote.split(" -- ").nth(1).unwrap_or_default();
    assert!(assignments.contains("'-ODD=x'"), "{remote}");
    assert!(
        !remote.contains("TWR_THREAD") && !remote.contains("TWR_AGENT="),
        "{remote}"
    );
    h.shutdown().await;
}

/// The fake runtime, with an attach fitted to this box (as Claude Code's is, to its probed
/// `--help`): the hub's own box says so in its local attach command.
struct FittedLocally(FakeAdapter);

impl Adapter for FittedLocally {
    fn info(&self) -> twr_core::adapter::AdapterInfo {
        self.0.info()
    }

    fn spawn(
        &self,
        cfg: twr_core::adapter::SpawnConfig,
        prompt: Option<String>,
    ) -> anyhow::Result<twr_core::adapter::AdapterHandle> {
        self.0.spawn(cfg, prompt)
    }

    fn attach_command(
        &self,
        cfg: &twr_core::adapter::SpawnConfig,
        session_id: &str,
    ) -> Option<std::process::Command> {
        let mut cmd = self.0.attach_command(cfg, session_id)?;
        cmd.arg("--fitted-to-this-box");
        Some(cmd)
    }

    fn remote_attach_command(
        &self,
        cfg: &twr_core::adapter::SpawnConfig,
        session_id: &str,
    ) -> Option<std::process::Command> {
        self.0.attach_command(cfg, session_id)
    }
}

/// The command of a remote attach is the hub's adapter's for another box: nothing it fitted
/// to the hub's own binaries (a probed Claude Code that lacks `--effort`) goes over ssh.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_attach_is_never_fitted_to_the_hubs_own_binaries() {
    let dir = tempfile::tempdir().unwrap();
    let mut opts = hub_opts(dir.path(), config(Some("me@gpu.lan")));
    opts.adapters = Arc::new(|name: &str| -> Option<Arc<dyn Adapter>> {
        match name {
            "fake" => Some(Arc::new(FittedLocally(FakeAdapter::new()))),
            other => twr_adapters::find(other),
        }
    });
    let h = TestHub::start_opts(opts).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "fit").await;
    raw_frame(&mut ws, frame(&t, 2, Some(1), turn_ended())).await;
    let hub = h.hub().clone();
    let tt = t.clone();
    let attach =
        h.rt.as_ref()
            .unwrap()
            .spawn(async move { hub.attach_begin(&tt, false, "cli").await });
    let (cmd_id, op) = raw_cmd(&mut ws).await;
    assert!(matches!(op, Op::AttachInfo { .. }), "{op:?}");
    let answer = twr_core::runner::AttachInfo {
        program: "sh".into(),
        ..Default::default()
    };
    let v = serde_json::to_value(Some(answer)).unwrap();
    raw_send(
        &mut ws,
        &RunnerMsg::CmdResult(CmdResult::new(cmd_id, Ok(v))),
    )
    .await;
    let (id, op) = raw_cmd(&mut ws).await;
    assert!(matches!(&op, Op::Stop { .. }), "{op:?}");
    raw_ok(&mut ws, id).await;
    raw_frame(&mut ws, exited(&t, 3, Some(0), Some("r1"))).await;
    let grant = attach.await.unwrap().unwrap();
    let remote = &grant.argv[grant.argv.len() - 1];
    assert!(remote.contains("'twr-fake-attach' 's-fit'"), "{remote}");
    assert!(!remote.contains("--fitted-to-this-box"), "{remote}");
    h.shutdown().await;
}

/// The fake runtime with an attach command that applies none of the thread's settings
/// (`Adapter::attach_applies` left at its default), as a generic ACP agent's without `{model}`
/// / `{effort}` placeholders.
struct AppliesNothing(FakeAdapter);

impl Adapter for AppliesNothing {
    fn info(&self) -> twr_core::adapter::AdapterInfo {
        self.0.info()
    }

    fn spawn(
        &self,
        cfg: twr_core::adapter::SpawnConfig,
        prompt: Option<String>,
    ) -> anyhow::Result<twr_core::adapter::AdapterHandle> {
        self.0.spawn(cfg, prompt)
    }

    fn attach_command(
        &self,
        cfg: &twr_core::adapter::SpawnConfig,
        session_id: &str,
    ) -> Option<std::process::Command> {
        self.0.attach_command(cfg, session_id)
    }
}

/// A remote attach follows the rule of a local one: a session that has not run the thread's
/// new model yet is not attached through a command that cannot apply it (as the hub's own
/// adapter, which makes the command, says), refused before the runner is asked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_attach_refuses_settings_its_command_cannot_apply() {
    let dir = tempfile::tempdir().unwrap();
    let mut opts = hub_opts(dir.path(), config(Some("me@gpu.lan")));
    opts.adapters = Arc::new(|name: &str| -> Option<Arc<dyn Adapter>> {
        match name {
            "fake" => Some(Arc::new(AppliesNothing(FakeAdapter::new()))),
            other => twr_adapters::find(other),
        }
    });
    let h = TestHub::start_opts(opts).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "stale").await;
    raw_frame(&mut ws, frame(&t, 2, Some(1), turn_ended())).await;
    raw_frame(&mut ws, exited(&t, 3, Some(0), Some("r1"))).await;
    let r = h
        .hub()
        .set_model(&t, Some("fake-big"), None, &Caller::user("test"))
        .unwrap();
    assert_eq!(r["applies"], "next_start", "{r}");
    let e = h.hub().attach_begin(&t, false, "cli").await.unwrap_err();
    assert_eq!(e.code, codes::CONFLICT, "{e:?}");
    assert!(
        e.message.contains("runs model the runtime's default")
            && e.message.contains(r#"model "fake-big""#),
        "{}",
        e.message
    );
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_attach_refuses_a_thread_whose_session_changed_meanwhile() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(Some("me@gpu.lan"))).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "moving").await;
    raw_frame(&mut ws, frame(&t, 2, Some(1), turn_ended())).await;
    let hub = h.hub().clone();
    let tt = t.clone();
    let attach =
        h.rt.as_ref()
            .unwrap()
            .spawn(async move { hub.attach_begin(&tt, false, "cli").await });
    let (cmd_id, op) = raw_cmd(&mut ws).await;
    assert!(matches!(op, Op::AttachInfo { .. }), "{op:?}");
    // While the runner is asked, the runtime starts another session.
    raw_frame(&mut ws, frame(&t, 3, None, started("s-other"))).await;
    let info = twr_core::runner::AttachInfo {
        program: "sh".into(),
        args: vec![],
        cwd: None,
        env: vec![],
    };
    let v = serde_json::to_value(Some(info)).unwrap();
    raw_send(
        &mut ws,
        &RunnerMsg::CmdResult(CmdResult::new(cmd_id, Ok(v))),
    )
    .await;
    let e = attach.await.unwrap().unwrap_err();
    assert_eq!(e.code, codes::CONFLICT, "{e:?}");
    // Refused before anything was stopped.
    no_cmd(&mut ws, 500).await;
    assert!(h.hub().thread_view(&t).unwrap().attached.is_none());
    h.shutdown().await;
}

/// A model or effort change while the runner is asked for a remote attach's command refuses
/// the attach (`conflict`, try again): the command was built with the old settings.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_attach_refuses_a_thread_whose_model_changed_meanwhile() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(Some("me@gpu.lan"))).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "remodel").await;
    raw_frame(&mut ws, frame(&t, 2, Some(1), turn_ended())).await;
    let hub = h.hub().clone();
    let tt = t.clone();
    let attach =
        h.rt.as_ref()
            .unwrap()
            .spawn(async move { hub.attach_begin(&tt, false, "cli").await });
    let (cmd_id, op) = raw_cmd(&mut ws).await;
    assert!(matches!(op, Op::AttachInfo { .. }), "{op:?}");
    // While the runner is asked, the thread's model and effort change.
    let r = h
        .hub()
        .set_model(&t, Some("fake-big"), Some("deep"), &Caller::user("test"))
        .unwrap();
    assert_eq!(r["changed"], true, "{r}");
    let info = twr_core::runner::AttachInfo {
        program: "sh".into(),
        args: vec![],
        cwd: None,
        env: vec![],
    };
    let v = serde_json::to_value(Some(info)).unwrap();
    raw_send(
        &mut ws,
        &RunnerMsg::CmdResult(CmdResult::new(cmd_id, Ok(v))),
    )
    .await;
    let e = attach.await.unwrap().unwrap_err();
    assert_eq!(e.code, codes::CONFLICT, "{e:?}");
    assert!(e.message.contains("try again"), "{e:?}");
    assert!(h.hub().thread_view(&t).unwrap().attached.is_none());
    h.shutdown().await;
}

/// A settings change accepted while a remote attach waits for the runtime's exit (its command
/// was made with the old settings, before the stop) refuses that attach when it would be
/// granted (`conflict`, try again): the attach recorded on the exit is ended again. `change`
/// makes the change.
async fn a_change_while_the_remote_runtime_stops_refuses_the_attach(
    name: &str,
    change: impl FnOnce(&Hub, &str),
) {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(Some("me@gpu.lan"))).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), name).await;
    raw_frame(&mut ws, frame(&t, 2, Some(1), turn_ended())).await;
    let hub = h.hub().clone();
    let tt = t.clone();
    let attach =
        h.rt.as_ref()
            .unwrap()
            .spawn(async move { hub.attach_begin(&tt, false, "cli").await });
    let (cmd_id, op) = raw_cmd(&mut ws).await;
    assert!(matches!(op, Op::AttachInfo { .. }), "{op:?}");
    let info = twr_core::runner::AttachInfo {
        program: "sh".into(),
        args: vec![],
        cwd: None,
        env: vec![],
    };
    let v = serde_json::to_value(Some(info)).unwrap();
    raw_send(
        &mut ws,
        &RunnerMsg::CmdResult(CmdResult::new(cmd_id, Ok(v))),
    )
    .await;
    // The lease is taken and the runtime is being stopped on the box: the change is accepted.
    let (id, op) = raw_cmd(&mut ws).await;
    assert!(matches!(&op, Op::Stop { .. }), "{op:?}");
    raw_ok(&mut ws, id).await;
    change(h.hub(), &t);
    raw_frame(&mut ws, exited(&t, 3, Some(0), Some("r1"))).await;
    let e = attach.await.unwrap().unwrap_err();
    assert_eq!(e.code, codes::CONFLICT, "{e:?}");
    assert!(
        e.message
            .contains(&format!("{name} changed while attaching; try again")),
        "{e:?}"
    );
    assert!(h.hub().thread_view(&t).unwrap().attached.is_none());
    let (_, evs) = h.hub().thread_events(&t, 0, 1000).unwrap();
    let i = evs
        .iter()
        .rposition(|e| e.kind() == "thread.attached")
        .unwrap();
    let EventBody::ThreadDetached(p) = &evs[i + 1].body else {
        panic!("{:?}", evs[i + 1])
    };
    assert_eq!(
        p.ext.as_ref().unwrap()["twr"]["reason"],
        json!("changed_while_attaching")
    );
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_model_change_while_the_remote_runtime_stops_refuses_the_attach() {
    a_change_while_the_remote_runtime_stops_refuses_the_attach("remodel", |hub, t| {
        let r = hub
            .set_model(t, Some("fake-big"), Some("deep"), &Caller::user("test"))
            .unwrap();
        assert_eq!(r["applies"], "after_turn", "{r}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mode_change_while_the_remote_runtime_stops_refuses_the_attach() {
    a_change_while_the_remote_runtime_stops_refuses_the_attach("remode", |hub, t| {
        let r = hub.set_mode(t, "plan", &Caller::user("test")).unwrap();
        assert_eq!(r["applies"], "after_turn", "{r}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connections_that_have_not_authenticated_are_limited_in_number_and_size() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    // A message over the pre-authentication limit closes the connection unanswered.
    let mut ws = raw(&h.url).await;
    let big = format!(
        "{{\"type\":\"hello\",\"pad\":\"{}\"}}",
        "x".repeat(100 * 1024)
    );
    let _ = ws.send(Message::Text(big.into())).await;
    assert!(raw_next(&mut ws).await.is_none());
    // Connections that never finish their handshake hold the unauthenticated slots ...
    let addr = h.url.trim_start_matches("ws://").to_string();
    let mut idle = Vec::new();
    for _ in 0..twr_hub::listener::PRE_AUTH_CONNECTIONS {
        idle.push(tokio::net::TcpStream::connect(&addr).await.unwrap());
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    // ... so one more is closed at once.
    let r = tokio::time::timeout(
        Duration::from_secs(5),
        tokio_tungstenite::connect_async(h.url.as_str()),
    )
    .await
    .expect("the extra connection was left waiting");
    assert!(
        r.is_err(),
        "an extra unauthenticated connection was accepted"
    );
    // Once they are gone, a runner gets in (and, authenticated, may send large messages).
    drop(idle);
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "big").await;
    let long = EventBody::AgentText(AgentText {
        text: "y".repeat(200 * 1024),
        is_final: true,
        ..Default::default()
    });
    raw_frame(&mut ws, frame(&t, 2, Some(1), long)).await;
    h.shutdown().await;
}

/// A WebSocket client that never reads: it upgrades by hand (small receive buffer), then
/// sends pings as fast as the hub takes them, so the hub's pongs fill both sides' buffers and
/// every write to it blocks. Runs until the hub stops reading.
async fn ping_flooder(addr: String) -> tokio::task::JoinHandle<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let sock = tokio::net::TcpSocket::new_v4().unwrap();
    sock.set_recv_buffer_size(4096).unwrap();
    let mut tcp = sock.connect(addr.parse().unwrap()).await.unwrap();
    tcp.write_all(
        format!(
            "GET / HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut b = [0u8; 1];
        assert_eq!(
            tcp.read(&mut b).await.unwrap(),
            1,
            "the upgrade was refused"
        );
        head.push(b[0]);
    }
    assert!(head.starts_with(b"HTTP/1.1 101"), "{head:?}");
    // A masked ping with a 125-byte payload (mask 0: the payload goes as is), many per write.
    let mut ping = vec![0x89, 0x80 | 125, 0, 0, 0, 0];
    ping.extend([b'p'; 125]);
    let burst = ping.repeat(64);
    tokio::spawn(async move {
        while tcp.write_all(&burst).await.is_ok() {}
        // Keep the connection open (unread) until the test ends.
        std::future::pending::<()>().await;
    })
}

/// Unauthenticated peers that flood pings and never read time out without
/// keeping their pre-authentication slots: the hub's close to them is bounded and the slot
/// is released first, so a real runner gets in afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peers_that_never_read_do_not_keep_the_unauthenticated_slots() {
    let dir = tempfile::tempdir().unwrap();
    let mut opts = hub_opts(dir.path(), config(None));
    // Long enough that every flooder is still within it when the extra connection tries,
    // however busy the machine.
    opts.hello_timeout = Duration::from_secs(8);
    let h = TestHub::start_opts(opts).await;
    let addr = h
        .url
        .trim_start_matches("ws://")
        .trim_end_matches('/')
        .to_string();
    let floods = futures::future::join_all(
        (0..twr_hub::listener::PRE_AUTH_CONNECTIONS).map(|_| ping_flooder(addr.clone())),
    )
    .await;
    // Every slot is taken while they flood ...
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        tokio_tungstenite::connect_async(h.url.as_str())
            .await
            .is_err(),
        "an extra unauthenticated connection was accepted"
    );
    // ... and free once their hello time is up, although they still read nothing. When
    // exactly depends on how busy the machine is: tried until then (a slot still taken
    // closes the connection before its upgrade).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    let ws = loop {
        match tokio_tungstenite::connect_async(h.url.as_str()).await {
            Ok((ws, _)) => break ws,
            Err(e) => {
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "the unauthenticated slots were never freed: {e}"
                );
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    };
    let _ws = raw_online_on(&h, ws, vec![]).await;
    for f in floods {
        f.abort();
    }
    h.shutdown().await;
}

/// A box's trash listing speaks for that box only: an item whose manifest names another box
/// is left out, an item is listed under the box that listed it, and restoring an item brings
/// back only a thread of that box (here a runner lists an item that names a deleted local
/// thread: refused, nothing restored).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runner_lists_and_restores_only_its_own_boxes_trash() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let local = h.new_local_thread(&project(dir.path()), "home").await;
    let user = Caller::user("test");
    let card = {
        let user = user.clone();
        h.run(move |hub| async move { hub.delete_threads(&["home".to_string()], &user).await })
            .await
            .unwrap()
    };
    let (attn, rev) = (
        card["attn_id"].as_str().unwrap().to_string(),
        card["revision"].as_u64(),
    );
    h.sync(|hub| hub.respond_at(&attn, Answer::new("Delete"), rev, &user))
        .unwrap();
    let (hub, home) = (h.hub().clone(), local.clone());
    wait_for("home deleted", move || hub.thread_view(&local).is_err()).await;
    // The gpu runner lists two forged items: one naming the local box, one naming the
    // deleted local thread under its own box.
    let mut ws = raw_online(&h, vec![]).await;
    let item = |id: &str, box_name: &str| twr_core::git::TrashItem {
        dir: format!("/nonexistent/trash/{id}").into(),
        manifest: twr_core::git::TrashManifest {
            item: id.into(),
            info: twr_core::git::TrashInfo {
                thread: home.clone(),
                thread_name: "home".into(),
                box_name: box_name.into(),
                runtime: "fake".into(),
                deleted_at_ms: 1,
                ..Default::default()
            },
            complete: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let forged = serde_json::to_value(vec![
        item("01FORGEDLOCAL", "local"),
        item("01FORGEDGPU", BOX),
    ])
    .unwrap();
    let answering = tokio::spawn(async move {
        while let Some(m) = raw_next(&mut ws).await {
            if let HubMsg::Cmd { cmd_id, op } = m {
                let r = match op {
                    Op::TrashList {} => Ok(forged.clone()),
                    _ => Ok(Value::Null),
                };
                raw_send(&mut ws, &RunnerMsg::CmdResult(CmdResult::new(cmd_id, r))).await;
            }
        }
    });
    let list = h
        .run(|hub| async move { hub.trash_list().await })
        .await
        .unwrap();
    let items = list["items"].as_array().unwrap();
    assert!(
        !items.iter().any(|i| i["item"] == "01FORGEDLOCAL"),
        "an item naming another box was listed: {list}"
    );
    let gpu = items
        .iter()
        .find(|i| i["item"] == "01FORGEDGPU")
        .unwrap_or_else(|| panic!("{list}"));
    assert_eq!(gpu["box"], json!(BOX));
    let e = {
        let user = user.clone();
        h.run(move |hub| async move { hub.trash_restore("01FORGEDGPU", None, &user).await })
            .await
            .unwrap_err()
    };
    assert_eq!(e.code, codes::CONFLICT, "{}", e.message);
    assert!(
        e.message.contains("cannot bring that thread back"),
        "{}",
        e.message
    );
    assert!(
        h.hub().thread_view("home").is_err(),
        "the local thread came back"
    );
    answering.abort();
    h.shutdown().await;
}

/// A peer that floods WebSocket pings and reads nothing holds a bounded backlog of pongs in
/// the hub (tungstenite buffers without bound by default: one pong per ping): once it reads
/// again, it gets far fewer pong bytes than its pings, then the hub's answer to its hello.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_ping_flood_that_reads_nothing_holds_a_bounded_backlog() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let dir = tempfile::tempdir().unwrap();
    let mut opts = hub_opts(dir.path(), config(None));
    opts.hello_timeout = Duration::from_secs(120);
    let h = TestHub::start_opts(opts).await;
    let addr = h.url.trim_start_matches("ws://").to_string();
    let sock = tokio::net::TcpSocket::new_v4().unwrap();
    sock.set_recv_buffer_size(4096).unwrap();
    let mut tcp = sock.connect(addr.parse().unwrap()).await.unwrap();
    tcp.write_all(
        format!(
            "GET / HTTP/1.1\r\nHost: {addr}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut b = [0u8; 1];
        assert_eq!(tcp.read(&mut b).await.unwrap(), 1);
        head.push(b[0]);
    }
    assert!(head.starts_with(b"HTTP/1.1 101"), "{head:?}");
    // 32 MiB of masked pings with 125-byte payloads (mask 0), then a hello (a masked text
    // frame with a 16-bit length).
    let mut ping = vec![0x89, 0x80 | 125, 0, 0, 0, 0];
    ping.extend([b'p'; 125]);
    let burst = ping.repeat(1024);
    let pings = 256;
    let (mut rd, mut wr) = tcp.into_split();
    let writer = tokio::spawn(async move {
        for _ in 0..pings {
            wr.write_all(&burst).await.unwrap();
        }
        let hello = serde_json::to_vec(&RunnerMsg::Hello(twr_core::runner::Hello {
            box_name: BOX.into(),
            protocol: PROTOCOL_VERSION,
            nonce: new_nonce().unwrap(),
        }))
        .unwrap();
        let mut frame = vec![0x81, 0x80 | 126];
        frame.extend((hello.len() as u16).to_be_bytes());
        frame.extend([0, 0, 0, 0]);
        frame.extend(hello);
        wr.write_all(&frame).await.unwrap();
        wr
    });
    let sent_pong_bytes = (pings * 1024 * 125) as u64;
    // Let the hub read everything while nothing is read here.
    let _wr = tokio::time::timeout(Duration::from_secs(120), writer)
        .await
        .expect("the hub stopped reading")
        .unwrap();
    // Now read: pongs, then the challenge.
    let mut pong_bytes = 0u64;
    let read = async {
        loop {
            let mut h2 = [0u8; 2];
            rd.read_exact(&mut h2).await.unwrap();
            let mut len = u64::from(h2[1] & 0x7f);
            if len == 126 {
                let mut l = [0u8; 2];
                rd.read_exact(&mut l).await.unwrap();
                len = u64::from(u16::from_be_bytes(l));
            } else if len == 127 {
                let mut l = [0u8; 8];
                rd.read_exact(&mut l).await.unwrap();
                len = u64::from_be_bytes(l);
            }
            let mut payload = vec![0u8; len as usize];
            rd.read_exact(&mut payload).await.unwrap();
            match h2[0] & 0x0f {
                0xA => pong_bytes += len,
                0x1 => return String::from_utf8(payload).unwrap(),
                op => panic!("unexpected opcode {op}"),
            }
        }
    };
    let text = tokio::time::timeout(Duration::from_secs(60), read)
        .await
        .expect("no challenge");
    assert!(text.contains("\"challenge\""), "{text}");
    assert!(
        pong_bytes < sent_pong_bytes / 3,
        "{pong_bytes} pong bytes for {sent_pong_bytes} of pings: the backlog is not bounded"
    );
    h.shutdown().await;
}

/// WebSocket pings do not keep an authenticated connection open: one that sends nothing but
/// them is closed once its idle time is up (the runner's own `ping` messages do count).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn websocket_pings_do_not_keep_a_silent_runner_connected() {
    let dir = tempfile::tempdir().unwrap();
    let mut opts = hub_opts(dir.path(), config(None));
    opts.runner_idle_timeout = Duration::from_secs(2);
    let h = TestHub::start_opts(opts).await;
    let mut ws = raw_online(&h, vec![]).await;
    // Protocol pings keep it: 3 s of them.
    let until = tokio::time::Instant::now() + Duration::from_secs(3);
    while tokio::time::Instant::now() < until {
        raw_send(&mut ws, &RunnerMsg::Ping).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    assert!(h.hub().boxes()[0].1, "closed although it sent pings");
    // WebSocket pings alone do not.
    let mut tick = tokio::time::interval(Duration::from_millis(200));
    let closed = tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    if ws.send(Message::Ping(Vec::new().into())).await.is_err() {
                        return;
                    }
                }
                m = ws.next() => match m {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                    Some(Ok(_)) => {}
                },
            }
        }
    })
    .await;
    assert!(
        closed.is_ok(),
        "WebSocket pings kept a silent connection open"
    );
    h.shutdown().await;
}

/// One deadline from the accept covers a connection's handshakes and its hello: a peer that
/// takes most of it for the WebSocket handshake does not get the hello's time again after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_deadline_covers_the_handshakes_and_the_hello() {
    let dir = tempfile::tempdir().unwrap();
    let mut opts = hub_opts(dir.path(), config(None));
    opts.hello_timeout = Duration::from_secs(3);
    let h = TestHub::start_opts(opts).await;
    let addr = h.url.trim_start_matches("ws://").to_string();
    let tcp = tokio::net::TcpStream::connect(&addr).await.unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let (mut ws, _) = tokio_tungstenite::client_async(h.url.as_str(), tcp)
        .await
        .expect("the WebSocket handshake within the deadline");
    tokio::time::sleep(Duration::from_secs(2)).await;
    let _ = ws
        .send(Message::Text(
            serde_json::to_string(&RunnerMsg::Hello(twr_core::runner::Hello {
                box_name: BOX.into(),
                protocol: PROTOCOL_VERSION,
                nonce: new_nonce().unwrap(),
            }))
            .unwrap()
            .into(),
        ))
        .await;
    let next = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("the hub neither answered nor closed");
    assert!(
        !matches!(&next, Some(Ok(Message::Text(_)))),
        "a hello past the deadline was answered: {next:?}"
    );
    h.shutdown().await;
}

/// On a remote box, a runtime the hub gave up on keeps its thread until the
/// runner reports its exit. Its stop (a session command) goes out again when the box is back,
/// and nothing is resumed next to the runtime meanwhile.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_runtime_the_hub_gave_up_on_is_stopped_again_and_keeps_its_thread() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "gaveup").await;
    // The turn's reactions can never be recorded: the hub gives the runtime up.
    db_exec(
        dir.path(),
        "CREATE TRIGGER inject BEFORE INSERT ON events WHEN NEW.kind = 'report.ready'
         BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;",
    );
    raw_frame(&mut ws, frame(&t, 2, Some(1), turn_ended())).await;
    let (_, op) = raw_cmd(&mut ws).await;
    assert!(
        matches!(&op, Op::Stop { thread, .. } if thread == &t),
        "{op:?}"
    );
    db_exec(dir.path(), "DROP TRIGGER inject");
    // The connection drops before the stop is answered; a prompt waits.
    drop(ws);
    let hub = h.hub().clone();
    wait_for("offline", move || !hub.boxes()[0].1).await;
    h.send(&t, "next");
    // The runner comes back, still running the session: the stop goes out again, and
    // nothing starts next to it.
    let mut ws = raw_online(&h, vec![running(&t, "r1")]).await;
    let (_, op) = raw_cmd(&mut ws).await;
    assert!(
        matches!(&op, Op::Stop { thread, .. } if thread == &t),
        "{op:?}"
    );
    no_cmd(&mut ws, 500).await;
    let all = log(h.hub(), &t);
    assert_eq!(count(&all, "thread.crashed"), 0, "{:?}", kinds(&all));
    // Its exit records the crash.
    raw_frame(&mut ws, exited(&t, 3, Some(0), Some("r1"))).await;
    let all = log(h.hub(), &t);
    let crash = all
        .iter()
        .find_map(|e| match &e.body {
            EventBody::ThreadCrashed(c) => Some(c.stderr_tail.clone()),
            _ => None,
        })
        .expect("no crash recorded");
    assert!(crash.contains("could not record"), "{crash}");
    h.shutdown().await;
}

/// A turn's end whose batch cannot be synced (the connection closes, the runner
/// sends the frame again, a duplicate then) still gets the hub's reactions right away: its
/// report, here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_turn_end_whose_sync_fails_still_gets_its_reactions() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "nosync").await;
    // Syncing the log fails (the hub cannot open its write-ahead log again); writes through
    // the open connection still work.
    let wal = dir.path().join("hub/twr.db-wal");
    assert!(wal.exists());
    std::fs::set_permissions(&wal, std::fs::Permissions::from_mode(0o000)).unwrap();
    raw_send(&mut ws, &frame(&t, 2, Some(1), turn_ended())).await;
    let closed = tokio::time::timeout(WAIT, async {
        loop {
            match raw_expect(&mut ws).await {
                Some(HubMsg::Ack { rseq, .. }) if rseq >= 2 => return false,
                Some(_) => continue,
                None => return true,
            }
        }
    })
    .await
    .expect("the connection neither acked nor closed");
    std::fs::set_permissions(&wal, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(closed, "the sync did not fail");
    let hub = h.hub().clone();
    let t2 = t.clone();
    wait_for("the turn's report", move || reports(&hub, &t2, 1) == 1).await;
    h.shutdown().await;
}

/// A duplicate `turn.ended` (a replay of a frame the hub recorded) whose
/// reactions are missing (here they could not be recorded, so the hub gave the runtime up)
/// gets them when it comes again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_replayed_turn_end_gets_the_reactions_it_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "replayed").await;
    db_exec(
        dir.path(),
        "CREATE TRIGGER inject BEFORE INSERT ON events WHEN NEW.kind = 'report.ready'
         BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;",
    );
    raw_frame(&mut ws, frame(&t, 2, Some(1), turn_ended())).await;
    let (_, op) = raw_cmd(&mut ws).await;
    assert!(
        matches!(&op, Op::Stop { thread, .. } if thread == &t),
        "{op:?}"
    );
    db_exec(dir.path(), "DROP TRIGGER inject");
    assert_eq!(reports(h.hub(), &t, 1), 0);
    drop(ws);
    let hub = h.hub().clone();
    wait_for("offline", move || !hub.boxes()[0].1).await;
    // The runner comes back and replays the turn's end: a duplicate, acked, and the turn
    // gets its report.
    let mut ws = raw_online(&h, vec![running(&t, "r1")]).await;
    raw_frame(&mut ws, frame(&t, 2, Some(1), turn_ended())).await;
    assert_eq!(reports(h.hub(), &t, 1), 1);
    // Once is enough: another replay adds nothing.
    raw_frame(&mut ws, frame(&t, 2, Some(1), turn_ended())).await;
    assert_eq!(reports(h.hub(), &t, 1), 1);
    assert_eq!(count(&log(h.hub(), &t), "turn.ended"), 1);
    h.shutdown().await;
}

/// A remote runtime's own turn that raced a prompt the hub had already started as turn 2: the
/// runtime began and finished it before the prompt reached it (its `send` not even answered).
/// Its frames never take turn 2's number nor close it, whether they come after a hub restart or
/// again in a replay: its start is dropped, its output goes to turn 2, and its end, which names
/// it (`ext.twr.runtime_turn`), closes nothing and adds its usage to turn 2's. The runtime's next
/// turn of its own is turn 3, which only its own end closes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_runtime_turn_that_raced_a_prompt_never_takes_its_number() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "raced").await;
    raw_frame(&mut ws, frame(&t, 2, Some(1), turn_ended())).await;
    h.send(&t, "next");
    let (_, op) = raw_cmd(&mut ws).await;
    assert!(matches!(&op, Op::Send { turn: 2, .. }), "{op:?}");
    // The hub dies before the runtime's own turn reaches it; the runner sends it to the next.
    drop(ws);
    h.crash();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![running(&t, "r1")]).await;
    let tagged = |id: &str| {
        let mut ext = None;
        set_twr_ext(&mut ext, RUNTIME_TURN_KEY, id.into());
        ext
    };
    let runtime_turn = |id: &str| {
        EventBody::TurnStarted(TurnStarted {
            from: TurnFrom::Runtime,
            ext: tagged(id),
            ..Default::default()
        })
    };
    let end = |id: Option<&str>, input, output, cost| {
        EventBody::TurnEnded(TurnEnded {
            usage: Some(Usage {
                input,
                output,
                cost: Some(cost),
            }),
            ext: id.and_then(tagged),
            ..Default::default()
        })
    };
    let raced = [
        frame(&t, 3, None, runtime_turn("r1")),
        frame(
            &t,
            4,
            None,
            EventBody::AgentText(AgentText {
                text: "review finished".into(),
                is_final: true,
                ..Default::default()
            }),
        ),
        frame(&t, 5, None, end(Some("r1"), 10, 5, 0.25)),
    ];
    for f in raced.clone() {
        raw_frame(&mut ws, f).await;
    }
    assert_eq!(h.hub().thread_view(&t).unwrap().current_turn, Some(2));
    // The connection drops; the runner replays the same frames on the next one.
    drop(ws);
    let hub = h.hub().clone();
    wait_for("offline", move || !hub.boxes()[0].1).await;
    let mut ws = raw_online(&h, vec![running(&t, "r1")]).await;
    // Duplicates, acked with the watermark (5): the last one's ack covers them all.
    let [started, text, ended] = raced;
    raw_send(&mut ws, &started).await;
    raw_send(&mut ws, &text).await;
    raw_frame(&mut ws, ended).await;
    assert_eq!(h.hub().thread_view(&t).unwrap().current_turn, Some(2));
    raw_frame(&mut ws, frame(&t, 6, Some(2), end(None, 7, 3, 0.5))).await;
    raw_frame(&mut ws, frame(&t, 7, None, runtime_turn("r2"))).await;
    raw_frame(&mut ws, frame(&t, 8, None, end(Some("r1"), 1, 1, 0.0))).await;
    assert_eq!(h.hub().thread_view(&t).unwrap().current_turn, Some(3));
    raw_frame(&mut ws, frame(&t, 9, None, end(Some("r2"), 2, 2, 0.0))).await;
    let all = log(h.hub(), &t);
    let turns: Vec<(&str, Option<u64>)> = all
        .iter()
        .filter(|e| matches!(e.kind(), "turn.started" | "turn.ended"))
        .map(|e| (e.kind(), e.turn))
        .collect();
    assert_eq!(
        turns,
        vec![
            ("turn.started", Some(1)),
            ("turn.ended", Some(1)),
            ("turn.started", Some(2)),
            ("turn.ended", Some(2)),
            ("turn.started", Some(3)),
            ("turn.ended", Some(3)),
        ]
    );
    let text = all.iter().find(|e| e.kind() == "agent.text").unwrap();
    assert_eq!(text.turn, Some(2));
    let usage = turn_ends(h.hub(), &t, 2)
        .into_iter()
        .map(|e| e.usage)
        .collect::<Vec<_>>();
    assert_eq!(
        usage,
        [Some(Usage {
            input: 17,
            output: 8,
            cost: Some(0.75)
        })]
    );
    // r1's late second end counts for nothing: r1 was counted in turn 2.
    let usage = turn_ends(h.hub(), &t, 3)
        .into_iter()
        .map(|e| e.usage)
        .collect::<Vec<_>>();
    assert_eq!(
        usage,
        [Some(Usage {
            input: 2,
            output: 2,
            cost: Some(0.0)
        })]
    );
    h.shutdown().await;
}

/// The end of a runtime turn whose start the hub dropped comes in a frame whose ack fails once:
/// the hub tries the frame again, and the turn that took the runtime turn in counts its usage
/// once.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_runtime_end_retried_after_a_failed_ack_counts_its_usage_once() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "retried").await;
    raw_frame(&mut ws, frame(&t, 2, Some(1), turn_ended())).await;
    h.send(&t, "next");
    let (_, op) = raw_cmd(&mut ws).await;
    assert!(matches!(&op, Op::Send { turn: 2, .. }), "{op:?}");
    let mut ext = None;
    set_twr_ext(&mut ext, RUNTIME_TURN_KEY, "r1".into());
    let start = EventBody::TurnStarted(TurnStarted {
        from: TurnFrom::Runtime,
        ext: ext.clone(),
        ..Default::default()
    });
    raw_frame(&mut ws, frame(&t, 3, None, start)).await;
    let end = |ext, input, output| {
        EventBody::TurnEnded(TurnEnded {
            usage: Some(Usage {
                input,
                output,
                cost: None,
            }),
            ext,
            ..Default::default()
        })
    };
    db_exec(
        dir.path(),
        "CREATE TRIGGER inject BEFORE INSERT ON runner_acks
         BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;",
    );
    raw_send(&mut ws, &frame(&t, 4, None, end(ext, 10, 5))).await;
    let hub = h.hub().clone();
    wait_for("a retried frame", move || {
        hub.info()["persist_retries"].as_u64().unwrap() > 0
    })
    .await;
    db_exec(dir.path(), "DROP TRIGGER inject");
    loop {
        match raw_expect(&mut ws).await {
            Some(HubMsg::Ack { rseq: 4, .. }) => break,
            Some(_) => continue,
            None => panic!("closed before the ack"),
        }
    }
    raw_frame(&mut ws, frame(&t, 5, Some(2), end(None, 7, 3))).await;
    let usage = turn_ends(h.hub(), &t, 2)
        .into_iter()
        .map(|e| e.usage)
        .collect::<Vec<_>>();
    assert_eq!(
        usage,
        [Some(Usage {
            input: 17,
            output: 8,
            cost: None
        })]
    );
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_exit_of_an_earlier_run_closes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "twice").await;
    // Run r1 ends (a spool overflow's synthetic exit, say).
    raw_frame(&mut ws, exited(&t, 2, Some(1), Some("r1"))).await;
    // The user sends again: run r2.
    h.send(&t, "again");
    let (id, op) = raw_cmd(&mut ws).await;
    assert!(matches!(&op, Op::Resume { .. }), "{op:?}");
    raw_ok(&mut ws, id).await;
    let (id, op) = raw_cmd(&mut ws).await;
    assert!(matches!(&op, Op::Send { turn: 2, .. }), "{op:?}");
    raw_ok(&mut ws, id).await;
    let r2 = RunnerMsg::Event {
        thread: t.clone(),
        rseq: 3,
        turn: None,
        body: started("s-twice"),
        raw: None,
        run: Some(run_id(&t, "r2")),
        at_ms: None,
    };
    raw_frame(&mut ws, r2).await;
    // A runner that restarted reports r1's end again, naming no run: not r2's end.
    raw_frame(&mut ws, exited(&t, 4, None, None)).await;
    let all = log(h.hub(), &t);
    assert_eq!(count(&all, "thread.crashed"), 1, "{:?}", kinds(&all));
    assert_eq!(h.hub().get_thread(&t).unwrap()["live"], json!(true));
    assert_eq!(h.hub().thread_view(&t).unwrap().current_turn, Some(2));
    h.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_question_raised_between_turns_survives_a_hub_restart() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "idleq").await;
    raw_frame(&mut ws, frame(&t, 2, Some(1), turn_ended())).await;
    // Between turns, the runtime asks something (a PTY runtime at its prompt).
    raw_frame(&mut ws, frame(&t, 3, None, question("q-idle"))).await;
    assert_eq!(items(h.hub()).len(), 1);
    drop(ws);
    h.crash();
    // The runtime still runs on the box: its question still waits for the user.
    let h = TestHub::start(dir.path(), config(None)).await;
    let _ws = raw_online(&h, vec![running(&t, "r1")]).await;
    assert_eq!(items(h.hub()), vec![(format!("{t}:1:q-idle"), t.clone())]);
    h.shutdown().await;
}

/// A runtime's question between turns (a PTY at its prompt) is answered, then
/// the user sends a prompt before the box acknowledged the answer. The prompt waits for the
/// acknowledgement (it would otherwise overtake the answer, and a reconnection would replay
/// it and drop the answer, leaving the PTY waiting forever); a hub restart meanwhile sends
/// the answer again first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_prompt_waits_for_the_answer_given_between_turns() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "ptyq").await;
    raw_frame(&mut ws, frame(&t, 2, Some(1), turn_ended())).await;
    raw_frame(&mut ws, frame(&t, 3, None, question("q-idle"))).await;
    let q = format!("{t}:1:q-idle");
    h.sync(|hub| hub.respond(&q, Answer::new("yes"), "test").unwrap());
    let (respond_id, op) = raw_cmd(&mut ws).await;
    // Bound to the run that raised the item (the runner refuses it for any later run).
    assert!(
        matches!(&op, Op::Respond { run: Some(r), .. } if *r == run_id(&t, "r1")),
        "{op:?}"
    );
    assert!(respond_id.starts_with("respond:1:"), "{respond_id}");
    // Not acknowledged yet: the next prompt waits in the queue.
    h.send(&t, "next prompt");
    no_cmd(&mut ws, 500).await;
    assert_eq!(h.hub().get_thread(&t).unwrap()["pending"], 1);
    // The hub dies; the next one sends the answer again (same id), still before any prompt.
    drop(ws);
    h.crash();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![running(&t, "r1")]).await;
    let (again, op) = raw_cmd(&mut ws).await;
    assert_eq!(again, respond_id);
    // Sent again as recorded: still bound to the run that raised the item.
    assert!(
        matches!(&op, Op::Respond { run: Some(r), .. } if *r == run_id(&t, "r1")),
        "{op:?}"
    );
    no_cmd(&mut ws, 500).await;
    assert_eq!(h.hub().get_thread(&t).unwrap()["pending"], 1);
    // Acknowledged: the prompt goes.
    raw_ok(&mut ws, respond_id).await;
    let (_, op) = raw_cmd(&mut ws).await;
    assert!(
        matches!(&op, Op::Send { turn: 2, text, .. } if text.contains("next prompt")),
        "{op:?}"
    );
    assert_eq!(outbox_rows(dir.path()), 1);
    h.shutdown().await;
}

/// An answer given in a turn that the runtime ended before the answer reached it
/// (the end was in the runner's spool when the connection dropped) still goes out at the
/// reconnection: it belongs to the runtime (its generation), not to the turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_answer_outlives_the_turn_it_was_given_in() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "late").await;
    raw_frame(&mut ws, frame(&t, 2, Some(1), question("q1"))).await;
    let q = format!("{t}:1:q1");
    h.sync(|hub| hub.respond(&q, Answer::new("yes"), "test").unwrap());
    let (respond_id, _) = raw_cmd(&mut ws).await;
    drop(ws);
    let hub = h.hub().clone();
    wait_for("offline", move || !hub.boxes()[0].1).await;
    // The runner replays the turn's end, then syncs.
    let mut ws = raw(&h.url).await;
    let mut hi = hello(TOKEN);
    hi.running = vec![running(&t, "r1")];
    raw_hello(&mut ws, hi).await;
    let Some(HubMsg::Welcome { .. }) = raw_expect(&mut ws).await else {
        panic!("no welcome")
    };
    raw_frame(&mut ws, frame(&t, 3, Some(1), turn_ended())).await;
    assert_eq!(h.hub().thread_view(&t).unwrap().current_turn, None);
    raw_send(&mut ws, &RunnerMsg::Synced).await;
    h.online().await;
    let (again, op) = raw_cmd(&mut ws).await;
    assert_eq!(again, respond_id);
    assert!(matches!(&op, Op::Respond { .. }), "{op:?}");
    raw_ok(&mut ws, respond_id).await;
    let d = dir.path().to_path_buf();
    wait_for("the answer forgotten", move || outbox_rows(&d) == 0).await;
    h.shutdown().await;
}

/// Capping metadata through the runner ingest path: a runner's `_capped` markers and `<field>_ref`
/// siblings (pointing at blobs its raw lines create) are stripped, so after a hub restart the
/// replayed session id and attention item are exactly the live ones.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runner_supplied_capping_metadata_cannot_split_replay_from_live() {
    fn spoofed(body: EventBody, field: &str, other: &str) -> EventBody {
        let mut data = body.data().unwrap();
        data["_capped"] = json!([field]);
        data[format!("{field}_ref")] = json!(twr_core::blob::blob_ref(other.as_bytes()));
        EventBody::from_kind_data(body.kind(), data).unwrap()
    }
    fn with_raw(mut m: RunnerMsg, line: &str) -> RunnerMsg {
        if let RunnerMsg::Event { raw, .. } = &mut m {
            *raw = Some(line.into());
        }
        m
    }
    let item_state = |hub: &Hub| -> Vec<Value> {
        hub.attention_list(true)
            .into_iter()
            .map(|i| json!([i["attn_id"], i["reason"], i["summary"], i["options"]]))
            .collect()
    };
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.new_thread_raw_named(&mut ws, dir.path(), "spoof").await;
    h.send(&t, "hi");
    let (cmd_id, op) = raw_cmd(&mut ws).await;
    assert!(matches!(op, Op::Spawn { .. }), "{op:?}");
    raw_ok(&mut ws, cmd_id).await;
    let started = spoofed(started("session-"), "session_id", "session-other");
    raw_frame(
        &mut ws,
        with_raw(frame(&t, 1, None, started), "session-other"),
    )
    .await;
    raw_frame(&mut ws, frame(&t, 2, Some(1), turn_ended())).await;
    let q = spoofed(question("q1"), "summary", "which one? rm -rf /");
    raw_frame(
        &mut ws,
        with_raw(frame(&t, 3, None, q), "which one? rm -rf /"),
    )
    .await;
    let live_session = h.hub().thread_view(&t).unwrap().session_id;
    assert_eq!(live_session.as_deref(), Some("session-"));
    let live_items = item_state(h.hub());
    assert_eq!(live_items.len(), 1);
    assert_eq!(live_items[0][2], "which one?");
    assert_eq!(h.hub().info()["sanitized_events"], 2);
    drop(ws);
    h.crash();
    let h = TestHub::start(dir.path(), config(None)).await;
    let _ws = raw_online(&h, vec![running(&t, "r1")]).await;
    assert_eq!(h.hub().thread_view(&t).unwrap().session_id, live_session);
    assert_eq!(item_state(h.hub()), live_items);
    h.shutdown().await;
}

// ------------------------------------------------------------------------------------------
// Failed and expired recorded commands are never only logged
// ------------------------------------------------------------------------------------------

/// The runner's answer to recorded command `cmd_id`: it could not run it (its spool refused
/// it, as when the disk is full).
async fn raw_refuse(ws: &mut RawWs, cmd_id: String) {
    let e = CmdError::new(cmd_errors::FAILED, "runner spool: database or disk is full");
    raw_send(ws, &RunnerMsg::CmdResult(CmdResult::new(cmd_id, Err(e)))).await;
}

/// Waits until the hub has handled everything sent before (it answers a ping in order);
/// fails on any command meanwhile.
async fn raw_barrier(ws: &mut RawWs) {
    raw_send(ws, &RunnerMsg::Ping).await;
    loop {
        match raw_next(ws).await {
            Some(HubMsg::Pong) => return,
            Some(HubMsg::Cmd { op, .. }) => panic!("unexpected command {op:?}"),
            Some(_) => continue,
            None => panic!("closed"),
        }
    }
}

/// `ext.twr.error` of the one `turn.ended` of turn `turn` of `thread` (which must be `error`).
fn turn_error(hub: &Hub, thread: &str, turn: u64) -> String {
    let ends = turn_ends(hub, thread, turn);
    assert_eq!(ends.len(), 1, "{ends:?}");
    assert_eq!(ends[0].result, TurnResult::Error);
    ends[0].ext.as_ref().unwrap()["twr"]["error"]
        .as_str()
        .unwrap()
        .to_string()
}

/// Summaries of the open `error` items of `thread`.
fn error_items(hub: &Hub, thread: &str) -> Vec<String> {
    hub.attention_list(true)
        .iter()
        .filter(|i| i["thread"] == thread && i["reason"] == "error")
        .map(|i| i["summary"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn stop_request_rows(dir: &Path) -> i64 {
    rusqlite::Connection::open(dir.join("hub/twr.db"))
        .unwrap()
        .query_row("SELECT COUNT(*) FROM stop_requests", [], |r| r.get(0))
        .unwrap()
}

/// Sends the `exited` frame of run `r1` and returns the first command that follows (it may
/// come before the frame's ack).
async fn exit_then_cmd(ws: &mut RawWs, thread: &str, rseq: u64) -> (String, Op) {
    let mut cmds = raw_frame(ws, exited(thread, rseq, Some(0), Some("r1"))).await;
    if cmds.is_empty() {
        raw_cmd(ws).await
    } else {
        cmds.remove(0)
    }
}

/// A `send` the runner refuses ends its turn `error` (the error item raised),
/// retires the record in the same batch, and holds the queue: nothing is dispatched to the
/// runtime, even when the box reconnects, until the user sends again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_the_runner_refuses_fails_its_turn_and_holds_the_queue() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "nosend").await;
    raw_frame(&mut ws, frame(&t, 2, Some(1), turn_ended())).await;
    h.send(&t, "second");
    let (send_id, op) = raw_cmd(&mut ws).await;
    assert!(matches!(&op, Op::Send { turn: 2, .. }), "{op:?}");
    h.send(&t, "third");
    assert_eq!(h.hub().get_thread(&t).unwrap()["pending"], 1);
    raw_refuse(&mut ws, send_id).await;
    raw_barrier(&mut ws).await;
    let why = turn_error(h.hub(), &t, 2);
    assert!(
        why.contains("runner could not deliver the prompt") && why.contains("disk is full"),
        "{why}"
    );
    assert_eq!(outbox_rows(dir.path()), 0);
    let errors = error_items(h.hub(), &t);
    assert_eq!(errors.len(), 1, "{errors:?}");
    // The runtime never got the prompt: it is not stopped.
    assert_eq!(stop_request_rows(dir.path()), 0);
    // Held: nothing goes out, not even when the box comes back.
    no_cmd(&mut ws, 500).await;
    drop(ws);
    let hub = h.hub().clone();
    wait_for("offline", move || !hub.boxes()[0].1).await;
    let mut ws = raw_online(&h, vec![running(&t, "r1")]).await;
    no_cmd(&mut ws, 500).await;
    assert_eq!(h.hub().get_thread(&t).unwrap()["pending"], 1);
    // The user's next send releases the queue, in order.
    h.send(&t, "fourth");
    let (_, op) = raw_cmd(&mut ws).await;
    assert!(
        matches!(&op, Op::Send { turn: 3, text, .. } if text.contains("third")),
        "{op:?}"
    );
    h.shutdown().await;
}

/// A runner that reconnects running another run of a thread than the hub's session (both
/// runs known): the hub's session is lost (closed like one the runner no longer runs) and the
/// runner's run is a stray, stopped by its run; no prompt of the hub's ever reaches it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reconnection_running_another_run_closes_the_session_and_stops_that_run() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "otherrun").await;
    raw_frame(&mut ws, frame(&t, 2, Some(1), turn_ended())).await;
    drop(ws);
    let hub = h.hub().clone();
    wait_for("offline", move || !hub.boxes()[0].1).await;
    let mut ws = raw_online(&h, vec![running(&t, "rx")]).await;
    let (cmd_id, op) = raw_cmd(&mut ws).await;
    assert!(
        matches!(&op, Op::Stop { thread, run: Some(r) } if *thread == t && r == "rx"),
        "{op:?}"
    );
    raw_ok(&mut ws, cmd_id).await;
    let hub = h.hub().clone();
    let t2 = t.clone();
    wait_for("the lost session closed", move || {
        count(&log(&hub, &t2), "thread.crashed") == 1
    })
    .await;
    // The next prompt starts a session of its own (bound to no earlier run), never a `send`.
    h.send(&t, "next");
    let (_, op) = raw_cmd(&mut ws).await;
    assert!(matches!(&op, Op::Resume { .. }), "{op:?}");
    h.shutdown().await;
}

/// A prompt for a remote session goes bound to its run (known from its frames), recorded and
/// live: a runner whose live session is another run refuses it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_send_is_bound_to_the_run_of_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "boundsend").await;
    raw_frame(&mut ws, frame(&t, 2, Some(1), turn_ended())).await;
    h.send(&t, "second");
    let (_, op) = raw_cmd(&mut ws).await;
    assert!(
        matches!(&op, Op::Send { turn: 2, run: Some(r), .. } if *r == run_id(&t, "r1")),
        "{op:?}"
    );
    // And so is the recorded one a reconnection sends again.
    drop(ws);
    let hub = h.hub().clone();
    wait_for("offline", move || !hub.boxes()[0].1).await;
    let mut ws = raw_online(&h, vec![running(&t, "r1")]).await;
    let (_, op) = raw_cmd(&mut ws).await;
    assert!(
        matches!(&op, Op::Send { turn: 2, run: Some(r), .. } if *r == run_id(&t, "r1")),
        "{op:?}"
    );
    h.shutdown().await;
}

/// A session's forwarder that the hub replaces (a new session of the thread started) while a
/// command of it is in flight gives up on that command and on what is queued behind it: the
/// recorded ones (the `send` in flight, a `thread.stop`'s `stop` queued) are settled while the
/// box stays connected (forgotten: their runtime is gone), not left in `remote_outbox` until
/// the next connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recorded_commands_a_replaced_forwarder_gave_up_on_are_settled_while_connected() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "orphans").await;
    raw_frame(&mut ws, frame(&t, 2, Some(1), turn_ended())).await;
    h.send(&t, "second");
    // The prompt is in flight (the runner never answers it); the stop waits behind it.
    let (_, op) = raw_cmd(&mut ws).await;
    assert!(matches!(&op, Op::Send { turn: 2, .. }), "{op:?}");
    h.sync(|hub| hub.stop_thread(&t, "test").unwrap());
    h.send(&t, "next");
    // The runtime ends anyway: the thread stops, and the queue starts a new session.
    raw_send(&mut ws, &exited(&t, 3, Some(0), Some("r1"))).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let mut ops = Vec::new();
    while tokio::time::Instant::now() < deadline {
        if let Ok((id, op)) =
            tokio::time::timeout(Duration::from_millis(200), raw_cmd(&mut ws)).await
        {
            ops.push(op.name().to_string());
            raw_ok(&mut ws, id).await;
        }
    }
    assert!(ops.iter().any(|o| o == "resume"), "{ops:?}");
    assert!(!ops.iter().any(|o| o == "stop"), "{ops:?}");
    assert_eq!(count(&log(h.hub(), &t), "thread.stopped"), 1);
    let d = dir.path().to_path_buf();
    wait_for("the outbox settled", move || outbox_rows(&d) == 0).await;
    h.shutdown().await;
}

/// An answer the runner refuses because its runtime has exited (`not_running`) no longer
/// holds the thread's queue (a thread takes no new turn while an answer to its remote
/// runtime is unacknowledged): once it is retired, the prompts queued after a stop go.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_answer_to_an_exited_runtime_releases_the_queue() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "lateanswer").await;
    raw_frame(&mut ws, frame(&t, 2, Some(1), question("q1"))).await;
    let q1 = format!("{t}:1:q1");
    h.sync(|hub| hub.respond(&q1, Answer::new("this one"), "test").unwrap());
    let (respond_id, op) = raw_cmd(&mut ws).await;
    assert!(matches!(&op, Op::Respond { .. }), "{op:?}");
    h.sync(|hub| hub.stop_thread(&t, "test").unwrap());
    h.send(&t, "next");
    let cmds = raw_frame(&mut ws, exited(&t, 3, Some(0), Some("r1"))).await;
    assert!(
        cmds.iter().all(|(_, op)| matches!(op, Op::Stop { .. })),
        "{cmds:?}"
    );
    // The answer never got to the runtime (it had exited).
    let e = CmdError::new(cmd_errors::NOT_RUNNING, "thread has no running session");
    raw_send(
        &mut ws,
        &RunnerMsg::CmdResult(CmdResult::new(respond_id, Err(e))),
    )
    .await;
    let op = loop {
        let (id, op) = raw_cmd(&mut ws).await;
        raw_ok(&mut ws, id).await;
        if !matches!(op, Op::Stop { .. }) {
            break op;
        }
    };
    assert!(matches!(&op, Op::Resume { .. }), "{op:?}");
    assert_eq!(count(&log(h.hub(), &t), "thread.stopped"), 1);
    h.shutdown().await;
}

/// `thread.stop` of a thread with no runtime drops the queue and records `thread.stopped` in
/// one batch: when storage refuses the stop, nothing was dropped (the answer is an error and
/// the queue is intact); once it goes in, the answer says what was dropped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_without_a_runtime_drops_the_queue_only_with_its_thread_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "stopbatch").await;
    raw_frame(&mut ws, frame(&t, 2, Some(1), turn_ended())).await;
    raw_frame(&mut ws, exited(&t, 3, Some(1), Some("r1"))).await;
    assert_eq!(count(&log(h.hub(), &t), "thread.crashed"), 1);
    drop(ws);
    let hub = h.hub().clone();
    wait_for("offline", move || !hub.boxes()[0].1).await;
    // The box is away: the prompts wait.
    h.send(&t, "one");
    h.send(&t, "two");
    assert_eq!(h.hub().get_thread(&t).unwrap()["pending"], 2);
    db_exec(
        dir.path(),
        "CREATE TRIGGER no_stop BEFORE INSERT ON events WHEN NEW.kind = 'thread.stopped' \
         BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    );
    assert!(h.sync(|hub| hub.stop_thread(&t, "test")).is_err());
    assert_eq!(h.hub().get_thread(&t).unwrap()["pending"], 2);
    db_exec(dir.path(), "DROP TRIGGER no_stop");
    let r = h.sync(|hub| hub.stop_thread(&t, "test")).unwrap();
    assert_eq!(r["stopping"], json!(false), "{r}");
    assert_eq!(r["dropped_prompts"], json!(2), "{r}");
    assert_eq!(h.hub().get_thread(&t).unwrap()["pending"], 0);
    assert_eq!(count(&log(h.hub(), &t), "thread.stopped"), 1);
    h.shutdown().await;
}

/// A box that goes offline while `thread.new` asks its runner where the repository is: the
/// request fails `box_offline` (try again once it is back), not `bad_request`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_box_going_offline_during_thread_new_answers_box_offline() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let req = NewThread {
        project: dir.path().join("rawproj").to_string_lossy().into_owned(),
        runtime: Some("fake".into()),
        name: Some("away".into()),
        box_name: Some(BOX.into()),
        ..Default::default()
    };
    let hub = h.hub().clone();
    let created =
        h.rt.as_ref()
            .unwrap()
            .spawn(async move { hub.new_thread(req, "test").await });
    let (_, op) = raw_cmd(&mut ws).await;
    assert!(matches!(op, Op::RepoRoot { .. }), "{op:?}");
    drop(ws);
    let e = created.await.unwrap().unwrap_err();
    assert_eq!(e.code, "box_offline", "{e:?}");
    h.shutdown().await;
}

/// A new session of a thread knows its run from the start (the hub names it in `spawn` /
/// `resume`): the exit of a stray run of the thread (stopped at the reconciliation) that
/// arrives after the new session started is the stray's, and closes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_strays_exit_after_a_new_session_started_closes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "strayexit").await;
    raw_frame(&mut ws, frame(&t, 2, Some(1), turn_ended())).await;
    h.sync(|hub| hub.stop_thread(&t, "test").unwrap());
    let (stop_id, _) = raw_cmd(&mut ws).await;
    raw_ok(&mut ws, stop_id).await;
    raw_frame(&mut ws, exited(&t, 3, Some(0), Some("r1"))).await;
    assert_eq!(count(&log(h.hub(), &t), "thread.stopped"), 1);
    drop(ws);
    let hub = h.hub().clone();
    wait_for("offline", move || !hub.boxes()[0].1).await;
    // The runner comes back running a run of the thread the hub does not run: a stray.
    let mut ws = raw_online(&h, vec![running(&t, "rx")]).await;
    let (stop_id, op) = raw_cmd(&mut ws).await;
    assert!(
        matches!(&op, Op::Stop { run: Some(r), .. } if r == "rx"),
        "{op:?}"
    );
    raw_ok(&mut ws, stop_id).await;
    h.send(&t, "again");
    let (id, op) = raw_cmd(&mut ws).await;
    let run = match &op {
        Op::Resume { run, .. } => run.clone(),
        other => panic!("{other:?}"),
    };
    assert!(run.as_deref().is_some_and(|r| r != "rx"), "{op:?}");
    raw_ok(&mut ws, id).await;
    // The stray's exit comes after that: acked, and nothing is closed.
    raw_frame(&mut ws, exited(&t, 4, Some(0), Some("rx"))).await;
    let all = log(h.hub(), &t);
    assert_eq!(count(&all, "thread.stopped"), 1, "{:?}", kinds(&all));
    assert_eq!(count(&all, "thread.crashed"), 0, "{:?}", kinds(&all));
    assert_eq!(h.hub().thread_view(&t).unwrap().current_turn, Some(2));
    h.shutdown().await;
}

/// A session the hub named the run of, of which no frame came yet, is closed by the exit a
/// restarted runner records (naming no run) for the runtime its predecessor left behind: the
/// run the hub named may well be that one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recovered_exit_naming_no_run_closes_a_session_not_heard_from_yet() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h
        .new_thread_raw_named(&mut ws, dir.path(), "leftbehind")
        .await;
    h.send(&t, "hi");
    let (cmd_id, op) = raw_cmd(&mut ws).await;
    assert!(matches!(&op, Op::Spawn { run: Some(_), .. }), "{op:?}");
    raw_ok(&mut ws, cmd_id).await;
    // The runner dies before the runtime says anything; the next one finds it and records
    // its end.
    drop(ws);
    let hub = h.hub().clone();
    wait_for("offline", move || !hub.boxes()[0].1).await;
    let mut ws = raw(&h.url).await;
    let mut hi = hello(TOKEN);
    hi.acks_needed = std::collections::BTreeMap::from([(t.clone(), 1)]);
    raw_hello(&mut ws, hi).await;
    let Some(HubMsg::Welcome { .. }) = raw_expect(&mut ws).await else {
        panic!("no welcome")
    };
    raw_frame(&mut ws, exited(&t, 1, None, None)).await;
    raw_send(&mut ws, &RunnerMsg::Synced).await;
    h.online().await;
    let hub = h.hub().clone();
    let t2 = t.clone();
    wait_for("the turn closed", move || {
        hub.thread_view(&t2).unwrap().current_turn.is_none()
    })
    .await;
    assert_eq!(count(&log(h.hub(), &t), "thread.crashed"), 1);
    h.shutdown().await;
}

/// A recorded command's result changes nothing once a newer connection replaced
/// the one it came on, checked at every attempt. The old connection's refusal of a `send` is
/// retried (storage refuses the `turn.ended` it means) while a replacement connects, syncs
/// and gets the `send` again: when storage recovers, the old retry neither ends the turn nor
/// retires the record, and the replacement's own answer completes the turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fenced_connections_command_result_retry_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws1 = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws1, dir.path(), "resent").await;
    raw_frame(&mut ws1, frame(&t, 2, Some(1), turn_ended())).await;
    h.send(&t, "second");
    let (send_id, op) = raw_cmd(&mut ws1).await;
    assert!(matches!(&op, Op::Send { turn: 2, .. }), "{op:?}");
    db_exec(dir.path(), STORAGE_REFUSES_TURN_ENDS);
    raw_refuse(&mut ws1, send_id.clone()).await;
    let hub = h.hub().clone();
    wait_for("a retry", move || {
        hub.info()["persist_retries"].as_u64().unwrap() > 0
    })
    .await;
    // A replacement connects and syncs meanwhile: the `send` goes to it again.
    let mut ws2 = raw(&h.url).await;
    let mut hi = hello(TOKEN);
    hi.running = vec![running(&t, "r1")];
    raw_hello(&mut ws2, hi).await;
    let Some(HubMsg::Welcome { .. }) = raw_expect(&mut ws2).await else {
        panic!("no welcome")
    };
    raw_send(&mut ws2, &RunnerMsg::Synced).await;
    let (again, op) = raw_cmd(&mut ws2).await;
    assert_eq!(again, send_id);
    assert!(matches!(&op, Op::Send { turn: 2, .. }), "{op:?}");
    db_exec(dir.path(), STORAGE_OK);
    // The old connection's retry commits nothing; it is closed.
    while raw_next(&mut ws1).await.is_some() {}
    assert!(turn_ends(h.hub(), &t, 2).is_empty());
    assert_eq!(outbox_rows(dir.path()), 1, "the reissued send was retired");
    assert_eq!(h.hub().thread_view(&t).unwrap().current_turn, Some(2));
    assert!(error_items(h.hub(), &t).is_empty());
    // The replacement's flow is intact: its answer retires the record, its frames end the turn.
    raw_ok(&mut ws2, again).await;
    let d = dir.path().to_path_buf();
    wait_for("the send forgotten", move || outbox_rows(&d) == 0).await;
    raw_frame(&mut ws2, frame(&t, 3, Some(2), turn_ended())).await;
    let ends = turn_ends(h.hub(), &t, 2);
    assert_eq!(ends.len(), 1, "{ends:?}");
    assert_eq!(ends[0].result, TurnResult::Completed);
    assert!(h.hub().boxes()[0].1, "the replacement is online");
    h.shutdown().await;
}

/// A `respond` the runner refuses leaves the runtime waiting for an answer whose
/// item is resolved. In one batch the record is retired, a durable stop of the runtime is
/// recorded and the turn ends `error`; the stop goes out, nothing is dispatched to the
/// runtime, the error stays visible, and the user can resume the thread afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_respond_the_runner_refuses_stops_the_waiting_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "noanswer").await;
    raw_frame(&mut ws, frame(&t, 2, Some(1), question("q1"))).await;
    let q1 = format!("{t}:1:q1");
    h.sync(|hub| hub.respond(&q1, Answer::new("this one"), "test").unwrap());
    let (respond_id, op) = raw_cmd(&mut ws).await;
    assert!(matches!(&op, Op::Respond { .. }), "{op:?}");
    h.send(&t, "queued");
    raw_refuse(&mut ws, respond_id).await;
    let (stop_id, op) = raw_cmd(&mut ws).await;
    assert!(
        matches!(&op, Op::Stop { thread, .. } if thread == &t),
        "{op:?}"
    );
    assert_eq!(stop_id, format!("stop:{t}:1"));
    // Durable: the stop request and its command are recorded.
    assert_eq!(stop_request_rows(dir.path()), 1);
    assert_eq!(outbox_rows(dir.path()), 1);
    let why = turn_error(h.hub(), &t, 1);
    assert!(
        why.contains("runner could not deliver your answer")
            && why.contains("the runtime was stopped"),
        "{why}"
    );
    assert_eq!(error_items(h.hub(), &t).len(), 1);
    // Nothing is dispatched to the runtime being stopped.
    no_cmd(&mut ws, 500).await;
    raw_ok(&mut ws, stop_id).await;
    let cmds = raw_frame(&mut ws, exited(&t, 3, Some(0), Some("r1"))).await;
    assert!(cmds.is_empty(), "{cmds:?}");
    let all = log(h.hub(), &t);
    assert_eq!(count(&all, "thread.stopped"), 1, "{:?}", kinds(&all));
    assert_eq!(count(&all, "thread.crashed"), 0, "{:?}", kinds(&all));
    assert_eq!(stop_request_rows(dir.path()), 0);
    let d = dir.path().to_path_buf();
    wait_for("the stop forgotten", move || outbox_rows(&d) == 0).await;
    // Still held after the exit; the error stays for the user.
    no_cmd(&mut ws, 500).await;
    assert_eq!(error_items(h.hub(), &t).len(), 1);
    assert_eq!(h.hub().get_thread(&t).unwrap()["pending"], 1);
    // The user sends again: the thread resumes in a new session, queue first.
    h.send(&t, "again");
    let (_, op) = raw_cmd(&mut ws).await;
    assert!(matches!(&op, Op::Resume { .. }), "{op:?}");
    h.shutdown().await;
}

/// A `stop` the runner could not run is never dropped: the stop request stands
/// and the stop goes again (same id) at the next connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_the_runner_refuses_goes_again_at_the_next_connection() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "nostop").await;
    h.sync(|hub| hub.stop_thread(&t, "test").unwrap());
    let (stop_id, op) = raw_cmd(&mut ws).await;
    assert!(matches!(&op, Op::Stop { .. }), "{op:?}");
    raw_refuse(&mut ws, stop_id.clone()).await;
    raw_barrier(&mut ws).await;
    assert_eq!(outbox_rows(dir.path()), 1);
    assert_eq!(stop_request_rows(dir.path()), 1);
    drop(ws);
    let hub = h.hub().clone();
    wait_for("offline", move || !hub.boxes()[0].1).await;
    let mut ws = raw_online(&h, vec![running(&t, "r1")]).await;
    let (id, op) = raw_cmd(&mut ws).await;
    assert_eq!(id, stop_id, "{op:?}");
    raw_ok(&mut ws, id).await;
    raw_frame(&mut ws, exited(&t, 2, Some(0), Some("r1"))).await;
    let all = log(h.hub(), &t);
    assert_eq!(count(&all, "thread.stopped"), 1, "{:?}", kinds(&all));
    let d = dir.path().to_path_buf();
    wait_for("the stop forgotten", move || outbox_rows(&d) == 0).await;
    h.shutdown().await;
}

/// The user answers, delivery is interrupted, and the runner comes back after
/// the resend horizon. The answer is not sent again, and the runtime still waiting for it is
/// stopped (recorded with the turn's end, sent on this reconnection); nothing is dispatched to
/// it, the error is visible, and the user resumes the thread afterwards.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_respond_stops_the_runtime_that_waits_for_it() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "late").await;
    raw_frame(&mut ws, frame(&t, 2, Some(1), question("q1"))).await;
    let q1 = format!("{t}:1:q1");
    h.sync(|hub| hub.respond(&q1, Answer::new("this one"), "test").unwrap());
    let (_, op) = raw_cmd(&mut ws).await;
    assert!(matches!(&op, Op::Respond { .. }), "{op:?}");
    drop(ws);
    let hub = h.hub().clone();
    wait_for("offline", move || !hub.boxes()[0].1).await;
    db_exec(
        dir.path(),
        "UPDATE remote_outbox SET created = '2000-01-01T00:00:00.000Z'",
    );
    let mut ws = raw_online(&h, vec![running(&t, "r1")]).await;
    let (stop_id, op) = raw_cmd(&mut ws).await;
    assert!(
        matches!(&op, Op::Stop { thread, .. } if thread == &t),
        "{op:?}"
    );
    assert_eq!(stop_id, format!("stop:{t}:1"));
    assert_eq!(stop_request_rows(dir.path()), 1);
    let why = turn_error(h.hub(), &t, 1);
    assert!(
        why.contains("never reached box") && why.contains("the runtime was stopped"),
        "{why}"
    );
    assert_eq!(error_items(h.hub(), &t).len(), 1);
    // Nothing goes to the runtime before its exit is recorded.
    h.send(&t, "next");
    no_cmd(&mut ws, 500).await;
    raw_ok(&mut ws, stop_id).await;
    let (_, op) = exit_then_cmd(&mut ws, &t, 3).await;
    assert!(matches!(&op, Op::Resume { .. }), "{op:?}");
    let all = log(h.hub(), &t);
    assert_eq!(count(&all, "thread.stopped"), 1, "{:?}", kinds(&all));
    assert_eq!(count(&all, "thread.crashed"), 0, "{:?}", kinds(&all));
    h.shutdown().await;
}

/// The same for a `send`: when the runtime recorded something in the turn (it got the
/// prompt and never finished it), the expired record stops it too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_expired_send_the_runtime_is_working_on_stops_it() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "busy").await;
    raw_frame(&mut ws, frame(&t, 2, Some(1), turn_ended())).await;
    h.send(&t, "second");
    let (_, op) = raw_cmd(&mut ws).await;
    assert!(matches!(&op, Op::Send { turn: 2, .. }), "{op:?}");
    raw_frame(&mut ws, frame(&t, 3, Some(2), question("q2"))).await;
    drop(ws);
    let hub = h.hub().clone();
    wait_for("offline", move || !hub.boxes()[0].1).await;
    db_exec(
        dir.path(),
        "UPDATE remote_outbox SET created = '2000-01-01T00:00:00.000Z'",
    );
    let mut ws = raw_online(&h, vec![running(&t, "r1")]).await;
    let (stop_id, op) = raw_cmd(&mut ws).await;
    assert!(matches!(&op, Op::Stop { .. }), "{op:?}");
    assert_eq!(stop_id, format!("stop:{t}:1"));
    let why = turn_error(h.hub(), &t, 2);
    assert!(why.contains("the runtime was stopped"), "{why}");
    h.shutdown().await;
}

/// A recorded stop past the resend horizon still goes out while its runtime lives (running a
/// stop twice is harmless; dropping it would leave the runtime running for good).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_old_stop_still_goes_out_while_its_runtime_lives() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "oldstop").await;
    h.sync(|hub| hub.stop_thread(&t, "test").unwrap());
    let (stop_id, _) = raw_cmd(&mut ws).await;
    drop(ws);
    let hub = h.hub().clone();
    wait_for("offline", move || !hub.boxes()[0].1).await;
    db_exec(
        dir.path(),
        "UPDATE remote_outbox SET created = '2000-01-01T00:00:00.000Z'",
    );
    let mut ws = raw_online(&h, vec![running(&t, "r1")]).await;
    let (id, op) = raw_cmd(&mut ws).await;
    assert_eq!(id, stop_id, "{op:?}");
    assert_eq!(outbox_rows(dir.path()), 1);
    h.shutdown().await;
}

fn permission(attn_id: &str) -> EventBody {
    EventBody::AttentionRaised(AttentionRaised {
        attn_id: attn_id.into(),
        reason: AttentionReason::Permission,
        summary: "may I?".into(),
        options: vec!["allow".into(), "deny".into()],
        ..Default::default()
    })
}

/// The `report.ready` events of turn `turn` of `thread`.
fn reports(hub: &Hub, thread: &str, turn: u64) -> usize {
    log(hub, thread)
        .iter()
        .filter(|e| e.kind() == "report.ready" && e.turn == Some(turn))
        .count()
}

/// The cleanup that follows a turn's end (its runtime no longer waits on the
/// requests it raised in it) is retried while storage refuses the turn's report. Meanwhile the
/// runner reconnects and the next turn raises a permission: the retry closes only what the
/// ended turn raised, and the new permission stays open and answerable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retried_turn_end_cleanup_leaves_the_next_turns_permission_open() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws1 = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws1, dir.path(), "retry").await;
    raw_frame(&mut ws1, frame(&t, 2, Some(1), question("q1"))).await;
    db_exec(
        dir.path(),
        "CREATE TRIGGER refuse_report BEFORE INSERT ON events WHEN NEW.kind = 'report.ready'
         BEGIN SELECT RAISE(ABORT, 'storage refuses report.ready'); END;",
    );
    // Turn 1 ends: recorded and acked, its question closed; its report is refused, so the
    // reactions are retried (about 2 s before the hub gives the runtime up).
    raw_frame(&mut ws1, frame(&t, 3, Some(1), turn_ended())).await;
    let hub = h.hub().clone();
    wait_for("a retry", move || {
        hub.info()["persist_retries"].as_u64().unwrap() > 0
    })
    .await;
    assert!(items(h.hub()).is_empty(), "{:?}", items(h.hub()));
    // The runner reconnects, and turn 2 raises a permission.
    let mut ws2 = raw_online(&h, vec![running(&t, "r1")]).await;
    h.send(&t, "second");
    let (send_id, op) = raw_cmd(&mut ws2).await;
    assert!(matches!(&op, Op::Send { turn: 2, .. }), "{op:?}");
    raw_ok(&mut ws2, send_id).await;
    raw_frame(&mut ws2, frame(&t, 4, Some(2), permission("p2"))).await;
    let p2 = format!("{t}:1:p2");
    // Storage recovers: the retry records turn 1's report.
    db_exec(dir.path(), "DROP TRIGGER refuse_report");
    let hub = h.hub().clone();
    let t2 = t.clone();
    wait_for("turn 1's report", move || reports(&hub, &t2, 1) == 1).await;
    let all = log(h.hub(), &t);
    assert_eq!(count(&all, "thread.crashed"), 0, "{:?}", kinds(&all));
    // Turn 2's permission is still open, and the answer goes to the runtime.
    assert_eq!(items(h.hub()), vec![(p2.clone(), t.clone())]);
    h.sync(|hub| hub.respond(&p2, Answer::new("allow"), "test").unwrap());
    let (_, op) = raw_cmd(&mut ws2).await;
    assert!(
        matches!(&op, Op::Respond { attn_id, .. } if attn_id == "p2"),
        "{op:?}"
    );
    drop(ws1);
    h.shutdown().await;
}

/// A remote session waiting in turn 1 on a question, then the hub dies: the next hub has only
/// the log (the runtime is not taken over before its box connects).
async fn remote_runtime_before_reconnection(dir: &Path, name: &str) -> (TestHub, String) {
    let h = TestHub::start(dir, config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir, name).await;
    raw_frame(&mut ws, frame(&t, 2, Some(1), question("q1"))).await;
    drop(ws);
    h.crash();
    let h = TestHub::start(dir, config(None)).await;
    assert!(!h.hub().boxes()[0].1);
    (h, t)
}

/// `thread.stop` of `t`, whose box is offline: durable, and nothing is closed yet.
fn stop_while_offline(h: &TestHub, dir: &Path, t: &str) {
    h.send(t, "queued");
    let r = h.sync(|hub| hub.stop_thread(t, "test").unwrap());
    assert_eq!(r["stopping"], json!(true), "{r}");
    assert_eq!(r["box_offline"], json!(true), "{r}");
    assert_eq!(r["dropped_prompts"], json!(1), "{r}");
    assert_eq!(stop_request_rows(dir), 1);
    assert_eq!(outbox_rows(dir), 1);
    let all = log(h.hub(), t);
    assert_eq!(count(&all, "thread.stopped"), 0, "{:?}", kinds(&all));
    assert_eq!(h.hub().thread_view(t).unwrap().current_turn, Some(1));
    // Asking again changes nothing.
    let r = h.sync(|hub| hub.stop_thread(t, "test").unwrap());
    assert_eq!(r["stopping"], json!(true), "{r}");
    assert_eq!(outbox_rows(dir), 1);
}

/// `thread.stop` after a hub restart, before the runner of the thread's box
/// reconnected. The log shows the remote runtime running (the hub has not taken it over
/// yet): the stop is recorded and waits for the box, the thread is not closed. The runner
/// comes back with the runtime: the stop goes out, and its exit closes the thread as stopped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_before_the_runner_reconnects_goes_out_when_it_does() {
    let dir = tempfile::tempdir().unwrap();
    let (h, t) = remote_runtime_before_reconnection(dir.path(), "early").await;
    stop_while_offline(&h, dir.path(), &t);
    let mut ws = raw_online(&h, vec![running(&t, "r1")]).await;
    let stop_id = format!("stop:{t}:1");
    loop {
        let (id, op) = raw_cmd(&mut ws).await;
        assert!(
            matches!(&op, Op::Stop { thread, .. } if thread == &t),
            "{op:?}"
        );
        raw_ok(&mut ws, id.clone()).await;
        if id == stop_id {
            break;
        }
    }
    raw_frame(&mut ws, exited(&t, 3, Some(137), Some("r1"))).await;
    let all = log(h.hub(), &t);
    let ks = kinds(&all);
    assert_eq!(
        ks[ks.len() - 3..],
        ["turn.ended", "attention.resolved", "thread.stopped"],
        "{ks:?}"
    );
    assert_eq!(count(&all, "thread.crashed"), 0, "{ks:?}");
    assert!(items(h.hub()).is_empty());
    assert_eq!(stop_request_rows(dir.path()), 0);
    let d = dir.path().to_path_buf();
    wait_for("the stop forgotten", move || outbox_rows(&d) == 0).await;
    h.shutdown().await;
}

/// The same, but the runner comes back without the runtime: reconciliation
/// closes the thread as stopped (not crashed), and the recorded stop is forgotten unsent.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_before_the_runner_reconnects_closes_a_runtime_it_lost_as_stopped() {
    let dir = tempfile::tempdir().unwrap();
    let (h, t) = remote_runtime_before_reconnection(dir.path(), "gone").await;
    stop_while_offline(&h, dir.path(), &t);
    let mut ws = raw_online(&h, vec![]).await;
    let all = log(h.hub(), &t);
    assert_eq!(count(&all, "thread.stopped"), 1, "{:?}", kinds(&all));
    assert_eq!(count(&all, "thread.crashed"), 0, "{:?}", kinds(&all));
    assert!(items(h.hub()).is_empty());
    assert_eq!(stop_request_rows(dir.path()), 0);
    assert_eq!(outbox_rows(dir.path()), 0);
    no_cmd(&mut ws, 300).await;
    h.shutdown().await;
}

/// A `thread.stop` of a live remote runtime whose box went away (no hub restart) is the same
/// durable stop: `box_offline`, recorded, sent when the box is back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_while_the_box_is_away_goes_out_when_it_is_back() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "away").await;
    raw_frame(&mut ws, frame(&t, 2, Some(1), question("q1"))).await;
    drop(ws);
    let hub = h.hub().clone();
    wait_for("offline", move || !hub.boxes()[0].1).await;
    stop_while_offline(&h, dir.path(), &t);
    let mut ws = raw_online(&h, vec![running(&t, "r1")]).await;
    let stop_id = format!("stop:{t}:1");
    let (id, op) = raw_cmd(&mut ws).await;
    assert_eq!(id, stop_id, "{op:?}");
    raw_ok(&mut ws, id).await;
    raw_frame(&mut ws, exited(&t, 3, Some(0), Some("r1"))).await;
    let all = log(h.hub(), &t);
    assert_eq!(count(&all, "thread.stopped"), 1, "{:?}", kinds(&all));
    assert_eq!(count(&all, "thread.crashed"), 0, "{:?}", kinds(&all));
    h.shutdown().await;
}

fn init_logs() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .with_writer(|| LogWriter)
            .finish();
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

struct LogWriter;

impl std::io::Write for LogWriter {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        LOGS.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Frames replayed from a runner's spool keep the time the runner captured them (`at_ms`):
/// the event's `ts` is that time, kept after the thread's previous event and never in the
/// future; the thread's activity (what the stall timer reads) follows it, and so does the
/// closing sequence of an `exited` frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn replayed_frames_keep_the_runners_capture_time() {
    use chrono::{DateTime, Duration as Span, Utc};
    let ms = |t: DateTime<Utc>| Some(t.timestamp_millis() as u64);
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "timed").await;
    let last_ts = |h: &TestHub| log(h.hub(), &t).last().unwrap().ts;
    let prev = last_ts(&h);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    // Captured between the previous event and now: recorded at that time, not now.
    let captured = prev + Span::milliseconds(700);
    let mut m = frame(&t, 2, Some(1), text("while the hub was away"));
    if let RunnerMsg::Event { at_ms, .. } = &mut m {
        *at_ms = ms(captured);
    }
    raw_frame(&mut ws, m).await;
    let ev = log(h.hub(), &t).last().unwrap().clone();
    assert_eq!(ev.kind(), "agent.text");
    assert_eq!(ev.ts, captured);
    assert_eq!(
        h.hub().thread_view(&t).unwrap().last_activity,
        Some(captured)
    );
    // Before the thread's previous event (a runner clock behind): never out of order.
    let mut m = frame(&t, 3, Some(1), text("clock behind"));
    if let RunnerMsg::Event { at_ms, .. } = &mut m {
        *at_ms = ms(captured - Span::hours(1));
    }
    raw_frame(&mut ws, m).await;
    assert_eq!(last_ts(&h), captured);
    // In the future (a runner clock ahead): now.
    let mut m = frame(&t, 4, Some(1), text("clock ahead"));
    if let RunnerMsg::Event { at_ms, .. } = &mut m {
        *at_ms = ms(Utc::now() + Span::hours(1));
    }
    raw_frame(&mut ws, m).await;
    let ts = last_ts(&h);
    assert!(ts > captured && ts <= Utc::now(), "{ts}");
    // Out of range: the hub's time.
    let mut m = frame(&t, 5, Some(1), text("nonsense"));
    if let RunnerMsg::Event { at_ms, .. } = &mut m {
        *at_ms = Some(u64::MAX);
    }
    raw_frame(&mut ws, m).await;
    assert!(last_ts(&h) >= ts);
    // An exit: its closing sequence is recorded at the exit's time.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let before = last_ts(&h);
    let gone = before + Span::milliseconds(500);
    let mut m = exited(&t, 6, Some(1), Some("r1"));
    if let RunnerMsg::Exited { at_ms, .. } = &mut m {
        *at_ms = ms(gone);
    }
    raw_frame(&mut ws, m).await;
    let all = log(h.hub(), &t);
    let crashed = all.iter().find(|e| e.kind() == "thread.crashed").unwrap();
    assert_eq!(crashed.ts, gone);
    // The log stays in time order.
    assert!(all.windows(2).all(|w| w[0].ts <= w[1].ts));
    h.shutdown().await;
}

fn text(t: &str) -> EventBody {
    EventBody::AgentText(AgentText {
        text: t.into(),
        ..Default::default()
    })
}

/// A 2000-frame backlog (what a runner replays after an outage) is recorded in batches: one
/// sync per batch instead of one per frame, acked cumulatively once synced. It goes through
/// quickly, in order, with nothing lost or recorded twice; each ack covers only committed
/// frames (acks only ever rise).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_large_backlog_replays_in_synced_batches() {
    const N: u64 = 2000;
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(None)).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.raw_session(&mut ws, dir.path(), "backlog").await;
    let started = std::time::Instant::now();
    let (mut tx, mut rx) = ws.split();
    let frames: Vec<String> = (0..N)
        .map(|i| {
            serde_json::to_string(&frame(&t, i + 2, Some(1), text(&format!("w{i} ")))).unwrap()
        })
        .collect();
    let sender = tokio::spawn(async move {
        for f in frames {
            tx.send(Message::Text(f.into())).await.unwrap();
        }
        tx
    });
    let mut acks = Vec::new();
    let last = N + 1;
    tokio::time::timeout(Duration::from_secs(60), async {
        while let Some(m) = rx.next().await {
            let Ok(Message::Text(m)) = m else { continue };
            if let Ok(HubMsg::Ack { thread, rseq }) = serde_json::from_str(m.as_str()) {
                assert_eq!(thread, t);
                acks.push(rseq);
                if rseq == last {
                    return;
                }
            }
        }
        panic!("closed before the last ack");
    })
    .await
    .expect("the backlog was not acked within 60 s");
    let elapsed = started.elapsed();
    let _tx = sender.await.unwrap();
    // Batched: far fewer acks than frames, never going back.
    assert!(acks.windows(2).all(|w| w[0] < w[1]), "{acks:?}");
    assert!(acks.len() < (N / 4) as usize, "{} acks", acks.len());
    let texts: Vec<String> = log(h.hub(), &t)
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::AgentText(a) => Some(a.text.clone()),
            _ => None,
        })
        .collect();
    let want: Vec<String> = (0..N).map(|i| format!("w{i} ")).collect();
    assert_eq!(texts, want, "gaps or duplicates");
    // One sync per frame took ~60 s here (35 frames/s); batched it takes well under a second.
    assert!(elapsed < Duration::from_secs(20), "{elapsed:?}");
    eprintln!("{N} frames in {elapsed:?} ({} acks)", acks.len());
    h.shutdown().await;
}

/// A session id a runner reports for its thread reaches a command line only when it is one
/// twr passes there: `--dangerously-skip-permissions` as the id would make `claude --resume`
/// open its session picker with permissions bypassed. The attach is refused before anything
/// is stopped or the runner asked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_remote_attach_refuses_a_session_id_that_reads_as_an_option() {
    let dir = tempfile::tempdir().unwrap();
    let h = TestHub::start(dir.path(), config(Some("me@gpu.lan"))).await;
    let mut ws = raw_online(&h, vec![]).await;
    let t = h.new_thread_raw_named(&mut ws, dir.path(), "evil").await;
    h.send(&t, "hi");
    let (cmd_id, op) = raw_cmd(&mut ws).await;
    assert!(matches!(op, Op::Spawn { .. }), "{op:?}");
    raw_ok(&mut ws, cmd_id).await;
    raw_frame(
        &mut ws,
        frame(&t, 1, None, started("--dangerously-skip-permissions")),
    )
    .await;
    raw_frame(&mut ws, frame(&t, 2, Some(1), turn_ended())).await;
    let e = h.hub().attach_begin(&t, false, "cli").await.unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST, "{e:?}");
    assert!(e.message.contains("not a session id"), "{}", e.message);
    assert!(h.hub().get_thread(&t).unwrap()["attached"].is_null());
    h.shutdown().await;
}
