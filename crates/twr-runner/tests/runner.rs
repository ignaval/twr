//! Runner-side tests against an in-test fake hub (a tokio-tungstenite server) and the fake
//! adapter.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use twr_core::event::EventBody;
use twr_core::runner::{
    auth_proof, check_auth_proof, cmd_errors, new_nonce, CmdResult, Hello, HubMsg, Inventory, Op,
    Prover, RunnerMsg, Secret, WireSpawnConfig, PROTOCOL_VERSION,
};
use twr_runner::spool::Appended;
use twr_runner::{LocalBox, LocalBoxOptions, RunnerOptions};

const TOKEN: &str = "s3cr3t-runner-token-e2f1";
const BOX: &str = "gpu";

/// A committed runner frame: (thread, rseq, kind), kind `exited` for exits.
type Committed = (String, u64, String);

#[derive(Default)]
struct HubState {
    epoch: u64,
    watermarks: BTreeMap<String, u64>,
    hellos: Vec<Hello>,
    /// The inventories of the connections that authenticated.
    inventories: Vec<Inventory>,
    /// Connections whose runner did not answer the hub's challenge (it did not accept the
    /// hub's proof).
    unanswered: u64,
    /// When each of those connections ended (the runner left after the challenge).
    unanswered_at: Vec<std::time::Instant>,
    /// Every event/exited frame received, duplicates included.
    received: Vec<(String, u64)>,
    committed: Vec<Committed>,
    /// Bodies of committed events.
    bodies: Vec<(String, EventBody)>,
    results: Vec<CmdResult>,
    synced: u64,
    /// Close the next connection after this many event/exited frames (the last one is not
    /// processed).
    drop_after: Option<usize>,
    manual_acks: bool,
    /// The frame lost with a dropped connection.
    lost: Option<(String, u64)>,
    connections: u64,
    closed: u64,
}

#[derive(Clone)]
struct FakeHub {
    url: String,
    state: Arc<Mutex<HubState>>,
    conn: Arc<Mutex<Option<mpsc::UnboundedSender<HubMsg>>>>,
}

impl FakeHub {
    async fn start() -> FakeHub {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hub = FakeHub {
            url: format!("ws://{addr}"),
            state: Arc::default(),
            conn: Arc::default(),
        };
        let h = hub.clone();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = listener.accept().await {
                let h = h.clone();
                tokio::spawn(async move {
                    if let Ok(ws) = tokio_tungstenite::accept_async(tcp).await {
                        h.serve(ws).await;
                    }
                    h.state().closed += 1;
                });
            }
        });
        hub
    }

    fn state(&self) -> std::sync::MutexGuard<'_, HubState> {
        self.state.lock().unwrap()
    }

    async fn serve(&self, ws: tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>) {
        let (mut sink, mut stream) = ws.split();
        let Some(Ok(Message::Text(t))) = stream.next().await else {
            return;
        };
        let Ok(RunnerMsg::Hello(hello)) = serde_json::from_str::<RunnerMsg>(t.as_str()) else {
            return;
        };
        // The hub's side of the handshake, with the box's token (`TOKEN`).
        let token = Secret::new(TOKEN);
        let hub_nonce = new_nonce().unwrap();
        let proof = auth_proof(
            &token,
            Prover::Hub,
            &hello.box_name,
            &hello.nonce,
            &hub_nonce,
        );
        self.state().hellos.push(hello.clone());
        let text = |m: &HubMsg| Message::Text(serde_json::to_string(m).unwrap().into());
        let challenge = HubMsg::Challenge {
            nonce: hub_nonce.clone(),
            proof,
        };
        if sink.send(text(&challenge)).await.is_err() {
            return;
        }
        let next = |t: Option<Result<Message, _>>| match t {
            Some(Ok(Message::Text(t))) => serde_json::from_str::<RunnerMsg>(t.as_str()).ok(),
            _ => None,
        };
        let Some(RunnerMsg::Auth { proof }) = next(stream.next().await) else {
            let mut st = self.state();
            st.unanswered += 1;
            st.unanswered_at.push(std::time::Instant::now());
            return;
        };
        assert!(
            check_auth_proof(
                &token,
                Prover::Runner,
                BOX,
                &hello.nonce,
                &hub_nonce,
                &proof
            ) && hello.box_name == BOX,
            "a runner that accepted the hub's proof must prove the same token"
        );
        if sink.send(text(&HubMsg::Authenticated)).await.is_err() {
            return;
        }
        let Some(RunnerMsg::Inventory(inv)) = next(stream.next().await) else {
            return;
        };
        let (welcome, mut drop_after) = {
            let mut s = self.state();
            s.epoch += 1;
            s.connections += 1;
            s.inventories.push(inv);
            (
                HubMsg::Welcome {
                    epoch: s.epoch,
                    watermarks: s.watermarks.clone(),
                },
                s.drop_after.take(),
            )
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        *self.conn.lock().unwrap() = Some(tx);
        let send = |m: &HubMsg| Message::Text(serde_json::to_string(m).unwrap().into());
        if sink.send(send(&welcome)).await.is_err() {
            return;
        }
        loop {
            tokio::select! {
                m = rx.recv() => {
                    let Some(m) = m else { return };
                    if sink.send(send(&m)).await.is_err() { return; }
                }
                f = stream.next() => {
                    let Some(Ok(f)) = f else { return };
                    let Message::Text(t) = f else {
                        if matches!(f, Message::Close(_)) { return; }
                        continue;
                    };
                    let msg: RunnerMsg = serde_json::from_str(t.as_str()).unwrap();
                    let (thread, rseq, kind, body) = match &msg {
                        RunnerMsg::Event { thread, rseq, body, .. } => {
                            (thread.clone(), *rseq, body.kind().to_string(), Some(body.clone()))
                        }
                        RunnerMsg::Exited { thread, rseq, .. } => {
                            (thread.clone(), *rseq, "exited".to_string(), None)
                        }
                        RunnerMsg::CmdResult(r) => { self.state().results.push(r.clone()); continue; }
                        RunnerMsg::Synced => { self.state().synced += 1; continue; }
                        RunnerMsg::Ping => {
                            if sink.send(send(&HubMsg::Pong)).await.is_err() { return; }
                            continue;
                        }
                        _ => continue,
                    };
                    if let Some(n) = drop_after.as_mut() {
                        *n -= 1;
                        if *n == 0 {
                            // Abrupt loss: this frame is neither committed nor acked.
                            self.state().lost = Some((thread, rseq));
                            return;
                        }
                    }
                    let ack = {
                        let mut s = self.state();
                        s.received.push((thread.clone(), rseq));
                        let wm = s.watermarks.get(&thread).copied().unwrap_or(0);
                        if rseq > wm + 1 {
                            // Gap: close; the runner replays.
                            return;
                        }
                        if rseq == wm + 1 {
                            s.watermarks.insert(thread.clone(), rseq);
                            s.committed.push((thread.clone(), rseq, kind));
                            if let Some(b) = body {
                                s.bodies.push((thread.clone(), b));
                            }
                        }
                        !s.manual_acks
                    };
                    if ack && sink.send(send(&HubMsg::Ack { thread, rseq })).await.is_err() {
                        return;
                    }
                }
            }
        }
    }

    fn send(&self, m: HubMsg) {
        self.conn
            .lock()
            .unwrap()
            .as_ref()
            .expect("no runner connection")
            .send(m)
            .unwrap();
    }

    fn cmd(&self, cmd_id: &str, op: Op) {
        self.send(HubMsg::Cmd {
            cmd_id: cmd_id.into(),
            op,
        });
    }

    async fn wait(&self, what: &str, cond: impl Fn(&HubState) -> bool) {
        wait_for(what, || cond(&self.state())).await;
    }

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
    wait_for_within(what, Duration::from_secs(10), cond).await;
}

/// [`wait_for`] with a budget of `within`.
async fn wait_for_within(what: &str, within: Duration, cond: impl Fn() -> bool) {
    let deadline = tokio::time::Instant::now() + within;
    while !cond() {
        if tokio::time::Instant::now() > deadline {
            panic!("timed out waiting for {what}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn opts(hub: &FakeHub, dir: &Path) -> RunnerOptions {
    init_logs();
    let mut o = RunnerOptions::new(hub.url.clone(), Secret::new(TOKEN), BOX, dir);
    o.backoff_min = Duration::from_millis(20);
    o.backoff_max = Duration::from_millis(100);
    o.refused_retry = Duration::from_millis(20);
    o.ping_interval = Duration::from_millis(200);
    o.idle_timeout = Duration::from_secs(5);
    o.welcome_timeout = Duration::from_secs(5);
    o.shutdown_grace = Duration::from_secs(3);
    o
}

/// The built-in adapters with the fake one (production registries leave it out), and no
/// login shell asked or agent CLI probed (`box_options` asks the user's): no real shell or
/// agent CLI runs under a test.
fn with_fake(mut o: LocalBoxOptions) -> LocalBoxOptions {
    o.env = twr_runner::BoxEnv::none();
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

fn open_box(dir: &Path) -> LocalBox {
    init_logs();
    LocalBox::open(with_fake(LocalBoxOptions::new(dir, BOX))).unwrap()
}

struct Running {
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Running {
    async fn shutdown(mut self) {
        let _ = self.stop.take().unwrap().send(());
        tokio::time::timeout(Duration::from_secs(10), &mut self.task)
            .await
            .expect("runner did not stop")
            .unwrap()
            .unwrap();
    }

    /// A crash: the runner task is gone mid-flight, nothing graceful happens.
    async fn crash(self) {
        self.task.abort();
        let _ = self.task.await;
        // Let the aborted connection tasks drop their handles on the box.
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn start(o: RunnerOptions, lbox: LocalBox) -> Running {
    let (tx, rx) = oneshot::channel::<()>();
    let task = tokio::spawn(twr_runner::run_with(o, lbox, async move {
        let _ = rx.await;
    }));
    Running {
        stop: Some(tx),
        task,
    }
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

/// Kinds of the spooled frames of `thread`, in order.
fn spooled(lbox: &LocalBox, thread: &str) -> Vec<(u64, String)> {
    lbox.spool()
        .rows_after(0, 10_000)
        .unwrap()
        .into_iter()
        .filter(|r| r.thread == thread)
        .map(|r| {
            let kind = match serde_json::from_str::<RunnerMsg>(&r.frame).unwrap() {
                RunnerMsg::Event { body, .. } => body.kind().to_string(),
                RunnerMsg::Exited { .. } => "exited".to_string(),
                other => panic!("unexpected spooled frame {other:?}"),
            };
            (r.rseq, kind)
        })
        .collect()
}

fn assert_contiguous(frames: &[(u64, String)], from: u64) {
    let rseqs: Vec<u64> = frames.iter().map(|f| f.0).collect();
    let want: Vec<u64> = (from..from + rseqs.len() as u64).collect();
    assert_eq!(rseqs, want, "{frames:?}");
}

#[tokio::test]
async fn hello_welcome_then_live_events_in_rseq_order() {
    let dir = tempfile::tempdir().unwrap();
    let hub = FakeHub::start().await;
    let lbox = open_box(dir.path());
    let r = start(opts(&hub, dir.path()), lbox.clone());
    hub.wait("welcome + synced", |s| s.synced == 1).await;
    {
        let s = hub.state();
        assert_eq!(s.hellos[0].box_name, BOX);
        assert_eq!(s.hellos[0].protocol, PROTOCOL_VERSION);
        let h = &s.inventories[0];
        assert_eq!(h.version, twr_runner::client::version());
        assert!(h.runtimes.contains(&"fake".to_string()));
        assert!(h.running.is_empty());
        assert_eq!(h.worktrees, dir.path().join("worktrees"));
    }
    hub.cmd("c1", spawn_op("t1", "hello brave new world"));
    hub.wait("turn.ended", |s| {
        s.committed.iter().any(|c| c.2 == "turn.ended")
    })
    .await;
    let frames = hub.committed("t1");
    assert_contiguous(&frames, 1);
    assert_eq!(frames[0].1, "thread.started");
    assert_eq!(frames.last().unwrap().1, "turn.ended");
    // Acked frames leave the spool.
    wait_for("acks applied", || lbox.spool().is_empty().unwrap()).await;
    // A hello after reconnecting reports the live session.
    assert_eq!(lbox.running().len(), 1);
    hub.wait("spawn result", |s| s.results.len() == 1).await;
    assert!(hub.state().results[0].ok);

    // Graceful shutdown stops the runtime and delivers its exit before closing.
    r.shutdown().await;
    let frames = hub.committed("t1");
    assert_eq!(frames.last().unwrap().1, "exited");
    assert_contiguous(&frames, 1);
    assert!(lbox.spool().is_empty().unwrap());
    assert_eq!(lbox.live_count(), 0);
}

/// Something that is not the hub listens at the hub's address (another user's process on a
/// loopback port an SSH tunnel did not get, say). It never learns the token, and it cannot
/// drive the box: the runner sends it nothing but its hello, whether it answers with a
/// challenge it cannot prove (a random proof) or skips the handshake (a welcome and a spawn
/// right away, as an older hub would be impersonated).
#[tokio::test]
async fn a_peer_that_cannot_prove_the_token_gets_nothing_but_the_hello() {
    let dir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("ws://{}", listener.local_addr().unwrap());
    // Every text message each connection got, in order.
    let seen: Arc<Mutex<Vec<Vec<String>>>> = Arc::default();
    let squat = {
        let seen = seen.clone();
        tokio::spawn(async move {
            let mut n = 0;
            while let Ok((tcp, _)) = listener.accept().await {
                n += 1;
                let seen = seen.clone();
                tokio::spawn(async move {
                    let Ok(ws) = tokio_tungstenite::accept_async(tcp).await else {
                        return;
                    };
                    let (mut sink, mut stream) = ws.split();
                    let i = {
                        let mut s = seen.lock().unwrap();
                        s.push(Vec::new());
                        s.len() - 1
                    };
                    let mut answered = false;
                    while let Some(Ok(m)) = stream.next().await {
                        let Message::Text(t) = m else { continue };
                        seen.lock().unwrap()[i].push(t.to_string());
                        if answered {
                            continue;
                        }
                        answered = true;
                        let text =
                            |m: &HubMsg| Message::Text(serde_json::to_string(m).unwrap().into());
                        let replies = if n % 2 == 1 {
                            vec![HubMsg::Challenge {
                                nonce: new_nonce().unwrap(),
                                proof: new_nonce().unwrap(),
                            }]
                        } else {
                            vec![
                                HubMsg::Welcome {
                                    epoch: 1,
                                    watermarks: BTreeMap::new(),
                                },
                                HubMsg::Cmd {
                                    cmd_id: "c-evil".into(),
                                    op: spawn_op("t-evil", "rm -rf ~"),
                                },
                            ]
                        };
                        for r in replies {
                            let _ = sink.send(text(&r)).await;
                        }
                    }
                });
            }
        })
    };
    let lbox = open_box(dir.path());
    init_logs();
    let mut o = RunnerOptions::new(url, Secret::new(TOKEN), BOX, dir.path());
    o.backoff_min = Duration::from_millis(20);
    o.backoff_max = Duration::from_millis(50);
    o.refused_retry = Duration::from_millis(50);
    o.welcome_timeout = Duration::from_secs(5);
    let run = start(o, lbox.clone());
    wait_for("four connections", || seen.lock().unwrap().len() >= 4).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    run.shutdown().await;
    squat.abort();
    let seen = seen.lock().unwrap().clone();
    for msgs in &seen {
        assert!(msgs.len() <= 1, "more than the hello: {msgs:?}");
        for m in msgs {
            assert!(!m.contains(TOKEN), "{m}");
            let RunnerMsg::Hello(h) = serde_json::from_str(m).unwrap() else {
                panic!("not a hello: {m}")
            };
            assert_eq!(h.box_name, BOX);
        }
    }
    assert_eq!(lbox.live_count(), 0, "the squatter's spawn ran");
    assert!(lbox.spool().is_empty().unwrap(), "something was spooled");
    assert!(lbox.in_flight().is_empty());
}

/// One runner per box on a machine: a second box over the same spool, or over the same
/// worktrees from another data directory, is refused while the first is open (it would kill
/// the first one's runtimes as leftovers, and its connection would tell the hub that the first
/// one's repository changes are over). Once the first is gone, the box opens; another box
/// beside it (another spool, the same worktrees) is no conflict.
#[test]
fn a_second_runner_of_a_box_on_the_same_machine_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let first = LocalBox::open(with_fake(LocalBoxOptions::new(dir.path(), BOX))).unwrap();
    let e = LocalBox::open(with_fake(LocalBoxOptions::new(dir.path(), BOX)))
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("another runner of this box is running on this machine"),
        "{e}"
    );
    let mut elsewhere = with_fake(LocalBoxOptions::new(&dir.path().join("other"), BOX));
    elsewhere.worktrees = dir.path().join("worktrees");
    let e = LocalBox::open(elsewhere.clone()).unwrap_err().to_string();
    assert!(e.contains(".twr-runner-gpu.lock"), "{e}");
    let mut other_box = with_fake(LocalBoxOptions::new(dir.path(), "cpu"));
    other_box.worktrees = dir.path().join("worktrees");
    let other = LocalBox::open(other_box).unwrap();
    drop(first);
    let again = LocalBox::open(elsewhere).unwrap();
    drop((again, other));
}

#[tokio::test]
async fn welcome_drops_committed_frames_and_replays_the_rest_in_order() {
    let dir = tempfile::tempdir().unwrap();
    // Events produced while no hub is reachable wait in the spool.
    let lbox = open_box(dir.path());
    let r = lbox.handle_cmd("c1", spawn_op("t1", "one two three")).await;
    assert!(r.ok, "{r:?}");
    let r = lbox.handle_cmd("c2", spawn_op("t2", "four five")).await;
    assert!(r.ok, "{r:?}");
    wait_for("both turns spooled", || {
        ["t1", "t2"]
            .iter()
            .all(|t| spooled(&lbox, t).iter().any(|f| f.1 == "turn.ended"))
    })
    .await;
    let t1 = spooled(&lbox, "t1");
    let t2 = spooled(&lbox, "t2");

    let hub = FakeHub::start().await;
    // The hub already has t1 up to 2.
    hub.state().watermarks.insert("t1".into(), 2);
    let run = start(opts(&hub, dir.path()), lbox.clone());
    hub.wait("replay", |s| s.synced == 1).await;
    {
        let s = hub.state();
        assert_eq!(
            s.inventories[0].acks_needed.get("t1"),
            Some(&(t1.len() as u64))
        );
        assert_eq!(s.inventories[0].running.len(), 2);
        // Nothing at or below the watermark was sent.
        assert!(s.received.iter().all(|(t, rs)| t != "t1" || *rs > 2));
    }
    assert_eq!(hub.committed("t1"), t1[2..].to_vec());
    assert_eq!(hub.committed("t2"), t2);
    wait_for("acks applied", || lbox.spool().is_empty().unwrap()).await;
    run.shutdown().await;
}

#[tokio::test]
async fn acks_delete_spooled_frames() {
    let dir = tempfile::tempdir().unwrap();
    let hub = FakeHub::start().await;
    hub.state().manual_acks = true;
    let lbox = open_box(dir.path());
    let run = start(opts(&hub, dir.path()), lbox.clone());
    hub.wait("synced", |s| s.synced == 1).await;
    hub.cmd("c1", spawn_op("t1", "a b c"));
    hub.wait("turn.ended", |s| {
        s.committed.iter().any(|c| c.2 == "turn.ended")
    })
    .await;
    let n = hub.committed("t1").len() as u64;
    assert_eq!(lbox.spool().len().unwrap(), n);
    hub.send(HubMsg::Ack {
        thread: "t1".into(),
        rseq: 2,
    });
    wait_for("partial ack", || lbox.spool().len().unwrap() == n - 2).await;
    assert_eq!(spooled(&lbox, "t1")[0].0, 3);
    hub.send(HubMsg::Ack {
        thread: "t1".into(),
        rseq: n,
    });
    wait_for("full ack", || lbox.spool().is_empty().unwrap()).await;
    assert_eq!(lbox.spool().bytes(), 0);
    hub.state().manual_acks = false;
    run.shutdown().await;
}

#[tokio::test]
async fn a_repeated_command_id_runs_once() {
    let dir = tempfile::tempdir().unwrap();
    let hub = FakeHub::start().await;
    let lbox = open_box(dir.path());
    let run = start(opts(&hub, dir.path()), lbox.clone());
    hub.wait("synced", |s| s.synced == 1).await;
    hub.cmd("c1", spawn_op("t1", "hi"));
    hub.cmd("c1", spawn_op("t1", "hi"));
    hub.wait("two results", |s| s.results.len() == 2).await;
    hub.wait("turn.ended", |s| {
        s.committed.iter().any(|c| c.2 == "turn.ended")
    })
    .await;
    {
        let s = hub.state();
        assert_eq!(s.results[0], s.results[1]);
        assert!(s.results[0].ok);
    }
    let started = hub
        .committed("t1")
        .iter()
        .filter(|f| f.1 == "thread.started")
        .count();
    assert_eq!(started, 1);
    // A new id for the same thread is a new command: the session is already running.
    hub.cmd("c2", spawn_op("t1", "hi"));
    hub.wait("third result", |s| s.results.len() == 3).await;
    let e = hub.state().results[2].error.clone().unwrap();
    assert_eq!(e.code, cmd_errors::ALREADY_RUNNING);
    // Session ops on a thread without a session.
    hub.cmd(
        "c3",
        Op::Send {
            thread: "nope".into(),
            turn: 1,
            text: "x".into(),
            run: None,
        },
    );
    hub.cmd(
        "c4",
        Op::Stop {
            thread: "nope".into(),
            run: None,
        },
    );
    hub.cmd(
        "c5",
        Op::Spawn {
            thread: "t9".into(),
            runtime: "no-such-runtime".into(),
            cfg: WireSpawnConfig::default(),
            prompt: None,
            run: None,
        },
    );
    hub.wait("more results", |s| s.results.len() == 6).await;
    {
        let s = hub.state();
        let by_id = |id: &str| s.results.iter().find(|r| r.cmd_id == id).unwrap().clone();
        assert_eq!(by_id("c3").error.unwrap().code, cmd_errors::NOT_RUNNING);
        assert!(by_id("c4").ok);
        assert_eq!(by_id("c5").error.unwrap().code, cmd_errors::UNKNOWN_RUNTIME);
    }
    // The dedup survives a reconnect and a restart.
    run.shutdown().await;
    drop(lbox);
    let lbox = open_box(dir.path());
    let r = lbox.handle_cmd("c1", spawn_op("t1", "hi")).await;
    assert!(r.ok);
    assert_eq!(lbox.live_count(), 0, "c1 must not spawn again");
}

#[tokio::test]
async fn a_lost_connection_replays_unacked_frames_without_gaps_or_duplicates() {
    let dir = tempfile::tempdir().unwrap();
    let hub = FakeHub::start().await;
    let lbox = open_box(dir.path());
    let run = start(opts(&hub, dir.path()), lbox.clone());
    hub.wait("synced", |s| s.synced == 1).await;
    hub.cmd(
        "c1",
        spawn_op("t1", "!sleep 1000\nalpha beta gamma delta epsilon zeta"),
    );
    // The command must have run before its connection is killed: a command lost with its
    // connection is the hub's to send again (this fake hub never does), and the runner can
    // notice the dead connection on a write (its ping) before its reader has read a command
    // that arrived just before the close, which aborts that reader. (The race behind this
    // test's rare timeout under load.) The turn's text comes a second later, after the kill.
    hub.wait("the spawn ran", |s| s.results.len() == 1).await;
    // Kill the current connection mid-stream: close it from the hub side now; the next
    // connection drops after 3 frames (the 3rd is lost in flight).
    hub.state().drop_after = Some(3);
    *hub.conn.lock().unwrap() = None; // drops the sender: the hub side closes
    hub.wait("reconnect after the drop", |s| s.connections >= 3)
        .await;
    hub.wait("turn.ended", |s| {
        s.committed.iter().any(|c| c.2 == "turn.ended")
    })
    .await;
    let frames = hub.committed("t1");
    assert_contiguous(&frames, 1);
    assert_eq!(frames[0].1, "thread.started");
    // The frame lost with the connection was sent again and committed once.
    let lost = hub
        .state()
        .lost
        .clone()
        .expect("a frame was lost in flight");
    assert!(frames.iter().any(|f| f.0 == lost.1), "{lost:?} {frames:?}");
    wait_for("acks applied", || lbox.spool().is_empty().unwrap()).await;
    run.shutdown().await;
}

#[tokio::test]
async fn a_restarted_runner_replays_its_spool_and_continues_rseq() {
    let dir = tempfile::tempdir().unwrap();
    let hub = FakeHub::start().await;
    hub.state().manual_acks = true;
    let lbox = open_box(dir.path());
    let run = start(opts(&hub, dir.path()), lbox.clone());
    hub.wait("synced", |s| s.synced == 1).await;
    hub.cmd("c1", spawn_op("t1", "one two"));
    hub.wait("turn.ended", |s| {
        s.committed.iter().any(|c| c.2 == "turn.ended")
    })
    .await;
    let first = hub.committed("t1");
    // The runner crashes before any ack: everything is still spooled.
    run.crash().await;
    drop(lbox);
    // The hub lost its commits too (worst case): it has nothing for t1.
    {
        let mut s = hub.state();
        s.watermarks.clear();
        s.committed.clear();
        s.manual_acks = false;
    }
    let lbox = open_box(dir.path());
    // The session died with the runner (its runtime never reported a process, so only its
    // start intent was left): the new runner records its exit.
    let mut first = first;
    first.push((first.len() as u64 + 1, "exited".to_string()));
    assert_eq!(spooled(&lbox, "t1"), first);
    assert_eq!(
        lbox.spool().next_rseq("t1").unwrap(),
        first.len() as u64 + 1
    );
    let run = start(opts(&hub, dir.path()), lbox.clone());
    hub.wait("replay", |s| s.committed.len() == first.len())
        .await;
    assert_eq!(hub.committed("t1"), first);
    wait_for("acks applied", || lbox.spool().is_empty().unwrap()).await;
    // The crashed session is gone: the hub starts a new one, whose frames continue the
    // thread's sequence.
    assert!(lbox.running().is_empty());
    hub.cmd("c2", spawn_op("t1", "three"));
    hub.wait("second turn", |s| {
        s.committed.iter().filter(|c| c.2 == "turn.ended").count() == 2
    })
    .await;
    let all = hub.committed("t1");
    assert_contiguous(&all, 1);
    assert!(all.len() > first.len());
    run.shutdown().await;
    // And again after a clean restart.
    drop(lbox);
    let lbox = open_box(dir.path());
    assert_eq!(
        lbox.spool().next_rseq("t1").unwrap(),
        hub.committed("t1").len() as u64 + 1
    );
}

/// A test's box asks no login shell and probes no agent CLI, though a runner's own box
/// options ask the user's.
#[test]
fn a_tests_box_asks_nothing_of_the_users_environment() {
    let o = RunnerOptions::new("ws://127.0.0.1:1/", Secret::new(TOKEN), BOX, "/nonexistent");
    assert_eq!(o.box_options().env, twr_runner::BoxEnv::user());
    assert_eq!(with_fake(o.box_options()).env, twr_runner::BoxEnv::none());
}

#[tokio::test]
async fn the_spool_is_private_and_the_token_never_logged() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let hub = FakeHub::start().await;
    let o = opts(&hub, dir.path());
    o.validate().unwrap();
    let lbox = LocalBox::open(with_fake(o.box_options())).unwrap();
    let mode = std::fs::metadata(dir.path().join(format!("runner-{BOX}.db")))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    let run = start(o.clone(), lbox.clone());
    hub.wait("synced", |s| s.synced == 1).await;
    hub.cmd("c1", spawn_op("t1", "hi"));
    hub.wait("turn.ended", |s| {
        s.committed.iter().any(|c| c.2 == "turn.ended")
    })
    .await;
    run.shutdown().await;

    // With a wrong token the runner does not accept the hub's proof: it sends nothing after
    // its hello, tries again no sooner than `refused_retry` (however short its backoff), and
    // logs neither token.
    let mut bad = o.clone();
    bad.token = Secret::new("wrong-token-7c9d");
    bad.refused_retry = Duration::from_millis(400);
    let connections = hub.state().connections;
    let before = hub.state().unanswered_at.len();
    let run = start(bad, lbox.clone());
    hub.wait("refused hellos", |s| s.unanswered_at.len() >= before + 3)
        .await;
    run.shutdown().await;
    // Each retry starts after the previous connection ended and the wait passed, so the ends
    // of successive refused connections are at least `refused_retry` apart.
    let ends = hub.state().unanswered_at[before..].to_vec();
    for w in ends.windows(2) {
        let gap = w[1] - w[0];
        assert!(gap >= Duration::from_millis(400), "{gap:?}");
    }
    assert_eq!(hub.state().connections, connections);

    // Every test in this file logs into the same capture (at TRACE).
    let logs = String::from_utf8(LOGS.lock().unwrap().clone()).unwrap();
    assert!(logs.contains("connected to the hub"), "{logs}");
    assert!(
        logs.contains("could not prove that it knows this box's token"),
        "{logs}"
    );
    assert!(!logs.contains(TOKEN), "{logs}");
    assert!(!logs.contains("wrong-token-7c9d"), "{logs}");
}

#[tokio::test]
async fn an_overflowing_spool_stops_the_runtime_with_a_synthetic_exit() {
    let dir = tempfile::tempdir().unwrap();
    let mut o = with_fake(LocalBoxOptions::new(dir.path(), BOX));
    o.spool_max_bytes = 1500;
    let lbox = LocalBox::open(o).unwrap();
    let r = lbox
        .handle_cmd("c1", spawn_op("t1", &"word ".repeat(200)))
        .await;
    assert!(r.ok);
    wait_for("runtime stopped", || lbox.live_count() == 0).await;
    let frames = spooled(&lbox, "t1");
    assert_contiguous(&frames, 1);
    assert_eq!(frames.last().unwrap().1, "exited");
    assert_eq!(
        frames.iter().filter(|f| f.1 == "exited").count(),
        1,
        "{frames:?}"
    );
    let last = lbox.spool().rows_after(0, 1000).unwrap().pop().unwrap();
    let RunnerMsg::Exited {
        code, stderr_tail, ..
    } = serde_json::from_str(&last.frame).unwrap()
    else {
        panic!("not an exit")
    };
    assert_eq!(code, None);
    assert!(stderr_tail.contains("spool full"), "{stderr_tail}");
    // The bound is respected except for the forced exit.
    assert!(lbox.spool().bytes() <= 1500 + last.frame.len() as u64);
    // New sessions still work once there is room (here: the spool is still full, so the
    // next session is stopped at its first event).
    let r = lbox.handle_cmd("c2", spawn_op("t2", "x")).await;
    assert!(r.ok);
    wait_for("second runtime stopped", || lbox.live_count() == 0).await;
    let t2 = spooled(&lbox, "t2");
    assert_eq!(t2, vec![(1, "exited".to_string())]);
    assert!(matches!(
        lbox.spool().append("t3", true, |_| "{}".into()).unwrap(),
        Appended::Stored { rseq: 1 }
    ));
}

#[tokio::test]
async fn git_and_attach_ops_run_on_the_box() {
    use twr_core::runner::{decode_result, AttachInfo, WireRemoveOutcome, WireWorktreeCreated};
    let dir = tempfile::tempdir().unwrap();
    let lbox = open_box(&dir.path().join("data"));
    let repo = twr_core::testkit::git::init_repo(&dir.path().join("repo"));
    let wt = dir.path().join("data/worktrees/p/t1");
    let ok = |r: CmdResult| r.into_outcome().unwrap();
    let root = ok(lbox
        .handle_cmd("g1", Op::RepoRoot { dir: repo.clone() })
        .await);
    assert_eq!(root, json!(repo));
    let made: WireWorktreeCreated = decode_result(ok(lbox
        .handle_cmd(
            "g2",
            Op::WorktreeCreate {
                repo: repo.clone(),
                path: wt.clone(),
                branch: "twr/t1".into(),
                base: None,
            },
        )
        .await))
    .unwrap();
    assert!(made.new_branch);
    assert_eq!(
        ok(lbox
            .handle_cmd("g3", Op::WorktreeExists { path: wt.clone() })
            .await),
        json!(true)
    );
    std::fs::write(wt.join("x.txt"), "x").unwrap();
    let snap: twr_core::event::GitSnapshot = decode_result(ok(lbox
        .handle_cmd(
            "g4",
            Op::GitSnapshot {
                path: wt.clone(),
                base: None,
            },
        )
        .await))
    .unwrap();
    assert_eq!(snap.dirty_files, 1);
    let rm: WireRemoveOutcome = decode_result(ok(lbox
        .handle_cmd(
            "g5",
            Op::WorktreeRemove {
                repo: repo.clone(),
                path: wt.clone(),
                branch: Some("twr/t1".into()),
                disposable: None,
            },
        )
        .await))
    .unwrap();
    assert_eq!(rm, WireRemoveOutcome::KeptDirty { dirty_files: 1 });
    let e = lbox
        .handle_cmd(
            "g6",
            Op::GitSnapshot {
                path: dir.path().join("nowhere"),
                base: None,
            },
        )
        .await;
    assert_eq!(e.error.unwrap().code, cmd_errors::FAILED);

    let info: AttachInfo = decode_result(ok(lbox
        .handle_cmd(
            "a1",
            Op::AttachInfo {
                thread: "t1".into(),
                runtime: "fake".into(),
                cfg: WireSpawnConfig {
                    cwd: wt.clone(),
                    ..Default::default()
                },
                session_id: "sess".into(),
            },
        )
        .await))
    .unwrap();
    assert_eq!(info.program, "sh");
    assert!(info.args.contains(&"sess".to_string()), "{info:?}");

    // An `acp` preset's attach template survives the op (the adapter has no command without
    // it); nothing else the hub sent reaches the adapter.
    let env = |runtime: &str| WireSpawnConfig {
        cwd: wt.clone(),
        env: vec![
            (
                twr_adapters::acp::ATTACH_ENV.into(),
                r#"["stub-cli","--resume","{session}"]"#.into(),
            ),
            (twr_adapters::acp::COMMAND_ENV.into(), r#"["agent"]"#.into()),
            ("TWR_HUB_ONLY_SECRET".into(), "s3cr3t".into()),
            ("TWR_THREAD".into(), format!("{runtime}-thread")),
        ],
        ..Default::default()
    };
    let info: Option<AttachInfo> = decode_result(ok(lbox
        .handle_cmd(
            "a2",
            Op::AttachInfo {
                thread: "t1".into(),
                runtime: twr_adapters::acp::NAME.into(),
                cfg: env("acp"),
                session_id: "sess".into(),
            },
        )
        .await))
    .unwrap();
    let info = info.expect("the preset's attach template reached the adapter");
    assert_eq!(
        (info.program.as_str(), info.args.as_slice()),
        (
            "stub-cli",
            &["--resume".to_string(), "sess".to_string()][..]
        )
    );
    // An adapter that puts the config's environment on its attach command gets none of it.
    let info: AttachInfo = decode_result(ok(lbox
        .handle_cmd(
            "a3",
            Op::AttachInfo {
                thread: "t1".into(),
                runtime: twr_adapters::grok_build::NAME.into(),
                cfg: env("grok"),
                session_id: "sess".into(),
            },
        )
        .await))
    .unwrap();
    let keys: Vec<&str> = info.env.iter().map(|(k, _)| k.as_str()).collect();
    assert!(
        !keys.contains(&"TWR_HUB_ONLY_SECRET")
            && !keys.contains(&"TWR_THREAD")
            && !keys.contains(&twr_adapters::acp::COMMAND_ENV),
        "{keys:?}"
    );
}

#[tokio::test]
async fn permission_round_trip_through_commands() {
    let dir = tempfile::tempdir().unwrap();
    let hub = FakeHub::start().await;
    let lbox = open_box(dir.path());
    let run = start(opts(&hub, dir.path()), lbox.clone());
    hub.wait("synced", |s| s.synced == 1).await;
    hub.cmd("c1", spawn_op("t1", "!perm Bash rm -rf build"));
    hub.wait("attention", |s| {
        s.committed.iter().any(|c| c.2 == "attention.raised")
    })
    .await;
    let attn_id = hub
        .state()
        .bodies
        .iter()
        .find_map(|(_, b)| match b {
            EventBody::AttentionRaised(a) => Some(a.attn_id.clone()),
            _ => None,
        })
        .unwrap();
    // An answer bound to another run (the one that raised the item has ended; a later run
    // numbers its requests anew) never reaches this run's runtime.
    hub.cmd(
        "c2",
        Op::Respond {
            thread: "t1".into(),
            attn_id: attn_id.clone(),
            answer: twr_core::event::Answer::allow(),
            run: Some("an-earlier-run".into()),
        },
    );
    hub.wait("the refused answer", |s| {
        s.results.iter().any(|r| r.cmd_id == "c2")
    })
    .await;
    let r = hub
        .state()
        .results
        .iter()
        .find(|r| r.cmd_id == "c2")
        .cloned()
        .unwrap();
    assert_eq!(r.error.unwrap().code, cmd_errors::NOT_RUNNING);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !hub.state()
            .committed
            .iter()
            .any(|c| c.2 == "agent.tool_result" || c.2 == "turn.ended"),
        "the runtime got an answer meant for another run"
    );
    let run_id = lbox.running()[0].run.clone();
    assert!(run_id.is_some());
    hub.cmd(
        "c3",
        Op::Respond {
            thread: "t1".into(),
            attn_id,
            answer: twr_core::event::Answer::allow(),
            run: run_id,
        },
    );
    hub.wait("turn.ended", |s| {
        s.committed.iter().any(|c| c.2 == "turn.ended")
    })
    .await;
    run.shutdown().await;
}

/// A `spawn` naming its run (the hub's) runs under that id; an id that cannot be one is
/// refused before anything starts.
#[tokio::test]
async fn a_spawn_runs_under_the_run_the_hub_names() {
    let dir = tempfile::tempdir().unwrap();
    let lbox = open_box(dir.path());
    let with_run = |run: &str| match spawn_op("t1", "!sleep 60000") {
        Op::Spawn {
            thread,
            runtime,
            cfg,
            prompt,
            ..
        } => Op::Spawn {
            thread,
            runtime,
            cfg,
            prompt,
            run: Some(run.into()),
        },
        _ => unreachable!(),
    };
    let r = lbox.handle_cmd("c0", with_run("bad run/id")).await;
    assert_eq!(r.error.unwrap().code, cmd_errors::FAILED);
    assert!(lbox.running().is_empty());
    let r = lbox.handle_cmd("c1", with_run("01HUBRUN")).await;
    assert!(r.ok, "{r:?}");
    assert_eq!(lbox.running()[0].run.as_deref(), Some("01HUBRUN"));
    lbox.shutdown(Duration::from_secs(5)).await;
}

/// A prompt bound to a run (the hub's session when it sent it) never reaches another run of
/// the thread: `not_running`, and the runtime gets nothing.
#[tokio::test]
async fn a_send_bound_to_another_run_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let hub = FakeHub::start().await;
    let lbox = open_box(dir.path());
    let run = start(opts(&hub, dir.path()), lbox.clone());
    hub.wait("synced", |s| s.synced == 1).await;
    hub.cmd("c1", spawn_op("t1", "first"));
    hub.wait("turn 1 ended", |s| {
        s.committed.iter().any(|c| c.2 == "turn.ended")
    })
    .await;
    hub.cmd(
        "c2",
        Op::Send {
            thread: "t1".into(),
            turn: 2,
            text: "second".into(),
            run: Some("an-earlier-run".into()),
        },
    );
    hub.wait("the refused prompt", |s| {
        s.results.iter().any(|r| r.cmd_id == "c2")
    })
    .await;
    let r = hub
        .state()
        .results
        .iter()
        .find(|r| r.cmd_id == "c2")
        .cloned()
        .unwrap();
    assert_eq!(r.error.unwrap().code, cmd_errors::NOT_RUNNING);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let ended = |s: &HubState| s.committed.iter().filter(|c| c.2 == "turn.ended").count();
    assert_eq!(
        ended(&hub.state()),
        1,
        "the runtime got a prompt meant for another run"
    );
    let run_id = lbox.running()[0].run.clone();
    assert!(run_id.is_some());
    hub.cmd(
        "c3",
        Op::Send {
            thread: "t1".into(),
            turn: 2,
            text: "second".into(),
            run: run_id,
        },
    );
    hub.wait("turn 2 ended", |s| ended(s) == 2).await;
    run.shutdown().await;
}

#[tokio::test]
async fn a_failed_spool_write_is_retried_before_the_next_event() {
    let dir = tempfile::tempdir().unwrap();
    let lbox = open_box(dir.path());
    // The first two appends fail: the same event is retried, nothing is skipped.
    lbox.spool().inject_failures(2, 0);
    let r = lbox
        .handle_cmd("c1", spawn_op("t1", "alpha beta gamma"))
        .await;
    assert!(r.ok, "{r:?}");
    wait_for("turn spooled", || {
        spooled(&lbox, "t1").iter().any(|f| f.1 == "turn.ended")
    })
    .await;
    let frames = spooled(&lbox, "t1");
    assert_contiguous(&frames, 1);
    let kinds: Vec<&str> = frames.iter().map(|f| f.1.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "thread.started",
            "agent.text",
            "agent.text",
            "agent.text",
            "agent.text",
            "turn.ended"
        ]
    );
    assert!(lbox.fatal_error().is_none());
}

/// A session's start event says what it was started with, including an `acp` agent's argv
/// (`ext.twr.acp_command`, from the spawn's `TWR_ACP_COMMAND`): a hub that takes the session
/// over after the thread moved to another agent knows it for the previous one.
#[tokio::test]
async fn a_start_event_is_stamped_with_its_settings_and_agent() {
    let dir = tempfile::tempdir().unwrap();
    let lbox = open_box(dir.path());
    let twr_of_start = |thread: &str| {
        lbox.spool()
            .rows_after(0, 10_000)
            .unwrap()
            .into_iter()
            .filter(|r| r.thread == thread)
            .find_map(
                |r| match serde_json::from_str::<RunnerMsg>(&r.frame).unwrap() {
                    RunnerMsg::Event {
                        body: EventBody::ThreadStarted(p),
                        ..
                    } => Some(p.ext.unwrap()["twr"].clone()),
                    _ => None,
                },
            )
    };
    let mut op = spawn_op("t1", "hi");
    if let Op::Spawn { cfg, .. } = &mut op {
        cfg.mode = "plan".into();
        cfg.env.push((
            twr_adapters::acp::COMMAND_ENV.into(),
            r#"["agent","--acp"]"#.into(),
        ));
    }
    assert!(lbox.handle_cmd("c1", op).await.ok);
    assert!(lbox.handle_cmd("c2", spawn_op("t2", "hi")).await.ok);
    wait_for("both starts spooled", || {
        twr_of_start("t1").is_some() && twr_of_start("t2").is_some()
    })
    .await;
    let twr = twr_of_start("t1").unwrap();
    assert_eq!(
        (&twr["runtime"], &twr["mode"], &twr["acp_command"]),
        (&json!("fake"), &json!("plan"), &json!(["agent", "--acp"])),
        "{twr}"
    );
    assert!(twr["model"].is_null() && twr["effort"].is_null(), "{twr}");
    // No agent, no key.
    let twr = twr_of_start("t2").unwrap();
    assert!(twr.get("acp_command").is_none(), "{twr}");
}

#[tokio::test]
async fn a_spool_that_stays_unwritable_stops_the_runtime_and_fails_the_runner() {
    let dir = tempfile::tempdir().unwrap();
    let lbox = open_box(dir.path());
    let r = lbox
        .handle_cmd("c1", spawn_op("t1", "!sleep 400\nalpha beta"))
        .await;
    assert!(r.ok, "{r:?}");
    wait_for("thread.started spooled", || spooled(&lbox, "t1").len() == 1).await;
    // Every later write fails: the text is never skipped over; the runtime is stopped and
    // nothing more of the session is recorded (no later event, no exit).
    lbox.spool().inject_failures(1000, 0);
    wait_for("the box failed", || lbox.fatal_error().is_some()).await;
    wait_for("runtime stopped", || lbox.live_count() == 0).await;
    lbox.spool().inject_failures(0, 0);
    let frames = spooled(&lbox, "t1");
    assert_eq!(frames, vec![(1, "thread.started".to_string())]);
    let e = lbox.fatal_error().unwrap();
    assert!(e.contains("agent.text") && e.contains("injected"), "{e}");
    // A failed box ends the runner with its error.
    let hub = FakeHub::start().await;
    let r = tokio::time::timeout(
        Duration::from_secs(10),
        twr_runner::run_with(opts(&hub, dir.path()), lbox.clone(), std::future::pending()),
    )
    .await
    .expect("the runner did not stop");
    assert!(
        r.as_ref().is_err_and(|e| e.to_string().contains("failed")),
        "{r:?}"
    );
}

#[tokio::test]
async fn a_command_result_that_cannot_be_stored_is_never_run_twice() {
    let dir = tempfile::tempdir().unwrap();
    let lbox = open_box(dir.path());
    let started = |lbox: &LocalBox, t: &str| {
        spooled(lbox, t)
            .iter()
            .filter(|f| f.1 == "thread.started")
            .count()
    };
    // Two failed writes of the result: retried, then published.
    lbox.spool().inject_failures(0, 2);
    let r = lbox.handle_cmd("c1", spawn_op("t1", "hi")).await;
    assert!(r.ok, "{r:?}");
    let again = lbox.handle_cmd("c1", spawn_op("t1", "hi")).await;
    assert_eq!(again.into_outcome().unwrap(), r.into_outcome().unwrap());
    wait_for("t1 started", || started(&lbox, "t1") == 1).await;
    assert!(lbox.fatal_error().is_none());

    // The result can never be stored: the command answers `interrupted` (its effect is
    // unknown to the hub), the box fails, and a repeat of the id does not run it again.
    lbox.spool().inject_failures(0, 1000);
    let r = lbox.handle_cmd("c2", spawn_op("t2", "hi")).await;
    lbox.spool().inject_failures(0, 0);
    assert_eq!(r.error.unwrap().code, cmd_errors::INTERRUPTED);
    assert!(lbox.fatal_error().is_some());
    wait_for("t2 started", || started(&lbox, "t2") == 1).await;
    let again = lbox.handle_cmd("c2", spawn_op("t2", "hi")).await;
    assert_eq!(again.error.unwrap().code, cmd_errors::INTERRUPTED);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(started(&lbox, "t2"), 1, "c2 ran twice");
    // Not after a restart either.
    lbox.shutdown(Duration::from_secs(3)).await;
    drop(lbox);
    let lbox = open_box(dir.path());
    let again = lbox.handle_cmd("c2", spawn_op("t2", "hi")).await;
    assert_eq!(again.error.unwrap().code, cmd_errors::INTERRUPTED);
    assert_eq!(lbox.live_count(), 0);
}

/// A runtime with a real process (`sleep 300` in its own process group, marked with
/// `TWR_THREAD` by the box) that reports its pid like the Claude Code adapter does. The slow
/// one never finishes initializing: it never reports its process (it writes its pid to
/// `<cwd>/slow.pid` for the test instead).
struct Sleeper {
    slow: bool,
}

impl twr_core::adapter::Adapter for Sleeper {
    fn info(&self) -> twr_core::adapter::AdapterInfo {
        twr_core::adapter::AdapterInfo {
            name: if self.slow { "slow-sleeper" } else { "sleeper" },
            tier: twr_core::adapter::Tier::Headless,
            description: "a real process for runner crash tests",
        }
    }

    fn spawn(
        &self,
        cfg: twr_core::adapter::SpawnConfig,
        _prompt: Option<String>,
    ) -> anyhow::Result<twr_core::adapter::AdapterHandle> {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("300").process_group(0).envs(cfg.env.clone());
        let mut child = cmd.spawn()?;
        let pid = child.id().unwrap();
        let (h, mut end) = twr_core::adapter::AdapterHandle::pair();
        if self.slow {
            let pidfile = cfg.cwd.join("slow.pid");
            std::fs::write(pidfile.with_extension("tmp"), pid.to_string())?;
            std::fs::rename(pidfile.with_extension("tmp"), pidfile)?;
            tokio::spawn(async move {
                let _ = end.cmds.recv().await;
                let _ = child.kill().await;
                end.exited(None, "stopped");
            });
            return Ok(h);
        }
        tokio::spawn(async move {
            // Wait for the exec, so the process table shows `sleep` and its environment.
            for _ in 0..500 {
                let cmdline = twr_runner::reap::process_cmdline(pid).unwrap_or_default();
                let env = twr_runner::reap::process_environ(pid).unwrap_or_default();
                if cmdline.starts_with(b"sleep") && !env.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            end.emit(
                EventBody::ThreadStarted(twr_core::event::SessionPayload {
                    session_id: Some("s-sleeper".into()),
                    ext: serde_json::from_value(json!({"twr": {"pid": pid, "exe": "sleep"}}))
                        .unwrap(),
                    ..Default::default()
                }),
                None,
            );
            loop {
                tokio::select! {
                    c = end.cmds.recv() => {
                        if matches!(c, None | Some(twr_core::adapter::AdapterCmd::Stop)) {
                            let _ = child.kill().await;
                            end.exited(None, "stopped");
                            return;
                        }
                    }
                    st = child.wait() => {
                        end.exited(st.ok().and_then(|s| s.code()), "");
                        return;
                    }
                }
            }
        });
        Ok(h)
    }
}

fn sleeper_box(dir: &Path) -> LocalBox {
    init_logs();
    let mut o = LocalBoxOptions::new(dir, BOX);
    let sleeper: Arc<dyn twr_core::adapter::Adapter> = Arc::new(Sleeper { slow: false });
    let slow: Arc<dyn twr_core::adapter::Adapter> = Arc::new(Sleeper { slow: true });
    o.adapters = Arc::new(move |n: &str| match n {
        "sleeper" => Some(Arc::clone(&sleeper)),
        "slow-sleeper" => Some(Arc::clone(&slow)),
        _ => None,
    });
    o.runtimes = vec!["sleeper".into(), "slow-sleeper".into()];
    LocalBox::open(o).unwrap()
}

const CHILD_ENV: &str = "TWR_TEST_RUNNER_CHILD";

/// Which runtime the child runner starts (`sleeper` when unset).
const CHILD_RUNTIME_ENV: &str = "TWR_TEST_RUNNER_CHILD_RUNTIME";

/// The thread the child runner starts a session of (`t1` when unset).
const CHILD_THREAD_ENV: &str = "TWR_TEST_RUNNER_CHILD_THREAD";

/// The runner process of the killed-runner tests (does nothing unless started by one): starts
/// a session of `$TWR_TEST_RUNNER_CHILD_RUNTIME`, says so (with its pid) in `<dir>/ready` once
/// the runtime runs (`sleeper`: once it is recorded; `slow-sleeper`: once its process exists,
/// still initializing), then waits to be killed.
#[tokio::test]
async fn runner_child_process() {
    let Ok(dir) = std::env::var(CHILD_ENV) else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let runtime = std::env::var(CHILD_RUNTIME_ENV).unwrap_or_else(|_| "sleeper".into());
    let thread = std::env::var(CHILD_THREAD_ENV).unwrap_or_else(|_| "t1".into());
    let lbox = sleeper_box(&dir);
    let r = lbox
        .handle_cmd(
            "c1",
            Op::Spawn {
                thread,
                runtime: runtime.clone(),
                cfg: WireSpawnConfig {
                    cwd: dir.clone(),
                    ..Default::default()
                },
                prompt: None,
                run: None,
            },
        )
        .await;
    assert!(r.ok, "{r:?}");
    let pid = if runtime == "slow-sleeper" {
        wait_for("the runtime's process", || dir.join("slow.pid").exists()).await;
        assert!(lbox.spool().runtimes().unwrap().is_empty());
        assert_eq!(lbox.spool().intents().unwrap().len(), 1);
        std::fs::read_to_string(dir.join("slow.pid"))
            .unwrap()
            .parse()
            .unwrap()
    } else {
        wait_for("runtime recorded", || {
            lbox.spool().runtimes().unwrap().len() == 1
        })
        .await;
        lbox.spool().runtimes().unwrap()[0].pid
    };
    std::fs::write(dir.join("ready.tmp"), pid.to_string()).unwrap();
    std::fs::rename(dir.join("ready.tmp"), dir.join("ready")).unwrap();
    std::future::pending::<()>().await;
}

/// Starts this test binary as a runner process running only `runner_child_process` with a
/// session of `runtime` for `thread`, and waits until it is ready. Returns it and its
/// runtime's pid.
fn child_runner(dir: &Path, runtime: &str, thread: &str) -> (std::process::Child, u32) {
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "runner_child_process",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_ENV, dir)
        .env(CHILD_RUNTIME_ENV, runtime)
        .env(CHILD_THREAD_ENV, thread)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let ready = dir.join("ready");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !ready.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the child runner never started its runtime"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let pid = std::fs::read_to_string(&ready).unwrap().parse().unwrap();
    (child, pid)
}

#[tokio::test]
async fn a_runner_killed_while_its_runtime_initializes_leaves_nothing_behind() {
    let _one = RECOVERY.lock().await;
    let dir = tempfile::tempdir().unwrap();
    // Recovery finds the runtime by its thread marker alone, machine-wide: a thread id no
    // other test (or concurrent test run) uses, as real thread ids are.
    let thread = format!("t-slow-{}", twr_core::event::new_id());
    let (mut child, pid) = child_runner(dir.path(), "slow-sleeper", &thread);
    // Wait for the exec, so the process table shows `sleep` with its marker.
    wait_for("the runtime's exec", || {
        twr_runner::reap::process_environ(pid)
            .is_ok_and(|e| twr_runner::reap::environ_has(&e, twr_runner::reap::THREAD_ENV, &thread))
    })
    .await;
    let start = twr_runner::reap::process_start_time(pid).expect("the runtime runs");
    // SIGKILL before the runtime reported its process: only the start intent knows of it.
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(twr_runner::reap::process_start_time(pid), Some(start));

    let lbox = sleeper_box(dir.path());
    assert!(
        twr_runner::reap::process_start_time(pid) != Some(start),
        "the orphaned runtime still runs"
    );
    assert!(lbox.spool().intents().unwrap().is_empty());
    assert!(lbox.spool().runtimes().unwrap().is_empty());
    assert_eq!(spooled(&lbox, &thread), vec![(1, "exited".to_string())]);
    let last = lbox.spool().rows_after(0, 10).unwrap().pop().unwrap();
    let RunnerMsg::Exited { stderr_tail, .. } = serde_json::from_str(&last.frame).unwrap() else {
        panic!("not an exit")
    };
    assert!(stderr_tail.contains("were killed"), "{stderr_tail}");
}

#[tokio::test]
async fn a_stop_for_another_run_leaves_the_session_alone() {
    let dir = tempfile::tempdir().unwrap();
    let lbox = sleeper_box(dir.path());
    let spawn = |id: &str| {
        let lbox = lbox.clone();
        let (id, cwd) = (id.to_string(), dir.path().to_path_buf());
        async move {
            let r = lbox
                .handle_cmd(
                    &id,
                    Op::Spawn {
                        thread: "t1".into(),
                        runtime: "sleeper".into(),
                        cfg: WireSpawnConfig {
                            cwd,
                            ..Default::default()
                        },
                        prompt: None,
                        run: None,
                    },
                )
                .await;
            assert!(r.ok, "{r:?}");
        }
    };
    spawn("c1").await;
    let first = lbox.running()[0].run.clone().expect("a run id");
    let stop = |id: &'static str, run: Option<String>| {
        lbox.handle_cmd(
            id,
            Op::Stop {
                thread: "t1".into(),
                run,
            },
        )
    };
    assert!(stop("c2", Some(first.clone())).await.ok);
    wait_for("the first run ended", || lbox.live_count() == 0).await;
    // A replacement run; the stop meant for the first one comes late: ignored.
    spawn("c3").await;
    let second = lbox.running()[0].run.clone().expect("a run id");
    assert_ne!(first, second);
    assert!(stop("c4", Some(first)).await.ok);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(lbox.live_count(), 1, "the replacement was stopped");
    // Its frames carry its run.
    let frames = lbox.spool().rows_after(0, 100).unwrap();
    let runs: Vec<Option<String>> = frames
        .iter()
        .map(|r| match serde_json::from_str(&r.frame).unwrap() {
            RunnerMsg::Event { run, .. } | RunnerMsg::Exited { run, .. } => run,
            _ => None,
        })
        .collect();
    assert!(runs.iter().all(Option::is_some), "{runs:?}");
    assert!(stop("c5", Some(second)).await.ok);
    wait_for("the second run ended", || lbox.live_count() == 0).await;
}

#[tokio::test]
async fn a_killed_runner_leaves_no_runtime_behind() {
    let _one = RECOVERY.lock().await;
    let dir = tempfile::tempdir().unwrap();
    // A real runner process (this test binary, running only `runner_child_process`).
    // Recovery finds the runtime by its thread marker alone, machine-wide: a thread id no
    // other test (or concurrent test run) uses, as real thread ids are.
    let thread = format!("t-left-{}", twr_core::event::new_id());
    let (mut child, pid) = child_runner(dir.path(), "sleeper", &thread);
    let start = twr_runner::reap::process_start_time(pid).expect("the runtime runs");
    // SIGKILL: no cleanup at all. The runtime (its own process group) survives it.
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(twr_runner::reap::process_start_time(pid), Some(start));

    // The next runner of the box kills it before anything else, and records its exit.
    let lbox = sleeper_box(dir.path());
    assert!(
        twr_runner::reap::process_start_time(pid) != Some(start),
        "the orphaned runtime still runs"
    );
    assert!(lbox.running().is_empty());
    assert!(lbox.spool().runtimes().unwrap().is_empty());
    let frames = spooled(&lbox, &thread);
    assert_eq!(
        frames,
        vec![(1, "thread.started".to_string()), (2, "exited".to_string())]
    );
    let last = lbox.spool().rows_after(0, 10).unwrap().pop().unwrap();
    let RunnerMsg::Exited { stderr_tail, .. } = serde_json::from_str(&last.frame).unwrap() else {
        panic!("not an exit")
    };
    assert!(stderr_tail.contains("was killed"), "{stderr_tail}");
}

/// A runtime left behind that cannot be shown gone keeps its record and its exit unrecorded:
/// the box does not open (so the runner never connects and the hub never starts a runtime
/// next to it). Here the process table cannot be read (`/proc` unavailable), so nothing is
/// killed and nothing can be told; once it can, the next open kills it and records its exit.
#[cfg(all(target_os = "linux", not(twr_ps)))]
#[tokio::test]
async fn a_runtime_left_behind_that_may_still_run_keeps_the_box_closed() {
    let _one = RECOVERY.lock().await;
    let dir = tempfile::tempdir().unwrap();
    // Recovery finds the runtime by its thread marker alone, machine-wide: a thread id no
    // other test (or concurrent test run) uses, as real thread ids are.
    let thread = format!("t-left-{}", twr_core::event::new_id());
    let (mut child, pid) = child_runner(dir.path(), "sleeper", &thread);
    let start = twr_runner::reap::process_start_time(pid).expect("the runtime runs");
    child.kill().unwrap();
    child.wait().unwrap();
    let faults = twr_runner::reap::ProcFaults {
        unavailable: true,
        ..Default::default()
    };
    let e = twr_runner::reap::with_proc_faults(faults, || {
        let mut o = LocalBoxOptions::new(dir.path(), BOX);
        o.runtimes = vec![];
        LocalBox::open(o).map(|_| ())
    })
    .unwrap_err()
    .to_string();
    assert!(e.contains("may still run"), "{e}");
    assert!(e.contains(&format!("process {pid}")), "{e}");
    assert_eq!(
        twr_runner::reap::process_start_time(pid),
        Some(start),
        "it was killed although it could not be told"
    );
    // Its record stays, and no exit was recorded for it.
    let lbox = sleeper_box(dir.path());
    assert!(
        twr_runner::reap::process_start_time(pid) != Some(start),
        "the orphaned runtime still runs"
    );
    let frames = spooled(&lbox, &thread);
    assert_eq!(
        frames,
        vec![(1, "thread.started".to_string()), (2, "exited".to_string())]
    );
}

/// Held by the tests whose box recovers leftover runtimes: the recovery counts processes
/// marked for a thread, or whose environment cannot be read, and the non-dumpable process of
/// [`a_runtime_whose_descendant_hides_its_environment_keeps_the_box_closed`] would count in
/// any of them.
static RECOVERY: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Kills its process when dropped (a test that fails leaves nothing running).
struct KillOnDrop(std::process::Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A process marked for `thread` (`sleep 300`), run as a recorded runtime would be.
fn marked_sleep(thread: &str) -> KillOnDrop {
    let child = std::process::Command::new("sleep")
        .arg("300")
        .env(twr_runner::reap::THREAD_ENV, thread)
        .spawn()
        .unwrap();
    let pid = child.id();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !twr_runner::reap::process_environ(pid)
        .is_ok_and(|e| twr_runner::reap::is_marked(&e, thread, None))
    {
        assert!(std::time::Instant::now() < deadline, "sleep never exec'd");
        std::thread::sleep(Duration::from_millis(5));
    }
    KillOnDrop(child)
}

/// Records, in the spool of the box in `dir`, a runtime of `thread` with process `pid` (and
/// `start_time`, `min_start`), as a runner that died would have left it.
fn record_runtime(
    dir: &Path,
    thread: &str,
    pid: u32,
    start_time: Option<u64>,
    min_start: Option<u64>,
) {
    let spool = twr_runner::spool::Spool::open(
        &twr_runner::lbox::spool_path(dir, BOX),
        twr_runner::spool::DEFAULT_MAX_BYTES,
    )
    .unwrap();
    let rec = twr_runner::spool::RuntimeRecord {
        thread: thread.into(),
        runtime: "sleeper".into(),
        pid,
        start_time,
        exe: None,
        min_start,
    };
    let frame = |rseq| {
        serde_json::to_string(&RunnerMsg::Event {
            thread: thread.into(),
            rseq,
            turn: None,
            body: EventBody::ThreadStarted(Default::default()),
            raw: None,
            run: None,
            at_ms: None,
        })
        .unwrap()
    };
    spool
        .append_with(
            thread,
            true,
            frame,
            twr_runner::spool::RuntimeChange::Started(rec),
        )
        .unwrap();
}

/// A recorded runtime whose start time was never recorded is not taken for gone while its
/// pid still names a process marked for its thread. With no lower bound on its start (a
/// record from before one was kept) nothing is killed and the box does not open; with one
/// (the start of the runner that started it) the marked process is killed, and only then is
/// its exit recorded.
#[tokio::test]
async fn a_recorded_runtime_without_a_start_time_is_not_taken_for_gone() {
    // A thread id of this test run only (recovery finds marked processes machine-wide).
    let t = format!("t-nostart-{}", twr_core::event::new_id());
    let _one = RECOVERY.lock().await;
    init_logs();
    let dir = tempfile::tempdir().unwrap();
    let lower = twr_runner::reap::own_start_time();
    let rt = marked_sleep(&t);
    let pid = rt.0.id();
    record_runtime(dir.path(), &t, pid, None, None);
    let mut o = LocalBoxOptions::new(dir.path(), BOX);
    o.runtimes = vec![];
    let e = LocalBox::open(o.clone()).unwrap_err().to_string();
    assert!(e.contains(&format!("process {pid} may still run")), "{e}");
    assert!(alive(pid), "killed without knowing it was the runtime");
    // A record with the lower bound: the marked process is killed, then its exit recorded.
    let dir2 = tempfile::tempdir().unwrap();
    record_runtime(dir2.path(), &t, pid, None, lower);
    let mut o = LocalBoxOptions::new(dir2.path(), BOX);
    o.runtimes = vec![];
    let lbox = LocalBox::open(o).unwrap();
    assert!(!alive(pid), "the runtime still runs");
    assert_eq!(
        spooled(&lbox, &t),
        vec![(1, "thread.started".to_string()), (2, "exited".to_string())]
    );
    drop(rt);
}

/// What `a_runtime_whose_descendant_hides_its_environment_keeps_the_box_closed` starts (does
/// nothing unless started by it): a process marked for a thread that makes itself
/// non-dumpable (its environment then reads to no one but root), says so in `<dir>/ready`,
/// and waits to be killed.
#[cfg(all(target_os = "linux", not(twr_ps)))]
#[test]
fn nondumpable_child_process() {
    let Ok(dir) = std::env::var(NONDUMPABLE_ENV) else {
        return;
    };
    extern "C" {
        fn prctl(option: i32, ...) -> i32;
    }
    const PR_SET_DUMPABLE: i32 = 4;
    // SAFETY: prctl(PR_SET_DUMPABLE, 0) only changes this process's dumpable flag.
    assert_eq!(unsafe { prctl(PR_SET_DUMPABLE, 0u64, 0u64, 0u64, 0u64) }, 0);
    let dir = std::path::PathBuf::from(dir);
    std::fs::write(dir.join("ready"), std::process::id().to_string()).unwrap();
    std::thread::sleep(Duration::from_secs(300));
}

/// Where [`nondumpable_child_process`] says it is ready.
#[cfg(all(target_os = "linux", not(twr_ps)))]
const NONDUMPABLE_ENV: &str = "TWR_TEST_NONDUMPABLE";

/// Starts [`nondumpable_child_process`] (in process group `group`, else a group of its own),
/// marked for `thread` (unreadable once it is non-dumpable), and waits until it is
/// non-dumpable. Returns it and its pid.
#[cfg(all(target_os = "linux", not(twr_ps)))]
async fn nondumpable(dir: &Path, thread: &str, group: Option<u32>) -> (KillOnDrop, u32) {
    use std::os::unix::process::CommandExt;
    let ready = dir.join("ready");
    let _ = std::fs::remove_file(&ready);
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "nondumpable_child_process",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(NONDUMPABLE_ENV, dir)
        .env(twr_runner::reap::THREAD_ENV, thread)
        .process_group(group.map_or(0, |g| g as i32))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let child = KillOnDrop(child);
    wait_for("the hidden child", || ready.exists()).await;
    let pid: u32 = std::fs::read_to_string(&ready).unwrap().parse().unwrap();
    assert!(
        twr_runner::reap::process_environ(pid).is_err(),
        "its environment reads"
    );
    (child, pid)
}

/// A recorded runtime (a group leader, as adapters start them) whose process is gone: returns
/// its pid and start time, its group kept alive by `keep` (spawned into it before the leader
/// exits).
#[cfg(all(target_os = "linux", not(twr_ps)))]
async fn gone_leader(dir: &Path, thread: &str, descendant: bool) -> (u32, u64, (KillOnDrop, u32)) {
    use std::os::unix::process::CommandExt;
    let mut leader = std::process::Command::new("sleep")
        .arg("300")
        .process_group(0)
        .spawn()
        .unwrap();
    let pid = leader.id();
    let start = twr_runner::reap::process_start_time(pid).expect("the leader runs");
    let hidden = nondumpable(dir, thread, descendant.then_some(pid)).await;
    leader.kill().unwrap();
    leader.wait().unwrap();
    (pid, start, hidden)
}

/// A runtime whose process is gone but whose descendant (left in its process group, this
/// user's, in this control group) hides its environment, so it cannot be told from the
/// runtime's: it counts as still running. The box does not open and the runtime's record
/// stays; once it is gone the box opens.
#[cfg(all(target_os = "linux", not(twr_ps)))]
#[tokio::test]
async fn a_runtime_whose_descendant_hides_its_environment_keeps_the_box_closed() {
    // A thread id of this test run only (recovery finds marked processes machine-wide).
    let t = format!("t-hidden-{}", twr_core::event::new_id());
    let _one = RECOVERY.lock().await;
    init_logs();
    let dir = tempfile::tempdir().unwrap();
    let since = twr_runner::reap::own_start_time();
    let (gone, start, (child, hidden)) = gone_leader(dir.path(), &t, true).await;
    record_runtime(dir.path(), &t, gone, Some(start), since);
    let mut o = LocalBoxOptions::new(dir.path(), BOX);
    o.runtimes = vec![];
    let e = LocalBox::open(o.clone()).unwrap_err().to_string();
    assert!(
        e.contains(&format!("process {hidden} may be one of it")),
        "{e}"
    );
    drop(child);
    // Once it is gone the box opens, and records the exit.
    let lbox = LocalBox::open(o).unwrap();
    assert_eq!(
        spooled(&lbox, &t),
        vec![(1, "thread.started".to_string()), (2, "exited".to_string())]
    );
}

/// A process of the user's that hides its environment but is not tied to the recorded
/// runtime (another process group, no marked or runtime parent: an `ssh-agent` or a `sudo`
/// started in the runner's terminal after it, say) does not keep the box closed: the box
/// opens, records the runtime's exit, and leaves that process alone.
#[cfg(all(target_os = "linux", not(twr_ps)))]
#[tokio::test]
async fn an_unrelated_process_hiding_its_environment_does_not_keep_the_box_closed() {
    // A thread id of this test run only (recovery finds marked processes machine-wide).
    let t = format!("t-unrelated-{}", twr_core::event::new_id());
    let _one = RECOVERY.lock().await;
    init_logs();
    let dir = tempfile::tempdir().unwrap();
    let since = twr_runner::reap::own_start_time();
    let (gone, start, (child, stranger)) = gone_leader(dir.path(), &t, false).await;
    record_runtime(dir.path(), &t, gone, Some(start), since);
    let mut o = LocalBoxOptions::new(dir.path(), BOX);
    o.runtimes = vec![];
    let lbox = LocalBox::open(o).unwrap();
    assert_eq!(
        spooled(&lbox, &t),
        vec![(1, "thread.started".to_string()), (2, "exited".to_string())]
    );
    assert!(alive(stranger), "the unrelated process was killed");
    drop(child);
}

/// The box's machine lock does not move with the data directory: it is keyed by the hub and
/// the box alone, so a second runner of the box with another `--data-dir` (and so other
/// default worktrees) is refused while the first is open.
#[test]
fn a_second_runner_with_another_data_dir_is_refused() {
    init_logs();
    let dir = tempfile::tempdir().unwrap();
    // A hub address of this test run only (the lock lives in the user's runtime directory).
    let url = format!("ws://127.0.0.1:1/{}", new_nonce().unwrap());
    let opts =
        |d: &str| RunnerOptions::new(url.clone(), Secret::new(TOKEN), BOX, dir.path().join(d));
    let (a, b) = (opts("a").box_options(), opts("b").box_options());
    assert_ne!(a.spool_path, b.spool_path);
    assert_ne!(a.worktrees, b.worktrees);
    assert_eq!(a.machine_lock, b.machine_lock);
    let lock = a.machine_lock.clone().expect("a machine lock");
    assert!(!lock.starts_with(dir.path()), "{}", lock.display());
    // Another box, or another hub, is another key.
    let other = RunnerOptions::new(url.clone(), Secret::new(TOKEN), "cpu", dir.path());
    assert_ne!(other.box_options().machine_lock, a.machine_lock);
    let elsewhere = RunnerOptions::new(
        "ws://127.0.0.1:2/",
        Secret::new(TOKEN),
        BOX,
        dir.path().join("a"),
    );
    assert_ne!(elsewhere.box_options().machine_lock, a.machine_lock);
    let first = LocalBox::open(with_fake(a)).unwrap();
    let e = LocalBox::open(with_fake(b.clone()))
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("another runner of this box is running on this machine"),
        "{e}"
    );
    assert!(e.contains(&lock.display().to_string()), "{e}");
    drop(first);
    drop(LocalBox::open(with_fake(b)).unwrap());
    let _ = std::fs::remove_file(lock);
}

/// Everything any test here logs.
static LOGS: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// Installs the process-wide TRACE subscriber writing into [`LOGS`] (a scoped subscriber
/// would miss events whose callsites another test thread registered first).
fn init_logs() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .with_writer(|| LogWriter)
            .finish();
        tracing::subscriber::set_global_default(subscriber).unwrap();
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

/// Whether process `pid` runs (exists and is not a zombie).
fn alive(pid: u32) -> bool {
    twr_runner::reap::process_alive(pid)
}

/// A runtime with a real process (`sleep 300`, marked like any runtime) that reports it, then
/// emits one large event (enough to overflow a small spool) and ignores `stop` (counting it in
/// `stops`): it exits only once `release` is notified, or when its process dies.
struct Hog {
    release: Arc<tokio::sync::Notify>,
    stops: Arc<std::sync::atomic::AtomicUsize>,
}

impl twr_core::adapter::Adapter for Hog {
    fn info(&self) -> twr_core::adapter::AdapterInfo {
        twr_core::adapter::AdapterInfo {
            name: "hog",
            tier: twr_core::adapter::Tier::Headless,
            description: "overflows the spool and stops late",
        }
    }

    fn spawn(
        &self,
        cfg: twr_core::adapter::SpawnConfig,
        _prompt: Option<String>,
    ) -> anyhow::Result<twr_core::adapter::AdapterHandle> {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("300").process_group(0).envs(cfg.env.clone());
        let mut child = cmd.spawn()?;
        let pid = child.id().unwrap();
        let (h, mut end) = twr_core::adapter::AdapterHandle::pair();
        let (release, stops) = (self.release.clone(), self.stops.clone());
        tokio::spawn(async move {
            for _ in 0..500 {
                let cmdline = twr_runner::reap::process_cmdline(pid).unwrap_or_default();
                let env = twr_runner::reap::process_environ(pid).unwrap_or_default();
                if cmdline.starts_with(b"sleep") && !env.is_empty() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            end.emit(
                EventBody::ThreadStarted(twr_core::event::SessionPayload {
                    session_id: Some("s-hog".into()),
                    ext: serde_json::from_value(json!({"twr": {"pid": pid, "exe": "sleep"}}))
                        .unwrap(),
                    ..Default::default()
                }),
                None,
            );
            end.emit(
                EventBody::AgentText(twr_core::event::AgentText {
                    text: "x".repeat(4000),
                    ..Default::default()
                }),
                Some(1),
            );
            let mut cmds_open = true;
            loop {
                tokio::select! {
                    () = release.notified() => {
                        let _ = child.kill().await;
                        end.exited(None, "stopped late");
                        break;
                    }
                    st = child.wait() => {
                        end.exited(st.ok().and_then(|s| s.code()), "");
                        break;
                    }
                    cmd = end.cmds.recv(), if cmds_open => match cmd {
                        Some(twr_core::adapter::AdapterCmd::Stop) => {
                            stops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        }
                        Some(_) => {}
                        None => cmds_open = false,
                    },
                }
            }
        });
        Ok(h)
    }
}

fn hog_box(dir: &Path, release: &Arc<tokio::sync::Notify>) -> LocalBox {
    hog_box_counting(dir, release, &Arc::default())
}

/// [`hog_box`], its hogs counting the stops they get in `stops`.
fn hog_box_counting(
    dir: &Path,
    release: &Arc<tokio::sync::Notify>,
    stops: &Arc<std::sync::atomic::AtomicUsize>,
) -> LocalBox {
    init_logs();
    let mut o = LocalBoxOptions::new(dir, BOX);
    o.spool_max_bytes = 1500;
    let hog: Arc<dyn twr_core::adapter::Adapter> = Arc::new(Hog {
        release: release.clone(),
        stops: stops.clone(),
    });
    o.adapters = Arc::new(move |n: &str| (n == "hog").then(|| Arc::clone(&hog)));
    o.runtimes = vec!["hog".into()];
    LocalBox::open(o).unwrap()
}

fn hog_spawn(cwd: &Path) -> Op {
    Op::Spawn {
        thread: "t1".into(),
        runtime: "hog".into(),
        cfg: WireSpawnConfig {
            cwd: cwd.to_path_buf(),
            ..Default::default()
        },
        prompt: None,
        run: None,
    }
}

/// Waits until the box has stopped `t1`'s runtime for the spool overflow (its hog counted the
/// stop).
async fn wait_overflowed(stops: &std::sync::atomic::AtomicUsize) {
    wait_for("the overflow's stop", || {
        stops.load(std::sync::atomic::Ordering::SeqCst) > 0
    })
    .await;
}

/// Spool overflow stops the runtime, but its session stays live and its exit is
/// recorded only once its process has really exited, with the overflow as its reason: the hub
/// never sees a process that still runs as gone. Its worktree is not removed under it either.
/// A runner that dies in between still has the next one kill it (its row stays).
#[tokio::test]
async fn an_overflowed_runtime_stays_live_until_it_really_exits() {
    use twr_core::runner::{decode_result, WireRemoveOutcome};
    let dir = tempfile::tempdir().unwrap();
    let release = Arc::new(tokio::sync::Notify::new());
    let stops = Arc::default();
    let lbox = hog_box_counting(&dir.path().join("data"), &release, &stops);
    let repo = twr_core::testkit::git::init_repo(&dir.path().join("repo"));
    let wt = dir.path().join("data/worktrees/p/t1");
    let made = lbox
        .handle_cmd(
            "g1",
            Op::WorktreeCreate {
                repo: repo.clone(),
                path: wt.clone(),
                branch: "twr/t1".into(),
                base: None,
            },
        )
        .await;
    assert!(made.ok, "{made:?}");
    assert!(lbox.handle_cmd("c1", hog_spawn(&wt)).await.ok);
    wait_overflowed(&stops).await;
    // Still running (it ignores the stop): no exit yet, the session and its row stay.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let rows = lbox.spool().runtimes().unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    let pid = rows[0].pid;
    assert!(alive(pid));
    assert_eq!(lbox.live_count(), 1);
    assert!(
        !spooled(&lbox, "t1").iter().any(|f| f.1 == "exited"),
        "an exit was recorded while the runtime still ran: {:?}",
        spooled(&lbox, "t1")
    );
    // Its worktree is not removed under it.
    let remove = |id: &'static str| {
        let (lbox, repo, wt) = (lbox.clone(), repo.clone(), wt.clone());
        async move {
            let r = lbox
                .handle_cmd(
                    id,
                    Op::WorktreeRemove {
                        repo,
                        path: wt,
                        branch: Some("twr/t1".into()),
                        disposable: None,
                    },
                )
                .await;
            decode_result::<WireRemoveOutcome>(r.into_outcome().unwrap()).unwrap()
        }
    };
    let kept = remove("g2").await;
    assert!(
        matches!(&kept, WireRemoveOutcome::Kept { reason } if reason.contains("still runs")),
        "{kept:?}"
    );
    assert!(wt.is_dir(), "the worktree was removed under a live runtime");
    // Nothing more reaches the stopping runtime.
    let sent = lbox
        .execute(Op::Send {
            thread: "t1".into(),
            turn: 2,
            text: "more".into(),
            run: None,
        })
        .await
        .unwrap_err();
    assert_eq!(sent.code, cmd_errors::NOT_RUNNING, "{sent:?}");
    assert!(sent.message.contains("spool full"), "{sent:?}");
    // It exits: its one exit is recorded, with the overflow as the reason, and the row goes.
    release.notify_one();
    wait_for("the session ended", || lbox.live_count() == 0).await;
    assert!(lbox.spool().runtimes().unwrap().is_empty());
    let exits: Vec<_> = lbox
        .spool()
        .rows_after(0, 1000)
        .unwrap()
        .into_iter()
        .filter_map(|r| match serde_json::from_str(&r.frame).unwrap() {
            RunnerMsg::Exited {
                code, stderr_tail, ..
            } => Some((code, stderr_tail)),
            _ => None,
        })
        .collect();
    assert_eq!(exits.len(), 1, "{exits:?}");
    assert_eq!(exits[0].0, None);
    assert!(exits[0].1.contains("spool full"), "{exits:?}");
    assert!(exits[0].1.contains("stopped late"), "{exits:?}");
    // Now it goes.
    assert_eq!(remove("g3").await, WireRemoveOutcome::Removed);
    drop(lbox);

    // Again, but the runner dies before the process exits: the next runner kills it.
    let dir = tempfile::tempdir().unwrap();
    let never = Arc::new(tokio::sync::Notify::new());
    let stops = Arc::default();
    let lbox = hog_box_counting(dir.path(), &never, &stops);
    assert!(
        lbox.handle_cmd("c1", hog_spawn(&std::env::temp_dir()))
            .await
            .ok
    );
    wait_overflowed(&stops).await;
    let pid = lbox.spool().runtimes().unwrap()[0].pid;
    drop(lbox);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(alive(pid), "the runtime should survive the runner");
    let lbox = hog_box(dir.path(), &never);
    wait_for("the orphan killed", || !alive(pid)).await;
    assert!(lbox.spool().runtimes().unwrap().is_empty());
    assert_eq!(
        spooled(&lbox, "t1")
            .iter()
            .filter(|f| f.1 == "exited")
            .count(),
        1
    );
}

/// The kill of an overflowed run takes the process it kills from its session
/// under the same lock that shows the session is still that run, and signals it only while it
/// carries the run's marker. Scheduled deterministically: the kill pauses right after that
/// check; meanwhile the run exits and the thread's next run starts and records its own process;
/// then the kill goes on. The next run survives.
#[tokio::test]
async fn an_overflow_kill_never_reaches_the_threads_next_run() {
    let dir = tempfile::tempdir().unwrap();
    init_logs();
    let release = Arc::new(tokio::sync::Notify::new());
    let stops: Arc<std::sync::atomic::AtomicUsize> = Arc::default();
    let mut o = LocalBoxOptions::new(dir.path(), BOX);
    o.spool_max_bytes = 1500;
    let hog: Arc<dyn twr_core::adapter::Adapter> = Arc::new(Hog {
        release: release.clone(),
        stops: stops.clone(),
    });
    let sleeper: Arc<dyn twr_core::adapter::Adapter> = Arc::new(Sleeper { slow: false });
    o.adapters = Arc::new(move |n: &str| match n {
        "hog" => Some(Arc::clone(&hog)),
        "sleeper" => Some(Arc::clone(&sleeper)),
        _ => None,
    });
    o.runtimes = vec!["hog".into(), "sleeper".into()];
    let lbox = LocalBox::open(o).unwrap();
    let mut pauses = lbox.hold_overflow_kills(Duration::from_millis(50));

    // Run 1 overflows the spool; its kill checks the session and pauses.
    assert!(lbox.handle_cmd("c1", hog_spawn(dir.path())).await.ok);
    let pause = tokio::time::timeout(Duration::from_secs(60), pauses.recv())
        .await
        .expect("the overflow kill never came")
        .unwrap();
    assert_eq!(pause.thread, "t1");
    let rows = lbox.spool().runtimes().unwrap();
    let old = rows[0].pid;

    // Run 1 exits; run 2 of the thread starts and records its process.
    release.notify_one();
    wait_for("run 1 ended", || lbox.live_count() == 0).await;
    let last = spooled(&lbox, "t1").last().unwrap().0;
    lbox.spool().ack("t1", last).unwrap();
    let next = lbox
        .handle_cmd(
            "c2",
            Op::Spawn {
                thread: "t1".into(),
                runtime: "sleeper".into(),
                cfg: WireSpawnConfig {
                    cwd: dir.path().to_path_buf(),
                    ..Default::default()
                },
                prompt: None,
                run: None,
            },
        )
        .await;
    assert!(next.ok, "{next:?}");
    let new = {
        let lbox = lbox.clone();
        wait_for_within(
            "run 2 recorded its process",
            Duration::from_secs(30),
            || {
                lbox.spool()
                    .runtimes()
                    .unwrap()
                    .iter()
                    .any(|r| r.thread == "t1" && r.pid != old)
            },
        )
        .await;
        lbox.spool().runtimes().unwrap()[0].pid
    };
    assert!(alive(new));

    // Run 1's kill goes on: it must not touch run 2.
    pause.go.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(60), pause.done)
        .await
        .expect("the overflow kill never finished")
        .unwrap();
    assert!(
        alive(new),
        "run 1's overflow kill killed the thread's next run"
    );
    assert_eq!(lbox.live_count(), 1);
    lbox.execute(Op::Stop {
        thread: "t1".into(),
        run: None,
    })
    .await
    .unwrap();
    wait_for_within("run 2 stopped", Duration::from_secs(30), || {
        lbox.live_count() == 0
    })
    .await;
}

/// A worktree removal reserves its directory from its safety checks until git has finished
/// (when nobody waits for it any more, git is killed and reaped first). A runtime starting
/// there meanwhile is refused (the removal would delete the directory, and what the runtime
/// writes in it, underneath it), and asking whether the worktree exists waits for the
/// removal's outcome.
#[tokio::test]
async fn a_worktree_removal_in_progress_keeps_runtimes_out() {
    use twr_core::runner::{decode_result, WireRemoveOutcome};
    let dir = tempfile::tempdir().unwrap();
    init_logs();
    let held = twr_core::testkit::git::HeldGit::install(&dir.path().join("bin"));
    let mut o = LocalBoxOptions::new(&dir.path().join("data"), BOX);
    o.git = twr_runner::local::LocalGit::with_bin(&held.bin);
    let sleeper: Arc<dyn twr_core::adapter::Adapter> = Arc::new(Sleeper { slow: false });
    o.adapters = Arc::new(move |n: &str| (n == "sleeper").then(|| Arc::clone(&sleeper)));
    o.runtimes = vec!["sleeper".into()];
    let lbox = LocalBox::open(o).unwrap();
    let repo = twr_core::testkit::git::init_repo(&dir.path().join("repo"));
    let wt = dir.path().join("data/worktrees/p/t1");
    let create = |id: &'static str| {
        let (lbox, repo, wt) = (lbox.clone(), repo.clone(), wt.clone());
        async move {
            lbox.handle_cmd(
                id,
                Op::WorktreeCreate {
                    repo,
                    path: wt,
                    branch: "twr/t1".into(),
                    base: None,
                },
            )
            .await
        }
    };
    let spawn = |id: &'static str| {
        let (lbox, wt) = (lbox.clone(), wt.clone());
        async move {
            lbox.handle_cmd(
                id,
                Op::Spawn {
                    thread: "t1".into(),
                    runtime: "sleeper".into(),
                    cfg: WireSpawnConfig {
                        cwd: wt,
                        ..Default::default()
                    },
                    prompt: None,
                    run: None,
                },
            )
            .await
        }
    };
    let made = create("g1").await;
    assert!(made.ok, "{made:?}");

    // The removal passes its checks; git is held.
    let removal = {
        let (lbox, repo, wt) = (lbox.clone(), repo.clone(), wt.clone());
        tokio::spawn(async move {
            lbox.execute(Op::WorktreeRemove {
                repo,
                path: wt,
                branch: Some("twr/t1".into()),
                disposable: None,
            })
            .await
        })
    };
    wait_for_within("git holds the removal", Duration::from_secs(60), || {
        held.held()
    })
    .await;

    // A runtime starting in the worktree now is refused, and says why.
    // (Had it started, the removal would delete the worktree, and what the runtime writes
    // there, e.g. an ignored `.env`, underneath it.)
    let started = spawn("c1").await;
    let e = started
        .error
        .expect("a runtime started in a worktree being removed");
    assert_eq!(e.code, cmd_errors::FAILED, "{e:?}");
    assert!(e.message.contains("being removed"), "{e:?}");
    // Whether the worktree exists is answered once the removal has finished.
    let exists = {
        let (lbox, wt) = (lbox.clone(), wt.clone());
        tokio::spawn(async move { lbox.execute(Op::WorktreeExists { path: wt }).await })
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !exists.is_finished(),
        "worktree_exists answered while a removal of it was running"
    );
    held.release();
    let removed = tokio::time::timeout(Duration::from_secs(60), removal)
        .await
        .expect("the removal never finished")
        .unwrap()
        .unwrap();
    assert!(matches!(
        decode_result::<WireRemoveOutcome>(removed).unwrap(),
        WireRemoveOutcome::Removed
    ));
    let exists = tokio::time::timeout(Duration::from_secs(60), exists)
        .await
        .expect("worktree_exists never answered")
        .unwrap()
        .unwrap();
    assert!(held.finished());
    assert!(!decode_result::<bool>(exists).unwrap());
    assert!(!wt.exists());
    assert_eq!(lbox.live_count(), 0);

    // Recreated, it takes a runtime again, and is not removed under it.
    assert!(create("g2").await.ok);
    let started = spawn("c2").await;
    assert!(started.ok, "{started:?}");
    let kept = lbox
        .execute(Op::WorktreeRemove {
            repo: repo.clone(),
            path: wt.clone(),
            branch: Some("twr/t1".into()),
            disposable: None,
        })
        .await
        .unwrap();
    let kept = decode_result::<WireRemoveOutcome>(kept).unwrap();
    assert!(
        matches!(&kept, WireRemoveOutcome::Kept { reason } if reason.contains("still runs")),
        "{kept:?}"
    );
    assert!(wt.is_dir());
    lbox.execute(Op::Stop {
        thread: "t1".into(),
        run: None,
    })
    .await
    .unwrap();
    wait_for_within("the runtime stopped", Duration::from_secs(30), || {
        lbox.live_count() == 0
    })
    .await;
}

/// A runtime whose process (`sleep 300`, marked like any runtime) never reports itself and
/// ignores `stop`; it exits when its process dies. With `vanish`, its adapter goes away at once
/// without reporting any exit, leaving the process running.
struct Mute {
    pid: Arc<Mutex<Option<u32>>>,
    vanish: bool,
}

impl twr_core::adapter::Adapter for Mute {
    fn info(&self) -> twr_core::adapter::AdapterInfo {
        twr_core::adapter::AdapterInfo {
            name: "mute",
            tier: twr_core::adapter::Tier::Headless,
            description: "never reports its process, ignores stop",
        }
    }

    fn spawn(
        &self,
        cfg: twr_core::adapter::SpawnConfig,
        _prompt: Option<String>,
    ) -> anyhow::Result<twr_core::adapter::AdapterHandle> {
        let mut cmd = tokio::process::Command::new("sleep");
        cmd.arg("300").process_group(0).envs(cfg.env.clone());
        let mut child = cmd.spawn()?;
        *self.pid.lock().unwrap() = child.id();
        let (h, end) = twr_core::adapter::AdapterHandle::pair();
        if self.vanish {
            let pid = child.id().unwrap();
            tokio::spawn(async move {
                // Once past execve (its marker visible), the adapter goes away; the process
                // runs on (not killed on drop).
                for _ in 0..500 {
                    let env = twr_runner::reap::process_environ(pid).unwrap_or_default();
                    if twr_runner::reap::environ_has(&env, "TWR_THREAD", "t1") {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                drop((child, end));
            });
            return Ok(h);
        }
        tokio::spawn(async move {
            let st = child.wait().await;
            end.exited(st.ok().and_then(|s| s.code()), "");
        });
        Ok(h)
    }
}

/// A runtime still running when the runner shuts down gets its exit
/// recorded only once none of its processes is left, also when it never reported its process
/// (found by its thread and run markers).
#[tokio::test]
async fn shutdown_records_an_exit_only_once_the_runtime_is_gone() {
    let dir = tempfile::tempdir().unwrap();
    let (lbox, pid) = mute_box(dir.path(), false).await;
    // Past execve: its marker is visible.
    wait_for("the runtime's environment", || {
        twr_runner::reap::process_environ(pid)
            .is_ok_and(|e| twr_runner::reap::environ_has(&e, "TWR_THREAD", "t1"))
    })
    .await;
    let environ = twr_runner::reap::process_environ(pid).unwrap();
    let run = lbox.running()[0].run.clone().unwrap();
    assert!(twr_runner::reap::environ_has(
        &environ,
        twr_runner::reap::RUNTIME_GEN_ENV,
        &run
    ));
    lbox.shutdown(Duration::from_millis(200)).await;
    let still = alive(pid);
    if still {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status();
    }
    assert!(
        !still,
        "the runner recorded the exit of a runtime that still ran"
    );
    assert_eq!(spooled(&lbox, "t1"), vec![(1, "exited".to_string())]);
}

/// A box running thread `t1` on a [`Mute`] runtime (`vanish`: its adapter goes away at once);
/// returns the box and the runtime's pid.
async fn mute_box(dir: &Path, vanish: bool) -> (LocalBox, u32) {
    init_logs();
    let pid = Arc::new(Mutex::new(None));
    let mut o = LocalBoxOptions::new(dir, BOX);
    let mute: Arc<dyn twr_core::adapter::Adapter> = Arc::new(Mute {
        pid: pid.clone(),
        vanish,
    });
    o.adapters = Arc::new(move |n: &str| (n == "mute").then(|| Arc::clone(&mute)));
    o.runtimes = vec!["mute".into()];
    let lbox = LocalBox::open(o).unwrap();
    let spawn = Op::Spawn {
        thread: "t1".into(),
        runtime: "mute".into(),
        cfg: WireSpawnConfig {
            cwd: std::env::temp_dir(),
            ..Default::default()
        },
        prompt: None,
        run: None,
    };
    assert!(lbox.handle_cmd("c1", spawn).await.ok);
    let pid = pid.lock().unwrap().unwrap();
    (lbox, pid)
}

/// An adapter that goes away without reporting an exit may leave its
/// process running; the box kills it (found by its thread and run markers) before it records
/// the exit.
#[tokio::test]
async fn an_adapter_that_vanishes_has_its_process_killed_before_the_exit_is_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let (lbox, pid) = mute_box(dir.path(), true).await;
    wait_for("the exit", || {
        spooled(&lbox, "t1").iter().any(|f| f.1 == "exited")
    })
    .await;
    let still = alive(pid);
    if still {
        let _ = std::process::Command::new("kill")
            .args(["-KILL", &pid.to_string()])
            .status();
    }
    assert!(
        !still,
        "the runner recorded the exit of a runtime that still ran"
    );
    assert_eq!(lbox.live_count(), 0);
}

/// A runtime whose events are too large for one WebSocket message.
struct Big;

impl twr_core::adapter::Adapter for Big {
    fn info(&self) -> twr_core::adapter::AdapterInfo {
        twr_core::adapter::AdapterInfo {
            name: "big",
            tier: twr_core::adapter::Tier::Headless,
            description: "emits oversized events",
        }
    }

    fn spawn(
        &self,
        _cfg: twr_core::adapter::SpawnConfig,
        _prompt: Option<String>,
    ) -> anyhow::Result<twr_core::adapter::AdapterHandle> {
        let (h, end) = twr_core::adapter::AdapterHandle::pair();
        let huge = "y".repeat(twr_core::runner::MAX_FRAME_BYTES + 10);
        end.emit(
            EventBody::AgentText(twr_core::event::AgentText {
                text: huge.clone(),
                is_final: true,
                ..Default::default()
            }),
            Some(1),
        );
        end.emit(
            EventBody::TurnEnded(twr_core::event::TurnEnded {
                result: twr_core::event::TurnResult::Error,
                session_id: Some("s-big".into()),
                ext: serde_json::from_value(json!({"blob": huge})).unwrap(),
                ..Default::default()
            }),
            Some(1),
        );
        end.exited(Some(0), "");
        Ok(h)
    }
}

#[tokio::test]
async fn oversized_events_become_marked_placeholders() {
    let dir = tempfile::tempdir().unwrap();
    init_logs();
    let mut o = LocalBoxOptions::new(dir.path(), BOX);
    o.adapters = Arc::new(|n: &str| {
        (n == "big").then(|| Arc::new(Big) as Arc<dyn twr_core::adapter::Adapter>)
    });
    let lbox = LocalBox::open(o).unwrap();
    let r = lbox
        .handle_cmd(
            "c1",
            Op::Spawn {
                thread: "t1".into(),
                runtime: "big".into(),
                cfg: WireSpawnConfig::default(),
                prompt: None,
                run: None,
            },
        )
        .await;
    assert!(r.ok, "{r:?}");
    // Each 16 MiB event is serialized a few times while it is fitted (seconds apiece in a
    // debug build on a loaded host): a generous budget.
    wait_for_within("the session ended", Duration::from_secs(60), || {
        lbox.live_count() == 0
    })
    .await;
    assert_eq!(
        spooled(&lbox, "t1")
            .into_iter()
            .map(|f| f.1)
            .collect::<Vec<_>>(),
        vec!["agent.text", "turn.ended", "exited"]
    );
    assert_eq!(lbox.dropped_events(), 2);
    // Every spooled frame carries the time it was captured (the hub records replayed frames
    // at that time).
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    for r in lbox.spool().rows_after(0, 10).unwrap() {
        let (RunnerMsg::Event { at_ms, .. } | RunnerMsg::Exited { at_ms, .. }) =
            serde_json::from_str(&r.frame).unwrap()
        else {
            panic!("not a frame")
        };
        let at = at_ms.expect("a capture time");
        assert!(at <= now_ms && now_ms - at < 60_000, "{at} vs {now_ms}");
    }
    let bodies: Vec<EventBody> = lbox
        .spool()
        .rows_after(0, 10)
        .unwrap()
        .into_iter()
        .filter_map(|r| match serde_json::from_str(&r.frame).unwrap() {
            RunnerMsg::Event { body, .. } => Some(body),
            _ => None,
        })
        .collect();
    let dropped = |ext: &Option<twr_core::event::JsonMap>| {
        ext.as_ref().unwrap()["twr"]["_dropped"]["bytes"]
            .as_u64()
            .unwrap()
    };
    let EventBody::AgentText(t) = &bodies[0] else {
        panic!("{:?}", bodies[0].kind())
    };
    assert!(t.is_final);
    assert!(t.text.len() <= 4096 && t.text.starts_with("yyyy"));
    assert!(dropped(&t.ext) > twr_core::runner::MAX_FRAME_BYTES as u64);
    let EventBody::TurnEnded(e) = &bodies[1] else {
        panic!("{:?}", bodies[1].kind())
    };
    // What closes the turn survives.
    assert_eq!(e.result, twr_core::event::TurnResult::Error);
    assert_eq!(e.session_id.as_deref(), Some("s-big"));
    assert!(dropped(&e.ext) > twr_core::runner::MAX_FRAME_BYTES as u64);
}

/// Creates the executable `path` (mode 755, its directories made) holding `content`, without
/// this process ever holding it open for writing: the bytes go to a side file, `cp` writes
/// `path`, and the side file is removed. A child another test thread forks while this process
/// writes the file would inherit the descriptor, and exec'ing the file would fail with
/// ETXTBSY until that child execs or exits.
fn install(path: &Path, content: impl AsRef<[u8]>) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut src = path.as_os_str().to_owned();
    src.push(".src");
    std::fs::write(&src, content).unwrap();
    let st = std::process::Command::new("cp")
        .arg(&src)
        .arg(path)
        .status()
        .unwrap();
    assert!(st.success(), "cp failed");
    std::fs::remove_file(&src).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// At most `MAX_CONCURRENT_OPS` non-session ops (here: git snapshots) run at once.
#[tokio::test]
async fn non_session_ops_are_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let probe = dir.path().join("probe");
    std::fs::create_dir(&probe).unwrap();
    let git = dir.path().join("fake-git");
    install(
        &git,
        format!(
            // (The listing of the config's drivers before each command, and where git finds its
            // repository, before and after it: answered at once.)
            "#!/bin/sh\nfor a in \"$@\"; do case \"$a\" in --name-only|--absolute-git-dir) exit 0;; esac; done\nd='{}'\ntouch \"$d/run.$$\"\nls \"$d\" | grep -c '^run\\.' >> \"$d/../counts\"\nsleep 0.2\nrm -f \"$d/run.$$\"\n",
            probe.display()
        ),
    );
    init_logs();
    let mut o = LocalBoxOptions::new(&dir.path().join("data"), BOX);
    o.git = twr_runner::local::LocalGit::with_bin(&git);
    let lbox = LocalBox::open(o).unwrap();
    let n = 3 * twr_runner::lbox::MAX_CONCURRENT_OPS;
    let results = futures::future::join_all((0..n).map(|i| {
        let lbox = lbox.clone();
        let path = dir.path().to_path_buf();
        async move {
            lbox.handle_cmd(&format!("s{i}"), Op::GitSnapshot { path, base: None })
                .await
        }
    }))
    .await;
    assert!(results.iter().all(|r| r.ok), "{results:?}");
    let counts: Vec<usize> = std::fs::read_to_string(dir.path().join("counts"))
        .unwrap()
        .lines()
        .map(|l| l.trim().parse().unwrap())
        .collect();
    assert_eq!(counts.len(), n);
    let max = *counts.iter().max().unwrap();
    assert!(max <= twr_runner::lbox::MAX_CONCURRENT_OPS, "{counts:?}");
    assert!(max >= 2, "ops should still run concurrently: {counts:?}");
}
