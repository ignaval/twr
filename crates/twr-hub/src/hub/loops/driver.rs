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
//! description `reviewer <model> <effort>`), the run itself (`hub::loops::review`), and its end:
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
//! interrupted); and, once after the hub started, a step whose turn ended while no hub ran is
//! reacted to as at its end. Nothing runs before the hub's start-up recovery is done.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use serde_json::{json, Value};
use twr_core::event::*;
use twr_core::loops::contract::{render_ledger, FINDINGS_SCHEMA};
use twr_core::loops::{Action, Input, LoopState, StepError, Wait};

use super::super::{twr_ext, twr_ext_map, Batch, Core, Inner, LOCAL_BOX};
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
    ready: bool,
}

impl LoopRuns {
    /// Ends thread `thread`'s review run, if any, with `why`.
    pub(in crate::hub) fn cancel(&self, thread: &str, why: &str) {
        if let Some(r) = self.runs.get(thread) {
            let _ = r.cancel.send(Some(why.to_string()));
        }
    }

    /// Ends every review run.
    pub(in crate::hub) fn cancel_all(&self, why: &str) {
        for r in self.runs.values() {
            let _ = r.cancel.send(Some(why.to_string()));
        }
    }

    /// Whether any runs.
    pub(in crate::hub) fn any(&self) -> bool {
        !self.runs.is_empty()
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
/// `triage m3`, `sweep`, `report`); `None` while it waits for a review or nothing.
pub(in crate::hub) fn step_tag(s: &LoopState) -> Option<String> {
    match &s.wait {
        Wait::Goal => Some("scope".into()),
        Wait::Triage { round } => Some(format!("triage {round}")),
        Wait::Sweep => Some("sweep".into()),
        Wait::Report => Some("report".into()),
        Wait::Review { .. } | Wait::Ended => None,
    }
}

/// What a staged step leaves to do once its batch is committed.
#[derive(Debug, Default, Clone, Copy)]
pub(in crate::hub) struct Staged {
    /// A prompt was queued: the thread's queue is to be dispatched.
    pub(in crate::hub) prompted: bool,
    /// A review is to run.
    pub(in crate::hub) review: bool,
}

impl Staged {
    fn and(self, other: Staged) -> Staged {
        Staged {
            prompted: self.prompted || other.prompted,
            review: self.review || other.review,
        }
    }
}

/// What a turn's end brings its thread's loop ([`Core::loop_answer`]).
pub(in crate::hub) struct LoopAnswer {
    /// `report.ready` `ext.twr.loop` of the turn's report.
    pub(in crate::hub) mark: Value,
    /// The report's summary instead of the turn's own (the loop's last report).
    pub(in crate::hub) summary: Option<String>,
    /// What to step; `None`: an interim end, the step still waits.
    input: Option<Input>,
}

/// The folder of loop `id` as `log` records it (else where the hub puts loop folders).
fn folder_of(hub: &Inner, log: &LoopLog, id: &str) -> PathBuf {
    log.started
        .as_ref()
        .and_then(|s| s.folder.as_deref())
        .map(PathBuf::from)
        .unwrap_or_else(|| loops_root(hub).join(id))
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
/// schema, `ledger.md` and `goal.md`.
fn write_common(hub: &Inner, folder: &Path, log: &LoopLog) -> anyhow::Result<()> {
    make_folder(hub, folder)?;
    std::fs::write(folder.join("findings.schema.json"), FINDINGS_SCHEMA)?;
    std::fs::write(folder.join("ledger.md"), render_ledger(&log.ledger))?;
    let goal = log
        .goal
        .as_deref()
        .unwrap_or("(not written yet: the scope turn writes it)");
    std::fs::write(
        folder.join("goal.md"),
        format!("# Goal\n\n{}\n", goal.trim()),
    )?;
    Ok(())
}

/// `- <repo> (from <base>)` per repository of `log`'s start.
fn repos_list(log: &LoopLog) -> String {
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
                                "{name}: its review loop is stuck: {}. Resume asks for that step \
                                 again; stop ends the loop.",
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
                    let log = log.get_or_insert_with(|| self.loop_log(thread, &state.id));
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
        let folder = folder_of(hub, log, &state.id);
        write_common(hub, &folder, log).map_err(|e| format!("{e:#}"))?;
        let dir = prompts::templates_dir(hub.config_file.as_deref());
        let dir = dir.as_deref();
        let started = log.started.clone().unwrap_or_default();
        let commit = prompts::commit_rule(started.settings.review_commit).to_string();
        let ledger_path = folder.join("ledger.md").display().to_string();
        let tag = step_tag(state).unwrap_or_else(|| "scope".into());
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
            Action::AskTriage { round, findings } => (
                "triage",
                false,
                vec![
                    ("round", round.clone()),
                    (
                        "count",
                        match findings.len() {
                            1 => "1 finding".to_string(),
                            n => format!("{n} findings"),
                        },
                    ),
                    ("findings", prompts::wrap_findings(round, findings)),
                    (
                        "finding_ids",
                        findings
                            .iter()
                            .map(|f| f.id.clone())
                            .collect::<Vec<_>>()
                            .join(", "),
                    ),
                    ("ledger_path", ledger_path.clone()),
                    ("commit_rule", commit.clone()),
                ],
            ),
            Action::AskCorrection { step, problems } => (
                "correction",
                true,
                vec![
                    ("step", tag.clone()),
                    (
                        "problems",
                        problems
                            .iter()
                            .map(|p| format!("- {}", prompts::one_line(p)))
                            .collect::<Vec<_>>()
                            .join("\n"),
                    ),
                    (
                        "block",
                        match step {
                            twr_core::loops::Step::Goal => "twr-goal",
                            twr_core::loops::Step::Triage => "twr-triage",
                            twr_core::loops::Step::Sweep => "twr-sweep",
                        }
                        .to_string(),
                    ),
                ],
            ),
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
                        ("commit_rule", commit.clone()),
                    ],
                )
            }
            Action::AskReport => (
                "report",
                false,
                vec![
                    (
                        "outcome",
                        loop_report::outcome_words(
                            state.outcome.unwrap_or_default(),
                            log.answered_rounds(),
                        ),
                    ),
                    (
                        "report_path",
                        folder.join("report.md").display().to_string(),
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
        for (key, needed) in [("triage", "findings"), ("correction", "problems")] {
            if name == key && !template.contains(&format!("{{{{{needed}}}}}")) {
                if let Some((_, v)) = values.iter().find(|(k, _)| *k == needed) {
                    body.push_str("\n\n");
                    body.push_str(v);
                }
            }
        }
        Ok(format!(
            "{}\n\n{}",
            prompts::marker(&state.id, &tag, correction),
            body.trim_end()
        ))
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
        if self.shutting_down || !self.loop_runs.ready {
            return None;
        }
        let state = self.loop_state(thread)?;
        let tag = step_tag(&state)?;
        let started = self.turn_start(thread, turn)?;
        let owner = self.turn_owner(thread, turn, &started, &state, &tag)?;
        let mut mark = json!({"id": state.id, "step": tag});
        let accepted = |i: &Input| {
            matches!(
                i,
                Input::GoalWritten { .. } | Input::TriageAccepted { .. } | Input::SweepDone { .. }
            )
        };
        match owner {
            Owner::IfBlock => {
                if state.wait == Wait::Report {
                    return None;
                }
                let input = state.reply_input(final_text?)?;
                accepted(&input).then_some(LoopAnswer {
                    mark,
                    summary: None,
                    input: Some(input),
                })
            }
            Owner::Loop if interim => {
                mark["interim"] = json!(true);
                Some(LoopAnswer {
                    mark,
                    summary: None,
                    input: None,
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
                );
                let written = write_common(hub, &folder, &log)
                    .and_then(|()| std::fs::write(&path, text).map_err(anyhow::Error::from));
                let input = match written {
                    Ok(()) => {
                        let shown = path.display().to_string();
                        mark["outcome"] = json!(outcome);
                        mark["report_path"] = json!(shown);
                        Input::ReportWritten {
                            report_path: shown,
                            commit: None,
                        }
                    }
                    Err(e) => Input::Fail {
                        why: format!("cannot write {}: {e:#}", path.display()),
                    },
                };
                let summary = format!(
                    "{}; report: {}",
                    loop_report::outcome_words(outcome, log.answered_rounds()),
                    path.display()
                );
                Some(LoopAnswer {
                    mark,
                    summary: Some(summary),
                    input: Some(input),
                })
            }
            Owner::Loop => Some(LoopAnswer {
                mark,
                summary: None,
                input: Some(state.reply_input(final_text.unwrap_or(""))?),
            }),
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
            return Staged::default();
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
        let detail = match self.stored_event(thread, Some(failed), "turn.ended") {
            Ok(Some(Event {
                body: EventBody::TurnEnded(p),
                ..
            })) => twr_ext(&p.ext)
                .and_then(|t| t.get("error"))
                .and_then(Value::as_str)
                .map(|e| format!(": {}", prompts::one_line(e)))
                .unwrap_or_default(),
            _ => String::new(),
        };
        let what = if view.crashed { "crashed" } else { "failed" };
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
        match self.loop_step(
            hub,
            &mut batch,
            thread,
            Input::TurnFailed { why },
            Some(turn),
        ) {
            Ok(_) => {
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
        // took no step from (its own shutdown's).
        !view
            .last_turn
            .as_ref()
            .is_some_and(|t| t.turn >= ask.turn && t.result == TurnResult::Interrupted)
    }

    /// The review tasks thread `thread` shows that no run of this hub's is (a previous hub's).
    fn orphan_review_tasks(&self, thread: &str) -> Vec<String> {
        let mine = self.loop_runs.runs.get(thread);
        self.registry
            .get(thread)
            .map(|v| {
                v.background_tasks
                    .iter()
                    .filter(|(id, t)| {
                        t.review
                            && !mine
                                .is_some_and(|r| **id == format!("{LOOP_TASK_PREFIX}{}", r.loop_id))
                    })
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
        let mut batch = Batch::default();
        let mut staged = Staged::default();
        if !self.loop_runs.runs.contains_key(thread) {
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
            Wait::Ended => {}
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
                        .is_some_and(|s| s.wait == state.wait)
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
                }
            }
        }
        if !batch.events.is_empty() || !batch.prompts.is_empty() {
            if let Err(e) = self.append_batch(hub, batch) {
                tracing::warn!(%thread, error = %format!("{e:#}"), "cannot record a loop's step");
                return;
            }
        }
        self.after_loop_batch(hub, thread, staged);
    }

    /// At startup: the turn of thread `thread`'s step that ended while no hub ran, taken as at
    /// its end; one the hub's stop interrupted, or that failed then, is asked again
    /// ([`Core::check_loop`] does, finding no ask).
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
        if last.result != TurnResult::Completed {
            return None;
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
        if staged.prompted {
            if let Err(e) = self.try_dispatch(hub, thread) {
                tracing::warn!(%thread, error = %format!("{e:#}"), "dispatching a loop's prompt failed");
            }
        }
        if staged.review {
            self.start_review(hub, thread);
        }
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

/// What a review run needs, read under the hub's lock.
struct Prep {
    name: String,
    log: LoopLog,
    folder: PathBuf,
    call: review::Call,
    task: String,
    description: String,
}

/// Why a review run does not run.
enum NoRun {
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
        let view = self.registry.get(thread).ok_or(NoRun::Gone)?;
        let log = self.loop_log(thread, id);
        let started = log
            .started
            .clone()
            .ok_or_else(|| NoRun::Fail("the loop's start is not in the log".into()))?;
        let folder = folder_of(hub, &log, id);
        let cwd = view
            .worktree
            .clone()
            .map(PathBuf::from)
            .or_else(|| started.bases.first().map(|b| PathBuf::from(&b.repo)))
            .ok_or_else(|| NoRun::Fail(format!("{} has no working directory", view.name)))?;
        if !cwd.is_dir() {
            return Err(NoRun::Fail(format!(
                "{}'s working directory {} is gone",
                view.name,
                cwd.display()
            )));
        }
        let reviewer = &started.settings.reviewer;
        let effort = state.reviewer.effort.clone().unwrap_or_default();
        let account_env = match &reviewer.account {
            None => None,
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
        let file = |name: String| folder.join(name);
        Ok(Prep {
            name: view.name.clone(),
            folder: folder.clone(),
            task: format!("{LOOP_TASK_PREFIX}{id}"),
            description: format!("reviewer {} {effort}", reviewer.model),
            call: review::Call {
                runtime: reviewer.runtime.clone(),
                bin: hub.reviewer_bin(&reviewer.runtime),
                model: reviewer.model.clone(),
                effort,
                cwd,
                folder: folder.clone(),
                prompt: String::new(),
                schema_file: file("findings.schema.json".into()),
                answer_file: file(format!("round-{round}.answer.json")),
                log_file: file(format!("round-{round}.log")),
                account_env,
                marker: format!("{thread}:loop-{id}"),
                timeout: hub.config.loops.round_timeout(),
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
        Err(why) => {
            hub.core().review_end(
                &hub,
                &thread,
                &run,
                &round,
                Some(&prep),
                Err(NoRun::Fail(why)),
                false,
                began.elapsed(),
            );
            return;
        }
    };
    let prompt = review_prompt(&hub, &prep, &diffs, &truncated);
    let prompt_file = prep.folder.join(format!("round-{round}.prompt.md"));
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
    let renew = {
        let hub = Arc::clone(&hub);
        let (thread, task, description) =
            (thread.clone(), prep.task.clone(), prep.description.clone());
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
    let call = prep.call.clone();
    let renewer = {
        let hub = Arc::clone(&hub);
        let (thread, task, description) =
            (thread.clone(), prep.task.clone(), prep.description.clone());
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
    let ended = review::run(call, renew, cancel).await;
    renewer.abort();
    // An empty answer: whether the worktree is still the one the round read, which lets the
    // machine skip a verification round that could only repeat this one.
    let unchanged = match &ended {
        review::Ended::Answered { findings, .. } if findings.is_empty() => {
            still_as_read(&hub, &prep, read.as_deref()).await
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

/// Whether every repository's diff, built again now, is byte for byte `read`, the one the
/// round reviewed (`None`: a diff was cut, so a change past the cut could not be seen; never
/// the same then). A diff that can't be built is not the same either.
async fn still_as_read(hub: &Arc<Inner>, prep: &Prep, read: Option<&[Vec<u8>]>) -> bool {
    let Some(read) = read else {
        return false;
    };
    let Some(git) = hub.host(LOCAL_BOX).and_then(|h| h.local_git()) else {
        return false;
    };
    let started = prep.log.started.clone().unwrap_or_default();
    if started.bases.len() != read.len() {
        return false;
    }
    for (b, was) in started.bases.iter().zip(read) {
        match super::git::round_diff(&git, Path::new(&b.repo), &b.base).await {
            Ok(d) if d.truncated.is_none() && d.patch == *was => {}
            _ => return false,
        }
    }
    true
}

/// The diffs' paths, a note of each one that was cut, and the patches as read (`None` when
/// one was cut) ([`build_round`]).
type Built = (Vec<String>, Vec<String>, Option<Vec<Vec<u8>>>);

/// The round's files in the loop folder: the common ones, and one diff per repository
/// (`diff-<slug>-<round>.patch`), built fresh from the loop's recorded base.
async fn build_round(hub: &Arc<Inner>, prep: &Prep, round: &str) -> Result<Built, String> {
    write_common(hub, &prep.folder, &prep.log).map_err(|e| format!("{e:#}"))?;
    let git = hub
        .host(LOCAL_BOX)
        .and_then(|h| h.local_git())
        .ok_or("the hub's own box has no git")?;
    let started = prep.log.started.clone().unwrap_or_default();
    let mut diffs = Vec::new();
    let mut notes = Vec::new();
    let mut read = Some(Vec::new());
    for b in &started.bases {
        let d = super::git::round_diff(&git, Path::new(&b.repo), &b.base).await?;
        let path = prep.folder.join(format!("diff-{}-{round}.patch", b.slug));
        std::fs::write(&path, &d.patch)
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        match (&mut read, d.truncated) {
            (Some(r), None) => r.push(d.patch.clone()),
            _ => read = None,
        }
        if let Some((kept, whole)) = d.truncated {
            notes.push(format!(
                "NOTE: the diff of {} is TRUNCATED ({kept} of {whole} bytes): review the rest from \
                 the repository on disk (git diff {}).",
                b.repo, b.base
            ));
        }
        diffs.push(path.display().to_string());
    }
    if diffs.is_empty() {
        return Err("the loop records no repository to diff".into());
    }
    Ok((diffs, notes, read))
}

/// The reviewer's prompt for `prep`'s round: the `review` template filled in.
fn review_prompt(hub: &Inner, prep: &Prep, diffs: &[String], notes: &[String]) -> String {
    let dir = prompts::templates_dir(hub.config_file.as_deref());
    let values = vec![
        (
            "goal_path",
            prep.folder.join("goal.md").display().to_string(),
        ),
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
            prep.folder.join("ledger.md").display().to_string(),
        ),
        ("ledger_state", prompts::ledger_state(&prep.log.ledger)),
        ("folder", prep.folder.display().to_string()),
        ("thread", prep.name.clone()),
    ];
    prompts::render(&prompts::template(dir.as_deref(), "review"), &values)
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
