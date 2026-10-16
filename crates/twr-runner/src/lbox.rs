//! [`LocalBox`]: the runner's executor. It owns the box's adapters and git, runs the hub's
//! operations ([`Op`]), keeps a live handle per running thread, and pumps every adapter
//! event into the [`Spool`] with a durable per-thread `rseq` (persisted before anything is
//! sent).
//!
//! Commands are deduplicated by `cmd_id`: a repeated id returns the stored result (or waits
//! for the running execution) instead of running again.
//!
//! Spool overflow: when an event does not fit in the spool's bound, the runtime that produced
//! it is stopped and the session's later events are dropped. The session stays live (it is in
//! [`LocalBox::running`], takes no `send` / `respond`, and its working directory is not
//! removed) until its process has really exited: its adapter reports the exit (after its own
//! kill escalation; the box kills what is left of the run after [`OVERFLOW_KILL_AFTER`]).
//! Only then is its `exited{code: null, stderr_tail: "twr runner: spool full ..."}` frame
//! recorded, forced past the bound (an exit always has room), so the hub never treats a
//! process that may still run as gone. Other runtimes keep running; each one that emits while
//! the spool is still full is stopped the same way. Nothing is lost silently: the hub sees the
//! session end, with the overflow as its reason.
//!
//! Working directories: every live session holds a use of its spawn `cwd` in the box git's
//! reservations ([`crate::workdirs`]) until it is forgotten. `WorktreeRemove` keeps (never
//! removes) a directory a live runtime of this box runs in (its spawn `cwd`, or one inside it),
//! and a `spawn` / `resume` whose `cwd` is being removed or created (for instance by a removal
//! the hub stopped waiting for: it runs on to its end) fails with an error saying so; the
//! reservation of a change lasts until git has finished, whoever still waits for it.
//!
//! Killing a run (overflow, shutdown, a vanished adapter): the process the run reported is
//! taken from its session while the session table's lock shows that the session is still that
//! run, and it is signalled only while it carries the run's marker ([`reap::RUNTIME_GEN_ENV`]):
//! a later run of the thread is never taken for it.
//!
//! Oversized events: an event whose frame would not fit in one WebSocket message
//! ([`MAX_FRAME_BYTES`]) is sent as a placeholder of the same kind instead (a `turn.ended`
//! keeps its result and session id; other kinds keep their shape with long strings cut and
//! large arrays/objects emptied), marked `ext.twr._dropped` (and `_dropped` where the payload
//! keeps unknown keys) with the original size, and counted ([`LocalBox::dropped_events`]).
//!
//! Concurrency: session ops run in the order received; at most [`MAX_CONCURRENT_OPS`] of the
//! other ops (git, attach info) execute at once, the rest wait their turn.
//!
//! Runtime start times: on `thread.started` / `thread.resumed` events whose `ext.twr.pid` is
//! set, the box adds `ext.twr.start_time` (from this box's process table), since only the box
//! that runs the process can read it.
//!
//! Runtime identity on the box: the same event also records the process (thread,
//! runtime, pid, start time, exe) in the spool's `runtimes` table, in the frame's transaction;
//! the session's `exited` frame deletes it in its own. A runner that dies without recording an
//! exit (SIGKILL, power loss) leaves the row behind: [`LocalBox::open`] then kills that process
//! and what is left of its group, and every process marked for the thread that started since
//! ([`crate::reap`]; only processes whose start time and `TWR_THREAD` marker match), checks
//! that none of them runs any more, and only then records its `exited` frame, before the
//! runner connects, so the hub closes the thread's turn and never starts a replacement next to
//! a survivor ([`reap::leftover`]). The row also keeps a lower bound on the runtime's start
//! (the runner's own start time), so a row without the runtime's start time still bounds the
//! sweep and tells a reused pid from the runtime. When one may survive (it still runs, its
//! recorded pid may still be it, a process of this user in this control group started since
//! hides its environment and is tied to the runtime, by its process group or a parent that is
//! the runtime or marked for its thread, or the process table cannot be read) its exit is not recorded, its
//! row stays, and the box does not open: the runner stops with an error naming it, and the
//! next runner tries again.
//!
//! One runner per box per machine: before anything else, [`LocalBox::open`] takes exclusive
//! locks on the box's spool ([`Spool::open`]: `runner-<box>.db.lock` beside it), on its
//! worktrees directory (`.twr-runner-<box>.lock` in it) and on the box's machine lock
//! ([`LocalBoxOptions::machine_lock`]: one file per hub and box in the user's runtime
//! directory, which no `--data-dir` moves), and holds them for as long as the box. A second
//! runner of the box on the same machine, whatever its data directory, is refused: it would
//! kill the first one's runtimes as leftovers, and the hub would take its connection as proof
//! that the first one's repository changes are over while they still run.
//!
//! A runtime reports its process only once it has initialized, so every start is recorded
//! first as an intent (the spool's `spawn_intents`, written before the adapter launches
//! anything; the runtime's row or its exit replaces it). An intent found when the spool opens
//! is a runtime whose runner died while it initialized: every process carrying the thread's
//! `TWR_THREAD` marker (always set, see [`crate::reap::THREAD_ENV`]) that started after the
//! runner that recorded the intent is killed with its group ([`crate::reap::reap_marked`]),
//! and, once none is left, the session's `exited` frame is recorded (as for a row above).
//!
//! Runs: every `spawn` / `resume` starts a new *run* of the thread's session, with a fresh id
//! ([`RunningSession::run`]) that its frames carry. A `stop` naming a run leaves any other run
//! alone, so a stop the hub meant for a run that has since ended never kills its replacement.
//!
//! Spool failures are never skipped: an event or exit that cannot be stored is retried (with
//! backoff, about 2 s in all) before the session's next event is taken, so `rseq` never hides
//! a loss. If the spool stays unwritable the runtime is stopped, nothing more of that session
//! is recorded (its runtime row, if any, makes the next runner record its end) and the box
//! fails ([`LocalBox::fatal_error`]): the runner stops instead of running on. The same goes
//! for a command's result: it is published only once it is stored; if it cannot be stored the
//! command answers `interrupted` (its row stays unfinished, which is never run again).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot, watch, Notify};
use tokio::task::AbortHandle;
use twr_core::adapter::{Adapter, AdapterCmd, AdapterEvent, SpawnConfig};
use twr_core::event::{EventBody, ACP_COMMAND_KEY};
use twr_core::git::{GitOps, WorktreeSpec};
use twr_core::runner::{
    cmd_errors, AttachInfo, CmdError, CmdResult, Op, RunnerMsg, RunningSession, WireRemoveOutcome,
    WireWorktreeCreated, MAX_FRAME_BYTES,
};

use crate::local::LocalGit;
use crate::reap::{self, StaleProcess};
use crate::spool::{
    interrupted_result, Appended, CmdState, RuntimeChange, RuntimeRecord, SpawnIntent, Spool,
    DEFAULT_MAX_BYTES,
};
use crate::workdirs::WorkdirUse;

/// Looks an adapter up by runtime name.
pub type AdapterLookup = Arc<dyn Fn(&str) -> Option<Arc<dyn Adapter>> + Send + Sync>;

/// Room left in a frame for the `rseq` digits and framing.
const FRAME_MARGIN: usize = 64;

/// Non-session ops (git, attach info) executing at once, at most.
pub const MAX_CONCURRENT_OPS: usize = 8;

/// Strings of an oversized event's placeholder are cut to this many bytes.
const PLACEHOLDER_STRING: usize = 4 << 10;

/// Arrays / objects of an oversized event's placeholder larger than this (serialized) are
/// emptied.
const PLACEHOLDER_CONTAINER: usize = 64 << 10;

/// After a spool overflow stopped a runtime, how long its adapter gets to report the exit
/// (it escalates to a kill itself) before the box kills what is left of the run (identity-
/// checked: its recorded process, and the processes marked with the run's id).
pub const OVERFLOW_KILL_AFTER: Duration = Duration::from_secs(15);

/// How long the box waits before it tries again to kill a runtime whose adapter went away
/// without reporting an exit and that survived a kill.
const REAP_RETRY: Duration = Duration::from_secs(5);

/// Delays between attempts at a spool write that failed (5 retries, about 2 s in all).
const SPOOL_RETRY: [Duration; 5] = [
    Duration::from_millis(50),
    Duration::from_millis(150),
    Duration::from_millis(300),
    Duration::from_millis(600),
    Duration::from_millis(900),
];

/// How a [`LocalBox`] is set up.
#[derive(Clone)]
pub struct LocalBoxOptions {
    /// The spool database (`<data>/runner-<box>.db`).
    pub spool_path: PathBuf,
    /// Bound of the spool's outbox in bytes.
    pub spool_max_bytes: u64,
    /// Where thread worktrees live on this box.
    pub worktrees: PathBuf,
    /// The box's trash, where deleted threads' worktrees and branches go (default
    /// `<data_dir>/trash`).
    pub trash: PathBuf,
    /// Adapter lookup.
    pub adapters: AdapterLookup,
    /// Runtime names offered (reported in the inventory).
    pub runtimes: Vec<String>,
    /// Git binary.
    pub git: LocalGit,
    /// The box's lock on this machine, outside every directory an option can move
    /// ([`crate::client::machine_lock_path`]: keyed by the hub and the box); `None` (tests, and
    /// a box opened without a hub) takes none.
    pub machine_lock: Option<PathBuf>,
}

impl std::fmt::Debug for LocalBoxOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalBoxOptions")
            .field("spool_path", &self.spool_path)
            .field("spool_max_bytes", &self.spool_max_bytes)
            .field("worktrees", &self.worktrees)
            .field("runtimes", &self.runtimes)
            .finish()
    }
}

/// The spool file of box `box_name` in `data_dir`.
pub fn spool_path(data_dir: &std::path::Path, box_name: &str) -> PathBuf {
    data_dir.join(format!("runner-{box_name}.db"))
}

impl LocalBoxOptions {
    /// The built-in adapters, the spool at `<data_dir>/runner-<box>.db` with the default
    /// bound, and worktrees under `<data_dir>/worktrees`.
    pub fn new(data_dir: &std::path::Path, box_name: &str) -> Self {
        LocalBoxOptions {
            spool_path: spool_path(data_dir, box_name),
            spool_max_bytes: DEFAULT_MAX_BYTES,
            worktrees: data_dir.join("worktrees"),
            trash: data_dir.join("trash"),
            adapters: Arc::new(twr_adapters::find),
            runtimes: twr_adapters::registry()
                .iter()
                .map(|a| a.info().name.to_string())
                .collect(),
            git: LocalGit::default(),
            machine_lock: None,
        }
    }
}

/// A live runtime session.
struct Live {
    gen: u64,
    /// The run's id ([`RunningSession::run`]).
    run: String,
    runtime: String,
    session_id: Option<String>,
    pid: Option<u32>,
    cmd: mpsc::UnboundedSender<AdapterCmd>,
    pump: Option<AbortHandle>,
    /// The spool overflowed on its events: it was stopped, its later events are dropped, and
    /// this is why its eventual `exited` frame says it ended.
    overflow: Option<String>,
    /// The process this run reported (with the run's id as its generation), once it has: what
    /// a kill of this run targets, taken under the session table's lock.
    process: Option<StaleProcess>,
    /// Its use of where it runs (its spawn `cwd`): never removed under it.
    _workdir: Option<WorkdirUse>,
}

/// State the pumps share with the box. Pumps hold it weakly: dropping the box closes the
/// spool (and releases its lock) at once.
struct Shared {
    spool: Arc<Spool>,
    /// Woken whenever the outbox grows.
    notify: Notify,
    sessions: Mutex<HashMap<String, Live>>,
    /// Woken whenever a session ends.
    ended: Notify,
    /// Why the box failed (the spool could not record what it must), once it has.
    fatal: watch::Sender<Option<String>>,
    /// Events replaced by a placeholder because they did not fit in a frame.
    dropped_events: AtomicU64,
    /// Test hook ([`LocalBox::hold_overflow_kills`]).
    overflow_hold: Mutex<Option<OverflowHold>>,
    /// Start time of this runner's process ([`reap::own_start_time`], read once when the box
    /// opens): every runtime it starts starts later (the lower bound of its spawn intents and of
    /// the marked processes a kill of a run looks for). `None` when it could not be read (said
    /// loudly at open): intents are then recorded without a bound, and kill nothing.
    start_time: Option<u64>,
}

/// Set by [`LocalBox::hold_overflow_kills`].
#[derive(Clone)]
struct OverflowHold {
    after: Duration,
    pauses: mpsc::UnboundedSender<OverflowKillPause>,
}

/// An overflow kill paused by [`LocalBox::hold_overflow_kills`] (tests): it has checked that
/// its session is still the run it kills and taken the process to kill. Send `go` to let it
/// kill; `done` fires once it has.
#[doc(hidden)]
#[derive(Debug)]
pub struct OverflowKillPause {
    /// The thread whose run is killed.
    pub thread: String,
    /// Lets the kill go on (dropping it does too).
    pub go: oneshot::Sender<()>,
    /// Fires once the kill has finished.
    pub done: oneshot::Receiver<()>,
}

impl Shared {
    fn sessions(&self) -> MutexGuard<'_, HashMap<String, Live>> {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The box cannot go on: records why (the first reason wins) and wakes the connection.
    fn fail(&self, msg: String) {
        tracing::error!(error = %msg, "fatal: the runner cannot record its state; stopping");
        self.fatal.send_if_modified(|f| {
            if f.is_none() {
                *f = Some(msg);
                true
            } else {
                false
            }
        });
        self.notify.notify_waiters();
    }
}

struct Inner {
    shared: Arc<Shared>,
    /// The box's locks ([`lock_box`]), held for as long as the box.
    _locks: Vec<std::fs::File>,
    adapters: AdapterLookup,
    runtimes: Vec<String>,
    git: LocalGit,
    worktrees: PathBuf,
    next_gen: AtomicU64,
    inflight: Mutex<HashMap<String, watch::Receiver<Option<CmdResult>>>>,
    /// Bounds the non-session ops executing at once ([`MAX_CONCURRENT_OPS`]).
    op_permits: tokio::sync::Semaphore,
    /// Repository changes received from the hub and not finished ([`LocalBox::begin_change`]).
    changes: watch::Sender<usize>,
    /// [`LocalBox::hold_change_receipts`] (tests).
    receipt_hold: Mutex<Option<std::sync::mpsc::Sender<ChangeReceiptPause>>>,
}

/// A repository change paused by [`LocalBox::hold_change_receipts`] (tests) as the
/// connection's reader takes it, before it is counted: the reader's thread is blocked until
/// `go` is sent (or dropped).
#[doc(hidden)]
#[derive(Debug)]
pub struct ChangeReceiptPause {
    /// Lets the reader go on.
    pub go: std::sync::mpsc::Sender<()>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Dropping the box is a crash as far as the spool is concerned: pumps stop where they
        // are and nothing more is recorded. Runtimes see their command channel close.
        for (_, live) in self.shared.sessions().drain() {
            if let Some(p) = live.pump {
                p.abort();
            }
        }
    }
}

/// A repository change received from the hub, in progress ([`LocalBox::begin_change`]).
pub struct ChangeInProgress {
    changes: watch::Sender<usize>,
}

impl Drop for ChangeInProgress {
    fn drop(&mut self) {
        self.changes.send_modify(|n| *n = n.saturating_sub(1));
    }
}

/// The box a runner runs: adapters + git + the spool. Cheap to clone.
#[derive(Clone)]
pub struct LocalBox {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for LocalBox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalBox")
            .field("spool", &self.inner.shared.spool)
            .finish()
    }
}

impl LocalBox {
    /// Opens the box (and its spool). Refused while another runner of the box runs on this
    /// machine (see the module docs). Runtimes a previous runner of this spool left behind
    /// (it died without recording their exit) are killed and their exits recorded first (see
    /// the module docs; refused when one survives); this blocks for as long as that takes
    /// (bounded, a few seconds per process).
    pub fn open(opts: LocalBoxOptions) -> anyhow::Result<LocalBox> {
        if let Some(parent) = opts.spool_path.parent() {
            twr_core::paths::ensure_private_dir(parent)?;
        }
        let locks = lock_box(
            &opts.spool_path,
            &opts.worktrees,
            opts.machine_lock.as_deref(),
        )?;
        let spool = Arc::new(Spool::open(&opts.spool_path, opts.spool_max_bytes)?);
        recover_runtimes(&spool)?;
        let start_time = reap::own_start_time();
        Ok(LocalBox {
            inner: Arc::new(Inner {
                shared: Arc::new(Shared {
                    spool,
                    notify: Notify::new(),
                    sessions: Mutex::new(HashMap::new()),
                    ended: Notify::new(),
                    fatal: watch::Sender::new(None),
                    dropped_events: AtomicU64::new(0),
                    overflow_hold: Mutex::new(None),
                    start_time,
                }),
                _locks: locks,
                adapters: opts.adapters,
                runtimes: opts.runtimes,
                // Pruning stale worktree records stays within this box's worktrees.
                git: {
                    let git = match opts.git.managed_root() {
                        Some(_) => opts.git,
                        None => opts.git.with_managed_root(&opts.worktrees),
                    };
                    match git.trash_root() {
                        Some(_) => git,
                        None => git.with_trash_root(&opts.trash),
                    }
                },
                worktrees: opts.worktrees,
                next_gen: AtomicU64::new(1),
                inflight: Mutex::new(HashMap::new()),
                op_permits: tokio::sync::Semaphore::new(MAX_CONCURRENT_OPS),
                changes: watch::Sender::new(0),
                receipt_hold: Mutex::new(None),
            }),
        })
    }

    /// A repository change ([`Op::changes_repository`]) received from the hub begins: counted
    /// until the returned guard is dropped (taken when the command arrives, before its task
    /// runs). [`LocalBox::changes_done`] waits for all of them.
    pub fn begin_change(&self) -> ChangeInProgress {
        self.inner.changes.send_modify(|n| *n += 1);
        ChangeInProgress {
            changes: self.inner.changes.clone(),
        }
    }

    /// Test hook: from now on the connection's reader, as it takes a repository change from
    /// the hub and before it counts it ([`LocalBox::begin_change`]), sends a
    /// [`ChangeReceiptPause`] on the returned channel and blocks its thread until the pause's
    /// `go`: a reader in the middle of being polled, which aborting its task does not stop.
    #[doc(hidden)]
    pub fn hold_change_receipts(&self) -> std::sync::mpsc::Receiver<ChangeReceiptPause> {
        let (tx, rx) = std::sync::mpsc::channel();
        *self
            .inner
            .receipt_hold
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(tx);
        rx
    }

    /// The connection's reader takes a repository change ([`LocalBox::hold_change_receipts`]).
    pub(crate) fn change_received(&self) {
        let hold = self
            .inner
            .receipt_hold
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(hold) = hold {
            let (go, wait) = std::sync::mpsc::channel();
            if hold.send(ChangeReceiptPause { go }).is_ok() {
                let _ = wait.recv();
            }
        }
    }

    /// Waits, at most `within`, until no operation of the box's git that changes its
    /// repositories or worktrees runs any more ([`LocalGit::changes_done`]; one whose command
    /// or connection went away runs on to its end): whether none does.
    pub async fn git_changes_done(&self, within: Duration) -> bool {
        self.inner.git.changes_done(within).await
    }

    /// Repository changes received from the hub and not finished.
    pub fn changes_running(&self) -> usize {
        *self.inner.changes.borrow()
    }

    /// Waits until every repository change received from the hub has finished (every git it
    /// started reaped): a runner connects again only then, so the hub knows that nothing it
    /// left unanswered still runs.
    pub async fn changes_done(&self) {
        let mut rx = self.inner.changes.subscribe();
        let _ = rx.wait_for(|n| *n == 0).await;
    }

    /// The spool.
    pub fn spool(&self) -> &Arc<Spool> {
        &self.inner.shared.spool
    }

    /// Notified whenever a frame is spooled (and when the box fails).
    pub fn outbox_notify(&self) -> &Notify {
        &self.inner.shared.notify
    }

    /// Why the box failed, if it has: the spool could not record an event, an exit or a
    /// command result. The runner stops.
    pub fn fatal_error(&self) -> Option<String> {
        self.inner.shared.fatal.borrow().clone()
    }

    /// Changes when the box fails ([`LocalBox::fatal_error`]).
    pub fn fatal_watch(&self) -> watch::Receiver<Option<String>> {
        self.inner.shared.fatal.subscribe()
    }

    /// Events sent as placeholders because they did not fit in one frame (see the module
    /// docs), since the box opened.
    pub fn dropped_events(&self) -> u64 {
        self.inner.shared.dropped_events.load(Ordering::Relaxed)
    }

    /// Test hook: the box kills an overflowed runtime ([`OVERFLOW_KILL_AFTER`]) `after` the
    /// overflow instead, and each such kill pauses once it has checked its session and taken
    /// the process it kills, until the returned receiver's [`OverflowKillPause::go`] is sent.
    #[doc(hidden)]
    pub fn hold_overflow_kills(
        &self,
        after: Duration,
    ) -> mpsc::UnboundedReceiver<OverflowKillPause> {
        let (tx, rx) = mpsc::unbounded_channel();
        *self
            .inner
            .shared
            .overflow_hold
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(OverflowHold { after, pauses: tx });
        rx
    }

    /// Runtimes offered.
    pub fn runtimes(&self) -> &[String] {
        &self.inner.runtimes
    }

    /// Where thread worktrees live.
    pub fn worktrees(&self) -> &std::path::Path {
        &self.inner.worktrees
    }

    /// Sessions running now.
    pub fn running(&self) -> Vec<RunningSession> {
        let mut v: Vec<RunningSession> = self
            .inner
            .shared
            .sessions()
            .iter()
            .map(|(t, l)| RunningSession {
                thread: t.clone(),
                runtime: l.runtime.clone(),
                session_id: l.session_id.clone(),
                pid: l.pid,
                run: Some(l.run.clone()),
            })
            .collect();
        v.sort_by(|a, b| a.thread.cmp(&b.thread));
        v
    }

    /// Commands still executing, plus those a crashed runner left unfinished.
    pub fn in_flight(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .inner
            .inflight
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect();
        v.extend(self.spool().interrupted().iter().cloned());
        v.sort();
        v.dedup();
        v
    }

    /// Runs command `cmd_id` once: a repeated id returns the first execution's result.
    pub async fn handle_cmd(&self, cmd_id: &str, op: Op) -> CmdResult {
        enum Plan {
            Wait(watch::Receiver<Option<CmdResult>>),
            Done(CmdResult),
            Run(watch::Sender<Option<CmdResult>>),
        }
        let plan = {
            let mut inflight = self
                .inner
                .inflight
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(rx) = inflight.get(cmd_id) {
                Plan::Wait(rx.clone())
            } else {
                match self.spool().cmd_begin(cmd_id) {
                    Ok(CmdState::Done(r)) => Plan::Done(r),
                    Ok(CmdState::New) => {
                        let (tx, rx) = watch::channel(None);
                        inflight.insert(cmd_id.to_string(), rx);
                        Plan::Run(tx)
                    }
                    Err(e) => Plan::Done(CmdResult::new(
                        cmd_id,
                        Err(CmdError::new(
                            cmd_errors::FAILED,
                            format!("runner spool: {e:#}"),
                        )),
                    )),
                }
            }
        };
        match plan {
            Plan::Done(r) => r,
            Plan::Wait(mut rx) => loop {
                if let Some(r) = rx.borrow().clone() {
                    break r;
                }
                if rx.changed().await.is_err() {
                    // The execution was dropped mid-way; its stored result (if any) wins.
                    break match self.spool().cmd_result(cmd_id) {
                        Ok(Some(r)) => r,
                        _ => CmdResult::new(
                            cmd_id,
                            Err(CmdError::new(
                                cmd_errors::INTERRUPTED,
                                "the command's execution was cancelled",
                            )),
                        ),
                    };
                }
            },
            Plan::Run(tx) => {
                tracing::debug!(cmd_id, op = op.name(), thread = ?op.thread(), "running command");
                let outcome = if op.is_session_op() {
                    self.execute(op).await
                } else {
                    // The semaphore is never closed.
                    let _permit = self.inner.op_permits.acquire().await.ok();
                    self.execute(op).await
                };
                let mut result = CmdResult::new(cmd_id, outcome);
                // Published only once stored: a result the hub saw but the spool lacks would
                // let a repeat of the id run again after a restart.
                let mut delays = SPOOL_RETRY.iter();
                loop {
                    let e = match self.spool().cmd_finish(&result) {
                        Ok(()) => break,
                        Err(e) => e,
                    };
                    let Some(d) = delays.next() else {
                        self.inner.shared.fail(format!(
                            "cannot record the result of command {cmd_id}: {e:#}"
                        ));
                        // The row stays unfinished: it answers `interrupted` from now on.
                        result = interrupted_result(
                            cmd_id,
                            "the runner could not record the command's result; its effect is unknown",
                        );
                        break;
                    };
                    tracing::warn!(cmd_id, error = %format!("{e:#}"), ?d, "storing a command result failed; retrying");
                    tokio::time::sleep(*d).await;
                }
                let _ = tx.send(Some(result.clone()));
                self.inner
                    .inflight
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(cmd_id);
                result
            }
        }
    }

    /// Runs `op` (no dedup).
    pub async fn execute(&self, op: Op) -> Result<Value, CmdError> {
        let failed = |e: anyhow::Error| CmdError::new(cmd_errors::FAILED, format!("{e:#}"));
        let git = &self.inner.git;
        match op {
            Op::Spawn {
                thread,
                runtime,
                cfg,
                prompt,
                run,
            } => {
                let mut cfg: SpawnConfig = cfg.into();
                cfg.thread = thread.clone();
                self.start(&thread, &runtime, cfg, run, |a, cfg| a.spawn(cfg, prompt))
            }
            Op::Resume {
                thread,
                runtime,
                cfg,
                session_id,
                run,
            } => {
                let mut cfg: SpawnConfig = cfg.into();
                cfg.thread = thread.clone();
                self.start(&thread, &runtime, cfg, run, |a, cfg| {
                    a.resume(cfg, session_id)
                })
            }
            Op::Send {
                thread,
                turn,
                text,
                run,
            } => {
                // Only to the run it was meant for: a prompt for a run that has ended never
                // reaches the next one.
                let other_run = run.is_some_and(|r| {
                    self.inner
                        .shared
                        .sessions()
                        .get(&thread)
                        .is_some_and(|l| l.run != r)
                });
                if other_run {
                    tracing::info!(thread, turn, "refusing a prompt meant for an earlier run");
                    return Err(CmdError::new(
                        cmd_errors::NOT_RUNNING,
                        format!("the run of thread {thread} turn {turn} was sent to has ended"),
                    ));
                }
                self.command(&thread, AdapterCmd::Send { text, turn })
            }
            Op::Respond {
                thread,
                attn_id,
                answer,
                run,
            } => {
                // Only to the run that raised the item: a later run's runtime numbers its
                // requests anew, so the same id may ask something else there.
                let other_run = run.is_some_and(|r| {
                    self.inner
                        .shared
                        .sessions()
                        .get(&thread)
                        .is_some_and(|l| l.run != r)
                });
                if other_run {
                    tracing::info!(
                        thread,
                        attn_id,
                        "refusing an answer meant for an earlier run"
                    );
                    return Err(CmdError::new(
                        cmd_errors::NOT_RUNNING,
                        format!("the run of thread {thread} that raised {attn_id} has ended"),
                    ));
                }
                self.command(&thread, AdapterCmd::Respond { attn_id, answer })
            }
            Op::Stop { thread, run } => {
                let other_run = run.is_some_and(|r| {
                    self.inner
                        .shared
                        .sessions()
                        .get(&thread)
                        .is_some_and(|l| l.run != r)
                });
                if other_run {
                    tracing::info!(thread, "ignoring a stop meant for an earlier run");
                    return Ok(Value::Null);
                }
                // Nothing running is fine: the thread is stopped either way.
                let _ = self.command(&thread, AdapterCmd::Stop);
                Ok(Value::Null)
            }
            Op::AttachInfo {
                thread,
                runtime,
                cfg,
                session_id,
            } => {
                let adapter = self.adapter(&runtime)?;
                let mut cfg: SpawnConfig = cfg.into();
                cfg.thread = thread;
                // Only the runtime spec's attach template (an `acp` preset's): without it the
                // adapter has no attach command. Nothing else the hub sent is kept.
                twr_adapters::retain_attach_env(&mut cfg.env);
                Ok(match adapter.attach_command(&cfg, &session_id) {
                    Some(c) => json!(AttachInfo::from_command(&c)),
                    None => Value::Null,
                })
            }
            Op::RepoRoot { dir } => git.repo_root(dir).await.map(|r| json!(r)).map_err(failed),
            Op::GitCommonDir { dir } => git
                .git_common_dir(dir)
                .await
                .map(|c| json!(c))
                .map_err(failed),
            Op::WorktreeCreate {
                repo,
                path,
                branch,
                base,
            } => git
                .worktree_create(WorktreeSpec {
                    repo,
                    path,
                    branch,
                    base,
                })
                .await
                .map(|c| json!(WireWorktreeCreated::from(c)))
                .map_err(failed),
            // Never under a live runtime of this box (the hub should not ask; defense in
            // depth): the git's reservations refuse it (kept, with the reason), and hold the
            // path until git has finished, so no runtime starts in it meanwhile.
            // With the hub's list of disposable ignored paths, when it sent one.
            Op::WorktreeRemove {
                repo,
                path,
                branch,
                disposable,
            } => match disposable {
                Some(d) => git
                    .clone()
                    .with_park_disposable(d)
                    .worktree_remove(repo, path, branch),
                None => git.worktree_remove(repo, path, branch),
            }
            .await
            .map(|o| json!(WireRemoveOutcome::from(o)))
            .map_err(failed),
            Op::WorktreePrune { repo } => git
                .worktree_prune(repo)
                .await
                .map(|()| Value::Null)
                .map_err(failed),
            Op::WorktreeExists { path } => git
                .worktree_exists(path)
                .await
                .map(|b| json!(b))
                .map_err(failed),
            Op::GitSnapshot { path, base } => git
                .snapshot(path, base)
                .await
                .map(|s| json!(s))
                .map_err(failed),
            // Under the same reservations as a removal: refused while a runtime of this box
            // works in it.
            // Build output goes only when the hub names it (an older hub: nothing).
            Op::WorktreeTrash {
                request,
                disposable,
            } => git
                .clone()
                .with_park_disposable(disposable.unwrap_or_default())
                .worktree_trash(request)
                .await
                .map(|o| json!(o))
                .map_err(failed),
            Op::BranchOnlyCommits { targets } => git
                .branch_only_commits(targets)
                .await
                .map(|o| json!(o))
                .map_err(failed),
            Op::WorktreeInventory {
                repo,
                path,
                disposable,
            } => git
                .clone()
                .with_park_disposable(disposable.unwrap_or_default())
                .worktree_inventory(repo, path)
                .await
                .map(|i| json!(i))
                .map_err(failed),
            Op::TrashList {} => git.trash_list().await.map(|i| json!(i)).map_err(failed),
            Op::TrashPurge { purge } => git
                .trash_purge(purge)
                .await
                .map(|o| json!(o))
                .map_err(failed),
            Op::TrashRestore { request } => git
                .trash_restore(request)
                .await
                .map(|o| json!(o))
                .map_err(failed),
            Op::TrashRestoreUndo { item } => git
                .trash_restore_undo(item)
                .await
                .map(|o| json!(o))
                .map_err(failed),
            Op::DiskUsage {
                worktree,
                disposable,
            } => match disposable {
                Some(d) => git.clone().with_park_disposable(d).disk_usage(worktree),
                None => git.disk_usage(worktree),
            }
            .await
            .map(|o| json!(o))
            .map_err(failed),
        }
    }

    fn adapter(&self, runtime: &str) -> Result<Arc<dyn Adapter>, CmdError> {
        (self.inner.adapters)(runtime).ok_or_else(|| {
            CmdError::new(
                cmd_errors::UNKNOWN_RUNTIME,
                format!("this box has no runtime {runtime:?}"),
            )
        })
    }

    fn command(&self, thread: &str, cmd: AdapterCmd) -> Result<Value, CmdError> {
        let sessions = self.inner.shared.sessions();
        let live = sessions.get(thread).ok_or_else(|| {
            CmdError::new(
                cmd_errors::NOT_RUNNING,
                format!("thread {thread} has no running session on this box"),
            )
        })?;
        // Stopped for a spool overflow: nothing more reaches it (its events are dropped).
        if let Some(why) = live.overflow.as_ref().filter(|_| cmd != AdapterCmd::Stop) {
            return Err(CmdError::new(
                cmd_errors::NOT_RUNNING,
                format!("thread {thread}'s runtime is stopping: {why}"),
            ));
        }
        live.cmd.send(cmd).map_err(|_| {
            CmdError::new(
                cmd_errors::NOT_RUNNING,
                format!("thread {thread}'s runtime has exited"),
            )
        })?;
        Ok(Value::Null)
    }

    /// Spawns or resumes a session (`go` with `cfg`, marked for the thread and the new run,
    /// [`mark`]) and starts its pump. The run's id is `run` (the hub's) when given, else a
    /// fresh one.
    fn start(
        &self,
        thread: &str,
        runtime: &str,
        mut cfg: SpawnConfig,
        run: Option<String>,
        go: impl FnOnce(&dyn Adapter, SpawnConfig) -> anyhow::Result<twr_core::adapter::AdapterHandle>,
    ) -> Result<Value, CmdError> {
        if run
            .as_deref()
            .is_some_and(|r| !twr_core::runner::valid_run_id(r))
        {
            return Err(CmdError::new(cmd_errors::FAILED, "invalid run id"));
        }
        let cwd = cfg.cwd.clone();
        let adapter = self.adapter(runtime)?;
        let shared = &self.inner.shared;
        let mut sessions = shared.sessions();
        if sessions.contains_key(thread) {
            return Err(CmdError::new(
                cmd_errors::ALREADY_RUNNING,
                format!("thread {thread} already has a running session on this box"),
            ));
        }
        // Held for as long as the session lives: no worktree change removes `cwd` under it.
        // Refused while one is in progress (a removal the hub stopped waiting for runs on).
        let workdir = self
            .inner
            .git
            .workdirs()
            .begin_use(&cwd, thread)
            .map_err(|e| CmdError::new(cmd_errors::FAILED, e))?;
        // Recorded before anything is launched: a runner killed while the runtime initializes
        // (before it reports its process) still lets the next one find it.
        shared
            .spool
            .intend(&SpawnIntent {
                thread: thread.to_string(),
                runtime: runtime.to_string(),
                min_start: shared.start_time,
            })
            .map_err(|e| {
                CmdError::new(
                    cmd_errors::FAILED,
                    format!("runner spool: cannot record the start: {e:#}"),
                )
            })?;
        let run = run.unwrap_or_else(twr_core::event::new_id);
        mark(&mut cfg, &run);
        // The runtime, permission mode, model and reasoning effort the session starts with go
        // with its start event, so a hub that takes the session over later knows them
        // (`twr-hub` `hub::restart`).
        let settings = StartSettings::of(runtime, &cfg);
        let started = go(adapter.as_ref(), cfg)
            .map_err(|e| CmdError::new(cmd_errors::FAILED, format!("{e:#}")))
            .and_then(|mut handle| match handle.take_events() {
                Some(events) => Ok((handle, events)),
                None => Err(CmdError::new(
                    cmd_errors::FAILED,
                    "the adapter returned a handle without events",
                )),
            });
        let (handle, events) = match started {
            Ok(h) => h,
            Err(e) => {
                // Nothing was launched (an adapter that fails to spawn leaves no process).
                if let Err(err) = shared.spool.forget_intent(thread) {
                    tracing::warn!(thread, error = %format!("{err:#}"), "cannot forget a start intent");
                }
                return Err(e);
            }
        };
        let gen = self.inner.next_gen.fetch_add(1, Ordering::Relaxed);
        let pump = tokio::spawn(pump(
            Arc::downgrade(shared),
            thread.to_string(),
            runtime.to_string(),
            gen,
            run.clone(),
            settings,
            handle.commander(),
            events,
        ));
        sessions.insert(
            thread.to_string(),
            Live {
                gen,
                run,
                runtime: runtime.to_string(),
                session_id: None,
                pid: None,
                cmd: handle.commander(),
                pump: Some(pump.abort_handle()),
                overflow: None,
                process: None,
                _workdir: workdir,
            },
        );
        tracing::info!(thread, runtime, "runtime started");
        Ok(Value::Null)
    }

    /// Number of live sessions.
    pub fn live_count(&self) -> usize {
        self.inner.shared.sessions().len()
    }

    /// Asks every runtime to stop (their exits are spooled as they happen).
    pub fn stop_all(&self) {
        for (thread, live) in self.inner.shared.sessions().iter() {
            tracing::info!(thread = %thread, "stopping runtime");
            let _ = live.cmd.send(AdapterCmd::Stop);
        }
    }

    /// Graceful shutdown: stops every runtime and waits up to `grace` for each exit to be
    /// spooled. Runtimes still running after that are abandoned with a synthetic
    /// `exited{code: null}` frame, so the hub closes their threads on the next connection.
    pub async fn shutdown(&self, grace: Duration) {
        self.stop_all();
        let deadline = tokio::time::Instant::now() + grace;
        loop {
            let ended = self.inner.shared.ended.notified();
            tokio::pin!(ended);
            ended.as_mut().enable();
            if self.live_count() == 0 {
                return;
            }
            if tokio::time::timeout_at(deadline, ended).await.is_err() {
                break;
            }
        }
        let shared = &self.inner.shared;
        // Each session's process taken with it, under the session table's lock. Their working
        // directories stay in use until the end.
        let mut abandoned: Vec<(String, Live)> = shared.sessions().drain().collect();
        for (thread, live) in &mut abandoned {
            if let Some(p) = live.pump.take() {
                p.abort();
            }
            let (run, overflow) = (live.run.clone(), live.overflow.take());
            tracing::warn!(thread = %thread, "runtime did not stop in time; killing it");
            // Its processes do not outlive the runner: the one it reported, and those marked
            // with its run (it may not have reported one yet).
            if !reap_run(live.process.clone(), thread, &run, shared.start_time).await {
                // No exit for a process that may still run: its row (or start intent) stays,
                // and the next runner kills it and records the exit.
                tracing::error!(thread = %thread, "runtime still runs after being killed; leaving it to the next runner start");
                continue;
            }
            // A runtime stopped for a spool overflow ends for that reason.
            let tail = overflow
                .unwrap_or_else(|| "twr runner: shut down before the runtime exited".into());
            spool_exit(
                shared,
                thread,
                None,
                &tail,
                Some(&run),
                RuntimeChange::Ended,
            )
            .await;
        }
        drop(abandoned);
        shared.notify.notify_waiters();
    }
}

/// Sets the thread marker ([`reap::THREAD_ENV`]) and the run's id ([`reap::RUNTIME_GEN_ENV`])
/// in a runtime's environment (replacing whatever the hub sent): what finds its processes
/// after a runner crash, and tells them from another run's.
fn mark(cfg: &mut SpawnConfig, run: &str) {
    cfg.env
        .retain(|(k, _)| k != reap::THREAD_ENV && k != reap::RUNTIME_GEN_ENV);
    cfg.env
        .push((reap::THREAD_ENV.to_string(), cfg.thread.clone()));
    cfg.env
        .push((reap::RUNTIME_GEN_ENV.to_string(), run.to_string()));
}

/// The `exited` frame text.
fn exit_frame(
    thread: &str,
    rseq: u64,
    code: Option<i32>,
    stderr_tail: &str,
    run: Option<&str>,
) -> String {
    serde_json::to_string(&RunnerMsg::Exited {
        thread: thread.to_string(),
        rseq,
        code,
        stderr_tail: stderr_tail.to_string(),
        run: run.map(str::to_string),
        at_ms: now_ms(),
    })
    .unwrap_or_default()
}

/// The time a frame is captured ([`RunnerMsg::Event`] `at_ms`): Unix milliseconds, `None` for
/// a clock before 1970.
fn now_ms() -> Option<u64> {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    u64::try_from(d.as_millis()).ok()
}

/// Records an `exited` frame (forced past the spool bound) with `change` to the session's
/// runtime row, retrying failures ([`SPOOL_RETRY`]). When it cannot be stored the box fails.
/// Returns whether it was stored.
async fn spool_exit(
    shared: &Shared,
    thread: &str,
    code: Option<i32>,
    stderr_tail: &str,
    run: Option<&str>,
    change: RuntimeChange,
) -> bool {
    let mut delays = SPOOL_RETRY.iter();
    loop {
        let r = shared.spool.append_with(
            thread,
            true,
            |rseq| exit_frame(thread, rseq, code, stderr_tail, run),
            change.clone(),
        );
        let e = match r {
            Ok(_) => return true,
            Err(e) => e,
        };
        let Some(d) = delays.next() else {
            shared.fail(format!(
                "cannot record the exit of thread {thread}'s runtime: {e:#}"
            ));
            return false;
        };
        tracing::warn!(thread, error = %format!("{e:#}"), ?d, "spooling a runtime exit failed; retrying");
        tokio::time::sleep(*d).await;
    }
}

/// A recorded runtime as a stale-process description, of run `generation` when given.
fn stale_of(r: &RuntimeRecord, generation: Option<&str>) -> StaleProcess {
    StaleProcess {
        thread: r.thread.clone(),
        pid: r.pid,
        start_time: r.start_time,
        exe: r.exe.clone(),
        generation: generation.map(str::to_string),
    }
}

/// The box's locks (see the module docs): its worktrees lock (`.twr-runner-<box>.lock` in
/// `worktrees`, made if missing) and, when given, its machine lock `machine` (its directory
/// made private if missing). The spool locks itself ([`Spool::open`]). An error names the lock
/// another process holds ([`lock_file`]).
fn lock_box(
    spool: &std::path::Path,
    worktrees: &std::path::Path,
    machine: Option<&std::path::Path>,
) -> anyhow::Result<Vec<std::fs::File>> {
    let stem = spool
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "runner".into());
    std::fs::create_dir_all(worktrees).map_err(|e| {
        anyhow::anyhow!(
            "creating the worktrees directory {}: {e}",
            worktrees.display()
        )
    })?;
    let mut locks = vec![lock_file(&worktrees.join(format!(".twr-{stem}.lock")))?];
    if let Some(machine) = machine {
        if let Some(dir) = machine.parent() {
            twr_core::paths::ensure_private_dir(dir)?;
        }
        locks.push(lock_file(machine)?);
    }
    Ok(locks)
}

/// An exclusive lock on the file at `path` (made, private, if missing), waiting at most
/// [`crate::spool::LOCK_WAIT`] for it (blocking): a runner started again right as its predecessor exits gets
/// it, a second one running beside the first does not.
fn lock_file(path: &std::path::Path) -> anyhow::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| anyhow::anyhow!("opening the runner lock {}: {e}", path.display()))?;
    let deadline = std::time::Instant::now() + crate::spool::LOCK_WAIT;
    let mut locked = file.try_lock();
    while matches!(locked, Err(std::fs::TryLockError::WouldBlock))
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(50));
        locked = file.try_lock();
    }
    match locked {
        Ok(()) => Ok(file),
        Err(std::fs::TryLockError::WouldBlock) => Err(anyhow::anyhow!(
            "another runner of this box is running on this machine (it holds {}): stop it \
             first; two runners of one box would undo each other's work",
            path.display()
        )),
        Err(std::fs::TryLockError::Error(e)) => Err(anyhow::anyhow!(
            "locking the runner lock {}: {e}",
            path.display()
        )),
    }
}

/// Kills the runtimes a previous runner left behind (rows in `runtimes`: their exit was never
/// recorded; rows in `spawn_intents`: they never reported their process), checks that nothing
/// of them runs any more, and records their exits. One that still runs (or whose processes
/// cannot be told apart, the process table unreadable) keeps its row and has no exit recorded;
/// then this fails, naming them, once the others are recorded (see the module docs). Blocking.
fn recover_runtimes(spool: &Spool) -> anyhow::Result<()> {
    let mut survivors: Vec<String> = Vec::new();
    for i in spool.intents()? {
        // Without a lower bound on the start time nothing is safe to kill.
        let killed = i
            .min_start
            // Every run of the thread: the runner that started them is gone, and this one
            // has started none yet.
            .is_some_and(|min| reap::reap_marked(&i.thread, min, None));
        // What is left: any process marked for the thread (this runner has started none, and
        // without a lower bound every one of them counts), or that may be (unreadable).
        if let Some(why) = reap::leftover(&i.thread, None, i.min_start) {
            tracing::error!(thread = %i.thread, why, "a runtime start left by a previous runner may still run: its exit is not recorded");
            survivors.push(format!(
                "thread {} (a runtime that never reported its process: {why})",
                i.thread
            ));
            continue;
        }
        // The runtime never reported its process (it was initializing, or its adapter reports
        // none).
        let tail = if killed {
            "twr runner: the runner that started this runtime died without recording its exit; \
             the processes marked for its thread were killed"
        } else {
            "twr runner: the runner that started this runtime died without recording its exit; \
             no process of it was left"
        };
        tracing::warn!(thread = %i.thread, killed, "recovered a runtime start left by a previous runner");
        spool.append_with(
            &i.thread,
            true,
            |rseq| exit_frame(&i.thread, rseq, None, tail, None),
            RuntimeChange::Ended,
        )?;
    }
    for r in spool.runtimes()? {
        // Any run: the runner that started it is gone. Without its start time the process
        // itself is never signalled (its pid alone may be another's now).
        let stale = stale_of(&r, None);
        let mut killed = reap::reap_stale(&stale);
        // The thread's marked processes since it started (its descendants that left its
        // group; the runtime too when only a lower bound is known).
        if let Some(from) = r.start_time.or(r.min_start) {
            killed |= reap::reap_marked(&r.thread, from, None);
        }
        let from = r.start_time.or(r.min_start);
        if let Some(why) = reap::leftover(&r.thread, Some(&stale), from) {
            tracing::error!(thread = %r.thread, pid = r.pid, why, "a runtime left by a previous runner may still run: its exit is not recorded");
            survivors.push(format!("thread {} ({why})", r.thread));
            continue;
        }
        let tail = if killed {
            format!(
                "twr runner: the runner that started this runtime died without recording its \
                 exit; its process {} was killed",
                r.pid
            )
        } else {
            format!(
                "twr runner: the runner that started this runtime died without recording its \
                 exit; its process {} was already gone",
                r.pid
            )
        };
        tracing::warn!(thread = %r.thread, pid = r.pid, killed, "recovered a runtime left by a previous runner");
        spool.append_with(
            &r.thread,
            true,
            |rseq| exit_frame(&r.thread, rseq, None, &tail, None),
            RuntimeChange::Ended,
        )?;
    }
    if !survivors.is_empty() {
        anyhow::bail!(
            "runtimes a previous runner of this box left behind may still run after being \
             killed: {}. Their ends are not recorded, so the hub starts nothing next to them; \
             the runner does not start until they are gone (stop them, then start it again)",
            survivors.join("; ")
        );
    }
    Ok(())
}

/// `ext.twr.pid` of a session start event.
fn session_pid(body: &EventBody) -> Option<u32> {
    let (EventBody::ThreadStarted(p) | EventBody::ThreadResumed(p)) = body else {
        return None;
    };
    p.ext
        .as_ref()?
        .get("twr")?
        .get("pid")?
        .as_u64()?
        .try_into()
        .ok()
}

/// Adds `ext.twr.start_time` (`start_time`, read off the async runtime by the caller) next to
/// `ext.twr.pid`.
fn add_start_time(body: &mut EventBody, start_time: Option<u64>) {
    let (EventBody::ThreadStarted(p) | EventBody::ThreadResumed(p)) = body else {
        return;
    };
    let Some(twr) = p
        .ext
        .as_mut()
        .and_then(|e| e.get_mut("twr"))
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    if twr.contains_key("start_time") {
        return;
    }
    if let Some(st) = start_time {
        twr.insert("start_time".into(), json!(st));
    }
}

/// An `event` frame waiting for its `rseq`.
struct EventFrame {
    thread: String,
    body: EventBody,
    raw: Option<String>,
    turn: Option<u64>,
    run: String,
    /// When the adapter emitted it ([`RunnerMsg::Event`] `at_ms`).
    at_ms: Option<u64>,
}

impl EventFrame {
    fn text(&self, rseq: u64) -> String {
        serde_json::to_string(&RunnerMsg::Event {
            thread: self.thread.clone(),
            rseq,
            turn: self.turn,
            body: self.body.clone(),
            raw: self.raw.clone(),
            run: Some(self.run.clone()),
            at_ms: self.at_ms,
        })
        .unwrap_or_default()
    }

    /// Drops the raw line if the frame does not fit in a WebSocket message; `Err` (with the
    /// frame's size) when it does not fit even without it.
    fn fit(&mut self) -> Result<(), usize> {
        let limit = MAX_FRAME_BYTES - FRAME_MARGIN;
        if self.text(0).len() > limit {
            self.raw = None;
            let len = self.text(0).len();
            if len > limit {
                return Err(len);
            }
        }
        Ok(())
    }
}

/// The placeholder sent instead of an event too large for one frame (see the module docs):
/// the same kind, marked `_dropped` with the original size.
fn oversized_placeholder(body: &EventBody, bytes: usize) -> EventBody {
    let marker = json!({
        "bytes": bytes,
        "reason": "event too large for one runner frame; its content was dropped",
    });
    let twr_ext = |ext: Option<twr_core::event::JsonMap>| {
        let mut ext = ext.unwrap_or_default();
        let twr = ext.entry("twr").or_insert_with(|| json!({}));
        if !twr.is_object() {
            *twr = json!({});
        }
        if let Some(t) = twr.as_object_mut() {
            t.insert("_dropped".into(), marker.clone());
        }
        Some(ext)
    };
    if let EventBody::TurnEnded(t) = body {
        // What closes the turn survives; usage and extras go.
        return EventBody::TurnEnded(twr_core::event::TurnEnded {
            result: t.result,
            session_id: t
                .session_id
                .as_deref()
                .map(|s| twr_core::blob::truncate_utf8(s, PLACEHOLDER_STRING).to_string()),
            ext: twr_ext(None),
            ..Default::default()
        });
    }
    let kind = body.kind().to_string();
    let mut data = body.data().unwrap_or(Value::Null);
    shrink_value(&mut data, true);
    if let Value::Object(obj) = &mut data {
        obj.insert("_dropped".into(), marker.clone());
        let ext = match obj.remove("ext") {
            Some(Value::Object(m)) => Some(m),
            _ => None,
        };
        obj.insert(
            "ext".into(),
            Value::Object(twr_ext(ext).unwrap_or_default()),
        );
    }
    EventBody::from_kind_data(&kind, data).unwrap_or_else(|_| {
        // The shrunk payload no longer parses (a field's type changed shape): the kind's
        // defaults, marked.
        let data = json!({"_dropped": marker, "ext": {"twr": {"_dropped": marker}}});
        EventBody::from_kind_data(&kind, data.clone()).unwrap_or(EventBody::Unknown { kind, data })
    })
}

/// Cuts long strings and empties large arrays/objects of `v`, in place.
fn shrink_value(v: &mut Value, top: bool) {
    match v {
        Value::String(s) if s.len() > PLACEHOLDER_STRING => {
            *s = twr_core::blob::truncate_utf8(s, PLACEHOLDER_STRING).to_string();
        }
        Value::Array(a) => {
            if !top && serde_json::to_vec(a).map_or(true, |b| b.len() > PLACEHOLDER_CONTAINER) {
                a.clear();
            } else {
                a.iter_mut().for_each(|x| shrink_value(x, false));
            }
        }
        Value::Object(o) => {
            if !top && serde_json::to_vec(o).map_or(true, |b| b.len() > PLACEHOLDER_CONTAINER) {
                o.clear();
            } else {
                o.values_mut().for_each(|x| shrink_value(x, false));
            }
        }
        _ => {}
    }
}

/// The runtime process a session start event reports (`ext.twr.pid`, with `start_time` and
/// `exe` when known), for the spool's `runtimes` table.
fn runtime_record(thread: &str, runtime: &str, body: &EventBody) -> Option<RuntimeRecord> {
    let (EventBody::ThreadStarted(p) | EventBody::ThreadResumed(p)) = body else {
        return None;
    };
    let twr = p.ext.as_ref()?.get("twr")?;
    Some(RuntimeRecord {
        thread: thread.to_string(),
        runtime: runtime.to_string(),
        pid: twr.get("pid")?.as_u64()?.try_into().ok()?,
        start_time: twr.get("start_time").and_then(Value::as_u64),
        exe: twr.get("exe").and_then(Value::as_str).map(str::to_string),
        min_start: None,
    })
}

/// Spools `frame` for its thread, retrying failures ([`SPOOL_RETRY`]) before giving up.
async fn store_event(
    shared: &Shared,
    frame: Arc<EventFrame>,
    change: RuntimeChange,
) -> anyhow::Result<Appended> {
    let mut delays = SPOOL_RETRY.iter();
    loop {
        let (spool, f, ch) = (shared.spool.clone(), Arc::clone(&frame), change.clone());
        let r = tokio::task::spawn_blocking(move || {
            spool.append_with(&f.thread, false, |rseq| f.text(rseq), ch)
        })
        .await;
        let e = match r {
            Ok(Ok(a)) => return Ok(a),
            Ok(Err(e)) => e,
            Err(e) => anyhow::anyhow!("spool task failed: {e}"),
        };
        let Some(d) = delays.next() else {
            return Err(e);
        };
        tracing::warn!(thread = %frame.thread, error = %format!("{e:#}"), ?d, "spooling an event failed; retrying");
        tokio::time::sleep(*d).await;
    }
}

/// Records the session's exit and forgets it (if it is still generation `gen`) atomically, so
/// a new session of the thread can only start after its predecessor's last frame. Each attempt
/// holds the session table; failures are retried ([`SPOOL_RETRY`]), then the box fails and the
/// session is forgotten without an exit (its runtime row, if any, makes the next runner record
/// one).
async fn finish_session(
    shared: &Shared,
    thread: &str,
    (gen, run): (u64, &str),
    code: Option<i32>,
    tail: &str,
) {
    let mut delays = SPOOL_RETRY.iter();
    loop {
        let e = {
            let mut sessions = shared.sessions();
            let r = shared.spool.append_with(
                thread,
                true,
                |rseq| exit_frame(thread, rseq, code, tail, Some(run)),
                RuntimeChange::Ended,
            );
            match r {
                Ok(_) => {
                    if sessions.get(thread).is_some_and(|l| l.gen == gen) {
                        sessions.remove(thread);
                    }
                    return;
                }
                Err(e) => e,
            }
        };
        let Some(d) = delays.next() else {
            shared.fail(format!(
                "cannot record the exit of thread {thread}'s runtime: {e:#}"
            ));
            forget_session(shared, thread, gen);
            return;
        };
        tracing::warn!(thread, error = %format!("{e:#}"), ?d, "spooling a runtime exit failed; retrying");
        tokio::time::sleep(*d).await;
    }
}

fn forget_session(shared: &Shared, thread: &str, gen: u64) {
    let mut sessions = shared.sessions();
    if sessions.get(thread).is_some_and(|l| l.gen == gen) {
        sessions.remove(thread);
    }
}

/// A runtime stopped for a spool overflow that is still live after [`OVERFLOW_KILL_AFTER`]
/// (its adapter did not report its exit) has what is left of its run killed: its recorded
/// process and the processes marked with the run's id ([`reap::RUNTIME_GEN_ENV`]). Its adapter
/// then reports the exit, which records it.
async fn kill_overflowed(weak: Weak<Shared>, thread: String, gen: u64, run: String) {
    let hold = match weak.upgrade() {
        Some(shared) => shared
            .overflow_hold
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone(),
        None => return,
    };
    tokio::time::sleep(hold.as_ref().map_or(OVERFLOW_KILL_AFTER, |h| h.after)).await;
    // The session is still this run, and the process it reported, taken under one lock: once
    // the lock is released the run may end and the thread's next run record its own process.
    let recorded = {
        let Some(shared) = weak.upgrade() else {
            return;
        };
        let sessions = shared.sessions();
        match sessions.get(&thread).filter(|l| l.gen == gen) {
            Some(l) => l.process.clone(),
            None => return,
        }
    };
    let done = match &hold {
        Some(h) => overflow_pause(&h.pauses, &thread).await,
        None => None,
    };
    tracing::warn!(thread = %thread, "a runtime stopped for a spool overflow is still running; killing it");
    let min_start = weak.upgrade().and_then(|s| s.start_time);
    reap_run(recorded, &thread, &run, min_start).await;
    if let Some(done) = done {
        let _ = done.send(());
    }
}

/// [`LocalBox::hold_overflow_kills`]: reports the kill of `thread`'s run and waits for its go.
async fn overflow_pause(
    pauses: &mpsc::UnboundedSender<OverflowKillPause>,
    thread: &str,
) -> Option<oneshot::Sender<()>> {
    let (go, go_rx) = oneshot::channel();
    let (done_tx, done) = oneshot::channel();
    pauses
        .send(OverflowKillPause {
            thread: thread.to_string(),
            go,
            done,
        })
        .ok()?;
    let _ = go_rx.await;
    Some(done_tx)
}

/// The process run `gen` of `thread` reported, taken under the session table's lock while the
/// session is still that run (`None` once it is not, or before it reported one).
fn session_process(shared: &Shared, thread: &str, gen: u64) -> Option<StaleProcess> {
    shared
        .sessions()
        .get(thread)
        .filter(|l| l.gen == gen)
        .and_then(|l| l.process.clone())
}

/// Kills what is left of run `run` of `thread` on this box: its recorded process `rec`
/// (identity-checked, including the run's marker: `rec` names the run as its generation) and
/// the processes marked with the run's id ([`reap::RUNTIME_GEN_ENV`]) that started after this
/// runner (`min_start`, [`Shared::start_time`]). Returns whether none of them is left running
/// (a process table that cannot be read counts as running).
async fn reap_run(
    rec: Option<StaleProcess>,
    thread: &str,
    run: &str,
    min_start: Option<u64>,
) -> bool {
    let (t, r) = (thread.to_string(), run.to_string());
    tokio::task::spawn_blocking(move || {
        if let Some(p) = &rec {
            reap::reap_stale(p);
        }
        if let Some(m) = min_start {
            reap::reap_marked(&t, m, Some(&r));
        }
        !reap::runtime_running(&t, rec.as_ref(), min_start, Some(&r))
    })
    .await
    .unwrap_or(false)
}

/// What a session was started with, stamped on its start events ([`add_settings`]).
struct StartSettings {
    /// The runtime.
    runtime: String,
    /// The permission mode.
    mode: String,
    /// The model (`None`: the runtime's default).
    model: Option<String>,
    /// The reasoning effort (`None`: the runtime's default).
    effort: Option<String>,
    /// An `acp` session's agent: its argv, from the spawn's
    /// [`twr_adapters::acp::COMMAND_ENV`] (`None`: none, or not a JSON array).
    agent: Option<Value>,
}

impl StartSettings {
    /// The settings `cfg` starts a session of `runtime` with (a blank model or effort is
    /// none).
    fn of(runtime: &str, cfg: &SpawnConfig) -> StartSettings {
        let set = |v: &Option<String>| v.clone().filter(|s| !s.trim().is_empty());
        let agent = cfg
            .env
            .iter()
            .find(|(k, _)| k == twr_adapters::acp::COMMAND_ENV)
            .and_then(|(_, v)| serde_json::from_str::<Value>(v).ok())
            .filter(Value::is_array);
        StartSettings {
            runtime: runtime.to_string(),
            mode: cfg.mode.clone(),
            model: set(&cfg.model),
            effort: set(&cfg.effort),
            agent,
        }
    }
}

/// Adds `ext.twr.runtime`, `ext.twr.mode`, `ext.twr.model` and `ext.twr.effort` (what the
/// session was started with; a model or effort of `null` is the runtime's default), and for
/// an `acp` session `ext.twr.acp_command` (its agent, so a hub that takes the session over
/// after a move to another agent knows it for the previous one), to a session start event
/// that has none of them.
fn add_settings(body: &mut EventBody, settings: &StartSettings) {
    let (EventBody::ThreadStarted(p) | EventBody::ThreadResumed(p)) = body else {
        return;
    };
    let ext = p.ext.get_or_insert_with(Default::default);
    let twr = ext
        .entry("twr")
        .or_insert_with(|| Value::Object(Default::default()));
    if let Some(twr) = twr.as_object_mut() {
        twr.entry("runtime")
            .or_insert_with(|| Value::String(settings.runtime.clone()));
        twr.entry("mode")
            .or_insert_with(|| Value::String(settings.mode.clone()));
        let value = |v: &Option<String>| v.clone().map_or(Value::Null, Value::String);
        twr.entry("model").or_insert_with(|| value(&settings.model));
        twr.entry("effort")
            .or_insert_with(|| value(&settings.effort));
        if let Some(agent) = &settings.agent {
            twr.entry(ACP_COMMAND_KEY).or_insert_with(|| agent.clone());
        }
    }
}

/// Moves one session's events into the spool until it exits.
#[allow(clippy::too_many_arguments)]
async fn pump(
    weak: Weak<Shared>,
    thread: String,
    runtime: String,
    gen: u64,
    run: String,
    settings: StartSettings,
    cmd: mpsc::UnboundedSender<AdapterCmd>,
    mut events: mpsc::UnboundedReceiver<AdapterEvent>,
) {
    // Spool full: the runtime was stopped (why); its exit, when it comes, says so.
    let mut overflowed: Option<String> = None;
    // The spool failed: the runtime was stopped and nothing more of it is recorded.
    let mut broken = false;
    let mut exit: Option<(Option<i32>, String)> = None;
    while let Some(ev) = events.recv().await {
        // The box is gone (dropped: a crash as far as the spool is concerned).
        let Some(shared) = weak.upgrade() else {
            return;
        };
        match ev {
            AdapterEvent::Emit {
                mut body,
                raw,
                turn,
            } => {
                if overflowed.is_some() || broken {
                    continue;
                }
                add_settings(&mut body, &settings);
                if let Some(pid) = session_pid(&body) {
                    // Reading the process table may take a while (`ps`, bounded).
                    let start_time =
                        tokio::task::spawn_blocking(move || reap::process_start_time(pid))
                            .await
                            .ok()
                            .flatten();
                    add_start_time(&mut body, start_time);
                    let session_id = match &body {
                        EventBody::ThreadStarted(p) | EventBody::ThreadResumed(p) => {
                            p.session_id.clone()
                        }
                        _ => None,
                    };
                    // What a kill of this run targets: its process, marked with the run.
                    let process =
                        runtime_record(&thread, &runtime, &body).map(|r| stale_of(&r, Some(&run)));
                    if let Some(l) = shared.sessions().get_mut(&thread).filter(|l| l.gen == gen) {
                        l.pid = Some(pid);
                        l.process = process;
                        l.session_id = session_id.or(l.session_id.take());
                    }
                } else if let EventBody::ThreadStarted(p) | EventBody::ThreadResumed(p) = &body {
                    if let Some(l) = shared.sessions().get_mut(&thread).filter(|l| l.gen == gen) {
                        l.session_id = p.session_id.clone().or(l.session_id.take());
                    }
                }
                let change = match runtime_record(&thread, &runtime, &body) {
                    Some(mut r) => {
                        // Started after this runner (what its intent recorded).
                        r.min_start = shared.start_time;
                        RuntimeChange::Started(r)
                    }
                    None => RuntimeChange::None,
                };
                let kind = body.kind().to_string();
                let mut frame = EventFrame {
                    thread: thread.clone(),
                    body,
                    raw,
                    turn,
                    run: run.clone(),
                    at_ms: now_ms(),
                };
                if let Err(bytes) = frame.fit() {
                    shared.dropped_events.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(
                        thread = %thread,
                        kind,
                        bytes,
                        "an event is too large for one WebSocket message; sending a placeholder"
                    );
                    frame.body = oversized_placeholder(&frame.body, bytes);
                    if let Err(bytes) = frame.fit() {
                        tracing::error!(thread = %thread, kind, bytes, "dropping an event whose placeholder is still too large");
                        continue;
                    }
                }
                match store_event(&shared, Arc::new(frame), change).await {
                    Ok(Appended::Stored { .. }) => shared.notify.notify_waiters(),
                    Ok(Appended::Full) => {
                        tracing::error!(
                            thread = %thread,
                            max_bytes = shared.spool.max_bytes(),
                            "runner spool is full; stopping this runtime"
                        );
                        let _ = cmd.send(AdapterCmd::Stop);
                        let tail = format!(
                            "twr runner: spool full ({} bytes); runtime stopped, later events lost",
                            shared.spool.max_bytes()
                        );
                        // The session stays live until its process has exited: only then is
                        // its exit recorded (below), so nothing treats it as gone before.
                        if let Some(l) = shared.sessions().get_mut(&thread).filter(|l| l.gen == gen)
                        {
                            l.overflow = Some(tail.clone());
                        }
                        overflowed = Some(tail);
                        tokio::spawn(kill_overflowed(
                            Weak::clone(&weak),
                            thread.clone(),
                            gen,
                            run.clone(),
                        ));
                    }
                    Err(e) => {
                        broken = true;
                        let _ = cmd.send(AdapterCmd::Stop);
                        shared.fail(format!(
                            "cannot spool an event ({kind}) of thread {thread}: {e:#}; its runtime \
                             was stopped"
                        ));
                    }
                }
            }
            AdapterEvent::Exited { code, stderr_tail } => {
                exit = Some((code, stderr_tail));
                break;
            }
        }
    }
    let Some(shared) = weak.upgrade() else {
        return;
    };
    if broken {
        // Nothing more is recorded: the spool failed (the box has failed; a runtime row makes
        // the next runner record the end).
        forget_session(&shared, &thread, gen);
    } else {
        let (code, tail) = match exit {
            Some(exit) => exit,
            None => {
                // The adapter went away without reporting an exit: its process may still run.
                // It is killed first, and the exit recorded only once none is left (as the
                // hub does for its own runtimes). (Its process is taken only while the session
                // is still this run; after a shutdown took the session, the run's markers
                // still find what is left.)
                let process = session_process(&shared, &thread, gen);
                while !reap_run(process.clone(), &thread, &run, shared.start_time).await {
                    tracing::error!(thread = %thread, "the runtime of an adapter that went away still runs after being killed; retrying");
                    tokio::time::sleep(REAP_RETRY).await;
                }
                (
                    None,
                    "twr runner: the adapter ended without reporting an exit".to_string(),
                )
            }
        };
        let (code, tail) = match overflowed {
            // Stopped for the overflow: that is why it ended (its own tail follows).
            Some(why) if tail.trim().is_empty() => (None, why),
            Some(why) => (None, format!("{why}\n{tail}")),
            None => (code, tail),
        };
        finish_session(&shared, &thread, (gen, &run), code, &tail).await;
    }
    tracing::info!(thread = %thread, "runtime exited");
    shared.notify.notify_waiters();
    shared.ended.notify_waiters();
}
