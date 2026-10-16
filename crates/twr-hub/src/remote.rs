//! Remote boxes: [`RemoteBox`] is the [`ThreadHost`] of a box whose runner
//! connects to the hub (`[boxes.<name>]`, transport `token`), or that the hub reaches over ssh
//! itself (transport `ssh`, [`crate::ssh_box`]). Every host call becomes a command
//! ([`HubMsg::Cmd`] with an [`Op`]) on the box's current connection; the runner answers
//! [`RunnerMsg::CmdResult`](twr_core::runner::RunnerMsg::CmdResult) once per command id.
//!
//! **Transports.** A token box's runner proves its token on the hub's listener
//! ([`RemoteBox::hub_proof`], [`RemoteBox::checks_runner_proof`]). An SSH box has no token:
//! its only connections are the sealed ones the hub's own ssh supervisor opens
//! (`Hub::serve_sealed_runner`), so on the listener it is a box the hub does not know (a
//! random proof, then `unauthorized`). Its threads' branches take the namespace of the hub's
//! own box ([`RemoteBox::with_branch_namespace`]): the box's daemon belongs to this hub
//! instance alone, as the hub's own worktrees do; a token box keeps `twr/<name>`.
//!
//! **Commands.** Each command gets a fresh id (a ULID) that it keeps for its whole life: a
//! command in flight when its connection drops is sent again, with the same id, once the box is
//! back (the runner deduplicates ids, returning the first execution's result). Two kinds:
//!
//! - *session commands* (`spawn`, `resume`, `send`, `respond`, `stop` of a session the hub
//!   runs there) wait for the box across disconnections, in the order they were issued;
//!   `spawn` / `resume` give up after [`SPAWN_TIMEOUT`].
//! - *requests* (git, `attach_info`, stopping a stray session) fail at once with
//!   [`codes::BOX_OFFLINE`](twr_core::proto::codes::BOX_OFFLINE) while the box is offline or
//!   when its connection drops, and after [`REQUEST_TIMEOUT`].
//!
//! A result `interrupted` (the runner died while running the command: its effect is unknown)
//! is an error like any other; a `spawn` is never retried blindly: its session fails.
//!
//! *Repository changes* ([`Op::changes_repository`]: worktree create / remove / prune, trash
//! move, purge, restore, undo) are requests the hub waits for up to
//! [`twr_core::git::CHANGE_REQUEST_TIMEOUT`], longer than the box's kill-and-reap bound of a
//! changing git command; the runner answers only once every git the op started is reaped, so
//! an answer means nothing of it runs any more. One left unanswered (timed out, its
//! connection dropped after it went out, or its caller gone) is *unsettled*: its outcome is
//! unknown and the box may still be running its git. Until it settles, the box runs no other
//! git op for the hub ([`RemoteBox::wait_settled`]: every git op but `git_snapshot` and
//! `trash_list` waits for it, fails `box_offline` while the box is offline, and gives up
//! after the same timeout), so no repository reservation the hub takes next is acted on while
//! that git may still change the box. It settles when its answer arrives after all, or at the
//! box's next authenticated connection: a runner finishes every repository change it received
//! before it reconnects (`twr_runner::client`), a restarted one runs none, and a second
//! runner of the box cannot start on the same machine while one runs
//! (`twr_runner::LocalBox::open` locks the box's spool and worktrees).
//!
//! *Durable commands.* `send`, `respond` and a `thread.stop`'s `stop` follow a state change
//! the hub has already committed (`turn.started`, `attention.resolved`, the stop request), so
//! the hub records them in the same transaction (`remote_outbox`, see [`crate::store`]), under
//! an id of their own: `send:<thread>:<turn>` ([`durable_cmd_id`]), `respond:<runtime
//! generation>:<ULID>` ([`respond_cmd_id`]; the session's forwarder learns it from
//! [`RemoteBox::expect_respond`], since runtimes reuse attention ids and the answer alone
//! cannot name it; the generation keeps it from ever reaching a replacement runtime), or
//! `stop:<thread>:<runtime generation>` ([`stop_cmd_id`], [`RemoteBox::expect_stop`]). A hub
//! that dies before the command
//! reached the runner sends it again, with that id, once the box is back
//! ([`RemoteBox::set_ready`]); the record goes when the runner answers. An error answer is
//! never only logged: the hub retires the record in one batch with what the failure means
//! (the turn ends `error`; a runtime left awaiting an answer is stopped; a failed `stop`
//! stays recorded and goes again), see `hub::runners`. `spawn` / `resume` are not recorded:
//! a hub restart closes the open turn of a session that never started.
//!
//! *Runs.* The hub names the run of each session it starts (`spawn` / `resume` `run`, a ULID;
//! [`RemoteBox::session_run`]), so it knows which frames are the session's before the first
//! one arrives. Recorded `send`s and `respond`s name the run they are for (the runner refuses
//! them for another run).
//!
//! *Stops of a stray session* name the run the hub saw ([`Op::Stop`] `run`), so a stop that
//! arrives after that run ended never kills its replacement.
//!
//! **Attention ids.** A runner's attention ids are its runtimes' own: the hub records them in
//! their thread's namespace, `<thread>:<id>`, like a local adapter's
//! ([`twr_core::adapter::hub_attn_id`]), and gives the runner back its own id in `respond`
//! (the hub strips the namespace before the command is made). A runner's frames only reach
//! threads of its box, so no box can raise, move or answer another box's (or the hub's) items.
//!
//! **Send queue.** Each connection has a bounded queue ([`SEND_QUEUE`] messages) that its
//! writer drains; a runner that does not read closes its connection when the queue fills up
//! (it reconnects and replays), instead of growing the hub's memory.
//!
//! **Sessions.** [`ThreadHost::spawn`] / [`ThreadHost::resume`] return an
//! [`AdapterHandle`] at once (the hub's core is synchronous): a forwarder task sends the
//! `spawn` / `resume` command, then turns the hub's [`AdapterCmd`]s into `send` / `respond` /
//! `stop` commands, in order. The session's events do NOT travel through this handle: the hub
//! records a runner's frames on its ingest path, where each frame's event and its delivery
//! watermark commit in one transaction before the frame is acked (see
//! `hub::runners`). The handle's event stream only ever carries an exit the hub makes up: the
//! session's first command failed (`spawn` refused, `interrupted`, no answer), so the session
//! never started as far as the hub knows. That keeps every event on one path: nothing is
//! appended twice.
//!
//! **Connections.** [`RemoteBox::connect`] takes a newly authenticated connection and fences
//! the previous one (it is told `fenced` and closed); its epoch is one more than the last,
//! kept in memory only: a hub restart starts over, which is fine since epochs only order the
//! connections one hub sees. The runner's `epoch_hint` is informational and never used: an
//! epoch the runner could steer (`u64::MAX`) would leave no room for the next connection, or
//! let two connections share one (the old one's cleanup would then disconnect its
//! replacement). Should the counter ever run out, the connection is refused before anything
//! changes. The box is *ready* once the
//! hub has replayed the runner's spool and reconciled sessions; only then are commands sent.
//!
//! **Attach.** [`RemoteBox::attach_command`] runs, under the user's own ssh identity, the
//! interactive command the hub's own adapter for the runtime builds for another box
//! (`Adapter::remote_attach_command`: never fitted to what the hub knows of its own binaries,
//! which may be other versions than the box's; never a program, argument or environment the
//! runner names), wrapped as `ssh -t <boxes.NAME.ssh> -- <shell command>` with every kind
//! of forwarding off ([`ssh_command`]). The runner is asked first (`attach_info`): it must
//! offer an interactive CLI for the runtime, and only the working directory of its answer is
//! used, when it is the thread's directory or lies under the box's worktrees or the thread's
//! repository ([`attach_dir`]).

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures::future::{BoxFuture, FutureExt};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, watch, Notify};
use twr_core::adapter::{AdapterCmd, AdapterEvent, AdapterHandle, SpawnConfig};
use twr_core::config::BoxTransport;
use twr_core::event::GitSnapshot;
use twr_core::git::{
    DeleteRefs, GitOps, OnlyCommits, PurgeOutcome, RemoveOutcome, RestoreOutcome, RestoreRequest,
    TrashItem, TrashOutcome, TrashPurge, TrashRequest, WorktreeCreated, WorktreeInventory,
    WorktreeSpec,
};
use twr_core::proto::codes;
use twr_core::runner::{
    auth_proof, check_auth_proof, cmd_errors, decode_result, hub_errors, AttachInfo, CmdError,
    CmdResult, HubMsg, Inventory, Op, Prover, Secret, WireRemoveOutcome, WireSpawnConfig,
    WireWorktreeCreated,
};

use crate::host::{path_component, StaleProcess, ThreadHost};

/// How long a session's `spawn` / `resume` may take to be answered (across reconnections).
pub const SPAWN_TIMEOUT: Duration = Duration::from_secs(30);

/// How long a request (git, `attach_info`) may take to be answered.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// Messages a connection's send queue holds; a full queue closes the connection.
pub const SEND_QUEUE: usize = 1024;

/// The sending side of one runner connection: a bounded queue ([`SEND_QUEUE`]) its writer
/// drains, and the switch that closes the connection (with why). Cheap to clone.
#[derive(Clone)]
pub struct ConnTx {
    tx: mpsc::Sender<HubMsg>,
    close: Arc<watch::Sender<Option<String>>>,
}

impl ConnTx {
    /// A queue for `tx`, closed through `close`.
    pub fn new(tx: mpsc::Sender<HubMsg>, close: watch::Sender<Option<String>>) -> Self {
        ConnTx {
            tx,
            close: Arc::new(close),
        }
    }

    /// Queues `m`. A full queue closes the connection (its runner does not keep up); returns
    /// whether `m` was queued.
    pub fn send(&self, m: HubMsg) -> bool {
        match self.tx.try_send(m) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.close(&format!(
                    "its send queue is full ({SEND_QUEUE} messages): the runner does not read"
                ));
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }

    /// Closes the connection (the first reason wins).
    pub fn close(&self, why: &str) {
        self.close.send_if_modified(|c| {
            if c.is_none() {
                *c = Some(why.to_string());
                true
            } else {
                false
            }
        });
    }
}

/// The id of a `send` (see the module docs): `send:<thread>:<turn>` (one per turn). `None`
/// for other ops.
pub fn durable_cmd_id(op: &Op) -> Option<String> {
    match op {
        Op::Send { thread, turn, .. } => Some(send_cmd_id(thread, *turn)),
        _ => None,
    }
}

/// The id of the `send` of turn `turn` of `thread`: `send:<thread>:<turn>`.
fn send_cmd_id(thread: &str, turn: u64) -> String {
    format!("send:{thread}:{turn}")
}

/// A fresh id for a recorded `respond` to runtime generation `gen` of its thread (see
/// [`RemoteBox::expect_respond`]): `respond:<gen>:<ULID>`.
pub fn respond_cmd_id(gen: u64) -> String {
    format!("respond:{gen}:{}", twr_core::event::new_id())
}

/// The runtime generation a recorded `respond`'s id names ([`respond_cmd_id`]). `None` for
/// any other id, and for a `respond` recorded before ids named it (`respond:<ULID>`).
pub fn respond_cmd_gen(cmd_id: &str) -> Option<u64> {
    let (gen, _) = cmd_id.strip_prefix("respond:")?.split_once(':')?;
    gen.parse().ok()
}

/// Whether `cmd_id` is a recorded command's (a `send`, `respond` or `stop`, see the module
/// docs; [`twr_core::runner::is_durable_cmd_id`]).
pub fn is_durable_cmd_id(cmd_id: &str) -> bool {
    twr_core::runner::is_durable_cmd_id(cmd_id)
}

/// The id of the recorded `stop` of generation `gen` of `thread`'s runtime (see
/// [`RemoteBox::expect_stop`]): `stop:<thread>:<gen>`.
pub fn stop_cmd_id(thread: &str, gen: u64) -> String {
    format!("stop:{thread}:{gen}")
}

/// What the latest inventory of a box said about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoxInfo {
    /// Runner version.
    pub version: String,
    /// Runtimes the box offers.
    pub runtimes: Vec<String>,
    /// Where the box keeps thread worktrees.
    pub worktrees: PathBuf,
}

/// A remote box (see the module docs). Cheap to clone.
#[derive(Clone)]
pub struct RemoteBox {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for RemoteBox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteBox")
            .field("name", &self.inner.name)
            .finish()
    }
}

struct Inner {
    name: String,
    /// How the box connects: a token box's runner dials the listener, an SSH box's daemon is
    /// dialed by the hub (see the module docs).
    transport: BoxTransport,
    /// The token box's token (empty for an SSH box, and for a token box configured without
    /// one: neither is accepted on the listener).
    token: Secret,
    ssh: Option<String>,
    /// Namespace of its threads' branches (`twr/<namespace>/<name>`; `None`: `twr/<name>`).
    branch_namespace: Option<String>,
    state: Mutex<State>,
    /// Session generations (this box's own counter).
    gens: AtomicU64,
    /// Runner frames refused because they name a thread of another box.
    rejected: AtomicU64,
    /// Wakes the hub when [`State::orphans`] has ids ([`RemoteBox::orphans_changed`]).
    orphans: Notify,
    /// The ignored paths a park may delete with a worktree (`[threads] park_disposable`), sent
    /// with every worktree removal (`None`: the runner's default list), and those a deletion
    /// deletes instead of moving them to the trash, sent with its inventory and its move
    /// (`None`: none).
    disposable: Option<Vec<String>>,
    /// How long a repository change is waited for ([`twr_core::git::CHANGE_REQUEST_TIMEOUT`]).
    change_timeout: Duration,
    /// Wakes [`RemoteBox::wait_settled`] when [`State::unsettled`] shrinks or the box's
    /// readiness changes.
    settled: Notify,
    /// The latest refused connection that named this box, until one authenticates
    /// ([`RemoteBox::note_refusal`]).
    refusal: Mutex<Option<Refusal>>,
    /// Hub operations on the box under way that passed its switching gate
    /// ([`RemoteBox::begin_op`]).
    ops: Arc<AtomicUsize>,
}

/// A hub operation on a box under way ([`RemoteBox::begin_op`]): counted until dropped.
#[derive(Debug)]
pub struct BoxOp(Arc<AtomicUsize>);

impl Drop for BoxOp {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A connection naming a box that the hub refused before it authenticated: when, and why in
/// words for `twr info`. Nothing in it is proven: any peer that reaches the
/// listener can name the box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub at: chrono::DateTime<chrono::Utc>,
    pub reason: String,
}

/// The box's current connection.
struct Conn {
    epoch: u64,
    tx: ConnTx,
}

#[derive(Default)]
struct State {
    conn: Option<Conn>,
    last_epoch: u64,
    /// Synced and reconciled: commands may be sent.
    ready: bool,
    info: Option<BoxInfo>,
    /// Commands waiting for their result, in issue order.
    pending: BTreeMap<u64, Pending>,
    /// Command id -> issue order.
    ids: HashMap<String, u64>,
    next_order: u64,
    /// The session the hub runs on the box, per thread.
    sessions: HashMap<String, Slot>,
    /// Ids of recorded `respond`s the forwarders are about to issue: (thread, the runner's
    /// attention id) -> (command id, the run it is bound to), in the order they were recorded.
    /// A queue: runtimes reuse attention ids, so a second answer to the same id (to the item
    /// raised again) can be recorded before the forwarder issued the first, and each must go
    /// out under its own id (a map would give the first answer the second's id, and leave the
    /// first's record unsent for good).
    responds: HashMap<(String, String), VecDeque<ExpectedRespond>>,
    /// Ids of recorded `stop`s the forwarders are about to issue: thread -> command id.
    stops: HashMap<String, String>,
    /// Recorded `send`s the forwarders are about to issue: command id -> the run it is bound
    /// to.
    sends: HashMap<String, Option<String>>,
    /// Recorded commands a session's forwarder gave up on unfinished (cancelled: the hub
    /// started another session of the thread, or the session's first command failed), for the
    /// hub to settle while the box is connected ([`RemoteBox::take_orphans`]).
    orphans: Vec<String>,
    /// Repository changes that went out and were not answered (see the module docs): the
    /// box may still be running their git.
    unsettled: HashSet<String>,
}

/// A recorded `respond` a forwarder is about to issue: its command id and the run it is bound
/// to ([`RemoteBox::expect_respond`]).
type ExpectedRespond = (String, Option<String>);

struct Pending {
    cmd_id: String,
    op: Op,
    session: bool,
    tx: oneshot::Sender<Result<Value, CmdError>>,
    /// Sent on the current connection.
    sent: bool,
    /// Sent on some connection (a repository change that went out may run on the box).
    went_out: bool,
}

struct Slot {
    gen: u64,
    /// The session's run, when the hub named it (`spawn` / `resume`).
    run: Option<String>,
    /// The session's event stream (only a made-up exit ever goes through it).
    events: mpsc::UnboundedSender<AdapterEvent>,
    /// Set when the session is over: its forwarder stops.
    cancel: watch::Sender<bool>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Request,
    Session,
}

fn offline(name: &str) -> CmdError {
    CmdError::new(
        codes::BOX_OFFLINE,
        format!("box {name:?} is offline (its runner is not connected)"),
    )
}

impl State {
    /// The id (and run) of the oldest recorded `respond` of `thread` to `attn_id` not issued
    /// yet ([`RemoteBox::expect_respond`]).
    fn take_respond(&mut self, thread: &str, attn_id: &str) -> Option<ExpectedRespond> {
        let key = (thread.to_string(), attn_id.to_string());
        let queue = self.responds.get_mut(&key)?;
        let next = queue.pop_front();
        if queue.is_empty() {
            self.responds.remove(&key);
        }
        next
    }

    fn remove_pending(&mut self, cmd_id: &str) -> Option<Pending> {
        let order = self.ids.remove(cmd_id)?;
        self.pending.remove(&order)
    }

    /// Forgets pending command `cmd_id`, unanswered: a repository change that went out stays
    /// [`State::unsettled`].
    fn abandon_pending(&mut self, cmd_id: &str) -> Option<Pending> {
        let p = self.remove_pending(cmd_id)?;
        if p.went_out && p.op.changes_repository() {
            self.unsettled.insert(p.cmd_id.clone());
        }
        Some(p)
    }

    /// Fails every request (they never wait for a reconnection) and marks session commands
    /// unsent (they go again, same ids, once the box is ready).
    fn drop_requests(&mut self, name: &str) {
        let requests: Vec<String> = self
            .pending
            .values()
            .filter(|p| !p.session)
            .map(|p| p.cmd_id.clone())
            .collect();
        for id in requests {
            if let Some(p) = self.abandon_pending(&id) {
                let _ = p.tx.send(Err(offline(name)));
            }
        }
        for p in self.pending.values_mut() {
            p.sent = false;
        }
    }

    /// Adds a command to the pending table (not sent yet).
    fn enqueue(
        &mut self,
        cmd_id: String,
        op: Op,
        session: bool,
    ) -> oneshot::Receiver<Result<Value, CmdError>> {
        let (tx, rx) = oneshot::channel();
        self.next_order += 1;
        let order = self.next_order;
        self.ids.insert(cmd_id.clone(), order);
        self.pending.insert(
            order,
            Pending {
                cmd_id,
                op,
                session,
                tx,
                sent: false,
                went_out: false,
            },
        );
        rx
    }

    /// Sends pending command `order` on the current connection, if the box is ready.
    fn send_pending(&mut self, order: u64) {
        let (Some(conn), true) = (self.conn.as_ref(), self.ready) else {
            return;
        };
        if let Some(p) = self.pending.get_mut(&order) {
            p.sent = conn.tx.send(HubMsg::Cmd {
                cmd_id: p.cmd_id.clone(),
                op: p.op.clone(),
            });
            p.went_out |= p.sent;
        }
    }
}

impl RemoteBox {
    /// Token box `name`, whose runner authenticates with `token`; `ssh` is its `ssh`
    /// destination (for attach).
    pub fn new(name: impl Into<String>, token: Secret, ssh: Option<String>) -> Self {
        Self::with_transport(name.into(), BoxTransport::Token, token, ssh)
    }

    /// SSH box `name`, which the hub reaches with `ssh` (its destination, also used for
    /// attach). It has no token: the listener refuses every connection naming it.
    pub fn new_ssh(name: impl Into<String>, ssh: String) -> Self {
        Self::with_transport(name.into(), BoxTransport::Ssh, Secret::default(), Some(ssh))
    }

    fn with_transport(
        name: String,
        transport: BoxTransport,
        token: Secret,
        ssh: Option<String>,
    ) -> Self {
        RemoteBox {
            inner: Arc::new(Inner {
                name,
                transport,
                token,
                ssh,
                branch_namespace: None,
                state: Mutex::new(State::default()),
                gens: AtomicU64::new(1),
                rejected: AtomicU64::new(0),
                orphans: Notify::new(),
                disposable: None,
                change_timeout: twr_core::git::CHANGE_REQUEST_TIMEOUT,
                settled: Notify::new(),
                refusal: Mutex::new(None),
                ops: Arc::default(),
            }),
        }
    }

    /// How long a repository change is waited for (tests shorten it; default
    /// [`twr_core::git::CHANGE_REQUEST_TIMEOUT`]). Set before the box is shared.
    pub fn with_change_timeout(mut self, timeout: Duration) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("the box is configured before it is shared")
            .change_timeout = timeout;
        self
    }

    /// Namespace of the branches of new worktrees on this box (`twr/<namespace>/<name>`;
    /// `None`, the default: `twr/<name>`), for creation and restore alike. An SSH box gets the
    /// hub's own box's (see the module docs). Set before the box is shared.
    pub fn with_branch_namespace(mut self, namespace: Option<String>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("the box is configured before it is shared")
            .branch_namespace = namespace;
        self
    }

    /// How the box connects (see the module docs).
    pub fn transport(&self) -> BoxTransport {
        self.inner.transport
    }

    /// Repository changes on the box whose outcome is unknown (see the module docs).
    pub fn unsettled(&self) -> usize {
        self.state().unsettled.len()
    }

    /// A hub operation on the box begins that checked its switching gate and then awaits the
    /// box (a new thread's workspace, a deletion, a trash restore, an attach's question):
    /// counted until the returned guard is dropped, on every way out. The hub takes it under
    /// its core lock in the same step as the gate check (`Core::reserve_box`), and a box with
    /// any is not quiet (`Core::box_quiet`), so no switch begins while one runs, and none
    /// starts once one began.
    pub fn begin_op(&self) -> BoxOp {
        self.inner.ops.fetch_add(1, Ordering::SeqCst);
        BoxOp(Arc::clone(&self.inner.ops))
    }

    /// Hub operations on the box under way ([`RemoteBox::begin_op`]).
    pub fn ops_in_progress(&self) -> usize {
        self.inner.ops.load(Ordering::SeqCst)
    }

    /// Repository changes on the box waiting for their answer or unsettled: what may still
    /// change the box's repositories ([`Op::changes_repository`], see the module docs).
    pub fn repository_changes(&self) -> usize {
        let st = self.state();
        st.pending
            .values()
            .filter(|p| !p.session && p.op.changes_repository())
            .count()
            + st.unsettled.len()
    }

    /// Closes the box's current connection, if it has one, saying `why`: it ends as a lost
    /// connection does (the box goes offline, and its runner keeps everything for the next).
    pub fn close_connection(&self, why: &str) {
        if let Some(c) = &self.state().conn {
            c.tx.close(why);
        }
    }

    /// Waits until no repository change of the hub may still run on the box (see the module
    /// docs). Fails `box_offline` while the box is not ready (no git op could run then
    /// anyway), and after the box's change timeout.
    pub async fn wait_settled(&self) -> Result<(), CmdError> {
        let deadline = tokio::time::Instant::now() + self.inner.change_timeout;
        loop {
            let changed = self.inner.settled.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let st = self.state();
                if st.unsettled.is_empty() {
                    return Ok(());
                }
                if !st.ready {
                    return Err(offline(&self.inner.name));
                }
            }
            tokio::select! {
                () = &mut changed => {}
                () = tokio::time::sleep_until(deadline) => {
                    return Err(CmdError::new(
                        cmd_errors::FAILED,
                        format!(
                            "an earlier repository change on box {:?} has not been answered \
                             (its outcome is unknown and its git may still run there); try again \
                             once the box answers it or reconnects",
                            self.inner.name
                        ),
                    ));
                }
            }
        }
    }

    /// The ignored paths a park may delete with a worktree on this box (`[threads]
    /// park_disposable`): sent with its worktree removals, and with deletions (their
    /// inventories and moves to the trash), which delete them. Set before the box is shared.
    pub fn with_park_disposable(mut self, disposable: Vec<String>) -> Self {
        Arc::get_mut(&mut self.inner)
            .expect("the box is configured before it is shared")
            .disposable = Some(disposable);
        self
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Box name.
    pub fn name(&self) -> &str {
        &self.inner.name
    }

    /// The hub's handshake proof for this box over the runner's and the hub's nonces
    /// ([`twr_core::runner::auth_proof`] by [`Prover::Hub`]); `None` for a box without a token
    /// (it accepts no runner on the listener), an SSH box among them.
    pub fn hub_proof(&self, runner_nonce: &str, hub_nonce: &str) -> Option<String> {
        let token_box = self.inner.transport == BoxTransport::Token;
        (token_box && !self.inner.token.is_empty()).then(|| {
            auth_proof(
                &self.inner.token,
                Prover::Hub,
                &self.inner.name,
                runner_nonce,
                hub_nonce,
            )
        })
    }

    /// Whether `proof` is the runner's handshake proof for this box over these nonces
    /// (constant time; a box without a token accepts none, an SSH box none either).
    pub fn checks_runner_proof(&self, runner_nonce: &str, hub_nonce: &str, proof: &str) -> bool {
        self.inner.transport == BoxTransport::Token
            && check_auth_proof(
                &self.inner.token,
                Prover::Runner,
                &self.inner.name,
                runner_nonce,
                hub_nonce,
                proof,
            )
    }

    /// What the latest inventory said (kept while the box is offline).
    pub fn info(&self) -> Option<BoxInfo> {
        self.state().info.clone()
    }

    /// Whether the box is connected, synced and reconciled.
    pub fn is_ready(&self) -> bool {
        self.state().ready
    }

    /// Whether `epoch` is the current connection.
    pub fn is_current(&self, epoch: u64) -> bool {
        self.state().conn.as_ref().is_some_and(|c| c.epoch == epoch)
    }

    /// Counts a runner frame refused because it names a thread of another box.
    pub fn note_rejected(&self) {
        self.inner.rejected.fetch_add(1, Ordering::Relaxed);
    }

    /// Runner frames refused because they named a thread of another box.
    pub fn rejected_frames(&self) -> u64 {
        self.inner.rejected.load(Ordering::Relaxed)
    }

    /// Records that a connection naming this box was refused before it authenticated, and
    /// why (shown while the box is offline, so a runner that keeps being refused is visible
    /// on the hub's side too). The next authenticated connection clears it.
    pub fn note_refusal(&self, reason: impl Into<String>) {
        *self.refusal_slot() = Some(Refusal {
            at: chrono::Utc::now(),
            reason: reason.into(),
        });
    }

    /// The latest refusal [`RemoteBox::note_refusal`] recorded since a connection of this box
    /// last authenticated.
    pub fn refusal(&self) -> Option<Refusal> {
        self.refusal_slot().clone()
    }

    fn refusal_slot(&self) -> MutexGuard<'_, Option<Refusal>> {
        self.inner.refusal.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The operation `cmd` of thread `thread`'s session is on the box (a `respond` already
    /// carries the runtime's own attention id, see the module docs).
    pub fn session_op(&self, thread: &str, cmd: AdapterCmd) -> Option<Op> {
        let thread = thread.to_string();
        Some(match cmd {
            AdapterCmd::Send { text, turn } => Op::Send {
                thread,
                turn,
                text,
                run: None,
            },
            AdapterCmd::Respond { attn_id, answer } => Op::Respond {
                attn_id,
                thread,
                answer,
                run: None,
            },
            AdapterCmd::Stop => Op::Stop { thread, run: None },
            AdapterCmd::Interrupt => {
                tracing::warn!(%thread, "remote turn interruption is unsupported; session retained");
                return None;
            }
        })
    }

    /// The next `respond` of `thread` to the runner's attention `attn_id` is recorded as
    /// `cmd_id` ([`respond_cmd_id`]) for run `run` (the run that raised the item, when known):
    /// the session's forwarder issues it under that id, bound to that run.
    pub fn expect_respond(&self, thread: &str, attn_id: &str, cmd_id: &str, run: Option<String>) {
        self.state()
            .responds
            .entry((thread.to_string(), attn_id.to_string()))
            .or_default()
            .push_back((cmd_id.to_string(), run));
    }

    /// Withdraws [`RemoteBox::expect_respond`]'s `cmd_id` for `thread`'s `attn_id`: no
    /// forwarder will issue it (the runtime was gone when it was answered).
    pub fn forget_respond(&self, thread: &str, attn_id: &str, cmd_id: &str) {
        let mut st = self.state();
        let key = (thread.to_string(), attn_id.to_string());
        if let Some(queue) = st.responds.get_mut(&key) {
            queue.retain(|(id, _)| id != cmd_id);
            if queue.is_empty() {
                st.responds.remove(&key);
            }
        }
    }

    /// The recorded `send` `cmd_id` ([`durable_cmd_id`]) is bound to run `run` (the session's,
    /// when known): the session's forwarder issues it bound to that run.
    pub fn expect_send(&self, cmd_id: &str, run: Option<String>) {
        self.state().sends.insert(cmd_id.to_string(), run);
    }

    /// The next `stop` of `thread`'s session is recorded as `cmd_id` ([`stop_cmd_id`]): the
    /// session's forwarder issues it under that id.
    pub fn expect_stop(&self, thread: &str, cmd_id: &str) {
        self.state()
            .stops
            .insert(thread.to_string(), cmd_id.to_string());
    }

    /// Takes a new authenticated connection (see the module docs): fences the previous one,
    /// fails requests in flight, and returns the new epoch. Not ready yet. `None` (nothing
    /// changed; the connection is to be refused) once the hub has numbered `u64::MAX`
    /// connections of this box.
    pub fn connect(&self, inv: &Inventory, tx: ConnTx) -> Option<u64> {
        *self.refusal_slot() = None;
        let mut st = self.state();
        // The hub's own counter, never the runner's `epoch_hint` (see the module docs).
        let epoch = st.last_epoch.checked_add(1)?;
        if let Some(old) = st.conn.take() {
            tracing::warn!(
                box_name = %self.inner.name,
                epoch = old.epoch,
                "a new connection of the box replaces the current one"
            );
            old.tx.send(HubMsg::Error {
                code: hub_errors::FENCED.into(),
                message: "a newer connection of this box replaced this one".into(),
            });
            old.tx.close("fenced by a newer connection");
        }
        st.drop_requests(&self.inner.name);
        // A runner connects only once it has finished every repository change it received (a
        // restarted one runs none, and two runners of one box never run at once on one
        // machine, `twr_runner::LocalBox::open`): nothing of the unanswered ones runs any more.
        if !st.unsettled.is_empty() {
            tracing::info!(box_name = %self.inner.name, changes = st.unsettled.len(), "the box reconnected: its unanswered repository changes are over (outcome unknown)");
            st.unsettled.clear();
        }
        st.last_epoch = epoch;
        st.conn = Some(Conn { epoch, tx });
        st.ready = false;
        st.info = Some(BoxInfo {
            version: inv.version.clone(),
            runtimes: inv.runtimes.clone(),
            worktrees: inv.worktrees.clone(),
        });
        drop(st);
        self.inner.settled.notify_waiters();
        Some(epoch)
    }

    /// The box is synced and reconciled on connection `epoch`. In this order: stops the
    /// stray sessions `strays` (`(thread, run)`), then sends the session commands that wait
    /// for the box (in order, same ids), `resend` included (recorded commands a previous hub
    /// may not have delivered: `(cmd_id, op)`, skipped when already waiting). Returns false
    /// (and does nothing) when `epoch` is not current.
    ///
    /// `resend` is every recorded command still eligible to go out (see
    /// `Core::remote_resends`); the same rule applies to those waiting here: a recorded
    /// (durable-id) session command that is not in `resend` expired or no longer applies, so it
    /// is dropped unsent and its waiter gets an error. Commands without a record (`spawn` /
    /// `resume`, bounded by their own timeout) still go.
    pub fn set_ready(
        &self,
        epoch: u64,
        strays: &[(String, Option<String>)],
        resend: Vec<(String, Op)>,
    ) -> bool {
        let mut st = self.state();
        if !st.conn.as_ref().is_some_and(|c| c.epoch == epoch) {
            return false;
        }
        st.ready = true;
        for (thread, run) in strays {
            let op = Op::Stop {
                thread: thread.clone(),
                run: run.clone(),
            };
            let cmd_id = twr_core::event::new_id();
            let rx = st.enqueue(cmd_id.clone(), op, false);
            let order = st.next_order;
            st.send_pending(order);
            tokio::spawn(self.clone().await_stray_stop(thread.clone(), cmd_id, rx));
        }
        let eligible: HashSet<&str> = resend.iter().map(|(id, _)| id.as_str()).collect();
        let obsolete: Vec<String> = st
            .pending
            .values()
            .filter(|p| {
                p.session
                    && !p.sent
                    && is_durable_cmd_id(&p.cmd_id)
                    && !eligible.contains(p.cmd_id.as_str())
            })
            .map(|p| p.cmd_id.clone())
            .collect();
        for id in obsolete {
            if let Some(p) = st.remove_pending(&id) {
                tracing::warn!(box_name = %self.inner.name, cmd_id = %id, "not sending a command whose record expired or no longer applies");
                let _ = p.tx.send(Err(CmdError::new(
                    cmd_errors::FAILED,
                    format!(
                        "the {} for box {:?} expired or no longer applies; it is not sent again",
                        p.op.name(),
                        self.inner.name
                    ),
                )));
            }
        }
        for (cmd_id, op) in resend {
            if !st.ids.contains_key(&cmd_id) {
                tracing::info!(box_name = %self.inner.name, cmd_id, "sending a recorded command again");
                // Nobody waits for its result: the ingest path forgets the record.
                drop(st.enqueue(cmd_id, op, true));
            }
        }
        let unsent: Vec<u64> = st
            .pending
            .iter()
            .filter(|(_, p)| !p.sent)
            .map(|(o, _)| *o)
            .collect();
        for order in unsent {
            st.send_pending(order);
        }
        true
    }

    /// Waits until a session's forwarder gave up on recorded commands unfinished (see
    /// [`RemoteBox::take_orphans`]).
    pub async fn orphans_changed(&self) {
        self.inner.orphans.notified().await;
    }

    /// The ids of the recorded commands sessions' forwarders gave up on unfinished (a stop
    /// queued behind a command in flight when the hub started another session of the thread,
    /// ...): their records stay (`remote_outbox`) and would wait for the next connection, so
    /// the hub settles them now (`Core::remote_resends`: forgotten when they no longer apply,
    /// else sent again with [`RemoteBox::resend`]).
    pub fn take_orphans(&self) -> Vec<String> {
        std::mem::take(&mut self.state().orphans)
    }

    /// Sends recorded commands `resend` (`(cmd_id, op)`) again now, when the box is ready (and
    /// each is not already waiting): nobody waits for their results (the ingest path retires
    /// the records). Returns false (nothing sent) when the box is not ready: the next
    /// [`RemoteBox::set_ready`] sends what still applies then.
    pub fn resend(&self, resend: Vec<(String, Op)>) -> bool {
        let mut st = self.state();
        if !st.ready {
            return false;
        }
        for (cmd_id, op) in resend {
            if !st.ids.contains_key(&cmd_id) {
                tracing::info!(box_name = %self.inner.name, cmd_id, "sending a recorded command again");
                drop(st.enqueue(cmd_id, op, true));
                let order = st.next_order;
                st.send_pending(order);
            }
        }
        true
    }

    /// Records that a forwarder gave up on `ids` (see [`RemoteBox::take_orphans`]); only the
    /// recorded ones count.
    fn orphan(&self, ids: impl IntoIterator<Item = String>) {
        let mut ids: Vec<String> = ids.into_iter().filter(|i| is_durable_cmd_id(i)).collect();
        if ids.is_empty() {
            return;
        }
        tracing::info!(box_name = %self.inner.name, ?ids, "a session's forwarder gave up on recorded commands");
        self.state().orphans.append(&mut ids);
        self.inner.orphans.notify_one();
    }

    /// The ids the commands still queued in a forwarder's `cmds` would have been issued under
    /// ([`RemoteBox::begin`]); drains the queue and forgets the ids the forwarder was told.
    fn drain_ids(
        &self,
        thread: &str,
        cmds: &mut mpsc::UnboundedReceiver<AdapterCmd>,
    ) -> Vec<String> {
        let mut ids = Vec::new();
        let mut st = self.state();
        while let Ok(cmd) = cmds.try_recv() {
            let id = match cmd {
                AdapterCmd::Send { turn, .. } => {
                    let id = send_cmd_id(thread, turn);
                    st.sends.remove(&id);
                    Some(id)
                }
                AdapterCmd::Respond { attn_id, .. } => {
                    st.take_respond(thread, &attn_id).map(|(id, _)| id)
                }
                AdapterCmd::Stop => st.stops.remove(thread),
                AdapterCmd::Interrupt => None,
            };
            ids.extend(id);
        }
        ids
    }

    /// Connection `epoch` is gone. Returns whether it was the current one (the box is then
    /// offline).
    pub fn disconnect(&self, epoch: u64) -> bool {
        let mut st = self.state();
        if !st.conn.as_ref().is_some_and(|c| c.epoch == epoch) {
            return false;
        }
        st.conn = None;
        st.ready = false;
        st.drop_requests(&self.inner.name);
        drop(st);
        self.inner.settled.notify_waiters();
        true
    }

    /// A command's result arrived on connection `epoch`.
    pub fn on_result(&self, epoch: u64, r: CmdResult) {
        let mut st = self.state();
        // An answer, on whatever connection, means the change's git is over.
        if st.unsettled.remove(&r.cmd_id) {
            tracing::info!(box_name = %self.inner.name, cmd_id = %r.cmd_id, "an unanswered repository change was answered after all");
            self.inner.settled.notify_waiters();
        }
        if !st.conn.as_ref().is_some_and(|c| c.epoch == epoch) {
            return;
        }
        match st.remove_pending(&r.cmd_id) {
            Some(p) => {
                let _ = p.tx.send(r.into_outcome());
            }
            None => tracing::debug!(cmd_id = %r.cmd_id, "result of a command nobody waits for"),
        }
    }

    /// Issues `op` (sent now if the box is ready) under its recorded id (see the module
    /// docs), or a fresh one.
    /// With `on`, only while that connection is the box's current one (checked under the
    /// state lock, where [`RemoteBox::connect`] replaces it).
    fn begin(
        &self,
        mut op: Op,
        kind: Kind,
        on: Option<u64>,
    ) -> Result<(String, oneshot::Receiver<Result<Value, CmdError>>), CmdError> {
        let mut st = self.state();
        if kind == Kind::Request && !st.ready {
            return Err(offline(&self.inner.name));
        }
        if on.is_some_and(|e| !st.conn.as_ref().is_some_and(|c| c.epoch == e)) {
            return Err(CmdError::new(
                cmd_errors::FAILED,
                format!(
                    "the connection of box {:?} that asked for it was replaced",
                    self.inner.name
                ),
            ));
        }
        let cmd_id = match &mut op {
            // Sent as recorded: bound to the run that raised the item.
            Op::Respond {
                thread,
                attn_id,
                run,
                ..
            } => st.take_respond(thread, attn_id).map(|(id, r)| {
                *run = r;
                id
            }),
            Op::Stop { thread, run: None } if kind == Kind::Session => st.stops.remove(thread),
            // Sent as recorded: bound to the session's run.
            Op::Send {
                thread, turn, run, ..
            } => {
                let id = send_cmd_id(thread, *turn);
                if let Some(r) = st.sends.remove(&id) {
                    *run = r;
                }
                Some(id)
            }
            _ => durable_cmd_id(&op),
        }
        .unwrap_or_else(twr_core::event::new_id);
        // A recorded command a reconnection already sent again: its result is awaited here.
        if let Some(&order) = st.ids.get(&cmd_id) {
            let (tx, rx) = oneshot::channel();
            if let Some(p) = st.pending.get_mut(&order) {
                p.tx = tx;
            }
            return Ok((cmd_id, rx));
        }
        let rx = st.enqueue(cmd_id.clone(), op, kind == Kind::Session);
        let order = st.next_order;
        st.send_pending(order);
        Ok((cmd_id, rx))
    }

    /// Runs `op` on the box and waits for its result (see the module docs for the kinds).
    async fn call(&self, op: Op, kind: Kind) -> Result<Value, CmdError> {
        self.call_on(op, kind, None).await
    }

    /// [`RemoteBox::call`], issued only while connection `on` (when given) is the current one.
    async fn call_on(&self, op: Op, kind: Kind, on: Option<u64>) -> Result<Value, CmdError> {
        let what = op.name();
        let op_changes = op.changes_repository();
        let (cmd_id, rx) = self.begin(op, kind, on)?;
        // Forgets the command if this future is dropped or times out: it is not sent again (a
        // repository change that went out stays unsettled).
        let _guard = PendingGuard {
            rbox: self.clone(),
            cmd_id,
        };
        let r = match kind {
            Kind::Request => {
                let (limit, change) = if op_changes {
                    (self.inner.change_timeout, true)
                } else {
                    (REQUEST_TIMEOUT, false)
                };
                match tokio::time::timeout(limit, rx).await {
                    Ok(r) => r,
                    Err(_) => {
                        return Err(CmdError::new(
                            cmd_errors::FAILED,
                            format!(
                                "box {:?} did not answer {what} within {}s{}",
                                self.inner.name,
                                limit.as_secs_f64(),
                                if change {
                                    "; its outcome is unknown, and no other git op runs on the \
                                     box until it answers it or reconnects"
                                } else {
                                    ""
                                }
                            ),
                        ));
                    }
                }
            }
            Kind::Session => rx.await,
        };
        r.unwrap_or_else(|_| Err(offline(&self.inner.name)))
    }

    /// Asks the runner to stop run `run` (whatever runs, when `None`) of a session the hub
    /// does not run (a stray), in the background, on connection `epoch` (the one whose frame
    /// showed the stray): once a newer connection replaced it, nothing is sent (that
    /// connection's reconciliation decides what is a stray, and the hub may have taken the
    /// session over meanwhile).
    pub fn stop_stray(&self, epoch: u64, thread: &str, run: Option<String>) {
        tracing::warn!(box_name = %self.inner.name, thread, ?run, "stopping a session the hub does not run");
        let (rbox, thread) = (self.clone(), thread.to_string());
        tokio::spawn(async move {
            let op = Op::Stop {
                thread: thread.clone(),
                run,
            };
            if let Err(e) = rbox.call_on(op, Kind::Request, Some(epoch)).await {
                tracing::warn!(thread = %thread, error = %e, "stopping a stray session failed");
            }
        });
    }

    /// Waits for the result of a stray's stop issued by [`RemoteBox::set_ready`].
    async fn await_stray_stop(
        self,
        thread: String,
        cmd_id: String,
        rx: oneshot::Receiver<Result<Value, CmdError>>,
    ) {
        tracing::warn!(box_name = %self.inner.name, thread, "stopping a session the hub does not run");
        let _guard = PendingGuard {
            rbox: self.clone(),
            cmd_id,
        };
        let r = match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(offline(&self.inner.name)),
            Err(_) => Err(CmdError::new(cmd_errors::FAILED, "no answer")),
        };
        if let Err(e) = r {
            tracing::warn!(thread = %thread, error = %e, "stopping a stray session failed");
        }
    }

    /// A handle on thread `thread`'s session on the box: `first` (`spawn` / `resume`) starts
    /// it; `None` takes over a session the runner already runs (after a hub restart).
    pub fn start_session(&self, thread: &str, first: Option<Op>) -> AdapterHandle {
        let (handle, end) = AdapterHandle::pair();
        let gen = self.inner.gens.fetch_add(1, Ordering::Relaxed);
        let (cancel, cancelled) = watch::channel(false);
        let run = match &first {
            Some(Op::Spawn { run, .. } | Op::Resume { run, .. }) => run.clone(),
            _ => None,
        };
        let old = self.state().sessions.insert(
            thread.to_string(),
            Slot {
                gen,
                run,
                events: end.events,
                cancel,
            },
        );
        if let Some(old) = old {
            let _ = old.cancel.send(true);
        }
        tokio::spawn(
            self.clone()
                .forward(thread.to_string(), gen, first, end.cmds, cancelled),
        );
        handle
    }

    /// The run the hub named for `thread`'s current session ([`RemoteBox::start_session`]),
    /// if any.
    pub fn session_run(&self, thread: &str) -> Option<String> {
        self.state()
            .sessions
            .get(thread)
            .and_then(|s| s.run.clone())
    }

    /// The session's first command failed: it never started as far as the hub knows. Its
    /// handle reports an exit with `why`, and the session is forgotten.
    fn fail_session(&self, thread: &str, gen: u64, why: String) {
        let slot = {
            let mut st = self.state();
            match st.sessions.get(thread) {
                Some(s) if s.gen == gen => st.sessions.remove(thread),
                _ => None,
            }
        };
        if let Some(slot) = slot {
            tracing::warn!(box_name = %self.inner.name, thread, error = %why, "a remote session failed to start");
            let _ = slot.events.send(AdapterEvent::Exited {
                code: None,
                stderr_tail: why,
            });
            let _ = slot.cancel.send(true);
        }
    }

    /// The hub let go of the session (its handle is dropped): forgets it.
    fn end_session(&self, thread: &str, gen: u64) {
        let mut st = self.state();
        if st.sessions.get(thread).is_some_and(|s| s.gen == gen) {
            st.sessions.remove(thread);
        }
    }

    /// The session's forwarder: its first command, then the hub's commands in order.
    async fn forward(
        self,
        thread: String,
        gen: u64,
        first: Option<Op>,
        mut cmds: mpsc::UnboundedReceiver<AdapterCmd>,
        mut cancelled: watch::Receiver<bool>,
    ) {
        if let Some(op) = first {
            let what = op.name();
            let r = tokio::select! {
                r = tokio::time::timeout(SPAWN_TIMEOUT, self.call(op, Kind::Session)) => r,
                _ = cancelled.wait_for(|c| *c) => {
                    let ids = self.drain_ids(&thread, &mut cmds);
                    self.orphan(ids);
                    return;
                }
            };
            let why = match r {
                Ok(Ok(_)) => None,
                Ok(Err(e)) if e.code == cmd_errors::INTERRUPTED => Some(format!(
                    "twr: {what} on box {} was interrupted (the runner restarted while running \
                     it; its effect is unknown, so it is not retried): {}",
                    self.inner.name, e.message
                )),
                Ok(Err(e)) => Some(format!(
                    "twr: {what} on box {} failed: {e}",
                    self.inner.name
                )),
                Err(_) => Some(format!(
                    "twr: box {} did not answer {what} within {}s",
                    self.inner.name,
                    SPAWN_TIMEOUT.as_secs()
                )),
            };
            if let Some(why) = why {
                self.fail_session(&thread, gen, why);
                // What waited behind the first command is not sent (the session never
                // started); the hub settles its records.
                let ids = self.drain_ids(&thread, &mut cmds);
                self.orphan(ids);
                return;
            }
        }
        loop {
            let cmd = tokio::select! {
                c = cmds.recv() => c,
                _ = cancelled.wait_for(|c| *c) => {
                    let ids = self.drain_ids(&thread, &mut cmds);
                    self.orphan(ids);
                    None
                }
            };
            let Some(cmd) = cmd else {
                break;
            };
            let Some(op) = self.session_op(&thread, cmd) else {
                continue;
            };
            let what = op.name();
            // (A session command is issued whatever the box's state: it waits for the box.)
            let (cmd_id, rx) = match self.begin(op, Kind::Session, None) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(box_name = %self.inner.name, thread = %thread, op = what, error = %e, "a session command could not be issued");
                    continue;
                }
            };
            let guard = PendingGuard {
                rbox: self.clone(),
                cmd_id: cmd_id.clone(),
            };
            let r = tokio::select! {
                r = rx => r.unwrap_or_else(|_| Err(offline(&self.inner.name))),
                _ = cancelled.wait_for(|c| *c) => {
                    // Cancelled (another session of the thread started): the command in
                    // flight and those queued behind it are not sent by this forwarder; their
                    // records (if any) are settled by the hub now, not at the next connection.
                    drop(guard);
                    let mut ids = vec![cmd_id];
                    ids.extend(self.drain_ids(&thread, &mut cmds));
                    self.orphan(ids);
                    break;
                }
            };
            drop(guard);
            if let Err(e) = r {
                // Every session command here is recorded (`send`, `respond`, `thread.stop`'s
                // `stop`) except the stops of an attach or a park (whose waits time out), of a
                // runtime the hub gave up on (a session command: it waits for the box),
                // and the repeat of a recorded stop for a session taken over (the record goes
                // again itself). A recorded command's
                // error is acted on where the runner's answer arrives, in the hub
                // (`hub::runners`, `Core::failed_remote_cmd`): its turn ends `error`, a
                // runtime left waiting is stopped, a failed stop is sent again. Errors made
                // up here (the record expired or no longer applies) follow the record's own
                // fate (`Core::remote_resends`). `not_running`: the session is gone; its
                // `exited` frame (or the reconciliation after a runner restart) closes the
                // thread.
                tracing::warn!(box_name = %self.inner.name, thread = %thread, op = what, error = %e, "a session command failed");
            }
        }
        self.end_session(&thread, gen);
    }

    /// The interactive command of `twr attach` for a thread of this box (see the module docs):
    /// `local`, the command the hub's own adapter for `runtime` builds for `session_id` on
    /// another box (`Adapter::remote_attach_command`), run in
    /// the directory [`attach_dir`] settles on, wrapped as `ssh -t <ssh> -- <command>`
    /// ([`ssh_command`]). `repo` is the thread's repository on the box (`thread.created`).
    /// Returns the command and that directory on the box. `None` when the runner has no
    /// interactive CLI for the runtime. `unsupported` without `[boxes.NAME] ssh`.
    pub async fn attach_command(
        &self,
        runtime: &str,
        cfg: &SpawnConfig,
        session_id: &str,
        local: &Command,
        repo: Option<&Path>,
    ) -> Result<Option<(Command, Option<PathBuf>)>, CmdError> {
        let Some(ssh) = self.inner.ssh.clone().filter(|s| !s.trim().is_empty()) else {
            return Err(CmdError::new(
                codes::UNSUPPORTED,
                format!(
                    "set [boxes.{}] ssh = \"user@host\" to attach to threads on that box",
                    self.inner.name
                ),
            ));
        };
        let mut wire = WireSpawnConfig::from(cfg.clone());
        // Nothing of the hub's environment but the runtime spec's attach template (the
        // runner's adapter builds the attach command from it; the runner keeps the same keys).
        twr_adapters::retain_attach_env(&mut wire.env);
        let v = self
            .call(
                Op::AttachInfo {
                    thread: cfg.thread.clone(),
                    runtime: runtime.to_string(),
                    cfg: wire,
                    session_id: session_id.to_string(),
                },
                Kind::Request,
            )
            .await?;
        let Some(answer) = decode_result::<Option<AttachInfo>>(v)? else {
            return Ok(None);
        };
        let worktrees = self.info().map(|i| i.worktrees);
        let dir = attach_dir(answer.cwd.as_deref(), &cfg.cwd, worktrees.as_deref(), repo);
        let mut info = AttachInfo::from_command(local);
        info.cwd = dir.clone();
        let cmd = ssh_command(&ssh, &info, None).map_err(|why| {
            CmdError::new(
                codes::BAD_REQUEST,
                format!("[boxes.{}] ssh = {ssh:?}: {why}", self.inner.name),
            )
        })?;
        Ok(Some((cmd, dir)))
    }
}

/// Removes a command from the pending table when its caller stops waiting.
struct PendingGuard {
    rbox: RemoteBox,
    cmd_id: String,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.rbox.state().abandon_pending(&self.cmd_id);
    }
}

impl ThreadHost for RemoteBox {
    fn has_runtime(&self, runtime: &str) -> bool {
        self.state()
            .info
            .as_ref()
            .is_some_and(|i| i.runtimes.iter().any(|r| r == runtime))
    }

    fn spawn(
        &self,
        runtime: &str,
        cfg: SpawnConfig,
        prompt: Option<String>,
    ) -> anyhow::Result<AdapterHandle> {
        let thread = cfg.thread.clone();
        let op = Op::Spawn {
            thread: thread.clone(),
            runtime: runtime.to_string(),
            cfg: cfg.into(),
            prompt,
            run: Some(twr_core::event::new_id()),
        };
        Ok(self.start_session(&thread, Some(op)))
    }

    fn resume(
        &self,
        runtime: &str,
        cfg: SpawnConfig,
        session_id: String,
    ) -> anyhow::Result<AdapterHandle> {
        let thread = cfg.thread.clone();
        let op = Op::Resume {
            thread: thread.clone(),
            runtime: runtime.to_string(),
            cfg: cfg.into(),
            session_id,
            run: Some(twr_core::event::new_id()),
        };
        Ok(self.start_session(&thread, Some(op)))
    }

    fn worktree_branch(&self, name: &str) -> String {
        match &self.inner.branch_namespace {
            Some(ns) => format!("twr/{ns}/{name}"),
            None => twr_core::git::thread_branch(name),
        }
    }

    fn attach_command(
        &self,
        _runtime: &str,
        _cfg: &SpawnConfig,
        _session_id: &str,
    ) -> anyhow::Result<Option<Command>> {
        anyhow::bail!(
            "attaching to a thread on box {:?} goes through its runner (RemoteBox::attach_command)",
            self.inner.name
        )
    }

    fn remote_attach_command(
        &self,
        runtime: &str,
        cfg: &SpawnConfig,
        session_id: &str,
    ) -> anyhow::Result<Option<Command>> {
        // The hub's own box builds the commands for others (this one's: `attach_command`).
        ThreadHost::attach_command(self, runtime, cfg, session_id)
    }

    fn git(&self) -> Arc<dyn GitOps> {
        Arc::new(RemoteGit(self.clone()))
    }

    fn worktree_path(&self, project: &str, name: &str) -> PathBuf {
        let base = self
            .state()
            .info
            .as_ref()
            .map(|i| i.worktrees.clone())
            .unwrap_or_default();
        base.join(path_component(project)).join(name)
    }

    fn process_start_time(&self, _pid: u32) -> Option<u64> {
        None
    }

    fn reap_stale(&self, _p: &StaleProcess) -> bool {
        // The box's runner reaps its own runtimes (twr_runner::reap).
        false
    }

    fn runtime_running(
        &self,
        _thread: &str,
        _p: Option<&StaleProcess>,
        _min_start: Option<u64>,
        _generation: Option<u64>,
    ) -> bool {
        // The box's runner reports its runtimes' exits (an `exited` frame, or a running list
        // without the session when it reconnects).
        false
    }

    fn is_remote(&self) -> bool {
        true
    }
}

/// The git of a remote box: every call is a request to its runner.
struct RemoteGit(RemoteBox);

fn cmd_err(name: &str, e: CmdError) -> anyhow::Error {
    anyhow::Error::new(e).context(format!("on box {name:?}"))
}

impl RemoteGit {
    fn run<T: serde::de::DeserializeOwned + Send + 'static>(
        &self,
        op: Op,
    ) -> BoxFuture<'static, anyhow::Result<T>> {
        let rbox = self.0.clone();
        async move {
            // Only status and the trash's listing (shown, never acted on as they are) go while
            // an earlier repository change may still run on the box.
            if !matches!(op, Op::GitSnapshot { .. } | Op::TrashList {}) {
                rbox.wait_settled()
                    .await
                    .map_err(|e| cmd_err(rbox.name(), e))?;
            }
            let v = rbox
                .call(op, Kind::Request)
                .await
                .map_err(|e| cmd_err(rbox.name(), e))?;
            decode_result(v).map_err(|e| cmd_err(rbox.name(), e))
        }
        .boxed()
    }
}

/// The orchestrator's reads of committed files (`hub::project_read`) run on the hub's own box
/// only: the runner protocol has no op for them yet.
fn not_on_remote_boxes<T: Send + 'static>() -> BoxFuture<'static, anyhow::Result<T>> {
    async {
        Err(anyhow::anyhow!(
            "reading committed files is not available on remote boxes yet"
        ))
    }
    .boxed()
}

impl GitOps for RemoteGit {
    fn repo_root(&self, dir: PathBuf) -> BoxFuture<'static, anyhow::Result<Option<PathBuf>>> {
        self.run(Op::RepoRoot { dir })
    }

    fn git_common_dir(&self, dir: PathBuf) -> BoxFuture<'static, anyhow::Result<PathBuf>> {
        self.run(Op::GitCommonDir { dir })
    }

    fn worktree_create(
        &self,
        spec: WorktreeSpec,
    ) -> BoxFuture<'static, anyhow::Result<WorktreeCreated>> {
        let f = self.run::<WireWorktreeCreated>(Op::worktree_create(spec));
        async move { f.await.map(WorktreeCreated::from) }.boxed()
    }

    fn worktree_remove(
        &self,
        repo: PathBuf,
        path: PathBuf,
        branch: Option<String>,
    ) -> BoxFuture<'static, anyhow::Result<RemoveOutcome>> {
        let f = self.run::<WireRemoveOutcome>(Op::WorktreeRemove {
            repo,
            path,
            branch,
            disposable: self.0.inner.disposable.clone(),
        });
        async move { f.await.map(RemoveOutcome::from) }.boxed()
    }

    fn worktree_prune(&self, repo: PathBuf) -> BoxFuture<'static, anyhow::Result<()>> {
        let f = self.run::<Value>(Op::WorktreePrune { repo });
        async move { f.await.map(|_| ()) }.boxed()
    }

    fn worktree_exists(&self, path: PathBuf) -> BoxFuture<'static, anyhow::Result<bool>> {
        self.run(Op::WorktreeExists { path })
    }

    fn snapshot(
        &self,
        dir: PathBuf,
        base: Option<String>,
    ) -> BoxFuture<'static, anyhow::Result<GitSnapshot>> {
        self.run(Op::GitSnapshot { path: dir, base })
    }

    fn worktree_trash(
        &self,
        request: TrashRequest,
    ) -> BoxFuture<'static, anyhow::Result<TrashOutcome>> {
        self.run(Op::WorktreeTrash {
            request,
            disposable: self.0.inner.disposable.clone(),
        })
    }

    fn trash_list(&self) -> BoxFuture<'static, anyhow::Result<Vec<TrashItem>>> {
        self.run(Op::TrashList {})
    }

    fn trash_purge(&self, purge: TrashPurge) -> BoxFuture<'static, anyhow::Result<PurgeOutcome>> {
        self.run(Op::TrashPurge { purge })
    }

    fn trash_restore(
        &self,
        request: RestoreRequest,
    ) -> BoxFuture<'static, anyhow::Result<RestoreOutcome>> {
        self.run(Op::TrashRestore { request })
    }

    fn trash_restore_undo(&self, item: String) -> BoxFuture<'static, anyhow::Result<bool>> {
        self.run(Op::TrashRestoreUndo { item })
    }

    fn disk_usage(
        &self,
        worktree: Option<PathBuf>,
    ) -> BoxFuture<'static, anyhow::Result<twr_core::git::DiskReading>> {
        self.run(Op::DiskUsage {
            worktree,
            disposable: self.0.inner.disposable.clone(),
        })
    }

    fn worktree_trim(
        &self,
        _request: twr_core::git::TrimRequest,
    ) -> BoxFuture<'static, anyhow::Result<twr_core::git::TrimOutcome>> {
        // Only the hub's own box trims: a remote request that timed out would leave the
        // outcome unknown while the thread goes on.
        async { anyhow::bail!("twr trims only on the hub's own box (local) for now") }.boxed()
    }

    fn branch_only_commits(
        &self,
        targets: Vec<DeleteRefs>,
    ) -> BoxFuture<'static, anyhow::Result<Vec<OnlyCommits>>> {
        let n = targets.len();
        let f = self.run::<Vec<OnlyCommits>>(Op::BranchOnlyCommits { targets });
        async move {
            let out = f.await?;
            if out.len() != n {
                anyhow::bail!("the runner answered for {} of {n} threads", out.len());
            }
            Ok(out)
        }
        .boxed()
    }

    fn worktree_inventory(
        &self,
        repo: PathBuf,
        path: PathBuf,
    ) -> BoxFuture<'static, anyhow::Result<WorktreeInventory>> {
        self.run(Op::WorktreeInventory {
            repo,
            path,
            disposable: self.0.inner.disposable.clone(),
        })
    }

    fn commit_of(
        &self,
        _repo: PathBuf,
        _rev: String,
    ) -> BoxFuture<'static, anyhow::Result<Option<String>>> {
        not_on_remote_boxes()
    }

    fn tree_list(
        &self,
        _repo: PathBuf,
        _commit: String,
        _dir: String,
    ) -> BoxFuture<'static, anyhow::Result<Option<Vec<twr_core::git::TreeEntry>>>> {
        not_on_remote_boxes()
    }

    fn tree_entry(
        &self,
        _repo: PathBuf,
        _commit: String,
        _path: String,
    ) -> BoxFuture<'static, anyhow::Result<Option<twr_core::git::TreeEntry>>> {
        not_on_remote_boxes()
    }

    fn partial_clone(&self, _repo: PathBuf) -> BoxFuture<'static, anyhow::Result<bool>> {
        not_on_remote_boxes()
    }

    fn tree_files(
        &self,
        _repo: PathBuf,
        _commit: String,
        _dir: String,
    ) -> BoxFuture<'static, anyhow::Result<Vec<twr_core::git::TreeEntry>>> {
        not_on_remote_boxes()
    }

    fn blob_read(
        &self,
        _repo: PathBuf,
        _oid: String,
        _max: u64,
    ) -> BoxFuture<'static, anyhow::Result<Vec<u8>>> {
        not_on_remote_boxes()
    }

    fn grep_commit(
        &self,
        _repo: PathBuf,
        _req: twr_core::git::GrepRequest,
    ) -> BoxFuture<'static, anyhow::Result<twr_core::git::GrepOut>> {
        not_on_remote_boxes()
    }
}

/// The directory a remote attach runs in: the runner's answer `runner` when it is the thread's
/// directory `thread_dir` or lies (lexically: absolute, no `..`) under the box's worktrees or
/// the thread's repository, else `thread_dir` (the runner never chooses where the user's
/// command runs). `None` when neither is set.
pub fn attach_dir(
    runner: Option<&Path>,
    thread_dir: &Path,
    worktrees: Option<&Path>,
    repo: Option<&Path>,
) -> Option<PathBuf> {
    use std::path::Component;
    let under = |p: &Path, base: Option<&Path>| {
        base.is_some_and(|b| b.is_absolute() && !b.as_os_str().is_empty() && p.starts_with(b))
    };
    let accepted = runner.filter(|p| {
        p.is_absolute()
            && !p.components().any(|c| matches!(c, Component::ParentDir))
            && (*p == thread_dir || under(p, worktrees) || under(p, repo))
    });
    match accepted {
        Some(p) => Some(p.to_path_buf()),
        None => {
            if let Some(p) = runner.filter(|p| *p != thread_dir) {
                tracing::warn!(dir = %p.display(), "ignoring an attach directory a runner named outside the thread's directories");
            }
            (!thread_dir.as_os_str().is_empty()).then(|| thread_dir.to_path_buf())
        }
    }
}

/// `s` quoted for a POSIX shell (always single-quoted; `'` becomes `'\''`).
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The `ssh` arguments a box's `ssh` setting stands for: its whitespace-separated words (a
/// destination, possibly preceded by `ssh` options), as attach and the hub's own ssh
/// ([`crate::ssh_box`]) pass them, once [`twr_core::config::check_ssh_words`] accepts them
/// (else why not: they forward something, or change what ssh runs or how). Every caller
/// passes [`SSH_NO_FORWARDING`] after them.
pub fn ssh_words(ssh: &str) -> Result<Vec<&str>, String> {
    twr_core::config::check_ssh_words(ssh)?;
    Ok(ssh.split_whitespace().collect())
}

/// What every ssh twr runs passes after a box's `ssh` words ([`ssh_words`]) and before `--`:
/// no agent forwarding (`-a`), no X11 forwarding (`-x`), no GSSAPI credential forwarding
/// (`-k`). ssh takes options after the destination too, and the last of `-a`/`-A` (and so
/// on) wins over an earlier one and over any `ForwardAgent` (`ForwardX11`,
/// `GSSAPIDelegateCredentials`) of the command line or a config file, so these hold whatever
/// precedes them.
pub const SSH_NO_FORWARDING: &[&str] = &["-a", "-x", "-k"];

/// The options `twr attach`'s ssh passes before a box's `ssh` words: a terminal (`-t`), and
/// what the hub's own ssh clears too (`crate::ssh_box::SSH_OPTIONS`; ssh takes the first
/// value of a `-o` option, so these hold over your ssh config files): no forwarded port,
/// agent, X11 display or tunnel, no local command.
pub const ATTACH_SSH_OPTIONS: &[&str] = &[
    "-t",
    "-o",
    "ClearAllForwardings=yes",
    "-o",
    "ForwardAgent=no",
    "-o",
    "ForwardX11=no",
    "-o",
    "PermitLocalCommand=no",
    "-o",
    "Tunnel=no",
];

/// The local command that runs `info` on a remote box over ssh:
/// `ssh -t -o <no forwarding...> <ssh...> -a -x -k -- 'cd <cwd> && exec env -u K -- 'K2=V'
/// <program> <args>'` ([`ATTACH_SSH_OPTIONS`], [`SSH_NO_FORWARDING`]),
/// every part shell-quoted (the remote side runs it with the user's login shell; `--` ends
/// `env`'s options, so a variable whose name starts with `-` is set, not taken for one).
/// `cwd` is used when the command has no directory of its own. `ssh` is split on whitespace
/// (a destination, possibly preceded by `ssh` options: [`ssh_words`], an error when it
/// refuses them). An attached CLI on the box gets no forwarding either: it is an agent's
/// CLI, and must not reach your ssh agent, X11 display, Kerberos credentials or forwarded
/// ports any more than the box daemon's runtimes can.
pub fn ssh_command(ssh: &str, info: &AttachInfo, cwd: Option<&Path>) -> Result<Command, String> {
    let mut remote = String::new();
    let dir = info
        .cwd
        .as_deref()
        .or(cwd)
        .filter(|d| !d.as_os_str().is_empty());
    if let Some(d) = dir {
        remote.push_str(&format!("cd {} && ", sh_quote(&d.to_string_lossy())));
    }
    remote.push_str("exec ");
    if !info.env.is_empty() {
        remote.push_str("env ");
        for (k, v) in &info.env {
            if v.is_none() {
                remote.push_str(&format!("-u {} ", sh_quote(k)));
            }
        }
        remote.push_str("-- ");
        for (k, v) in &info.env {
            if let Some(v) = v {
                remote.push_str(&format!("{} ", sh_quote(&format!("{k}={v}"))));
            }
        }
    }
    remote.push_str(&sh_quote(&info.program));
    for a in &info.args {
        remote.push(' ');
        remote.push_str(&sh_quote(a));
    }
    let mut c = Command::new("ssh");
    c.args(ATTACH_SSH_OPTIONS)
        .args(ssh_words(ssh)?)
        .args(SSH_NO_FORWARDING)
        .arg("--")
        .arg(remote);
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> (ConnTx, watch::Receiver<Option<String>>) {
        let (tx, _rx) = mpsc::channel(4);
        let (close, closed) = watch::channel(None);
        (ConnTx::new(tx, close), closed)
    }

    /// Two answers to the same runner attention id (the item raised again) recorded before
    /// the session's forwarder issued the first go out in order, each under its own recorded
    /// id and run; nothing recorded is left behind (seed 1134 of the runner fuzz: the second
    /// answer's id replaced the first's, the first answer went out under it, and the first's
    /// record stayed in `remote_outbox` unsent).
    #[test]
    fn answers_to_a_reused_attention_id_keep_their_own_ids() {
        let b = RemoteBox::new("gpu", Secret::new("t"), None);
        b.expect_respond("t1", "a1", "respond:2:A", Some("r1".into()));
        b.expect_respond("t1", "a1", "respond:2:B", Some("r2".into()));
        let respond = |decision: &str| Op::Respond {
            thread: "t1".into(),
            attn_id: "a1".into(),
            answer: twr_core::event::Answer::new(decision),
            run: None,
        };
        let (first, _rx1) = b.begin(respond("y"), Kind::Session, None).unwrap();
        let (second, _rx2) = b.begin(respond("x"), Kind::Session, None).unwrap();
        assert_eq!(
            (first.as_str(), second.as_str()),
            ("respond:2:A", "respond:2:B")
        );
        let st = b.state();
        let runs: Vec<Option<String>> = st
            .pending
            .values()
            .filter_map(|p| match &p.op {
                Op::Respond { run, .. } => Some(run.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(runs, [Some("r1".into()), Some("r2".into())]);
        assert!(st.responds.is_empty(), "{:?}", st.responds);
        drop(st);
        // One that no forwarder will issue is withdrawn, and never named for a later answer.
        b.expect_respond("t1", "a1", "respond:2:C", None);
        b.forget_respond("t1", "a1", "respond:2:C");
        let (later, _rx3) = b.begin(respond("y"), Kind::Session, None).unwrap();
        assert_ne!(later, "respond:2:C");
    }

    /// A recorded `respond` names the runtime generation it answers; an older
    /// id (`respond:<ULID>`) names none.
    #[test]
    fn respond_ids_name_their_runtime_generation() {
        let id = respond_cmd_id(7);
        assert!(is_durable_cmd_id(&id), "{id}");
        assert_eq!(respond_cmd_gen(&id), Some(7));
        assert_ne!(respond_cmd_id(7), id);
        assert_eq!(respond_cmd_gen("respond:01J0000000000000000000000"), None);
        assert_eq!(respond_cmd_gen("respond:x:01J"), None);
        assert_eq!(respond_cmd_gen("stop:t1:7"), None);
        assert_eq!(respond_cmd_gen("send:7:1"), None);
    }

    /// Epochs are the hub's own counter, whatever the runner hints; an exhausted
    /// counter refuses the connection before anything changes.
    #[test]
    fn epochs_ignore_the_hint_and_never_repeat_or_overflow() {
        let b = RemoteBox::new("gpu", Secret::new("t"), None);
        let hello = Inventory {
            epoch_hint: Some(u64::MAX),
            ..Default::default()
        };
        let (first, first_closed) = conn();
        assert_eq!(b.connect(&hello, first), Some(1));
        let (second, _) = conn();
        assert_eq!(b.connect(&hello, second), Some(2));
        assert!(first_closed.borrow().is_some(), "the first is fenced");
        assert!(!b.is_current(1) && b.is_current(2));
        // The first connection's end leaves its replacement alone.
        assert!(!b.disconnect(1));
        assert!(b.is_current(2));

        b.state().last_epoch = u64::MAX - 1;
        let (last, last_closed) = conn();
        assert_eq!(b.connect(&Inventory::default(), last), Some(u64::MAX));
        let (extra, extra_closed) = conn();
        assert_eq!(b.connect(&hello, extra), None);
        assert!(b.is_current(u64::MAX), "the current connection stays");
        assert!(last_closed.borrow().is_none(), "and is not fenced");
        assert!(extra_closed.borrow().is_none());
    }

    #[test]
    fn quoting_survives_the_shell() {
        assert_eq!(sh_quote("a b"), "'a b'");
        assert_eq!(sh_quote("it's"), r"'it'\''s'");
        // Round trip through a real shell.
        for s in [
            "plain",
            "sp ace",
            "it's",
            "$HOME `x` \"q\" \\ ;&|",
            "",
            "-dash",
        ] {
            let out = std::process::Command::new("sh")
                .args(["-c", &format!("printf %s {}", sh_quote(s))])
                .output()
                .unwrap();
            assert_eq!(String::from_utf8(out.stdout).unwrap(), s);
        }
    }

    #[test]
    fn attach_directories_stay_where_the_thread_works() {
        let thread = Path::new("/srv/wt/api/t1");
        let (wt, repo) = (Some(Path::new("/srv/wt")), Some(Path::new("/src/api")));
        let dir = |runner: Option<&str>| attach_dir(runner.map(Path::new), thread, wt, repo);
        assert_eq!(dir(None), Some(thread.to_path_buf()));
        assert_eq!(dir(Some("/srv/wt/api/t1")), Some(thread.to_path_buf()));
        assert_eq!(
            dir(Some("/srv/wt/api/t1/sub")),
            Some("/srv/wt/api/t1/sub".into())
        );
        assert_eq!(dir(Some("/src/api/pkg")), Some("/src/api/pkg".into()));
        // Anywhere else, relative, or climbing out: the thread's own directory.
        for bad in ["/etc", "/srv/wt/../../etc", "srv/wt/api", "/src/apix", ""] {
            assert_eq!(dir(Some(bad)), Some(thread.to_path_buf()), "{bad}");
        }
        assert_eq!(attach_dir(None, Path::new(""), wt, repo), None);
    }

    #[test]
    fn attach_commands_are_wrapped_in_ssh() {
        let info = AttachInfo {
            program: "claude".into(),
            args: vec!["--resume".into(), "s 1".into()],
            cwd: Some("/w/it's".into()),
            env: vec![("A".into(), Some("x y".into())), ("B".into(), None)],
        };
        let c = ssh_command("-p 2222 me@gpu", &info, None).unwrap();
        let argv: Vec<String> = std::iter::once(c.get_program())
            .chain(c.get_args())
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            argv,
            vec![
                "ssh",
                "-t",
                "-o",
                "ClearAllForwardings=yes",
                "-o",
                "ForwardAgent=no",
                "-o",
                "ForwardX11=no",
                "-o",
                "PermitLocalCommand=no",
                "-o",
                "Tunnel=no",
                "-p",
                "2222",
                "me@gpu",
                "-a",
                "-x",
                "-k",
                "--",
                r"cd '/w/it'\''s' && exec env -u 'B' -- 'A=x y' 'claude' '--resume' 's 1'"
            ]
        );
        // Words that forward something, or decide the terminal, are refused: attach's own
        // `-t` is the only one.
        for (bad, says) in [("-A me@gpu", "ssh agent"), ("-T me@gpu", "terminal")] {
            let e = ssh_command(bad, &info, None).unwrap_err();
            assert!(e.contains(says), "{bad}: {e}");
        }
        // The remote string runs as meant in a shell (echo instead of the program).
        let info = AttachInfo {
            program: "sh".into(),
            args: vec![
                "-c".into(),
                "printf '%s|%s|%s' \"$PWD\" \"$A\" \"${B-unset}\"".into(),
            ],
            cwd: None,
            env: vec![("A".into(), Some("x y".into())), ("B".into(), None)],
        };
        let dir = tempfile::tempdir().unwrap();
        let c = ssh_command("host", &info, Some(dir.path())).unwrap();
        let remote = c.get_args().last().unwrap().to_string_lossy().into_owned();
        let out = std::process::Command::new("sh")
            .args(["-c", &remote])
            .env("B", "set")
            .output()
            .unwrap();
        let want = format!("{}|x y|unset", dir.path().display());
        assert_eq!(String::from_utf8(out.stdout).unwrap(), want);
        // A name that starts with `-` is set, not taken for an option of `env`.
        let info = AttachInfo {
            program: "env".into(),
            args: Vec::new(),
            cwd: None,
            env: vec![("-ODD".into(), Some("y".into())), ("B".into(), None)],
        };
        let c = ssh_command("host", &info, Some(dir.path())).unwrap();
        let remote = c.get_args().last().unwrap().to_string_lossy().into_owned();
        let out = std::process::Command::new("sh")
            .args(["-c", &remote])
            .output()
            .unwrap();
        let env = String::from_utf8(out.stdout).unwrap();
        assert!(env.lines().any(|l| l == "-ODD=y"), "{remote}: {env}");
    }
}
