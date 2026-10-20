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
//! and the orchestrator in its conversation.
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
}

/// The hub's wakes (see the module docs).
#[derive(Debug, Default)]
pub(super) struct WakeState {
    /// [`super::HubOptions::wake_after`], set once the log is replayed (zero until then:
    /// replay schedules no wake); zero = never.
    pub(super) after: chrono::Duration,
    pending: HashMap<String, Pending>,
    /// The tasks that ended since each thread's last turn ended.
    ended: HashMap<String, Vec<EndedTask>>,
    /// The last turn the hub woke each thread after, or decided not to.
    woken: HashMap<String, u64>,
    /// Wakes in a row per thread, with no turn of the user's or the orchestrator's since.
    chain: HashMap<String, u32>,
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
                if v.is_orchestrator() || ev.thread == HUB_THREAD || v.current_turn.is_some() {
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
                    if self.wake.woken.get(&ev.thread) != Some(&turn) {
                        self.wake.pending.insert(
                            ev.thread.clone(),
                            Pending {
                                turn,
                                due: ev.ts + self.wake.after,
                            },
                        );
                    }
                }
            }
            // Busy again: a new wait starts when that task ends.
            EventBody::TaskUpdated(p) => {
                if self
                    .registry
                    .get(&ev.thread)
                    .is_some_and(|v| v.background_tasks.contains_key(&p.task_id))
                {
                    self.wake.pending.remove(&ev.thread);
                }
            }
            EventBody::TurnStarted(p) => {
                self.wake.pending.remove(&ev.thread);
                self.wake.ended.remove(&ev.thread);
                // A turn of the user's or the orchestrator's: someone is looking after it.
                if p.from != TurnFrom::Runtime {
                    self.wake.chain.remove(&ev.thread);
                }
            }
            EventBody::TurnEnded(_) => {
                self.wake.ended.remove(&ev.thread);
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
            EventBody::ThreadStopped(_) | EventBody::ThreadParked(_) => {
                self.wake.pending.remove(&ev.thread);
            }
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
            if let Err(e) = self.wake_thread(hub, &thread, p.turn) {
                tracing::warn!(thread = %thread, error = %format!("{e:#}"), "could not wake a thread after its background work");
            }
        }
    }

    /// Whether thread `id` can take a wake for turn `turn` now: still idle after that turn's
    /// interim report, nothing queued, nothing in the way of a turn.
    fn wake_ready(&self, id: &str, turn: u64) -> anyhow::Result<bool> {
        let Some(v) = self.registry.get(id) else {
            return Ok(false);
        };
        if interim_turn(v) != Some(turn)
            || v.turns != turn
            || v.attached.is_some()
            || v.crashed
            || v.is_blocked()
            || self.wake.woken.get(id) == Some(&turn)
            || !self.box_online(&v.box_name)
            || self
                .rt
                .get(id)
                .is_some_and(|r| r.queue_held || r.stop_requested)
        {
            return Ok(false);
        }
        Ok(self.store.pending_count(id)? == 0)
    }

    /// Wakes thread `id` after turn `turn`'s interim report, if it still needs it.
    fn wake_thread(&mut self, hub: &Arc<Inner>, id: &str, turn: u64) -> anyhow::Result<()> {
        if !self.wake_ready(id, turn)? {
            return Ok(());
        }
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
        let prompt = wake_prompt(turn, &ended);
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
