//! The hub's Unix socket server: NDJSON requests per [`twr_core::proto`].
//!
//! Methods (params -> result):
//!
//! | method              | params                                              | result |
//! |---------------------|-----------------------------------------------------|--------|
//! | `hello`             | `{client, token?, protocol?}`                       | `{client, identity, workspace}` |
//! | `hub.info`          | -                                                   | `{version, pid, db, started_at, threads, control_head, open_attention, orchestrator, boxes: [{name, transport, state, online, version, refused, last_error, notes, os, arch, build, switch_pending}], client, identity, warnings}` |
//! | `box.add`           | `{name}` (users only)                               | `{box, transport, listening}`: box `name` of `config.toml` / `boxes.toml`, read again, added to the running hub (`hub::boxes`) |
//! | `box.install`       | `{name}` (users only)                               | `{box}`: SSH box `name`'s supervisor tries again now (`crate::ssh_box`) |
//! | `hub.shutdown`      | -                                                   | `{ok: true}` (then the hub stops) |
//! | `config.show`       | -                                                   | the configuration the hub loaded, from an allowlist (`hub::config_view`): no token, `env` value, preset command line, `ssh` destination or TLS file; `env` tables as `env_names` (not for agents) |
//! | `thread.spawn` | `{operation_id, request: <thread.new params>, prompt?}` | durable operation (pending/complete/error) |
//! | `thread.spawn_status` | `{operation_id}` | durable operation |
//! | `thread.transcript` | `{thread, turn?, since_seq?, limit?}` | filtered semantic events, default latest turn |
//! | `thread.new`        | `{project, runtime?, name?, mode?, model?, effort?, aliases?, box?, worktree?, no_worktree?, base?}` | thread object |
//! | `thread.list`       | -                                                   | `[thread object]` |
//! | `thread.get`        | `{thread}`                                          | thread object + `attention: [item]` |
//! | `thread.send`       | `{thread, prompt}`                                  | `{thread, queued, ref, turn, pending, dispatch_error?}` |
//! | `say`               | `{message, focused?, seen_gseq?}`                   | `{action, thread?, thread_name?, ref?, turn?, attn_id?, options?, answer?, decision_ids}` |
//! | `message.withdraw`  | `{thread, ref}` (users only)                        | `{withdrawn: true, state: "queued" \| "running", thread, thread_name, ref, prompt, turn?}` or `{withdrawn: false, reason: "acted" \| "ended" \| "uninterruptible" \| "withdrawn" \| "unknown", message, thread, thread_name, ref, turn?}` (`hub::withdraw`) |
//! | `thread.stop`       | `{thread}`                                          | `{thread, stopping, box_offline?, dropped_prompts}` |
//! | `thread.set_mode`   | `{thread, mode}`                                    | `{thread, thread_name, mode, previous, changed, applies?, message?, dispatch_error?}` (the orchestrator: or `{status: "needs_confirmation", ...}`) |
//! | `thread.set_model`  | `{thread, runtime?, model?, effort?}` (at least one; blank or `default` = the runtime's default; with another `runtime`, one left out is that runtime's default) | `{thread, thread_name, runtime, previous_runtime, runtime_changed, model, effort, previous_model, previous_effort, changed, applies?, message?, dispatch_error?}` |
//! | `thread.rename`     | `{thread, name}` (a worker thread; the name free and valid as for `thread.new`) | `{thread, thread_name, previous, changed, worktree, branch, message}` (`thread.renamed`; the worktree and branch keep their names) |
//! | `thread.delete`     | `{thread}` or `{threads: [string]}` (one to 32)      | `{status: "needs_confirmation", threads: [{thread, thread_name, pending, worktree?, branch?, dirty_files?, hidden_changes?, ignored?, ignored_entries?, head_only_commits?, branch_only_commits?, checkout?, checkout_only_commits?, shared?, existing_branch?, worktree_missing?, worktree_unchecked?}], thread, thread_name, attn_id, revision, summary, options, existing, replaces?, message}` (a delete card for all of them; nothing is deleted until the user confirms it; `thread` / `thread_name`: the first thread's) |
//! | `trash.list`        | -                                                   | `{items: [{item, short, thread, thread_name, project, box, deleted_at, deleted_at_ms, purge_at, purge_at_ms, bytes, size, losses, worktree, branch, dir, incomplete?, kept?, box_offline?}], keep_days, bytes, size, unavailable: [{box, error}], message}` (oldest first) |
//! | `trash.empty`       | `{items?: [string], all?, dry_run?}` (users only)   | `{purged: [{item, short, thread_name, box, bytes}], failed: [{item, box, error}], bytes, size, message}`; with `dry_run` nothing is purged: `{dry_run: true, items: [item as in trash.list], bytes, size, message}` (clients purge exactly those ids after asking) |
//! | `trash.restore`     | `{item, as?}` (users only)                          | `{thread, thread_name, previous_name, item, box, worktree, branch, checkout?, notes, skipped, session_id, message}` |
//! | `orchestrator.show` | -                                                   | `{available, runtime, model, thread, name, status, live, configured: {runtime, model}, retired, runtimes}` |
//! | `orchestrator.reset` | - (users only)                                      | Fresh session on the current runtime/model, no handoff; same result as `orchestrator.use` with `handoff: null` |
//! | `orchestrator.use`  | `{runtime, model?, summary?, wait_secs?}` (users only) | `{thread, name, runtime, model, previous: {thread, name, runtime, model, stopping, dropped_prompts} \| null, started, start_error?, handoff: {turn, bytes, summary, summary_note, exchanges, dropped_exchanges} \| null}` |
//! | `thread.park`       | `{thread, keep?, force?}`                           | `{thread, parked: true, worktree?, worktree_removed, kept_dirty, dirty_files?, kept_reason?, kept, worktree_missing?, worktree_error?, pending}` |
//! | `thread.resume`     | `{thread}`                                          | `{thread, resumed: true, worktree?, worktree_recreated, turn, pending}` |
//! | `attention.respond` | `{attn_id, revision?, answer: "allow" \| {decision, message?}, message?}` or `{attn_id, revision?, option_index, message?}` | the `attention.resolved` event (the orchestrator: or `{status: "needs_confirmation", attn_id, answer, message}`) |
//! | `attention.dismiss` | `{attn_id, revision?}` (users only; hub-raised items only) | the `attention.resolved` event (`by: user`, answer `cancel`) |
//! | `attention.list`    | `{all?}`                                            | `[item + thread_name]` in stack order |
//! | `attention.annotate`| `{attn_id, note}`                                   | the `attention.annotated` event |
//! | `report.write`      | `{thread, turn?, summary, grade}`                   | `{thread, turn, report, decision, review, summary}` |
//! | `attach.begin`      | `{thread, force?}`                                  | `{thread, lease, argv, cwd, env, session_id, runtime, box?, trust_prompt?}` |
//! | `attach.child`      | `{thread, pid, start_time}` (on the connection that attached) | `{thread}` |
//! | `attach.end`        | `{thread}` (on the connection that attached)        | `{thread, detached: true}` |
//! | `thread.events`     | `{thread, since_seq?, limit?}`                      | `{thread, events: [event], last_seq, next, more}` |
//! | `thread.subscribe`  | `{thread, since_seq?}`                              | ack `{thread, since_seq, head}`, then `{"stream":"thread","event":...}` items |
//! | `control.read`      | `{since?, limit?}`                                  | `{items: [{"stream":"control","gseq","event"}], head, next, more}` |
//! | `control.subscribe` | `{since?}`                                          | ack `{since, head}`, then `{"stream":"control","gseq":n,"event":...}` items |
//! | `blob.get`          | `{ref, offset?, len?}`                              | `{ref, size, offset, encoding: "utf8" \| "base64", data, eof}` |
//! | `context.read`      | `{project}`                                         | `{project, path, content, hash, bytes, remote, utf8, truncated}` |
//! | `project.files`     | `{project, path?, thread?}`                         | `{project, commit, branch_of, path, entries: [{name, kind, size, utf8?, secret?}], total, more}`: a directory of a committed tree (`hub::project_read`; not for agents) |
//! | `project.read`      | `{project, path, thread?, offset?}`                 | `{project, commit, branch_of, path, kind, content, offset, end, total, utf8?, note?}`: 16 KB of a committed file; secrets, binaries and files over 1 MiB refused |
//! | `project.search`    | `{project, query, path?, thread?, ignore_case?}`    | `{project, commit, branch_of, query, path, hits: [{path, line, text}], more, hidden}` |
//! | `context.set`       | `{project, content, expect_hash?}`                  | `{project, path, hash, bytes, changed}` (`conflict` if `expect_hash` is stale or the file is over 1 MiB) |
//! | `usage.get`         | -                                                   | `{now, runtimes: [{runtime, account, label, plan, windows: [{window, label, used_pct, resets_at, minutes}], observed_at, age_secs, stale, source}]}` |
//!
//! Identity: a connection is `user:<client>` (source of what it does; its
//! turns are `from: user`, whatever `thread.send` `from` says) unless its `hello` carried the
//! current orchestrator token (`TWR_ORCH_TOKEN`, made by the hub for each orchestrator
//! process): then it is the orchestrator (source `orchestrator`, turns `from: orchestrator`),
//! re-checked on every request. A wrong token, or the client name `orchestrator` without one,
//! is `unauthorized`. The orchestrator may call only `ORCHESTRATOR_METHODS`, and its
//! `attention.respond` and `thread.set_mode` go through the relay guard (`hub::orch`): refused
//! (`relay_forbidden`) when provenance fails, applied when the user's latest message is a
//! short command that asks for exactly that, and otherwise recorded as a proposal the user confirms
//! on a card (`{status: "needs_confirmation"}`, `hub::proposal`). Only a user answers a confirm
//! card.
//! A connection whose peer process is, or descends from, a process twr started for a thread on
//! this machine (a runtime of the hub's own box, worker or orchestrator, from its adapter's
//! spawn on, or an attach's reported process; matched by pid and start time), or which, or
//! any process of whose ancestry, carries the agent marker `TWR_AGENT` those processes are
//! started with, is that thread's agent (`hub::agents`),
//! decided when it connects; so is a peer whose ancestry cannot be read to the end,
//! or whose own environment, or that of an ancestor the agent could have hidden it in, cannot
//! be read, or whose credentials cannot be read or name no pid the hub can see (an agent of
//! no thread; the hub's own process, its in-process clients, excepted). An ancestor procfs
//! hides (`hidepid`) ends the ancestry like init, and without
//! `/proc` the process table is unknown, not every process gone.
//! It may call only `AGENT_METHODS`: reads, and `job.start` / `job.cancel` for its own
//! thread's jobs, only from a process twr started for that thread (or one that descends from
//! it; a marker alone is not enough), its ancestry walked again at each of them (the process
//! that opened the connection, still under the thread's live runtime or attach), and
//! `job.start` only while the thread's mode may run commands (`manual` or wider, `hub::jobs`;
//! else `relay_forbidden`). Anything else is
//! `unauthorized` ("this request comes from <name>'s agent (pid N, ...)"), and its `hello` and `hub.info` say
//! `identity: agent:<name>`. The orchestrator token is accepted only from a peer whose
//! ancestry contains the recorded process of the orchestrator runtime the token belongs to
//! (its MCP server), walked again at the `hello` and at every request after it
//! (a process of an earlier orchestrator runtime that outlived a restart is refused
//! on the connection it opened then), and only while the peer's pid still names the process
//! that opened the connection, as read when it connected; any other peer that
//! presents it, whatever its marker says, is `unauthorized`. A
//! process that both left its parent (reparented to init) and exec'd again without the marker
//! is not recognized, nor is a command a daemon of the user's that already runs (tmux, screen,
//! `systemd-run --user`, an IDE server) starts for an agent.
//! `orchestrator.use` switches the orchestrator to another runtime (and model): a new
//! orchestrator thread with a new session (no conversation moves between runtimes); the
//! current one is stopped and retired (`hub::orch`). The new one starts with a handoff turn
//! (`hub::handoff`): the previous one's own summary, asked for first unless `summary: false`
//! (the call then waits up to `wait_secs`, default 60, for a busy one to be idle, and for its
//! reply), plus the state the log shows. Only a user may call it: it is not in
//! `ORCHESTRATOR_METHODS`, and the hub refuses the orchestrator identity as well.
//! `thread.new` gets a twr-managed worktree unless `no_worktree`, or unless the project's
//! `worktree` (else `threads.worktree`) is `false` and neither `worktree` nor `base` asks for
//! one (`worktree` and `no_worktree` together are a `bad_request`).
//! `say` routes a user message with the decision ladder (see `hub::route`); `action` is
//! `send`, `respond`, `new_thread`, `delegate` or `ask` (then `attn_id` is the hub question on
//! the `hub` thread, answered with `attention.respond`; its `cancel` option drops the message),
//! or, without `seen_gseq`, `pending` for a message that tries to answer a waiting permission
//! or question (nothing routed or recorded; `items` lists the open ones with their revisions).
//! `attention.dismiss` resolves an item the hub raised (`say` question, review, error,
//! stalled) as `by: user, answer: cancel`, routing nothing; a runtime's permission or question
//! is refused (`bad_request`: answer it or stop the thread), and so is the orchestrator
//! (`relay_forbidden`).
//! `report.write` writes the report of a worker's finished turn (default: its last) as the
//! caller (`written_by` `orchestrator` or `user:<client>`); `gave_up` / `needs_verification`
//! raise a review unless the hub already raised one for that turn.
//! `attention.list` returns open items (`attn_id, thread, thread_name, reason, summary,
//! options, call_id, note, raised_at, event_id, revision, first_revision`) ordered like the
//! attention stack; without `all` only the ones blocking a thread (permission, question).
//! An item's `revision` is the control gseq of its latest `attention.raised` (an open item
//! raised again, e.g. a permission re-sent with new input, is a new revision; `first_revision`
//! is the raise that opened it).
//! `attach.begin` makes its connection the attach lease: the hub stops the
//! thread's runtime (refusing with `conflict` while a turn or background tasks run unless
//! `force`, which interrupts the turn and ends the tasks), waits for it to exit, appends
//! `thread.attached` and returns the runtime's interactive command (`env` values are strings to
//! set or `null` to remove). The caller runs
//! it and then calls `attach.end` on the same connection; `attach.end` from any other
//! connection is a `conflict` and changes nothing. Right after starting the command the caller
//! reports its process with `attach.child` on the same connection (`pid`, `start_time` as the
//! hub's machine reads it: on Linux clock ticks after boot, `/proc/<pid>/stat` field 22; on
//! macOS seconds since the Unix epoch, `ps`'s `lstart`; the process must carry the grant's
//! `TWR_ATTACH` marker: `bad_request` otherwise). If the lease connection closes first the hub
//! detaches (`thread.detached` `ext.twr.reason: "connection_closed"`), unless a reported
//! process still runs: then the thread stays attached until that process exits (reason
//! `attach process exited`), also across a hub restart. `unsupported` when the runtime
//! has no interactive CLI or the thread has no session yet (or its remote box has no `ssh`);
//! `box_offline` while its box's runner is not connected. A change of the thread's mode, model
//! or effort while its runtime is being stopped refuses the attach (`conflict`, "<name> changed
//! while attaching; try again"): the attach recorded is ended again (`thread.detached` reason
//! `changed_while_attaching`), since the command was made with the old settings. On a remote
//! box the runtime is quiesced through the runner and `argv` is `ssh -t <boxes.NAME.ssh> --
//! <command>` (`box` set, `cwd` null, `env` empty; see `hub::runners`).
//! While attached, `thread.send` queues and nothing is dispatched.
//! A thread object is the `ThreadView` plus `status`, `pending` (queued prompts) and `live`,
//! and while attached with a reported process `attach_process` (`{pid, orphaned}`: `orphaned`
//! once its lease connection is gone and the hub waits for the process to exit).
//! `attention.respond` takes exactly one of `answer` and `option_index` (0-based index into the
//! item's `options`, so a client can answer without the option's full text). `revision` is
//! the revision of the item the client displayed: the answer is refused (`conflict`, "the
//! question changed; look again") once the item was raised again since, since it may ask
//! something else now. Without `revision` a user's answer is accepted only while the item was
//! never raised again (its `revision` is its `first_revision`); the orchestrator's is judged by
//! the relay guard (the user's message must postdate the item's current revision). Checked
//! under the lock that records the answer.
//! `blob.get` returns at most `len` bytes (default and maximum [`BLOB_CHUNK_MAX`]) from byte
//! `offset` (default 0); `encoding` is `utf8` when the chunk is valid UTF-8, else `base64`;
//! `eof` says whether the chunk reaches the end of the blob. `offset` past `size` is a
//! `bad_request`. [`twr_core::client::Client::blob_get`] assembles whole blobs.
//! `thread.new` gives a worker thread in a git repository its own worktree on branch
//! `twr/<name>` (from `base`, default `HEAD`) unless `no_worktree`; see `hub::git`.
//! `thread.park` stops the runtime like an attach (`conflict` while attached, already parked,
//! or while a turn or background tasks run unless `force`), appends `thread.parked`, and
//! removes a clean hub-managed worktree unless `keep` (a dirty one is kept: `kept_dirty`).
//! Prompts sent to a parked thread queue (`thread.send` says `parked: true`) until
//! `thread.resume`, which recreates a missing worktree at the same path, appends
//! `thread.resumed` and dispatches the queue on the resumed session.
//! `thread.set_model` with a `runtime` other than the thread's moves the thread to it
//! (`hub::switch`): the same thread, worktree and branch, a new session of the new runtime
//! from its next turn, whose prompt carries a handoff of what the previous session did. The
//! orchestrator may only name an agent runtime or a configured preset (as for `thread.new`).
//! `thread.delete` deletes nothing itself: it raises one delete card on the `hub` thread for
//! the threads it names, which only the user answers (`hub::delete`); `Delete` deletes them
//! one after another: stops the runtime, moves the worktree (also with uncommitted changes;
//! only its record when a park removed it already) and the `twr/<name>` branch to the trash
//! of the thread's box and records `thread.deleted`, after which the thread is gone from every
//! listing. `trash.list` lists the trash of every box (the orchestrator may call it);
//! `trash.empty` purges items now and `trash.restore` puts a thread back (`hub::trash`): users
//! only, not in `ORCHESTRATOR_METHODS`, and the hub refuses the orchestrator identity as
//! well. Items are purged `[trash] keep_days` after their deletion. `box.add` and
//! `box.install` change the hub's boxes (`hub::boxes`): users only too, refused to the
//! orchestrator and to agents alike.
//! `thread.send` succeeds once the prompt is queued (`queued` is its queue row id, and `ref`
//! too: the message's ref); if dispatching it then failed, `dispatch_error` says why and the
//! prompt stays queued. `say` returns the same `ref` when it queued the message (`send`,
//! `delegate`, `new_thread`); a short command applied at once (`command`) has none.
//! `message.withdraw` takes a message back by its ref (`hub::withdraw`): still queued, it is
//! removed (`prompt.withdrawn`); its turn running and not acted yet (no tool call, request or
//! change asked of the hub), the turn is withdrawn (`turn.withdrawn`: it has no authority left)
//! and interrupted; otherwise it is refused with a `reason`. Only a user may call it. Once the
//! user took back the orchestrator's turn's message, every change the orchestrator asks for in
//! that turn (`ORCHESTRATOR_CHANGES`) is `relay_forbidden`.
//! A malformed or oversized request line gets a `bad_request` error (id 0) and the connection
//! is closed.
//! `thread.events` and `control.read` return at most `limit` (default 1000, at most 10000)
//! items and at most [`PAGE_BYTES`] of them serialized (always at least one). `more` says
//! whether items follow; `next` is the `since_seq` / `since` that continues the read (the last
//! item's seq / gseq; `last_seq` is the same for `thread.events`). Clients page until `more`
//! is false.
//! `usage.get` returns the latest usage the hub knows of each runtime's account (`hub::usage`):
//! the plan windows that have not reset, `used_pct` the share used, `stale` once the data is
//! ten minutes old. Runtimes with no data are left out.
//! `context.read` / `context.set` read and replace a project's standing context
//! (`<repo>/.twr/context.md`; `project` is a configured project or the project of a worker
//! thread; see `hub::context`). `context.set` logs `context.updated` on the `hub` thread before
//! writing the file; content over 1 MiB is a `bad_request`.
//! `thread` params accept an id, name, alias or unique prefix. `since*` are exclusive (items
//! strictly after). Subscriptions deliver the backlog from SQLite and then live items, with no
//! gaps or duplicates; they end when the hub shuts down. A connection holds at most
//! [`SUBS_PER_CONNECTION`] at once and the hub serves at most [`SUBS_MAX`], of which agents'
//! connections hold at most [`AGENT_SUBS_MAX`]: one more is a `conflict`.

use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;
use tokio::task::JoinSet;
use twr_core::event::{Answer, Event, Grade, TurnFrom};
use twr_core::proto::{
    codes, read_frame, write_frame, Request, Response, RpcError, StreamKind, BLOB_CHUNK_MAX,
    PROTOCOL_VERSION,
};
use twr_runner::reap::{self, AncestryEnd, ProcessId};

use crate::hub::{
    wait_state, AgentPeer, AgentWhy, Caller, Hub, HubState, NewThread, PeerWalk, ReadTool,
    StartedKind, SwitchOptions, SUMMARY_WAIT_MAX,
};

/// Attach leases held by one connection: `(thread id, lease id)`.
type Leases = Vec<(String, u64)>;

/// Default and maximum page sizes for `thread.events` / `control.read`.
const DEFAULT_LIMIT: usize = 1000;
const MAX_LIMIT: usize = 10_000;

/// Most serialized bytes of events in one `thread.events` / `control.read` page, so a whole
/// response stays well under the client's `MAX_FRAME` (16 MiB) whatever the page size asked.
/// A page holds at least one event (a capped event is far smaller than this).
pub const PAGE_BYTES: usize = 8 * 1024 * 1024;

/// How long connections get to flush their subscriptions after the hub stops.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

/// Most subscriptions (`control.subscribe`, `thread.subscribe`) one connection holds at once:
/// twr's own clients hold one each. Each is a task of the hub's while it lasts.
pub const SUBS_PER_CONNECTION: usize = 16;
/// Most subscriptions the hub serves at once, every connection's together.
pub const SUBS_MAX: usize = 1024;
/// Most of [`SUBS_MAX`] the connections of threads' agents hold together, so that agents
/// opening connection after connection leave the user's clients room.
pub const AGENT_SUBS_MAX: usize = 256;

/// The subscriptions the hub serves ([`SUBS_MAX`], [`AGENT_SUBS_MAX`]).
#[derive(Default)]
struct SubCount {
    all: AtomicUsize,
    agents: AtomicUsize,
}

/// One subscription counted in [`SubCount`] while it lasts (its task holds it).
struct SubSlot {
    count: Arc<SubCount>,
    agent: bool,
}

impl SubCount {
    /// A slot for one more subscription, of a thread's agent's connection when `agent`; `None`
    /// when the hub serves as many as it takes.
    fn take(self: &Arc<Self>, agent: bool) -> Option<SubSlot> {
        if self.all.fetch_add(1, Ordering::SeqCst) >= SUBS_MAX {
            self.all.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        if agent && self.agents.fetch_add(1, Ordering::SeqCst) >= AGENT_SUBS_MAX {
            self.agents.fetch_sub(1, Ordering::SeqCst);
            self.all.fetch_sub(1, Ordering::SeqCst);
            return None;
        }
        Some(SubSlot {
            count: Arc::clone(self),
            agent,
        })
    }
}

impl Drop for SubSlot {
    fn drop(&mut self) {
        self.count.all.fetch_sub(1, Ordering::SeqCst);
        if self.agent {
            self.count.agents.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

/// A slot for one more subscription on a connection that holds `held` (its ended ones
/// reaped), as a thread's agent's when `agent`: refused (`conflict`) over
/// [`SUBS_PER_CONNECTION`], or when the hub serves as many as it takes.
fn sub_slot(count: &Arc<SubCount>, held: usize, agent: bool) -> Result<SubSlot, RpcError> {
    if held >= SUBS_PER_CONNECTION {
        return Err(RpcError::new(
            codes::CONFLICT,
            format!(
                "this connection holds {held} subscriptions already, the most one may: open \
                 another connection, or end one"
            ),
        ));
    }
    count.take(agent).ok_or_else(|| {
        RpcError::new(
            codes::CONFLICT,
            "the hub serves as many subscriptions as it takes; try again later",
        )
    })
}

/// Binds the hub socket: creates its directory (mode 0700) if missing, refuses to start if
/// another hub answers on it, and removes a stale socket file.
///
/// The socket is never connectable by other users, even in a shared directory and whatever
/// the umask: it is bound inside a fresh private (0700) directory next to `path`, set to mode
/// 0600 there, and only then linked into place (a link, not a rename, so a socket another hub
/// bound in the meantime is never replaced). Those steps go by path, so a directory in which
/// another user could rename that private directory, or the socket, is refused first
/// ([`twr_core::paths::check_socket_dir`]: another user's, or one every user may write to
/// without the sticky bit; one its group may write to is only logged).
pub async fn bind(path: &Path) -> anyhow::Result<UnixListener> {
    twr_core::paths::check_socket_path(path)?;
    let dir = match path.parent().filter(|d| !d.as_os_str().is_empty()) {
        Some(d) => d.to_path_buf(),
        None => PathBuf::from("."),
    };
    if !dir.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .with_context(|| format!("creating {}", dir.display()))?;
    }
    twr_core::paths::check_socket_dir(&dir)?;
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if UnixStream::connect(path).await.is_ok() {
            bail!("a hub is already listening on {}", path.display());
        }
        if !meta.file_type().is_socket() {
            bail!("{} exists and is not a socket", path.display());
        }
        tracing::info!(socket = %path.display(), "removing stale socket");
        std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
    }
    let private = PrivateDir::create(&dir)?;
    let tmp = private.0.join("s");
    if tmp.as_os_str().len() > twr_core::paths::MAX_SOCKET_PATH {
        bail!(
            "the hub socket path {} is too long: the hub first binds it at {} ({} bytes; Unix \
             socket paths can be at most {} bytes). Pass a shorter one with --socket, or set \
             TWR_HOME to a shorter directory",
            path.display(),
            tmp.display(),
            tmp.as_os_str().len(),
            twr_core::paths::MAX_SOCKET_PATH
        );
    }
    let listener =
        UnixListener::bind(&tmp).with_context(|| format!("binding {}", tmp.display()))?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    match std::fs::hard_link(&tmp, path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            bail!("a hub is already listening on {}", path.display());
        }
        Err(e) => {
            return Err(e).with_context(|| format!("binding {}", path.display()));
        }
    }
    let _ = std::fs::remove_file(&tmp);
    Ok(listener)
}

/// A fresh mode-0700 directory, removed (with whatever is left in it) when dropped.
struct PrivateDir(PathBuf);

impl PrivateDir {
    fn create(parent: &Path) -> anyhow::Result<PrivateDir> {
        static N: AtomicU64 = AtomicU64::new(0);
        let pid = std::process::id();
        let mut attempts = 0;
        loop {
            // Short, so the socket path inside stays about as long as the final one.
            let n = N.fetch_add(1, Ordering::Relaxed);
            let d = parent.join(format!(".t{pid}.{n}"));
            match std::fs::DirBuilder::new().mode(0o700).create(&d) {
                Ok(()) => {
                    // The umask can only have removed bits; make sure group/other have none.
                    std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700))?;
                    return Ok(PrivateDir(d));
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempts < 100 => {
                    attempts += 1;
                }
                Err(e) => return Err(e).with_context(|| format!("creating {}", d.display())),
            }
        }
    }
}

impl Drop for PrivateDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.0.join("s"));
        let _ = std::fs::remove_dir(&self.0);
    }
}

/// The socket file this server bound, identified by (device, inode) so that shutdown never
/// removes a socket a newer hub has since bound at the same path.
struct SocketFile {
    path: PathBuf,
    id: Option<(u64, u64)>,
}

impl SocketFile {
    fn new(path: PathBuf) -> SocketFile {
        let id = std::fs::symlink_metadata(&path)
            .ok()
            .map(|m| (m.dev(), m.ino()));
        SocketFile { path, id }
    }

    /// Removes the socket file if it is still the one this server bound.
    fn remove(&mut self) {
        let Some(id) = self.id.take() else {
            return;
        };
        let current = std::fs::symlink_metadata(&self.path)
            .ok()
            .map(|m| (m.dev(), m.ino()));
        if current == Some(id) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

impl Drop for SocketFile {
    fn drop(&mut self) {
        self.remove();
    }
}

/// Serves `hub` on an already bound `listener` until shutdown (a `hub.shutdown` call,
/// [`Hub::request_shutdown`], or SIGINT/SIGTERM when `signals` is set). On the way out it
/// stops accepting and removes the socket file (if it is still ours), then runs
/// [`Hub::shutdown`] and lets subscriptions drain, and then waits for the repository changes
/// of the hub's own box still running ([`Hub::box_changes_done`], at most
/// [`twr_core::git::GIT_CHANGE_TIMEOUT`]): a connection aborted at the end of the drain does
/// not stop the change its request started, and nothing it holds is let go before it ends. Fails when the hub stopped because of a
/// fatal error ([`Hub::fatal_error`]), so the process exits non-zero.
pub async fn serve(
    hub: Hub,
    listener: UnixListener,
    socket: PathBuf,
    signals: bool,
) -> anyhow::Result<()> {
    let mut socket = SocketFile::new(socket);
    let mut state = hub.state();
    let mut conns = JoinSet::new();
    let mut n = 0u64;
    let signal = wait_for_signal(signals);
    let mut signalled = false;
    let sub_count = Arc::new(SubCount::default());
    tokio::pin!(signal);
    loop {
        tokio::select! {
            r = listener.accept() => match r {
                Ok((stream, _)) => {
                    n += 1;
                    conns.spawn(connection(
                        hub.clone(),
                        stream,
                        format!("cli-{n}"),
                        Arc::clone(&sub_count),
                    ));
                }
                Err(e) => tracing::warn!(error = %e, "accept failed"),
            },
            _ = &mut signal, if !signalled => {
                signalled = true;
                tracing::info!("signal received; shutting down");
                hub.request_shutdown();
            }
            _ = wait_state(&mut state, |s| *s != HubState::Running) => break,
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
        }
    }
    drop(listener);
    // Right away: a client that finds no socket may start a new hub while this one drains.
    socket.remove();
    hub.shutdown().await;
    let drained = tokio::time::timeout(DRAIN_TIMEOUT, async {
        while conns.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        conns.abort_all();
    }
    // A repository change of the hub's own box (a trash move, a restore, a removal) whose
    // request went away with its connection runs on to its end: the hub exits only once it
    // is over, at most for as long as a git change may run before it is killed.
    if !hub.box_changes_done(Duration::ZERO).await {
        tracing::info!("waiting for the repository changes in progress before stopping");
        if !hub
            .box_changes_done(twr_core::git::GIT_CHANGE_TIMEOUT)
            .await
        {
            tracing::warn!(
                waited_s = twr_core::git::GIT_CHANGE_TIMEOUT.as_secs(),
                "stopping with a repository change still running; what it was changing is \
                 left part way (an incomplete trash item keeps what it moved)"
            );
        }
    }
    if let Some(e) = hub.fatal_error() {
        bail!("the hub stopped after a fatal error: {e}");
    }
    tracing::info!("hub stopped");
    Ok(())
}

async fn wait_for_signal(enabled: bool) {
    if !enabled {
        return std::future::pending().await;
    }
    use tokio::signal::unix::{signal, SignalKind};
    let (Ok(mut term), Ok(mut int)) = (
        signal(SignalKind::terminate()),
        signal(SignalKind::interrupt()),
    ) else {
        tracing::warn!("cannot install signal handlers");
        return std::future::pending().await;
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

type Writer = Arc<Mutex<OwnedWriteHalf>>;

async fn send<T: Serialize>(w: &Writer, msg: &T) -> std::io::Result<()> {
    let mut w = w.lock().await;
    write_frame(&mut *w, msg).await
}

/// Who a connection is: `user:<client>` unless its `hello` presented the current
/// orchestrator token, or its peer is a thread's agent.
struct Conn {
    client: String,
    /// The orchestrator token this connection presented; checked again on every request, with
    /// the peer's ancestry walked again ([`still_orchestrator`]), so a connection from an
    /// earlier orchestrator process loses its identity.
    token: Option<String>,
    /// The thread whose agent the connection's peer is, found when it connected
    /// ([`Hub::agent_peer`]; a connection's peer does not change). Only ever narrowed later,
    /// or replaced by a walk that shows the current orchestrator runtime ([`orchestrator_peer`]);
    /// never trusted as proof for the orchestrator token.
    agent: Option<AgentPeer>,
    /// The connection's peer process, when its pid could be read.
    peer: Option<u32>,
    /// The peer as it was read when the connection opened (pid and start time; `None` when it
    /// could not be read). The orchestrator token is only accepted, and only keeps working,
    /// while `peer` still names this process: once it is gone, its pid may
    /// name another process, and the connection may be in the hands of a process that
    /// inherited it.
    opened_by: Option<ProcessId>,
    /// This connection, among the hub's (the notification hook's `notify.looking` leases).
    id: u64,
}

/// The next connection's [`Conn::id`].
static NEXT_CONN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl Conn {
    /// The agent this connection is restricted as: its peer is a thread's agent and it did not
    /// present the orchestrator token (only the orchestrator's own agent may, [`hello`]).
    fn restricted(&self) -> Option<&AgentPeer> {
        self.agent.as_ref().filter(|_| self.token.is_none())
    }

    /// The identity `hello` and `hub.info` report: `agent:<thread name>` for a restricted
    /// connection, else the caller's source.
    fn identity(&self, caller: &Caller) -> String {
        match self.restricted() {
            Some(a) => format!("agent:{}", a.thread_name),
            None => caller.source().to_string(),
        }
    }

    fn caller(&self, hub: &Hub) -> Result<Caller, RpcError> {
        match &self.token {
            None => Ok(Caller::user(self.client.clone())),
            Some(t) if hub.orchestrator_token_is(t) => Ok(Caller::Orchestrator),
            Some(_) => Err(RpcError::new(
                codes::UNAUTHORIZED,
                "the orchestrator token of this connection is no longer valid (the orchestrator \
                 was restarted); reconnect",
            )),
        }
    }
}

/// Every method the server dispatches (a test checks it against the dispatch's arms): what
/// [`AGENT_METHODS`] and [`ORCHESTRATOR_METHODS`] are subsets of, and what the boundary tests
/// try from an agent's and the orchestrator's connections.
pub const METHODS: &[&str] = &[
    "hello",
    "say",
    "hub.info",
    "hub.shutdown",
    "config.show",
    "box.add",
    "box.install",
    "notify.looking",
    "usage.get",
    "usage.threads",
    "thread.new",
    "thread.spawn",
    "thread.spawn_status",
    "thread.list",
    "thread.activity",
    "thread.get",
    "thread.send",
    "thread.send_cancel",
    "thread.stop",
    "thread.set_mode",
    "thread.set_model",
    "thread.rename",
    "thread.delete",
    "thread.park",
    "thread.resume",
    "thread.trim",
    "thread.events",
    "thread.transcript",
    "thread.subscribe",
    "message.withdraw",
    "trash.list",
    "trash.empty",
    "trash.restore",
    "orchestrator.show",
    "orchestrator.reset",
    "orchestrator.use",
    "orchestrator.watch",
    "orchestrator.unwatch",
    "orchestrator.watches",
    "attention.list",
    "attention.respond",
    "attention.dismiss",
    "attention.annotate",
    "report.write",
    "attach.begin",
    "attach.child",
    "attach.end",
    "control.read",
    "control.subscribe",
    "blob.get",
    "context.read",
    "context.set",
    "project.files",
    "project.read",
    "project.search",
    "job.start",
    "job.cancel",
    "job.list",
    "job.log",
];

/// Methods the orchestrator's connection may call (it may not shut the hub down, attach, or
/// `say` on the user's behalf).
pub const ORCHESTRATOR_METHODS: &[&str] = &[
    "hello",
    "hub.info",
    "config.show",
    "thread.new",
    "thread.spawn",
    "thread.spawn_status",
    "thread.list",
    "thread.activity",
    "usage.get",
    "usage.threads",
    "thread.get",
    "thread.send",
    "thread.send_cancel",
    "thread.stop",
    "thread.set_mode",
    "thread.set_model",
    "thread.rename",
    "thread.delete",
    "trash.list",
    "thread.park",
    "thread.resume",
    "thread.events",
    "thread.transcript",
    "thread.subscribe",
    "attention.list",
    "attention.respond",
    "attention.annotate",
    "control.read",
    "control.subscribe",
    "blob.get",
    "report.write",
    "context.read",
    "context.set",
    "project.files",
    "project.read",
    "project.search",
    "orchestrator.watch",
    "orchestrator.unwatch",
    "orchestrator.watches",
];

/// The [`ORCHESTRATOR_METHODS`] that change something: each one marks the orchestrator's turn
/// as having acted, and is refused once the user took back that turn's message
/// ([`Hub::orchestrator_acts`]).
const ORCHESTRATOR_CHANGES: &[&str] = &[
    "thread.new",
    "thread.spawn",
    "thread.send",
    "thread.send_cancel",
    "thread.stop",
    "thread.set_mode",
    "thread.set_model",
    "thread.rename",
    "thread.delete",
    "thread.park",
    "thread.resume",
    "attention.respond",
    "attention.annotate",
    "report.write",
    "context.set",
    "orchestrator.watch",
    "orchestrator.unwatch",
];

/// Methods a thread's agent may call: reads, and starting and cancelling its own thread's jobs
/// (from a process twr started for the thread; starting only while the thread's mode may run
/// commands; [`agent_thread`], `hub::jobs`). It may not answer, change a mode or model, send or route a
/// prompt, stop, park, resume, create or delete threads, attach, write reports, notes or
/// context, touch the trash or the orchestrator, or stop the hub.
pub const AGENT_METHODS: &[&str] = &[
    "hello",
    // Their own thread's only (`hub::jobs`; checked in `call`).
    "job.start",
    "job.cancel",
    // Any thread's, like transcripts.
    "job.list",
    "job.log",
    "hub.info",
    "thread.list",
    "thread.activity",
    "usage.get",
    "usage.threads",
    "thread.get",
    "thread.events",
    "thread.transcript",
    "thread.subscribe",
    "attention.list",
    "control.read",
    "control.subscribe",
    "blob.get",
    "context.read",
    "trash.list",
    "orchestrator.show",
];

/// The refusal of a request from a thread's agent.
fn agent_refused(agent: &AgentPeer) -> RpcError {
    RpcError::new(
        codes::UNAUTHORIZED,
        format!(
            "this request comes from {}: agents cannot answer, change modes, delete, empty or \
             restore the trash, or send prompts — only you can, from your own terminal or the \
             TUI",
            agent.detail()
        ),
    )
}

/// Which process a connection's peer is, as its credentials say ([`peer_pid`]).
#[derive(Debug, PartialEq, Eq)]
enum PeerPid {
    /// The hub's own process (its in-process clients: tests).
    Hub(u32),
    /// Another process this hub sees.
    Other(u32),
    /// None this hub can identify, and why: the credentials could not be read, or name no pid
    /// it can see (none, or 0: a process of another pid namespace).
    Unknown(String),
}

/// The pid of a connection's peer from its credentials' `cred` (`peer_cred()`'s pid, or the
/// error reading them), against the hub's own `hub_pid`.
fn peer_pid(cred: std::io::Result<Option<i32>>, hub_pid: u32) -> PeerPid {
    match cred {
        Err(e) => PeerPid::Unknown(format!("its credentials cannot be read: {e}")),
        Ok(pid) => match pid.and_then(|p| u32::try_from(p).ok()).filter(|p| *p > 0) {
            Some(pid) if pid == hub_pid => PeerPid::Hub(pid),
            Some(pid) => PeerPid::Other(pid),
            None => PeerPid::Unknown(format!("it has no pid this hub can see ({pid:?})")),
        },
    }
}

/// The peer of `stream`, which process it is and the thread whose agent it is, if any.
/// A peer whose pid cannot be read is an agent of no thread ([`AgentPeer::unidentified`],
/// logged): restricted to [`AGENT_METHODS`], and it starts no job ([`agent_thread`]).
async fn agent_of(hub: &Hub, stream: &UnixStream) -> (Option<u32>, PeerWalk) {
    let cred = stream.peer_cred().map(|c| c.pid());
    match peer_pid(cred, std::process::id()) {
        // The hub's own process (in-process clients: tests) is no agent of itself, wherever
        // it descends from: twr never has the hub connect to its own socket otherwise.
        PeerPid::Hub(pid) => {
            let mut walk = hub.agent_peer(pid).await;
            walk.agent = None;
            (Some(pid), walk)
        }
        PeerPid::Other(pid) => (Some(pid), hub.agent_peer(pid).await),
        PeerPid::Unknown(why) => {
            tracing::warn!(
                "a connection's peer is no process this hub can identify ({why}); it counts as \
                 an agent of no thread"
            );
            (
                None,
                PeerWalk {
                    process: None,
                    agent: Some(AgentPeer::unidentified()),
                },
            )
        }
    }
}

async fn connection(
    hub: Hub,
    stream: UnixStream,
    default_client: String,
    sub_count: Arc<SubCount>,
) {
    let (peer, PeerWalk { process, agent }) = agent_of(&hub, &stream).await;
    if let Some(a) = &agent {
        tracing::info!(thread = ?a.thread, peer = a.peer, why = ?a.why, "a connection from a thread's agent: it reads, and starts and cancels its own thread's jobs only");
    }
    let (r, w) = stream.into_split();
    let mut reader = BufReader::new(r);
    let writer: Writer = Arc::new(Mutex::new(w));
    let mut conn = Conn {
        client: default_client,
        token: None,
        agent,
        peer,
        opened_by: process,
        id: NEXT_CONN.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
    };
    let mut subs = JoinSet::new();
    let mut state = hub.state();
    let mut leases: Leases = Vec::new();
    loop {
        let frame = tokio::select! {
            f = read_frame::<_, Request>(&mut reader) => f,
            _ = wait_state(&mut state, |s| *s == HubState::Stopped) => {
                // Subscriptions end on their own once they have flushed. Attach leases are
                // left for the next hub to close (`thread.detached` reason `hub_restart`).
                while subs.join_next().await.is_some() {}
                return;
            }
        };
        let req = match frame {
            Ok(Some(req)) => req,
            Ok(None) => break,
            Err(e) if e.kind() == std::io::ErrorKind::InvalidData => {
                // Malformed or oversized frame: say why, then hang up (the peer is not speaking
                // the protocol, and an oversized line may be an attempt to exhaust memory).
                let resp = Response::err(0, RpcError::new(codes::BAD_REQUEST, e.to_string()));
                let _ = send(&writer, &resp).await;
                break;
            }
            Err(_) => break,
        };
        let id = req.id;
        // A thread's agent reads, and starts and cancels its own thread's jobs (`call`).
        if let Some(agent) = conn.restricted() {
            if !AGENT_METHODS.contains(&req.method.as_str()) {
                tracing::warn!(thread = ?agent.thread, peer = agent.peer, method = %req.method, "refused a request from a thread's agent");
                let e = agent_refused(agent);
                if send(&writer, &Response::err(id, e)).await.is_err() {
                    break;
                }
                continue;
            }
        }
        // The orchestrator as it is when this request comes in, before its peer is checked
        // again (it may change meanwhile): a change it asks for is bound to it (`hub::withdraw`).
        let received = matches!(conn.caller(&hub), Ok(Caller::Orchestrator))
            .then(|| hub.orchestrator_call())
            .flatten();
        // The caller's identity for this request (`hello` sets it). The orchestrator's is
        // proven again: its peer still descends from the runtime the token belongs to.
        let caller = match conn.caller(&hub) {
            Ok(Caller::Orchestrator) if req.method != "hello" => {
                match still_orchestrator(&hub, &mut conn).await {
                    Ok(()) => Caller::Orchestrator,
                    Err(e) => {
                        if send(&writer, &Response::err(id, e)).await.is_err() {
                            break;
                        }
                        continue;
                    }
                }
            }
            Ok(c) => c,
            Err(e) if req.method != "hello" => {
                if send(&writer, &Response::err(id, e)).await.is_err() {
                    break;
                }
                continue;
            }
            Err(_) => Caller::user(conn.client.clone()),
        };
        if caller.is_orchestrator() && !ORCHESTRATOR_METHODS.contains(&req.method.as_str()) {
            let e = RpcError::new(
                codes::UNAUTHORIZED,
                format!("the orchestrator may not call {}", req.method),
            );
            if send(&writer, &Response::err(id, e)).await.is_err() {
                break;
            }
            continue;
        }
        // A change the orchestrator asks for: refused once the user took back its turn's
        // message, else that turn has acted (`hub::withdraw`).
        if caller.is_orchestrator() && ORCHESTRATOR_CHANGES.contains(&req.method.as_str()) {
            if let Err(e) = hub.orchestrator_acts(&req.method, received.as_ref()) {
                if send(&writer, &Response::err(id, e)).await.is_err() {
                    break;
                }
                continue;
            }
        }
        let client = conn.client.clone();
        // Subscriptions that ended (their stream ended or failed, or their client went) no
        // longer count against this connection's.
        while subs.try_join_next().is_some() {}
        let agent = conn.restricted().is_some();
        match req.method.as_str() {
            "control.subscribe" => {
                match sub_slot(&sub_count, subs.len(), agent)
                    .and_then(|slot| Ok((slot, control_subscribe(&hub, req.params)?)))
                {
                    Ok((slot, (ack, sub))) => {
                        subs.spawn(pump_control(writer.clone(), id, ack, sub, slot));
                    }
                    Err(e) => {
                        let _ = send(&writer, &Response::err(id, e)).await;
                    }
                }
            }
            "thread.subscribe" => {
                match sub_slot(&sub_count, subs.len(), agent)
                    .and_then(|slot| Ok((slot, thread_subscribe(&hub, req.params)?)))
                {
                    Ok((slot, (ack, sub))) => {
                        subs.spawn(pump_thread(writer.clone(), id, ack, sub, slot));
                    }
                    Err(e) => {
                        let _ = send(&writer, &Response::err(id, e)).await;
                    }
                }
            }
            "attach.begin" => {
                let result = attach_begin(&hub, &client, req.params, &mut leases).await;
                let resp = match result {
                    Ok(v) => Response::ok(id, v),
                    Err(e) => Response::err(id, e),
                };
                if send(&writer, &resp).await.is_err() {
                    break;
                }
            }
            "attach.child" => {
                let result = attach_child(&hub, req.params, &leases).await;
                let resp = match result {
                    Ok(v) => Response::ok(id, v),
                    Err(e) => Response::err(id, e),
                };
                if send(&writer, &resp).await.is_err() {
                    break;
                }
            }
            "attach.end" => {
                let result = attach_end(&hub, &client, req.params, &mut leases);
                let resp = match result {
                    Ok(v) => Response::ok(id, v),
                    Err(e) => Response::err(id, e),
                };
                if send(&writer, &resp).await.is_err() {
                    break;
                }
            }
            "thread.new" | "thread.park" | "thread.resume" | "thread.delete" | "thread.trim"
            | "usage.threads" | "context.read" | "context.set" | "orchestrator.use"
            | "orchestrator.reset" | "trash.list" | "trash.empty" | "trash.restore"
            | "project.files" | "project.read" | "project.search" | "box.add" | "box.install" => {
                let result = call_async(&hub, &caller, &req.method, req.params).await;
                let resp = match result {
                    Ok(v) => Response::ok(id, v),
                    Err(e) => Response::err(id, e),
                };
                if send(&writer, &resp).await.is_err() {
                    break;
                }
            }
            "job.start" | "job.cancel" => {
                let result = job_request(&hub, &conn, &caller, &req.method, req.params).await;
                let resp = match result {
                    Ok(v) => Response::ok(id, v),
                    Err(e) => Response::err(id, e),
                };
                if send(&writer, &resp).await.is_err() {
                    break;
                }
            }
            "hello" => {
                let result = hello(&hub, &mut conn, req.params).await;
                let resp = match result {
                    Ok(v) => Response::ok(id, v),
                    Err(e) => Response::err(id, e),
                };
                if send(&writer, &resp).await.is_err() {
                    break;
                }
            }
            "say" => {
                let result = say(&hub, &caller, req.params).await;
                let resp = match result {
                    Ok(v) => Response::ok(id, v),
                    Err(e) => Response::err(id, e),
                };
                if send(&writer, &resp).await.is_err() {
                    break;
                }
            }
            method => {
                let result = call(&hub, &mut conn, &caller, method, req.params);
                let shutdown = method == "hub.shutdown" && result.is_ok();
                let resp = match result {
                    Ok(v) => Response::ok(id, v),
                    Err(e) => Response::err(id, e),
                };
                let sent = send(&writer, &resp).await;
                if shutdown {
                    hub.request_shutdown();
                }
                if sent.is_err() {
                    break;
                }
            }
        }
    }
    subs.abort_all();
    // A TUI that said the user looks at it is gone: it says nothing any more.
    hub.set_looking(conn.id, false);
    // The lease connection is gone: whoever held the attach is no longer there. The attach ends
    // now, or once the interactive process it reported has exited.
    for (thread, lease) in leases {
        if let Err(e) = hub.attach_connection_closed(&thread, &conn.client, lease) {
            tracing::warn!(%thread, error = %e, "detaching after the lease connection closed failed");
        }
    }
}

/// `orchestrator.use` parameters.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct UseOrchestratorParams {
    runtime: String,
    #[serde(default)]
    model: Option<String>,
    /// Ask the previous orchestrator for a handoff summary (default true).
    #[serde(default = "yes")]
    summary: bool,
    /// How long to wait for a busy previous orchestrator (default 60, at most 600).
    #[serde(default)]
    wait_secs: Option<u64>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyParams {}

fn yes() -> bool {
    true
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AttachParams {
    thread: String,
    #[serde(default)]
    force: bool,
}

async fn attach_begin(
    hub: &Hub,
    client: &str,
    p: Value,
    leases: &mut Leases,
) -> Result<Value, RpcError> {
    let p: AttachParams = params(p)?;
    let grant = hub.attach_begin(&p.thread, p.force, client).await?;
    leases.push((grant.thread.clone(), grant.lease));
    serde_json::to_value(&grant).map_err(internal)
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AttachChildParams {
    thread: String,
    pid: u32,
    start_time: u64,
}

/// `attach.child` records the interactive process of an attach this connection holds.
async fn attach_child(hub: &Hub, p: Value, leases: &Leases) -> Result<Value, RpcError> {
    let p: AttachChildParams = params(p)?;
    let thread = hub.resolve(&p.thread)?;
    let Some(lease) = leases.iter().find(|(t, _)| *t == thread).map(|(_, l)| *l) else {
        return Err(RpcError::new(
            codes::CONFLICT,
            format!("this connection holds no attach lease on {}", p.thread),
        ));
    };
    hub.attach_child(&thread, lease, p.pid, p.start_time)
        .await?;
    Ok(json!({ "thread": thread }))
}

/// `attach.end` ends only an attach this connection holds (the lease its `attach.begin` took on
/// the thread). Anything else is a `conflict` and changes nothing: ending another client's
/// attach would hand the session back to the headless runtime while that client's interactive
/// CLI still runs on it.
fn attach_end(hub: &Hub, client: &str, p: Value, leases: &mut Leases) -> Result<Value, RpcError> {
    let p: ThreadRef = params(p)?;
    let thread = hub.resolve(&p.thread)?;
    let Some(lease) = leases.iter().find(|(t, _)| *t == thread).map(|(_, l)| *l) else {
        return Err(RpcError::new(
            codes::CONFLICT,
            format!(
                "this connection holds no attach lease on {} (only the connection that attached \
                 can end the attach)",
                p.thread
            ),
        ));
    };
    let ended = hub.attach_end(&thread, client, lease, None);
    // The lease is over for this connection once the hub says so (ended, or not attached under
    // it any more). On an error (still starting, or `thread.detached` could not be recorded)
    // the connection keeps it: `attach.end` can be repeated, and closing the connection
    // detaches.
    if ended.is_ok() {
        leases.retain(|(t, l)| !(*t == thread && *l == lease));
    }
    if !ended? {
        return Err(RpcError::new(
            codes::CONFLICT,
            format!(
                "{} is no longer attached under this connection's lease",
                p.thread
            ),
        ));
    }
    Ok(json!({ "thread": thread, "detached": true }))
}

#[derive(Serialize)]
struct Item<'a> {
    stream: StreamKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    gseq: Option<u64>,
    event: &'a Event,
}

/// Serves control subscription `sub` (request `id`, acknowledged with `ack`) until it ends,
/// counted in the hub's subscriptions (`_slot`) meanwhile.
async fn pump_control(
    w: Writer,
    id: u64,
    ack: Value,
    mut sub: crate::hub::ControlSubscription,
    _slot: SubSlot,
) {
    if send(&w, &Response::ok(id, ack)).await.is_err() {
        return;
    }
    loop {
        match sub.next().await {
            Ok(Some((gseq, ev))) => {
                let item = Item {
                    stream: StreamKind::Control,
                    gseq: Some(gseq),
                    event: &ev,
                };
                if send(&w, &item).await.is_err() {
                    return;
                }
            }
            Ok(None) => break,
            Err(e) => {
                let _ = send(
                    &w,
                    &Response::err(id, RpcError::new(codes::INTERNAL, e.to_string())),
                )
                .await;
                break;
            }
        }
    }
    // End of stream: close our side so the client sees EOF.
    let _ = w.lock().await.shutdown().await;
}

/// Serves thread subscription `sub` like [`pump_control`].
async fn pump_thread(
    w: Writer,
    id: u64,
    ack: Value,
    mut sub: crate::hub::ThreadSubscription,
    _slot: SubSlot,
) {
    if send(&w, &Response::ok(id, ack)).await.is_err() {
        return;
    }
    loop {
        match sub.next().await {
            Ok(Some(ev)) => {
                let item = Item {
                    stream: StreamKind::Thread,
                    gseq: None,
                    event: &ev,
                };
                if send(&w, &item).await.is_err() {
                    return;
                }
            }
            Ok(None) => break,
            Err(e) => {
                let _ = send(
                    &w,
                    &Response::err(id, RpcError::new(codes::INTERNAL, e.to_string())),
                )
                .await;
                break;
            }
        }
    }
    let _ = w.lock().await.shutdown().await;
}

fn params<T: DeserializeOwned + Default>(v: Value) -> Result<T, RpcError> {
    if v.is_null() {
        return Ok(T::default());
    }
    serde_json::from_value(v).map_err(|e| RpcError::new(codes::BAD_REQUEST, e.to_string()))
}

/// The thread of this hub whose agent `agent` is: jobs are for it alone. Only a process twr
/// started for the thread, or one that descends from it (its runtime, an attach's CLI), proves
/// which thread it is: a marker in its environment alone (a detached process, which anyone
/// can set) does not, and an agent of no thread of this hub (another workspace's, one twr
/// cannot trace) starts none.
fn agent_thread(hub: &Hub, agent: &crate::hub::AgentPeer) -> Result<String, RpcError> {
    if !matches!(agent.why, AgentWhy::Started { .. }) {
        return Err(RpcError::new(
            codes::RELAY_FORBIDDEN,
            format!(
                "{}: jobs are started and cancelled by a thread's runtime or attached CLI (or \
                 you), from a process twr started",
                agent.summary()
            ),
        ));
    }
    agent
        .thread
        .as_deref()
        .and_then(|t| hub.thread_view(t).ok())
        .map(|v| v.id)
        .ok_or_else(|| {
            RpcError::new(
                codes::RELAY_FORBIDDEN,
                "only a thread of this hub may start jobs for itself",
            )
        })
}

/// `job.start` and `job.cancel` (`hub::jobs`). The user starts a job for any thread (`thread`
/// names it) and cancels any. A thread's agent starts and cancels its own thread's only, and
/// only while its connection's peer, walked again now ([`job_opener`]), still is, or descends
/// from, a process twr started for that thread: what it found when the connection opened is
/// not kept as that proof (the peer may have left its runtime since, or the runtime or attach
/// may have ended).
async fn job_request(
    hub: &Hub,
    conn: &Conn,
    caller: &Caller,
    method: &str,
    p: Value,
) -> Result<Value, RpcError> {
    let opener = match conn.restricted() {
        Some(agent) => Some(job_opener(hub, conn, agent).await?),
        None => None,
    };
    match method {
        "job.start" => {
            let req: crate::hub::JobStart = params(p)?;
            match opener {
                Some((own, by)) => {
                    if let Some(t) = &req.thread {
                        if hub.resolve(t)? != own {
                            return Err(RpcError::new(
                                codes::RELAY_FORBIDDEN,
                                "a thread's agent starts jobs for its own thread only",
                            ));
                        }
                    }
                    hub.start_agent_job(&own, req, caller.source(), by)
                }
                None => {
                    let thread = req.thread.clone().ok_or_else(|| {
                        RpcError::new(codes::BAD_REQUEST, "name the thread the job is for")
                    })?;
                    hub.start_job(&thread, req, caller.source())
                }
            }
        }
        "job.cancel" => {
            #[derive(Default, Deserialize)]
            #[serde(deny_unknown_fields)]
            struct JobCancelParams {
                job: String,
            }
            let p: JobCancelParams = params(p)?;
            match opener {
                Some((own, by)) => hub.cancel_agent_job(&p.job, &own, by),
                None => hub.cancel_job(&p.job, None),
            }
        }
        other => Err(RpcError::new(
            codes::METHOD_NOT_FOUND,
            format!("unknown method {other:?}"),
        )),
    }
}

/// The thread whose jobs a thread's agent's connection (`agent`, as it was classified when the
/// connection opened) starts and cancels, and which of its processes asks: only while the
/// peer's pid still names the process that opened the connection ([`Conn::opened_by`]) and its
/// ancestry, walked again now and confirmed ([`Hub::confirm_descent`]), still reaches a
/// process twr started for that thread (its runtime, or the CLI attached to it, live; checked
/// once more under the hub's lock when the request is applied). Else `relay_forbidden`; the
/// connection still reads.
async fn job_opener(
    hub: &Hub,
    conn: &Conn,
    agent: &AgentPeer,
) -> Result<(String, crate::hub::AgentJobBy), RpcError> {
    let own = agent_thread(hub, agent)?;
    let refused = |why: &str| {
        RpcError::new(
            codes::RELAY_FORBIDDEN,
            format!(
                "{}: {why}, so it starts and cancels no job (it still reads)",
                agent.summary()
            ),
        )
    };
    let Some(peer) = conn.peer else {
        return Err(refused(
            "the process that opened this connection is not known",
        ));
    };
    let PeerWalk {
        process,
        agent: now,
    } = hub.agent_peer(peer).await;
    if conn.opened_by.is_none() || process != conn.opened_by {
        tracing::warn!(peer, opened_by = ?conn.opened_by, now = ?process, "a job request from a connection whose opener is gone refused");
        return Err(refused("the process that opened this connection is gone"));
    }
    match now {
        Some(AgentPeer {
            thread: Some(t),
            why:
                AgentWhy::Started {
                    pid,
                    start_time,
                    kind,
                    ..
                },
            ..
        }) if t == own => {
            // The walk read each process once: one that exited meanwhile may still read as
            // its old self (a zombie) while its child, the peer, has left. Walked again and
            // confirmed link by link.
            let started = ProcessId { pid, start_time };
            if !hub.confirm_descent(peer, started).await {
                tracing::warn!(peer, thread = %own, "a job request from a process whose descent from its thread's runtime or attach did not hold refused");
                return Err(refused(&format!(
                    "it no longer runs under the runtime or attached CLI twr started for {} \
                     (they ended, or it left them)",
                    agent.thread_name
                )));
            }
            let by = match kind {
                StartedKind::Runtime => crate::hub::AgentJobBy::Runtime(started),
                StartedKind::Attach => crate::hub::AgentJobBy::Attach(started),
            };
            Ok((own, by))
        }
        now => {
            tracing::warn!(peer, thread = %own, now = ?now, "a job request from a process that no longer runs under its thread's runtime or attach refused");
            Err(refused(&format!(
                "it no longer runs under the runtime or attached CLI twr started for {} \
                 (they ended, or it left them)",
                agent.thread_name
            )))
        }
    }
}

fn internal(e: impl std::fmt::Display) -> RpcError {
    RpcError::new(codes::INTERNAL, e.to_string())
}

fn limit(l: Option<usize>) -> usize {
    l.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

/// The first rows of `rows` that fit a `thread.events` / `control.read` page: at most `limit`,
/// and at most [`PAGE_BYTES`] serialized (but always at least one), as JSON: the page holds
/// `rows[..len]`. The flag says whether rows were left out (the page is not the end).
fn page<'a, R, T: Serialize>(
    rows: &'a [R],
    limit: usize,
    item: impl Fn(&'a R) -> T,
) -> Result<(Vec<Value>, bool), RpcError> {
    let mut out = Vec::new();
    let mut bytes = 0;
    for row in rows.iter().take(limit) {
        let it = item(row);
        // The item's size in the response (plus its separating comma).
        let size = serde_json::to_vec(&it).map_err(internal)?.len() + 1;
        if !out.is_empty() && bytes + size > PAGE_BYTES {
            return Ok((out, true));
        }
        bytes += size;
        out.push(serde_json::to_value(&it).map_err(internal)?);
    }
    let more = rows.len() > out.len();
    Ok((out, more))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ThreadRef {
    thread: String,
}

/// `trash.empty`: items, or all of them; with `dry_run`, only which.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrashEmptyParams {
    #[serde(default)]
    items: Vec<String>,
    #[serde(default)]
    all: bool,
    #[serde(default)]
    dry_run: bool,
}

/// `box.add`, `box.install`: the box.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct BoxParams {
    name: String,
}

/// `trash.restore`: the item, and the name to restore it under.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct TrashRestoreParams {
    item: String,
    #[serde(rename = "as")]
    as_name: Option<String>,
}

/// `thread.delete`: `thread` or `threads`, not both.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteParams {
    thread: Option<String>,
    threads: Option<Vec<String>>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct HelloParams {
    client: String,
    /// The orchestrator token (`TWR_ORCH_TOKEN`), from the orchestrator's MCP server only.
    token: Option<String>,
    /// What the client can decode ([`PROTOCOL_VERSION`]); none from a twr before it.
    protocol: Option<u32>,
    /// A test's user token ([`twr_core::client::TEST_USER_TOKEN_ENV`]): the connection is the
    /// user's whatever its ancestry, on a hub started with the same token (tests only).
    user_token: Option<String>,
}

/// The `hello` client names of twr's own clients: the CLI, the TUI and the orchestrator's MCP
/// server. These decode events strictly, so one from before [`PROTOCOL_VERSION`] (no
/// `protocol`) is refused; another client without `protocol` is taken as it is.
const TWR_CLIENTS: [&str; 3] = ["cli", "tui", twr_mcp::CLIENT_NAME];

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SendParams {
    thread: String,
    prompt: String,
    /// Threads the prompt waits for (`hub::after`): sent once each finished its next turn.
    /// Given, it names 1 to 8 of them.
    #[serde(default)]
    after: Option<Vec<String>>,
    /// Accepted for compatibility and ignored: `from` follows the caller's identity.
    #[serde(default)]
    #[allow(dead_code)]
    from: TurnFrom,
}

/// `message.withdraw` parameters.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct WithdrawParams {
    thread: String,
    /// The message's ref, as `say` / `thread.send` returned it.
    #[serde(rename = "ref")]
    msg_ref: i64,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SayParams {
    message: String,
    focused: Option<String>,
    /// The control gseq the client had rendered: only items raised by then can be answered.
    seen_gseq: Option<u64>,
    /// `"orchestrator"`: hand the message to the orchestrator without routing it.
    to: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SetModeParams {
    thread: String,
    mode: String,
}

/// `thread.set_model` params: a missing `model` / `effort` keeps the thread's.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SetModelParams {
    thread: String,
    runtime: Option<String>,
    model: Option<String>,
    effort: Option<String>,
}

/// `thread.rename` params.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RenameParams {
    thread: String,
    name: String,
}

/// `orchestrator.watch` params.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct WatchParams {
    thread: String,
    note: String,
    #[serde(default)]
    repeat: bool,
}

/// `orchestrator.unwatch` params.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct UnwatchParams {
    watch: String,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectParams {
    project: String,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextSetParams {
    project: String,
    content: String,
    /// The `hash` of the `context.read` the new content is based on (`conflict` if the
    /// context changed since).
    #[serde(default)]
    expect_hash: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct DismissParams {
    attn_id: String,
    revision: Option<u64>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AnnotateParams {
    attn_id: String,
    note: String,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReportParams {
    thread: String,
    turn: Option<u64>,
    summary: String,
    grade: Grade,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum AnswerParam {
    Decision(String),
    Full(Answer),
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RespondParams {
    attn_id: String,
    revision: Option<u64>,
    answer: Option<AnswerParam>,
    option_index: Option<usize>,
    message: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AttentionListParams {
    #[serde(default)]
    all: bool,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct EventsParams {
    thread: String,
    #[serde(default)]
    since_seq: u64,
    limit: Option<usize>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ControlParams {
    #[serde(default)]
    since: u64,
    limit: Option<usize>,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct BlobParams {
    #[serde(rename = "ref")]
    blob_ref: String,
    #[serde(default)]
    offset: u64,
    len: Option<usize>,
}

/// Client names become `user:<client>` sources; keep them short and printable.
fn valid_client(c: &str) -> bool {
    !c.is_empty()
        && c.len() <= 64
        && c.chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.'))
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ParkParams {
    thread: String,
    #[serde(default)]
    keep: bool,
    #[serde(default)]
    force: bool,
}

/// Methods that wait on git or on a runtime's exit.
async fn call_async(hub: &Hub, caller: &Caller, method: &str, p: Value) -> Result<Value, RpcError> {
    match method {
        // Committed files of a project on the hub's box (`hub::project_read`).
        "project.files" => hub.project_read(ReadTool::Files, params(p)?, caller).await,
        "project.read" => hub.project_read(ReadTool::Read, params(p)?, caller).await,
        "project.search" => hub.project_read(ReadTool::Search, params(p)?, caller).await,
        "thread.new" => {
            let p: NewThread = params(p)?;
            hub.new_thread_by(p, caller).await
        }
        "thread.park" => {
            let p: ParkParams = params(p)?;
            let out = hub.park_thread(&p.thread, p.keep, p.force, caller).await?;
            serde_json::to_value(out).map_err(internal)
        }
        "thread.resume" => {
            let p: ThreadRef = params(p)?;
            let out = hub.resume_thread(&p.thread, caller).await?;
            serde_json::to_value(out).map_err(internal)
        }
        "thread.trim" => {
            let p: ThreadRef = params(p)?;
            hub.trim_thread(&p.thread, caller).await
        }
        "usage.threads" => {
            #[derive(Default, Deserialize)]
            #[serde(deny_unknown_fields)]
            struct UsageThreadsParams {
                since: Option<chrono::DateTime<chrono::Utc>>,
                #[serde(default)]
                by: crate::hub::SpentBy,
            }
            let p: UsageThreadsParams = params(p)?;
            hub.usage_threads(p.since, p.by).await.map_err(internal)
        }
        // Checks the worktree and branch on the thread's box, then raises the delete card.
        "thread.delete" => {
            let p: DeleteParams = params(p)?;
            let threads = match (p.thread, p.threads) {
                (Some(t), None) => vec![t],
                (None, Some(ts)) if !ts.is_empty() => ts,
                (None, Some(_)) => {
                    return Err(RpcError::new(
                        codes::BAD_REQUEST,
                        "threads is empty: name at least one thread",
                    ))
                }
                _ => {
                    return Err(RpcError::new(
                        codes::BAD_REQUEST,
                        "give either thread or threads",
                    ))
                }
            };
            hub.delete_threads(&threads, caller).await
        }
        // Each box lists, purges and restores its own trash.
        "trash.list" => hub.trash_list().await,
        "trash.empty" => {
            let p: TrashEmptyParams = params(p)?;
            if p.dry_run {
                hub.trash_empty_preview(&p.items, p.all, caller).await
            } else {
                hub.trash_empty(&p.items, p.all, caller).await
            }
        }
        "trash.restore" => {
            let p: TrashRestoreParams = params(p)?;
            hub.trash_restore(&p.item, p.as_name.as_deref(), caller)
                .await
        }
        // Reads config.toml and boxes.toml again (`hub::boxes`).
        "box.add" => {
            let p: BoxParams = params(p)?;
            hub.box_add(&p.name, caller).await
        }
        // Reads the box's `build` from them again.
        "box.install" => {
            let p: BoxParams = params(p)?;
            hub.box_install(&p.name, caller).await
        }
        "context.read" => {
            let p: ProjectParams = params(p)?;
            hub.context_read(&p.project).await
        }
        "context.set" => {
            let p: ContextSetParams = params(p)?;
            hub.context_set(&p.project, &p.content, p.expect_hash.as_deref(), caller)
                .await
        }
        "orchestrator.reset" => {
            let _: EmptyParams = params(p)?;
            hub.reset_orchestrator(caller).await
        }
        // Waits for the previous orchestrator's handoff summary (`hub::handoff`).
        "orchestrator.use" => {
            let p: UseOrchestratorParams = params(p)?;
            let mut opts = SwitchOptions {
                summary: p.summary,
                ..SwitchOptions::default()
            };
            if let Some(w) = p.wait_secs {
                if w > SUMMARY_WAIT_MAX.as_secs() {
                    return Err(RpcError::new(
                        codes::BAD_REQUEST,
                        format!("wait_secs is over {}", SUMMARY_WAIT_MAX.as_secs()),
                    ));
                }
                opts.wait = Duration::from_secs(w);
            }
            hub.use_orchestrator(&p.runtime, p.model.as_deref(), opts, caller)
                .await
        }
        other => Err(RpcError::new(
            codes::METHOD_NOT_FOUND,
            format!("unknown method {other:?}"),
        )),
    }
}

async fn say(hub: &Hub, caller: &Caller, p: Value) -> Result<Value, RpcError> {
    let p: SayParams = params(p)?;
    match p.to.as_deref() {
        None => {
            hub.say_seen(&p.message, p.focused.as_deref(), p.seen_gseq, caller)
                .await
        }
        Some("orchestrator") => hub.say_to_orchestrator(&p.message, p.seen_gseq, caller),
        Some(other) => Err(RpcError::new(
            codes::BAD_REQUEST,
            format!("say `to` can only be \"orchestrator\" (not {other:?}); omit it to route"),
        )),
    }
}

/// How long `hello` waits before it walks the ancestry of a peer that presented the
/// orchestrator token again, when the first walk did not show the orchestrator's runtime
/// (its MCP server may have connected before the hub knew the runtime's process).
const TOKEN_REWALK_DELAY: Duration = Duration::from_millis(200);

/// `hello`: names the connection and, with the current orchestrator token, makes it the
/// orchestrator. A wrong token, the reserved name `orchestrator` without one, or a token-less
/// `hello` on a connection that presented a token (no downgrade to a user client) is refused
/// (`unauthorized`, logged) and leaves the connection as it was. So is the token from a peer
/// that is not, and does not descend from, the process of the orchestrator runtime the token
/// belongs to as the hub recorded it, walked again now ([`orchestrator_peer`];
/// its second walk may restrict a connection that counted as the user):
/// a process of an earlier orchestrator runtime is refused on the connection it opened then.
/// A twr client older than the hub ([`PROTOCOL_VERSION`], [`TWR_CLIENTS`]) is refused
/// (`unsupported`) and told to update.
async fn hello(hub: &Hub, conn: &mut Conn, p: Value) -> Result<Value, RpcError> {
    let p: HelloParams = params(p)?;
    if !valid_client(&p.client) {
        return Err(RpcError::new(
            codes::BAD_REQUEST,
            "client must be 1-64 of [A-Za-z0-9._-]",
        ));
    }
    // A twr older than the hub would stop reading its streams at the first event it cannot
    // decode, with an error that does not say why: it is told to update instead.
    let outdated = match p.protocol {
        Some(v) => v < PROTOCOL_VERSION,
        None => TWR_CLIENTS.contains(&p.client.as_str()),
    };
    if outdated {
        tracing::warn!(client = %p.client, protocol = ?p.protocol, "hello from a twr older than the hub refused");
        return Err(RpcError::new(
            codes::UNSUPPORTED,
            format!(
                "this twr is older than the hub (protocol {}, the hub's is {PROTOCOL_VERSION}): \
                 update it with `twr update` and start it again",
                p.protocol.unwrap_or(0)
            ),
        ));
    }
    // The token reaches the orchestrator's MCP server through a 0600 file and its environment,
    // which another thread's agent (the same OS user) can read: only a process of the
    // orchestrator's runtime (its MCP server) may present it. A process of another thread's
    // runtime or attach, or of an earlier runtime of the orchestrator's thread (it outlived a
    // restart), is refused at once.
    // Trusted only on a hub started with it; elsewhere it is no token at all.
    if p.user_token
        .as_deref()
        .is_some_and(|t| hub.test_user_token_is(t))
    {
        conn.agent = None;
    }
    if let Some(token) = &p.token {
        let orchestrator = hub.orchestrator_token_runtime(token);
        let orchestrator_thread = hub.orchestrator_thread();
        let other_agent = conn.agent.as_ref().filter(|a| match &a.why {
            AgentWhy::Started { kind, .. } => match &orchestrator {
                Some(rt) => !a.descends_from_runtime(rt),
                // A stale or wrong token is refused as such below, unless another thread's
                // process presents it.
                None => {
                    *kind != StartedKind::Runtime
                        || orchestrator_thread.is_none()
                        || a.thread != orchestrator_thread
                }
            },
            _ => false,
        });
        if let Some(a) = other_agent {
            tracing::warn!(client = %p.client, thread = ?a.thread, peer = a.peer, "hello with the orchestrator token from another thread's agent refused");
            return Err(RpcError::new(
                codes::UNAUTHORIZED,
                format!(
                    "this connection comes from {}: only the orchestrator's own MCP server may \
                     present the orchestrator token",
                    a.summary()
                ),
            ));
        }
    }
    match p.token {
        Some(token) => {
            if !hub.orchestrator_token_is(&token) {
                tracing::warn!(client = %p.client, "hello with a wrong or stale orchestrator token refused");
                return Err(RpcError::new(
                    codes::UNAUTHORIZED,
                    "not the current orchestrator token",
                ));
            }
            if !orchestrator_peer(hub, conn, &token).await {
                let who = match (&conn.agent, conn.peer) {
                    (Some(a), _) => a.summary(),
                    (None, Some(pid)) => format!("pid {pid}"),
                    (None, None) => "a process this hub cannot see".to_string(),
                };
                tracing::warn!(client = %p.client, peer = ?conn.peer, agent = ?conn.agent, "hello with the orchestrator token from a process that does not descend from the orchestrator's runtime refused");
                return Err(RpcError::new(
                    codes::UNAUTHORIZED,
                    format!(
                        "this connection comes from {who}, which is neither the orchestrator's \
                         runtime nor a process it started: only the orchestrator's own MCP \
                         server may present the orchestrator token"
                    ),
                ));
            }
            conn.client = p.client;
            conn.token = Some(token);
            Ok(
                json!({ "client": conn.client, "identity": "orchestrator", "workspace": hub.workspace() }),
            )
        }
        // Once a connection presented a token it stays the orchestrator's (or nobody's, when
        // the token went stale): a token-less `hello` would make it a user client.
        None if conn.token.is_some() => {
            tracing::warn!(client = %p.client, "hello without a token on an orchestrator connection refused");
            Err(RpcError::new(
                codes::UNAUTHORIZED,
                "this connection authenticated as the orchestrator; it cannot become a user \
                 client",
            ))
        }
        None if p.client == twr_mcp::CLIENT_NAME => {
            tracing::warn!("hello as {:?} without a token refused", p.client);
            Err(RpcError::new(
                codes::UNAUTHORIZED,
                "the client name \"orchestrator\" needs the orchestrator token",
            ))
        }
        None => {
            conn.client = p.client;
            conn.token = None;
            let identity = conn.identity(&Caller::user(conn.client.clone()));
            Ok(json!({ "client": conn.client, "identity": identity, "workspace": hub.workspace() }))
        }
    }
}

/// Whether the peer of `conn`, its ancestry walked now, is or descends from the process of
/// the orchestrator runtime `token` currently belongs to, as the hub recorded it (pid and start
/// time, or the pid its adapter reported starting, of that runtime's generation;
/// [`AgentPeer::descends_from_runtime`]): the only proof the orchestrator token is accepted
/// with. The walk is fresh each time: a classification made when
/// the connection opened may name a runtime of the orchestrator's thread that has since been
/// replaced, and a process of it that outlived it does not descend from the current one.
/// Classification only restricts: a marker naming the orchestrator's thread, an ancestry that
/// could not be read, or no classification at all (the peer counts as the user) is no proof.
/// When the walk does not show the runtime, the ancestry is walked once more after
/// [`TOKEN_REWALK_DELAY`] (its MCP server may have connected before the hub knew the runtime's
/// process); a walk that shows the runtime replaces the connection's classification, and so
/// does the last one when the connection had none (a peer that counted as the user is then
/// restricted as what it is). Before anything, the peer's pid must still name the process that
/// opened the connection ([`Conn::opened_by`]): a connection whose opener is
/// gone (in the hands of a process that inherited it, the pid maybe reused) never presents the
/// token, and is restricted as a process twr cannot trace when it had no classification.
async fn orchestrator_peer(hub: &Hub, conn: &mut Conn, token: &str) -> bool {
    if conn.agent.is_none() && hub.orchestrator_token_unverified() {
        return opener_unchanged(conn).await;
    }
    let Some(peer) = conn.peer else {
        return false;
    };
    for walk in 0..2 {
        if walk > 0 {
            tokio::time::sleep(TOKEN_REWALK_DELAY).await;
        }
        match token_runtime_walk(hub, conn, peer, token).await {
            TokenWalk::Shows(a) => {
                conn.agent = a;
                return true;
            }
            TokenWalk::OpenerGone => {
                conn.agent
                    .get_or_insert_with(|| AgentPeer::untraced(peer, AncestryEnd::Gone));
                return false;
            }
            TokenWalk::Not(a) => {
                if walk > 0 && conn.agent.is_none() {
                    if let Some(a) = a {
                        tracing::info!(thread = ?a.thread, peer = a.peer, why = ?a.why, "a connection from a thread's agent, found on a second walk: it reads, and starts and cancels its own thread's jobs only");
                        conn.agent = Some(a);
                    }
                }
            }
        }
    }
    false
}

/// What a walk of a connection's peer for the orchestrator token found ([`token_runtime_walk`]).
enum TokenWalk {
    /// The peer is, or descends from, the process of the runtime the token belongs to: what
    /// the peer is.
    Shows(Option<AgentPeer>),
    /// It is not: what the peer is.
    Not(Option<AgentPeer>),
    /// The process that opened the connection is gone: the pid names another process, or none
    /// ([`Conn::opened_by`]).
    OpenerGone,
}

/// What the peer `peer` of `conn` is, its ancestry walked now ([`Hub::agent_peer`]), and
/// whether that shows the process of the orchestrator runtime `token` belongs to once the walk
/// is done (a token that went stale meanwhile shows nothing). The walk's own read of the peer
/// must be the process that opened the connection, same pid and start time
/// ([`Conn::opened_by`]), else what it found is another process's ancestry
/// ([`TokenWalk::OpenerGone`]).
async fn token_runtime_walk(hub: &Hub, conn: &Conn, peer: u32, token: &str) -> TokenWalk {
    let PeerWalk { process, agent } = hub.agent_peer(peer).await;
    if conn.opened_by.is_none() || process != conn.opened_by {
        tracing::warn!(peer, opened_by = ?conn.opened_by, now = ?process, "the process that opened a connection presenting the orchestrator token is gone");
        return TokenWalk::OpenerGone;
    }
    let shows = match (&agent, hub.orchestrator_token_runtime(token)) {
        (Some(a), Some(rt)) => a.descends_from_runtime(&rt),
        _ => false,
    };
    if shows {
        TokenWalk::Shows(agent)
    } else {
        TokenWalk::Not(agent)
    }
}

/// Whether the peer of `conn` still is the process that opened it ([`Conn::opened_by`]), read
/// now (where no walk is made: [`HubOptions::orchestrator_token_unverified`]).
///
/// [`HubOptions::orchestrator_token_unverified`]: crate::HubOptions::orchestrator_token_unverified
async fn opener_unchanged(conn: &Conn) -> bool {
    let (Some(peer), Some(opened_by)) = (conn.peer, conn.opened_by) else {
        return false;
    };
    let now = tokio::task::spawn_blocking(move || reap::process_start_time(peer))
        .await
        .ok()
        .flatten();
    let same = now == Some(opened_by.start_time);
    if !same {
        tracing::warn!(
            peer,
            ?opened_by,
            ?now,
            "the process that opened a connection presenting the orchestrator token is gone"
        );
    }
    same
}

/// The per-request proof of an orchestrator connection (its token is current): its peer's
/// ancestry, walked again now, still shows the process of the runtime that token belongs to.
/// Else the request is refused (`unauthorized`) and the connection stays as it is: never the
/// user's, and restricted as what the walk found when it had no classification.
async fn still_orchestrator(hub: &Hub, conn: &mut Conn) -> Result<(), RpcError> {
    if conn.agent.is_none() && hub.orchestrator_token_unverified() {
        if opener_unchanged(conn).await {
            return Ok(());
        }
        return Err(no_longer_orchestrator());
    }
    let (Some(peer), Some(token)) = (conn.peer, conn.token.as_deref()) else {
        return Err(no_longer_orchestrator());
    };
    let a = match token_runtime_walk(hub, conn, peer, token).await {
        TokenWalk::Shows(_) => return Ok(()),
        TokenWalk::OpenerGone => Some(AgentPeer::untraced(peer, AncestryEnd::Gone)),
        TokenWalk::Not(a) => a,
    };
    tracing::warn!(peer, agent = ?a, "a request on an orchestrator connection whose peer no longer descends from the orchestrator's runtime refused");
    if conn.agent.is_none() {
        conn.agent = a;
    }
    Err(no_longer_orchestrator())
}

/// The refusal of a request on an orchestrator connection whose peer no longer descends from
/// the orchestrator's current runtime, or is no longer the process that opened it.
fn no_longer_orchestrator() -> RpcError {
    RpcError::new(
        codes::UNAUTHORIZED,
        "this connection no longer comes from the orchestrator's current runtime (it was \
         restarted, or the process that opened this connection is gone): only its own MCP \
         server may act as the orchestrator",
    )
}

fn call(
    hub: &Hub,
    conn: &mut Conn,
    caller: &Caller,
    method: &str,
    p: Value,
) -> Result<Value, RpcError> {
    let client = conn.client.clone();
    match method {
        "hub.info" => {
            let mut info = hub.info();
            info["client"] = json!(client);
            info["identity"] = json!(conn.identity(caller));
            Ok(info)
        }
        "hub.shutdown" => Ok(json!({ "ok": true })),
        "config.show" => Ok(hub.config_show()),
        "thread.spawn" => {
            #[derive(Default, Deserialize)]
            #[serde(deny_unknown_fields)]
            struct SpawnParams {
                operation_id: String,
                request: NewThread,
                prompt: Option<String>,
            }
            let p: SpawnParams = params(p)?;
            hub.begin_spawn(&p.operation_id, p.request, p.prompt, caller)
        }
        "thread.spawn_status" => {
            #[derive(Default, Deserialize)]
            #[serde(deny_unknown_fields)]
            struct StatusParams {
                operation_id: String,
            }
            let p: StatusParams = params(p)?;
            hub.spawn_status(&p.operation_id, caller)
        }
        "thread.list" => Ok(Value::Array(hub.list_threads())),
        "thread.activity" => Ok(hub.thread_activity()),
        "usage.get" => Ok(hub.usage()),
        // A user's TUI: whether the user looks at it, so the notification hook keeps quiet
        // meanwhile (`hub::hook`).
        "notify.looking" => {
            #[derive(Default, Deserialize)]
            #[serde(deny_unknown_fields)]
            struct LookingParams {
                looking: bool,
            }
            let p: LookingParams = params(p)?;
            hub.set_looking(conn.id, p.looking);
            Ok(json!({ "ok": true }))
        }
        // Jobs (`hub::jobs`): `job.start` and `job.cancel` are `job_request`'s.
        "job.list" => {
            #[derive(Default, Deserialize)]
            #[serde(deny_unknown_fields)]
            struct JobListParams {
                thread: Option<String>,
            }
            let p: JobListParams = params(p)?;
            hub.list_jobs(p.thread.as_deref())
        }
        "job.log" => {
            #[derive(Default, Deserialize)]
            #[serde(deny_unknown_fields)]
            struct JobLogParams {
                job: String,
                lines: Option<usize>,
            }
            let p: JobLogParams = params(p)?;
            hub.job_log(&p.job, p.lines)
        }
        "thread.get" => {
            let p: ThreadRef = params(p)?;
            hub.get_thread(&p.thread)
        }
        "thread.send" => {
            let p: SendParams = params(p)?;
            if let Some(after) = &p.after {
                return hub.send_after_by(&p.thread, &p.prompt, after, caller);
            }
            let out = hub.send_by(&p.thread, &p.prompt, caller)?;
            serde_json::to_value(out).map_err(internal)
        }
        "thread.send_cancel" => {
            #[derive(Default, Deserialize)]
            #[serde(deny_unknown_fields)]
            struct CancelParams {
                id: String,
            }
            let p: CancelParams = params(p)?;
            hub.cancel_send_after(&p.id)
        }
        "thread.stop" => {
            let p: ThreadRef = params(p)?;
            hub.stop_thread_by(&p.thread, caller)
        }
        "message.withdraw" => {
            let p: WithdrawParams = params(p)?;
            hub.withdraw_message(&p.thread, p.msg_ref, caller)
        }
        "thread.set_mode" => {
            let p: SetModeParams = params(p)?;
            hub.set_mode(&p.thread, &p.mode, caller)
        }
        "thread.set_model" => {
            let p: SetModelParams = params(p)?;
            hub.set_runtime_model(
                &p.thread,
                p.runtime.as_deref(),
                p.model.as_deref(),
                p.effort.as_deref(),
                caller,
            )
        }
        "thread.rename" => {
            let p: RenameParams = params(p)?;
            hub.rename_thread(&p.thread, &p.name, caller)
        }
        "orchestrator.show" => Ok(hub.orchestrator_info()),
        "orchestrator.watch" => {
            let p: WatchParams = params(p)?;
            hub.watch_thread(&p.thread, &p.note, p.repeat, caller)
        }
        "orchestrator.unwatch" => {
            let p: UnwatchParams = params(p)?;
            hub.unwatch(&p.watch, caller)
        }
        "orchestrator.watches" => Ok(hub.watches()),
        "attention.list" => {
            let p: AttentionListParams = params(p)?;
            Ok(Value::Array(hub.attention_list(p.all)))
        }
        "attention.respond" => {
            let p: RespondParams = params(p)?;
            let mut answer = match (p.answer, p.option_index) {
                (Some(_), Some(_)) => {
                    return Err(RpcError::new(
                        codes::BAD_REQUEST,
                        "give either answer or option_index, not both",
                    ))
                }
                (None, Some(i)) => {
                    return hub
                        .respond_option_relay(&p.attn_id, i, p.message, p.revision, caller)?
                        .into_json();
                }
                (Some(AnswerParam::Decision(d)), None) => Answer::new(d),
                (Some(AnswerParam::Full(a)), None) => a,
                (None, None) => {
                    return Err(RpcError::new(
                        codes::BAD_REQUEST,
                        "answer or option_index is required",
                    ))
                }
            };
            if p.message.is_some() {
                answer.message = p.message;
            }
            if answer.decision.is_empty() {
                return Err(RpcError::new(codes::BAD_REQUEST, "answer is empty"));
            }
            hub.respond_relay(&p.attn_id, answer, p.revision, caller)?
                .into_json()
        }
        "attention.dismiss" => {
            let p: DismissParams = params(p)?;
            let ev = hub.attention_dismiss(&p.attn_id, p.revision, caller)?;
            serde_json::to_value(ev).map_err(internal)
        }
        "attention.annotate" => {
            let p: AnnotateParams = params(p)?;
            let ev = hub.annotate(&p.attn_id, &p.note, caller)?;
            serde_json::to_value(ev).map_err(internal)
        }
        "report.write" => {
            let p: ReportParams = params(p)?;
            hub.write_report(&p.thread, p.turn, &p.summary, p.grade, caller)
        }
        "thread.events" => {
            let p: EventsParams = params(p)?;
            let limit = limit(p.limit);
            // One extra row tells whether more follow.
            let (thread, events) = hub.thread_events(&p.thread, p.since_seq, limit + 1)?;
            let (page, more) = page(&events, limit, |e| e)?;
            let next = page
                .len()
                .checked_sub(1)
                .map_or(p.since_seq, |i| events[i].seq);
            Ok(json!({
                "thread": thread,
                "events": page,
                "last_seq": next,
                "next": next,
                "more": more,
            }))
        }
        "thread.transcript" => {
            #[derive(Default, Deserialize)]
            #[serde(deny_unknown_fields)]
            struct TranscriptParams {
                thread: String,
                since_seq: Option<u64>,
                turn: Option<u64>,
                limit: Option<usize>,
            }
            let p: TranscriptParams = params(p)?;
            let limit = p.limit.unwrap_or(500).clamp(1, 500);
            let (thread, since, turn, events) =
                hub.transcript_events(&p.thread, p.since_seq, p.turn, limit + 1)?;
            let (page, more) = page(&events, limit, |e| e)?;
            let next = page.last().map_or(since, |_| events[page.len() - 1].seq);
            Ok(
                json!({ "thread": thread, "events": page, "since_seq": since,
                "last_seq": next, "next": next, "turn": turn, "more": more }),
            )
        }
        "control.read" => {
            let p: ControlParams = params(p)?;
            let limit = limit(p.limit);
            let (items, head) = hub.control_read(p.since, limit + 1).map_err(internal)?;
            let (page, more) = page(&items, limit, |(g, ev)| Item {
                stream: StreamKind::Control,
                gseq: Some(*g),
                event: ev,
            })?;
            let next = page.len().checked_sub(1).map_or(p.since, |i| items[i].0);
            Ok(json!({ "items": page, "head": head, "next": next, "more": more }))
        }
        "blob.get" => {
            let p: BlobParams = params(p)?;
            let len = p.len.unwrap_or(BLOB_CHUNK_MAX).clamp(1, BLOB_CHUNK_MAX);
            let Some((size, bytes)) = hub
                .blob_chunk(&p.blob_ref, p.offset, len)
                .map_err(internal)?
            else {
                return Err(RpcError::new(
                    codes::NOT_FOUND,
                    format!("no blob {}", p.blob_ref),
                ));
            };
            if p.offset > size {
                return Err(RpcError::new(
                    codes::BAD_REQUEST,
                    format!(
                        "offset {} is past the end of the blob ({size} bytes)",
                        p.offset
                    ),
                ));
            }
            let eof = p.offset + bytes.len() as u64 >= size;
            let (encoding, data) = match String::from_utf8(bytes) {
                Ok(text) => ("utf8", text),
                Err(e) => ("base64", twr_core::b64::encode(e.as_bytes())),
            };
            Ok(json!({
                "ref": p.blob_ref,
                "size": size,
                "offset": p.offset,
                "encoding": encoding,
                "data": data,
                "eof": eof,
            }))
        }
        other => Err(RpcError::new(
            codes::METHOD_NOT_FOUND,
            format!("unknown method {other:?}"),
        )),
    }
}

fn control_subscribe(
    hub: &Hub,
    p: Value,
) -> Result<(Value, crate::hub::ControlSubscription), RpcError> {
    let p: ControlParams = params(p)?;
    let sub = hub.subscribe_control(p.since);
    let ack = json!({ "since": p.since, "head": sub.head() });
    Ok((ack, sub))
}

fn thread_subscribe(
    hub: &Hub,
    p: Value,
) -> Result<(Value, crate::hub::ThreadSubscription), RpcError> {
    let p: EventsParams = params(p)?;
    let sub = hub.subscribe_thread(&p.thread, p.since_seq)?;
    let ack = json!({ "thread": sub.thread(), "since_seq": p.since_seq, "head": sub.head() });
    Ok((ack, sub))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HubOptions;
    use twr_core::config::Config;

    /// A connection from this test process that presented `token`, opened by this process
    /// started at `start_time` as the hub read it (`None`: it could not be read).
    fn conn(start_time: Option<u64>) -> Conn {
        let me = std::process::id();
        Conn {
            client: "c".into(),
            token: Some("t".into()),
            agent: None,
            peer: Some(me),
            opened_by: start_time.map(|start_time| ProcessId {
                pid: me,
                start_time,
            }),
            id: 0,
        }
    }

    /// A peer whose credentials cannot be read, or name no pid the hub can see, is no user: it
    /// is an agent of no thread, restricted to [`AGENT_METHODS`] and starting no job; the
    /// hub's own process (its in-process clients) stays the user.
    #[tokio::test]
    async fn a_peer_the_hub_cannot_identify_is_an_agent_of_no_thread() {
        let me = std::process::id();
        let unreadable = Err(std::io::Error::other("no SO_PEERCRED"));
        for cred in [unreadable, Ok(None), Ok(Some(0)), Ok(Some(-1))] {
            assert!(
                matches!(peer_pid(cred, me), PeerPid::Unknown(_)),
                "an unidentified peer"
            );
        }
        assert_eq!(peer_pid(Ok(Some(me as i32)), me), PeerPid::Hub(me));
        assert_eq!(peer_pid(Ok(Some(4242)), me), PeerPid::Other(4242));

        let dir = tempfile::tempdir().unwrap();
        let hub = Hub::open(HubOptions::new(
            dir.path().join("twr.db"),
            Config::default(),
        ))
        .await
        .unwrap();
        let c = Conn {
            client: "c".into(),
            token: None,
            agent: Some(AgentPeer::unidentified()),
            peer: None,
            opened_by: None,
            id: 0,
        };
        let agent = c.restricted().expect("restricted");
        assert_eq!(c.identity(&Caller::user("c")), "agent:unknown");
        let e = agent_thread(&hub, agent).unwrap_err();
        assert_eq!(e.code, codes::RELAY_FORBIDDEN);
        assert!(
            e.message.starts_with("a process twr cannot identify:"),
            "{e:?}"
        );
        assert!(agent_refused(agent)
            .message
            .contains("a process twr cannot identify (the connection's peer credentials"));
    }

    /// The hub's subscriptions: a connection holds at most [`SUBS_PER_CONNECTION`]; agents'
    /// connections together take at most [`AGENT_SUBS_MAX`], which leaves the user's clients
    /// room up to [`SUBS_MAX`]; a slot let go (its subscription ended) is free again.
    #[test]
    fn subscriptions_are_bounded_per_connection_and_hub_wide() {
        let count = Arc::new(SubCount::default());
        let e = sub_slot(&count, SUBS_PER_CONNECTION, false).err().unwrap();
        assert_eq!(e.code, codes::CONFLICT);
        assert!(
            e.message.contains("holds 16 subscriptions already"),
            "{e:?}"
        );
        let agents: Vec<SubSlot> = (0..AGENT_SUBS_MAX)
            .map(|_| sub_slot(&count, 0, true).ok().unwrap())
            .collect();
        assert!(
            sub_slot(&count, 0, true).is_err(),
            "agents have taken theirs"
        );
        let users: Vec<SubSlot> = (AGENT_SUBS_MAX..SUBS_MAX)
            .map(|_| sub_slot(&count, 0, false).ok().unwrap())
            .collect();
        let e = sub_slot(&count, 0, false).err().unwrap();
        assert!(
            e.message.contains("as many subscriptions as it takes"),
            "{e:?}"
        );
        assert_eq!(count.all.load(Ordering::SeqCst), SUBS_MAX);
        drop(agents);
        assert_eq!(count.agents.load(Ordering::SeqCst), 0);
        assert_eq!(count.all.load(Ordering::SeqCst), SUBS_MAX - AGENT_SUBS_MAX);
        assert!(sub_slot(&count, 0, true).is_ok());
        drop(users);
        assert_eq!(count.all.load(Ordering::SeqCst), 0);
    }

    /// [`METHODS`] is every method the server dispatches (each string arm of its dispatch
    /// matches, read from this file), and the agent's and the orchestrator's lists name only
    /// methods of it.
    #[test]
    fn methods_are_every_dispatched_method() {
        let mut dispatched: Vec<&str> = include_str!("server.rs")
            .lines()
            .filter_map(|l| {
                let rest = l.trim_start().strip_prefix('"')?;
                let (name, after) = rest.split_once('"')?;
                after.trim_start().starts_with("=>").then_some(name)
            })
            .collect();
        dispatched.sort_unstable();
        let mut methods = METHODS.to_vec();
        methods.sort_unstable();
        assert_eq!(methods, dispatched);
        for m in AGENT_METHODS.iter().chain(ORCHESTRATOR_METHODS) {
            assert!(METHODS.contains(m), "{m}");
        }
    }

    /// The orchestrator token is only presented, and only keeps working, while the connection's
    /// peer pid still names the process that opened it: when the start time
    /// recorded at accept is no longer that pid's (the opener exited and its pid was reused,
    /// simulated by recording another one), or none was recorded, the walk shows nothing and
    /// the connection is refused and restricted, also under the test hook that skips the walk.
    #[tokio::test]
    async fn the_token_needs_the_process_that_opened_the_connection() {
        let dir = tempfile::tempdir().unwrap();
        let me = std::process::id();
        let opened = reap::process_start_time(me).unwrap();
        for unverified in [false, true] {
            let mut opts = HubOptions::new(dir.path().join("twr.db"), Config::default());
            opts.orchestrator_token_unverified = unverified;
            let hub = Hub::open(opts).await.unwrap();
            for gone in [Some(opened + 1), None] {
                let c = conn(gone);
                if !unverified {
                    assert!(matches!(
                        token_runtime_walk(&hub, &c, me, "t").await,
                        TokenWalk::OpenerGone
                    ));
                }
                let mut c = conn(gone);
                assert!(!orchestrator_peer(&hub, &mut c, "t").await, "{gone:?}");
                let mut c = conn(gone);
                let e = still_orchestrator(&hub, &mut c).await.unwrap_err();
                assert_eq!(e.code, codes::UNAUTHORIZED);
                assert!(
                    e.message
                        .contains("the process that opened this connection is gone"),
                    "{e:?}"
                );
                if !unverified {
                    assert_eq!(
                        c.agent.map(|a| a.why),
                        Some(AgentWhy::Unresolved(AncestryEnd::Gone))
                    );
                }
            }
            // The process that opened it: the walk goes on (no orchestrator here, so it shows
            // nothing), and the test hook accepts it.
            let c = conn(Some(opened));
            assert!(matches!(
                token_runtime_walk(&hub, &c, me, "t").await,
                TokenWalk::Not(_)
            ));
            let mut c = conn(Some(opened));
            assert_eq!(orchestrator_peer(&hub, &mut c, "t").await, unverified);
            let mut c = conn(Some(opened));
            assert_eq!(still_orchestrator(&hub, &mut c).await.is_ok(), unverified);
            drop(hub);
        }
    }
}
