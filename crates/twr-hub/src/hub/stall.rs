//! The stall timer: a thread with an open turn or background tasks of its runtime (a job has
//! its own timeout, and its records are no activity) that is not waiting on the user (no open
//! permission / question) and has had no event for `stall_after` gets one hub-raised
//! `attention.raised{reason: stalled, summary: "No activity for N min"}` for that turn. Not
//! while one of its jobs runs (its own, not a `!` command of the user's, renewed within
//! [`JOB_ALIVE`]): its turn or its runtime's watcher waits on the job, legitimately silent;
//! once the job ends, the silence counts again.
//!
//! A hung turn: a turn open and silent for `hung_after` (`min(stall_after, 5 min)`), counted
//! from its last event or from when the oldest prompt queued for the thread arrived, whichever
//! is later, while that prompt waits (a job's end, a message): the turn should have ended and
//! did not (a runtime that wrote its reply and never closed the turn). Such a turn gets no
//! plain stalled item meanwhile; it gets a distinct one, a hung-turn card ([`HUNG_KEY`]:
//! `{turn, waiting}`), replacing an open plain one in the same batch: `"<thread>'s turn N
//! seems hung: ..."` with the options [`HUNG_INTERRUPT`] (the turn is interrupted, as
//! `message.send_now` does, and the prompt that waits runs next) and [`HUNG_WAIT`] (the card
//! closes, and no other hung-turn card is raised for that turn; plain stalls go on). Only the
//! user answers it. It notifies as `hung` (on by default, unlike `stalled`), and the
//! orchestrator gets it as an urgent notice line (`reason=hung`).
//!
//! The item resolves `by: timeout` (activity resumed) as soon as another progress event is appended
//! to that thread, including the turn's end or the runtime's closing sequence: the resolution
//! is part of the batch that records the activity ([`Core::stall_resolutions`]), so a hub that
//! dies in between cannot leave it open. Startup resolves any stalled item whose turn is no
//! longer active (logs written before that). Inventory heartbeats renew task liveness without
//! resetting stalls or repeatedly resolving and raising them. A thread has at
//! most one open stalled item. The check runs on a hub tick every `min(30 s, stall_after / 4)`;
//! `stall_after_secs = 0` turns it off.

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::{json, Value};
use twr_core::event::*;
use twr_core::proto::{codes, RpcError};
use twr_core::reduce::{AttentionItem, ThreadView};

use super::orch::cut;
use super::{
    append_err, rpc, twr_ext, twr_ext_map, Batch, BatchEvent, Caller, Core, HubState, Inner,
    ResolvedBy,
};

/// The longest a turn may be silent while a prompt waits before it is hung.
pub const HUNG_AFTER_MAX: Duration = Duration::from_secs(5 * 60);

/// How recently a job must have renewed its task to count as running (it renews every
/// [`super::jobs::PROGRESS_EVERY`]).
pub const JOB_ALIVE: chrono::Duration = chrono::Duration::minutes(3);

/// How long a turn may be silent while a prompt waits, for stall threshold `after`.
pub fn hung_after(after: Duration) -> Duration {
    after.min(HUNG_AFTER_MAX)
}

/// The hung-turn cards (see the module docs), folded from the log.
#[derive(Debug, Default)]
pub(super) struct HungState {
    /// Open cards: attn_id -> (thread, turn).
    cards: HashMap<String, (String, u64)>,
    /// The turn of each thread whose card the user answered: no other card for it.
    answered: HashMap<String, u64>,
}

/// Longest tick period.
const TICK_MAX: Duration = Duration::from_secs(30);

/// Shortest tick period.
const TICK_MIN: Duration = Duration::from_millis(10);

/// "No activity for N min" (seconds below a minute).
fn stall_summary(after: Duration) -> String {
    let secs = after.as_secs();
    if secs >= 60 {
        format!("No activity for {} min", secs / 60)
    } else {
        format!("No activity for {} s", secs.max(1))
    }
}

/// The tick period for `after`.
pub(super) fn tick_period(after: Duration) -> Duration {
    (after / 4).clamp(TICK_MIN, TICK_MAX)
}

impl Core {
    /// Raises a stalled item for every running, unblocked thread silent for `after` (a
    /// hung-turn card where a prompt waits behind its turn).
    pub(super) fn check_stalls(&mut self, hub: &Inner, after: Duration) -> anyhow::Result<()> {
        if self.shutting_down {
            return Ok(());
        }
        let (Ok(after_chrono), Ok(hung_chrono)) = (
            chrono::Duration::from_std(after),
            chrono::Duration::from_std(hung_after(after)),
        ) else {
            return Ok(());
        };
        let now = Utc::now();
        let mut hung: Vec<(String, u64, String, Option<String>)> = Vec::new();
        let mut due: Vec<(String, Option<u64>, String)> = Vec::new();
        for v in self.registry.list() {
            if v.attached.is_some() || v.is_blocked() || v.last_activity.is_none() {
                continue;
            }
            if let Some((turn, waiting)) = self.hung_turn(v, now, hung_chrono)? {
                let plain = self
                    .stalled
                    .get(&v.id)
                    .filter(|a| !self.hung.cards.contains_key(*a))
                    .cloned();
                hung.push((v.id.clone(), turn, waiting, plain));
                continue;
            }
            // A prompt waits behind its open turn: the hung-turn check decides, within
            // `hung_after` of its arrival.
            // (Not a turn being interrupted or taken back: no hung-turn card comes for it.)
            let behind = v
                .current_turn
                .is_some_and(|t| self.hung.answered.get(&v.id) != Some(&t))
                && !v.current_turn_interrupted
                && !v.current_turn_withdrawn
                && self.store.pending_count(&v.id)? > 0;
            if behind
                || self.stalled.contains_key(&v.id)
                || !v.last_activity.is_some_and(|t| now - t >= after_chrono)
                // A job has its own timeout: only the runtime's own work stalls.
                || !v.runtime_work()
                // Its turn or its runtime's watcher waits on a job of its own.
                || job_running(v, now)
            {
                continue;
            }
            let summary = if v.current_turn.is_some() {
                stall_summary(after)
            } else {
                format!("{} from its background tasks", stall_summary(after))
            };
            due.push((v.id.clone(), v.current_turn, summary));
        }
        // One independent item per thread: a failure part-way leaves the rest for the next
        // tick.
        for (thread, turn, waiting, plain) in hung {
            let name = self.thread_name(&thread);
            let silent = hung_after(after).as_secs() / 60;
            let summary = format!(
                "{name}'s turn {turn} seems hung: no output from its runtime for {} while {waiting} \
                 waits to start the next turn. Interrupt it to run that now, or wait.",
                if silent >= 1 {
                    format!("{silent} min")
                } else {
                    format!("{} s", hung_after(after).as_secs().max(1))
                }
            );
            let mut batch = Batch::default();
            if let Some(attn_id) = plain {
                batch.push(&thread, Some(turn), Source::Hub, resolution(&attn_id));
            }
            batch.push(
                &thread,
                Some(turn),
                Source::Hub,
                EventBody::AttentionRaised(AttentionRaised {
                    attn_id: new_id(),
                    reason: AttentionReason::Stalled,
                    summary,
                    options: vec![HUNG_INTERRUPT.into(), HUNG_WAIT.into()],
                    ext: twr_ext_map(json!({ HUNG_KEY: {"turn": turn, "waiting": waiting} })),
                    ..Default::default()
                }),
            );
            self.append_batch(hub, batch)?;
        }
        for (thread, turn, summary) in due {
            self.append(
                hub,
                &thread,
                turn,
                Source::Hub,
                EventBody::AttentionRaised(AttentionRaised {
                    attn_id: new_id(),
                    reason: AttentionReason::Stalled,
                    summary,
                    options: vec!["dismiss".into()],
                    ..Default::default()
                }),
            )?;
        }
        Ok(())
    }

    /// The open turn of `v` and what waits behind it, when it is hung at `now`: silent for
    /// `hung_after` since its last event and since the oldest prompt queued for it arrived,
    /// with no hung-turn card open or answered for that turn.
    fn hung_turn(
        &self,
        v: &ThreadView,
        now: DateTime<Utc>,
        hung_after: chrono::Duration,
    ) -> anyhow::Result<Option<(u64, String)>> {
        let Some(turn) = v.current_turn else {
            return Ok(None);
        };
        if v.current_turn_interrupted
            || v.current_turn_withdrawn
            || self.hung.answered.get(&v.id) == Some(&turn)
            || self
                .stalled
                .get(&v.id)
                .is_some_and(|a| self.hung.cards.contains_key(a))
            || v.last_activity.is_none_or(|t| now - t < hung_after)
        {
            return Ok(None);
        }
        let Some(waiting) = self.store.pending_list(&v.id, 1)?.into_iter().next() else {
            return Ok(None);
        };
        let queued_at = DateTime::parse_from_rfc3339(&waiting.queued_at)
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or(now);
        if now - queued_at < hung_after {
            return Ok(None);
        }
        let what = match waiting.from {
            TurnFrom::User => "a message of yours".to_string(),
            TurnFrom::Orchestrator => "a message from the orchestrator".to_string(),
            TurnFrom::Runtime => {
                let first = waiting.prompt.lines().next().unwrap_or_default();
                match first.strip_prefix("[twr job ") {
                    Some(rest) => format!(
                        "the end of job {}",
                        cut(&crate::report::display_text(rest, false), 160)
                    ),
                    None => format!(
                        "a prompt of the hub's ({})",
                        cut(&crate::report::display_text(first, false), 160)
                    ),
                }
            }
        };
        Ok(Some((turn, what)))
    }

    /// Bookkeeping of open stalled items (live and on replay).
    pub(super) fn track_stall(&mut self, ev: &Event) {
        match &ev.body {
            EventBody::AttentionRaised(p)
                if p.reason == AttentionReason::Stalled
                    && ev.source == Source::Hub
                    && ev.thread != HUB_THREAD =>
            {
                self.stalled.insert(ev.thread.clone(), p.attn_id.clone());
            }
            EventBody::AttentionResolved(p) if self.stalled.get(&ev.thread) == Some(&p.attn_id) => {
                self.stalled.remove(&ev.thread);
            }
            _ => {}
        }
        match &ev.body {
            EventBody::AttentionRaised(p)
                if p.reason == AttentionReason::Stalled
                    && ev.source == Source::Hub
                    && is_hung_card(&p.ext) =>
            {
                let turn = twr_ext(&p.ext)
                    .and_then(|t| t.get(HUNG_KEY))
                    .and_then(|h| h.get("turn"))
                    .and_then(Value::as_u64)
                    .or(ev.turn)
                    .unwrap_or_default();
                self.hung
                    .cards
                    .insert(p.attn_id.clone(), (ev.thread.clone(), turn));
            }
            EventBody::AttentionResolved(p) => {
                if let Some((thread, turn)) = self.hung.cards.remove(&p.attn_id) {
                    if p.by == ResolvedBy::User {
                        self.hung.answered.insert(thread, turn);
                    }
                }
            }
            EventBody::ThreadDeleted(_) => {
                self.hung.answered.remove(&ev.thread);
            }
            _ => {}
        }
    }

    /// Whether `attn_id` is an open hung-turn card.
    pub(super) fn hung_card(&self, attn_id: &str) -> bool {
        self.hung.cards.contains_key(attn_id)
    }

    /// Answers hung-turn card `item` (see the module docs) with `decision` (`None`: a
    /// dismissal, as [`HUNG_WAIT`]). Users only.
    pub(super) fn answer_hung_card(
        &mut self,
        hub: &Arc<Inner>,
        item: &AttentionItem,
        decision: Option<&str>,
        caller: &Caller,
    ) -> Result<Event, RpcError> {
        if caller.is_orchestrator() {
            return Err(rpc(
                codes::RELAY_FORBIDDEN,
                format!(
                    "{} is a hung-turn card: only the user answers it; tell the user",
                    item.attn_id
                ),
            ));
        }
        let Some((thread, turn)) = self.hung.cards.get(&item.attn_id).cloned() else {
            return Err(rpc(codes::NOT_FOUND, "no such hung-turn card"));
        };
        let decision = decision.unwrap_or(HUNG_WAIT).trim().to_ascii_lowercase();
        let interrupt = match decision.as_str() {
            HUNG_INTERRUPT => true,
            HUNG_WAIT | "dismiss" => false,
            other => {
                return Err(rpc(
                    codes::BAD_REQUEST,
                    format!(
                        "{other:?} is no option of {}: {HUNG_INTERRUPT} or {HUNG_WAIT}",
                        item.attn_id
                    ),
                ))
            }
        };
        let view = self.view(&thread)?.clone();
        let head = self
            .store
            .pending_list(&thread, 1)
            .map_err(super::internal)?
            .into_iter()
            .next();
        // Still the turn the card is about, with something waiting: it is interrupted.
        let interrupting = interrupt && view.current_turn == Some(turn) && head.is_some();
        if interrupting
            && !view.current_turn_interrupted
            && !self.turn_interruptible(hub, &thread, &view)
        {
            return Err(rpc(
                codes::CONFLICT,
                format!(
                    "{}'s runtime cannot interrupt a turn: `twr stop {}` stops it",
                    view.name, view.name
                ),
            ));
        }
        let mut batch = Batch::default();
        let answered = batch.push(
            &thread,
            view.current_turn,
            caller.source(),
            EventBody::AttentionResolved(AttentionResolved {
                attn_id: item.attn_id.clone(),
                by: ResolvedBy::User,
                answer: Some(twr_core::adapter::Answer::new(if interrupt {
                    HUNG_INTERRUPT
                } else {
                    HUNG_WAIT
                })),
                ..Default::default()
            }),
        );
        let done = self.append_batch(hub, batch).map_err(append_err)?;
        let ev = done
            .events
            .get(answered)
            .cloned()
            .ok_or_else(|| super::internal("the answer was not recorded"))?;
        if let (true, Some(head)) = (interrupting, head) {
            self.send_now_locked(hub, &thread, head.id, caller)?;
        }
        Ok(ev)
    }

    /// The resolutions (`by: timeout`) a batch of `events` implies: activity on a thread with
    /// an open stalled item resolves it, after the batch's own events. The item's own events do
    /// not count, and a batch that resolves the item itself needs none.
    pub(super) fn stall_resolutions(&self, events: &[BatchEvent]) -> Vec<BatchEvent> {
        let mut out: Vec<BatchEvent> = Vec::new();
        for ev in events {
            if is_job_task(&ev.source, &ev.body) {
                continue;
            }
            if matches!(
                ev.body,
                EventBody::ConversationUpdate(_)
                    | EventBody::WorktreeDisk(_)
                    | EventBody::WorktreeTrimmed(_)
                    | EventBody::TaskUpdated(TaskUpdated {
                        heartbeat: true,
                        ..
                    })
            ) {
                continue;
            }
            let Some(attn_id) = self.stalled.get(&ev.thread) else {
                continue;
            };
            if out.iter().any(|r| r.thread == ev.thread) || is_own(&ev.body, attn_id) {
                continue;
            }
            let resolved_here = events.iter().any(|e| {
                e.thread == ev.thread
                    && matches!(&e.body, EventBody::AttentionResolved(p) if p.attn_id == *attn_id)
            });
            if resolved_here {
                continue;
            }
            let open = self.registry.get(&ev.thread).and_then(|v| v.current_turn);
            out.push(BatchEvent {
                thread: ev.thread.clone(),
                turn: ev.turn.or(open),
                source: Source::Hub,
                body: resolution(attn_id),
                raw: None,
            });
        }
        out
    }

    /// Startup: resolves the stalled items of threads with no remaining work (a log from
    /// before the resolution was part of the activity's batch). Each is its own append.
    pub(super) fn repair_stalls(&mut self, hub: &Inner) -> anyhow::Result<()> {
        let stale: Vec<(String, String)> = self
            .stalled
            .iter()
            .filter(|(t, _)| {
                self.registry
                    .get(t.as_str())
                    .is_none_or(|v| !v.runtime_work())
            })
            .map(|(t, a)| (t.clone(), a.clone()))
            .collect();
        for (thread, attn_id) in stale {
            let turn = self
                .registry
                .get(&thread)
                .and_then(|v| v.last_turn.as_ref())
                .map(|t| t.turn);
            self.append(hub, &thread, turn, Source::Hub, resolution(&attn_id))?;
        }
        Ok(())
    }
}

/// Whether a job of `v`'s own (not a `!` command of the user's) runs at `now`: renewed within
/// [`JOB_ALIVE`].
fn job_running(v: &ThreadView, now: DateTime<Utc>) -> bool {
    v.background_tasks
        .values()
        .any(|t| t.job && !t.shell && now - t.updated_at <= JOB_ALIVE)
}

/// Whether `body` is one of the stalled item `attn_id`'s own events.
fn is_own(body: &EventBody, attn_id: &str) -> bool {
    match body {
        EventBody::AttentionRaised(p) => p.attn_id == attn_id,
        EventBody::AttentionAnnotated(p) => p.attn_id == attn_id,
        EventBody::AttentionResolved(p) => p.attn_id == attn_id,
        _ => false,
    }
}

fn resolution(attn_id: &str) -> EventBody {
    EventBody::AttentionResolved(AttentionResolved {
        attn_id: attn_id.to_string(),
        by: ResolvedBy::Timeout,
        answer: None,
        ..Default::default()
    })
}

/// The stall tick task: checks every [`tick_period`] until the hub stops or is dropped.
pub(super) async fn stall_ticker(hub: Weak<Inner>, after: Duration) {
    let mut tick = tokio::time::interval(tick_period(after));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let Some(h) = hub.upgrade() else {
            return;
        };
        if *h.state.borrow() == HubState::Stopped {
            return;
        }
        let r = h.core().check_stalls(&h, after);
        if let Err(e) = r {
            tracing::warn!(error = %format!("{e:#}"), "stall check failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summaries_and_ticks() {
        assert_eq!(
            stall_summary(Duration::from_secs(600)),
            "No activity for 10 min"
        );
        assert_eq!(
            stall_summary(Duration::from_millis(300)),
            "No activity for 1 s"
        );
        assert_eq!(tick_period(Duration::from_secs(600)), TICK_MAX);
        assert_eq!(
            tick_period(Duration::from_millis(400)),
            Duration::from_millis(100)
        );
        assert_eq!(tick_period(Duration::from_millis(1)), TICK_MIN);
    }
}
