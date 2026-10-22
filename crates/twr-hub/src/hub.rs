//! The hub core: single-writer append, reducers, turn dispatch and adapter supervision.
//!
//! All state changes go through `Core::append_batch` under one mutex: it assigns each event
//! its per-thread seq, ULID and timestamp, caps the payloads, commits the events, their control
//! rows and blobs (and a dequeued prompt, and newly queued prompts) in one SQLite transaction, then
//! folds the events into the [`Registry`] and [`AttentionStack`] and broadcasts them.
//! Subscribers that fall behind the broadcast channel catch up from SQLite, so nothing is ever
//! dropped. A logical operation made of several appends is one batch: a hub
//! question's answer with its routing decision and queued message; a turn end's grade, report
//! and review; a written report with its review; a turn's start with the context it logs and
//! the items a retry resolves; a closing sequence; `say`'s decisions with what they route.
//! The few operations that stay multi-step (they involve git, files or a runtime) say at the
//! call site why stopping part-way is safe or repaired at startup.
//!
//! Turns: `thread.send` queues the prompt in `pending_prompts`; a thread gets
//! its next prompt only when no turn is open. Dispatch appends `turn.started` (removing the
//! queue row in the same transaction), then sends the prompt to the live adapter, or resumes
//! the session, or spawns a new one. The queue row id is the message's ref, which the turn
//! records (`ext.twr.ref`): `message.withdraw` takes a message back by it while it is queued,
//! or while its turn runs and has not acted yet (`hub::withdraw`).
//!
//! Process exit: the open turn ends `interrupted`, open adapter-raised
//! attention items resolve `thread_stopped`, then `thread.stopped` (the hub asked) or
//! `thread.crashed` plus an `error` attention for the user. Startup applies the same closing
//! sequence to every thread whose log shows a live process. From the exit until its closing
//! sequence is recorded the thread takes no turn and nothing else closes it
//! ([`RuntimeExiting`]). A `thread.stop` is recorded (the store's `stop_requests`, and the
//! remote `stop` command) before the runtime is told: whenever its exit is recorded (by this
//! hub, the next one at startup, or after a remote session is taken over again) the thread
//! closes as stopped, never crashed. A remote runtime whose box is offline (after a hub
//! restart, one the log shows running that is not taken over yet) is stopped the same way:
//! `thread.stop` answers `stopping` with `box_offline`, and the stop goes out when the box
//! reconnects. A local runtime is recorded as a spawn intent before it starts, so a hub that
//! dies before the runtime reported its pid still kills it at startup (by its `TWR_THREAD`
//! marker and start time). Shutdown kills a runtime that outlives its stop
//! (and the stop timeout) before recording it stopped.
//!
//! Attention ids of a runtime are `<thread>:<generation>:<its own id>`: the generation is one
//! more than the runtimes the thread's log has closed, so an answer to an item of an earlier
//! runtime (runtimes number their requests per process) never reaches a later one.
//!
//! Attach: `attach.begin` hands the thread to the runtime's own interactive CLI.
//! It refuses while another client is attached, and while a turn or background tasks run unless
//! forced (then the turn is interrupted and the tasks end with the runtime). The live runtime is
//! stopped and its exit awaited; that exit closes the open turn and adapter attention like any
//! other exit but appends `thread.attached` instead of `thread.stopped`. While attached nothing
//! is dispatched (prompts queue). `attach.end`, the
//! lease connection closing, or a hub restart append `thread.detached`, and the queue then
//! resumes the session lazily. The client reports the interactive process it runs
//! (`attach.child`: pid and start time; the process carries [`ATTACH_ENV`]`=<thread>:<lease>`
//! and the agent marker `TWR_AGENT`, never the runtime markers). Once reported, the attach outlives its lease connection and a
//! hub restart (`store` `attach_processes`) for as long as that process runs: the thread stays
//! attached (no park, no dispatch, its directory held) and `thread.detached` (reason
//! `attach process exited`) is appended when it is gone. The check reads `/proc` on the hub's
//! machine, which is where the client runs (it talks to the hub over its local socket; for a
//! remote box the process is the local `ssh`).
//!
//! Decisions and the orchestrator: grading runs the `twr_decisions` ladder (its records become
//! `decision.made`); `say` routes user messages (`hub::route`); the orchestrator thread, its
//! token, notices, the relay guard, written reports and annotations live in `hub::orch`.
//! Callers are identified by the server ([`Caller`]); a connection that comes from a process
//! twr started for a thread is that thread's agent ([`AgentPeer`], `hub::agents`), which the
//! server lets only read.
//! Standing project context and its injection into worker prompts live in `hub::context`;
//! the block_kind decision and the one automatic retry of a failed worker turn in
//! `hub::retry`; the stall timer in `hub::stall`.
//!
//! Worktrees, park and resume (`hub::git`): `thread.new` gives a worker thread in
//! a git repository its own worktree on branch `twr/<name>` before `thread.created` is
//! appended. `thread.park` quiesces the thread like an attach (the runtime's exit runs the
//! closing sequence and appends `thread.parked` instead of `thread.stopped`), then removes the
//! worktree when it is clean; `thread.resume` recreates it at the same path and appends
//! `thread.resumed`. Nothing is dispatched while a thread is parked (prompts queue). The hub
//! appends `git.snapshot` after turns end and on a tick, only when it changed.
//!
//! Remote boxes (`hub::runners`, [`crate::remote`]): a thread on a box whose
//! runner connects to the hub runs there; its events arrive as runner frames, each recorded
//! with its delivery watermark in one transaction (not through `supervise`), its commands go
//! out through a [`RemoteBox`] session. While the box is offline its threads keep their state,
//! nothing is dispatched to them and box commands answer `box_offline`. At startup the hub
//! leaves such threads to the reconciliation of the runner's next connection; at shutdown it
//! lets go of their runtimes (they keep running) instead of stopping them.
//!
//! One hub per data directory: [`Hub::open`] takes an exclusive lock on `<data_dir>/twr.lock`
//! before reading the log and holds it for the hub's lifetime.
//!
//! Adapter events are checked before they are recorded: only the kinds an adapter may emit
//! ([`adapter_may_emit`]) are accepted, in-turn kinds without a turn get the open turn, and a
//! `turn.ended` for anything but the open turn is dropped, so reports and dispatch only ever
//! react to the turn the hub started.
//!
//! Store failures: an adapter event whose append fails for any reason but its content (see
//! [`rejection`]) is retried with backoff before the adapter's next event is taken, so the log
//! keeps the runtime's order. When the store stays unwritable the hub stops that runtime and
//! records its closing sequence (`thread.crashed`); if that cannot be recorded either, the hub
//! fails ([`Hub::fatal_error`]) and shuts down rather than run on with state its log lacks.
//!
//! Layout: this file holds the hub's types ([`Hub`], `Inner`, `Core`, `Batch`, the runtime
//! bookkeeping), the turn queue and dispatch. Recording events and the checks before it are in
//! `hub::append`; the adapter supervisor, the adapter event checks and the store retries in
//! `hub::supervisor`; closing sequences in `hub::closing`; turn-end reactions in
//! `hub::reactions`; the thread, attach, attention and log-reading RPCs in `hub::threads`,
//! `hub::attach`, `hub::attention` and `hub::subscribe`; opening and shutting down the hub in
//! `hub::lifecycle`.
//!
//! [`ATTACH_ENV`]: crate::host::ATTACH_ENV

mod activity;
mod after;
mod agents;
mod append;
mod attach;
mod attention;
mod background;
#[cfg(test)]
mod bench;
mod box_agents;
mod box_switch;
mod boxes;
mod checkpoint;
mod closing;
mod coalesce;
mod commands;
mod compact;
mod config_view;
mod context;
/// `O_NOFOLLOW | O_NONBLOCK` for this platform (0 where twr does not spell it out): for opening
/// a file that must be neither a symlink swapped in nor a FIFO that blocks the open.
pub use context::OPEN_FLAGS as OPEN_NOFOLLOW_NONBLOCK;
mod delete;
mod disk;
mod drain;
mod find_project;
mod git;
mod glance;
mod handoff;
mod hook;
mod jobs;
mod lifecycle;
mod limits;
pub mod memory;
pub use memory::DIR as MEMORY_DIR;
mod mode;
mod model;
mod orch;
mod project_read;
mod proposal;
mod prune;
mod queue;
mod reactions;
mod rename;
mod restart;
mod retry;
mod route;
mod runners;
mod runtime_spec;
mod shell;
mod spawn;
mod stall;
mod subscribe;
mod suggest;
pub use suggest::SuggestAsk;
mod summary;
pub use summary::HOURLY_MAX as SUMMARY_HOURLY_MAX;
mod supervisor;
mod switch;
mod threads;
mod trash;
mod updates;
mod usage;
mod wake;
mod watches;
mod withdraw;

pub use agents::{
    local_agent, AgentPeer, AgentWhy, PeerWalk, RuntimeRef, Started, StartedKind, StartedTable,
};
pub use append::{rejection, InvalidEvent};
pub use attach::AttachGrant;
pub use attention::Relay;
pub use box_agents::{
    own_agent_probe, AgentProbe, AgentsPoll, BoxView, AGENTS_EVERY, AGENTS_FIRST, AGENTS_TIMEOUT,
};
pub(crate) use boxes::WeakHub;
pub use checkpoint::{CHECKPOINT_EVERY, CHECKPOINT_TICK};
pub use config_view::config_view;
pub use context::{injection, CONTEXT_DIR, CONTEXT_FILE, INJECT_MAX, SET_MAX};
pub use disk::{DISK_TICK, NOT_NOW as DISK_NOT_NOW, TRIM as DISK_TRIM};
pub use git::{ParkOutcome, ResumeOutcome, GIT_TICK};
pub use handoff::{
    is_handoff, SwitchOptions, CONVERSATION_MAX, HANDOFF_REPLY_TIMEOUT, SUMMARY_WAIT,
    SUMMARY_WAIT_MAX,
};
pub use jobs::{AgentJobBy, JobStart, ShellJob, SlotPool};
pub(crate) use lifecycle::lock_data_dir;
pub use lifecycle::lock_opened;
pub use project_read::{ReadParams, Tool as ReadTool};
pub use retry::RETRY_PROMPT;
pub use runners::SealedEnd;
pub use shell::ShellRun;
pub(crate) use subscribe::stream_line;
pub(crate) use subscribe::wait_state;
pub use subscribe::{ControlSubscription, ThreadSubscription};
pub use supervisor::{adapter_may_emit, runtime_attn_id};
pub use usage::SpentBy;
pub use wake::{WAKE_CHAIN_MAX, WAKE_PROMPT_START};
pub use watches::{IDLE_GRACE, WATCHES_MAX, WATCH_NOTE_MAX};
pub use withdraw::OrchCall;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use twr_core::adapter::{AdapterHandle, Answer, SpawnConfig};
use twr_core::config::Config;
use twr_core::event::*;
use twr_core::proto::{codes, RpcError};
use twr_core::reduce::{AttentionStack, Registry, ResolveError, ThreadView};
use twr_decisions::Ladder;

use crate::host::{
    builtin_adapters, AdapterLookup, ThreadHost, AGENT_ENV, RUNTIME_GEN_ENV, THREAD_ENV,
};
use crate::store::{self, StagedBlobs, Store};
use supervisor::{split_runtime_attn_id, supervise};

/// Name of the hub's own box.
pub const LOCAL_BOX: &str = "local";

/// Lock file in the data directory held by the running hub.
pub const LOCK_FILE: &str = "twr.lock";

/// Longest identifier (`attn_id`, `call_id`, `session_id`, any other `*_id`) an event may carry,
/// in bytes. Identifiers are never moved to the blob store, so they must stay small.
pub const MAX_ID_BYTES: usize = 256;

/// Most aliases a thread may have.
pub const MAX_ALIASES: usize = 16;

/// How long `attach.begin` waits for the thread's runtime to exit (adapters escalate a stop to
/// a kill within their own grace period, e.g. 5 s for Claude Code).
const ATTACH_QUIESCE_TIMEOUT: Duration = Duration::from_secs(20);

/// How long shutdown waits for runtimes to exit by default: longer than any adapter's own
/// stop-to-kill grace (5 s for Claude Code), so a runtime that honors its stop is recorded
/// normally, and only one that outlives its kill is killed by the hub.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(8);

/// A closing sequence refused because the thread's runtime is exiting: its exit records the
/// closing sequence ([`Core::begin_exit`] .. [`Core::finish_exit`]), nothing else may.
#[derive(Debug)]
pub struct RuntimeExiting(pub String);

impl std::fmt::Display for RuntimeExiting {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the runtime of thread {} is exiting; try again in a moment",
            self.0
        )
    }
}

impl std::error::Error for RuntimeExiting {}

/// A control stream item: `(gseq, event)`.
pub type ControlItem = (u64, Arc<Event>);

/// Hub construction options.
pub struct HubOptions {
    /// SQLite database file.
    pub db_path: PathBuf,
    /// Configuration.
    pub config: Config,
    /// Instance label reported to clients (the CLI uses the `TWR_HOME` directory name).
    pub instance_name: String,
    /// The named workspace this hub serves (`None`: `default`): reports name it in their
    /// `twr -w <workspace> attach` command, and an attached CLI gets it as `TWR_WORKSPACE`.
    pub workspace: Option<String>,
    /// The data directory of the root the hub's workspace is in (the `default` workspace's):
    /// a local thread is never started in another workspace's worktrees
    /// (`<root>/worktrees`, `<root>/workspaces/<name>/worktrees`), which that workspace's hub
    /// removes and moves. `None`: no such check.
    pub workspaces_root: Option<PathBuf>,
    /// Namespace for new local worktree branches; `None` keeps `twr/<name>`.
    pub worktree_namespace: Option<String>,
    /// Capacity of the live broadcast channels (subscribers beyond it catch up from SQLite).
    pub broadcast_capacity: usize,
    /// How long shutdown waits for adapters to exit.
    pub stop_timeout: Duration,
    /// Runtime lookup for the local box.
    pub adapters: AdapterLookup,
    /// The socket the hub serves (the orchestrator's MCP server connects to it); without one
    /// the orchestrator runs without the twr tools.
    pub socket: Option<PathBuf>,
    /// The `twr` binary the orchestrator's runtime launches as its MCP server
    /// (`<twr_exe> mcp --socket <socket>`); defaults to the current executable.
    pub twr_exe: Option<PathBuf>,
    /// The program jobs start through, as `<it> __job-gate -- <argv...>`
    /// ([`crate::job_gate`]): the current executable by default, which is `twr` for a `twr hub`
    /// (whose `main` runs the gate first thing). `None`: jobs are refused. Tests set this
    /// crate's `twr-job-gate`.
    pub job_gate_exe: Option<PathBuf>,
    /// The Claude Code CLI that answers next-message suggestions (`hub::suggest`): `None`
    /// uses `TWR_CLAUDE_BIN`, else `claude` on the hub's `PATH`. Tests give a script.
    pub suggest_bin: Option<PathBuf>,
    /// The Codex CLI that answers them for Codex threads, or when Claude Code can't
    /// (`hub::suggest`): `None` uses `TWR_CODEX_BIN`, else `codex` on the hub's `PATH`. Tests
    /// give a script, or a path that doesn't exist.
    pub suggest_codex_bin: Option<PathBuf>,
    /// The Codex home whose global instructions keep Codex from answering suggestions
    /// (`hub::suggest`): `None` uses `$CODEX_HOME`, else `~/.codex`. Tests give a folder.
    pub suggest_codex_home: Option<PathBuf>,
    /// Where Claude Code's managed policy is read, to tell whether a Claude Code orchestrator's
    /// memory guard would run (`orch::restrict`); defaults to this system's locations (tests
    /// give their own).
    pub claude_policy: twr_adapters::claude_code::guard::PolicySources,
    /// How long a running turn may go without events before it is `stalled`; `None` uses
    /// `stall_after_secs` from the config. Zero turns the stall timer off.
    pub stall_after: Option<Duration>,
    /// How long after a worker's background work ended, with no turn after its interim
    /// report, the hub waits before it wakes the worker itself (`hub::wake`); `None` uses
    /// `wake_after_secs` from the config. Zero: never.
    pub wake_after: Option<Duration>,
    /// Period of the `git.snapshot` tick; `None` = [`GIT_TICK`] (30 s), zero = no tick
    /// (snapshots then only follow turn ends).
    pub git_tick: Option<Duration>,
    /// Period of the trash purge (items older than `[trash] keep_days` go, on every box that
    /// is online; also once right after the hub starts); `None` = [`trash::TRASH_TICK`] (an
    /// hour), zero = never on its own (`Hub::purge_trash` still does).
    pub trash_tick: Option<Duration>,
    /// Period of the disk round (each box's free space, the worktrees due a measurement, the
    /// disk cards; `hub::disk`); `None` = [`DISK_TICK`] (5 minutes), zero = never.
    pub disk_tick: Option<Duration>,
    /// How often the hub looks whether a checkpoint of its folded state is due
    /// (`hub::checkpoint`; the first time sooner); `None` = [`CHECKPOINT_TICK`] (an hour),
    /// zero = never.
    pub checkpoint_tick: Option<Duration>,
    /// How many events must come after the newest checkpoint before another one is written;
    /// `None` = [`CHECKPOINT_EVERY`] (5 000).
    pub checkpoint_events: Option<u64>,
    /// How often the hub looks whether its log has agent events to archive or prune
    /// (`hub::prune`, `[log]`; the first time sooner); `None` = an hour, zero = never on its
    /// own (`log.compact` still does).
    pub prune_tick: Option<Duration>,
    /// The lock of `boxes.toml` ([`twr_core::config::BoxesFile::lock`]) taken before `config`
    /// was read (`twr hub`), held by [`crate::run`] until the hub's socket is bound: a `twr box
    /// remove` or `twr box token --rotate` that edits the file under that lock and then finds
    /// no hub on the socket did so before this hub read the files, so the hub reads its edit.
    /// `None`: nothing is held (tests, or a lock file `twr hub` could not open).
    pub boxes_lock: Option<twr_core::config::BoxesLock>,
    /// The file `config` was read from: the hub warns when it holds secrets (a box token, a
    /// `[notify]` command) and other users can access it ([`Config::warnings`]). `None` (tests):
    /// only the config itself is checked.
    pub config_file: Option<PathBuf>,
    /// A test's user token ([`twr_core::client::TEST_USER_TOKEN_ENV`]): a connection whose
    /// `hello` presents it is the user's whatever its process ancestry. `twr hub` sets it only
    /// under `TWR_HOME` (a throwaway hub); `None` (always, for real use) = no such token.
    pub test_user_token: Option<String>,
    /// Where the machine's job slots are (`hub::jobs`): one directory for every hub of a home
    /// (`twr` passes `<data>/job-slots` of the home's root); `None` = `job-slots` beside the
    /// database (tests).
    pub job_slots_dir: Option<PathBuf>,
    /// How long a new runner connection has to authenticate, counted from its accept (TLS and
    /// WebSocket handshakes, hello and proofs together; [`runners::HELLO_TIMEOUT`] by
    /// default), and then again to send its inventory.
    pub hello_timeout: Duration,
    /// How long an authenticated runner connection may send no protocol message before it is
    /// closed (WebSocket pings do not count; [`runners::IDLE_TIMEOUT`] by default).
    pub runner_idle_timeout: Duration,
    /// Longest a runner frame's commit waits for its ack batch to be synced and acked
    /// ([`runners::ACK_BATCH_DELAY`] by default).
    pub ack_batch_delay: Duration,
    /// The git executable of the local box (`git` on `PATH` when `None`).
    pub git_bin: Option<PathBuf>,
    /// How long `orchestrator.use` waits for the previous orchestrator's handoff summary once
    /// it was asked for one ([`HANDOFF_REPLY_TIMEOUT`] by default).
    pub handoff_reply_timeout: Duration,
    /// How long a remote box's answer to a repository change is waited for
    /// ([`twr_core::git::CHANGE_REQUEST_TIMEOUT`] by default; tests shorten it).
    pub change_request_timeout: Duration,
    /// When an SSH box whose daemon runs another build than the one launched is switched to
    /// it ([`crate::ssh_box::SwitchTiming`]; its defaults, tests shorten them).
    pub box_switch: crate::ssh_box::SwitchTiming,
    /// The `systemd-run` and `systemctl` that limit the local workers under `[threads]
    /// cpu_weight` or `memory_high` (`hub::limits`; `None`: the ones on `PATH`).
    pub systemd_tools: Option<(PathBuf, PathBuf)>,
    /// How many of the local box's first start-time reads fail on purpose (tests of a
    /// runtime whose start time cannot be read).
    #[doc(hidden)]
    pub start_time_faults: usize,
    /// Test hook: the orchestrator token is accepted from a connection whose peer is not a
    /// thread's agent although its ancestry does not show the orchestrator's runtime (an
    /// in-process test runtime has no process to descend from). Never set
    /// outside tests.
    #[doc(hidden)]
    pub orchestrator_token_unverified: bool,
    /// Test hook: called with a connection's peer, off the async runtime, after its ancestry
    /// was walked and before its environments are read (a peer that exits in between). Never
    /// set outside tests.
    #[doc(hidden)]
    pub marker_read_hook: Option<MarkerReadHook>,
    /// Test hook: called with an agent's job request's peer, off the async runtime, after its
    /// ancestry was walked again and before that walk is confirmed (`server::job_opener`).
    /// Never set outside tests.
    #[doc(hidden)]
    pub peer_confirm_hook: Option<MarkerReadHook>,
    /// Test hook: called with a job's child, off the async runtime, while it waits at its start
    /// gate, before its process group is recorded (`hub::jobs`). Never set outside tests.
    #[doc(hidden)]
    pub job_gate_hook: Option<MarkerReadHook>,
    /// Test hook: what cleaning up a lost job's process group found, in place of
    /// [`twr_runner::reap::reap_job_group`]. Never set outside tests.
    #[doc(hidden)]
    pub reap_job_group_hook: Option<ReapJobGroupHook>,
    /// Test hook: called with a running job's process group, off the async runtime, before
    /// its members are read (`hub::jobs`, every 5 s while it runs). Never set outside tests.
    #[doc(hidden)]
    pub job_members_hook: Option<MarkerReadHook>,
    /// Test hook: an SSH box's supervisor dials the box through it instead of ssh (no probe,
    /// no upload, no relay; [`crate::ssh_box::BoxDialer`]). Never set outside tests.
    #[doc(hidden)]
    pub box_dialer: Option<crate::ssh_box::BoxDialer>,
    /// Test hook: awaited with the box's name by `box.add` and `box.projects` once they have
    /// read the config files, before they compare and apply what they read (a slow read).
    /// Never set outside tests.
    #[doc(hidden)]
    pub box_files_read_hook: Option<BoxFilesReadHook>,
    /// Test hook: awaited by [`crate::server::serve`] once the hub's socket is removed, before
    /// it stops its runtimes (a slow shutdown, its runner listener still up). Never set
    /// outside tests.
    #[doc(hidden)]
    pub socket_removed_hook: Option<SocketRemovedHook>,
    /// How the hub probes its own box's agent CLIs (`hub.info` `agents`, `box.agents local`;
    /// [`own_agent_probe`] for `twr hub`); `None` (the default, and tests): it probes none and
    /// reports none.
    pub agent_probe: Option<AgentProbe>,
    /// When the hub asks the remote boxes for their agent CLIs, and probes its own again
    /// ([`AgentsPoll::default`]; tests shorten it).
    pub agents_poll: AgentsPoll,
}

/// [`HubOptions::marker_read_hook`].
#[doc(hidden)]
pub type MarkerReadHook = Arc<dyn Fn(u32) + Send + Sync>;

/// [`HubOptions::box_files_read_hook`].
#[doc(hidden)]
pub type BoxFilesReadHook =
    Arc<dyn Fn(&str) -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

/// [`HubOptions::socket_removed_hook`].
#[doc(hidden)]
pub type SocketRemovedHook = Arc<dyn Fn() -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

/// [`HubOptions::reap_job_group_hook`]: given the group's id and its leader's start time.
#[doc(hidden)]
pub type ReapJobGroupHook = Arc<dyn Fn(u32, u64) -> twr_runner::reap::Reaped + Send + Sync>;

impl HubOptions {
    /// Defaults: built-in adapters, capacity 1024, [`STOP_TIMEOUT`], no socket, the current
    /// executable as `twr`.
    pub fn new(db_path: impl Into<PathBuf>, config: Config) -> Self {
        HubOptions {
            db_path: db_path.into(),
            config,
            instance_name: "default".into(),
            workspace: None,
            workspaces_root: None,
            worktree_namespace: None,
            broadcast_capacity: 1024,
            stop_timeout: STOP_TIMEOUT,
            adapters: builtin_adapters(),
            socket: None,
            twr_exe: std::env::current_exe().ok(),
            job_gate_exe: std::env::current_exe().ok(),
            suggest_bin: None,
            suggest_codex_bin: None,
            suggest_codex_home: None,
            claude_policy: twr_adapters::claude_code::guard::PolicySources::system(),
            stall_after: None,
            wake_after: None,
            git_tick: None,
            trash_tick: None,
            disk_tick: None,
            checkpoint_tick: None,
            checkpoint_events: None,
            prune_tick: None,
            job_slots_dir: None,
            boxes_lock: None,
            config_file: None,
            test_user_token: None,
            hello_timeout: runners::HELLO_TIMEOUT,
            runner_idle_timeout: runners::IDLE_TIMEOUT,
            ack_batch_delay: runners::ACK_BATCH_DELAY,
            git_bin: None,
            handoff_reply_timeout: handoff::HANDOFF_REPLY_TIMEOUT,
            change_request_timeout: twr_core::git::CHANGE_REQUEST_TIMEOUT,
            box_switch: crate::ssh_box::SwitchTiming::default(),
            systemd_tools: None,
            start_time_faults: 0,
            orchestrator_token_unverified: false,
            marker_read_hook: None,
            peer_confirm_hook: None,
            job_gate_hook: None,
            reap_job_group_hook: None,
            job_members_hook: None,
            box_dialer: None,
            box_files_read_hook: None,
            socket_removed_hook: None,
            agent_probe: None,
            agents_poll: AgentsPoll::default(),
        }
    }
}

/// Who is calling the hub. Assigned by the server from the connection's
/// `hello`, never claimed per request: a connection that presented the current orchestrator
/// token is the orchestrator, every other one is the user through its client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Caller {
    /// A user client (`source = user:<client>`, turns `from: user`).
    User(String),
    /// The orchestrator's MCP server (`source = orchestrator`, turns `from: orchestrator`).
    Orchestrator,
}

impl Caller {
    /// A user client.
    pub fn user(client: impl Into<String>) -> Caller {
        Caller::User(client.into())
    }

    /// The event source for what this caller does.
    pub fn source(&self) -> Source {
        match self {
            Caller::User(c) => Source::User(c.clone()),
            Caller::Orchestrator => Source::Orchestrator,
        }
    }

    /// `from` of the turns this caller starts.
    pub fn from(&self) -> TurnFrom {
        match self {
            Caller::User(_) => TurnFrom::User,
            Caller::Orchestrator => TurnFrom::Orchestrator,
        }
    }

    /// Whether this is the orchestrator.
    pub fn is_orchestrator(&self) -> bool {
        matches!(self, Caller::Orchestrator)
    }
}

/// Parameters of `thread.new`.
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct NewThread {
    /// A configured project name, an absolute path (project name = its basename), or the
    /// folder name of a git repository under `project_roots` (`hub::find_project`; not on a
    /// remote box), then taken as its absolute path.
    pub project: String,
    /// Runtime; defaults to the project's, then `default_runtime`.
    pub runtime: Option<String>,
    /// Unique name; generated when absent.
    pub name: Option<String>,
    /// Permission mode; defaults to `threads.default_mode`. The orchestrator may only omit it
    /// or give that default (`bad_request` otherwise).
    pub mode: Option<String>,
    /// Model; defaults to `threads.model` (unset: the runtime's default). Recorded as
    /// `thread.created` `ext.twr.model`.
    pub model: Option<String>,
    /// Reasoning effort, one the runtime takes (`runtime_spec::canonical_effort`); unset,
    /// blank or `default`: the runtime's default. Recorded as `thread.created`
    /// `ext.twr.effort`.
    pub effort: Option<String>,
    /// Extra names.
    pub aliases: Vec<String>,
    /// Box; defaults to the project's `box`, then `local`.
    #[serde(rename = "box")]
    pub box_name: Option<String>,
    /// Run in the repository itself instead of a worktree of its own.
    pub no_worktree: bool,
    /// Give the thread a worktree of its own even when the project's `worktree` (else
    /// `threads.worktree`) is `false`. Not together with `no_worktree`.
    pub worktree: bool,
    /// Start point of the thread's branch (default: the repository's `HEAD`); only for a new
    /// worktree.
    pub base: Option<String>,
    /// Internal atomic creation bookkeeping, never accepted by `thread.new`.
    #[serde(skip)]
    pub spawn_operation: Option<String>,
    #[serde(skip)]
    pub first_prompt: Option<String>,
}

/// Which revision of an item an answer was given for (see [`AttentionItem::revision`]):
/// checked under the lock that records the answer, so an item raised again (asking something
/// else) is never answered by what was said about an earlier revision.
///
/// [`AttentionItem::revision`]: twr_core::reduce::AttentionItem::revision
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnswerFor {
    /// The revision the client displayed (`attention.respond` `revision`): only while it is
    /// still the item's current one.
    Revision(u64),
    /// No revision given (a bare attn_id): only an item never raised again since it opened.
    /// The orchestrator is held to the relay guard instead (the user's message must postdate
    /// the current revision, `hub::orch`).
    Unspecified,
    /// A `say` reply: the item's current revision was raised at or before control position
    /// `seen` (what the user had on screen, `hub::route`).
    SeenBy(u64),
}

/// How `attention.respond` answers.
enum Response {
    /// A full answer (decision label, message, extras).
    Answer(Answer),
    /// The item's option at `index` (0-based).
    Option {
        index: usize,
        message: Option<String>,
    },
    /// `attention.dismiss`: the user closes a hub-raised item without answering it (recorded
    /// as the answer [`route::CANCEL`]; a hub question's message goes nowhere).
    Dismiss,
}

/// Result of `thread.send`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SendOutcome {
    /// Thread id.
    pub thread: String,
    /// Thread name (the orchestrator's tools lead their result with it).
    pub thread_name: String,
    /// Queue row id.
    pub queued: i64,
    /// The message's ref (the queue row id, as `queued`): `message.withdraw` takes it back by
    /// it, and the turn it starts records it (`ext.twr.ref`).
    #[serde(rename = "ref")]
    pub msg_ref: i64,
    /// Turn number, when the prompt was dispatched right away.
    pub turn: Option<u64>,
    /// Prompts still waiting for this thread.
    pub pending: u64,
    /// Set when the prompt was queued (it stays queued and is dispatched later) but
    /// dispatching failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dispatch_error: Option<String>,
    /// The thread is parked: the prompt waits in the queue until `thread.resume`.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub parked: bool,
    /// Advisory: no runtime of the thread runs, so this prompt starts (or resumes) one, now
    /// or once it is dispatched, and the thread's box reports its agent's CLI missing
    /// (`hub::box_agents`). The prompt is queued either way.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// `ext.twr.reason` of the `thread.stopped` the hub records for a runtime its own shutdown
/// stopped (not the user).
pub const HUB_SHUTDOWN: &str = "hub_shutdown";

/// Shutdown progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HubState {
    /// Serving.
    Running,
    /// Shutdown requested (signal, `hub.shutdown`, or a fatal store failure, see
    /// [`Hub::fatal_error`]); the server should call [`Hub::shutdown`].
    ShutdownRequested,
    /// Adapters stopped; subscriptions end.
    Stopped,
}

/// The hub. Cheap to clone.
#[derive(Clone)]
pub struct Hub {
    inner: Arc<Inner>,
}

struct Inner {
    core: Mutex<Core>,
    reader: Mutex<Connection>,
    /// The control items as the compacted backlog's selection reads them
    /// ([`subscribe::CompactRows`]). Taken by a compacted subscription's selection only,
    /// never with the core lock held.
    compact_rows: Arc<Mutex<subscribe::CompactRows>>,
    control_tx: broadcast::Sender<ControlItem>,
    events_tx: broadcast::Sender<Arc<Event>>,
    live_count: watch::Sender<usize>,
    state: watch::Sender<HubState>,
    /// The boxes: the hub's own and the remote ones (`[boxes.<name>]`), reached only through
    /// the accessors of `hub::boxes` (`Inner::host`, `Inner::remote`, ...), which never hold
    /// it across an `.await` or while taking the core lock.
    boxes: std::sync::RwLock<boxes::Boxes>,
    /// The configuration the hub started with. Its `boxes` are those of the start: a box added
    /// since is only in [`Inner::boxes`] (`Inner::box_config`).
    config: Config,
    /// [`HubOptions::config_file`]: where `box.add` reads boxes from.
    config_file: Option<PathBuf>,
    /// [`HubOptions::change_request_timeout`], for the remote boxes added later.
    change_request_timeout: Duration,
    /// Serializes `box.add`, `box.install`, `box.projects` and `box.token`: each reads the
    /// files, compares and applies, so a slower one's older read can't be applied after a newer
    /// one's (`hub::boxes`). Held across `.await`s and taken before any other lock (the boxes
    /// lock, the core lock, a box's state lock), never while one of them is held.
    box_files_lock: tokio::sync::Mutex<()>,
    /// [`HubOptions::box_files_read_hook`].
    box_files_read_hook: Option<BoxFilesReadHook>,
    /// [`HubOptions::socket_removed_hook`].
    socket_removed_hook: Option<SocketRemovedHook>,
    /// [`HubOptions::box_switch`].
    box_switch: crate::ssh_box::SwitchTiming,
    /// [`HubOptions::box_dialer`].
    box_dialer: Option<crate::ssh_box::BoxDialer>,
    /// The hub's own box's agent CLIs ([`HubOptions::agent_probe`], `hub::box_agents`).
    local_agents: Arc<box_agents::LocalAgents>,
    /// [`HubOptions::agents_poll`].
    agents_poll: AgentsPoll,
    /// [`HubOptions::worktree_namespace`]: the branch namespace of the hub's own box, also the
    /// SSH boxes' ([`crate::remote`]).
    worktree_namespace: Option<String>,
    /// The runner listener serves this hub ([`crate::listener::RunnerListener::serve`]): a
    /// token box can connect.
    listening: std::sync::atomic::AtomicBool,
    /// The hub's box key (`<data dir>/box-key`, [`crate::ssh_box`]), once an SSH box needed
    /// it.
    box_key: tokio::sync::OnceCell<Arc<twr_core::sealed::HubKey>>,
    db_path: PathBuf,
    instance_name: String,
    /// [`HubOptions::workspace`].
    workspace: Option<String>,
    /// [`HubOptions::workspaces_root`].
    workspaces_root: Option<PathBuf>,
    stop_timeout: Duration,
    /// How long a runner connection has to authenticate ([`HubOptions::hello_timeout`]).
    hello_timeout: Duration,
    /// [`HubOptions::runner_idle_timeout`].
    runner_idle_timeout: Duration,
    /// [`HubOptions::ack_batch_delay`].
    ack_batch_delay: Duration,
    started_at: DateTime<Utc>,
    /// What is wrong with the configuration but does not stop the hub (`hub.info`'s
    /// `warnings`; logged): [`Config::warnings`] at start, and the warnings of the boxes added
    /// since (`Inner::warn`).
    warnings: Mutex<Vec<String>>,
    /// Why the hub failed ([`fail_hub`]), once it has.
    ///
    /// [`fail_hub`]: supervisor::fail_hub
    fatal: Mutex<Option<String>>,
    /// Decision ladder (code, then the orchestrator).
    ladder: Ladder,
    /// The socket the hub serves, for the orchestrator's MCP server.
    socket: Option<PathBuf>,
    /// The `twr` binary for the orchestrator's MCP server.
    twr_exe: Option<PathBuf>,
    /// [`HubOptions::job_gate_exe`].
    job_gate_exe: Option<PathBuf>,
    /// [`HubOptions::suggest_bin`], resolved.
    suggest_bin: PathBuf,
    /// [`HubOptions::suggest_codex_bin`], resolved.
    suggest_codex_bin: PathBuf,
    /// [`HubOptions::suggest_codex_home`] (`None`: read from the environment when needed).
    suggest_codex_home: Option<PathBuf>,
    /// Next-message suggestions asked and answered (`hub::suggest`).
    suggestions: Mutex<suggest::Suggestions>,
    /// Thread summaries asked and answered (`hub::summary`).
    summaries: Mutex<summary::Summaries>,
    /// [`HubOptions::claude_policy`].
    claude_policy: twr_adapters::claude_code::guard::PolicySources,
    /// Wakes the notifier task ([`orch::notifier`]).
    notify_tx: mpsc::UnboundedSender<()>,
    /// [`HubOptions::handoff_reply_timeout`].
    handoff_reply_timeout: Duration,
    /// Stall threshold ([`HubOptions::stall_after`]); zero = off.
    stall_after: Duration,
    /// `git.snapshot` tick ([`HubOptions::git_tick`]); zero = off.
    git_tick: Duration,
    /// The trash purge's period ([`HubOptions::trash_tick`]); zero = off.
    trash_tick: Duration,
    /// The disk round's period ([`HubOptions::disk_tick`]); zero = off.
    disk_tick: Duration,
    /// [`HubOptions::checkpoint_tick`]; zero = off.
    checkpoint_tick: Duration,
    /// [`HubOptions::checkpoint_events`].
    checkpoint_events: u64,
    /// [`HubOptions::prune_tick`]; zero = off.
    prune_tick: Duration,
    /// Held while a checkpoint is written or the log is compacted (`hub::checkpoint`,
    /// `hub::prune`): a `VACUUM` renumbers the rows a checkpoint's replay looks up.
    maintenance: Mutex<()>,
    /// A reader of `archive.db` (`hub::prune`), once there is one.
    archive_reader: Mutex<Option<Connection>>,
    /// The machine's job slots ([`HubOptions::job_slots_dir`], `hub::jobs`).
    job_slots_dir: PathBuf,
    /// [`HubOptions::test_user_token`].
    test_user_token: Option<twr_core::runner::Secret>,
    /// Until when each TUI connection said the user looks at it (`notify.looking`,
    /// `hub::hook`), by connection.
    looking_until: std::sync::Mutex<HashMap<u64, std::time::Instant>>,
    /// The notification hook's task (`hub::hook`), awaited (bounded) at shutdown.
    hook_task: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Tells the notification hook's task to stop (a shutdown), ending a command it runs.
    hook_stop: watch::Sender<bool>,
    /// What a local worker's runtime starts under (`[threads] cpu_weight`, `memory_high`).
    limits: limits::Limits,
    /// Serializes snapshots, so an older `git status` is never recorded after a newer one.
    snapshots: tokio::sync::Mutex<()>,
    /// The repositories changes that remove something of one hold from their check through
    /// their removal: deletions, and parks, resumes and new threads that remove or create a
    /// worktree ([`delete::Reservations`]).
    reservations: Arc<delete::Reservations>,
    /// Bounds how many queued `thread.spawn` operations create threads at once
    /// ([`Hub::begin_spawn`]).
    spawn_slots: Arc<tokio::sync::Semaphore>,
    /// Start time of the hub's own process (`twr_runner::reap::own_start_time`): every local
    /// runtime it starts starts later (its spawn intents, `crate::store`). `None` where it
    /// cannot be read (said loudly at start; then no intent is recorded).
    start_ticks: Option<u64>,
    /// [`HubOptions::orchestrator_token_unverified`].
    orchestrator_token_unverified: bool,
    /// [`HubOptions::marker_read_hook`].
    marker_read_hook: Option<MarkerReadHook>,
    /// [`HubOptions::peer_confirm_hook`].
    peer_confirm_hook: Option<MarkerReadHook>,
    /// [`HubOptions::job_gate_hook`].
    job_gate_hook: Option<MarkerReadHook>,
    /// [`HubOptions::reap_job_group_hook`].
    reap_job_group_hook: Option<ReapJobGroupHook>,
    /// [`HubOptions::job_members_hook`].
    job_members_hook: Option<MarkerReadHook>,
    /// Exclusive lock on `<data_dir>/twr.lock`, released when the hub is dropped.
    _lock: File,
}

impl Inner {
    fn core(&self) -> MutexGuard<'_, Core> {
        self.core.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn reader(&self) -> MutexGuard<'_, Connection> {
        self.reader.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The reader of `archive.db`, opened the first time there is one (`None` until then).
    fn archive_reader(&self) -> MutexGuard<'_, Option<Connection>> {
        let mut a = self
            .archive_reader
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if a.is_none() {
            *a = crate::archive::open_reader(&crate::archive::path_of(&self.db_path));
        }
        a
    }

    fn fatal(&self) -> Option<String> {
        self.fatal
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// `TWR_CHECKPOINT_CHECK=1` (the crate's tests): each hub's log is checked against checkpoints
/// as the hub goes away (`hub::checkpoint`).
#[cfg(test)]
impl Drop for Inner {
    fn drop(&mut self) {
        checkpoint::tests::check_on_drop(&self.db_path, &self.config.notify);
    }
}

/// Per-thread runtime bookkeeping (not state: rebuilt from the log or from live processes).
/// What the log rebuilds goes into the hub's checkpoints (`hub::checkpoint`); the fields
/// skipped there are memory only, and the fold never sets them.
#[derive(Default, serde::Serialize, Deserialize)]
struct ThreadRt {
    /// The running adapter, if any.
    #[serde(skip)]
    live: Option<Live>,
    /// The log shows a runtime process that has not stopped, crashed or parked.
    process_live: bool,
    /// `thread.started` `ext.twr.pid` / `ext.twr.start_time` / `ext.twr.exe` of that process.
    pid: Option<u32>,
    start_time: Option<u64>,
    exe: Option<String>,
    /// Full text of the open turn's latest final `agent.text` (report fallback).
    final_text: Option<String>,
    /// The attach lease, from `attach.begin` until `thread.detached`.
    #[serde(skip)]
    attach: Option<AttachLease>,
    /// Where the thread works (`thread.created` `ext.twr`, `hub::git`).
    workspace: git::Workspace,
    /// A `thread.park` in progress (from the request until its worktree is dealt with).
    #[serde(skip)]
    park: Option<git::ParkLease>,
    /// A `thread.resume` in progress (recreating the worktree).
    resuming: bool,
    /// The argv of a `pty` thread (`thread.created` `ext.twr.pty_command`), as the JSON
    /// array text its runtime gets in `TWR_PTY_COMMAND`.
    pty_command: Option<String>,
    /// An `acp` thread's `TWR_ACP_COMMAND` / `TWR_ACP_ATTACH` (`ext.twr.acp_command` /
    /// `acp_attach`, see `hub::runtime_spec`).
    acp_env: Vec<(String, String)>,
    /// A runtime exit whose closing sequence is not recorded yet ([`Core::begin_exit`] until
    /// [`Core::finish_exit`]): the thread takes no new turn, and nothing else closes it.
    exiting: bool,
    /// The runtime that exit took, when a runner connection brought it
    /// ([`Core::begin_exit_on`]): kept until [`Core::finish_exit`], to be put back as it was
    /// when `box.token` changes the token that connection proved ([`TakenExit`]).
    #[serde(skip)]
    taken: Option<TakenExit>,
    /// A `thread.stop` asked the runtime to stop and its closing sequence is not recorded yet
    /// (mirrors the durable `stop_requests` row, see [`crate::store`]): its exit, whenever and
    /// however it comes (after a hub restart, through a runner that lost the process), closes
    /// the thread as stopped.
    stop_requested: bool,
    /// Runtimes of the thread whose life the log has closed (`thread.stopped` / `crashed` /
    /// `parked` / `attached`): the live runtime is generation `closed_runtimes + 1`, the
    /// middle part of its attention ids ([`namespace_attention`]).
    ///
    /// [`namespace_attention`]: supervisor::namespace_attention
    closed_runtimes: u64,
    /// The thread's runtime failed to start, or a remote runtime's recorded
    /// `send` / `respond` failed or expired (`hub::runners`), or a worker's runtime crashed
    /// (`thread.crashed`, folded from the log, so a hub restart keeps that one; the others a
    /// hub restart forgets): until the user's own `thread.send` / `say` / `thread.resume` (not
    /// the orchestrator's, nor a job's wake: [`Batch::user_send`]), or a runtime of the thread
    /// starts (a due automatic retry's, for one), only a due automatic retry is dispatched
    /// ([`Core::dispatch_next`]); queued prompts wait. Every path that dispatches
    /// after a failure (a detach, a runner reconnecting, the background ticker, ...) goes
    /// through that check.
    queue_held: bool,
    /// The control position of the `thread.crashed` that set [`ThreadRt::queue_held`] (None
    /// when nothing did, or another failure holds it). The user's send releases the hold only
    /// in memory, but its queue row keeps the control head it was queued at
    /// (`pending_prompts.user_sent`): a hub that opens finds a prompt the user sent after the
    /// crash and releases the hold again ([`Core::release_crash_holds`]).
    crash_hold: Option<u64>,
    /// Kills of what is left of a runtime of the thread in progress ([`reap_runtime`]): until
    /// they are done the thread takes no new runtime (one started now could be hit by them)
    /// and is neither attached nor parked ([`ThreadRt::giving_up`]).
    ///
    /// [`reap_runtime`]: supervisor::reap_runtime
    reaping: u32,
    /// A dispatch ([`Core::dispatch_next`]) or an automatic retry was held back by
    /// [`ThreadRt::reaping`]: the end of the kill runs it ([`reap_runtime`]).
    ///
    /// [`reap_runtime`]: supervisor::reap_runtime
    #[serde(skip)]
    after_reap: AfterReap,
    /// The permission mode, model and reasoning effort the thread's runtime process was
    /// started with: set when the hub starts it, and from the log (`thread.started` /
    /// `thread.resumed` `ext.twr.mode` / `model` / `effort`, else the thread's settings then),
    /// so a remote runtime taken over after a restart has them too; `None` once the log closed
    /// the process. A live runtime whose settings differ from the thread's
    /// (`thread.mode_changed`, `thread.model_changed`) is restarted once it is idle
    /// (`hub::restart`).
    runtime_settings: Option<restart::RunSettings>,
    /// The settings the thread's session last ran in under a headless runtime: the
    /// [`ThreadRt::runtime_settings`] of its latest runtime process, kept once that process is
    /// gone (rebuilt from the log at startup). An attach whose command cannot apply the
    /// thread's model or effort is refused while these differ (`hub::attach`).
    session_settings: Option<restart::RunSettings>,
    /// The runtime is being stopped to restart in the thread's new settings (`hub::restart`):
    /// its exit is recorded as a `thread.stopped` with this `ext.twr.reason` (`mode_change` or
    /// `model_change`) and the runtime is started again on its session. Also a stop for a
    /// trim (`trim`, `hub::disk`) or for the switch of its box to another build
    /// (`box_upgrade`, `hub::box_switch`), after which the thread is not free until that is
    /// over. A `thread.stop` meanwhile clears it (the stop wins). Memory only: a hub restart
    /// starts every local runtime afresh in the thread's settings anyway.
    #[serde(skip)]
    restart_reason: Option<&'static str>,
    /// The runtime's own turns whose start the hub dropped (`hub::supervisor`): runtime turn
    /// id → the open turn that took them in. Each one's end adds its usage to that turn once.
    /// Memory only: a hub restart before that end loses the usage.
    absorbed: HashMap<String, u64>,
    /// Usage of absorbed runtime turns that ended, for the end of turn `.0`.
    folded_usage: Option<(u64, Usage)>,
    /// The user confirmed deleting the thread and the deletion runs or waits for the threads
    /// before it on its card (`hub::delete`): it takes no turn, prompt, settings change,
    /// resume, attach or park meanwhile, and no attach starts inside its worktree. Memory
    /// only: a deletion a hub restart cut short leaves the thread, to be deleted again.
    deleting: bool,
    /// Its worktree's build output is being deleted (`hub::disk`, `thread.trim`): it takes no
    /// turn, and is neither attached, parked, resumed nor deleted meanwhile. Memory only.
    trimming: bool,
    /// The turn whose message the user took back that its runtime showed it still runs, and
    /// the signs it showed (`hub::withdraw`): each kind of sign interrupts it again once.
    /// Memory only.
    reinterrupted: Option<(u64, u8)>,
    /// A compaction of its context the user asked for that has not started (`hub::compact`):
    /// the next turn once the messages queued before it are done. Memory only.
    #[serde(skip)]
    compact: Option<compact::CompactAsk>,
}

/// Equal in every field (tests: a fold from a checkpoint against a fold of the whole log,
/// `hub::checkpoint`); a live runtime, lease or exit, and a compaction asked for, by whether
/// there is one.
#[cfg(test)]
impl PartialEq for ThreadRt {
    fn eq(&self, o: &Self) -> bool {
        let ThreadRt {
            live,
            process_live,
            pid,
            start_time,
            exe,
            final_text,
            attach,
            workspace,
            park,
            resuming,
            pty_command,
            acp_env,
            exiting,
            taken,
            stop_requested,
            closed_runtimes,
            queue_held,
            crash_hold,
            reaping,
            after_reap,
            runtime_settings,
            session_settings,
            restart_reason,
            absorbed,
            folded_usage,
            deleting,
            trimming,
            reinterrupted,
            compact,
        } = self;
        live.is_some() == o.live.is_some()
            && *process_live == o.process_live
            && *pid == o.pid
            && *start_time == o.start_time
            && *exe == o.exe
            && *final_text == o.final_text
            && attach.is_some() == o.attach.is_some()
            && *workspace == o.workspace
            && park.is_some() == o.park.is_some()
            && *resuming == o.resuming
            && *pty_command == o.pty_command
            && *acp_env == o.acp_env
            && *exiting == o.exiting
            && taken.is_some() == o.taken.is_some()
            && *stop_requested == o.stop_requested
            && *closed_runtimes == o.closed_runtimes
            && *queue_held == o.queue_held
            && *crash_hold == o.crash_hold
            && *reaping == o.reaping
            && *after_reap == o.after_reap
            && *runtime_settings == o.runtime_settings
            && *session_settings == o.session_settings
            && *restart_reason == o.restart_reason
            && *absorbed == o.absorbed
            && *folded_usage == o.folded_usage
            && *deleting == o.deleting
            && *trimming == o.trimming
            && *reinterrupted == o.reinterrupted
            && compact.is_some() == o.compact.is_some()
    }
}

/// A runtime an exit a runner connection brought took ([`ThreadRt::taken`]), and that
/// connection's fence. A token change of the box (`box.token`) puts it back, live and not
/// exiting, in the step under the core lock that changes the token
/// ([`Core::put_back_revoked_exits`]): from then on the box's next connection finds the runtime
/// as it was (its adoption, replay and reconciliation), and the exit records nothing
/// (`supervisor::close_exited`).
struct TakenExit {
    live: Box<Live>,
    fence: runners::Fence,
}

/// What a dispatch held back by a runtime kill in progress ([`ThreadRt::reaping`]) runs once
/// the kill is done.
#[derive(Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum AfterReap {
    #[default]
    Nothing,
    /// A due automatic retry only (after a crash).
    Retry,
    /// A full dispatch ([`Core::try_dispatch`]): the retry, else the queue.
    Dispatch,
}

impl ThreadRt {
    /// The fields checkpoints hold (tests: what it serializes to, `hub::checkpoint`).
    #[cfg(test)]
    const KEPT: &'static [&'static str] = &[
        "process_live",
        "pid",
        "start_time",
        "exe",
        "final_text",
        "workspace",
        "resuming",
        "pty_command",
        "acp_env",
        "exiting",
        "stop_requested",
        "closed_runtimes",
        "queue_held",
        "crash_hold",
        "reaping",
        "runtime_settings",
        "session_settings",
        "absorbed",
        "folded_usage",
        "deleting",
        "trimming",
        "reinterrupted",
    ];

    /// The fields checkpoints skip (`#[serde(skip)]`) that are set (tests: a fold must set
    /// none of them, `hub::checkpoint`). Names every field: a new one is placed here, and in
    /// [`ThreadRt::KEPT`] when checkpoints hold it.
    #[cfg(test)]
    fn skipped_set(&self) -> Vec<&'static str> {
        let ThreadRt {
            live,
            attach,
            park,
            taken,
            after_reap,
            restart_reason,
            compact,
            // In the checkpoint.
            process_live: _,
            pid: _,
            start_time: _,
            exe: _,
            final_text: _,
            workspace: _,
            resuming: _,
            pty_command: _,
            acp_env: _,
            exiting: _,
            stop_requested: _,
            closed_runtimes: _,
            queue_held: _,
            crash_hold: _,
            reaping: _,
            runtime_settings: _,
            session_settings: _,
            absorbed: _,
            folded_usage: _,
            deleting: _,
            trimming: _,
            reinterrupted: _,
        } = self;
        [
            ("live", live.is_some()),
            ("attach", attach.is_some()),
            ("park", park.is_some()),
            ("taken", taken.is_some()),
            ("after_reap", *after_reap != AfterReap::Nothing),
            ("restart_reason", restart_reason.is_some()),
            ("compact", compact.is_some()),
        ]
        .into_iter()
        .filter_map(|(f, set)| set.then_some(f))
        .collect()
    }

    /// The live runtime is one the hub gave up on ([`give_up_runtime`]) and has not exited yet,
    /// or what may be left of a runtime of the thread is being killed ([`reap_runtime`]).
    ///
    /// [`give_up_runtime`]: supervisor::give_up_runtime
    /// [`reap_runtime`]: supervisor::reap_runtime
    fn giving_up(&self) -> bool {
        self.live.as_ref().is_some_and(|l| l.given_up.is_some()) || self.reaping > 0
    }

    /// Its runtime is being stopped or restarted already, for whatever reason: a stop was asked
    /// for (a `thread.stop`, a settings restart, a box switch), its exit is being recorded, or
    /// the hub gave up on it.
    fn stopping(&self) -> bool {
        self.exiting
            || self.stop_requested
            || self.restart_reason.is_some()
            || self.giving_up()
            || self.live.as_ref().is_some_and(|l| l.stop_requested)
    }
}

/// An `attach.begin` in progress or granted.
struct AttachLease {
    /// Hub-assigned id; a lease connection that closes only ends its own lease.
    lease: u64,
    /// Client that attached (`thread.attached` / `thread.detached` `client`).
    client: String,
    /// Woken once `thread.attached` is appended.
    waiter: Option<oneshot::Sender<()>>,
    /// `thread.attached` is appended.
    attached: bool,
    /// The directory the interactive runtime works in, on the thread's box: a remote box
    /// cannot see it (the command runs over ssh, not through its runner), so the hub keeps
    /// that box's worktree removals out of it itself ([`Core::attach_inside`]).
    dir: Option<PathBuf>,
    /// Its use of that directory on a local box ([`ThreadHost::use_workdir`]): no worktree
    /// change of the box removes it while the lease lasts (dropped with the lease).
    _workdir: Option<twr_runner::workdirs::WorkdirUse>,
    /// The interactive process, once its client reported it (`attach.child`) or a restarted
    /// hub found it still running: while it runs, the attach does not end with its lease
    /// connection.
    child: Option<store::AttachProcess>,
    /// The lease connection is gone (or the hub restarted) while the process ran: the hub
    /// watches the process and detaches once it has exited ([`watch_attach_process`]).
    ///
    /// [`watch_attach_process`]: attach::watch_attach_process
    orphaned: bool,
}

struct Live {
    handle: AdapterHandle,
    generation: u64,
    stop_requested: bool,
    /// Stopped for `attach.begin`: its exit appends `thread.attached`, not `thread.stopped`.
    attach: bool,
    /// Stopped for `thread.park`: its exit appends `thread.parked`, not `thread.stopped`.
    park: bool,
    /// The orchestrator's MCP config file (it holds the token), removed with this runtime.
    _mcp_file: Option<orch::PrivateFile>,
    /// Its use of its working directory on a local box ([`ThreadHost::use_workdir`]): no
    /// worktree change removes it underneath while the runtime lives.
    _workdir: Option<twr_runner::workdirs::WorkdirUse>,
    /// A remote session's run on its box (`hub::runners`): named by the hub when it started the
    /// session, else once the inventory or a frame named it. An `exited` frame of another run (or
    /// of none) is not this session's.
    run: Option<String>,
    /// The runner confirmed `run` (a frame of it, or the inventory, named it). Until then an exit
    /// naming no run (a restarted runner's, for a run its predecessor left behind) may still
    /// be this session's: the run the hub named may be the one that was left behind.
    run_confirmed: bool,
    /// The hub could not record what this runtime did and stopped it ([`give_up_runtime`]):
    /// the crash message its exit records. Until that exit is observed the runtime keeps its
    /// slot (the thread takes no turn, attach and park wait) and its events are dropped.
    ///
    /// [`give_up_runtime`]: supervisor::give_up_runtime
    given_up: Option<String>,
    /// The runtime of the log this is ([`Core::runtime_gen`] when it started or was taken
    /// over): once the log has closed that runtime, an exit of this one closes nothing more
    /// (its closing sequence is recorded already).
    log_runtime: u64,
}

#[derive(Clone)]
enum Exit {
    Stopped,
    Crashed {
        exit_code: Option<i32>,
        stderr_tail: String,
    },
    /// Quiesced for `attach.begin` by `client`.
    Attach {
        client: String,
    },
    /// Quiesced for `thread.park` (`thread.parked` by `source`, `ext.twr.keep`).
    Park {
        source: Source,
        keep: bool,
    },
}

/// One event of a precomputed append sequence (see [`Core::closing_plan`]).
struct Planned {
    turn: Option<u64>,
    source: Source,
    body: EventBody,
}

/// One event of a [`Batch`].
struct BatchEvent {
    thread: String,
    turn: Option<u64>,
    source: Source,
    body: EventBody,
    raw: Option<String>,
}

/// The routing decision a prompt queued by a [`Batch`] records.
enum RoutedBy {
    /// An event already in the log.
    Id(String),
    /// The batch's event at this index.
    Event(usize),
}

/// A prompt a [`Batch`] queues (`said_after`: the control head after the batch's events).
struct BatchPrompt {
    thread: String,
    prompt: String,
    from: TurnFrom,
    source: Source,
    routed_by: Option<RoutedBy>,
    /// Caps `said_after` (what the user had seen when saying it, `hub::route`).
    said_after: Option<u64>,
    /// The user's own send ([`Batch::user_send`]): it releases a held queue.
    user_send: bool,
}

/// Appends that make up one logical operation, recorded all or nothing by
/// [`Core::append_batch`].
#[derive(Default)]
struct Batch {
    events: Vec<BatchEvent>,
    prompts: Vec<BatchPrompt>,
    /// Pending prompt row removed with the events (a dispatched turn, or a message the user
    /// took back, `hub::withdraw`).
    dequeue: Option<i64>,
    /// Blobs committed with the events (besides those the payload caps stage).
    staged: StagedBlobs,
    /// The runner frame these events come from: its watermark is raised in the same
    /// transaction.
    runner_ack: Option<store::RunnerAck>,
    /// Commands for remote boxes these events imply (`remote_outbox`, `hub::runners`).
    remote_cmds: Vec<store::RemoteCmd>,
    /// Recorded remote commands (`(box, cmd_id)`) these events retire (a failed or expired
    /// command with the `turn.ended` and the stop its failure means, `hub::runners`).
    remote_done: Vec<(String, String)>,
    /// Threads whose runtime the hub asks to stop (`stop_requests`, their queue kept): a
    /// remote command that failed or expired (`hub::runners`).
    stop_requests: Vec<String>,
    /// Threads whose queued prompts are dropped with the events (`thread.stop` of a thread
    /// with no runtime to wait for).
    drop_queues: Vec<String>,
    /// When the events happened, if earlier than now: a runner frame's capture time
    /// (`hub::runners`), so a frame replayed from its spool keeps its time. Each event's `ts`
    /// is this time kept within `[the thread's previous event's ts, now]`
    /// ([`Core::event_time`]).
    at: Option<DateTime<Utc>>,
}

impl Batch {
    /// Adds an event (no raw message); returns its index in the batch.
    fn push(&mut self, thread: &str, turn: Option<u64>, source: Source, body: EventBody) -> usize {
        self.events.push(BatchEvent {
            thread: thread.to_string(),
            turn,
            source,
            body,
            raw: None,
        });
        self.events.len() - 1
    }

    /// Marks the prompt just queued as the user's own send (`thread.send`, `say`, the user's
    /// answer that routes a message): it releases a queue a failure held
    /// ([`ThreadRt::queue_held`]), live ([`Core::dispatch_queued`]) and at the next start
    /// (`pending_prompts.user_sent`). Nothing else that queues a prompt does: a job's wake,
    /// the orchestrator's send, a prompt that waited for other threads.
    fn user_send(&mut self) {
        if let Some(p) = self.prompts.last_mut() {
            p.user_send = true;
        }
    }

    fn is_empty(&self) -> bool {
        self.events.is_empty()
            && self.prompts.is_empty()
            && self.dequeue.is_none()
            && self.runner_ack.is_none()
            && self.remote_cmds.is_empty()
            && self.remote_done.is_empty()
            && self.stop_requests.is_empty()
    }
}

/// What [`Core::append_batch`] recorded.
struct Committed {
    /// The stored events (payloads capped), in batch order.
    events: Vec<Event>,
    /// Queue row ids of the batch's prompts, in order.
    queued: Vec<i64>,
    /// Queued prompts dropped ([`Batch::drop_queues`]).
    dropped: usize,
}

/// Which of a thread's open adapter items a bulk resolution closes
/// ([`Core::open_adapter_attention`]).
#[derive(Clone, Copy, Debug)]
enum AdapterScope {
    /// Every one: the thread's runtime is gone (a closing sequence).
    All,
    /// The end of turn `turn` of runtime generation `gen`: items that runtime raised in that
    /// turn or an earlier one (a straggler the earlier turn's end missed). Items of a later
    /// turn, raised between turns (no turn) or of another generation stay open. An id without
    /// a generation (`<thread>:<adapter id>` from before generations, or not namespaced) is
    /// judged by its turn alone.
    Turn { turn: u64, gen: u64 },
}

impl AdapterScope {
    /// Whether item `attn_id` of `thread`, raised by an adapter in turn `raised_in`, is in
    /// the scope.
    fn covers(self, thread: &str, attn_id: &str, raised_in: Option<u64>) -> bool {
        match self {
            AdapterScope::All => true,
            AdapterScope::Turn { turn, gen } => {
                // No generation in the id (namespaced before generations, or recorded as it
                // came): its turn decides.
                let same_gen = match split_runtime_attn_id(thread, attn_id) {
                    Some((Some(g), _)) => g == gen,
                    _ => true,
                };
                same_gen && raised_in.is_some_and(|t| t <= turn)
            }
        }
    }
}

/// A turn an adapter just ended, with the turn's full final text: what
/// [`Core::react_to_adapter_turn_end`] needs.
struct EndedTurn {
    turn: u64,
    /// The generation of the runtime that ended it ([`Core::runtime_gen`] when it was
    /// recorded): the adapter items its end closes are that runtime's ([`AdapterScope`]).
    runtime_gen: u64,
    result: TurnResult,
    final_text: Option<String>,
}

/// An adapter exit whose closing sequence is still to be recorded (see [`Core::begin_exit`]).
/// The sequence itself is planned from the state at each attempt to record it
/// ([`Core::exit_plan`]), never from an earlier view.
struct PendingExit {
    /// How the runtime ended (a crash may still turn into a stop, [`Core::exit_plan`]).
    exit: Exit,
    /// The runtime of the log that exited ([`Live::log_runtime`]): once the log has closed it
    /// (another closing sequence went in first), this exit records nothing but its frame.
    log_runtime: u64,
    /// The client the runtime was quiesced for (`attach.begin`).
    attach_client: Option<String>,
    /// Stopped to restart in the thread's new settings (`hub::restart`): once closed, the
    /// queue dispatches and, if nothing started a runtime, it starts again on its session.
    restart: bool,
    /// The hub asked the runtime to stop: queued prompts dispatch after the exit.
    stopped: bool,
    /// Stopped for `thread.park`: nothing is dispatched after the exit.
    park: bool,
    /// The plan is a crash's (`thread.crashed` and its `error` item).
    crash: bool,
    /// The runner frame (`exited`) this exit came in, committed with the closing sequence
    /// while its connection still holds `fence` (`hub::runners`).
    runner_ack: Option<store::RunnerAck>,
    /// The runner connection the exit came from, by its frame or its reconciliation
    /// ([`Core::begin_exit_on`]): the frame's watermark commits only while it holds, and
    /// nothing commits once its token was revoked (the runtime the exit took is back then,
    /// [`ThreadRt::taken`], and the box's next connection reports the exit again: the runner
    /// sends its frame again, or the reconciliation finds the runtime lost again).
    fence: Option<runners::Fence>,
    /// When the runner recorded the exit ([`Batch::at`]).
    at: Option<DateTime<Utc>>,
}

/// The hub's state, under its lock. What the log rebuilds (the fold: [`Core::apply`] of every
/// event, live as it is recorded and on replay, from the event and this state alone) is also
/// what the hub's checkpoints hold (`hub::checkpoint`): a field the fold sets must go into
/// [`checkpoint::FoldSnapshot`], and every field that stays out is named memory only in
/// [`Core::into_snapshot`], which lists each one, so that a new field does not compile until it
/// is placed. Inside a folded field's type, what the checkpoint skips (`#[serde(skip)]`) is
/// memory only too: the fold never sets it. A hash set in one is serialized sorted
/// (`twr_core::sorted`; a test looks for any that is not). The checkpoint tests compare a fold
/// that starts from a checkpoint with a fold of the whole log.
struct Core {
    store: Store,
    registry: Registry,
    attention: AttentionStack,
    seqs: HashMap<String, u64>,
    /// Timestamp of each thread's latest event (a runner's replayed frame is never recorded
    /// before it, [`Core::event_time`]).
    last_ts: HashMap<String, DateTime<Utc>>,
    gseq: u64,
    /// Every attn_id ever raised (to tell "already resolved" from "unknown").
    known_attn: HashSet<String>,
    /// Open attention items raised by an adapter (answers are forwarded to it), with the
    /// turn of the event that raised it (last) ([`AdapterScope`]).
    adapter_attn: HashMap<String, Option<u64>>,
    rt: HashMap<String, ThreadRt>,
    generation: u64,
    /// Last attach lease id handed out.
    leases: u64,
    shutting_down: bool,
    /// Adapter events refused by [`rejection`] (oversized identifiers or payloads).
    dropped_events: u64,
    /// Adapter / runner events that carried capping metadata only the hub may write (`_capped`
    /// markers, `<field>_ref` siblings); it was stripped before recording
    /// ([`strip_body_capping_metadata`]).
    ///
    /// [`strip_body_capping_metadata`]: twr_core::blob::strip_body_capping_metadata
    sanitized_events: u64,
    /// Runtime task updates not recorded (`hub::coalesce`).
    coalesced_events: u64,
    /// The control items as the compacted backlog reads them, as the replay found them: taken
    /// into [`Inner::compact_rows`] when the hub opens.
    compact_rows: subscribe::CompactRows,
    /// When each runtime task's progress was last recorded (`hub::coalesce`).
    task_progress: coalesce::TaskProgress,
    /// Store writes retried after a failure ([`persist`]).
    ///
    /// [`persist`]: supervisor::persist
    persist_retries: u64,
    /// Orchestrator bookkeeping (rebuilt from the log; the token lives only here).
    orch: orch::OrchState,
    /// Hub questions (`attention.raised` on [`HUB_THREAD`] from `say`) still open, with the
    /// message each one routes once answered (`ext.twr.pending_message`).
    hub_questions: HashMap<String, route::PendingRoute>,
    /// Confirm cards (`attention.raised` on [`HUB_THREAD`] with `ext.twr.proposal`) still
    /// open, with what each applies once confirmed (`hub::proposal`).
    hub_cards: HashMap<String, proposal::Proposal>,
    /// Delete cards (`attention.raised` on [`HUB_THREAD`] with `ext.twr.delete`) still open,
    /// with the threads each one deletes once confirmed (`hub::delete`).
    delete_cards: HashMap<String, delete::DeleteCard>,
    /// What the hub knows of its boxes' disks: free space, worktree measurements, the open
    /// disk cards (`hub::disk`).
    disk: disk::DiskState,
    /// The hub's queued and running jobs (`hub::jobs`).
    jobs: jobs::JobState,
    /// What feeds the notification hook (`hub::hook`); `None` without `[notify] command`.
    hook: Option<hook::HookFeed>,
    /// Prompts waiting for other threads to finish (`hub::after`).
    after: after::AfterState,
    /// What the user did with each resolved confirm card ("the user confirmed it. Applied:
    /// ..."), so that answering it again reports the original outcome (`hub::proposal`).
    card_outcomes: HashMap<String, String>,
    /// Last `context.updated` hash per context file (`hub::context`).
    context_hashes: HashMap<String, String>,
    /// Last `context.updated` hash per project: remote threads' context.
    project_contexts: HashMap<String, String>,
    /// Context files read at dispatch, per version (`hub::context`).
    context_files: context::FileCache,
    /// The repository root of each configured local project, as last resolved (by
    /// `context.read` / `context.set`, or a thread's creation): where its context file is
    /// (`hub::context`).
    context_roots: HashMap<String, PathBuf>,
    /// Open hub-raised `error` items: attn_id -> (thread, turn) (`hub::retry`).
    hub_errors: HashMap<String, (String, Option<u64>)>,
    /// Open stalled item per thread: thread -> attn_id (`hub::stall`).
    stalled: HashMap<String, String>,
    /// Workers to wake after their background work ended (`hub::wake`).
    wake: wake::WakeState,
    /// Hung-turn cards (`hub::stall`).
    hung: stall::HungState,
    /// The stalls each thread went on from (`hub::stall`): what a summary's basis reads past.
    went_on: stall::WentOn,
    /// What each thread is doing now (`hub::activity`).
    activity: HashMap<String, activity::Activity>,
    /// Thread names being created (their worktree is being made, `hub::git`) or restored
    /// (`hub::trash`): taken.
    reserved_names: HashSet<String>,
    /// Trash items the log knows of (`thread.deleted` `trash`), by id, less those purged
    /// since the hub started (`hub::trash`).
    trash: BTreeMap<String, trash::Seen>,
    /// What deleted threads' runtime bookkeeping held, by thread, for a restore (`hub::trash`).
    deleted_rt: HashMap<String, trash::DeletedRt>,
    /// Trash items an operation of the hub works on (`hub::trash`): one at a time each. A
    /// deletion holds its new item from before it is made until `thread.deleted` is recorded
    /// (or the deletion failed); a restore from its start until `thread.restored` is recorded
    /// (or what it made is removed); a purge and a reconciliation while they work on it.
    trash_busy: HashSet<String>,
    /// Every trash item a `thread.deleted` named, and every one a `thread.restored` named,
    /// with the name and time of that restore (from the log): what the hub recorded of an
    /// item, which settles what its box shows (`hub::trash`, [`Hub::reconcile_trash`]); a
    /// restored item is listed as such, and not restored again.
    items_deleted: HashSet<String>,
    items_restored: HashMap<String, trash::Restored>,
    /// Remote boxes whose runner is connected, synced and reconciled (`hub::runners`):
    /// only their threads get new turns and box commands.
    online_boxes: HashSet<String>,
    /// SSH boxes being switched to the build the hub launched there (`hub::box_switch`): their
    /// threads take no turn and start no runtime until the box's next connection is
    /// reconciled. Memory only.
    switching_boxes: HashSet<String>,
    /// Boxes whose runner speaks an older runner protocol that the hub drains (`hub::drain`),
    /// with how far the drain is: their threads take no turn and start no runtime until a
    /// connection of the hub's protocol is reconciled. Memory only.
    draining_boxes: HashMap<String, drain::Drain>,
    /// Runner frames are being committed without a sync each: their connection syncs the
    /// batch before acking it (`hub::runners`, [`store::AppendBatch::defer_sync`]).
    defer_ack_sync: bool,
    /// The latest usage of each runtime's account, from `account.usage` (`hub::usage`).
    usage: twr_core::usage::UsageView,
    /// How the hub's start rebuilt this state (`hub::checkpoint`). Memory only.
    replayed: checkpoint::Replayed,
    /// What the orchestrator's runtime got of its memory (`hub::memory`). Memory only: a
    /// restarted hub starts the orchestrator's runtime again, which gets its memory then.
    memory_seen: Option<memory::Seen>,
}

fn rpc(code: &str, msg: impl Into<String>) -> RpcError {
    RpcError::new(code, msg)
}

fn internal(e: impl std::fmt::Display) -> RpcError {
    rpc(codes::INTERNAL, e.to_string())
}

/// An append failure on a client request: `bad_request` when the event itself was refused.
fn append_err(e: anyhow::Error) -> RpcError {
    if let Some(x) = e.downcast_ref::<RuntimeExiting>() {
        return rpc(codes::CONFLICT, x.to_string());
    }
    match rejection(&e) {
        Some(msg) => rpc(codes::BAD_REQUEST, msg),
        None => internal(format!("{e:#}")),
    }
}

fn resolve_err(e: ResolveError) -> RpcError {
    match e {
        ResolveError::NotFound(_) => rpc(codes::NOT_FOUND, e.to_string()),
        ResolveError::Ambiguous { .. } => rpc(codes::AMBIGUOUS, e.to_string()),
    }
}

/// Whether thread names (or ids, aliases) `a` and `b` are one name: compared without ASCII
/// case, as the user's commands read them (`twr_decisions::code::command`) and as a
/// case-insensitive file system compares branch names, so `API` never sits beside `api`.
fn same_name(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

fn twr_ext(map: &Option<JsonMap>) -> Option<&Map<String, Value>> {
    map.as_ref()?.get("twr")?.as_object()
}

fn twr_ext_map(v: Value) -> Option<JsonMap> {
    let mut m = JsonMap::new();
    m.insert("twr".into(), v);
    Some(m)
}

impl Core {
    /// `thread`'s runtime bookkeeping, created when it has none: only for a thread known to be
    /// there (checked under the same lock: its `thread.created` being recorded, its view found).
    /// Code that runs after an `.await` or from a background task or timer (a kill's end, an
    /// exit, a park or attach that waited) uses `self.rt.get_mut` instead: the thread may have
    /// been deleted meanwhile (`hub::delete`), and nothing may come back for it.
    fn rt(&mut self, thread: &str) -> &mut ThreadRt {
        self.rt.entry(thread.to_string()).or_default()
    }

    /// The generation of `thread`'s current (or next) runtime: one more than the runtimes the
    /// log has closed. Derived from the log, so a remote runtime taken over after a hub restart
    /// keeps its generation.
    fn runtime_gen(&self, thread: &str) -> u64 {
        self.rt.get(thread).map_or(0, |r| r.closed_runtimes) + 1
    }

    fn view(&self, thread: &str) -> Result<&ThreadView, RpcError> {
        self.registry
            .get(thread)
            .ok_or_else(|| rpc(codes::NOT_FOUND, format!("no thread {thread:?}")))
    }

    fn resolve(&self, query: &str) -> Result<String, RpcError> {
        self.registry
            .resolve(query)
            .map(|v| v.id.clone())
            .map_err(resolve_err)
    }

    /// Whether `name` is already a thread id, name, alias or former name (a renamed thread's
    /// earlier names stay its own: its worktree path and branch may carry one), or reserved.
    /// Case does not count ([`same_name`]).
    fn name_taken(&self, name: &str) -> bool {
        self.name_taken_but(name, None)
    }

    /// [`Core::name_taken`], except that thread `own`'s name and former names do not count (it
    /// may take one back, or change the case of its name).
    fn name_taken_but(&self, name: &str, own: Option<&str>) -> bool {
        let same = |n: &String| same_name(n, name);
        self.reserved_names.iter().any(same)
            || self.registry.list().iter().any(|t| {
                let mine = own == Some(t.id.as_str());
                same(&t.id)
                    || (!mine && same(&t.name))
                    || t.aliases.iter().any(same)
                    || (!mine && t.former_names.iter().any(same))
            })
    }

    fn thread_json(&self, v: &ThreadView) -> Value {
        let mut j = serde_json::to_value(v).unwrap_or(Value::Null);
        if let Value::Object(m) = &mut j {
            // A job this hub runs says what it does now (`hub::jobs`), not its last renewal.
            if let Some(Value::Object(tasks)) = m.get_mut("background_tasks") {
                for (id, t) in tasks.iter_mut() {
                    let now = v
                        .background_tasks
                        .get(id)
                        .filter(|b| b.job)
                        .and_then(|_| self.job_description(id));
                    if let (Some(now), Some(t)) = (now, t.as_object_mut()) {
                        t.insert("description".into(), json!(now));
                    }
                }
            }
            m.insert("status".into(), json!(v.status()));
            // What it does now, one line (`hub::activity`).
            m.insert("activity".into(), json!(self.activity_line(v)));
            m.insert(
                "background_task_count".into(),
                json!(v.background_tasks.len()),
            );
            let pending = self.store.pending_count(&v.id).unwrap_or(0);
            m.insert("pending".into(), json!(pending));
            // Prompts for it that wait for other threads (`hub::after`).
            let gated = self.gated_json(&v.id);
            if !gated.is_empty() {
                m.insert("queued_after".into(), json!(gated));
            }
            let live = self.rt.get(&v.id).is_some_and(|r| r.live.is_some());
            m.insert("live".into(), json!(live));
            // Its box connected and reconciled: what the log shows of it is current (`false`:
            // its runtime may have moved on, unseen).
            m.insert("box_online".into(), json!(self.box_online(&v.box_name)));
            // The settings the live runtime was started in, each when it is not the thread's
            // yet (it is restarted once idle, `hub::restart`): `runtime_model` /
            // `runtime_effort` null for the runtime's default.
            if let Some(rs) = self
                .rt
                .get(&v.id)
                .filter(|r| r.live.is_some() && !v.is_orchestrator())
                .and_then(|r| r.runtime_settings.as_ref())
            {
                if rs.mode != v.mode {
                    m.insert("runtime_mode".into(), json!(rs.mode));
                }
                if rs.model != v.model {
                    m.insert("runtime_model".into(), json!(rs.model));
                }
                if rs.effort != v.effort {
                    m.insert("runtime_effort".into(), json!(rs.effort));
                }
            }
            // The interactive process an attach waits for (`attach.child`), and whether its
            // lease connection is gone.
            if let Some(a) = self.rt.get(&v.id).and_then(|r| r.attach.as_ref()) {
                if let Some(c) = &a.child {
                    m.insert(
                        "attach_process".into(),
                        json!({ "pid": c.pid, "orphaned": a.orphaned }),
                    );
                }
            }
            if let Some(argv) = self.rt.get(&v.id).and_then(|r| r.pty_command.as_deref()) {
                let argv: Value = serde_json::from_str(argv).unwrap_or(Value::Null);
                m.insert("pty_command".into(), argv);
            }
            // (An `acp` thread's `acp_command` is the view's own, `ThreadView::acp_command`.)
        }
        j
    }

    // --- turns -------------------------------------------------------------------------------

    /// Queues `prompt` for thread `id` and dispatches it if the thread is free.
    fn queue_prompt(
        &mut self,
        hub: &Arc<Inner>,
        id: &str,
        prompt: &str,
        from: TurnFrom,
        source: Source,
        routed_by: Option<&str>,
    ) -> Result<SendOutcome, RpcError> {
        let mut batch = Batch::default();
        let routed_by = routed_by.map(|r| RoutedBy::Id(r.to_string()));
        self.push_prompt(&mut batch, id, prompt, from, source, routed_by)?;
        if from == TurnFrom::User {
            batch.user_send();
        }
        // Read before the dispatch, which may start it.
        let starts = self.rt.get(id).and_then(|r| r.live.as_ref()).is_none();
        let queued = self.commit_prompts(hub, batch)?;
        let mut out = self.dispatch_queued(hub, id, queued);
        if starts {
            out.warning = self.start_warning(hub, id);
        }
        Ok(out)
    }

    /// What a change that starts or restarts thread `id`'s runtime is warned of: its box
    /// reports the runtime's CLI missing (`hub::box_agents`).
    fn start_warning(&self, hub: &Inner, id: &str) -> Option<String> {
        let view = self.registry.get(id)?;
        let w = box_agents::agent_warning(hub, &view.box_name, &view.runtime)?;
        tracing::info!(thread = %id, box_name = %view.box_name, "{w}");
        Some(w)
    }

    /// Checks that thread `id` can take `prompt` now and adds it to `batch` (queued when the
    /// batch commits; `said_after` is the control head after the batch's events).
    fn push_prompt(
        &self,
        batch: &mut Batch,
        id: &str,
        prompt: &str,
        from: TurnFrom,
        source: Source,
        routed_by: Option<RoutedBy>,
    ) -> Result<(), RpcError> {
        if prompt.trim().is_empty() {
            return Err(rpc(codes::BAD_REQUEST, "prompt is empty"));
        }
        compact::refuse_compact_command(prompt, from)?;
        if self.shutting_down {
            return Err(rpc(codes::CONFLICT, "hub is shutting down"));
        }
        self.view(id)?;
        self.refuse_retired(id)?;
        self.refuse_deleting(id)?;
        self.refuse_switching(id)?;
        // A user message for the orchestrator records its arrival with it, in log order: it
        // ends the authority of the orchestrator's turn in progress (`hub::orch`).
        let routed_by = if from == TurnFrom::User && self.orch.is_thread(id) {
            self.mark_user_prompt(batch, id, &source, routed_by)
        } else {
            routed_by
        };
        batch.prompts.push(BatchPrompt {
            thread: id.to_string(),
            prompt: prompt.to_string(),
            from,
            source,
            routed_by,
            said_after: None,
            user_send: false,
        });
        Ok(())
    }

    /// Commits `batch`, which queues exactly one prompt; returns that prompt's queue row.
    fn commit_prompts(&mut self, hub: &Inner, batch: Batch) -> Result<i64, RpcError> {
        let done = self.append_batch(hub, batch).map_err(append_err)?;
        done.queued
            .first()
            .copied()
            .ok_or_else(|| internal("the batch queued no prompt"))
    }

    /// Dispatches thread `id` after its prompt `queued` was committed. The prompt is in the
    /// queue: from here on errors are reported, not returned, so the caller never retries
    /// (and duplicates) a prompt that is already queued.
    fn dispatch_queued(&mut self, hub: &Arc<Inner>, id: &str, queued: i64) -> SendOutcome {
        if let Err(e) = self.preempt_notice(hub, id) {
            tracing::warn!(thread=%id, error=%e, "could not interrupt background notice");
        }
        // The user's own send releases a queue a failure held (a runtime-start failure, a
        // worker's crash), for this one dispatch: if the runtime fails again, the queue is held
        // again. A prompt anything else queued (a job's wake, the orchestrator's send, a
        // prompt that waited for other threads) waits with the rest (`Batch::user_send`).
        match self.store.is_user_send(queued) {
            Ok(true) => self.release_queue(id),
            Ok(false) => {}
            Err(e) => {
                tracing::warn!(thread = %id, error = %format!("{e:#}"), "cannot tell whether the user sent a queued prompt; a held queue stays held");
            }
        }
        // Only report a turn when it was this prompt that got dispatched (not an earlier
        // queued prompt, nor an automatic retry).
        let dispatched = self
            .dispatch_next(hub, id)
            .map(|d| d.filter(|(_, row)| *row == Some(queued)).map(|(t, _)| t));
        let (turn, mut dispatch_error) = match dispatched {
            Ok(turn) => (turn, None),
            Err(e) => {
                tracing::error!(thread = %id, error = %e, "dispatching a queued prompt failed");
                (None, Some(format!("{e:#}")))
            }
        };
        let pending = match self.store.pending_count(id) {
            Ok(n) => n,
            Err(e) => {
                dispatch_error.get_or_insert_with(|| format!("{e:#}"));
                0
            }
        };
        let parked = self.registry.get(id).is_some_and(|v| v.parked)
            || self.rt.get(id).is_some_and(|r| r.park.is_some());
        SendOutcome {
            thread: id.to_string(),
            thread_name: self.thread_name(id),
            queued,
            msg_ref: queued,
            turn,
            pending,
            dispatch_error,
            parked,
            warning: None,
        }
    }

    /// Whether the box `name` can run commands now: `local`, or a remote box that is online.
    fn box_online(&self, name: &str) -> bool {
        name == LOCAL_BOX || self.online_boxes.contains(name)
    }

    /// `box_offline` unless the thread's box is online ([`Core::box_online`]) and not being
    /// switched to another build or drained ([`Core::refuse_switching_box`]).
    fn require_box(&self, view: &ThreadView) -> Result<(), RpcError> {
        self.require_online(view)?;
        self.refuse_switching_box(&view.box_name)
    }

    /// `box_offline` unless the thread's box is online ([`Core::box_online`]).
    fn require_online(&self, view: &ThreadView) -> Result<(), RpcError> {
        if self.box_online(&view.box_name) {
            Ok(())
        } else {
            Err(rpc(
                codes::BOX_OFFLINE,
                format!(
                    "box {:?} of {} is offline (its runner is not connected)",
                    view.box_name, view.name
                ),
            ))
        }
    }

    /// [`Core::require_box`] for an answer to a runtime on the thread's box: a box being
    /// drained takes it while its running turns may still finish (`hub::drain`), so a turn
    /// that waits on a permission can end.
    fn require_box_for_answer(&self, view: &ThreadView) -> Result<(), RpcError> {
        self.require_online(view)?;
        match self.draining_boxes.get(&view.box_name) {
            Some(d) if d.takes_answers() && !self.switching_boxes.contains(&view.box_name) => {
                Ok(())
            }
            _ => self.refuse_switching_box(&view.box_name),
        }
    }

    /// [`Core::refuse_switching_box`] of box `name` and, when it is a remote box, an
    /// operation on it begun in the same step ([`crate::remote::RemoteBox::begin_op`]): hold it
    /// across the awaits that follow, so that the box is not quiet (`Core::box_quiet`) and no
    /// switch begins until it is dropped.
    pub(super) fn reserve_box(
        &self,
        hub: &Inner,
        name: &str,
    ) -> Result<Option<crate::remote::BoxOp>, RpcError> {
        self.refuse_switching_box(name)?;
        Ok(hub.remote(name).map(|r| r.begin_op()))
    }

    /// `box_offline` while SSH box `name` is being switched to the hub's build (its
    /// `switching` gate, `hub::box_switch`): a repository request started now (a new thread,
    /// a park, resume, deletion or restore) would fail when the box's session closes. An
    /// operation that awaits the box after this check holds [`Core::reserve_box`]'s guard
    /// instead (one that records its own state in the same step, such as a park, resume,
    /// deletion or attach lease, is counted by that). The same while box `name` is drained
    /// (`hub::drain`): its runner speaks an older protocol, and gets nothing new.
    fn refuse_switching_box(&self, name: &str) -> Result<(), RpcError> {
        if let Some(d) = self.draining_boxes.get(name) {
            Err(rpc(codes::BOX_OFFLINE, d.refusal(name)))
        } else if self.switching_boxes.contains(name) {
            Err(rpc(
                codes::BOX_OFFLINE,
                format!("box {name:?} is switching to the hub's twr build: try again in a minute"),
            ))
        } else {
            Ok(())
        }
    }

    /// Whether the log shows a runtime of `view` (a thread on a remote box) that the hub has
    /// not taken over: a process that has not stopped, crashed, parked or attached, or an open
    /// turn (its runtime may be starting on the box), and no live runtime here. After a hub
    /// restart that is every such thread until its box connects (`hub::runners`
    /// `adopt_remote_sessions` / `reconcile` decide then); the thread is not idle.
    fn unadopted_remote_runtime(&self, view: &ThreadView) -> bool {
        let rt = self.rt.get(&view.id);
        !view.parked
            && view.attached.is_none()
            && !rt.is_some_and(|r| r.live.is_some())
            && (rt.is_some_and(|r| r.process_live) || view.has_work())
    }

    /// Whether `thread` can take a new turn now: the hub is not shutting down, no turn is
    /// open, it is not attached (or being attached), parked or resumed, its box is online and
    /// neither being switched to another build nor drained, its runtime is not exiting or being
    /// stopped, and no answer to its remote runtime awaits the box's acknowledgement.
    fn thread_free(&self, thread: &str) -> bool {
        self.thread_free_but_drain(thread)
            && self
                .registry
                .get(thread)
                .is_some_and(|v| !self.draining_boxes.contains_key(&v.box_name))
    }

    /// [`Core::thread_free`] but for the gate of a box being drained (`hub::drain`): what a
    /// settings restart waits for ([`Core::dispatch_next`]), so that a narrowed permission
    /// mode stops the runtime of a drained box as anywhere else.
    fn thread_free_but_drain(&self, thread: &str) -> bool {
        if self.shutting_down {
            return false;
        }
        let Some(view) = self.registry.get(thread) else {
            return false;
        };
        if view.current_turn.is_some() || view.attached.is_some() || view.parked || view.retired {
            return false;
        }
        // Being deleted: nothing more runs. Being trimmed: nothing runs until that is over.
        if self
            .rt
            .get(thread)
            .is_some_and(|r| r.deleting || r.trimming)
        {
            return false;
        }
        // A remote box's threads wait (prompts stay queued) while it is offline, or while it
        // is being switched to another build (`hub::box_switch`) or drained (`hub::drain`,
        // in `Core::thread_free`); its reconnection dispatches them (`hub::runners`).
        if !self.box_online(&view.box_name) || self.switching_boxes.contains(&view.box_name) {
            return false;
        }
        let rt = self.rt.get(thread);
        // Being attached (or quiescing for it): prompts wait for `thread.detached`. Being
        // parked or resumed: they wait for `thread.resumed`.
        if rt.is_some_and(|r| r.attach.is_some() || r.park.is_some() || r.resuming) {
            return false;
        }
        // A runtime that exited and whose closing sequence is not recorded yet: a new runtime
        // started now would be closed by that sequence.
        if rt.is_some_and(|r| r.exiting) {
            return false;
        }
        // A runtime the hub gave up on and that has not exited yet: a runtime started now
        // would run next to it, in the same session and worktree. Or the kill of what is left
        // of a runtime is in progress: a runtime started now could be hit by it.
        if rt.is_some_and(ThreadRt::giving_up) {
            return false;
        }
        // An answer recorded for a remote runtime that its box has not acknowledged yet (a
        // PTY's question between turns): a new turn's `send` could overtake it, and a
        // reconnection would then replay the prompt before the answer the runtime waits
        // for. The acknowledgement dispatches (`hub::runners`). (A `send` needs no such
        // wait: its turn is open until the runtime ends it; a `stop` is `stop_requested`.)
        if view.box_name != LOCAL_BOX {
            match self.store.unanswered_respond(thread) {
                Ok(false) => {}
                Ok(true) => return false,
                Err(e) => {
                    tracing::warn!(thread, error = %format!("{e:#}"), "cannot tell whether an answer is still on its way; the thread waits");
                    return false;
                }
            }
        }
        // A runtime that is being stopped gets nothing new; its exit dispatches the queue. The
        // durable request counts too (a runtime taken over after a restart, one whose exit is
        // being recorded): its exit closes the thread as stopped, and would close a runtime
        // started now with it.
        !rt.is_some_and(|r| r.stop_requested || r.live.as_ref().is_some_and(|l| l.stop_requested))
    }

    /// Whether a turn other than a due automatic retry (a queued prompt, an orchestrator
    /// notice turn, an autostart) may start on `thread` now: it is free ([`Core::thread_free`]:
    /// no open turn, not attached, parked, exiting or being stopped, ...) and no failure holds
    /// its queue ([`ThreadRt::queue_held`]).
    fn may_take_turn(&self, thread: &str) -> bool {
        self.thread_free(thread)
            && self
                .registry
                .get(thread)
                .is_some_and(|v| !v.runtime_tasks())
            && !self.rt.get(thread).is_some_and(|r| r.queue_held)
    }

    /// A dispatch (`what`) found the thread busy: when a runtime kill in progress
    /// ([`ThreadRt::reaping`]) is why, its end runs it ([`reap_runtime`]).
    ///
    /// [`reap_runtime`]: supervisor::reap_runtime
    fn hold_for_reap(&mut self, thread: &str, what: AfterReap) {
        if let Some(rt) = self.rt.get_mut(thread).filter(|r| r.reaping > 0) {
            rt.after_reap = rt.after_reap.max(what);
        }
    }

    /// Lets thread `thread`'s queued prompts be dispatched again after a failure held them
    /// ([`ThreadRt::queue_held`]): only the user's own send or resume does this.
    fn release_queue(&mut self, thread: &str) {
        if let Some(rt) = self.rt.get_mut(thread) {
            rt.queue_held = false;
            rt.crash_hold = None;
        }
    }

    /// Dispatches the thread's next turn if it is free ([`Core::thread_free`]): a due automatic
    /// retry first (`hub::retry`), else the next queued prompt unless a runtime-start failure
    /// holds the queue ([`ThreadRt::queue_held`]). Returns the turn number.
    fn try_dispatch(&mut self, hub: &Arc<Inner>, thread: &str) -> anyhow::Result<Option<u64>> {
        Ok(self.dispatch_next(hub, thread)?.map(|(turn, _)| turn))
    }

    /// [`Core::try_dispatch`], also returning the queue row the turn came from (`None` for a
    /// retry).
    fn dispatch_next(
        &mut self,
        hub: &Arc<Inner>,
        thread: &str,
    ) -> anyhow::Result<Option<(u64, Option<i64>)>> {
        if !self.thread_free_but_drain(thread) {
            self.hold_for_reap(thread, AfterReap::Dispatch);
            return Ok(None);
        }
        // An orchestrator being switched takes nothing from its queue: the switch stops it
        // and drops what is queued (`hub::handoff`).
        if self.orch.switching && self.orch.is_thread(thread) {
            return Ok(None);
        }
        // A live runtime started in another permission mode, model or effort than the
        // thread's now: it is restarted on its session before anything else runs
        // (`hub::restart`); its exit dispatches again. While an item it raised between turns
        // defers that, nothing runs in the old settings. On a box being drained too (its
        // `stop` is drain-safe), so a narrowed mode takes effect there as anywhere.
        if self.restart_for_settings(hub, thread)? {
            return Ok(None);
        }
        // Nothing else runs on a box being drained (`hub::drain`).
        if !self.thread_free(thread) {
            return Ok(None);
        }
        if let Some(turn) = self.start_retry(hub, thread)? {
            return Ok(Some((turn, None)));
        }
        if self.rt.get(thread).is_some_and(|r| r.queue_held) {
            return Ok(None);
        }
        let next = self.store.next_pending(thread)?;
        // A compaction the user asked for, once what was queued before it ran (`hub::compact`).
        if self.compaction_due(hub, thread, next.as_ref().map(|p| p.id)) {
            return Ok(Some((self.start_compact_turn(hub, thread)?, None)));
        }
        let Some(p) = next else {
            return Ok(None);
        };
        // A new orchestrator's handoff is its first turn, before the first queued prompt
        // (`hub::handoff`); the prompt follows once it ended.
        if let Some(turn) = self.deliver_pending_handoff(hub, thread)? {
            return Ok(Some((turn, None)));
        }
        // The orchestrator's turns record when the prompt arrived, so the relay guard reads
        // the same thing live and on replay. Rows from before schema 2 have none (the guard
        // then refuses to relay).
        // The message's ref goes with its turn: `message.withdraw` finds the turn by it.
        let mut twr = Map::new();
        twr.insert(MESSAGE_REF_KEY.into(), json!(p.id));
        if let Some(said_after) = p.said_after.filter(|_| self.orch.is_thread(thread)) {
            twr.insert("said_after".into(), json!(said_after));
        }
        let turn = self.start_turn(
            hub,
            thread,
            p.prompt,
            p.from,
            p.source,
            p.routed_by,
            twr,
            Batch {
                dequeue: Some(p.id),
                ..Batch::default()
            },
        )?;
        Ok(Some((turn, Some(p.id))))
    }

    /// Starts the thread's next turn with `prompt`: appends `turn.started` (the prompt as
    /// given, `twr` as `ext.twr`) in one batch with `batch` (what must be recorded with the
    /// turn's start, e.g. the queue row it removes) and a changed standing context, then
    /// delivers the prompt, with the project's standing context prepended for worker threads
    /// (`hub::context`; the turn then records `ext.twr.context_hash`).
    #[allow(clippy::too_many_arguments)]
    fn start_turn(
        &mut self,
        hub: &Arc<Inner>,
        thread: &str,
        prompt: String,
        from: TurnFrom,
        source: Source,
        routed_by: Option<String>,
        mut twr: Map<String, Value>,
        mut batch: Batch,
    ) -> anyhow::Result<u64> {
        let view = self.view(thread).map_err(anyhow::Error::new)?;
        let turn = view.turns + 1;
        // A thread that moved to another runtime: this turn starts that runtime's first
        // session, which is given what the previous one did (`hub::switch`).
        let handoff = (view.handoff_pending && !self.orch.is_thread(thread))
            .then(|| self.thread_handoff_or_note(thread));
        let injected = self.context_for_turn(hub, thread, &mut batch)?;
        if let Some(i) = &injected {
            context::turn_ext(i, &mut twr);
        }
        if let Some(h) = &handoff {
            twr.insert(switch::HANDOFF_KEY.into(), json!(h));
        }
        if self.is_orch(thread) {
            twr.insert(
                "prompt_fingerprint".into(),
                json!(orch::prompt_fingerprint()),
            );
        }
        // The orchestrator's memory, when its runtime has not got this version of it
        // (`hub::memory`): recorded by its version and size, never its text.
        let memory = self.memory_for_turn(hub, thread);
        if let Some(m) = memory.as_ref().and_then(|m| m.ext.clone()) {
            twr.insert("memory".into(), m);
        }
        let ext = if twr.is_empty() {
            None
        } else {
            twr_ext_map(Value::Object(twr))
        };
        batch.push(
            thread,
            Some(turn),
            source,
            EventBody::TurnStarted(TurnStarted {
                prompt: prompt.clone(),
                from,
                routed_by,
                ext,
                ..Default::default()
            }),
        );
        let delivered = format!(
            "{}{}{prompt}",
            injected.map_or(String::new(), |i| i.prefix),
            handoff.unwrap_or_default()
        );
        // A remote session's `send` is recorded with the turn (`hub::runners`).
        let send = self.remote_send(hub, thread, turn, &delivered);
        batch.remote_cmds.extend(send.clone());
        self.append_batch(hub, batch)?;
        if let Some(c) = &send {
            runners::expect_recorded(hub, c);
        }
        // A user turn of the orchestrator gets the updates since its previous one; any of its
        // turns, the message the user took back in the turn before (`hub::withdraw`).
        let delivered = if self.is_orch(thread) {
            let mut facts: Vec<String> = self
                .taken_back_fact(thread, turn)
                .iter()
                .map(|f| json!(f).to_string())
                .collect();
            if from == TurnFrom::User {
                let updates = self.update_context(thread).unwrap_or_else(|e| {
                    tracing::warn!(thread=%thread, error=%e, "could not load conversation updates");
                    String::new()
                });
                facts.extend((!updates.is_empty()).then_some(updates));
            }
            let delivered = updates::with_hub_facts(&facts, delivered, from == TurnFrom::User);
            match memory.as_ref().and_then(|m| m.text.as_deref()) {
                Some(block) => format!("{block}{delivered}"),
                None => delivered,
            }
        } else {
            delivered
        };
        self.deliver(hub, thread, delivered, turn)?;
        if let Some(m) = memory {
            self.memory_delivered(thread, m.rev);
        }
        Ok(turn)
    }

    /// Sends turn `turn` to the thread's runtime, resuming or spawning it as needed. When the
    /// runtime cannot take the turn, the turn ends `error` (with `ext.twr.error`) and gets its
    /// report, so the prompt is never silently lost.
    fn deliver(
        &mut self,
        hub: &Arc<Inner>,
        thread: &str,
        prompt: String,
        turn: u64,
    ) -> anyhow::Result<()> {
        let view = self.view(thread).map_err(anyhow::Error::new)?.clone();
        if let Some(live) = self.rt.get(thread).and_then(|r| r.live.as_ref()) {
            if live.handle.send(prompt, turn).is_ok() {
                return Ok(());
            }
            // The process is exiting; its supervisor closes the thread. The queue is not
            // dispatched further here: it would only hit the same dead runtime.
            let e = anyhow::anyhow!("the runtime exited before it received the turn");
            return self.fail_turn(hub, thread, turn, view.session_id.clone(), &e);
        }
        // `turn.started` is committed before the runtime gets the prompt: a spawn failure ends
        // the turn `error` (with its report), and a hub that dies first closes the open turn
        // at startup.
        if let Err(e) = self.start_runtime(hub, thread, Some((prompt, turn))) {
            // Held before the failure's reactions run, so nothing they dispatch takes the
            // queue.
            self.rt(thread).queue_held = true;
            self.fail_turn(hub, thread, turn, view.session_id.clone(), &e)?;
            // A due automatic retry (a transient failure) gets its one attempt: a retried
            // turn is never retried again, so this recurses at most once. Queued prompts are
            // NOT dispatched after a runtime that cannot start: each would most likely fail
            // the same way (a missing binary, a bad configuration), and dispatching them here
            // recursed through the whole queue. They stay queued behind the failed turn's
            // error item until the user acts (the next `send`, a resume, a hub restart), as
            // after a crash: `queue_held`, which every later dispatch checks.
            if self.thread_free(thread) {
                self.start_retry(hub, thread)?;
            }
        }
        Ok(())
    }

    /// Starts the thread's runtime (resuming its session when it has one) with `first` as the
    /// first turn, if given, and supervises it. The orchestrator gets its tools, system
    /// context and a fresh token ([`Core::orchestrator_spawn_config`]).
    fn start_runtime(
        &mut self,
        hub: &Arc<Inner>,
        thread: &str,
        first: Option<(String, u64)>,
    ) -> anyhow::Result<()> {
        let view = self.view(thread).map_err(anyhow::Error::new)?.clone();
        // Never a retired orchestrator: it would revoke the current one's token.
        self.refuse_retired(thread).map_err(anyhow::Error::new)?;
        // Nor while what a stopped hub's job left in its directory may still run. (The
        // orchestrator's only jobs are the user's `!` commands of its view: what is left of
        // one is the user's, and holds no runtime back.)
        if !view.is_orchestrator() {
            self.refuse_unreaped(thread).map_err(anyhow::Error::new)?;
        }
        // Taken whether or not the start succeeds: a failed start's processes (if any) never
        // share a generation marker with a later runtime.
        self.generation += 1;
        let generation = self.generation;
        // The agent marker tells the hub's socket that a connection from the runtime's
        // processes (also a background job that left its shell) is the thread's agent
        // (`hub::agents`).
        let mut cfg = SpawnConfig {
            prompt_turn: first.as_ref().map(|(_, t)| *t),
            ..spawn_config(hub, &view)
        };
        cfg.env.splice(
            0..0,
            [
                (THREAD_ENV.to_string(), thread.to_string()),
                (RUNTIME_GEN_ENV.to_string(), generation.to_string()),
                (AGENT_ENV.to_string(), format!("{thread}:{generation}")),
            ],
        );
        // A `pty` thread's command, on every spawn and resume (the adapter has no session to
        // resume, so a resume runs the command afresh).
        if let Some(argv) = self.rt.get(thread).and_then(|r| r.pty_command.clone()) {
            cfg.env
                .push((twr_adapters::pty::COMMAND_ENV.to_string(), argv));
        }
        // An `acp` thread's agent command, likewise.
        if let Some(r) = self.rt.get(thread) {
            cfg.env.extend(r.acp_env.iter().cloned());
        }
        // Removed on drop: when this runtime exits, or right away if it cannot start.
        let orch_spawn = if self.orch.is_thread(thread) {
            self.orchestrator_spawn_config(hub, &view.runtime, &mut cfg)?
        } else {
            None
        };
        // A worker on the hub's machine runs under the limits configured (`hub::limits`).
        if !self.orch.is_thread(thread)
            && !view.is_orchestrator()
            && hub.host(&view.box_name).is_some_and(|h| !h.is_remote())
        {
            cfg.wrap = hub.limits.wrap_for(&cfg.env);
        }
        let (mcp_file, orch_token) = match orch_spawn {
            Some(o) => (Some(o.file), Some(o.token)),
            None => (None, None),
        };
        // Its working directory is held from before it starts until it is gone; refused while
        // the directory's worktree is being removed (a park whose caller went away, whose git
        // runs on) or created.
        let workdir = match hub.host(&view.box_name) {
            Some(host) => host.use_workdir(&cfg.cwd, thread)?,
            None => None,
        };
        // A local runtime is recorded before it exists: a hub that dies before the runtime
        // reports its process finds it at startup by its marker (`crate::store`
        // `spawn_intents`). The closing sequence of the thread retires the record.
        let intent = match (hub.host(&view.box_name), hub.start_ticks) {
            (Some(host), Some(min_start)) if !host.is_remote() => {
                self.store.add_spawn_intent(thread, min_start)?;
                true
            }
            _ => false,
        };
        let started = match hub.host(&view.box_name) {
            None => Err(anyhow::anyhow!("box {:?} is not available", view.box_name)),
            Some(host) => match &view.session_id {
                Some(session) => host
                    .resume(&view.runtime, cfg, session.clone())
                    .and_then(|h| {
                        if let Some((prompt, turn)) = first {
                            h.send(prompt, turn)?;
                        }
                        Ok(h)
                    }),
                None => host.spawn(&view.runtime, cfg, first.map(|(p, _)| p)),
            },
        }
        .and_then(|mut handle| match handle.take_events() {
            Some(events) => Ok((handle, events)),
            None => Err(anyhow::anyhow!("adapter handle without an event stream")),
        });
        let (handle, events) = match started {
            Ok(s) => s,
            Err(e) => {
                if intent {
                    if let Err(e) = self.store.clear_spawn_intent(thread) {
                        tracing::warn!(thread, error = %format!("{e:#}"), "cannot forget a spawn intent (the next hub start does)");
                    }
                }
                return Err(e);
            }
        };
        // The orchestrator's token is valid from now on, and only while this runtime lives.
        if let Some(token) = orch_token {
            self.orch.arm_token(token, generation);
        }
        let log_runtime = self.runtime_gen(thread);
        // A remote session's run is the hub's to name (`spawn` / `resume`): known before any
        // frame of it, so no frame or exit of another run of the thread is ever taken for it.
        let run = hub
            .remote(&view.box_name)
            .and_then(|r| r.session_run(thread));
        // From here the process its adapter reported starting (`AdapterHandle::pid`) is the
        // thread's for the hub's socket, before the runtime reports it (`hub::agents`). The
        // core lock is held from the spawn on, and classifying a connection takes it, so no
        // connection is classified in between.
        self.rt(thread).live = Some(Live {
            handle,
            generation,
            stop_requested: false,
            attach: false,
            park: false,
            _mcp_file: mcp_file,
            _workdir: workdir,
            run,
            run_confirmed: false,
            given_up: None,
            log_runtime,
        });
        hub.live_count.send_modify(|n| *n += 1);
        // A runtime runs again: its turns' ends move the queue as usual.
        self.rt(thread).queue_held = false;
        self.rt(thread).crash_hold = None;
        // The settings it runs in (recorded on its `thread.started`, `hub::restart`).
        self.rt(thread).runtime_settings = Some(restart::RunSettings::of(&view));
        tokio::spawn(supervise(
            Arc::clone(hub),
            thread.to_string(),
            generation,
            Source::Adapter(view.runtime.clone()),
            events,
        ));
        Ok(())
    }
}

/// The runtime configuration of `view` (no turn; of the environment only the configured
/// variables of a worker thread, [`Config::thread_env`], its project's only when it was made
/// for that configured project): what `deliver` spawns with and what `attach.begin` builds the
/// interactive command from.
fn spawn_config(hub: &Inner, view: &ThreadView) -> SpawnConfig {
    let (cwd, env) = if view.is_orchestrator() {
        (orch::orchestrator_cwd(&hub.config), Vec::new())
    } else {
        let cwd = view.worktree.clone().or(view.repo.clone());
        (
            PathBuf::from(cwd.unwrap_or_default()),
            hub.config
                .thread_env(view.project.as_deref().filter(|_| view.configured_project)),
        )
    };
    SpawnConfig {
        thread: view.id.clone(),
        cwd,
        env,
        mode: view.mode.clone(),
        model: view.model.clone(),
        effort: view.effort.clone(),
        thinking: hub.config.threads.thinking,
        repo: (!view.is_orchestrator())
            .then(|| view.repo.clone().map(PathBuf::from))
            .flatten(),
        // A project's own setting only when the thread was made for that configured project,
        // as for its `env`; never the orchestrator's (it is never in `auto`).
        codex_auto: if view.is_orchestrator() {
            Default::default()
        } else {
            hub.config
                .codex_auto(view.project.as_deref().filter(|_| view.configured_project))
        },
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------------------------

impl Hub {
    /// How many times the store was synced to disk so far ([`store::Store::sync`]). For
    /// tests of what is durable when.
    #[doc(hidden)]
    pub fn store_syncs(&self) -> u64 {
        self.inner.core().store.sync_count()
    }

    /// Appends an event through the single writer (no hub reactions). Returns the stored
    /// event (payload capped, `raw` replaced by its blob pointer).
    pub fn append(
        &self,
        thread: &str,
        turn: Option<u64>,
        source: Source,
        body: EventBody,
        raw: Option<String>,
    ) -> anyhow::Result<Event> {
        self.inner
            .core()
            .append_ex(&self.inner, thread, turn, source, body, raw, None)
    }

    /// The configuration the hub started with (its `boxes` those of the start: a box added
    /// since is in [`Hub::config_show`]'s).
    pub fn config(&self) -> &Config {
        &self.inner.config
    }

    /// `config.show`: the configuration the hub runs with ([`config_view`]), its boxes the
    /// hub's own list (those added since it started included).
    pub fn config_show(&self) -> Value {
        config_view(&self.inner.config, Some(&self.inner.box_configs()))
    }

    /// The workspace this hub serves ([`HubOptions::workspace`]; `default` for none).
    pub fn workspace(&self) -> &str {
        self.inner
            .workspace
            .as_deref()
            .unwrap_or(twr_core::paths::DEFAULT_WORKSPACE)
    }

    /// `hub.info` as a user sees it ([`Hub::info_for`]).
    pub fn info(&self) -> Value {
        self.info_for(BoxView::Full)
    }

    /// `hub.info` as `view` sees it: the orchestrator and agents see less of the boxes and
    /// their agents (`hub::box_agents`).
    pub fn info_for(&self, view: BoxView) -> Value {
        // Read before taking the core: the hub's machine, where its local threads run.
        let load = twr_core::load::Load::sample_cached().map(|l| l.to_json());
        let boxes: Vec<Value> = self
            .boxes_json()
            .into_iter()
            .map(|b| box_agents::project_box(b, view))
            .collect();
        let agents = self.local_agents_json(view);
        // Whether next-message suggestions can run here (`hub::suggest`).
        let suggest_unavailable = self.suggest_unavailable();
        // (Read once, on the first call: not under the core lock.)
        let build = twr_core::build_id();
        let core = self.inner.core();
        json!({
            "boxes": boxes,
            "agents": agents,
            "version": twr_core::version(),
            "build": build,
            // The runner protocol its boxes' runners must speak (`twr box doctor` compares).
            "runner_protocol": twr_core::runner::PROTOCOL_VERSION,
            // The older runner protocols it drains (`hub::drain`): such a box is replaced once
            // its running turns are over.
            "drainable_protocols": twr_core::runner::DRAINABLE_PROTOCOLS,
            // Its runner listener is up: a token box's runner can connect (`twr box doctor`).
            "listening": self.inner.listening.load(std::sync::atomic::Ordering::Relaxed),
            "pid": std::process::id(),
            "db": self.inner.db_path,
            "instance": self.inner.instance_name,
            "workspace": self.workspace(),
            "started_at": self.inner.started_at,
            "threads": core.registry.len(),
            "control_head": core.gseq,
            "open_attention": core.attention.len(),
            "dropped_events": core.dropped_events,
            "sanitized_events": core.sanitized_events,
            "coalesced_events": core.coalesced_events,
            "persist_retries": core.persist_retries,
            "orchestrator": core.orchestrator_json(&self.inner),
            "load": load,
            "disk": core.disk_json(&self.inner),
            "notify_hook": !self.inner.config.notify.command.is_empty(),
            "worker_limits": self.inner.limits.by(),
            "warnings": self.inner.warnings(),
            "suggest": {"unavailable": suggest_unavailable},
        })
    }

    /// Resolves a thread reference (id, name, alias, unique prefix) to its id.
    pub fn resolve(&self, query: &str) -> Result<String, RpcError> {
        self.inner.core().resolve(query)
    }

    /// A snapshot of a thread's view.
    pub fn thread_view(&self, query: &str) -> Result<ThreadView, RpcError> {
        let core = self.inner.core();
        let id = core.resolve(query)?;
        core.view(&id).cloned()
    }
}
