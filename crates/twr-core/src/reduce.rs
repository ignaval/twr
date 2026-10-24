//! Pure reducers over the event log: thread views, the registry, the attention stack and the
//! stack order of threads. No IO; everything here can be rebuilt by replaying events.

use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::config::CardOrder;
use crate::event::*;

// ---------------------------------------------------------------------------------------------
// ThreadStatus
// ---------------------------------------------------------------------------------------------

/// Derived status of a thread.
///
/// Precedence (first matching row wins), see [`ThreadView::status`]:
///
/// | # | status     | condition                                                                     |
/// |---|------------|-------------------------------------------------------------------------------|
/// | 1 | `attached` | `thread.attached` without a later `thread.detached`                           |
/// | 2 | `parked`   | `thread.parked` without a later `thread.resumed` / `turn.started`             |
/// | 3 | `done`     | `thread.stopped` without a later `thread.started` / `resumed` / `turn.started` |
/// | 4 | `waiting`  | an open `permission` or `question` attention                                  |
/// | 5 | `running`  | a `turn.started` without its `turn.ended`                                     |
/// | 6 | `background` | no open turn, but session-scoped tasks remain                             |
/// | 7 | `error`    | last turn ended `error`, or a crash without a new runtime/turn start          |
/// | 8 | `done`     | last turn ended `completed` and a `report.ready` covers it                    |
/// | 9 | `idle`     | anything else (new, interrupted, completed but not yet reported)              |
///
/// `thread.crashed`, `thread.stopped` and `thread.parked` close any open turn and drop open
/// blocking (permission/question) attention, whether or not the hub also appended `turn.ended`
/// / `attention.resolved` before or after them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    /// Nothing happening.
    #[default]
    Idle,
    /// A turn is running.
    Running,
    /// No foreground turn, but the runtime still has work running.
    Background,
    /// Blocked on the user (permission or question).
    Waiting,
    /// Finished and reported, or stopped.
    Done,
    /// Last turn failed or the process crashed.
    Error,
    /// Parked: process stopped, worktree possibly pruned.
    Parked,
    /// A user has the runtime's own CLI attached.
    Attached,
}

impl ThreadStatus {
    /// Lowercase name, as serialized.
    pub fn as_str(self) -> &'static str {
        match self {
            ThreadStatus::Idle => "idle",
            ThreadStatus::Running => "running",
            ThreadStatus::Background => "background",
            ThreadStatus::Waiting => "waiting",
            ThreadStatus::Done => "done",
            ThreadStatus::Error => "error",
            ThreadStatus::Parked => "parked",
            ThreadStatus::Attached => "attached",
        }
    }

    /// Rank used to order threads without attention in the stack (lower = further left):
    /// running (and waiting/attached) > error > done > idle > parked.
    pub fn stack_rank(self) -> u8 {
        match self {
            ThreadStatus::Running
            | ThreadStatus::Background
            | ThreadStatus::Waiting
            | ThreadStatus::Attached => 0,
            ThreadStatus::Error => 1,
            ThreadStatus::Done => 2,
            ThreadStatus::Idle => 3,
            ThreadStatus::Parked => 4,
        }
    }
}

impl fmt::Display for ThreadStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------------------------
// ThreadView
// ---------------------------------------------------------------------------------------------

/// Summary of a finished turn.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastTurn {
    /// Turn number.
    pub turn: u64,
    /// How it ended.
    pub result: TurnResult,
    /// If this turn was an automatic retry, the turn it retried.
    pub retry_of: Option<u64>,
    /// The user took back its message (`turn.withdrawn`): no crash after it makes it a failed
    /// turn (no error item, no retry of a message taken back).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub withdrawn: bool,
    /// The user interrupted it to send the next queued message now (`turn.interrupted`): no
    /// crash after it makes it a failed turn either (its message is not run again).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub interrupted: bool,
}

/// One task still owned by the current runtime process.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackgroundTask {
    /// Runtime's description of the work.
    pub description: String,
    /// Most recent lifecycle or liveness signal, for the hub's timeout.
    pub updated_at: DateTime<Utc>,
    /// Thread seq of the update that started it: a task id reused after the task ended is
    /// another task. 0 in snapshots from before it was kept.
    #[serde(default)]
    pub started_seq: u64,
    /// A job the hub runs for the thread ([`crate::event::JOB_TASK_PREFIX`]): no end of a
    /// runtime clears it, only its own end.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub job: bool,
    /// A job started not to tell its thread when it ends (`ext.twr.job.wake: false`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub quiet: bool,
    /// A `!` command the user ran from the TUI (`ext.twr.job.shell`): the user's own work, not
    /// the thread's (the orchestrator is told nothing of it).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shell: bool,
    /// A job's process group as the hub recorded it before the job's command ran
    /// (`ext.twr.job.pgid`, the group's leader's pid, and `ext.twr.job.start_time`, its start
    /// time): what a later hub kills of it when the hub that ran it stopped first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<JobGroup>,
}

/// A job's process group ([`BackgroundTask::group`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobGroup {
    /// The group's id: its leader's pid.
    pub pgid: u32,
    /// The leader's start time, as the hub's machine reads it
    /// (`twr_runner::reap::process_start_time`).
    pub start_time: u64,
    /// Other members of the group as the hub last saw them while the job ran
    /// (`ext.twr.job.members`, `[pid, start time]` pairs, with its renewals): what proves the
    /// group the job's to a later hub once its leader is gone.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<(u32, u64)>,
}

/// The process group a job's task update records ([`BackgroundTask::group`]): its
/// `ext.twr.job.pgid` and `ext.twr.job.start_time`, both there.
fn job_group(ext: Option<&JsonMap>) -> Option<JobGroup> {
    let job = ext?.get("twr")?.get("job")?;
    Some(JobGroup {
        pgid: u32::try_from(job.get("pgid")?.as_u64()?).ok()?,
        start_time: job.get("start_time")?.as_u64()?,
        members: Vec::new(),
    })
}

/// The members a job's renewal records of its group ([`JobGroup::members`]), when it says.
fn job_members(ext: Option<&JsonMap>) -> Option<Vec<(u32, u64)>> {
    let members = ext?.get("twr")?.get("job")?.get("members")?.as_array()?;
    Some(
        members
            .iter()
            .filter_map(|m| {
                let m = m.as_array()?;
                Some((
                    u32::try_from(m.first()?.as_u64()?).ok()?,
                    m.get(1)?.as_u64()?,
                ))
            })
            .collect(),
    )
}

/// Everything known about one thread, folded from its events.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ThreadView {
    /// Thread id.
    pub id: String,
    /// Name (the id until `thread.created` is seen).
    pub name: String,
    /// Project.
    pub project: Option<String>,
    /// `thread.created`'s `ext.twr.configured_project`: the thread was made for the configured
    /// project named `project` (`[projects.<name>]` or `[boxes.<box>.projects.<name>]`), not
    /// for a repository path that has its name. Only then do its runtimes get that project's
    /// `env` (the hub).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub configured_project: bool,
    /// Repository path.
    pub repo: Option<String>,
    /// Worktree path.
    pub worktree: Option<String>,
    /// Box name.
    pub box_name: String,
    /// Runtime/adapter name.
    pub runtime: String,
    /// An `acp` thread's agent, its argv: `thread.created`'s `ext.twr.acp_command`
    /// ([`ACP_COMMAND_KEY`]), then that of the latest move to another runtime (`None` after a
    /// move to another adapter). Two agents of one adapter differ only here. Serialized as
    /// `acp_command`, as the hub reports a thread's agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp_command: Option<serde_json::Value>,
    /// Permission mode: `thread.created`'s, then the latest `thread.mode_changed`'s (what the
    /// thread's next runtime starts with).
    pub mode: String,
    /// Model: `thread.created`'s `ext.twr.model`, then the latest `thread.model_changed`'s
    /// (what the thread's next runtime starts with); `None` = the runtime's default.
    pub model: Option<String>,
    /// Reasoning effort, likewise (`ext.twr.effort`, then `thread.model_changed`); `None` =
    /// the runtime's default (and every log from before efforts).
    #[serde(default)]
    pub effort: Option<String>,
    /// The model the runtime reports it runs ([`reported_model`]): from the latest
    /// `thread.started` (a new runtime process: `None` when it reports none), or a later
    /// `thread.resumed` / `turn.ended` that reports one (ACP agents report a prompt's model in
    /// its result). A move to another runtime clears it, and while the move is pending
    /// ([`ThreadView::handoff_pending`]) no report of the previous runtime brings it back.
    /// Serialized as `model_actual`, next to the requested `model`.
    #[serde(rename = "model_actual", default)]
    pub actual_model: Option<String>,
    /// Aliases.
    pub aliases: Vec<String>,
    /// The names it had before its `thread.renamed`s, oldest first (without its name now).
    /// They stay reserved for it: its worktree path and branch may still carry one, so no
    /// other thread takes them (the hub), and a client may still meet them in older text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub former_names: Vec<String>,
    /// `thread.created` `ext.twr.role` (e.g. `orchestrator`).
    pub role: Option<String>,
    /// What its work merges back into as far as twr can tell (`ext.twr.base_ref`, a name that
    /// keeps its meaning from any checkout, see `WorktreeCreated::base_ref`). `None` for a
    /// thread without a worktree of its own, or made before twr recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// The turn whose background work (its runtime's tasks, not jobs) a stop, crash, park,
    /// attach or new runtime start ended before it finished; cleared by the next turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cut_short: Option<u64>,
    /// An orchestrator thread that a later orchestrator thread replaced (`twr orchestrator
    /// use`): the most recent `thread.created` with role `orchestrator` is the orchestrator,
    /// every earlier one is retired for good ([`Registry::apply`]). Derived from the order of
    /// the log, never stored.
    #[serde(default)]
    pub retired: bool,
    /// Latest known runtime session id. `None` after the thread moved to another runtime
    /// until that runtime starts a session ([`ThreadView::handoff_pending`]).
    pub session_id: Option<String>,
    /// The thread moved to another runtime (`thread.model_changed` with a `runtime`) and that
    /// runtime has not started a session yet: the next turn starts a new session, and its
    /// prompt carries the handoff from the previous one (the hub). Meanwhile no event of the
    /// previous runtime (its last `turn.ended`, its stop, a report) brings its session id or
    /// its model ([`ThreadView::actual_model`]) back;
    /// only a `thread.started` of the new runtime ends this.
    #[serde(default)]
    pub handoff_pending: bool,
    /// Highest turn number started.
    pub turns: u64,
    /// Currently open turn.
    pub current_turn: Option<u64>,
    /// Session-scoped tasks. Old logs and clients deserialize with none.
    #[serde(default)]
    pub background_tasks: BTreeMap<String, BackgroundTask>,
    /// An exit closed task tracking; a new runtime or turn reopens it without clearing
    /// remembered crash/retry history. Defaults open for older snapshots.
    #[serde(default)]
    pub task_tracking_closed: bool,
    /// `ext.twr.retry_of` of the currently open turn.
    pub current_turn_retry_of: Option<u64>,
    /// The runtime's id of the currently open turn when the runtime started it
    /// (`ext.twr.runtime_turn`): only an end with that id closes it.
    #[serde(default)]
    pub current_runtime_turn: Option<String>,
    /// The ref of the queued message the currently open turn started with
    /// (`ext.twr.ref`, [`MESSAGE_REF_KEY`]): what `message.withdraw` names it by.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_turn_ref: Option<i64>,
    /// The currently open turn has acted: its runtime called a tool (`agent.tool_call`) or
    /// raised a request (a permission or question) in it. Its message can no longer be taken
    /// back (the orchestrator's own changes through the hub count too: the hub keeps those).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub current_turn_acted: bool,
    /// The user took back the currently open turn's message (`turn.withdrawn`): the turn has
    /// no authority and is being interrupted.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub current_turn_withdrawn: bool,
    /// The user interrupted the currently open turn to send the next queued message now
    /// (`turn.interrupted`): it is being interrupted, and ends `interrupted`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub current_turn_interrupted: bool,
    /// The currently open turn is a compaction of the context the user asked for
    /// ([`is_compact_turn`]): maintenance, no work. Its end leaves [`ThreadView::last_turn`],
    /// the report and what the last work turn left (its interim report, its failure) as they
    /// were, so nothing that waits on the thread's work (a wake, a watch, `send --after`, a
    /// retry) counts it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub current_turn_compact: bool,
    /// Every automatic retry seen: retry turn -> the turn it retried (`ext.twr.retry_of`).
    /// Kept per turn number so a `turn.ended` that arrives after `thread.crashed` (which closes
    /// the open turn) still knows its turn was a retry.
    pub turn_retry_of: BTreeMap<u64, u64>,
    /// Last finished turn.
    pub last_turn: Option<LastTurn>,
    /// If the last finished turn was an automatic retry, the turn it retried (retry budget).
    pub last_turn_retry_of: Option<u64>,
    /// Latest report.
    pub last_report: Option<ReportReady>,
    /// Whether the latest report covers the last finished turn.
    pub reported: bool,
    /// Open attention items on this thread: attn_id -> reason.
    pub open_attention: BTreeMap<String, AttentionReason>,
    /// Latest git snapshot.
    pub git: Option<GitSnapshot>,
    /// What its worktree takes on disk: the latest `worktree.disk`, less what trims since freed
    /// (`worktree.trimmed`); `None` before its box measured it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<crate::git::WorktreeDisk>,
    /// When its worktree's build output was last trimmed (`worktree.trimmed`), and how much
    /// that freed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trimmed: Option<(DateTime<Utc>, u64)>,
    /// What its turns spent: tokens and cost, summed from `turn.ended` `usage`
    /// ([`crate::usage::Spent`]).
    #[serde(default, skip_serializing_if = "crate::usage::Spent::is_empty")]
    pub spent: crate::usage::Spent,
    /// How full its session's context is: the latest `turn.ended` `context`, then what a
    /// compaction since left (`context.compacted` `post_tokens`, the window kept; unknown when
    /// it did not say). A new
    /// session (a `thread.started` with another session id, a move to another runtime) has
    /// none until its first turn ends, and a turn end whose size cannot be trusted
    /// ([`recorded_context`]) leaves it unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<ContextSize>,
    /// When its runtime began compacting its context (`context.compacting`), while it does:
    /// until the `context.compacted`, the end of the turn or of the process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compacting: Option<DateTime<Utc>>,
    /// Client currently attached.
    pub attached: Option<String>,
    /// Parked.
    pub parked: bool,
    /// Stopped (`thread.stopped`, except the one closing a runtime restarted for a mode,
    /// model or effort change, [`is_settings_restart`]).
    pub stopped: bool,
    /// Crashed since the last turn started.
    pub crashed: bool,
    /// The turn the latest crash took down: the turn open at `thread.crashed`, or, when the hub
    /// already closed it, the last turn if it did not complete. None for an idle crash.
    pub crashed_turn: Option<u64>,
    /// Timestamp of `thread.created` (or first event seen).
    pub created_at: Option<DateTime<Utc>>,
    /// Timestamp of the latest event.
    pub last_activity: Option<DateTime<Utc>>,
    /// When the thread last worked, which `[projects.<name>] trim_idle_after` counts from: its
    /// latest event that is no bookkeeping of the hub's (as for
    /// [`ThreadView::last_activity`]) nor a git snapshot (its base moving is not its work), its
    /// jobs' counted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worked_at: Option<DateTime<Utc>>,
    /// Seq of the latest event applied.
    pub last_seq: u64,
    /// Seq of the thread's latest `thread.created` or `thread.restored`: which life of the
    /// thread this is. A confirmation raised for one life (a delete card, a confirm card's mode
    /// change) acts on no other: a thread deleted and restored since, under any name, has a
    /// new one. Derived from the log (clients get it as `lifecycle_seq`).
    #[serde(default)]
    pub lifecycle_seq: u64,
    /// Its color, a place in [`crate::palette::THREAD_PALETTE`]: given by [`Registry::apply`]
    /// when it folds the thread's first control event (its `thread.created`, as a rule: what a
    /// client that folds only the control stream sees first too; none for an
    /// orchestrator, which has a color of its own), and again by a `thread.restored` if a
    /// thread alive took it meanwhile. Derived from the log, the same on every client; `None`
    /// for an orchestrator and for a view no registry folded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<u8>,
}

impl ThreadView {
    /// Whether its runtime has background work of its own running (jobs aside, which the hub
    /// runs whatever the runtime does).
    pub fn runtime_tasks(&self) -> bool {
        self.background_tasks.values().any(|t| !t.job)
    }

    /// Whether the thread has background work of its own: a task of its runtime's or a job,
    /// not a `!` command the user runs ([`BackgroundTask::shell`]).
    pub fn own_background(&self) -> bool {
        self.background_tasks.values().any(|t| !t.shell)
    }

    /// An empty view for `id`.
    pub fn new(id: impl Into<String>) -> Self {
        let id = id.into();
        ThreadView {
            name: id.clone(),
            id,
            box_name: "local".into(),
            ..Default::default()
        }
    }

    /// Folds one event of this thread into the view.
    pub fn apply(&mut self, ev: &Event) {
        if self.created_at.is_none() {
            self.created_at = Some(ev.ts);
        }
        // Hub conversation facts, what the hub measures or trims of its worktree, and its
        // jobs are not activity from this runtime.
        if !is_job_task(&ev.source, &ev.body)
            && !matches!(
                ev.body,
                EventBody::ConversationUpdate(_)
                    | EventBody::WorktreeDisk(_)
                    | EventBody::WorktreeTrimmed(_)
                    | EventBody::TaskUpdated(TaskUpdated {
                        heartbeat: true,
                        ..
                    })
            )
        {
            self.last_activity = Some(ev.ts);
        }
        if !matches!(
            ev.body,
            EventBody::ConversationUpdate(_)
                | EventBody::WorktreeDisk(_)
                | EventBody::WorktreeTrimmed(_)
                | EventBody::GitSnapshot(_)
                | EventBody::TaskUpdated(TaskUpdated {
                    heartbeat: true,
                    ..
                })
        ) {
            self.worked_at = Some(ev.ts);
        }
        self.last_seq = self.last_seq.max(ev.seq);

        match &ev.body {
            EventBody::ThreadCreated(p) => {
                self.lifecycle_seq = ev.seq;
                self.name = p.name.clone();
                self.project = p.project.clone();
                self.repo = p.repo.clone();
                self.worktree = p.worktree.clone();
                self.box_name = p.box_name.clone();
                self.runtime = p.runtime.clone();
                self.acp_command = acp_command(p.ext.as_ref()).cloned();
                self.mode = p.mode.clone();
                self.aliases = p.aliases.clone();
                let twr = |key: &str| {
                    p.ext
                        .as_ref()
                        .and_then(|e| e.get("twr"))
                        .and_then(|t| t.get(key))
                        .and_then(|r| r.as_str())
                        .map(str::to_string)
                };
                self.role = twr("role");
                self.base = twr("base_ref");
                self.model = twr("model");
                self.effort = twr("effort");
                self.configured_project = p
                    .ext
                    .as_ref()
                    .and_then(|e| e.get("twr"))
                    .and_then(|t| t.get("configured_project"))
                    .and_then(|c| c.as_bool())
                    .unwrap_or(false);
                self.created_at = Some(ev.ts);
            }
            EventBody::ThreadStarted(p) => {
                // A start of the previous runtime (one that was starting when the thread moved)
                // is not the new runtime's session: another adapter's, one the hub stamped as
                // moved away from, or one stamped as another runtime or agent than the
                // thread's (a start the hub had not seen before the move, say after a hub
                // restart).
                if self.handoff_pending
                    && (!self.is_own_runtime(&ev.source) || moved_away(p) || self.is_other_agent(p))
                {
                    return;
                }
                self.handoff_pending = false;
                self.cut_tasks();
                self.task_tracking_closed = false;
                // Another session starts with a context of its own.
                if p.session_id.is_some() && p.session_id != self.session_id {
                    self.context = None;
                }
                self.compacting = None;
                self.set_session(&p.session_id);
                self.stopped = false;
                self.actual_model = reported_model(p.ext.as_ref()).map(str::to_string);
            }
            EventBody::ThreadResumed(p) => {
                self.set_session(&p.session_id);
                self.set_reported_model(p.ext.as_ref());
                self.task_tracking_closed = false;
                self.parked = false;
                self.stopped = false;
            }
            EventBody::ThreadParked(p) => {
                self.set_session(&p.session_id);
                self.parked = true;
                self.close_for_exit();
            }
            EventBody::ThreadStopped(p) => {
                self.set_session(&p.session_id);
                // A runtime the hub restarts in new settings (mode, model, effort) was not
                // stopped by anyone: the thread keeps its status while its runtime starts again.
                if !is_settings_restart(p) {
                    self.stopped = true;
                }
                self.close_for_exit();
            }
            // Back from the trash (`Registry::apply`): parked, with nothing open, under its
            // name now and at its worktree now. Its aliases stay behind: another thread may
            // have taken them meanwhile. Of its former names, only those the restore kept.
            EventBody::ThreadRestored(p) => {
                self.lifecycle_seq = ev.seq;
                self.name = p.name.clone();
                self.former_names = p.former_names.clone();
                self.former_names.retain(|n| *n != p.name);
                if p.worktree.is_some() {
                    self.worktree = p.worktree.clone();
                }
                self.aliases.clear();
                self.parked = true;
                self.attached = None;
                self.open_attention.clear();
                self.git = None;
                self.close_for_exit();
            }
            EventBody::ThreadRenamed(p) => {
                if p.name != self.name {
                    let previous = std::mem::replace(&mut self.name, p.name.clone());
                    self.former_names.retain(|n| *n != previous && *n != p.name);
                    self.former_names.push(previous);
                }
            }
            EventBody::ThreadModeChanged(p) => self.mode = p.mode.clone(),
            EventBody::ThreadModelChanged(p) => {
                self.model = p.model.clone();
                self.effort = p.effort.clone();
                // Sessions do not move between runtimes: the next start is a new session.
                if let Some(r) = &p.runtime {
                    self.runtime = r.clone();
                    self.acp_command = acp_command(p.ext.as_ref()).cloned();
                    self.session_id = None;
                    self.actual_model = None;
                    self.context = None;
                    self.handoff_pending = true;
                }
            }
            // A turn whose message the user took back is never the crashed one: retrying it
            // would deliver that message again.
            EventBody::ThreadCrashed(_) => {
                self.crashed = true;
                self.crashed_turn = match self.current_turn {
                    // (A compaction is no work to retry.)
                    Some(t) => (!self.current_turn_withdrawn
                        && !self.current_turn_interrupted
                        && !self.current_turn_compact)
                        .then_some(t),
                    None => self
                        .last_turn
                        .as_ref()
                        .filter(|t| {
                            t.result != TurnResult::Completed && !t.withdrawn && !t.interrupted
                        })
                        .map(|t| t.turn),
                };
                self.close_for_exit();
            }
            EventBody::ThreadAttached(p) => {
                self.attached = Some(p.client.clone());
                self.cut_tasks();
                self.task_tracking_closed = true;
            }
            EventBody::ThreadDetached(_) => self.attached = None,
            // A compaction (maintenance) keeps what the last work turn left.
            EventBody::TurnStarted(p) if is_compact_turn(p) => {
                let n = ev.turn.unwrap_or(self.turns + 1);
                self.turns = self.turns.max(n);
                self.current_turn = Some(n);
                self.task_tracking_closed = false;
                self.current_turn_retry_of = None;
                self.current_runtime_turn = None;
                self.current_turn_ref = None;
                self.current_turn_acted = false;
                self.current_turn_withdrawn = false;
                self.current_turn_interrupted = false;
                self.current_turn_compact = true;
                // (A crash before it stays the thread's failure: `crashed`, `crashed_turn`.)
                self.stopped = false;
                self.parked = false;
            }
            EventBody::TurnStarted(p) => {
                let n = ev.turn.unwrap_or(self.turns + 1);
                self.turns = self.turns.max(n);
                self.current_turn = Some(n);
                self.cut_short = None;
                self.task_tracking_closed = false;
                self.current_turn_retry_of = retry_of(p);
                self.current_runtime_turn = runtime_turn(p.ext.as_ref())
                    .filter(|_| p.from == TurnFrom::Runtime)
                    .map(str::to_string);
                self.current_turn_ref = message_ref(p);
                self.current_turn_acted = false;
                self.current_turn_withdrawn = false;
                self.current_turn_interrupted = false;
                if let Some(r) = self.current_turn_retry_of {
                    self.turn_retry_of.insert(n, r);
                }
                self.crashed = false;
                self.crashed_turn = None;
                self.stopped = false;
                self.parked = false;
                self.reported = false;
            }
            EventBody::TurnEnded(p) => {
                self.set_session(&p.session_id);
                self.set_reported_model(p.ext.as_ref());
                if let Some(u) = &p.usage {
                    self.spent.add(u);
                }
                if let Some(c) = recorded_context(p).filter(|_| !self.handoff_pending) {
                    self.context = c;
                }
                self.compacting = None;
                let n = ev.turn.or(self.current_turn).unwrap_or(self.turns);
                // The end of a compaction: maintenance, what the last work turn left stays.
                if self.current_turn == Some(n) && self.current_turn_compact {
                    self.close_turn();
                    return;
                }
                let retry = self.turn_retry_of.get(&n).copied();
                self.last_turn = Some(LastTurn {
                    turn: n,
                    result: p.result,
                    retry_of: retry,
                    withdrawn: self.current_turn == Some(n) && self.current_turn_withdrawn,
                    interrupted: self.current_turn == Some(n) && self.current_turn_interrupted,
                });
                self.last_turn_retry_of = retry;
                if self.current_turn == Some(n) || ev.turn.is_none() {
                    self.close_turn();
                }
                self.reported = false;
            }
            EventBody::TaskUpdated(p) => {
                // A job runs whatever the runtime does; only the hub records one.
                let job = is_job_task(&ev.source, &ev.body);
                if p.running
                    && !p.task_id.is_empty()
                    && (job || (!self.stopped && !self.parked && !self.task_tracking_closed))
                {
                    let task = self
                        .background_tasks
                        .entry(p.task_id.clone())
                        .or_insert_with(|| BackgroundTask {
                            description: p.description.clone(),
                            updated_at: ev.ts,
                            started_seq: ev.seq,
                            job,
                            group: None,
                            quiet: job
                                && p.ext
                                    .as_ref()
                                    .and_then(|e| e.get("twr"))
                                    .and_then(|t| t.get("job"))
                                    .and_then(|j| j.get("wake"))
                                    .and_then(serde_json::Value::as_bool)
                                    == Some(false),
                            shell: job
                                && p.ext
                                    .as_ref()
                                    .and_then(|e| e.get("twr"))
                                    .and_then(|t| t.get("job"))
                                    .is_some_and(|j| j.get("shell").is_some()),
                        });
                    if !p.description.is_empty() {
                        task.description.clone_from(&p.description);
                    }
                    if job {
                        if let Some(group) = job_group(p.ext.as_ref()) {
                            task.group = Some(group);
                        }
                        if let (Some(group), Some(members)) =
                            (task.group.as_mut(), job_members(p.ext.as_ref()))
                        {
                            group.members = members;
                        }
                    }
                    task.updated_at = ev.ts;
                } else if !p.running {
                    self.background_tasks.remove(&p.task_id);
                }
            }
            EventBody::ReportReady(p) => {
                self.set_session(&p.session_id);
                let covers = p.turn.or(ev.turn);
                // (A report of the last work turn that comes while a compaction runs, its
                // reaction late, covers it as well: the compaction's end keeps it.)
                if self.current_turn.is_none() || self.current_turn_compact {
                    if let Some(last) = &self.last_turn {
                        if covers.is_none_or(|t| t == last.turn) {
                            self.reported = true;
                        }
                    }
                }
                // The latest report wins, except that a report for an older turn never
                // replaces the one for a newer turn.
                let older = match (covers, self.last_report.as_ref().and_then(|r| r.turn)) {
                    (Some(new), Some(cur)) => new < cur,
                    _ => false,
                };
                if !older {
                    let mut report = p.clone();
                    report.summary =
                        without_attach_command(&report.summary, &self.name).to_string();
                    self.last_report = Some(report);
                }
            }
            EventBody::AttentionRaised(p) => {
                self.open_attention.insert(p.attn_id.clone(), p.reason);
                if p.reason.is_blocking() && self.in_open_turn(ev) {
                    self.current_turn_acted = true;
                }
            }
            // A call of the orchestrator's memory tools changes no thread: its message can
            // still be taken back.
            EventBody::AgentToolCall(p) if self.in_open_turn(ev) && !is_memory_call(&p.ext) => {
                self.current_turn_acted = true;
            }
            EventBody::TurnWithdrawn(_) if self.in_open_turn(ev) => {
                self.current_turn_withdrawn = true;
            }
            EventBody::TurnInterrupted(_) if self.in_open_turn(ev) => {
                self.current_turn_interrupted = true;
            }
            EventBody::AttentionResolved(p) => {
                self.open_attention.remove(&p.attn_id);
            }
            EventBody::GitSnapshot(p) => self.git = Some(p.clone()),
            EventBody::WorktreeDisk(p) => {
                self.disk = Some(crate::git::WorktreeDisk {
                    bytes: p.bytes,
                    build_bytes: p.build_bytes,
                    partial: p.partial,
                });
            }
            EventBody::WorktreeTrimmed(p) => {
                if let Some(d) = &mut self.disk {
                    let freed = p.freed.min(d.build_bytes);
                    d.build_bytes -= freed;
                    d.bytes = d.bytes.saturating_sub(freed);
                }
                self.trimmed = Some((ev.ts, p.freed));
            }
            EventBody::ContextCompacting(_) => self.compacting = Some(ev.ts),
            EventBody::ContextCompacted(p) => {
                self.compacting = None;
                // A compaction that worked replaced the context: what it left, unknown when it
                // did not say.
                if p.error.is_none() && !self.handoff_pending {
                    let window = self.context.and_then(|c| c.window);
                    self.context = p.post_tokens.map(|tokens| ContextSize { tokens, window });
                }
            }
            _ => {}
        }
    }

    /// Records session id `s`, if given, unless the thread waits for its new runtime's first
    /// session ([`ThreadView::handoff_pending`]): then it is the previous runtime's.
    fn set_session(&mut self, s: &Option<String>) {
        if self.handoff_pending {
            return;
        }
        if let Some(s) = s {
            self.session_id = Some(s.clone());
        }
    }

    /// Records the model `ext` reports ([`reported_model`]), if any, unless the thread waits
    /// for its new runtime's first session ([`ThreadView::handoff_pending`]): then it is the
    /// previous runtime's (its last `turn.ended`, say), and the thread shows the requested
    /// model until the new runtime reports its own.
    fn set_reported_model(&mut self, ext: Option<&JsonMap>) {
        if self.handoff_pending {
            return;
        }
        if let Some(m) = reported_model(ext) {
            self.actual_model = Some(m.to_string());
        }
    }

    /// Whether `source` is this thread's runtime (`adapter:<runtime>`), or no adapter at all.
    /// Two agents of one adapter (`acp`) look alike: see [`moved_away`] and
    /// [`ThreadView::is_other_agent`].
    fn is_own_runtime(&self, source: &Source) -> bool {
        match source {
            Source::Adapter(r) => *r == self.runtime,
            _ => true,
        }
    }

    /// Whether start `p` is stamped as another runtime (`ext.twr.runtime`) or another agent
    /// ([`ACP_COMMAND_KEY`]) than the thread's: the event's source does not tell, as a remote
    /// box's events are recorded as the thread's runtime of the moment. A start without the
    /// stamps (a runner from before them), or a thread without an agent, says nothing.
    fn is_other_agent(&self, p: &SessionPayload) -> bool {
        let runtime = p
            .ext
            .as_ref()
            .and_then(|e| e.get("twr"))
            .and_then(|t| t.get("runtime"))
            .and_then(serde_json::Value::as_str);
        if runtime.is_some_and(|r| r != self.runtime) {
            return true;
        }
        match (acp_command(p.ext.as_ref()), &self.acp_command) {
            (Some(started), Some(own)) => started != own,
            _ => false,
        }
    }

    /// The runtime's background tasks go, unfinished, with its process (a stop, crash, park,
    /// attach or a new runtime start); its jobs stay. The turn they were left running by did
    /// not finish its work: [`ThreadView::cut_short`].
    fn cut_tasks(&mut self) {
        if self.runtime_tasks() {
            self.cut_short = self.last_turn.as_ref().map(|t| t.turn);
        }
        self.background_tasks.retain(|_, t| t.job);
    }

    /// The runtime process is gone (stopped, crashed or parked): no turn can still be open and
    /// nothing can still be waiting on the agent side. Retry links survive in
    /// [`ThreadView::turn_retry_of`].
    fn close_for_exit(&mut self) {
        self.compacting = None;
        self.cut_tasks();
        self.task_tracking_closed = true;
        self.close_turn();
        self.open_attention.retain(|_, r| !r.is_blocking());
    }

    /// No turn is open any more: what was kept of the open one goes.
    fn close_turn(&mut self) {
        self.current_turn = None;
        self.current_turn_retry_of = None;
        self.current_runtime_turn = None;
        self.current_turn_ref = None;
        self.current_turn_acted = false;
        self.current_turn_withdrawn = false;
        self.current_turn_interrupted = false;
        self.current_turn_compact = false;
    }

    /// Whether `ev` was recorded in the currently open turn (not a request of the runtime's
    /// session, which has no turn).
    fn in_open_turn(&self, ev: &Event) -> bool {
        self.current_turn.is_some() && ev.turn == self.current_turn
    }

    /// The turn that failed most recently and has not been superseded: the crashed turn while
    /// [`ThreadView::crashed`], else the last finished turn if it ended `error`. None while a
    /// turn runs.
    pub fn failed_turn(&self) -> Option<u64> {
        if self.current_turn.is_some() {
            return None;
        }
        if self.crashed {
            return self.crashed_turn;
        }
        self.last_turn
            .as_ref()
            .filter(|t| t.result == TurnResult::Error)
            .map(|t| t.turn)
    }

    /// Whether the hub may start one automatic retry: the last turn failed (ended `error`
    /// or crashed), it is not itself a retry, no retry of it has started, and the thread is
    /// neither stopped nor parked.
    pub fn retry_eligible(&self) -> bool {
        // A job (the hub's) does not hold a retry back: only the runtime's own work does.
        if self.stopped || self.parked || self.current_turn.is_some() || self.runtime_tasks() {
            return false;
        }
        let Some(failed) = self.failed_turn() else {
            return false;
        };
        !self.turn_retry_of.contains_key(&failed)
            && !self.turn_retry_of.values().any(|&orig| orig == failed)
    }

    /// Whether an open permission/question blocks this thread.
    pub fn is_blocked(&self) -> bool {
        self.open_attention.values().any(|r| r.is_blocking())
    }

    /// Derived status; see the precedence table on [`ThreadStatus`].
    pub fn status(&self) -> ThreadStatus {
        if self.attached.is_some() {
            ThreadStatus::Attached
        } else if self.parked {
            ThreadStatus::Parked
        } else if self.stopped {
            ThreadStatus::Done
        } else if self.is_blocked() {
            ThreadStatus::Waiting
        } else if self.current_turn.is_some() {
            ThreadStatus::Running
        } else if !self.background_tasks.is_empty() {
            ThreadStatus::Background
        } else if self.crashed
            || self
                .last_turn
                .as_ref()
                .is_some_and(|t| t.result == TurnResult::Error)
        {
            ThreadStatus::Error
        } else if self.reported
            && self
                .last_turn
                .as_ref()
                .is_some_and(|t| t.result == TurnResult::Completed)
        {
            ThreadStatus::Done
        } else {
            ThreadStatus::Idle
        }
    }

    /// Status for compact displays, including work still running alongside a turn.
    pub fn status_label(&self) -> String {
        let status = self.status();
        if self.background_tasks.is_empty() {
            status.to_string()
        } else {
            format!("{status} ({} bg)", self.background_tasks.len())
        }
    }

    /// A foreground turn or session-scoped task still has work to do.
    pub fn has_work(&self) -> bool {
        self.current_turn.is_some() || !self.background_tasks.is_empty()
    }

    /// Whether its runtime has work: a turn open or background tasks of its own (not jobs).
    pub fn runtime_work(&self) -> bool {
        self.current_turn.is_some() || self.runtime_tasks()
    }

    /// The model to show for this thread: the one its runtime reports, else the requested one.
    /// `None`: the runtime's default, not reported (yet).
    pub fn shown_model(&self) -> Option<&str> {
        self.actual_model.as_deref().or(self.model.as_deref())
    }

    /// Whether this is an orchestrator thread: the current one, or a retired one
    /// ([`ThreadView::retired`]).
    pub fn is_orchestrator(&self) -> bool {
        self.role.as_deref() == Some("orchestrator")
    }

    /// Whether this is the current orchestrator thread (an orchestrator that is not retired).
    pub fn is_current_orchestrator(&self) -> bool {
        self.is_orchestrator() && !self.retired
    }
}

/// Whether start `p` is stamped as a runtime the thread moved away from
/// ([`RUNTIME_MOVED_KEY`]).
fn moved_away(p: &SessionPayload) -> bool {
    p.ext
        .as_ref()
        .and_then(|e| e.get("twr"))
        .and_then(|t| t.get(RUNTIME_MOVED_KEY))
        == Some(&serde_json::Value::Bool(true))
}

/// The model a runtime reports in an event's `ext`: a non-empty string `model` in a runtime's
/// section (`ext.claude.model` from Claude's `system/init`, `ext.codex.model` from Codex's
/// thread result, `ext.<dialect>.model` from an ACP agent). Never `ext.twr.model`, which is
/// the requested model (`thread.created`).
pub fn reported_model(ext: Option<&JsonMap>) -> Option<&str> {
    ext?.iter()
        .filter(|(k, _)| k.as_str() != "twr")
        .find_map(|(_, v)| v.get("model")?.as_str().filter(|m| !m.trim().is_empty()))
}

/// What turn end `p` says of its session's context: `None` nothing (the context stays as it
/// was); `Some(None)` that its size is unknown from then (a Claude Code turn that called the
/// model without reporting a call's size, `ext.claude.context_from: "unknown"`, or a size that
/// cannot be trusted, below); `Some(Some(c))` the size.
///
/// A Claude Code `turn.ended` (`ext.claude`) recorded before twr read the main agent's last
/// model call has no `ext.claude.context_from`, and its tokens are the `result`'s `usage`: the
/// sum of every model call of the turn. That is how full the context was for a turn of one call
/// (`num_turns: 1`) or of none (`usage` input 0: what a compaction left). For a turn of several
/// it counts the context once per call (a research turn of 62 calls recorded 6.9M tokens, 693%
/// of a 1M window, when its last call held 184k), and the event does not have the last call's
/// size: unknown. Such events stay in the log as they were.
pub fn recorded_context(p: &TurnEnded) -> Option<Option<ContextSize>> {
    // `twr_adapters::claude_code::EXT`.
    let claude = p.ext.as_ref().and_then(|e| e.get("claude"));
    let from = claude.and_then(|c| c.get("context_from"));
    let Some(c) = p.context else {
        return (from.and_then(serde_json::Value::as_str) == Some("unknown")).then_some(None);
    };
    let Some(claude) = claude.filter(|_| from.is_none()) else {
        return Some(Some(c));
    };
    let input = p.usage.as_ref().map_or(0, |u| u.input);
    let calls = claude.get("num_turns").and_then(serde_json::Value::as_u64);
    Some((input == 0 || calls == Some(1)).then_some(c))
}

fn retry_of(p: &TurnStarted) -> Option<u64> {
    p.ext.as_ref()?.get("twr")?.get("retry_of")?.as_u64()
}

// ---------------------------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------------------------

/// Why a thread reference did not resolve.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    /// Nothing matched.
    #[error("no thread matches {0:?}")]
    NotFound(String),
    /// Several threads matched a prefix, or share the queried name/alias.
    #[error("{query:?} is ambiguous: {candidates:?}")]
    Ambiguous {
        /// The query.
        query: String,
        /// Names of the matching threads.
        candidates: Vec<String>,
    },
}

/// All thread views, keyed by thread id. The reserved [`HUB_THREAD`] is never listed, nor is
/// a deleted thread (`thread.deleted`): its view is set aside, and every later event of it is
/// dropped until a `thread.restored` brings it back as it was.
///
/// Serialized whole in the hub's checkpoints of its folded state.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Registry {
    threads: BTreeMap<String, ThreadView>,
    /// Deleted threads, by id, with their views as they were when deleted.
    deleted: HashMap<String, ThreadView>,
}

impl Registry {
    /// Empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds a registry from a sequence of events.
    pub fn from_events<'a>(events: impl IntoIterator<Item = &'a Event>) -> Self {
        let mut r = Self::new();
        for ev in events {
            r.apply(ev);
        }
        r
    }

    /// Folds an event; events on [`HUB_THREAD`] are ignored.
    ///
    /// A `thread.created` with role `orchestrator` makes that thread the orchestrator and
    /// retires every other orchestrator thread ([`ThreadView::retired`]): the log's order
    /// decides, live and on replay alike.
    ///
    /// A `thread.deleted` sets the thread's view aside ([`Registry::deleted_view`]); a later
    /// `thread.restored` of it puts that view back (and applies itself), every other event of
    /// it is dropped.
    ///
    /// A thread's first control event (its `thread.created`, as a rule) gives it a color
    /// ([`ThreadView::color`], [`crate::palette::allocate`]) from those of the threads alive
    /// then, which an orchestrator's `thread.created` takes back; a restored thread keeps its
    /// own unless a thread alive took it meanwhile ([`crate::palette::keeps`]).
    pub fn apply(&mut self, ev: &Event) {
        if ev.thread == HUB_THREAD {
            return;
        }
        if let Some(view) = self.deleted.get(&ev.thread) {
            if matches!(ev.body, EventBody::ThreadRestored(_)) {
                let mut view = view.clone();
                self.deleted.remove(&ev.thread);
                view.apply(ev);
                if !view.is_orchestrator() {
                    let held = self.held_colors(&ev.thread);
                    if !view
                        .color
                        .is_some_and(|c| crate::palette::keeps(c, held.clone()))
                    {
                        view.color = Some(crate::palette::allocate(held));
                    }
                }
                self.threads.insert(ev.thread.clone(), view);
            }
            return;
        }
        if matches!(ev.body, EventBody::ThreadDeleted(_)) {
            let view = self
                .threads
                .remove(&ev.thread)
                .unwrap_or_else(|| ThreadView::new(ev.thread.clone()));
            self.deleted.insert(ev.thread.clone(), view);
            return;
        }
        let created = matches!(ev.body, EventBody::ThreadCreated(_));
        // Only a control event gives a color: a client folds the control stream alone.
        let colorless = |t: &ThreadView| t.color.is_none() && !t.is_orchestrator();
        let held = (ev.is_control() && self.threads.get(&ev.thread).is_none_or(colorless))
            .then(|| self.held_colors(&ev.thread));
        let view = self
            .threads
            .entry(ev.thread.clone())
            .or_insert_with(|| ThreadView::new(ev.thread.clone()));
        view.apply(ev);
        if view.is_orchestrator() {
            view.color = None;
        } else if let Some(held) = held.filter(|_| view.color.is_none()) {
            view.color = Some(crate::palette::allocate(held));
        }
        let new_orchestrator = created && view.is_orchestrator();
        if new_orchestrator {
            for (id, t) in self.threads.iter_mut() {
                if *id != ev.thread && t.is_orchestrator() {
                    t.retired = true;
                }
            }
        }
    }

    /// The colors ([`ThreadView::color`]) the threads alive other than `id` hold, one entry
    /// per thread.
    fn held_colors(&self, id: &str) -> Vec<u8> {
        self.threads
            .values()
            .filter(|t| t.id != id)
            .filter_map(|t| t.color)
            .collect()
    }

    /// The current orchestrator thread (the latest one created), if any.
    pub fn current_orchestrator(&self) -> Option<&ThreadView> {
        self.threads.values().find(|t| t.is_current_orchestrator())
    }

    /// View by thread id.
    pub fn get(&self, id: &str) -> Option<&ThreadView> {
        self.threads.get(id)
    }

    /// Whether thread `id` was deleted (`thread.deleted`) and not restored since.
    pub fn is_deleted(&self, id: &str) -> bool {
        self.deleted.contains_key(id)
    }

    /// The view of deleted thread `id` as it was when it was deleted.
    pub fn deleted_view(&self, id: &str) -> Option<&ThreadView> {
        self.deleted.get(id)
    }

    /// The views of the deleted threads ([`Registry::deleted_view`]), in no order.
    pub fn deleted_views(&self) -> impl Iterator<Item = &ThreadView> {
        self.deleted.values()
    }

    /// Number of threads.
    pub fn len(&self) -> usize {
        self.threads.len()
    }

    /// Whether there are no threads.
    pub fn is_empty(&self) -> bool {
        self.threads.is_empty()
    }

    /// All threads, oldest first (by creation time, then id).
    pub fn list(&self) -> Vec<&ThreadView> {
        let mut v: Vec<&ThreadView> = self.threads.values().collect();
        v.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
        v
    }

    /// Resolves a user reference: exact id, then exact name, then alias, then a unique prefix
    /// of a name or id.
    ///
    /// Several threads with the same name (or alias) resolve to the one that is not stopped
    /// when exactly one is live; otherwise the query is [`ResolveError::Ambiguous`]. Retired
    /// orchestrators ([`ThreadView::retired`]) count only when nothing else matches, so
    /// `orchestrator` / `orch` name the current orchestrator (a retired one keeps its name,
    /// orchestrators are never renamed; its id still reaches it). A name a thread had before
    /// it was renamed ([`ThreadView::former_names`]) reaches it after its names and aliases;
    /// one that several threads had is ambiguous (never a prefix of another's name).
    pub fn resolve(&self, query: &str) -> Result<&ThreadView, ResolveError> {
        if let Some(t) = self.threads.get(query) {
            return Ok(t);
        }
        let by_name: Vec<&ThreadView> = self.threads.values().filter(|t| t.name == query).collect();
        if let Some(r) = pick_duplicate(query, prefer_unretired(by_name)) {
            return r;
        }
        let by_alias: Vec<&ThreadView> = self
            .threads
            .values()
            .filter(|t| t.aliases.iter().any(|a| a == query))
            .collect();
        if let Some(r) = pick_duplicate(query, prefer_unretired(by_alias)) {
            return r;
        }
        let by_former: Vec<&ThreadView> = self
            .threads
            .values()
            .filter(|t| t.former_names.iter().any(|n| n == query))
            .collect();
        match by_former.as_slice() {
            [] => {}
            [one] => return Ok(one),
            many => {
                return Err(ResolveError::Ambiguous {
                    query: query.to_string(),
                    candidates: many.iter().map(|t| t.name.clone()).collect(),
                })
            }
        }
        let matches: Vec<&ThreadView> = if query.is_empty() {
            Vec::new()
        } else {
            prefer_unretired(
                self.threads
                    .values()
                    .filter(|t| t.name.starts_with(query) || t.id.starts_with(query))
                    .collect(),
            )
        };
        match matches.as_slice() {
            [one] => Ok(one),
            [] => Err(ResolveError::NotFound(query.to_string())),
            many => Err(ResolveError::Ambiguous {
                query: query.to_string(),
                candidates: many.iter().map(|t| t.name.clone()).collect(),
            }),
        }
    }
}

/// `matches` without retired orchestrators, unless those are all there is.
fn prefer_unretired(matches: Vec<&ThreadView>) -> Vec<&ThreadView> {
    if matches.iter().all(|t| t.retired) {
        return matches;
    }
    matches.into_iter().filter(|t| !t.retired).collect()
}

/// Exact name/alias matches: none -> `None`; one -> it; several -> the only non-stopped one,
/// else ambiguous.
fn pick_duplicate<'a>(
    query: &str,
    matches: Vec<&'a ThreadView>,
) -> Option<Result<&'a ThreadView, ResolveError>> {
    match matches.as_slice() {
        [] => None,
        [one] => Some(Ok(one)),
        many => {
            let live: Vec<&ThreadView> = many.iter().copied().filter(|t| !t.stopped).collect();
            Some(match live.as_slice() {
                [one] => Ok(one),
                _ => Err(ResolveError::Ambiguous {
                    query: query.to_string(),
                    candidates: many.iter().map(|t| t.name.clone()).collect(),
                }),
            })
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Attention
// ---------------------------------------------------------------------------------------------

/// One raised-but-unresolved attention item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttentionItem {
    /// Attention id.
    pub attn_id: String,
    /// Thread it belongs to (may be [`HUB_THREAD`] for hub-level questions).
    pub thread: String,
    /// Reason.
    pub reason: AttentionReason,
    /// Summary.
    pub summary: String,
    /// Answer options.
    pub options: Vec<String>,
    /// Related tool call.
    pub call_id: Option<String>,
    /// Latest orchestrator annotation.
    pub note: Option<String>,
    /// When it was raised.
    pub raised_at: DateTime<Utc>,
    /// Id of the `attention.raised` event (deterministic tie-break).
    pub event_id: String,
    /// The item's current *revision*: the control `gseq` of its latest `attention.raised` (an
    /// open item raised again, e.g. a permission re-sent with new input, is a new revision of
    /// the same item). An answer names the revision it was given for (`attention.respond`
    /// `revision`), so an answer to what the user saw never answers what the item asks now.
    /// 0 when unknown (folded without a gseq).
    #[serde(default)]
    pub revision: u64,
    /// The revision of the raise that opened the item (equal to `revision` until it is raised
    /// again). 0 when unknown.
    #[serde(default)]
    pub first_revision: u64,
    /// A confirm card ([`is_confirm_card`]): the hub's question listing the orchestrator's
    /// proposals, which only the user answers.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub proposal: bool,
    /// What a permission or question was raised in ([`RequestContext::of`]), when its event
    /// says anything about it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<RequestContext>,
}

/// The context a worker's permission or question was raised in, for the user to judge it
/// without asking the orchestrator: why the runtime says it asks and where it runs
/// (`ext.<runtime>.reason` or `description`, and `cwd`), the whole input when the summary is
/// cut (`ext.<runtime>.full_input`), and what the agent last said and the turn's prompt
/// (`ext.twr.context`, added by the hub).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RequestContext {
    /// Why the runtime asks, in its own words.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    /// Where the command would run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// What the agent last said before it asked (its end).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub said: Option<String>,
    /// The first line of the turn's prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    /// The request's whole input (its command), marked so that every character shows, when
    /// the summary shows only its start: the runtime section's `full_input`
    /// (`twr_adapters::claude_code::translate::FULL_INPUT_KEY`). Where the user approves it,
    /// this is shown in full.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
}

impl RequestContext {
    /// The context of a raised item of `reason` with `ext`: `None` for other reasons, and
    /// when the event says nothing about it.
    pub fn of(reason: AttentionReason, ext: &Option<JsonMap>) -> Option<Self> {
        if !matches!(
            reason,
            AttentionReason::Permission | AttentionReason::Question
        ) {
            return None;
        }
        let ext = ext.as_ref()?;
        let text = |v: Option<&serde_json::Value>| {
            v.and_then(serde_json::Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        // The runtime's own section (`claude`, `codex`, an ACP dialect's): the first that says.
        let runtime = |key: &str| {
            ext.iter()
                .filter(|(k, _)| k.as_str() != "twr")
                .find_map(|(_, v)| text(v.get(key)))
        };
        let twr = ext.get("twr").and_then(|t| t.get("context"));
        let c = Self {
            why: runtime("reason").or_else(|| runtime("description")),
            cwd: runtime("cwd"),
            said: text(twr.and_then(|c| c.get("said"))),
            task: text(twr.and_then(|c| c.get("task"))),
            // As marked, never trimmed: its ends are part of what it shows.
            input: ext
                .iter()
                .filter(|(k, _)| k.as_str() != "twr")
                .find_map(|(_, v)| v.get("full_input").and_then(serde_json::Value::as_str))
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        };
        (c != Self::default()).then_some(c)
    }
}

impl AttentionItem {
    /// Whether the client-local pin set pins this item (by attn_id or by thread id).
    pub fn is_pinned(&self, pins: &HashSet<String>) -> bool {
        pins.contains(&self.attn_id) || pins.contains(&self.thread)
    }
}

/// Attention ordering: reason priority (permission, question, error, review, stalled), then
/// age (older first), then pin (pinned first), then event id for determinism.
///
/// Pins are client-local view state; an entry pins an item when it equals the
/// item's attn_id or thread id.
pub fn compare_attention(a: &AttentionItem, b: &AttentionItem, pins: &HashSet<String>) -> Ordering {
    a.reason
        .priority()
        .cmp(&b.reason.priority())
        .then(a.raised_at.cmp(&b.raised_at))
        .then(b.is_pinned(pins).cmp(&a.is_pinned(pins)))
        .then(a.event_id.cmp(&b.event_id))
}

/// Open attention items across all threads (including [`HUB_THREAD`]). Serialized whole in the
/// hub's checkpoints of its folded state.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AttentionStack {
    items: BTreeMap<String, AttentionItem>,
}

impl AttentionStack {
    /// Empty stack.
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds a stack from a sequence of events.
    pub fn from_events<'a>(events: impl IntoIterator<Item = &'a Event>) -> Self {
        let mut s = Self::new();
        for ev in events {
            s.apply(ev);
        }
        s
    }

    /// Folds an event. `attention.raised` opens an item, `attention.annotated` updates its
    /// note in place, `attention.resolved` removes it. `thread.crashed` / `thread.stopped` /
    /// `thread.parked` drop that thread's blocking (permission/question) items, matching
    /// [`ThreadView`].
    ///
    /// Re-raising an open attn_id updates the item's thread, reason, summary, options and
    /// call_id but keeps its original `raised_at`, `event_id` and note, so it does not jump in
    /// the age order.
    ///
    /// Items keep no revision (0): use [`AttentionStack::apply_at`] with the event's control
    /// gseq where it is known.
    pub fn apply(&mut self, ev: &Event) {
        self.apply_at(ev, None);
    }

    /// [`AttentionStack::apply`] for a control event at control position `gseq`: an
    /// `attention.raised` sets the item's [`AttentionItem::revision`] to it (and, when it
    /// opens the item, [`AttentionItem::first_revision`]).
    pub fn apply_at(&mut self, ev: &Event, gseq: Option<u64>) {
        let revision = gseq.unwrap_or(0);
        match &ev.body {
            EventBody::AttentionRaised(p) => {
                if let Some(item) = self.items.get_mut(&p.attn_id) {
                    item.thread = ev.thread.clone();
                    item.reason = p.reason;
                    item.summary = p.summary.clone();
                    item.options = p.options.clone();
                    item.call_id = p.call_id.clone();
                    item.revision = revision;
                    item.proposal = is_confirm_card(&ev.thread, &p.ext);
                    item.context = RequestContext::of(p.reason, &p.ext);
                    return;
                }
                self.items.insert(
                    p.attn_id.clone(),
                    AttentionItem {
                        attn_id: p.attn_id.clone(),
                        thread: ev.thread.clone(),
                        reason: p.reason,
                        summary: p.summary.clone(),
                        options: p.options.clone(),
                        call_id: p.call_id.clone(),
                        note: None,
                        raised_at: ev.ts,
                        event_id: ev.id.clone(),
                        revision,
                        first_revision: revision,
                        proposal: is_confirm_card(&ev.thread, &p.ext),
                        context: RequestContext::of(p.reason, &p.ext),
                    },
                );
            }
            EventBody::AttentionAnnotated(p) => {
                if let Some(item) = self.items.get_mut(&p.attn_id) {
                    item.note = Some(p.note.clone());
                }
            }
            EventBody::AttentionResolved(p) => {
                self.items.remove(&p.attn_id);
            }
            EventBody::ThreadCrashed(_)
            | EventBody::ThreadStopped(_)
            | EventBody::ThreadParked(_) => {
                self.items
                    .retain(|_, i| i.thread != ev.thread || !i.reason.is_blocking());
            }
            // Nothing of a deleted thread stays open.
            EventBody::ThreadDeleted(_) => self.items.retain(|_, i| i.thread != ev.thread),
            _ => {}
        }
    }

    /// Item by attn_id.
    pub fn get(&self, attn_id: &str) -> Option<&AttentionItem> {
        self.items.get(attn_id)
    }

    /// Number of open items.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether nothing is open.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// All open items, ordered by [`compare_attention`].
    pub fn ordered(&self, pins: &HashSet<String>) -> Vec<&AttentionItem> {
        let mut v: Vec<&AttentionItem> = self.items.values().collect();
        v.sort_by(|a, b| compare_attention(a, b, pins));
        v
    }

    /// The most urgent item.
    pub fn top(&self, pins: &HashSet<String>) -> Option<&AttentionItem> {
        self.items
            .values()
            .min_by(|a, b| compare_attention(a, b, pins))
    }

    /// Open items of one thread, ordered.
    pub fn for_thread(&self, thread: &str, pins: &HashSet<String>) -> Vec<&AttentionItem> {
        let mut v: Vec<&AttentionItem> =
            self.items.values().filter(|i| i.thread == thread).collect();
        v.sort_by(|a, b| compare_attention(a, b, pins));
        v
    }
}

/// Left-to-right order of threads in the stack view (the first is the default focus).
///
/// Threads with open attention come first, sorted by their top item under
/// [`compare_attention`]. The rest follow by status ([`ThreadStatus::stack_rank`]: working
/// (running, background, waiting, attached) > error > done > idle > parked), then, with
/// [`CardOrder::Stable`], pinned threads first and then by creation (oldest first), so a
/// thread keeps its place in its group however often it acts; with [`CardOrder::Attention`],
/// by last activity (newest first), then pin. Then by id. [`HUB_THREAD`] is never included.
pub fn stack_order(
    registry: &Registry,
    attention: &AttentionStack,
    pins: &HashSet<String>,
    mode: CardOrder,
) -> Vec<String> {
    let mut with_attn: Vec<(&AttentionItem, &ThreadView)> = Vec::new();
    let mut without: Vec<&ThreadView> = Vec::new();
    for t in registry.threads.values() {
        match attention.for_thread(&t.id, pins).first() {
            Some(top) => with_attn.push((top, t)),
            None => without.push(t),
        }
    }
    with_attn.sort_by(|(a, _), (b, _)| compare_attention(a, b, pins));
    let pinned = |t: &ThreadView| pins.contains(&t.id);
    without.sort_by(|a, b| {
        let group = a.status().stack_rank().cmp(&b.status().stack_rank());
        match mode {
            CardOrder::Stable => group
                .then(pinned(b).cmp(&pinned(a)))
                .then(a.created_at.cmp(&b.created_at)),
            CardOrder::Attention => group
                .then(Reverse(a.last_activity).cmp(&Reverse(b.last_activity)))
                .then(pinned(b).cmp(&pinned(a))),
        }
        .then(a.id.cmp(&b.id))
    });
    with_attn
        .into_iter()
        .map(|(_, t)| t.id.clone())
        .chain(without.into_iter().map(|t| t.id.clone()))
        .collect()
}

/// `summary` of a report of thread `name` without the ` — twr attach <name>` (or ` — twr -w
/// <workspace> attach <name>`) the hub ended every report with until 2026-10, its own and
/// written ones: a log keeps them, and a thread's card and activity line show its last report.
/// The log is folded in order, so `name` is the one the hub wrote then; a command that attaches
/// another thread is the report's own text.
fn without_attach_command<'a>(summary: &'a str, name: &str) -> &'a str {
    let Some((body, tail)) = summary.rsplit_once(" — ") else {
        return summary;
    };
    let command = match tail.split(' ').collect::<Vec<_>>()[..] {
        ["twr", "attach", n] => n == name,
        ["twr", "-w", workspace, "attach", n] => {
            !workspace.is_empty() && !workspace.contains(char::is_whitespace) && n == name
        }
        _ => false,
    };
    if command {
        body
    } else {
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::ThreadScript;
    use AttentionReason::*;
    use ThreadStatus as S;

    /// A job (a `job:` task the hub records) outlives the runtime: a stop, a restart or an
    /// attach clears the runtime's own tasks only, and only the job's end removes it. A `job:`
    /// task an adapter reports is a runtime task like any other.
    #[test]
    fn background_work_ended_unfinished_is_kept_against_its_turn() {
        let mut t = ThreadScript::new("t1");
        let mut v = ThreadView::new("t1");
        v.apply(&t.created("a"));
        v.apply(&t.event(EventBody::ThreadStarted(SessionPayload::default())));
        v.apply(&t.turn_started("build it"));
        v.apply(&t.event(EventBody::TaskUpdated(TaskUpdated {
            task_id: "bash_1".into(),
            running: true,
            description: "x".into(),
            ..Default::default()
        })));
        v.apply(&t.turn_ended(TurnResult::Completed));
        assert_eq!(v.cut_short, None);
        // A park ends it: kept across the resume, until a turn starts.
        v.apply(&t.event(EventBody::ThreadParked(Default::default())));
        assert_eq!(v.cut_short, Some(1));
        v.apply(&t.event(EventBody::ThreadStarted(SessionPayload::default())));
        assert_eq!(v.cut_short, Some(1));
        v.apply(&t.turn_started("again"));
        assert_eq!(v.cut_short, None);
        // Work that ended by itself was not cut short.
        v.apply(&t.turn_ended(TurnResult::Completed));
        v.apply(&t.event(EventBody::ThreadStopped(SessionPayload::default())));
        assert_eq!(v.cut_short, None);
    }

    #[test]
    fn jobs_outlive_the_runtime() {
        let mut t = ThreadScript::new("t1");
        let mut v = ThreadView::new("t1");
        v.apply(&t.created("a"));
        v.apply(&t.event(EventBody::ThreadStarted(SessionPayload::default())));
        let task = |t: &mut ThreadScript, id: &str, running: bool, source: Source| {
            let mut ev = t.event(EventBody::TaskUpdated(TaskUpdated {
                task_id: id.into(),
                running,
                description: "x".into(),
                ..Default::default()
            }));
            ev.source = source;
            ev
        };
        v.apply(&task(&mut t, "job:J1", true, Source::Hub));
        v.apply(&task(
            &mut t,
            "bash_1",
            true,
            Source::Adapter("fake".into()),
        ));
        v.apply(&task(
            &mut t,
            "job:J2",
            true,
            Source::Adapter("fake".into()),
        ));
        assert_eq!(v.background_tasks.len(), 3);
        assert!(v.runtime_tasks());
        v.apply(&t.event(EventBody::ThreadStopped(SessionPayload::default())));
        assert_eq!(v.background_tasks.keys().collect::<Vec<_>>(), ["job:J1"]);
        assert!(v.background_tasks["job:J1"].job && !v.runtime_tasks());
        // Recorded while stopped, a job still counts; a runtime task does not.
        v.apply(&task(&mut t, "job:J3", true, Source::Hub));
        v.apply(&task(
            &mut t,
            "bash_2",
            true,
            Source::Adapter("fake".into()),
        ));
        assert_eq!(v.background_tasks.len(), 2);
        v.apply(&task(&mut t, "job:J1", false, Source::Hub));
        v.apply(&task(&mut t, "job:J3", false, Source::Hub));
        assert!(v.background_tasks.is_empty());
    }

    /// A compaction (maintenance) leaves what the last work turn left: its last turn, its
    /// report, and a crash that took it down (no `turn.ended`) as the thread's failure.
    #[test]
    fn a_compaction_keeps_the_last_work_turns_outcome() {
        let mut t = ThreadScript::new("t1");
        let mut v = ThreadView::new("t1");
        v.apply(&t.created("a"));
        v.apply(&t.event(EventBody::ThreadStarted(SessionPayload::default())));
        v.apply(&t.turn_started("build it"));
        v.apply(&t.event(EventBody::ThreadCrashed(Default::default())));
        assert_eq!(
            (v.crashed, v.crashed_turn, v.failed_turn()),
            (true, Some(1), Some(1))
        );
        let compact_of = |t: &mut ThreadScript| {
            let mut compact = t.turn_started("/compact");
            if let EventBody::TurnStarted(p) = &mut compact.body {
                set_twr_ext(&mut p.ext, COMPACT_TURN_KEY, serde_json::json!(true));
            }
            compact
        };
        v.apply(&compact_of(&mut t));
        assert!(v.current_turn_compact);
        v.apply(&t.turn_ended(TurnResult::Completed));
        assert!(v.current_turn.is_none() && !v.current_turn_compact);
        assert_eq!(
            (v.crashed, v.crashed_turn, v.failed_turn()),
            (true, Some(1), Some(1))
        );
        assert_eq!(v.status(), ThreadStatus::Error);
        assert_eq!(v.turns, 2);
        // The work turn's report, late, while a compaction runs, covers it all the same.
        let mut v = ThreadView::new("t1");
        let mut t = ThreadScript::new("t1");
        v.apply(&t.created("a"));
        v.apply(&t.turn_started("build it"));
        v.apply(&t.turn_ended(TurnResult::Completed));
        v.apply(&compact_of(&mut t));
        v.apply(&t.report(Some(1), Grade::Success));
        assert!(v.reported, "covers the work turn");
        assert_eq!(v.last_report.as_ref().and_then(|r| r.turn), Some(1));
        v.apply(&t.turn_ended(TurnResult::Completed));
        assert!(v.reported && v.current_turn.is_none());
        // A work turn after it is the thread's again.
        v.apply(&t.turn_started("again"));
        v.apply(&t.turn_ended(TurnResult::Completed));
        assert_eq!(v.last_turn.as_ref().map(|l| l.turn), Some(3));
        assert!(!v.crashed);
    }

    /// A thread's view sums what its turns spent; a turn without usage adds nothing.
    #[test]
    fn spent_sums_the_turns_usage() {
        let mut t = ThreadScript::new("t1");
        let mut v = ThreadView::new("t1");
        let with = |mut ev: Event, input, output, cost| {
            if let EventBody::TurnEnded(p) = &mut ev.body {
                p.usage = Some(crate::event::Usage {
                    input,
                    output,
                    cost,
                });
            }
            ev
        };
        v.apply(&t.created("a"));
        v.apply(&t.turn_started("one"));
        v.apply(&with(
            t.turn_ended(TurnResult::Completed),
            1000,
            10,
            Some(0.5),
        ));
        v.apply(&t.turn_started("two"));
        v.apply(&t.turn_ended(TurnResult::Error));
        v.apply(&t.turn_started("three"));
        v.apply(&with(t.turn_ended(TurnResult::Completed), 200, 20, None));
        assert_eq!((v.spent.input, v.spent.output), (1200, 30));
        assert_eq!((v.spent.turns, v.spent.costed_turns), (2, 1));
        assert_eq!(v.spent.cost(), Some(0.5));
        let json = serde_json::to_value(&v).unwrap();
        assert_eq!(json["spent"]["input"], 1200);
        // An older thread view, without `spent`, reads as nothing spent.
        let mut old = json.clone();
        old.as_object_mut().unwrap().remove("spent");
        let back: ThreadView = serde_json::from_value(old).unwrap();
        assert!(back.spent.is_empty());
        // Nothing spent is left out of the JSON.
        let fresh = serde_json::to_value(ThreadView::new("t2")).unwrap();
        assert!(fresh.get("spent").is_none());
    }

    /// How full the context is folds from the turns' ends. A Claude Code turn end recorded
    /// before twr read the last call (no `ext.claude.context_from`) holds the turn's summed
    /// usage: trusted for a turn of one call or none (a compaction's), unknown for a turn of
    /// several (the 693% card: 62 calls, 6.9M of a 1M window), whatever the earlier size was. A
    /// turn end that says its size is unknown makes it so; one that says nothing keeps it.
    /// Other runtimes' sizes are taken as they are. A compaction that worked sets what it left,
    /// unknown when it does not say.
    #[test]
    fn a_claude_turns_recorded_sum_is_no_context_size() {
        use serde_json::json;
        let mut t = ThreadScript::new("t1");
        let mut v = ThreadView::new("t1");
        v.apply(&t.created("a"));
        let size = |tokens| ContextSize {
            tokens,
            window: Some(1_000_000),
        };
        let mut ended = |input: u64, tokens: Option<u64>, claude: Option<serde_json::Value>| {
            let mut ev = t.turn_ended(TurnResult::Completed);
            if let EventBody::TurnEnded(p) = &mut ev.body {
                p.usage = Some(crate::event::Usage {
                    input,
                    output: 1,
                    cost: None,
                });
                p.context = tokens.map(size);
                p.ext = claude.map(|c| {
                    let mut m = JsonMap::new();
                    m.insert("claude".into(), c);
                    m
                });
            }
            ev
        };
        let legacy = |n: u64| json!({"num_turns": n, "subtype": "success"});
        // One call: its sum is its size.
        v.apply(&ended(150_000, Some(150_000), Some(legacy(1))));
        assert_eq!(v.context, Some(size(150_000)));
        // Several: unknown, not the previous size.
        v.apply(&ended(6_932_444, Some(6_932_444), Some(legacy(64))));
        assert_eq!(v.context, None);
        assert!(serde_json::to_value(&v).unwrap().get("context").is_none());
        // What a compaction left (no call, usage 0) holds.
        v.apply(&ended(0, Some(40_000), Some(legacy(0))));
        assert_eq!(v.context, Some(size(40_000)));
        // Several calls under the window are no better.
        v.apply(&ended(300_000, Some(300_000), Some(legacy(3))));
        assert_eq!(v.context, None);
        // A turn end of this twr: the last call's size, trusted.
        let read = |from: &str| json!({"num_turns": 64, "context_from": from});
        v.apply(&ended(6_932_444, Some(183_861), Some(read("last_call"))));
        assert_eq!(v.context, Some(size(183_861)));
        assert_eq!(v.context.unwrap().percent(), Some(18));
        // Nothing said (no call): kept.
        v.apply(&ended(0, None, Some(json!({"num_turns": 0}))));
        assert_eq!(v.context, Some(size(183_861)));
        // Calls without a size: unknown.
        v.apply(&ended(90_000, None, Some(read("unknown"))));
        assert_eq!(v.context, None);
        // Another runtime's size is its own.
        v.apply(&ended(500_000, Some(120_000), None));
        assert_eq!(v.context, Some(size(120_000)));
        // A compaction replaces it with what it left, the window kept; one that failed leaves
        // it; one that worked without saying makes it unknown.
        let compacted = |post: Option<u64>, error: Option<&str>| {
            EventBody::ContextCompacted(ContextCompacted {
                trigger: "manual".into(),
                post_tokens: post,
                error: error.map(str::to_string),
                ..Default::default()
            })
        };
        v.apply(&t.event(compacted(Some(30_000), None)));
        assert_eq!(v.context, Some(size(30_000)));
        v.apply(&t.event(compacted(None, Some("too_few_groups"))));
        assert_eq!(v.context, Some(size(30_000)));
        v.apply(&t.event(compacted(None, None)));
        assert_eq!(v.context, None);
    }

    /// A permission's context comes from its runtime's section (`reason`, else
    /// `description`; `cwd`) and the hub's `ext.twr.context`; other reasons have none.
    #[test]
    fn request_context_reads_the_runtime_and_the_hub() {
        let ext = |v: serde_json::Value| v.as_object().cloned();
        let c = RequestContext::of(
            AttentionReason::Permission,
            &ext(serde_json::json!({
                "claude": {"tool": "Bash", "description": "Run the tests"},
                "twr": {"context": {"said": "done", "task": "fix it"}},
            })),
        )
        .unwrap();
        assert_eq!(c.why.as_deref(), Some("Run the tests"));
        assert_eq!(
            (c.said.as_deref(), c.task.as_deref()),
            (Some("done"), Some("fix it"))
        );
        assert_eq!(c.cwd, None);
        let c = RequestContext::of(
            AttentionReason::Question,
            &ext(serde_json::json!({"codex": {"reason": " net ", "cwd": "/w"}})),
        )
        .unwrap();
        assert_eq!(
            (c.why.as_deref(), c.cwd.as_deref()),
            (Some("net"), Some("/w"))
        );
        assert_eq!(
            RequestContext::of(
                AttentionReason::Error,
                &ext(serde_json::json!({"codex": {"reason": "x"}}))
            ),
            None
        );
        assert_eq!(
            RequestContext::of(
                AttentionReason::Permission,
                &ext(serde_json::json!({"codex": {"reason": ""}}))
            ),
            None
        );
    }

    fn view(events: &[Event]) -> ThreadView {
        let mut v = ThreadView::new(events[0].thread.clone());
        for e in events {
            v.apply(e);
        }
        v
    }

    /// Thread `id`'s view in a registry folded from `events`, without the color the registry
    /// gives it ([`ThreadView::color`]): what folding its events into a view alone gives.
    fn replayed(events: &[Event], id: &str) -> ThreadView {
        let mut v = Registry::from_events(events).get(id).unwrap().clone();
        v.color = None;
        v
    }

    /// Applies events one by one, asserting the status after each.
    fn assert_statuses(steps: &[(Event, ThreadStatus)]) {
        let mut v = ThreadView::new(steps[0].0.thread.clone());
        for (i, (e, want)) in steps.iter().enumerate() {
            v.apply(e);
            assert_eq!(v.status(), *want, "after step {i} ({})", e.kind());
        }
    }

    fn no_pins() -> HashSet<String> {
        HashSet::new()
    }

    // --- status transitions ------------------------------------------------------------------

    #[test]
    fn happy_path_idle_running_idle_done() {
        let mut s = ThreadScript::new("t1");
        assert_statuses(&[
            (s.created("api"), S::Idle),
            (s.started("sess"), S::Idle),
            (s.turn_started("hi"), S::Running),
            (s.text("hello"), S::Running),
            (s.turn_ended(TurnResult::Completed), S::Idle),
            (s.report(Some(1), Grade::Success), S::Done),
            (s.turn_started("more"), S::Running),
            (s.turn_ended(TurnResult::Completed), S::Idle),
        ]);
    }

    /// A turn started from the queue keeps its message's ref while it runs, and says whether
    /// it acted (a tool call or a request in it) or its message was taken back; none of it
    /// outlives the turn.
    #[test]
    fn the_open_turn_keeps_its_ref_and_whether_it_acted_or_was_withdrawn() {
        let mut s = ThreadScript::new("t1");
        let mut start = s.turn_started("go");
        if let EventBody::TurnStarted(p) = &mut start.body {
            p.ext = serde_json::json!({"twr": {"ref": 5}}).as_object().cloned();
        }
        let mut v = view(&[s.created("api"), start]);
        assert_eq!(v.current_turn_ref, Some(5));
        assert!(!v.current_turn_acted && !v.current_turn_withdrawn);
        // A session's request (recorded without a turn) is not the turn's.
        let mut session = s.raise("p0", Permission);
        session.turn = None;
        v.apply(&session);
        v.apply(&s.text("thinking out loud"));
        assert!(!v.current_turn_acted);
        v.apply(&s.event(EventBody::TurnWithdrawn(TurnWithdrawn {
            msg_ref: 5,
            turn: 1,
            ..Default::default()
        })));
        assert!(v.current_turn_withdrawn);
        v.apply(&s.event(EventBody::AgentToolCall(AgentToolCall {
            call_id: "c1".into(),
            tool: "Bash".into(),
            ..Default::default()
        })));
        assert!(v.current_turn_acted);
        v.apply(&s.turn_ended(TurnResult::Interrupted));
        assert_eq!(v.current_turn_ref, None);
        assert!(!v.current_turn_acted && !v.current_turn_withdrawn);
        // A request raised in the next turn acts; that turn has no ref.
        v.apply(&s.turn_started("more"));
        assert_eq!(v.current_turn_ref, None);
        v.apply(&s.raise("p1", Question));
        assert!(v.current_turn_acted);
        v.apply(&s.crashed(1));
        assert!(!v.current_turn_acted);
    }

    /// A crash after the user took back the turn's message, while it ran or once it ended
    /// interrupted, has no failed turn: no error item for it, no retry of that message.
    #[test]
    fn a_crash_after_a_withdrawal_fails_no_turn() {
        let withdrawn = |s: &mut ThreadScript, turn: u64| {
            s.event(EventBody::TurnWithdrawn(TurnWithdrawn {
                msg_ref: 1,
                turn,
                ..Default::default()
            }))
        };
        let mut s = ThreadScript::new("t1");
        let (created, start) = (s.created("api"), s.turn_started("go"));
        let w = withdrawn(&mut s, 1);
        let mut v = view(&[created, start, w]);
        v.apply(&s.turn_ended(TurnResult::Interrupted));
        assert!(v.last_turn.as_ref().unwrap().withdrawn);
        v.apply(&s.crashed(1));
        assert_eq!((v.crashed_turn, v.failed_turn()), (None, None));
        assert!(!v.retry_eligible());
        // Crashed while it still ran.
        v.apply(&s.turn_started("again"));
        v.apply(&withdrawn(&mut s, 2));
        v.apply(&s.crashed(1));
        assert_eq!((v.crashed_turn, v.failed_turn()), (None, None));
        // A turn not taken back is the crashed one, as before.
        v.apply(&s.turn_started("third"));
        v.apply(&s.turn_ended(TurnResult::Interrupted));
        v.apply(&s.crashed(1));
        assert_eq!(v.failed_turn(), Some(3));
    }

    #[test]
    fn waiting_on_permission_and_question_only() {
        let mut s = ThreadScript::new("t1");
        assert_statuses(&[
            (s.created("api"), S::Idle),
            (s.turn_started("go"), S::Running),
            (s.raise("p1", Permission), S::Waiting),
            (s.annotate("p1", "looks safe"), S::Waiting),
            (s.resolve("p1"), S::Running),
            (s.raise("q1", Question), S::Waiting),
            (s.resolve("q1"), S::Running),
            (s.raise("st", Stalled), S::Running),
            (s.raise("rv", Review), S::Running),
            (s.raise("er", AttentionReason::Error), S::Running),
            (s.turn_ended(TurnResult::Completed), S::Idle),
        ]);
    }

    #[test]
    fn error_until_next_turn() {
        let mut s = ThreadScript::new("t1");
        assert_statuses(&[
            (s.created("api"), S::Idle),
            (s.turn_started("go"), S::Running),
            (s.turn_ended(TurnResult::Error), S::Error),
            (s.report(Some(1), Grade::GaveUp), S::Error),
            (s.turn_started("again"), S::Running),
            (s.turn_ended(TurnResult::Completed), S::Idle),
        ]);
    }

    #[test]
    fn interrupted_turn_is_idle() {
        let mut s = ThreadScript::new("t1");
        assert_statuses(&[
            (s.created("api"), S::Idle),
            (s.turn_started("go"), S::Running),
            (s.turn_ended(TurnResult::Interrupted), S::Idle),
            (s.report(Some(1), Grade::Partial), S::Idle),
        ]);
    }

    #[test]
    fn stale_report_does_not_mark_done() {
        let mut s = ThreadScript::new("t1");
        let evs = [
            s.created("api"),
            s.turn_started("one"),
            s.turn_ended(TurnResult::Completed),
            s.turn_started("two"),
            // Report for turn 1 arrives while turn 2 runs.
            s.report(Some(1), Grade::Success),
            s.turn_ended(TurnResult::Completed),
        ];
        let v = view(&evs);
        assert_eq!(v.status(), S::Idle);
        assert!(!v.reported);
        assert!(v.last_report.is_some());

        // A report for a different turn after the turn ended also does not count.
        let mut v2 = v.clone();
        v2.apply(&s.report(Some(1), Grade::Success));
        assert_eq!(v2.status(), S::Idle);
        // A report without a turn number covers the last finished turn.
        v2.apply(&s.report(None, Grade::Success));
        assert_eq!(v2.status(), S::Done);
    }

    #[test]
    fn latest_report_for_a_turn_wins_but_older_turns_never_replace_newer() {
        let mut s = ThreadScript::new("t1");
        let mut v = view(&[
            s.created("api"),
            s.turn_started("one"),
            s.turn_ended(TurnResult::Completed),
            s.report(Some(1), Grade::Success),
        ]);
        // A second report for the same turn (the orchestrator's) supersedes the hub's.
        v.apply(&s.report(Some(1), Grade::NeedsVerification));
        assert_eq!(
            v.last_report.as_ref().unwrap().grade,
            Grade::NeedsVerification
        );
        v.apply(&s.turn_started("two"));
        v.apply(&s.turn_ended(TurnResult::Completed));
        v.apply(&s.report(Some(2), Grade::Partial));
        // A late report for turn 1 does not hide turn 2's.
        v.apply(&s.report(Some(1), Grade::GaveUp));
        let r = v.last_report.as_ref().unwrap();
        assert_eq!((r.turn, r.grade), (Some(2), Grade::Partial));
        assert_eq!(v.status(), S::Done);
    }

    /// A report logged when the hub ended reports with ` — twr attach <name>` (its own and
    /// written ones) shows without it, also once the thread was renamed; a summary that only
    /// looks like it, or whose command attaches another thread, shows as written.
    #[test]
    fn a_logged_reports_attach_command_is_left_out() {
        let report = |s: &mut ThreadScript, summary: &str, by: &str| {
            s.event(EventBody::ReportReady(ReportReady {
                summary: summary.into(),
                turn: Some(1),
                written_by: by.into(),
                ..Default::default()
            }))
        };
        for (summary, by, shown) in [
            ("Fixed it. — twr attach api", "hub", "Fixed it."),
            ("Done — twr -w client-x attach api", "hub", "Done"),
            ("A — b — twr attach api", "hub", "A — b"),
            ("Fixed it. — twr attach api", "orchestrator", "Fixed it."),
            ("Done. — twr -w w1 attach api", "user:tui", "Done."),
            (
                "Go on — twr attach reviewer",
                "orchestrator",
                "Go on — twr attach reviewer",
            ),
            (
                "Fixed it. — twr attach api-1",
                "hub",
                "Fixed it. — twr attach api-1",
            ),
            (
                "Ran it — twr attach api now",
                "hub",
                "Ran it — twr attach api now",
            ),
            ("Ran it — twr attach", "hub", "Ran it — twr attach"),
            ("Use — twr -w attach api", "hub", "Use — twr -w attach api"),
            ("Plain report.", "hub", "Plain report."),
        ] {
            let mut s = ThreadScript::new("t1");
            let mut v = view(&[
                s.created("api"),
                s.turn_started("one"),
                s.turn_ended(TurnResult::Completed),
                report(&mut s, summary, by),
            ]);
            assert_eq!(
                v.last_report.as_ref().unwrap().summary,
                shown,
                "{summary:?} by {by}"
            );
            // Renamed after: the command names the name the thread had then.
            v.apply(&s.event(EventBody::ThreadRenamed(ThreadRenamed {
                name: "gateway".into(),
                previous: "api".into(),
                ..Default::default()
            })));
            assert_eq!(v.last_report.unwrap().summary, shown, "{summary:?} renamed");
        }
        // A report of the new name after a rename.
        let mut s = ThreadScript::new("t1");
        let v = view(&[
            s.created("api"),
            s.event(EventBody::ThreadRenamed(ThreadRenamed {
                name: "gateway".into(),
                previous: "api".into(),
                ..Default::default()
            })),
            s.turn_started("one"),
            s.turn_ended(TurnResult::Completed),
            report(&mut s, "Done — twr attach gateway", "hub"),
        ]);
        assert_eq!(v.last_report.unwrap().summary, "Done");
    }

    #[test]
    fn crash_is_error_until_next_turn() {
        let mut s = ThreadScript::new("t1");
        assert_statuses(&[
            (s.created("api"), S::Idle),
            (s.turn_started("go"), S::Running),
            (s.crashed(1), S::Error),
            (s.started("sess2"), S::Error),
            (s.turn_started("again"), S::Running),
            (s.turn_ended(TurnResult::Completed), S::Idle),
        ]);
    }

    #[test]
    fn crash_closes_turn_and_blocking_attention_either_order() {
        // Order A: hub appends turn.ended + attention.resolved before crashed.
        let mut s = ThreadScript::new("t1");
        let a = view(&[
            s.created("api"),
            s.turn_started("go"),
            s.raise("p1", Permission),
            s.turn_ended(TurnResult::Interrupted),
            s.resolve("p1"),
            s.crashed(1),
        ]);
        assert_eq!(a.status(), S::Error);
        assert!(a.current_turn.is_none() && a.open_attention.is_empty());

        // Order B: crashed first, bookkeeping after.
        let mut s = ThreadScript::new("t1");
        let mut b = view(&[
            s.created("api"),
            s.turn_started("go"),
            s.raise("p1", Permission),
            s.raise("st", Stalled),
        ]);
        assert_eq!(b.status(), S::Waiting);
        b.apply(&s.crashed(1));
        assert_eq!(b.status(), S::Error);
        assert!(b.current_turn.is_none());
        assert!(!b.is_blocked());
        assert!(b.open_attention.contains_key("st"), "non-blocking kept");
        b.apply(&s.turn_ended(TurnResult::Interrupted));
        assert_eq!(b.status(), S::Error);
        b.apply(&s.resolve("p1"));
        assert_eq!(b.status(), S::Error);
    }

    #[test]
    fn stop_is_done_either_order() {
        let mut s = ThreadScript::new("t1");
        let a = view(&[
            s.created("api"),
            s.turn_started("go"),
            s.raise("q1", Question),
            s.turn_ended(TurnResult::Interrupted),
            s.resolve("q1"),
            s.stopped(),
        ]);
        assert_eq!(a.status(), S::Done);

        let mut s = ThreadScript::new("t1");
        let mut b = view(&[
            s.created("api"),
            s.turn_started("go"),
            s.raise("q1", Question),
        ]);
        b.apply(&s.stopped());
        assert_eq!(b.status(), S::Done);
        assert!(b.current_turn.is_none() && !b.is_blocked());
        b.apply(&s.turn_ended(TurnResult::Interrupted));
        assert_eq!(b.status(), S::Done);
        // Restart clears stopped.
        b.apply(&s.started("sess"));
        assert_eq!(b.status(), S::Idle);
        b.apply(&s.stopped());
        assert_eq!(b.status(), S::Done);
        b.apply(&s.turn_started("go"));
        assert_eq!(b.status(), S::Running);
    }

    #[test]
    fn stopped_beats_error() {
        let mut s = ThreadScript::new("t1");
        let v = view(&[
            s.created("api"),
            s.turn_started("go"),
            s.turn_ended(TurnResult::Error),
            s.stopped(),
        ]);
        assert_eq!(v.status(), S::Done);
    }

    #[test]
    fn parked_and_resumed() {
        let mut s = ThreadScript::new("t1");
        assert_statuses(&[
            (s.created("api"), S::Idle),
            (s.turn_started("go"), S::Running),
            (s.turn_ended(TurnResult::Completed), S::Idle),
            (s.parked(), S::Parked),
            (s.raise("st", Stalled), S::Parked),
            (s.resumed("sess"), S::Idle),
            (s.parked(), S::Parked),
            (s.turn_started("wake"), S::Running),
        ]);
    }

    #[test]
    fn parked_beats_stopped_and_waiting() {
        let mut s = ThreadScript::new("t1");
        assert_statuses(&[
            (s.created("api"), S::Idle),
            (s.turn_started("go"), S::Running),
            (s.raise("p1", Permission), S::Waiting),
            (s.parked(), S::Parked),
            (s.stopped(), S::Parked),
            (s.resumed("s"), S::Idle),
        ]);
    }

    #[test]
    fn attached_beats_everything() {
        let mut s = ThreadScript::new("t1");
        assert_statuses(&[
            (s.created("api"), S::Idle),
            (s.turn_started("go"), S::Running),
            (s.raise("p1", Permission), S::Waiting),
            (s.attached(), S::Attached),
            (s.parked(), S::Attached),
            (s.detached(), S::Parked),
            (s.attached(), S::Attached),
            (s.resumed("s"), S::Attached),
            // Parking dropped the permission and closed the turn.
            (s.detached(), S::Idle),
        ]);
    }

    #[test]
    fn park_closes_turn_and_drops_blocking_attention() {
        let mut s = ThreadScript::new("t1");
        let evs = [
            s.created("api"),
            s.turn_started("go"),
            s.raise("p1", Permission),
            s.raise("st", Stalled),
            s.parked(),
        ];
        let v = view(&evs);
        assert_eq!(v.status(), S::Parked);
        assert!(v.current_turn.is_none());
        assert!(!v.is_blocked());
        assert!(v.open_attention.contains_key("st"), "non-blocking kept");
        let st = AttentionStack::from_events(&evs);
        assert!(st.get("p1").is_none());
        assert!(st.get("st").is_some());
        // Late bookkeeping from the hub is harmless.
        let mut v = v;
        v.apply(&s.turn_ended(TurnResult::Interrupted));
        v.apply(&s.resolve("p1"));
        v.apply(&s.resumed("sess"));
        assert_eq!(v.status(), S::Idle);
    }

    #[test]
    fn view_fields_are_folded() {
        let mut s = ThreadScript::new("t1");
        let mut created = s.created_with_aliases("api", &["a", "backend"]);
        if let EventBody::ThreadCreated(p) = &mut created.body {
            p.ext = serde_json::json!({"twr": {"role": "orchestrator", "model": "opus"}})
                .as_object()
                .cloned();
            p.worktree = Some("/wt".into());
        }
        let v = view(&[
            created,
            s.started("sess-1"),
            s.turn_started("go"),
            s.turn_ended(TurnResult::Completed),
            s.git("twr/api", 2),
            s.resumed("sess-2"),
        ]);
        assert_eq!(v.name, "api");
        assert_eq!(v.model.as_deref(), Some("opus"));
        assert_eq!(v.aliases, vec!["a", "backend"]);
        assert_eq!(v.worktree.as_deref(), Some("/wt"));
        assert_eq!(v.runtime, "fake");
        assert!(v.is_orchestrator());
        assert_eq!(v.session_id.as_deref(), Some("sess-2"));
        assert_eq!(v.turns, 1);
        assert_eq!(v.last_turn.as_ref().unwrap().turn, 1);
        assert_eq!(v.git.as_ref().unwrap().dirty_files, 2);
        assert_eq!(v.last_seq, 6);
        assert_eq!(
            v.last_activity,
            Some(crate::testkit::base_time() + chrono::Duration::seconds(5))
        );
    }

    /// `thread.mode_changed` sets the mode the next runtime starts with; the `thread.stopped`
    /// of a mode restart closes the runtime without stopping the thread. Replay folds the
    /// same view.
    #[test]
    fn mode_changes_and_mode_restarts() {
        let mut s = ThreadScript::new("t1");
        let mode_changed = |s: &mut ThreadScript, mode: &str, previous: &str| {
            s.event(EventBody::ThreadModeChanged(ThreadModeChanged {
                mode: mode.into(),
                previous: previous.into(),
                by: ChangedBy::User,
                ..Default::default()
            }))
        };
        let mut restart = SessionPayload {
            session_id: Some("sess-1".into()),
            ..Default::default()
        };
        let mut ext = JsonMap::new();
        ext.insert(
            "twr".into(),
            serde_json::json!({ "reason": MODE_RESTART_REASON }),
        );
        restart.ext = Some(ext);
        let evs = vec![
            s.created("api"),
            s.started("sess-1"),
            s.turn_started("go"),
            s.turn_ended(TurnResult::Completed),
            mode_changed(&mut s, "auto", "manual"),
            s.event(EventBody::ThreadStopped(restart)),
        ];
        let v = view(&evs);
        assert_eq!(v.mode, "auto");
        assert!(!v.stopped, "a mode restart is no stop");
        assert_eq!(v.status(), S::Idle);
        assert_eq!(v.session_id.as_deref(), Some("sess-1"));
        let mut evs = evs;
        evs.push(s.started("sess-1"));
        evs.push(mode_changed(&mut s, "plan", "auto"));
        evs.push(s.stopped());
        let v = view(&evs);
        assert_eq!(v.mode, "plan");
        assert!(v.stopped, "a plain stop still stops");
        assert_eq!(replayed(&evs, "t1"), v);
    }

    /// The `thread.stopped` of a box switch (`box_upgrade`) folds like a mode restart's: the
    /// thread keeps its session and is not stopped, live and on replay; a plain stop after it
    /// still stops.
    #[test]
    fn a_box_upgrade_is_no_stop() {
        let mut s = ThreadScript::new("t1");
        let upgrade = SessionPayload {
            session_id: Some("sess-1".into()),
            ext: serde_json::json!({"twr": {"reason": BOX_UPGRADE_REASON}})
                .as_object()
                .cloned(),
            ..Default::default()
        };
        let mut evs = vec![
            s.created("api"),
            s.started("sess-1"),
            s.turn_started("go"),
            s.turn_ended(TurnResult::Completed),
            s.event(EventBody::ThreadStopped(upgrade)),
        ];
        let v = view(&evs);
        assert!(!v.stopped, "a box upgrade is no stop");
        assert_eq!(v.status(), S::Idle);
        assert_eq!(v.session_id.as_deref(), Some("sess-1"));
        assert_eq!(replayed(&evs, "t1"), v);
        evs.push(s.started("sess-1"));
        evs.push(s.stopped());
        let v = view(&evs);
        assert!(v.stopped, "a plain stop still stops");
        assert_eq!(replayed(&evs, "t1"), v);
    }

    /// The model and effort start as `thread.created`'s `ext.twr.model` / `ext.twr.effort`
    /// (none in a log from before efforts), then follow `thread.model_changed` (a cleared value
    /// is the runtime's default again); the `thread.stopped` of a model restart closes the
    /// runtime without stopping the thread. Replay equals live at every step.
    #[test]
    fn model_changes_and_model_restarts() {
        let mut s = ThreadScript::new("t1");
        let mut created = s.created("api");
        if let EventBody::ThreadCreated(p) = &mut created.body {
            p.ext = serde_json::json!({"twr": {"model": "sonnet"}})
                .as_object()
                .cloned();
        }
        let changed = |s: &mut ThreadScript, model: Option<&str>, effort: Option<&str>| {
            s.event(EventBody::ThreadModelChanged(ThreadModelChanged {
                model: model.map(str::to_string),
                effort: effort.map(str::to_string),
                by: ChangedBy::Orchestrator,
                ..Default::default()
            }))
        };
        let mut restart = SessionPayload {
            session_id: Some("sess-1".into()),
            ..Default::default()
        };
        restart.ext = serde_json::json!({"twr": {"reason": MODEL_RESTART_REASON}})
            .as_object()
            .cloned();
        let evs = [
            created,
            s.started("sess-1"),
            changed(&mut s, Some("claude-fable-5-1"), Some("high")),
            s.event(EventBody::ThreadStopped(restart)),
            s.started("sess-1"),
            changed(&mut s, None, Some("low")),
        ];
        let want = [
            (Some("sonnet"), None),
            (Some("sonnet"), None),
            (Some("claude-fable-5-1"), Some("high")),
            (Some("claude-fable-5-1"), Some("high")),
            (Some("claude-fable-5-1"), Some("high")),
            (None, Some("low")),
        ];
        for (n, (model, effort)) in want.iter().enumerate() {
            let v = view(&evs[..=n]);
            assert_eq!(
                (v.model.as_deref(), v.effort.as_deref()),
                (*model, *effort),
                "after event {n}"
            );
            assert!(!v.stopped, "a model restart is no stop (event {n})");
            assert_eq!(replayed(&evs[..=n], "t1"), v);
        }
        // A thread object from before efforts reads as the runtime's default.
        let mut j = serde_json::to_value(view(&evs[..1])).unwrap();
        j.as_object_mut().unwrap().remove("effort");
        let back: ThreadView = serde_json::from_value(j).unwrap();
        assert_eq!(back.effort, None);
    }

    /// A move to another runtime drops the session: what the previous runtime still reports
    /// (its turn's end, its stop, a report, a start that was under way) never brings its
    /// session back; the new runtime's first `thread.started` does, and ends the handoff.
    #[test]
    fn a_runtime_change_waits_for_the_new_runtimes_session() {
        let mut s = ThreadScript::new("t1");
        let mut restart = SessionPayload {
            session_id: Some("old".into()),
            ..Default::default()
        };
        restart.ext = serde_json::json!({"twr": {"reason": RUNTIME_RESTART_REASON}})
            .as_object()
            .cloned();
        let mut ended = s.turn_ended(TurnResult::Completed);
        if let EventBody::TurnEnded(p) = &mut ended.body {
            p.session_id = Some("old".into());
        }
        let mut late_start = s.started("old-2");
        late_start.source = Source::Adapter("fake".into());
        let mut new_start = s.started("new");
        new_start.source = Source::Adapter("codex".into());
        let mut report = s.report(None, Grade::Success);
        if let EventBody::ReportReady(p) = &mut report.body {
            p.session_id = Some("old".into());
        }
        let evs = [
            s.created("api"),
            s.started("old"),
            s.turn_started("go"),
            s.event(EventBody::ThreadModelChanged(ThreadModelChanged {
                model: Some("gpt-6.1-sol".into()),
                effort: Some("xhigh".into()),
                runtime: Some("codex".into()),
                previous_runtime: Some("fake".into()),
                ..Default::default()
            })),
            ended,
            report,
            late_start,
            s.event(EventBody::ThreadStopped(restart)),
            new_start,
        ];
        let at = |n: usize| view(&evs[..=n]);
        assert_eq!(at(2).session_id.as_deref(), Some("old"));
        assert!(!at(2).handoff_pending);
        for n in 3..=7 {
            let v = at(n);
            assert_eq!(v.runtime, "codex", "after event {n}");
            assert_eq!(v.session_id, None, "after event {n}");
            assert!(v.handoff_pending, "after event {n}");
            assert!(!v.stopped, "a runtime change is no stop (event {n})");
        }
        assert_eq!(
            (at(3).model.as_deref(), at(3).effort.as_deref()),
            (Some("gpt-6.1-sol"), Some("xhigh"))
        );
        let v = at(8);
        assert_eq!(v.session_id.as_deref(), Some("new"));
        assert!(!v.handoff_pending);
        assert_eq!(replayed(&evs, "t1"), v);
        // A thread object from before runtime changes has none pending.
        let mut j = serde_json::to_value(at(1)).unwrap();
        j.as_object_mut().unwrap().remove("handoff_pending");
        let back: ThreadView = serde_json::from_value(j).unwrap();
        assert!(!back.handoff_pending);
    }

    /// A move between two agents of one adapter (`acp`): a start of the previous agent that
    /// the hub stamped as moved away from is not the new agent's session; the next one is.
    #[test]
    fn a_start_moved_away_from_is_not_the_new_session() {
        let mut s = ThreadScript::new("t1");
        let acp = |mut e: Event| {
            e.source = Source::Adapter("acp".into());
            e
        };
        let mut late = acp(s.started("old-2"));
        if let EventBody::ThreadStarted(p) = &mut late.body {
            p.ext = serde_json::json!({"twr": {RUNTIME_MOVED_KEY: true}})
                .as_object()
                .cloned();
        }
        let mut created = s.created("api");
        if let EventBody::ThreadCreated(p) = &mut created.body {
            p.runtime = "acp".into();
        }
        let evs = [
            created,
            acp(s.started("old")),
            s.event(EventBody::ThreadModelChanged(ThreadModelChanged {
                runtime: Some("acp".into()),
                previous_runtime: Some("acp".into()),
                ..Default::default()
            })),
            late,
            acp(s.started("new")),
        ];
        let at = |n: usize| view(&evs[..=n]);
        assert_eq!(
            (at(3).session_id, at(3).handoff_pending),
            (None, true),
            "the previous agent's start"
        );
        assert_eq!(
            (at(4).session_id.as_deref(), at(4).handoff_pending),
            (Some("new"), false)
        );
    }

    /// A move between two agents of one adapter recorded before the previous agent's first
    /// start (nothing marked it moved away from, as after a hub restart): that start, stamped
    /// with its agent, is not the new agent's session; a start stamped with the new agent is,
    /// and so is one without an agent stamp (a runner from before agent stamps).
    #[test]
    fn a_start_of_another_agent_is_not_the_new_session() {
        let mut s = ThreadScript::new("t1");
        let agent = |cmd: &str| {
            serde_json::json!({"twr": {ACP_COMMAND_KEY: [cmd, "acp"]}})
                .as_object()
                .cloned()
        };
        let start = |s: &mut ThreadScript, session: &str, ext: Option<JsonMap>| {
            let mut e = s.started(session);
            e.source = Source::Adapter("acp".into());
            if let EventBody::ThreadStarted(p) = &mut e.body {
                p.ext = ext;
            }
            e
        };
        let mut created = s.created("api");
        if let EventBody::ThreadCreated(p) = &mut created.body {
            p.runtime = "acp".into();
            p.ext = agent("agent-a");
        }
        let moved = s.event(EventBody::ThreadModelChanged(ThreadModelChanged {
            runtime: Some("acp".into()),
            previous_runtime: Some("acp".into()),
            ext: agent("agent-b"),
            ..Default::default()
        }));
        let old = start(&mut s, "a-1", agent("agent-a"));
        let new = start(&mut s, "b-1", agent("agent-b"));
        let unstamped = start(&mut s, "x-1", None);
        let evs = [created.clone(), moved.clone(), old, new];
        let at = |n: usize| view(&evs[..=n]);
        assert_eq!(
            at(0).acp_command,
            Some(serde_json::json!(["agent-a", "acp"]))
        );
        assert_eq!(
            at(1).acp_command,
            Some(serde_json::json!(["agent-b", "acp"]))
        );
        assert_eq!(
            (at(2).session_id, at(2).handoff_pending),
            (None, true),
            "the previous agent's start"
        );
        assert_eq!(
            (at(3).session_id.as_deref(), at(3).handoff_pending),
            (Some("b-1"), false)
        );
        let v = view(&[created, moved, unstamped]);
        assert_eq!(
            (v.session_id.as_deref(), v.handoff_pending),
            (Some("x-1"), false)
        );
        // The agent goes over the wire as `acp_command`; a thread without one has no key.
        let j = serde_json::to_value(at(3)).unwrap();
        assert_eq!(j["acp_command"], serde_json::json!(["agent-b", "acp"]));
        assert_eq!(serde_json::from_value::<ThreadView>(j).unwrap(), at(3));
        let mut plain = ThreadScript::new("t2");
        let j = serde_json::to_value(view(&[plain.created("web")])).unwrap();
        assert!(j.get("acp_command").is_none(), "{j}");
        // Another adapter's start recorded under the new runtime (a remote box's events take
        // the thread's runtime of the moment as their source): its stamp tells.
        let mut r = ThreadScript::new("t3");
        let moved = r.event(EventBody::ThreadModelChanged(ThreadModelChanged {
            runtime: Some("codex".into()),
            previous_runtime: Some("claude-code".into()),
            ..Default::default()
        }));
        let stamped = |r: &mut ThreadScript, session: &str, runtime: &str| {
            let mut e = r.started(session);
            e.source = Source::Adapter("codex".into());
            if let EventBody::ThreadStarted(p) = &mut e.body {
                p.ext = serde_json::json!({"twr": {"runtime": runtime}})
                    .as_object()
                    .cloned();
            }
            e
        };
        let (old, new) = (
            stamped(&mut r, "cc-1", "claude-code"),
            stamped(&mut r, "cx-1", "codex"),
        );
        let evs = [r.created("api"), moved, old, new];
        let v = view(&evs[..3]);
        assert_eq!((v.session_id, v.handoff_pending), (None, true));
        let v = view(&evs);
        assert_eq!(
            (v.session_id.as_deref(), v.handoff_pending),
            (Some("cx-1"), false)
        );
    }

    /// A deleted thread leaves the registry and the attention stack, and nothing of it comes
    /// back with a later event; its name resolves to another thread again.
    #[test]
    fn a_deleted_thread_is_gone_for_good() {
        let mut a = ThreadScript::new("t1");
        let mut b = ThreadScript::new("t2");
        let mut evs = vec![
            a.created("api"),
            a.turn_started("go"),
            a.raise("p1", AttentionReason::Permission),
            a.raise("e1", AttentionReason::Error),
            a.event(EventBody::ThreadDeleted(ThreadDeleted {
                name: "api".into(),
                ..Default::default()
            })),
            a.turn_ended(TurnResult::Completed),
        ];
        let mut stack = AttentionStack::new();
        for (i, e) in evs.iter().enumerate() {
            stack.apply_at(e, Some(i as u64 + 1));
        }
        assert!(stack.is_empty());
        let r = Registry::from_events(&evs);
        assert!(r.get("t1").is_none() && r.is_deleted("t1"));
        assert!(r.is_empty());
        assert!(matches!(r.resolve("api"), Err(ResolveError::NotFound(_))));
        evs.push(b.created("api"));
        let r = Registry::from_events(&evs);
        assert_eq!(r.resolve("api").unwrap().id, "t2");
        assert_eq!(r.list().len(), 1);
        assert_eq!(r.deleted_view("t1").unwrap().name, "api");
    }

    /// Each thread gets a color of its own when its `thread.created` is folded, from those
    /// of the threads alive then: in the palette's order while none is freed, none for an
    /// orchestrator. It keeps it through renames, other threads' deletions and creations, and
    /// a replay; a deleted thread's view keeps it, and the next thread may take it.
    #[test]
    fn a_thread_gets_a_color_of_its_own_and_keeps_it() {
        let (mut a, mut b, mut c, mut o) = (
            ThreadScript::new("t-a"),
            ThreadScript::new("t-b"),
            ThreadScript::new("t-c"),
            ThreadScript::new("t-o"),
        );
        let mut orch = o.created("orchestrator");
        if let EventBody::ThreadCreated(p) = &mut orch.body {
            p.ext = serde_json::json!({"twr": {"role": "orchestrator"}})
                .as_object()
                .cloned();
        }
        let mut evs = vec![orch, a.created("api"), b.created("web"), c.created("docs")];
        let colors = |evs: &[Event]| {
            let r = Registry::from_events(evs);
            ["t-o", "t-a", "t-b", "t-c"].map(|id| r.get(id).and_then(|t| t.color))
        };
        assert_eq!(colors(&evs), [None, Some(0), Some(1), Some(2)]);
        evs.push(b.event(EventBody::ThreadRenamed(ThreadRenamed {
            name: "frontend".into(),
            previous: "web".into(),
            ..Default::default()
        })));
        evs.push(a.event(EventBody::ThreadDeleted(ThreadDeleted {
            name: "api".into(),
            ..Default::default()
        })));
        assert_eq!(colors(&evs), [None, None, Some(1), Some(2)]);
        let r = Registry::from_events(&evs);
        assert_eq!(r.deleted_view("t-a").unwrap().color, Some(0));
        // The next thread takes a color no thread alive has, the farthest from theirs.
        let mut d = ThreadScript::new("t-d");
        evs.push(d.created("api"));
        let r = Registry::from_events(&evs);
        let dc = r.get("t-d").unwrap().color.unwrap();
        assert_eq!(dc, crate::palette::allocate([1, 2]));
        assert!(dc != 1 && dc != 2);
        assert_eq!(colors(&evs), [None, None, Some(1), Some(2)]);
        // Live and on replay alike.
        let mut live = Registry::new();
        for e in &evs {
            live.apply(e);
        }
        assert_eq!(live, r);
    }

    /// A restored thread keeps its color unless a thread alive took it meanwhile; then it gets
    /// one no thread alive has.
    #[test]
    fn a_restored_thread_keeps_its_color_unless_it_was_taken() {
        let restored = |name: &str| {
            EventBody::ThreadRestored(ThreadRestored {
                item: "01J00000000000000000000000".into(),
                name: name.into(),
                ..Default::default()
            })
        };
        let deleted = |name: &str| {
            EventBody::ThreadDeleted(ThreadDeleted {
                name: name.into(),
                ..Default::default()
            })
        };
        let (mut a, mut b) = (ThreadScript::new("t-a"), ThreadScript::new("t-b"));
        // Nobody took it: kept.
        let evs = vec![
            a.created("api"),
            b.created("web"),
            a.event(deleted("api")),
            a.event(restored("api")),
        ];
        let r = Registry::from_events(&evs);
        assert_eq!(r.get("t-a").unwrap().color, Some(0));
        // Threads created meanwhile took it: another one, that no thread alive has.
        let mut evs = vec![a.created("api"), b.created("web"), a.event(deleted("api"))];
        for i in 0.. {
            let mut s = ThreadScript::new(format!("t-{i}"));
            evs.push(s.created(&format!("n{i}")));
            let r = Registry::from_events(&evs);
            if r.get(&format!("t-{i}")).unwrap().color == Some(0) {
                break;
            }
            assert!(i < 28);
        }
        evs.push(a.event(restored("api")));
        let r = Registry::from_events(&evs);
        let ac = r.get("t-a").unwrap().color.unwrap();
        let others: Vec<u8> = r
            .list()
            .iter()
            .filter(|t| t.id != "t-a")
            .filter_map(|t| t.color)
            .collect();
        assert!(!others.contains(&ac), "{ac} {others:?}");
        assert_eq!(ac, crate::palette::allocate(others));
    }

    /// A thread whose `thread.created` the log lacks holds a color from its first control
    /// event, so later threads don't take it, and the same as a fold of the control stream
    /// alone gives it (an agent event before it counts for nothing); an orchestrator seen
    /// before its `thread.created` gives its color back.
    #[test]
    fn a_thread_without_its_creation_holds_a_color_too() {
        let (mut x, mut o, mut y) = (
            ThreadScript::new("t-x"),
            ThreadScript::new("t-o"),
            ThreadScript::new("t-y"),
        );
        let mut evs = vec![y.text("hello"), x.turn_started("go"), o.turn_started("hi")];
        let mut orch = o.created("orchestrator");
        if let EventBody::ThreadCreated(p) = &mut orch.body {
            p.ext = serde_json::json!({"twr": {"role": "orchestrator"}})
                .as_object()
                .cloned();
        }
        evs.push(orch);
        for i in 0..28 {
            let mut s = ThreadScript::new(format!("t-{i}"));
            evs.push(s.created(&format!("n{i}")));
        }
        evs.push(y.turn_started("go"));
        let r = Registry::from_events(&evs);
        assert_eq!(r.get("t-x").unwrap().color, Some(0));
        assert_eq!(r.get("t-o").unwrap().color, None);
        let held: std::collections::HashSet<u8> = r.list().iter().filter_map(|t| t.color).collect();
        assert_eq!(held.len(), 30);
        let control = Registry::from_events(evs.iter().filter(|e| e.is_control()));
        assert_eq!(control.len(), r.len());
        for t in r.list() {
            assert_eq!(control.get(&t.id).unwrap().color, t.color, "{}", t.id);
        }
    }

    /// Past 30, deleting threads changes no survivor's color, and the next thread takes a
    /// freed color, not a shared one; two threads that came to share a color keep it until
    /// one goes.
    #[test]
    fn after_more_than_thirty_deletions_recolor_nobody() {
        let mut scripts: Vec<ThreadScript> = (0..31)
            .map(|i| ThreadScript::new(format!("t-{i}")))
            .collect();
        let mut evs: Vec<Event> = scripts
            .iter_mut()
            .enumerate()
            .map(|(i, s)| s.created(&format!("n{i}")))
            .collect();
        let before = Registry::from_events(&evs);
        let shared = before.get("t-30").unwrap().color.unwrap();
        assert_eq!(shared, 0);
        // t-5 had a color of its own.
        evs.push(scripts[5].event(EventBody::ThreadDeleted(ThreadDeleted {
            name: "n5".into(),
            ..Default::default()
        })));
        let after = Registry::from_events(&evs);
        for t in after.list() {
            assert_eq!(t.color, before.get(&t.id).unwrap().color, "{}", t.id);
        }
        let mut n = ThreadScript::new("t-new");
        evs.push(n.created("new"));
        let r = Registry::from_events(&evs);
        assert_eq!(r.get("t-new").unwrap().color, Some(5));
        assert_eq!(r.get("t-0").unwrap().color, r.get("t-30").unwrap().color);
    }

    /// Up to 30 threads alive never share a color; past that, colors are shared evenly.
    #[test]
    fn thirty_threads_have_thirty_colors_and_more_share_them_evenly() {
        let mut evs = Vec::new();
        for i in 0..45 {
            let mut s = ThreadScript::new(format!("t-{i}"));
            evs.push(s.created(&format!("n{i}")));
        }
        let r = Registry::from_events(&evs[..30]);
        let held: std::collections::HashSet<u8> = r.list().iter().filter_map(|t| t.color).collect();
        assert_eq!(held.len(), 30);
        let r = Registry::from_events(&evs);
        let mut count = [0usize; 30];
        for t in r.list() {
            count[usize::from(t.color.unwrap())] += 1;
        }
        assert!(count.iter().all(|&n| n == 1 || n == 2), "{count:?}");
    }

    /// A restored thread comes back as it was when deleted, parked, under its new name and
    /// worktree; its later events count again, live and on replay alike.
    #[test]
    fn a_restored_thread_comes_back_as_it_was() {
        let mut a = ThreadScript::new("t1");
        let evs = vec![
            a.created("api"),
            a.turn_started("go"),
            a.turn_ended(TurnResult::Completed),
            a.event(EventBody::ThreadParked(SessionPayload {
                session_id: Some("s1".into()),
                ..Default::default()
            })),
            a.event(EventBody::ThreadDeleted(ThreadDeleted {
                name: "api".into(),
                ..Default::default()
            })),
            a.turn_started("dropped"),
            a.event(EventBody::ThreadRestored(ThreadRestored {
                item: "01J00000000000000000000000".into(),
                name: "api-2".into(),
                previous_name: Some("api".into()),
                worktree: Some("/w2".into()),
                ..Default::default()
            })),
        ];
        let mut live = Registry::new();
        for e in &evs {
            live.apply(e);
        }
        let r = Registry::from_events(&evs);
        assert_eq!(r, live);
        let v = r.get("t1").unwrap();
        assert!(!r.is_deleted("t1") && v.parked && v.current_turn.is_none());
        assert_eq!((v.name.as_str(), v.turns), ("api-2", 1));
        assert_eq!(v.worktree.as_deref(), Some("/w2"));
        assert_eq!(v.session_id.as_deref(), Some("s1"));
        assert_eq!(r.resolve("api-2").unwrap().id, "t1");
        // A new life: the restore's seq, not the creation's.
        assert_eq!(
            evs[0].seq,
            Registry::from_events(&evs[..1])
                .get("t1")
                .unwrap()
                .lifecycle_seq
        );
        assert_eq!(v.lifecycle_seq, evs[6].seq);
        assert_ne!(v.lifecycle_seq, evs[0].seq);
        assert_eq!(
            serde_json::to_value(v).unwrap()["lifecycle_seq"],
            serde_json::json!(evs[6].seq)
        );
    }

    #[test]
    fn retry_budget_is_derived_from_events() {
        let mut s = ThreadScript::new("t1");
        let mut v = view(&[
            s.created("api"),
            s.turn_started("go"),
            s.turn_ended(TurnResult::Error),
        ]);
        assert_eq!(v.last_turn_retry_of, None);
        v.apply(&s.retry_turn_started(1));
        assert_eq!(v.current_turn, Some(2));
        assert_eq!(v.current_turn_retry_of, Some(1));
        v.apply(&s.turn_ended(TurnResult::Error));
        assert_eq!(v.last_turn_retry_of, Some(1));
        assert_eq!(v.last_turn.as_ref().unwrap().retry_of, Some(1));
        assert_eq!(v.status(), S::Error);
        v.apply(&s.turn_started("new work"));
        v.apply(&s.turn_ended(TurnResult::Error));
        assert_eq!(v.last_turn_retry_of, None);
    }

    #[test]
    fn retry_link_survives_crash_before_turn_ended() {
        let mut s = ThreadScript::new("t1");
        let mut v = view(&[
            s.created("api"),
            s.turn_started("go"),
            s.turn_ended(TurnResult::Error),
        ]);
        assert!(v.retry_eligible());
        v.apply(&s.retry_turn_started(1));
        assert!(!v.retry_eligible(), "a turn is running");
        // Crash first, then the hub's late turn.ended for turn 2.
        v.apply(&s.crashed(1));
        assert_eq!(v.crashed_turn, Some(2));
        assert_eq!(v.failed_turn(), Some(2));
        assert!(!v.retry_eligible(), "turn 2 is itself a retry");
        v.apply(&s.turn_ended(TurnResult::Interrupted));
        let last = v.last_turn.as_ref().unwrap();
        assert_eq!((last.turn, last.retry_of), (2, Some(1)));
        assert_eq!(v.last_turn_retry_of, Some(1));
        assert!(!v.retry_eligible());
        assert_eq!(v.status(), S::Error);
    }

    #[test]
    fn crash_in_hub_order_is_retry_eligible_once() {
        // The hub's own order: turn.ended{interrupted} before thread.crashed.
        let mut s = ThreadScript::new("t1");
        let mut v = view(&[
            s.created("api"),
            s.turn_started("go"),
            s.turn_ended(TurnResult::Interrupted),
            s.crashed(1),
        ]);
        assert_eq!(v.failed_turn(), Some(1));
        assert!(v.retry_eligible());
        v.apply(&s.started("sess2"));
        assert!(
            v.retry_eligible(),
            "restart before the retry keeps eligibility"
        );
        v.apply(&s.retry_turn_started(1));
        v.apply(&s.turn_ended(TurnResult::Interrupted));
        v.apply(&s.crashed(1));
        assert_eq!(v.failed_turn(), Some(2));
        assert!(!v.retry_eligible());

        // An idle crash after a completed turn has nothing to retry.
        let mut s = ThreadScript::new("t2");
        let v = view(&[
            s.created("api"),
            s.turn_started("go"),
            s.turn_ended(TurnResult::Completed),
            s.crashed(1),
        ]);
        assert_eq!(v.failed_turn(), None);
        assert!(!v.retry_eligible());
    }

    #[test]
    fn consecutive_transient_failures_retry_once_each() {
        let mut s = ThreadScript::new("t1");
        let mut v = view(&[s.created("api")]);
        assert!(!v.retry_eligible(), "nothing failed");
        v.apply(&s.turn_started("go"));
        v.apply(&s.turn_ended(TurnResult::Error));
        assert!(v.retry_eligible());
        v.apply(&s.retry_turn_started(1));
        v.apply(&s.turn_ended(TurnResult::Error));
        assert!(!v.retry_eligible(), "retry budget of turn 1 spent");
        // New user work gets its own budget.
        v.apply(&s.turn_started("new work"));
        v.apply(&s.turn_ended(TurnResult::Error));
        assert_eq!(v.failed_turn(), Some(3));
        assert!(v.retry_eligible());
        v.apply(&s.retry_turn_started(3));
        v.apply(&s.turn_ended(TurnResult::Completed));
        assert_eq!(v.failed_turn(), None);
        assert!(!v.retry_eligible());
        // Stopped or parked threads are never retried.
        v.apply(&s.turn_started("more"));
        v.apply(&s.turn_ended(TurnResult::Error));
        assert!(v.retry_eligible());
        let mut stopped = v.clone();
        stopped.apply(&s.stopped());
        assert!(!stopped.retry_eligible());
        v.apply(&s.parked());
        assert!(!v.retry_eligible());
    }

    /// The model a runtime reports: from the latest `thread.started` (a resume or restart
    /// replaces it, one reporting none clears it), a `turn.ended` or adapter `thread.resumed`
    /// that reports one, never `ext.twr.model`. Replay equals live at every step.
    #[test]
    fn actual_model_follows_the_runtime_reports() {
        let ext = |v: serde_json::Value| v.as_object().cloned();
        let mut s = ThreadScript::new("t1");
        let mut created = s.created("api");
        if let EventBody::ThreadCreated(p) = &mut created.body {
            p.ext = ext(serde_json::json!({"twr": {"model": "sonnet"}}));
        }
        let started = |s: &mut ThreadScript, sid: &str, e: serde_json::Value| {
            let mut ev = s.started(sid);
            if let EventBody::ThreadStarted(p) = &mut ev.body {
                p.ext = ext(e);
            }
            ev
        };
        let evs = vec![
            created,
            // twr's own section (the pid, a requested model) is not a report.
            started(
                &mut s,
                "s1",
                serde_json::json!({"twr": {"pid": 7, "model": "x"}}),
            ),
            started(
                &mut s,
                "s1",
                serde_json::json!({"twr": {"pid": 8},
                    "claude": {"model": "claude-sonnet-4-5-20250929", "resumed": false}}),
            ),
            s.turn_started("go"),
            s.turn_ended(TurnResult::Completed),
            // A resume (new process) reporting another model.
            started(
                &mut s,
                "s1",
                serde_json::json!({"claude": {"model": "claude-opus-4-1-20250805",
                    "resumed": true}}),
            ),
            // A hub-appended resume without a report keeps it.
            s.resumed("s1"),
            s.turn_started("more"),
            {
                let mut ev = s.turn_ended(TurnResult::Completed);
                if let EventBody::TurnEnded(p) = &mut ev.body {
                    p.ext = ext(serde_json::json!({"acp": {"model": "gemini-2.5-pro"}}));
                }
                ev
            },
            // A new process that reports nothing: unknown again.
            started(&mut s, "s2", serde_json::json!({"codex": {"model": ""}})),
        ];
        let want = [
            None,
            None,
            Some("claude-sonnet-4-5-20250929"),
            Some("claude-sonnet-4-5-20250929"),
            Some("claude-sonnet-4-5-20250929"),
            Some("claude-opus-4-1-20250805"),
            Some("claude-opus-4-1-20250805"),
            Some("claude-opus-4-1-20250805"),
            Some("gemini-2.5-pro"),
            None,
        ];
        let mut live = ThreadView::new("t1");
        for (i, e) in evs.iter().enumerate() {
            live.apply(e);
            assert_eq!(live.actual_model.as_deref(), want[i], "after step {i}");
            assert_eq!(live.model.as_deref(), Some("sonnet"));
            assert_eq!(replayed(&evs[..=i], "t1"), live, "after step {i}");
        }
        assert_eq!(live.shown_model(), Some("sonnet"));
        live.actual_model = Some("claude-sonnet-4-5".into());
        assert_eq!(live.shown_model(), Some("claude-sonnet-4-5"));
        // Clients see both, under `model` and `model_actual`.
        let j = serde_json::to_value(&live).unwrap();
        assert_eq!(j["model"], "sonnet");
        assert_eq!(j["model_actual"], "claude-sonnet-4-5");
        let back: ThreadView = serde_json::from_value(j).unwrap();
        assert_eq!(back, live);
    }

    /// A move to another runtime clears the reported model, and while it is pending the
    /// previous runtime's reports (its last `turn.ended`, an adapter `thread.resumed`) do not
    /// bring it back: the thread shows the requested model until the new runtime reports its
    /// own. Replay equals live at every step.
    #[test]
    fn a_runtime_move_forgets_the_previous_runtimes_model() {
        let ext = |v: serde_json::Value| v.as_object().cloned();
        let claude = || ext(serde_json::json!({"claude": {"model": "claude-opus-4-1"}}));
        let mut s = ThreadScript::new("t1");
        let mut started = s.started("old");
        if let EventBody::ThreadStarted(p) = &mut started.body {
            p.ext = claude();
        }
        let mut ended = s.turn_ended(TurnResult::Completed);
        if let EventBody::TurnEnded(p) = &mut ended.body {
            p.ext = claude();
        }
        let mut resumed = s.resumed("old");
        resumed.source = Source::Adapter("fake".into());
        if let EventBody::ThreadResumed(p) = &mut resumed.body {
            p.ext = claude();
        }
        let mut new_start = s.started("new");
        new_start.source = Source::Adapter("codex".into());
        if let EventBody::ThreadStarted(p) = &mut new_start.body {
            p.ext = ext(serde_json::json!({"codex": {"model": "gpt-6.1-sol"}}));
        }
        let evs = [
            s.created("api"),
            started,
            s.turn_started("go"),
            s.event(EventBody::ThreadModelChanged(ThreadModelChanged {
                model: Some("gpt-6.1".into()),
                runtime: Some("codex".into()),
                previous_runtime: Some("fake".into()),
                ..Default::default()
            })),
            ended,
            resumed,
            new_start,
        ];
        let want = [
            None,
            Some("claude-opus-4-1"),
            Some("claude-opus-4-1"),
            None,
            None,
            None,
            Some("gpt-6.1-sol"),
        ];
        let mut live = ThreadView::new("t1");
        for (i, e) in evs.iter().enumerate() {
            live.apply(e);
            assert_eq!(live.actual_model.as_deref(), want[i], "after step {i}");
            assert_eq!(replayed(&evs[..=i], "t1"), live, "after step {i}");
        }
        let pending = view(&evs[..=5]);
        assert_eq!(
            (pending.runtime.as_str(), pending.shown_model()),
            ("codex", Some("gpt-6.1"))
        );
    }

    #[test]
    fn retry_state_replayed_from_scratch_equals_live() {
        let mut s = ThreadScript::new("t1");
        let evs = vec![
            s.created("api"),
            s.turn_started("go"),
            s.turn_ended(TurnResult::Error),
            s.retry_turn_started(1),
            s.crashed(1),
            s.turn_ended(TurnResult::Interrupted),
            s.started("sess2"),
            s.turn_started("next"),
            s.turn_ended(TurnResult::Error),
        ];
        let mut live = ThreadView::new("t1");
        let mut eligibility = Vec::new();
        for (i, e) in evs.iter().enumerate() {
            live.apply(e);
            eligibility.push(live.retry_eligible());
            // A hub restarting at any point rebuilds the same view by replay.
            assert_eq!(replayed(&evs[..=i], "t1"), live, "after step {i}");
        }
        assert_eq!(
            eligibility,
            [false, false, true, false, false, false, false, false, true]
        );
        // And the view survives serialization (clients receive it over the wire).
        let json = serde_json::to_string(&live).unwrap();
        let back: ThreadView = serde_json::from_str(&json).unwrap();
        assert_eq!(back, live);
        assert!(back.retry_eligible());
    }

    // --- registry ----------------------------------------------------------------------------

    #[test]
    fn registry_excludes_hub_thread_and_creates_lazily() {
        let mut hub = ThreadScript::new(HUB_THREAD);
        let mut a = ThreadScript::new("t1");
        let mut b = ThreadScript::new("t2");
        let evs = [
            hub.raise("h1", Question),
            a.at(10).created("api"),
            b.at(5).turn_started("orphan"),
        ];
        let r = Registry::from_events(&evs);
        assert_eq!(r.len(), 2);
        assert!(r.get(HUB_THREAD).is_none());
        let names: Vec<_> = r.list().iter().map(|t| t.id.clone()).collect();
        assert_eq!(names, vec!["t2", "t1"], "oldest first");
        assert_eq!(r.get("t2").unwrap().name, "t2");
        assert_eq!(r.get("t2").unwrap().status(), S::Running);
    }

    #[test]
    fn registry_resolve() {
        let mut a = ThreadScript::new("01AAA");
        let mut b = ThreadScript::new("01BBB");
        let mut c = ThreadScript::new("01CCC");
        let r = Registry::from_events(&[
            a.created_with_aliases("api-server", &["backend"]),
            b.created_with_aliases("api-client", &["fe"]),
            c.created_with_aliases("docs", &["api"]),
        ]);
        assert_eq!(r.resolve("01BBB").unwrap().name, "api-client");
        assert_eq!(r.resolve("docs").unwrap().id, "01CCC");
        assert_eq!(r.resolve("backend").unwrap().id, "01AAA");
        // Alias beats prefix.
        assert_eq!(r.resolve("api").unwrap().id, "01CCC");
        assert_eq!(r.resolve("api-s").unwrap().id, "01AAA");
        assert_eq!(r.resolve("01C").unwrap().id, "01CCC");
        assert!(matches!(
            r.resolve("api-"),
            Err(ResolveError::Ambiguous { candidates, .. }) if candidates.len() == 2
        ));
        assert_eq!(
            r.resolve("zzz").unwrap_err(),
            ResolveError::NotFound("zzz".into())
        );
        assert!(r.resolve("").is_err());
    }

    #[test]
    fn registry_resolve_duplicate_names_and_aliases() {
        let mut a = ThreadScript::new("01AAA");
        let mut b = ThreadScript::new("01BBB");
        let mut c = ThreadScript::new("01CCC");
        let evs = vec![
            a.created_with_aliases("api", &["be"]),
            b.created_with_aliases("api", &["be"]),
            c.created("docs"),
        ];
        let r = Registry::from_events(&evs);
        for q in ["api", "be"] {
            assert!(
                matches!(
                    r.resolve(q),
                    Err(ResolveError::Ambiguous { ref candidates, .. }) if candidates.len() == 2
                ),
                "{q}"
            );
        }
        // Exactly one live duplicate wins.
        let mut evs2 = evs.clone();
        evs2.push(a.stopped());
        let r = Registry::from_events(&evs2);
        assert_eq!(r.resolve("api").unwrap().id, "01BBB");
        assert_eq!(r.resolve("be").unwrap().id, "01BBB");
        // All stopped: ambiguous again.
        evs2.push(b.stopped());
        let r = Registry::from_events(&evs2);
        assert!(matches!(
            r.resolve("api"),
            Err(ResolveError::Ambiguous { .. })
        ));
        // Ids still resolve exactly.
        assert_eq!(r.resolve("01AAA").unwrap().id, "01AAA");
    }

    /// `thread.renamed` gives a thread its new name; the names it had before stay with it
    /// (`former_names`, latest last, never its name now) and still reach it when no thread
    /// has them as a name or alias. Replay folds it the same way.
    #[test]
    fn renames_keep_former_names_reachable() {
        let mut a = ThreadScript::new("01AAA");
        let mut b = ThreadScript::new("01BBB");
        let rename = |s: &mut ThreadScript, from: &str, to: &str| {
            s.event(EventBody::ThreadRenamed(ThreadRenamed {
                name: to.into(),
                previous: from.into(),
                by: ChangedBy::Orchestrator,
                ..Default::default()
            }))
        };
        let evs = vec![
            a.created_with_aliases("twr-1", &["t"]),
            b.created("docs"),
            rename(&mut a, "twr-1", "notifications-design"),
            rename(&mut a, "notifications-design", "notifications"),
        ];
        let mut live = Registry::new();
        for e in &evs {
            live.apply(e);
        }
        let r = Registry::from_events(&evs);
        assert_eq!(r.get("01AAA"), live.get("01AAA"));
        let v = r.get("01AAA").unwrap();
        assert_eq!(v.name, "notifications");
        assert_eq!(v.former_names, ["twr-1", "notifications-design"]);
        assert_eq!(v.aliases, ["t"]);
        for q in ["notifications", "twr-1", "notifications-design", "t"] {
            assert_eq!(r.resolve(q).unwrap().id, "01AAA", "{q}");
        }
        // Back to an earlier name: it leaves the former names.
        let mut evs2 = evs.clone();
        evs2.push(rename(&mut a, "notifications", "twr-1"));
        let r = Registry::from_events(&evs2);
        let v = r.get("01AAA").unwrap();
        assert_eq!(v.name, "twr-1");
        assert_eq!(v.former_names, ["notifications-design", "notifications"]);
        // Serialized for clients only when there are any.
        let j = serde_json::to_value(r.get("01BBB").unwrap()).unwrap();
        assert!(j.get("former_names").is_none());
        let j = serde_json::to_value(v).unwrap();
        assert_eq!(j["former_names"][0], "notifications-design");
    }

    /// A restored thread keeps the former names its `thread.restored` lists, no others; a
    /// former name several threads had is ambiguous, never a prefix of a third thread's name.
    #[test]
    fn restores_keep_the_former_names_they_list() {
        let mut a = ThreadScript::new("01AAA");
        let mut b = ThreadScript::new("01BBB");
        let mut c = ThreadScript::new("01CCC");
        let rename = |s: &mut ThreadScript, from: &str, to: &str| {
            s.event(EventBody::ThreadRenamed(ThreadRenamed {
                name: to.into(),
                previous: from.into(),
                by: ChangedBy::User,
                ..Default::default()
            }))
        };
        let restore = |s: &mut ThreadScript, name: &str, former: &[&str]| {
            s.event(EventBody::ThreadRestored(ThreadRestored {
                item: "01J00000000000000000000000".into(),
                name: name.into(),
                former_names: former.iter().map(|n| n.to_string()).collect(),
                ..Default::default()
            }))
        };
        let mut evs = vec![
            a.created("api"),
            rename(&mut a, "api", "gateway"),
            a.event(EventBody::ThreadDeleted(ThreadDeleted {
                name: "gateway".into(),
                ..Default::default()
            })),
            b.created("api"),
            restore(&mut a, "gateway-restored", &[]),
        ];
        let r = Registry::from_events(&evs);
        assert!(r.get("01AAA").unwrap().former_names.is_empty());
        assert_eq!(r.resolve("api").unwrap().id, "01BBB");
        // Listed: kept (never its name now).
        let mut kept = evs.clone();
        kept[4] = restore(&mut a, "gateway", &["api", "gateway"]);
        let r = Registry::from_events(&kept);
        assert_eq!(r.get("01AAA").unwrap().former_names, ["api"]);
        // Two threads with the same former name: ambiguous, though `api-client` has it as a
        // prefix.
        evs.push(rename(&mut b, "api", "billing"));
        evs.push(c.created("api-client"));
        let r = Registry::from_events(&evs);
        assert_eq!(r.resolve("api").unwrap().id, "01BBB");
        kept.push(rename(&mut b, "api", "billing"));
        kept.push(c.created("api-client"));
        let r = Registry::from_events(&kept);
        assert!(matches!(
            r.resolve("api"),
            Err(ResolveError::Ambiguous { ref candidates, .. }) if candidates.len() == 2
        ));
        assert_eq!(r.resolve("api-c").unwrap().id, "01CCC");
    }

    /// The latest orchestrator `thread.created` is the orchestrator; earlier ones are retired
    /// and never win a name, alias or prefix lookup while the current one matches.
    #[test]
    fn a_new_orchestrator_retires_the_previous_one() {
        let orch = |s: &mut ThreadScript, runtime: &str| {
            let mut ev = s.created_with_aliases("orchestrator", &["orch"]);
            if let EventBody::ThreadCreated(p) = &mut ev.body {
                p.runtime = runtime.into();
                p.ext = serde_json::json!({"twr": {"role": "orchestrator"}})
                    .as_object()
                    .cloned();
            }
            ev
        };
        let mut a = ThreadScript::new("01AAA");
        let mut b = ThreadScript::new("01BBB");
        let mut c = ThreadScript::new("01CCC");
        let mut w = ThreadScript::new("01WWW");
        let mut evs = vec![orch(&mut a, "claude-code"), w.created("api")];
        let r = Registry::from_events(&evs);
        assert!(!r.get("01AAA").unwrap().retired);
        assert_eq!(r.current_orchestrator().unwrap().id, "01AAA");
        // Switched while running (not stopped yet): still the new one.
        evs.push(a.started("s-a"));
        evs.push(orch(&mut b, "codex"));
        let r = Registry::from_events(&evs);
        assert!(r.get("01AAA").unwrap().retired);
        assert!(!r.get("01BBB").unwrap().retired);
        assert!(r.get("01AAA").unwrap().is_orchestrator());
        assert!(!r.get("01AAA").unwrap().is_current_orchestrator());
        assert_eq!(r.current_orchestrator().unwrap().runtime, "codex");
        for q in ["orchestrator", "orch", "orchestr"] {
            assert_eq!(r.resolve(q).unwrap().id, "01BBB", "{q}");
        }
        // Even when the current one is stopped too.
        evs.push(a.stopped());
        evs.push(b.stopped());
        evs.push(orch(&mut c, "grok-build"));
        evs.push(c.stopped());
        let r = Registry::from_events(&evs);
        assert_eq!(r.resolve("orch").unwrap().id, "01CCC");
        assert!(
            !r.get("01WWW").unwrap().retired,
            "workers are never retired"
        );
        assert_eq!(r.resolve("01AAA").unwrap().id, "01AAA");
        // Every prefix of the log agrees with the live fold.
        let mut live = Registry::new();
        for (i, ev) in evs.iter().enumerate() {
            live.apply(ev);
            assert_eq!(live, Registry::from_events(&evs[..=i]));
        }
    }

    // --- attention stack ---------------------------------------------------------------------

    #[test]
    fn attention_raise_annotate_resolve() {
        let mut s = ThreadScript::new("t1");
        let mut st = AttentionStack::new();
        st.apply(&s.raise("p1", Permission));
        st.apply(&s.raise("q1", Question));
        assert_eq!(st.len(), 2);
        st.apply(&s.annotate("p1", "rm -rf in /tmp, fine"));
        assert_eq!(
            st.get("p1").unwrap().note.as_deref(),
            Some("rm -rf in /tmp, fine")
        );
        st.apply(&s.annotate("p1", "second thought"));
        assert_eq!(
            st.get("p1").unwrap().note.as_deref(),
            Some("second thought")
        );
        // Annotating an unknown item is a no-op.
        st.apply(&s.annotate("nope", "x"));
        assert!(st.get("nope").is_none());
        st.apply(&s.resolve("p1"));
        assert!(st.get("p1").is_none());
        assert_eq!(st.len(), 1);
        // Resolving twice is harmless.
        st.apply(&s.resolve("p1"));
        assert_eq!(st.len(), 1);
    }

    #[test]
    fn re_raise_updates_fields_but_keeps_age() {
        let mut s = ThreadScript::new("t1");
        let first = s.at(10).raise("a1", Question);
        let mut st = AttentionStack::from_events(&[first.clone(), s.at(20).raise("b1", Question)]);
        st.apply(&s.annotate("a1", "note"));
        let mut again = s.at(30).raise("a1", Permission);
        if let EventBody::AttentionRaised(p) = &mut again.body {
            p.summary = "updated".into();
            p.options = vec!["yes".into()];
            p.call_id = Some("c9".into());
        }
        st.apply(&again);
        assert_eq!(st.len(), 2);
        let item = st.get("a1").unwrap();
        assert_eq!(item.raised_at, first.ts);
        assert_eq!(item.event_id, first.id);
        assert_eq!(item.reason, Permission);
        assert_eq!(item.summary, "updated");
        assert_eq!(item.options, ["yes"]);
        assert_eq!(item.call_id.as_deref(), Some("c9"));
        assert_eq!(item.note.as_deref(), Some("note"));
        // Still the oldest question-or-better; now a permission, so it leads.
        assert_eq!(st.top(&no_pins()).unwrap().attn_id, "a1");
    }

    #[test]
    fn revisions_follow_the_raises_gseq() {
        let mut s = ThreadScript::new("t1");
        let mut st = AttentionStack::new();
        st.apply_at(&s.raise("a1", Permission), Some(4));
        let item = st.get("a1").unwrap();
        assert_eq!((item.revision, item.first_revision), (4, 4));
        st.apply_at(&s.raise("a1", Permission), Some(9));
        let item = st.get("a1").unwrap();
        assert_eq!((item.revision, item.first_revision), (9, 4));
        // Folded without a position: the revision is unknown, never the stale one.
        st.apply(&s.raise("a1", Permission));
        assert_eq!(st.get("a1").unwrap().revision, 0);
        // Resolved and raised again: a new item.
        st.apply_at(&s.resolve("a1"), Some(12));
        st.apply_at(&s.raise("a1", Question), Some(13));
        let item = st.get("a1").unwrap();
        assert_eq!((item.revision, item.first_revision), (13, 13));
    }

    #[test]
    fn attention_orders_by_reason_first() {
        let mut s = ThreadScript::new("t1");
        let st = AttentionStack::from_events(&[
            s.at(0).raise("stalled", Stalled),
            s.at(1).raise("review", Review),
            s.at(2).raise("error", AttentionReason::Error),
            s.at(3).raise("question", Question),
            s.at(4).raise("permission", Permission),
        ]);
        let ids: Vec<_> = st
            .ordered(&no_pins())
            .iter()
            .map(|i| i.attn_id.as_str())
            .collect();
        assert_eq!(
            ids,
            ["permission", "question", "error", "review", "stalled"]
        );
        assert_eq!(st.top(&no_pins()).unwrap().attn_id, "permission");
    }

    #[test]
    fn attention_orders_by_age_within_reason() {
        let mut a = ThreadScript::new("t1");
        let mut b = ThreadScript::new("t2");
        let st = AttentionStack::from_events(&[
            a.at(30).raise("newer", Question),
            b.at(10).raise("older", Question),
            b.at(20).raise("middle", Question),
        ]);
        let ids: Vec<_> = st
            .ordered(&no_pins())
            .iter()
            .map(|i| i.attn_id.as_str())
            .collect();
        assert_eq!(ids, ["older", "middle", "newer"]);
    }

    #[test]
    fn pin_breaks_ties_after_reason_and_age() {
        let mut a = ThreadScript::new("t1");
        let mut b = ThreadScript::new("t2");
        let st = AttentionStack::from_events(&[
            a.at(10).raise("a", Question),
            b.at(10).raise("b", Question),
            b.at(5).raise("perm", Permission),
            a.at(1).raise("old-review", Review),
        ]);
        let unpinned: Vec<_> = st
            .ordered(&no_pins())
            .iter()
            .map(|i| i.attn_id.clone())
            .collect();
        // Same reason + age: pin decides; pinning by thread id or attn id both work.
        for pin in ["t2", "b"] {
            let pins: HashSet<String> = [pin.to_string()].into();
            let ids: Vec<_> = st
                .ordered(&pins)
                .iter()
                .map(|i| i.attn_id.as_str())
                .collect();
            assert_eq!(ids, ["perm", "b", "a", "old-review"], "pin {pin}");
        }
        let pins: HashSet<String> = ["t1".to_string()].into();
        let ids: Vec<_> = st
            .ordered(&pins)
            .iter()
            .map(|i| i.attn_id.as_str())
            .collect();
        assert_eq!(ids, ["perm", "a", "b", "old-review"]);
        // Pin never beats reason or age.
        let pins: HashSet<String> = ["old-review".to_string()].into();
        assert_eq!(st.ordered(&pins).last().unwrap().attn_id, "old-review");
        assert_eq!(unpinned.len(), 4);
    }

    #[test]
    fn crash_or_stop_drops_blocking_items_of_that_thread() {
        let mut a = ThreadScript::new("t1");
        let mut b = ThreadScript::new("t2");
        let mut st = AttentionStack::from_events(&[
            a.raise("p1", Permission),
            a.raise("e1", AttentionReason::Error),
            b.raise("q2", Question),
        ]);
        st.apply(&a.crashed(1));
        assert!(st.get("p1").is_none());
        assert!(st.get("e1").is_some());
        assert!(st.get("q2").is_some());
        st.apply(&b.stopped());
        assert!(st.get("q2").is_none());
    }

    #[test]
    fn hub_thread_attention_is_kept() {
        let mut hub = ThreadScript::new(HUB_THREAD);
        let st = AttentionStack::from_events(&[hub.raise("route?", Question)]);
        assert_eq!(st.top(&no_pins()).unwrap().thread, HUB_THREAD);
    }

    // --- stack order -------------------------------------------------------------------------

    #[test]
    fn stack_order_attention_first_then_status_then_activity() {
        let mut hub = ThreadScript::new(HUB_THREAD);
        let mut perm = ThreadScript::new("perm");
        let mut review = ThreadScript::new("review");
        let mut running = ThreadScript::new("running");
        let mut errored = ThreadScript::new("errored");
        let mut done = ThreadScript::new("done");
        let mut idle_old = ThreadScript::new("idle-old");
        let mut idle_new = ThreadScript::new("idle-new");
        let mut parked = ThreadScript::new("parked");
        let evs = vec![
            hub.at(0).raise("h", Question),
            perm.at(0).created("perm"),
            perm.turn_started("x"),
            perm.at(50).raise("p", Permission),
            review.at(1).created("review"),
            review.at(2).raise("r", Review),
            running.at(3).created("running"),
            running.turn_started("x"),
            errored.at(4).created("errored"),
            errored.turn_started("x"),
            errored.turn_ended(TurnResult::Error),
            done.at(6).created("done"),
            done.turn_started("x"),
            done.turn_ended(TurnResult::Completed),
            done.report(None, Grade::Success),
            idle_old.at(10).created("idle-old"),
            idle_new.at(20).created("idle-new"),
            parked.at(100).created("parked"),
            parked.parked(),
        ];
        let reg = Registry::from_events(&evs);
        let st = AttentionStack::from_events(&evs);
        let order = stack_order(&reg, &st, &no_pins(), CardOrder::Attention);
        assert_eq!(
            order,
            ["perm", "review", "running", "errored", "done", "idle-new", "idle-old", "parked"]
        );
        // Stable: the same groups, each by creation.
        let order = stack_order(&reg, &st, &no_pins(), CardOrder::Stable);
        assert_eq!(
            order,
            ["perm", "review", "running", "errored", "done", "idle-old", "idle-new", "parked"]
        );
    }

    /// Stable order: running, background and waiting threads are one group, ordered by when
    /// they were created however their activity interleaves and their status flips; a pinned
    /// thread leads its group.
    #[test]
    fn stable_order_keeps_working_threads_in_creation_order() {
        let mut a = ThreadScript::new("a");
        let mut b = ThreadScript::new("b");
        let mut c = ThreadScript::new("c");
        let mut evs = vec![
            a.at(1).created("a"),
            b.at(2).created("b"),
            c.at(3).created("c"),
            a.turn_started("x"),
            b.turn_started("x"),
            c.turn_started("x"),
        ];
        let order = |evs: &[Event], pins: &HashSet<String>| {
            let reg = Registry::from_events(evs);
            let st = AttentionStack::from_events(evs);
            stack_order(&reg, &st, pins, CardOrder::Stable)
        };
        assert_eq!(order(&evs, &no_pins()), ["a", "b", "c"]);
        // c acts last, then a, then b; b waits on background work: still one group, in order.
        let text = || {
            EventBody::AgentText(AgentText {
                text: "working".into(),
                ..Default::default()
            })
        };
        for (t, who) in [(11, 2), (12, 0), (13, 1), (14, 2)] {
            let s = [&mut a, &mut b, &mut c][who].at(t).event(text());
            evs.push(s);
            assert_eq!(order(&evs, &no_pins()), ["a", "b", "c"]);
        }
        evs.push(b.event(EventBody::TaskUpdated(TaskUpdated {
            task_id: "t1".into(),
            running: true,
            ..Default::default()
        })));
        evs.push(b.turn_ended(TurnResult::Completed));
        assert_eq!(
            Registry::from_events(&evs).get("b").unwrap().status(),
            ThreadStatus::Background
        );
        assert_eq!(order(&evs, &no_pins()), ["a", "b", "c"]);
        // The recency order moves them on every event.
        let reg = Registry::from_events(&evs);
        let st = AttentionStack::from_events(&evs);
        assert_eq!(
            stack_order(&reg, &st, &no_pins(), CardOrder::Attention),
            ["b", "c", "a"]
        );
        let pins: HashSet<String> = ["c".to_string()].into();
        assert_eq!(order(&evs, &pins), ["c", "a", "b"]);
    }

    #[test]
    fn stack_order_uses_top_item_per_thread() {
        let mut a = ThreadScript::new("a");
        let mut b = ThreadScript::new("b");
        let evs = vec![
            a.at(0).raise("a-stalled", Stalled),
            a.at(40).raise("a-question", Question),
            b.at(10).raise("b-question", Question),
        ];
        for mode in [CardOrder::Stable, CardOrder::Attention] {
            let order = stack_order(
                &Registry::from_events(&evs),
                &AttentionStack::from_events(&evs),
                &no_pins(),
                mode,
            );
            // b's question is older than a's question.
            assert_eq!(order, ["b", "a"], "{mode:?}");
        }
    }

    #[test]
    fn stack_order_pin_breaks_ties_among_quiet_threads() {
        let mut a = ThreadScript::new("a");
        let mut b = ThreadScript::new("b");
        let evs = vec![a.at(5).created("a"), b.at(5).created("b")];
        let reg = Registry::from_events(&evs);
        let st = AttentionStack::new();
        for mode in [CardOrder::Stable, CardOrder::Attention] {
            assert_eq!(stack_order(&reg, &st, &no_pins(), mode), ["a", "b"]);
            let pins: HashSet<String> = ["b".to_string()].into();
            assert_eq!(stack_order(&reg, &st, &pins, mode), ["b", "a"]);
        }
    }

    #[test]
    fn status_serializes_snake_case() {
        assert_eq!(serde_json::to_value(S::Waiting).unwrap(), "waiting");
        assert_eq!(S::Attached.to_string(), "attached");
    }
    #[test]
    fn background_work_survives_turn_end_and_report_but_not_runtime_exit() {
        let mut script = ThreadScript::new("background");
        let mut view = ThreadView::new("background");
        for event in [
            script.created("background"),
            script.started("session"),
            script.turn_started("review"),
        ] {
            view.apply(&event);
        }
        let task = |id: &str, running| {
            EventBody::TaskUpdated(TaskUpdated {
                task_id: id.into(),
                running,
                description: "tests".into(),
                ..Default::default()
            })
        };
        view.apply(&script.event(task("tests", true)));
        view.apply(&script.event(task("review", true)));
        assert_eq!(view.status(), S::Running);
        view.apply(&script.turn_ended(TurnResult::Completed));
        view.apply(&script.report(Some(1), Grade::Success));
        assert_eq!(view.status(), S::Background);
        assert_eq!(view.status_label(), "background (2 bg)");
        view.apply(&script.event(task("tests", false)));
        view.apply(&script.event(task("tests", false)));
        assert_eq!(view.background_tasks.len(), 1);
        view.apply(&script.turn_started("automatic follow-up"));
        assert_eq!(view.status(), S::Running);
        view.apply(&script.turn_ended(TurnResult::Completed));
        for exit in [
            EventBody::ThreadStopped(SessionPayload::default()),
            EventBody::ThreadParked(SessionPayload::default()),
            EventBody::ThreadCrashed(ThreadCrashed::default()),
            EventBody::ThreadStarted(SessionPayload::default()),
            EventBody::ThreadAttached(ClientPayload::default()),
        ] {
            let mut copy = view.clone();
            copy.apply(&script.event(exit));
            assert!(copy.background_tasks.is_empty());
            assert!(!copy.has_work());
        }
        view.apply(&script.event(task("review", false)));
        assert!(!view.has_work());
        assert_eq!(view.status(), S::Idle);
    }

    #[test]
    fn background_heartbeat_renews_liveness_without_claiming_progress() {
        let mut script = ThreadScript::new("background");
        let mut view = ThreadView::new("background");
        view.apply(&script.created("background"));
        view.apply(&script.event(EventBody::TaskUpdated(TaskUpdated {
            task_id: "test".into(),
            running: true,
            ..Default::default()
        })));
        let activity = view.last_activity;
        let event = script.at(100).event(EventBody::TaskUpdated(TaskUpdated {
            task_id: "test".into(),
            running: true,
            heartbeat: true,
            ..Default::default()
        }));
        assert!(event.is_control());
        view.apply(&event);
        assert_eq!(view.last_activity, activity);
        assert_eq!(view.background_tasks["test"].updated_at, event.ts);
        let mut old = serde_json::to_value(&view).unwrap();
        old.as_object_mut().unwrap().remove("background_tasks");
        assert!(serde_json::from_value::<ThreadView>(old)
            .unwrap()
            .background_tasks
            .is_empty());
    }

    #[test]
    fn a_new_runtime_can_start_background_work_after_a_crash_without_a_prompt() {
        let mut script = ThreadScript::new("background");
        let mut view = ThreadView::new("background");
        view.apply(&script.created("background"));
        view.apply(&script.event(EventBody::ThreadCrashed(ThreadCrashed::default())));
        let start = EventBody::TaskUpdated(TaskUpdated {
            task_id: "work".into(),
            running: true,
            ..Default::default()
        });
        view.apply(&script.event(start.clone()));
        assert!(
            !view.has_work(),
            "late work from the crashed process stays closed"
        );
        view.apply(&script.started("new-session"));
        view.apply(&script.event(start));
        assert_eq!(view.status(), S::Background);
        assert!(view.current_turn.is_none());
    }
}
