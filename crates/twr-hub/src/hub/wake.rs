//! Waking a worker whose background work ended with no turn after it.
//!
//! A worker's turn may end while work it started still runs: its runtime's background tasks (a
//! Claude Code `run_in_background` command, a Monitor) or its jobs (`twr job run`). That turn's
//! report is interim (`report::interim_tasks`): the result comes in a later turn, which the
//! runtime normally starts itself once the work ends (`turn.started{from: runtime}`), or which a
//! job's wake prompt starts. Sometimes neither happens (a job started `--no-wake` whose watcher
//! ended without the runtime following up), and the worker sits idle with its work half done.
//!
//! So when the last of a worker's tasks ends (not a `!` command of the user's) with no turn
//! open, its last turn completed and its report interim, the hub waits [`HubOptions::wake_after`]
//! (`wake_after_secs`, 30 s by default; 0 = never) by log time. If by then no turn started, no
//! task started again and nothing is queued for it, and the thread is neither stopped, parked,
//! attached, crashed nor waiting on the user, the hub queues it a prompt itself (`from: runtime`,
//! source hub, like a job's wake): what ended since that turn, with each job's result and log,
//! and that it should continue. A `conversation.update` on the worker records the wake
//! (`ext.twr.auto_wake = {turn, tasks}`) in the same batch, which the user sees in the thread
//! and the orchestrator in its conversation. A runtime that is being stopped (a settings
//! restart) or a box offline holds a wake up: it is tried again for [`WAKE_RETRY_FOR`].
//!
//! A turn the runtime opened for a background task's notification and ended with no run
//! (`turn.ended` `ext.twr.no_run`, the Claude Code adapter's) handled nothing: its report is
//! the interim report it followed, again ([`no_run_report`], marked `ext.twr.no_run_of =
//! <that turn>`), so the worker is still one that yielded with its work unhandled, for the
//! wake, the orchestrator's idle line and `send --after` alike. That report, once the worker
//! has nothing else running, wakes it at once (the wait was the adapter's), for the tasks that
//! ended before that turn; and when work of it still runs, the last task's end does, as for
//! any interim report.
//!
//! No loops: a worker is woken at most once per interim report (the recorded `turn`), and at
//! most [`WAKE_CHAIN_MAX`] times in a row with no turn the user or the orchestrator started
//! in between; past that the hub records `ext.twr.auto_wake_skipped = {turn, why}` instead
//! and leaves it idle. A woken turn that ends without new background work has a final report,
//! so nothing wakes it again. Both are rebuilt from the log on replay. A wake still waiting is
//! memory only: a hub restarted meanwhile wakes nothing (the orchestrator's idle line goes
//! out instead).
//!
//! The runtime's own follow-up: one that starts within the wait opens a turn, and the hub
//! wakes nothing. One that starts after the hub's prompt was delivered runs as its own turn
//! (the Claude Code adapter stamps the runs in order); the prompt says to answer in one line
//! when the work was handled already. An orchestrator's watch on the worker fires on the woken
//! turn's report (a follow-up's: `follow_up=true`); the line about the worker going idle
//! (`hub::watches`) is held until the wake was decided, and goes out only if none started.
//!
//! [`HubOptions::wake_after`]: super::HubOptions::wake_after

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use twr_core::event::*;
use twr_core::reduce::ThreadView;

use super::{twr_ext, twr_ext_map, Batch, Core, Inner};
/// `turn.ended` `ext.twr` key of a turn its runtime opened for a background task's
/// notification that saw no run (the Claude Code adapter's).
pub(super) const NO_RUN_KEY: &str = twr_adapters::claude_code::translate::NO_RUN_KEY;
use crate::report;

/// `conversation.update` `ext.twr` key: the hub woke the worker, `{turn, tasks}`.
pub(super) const AUTO_WAKE_KEY: &str = "auto_wake";
/// `conversation.update` `ext.twr` key: the hub did not wake the worker, `{turn, why}`.
pub(super) const AUTO_WAKE_SKIPPED_KEY: &str = "auto_wake_skipped";
/// Most wakes in a row with no turn of the user's or the orchestrator's in between.
pub const WAKE_CHAIN_MAX: u32 = 5;
/// Most ended tasks a wake prompt lists.
const TASKS_LISTED: usize = 10;
/// The start of a wake prompt.
pub const WAKE_PROMPT_START: &str = "[twr] Your background work finished";

/// One task that ended since the worker's last turn ended.
#[derive(Debug, Clone, PartialEq)]
struct EndedTask {
    /// What its end said (a job's: `job <name>: exited 0 after 3m`).
    description: String,
    /// A job's id and log.
    job: Option<(String, String)>,
}

/// A wake waiting for its time.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Pending {
    /// The turn whose interim report the wake is for.
    turn: u64,
    due: DateTime<Utc>,
    /// When it first came due: a wake held up by something passing (a runtime restarting,
    /// its box offline) is tried again until [`WAKE_RETRY_FOR`] after that.
    first_due: DateTime<Utc>,
}

/// How long a wake held up by something passing is tried again.
const WAKE_RETRY_FOR: chrono::Duration = chrono::Duration::minutes(10);
/// How often it is tried again meanwhile.
const WAKE_RETRY_EVERY: chrono::Duration = chrono::Duration::seconds(2);

/// Whether a wake can go out now ([`Core::wake_ready`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ready {
    Now,
    /// Something passing is in the way: try again.
    Later,
    Never,
}

/// The hub's wakes (see the module docs).
#[derive(Debug, Default)]
pub(super) struct WakeState {
    /// [`super::HubOptions::wake_after`], set once the log is replayed (zero until then:
    /// replay schedules no wake); zero = never.
    pub(super) after: chrono::Duration,
    pending: HashMap<String, Pending>,
    /// The tasks that ended since each thread's last turn ended (one that saw no run aside):
    /// those that ended during a turn are its to handle, and go with its end.
    ended: HashMap<String, Vec<EndedTask>>,
    /// The last turn the hub woke each thread after, or decided not to.
    woken: HashMap<String, u64>,
    /// Wakes in a row per thread, with no turn of the user's or the orchestrator's since.
    chain: HashMap<String, u32>,
}

/// `report.ready` `ext.twr` key of the report of a turn that saw no run ([`no_run_report`]):
/// the turn whose interim report it repeats.
pub(super) const NO_RUN_OF_KEY: &str = "no_run_of";

/// The report of turn `turn` of `view`, a turn its runtime opened for a notification that saw
/// no run: the interim report it followed, again, marked [`NO_RUN_OF_KEY`]. `None` when the
/// last report is no interim one (the turn then reports as any other).
pub(super) fn no_run_report(view: &ThreadView, turn: u64) -> Option<ReportReady> {
    let r = view
        .last_report
        .as_ref()
        .filter(|r| report::interim_tasks(r) > 0 && r.turn.is_some_and(|t| t < turn))?;
    let mut report = r.clone();
    report.turn = Some(turn);
    report.written_by = "hub".into();
    let of = no_run_of(r).or(r.turn);
    set_twr_ext(&mut report.ext, NO_RUN_OF_KEY, json!(of));
    Some(report)
}

/// The turn a report of a turn that saw no run repeats the interim report of.
pub(super) fn no_run_of(r: &ReportReady) -> Option<u64> {
    r.ext.as_ref()?.get("twr")?.get(NO_RUN_OF_KEY)?.as_u64()
}

impl WakeState {
    /// When thread `thread`'s pending wake is due, if one is.
    pub(super) fn due(&self, thread: &str) -> Option<DateTime<Utc>> {
        self.pending.get(thread).map(|p| p.due)
    }
}

/// The turn whose interim report `view` waits on: its last turn, completed and reported with
/// an interim report, no turn open and no own background work left.
fn interim_turn(view: &ThreadView) -> Option<u64> {
    if view.is_orchestrator()
        || view.id == HUB_THREAD
        || view.stopped
        || view.parked
        || view.current_turn.is_some()
        || view.own_background()
        || !view.reported
    {
        return None;
    }
    let r = view
        .last_report
        .as_ref()
        .filter(|r| report::interim_tasks(r) > 0)?;
    let turn = r.turn.or(view.last_turn.as_ref().map(|t| t.turn));
    super::watches::turn_completed(view, turn).then_some(turn?)
}

impl Core {
    /// Wake bookkeeping for one applied event (live and on replay); see the module docs.
    pub(super) fn track_wake(&mut self, ev: &Event) {
        match &ev.body {
            EventBody::TaskUpdated(p) if !p.running => {
                if is_shell_job(p) {
                    return;
                }
                let Some(v) = self.registry.get(&ev.thread) else {
                    return;
                };
                if v.is_orchestrator() || ev.thread == HUB_THREAD {
                    return;
                }
                let job = twr_ext(&p.ext).and_then(|t| t.get("job"));
                let ended = EndedTask {
                    description: p.description.clone(),
                    job: job.map(|j| {
                        let s = |k: &str| j.get(k).and_then(Value::as_str).unwrap_or("");
                        (s("id").to_string(), s("log").to_string())
                    }),
                };
                self.wake
                    .ended
                    .entry(ev.thread.clone())
                    .or_default()
                    .push(ended);
                if self.wake.after <= chrono::Duration::zero() {
                    return;
                }
                if let Some(turn) = interim_turn(v) {
                    self.schedule_wake(&ev.thread, turn, ev.ts + self.wake.after);
                }
            }
            // Busy again (its own work, not a `!` command of the user's): a new wait starts
            // when that task ends.
            EventBody::TaskUpdated(p) => {
                if self
                    .registry
                    .get(&ev.thread)
                    .is_some_and(|v| v.background_tasks.get(&p.task_id).is_some_and(|t| !t.shell))
                {
                    self.wake.pending.remove(&ev.thread);
                }
            }
            EventBody::TurnStarted(p) => {
                self.wake.pending.remove(&ev.thread);
                // A turn of the user's or the orchestrator's: someone is looking after it.
                if p.from != TurnFrom::Runtime {
                    self.wake.chain.remove(&ev.thread);
                }
            }
            // The tasks that ended before it were the turn's to handle; not those of a turn
            // opened for a notification that saw no run: that turn handled nothing.
            EventBody::TurnEnded(p) => {
                let no_run = twr_ext(&p.ext)
                    .and_then(|t| t.get(NO_RUN_KEY))
                    .and_then(Value::as_bool)
                    == Some(true);
                if !no_run {
                    self.wake.ended.remove(&ev.thread);
                }
            }
            // The report of a turn that saw no run, the worker idle: woken at once.
            EventBody::ReportReady(p) if no_run_of(p).is_some() => {
                let Some(v) = self.registry.get(&ev.thread) else {
                    return;
                };
                if self.wake.after > chrono::Duration::zero() {
                    if let Some(turn) = interim_turn(v) {
                        self.schedule_wake(&ev.thread, turn, ev.ts);
                    }
                }
            }
            EventBody::ConversationUpdate(p) => {
                let Some(twr) = twr_ext(&p.ext) else {
                    return;
                };
                let woke = twr.get(AUTO_WAKE_KEY);
                let Some(turn) = woke
                    .or_else(|| twr.get(AUTO_WAKE_SKIPPED_KEY))
                    .and_then(|w| w.get("turn"))
                    .and_then(Value::as_u64)
                else {
                    return;
                };
                self.wake.woken.insert(ev.thread.clone(), turn);
                self.wake.pending.remove(&ev.thread);
                if woke.is_some() {
                    *self.wake.chain.entry(ev.thread.clone()).or_default() += 1;
                }
            }
            // (A stop or park cancels nothing here: a stop that restarts the runtime for a
            // settings change leaves the thread as it was, and a wake checks again when due.)
            EventBody::ThreadDeleted(_) => {
                self.wake.pending.remove(&ev.thread);
                self.wake.ended.remove(&ev.thread);
                self.wake.woken.remove(&ev.thread);
                self.wake.chain.remove(&ev.thread);
            }
            _ => {}
        }
    }

    /// Wakes the workers whose wait is over (the background ticker), each once.
    pub(super) fn check_wakes(&mut self, hub: &Arc<Inner>) {
        if self.shutting_down || self.wake.pending.is_empty() {
            return;
        }
        let now = Utc::now();
        let due: Vec<(String, Pending)> = self
            .wake
            .pending
            .iter()
            .filter(|(_, p)| p.due <= now)
            .map(|(t, p)| (t.clone(), *p))
            .collect();
        for (thread, p) in due {
            self.wake.pending.remove(&thread);
            match self.wake_ready(&thread, p.turn) {
                Ok(Ready::Now) => {}
                Ok(Ready::Later) if now - p.first_due < WAKE_RETRY_FOR => {
                    let due = now + WAKE_RETRY_EVERY;
                    // Its idle line keeps waiting for the wake.
                    self.orch
                        .hold_idle(&thread, due + super::watches::IDLE_GRACE);
                    self.wake.pending.insert(thread, Pending { due, ..p });
                    continue;
                }
                Ok(_) => continue,
                Err(e) => {
                    tracing::warn!(thread = %thread, error = %format!("{e:#}"), "could not tell whether to wake a thread after its background work");
                    continue;
                }
            }
            if let Err(e) = self.wake_thread(hub, &thread, p.turn) {
                tracing::warn!(thread = %thread, error = %format!("{e:#}"), "could not wake a thread after its background work");
            }
        }
    }

    /// Waits to wake thread `thread` after turn `turn` until `due`, unless it was woken (or
    /// not) for that turn already.
    fn schedule_wake(&mut self, thread: &str, turn: u64, due: DateTime<Utc>) {
        if self.wake.woken.get(thread) != Some(&turn) {
            self.wake.pending.insert(
                thread.to_string(),
                Pending {
                    turn,
                    due,
                    first_due: due,
                },
            );
        }
    }

    /// Whether thread `id` can take a wake for turn `turn` now: still idle after that turn's
    /// interim report, nothing queued, nothing in the way of a turn; [`Ready::Later`] while its runtime is being stopped (a settings restart) or its
    /// box is offline.
    fn wake_ready(&self, id: &str, turn: u64) -> anyhow::Result<Ready> {
        let Some(v) = self.registry.get(id) else {
            return Ok(Ready::Never);
        };
        if interim_turn(v) != Some(turn)
            || v.turns != turn
            || v.attached.is_some()
            || v.crashed
            || v.is_blocked()
            || self.wake.woken.get(id) == Some(&turn)
            || self.rt.get(id).is_some_and(|r| r.queue_held)
            || self.store.pending_count(id)? > 0
        {
            return Ok(Ready::Never);
        }
        if !self.box_online(&v.box_name) || self.rt.get(id).is_some_and(|r| r.stop_requested) {
            return Ok(Ready::Later);
        }
        Ok(Ready::Now)
    }

    /// Wakes thread `id` after turn `turn` (ready: [`Core::wake_ready`]).
    fn wake_thread(&mut self, hub: &Arc<Inner>, id: &str, turn: u64) -> anyhow::Result<()> {
        let name = self.thread_name(id);
        let chain = self.wake.chain.get(id).copied().unwrap_or(0);
        let mut batch = Batch::default();
        if chain >= WAKE_CHAIN_MAX {
            let why = format!(
                "woken {chain} times in a row already with no message from the user or the \
                 orchestrator"
            );
            batch.push(
                id,
                None,
                Source::Hub,
                EventBody::ConversationUpdate(ConversationUpdate {
                    text: format!(
                        "{name}'s background work finished and no turn started after it; twr \
                         did not wake it ({why}). Send it a message to continue."
                    ),
                    about_thread: Some(id.to_string()),
                    ext: twr_ext_map(json!({ AUTO_WAKE_SKIPPED_KEY: {"turn": turn, "why": why} })),
                    ..Default::default()
                }),
            );
            self.append_batch(hub, batch)?;
            return Ok(());
        }
        let ended = self.wake.ended.get(id).cloned().unwrap_or_default();
        // The turn the work was left running by: the interim report's.
        let shown = self
            .registry
            .get(id)
            .and_then(|v| v.last_report.as_ref())
            .and_then(no_run_of)
            .unwrap_or(turn);
        let prompt = wake_prompt(shown, &ended);
        let tasks: Vec<Value> = ended
            .iter()
            .map(|t| match &t.job {
                Some((job, log)) => json!({"description": t.description, "job": job, "log": log}),
                None => json!({"description": t.description}),
            })
            .collect();
        batch.push(
            id,
            None,
            Source::Hub,
            EventBody::ConversationUpdate(ConversationUpdate {
                text: format!(
                    "{name}'s background work finished and no turn started after it: twr woke it \
                     to continue (after its interim report of turn {turn})."
                ),
                about_thread: Some(id.to_string()),
                ext: twr_ext_map(json!({ AUTO_WAKE_KEY: {"turn": turn, "tasks": tasks} })),
                ..Default::default()
            }),
        );
        self.push_prompt(
            &mut batch,
            id,
            &prompt,
            TurnFrom::Runtime,
            Source::Hub,
            None,
        )
        .map_err(|e| anyhow::anyhow!("{}", e.message))?;
        let done = self.append_batch(hub, batch)?;
        if let Some(&q) = done.queued.first() {
            self.dispatch_queued(hub, id, q);
        }
        Ok(())
    }
}

/// Whether task update `p` is the end of a `!` command of the user's (`ext.twr.job.shell`).
fn is_shell_job(p: &TaskUpdated) -> bool {
    twr_ext(&p.ext)
        .and_then(|t| t.get("job"))
        .is_some_and(|j| j.get("shell").is_some())
}

/// The prompt that wakes a worker after turn `turn`'s interim report, listing `ended`.
fn wake_prompt(turn: u64, ended: &[EndedTask]) -> String {
    let mut lines: Vec<String> = ended
        .iter()
        .take(TASKS_LISTED)
        .map(|t| {
            let what = if t.description.trim().is_empty() {
                "a background task ended".to_string()
            } else {
                report::display_text(&t.description, false)
            };
            match &t.job {
                Some((id, log)) if !log.is_empty() => {
                    format!("- {what} (twr job {id}; its whole log: {log})")
                }
                Some((id, _)) => format!("- {what} (twr job {id})"),
                None => format!("- {what} (ended)"),
            }
        })
        .collect();
    if ended.len() > TASKS_LISTED {
        lines.push(format!("- and {} more", ended.len() - TASKS_LISTED));
    }
    let list = if lines.is_empty() {
        String::new()
    } else {
        format!(" What ended:\n{}\n", lines.join("\n"))
    };
    format!(
        "{WAKE_PROMPT_START} after your turn {turn} ended, and no turn started after it, so twr \
         is waking you (once).{list}\nContinue where you left off: check the results and finish \
         the task. If you already handled them, say so in one line."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompt_lists_what_ended_with_job_logs() {
        let p = wake_prompt(
            3,
            &[
                EndedTask {
                    description: "job cargo test: exited 101 after 14m 12s".into(),
                    job: Some(("01J".into(), "/data/jobs/01J.log".into())),
                },
                EndedTask {
                    description: "watch the\ngates".into(),
                    job: None,
                },
            ],
        );
        assert!(p.starts_with(WAKE_PROMPT_START), "{p}");
        assert!(p.contains("after your turn 3 ended"), "{p}");
        assert!(
            p.contains(
                "- job cargo test: exited 101 after 14m 12s (twr job 01J; its whole log: \
                 /data/jobs/01J.log)"
            ),
            "{p}"
        );
        assert!(p.contains("- watch the gates (ended)"), "{p}");
        let many: Vec<EndedTask> = (0..12)
            .map(|i| EndedTask {
                description: format!("t{i}"),
                job: None,
            })
            .collect();
        let p = wake_prompt(1, &many);
        assert!(p.contains("- t9 (ended)") && !p.contains("- t10"), "{p}");
        assert!(p.contains("- and 2 more"), "{p}");
    }
}
