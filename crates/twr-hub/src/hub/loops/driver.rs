//! The driver: what makes a started review loop run to its end. The machine
//! (`twr_core::loops::LoopState`) says what to do as [`Action`]s; the driver carries each out
//! and steps everything that happens back into it as an [`Input`], writing each fact with the
//! records its step decided in one batch, together with what follows from it where that can
//! be a write: the prompt it sends the thread, the stuck item it raises, the review run's task
//! it ends.
//!
//! **Prompts** ([`Core::stage`]) are queued as a job's wake prompt is (`from: runtime`, source
//! `hub`), in the batch of the step that decided them, and start with the loop's marker line
//! (`[twr loop <id> <step>]`, `hub::loops::prompts`). The thread's queue orders them with
//! whatever else it has: a message the user sends during a loop goes ahead as it was queued,
//! and the loop goes on after it. A loop that ends, by any path (`loop.control stop`, its
//! thread stopped, parked or deleted, a failure, its end), takes its prompts still queued with
//! it: [`Core::append_batch`] removes their rows in the batch that records `loop.ended`, each
//! with a `prompt.withdrawn` (source `hub`, `ext.twr.loop_ended`: the loop), as a message the
//! user took back is ([`Core::loop_withdrawals`]); and a loop's prompt whose loop is not the
//! one its thread runs never starts a turn: the dispatch withdraws it the same way and goes on
//! with the queue ([`Core::stale_loop_prompt`]).
//!
//! **Which turn answers** ([`Core::turn_owner`]). The hub keeps, per thread, the latest turn a
//! loop prompt started (`asks`, from its `turn.started`) and the latest turn the loop took as an
//! answer or a failure (`consumed`, from the turn stamped on those facts); both are folded
//! from the log ([`super::LoopsState`]). While the loop waits for a thread step (scope, a
//! triage, the sweep, the report), not held, a turn `T` that ends is the step's when:
//!
//! - `T` is the step's ask turn: its prompt carries the step's marker, and no turn since that
//!   ask was taken (`consumed < ask ≤ T`); or
//! - `T` came after that ask and the runtime started it (`from: runtime`) with no loop marker
//!   of its own: the follow-up of an interim turn (a thread that ran its gates as a job ends
//!   its turn with an interim report and answers in the turn its runtime, or the job's wake,
//!   starts later), or the automatic retry of a failed one.
//!
//! Such a turn's end steps the loop: an interim end (background work still running) waits;
//! a completed end is parsed (`LoopState::reply_input`) and accepted or refused, a missing
//! block being a refused answer like any other (one correction, then stuck), and the report
//! step's end writes `report.md`; a failed end (`error`, a crash) is `TurnFailed` (stuck)
//! unless the hub retries it automatically, whose retry turn is then the step's; an interrupted
//! one is `TurnFailed` too, but not when the hub's own shutdown interrupted it: the next hub
//! asks for the step again. A turn the user or the orchestrator started in between is not the
//! loop's, unless its final message carries a block the step accepts: then it answers the step
//! (and the ask turn that comes later is not taken); one without such a block changes nothing.
//! A turn that ends with no reply is a refused answer. Each turn is taken at most once: the
//! facts stamp it, and a reaction run again (startup) finds it taken.
//!
//! The reaction runs where the hub already reacts to a worker's turn end: the turn's report and
//! the loop's step are one batch (`hub::reactions`), the report marked `ext.twr.loop = {id,
//! step}`; a failed turn's after its block kind is decided (`hub::retry`).
//!
//! **Review runs** ([`run_review`]) are tasks of the hub's, one per thread at a time
//! ([`LoopRuns`]): the round's diffs, ledger, goal and prompt go into the loop folder, then
//! `loop.review_started` with the run's background task (`task.updated` `loop:<loop id>`, its
//! description `reviewer <model> <effort>`), the run itself (`twr_runner::loops::review` through `place`, on the thread's box), and its end:
//! the task's end, `loop.findings` and what follows, one batch. A run that answers no findings
//! has its diffs built again first: when every one is byte for byte what the round read (none
//! cut), `loop.findings` says `unchanged`, which is what lets the machine skip a verification
//! round that could only repeat this one. A run takes no job slot.
//! Stopping, parking or deleting the thread, `loop.control stop`, any end of the loop and the
//! hub's shutdown cancel it (its process group killed); a pause lets it finish and holds what
//! follows. A run the hub's shutdown cancelled records nothing: the next hub finds its task
//! still running in the log and its round started, and records it failed (the machine retries
//! it once), once what is left of its process group is killed.
//!
//! **Watching** ([`Core::check_loops`], on every kick and every tick of the background
//! ticker): a loop that waits for a review no run of this hub's serves gets one (or, when its
//! round had started, a failed run); one that waits for a thread step whose prompt is neither
//! queued nor delivered gets it again (a queue that refused it, a turn the hub's shutdown
//! interrupted, a turn whose runtime died with a hub that crashed); and, once after the hub
//! started, a step whose turn ended before a hub took it is taken as at its end: a completed
//! one with the turn's other reactions, in the start-up recovery that completes them (its
//! report marked, no review item), a failed one, or one whose runtime crashed on its own, as
//! `TurnFailed` (stuck). Nothing else runs before the hub's start-up recovery is done.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use serde_json::{json, Value};
use twr_core::event::*;
use twr_core::loops::contract::{render_ledger, FINDINGS_SCHEMA};
use twr_core::loops::{Action, Input, LoopKind, LoopState, ReviewTemplate, Step, StepError, Wait};
use twr_core::proto::codes;
use twr_core::reduce::ThreadView;

use super::super::{twr_ext, twr_ext_map, Batch, Core, Inner, LOCAL_BOX};
use super::place::{self, Place, Trouble};
use super::prompts;
use super::report::{self as loop_report, LoopLog};
use super::review;

/// How often a running review's task is renewed: for the log only.
const RENEW_EVERY: Duration = Duration::from_secs(5 * 60);

/// A review run of this hub's.
pub(in crate::hub) struct LoopRun {
    /// This run (a retry is another).
    run: String,
    /// Its loop.
    loop_id: String,
    /// The reviewer's model and the round's effort, as its task says them.
    model: String,
    effort: String,
    /// When it began.
    began: Instant,
    /// Ends it, with the reason.
    cancel: tokio::sync::watch::Sender<Option<String>>,
}

/// The review runs this hub runs now, by thread, and whether loops are driven yet (not before
/// the hub's start-up recovery is done). Memory only.
#[derive(Default)]
pub(in crate::hub) struct LoopRuns {
    runs: HashMap<String, LoopRun>,
    /// The audit runs (`sweep = "runs"`), by thread: up to `max_audit_runs` each.
    pub(super) audits: HashMap<String, Vec<super::audit::AuditRun>>,
    /// The commit checks running, by thread: the loop and the turn each checks.
    checks: HashMap<String, (String, u64)>,
    pub(super) ready: bool,
    /// Per thread on a remote box, the loop whose folder the hub last wrote to the box's copy
    /// and the box connection it wrote it on ([`Core::sync_box_folder`]): a new connection
    /// (the box back, the hub restarted) is written to again, a box that lost the folder gets
    /// it back.
    synced: HashMap<String, (String, u64)>,
    /// The threads whose runtime died with the previous hub (closed as crashed at this hub's
    /// start, `close_stale_runtimes`): their step is asked again, not taken as failed.
    pub(in crate::hub) died_with_hub: std::collections::HashSet<String>,
}

impl LoopRuns {
    /// Ends thread `thread`'s review run, if any, with `why`.
    pub(in crate::hub) fn cancel(&self, thread: &str, why: &str) {
        if let Some(r) = self.runs.get(thread) {
            let _ = r.cancel.send(Some(why.to_string()));
        }
        for a in self.audits.get(thread).into_iter().flatten() {
            a.cancel(why);
        }
    }

    /// Ends every review and audit run.
    pub(in crate::hub) fn cancel_all(&self, why: &str) {
        for r in self.runs.values() {
            let _ = r.cancel.send(Some(why.to_string()));
        }
        for a in self.audits.values().flatten() {
            a.cancel(why);
        }
    }

    /// Whether any runs.
    pub(in crate::hub) fn any(&self) -> bool {
        !self.runs.is_empty() || self.audits.values().any(|v| !v.is_empty())
    }
}

/// Who a turn's end is to the loop its thread runs ([`Core::turn_owner`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Owner {
    /// The step's own turn (its ask, a follow-up, a retry).
    Loop,
    /// A turn the user or the orchestrator started: the step's only with an accepted block.
    IfBlock,
}

/// The step a loop waits for in a thread turn, as its prompts' marker names it (`scope`,
/// `plan`, `triage p1`, `implement`, `triage m3`, `rules`, `sweep`, `report`, `commit`);
/// `None` while it waits for a review, the sweep's audit runs, the hub's own `report.md`, or
/// nothing.
pub(in crate::hub) fn step_tag(s: &LoopState) -> Option<String> {
    match &s.wait {
        Wait::Goal => Some("scope".into()),
        Wait::Triage { round } => Some(format!("triage {round}")),
        Wait::Sweep => Some("sweep".into()),
        Wait::Report => Some("report".into()),
        Wait::Plan => Some("plan".into()),
        Wait::Implement => Some("implement".into()),
        Wait::Commit => Some("commit".into()),
        Wait::Rules => Some("rules".into()),
        Wait::Review { .. } | Wait::Audits | Wait::WriteReport | Wait::Ended => None,
    }
}

/// What a staged step leaves to do once its batch is committed.
#[derive(Debug, Default, Clone, Copy)]
pub(in crate::hub) struct Staged {
    /// A prompt was queued: the thread's queue is to be dispatched.
    pub(in crate::hub) prompted: bool,
    /// A review is to run.
    pub(in crate::hub) review: bool,
    /// A commit is to be checked (the watch runs the check).
    pub(in crate::hub) check: bool,
    /// The commit is settled: `report.md` is to be written.
    pub(in crate::hub) write_report: bool,
    /// Audit runs are to start (`sweep = "runs"`).
    pub(in crate::hub) audits: bool,
}

impl Staged {
    pub(super) fn and(self, other: Staged) -> Staged {
        Staged {
            prompted: self.prompted || other.prompted,
            review: self.review || other.review,
            check: self.check || other.check,
            write_report: self.write_report || other.write_report,
            audits: self.audits || other.audits,
        }
    }

    /// Whether the loops' watch is to look at once (a review or a check due, a prompt queued).
    pub(in crate::hub) fn kicks(&self) -> bool {
        self.review || self.prompted || self.check || self.write_report || self.audits
    }
}

/// What a turn's end brings its thread's loop ([`Core::loop_answer`]).
pub(in crate::hub) struct LoopAnswer {
    /// `report.ready` `ext.twr.loop` of the turn's report.
    pub(in crate::hub) mark: Value,
    /// The report's summary instead of the turn's own (the loop's last report).
    pub(in crate::hub) summary: Option<String>,
    /// What to step; `None`: an interim end, the step still waits, or a commit to check.
    input: Option<Input>,
    /// The turn answered a commit step: its check is due ([`Core::check_loops`] runs it).
    check: bool,
}

/// What loop `state` committed, for its report and end.
fn committed_of(state: &LoopState) -> loop_report::Committed {
    loop_report::Committed {
        mode: state.commit,
        commits: state.commits.clone(),
        problem: state.commit_problem.clone(),
    }
}

impl LoopAnswer {
    /// What [`Inner::loop_record_point`] calls this answer's record, if it steps the loop or
    /// makes a commit's check due: `answer <step>`.
    pub(in crate::hub) fn point(&self) -> Option<String> {
        if self.input.is_none() && !self.check {
            return None;
        }
        Some(format!(
            "answer {}",
            self.mark["step"].as_str().unwrap_or_default()
        ))
    }
}

/// The folder of loop `id` as `log` records it (else where the hub puts loop folders).
pub(super) fn folder_of(hub: &Inner, log: &LoopLog, id: &str) -> PathBuf {
    log.started
        .as_ref()
        .and_then(|s| s.folder.as_deref())
        .map(PathBuf::from)
        .unwrap_or_else(|| loops_root(hub).join(id))
}

/// Where loop `id`'s thread and reviewer read its folder, as `log` records it: on a remote
/// box, the box's copy (`loop.started`'s `box_folder`, `hub::loops::place`); else the hub's
/// own ([`folder_of`]), which is the record either way.
pub(super) fn shown_folder(hub: &Inner, log: &LoopLog, id: &str) -> PathBuf {
    log.started
        .as_ref()
        .and_then(|s| s.ext.as_ref())
        .and_then(|e| e.get("twr"))
        .and_then(|t| t.get(super::LOOP_START_KEY))
        .and_then(|s| s.get("box_folder"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| folder_of(hub, log, id))
}

/// The files of the loop folder a thread's step or a reviewer may read, as the hub's
/// `record` has them: what a remote box's copy is given before they read it
/// ([`Place::sync`]).
pub(super) fn shared_files(record: &Path) -> Vec<String> {
    let fixed = [
        "findings.schema.json",
        "ledger.md",
        "goal.md",
        "task.md",
        "plan.md",
        "plan-1.md",
        "audits.md",
        "report.md",
    ];
    let mut out: Vec<String> = fixed
        .iter()
        .filter(|n| record.join(n).is_file())
        .map(|n| n.to_string())
        .collect();
    if let Ok(entries) = std::fs::read_dir(record) {
        for e in entries.flatten() {
            let n = e.file_name().to_string_lossy().into_owned();
            if n.starts_with("baseline-") && (n.ends_with(".patch") || n.ends_with(".paths.json")) {
                out.push(n);
            }
        }
    }
    out
}

/// `<data>/loops`.
fn loops_root(hub: &Inner) -> PathBuf {
    hub.db_path
        .parent()
        .map(|d| d.join("loops"))
        .unwrap_or_else(|| PathBuf::from("loops"))
}

/// Makes the loop folder (and `<data>/loops`), private.
fn make_folder(hub: &Inner, folder: &Path) -> anyhow::Result<()> {
    twr_core::paths::ensure_private_dir(&loops_root(hub))?;
    twr_core::paths::ensure_private_dir(folder)
}

/// The files of the loop folder every prompt refers to, as `log` has them now: the findings
/// schema, `ledger.md`, and a review loop's `goal.md` or an implementation loop's `task.md`
/// (when it has a task text). (`plan.md` is written when the plan is taken, and the baseline's
/// files when the loop starts.)
pub(super) fn write_common(hub: &Inner, folder: &Path, log: &LoopLog) -> anyhow::Result<()> {
    make_folder(hub, folder)?;
    std::fs::write(folder.join("findings.schema.json"), FINDINGS_SCHEMA)?;
    std::fs::write(folder.join("ledger.md"), render_ledger(&log.ledger))?;
    match log.kind() {
        LoopKind::Review => {
            let goal = log
                .goal
                .as_deref()
                .unwrap_or("(not written yet: the scope turn writes it)");
            std::fs::write(
                folder.join("goal.md"),
                format!("# Goal\n\n{}\n", goal.trim()),
            )?;
        }
        LoopKind::Implement => {
            if let Some(task) = task_of(log) {
                std::fs::write(folder.join("task.md"), format!("# Task\n\n{task}\n"))?;
            }
        }
    }
    Ok(())
}

/// An implementation loop's task text, if it has one (none: what the conversation settled).
fn task_of(log: &LoopLog) -> Option<String> {
    log.started
        .as_ref()
        .and_then(|s| s.task.as_deref())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

/// The most bytes of a task a prompt quotes whole; a longer one is read from `task.md`.
const TASK_INLINE_MAX: usize = 2000;

/// The task as a prompt names it: the thread's plan prompt (`thread`: what to do without
/// one), or a reviewer's (`GOAL / TASK:`).
pub(super) fn task_words(log: &LoopLog, folder: &Path, thread: bool) -> String {
    match task_of(log) {
        Some(t) if t.len() <= TASK_INLINE_MAX => t,
        Some(_) => format!(
            "the task is in {} (read it whole)",
            folder.join("task.md").display()
        ),
        None if thread => "(No task text was given: the task is what this conversation has \
                           already settled. State it in the plan's first section, in your own \
                           words: the reviewer and every later step read only the plan.)"
            .into(),
        None => "as the plan states it (its goal and interpretation): read the plan".into(),
    }
}

/// The baseline of `log`'s loop as a prompt names it: `out of scope` lines per repository
/// whose worktree had changes when the loop began (empty when none had). `reviewer`: the
/// impl-review template's wording.
pub(super) fn baseline_words(log: &LoopLog, folder: &Path, reviewer: bool) -> String {
    let Some(started) = log.started.as_ref() else {
        return String::new();
    };
    let notes = loop_report::baseline_notes(started);
    if notes.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for (slug, summary) in notes {
        let path = folder.join(format!("baseline-{slug}.patch"));
        if reviewer {
            out.push_str(&format!(
                "\nOUT OF SCOPE — already modified before this task started (see {}); review \
                 only changes beyond them:\n{summary}\n",
                path.display()
            ));
        } else {
            out.push_str(&format!(
                "\nOut of scope: the worktree already held changes when the loop began ({summary}; \
                 see {}). They are not this task's: leave them as they are, and out of any \
                 commit.\n",
                path.display()
            ));
        }
    }
    out
}

/// `- <repo> (from <base>)` per repository of `log`'s start.
pub(super) fn repos_list(log: &LoopLog) -> String {
    let started = log.started.clone().unwrap_or_default();
    if started.bases.is_empty() {
        return "- (none recorded)".into();
    }
    started
        .bases
        .iter()
        .map(|b| format!("- {} (from {})", b.repo, b.base))
        .collect::<Vec<_>>()
        .join("\n")
}

impl Core {
    /// Thread `thread`'s running loop, if it runs one.
    pub(in crate::hub) fn loop_state(&self, thread: &str) -> Option<LoopState> {
        self.registry
            .get(thread)?
            .r#loop
            .clone()
            .filter(|l| !l.ended)
    }

    /// Loop `id` of thread `thread` as its log tells it (hydrated).
    pub(in crate::hub) fn loop_log(&self, thread: &str, id: &str) -> LoopLog {
        let conn = self.store.conn();
        match crate::store::loop_events(conn, thread) {
            Ok(evs) => {
                let evs: Vec<Event> = evs
                    .into_iter()
                    .map(|ev| super::super::append::hydrated_from_store(conn, &ev).unwrap_or(ev))
                    .collect();
                LoopLog::of(id, &evs)
            }
            Err(e) => {
                tracing::warn!(%thread, error = %format!("{e:#}"), "cannot read a loop's log");
                LoopLog::default()
            }
        }
    }

    /// Loop `id` of thread `thread` as its log tells it with `batch`'s events of it, not
    /// written yet, after the log's: what a prompt staged in that batch reads (the audit that
    /// ended the sweep's runs, the triage that ended the ladder).
    pub(in crate::hub) fn loop_log_with(&self, thread: &str, id: &str, batch: &Batch) -> LoopLog {
        let conn = self.store.conn();
        let mut evs: Vec<Event> = match crate::store::loop_events(conn, thread) {
            Ok(evs) => evs
                .into_iter()
                .map(|ev| super::super::append::hydrated_from_store(conn, &ev).unwrap_or(ev))
                .collect(),
            Err(e) => {
                tracing::warn!(%thread, error = %format!("{e:#}"), "cannot read a loop's log");
                Vec::new()
            }
        };
        let next = evs.last().map_or(0, |e| e.seq) + 1;
        evs.extend(
            batch
                .events
                .iter()
                .filter(|e| e.thread == thread && e.body.kind().starts_with("loop."))
                .enumerate()
                .map(|(i, e)| {
                    Event::new(
                        thread,
                        next + i as u64,
                        e.turn,
                        e.source.clone(),
                        e.body.clone(),
                    )
                }),
        );
        LoopLog::of(id, &evs)
    }

    /// Steps `input` into thread `thread`'s loop and stages what it decided into `batch`
    /// ([`Core::stage`]); facts and records stamped `turn` (the thread turn they take).
    pub(in crate::hub) fn loop_step(
        &mut self,
        hub: &Inner,
        batch: &mut Batch,
        thread: &str,
        input: Input,
        turn: Option<u64>,
    ) -> Result<Staged, StepError> {
        let mut state = self.loop_state(thread).ok_or(StepError::Ended)?;
        let actions = state.step(input, Utc::now())?;
        Ok(self.stage(hub, batch, thread, &state, actions, turn, None, None))
    }

    /// Stages the actions of thread `thread`'s loop, stepped to `state`, into `batch`: each
    /// record (stamped `turn`; the first one recorded as from `first_source` when given, the
    /// fact's author), each prompt to the thread (queued; one that cannot be queued now is
    /// asked again by [`Core::check_loops`]), the stuck item; and the stuck item closed when
    /// the loop resumes, stops or ends. `log` is the loop's log when the caller has it (a start
    /// has it before its `loop.started` is written).
    #[allow(clippy::too_many_arguments)]
    pub(in crate::hub) fn stage(
        &mut self,
        hub: &Inner,
        batch: &mut Batch,
        thread: &str,
        state: &LoopState,
        actions: Vec<Action>,
        turn: Option<u64>,
        first_source: Option<Source>,
        log: Option<LoopLog>,
    ) -> Staged {
        let mut staged = Staged::default();
        let mut first = first_source;
        let mut closes = false;
        let mut log = log;
        for a in actions {
            match a {
                Action::Record(body) => {
                    closes |= matches!(&body, EventBody::LoopControl(c)
                        if matches!(c.action, twr_core::loops::ControlAction::Resume | twr_core::loops::ControlAction::Stop))
                        || matches!(body, EventBody::LoopEnded(_));
                    let source = first.take().unwrap_or(Source::Hub);
                    batch.push(thread, turn, source, body);
                }
                Action::RunReview { .. } => {
                    first = None;
                    staged.review = true;
                }
                Action::WriteReport => {
                    first = None;
                    staged.write_report = true;
                }
                Action::RunAudits => {
                    first = None;
                    staged.audits = true;
                }
                Action::Stuck { why } => {
                    first = None;
                    self.close_stuck(batch, thread, None);
                    let name = self.thread_name(thread);
                    batch.push(
                        thread,
                        None,
                        Source::Hub,
                        EventBody::AttentionRaised(AttentionRaised {
                            attn_id: new_id(),
                            reason: AttentionReason::Error,
                            summary: format!(
                                "{name}: its {} is stuck: {}. Resume asks for that step again; \
                                 stop ends the loop.",
                                loop_report::kind_words(state.kind),
                                prompts::one_line(&why)
                            ),
                            options: vec![LOOP_STUCK_RESUME.into(), LOOP_STUCK_STOP.into()],
                            ext: twr_ext_map(json!({
                                LOOP_STUCK_KEY: {"loop": state.id, "why": why}
                            })),
                            ..Default::default()
                        }),
                    );
                }
                Action::End { .. } => {
                    first = None;
                    closes = true;
                }
                ask => {
                    first = None;
                    if log.is_none() {
                        log = Some(self.loop_log_with(thread, &state.id, batch));
                    }
                    let log = log.as_ref().expect("just set");
                    match self.loop_prompt(hub, thread, state, &ask, log) {
                        Ok(text) => match self.push_prompt(
                            batch,
                            thread,
                            &text,
                            TurnFrom::Runtime,
                            Source::Hub,
                            None,
                        ) {
                            Ok(()) => staged.prompted = true,
                            Err(e) => {
                                tracing::info!(%thread, error = %e.message, "a loop's prompt waits: the thread takes none now")
                            }
                        },
                        Err(e) => {
                            tracing::warn!(%thread, error = %e, "cannot write a loop's prompt")
                        }
                    }
                }
            }
        }
        if closes {
            self.close_stuck(batch, thread, None);
        }
        staged
    }

    /// The prompts of loops that `events` end (`loop.ended`) still queued on their threads:
    /// a `prompt.withdrawn` (source the hub) for each, and their queue rows, which the batch
    /// removes with them ([`Core::append_batch`]). Not the row the batch dequeues itself, nor
    /// those of a thread whose queue it drops whole (`drop_queues`).
    pub(in crate::hub) fn loop_withdrawals(
        &self,
        events: &[super::super::BatchEvent],
        dequeue: Option<i64>,
        drop_queues: &[String],
    ) -> (Vec<super::super::BatchEvent>, Vec<i64>) {
        let (mut out, mut rows) = (Vec::new(), Vec::new());
        for e in events {
            let EventBody::LoopEnded(ended) = &e.body else {
                continue;
            };
            if drop_queues.contains(&e.thread) {
                continue;
            }
            let queued = match self.store.pending_list(&e.thread, 256) {
                Ok(q) => q,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "cannot read a thread's queue");
                    continue;
                }
            };
            for p in queued {
                let mine = p.source == Source::Hub
                    && prompts::parse_marker(&p.prompt).is_some_and(|(id, _)| id == ended.id);
                if !mine || Some(p.id) == dequeue || rows.contains(&p.id) {
                    continue;
                }
                rows.push(p.id);
                out.push(super::super::BatchEvent {
                    thread: e.thread.clone(),
                    turn: None,
                    source: Source::Hub,
                    body: EventBody::PromptWithdrawn(PromptWithdrawn {
                        msg_ref: p.id,
                        prompt: p.prompt,
                        ext: twr_ext_map(json!({"loop_ended": ended.id})),
                        ..Default::default()
                    }),
                    raw: None,
                });
            }
        }
        (out, rows)
    }

    /// The loop whose prompt queued prompt `prompt` of thread `thread` (queued by `source`)
    /// is, when that is not the loop the thread runs now (it ended, or another one runs): one
    /// such never starts a turn ([`Core::dispatch_next`] withdraws it).
    pub(in crate::hub) fn stale_loop_prompt(
        &self,
        thread: &str,
        source: &Source,
        prompt: &str,
    ) -> Option<String> {
        if *source != Source::Hub {
            return None;
        }
        let (id, _) = prompts::parse_marker(prompt)?;
        self.loop_state(thread)
            .is_none_or(|s| s.id != id)
            .then_some(id)
    }

    /// Closes thread `thread`'s open stuck items in `batch` (those it does not close already),
    /// answered `answer` when given (the user's), else by the hub.
    pub(in crate::hub) fn close_stuck(
        &self,
        batch: &mut Batch,
        thread: &str,
        answer: Option<&str>,
    ) {
        let open: Vec<String> = self
            .loops
            .stuck
            .iter()
            .filter(|(a, t)| *t == thread && self.attention.get(a).is_some())
            .map(|(a, _)| a.clone())
            .collect();
        for attn in open {
            let closed = batch
                .events
                .iter()
                .any(|e| matches!(&e.body, EventBody::AttentionResolved(r) if r.attn_id == attn));
            if !closed {
                batch.push(
                    thread,
                    None,
                    Source::Hub,
                    EventBody::AttentionResolved(AttentionResolved {
                        attn_id: attn,
                        by: if answer.is_some() {
                            ResolvedBy::User
                        } else {
                            ResolvedBy::ThreadStopped
                        },
                        answer: answer.map(twr_core::adapter::Answer::new),
                        ..Default::default()
                    }),
                );
            }
        }
    }

    /// The prompt of `ask` for thread `thread`'s loop at `state`: its marker line, then its
    /// template filled in (`hub::loops::prompts`). Writes the loop folder's common files.
    fn loop_prompt(
        &self,
        hub: &Inner,
        thread: &str,
        state: &LoopState,
        ask: &Action,
        log: &LoopLog,
    ) -> Result<String, String> {
        // Written to the hub's folder (the record); the prompt names where the thread reads
        // it (a remote box's copy, which the hub writes before the turn reads it).
        let record = folder_of(hub, log, &state.id);
        write_common(hub, &record, log).map_err(|e| format!("{e:#}"))?;
        let folder = shown_folder(hub, log, &state.id);
        let dir = prompts::templates_dir(hub.config_file.as_deref());
        let dir = dir.as_deref();
        let started = log.started.clone().unwrap_or_default();
        let kind = started.kind;
        let commit = prompts::commit_rule(kind, state.commit).to_string();
        let ledger_path = folder.join("ledger.md").display().to_string();
        let plan_path = folder.join("plan.md").display().to_string();
        let tag = step_tag(state).unwrap_or_else(|| "scope".into());
        let context = match kind {
            LoopKind::Review => format!(
                "The work under review is for the GOAL in {}.",
                folder.join("goal.md").display()
            ),
            LoopKind::Implement => format!(
                "The work implements the plan in {plan_path} (the task: {}).",
                match task_of(log) {
                    Some(t) if t.len() <= 300 && !t.contains('\n') => t,
                    Some(_) => format!("{}", folder.join("task.md").display()),
                    None => "as the plan states it".into(),
                }
            ),
        };
        let count = |n: usize| match n {
            1 => "1 finding".to_string(),
            n => format!("{n} findings"),
        };
        let ids = |findings: &[twr_core::loops::contract::Finding]| {
            findings
                .iter()
                .map(|f| f.id.clone())
                .collect::<Vec<_>>()
                .join(", ")
        };
        let (name, correction, mut values) = match ask {
            Action::AskGoal => (
                "goal",
                false,
                vec![
                    (
                        "scope",
                        match &started.scope {
                            Some(s) => format!("its {s} scope"),
                            None => "this thread's changes".into(),
                        },
                    ),
                    ("repos", repos_list(log)),
                ],
            ),
            Action::AskPlan => (
                "plan",
                false,
                vec![
                    ("task", task_words(log, &folder, true)),
                    ("repos", repos_list(log)),
                    ("baseline", baseline_words(log, &folder, false)),
                    ("plan_path", plan_path.clone()),
                    ("workers", self.workers_words(hub, thread, &started, "plan")),
                ],
            ),
            Action::AskPlanTriage { round, findings } => (
                "plan-triage",
                false,
                vec![
                    ("round", round.clone()),
                    ("count", count(findings.len())),
                    ("findings", prompts::wrap_findings(round, findings)),
                    ("finding_ids", ids(findings)),
                    ("ledger_path", ledger_path.clone()),
                    ("plan_path", plan_path.clone()),
                ],
            ),
            Action::AskImplement => (
                "implement",
                false,
                vec![
                    ("plan_path", plan_path.clone()),
                    (
                        "plan_note",
                        if state.plan_revision >= 2 {
                            " (revised after the plan check; the first one is plan-1.md beside it)"
                                .to_string()
                        } else {
                            String::new()
                        },
                    ),
                    (
                        "workers",
                        self.workers_words(hub, thread, &started, "implement"),
                    ),
                    ("baseline", baseline_words(log, &folder, false)),
                    ("commit_rule", commit.clone()),
                ],
            ),
            Action::AskTriage { round, findings } => (
                "triage",
                false,
                vec![
                    ("round", round.clone()),
                    ("count", count(findings.len())),
                    ("findings", prompts::wrap_findings(round, findings)),
                    ("finding_ids", ids(findings)),
                    ("ledger_path", ledger_path.clone()),
                    ("context", context.clone()),
                    ("commit_rule", commit.clone()),
                ],
            ),
            Action::AskCorrection { step, problems } => {
                let mut values = vec![
                    ("step", tag.clone()),
                    (
                        "problems",
                        problems
                            .iter()
                            .map(|p| format!("- {}", prompts::one_line(p)))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ),
                    ("block", step.block().to_string()),
                ];
                if *step == Step::Commit {
                    values.push(("commit", self.commit_words(hub, state, log, &folder)));
                }
                ("correction", true, values)
            }
            Action::AskSweep if !state.audits.is_empty() => (
                "sweep-verify",
                false,
                vec![
                    ("context", context.clone()),
                    (
                        "audits",
                        super::audit::reports_words(&record, &folder, log)
                            .map_err(|e| format!("{e:#}"))?,
                    ),
                    ("not_run", super::audit::not_run_words(log)),
                    ("ledger_path", ledger_path.clone()),
                    ("commit_rule", commit.clone()),
                    (
                        "rule_names",
                        state
                            .audits
                            .iter()
                            .map(|a| a.name.clone())
                            .collect::<Vec<_>>()
                            .join(", "),
                    ),
                ],
            ),
            Action::AskRules => {
                let rules = log.rules();
                let r = &started.settings.reviewer;
                (
                    "rules",
                    false,
                    vec![
                        (
                            "transition",
                            match &state.transition {
                                Some(t) => format!(
                                    "after {}: {}",
                                    t.round,
                                    loop_report::transition_words(t.reason)
                                ),
                                None => "it ended".into(),
                            },
                        ),
                        (
                            "rules",
                            if rules.is_empty() {
                                "none named".to_string()
                            } else {
                                rules.join(", ")
                            },
                        ),
                        ("fixed", prompts::wrap_fixed(&log.fixed())),
                        ("context", context.clone()),
                        (
                            "reviewer",
                            format!("{} · {} at {}", r.runtime, r.model, state.plan.audit_effort),
                        ),
                        (
                            "max_rules",
                            twr_core::loops::contract::MAX_AUDIT_RULES.to_string(),
                        ),
                    ],
                )
            }
            Action::AskSweep => {
                let rules = log.rules();
                (
                    "sweep",
                    false,
                    vec![
                        (
                            "transition",
                            match &state.transition {
                                Some(t) => format!(
                                    "after {}: {}",
                                    t.round,
                                    loop_report::transition_words(t.reason)
                                ),
                                None => "it ended".into(),
                            },
                        ),
                        (
                            "rules",
                            if rules.is_empty() {
                                "none named".to_string()
                            } else {
                                rules.join(", ")
                            },
                        ),
                        ("fixed", prompts::wrap_fixed(&log.fixed())),
                        ("ledger_path", ledger_path.clone()),
                        ("context", context.clone()),
                        (
                            "workers",
                            self.workers_words(hub, thread, &started, "sweep"),
                        ),
                        ("commit_rule", commit.clone()),
                    ],
                )
            }
            Action::AskReport => (
                match kind {
                    LoopKind::Review => "report",
                    LoopKind::Implement => "impl-report",
                },
                false,
                vec![
                    (
                        "outcome",
                        loop_report::outcome_words(
                            kind,
                            state.outcome.unwrap_or_default(),
                            log.answered_rounds(),
                        ),
                    ),
                    (
                        "report_path",
                        folder.join("report.md").display().to_string(),
                    ),
                    (
                        "commit",
                        if state.asks_commit() {
                            self.commit_words(hub, state, log, &folder)
                        } else {
                            String::new()
                        },
                    ),
                ],
            ),
            other => return Err(format!("{other:?} is no prompt")),
        };
        values.push(("folder", folder.display().to_string()));
        values.push(("loop_id", state.id.clone()));
        values.push(("thread", self.thread_name(thread)));
        let template = prompts::template(dir, name);
        let mut body = prompts::render(&template, &values);
        // What a template must carry, whatever it says.
        let commit_correction = matches!(
            ask,
            Action::AskCorrection {
                step: Step::Commit,
                ..
            }
        );
        for (key, needed) in [
            ("triage", "findings"),
            ("plan-triage", "findings"),
            ("correction", "problems"),
            ("sweep-verify", "audits"),
        ] {
            if name == key && !template.contains(&format!("{{{{{needed}}}}}")) {
                if let Some((_, v)) = values.iter().find(|(k, _)| *k == needed) {
                    body.push_str("\n\n");
                    body.push_str(v);
                }
            }
        }
        if commit_correction {
            body.push_str("\n\n");
            body.push_str(prompts::COMMIT_AGAIN);
        }
        // A thread on a remote box runs its gates itself: twr jobs run only on the hub's box.
        if let Some(b) = self
            .registry
            .get(thread)
            .map(|v| v.box_name.clone())
            .filter(|b| b != LOCAL_BOX)
        {
            body = prompts::without_jobs(&body, &b);
        }
        if (matches!(ask, Action::AskReport) && state.asks_commit() || commit_correction)
            && !template.contains("{{commit}}")
        {
            if let Some((_, v)) = values.iter().find(|(k, _)| *k == "commit") {
                body.push_str("\n\n");
                body.push_str(v);
            }
        }
        Ok(format!(
            "{}\n\n{}",
            prompts::marker(&state.id, &tag, correction),
            body.trim_end()
        ))
    }

    /// The commit's wording (the `commit` template) for loop `state`: what to commit, the
    /// baseline left out, the repositories the hub checks.
    fn commit_words(&self, hub: &Inner, state: &LoopState, log: &LoopLog, folder: &Path) -> String {
        let dir = prompts::templates_dir(hub.config_file.as_deref());
        let started = log.started.clone().unwrap_or_default();
        let notes = loop_report::baseline_notes(&started);
        let baseline = if notes.is_empty() {
            String::new()
        } else {
            format!(
                "Leave the changes that were already in the worktree when the loop began out of \
                 the commit ({}; see {}). ",
                notes
                    .iter()
                    .map(|(_, s)| s.clone())
                    .collect::<Vec<_>>()
                    .join("; "),
                notes
                    .iter()
                    .map(|(slug, _)| folder
                        .join(format!("baseline-{slug}.patch"))
                        .display()
                        .to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let repos = started
            .bases
            .iter()
            .map(|b| b.repo.clone())
            .collect::<Vec<_>>()
            .join(", ");
        prompts::render(
            &prompts::template(dir.as_deref(), "commit"),
            &[
                (
                    "what",
                    prompts::commit_what(state.kind, state.commit).to_string(),
                ),
                ("baseline", baseline),
                ("repos", repos),
            ],
        )
        .trim_end()
        .to_string()
    }

    /// What prompt `which` (`plan`, `implement`, `sweep`) says of the workers: their model
    /// where the implementer's runtime runs subagents (`settings::workers_apply`), else that
    /// the thread works alone.
    fn workers_words(
        &self,
        hub: &Inner,
        thread: &str,
        started: &twr_core::event::LoopStarted,
        which: &str,
    ) -> String {
        let view = self.registry.get(thread);
        let avail = super::availability(hub, &self.accounts, view);
        let w = &started.settings.workers;
        let model = match &w.effort {
            Some(e) => format!("{} at {e} effort", w.model),
            None => w.model.clone(),
        };
        let applies = twr_core::loops::settings::workers_apply(&started.settings, &avail);
        match (which, applies) {
            ("plan", true) => format!(
                "\nThe implement step delegates well-scoped work items to your own subagents on \
                 {model} (the loop's workers model): plan work items they can take.\n"
            ),
            ("implement", true) => format!(
                "- Delegate well-scoped work items to your own subagents (your Agent tool), each \
                 with a precise prompt (the plan's slice, the exact files, the constraints), and \
                 wait for each. Run them on {model}: the loop's workers model (pass it as the \
                 subagent's model). Keep architectural and cross-cutting items yourself."
            ),
            ("sweep", true) => format!(
                "Run the audit subagents on {model}, the loop's workers model (pass it as the \
                 subagent's model).\n"
            ),
            ("implement", false) => {
                "- Your runtime runs no subagents of its own: work through the items yourself, \
                 one after another."
                    .into()
            }
            _ => String::new(),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Which turn answers
    // -----------------------------------------------------------------------------------------

    /// Who turn `turn` of thread `thread` (started as `started`) is to its loop at `state`,
    /// waiting for step `tag` (module docs).
    fn turn_owner(
        &self,
        thread: &str,
        turn: u64,
        started: &TurnStarted,
        state: &LoopState,
        tag: &str,
    ) -> Option<Owner> {
        if state.held {
            return None;
        }
        let consumed = self.loops.consumed.get(thread).copied().unwrap_or(0);
        if turn <= consumed {
            return None;
        }
        let ask = self.loops.asks.get(thread).filter(|a| {
            a.loop_id == state.id && a.step == tag && a.turn > consumed && a.turn <= turn
        });
        if let Some(a) = ask {
            if a.turn == turn {
                return Some(Owner::Loop);
            }
            if started.from == TurnFrom::Runtime && !started.prompt.starts_with(LOOP_PROMPT_START) {
                return Some(Owner::Loop);
            }
        }
        (started.from != TurnFrom::Runtime).then_some(Owner::IfBlock)
    }

    /// Thread `thread`'s turn `turn`'s start, from the log.
    fn turn_start(&self, thread: &str, turn: u64) -> Option<TurnStarted> {
        match self.stored_event(thread, Some(turn), "turn.started") {
            Ok(Some(Event {
                body: EventBody::TurnStarted(p),
                ..
            })) => Some(p),
            _ => None,
        }
    }

    /// What completed turn `turn` of thread `thread`, its final message `final_text`, brings
    /// the thread's loop (module docs), if it is the loop's: `interim` when it ended with
    /// background work of its own still running. Writes `report.md` when it is the report
    /// step's. Nothing is recorded here: [`Core::loop_take`] does, in the turn's reaction batch.
    pub(in crate::hub) fn loop_answer(
        &self,
        hub: &Inner,
        thread: &str,
        turn: u64,
        final_text: Option<&str>,
        interim: bool,
    ) -> Option<LoopAnswer> {
        // (Also before the loops are driven: start-up recovery completes the reactions to a
        // turn that ended before the previous hub reacted to it, and the turn's answer is
        // taken with them, as at its end.)
        if self.shutting_down {
            return None;
        }
        let state = self.loop_state(thread)?;
        let tag = step_tag(&state)?;
        let started = self.turn_start(thread, turn)?;
        let owner = self.turn_owner(thread, turn, &started, &state, &tag)?;
        let mut mark = json!({"id": state.id, "step": tag});
        let accepted = |i: &Input| match i {
            Input::GoalWritten { .. }
            | Input::TriageAccepted { .. }
            | Input::SweepDone { .. }
            | Input::RulesWritten { .. }
            | Input::PlanWritten { .. }
            | Input::PlanTriaged { .. } => true,
            Input::Implemented { implemented } => implemented.problems().is_empty(),
            _ => false,
        };
        let commit_step = matches!(state.wait, Wait::Report | Wait::Commit);
        match owner {
            Owner::IfBlock => {
                if commit_step {
                    return None;
                }
                let input = state.reply_input(final_text?)?;
                accepted(&input).then(|| LoopAnswer {
                    mark,
                    summary: None,
                    input: Some(self.with_plan_file(hub, thread, &state, input)),
                    check: false,
                })
            }
            Owner::Loop if interim => {
                mark["interim"] = json!(true);
                Some(LoopAnswer {
                    mark,
                    summary: None,
                    input: None,
                    check: false,
                })
            }
            // The commit's check reads git: the watch runs it ([`Core::check_loops`]), and its
            // result is the step's.
            Owner::Loop if commit_step && state.asks_commit() => {
                mark["commit"] = json!("checking");
                let summary = (state.wait == Wait::Report).then(|| {
                    let log = self.loop_log(thread, &state.id);
                    format!(
                        "{}; checking its commit",
                        loop_report::outcome_words(
                            state.kind,
                            state.outcome.unwrap_or_default(),
                            log.answered_rounds()
                        )
                    )
                });
                Some(LoopAnswer {
                    mark,
                    summary,
                    input: None,
                    check: true,
                })
            }
            Owner::Loop if state.wait == Wait::Report => {
                let log = self.loop_log(thread, &state.id);
                let folder = folder_of(hub, &log, &state.id);
                let outcome = state.outcome.unwrap_or_default();
                let path = folder.join("report.md");
                let text = loop_report::report(
                    &self.thread_name(thread),
                    &log,
                    outcome,
                    &folder.display().to_string(),
                    final_text,
                    &committed_of(&state),
                );
                let written = write_common(hub, &folder, &log)
                    .and_then(|()| std::fs::write(&path, text).map_err(anyhow::Error::from));
                let input = match written {
                    Ok(()) => {
                        let shown = path.display().to_string();
                        mark["outcome"] = json!(outcome);
                        mark["report_path"] = json!(shown);
                        Input::ReportWritten { report_path: shown }
                    }
                    Err(e) => Input::Fail {
                        why: format!("cannot write {}: {e:#}", path.display()),
                    },
                };
                let summary = format!(
                    "{}; report: {}",
                    loop_report::outcome_words(state.kind, outcome, log.answered_rounds()),
                    path.display()
                );
                Some(LoopAnswer {
                    mark,
                    summary: Some(summary),
                    input: Some(input),
                    check: false,
                })
            }
            Owner::Loop => {
                let input = state.reply_input(final_text.unwrap_or(""))?;
                Some(LoopAnswer {
                    mark,
                    summary: None,
                    input: Some(self.with_plan_file(hub, thread, &state, input)),
                    check: false,
                })
            }
        }
    }

    /// `input` with the plan it brings written to the loop folder (module docs): the plan
    /// turn's to `plan.md`, the plan check's revision to `plan.md` with the first one kept as
    /// `plan-1.md`; its path filled in. A plan that can't be written fails the loop. Any
    /// other input as it is.
    fn with_plan_file(&self, hub: &Inner, thread: &str, state: &LoopState, input: Input) -> Input {
        let (text, revised) = match &input {
            Input::PlanWritten { plan, path: None } => (plan.text.clone(), false),
            Input::PlanTriaged {
                triage, path: None, ..
            } => match &triage.plan {
                Some(p) => (p.text.clone(), true),
                None => return input,
            },
            _ => return input,
        };
        let log = self.loop_log(thread, &state.id);
        let folder = folder_of(hub, &log, &state.id);
        let path = folder.join("plan.md");
        let written = (|| -> anyhow::Result<()> {
            make_folder(hub, &folder)?;
            let first = folder.join("plan-1.md");
            if revised && !first.exists() && path.exists() {
                std::fs::rename(&path, &first)?;
            }
            std::fs::write(&path, format!("{}\n", text.trim_end()))?;
            Ok(())
        })();
        let shown = Some(path.display().to_string());
        match (written, input) {
            (Err(e), _) => Input::Fail {
                why: format!("cannot write {}: {e:#}", path.display()),
            },
            (Ok(()), Input::PlanWritten { plan, .. }) => Input::PlanWritten { plan, path: shown },
            (Ok(()), Input::PlanTriaged { round, triage, .. }) => Input::PlanTriaged {
                round,
                triage,
                path: shown,
            },
            (Ok(()), other) => other,
        }
    }

    /// Takes `answer` of turn `turn` of thread `thread` into `batch` (the turn's reaction
    /// batch): its step's facts, records and what follows them.
    pub(in crate::hub) fn loop_take(
        &mut self,
        hub: &Inner,
        batch: &mut Batch,
        thread: &str,
        turn: u64,
        answer: LoopAnswer,
    ) -> Staged {
        let Some(input) = answer.input else {
            return Staged {
                check: answer.check,
                ..Default::default()
            };
        };
        match self.loop_step(hub, batch, thread, input, Some(turn)) {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(%thread, turn, error = %e, "a loop did not take a turn's answer");
                Staged::default()
            }
        }
    }

    /// After a failed turn's block kind is decided (`hub::retry`): the thread's loop takes its
    /// failed turn, if it is its step's, as `TurnFailed` (stuck), unless the hub retries it
    /// automatically (its retry turn is then the step's).
    pub(in crate::hub) fn loop_after_failure(&mut self, hub: &Inner, thread: &str) {
        if self.shutting_down || !self.loop_runs.ready {
            return;
        }
        let Some(view) = self.registry.get(thread).cloned() else {
            return;
        };
        let Some(failed) = view.failed_turn() else {
            return;
        };
        self.loop_failed_turn(hub, thread, &view, failed);
    }

    /// Failed turn `failed` of thread `thread` (`view`), if it is its loop's step's: taken as
    /// `TurnFailed` (stuck), unless the hub retries it automatically.
    fn loop_failed_turn(&mut self, hub: &Inner, thread: &str, view: &ThreadView, failed: u64) {
        // A compaction is maintenance, no step's answer.
        if self.is_compaction(thread, failed).unwrap_or(false) {
            return;
        }
        let (Some(state), Some(started)) =
            (self.loop_state(thread), self.turn_start(thread, failed))
        else {
            return;
        };
        let Some(tag) = step_tag(&state) else {
            return;
        };
        if self.turn_owner(thread, failed, &started, &state, &tag) != Some(Owner::Loop) {
            return;
        }
        let transient = crate::store::turn_reactions(self.store.conn(), thread, failed)
            .ok()
            .and_then(|d| d.block_kind)
            .and_then(|k| twr_decisions::BlockKind::parse(&k))
            == Some(twr_decisions::BlockKind::Transient);
        if transient && view.retry_eligible() {
            return;
        }
        let ended = match self.stored_event(thread, Some(failed), "turn.ended") {
            Ok(Some(Event {
                body: EventBody::TurnEnded(p),
                ..
            })) => Some(p),
            _ => None,
        };
        let detail = ended
            .as_ref()
            .and_then(|p| twr_ext(&p.ext))
            .and_then(|t| t.get("error"))
            .and_then(Value::as_str)
            .map(|e| format!(": {}", prompts::one_line(e)))
            .unwrap_or_default();
        // (A turn that ended `error` failed, whatever crashed after it; a crash cuts its turn.)
        let errored = ended.is_some_and(|p| p.result == TurnResult::Error);
        let what = if view.crashed && !errored {
            "crashed"
        } else {
            "failed"
        };
        self.loop_turn_failed(hub, thread, failed, format!("turn {failed} {what}{detail}"));
    }

    /// Turn `turn` of thread `thread` was interrupted: its loop takes it as `TurnFailed`
    /// (stuck) if it was its step's, unless the hub's own shutdown interrupted it.
    pub(in crate::hub) fn loop_turn_interrupted(&mut self, hub: &Inner, thread: &str, turn: u64) {
        if self.shutting_down || !self.loop_runs.ready {
            return;
        }
        let (Some(state), Some(started)) = (self.loop_state(thread), self.turn_start(thread, turn))
        else {
            return;
        };
        let Some(tag) = step_tag(&state) else {
            return;
        };
        if self.is_compaction(thread, turn).unwrap_or(false) {
            return;
        }
        if self.turn_owner(thread, turn, &started, &state, &tag) == Some(Owner::Loop) {
            self.loop_turn_failed(hub, thread, turn, format!("turn {turn} was interrupted"));
        }
    }

    /// Records turn `turn` of thread `thread` failed for its loop (`why`), one batch.
    fn loop_turn_failed(&mut self, hub: &Inner, thread: &str, turn: u64, why: String) {
        let mut batch = Batch::default();
        let tag = self.loop_state(thread).as_ref().and_then(step_tag);
        match self.loop_step(
            hub,
            &mut batch,
            thread,
            Input::TurnFailed { why },
            Some(turn),
        ) {
            Ok(_) => {
                hub.loop_record_point(&format!("failed {}", tag.unwrap_or_default()));
                if let Err(e) = self.append_batch(hub, batch) {
                    tracing::warn!(%thread, turn, error = %format!("{e:#}"), "cannot record a loop turn's failure");
                }
            }
            Err(e) => {
                tracing::debug!(%thread, turn, error = %e, "a loop did not take a failed turn")
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Watching
    // -----------------------------------------------------------------------------------------

    /// Whether thread `thread`'s loop at `state` has its step's prompt queued, or delivered and
    /// not answered yet (module docs).
    fn ask_present(&self, thread: &str, state: &LoopState, tag: &str) -> bool {
        let marker = prompts::marker(&state.id, tag, false);
        let marker = marker.trim_end_matches(']');
        let queued = self
            .store
            .pending_list(thread, 256)
            .map(|q| q.iter().any(|p| p.prompt.starts_with(marker)))
            .unwrap_or(true);
        if queued {
            return true;
        }
        let consumed = self.loops.consumed.get(thread).copied().unwrap_or(0);
        let Some(ask) = self
            .loops
            .asks
            .get(thread)
            .filter(|a| a.loop_id == state.id && a.step == tag && a.turn > consumed)
        else {
            return false;
        };
        let Some(view) = self.registry.get(thread) else {
            return true;
        };
        if view.current_turn.is_some() {
            return true;
        }
        // Delivered: answered by the turn's end, unless that end was an interruption the hub
        // took no step from (its own shutdown's). A turn of the user's that the shutdown
        // interrupted after the step's own turn answered it (a message sent as the report turn
        // ended, its commit's check still to run) does not ask for the step again.
        let interrupted = view
            .last_turn
            .as_ref()
            .is_some_and(|t| t.turn >= ask.turn && t.result == TurnResult::Interrupted);
        !interrupted
            || self
                .step_answer(thread, state, tag)
                .is_some_and(|t| t >= ask.turn)
    }

    /// The review and audit tasks thread `thread` shows that no run of this hub's is (a
    /// previous hub's).
    fn orphan_review_tasks(&self, thread: &str) -> Vec<String> {
        let mut mine: Vec<String> = self
            .loop_runs
            .runs
            .get(thread)
            .map(|r| format!("{LOOP_TASK_PREFIX}{}", r.loop_id))
            .into_iter()
            .collect();
        mine.extend(
            self.loop_runs
                .audits
                .get(thread)
                .into_iter()
                .flatten()
                .map(super::audit::AuditRun::task),
        );
        self.registry
            .get(thread)
            .map(|v| {
                v.background_tasks
                    .iter()
                    .filter(|(id, t)| t.review && !mine.contains(id))
                    .map(|(id, _)| id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Watches every running loop (module docs); `startup`: once after the hub started.
    pub(in crate::hub) fn check_loops(&mut self, hub: &Arc<Inner>, startup: bool) {
        if self.shutting_down || !self.loop_runs.ready {
            return;
        }
        let threads: Vec<String> = self
            .registry
            .list()
            .into_iter()
            .filter(|v| v.r#loop.as_ref().is_some_and(|l| !l.ended))
            .map(|v| v.id.clone())
            .collect();
        for thread in threads {
            self.check_loop(hub, &thread, startup);
        }
        // Review tasks of loops that are no more (a previous hub's, ended under it).
        let stale: Vec<(String, String)> = self
            .registry
            .list()
            .into_iter()
            .filter(|v| v.r#loop.is_none())
            .flat_map(|v| {
                self.orphan_review_tasks(&v.id)
                    .into_iter()
                    .map(|t| (v.id.clone(), t))
                    .collect::<Vec<_>>()
            })
            .collect();
        for (thread, task) in stale {
            let mut batch = Batch::default();
            batch.push(
                &thread,
                None,
                Source::Hub,
                task_end(&task, "the loop ended"),
            );
            if let Err(e) = self.append_batch(hub, batch) {
                tracing::warn!(%thread, error = %format!("{e:#}"), "cannot close a review task");
            }
        }
    }

    /// [`Core::check_loops`] of thread `thread`.
    fn check_loop(&mut self, hub: &Arc<Inner>, thread: &str, startup: bool) {
        let Some(state) = self.loop_state(thread) else {
            return;
        };
        // On a remote box: its copy of the folder written again on each new connection of the
        // box, and a loop that waited on the box marked back once it is.
        self.sync_box_folder(hub, thread, false);
        if state.box_away.is_some() {
            self.box_gate(hub, thread);
        }
        let mut batch = Batch::default();
        let mut staged = Staged::default();
        {
            for task in self.orphan_review_tasks(thread) {
                batch.push(
                    thread,
                    None,
                    Source::Hub,
                    task_end(&task, "the hub stopped while it ran"),
                );
            }
        }
        match &state.wait {
            Wait::Review { round } => {
                if self.loop_runs.runs.contains_key(thread) || state.held {
                    // (Running, or paused before it ran.)
                } else if state.review_started_at.is_some() {
                    // In flight when its hub stopped (or lost): a failed run.
                    match self.loop_step(
                        hub,
                        &mut batch,
                        thread,
                        Input::ReviewFailed {
                            round: round.clone(),
                            reason: "the hub stopped while it ran".into(),
                        },
                        None,
                    ) {
                        Ok(s) => staged = staged.and(s),
                        Err(e) => tracing::warn!(%thread, error = %e, "a lost review run"),
                    }
                } else {
                    staged.review = true;
                }
            }
            // (After this batch: `check_audits`.)
            Wait::Audits => {}
            Wait::Ended => {}
            Wait::WriteReport => staged.write_report = true,
            _ => {
                let tag = step_tag(&state).unwrap_or_default();
                if !state.held {
                    if startup {
                        if let Some(s) = self.recover_turn(hub, &mut batch, thread, &state, &tag) {
                            staged = staged.and(s);
                        }
                    }
                    // (A step taken just now asks for its next one itself.)
                    let still = self
                        .loop_state(thread)
                        .is_some_and(|s| s.wait == state.wait && !s.held)
                        && batch
                            .events
                            .iter()
                            .all(|e| !e.body.kind().starts_with("loop."));
                    if still && !self.ask_present(thread, &state, &tag) {
                        if let Some(ask) = state.ask() {
                            staged = staged.and(self.stage(
                                hub,
                                &mut batch,
                                thread,
                                &state,
                                vec![ask],
                                None,
                                None,
                                None,
                            ));
                        }
                    }
                    // A commit step's answer: its check.
                    if still && state.asks_commit() && !self.loop_runs.checks.contains_key(thread) {
                        if let Some(turn) = self.step_answer(thread, &state, &tag) {
                            self.start_commit_check(hub, thread, &state.id, turn);
                        }
                    }
                }
            }
        }
        if !batch.events.is_empty() || !batch.prompts.is_empty() {
            if let Err(e) = self.append_batch(hub, batch) {
                tracing::warn!(%thread, error = %format!("{e:#}"), "cannot record a loop's step");
                return;
            }
        }
        if state.wait == Wait::Audits {
            staged = staged.and(self.check_audits(hub, thread));
        }
        self.after_loop_batch(hub, thread, staged);
    }

    /// At startup: the turn of thread `thread`'s step that ended while no hub reacted to it,
    /// taken as at its end: a completed one as the step's answer, a failed one (it ended
    /// `error`, or its runtime crashed on its own) as `TurnFailed` (stuck, unless the hub
    /// retries it). One the hub's stop interrupted, or whose runtime died with the previous
    /// hub, is asked again ([`Core::check_loop`] does, finding no ask).
    fn recover_turn(
        &mut self,
        hub: &Inner,
        batch: &mut Batch,
        thread: &str,
        state: &LoopState,
        tag: &str,
    ) -> Option<Staged> {
        let view = self.registry.get(thread)?.clone();
        if view.current_turn.is_some() {
            return None;
        }
        let last = view.last_turn.clone()?;
        match last.result {
            TurnResult::Completed => {}
            TurnResult::Error => {
                self.loop_failed_turn(hub, thread, &view, last.turn);
                return None;
            }
            TurnResult::Interrupted => {
                if view.crashed
                    && view.crashed_turn == Some(last.turn)
                    && !self.loop_runs.died_with_hub.contains(thread)
                {
                    self.loop_failed_turn(hub, thread, &view, last.turn);
                }
                return None;
            }
        }
        let started = self.turn_start(thread, last.turn)?;
        self.turn_owner(thread, last.turn, &started, state, tag)?;
        let final_text = self.logged_final_text(thread, last.turn).ok().flatten();
        let report = self.logged_report(thread, last.turn).ok().flatten();
        let interim = report
            .as_ref()
            .is_some_and(super::super::reactions::is_interim);
        let answer = self.loop_answer(hub, thread, last.turn, final_text.as_deref(), interim)?;
        Some(self.loop_take(hub, batch, thread, last.turn, answer))
    }

    /// What follows a committed batch of thread `thread`'s loop: its queue dispatched when a
    /// prompt was queued, a review run started when one is due.
    pub(in crate::hub) fn after_loop_batch(
        &mut self,
        hub: &Arc<Inner>,
        thread: &str,
        staged: Staged,
    ) {
        // A remote box's copy of the loop folder gets what the step reads, ahead of its prompt.
        self.sync_box_folder(hub, thread, staged.prompted);
        if staged.prompted {
            if let Err(e) = self.try_dispatch(hub, thread) {
                tracing::warn!(%thread, error = %format!("{e:#}"), "dispatching a loop's prompt failed");
            }
        }
        if staged.review {
            self.start_review(hub, thread);
        }
        if staged.audits {
            self.start_audits(hub, thread);
        }
        if staged.check {
            hub.loop_kick.notify_one();
        }
        if staged.write_report {
            self.write_report(hub, thread);
        }
    }

    /// The turn that answered thread `thread`'s loop's step `tag` and was not taken yet (a
    /// commit step's, `report` or the correction's `commit`, waits for its check): the latest
    /// completed turn since the loop last took one that is the step's own
    /// ([`Core::turn_owner`]) and did not end with background work of its own still running (an
    /// interim end; its follow-up answers).
    fn step_answer(&self, thread: &str, state: &LoopState, tag: &str) -> Option<u64> {
        let view = self.registry.get(thread)?;
        let last = view.last_turn.as_ref()?.turn;
        let consumed = self.loops.consumed.get(thread).copied().unwrap_or(0);
        let mut turn = last;
        while turn > consumed && last - turn < 32 {
            let ended = matches!(
                self.stored_event(thread, Some(turn), "turn.ended"),
                Ok(Some(Event {
                    body: EventBody::TurnEnded(TurnEnded {
                        result: TurnResult::Completed,
                        ..
                    }),
                    ..
                }))
            );
            if ended {
                if let Some(started) = self.turn_start(thread, turn) {
                    if self.turn_owner(thread, turn, &started, state, tag) == Some(Owner::Loop) {
                        let interim = self
                            .logged_report(thread, turn)
                            .ok()
                            .flatten()
                            .as_ref()
                            .is_some_and(super::super::reactions::is_interim);
                        return (!interim).then_some(turn);
                    }
                }
            }
            turn -= 1;
        }
        None
    }

    /// Starts the check of the commit turn `turn` of thread `thread`'s loop `loop_id` answered.
    fn start_commit_check(&mut self, hub: &Arc<Inner>, thread: &str, loop_id: &str, turn: u64) {
        if self.shutting_down || !self.loop_runs.ready {
            return;
        }
        // The check reads the box's git: it waits while the box is away.
        if self.box_gate(hub, thread) {
            return;
        }
        self.loop_runs
            .checks
            .insert(thread.to_string(), (loop_id.to_string(), turn));
        tokio::spawn(run_commit_check(
            Arc::clone(hub),
            thread.to_string(),
            loop_id.to_string(),
            turn,
        ));
    }

    /// What the commit check of turn `turn` of thread `thread`'s loop `loop_id` reads: per
    /// repository, its path, `HEAD` when the loop began, the baseline's paths and whether the
    /// loop left work there; and the turn's reply. `None`: the loop moved on.
    fn commit_prep(
        &self,
        hub: &Inner,
        thread: &str,
        loop_id: &str,
        turn: u64,
    ) -> Option<(Vec<RepoCheck>, String)> {
        let state = self
            .loop_state(thread)
            .filter(|s| s.id == loop_id && s.asks_commit())
            .filter(|s| matches!(s.wait, Wait::Report | Wait::Commit))?;
        let log = self.loop_log(thread, loop_id);
        let folder = folder_of(hub, &log, loop_id);
        let started = log.started.clone()?;
        let heads = loop_start(&started, "heads");
        let fixed = !log.fixed().is_empty()
            || log.sweep.as_ref().is_some_and(|rules| {
                rules
                    .iter()
                    .any(|r| r.result == twr_core::loops::contract::SweepResult::Fixed)
            });
        let repos = started
            .bases
            .iter()
            .map(|b| RepoCheck {
                repo: PathBuf::from(&b.repo),
                from: match state.kind {
                    LoopKind::Implement => Some(b.base.clone()),
                    LoopKind::Review => heads
                        .get(&b.slug)
                        .and_then(Value::as_str)
                        .map(str::to_string),
                },
                baseline: baseline_paths(&folder, &b.slug),
                work: match state.kind {
                    LoopKind::Review => fixed,
                    LoopKind::Implement => latest_diff_has_changes(&folder, &log, &b.slug),
                },
            })
            .collect();
        let text = self
            .logged_final_text(thread, turn)
            .ok()
            .flatten()
            .unwrap_or_default();
        Some((repos, text))
    }

    /// Steps the commit check of turn `turn` of thread `thread`'s loop `loop_id` (`input`), if
    /// the loop still waits for it, with what follows: one batch.
    fn commit_checked(
        &mut self,
        hub: &Arc<Inner>,
        thread: &str,
        loop_id: &str,
        turn: u64,
        input: Option<Input>,
    ) {
        if self
            .loop_runs
            .checks
            .get(thread)
            .is_some_and(|(l, t)| l == loop_id && *t == turn)
        {
            self.loop_runs.checks.remove(thread);
        }
        let Some(input) = input else {
            return;
        };
        if self.shutting_down || self.loop_state(thread).is_none_or(|s| s.id != loop_id) {
            return;
        }
        let tag = self.loop_state(thread).as_ref().and_then(step_tag);
        let mut batch = Batch::default();
        match self.loop_step(hub, &mut batch, thread, input, Some(turn)) {
            Ok(staged) => {
                hub.loop_record_point(&format!("checked {}", tag.unwrap_or_default()));
                if let Err(e) = self.append_batch(hub, batch) {
                    tracing::warn!(%thread, error = %format!("{e:#}"), "cannot record a commit's check; the next start checks it again");
                    return;
                }
                self.after_loop_batch(hub, thread, staged);
            }
            Err(e) => {
                tracing::debug!(%thread, turn, error = %e, "a loop did not take a commit's check")
            }
        }
    }

    /// Writes thread `thread`'s loop's `report.md` once its commit is settled
    /// ([`Action::WriteReport`]): its report turn's reply without the `twr-commit` block,
    /// joined with what the hub counted and the commits; then `loop.ended` and the loop's
    /// last report (`report.ready` of the report turn, marked `ext.twr.loop` with the outcome,
    /// the path and the commits), one batch.
    fn write_report(&mut self, hub: &Arc<Inner>, thread: &str) {
        let Some(state) = self
            .loop_state(thread)
            .filter(|s| s.wait == Wait::WriteReport)
        else {
            return;
        };
        let log = self.loop_log(thread, &state.id);
        let folder = folder_of(hub, &log, &state.id);
        let turn = log.report_turn();
        let reply = turn
            .and_then(|t| self.logged_final_text(thread, t).ok().flatten())
            .map(|t| {
                twr_core::loops::contract::without_blocks(
                    &t,
                    &[twr_core::loops::contract::COMMIT_BLOCK],
                )
            });
        let committed = committed_of(&state);
        let outcome = state.outcome.unwrap_or_default();
        let path = folder.join("report.md");
        let text = loop_report::report(
            &self.thread_name(thread),
            &log,
            outcome,
            &folder.display().to_string(),
            reply.as_deref(),
            &committed,
        );
        let written = write_common(hub, &folder, &log)
            .and_then(|()| std::fs::write(&path, text).map_err(anyhow::Error::from));
        let shown = path.display().to_string();
        let input = match written {
            Ok(()) => Input::ReportWritten {
                report_path: shown.clone(),
            },
            Err(e) => Input::Fail {
                why: format!("cannot write {}: {e:#}", path.display()),
            },
        };
        let wrote = matches!(input, Input::ReportWritten { .. });
        let mut batch = Batch::default();
        let staged = match self.loop_step(hub, &mut batch, thread, input, turn) {
            Ok(s) => s,
            Err(e) => {
                tracing::debug!(%thread, error = %e, "a loop did not take its report");
                return;
            }
        };
        if let (true, Some(turn)) = (wrote, turn) {
            let grade = self
                .logged_report(thread, turn)
                .ok()
                .flatten()
                .map(|r| r.grade)
                .unwrap_or(Grade::Success);
            let mut summary =
                loop_report::outcome_words(state.kind, outcome, log.answered_rounds());
            if let Some(c) = committed.words() {
                summary.push_str("; ");
                summary.push_str(&c);
            }
            summary.push_str(&format!("; report: {shown}"));
            batch.push(
                thread,
                Some(turn),
                Source::Hub,
                EventBody::ReportReady(ReportReady {
                    summary,
                    grade,
                    turn: Some(turn),
                    written_by: "hub".into(),
                    ext: twr_ext_map(json!({ LOOP_REPORT_KEY: {
                        "id": state.id,
                        "step": "report",
                        "outcome": outcome,
                        "report_path": shown,
                        "commits": committed.commits,
                        "commit_problem": committed.problem,
                    }})),
                    ..Default::default()
                }),
            );
        }
        hub.loop_record_point("report written");
        if let Err(e) = self.append_batch(hub, batch) {
            tracing::warn!(%thread, error = %format!("{e:#}"), "cannot record a loop's report");
            return;
        }
        self.after_loop_batch(hub, thread, staged);
    }

    /// Whether thread `thread`'s remote box is away now (never for the hub's own box). A loop
    /// that needs it for a run of the hub's waits for it: `loop.box` marks it waiting the
    /// first time (its loop line says `waiting on box <name>`), and marks it back when the box
    /// is.
    pub(super) fn box_gate(&mut self, hub: &Arc<Inner>, thread: &str) -> bool {
        let Some(box_name) = self.registry.get(thread).map(|v| v.box_name.clone()) else {
            return false;
        };
        let away = Place::away(hub, &box_name);
        let Some(state) = self.loop_state(thread) else {
            return away;
        };
        if away != state.box_away.is_some() {
            let body = EventBody::LoopBox(LoopBoxAway {
                id: state.id.clone(),
                box_name: box_name.clone(),
                away,
                ..Default::default()
            });
            if let Err(e) = self.append(hub, thread, None, Source::Hub, body) {
                tracing::warn!(%thread, error = %format!("{e:#}"), "cannot record that a loop waits on its box");
            }
        }
        away
    }

    /// Writes thread `thread`'s loop's shared files ([`shared_files`]) into its remote box's
    /// copy of the folder, when the thread runs on one that is connected: `now` (a step was
    /// just asked of the thread), or when the box's connection is another than the one they
    /// were last written on (the box back, the hub restarted). Issued at once, ahead of the
    /// step's prompt on the box's connection: each small file with its data; a large one (a
    /// baseline patch) only when the box lacks it, which a box that lost its folder does,
    /// right after. The previous loop of the thread has its folder on the box removed when a
    /// new one first writes there. The thread's later turns may still read its last loop's
    /// files: they stay until then, or until the thread is deleted.
    pub(super) fn sync_box_folder(&mut self, hub: &Arc<Inner>, thread: &str, now: bool) {
        let Some(view) = self.registry.get(thread) else {
            return;
        };
        if view.box_name == LOCAL_BOX {
            return;
        }
        let Some(loop_id) = view
            .r#loop
            .as_ref()
            .map(|l| l.id.clone())
            .or_else(|| view.last_loop.as_ref().map(|l| l.id.clone()))
        else {
            return;
        };
        let box_name = view.box_name.clone();
        let Ok(place) = Place::of(hub, &box_name) else {
            return;
        };
        let Place::Remote(remote) = &place else {
            return;
        };
        let Some(epoch) = remote.rbox.epoch() else {
            return;
        };
        let last = self.loop_runs.synced.get(thread).cloned();
        if !now && last.as_ref() == Some(&(loop_id.clone(), epoch)) {
            return;
        }
        let log = self.loop_log(thread, &loop_id);
        let record = folder_of(hub, &log, &loop_id);
        let digests = log
            .started
            .as_ref()
            .map(|s| s.baseline_digests.clone())
            .unwrap_or_default();
        let mut files = Vec::new();
        let mut big = Vec::new();
        for name in shared_files(&record) {
            // A baseline patch's digest is the log's: it is not read here.
            let known = name
                .strip_prefix("baseline-")
                .and_then(|n| n.strip_suffix(".patch"))
                .and_then(|slug| digests.get(slug));
            if let Some(digest) = known {
                files.push(twr_core::runner::LoopFile {
                    name: name.clone(),
                    digest: digest.clone(),
                    data: None,
                });
                big.push(name);
                continue;
            }
            let Ok(data) = std::fs::read(record.join(&name)) else {
                continue;
            };
            files.push(twr_core::runner::LoopFile {
                digest: twr_core::blob::blob_ref(&data),
                data: Some(twr_core::b64::encode(&data)),
                name,
            });
        }
        let op = twr_core::runner::Op::LoopFiles {
            loop_id: loop_id.clone(),
            files,
            read: Vec::new(),
            max: 0,
        };
        let Ok(asked) = remote.rbox.issue(op) else {
            return;
        };
        self.loop_runs
            .synced
            .insert(thread.to_string(), (loop_id.clone(), epoch));
        let old = last.map(|(l, _)| l).filter(|l| *l != loop_id);
        tokio::spawn(async move {
            match asked.await {
                Ok(v) => {
                    let missing: Vec<String> = v["missing"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|n| n.as_str().map(str::to_string))
                        .filter(|n| big.contains(n))
                        .collect();
                    if !missing.is_empty() {
                        if let Err(t) = place.sync(&loop_id, &record, &missing).await {
                            tracing::warn!(box_name = %box_name, loop_id = %loop_id, error = %t.message(), "cannot write a loop's folder on its box");
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(box_name = %box_name, loop_id = %loop_id, error = %e, "cannot write a loop's folder on its box")
                }
            }
            if let Some(old) = old {
                place.forget(&old).await;
            }
        });
    }

    /// Removes thread `thread`'s loop's folder (its running loop's, else its last one's) from
    /// its remote box, when the box is connected (the thread is being deleted). Best effort: a
    /// box that is away keeps it until its runner prunes it (`twr_runner::loops::folder`).
    pub(in crate::hub) fn forget_box_loop(&mut self, hub: &Inner, thread: &str) {
        let Some(view) = self.registry.get(thread) else {
            return;
        };
        let Some(loop_id) = view
            .r#loop
            .as_ref()
            .map(|l| l.id.clone())
            .or_else(|| view.last_loop.as_ref().map(|l| l.id.clone()))
        else {
            return;
        };
        if let Ok(Place::Remote(r)) = Place::of(hub, &view.box_name) {
            if let Ok(asked) = r.rbox.issue(twr_core::runner::Op::LoopForget { loop_id }) {
                tokio::spawn(asked);
            }
        }
        self.loop_runs.synced.remove(thread);
    }

    /// Starts thread `thread`'s loop's review run, unless one runs.
    fn start_review(&mut self, hub: &Arc<Inner>, thread: &str) {
        if self.shutting_down || !self.loop_runs.ready || self.loop_runs.runs.contains_key(thread) {
            return;
        }
        let Some(state) = self.loop_state(thread) else {
            return;
        };
        let Wait::Review { round } = &state.wait else {
            return;
        };
        if state.held {
            return;
        }
        // Not while the thread's box is away: the run (a retry's too) waits for it.
        if self.box_gate(hub, thread) {
            return;
        }
        let (tx, rx) = tokio::sync::watch::channel(None);
        let run = new_id();
        self.loop_runs.runs.insert(
            thread.to_string(),
            LoopRun {
                run: run.clone(),
                loop_id: state.id.clone(),
                model: state.reviewer.model.clone(),
                effort: state.reviewer.effort.clone().unwrap_or_default(),
                began: Instant::now(),
                cancel: tx,
            },
        );
        tokio::spawn(run_review(
            Arc::clone(hub),
            thread.to_string(),
            run,
            state.id.clone(),
            round.clone(),
            rx,
        ));
    }

    /// What review task `task` (`loop:<id>`) does now, when this hub runs it: `reviewer
    /// gpt-6.1-sol xhigh · 6m`.
    pub(in crate::hub) fn review_description(&self, task: &str) -> Option<String> {
        if let Some(d) = self.audit_description(task) {
            return Some(d);
        }
        let id = task.strip_prefix(LOOP_TASK_PREFIX)?;
        let r = self.loop_runs.runs.values().find(|r| r.loop_id == id)?;
        let mins = r.began.elapsed().as_secs() / 60;
        Some(format!("reviewer {} {} · {mins}m", r.model, r.effort))
    }

    /// The process groups of review runs a stopped hub ran, as their tasks recorded them: what
    /// is left of them to kill before anything else runs.
    pub(in crate::hub) fn lost_review_groups(&self) -> Vec<(String, u32, u64)> {
        self.registry
            .list()
            .into_iter()
            .flat_map(|v| {
                v.background_tasks
                    .values()
                    .filter(|t| t.review)
                    .filter_map(|t| t.group.as_ref())
                    .map(|g| (v.id.clone(), g.pgid, g.start_time))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// Loops are driven from now on (after the hub's start-up recovery): every running loop is
    /// looked at once as after a restart.
    pub(in crate::hub) fn start_loops(&mut self, hub: &Arc<Inner>) {
        self.loop_runs.ready = true;
        self.check_loops(hub, true);
    }
}

/// The end of review task `task`, saying `what`.
fn task_end(task: &str, what: &str) -> EventBody {
    EventBody::TaskUpdated(TaskUpdated {
        task_id: task.to_string(),
        running: false,
        description: format!("review run: {what}"),
        ..Default::default()
    })
}

/// What a review (or audit) run needs, read under the hub's lock.
pub(super) struct Prep {
    pub(super) name: String,
    pub(super) template: ReviewTemplate,
    pub(super) log: LoopLog,
    /// The hub's loop folder: the record.
    pub(super) folder: PathBuf,
    /// Where the reviewer reads the folder (a remote box's copy, else [`Prep::folder`]).
    pub(super) shown: PathBuf,
    /// The loop.
    pub(super) loop_id: String,
    /// The thread's box.
    pub(super) place: Place,
    pub(super) call: review::Call,
    pub(super) task: String,
    pub(super) description: String,
}

/// Why a review (or audit) run does not run.
pub(super) enum NoRun {
    /// The loop moved on (or ended): nothing to record.
    Gone,
    /// A failed run.
    Failed(String),
    /// The hub can't go on.
    Fail(String),
}

impl Core {
    /// What thread `thread`'s review run of round `round` of loop `id` needs.
    fn review_prep(&self, hub: &Inner, thread: &str, id: &str, round: &str) -> Result<Prep, NoRun> {
        let state = self
            .loop_state(thread)
            .filter(|s| {
                s.id == id
                    && s.wait
                        == Wait::Review {
                            round: round.into(),
                        }
            })
            .ok_or(NoRun::Gone)?;
        let effort = state.reviewer.effort.clone().unwrap_or_default();
        self.reviewer_prep(
            hub,
            thread,
            &state,
            effort,
            &format!("round-{round}"),
            format!("{LOOP_TASK_PREFIX}{id}"),
            format!("{thread}:loop-{id}"),
        )
    }

    /// What a reviewer's run (a review run, or an audit run) of thread `thread`'s loop at
    /// `state` needs: the call at `effort`, its files named `<stem>.answer.json` and
    /// `<stem>.log`, its task `task` and its `TWR_AGENT` marker `marker`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn reviewer_prep(
        &self,
        hub: &Inner,
        thread: &str,
        state: &LoopState,
        effort: String,
        stem: &str,
        task: String,
        marker: String,
    ) -> Result<Prep, NoRun> {
        let id = state.id.as_str();
        let view = self.registry.get(thread).ok_or(NoRun::Gone)?;
        let log = self.loop_log(thread, id);
        let started = log
            .started
            .clone()
            .ok_or_else(|| NoRun::Fail("the loop's start is not in the log".into()))?;
        let folder = folder_of(hub, &log, id);
        // The thread's box: away, a failed run (retried once it is back); one whose runner
        // can't run loops any more, the loop's end.
        let place = Place::of(hub, &view.box_name).map_err(|e| {
            if e.code == codes::BOX_OFFLINE {
                NoRun::Failed(place::disconnected(&view.box_name))
            } else {
                NoRun::Fail(e.message)
            }
        })?;
        let shown = place.folder(&folder, id);
        let cwd = view
            .worktree
            .clone()
            .map(PathBuf::from)
            .or_else(|| started.bases.first().map(|b| PathBuf::from(&b.repo)))
            .ok_or_else(|| NoRun::Fail(format!("{} has no working directory", view.name)))?;
        // (On a remote box, the box says so when the run starts.)
        if !place.is_remote() && !cwd.is_dir() {
            return Err(NoRun::Fail(format!(
                "{}'s working directory {} is gone",
                view.name,
                cwd.display()
            )));
        }
        let reviewer = &started.settings.reviewer;
        let account_env = match &reviewer.account {
            None => None,
            // A thread on a remote box runs on the box's own login, and so does its reviewer.
            Some(name) if place.is_remote() => {
                return Err(NoRun::Failed(format!(
                    "the reviewer's account {}:{name} can't be used on box {}: a remote box's \
                     reviewer runs on the box's own login",
                    reviewer.runtime,
                    place.name()
                )))
            }
            Some(name) => {
                let who = format!("{}:{name}", reviewer.runtime);
                let info = self
                    .accounts
                    .info(&reviewer.runtime, Some(name))
                    .filter(|a| !a.native)
                    .ok_or_else(|| {
                        NoRun::Failed(format!("the reviewer's account {who} is not declared"))
                    })?;
                if info.removed {
                    return Err(NoRun::Failed(format!(
                        "the reviewer's account {who} was removed"
                    )));
                }
                if let Some(why) = self.account_conflict(&reviewer.runtime, name) {
                    return Err(NoRun::Failed(format!(
                        "the reviewer's account {who} can't be used: {why}"
                    )));
                }
                let dir = info.dir.filter(|d| d.is_dir()).ok_or_else(|| {
                    NoRun::Failed(format!("the reviewer's account {who} has no directory"))
                })?;
                Some(
                    twr_adapters::account::account_env(&reviewer.runtime, &dir).ok_or_else(
                        || NoRun::Failed(format!("{} takes no named accounts", reviewer.runtime)),
                    )?,
                )
            }
        };
        // The run's files are where it runs: on a remote box, the box's copy.
        let file = |name: String| shown.join(name);
        Ok(Prep {
            name: view.name.clone(),
            template: state.review_template(),
            folder: folder.clone(),
            shown: shown.clone(),
            loop_id: id.to_string(),
            place: place.clone(),
            task,
            description: format!("reviewer {} {effort}", reviewer.model),
            call: review::Call {
                runtime: reviewer.runtime.clone(),
                bin: hub.reviewer_bin(&reviewer.runtime),
                model: reviewer.model.clone(),
                effort,
                cwd,
                folder: shown.clone(),
                prompt: String::new(),
                schema_file: file("findings.schema.json".into()),
                answer_file: file(format!("{stem}.answer.json")),
                log_file: file(format!("{stem}.log")),
                account_env,
                marker,
                timeout: hub.config.loops.round_timeout(),
                acp: hub.acp_reviewer(&reviewer.runtime),
                path: None,
            },
            log,
        })
    }

    /// Records the start of the run `run` of round `round` (diffs `diffs`, prompt digest
    /// `digest`), its task with it, if the loop still waits for it.
    #[allow(clippy::too_many_arguments)]
    fn review_begin(
        &mut self,
        hub: &Inner,
        thread: &str,
        run: &str,
        round: &str,
        prep: &Prep,
        diffs: Vec<String>,
        digest: String,
    ) -> bool {
        if self.shutting_down || self.loop_runs.runs.get(thread).is_none_or(|r| r.run != run) {
            return false;
        }
        let mut batch = Batch::default();
        let staged = self.loop_step(
            hub,
            &mut batch,
            thread,
            Input::ReviewStarted {
                round: round.into(),
                diffs,
                prompt_digest: Some(digest),
            },
            None,
        );
        if staged.is_err() {
            return false;
        }
        batch.push(
            thread,
            None,
            Source::Hub,
            EventBody::TaskUpdated(TaskUpdated {
                task_id: prep.task.clone(),
                running: true,
                description: prep.description.clone(),
                ext: twr_ext_map(json!({"loop_review": {
                    "loop": prep.log.started.as_ref().map(|s| s.id.clone()),
                    "round": round,
                    "runtime": prep.call.runtime,
                    "model": prep.call.model,
                    "effort": prep.call.effort,
                    "log": prep.call.log_file,
                }})),
                ..Default::default()
            }),
        );
        match self.append_batch(hub, batch) {
            Ok(_) => true,
            Err(e) => {
                tracing::warn!(%thread, error = %format!("{e:#}"), "cannot record a review run's start");
                false
            }
        }
    }

    /// Records how run `run` of round `round` ended (`outcome`): its task's end, and, if the
    /// loop still waits for it, its answer or failure with what follows, one batch.
    #[allow(clippy::too_many_arguments)]
    fn review_end(
        &mut self,
        hub: &Arc<Inner>,
        thread: &str,
        run: &str,
        round: &str,
        prep: Option<&Prep>,
        outcome: Result<review::Ended, NoRun>,
        unchanged: bool,
        took: Duration,
    ) {
        if self
            .loop_runs
            .runs
            .get(thread)
            .is_some_and(|r| r.run == run)
        {
            self.loop_runs.runs.remove(thread);
        }
        // The hub's shutdown: nothing recorded, the next hub retries the round.
        if self.shutting_down || self.registry.get(thread).is_none() {
            return;
        }
        let mut batch = Batch::default();
        let mins = took.as_secs() / 60;
        let task_running = prep.is_some_and(|p| {
            self.registry
                .get(thread)
                .is_some_and(|v| v.background_tasks.contains_key(&p.task))
        });
        let input = match outcome {
            Err(NoRun::Gone) => None,
            Err(NoRun::Fail(why)) => Some(Input::Fail { why }),
            Err(NoRun::Failed(reason)) => Some(Input::ReviewFailed {
                round: round.into(),
                reason,
            }),
            Ok(review::Ended::Cancelled(_)) => None,
            Ok(review::Ended::Failed(reason)) => Some(Input::ReviewFailed {
                round: round.into(),
                reason,
            }),
            Ok(review::Ended::Answered { findings, usage }) => Some(Input::ReviewAnswered {
                round: round.into(),
                answer_path: prep.map(|p| p.call.answer_file.display().to_string()),
                findings,
                usage,
                unchanged,
            }),
        };
        if let (true, Some(p)) = (task_running, prep) {
            let what = match &input {
                Some(Input::ReviewAnswered { findings, .. }) => format!(
                    "{} finding{} after {mins}m",
                    findings.len(),
                    if findings.len() == 1 { "" } else { "s" }
                ),
                Some(Input::ReviewFailed { reason, .. }) => format!("failed: {reason}"),
                _ => "ended".into(),
            };
            batch.push(
                thread,
                None,
                Source::Hub,
                EventBody::TaskUpdated(TaskUpdated {
                    task_id: p.task.clone(),
                    running: false,
                    description: format!("{}: {what}", p.description),
                    ..Default::default()
                }),
            );
        }
        let mut staged = Staged::default();
        let stepped = input.is_some();
        if let Some(input) = input {
            let reviewer = prep.map(|p| p.call.runtime.clone());
            let mut state = self.loop_state(thread);
            let actions = state.as_mut().map(|s| s.step(input, Utc::now()));
            match (state, actions) {
                (Some(state), Some(Ok(mut actions))) => {
                    // What the run spent is the reviewer runtime's (`usage.threads` by runtime).
                    if let (Some(Action::Record(EventBody::LoopFindings(f))), Some(rt)) =
                        (actions.first_mut(), reviewer)
                    {
                        f.ext = twr_ext_map(json!({ "reviewer": rt }));
                    }
                    staged = self.stage(hub, &mut batch, thread, &state, actions, None, None, None);
                }
                (_, Some(Err(e))) => {
                    tracing::debug!(%thread, round, error = %e, "a loop did not take a review run's end")
                }
                _ => {}
            }
        }
        if !batch.events.is_empty() || !batch.prompts.is_empty() {
            if stepped {
                hub.loop_record_point(&format!("findings {round}"));
            }
            if let Err(e) = self.append_batch(hub, batch) {
                tracing::warn!(%thread, error = %format!("{e:#}"), "cannot record a review run's end; the next start retries it");
                return;
            }
        }
        self.after_loop_batch(hub, thread, staged);
    }
}

/// Runs review run `run` of thread `thread` (loop `id`, round `round`): module docs.
async fn run_review(
    hub: Arc<Inner>,
    thread: String,
    run: String,
    id: String,
    round: String,
    cancel: tokio::sync::watch::Receiver<Option<String>>,
) {
    let began = Instant::now();
    let prep = { hub.core().review_prep(&hub, &thread, &id, &round) };
    let mut prep = match prep {
        Ok(p) => p,
        Err(no) => {
            hub.core().review_end(
                &hub,
                &thread,
                &run,
                &round,
                None,
                Err(no),
                false,
                began.elapsed(),
            );
            return;
        }
    };
    let built = build_round(&hub, &prep, &round).await;
    let (diffs, truncated, read) = match built {
        Ok(d) => d,
        Err(trouble) => {
            // A box that went away: a failed run, retried once it is back. Anything else
            // (the worktree gone, git failing): the loop can't go on.
            let no = match trouble {
                Trouble::Away(why) => NoRun::Failed(why),
                Trouble::Failed(why) => NoRun::Fail(why),
            };
            hub.core().review_end(
                &hub,
                &thread,
                &run,
                &round,
                Some(&prep),
                Err(no),
                false,
                began.elapsed(),
            );
            return;
        }
    };
    let prompt = review_prompt(&hub, &prep, &diffs, &truncated);
    let prompt_name = format!("round-{round}.prompt.md");
    let prompt_file = prep.folder.join(&prompt_name);
    if let Err(e) = std::fs::write(&prompt_file, &prompt) {
        hub.core().review_end(
            &hub,
            &thread,
            &run,
            &round,
            Some(&prep),
            Err(NoRun::Fail(format!(
                "cannot write {}: {e}",
                prompt_file.display()
            ))),
            false,
            began.elapsed(),
        );
        return;
    }
    // A remote box's copy of the folder gets what the reviewer reads, its prompt with it.
    let mut names = shared_files(&prep.folder);
    names.push(prompt_name);
    if let Err(t) = prep.place.sync(&prep.loop_id, &prep.folder, &names).await {
        hub.core().review_end(
            &hub,
            &thread,
            &run,
            &round,
            Some(&prep),
            Err(NoRun::Failed(t.message().to_string())),
            false,
            began.elapsed(),
        );
        return;
    }
    prep.call.prompt = prompt.clone();
    let digest = twr_core::blob::blob_ref(prompt.as_bytes());
    let begun = {
        hub.core()
            .review_begin(&hub, &thread, &run, &round, &prep, diffs, digest)
    };
    if !begun {
        hub.core().review_end(
            &hub,
            &thread,
            &run,
            &round,
            Some(&prep),
            Err(NoRun::Gone),
            false,
            began.elapsed(),
        );
        return;
    }
    let (renew, renewer) = renewals(&hub, &thread, &prep);
    let call = prep.call.clone();
    let ended = prep
        .place
        .review(&prep.loop_id, &run, call, &prep.folder, renew, cancel)
        .await;
    renewer.abort();
    // An empty answer: whether the worktree is still the one the round read, which lets the
    // machine skip a verification round that could only repeat this one.
    let unchanged = match &ended {
        review::Ended::Answered { findings, .. } if findings.is_empty() => {
            still_as_read(&prep, read.as_deref()).await
        }
        _ => false,
    };
    hub.core().review_end(
        &hub,
        &thread,
        &run,
        &round,
        Some(&prep),
        Ok(ended),
        unchanged,
        began.elapsed(),
    );
}

/// What keeps run `prep`'s task of thread `thread` told: the callback that records its process
/// group once it started (`ext.twr.job.pgid`, what a later hub kills), and the renewer that
/// heartbeats the task every [`RENEW_EVERY`] (abort it when the run ends).
pub(super) fn renewals(
    hub: &Arc<Inner>,
    thread: &str,
    prep: &Prep,
) -> (
    impl FnOnce(u32) + Send + 'static,
    tokio::task::JoinHandle<()>,
) {
    let renew = {
        let hub = Arc::clone(hub);
        let (thread, task, description) = (
            thread.to_string(),
            prep.task.clone(),
            prep.description.clone(),
        );
        move |pid: u32| {
            tokio::spawn(async move {
                let start_time = match hub.host(LOCAL_BOX) {
                    Some(host) => crate::host::read_start_time(host, pid).await,
                    None => None,
                };
                let mut core = hub.core();
                if !core
                    .registry
                    .get(&thread)
                    .is_some_and(|v| v.background_tasks.contains_key(&task))
                {
                    return;
                }
                let body = EventBody::TaskUpdated(TaskUpdated {
                    task_id: task,
                    running: true,
                    description,
                    ext: twr_ext_map(json!({"job": {"pgid": pid, "start_time": start_time}})),
                    ..Default::default()
                });
                if let Err(e) = core.append(&hub, &thread, None, Source::Hub, body) {
                    tracing::warn!(%thread, error = %format!("{e:#}"), "cannot record a review run's process group");
                }
            });
        }
    };
    let renewer = {
        let hub = Arc::clone(hub);
        let (thread, task, description) = (
            thread.to_string(),
            prep.task.clone(),
            prep.description.clone(),
        );
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(RENEW_EVERY).await;
                let mut core = hub.core();
                if !core
                    .registry
                    .get(&thread)
                    .is_some_and(|v| v.background_tasks.contains_key(&task))
                {
                    return;
                }
                let body = EventBody::TaskUpdated(TaskUpdated {
                    task_id: task.clone(),
                    running: true,
                    description: description.clone(),
                    heartbeat: true,
                    ..Default::default()
                });
                let _ = core.append(&hub, &thread, None, Source::Hub, body);
            }
        })
    };
    (renew, renewer)
}

/// Whether every repository's diff, built again now, is byte for byte the one the round
/// reviewed (`read`: their digests; `None`: a diff was cut, so a change past the cut could not
/// be seen; never the same then). A diff that can't be built is not the same either.
async fn still_as_read(prep: &Prep, read: Option<&[String]>) -> bool {
    let Some(read) = read else {
        return false;
    };
    let started = prep.log.started.clone().unwrap_or_default();
    if started.bases.len() != read.len() {
        return false;
    }
    for (b, was) in started.bases.iter().zip(read) {
        let baseline = baseline_paths(&prep.folder, &b.slug);
        match prep
            .place
            .diff(Path::new(&b.repo), &b.base, &baseline, None, true)
            .await
        {
            Ok(d) if d.truncated.is_none() && d.digest == *was => {}
            _ => return false,
        }
    }
    true
}

/// The diffs' paths (where the reviewer reads them), a note of each one that was cut, and
/// the patches' digests as read (`None` when one was cut) ([`build_round`]).
pub(super) type Built = (Vec<String>, Vec<String>, Option<Vec<String>>);

/// The round's files in the loop folder: the common ones, and one diff per repository
/// (`diff-<slug>-<round>.patch`), built fresh from the loop's recorded base on the thread's
/// box (and kept there too, on a remote box) and written to the hub's folder.
pub(super) async fn build_round(
    hub: &Arc<Inner>,
    prep: &Prep,
    round: &str,
) -> Result<Built, Trouble> {
    write_common(hub, &prep.folder, &prep.log).map_err(|e| Trouble::Failed(format!("{e:#}")))?;
    // The plan check reads the plan, not a diff.
    if prep.template == ReviewTemplate::PlanReview {
        let plan = prep.folder.join("plan.md");
        if !plan.is_file() {
            return Err(Trouble::Failed(format!(
                "the plan is not in {}",
                plan.display()
            )));
        }
        return Ok((
            vec![prep.shown.join("plan.md").display().to_string()],
            Vec::new(),
            None,
        ));
    }
    let started = prep.log.started.clone().unwrap_or_default();
    let mut diffs = Vec::new();
    let mut notes = Vec::new();
    let mut read = Some(Vec::new());
    for b in &started.bases {
        let baseline = baseline_paths(&prep.folder, &b.slug);
        let name = format!("diff-{}-{round}.patch", b.slug);
        let d = prep
            .place
            .diff(
                Path::new(&b.repo),
                &b.base,
                &baseline,
                Some((&prep.loop_id, &name)),
                false,
            )
            .await?;
        let path = prep.folder.join(&name);
        std::fs::write(&path, d.patch.as_deref().unwrap_or_default())
            .map_err(|e| Trouble::Failed(format!("cannot write {}: {e}", path.display())))?;
        match (&mut read, d.truncated) {
            (Some(r), None) => r.push(d.digest.clone()),
            _ => read = None,
        }
        if let Some((kept, whole)) = d.truncated {
            notes.push(format!(
                "NOTE: the diff of {} is TRUNCATED ({kept} of {whole} bytes): review the rest from \
                 the repository on disk (git diff {}).",
                b.repo, b.base
            ));
        }
        diffs.push(prep.shown.join(&name).display().to_string());
    }
    if diffs.is_empty() {
        return Err(Trouble::Failed(
            "the loop records no repository to diff".into(),
        ));
    }
    Ok((diffs, notes, read))
}

/// The reviewer's prompt for `prep`'s round: its template (review, impl-review or
/// plan-review, as the machine named it) filled in.
fn review_prompt(hub: &Inner, prep: &Prep, diffs: &[String], notes: &[String]) -> String {
    let dir = prompts::templates_dir(hub.config_file.as_deref());
    let mut values = vec![
        (
            "diffs",
            diffs
                .iter()
                .map(|d| format!("- {d}"))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        ("repos", repos_list(&prep.log)),
        (
            "truncated",
            if notes.is_empty() {
                String::new()
            } else {
                format!("\n{}\n", notes.join("\n"))
            },
        ),
        (
            "ledger_path",
            prep.shown.join("ledger.md").display().to_string(),
        ),
        ("ledger_state", prompts::ledger_state(&prep.log.ledger)),
        ("folder", prep.shown.display().to_string()),
        ("thread", prep.name.clone()),
    ];
    match prep.template {
        ReviewTemplate::Review => values.push((
            "goal_path",
            prep.shown.join("goal.md").display().to_string(),
        )),
        ReviewTemplate::ImplReview | ReviewTemplate::PlanReview => {
            values.push(("task", task_words(&prep.log, &prep.shown, false)));
            values.push((
                "plan_path",
                prep.shown.join("plan.md").display().to_string(),
            ));
            values.push(("baseline", baseline_words(&prep.log, &prep.shown, true)));
        }
    }
    prompts::render(
        &prompts::template(dir.as_deref(), prep.template.as_str()),
        &values,
    )
}

/// Per repository slug, the commit check of a turn reads one of these ([`run_commit_check`]).
struct RepoCheck {
    repo: PathBuf,
    from: Option<String>,
    baseline: BTreeMap<String, Option<String>>,
    work: bool,
}

/// `ext.twr.`[`super::LOOP_START_KEY`]`.<key>` of loop `started`, as an object (empty when
/// absent).
fn loop_start(started: &LoopStarted, key: &str) -> serde_json::Map<String, Value> {
    started
        .ext
        .as_ref()
        .and_then(|e| e.get("twr"))
        .and_then(|t| t.get(super::LOOP_START_KEY))
        .and_then(|s| s.get(key))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

/// The baseline's changed paths of repository `slug` (`baseline-<slug>.paths.json` in the loop
/// folder; none for a review loop or a clean worktree).
fn baseline_paths(folder: &Path, slug: &str) -> BTreeMap<String, Option<String>> {
    std::fs::read(folder.join(format!("baseline-{slug}.paths.json")))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// Whether the latest round diff of repository `slug` shows a change (tracked or untracked):
/// whether the loop left work there. Without one, it did.
fn latest_diff_has_changes(folder: &Path, log: &LoopLog, slug: &str) -> bool {
    let Some(round) = log
        .rounds
        .iter()
        .rev()
        .find(|r| r.id != twr_core::loops::PLAN_CHECK_ROUND && r.answered)
    else {
        return true;
    };
    let Ok(patch) = std::fs::read(folder.join(format!("diff-{slug}-{}.patch", round.id))) else {
        return true;
    };
    let text = String::from_utf8_lossy(&patch);
    let section = |start: &str, end: &str| -> String {
        let Some(a) = text.find(start) else {
            return String::new();
        };
        let rest = &text[a..];
        let rest = rest.split_once('\n').map_or("", |(_, r)| r);
        match rest.find(end) {
            Some(b) => rest[..b].trim().to_string(),
            None => rest.trim().to_string(),
        }
    };
    let tracked = section("===== changes from the base", "\n===== new untracked files");
    let untracked = section(
        "===== new untracked files",
        "\n[twr: THIS DIFF IS TRUNCATED",
    );
    !tracked.is_empty() || (!untracked.is_empty() && untracked != "(none)")
}

/// Runs the commit check of turn `turn` of thread `thread`'s loop `loop_id`: each repository
/// read ([`super::git::repo_after`]) and checked ([`twr_core::loops::contract::check_commit`]),
/// which follows git; the turn's `twr-commit` block is the thread's note, and one missing or
/// invalid is noted, never a problem. The result is stepped as `CommitChecked`, stamped with
/// the turn.
async fn run_commit_check(hub: Arc<Inner>, thread: String, loop_id: String, turn: u64) {
    let prep = { hub.core().commit_prep(&hub, &thread, &loop_id, turn) };
    let Some((repos, text)) = prep else {
        hub.core()
            .commit_checked(&hub, &thread, &loop_id, turn, None);
        return;
    };
    // The thread's box: away, the check waits for it (the watch asks again once it is back).
    let place = {
        let core = hub.core();
        core.registry
            .get(&thread)
            .map(|v| v.box_name.clone())
            .and_then(|b| Place::of(&hub, &b).ok())
    };
    let Some(place) = place else {
        hub.core()
            .commit_checked(&hub, &thread, &loop_id, turn, None);
        return;
    };
    let notes: Vec<String> = match twr_core::loops::contract::parse_commit(&text) {
        Ok(_) => Vec::new(),
        Err(p)
            if p.iter()
                .any(|p| matches!(p, twr_core::loops::contract::BlockProblem::Missing(_))) =>
        {
            vec!["the thread wrote no twr-commit block (the check reads git, not the block)".into()]
        }
        Err(p) => vec![format!(
            "the thread's twr-commit block does not read: {} (the check reads git, not the block)",
            p.iter()
                .map(|p| prompts::one_line(&p.to_string()))
                .collect::<Vec<_>>()
                .join("; ")
        )],
    };
    let mut after = Vec::new();
    for r in &repos {
        match place
            .after(&r.repo, r.from.as_deref(), &r.baseline, r.work)
            .await
        {
            Ok(a) => after.push(a),
            // (Away: checked again once the box is back; never a commit problem.)
            Err(Trouble::Away(_)) => {
                hub.core()
                    .commit_checked(&hub, &thread, &loop_id, turn, None);
                return;
            }
            Err(Trouble::Failed(why)) => after.push(twr_core::loops::contract::RepoAfter {
                repo: r.repo.display().to_string(),
                from: r.from.clone(),
                left: vec![format!("(the box could not read it: {why})")],
                work: r.work,
                ..Default::default()
            }),
        }
    }
    let (commits, problems) = twr_core::loops::contract::check_commit(&after);
    hub.core().commit_checked(
        &hub,
        &thread,
        &loop_id,
        turn,
        Some(Input::CommitChecked {
            commits,
            problems,
            notes,
        }),
    );
}

/// The loops' watch: [`Core::check_loops`] on every kick (a step that leaves a review to run,
/// `Inner::loop_kick`) and every 2 s, until the hub stops.
pub(in crate::hub) async fn watch(hub: std::sync::Weak<Inner>, kick: Arc<tokio::sync::Notify>) {
    let Some(inner) = hub.upgrade() else {
        return;
    };
    let mut state = inner.state.subscribe();
    drop(inner);
    loop {
        tokio::select! {
            _ = state.wait_for(|s| *s != super::super::HubState::Running) => return,
            _ = kick.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
        }
        let Some(inner) = hub.upgrade() else {
            return;
        };
        inner.core().check_loops(&inner, false);
    }
}

/// Kills what is left of the review runs a stopped hub ran: each run's process group as its
/// task recorded it, and whatever still carries its `TWR_AGENT` marker.
pub(in crate::hub) async fn reap_reviews(hub: &Arc<Inner>, groups: Vec<(String, u32, u64)>) {
    let markers: Vec<String> = {
        let core = hub.core();
        core.registry
            .list()
            .into_iter()
            .flat_map(|v| {
                v.background_tasks
                    .iter()
                    .filter(|(_, t)| t.review)
                    .filter_map(|(id, _)| id.strip_prefix(LOOP_TASK_PREFIX))
                    .map(|l| format!("{}:loop-{l}", v.id))
                    .collect::<Vec<_>>()
            })
            .collect()
    };
    if groups.is_empty() && markers.is_empty() {
        return;
    }
    let hook = hub.reap_job_group_hook.clone();
    let done = tokio::task::spawn_blocking(move || {
        for (thread, pgid, start_time) in &groups {
            let reaped = match &hook {
                Some(h) => h(*pgid, *start_time),
                None => twr_runner::reap::reap_job_group(*pgid, *start_time, &[]),
            };
            tracing::info!(%thread, pgid, ?reaped, "what was left of a stopped hub's review run");
        }
        if !markers.is_empty() {
            twr_runner::reap::reap_agent_marked(&markers);
        }
    })
    .await;
    if let Err(e) = done {
        tracing::warn!(error = %e, "cleaning up after the review runs a stopped hub ran failed");
    }
}

impl Inner {
    /// [`super::super::HubOptions::loop_record_hook`], when set, with `point`.
    pub(in crate::hub) fn loop_record_point(&self, point: &str) {
        if let Some(hook) = &self.loop_record_hook {
            hook(point);
        }
    }

    /// The CLI of reviewer runtime `runtime` ([`super::super::HubOptions::reviewer_bins`]).
    pub(in crate::hub) fn reviewer_bin(&self, runtime: &str) -> PathBuf {
        self.reviewer_bins
            .get(runtime)
            .cloned()
            .or_else(|| twr_adapters::resolve::runtime_program(runtime, |k| std::env::var_os(k)))
            .unwrap_or_else(|| PathBuf::from(runtime))
    }
}

impl super::super::Hub {
    /// Ends every review run (the hub's shutdown) and waits, at most `within`, for them to be
    /// gone: their process groups killed, nothing recorded.
    pub(in crate::hub) async fn end_reviews(&self, within: Duration) {
        {
            self.inner
                .core()
                .loop_runs
                .cancel_all("the hub is stopping");
        }
        let deadline = Instant::now() + within;
        while self.inner.core().loop_runs.any() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
