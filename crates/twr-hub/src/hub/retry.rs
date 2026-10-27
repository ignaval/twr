//! Failed worker turns: the block_kind decision and the one automatic retry.
//!
//! When a worker turn ends `error`, or its runtime crashes while a turn is open, the hub runs
//! the block_kind ladder (code: an adapter's `ext.<runtime>.transient` flag → `transient`;
//! default `user_blocked`) and
//! logs it as `decision.made{site: block_kind}` for that turn. A turn that carried the resume
//! of a session the thread moved back to and failed before the runtime started it
//! (`ThreadView::resume_failed_turn`, `hub::switch`) counts as flagged transient: its retry
//! starts a new session instead (unless the adapter said it is not signed in).
//!
//! - `transient`, and the thread is [`ThreadView::retry_eligible`] (the failed turn is not
//!   itself a retry and no retry of it started): the hub starts one retry turn with
//!   [`RETRY_PROMPT`] (the failed turn's own prompt, as given, when its resume failed: no
//!   session got it), `source: hub`, the failed turn's `from`, and
//!   `turn.started.ext.twr.retry_of = <failed turn>`. Starting it resolves the hub's open
//!   `error` item for the failed turn (a crash raises one, but not for a turn whose resume
//!   failed) `by: auto_retry`. A failed turn gets no `error` item while its retry is pending.
//! - Anything else (`user_blocked`, or a retry that failed too): the hub raises an `error`
//!   attention for the turn (a crash already raised one), which stays for the user.
//! - `transient`, and the thread was stopped before its retry started (by the user while its
//!   runtime was being replaced, or by a hub that died meanwhile: a restart's reason is in
//!   its memory only, so the next hub records the stop as anyone's): no retry (a stopped
//!   thread is not eligible), and the `error` item is raised when the stop is recorded or at
//!   the next start, saying the turn was not retried.
//!
//! A failed worker turn gets this `error` item instead of the `review` its `gave_up` grade
//! would raise (one item per failure); its grade and report are logged as for any turn.
//!
//! Everything is derived from the log, so it survives a hub restart: at startup (and whenever
//! the thread frees up, see [`Core::try_dispatch`]) a failed turn whose block kind is
//! `transient`, that is still eligible, gets its retry. The retry's `turn.started` is what
//! makes the turn ineligible, so the retry starts exactly once. A missing block_kind decision
//! (the hub died between the failure and the decision) is made at startup.
//!
//! Crashes: queued prompts are not dispatched after a crash, to avoid crash
//! loops. The automatic retry is the one exception: it restarts (resumes) the runtime once for
//! the failed turn; if the retry fails too, nothing else is dispatched until the user acts.
//! A worker's `thread.crashed` holds its queue (`ThreadRt::queue_held`, folded from the log, so
//! a hub restart keeps it): every ordinary dispatch (the background ticker, a runner's
//! reconnection, a detach, ...) checks it, a due retry aside, and only the user's next prompt
//! (`thread.send`, `say`), resume (`thread.resume`) or a runtime that starts again releases it:
//! not a prompt anything else queues (a job's wake, the orchestrator's send or resume,
//! `Batch::user_send`).

use serde_json::{json, Map, Value};
use twr_core::event::*;
use twr_core::reduce::ThreadView;
use twr_decisions::{BlockContext, BlockKind, DecisionContext, LadderRun};

use super::append::hydrated_from_store;
use super::reactions::{decide, site_options};
use super::{twr_ext_map, AfterReap, Arc, Batch, Core, Inner, ResolvedBy};
use crate::store;

/// The prompt of an automatic retry turn.
pub const RETRY_PROMPT: &str =
    "Continue where you left off; the previous attempt failed with a transient error.";

/// Longest failure detail quoted in an error item's summary.
const DETAIL_MAX: usize = 200;

/// The block kind a ladder run settled on and its `decision.made` records: one per rung that
/// answered, plus the default `user_blocked` when no rung decided.
fn block_kind_records(run: LadderRun) -> (BlockKind, Vec<DecisionMade>) {
    let kind = run.block_kind();
    let decided = run.decision().is_some();
    let mut records: Vec<DecisionMade> = run.records.into_iter().map(DecisionMade::from).collect();
    if !decided {
        records.push(DecisionMade {
            site: DecisionSite::BlockKind,
            provider: DecisionProvider::Code,
            options: site_options(DecisionSite::BlockKind),
            chosen: kind.as_str().into(),
            // Not a finding, the site default: nothing said the failure was transient.
            confidence: 0.5,
            latency_ms: 0,
            ..Default::default()
        });
    }
    (kind, records)
}

/// Whether `view` is a worker thread the retry logic applies to.
fn is_worker(view: &ThreadView) -> bool {
    view.id != HUB_THREAD && !view.is_orchestrator()
}

/// A short reason from a failed `turn.ended`: `ext.twr.error`, else any runtime section's
/// `message` / `error`.
pub(super) fn failure_detail(ended: &TurnEnded) -> Option<String> {
    let ext = ended.ext.as_ref()?;
    let pick = |v: &Value| {
        ["error", "message"]
            .iter()
            .find_map(|k| v.get(*k).and_then(Value::as_str).map(str::to_string))
    };
    let mut detail = ext.get("twr").and_then(pick);
    if detail.is_none() {
        detail = ext
            .iter()
            .filter(|(k, _)| *k != "twr")
            .find_map(|(_, v)| pick(v));
    }
    let d = detail?.split_whitespace().collect::<Vec<_>>().join(" ");
    if d.is_empty() {
        return None;
    }
    Some(if d.chars().count() > DETAIL_MAX {
        format!("{}...", d.chars().take(DETAIL_MAX - 3).collect::<String>())
    } else {
        d
    })
}

impl Core {
    /// The event of `kind` of `thread` in `turn` (any turn when `None`), hydrated.
    pub(super) fn stored_event(
        &self,
        thread: &str,
        turn: Option<u64>,
        kind: &str,
    ) -> anyhow::Result<Option<Event>> {
        let ev = store::last_of_kind(self.store.conn(), thread, turn, kind)?;
        Ok(ev.map(|ev| hydrated_from_store(self.store.conn(), &ev).unwrap_or(ev)))
    }

    /// Classifies the thread's failed turn (once; see the module docs) and raises its `error`
    /// item unless a retry is due. Idempotent: every step checks the log for what is already
    /// there. Run when a turn fails, at startup, and when a thread is stopped
    /// ([`Core::react_to_stop`]).
    pub(super) fn react_to_failure(&mut self, hub: &Inner, thread: &str) -> anyhow::Result<()> {
        let Some(view) = self.registry.get(thread).cloned() else {
            return Ok(());
        };
        if !is_worker(&view) || view.parked {
            return Ok(());
        }
        let Some(failed) = view.failed_turn() else {
            return Ok(());
        };
        let done = store::turn_reactions(self.store.conn(), thread, failed)?;
        // A stopped thread: its failure was reacted to before the stop, except a transient one
        // whose retry had not started (its runtime was being replaced, say): the stop
        // forestalls the retry (`ThreadView::retry_eligible`), so it gets its item now, or
        // nobody learns its turn's prompt was not delivered. Nothing else is decided for it.
        let forestalled = view.stopped
            && done.block_kind.as_deref().and_then(BlockKind::parse) == Some(BlockKind::Transient)
            && !view.turn_retry_of.contains_key(&failed)
            && !view.turn_retry_of.values().any(|&orig| orig == failed);
        if view.stopped && !forestalled {
            return Ok(());
        }
        let ended = match self.stored_event(thread, Some(failed), "turn.ended")? {
            Some(Event {
                body: EventBody::TurnEnded(p),
                ..
            }) => Some(p),
            _ => None,
        };
        // The block_kind decision and the error item are separate appends: the logged
        // decision is authoritative (never re-decided), and the missing item is raised from it
        // on the next run (startup runs this for every worker).
        let kind = match done.block_kind.as_deref().and_then(BlockKind::parse) {
            Some(k) => k,
            None => self.decide_block_kind(hub, thread, failed, &view, ended.as_ref())?,
        };
        if kind == BlockKind::Transient && view.retry_eligible() {
            // The retry starts when the thread is free (Core::start_retry).
            return Ok(());
        }
        if done.errored {
            return Ok(());
        }
        let retried = view.turn_retry_of.contains_key(&failed);
        let detail = ended.as_ref().and_then(failure_detail);
        let what = if retried {
            "failed again after an automatic retry"
        } else if forestalled {
            "failed, and was not retried: the thread was stopped"
        } else {
            "failed"
        };
        let summary = match detail {
            Some(d) => format!("{}: turn {failed} {what}: {d}", view.name),
            None => format!("{}: turn {failed} {what}", view.name),
        };
        self.append(
            hub,
            thread,
            Some(failed),
            Source::Hub,
            EventBody::AttentionRaised(AttentionRaised {
                attn_id: new_id(),
                reason: AttentionReason::Error,
                summary,
                options: vec!["dismiss".into()],
                ext: twr_ext_map(json!({ "block_kind": kind.as_str(), "retried": retried })),
                ..Default::default()
            }),
        )?;
        Ok(())
    }

    /// Runs the block_kind ladder for turn `failed` and logs its records (plus the default
    /// `user_blocked` when no rung decided).
    fn decide_block_kind(
        &mut self,
        hub: &Inner,
        thread: &str,
        failed: u64,
        view: &ThreadView,
        ended: Option<&TurnEnded>,
    ) -> anyhow::Result<BlockKind> {
        let exit_code = if view.crashed {
            match self.stored_event(thread, None, "thread.crashed")? {
                Some(Event {
                    body: EventBody::ThreadCrashed(c),
                    ..
                }) => c.exit_code,
                _ => None,
            }
        } else {
            None
        };
        let ctx = DecisionContext::BlockKind(BlockContext {
            result: ended.map(|e| e.result),
            crashed: view.crashed,
            exit_code,
            // A resume that failed is retried once, on a new session (`hub::switch`).
            transient: view.resume_failed_turn == Some(failed)
                || twr_decisions::context::transient_flag(ended.and_then(|e| e.ext.as_ref())),
            auth_failed: twr_decisions::context::auth_failed_flag(
                ended.and_then(|e| e.ext.as_ref()),
            ),
        });
        let (kind, records) = block_kind_records(decide(hub, &ctx));
        let mut batch = Batch::default();
        for d in records {
            let source = Source::Decision(d.provider.as_str().into());
            batch.push(thread, Some(failed), source, EventBody::DecisionMade(d));
        }
        self.append_batch(hub, batch)?;
        Ok(kind)
    }

    /// The failed turn of `thread` whose automatic retry is due: eligible and classified
    /// `transient`.
    pub(super) fn due_retry(&self, thread: &str) -> anyhow::Result<Option<u64>> {
        let Some(view) = self.registry.get(thread) else {
            return Ok(None);
        };
        if !is_worker(view) || !view.retry_eligible() {
            return Ok(None);
        }
        let Some(failed) = view.failed_turn() else {
            return Ok(None);
        };
        let done = store::turn_reactions(self.store.conn(), thread, failed)?;
        let transient =
            done.block_kind.as_deref().and_then(BlockKind::parse) == Some(BlockKind::Transient);
        Ok(transient.then_some(failed))
    }

    /// Starts the due retry of `thread`, if any and if the thread can take a turn
    /// ([`Core::thread_free`], checked here too): resolves the hub's open `error` items for
    /// the failed turn `by: auto_retry`, then starts the retry turn. Returns its turn number.
    pub(super) fn start_retry(
        &mut self,
        hub: &Arc<Inner>,
        thread: &str,
    ) -> anyhow::Result<Option<u64>> {
        if !self.thread_free(thread) {
            self.hold_for_reap(thread, AfterReap::Retry);
            return Ok(None);
        }
        let Some(failed) = self.due_retry(thread)? else {
            return Ok(None);
        };
        // Resolving the failed turn's error items and starting its retry are one batch: the
        // item never closes without the retry that replaces it.
        let mut batch = Batch::default();
        let items: Vec<String> = self
            .hub_errors
            .iter()
            .filter(|(_, (t, turn))| t == thread && *turn == Some(failed))
            .map(|(a, _)| a.clone())
            .collect();
        for attn_id in items {
            batch.push(
                thread,
                Some(failed),
                Source::Hub,
                EventBody::AttentionResolved(AttentionResolved {
                    attn_id,
                    by: ResolvedBy::AutoRetry,
                    answer: None,
                    ..Default::default()
                }),
            );
        }
        let original = match self.stored_event(thread, Some(failed), "turn.started")? {
            Some(Event {
                body: EventBody::TurnStarted(p),
                ..
            }) => Some(p),
            _ => None,
        };
        let from = original.as_ref().map_or(TurnFrom::User, |p| p.from);
        // A turn whose resume failed reached no session: its retry, the new session's first
        // turn, delivers its message whole (after the full handoff, `hub::switch`). Any other
        // retry asks the session that got it to go on.
        let resume_failed = self
            .registry
            .get(thread)
            .is_some_and(|v| v.resume_failed_turn == Some(failed));
        let prompt = match original {
            Some(p) if resume_failed => p.prompt,
            _ => RETRY_PROMPT.to_string(),
        };
        let mut twr = Map::new();
        twr.insert("retry_of".into(), json!(failed));
        tracing::info!(
            thread,
            failed,
            "retrying a turn that failed with a transient error"
        );
        let turn = self.start_turn(hub, thread, prompt, from, Source::Hub, None, twr, batch)?;
        Ok(Some(turn))
    }

    /// A stopped runtime's closing sequence ran: a thread it left stopped whose transient
    /// failure was still to be retried gets its `error` item ([`Core::react_to_failure`]).
    pub(super) fn react_to_stop(&mut self, hub: &Inner, thread: &str) -> anyhow::Result<()> {
        if !self.registry.get(thread).is_some_and(|v| v.stopped) {
            return Ok(());
        }
        self.react_to_failure(hub, thread)
    }

    /// A crashed runtime's closing sequence ran: classify the crash and retry if it is due
    /// (the one exception to holding the queue after a crash, see the module docs).
    pub(super) fn react_to_crash(&mut self, hub: &Arc<Inner>, thread: &str) -> anyhow::Result<()> {
        self.react_to_failure(hub, thread)?;
        // (A busy thread: `start_retry` returns at once, noting a retry held back by a kill.)
        self.start_retry(hub, thread)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_details_come_from_ext() {
        let ended = |ext: Value| TurnEnded {
            ext: ext.as_object().cloned(),
            ..Default::default()
        };
        assert_eq!(
            failure_detail(&ended(json!({"twr": {"error": "spawn failed"}}))),
            Some("spawn failed".into())
        );
        assert_eq!(
            failure_detail(&ended(json!({"fake": {"message": "rate\n limited"}}))),
            Some("rate limited".into())
        );
        assert_eq!(failure_detail(&ended(json!({"fake": {}}))), None);
        assert_eq!(failure_detail(&TurnEnded::default()), None);
        let long = failure_detail(&ended(json!({"x": {"error": "e".repeat(500)}}))).unwrap();
        assert_eq!(long.chars().count(), DETAIL_MAX);
    }
}
