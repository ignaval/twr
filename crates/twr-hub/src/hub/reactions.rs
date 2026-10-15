//! Turn-end reactions: what the hub does once a turn has ended (a worker turn's grade,
//! fallback report and `review` item; a failed turn's block_kind decision and retry, see
//! `hub::retry`; a failed orchestrator turn's `error` item), and their recovery at startup for
//! turns whose reactions a previous hub did not record.

use std::sync::Arc;

use serde_json::{json, Value};
use twr_core::event::*;
use twr_decisions::{DecisionContext, GradeContext, LadderRun};

use super::append::hydrated_from_store;
use super::{retry, twr_ext_map, Batch, Core, EndedTurn, Inner};
use crate::report;
use crate::store;

impl Core {
    /// Ends turn `turn` with `error` because its runtime could not take it, then runs the
    /// usual turn-end reactions (a worker's grade, report and review attention; an
    /// orchestrator's `error` item).
    pub(super) fn fail_turn(
        &mut self,
        hub: &Inner,
        thread: &str,
        turn: u64,
        session_id: Option<String>,
        e: &anyhow::Error,
    ) -> anyhow::Result<()> {
        self.fail_turn_with(hub, thread, turn, session_id, e, Batch::default())
    }

    /// [`Core::fail_turn`], the `turn.ended` recorded in one batch with `batch` (what must
    /// not outlive or predate it, e.g. the expired remote command it replaces).
    fn fail_turn_with(
        &mut self,
        hub: &Inner,
        thread: &str,
        turn: u64,
        session_id: Option<String>,
        e: &anyhow::Error,
        batch: Batch,
    ) -> anyhow::Result<()> {
        // Three steps, each safe to stop after: the turn's end is the fact, and its reactions
        // (grade/report/review, then block_kind/error item) are derived from the log and
        // completed at startup (`recover_turn_reactions`, `react_to_failure`) if the hub dies
        // in between.
        self.end_failed_turn(hub, thread, turn, session_id, e, batch)?;
        self.react_to_failed_turn(hub, thread, turn)
    }

    /// The first step of [`Core::fail_turn_with`]: records turn `turn`'s `turn.ended` `error`
    /// (`e` as `ext.twr.error`) in one batch with `batch`.
    pub(super) fn end_failed_turn(
        &mut self,
        hub: &Inner,
        thread: &str,
        turn: u64,
        session_id: Option<String>,
        e: &anyhow::Error,
        mut batch: Batch,
    ) -> anyhow::Result<()> {
        tracing::error!(thread, turn, error = %e, "delivering a turn failed");
        let ended = EventBody::TurnEnded(TurnEnded {
            result: TurnResult::Error,
            session_id,
            ext: twr_ext_map(json!({ "error": format!("{e:#}") })),
            ..Default::default()
        });
        batch.push(thread, Some(turn), Source::Hub, ended);
        self.append_batch(hub, batch)?;
        Ok(())
    }

    /// The reactions of [`Core::fail_turn_with`] once the failed turn's end is recorded.
    pub(super) fn react_to_failed_turn(
        &mut self,
        hub: &Inner,
        thread: &str,
        turn: u64,
    ) -> anyhow::Result<()> {
        self.after_turn_ended(hub, thread, turn, TurnResult::Error)?;
        self.react_to_failure(hub, thread)
    }

    /// Hub reaction to a finished turn: grade, fallback report, review attention (for an
    /// orchestrator turn, its `error` item when it failed).
    fn after_turn_ended(
        &mut self,
        hub: &Inner,
        thread: &str,
        turn: u64,
        result: TurnResult,
    ) -> anyhow::Result<()> {
        let final_text = self.rt.get_mut(thread).and_then(|r| r.final_text.take());
        self.react_to_turn_end(hub, thread, turn, result, final_text)
    }

    /// Grade, fallback report and review attention for turn `turn` that ended `result`, with
    /// the turn's full final text (the report's `ext.twr.reply_seq` is the seq of that reply,
    /// the turn's last final `agent.text`), recorded as one batch ([`Core::append_batch`]). Idempotent:
    /// only what the log lacks for this turn is appended, so startup recovery can re-run it
    /// ([`Core::recover_turn_reactions`]). A report already logged for the turn is
    /// authoritative: its grade and writer decide the review (the latest report counts)
    /// and nothing is recomputed. Interrupted turns get nothing.
    /// An orchestrator turn gets [`Core::react_to_orchestrator_turn_end`] instead.
    fn react_to_turn_end(
        &mut self,
        hub: &Inner,
        thread: &str,
        turn: u64,
        result: TurnResult,
        final_text: Option<String>,
    ) -> anyhow::Result<()> {
        if result == TurnResult::Interrupted {
            return Ok(());
        }
        let done = store::turn_reactions(self.store.conn(), thread, turn)?;
        let view = self.view(thread).map_err(anyhow::Error::new)?;
        if view.is_orchestrator() {
            return self.react_to_orchestrator_turn_end(hub, thread, turn, result, &done);
        }
        let background_starts: serde_json::Map<String, Value> = view
            .background_tasks
            .iter()
            .map(|(id, t)| (id.clone(), json!(t.started_seq)))
            .collect();
        let background_count = background_starts.len();
        let name = view.name.clone();
        let session_id = view.session_id.clone();
        let mut batch = Batch::default();
        let report = match self.logged_report(thread, turn)? {
            Some(r) => r,
            None => {
                let grade = match done.grade {
                    // Graded but not reported (a log from before the reactions were one
                    // batch): the logged grade counts.
                    Some(g) => g,
                    None => self.grade_turn(hub, &mut batch, thread, turn, result, &final_text)?,
                };
                // The reply the summary quotes, for `read_transcript seq=<reply_seq>`.
                let reply_seq = match final_text {
                    Some(_) => {
                        store::last_final_text(self.store.conn(), thread, turn)?.map(|ev| ev.seq)
                    }
                    None => None,
                };
                let follow_up = self.is_follow_up_turn(thread, turn)?;
                let report = ReportReady {
                    summary: report::summary(final_text.as_deref(), result, turn, background_count),
                    grade,
                    turn: Some(turn),
                    session_id,
                    written_by: "hub".into(),
                    ext: {
                        let mut detail = json!({});
                        if background_count > 0 {
                            detail[report::BACKGROUND_TASKS_KEY] = json!(background_count);
                            detail[BACKGROUND_TASK_STARTS] = Value::Object(background_starts);
                        }
                        if follow_up {
                            detail[report::TURN_FROM_KEY] = json!("runtime");
                        }
                        if let Some(seq) = reply_seq {
                            detail[report::REPLY_SEQ_KEY] = json!(seq);
                        }
                        if let Some(text) = final_text.as_deref() {
                            let clean = report::display_text(text, true);
                            detail["reply_excerpt"] = json!(super::orch::cut(&clean, 3072));
                            detail["reply_truncated"] = json!(clean.len() > 3072);
                        }
                        if detail.as_object().is_some_and(|o| o.is_empty()) {
                            None
                        } else {
                            twr_ext_map(detail)
                        }
                    },
                    ..Default::default()
                };
                batch.push(
                    thread,
                    Some(turn),
                    Source::Hub,
                    EventBody::ReportReady(report.clone()),
                );
                report
            }
        };
        if background_count == 0
            && !is_interim(&report)
            && !done.reviewed
            && review_due(Some(result), report.grade, &done)
        {
            batch.push(
                thread,
                Some(turn),
                Source::Hub,
                EventBody::AttentionRaised(review_item(&name, turn, &report)),
            );
        }
        if !batch.is_empty() {
            self.append_batch(hub, batch)?;
        }
        Ok(())
    }

    /// The hub's reaction to orchestrator turn `turn` that ended `result`. The orchestrator's
    /// turns get no grade, report or review (its replies go to the user, who talks to it
    /// directly; a report would only point back at it). A failed one raises an `error` item
    /// quoting the failure (`turn.ended` `ext.twr.error`, e.g. a runtime that cannot start),
    /// unless the log has one for the turn. The item replaces the open hub `error` items of the
    /// thread's earlier turns (resolved `by: timeout`, `ext.twr.superseded_by`, in the same
    /// batch), so an orchestrator failing again (the next notice, each hub restart) leaves one
    /// item, its latest failure. Derived from the log only: startup recovery runs it for every
    /// failed orchestrator turn without its item, oldest first
    /// ([`Core::recover_turn_reactions`]), and ends the same way.
    fn react_to_orchestrator_turn_end(
        &mut self,
        hub: &Inner,
        thread: &str,
        turn: u64,
        result: TurnResult,
        done: &store::TurnReactions,
    ) -> anyhow::Result<()> {
        if result != TurnResult::Error || done.errored {
            return Ok(());
        }
        let name = self.view(thread).map_err(anyhow::Error::new)?.name.clone();
        let detail = match self.stored_event(thread, Some(turn), "turn.ended")? {
            Some(Event {
                body: EventBody::TurnEnded(p),
                ..
            }) => retry::failure_detail(&p),
            _ => None,
        };
        let summary = match detail {
            Some(d) => format!("{name}: turn {turn} failed: {d}"),
            None => format!("{name}: turn {turn} failed"),
        };
        let attn_id = new_id();
        let mut older: Vec<String> = self
            .hub_errors
            .iter()
            .filter(|(_, (t, n))| t == thread && n.is_some_and(|n| n < turn))
            .map(|(a, _)| a.clone())
            .collect();
        older.sort();
        let mut batch = Batch::default();
        for old in older {
            batch.push(
                thread,
                Some(turn),
                Source::Hub,
                EventBody::AttentionResolved(AttentionResolved {
                    attn_id: old,
                    by: ResolvedBy::Timeout,
                    answer: None,
                    ext: twr_ext_map(json!({ "superseded_by": attn_id })),
                    ..Default::default()
                }),
            );
        }
        batch.push(
            thread,
            Some(turn),
            Source::Hub,
            EventBody::AttentionRaised(AttentionRaised {
                attn_id,
                reason: AttentionReason::Error,
                summary,
                options: vec!["dismiss".into()],
                ..Default::default()
            }),
        );
        self.append_batch(hub, batch)?;
        Ok(())
    }

    /// Runs the grading ladder for turn `turn` and adds its records to `batch` (plus the code
    /// default when no rung decided); returns the grade.
    fn grade_turn(
        &mut self,
        hub: &Inner,
        batch: &mut Batch,
        thread: &str,
        turn: u64,
        result: TurnResult,
        final_text: &Option<String>,
    ) -> anyhow::Result<Grade> {
        let ctx = self.grade_context(thread, turn, result, final_text)?;
        let run = decide(hub, &DecisionContext::Grade(ctx));
        let graded = run.grade();
        let mut records: Vec<DecisionMade> =
            run.records.into_iter().map(DecisionMade::from).collect();
        if graded.defaulted {
            // No rung decided: the code rule's default, logged like any decision.
            records.push(DecisionMade {
                site: DecisionSite::Grade,
                provider: DecisionProvider::Code,
                options: site_options(DecisionSite::Grade),
                chosen: twr_decisions::grade_str(graded.grade).into(),
                confidence: graded.confidence,
                latency_ms: 0,
                ..Default::default()
            });
        }
        for d in records {
            let source = Source::Decision(d.provider.as_str().into());
            batch.push(thread, Some(turn), source, EventBody::DecisionMade(d));
        }
        Ok(graded.grade)
    }

    /// The grading context of turn `turn`: structured fields from the log (tool counts, the
    /// `turn.ended` duration and transient flags) plus the final text, which the code rules
    /// read.
    fn grade_context(
        &self,
        thread: &str,
        turn: u64,
        result: TurnResult,
        final_text: &Option<String>,
    ) -> anyhow::Result<GradeContext> {
        let (tool_calls, tool_failures) = store::turn_tool_counts(self.store.conn(), thread, turn)?;
        let ended = match self.stored_event(thread, Some(turn), "turn.ended")? {
            Some(Event {
                body: EventBody::TurnEnded(p),
                ..
            }) => Some(p),
            _ => None,
        };
        Ok(GradeContext {
            result,
            final_text: final_text.clone(),
            tool_calls,
            tool_failures,
            duration_ms: ended.as_ref().and_then(|e| e.duration_ms),
            transient: twr_decisions::context::transient_flag(
                ended.as_ref().and_then(|e| e.ext.as_ref()),
            ),
        })
    }

    /// Whether turn `turn` of `thread` is a follow-up: the runtime started it itself, after
    /// background work. Its reports say so (`ext.twr.turn_from`), rewrites too.
    pub(super) fn is_follow_up_turn(&self, thread: &str, turn: u64) -> anyhow::Result<bool> {
        Ok(matches!(
            self.stored_event(thread, Some(turn), "turn.started")?,
            Some(Event {
                body: EventBody::TurnStarted(TurnStarted {
                    from: TurnFrom::Runtime,
                    ..
                }),
                ..
            })
        ))
    }

    /// The latest `report.ready` of turn `turn` of `thread` (the one that counts), hydrated.
    pub(super) fn logged_report(
        &self,
        thread: &str,
        turn: u64,
    ) -> anyhow::Result<Option<ReportReady>> {
        let Some(ev) = store::last_of_kind(self.store.conn(), thread, Some(turn), "report.ready")?
        else {
            return Ok(None);
        };
        let ev = hydrated_from_store(self.store.conn(), &ev).unwrap_or(ev);
        match ev.body {
            EventBody::ReportReady(r) => Ok(Some(r)),
            _ => Ok(None),
        }
    }

    /// Startup recovery: completes the reactions of every ended (`completed` / `error`) worker
    /// turn that has no report (a hub crash between `turn.ended` and its reactions would
    /// otherwise skip them forever), oldest first, of every failed orchestrator turn without
    /// its `error` item (orchestrator turns get no report,
    /// [`Core::react_to_orchestrator_turn_end`]), and of every thread's last
    /// ended turn (a log written before reactions were one batch can also hold a report
    /// without its review). Uses the turn's final text from the log, hydrated, and the turn's
    /// latest logged report when there is one ([`Core::react_to_turn_end`]).
    pub(super) fn recover_turn_reactions(&mut self, hub: &Inner) -> anyhow::Result<()> {
        let mut todo: Vec<(String, u64, TurnResult)> = Vec::new();
        let orchestrators: Vec<String> = self
            .registry
            .list()
            .into_iter()
            .filter(|v| v.is_orchestrator())
            .map(|v| v.id.clone())
            .collect();
        for thread in &orchestrators {
            for turn in store::failed_turns_without_error_item(self.store.conn(), thread)? {
                todo.push((thread.clone(), turn, TurnResult::Error));
            }
        }
        for ev in store::unreported_turn_ends(self.store.conn(), &orchestrators)? {
            let (EventBody::TurnEnded(p), Some(turn)) = (&ev.body, ev.turn) else {
                continue;
            };
            if p.result != TurnResult::Interrupted && self.registry.get(&ev.thread).is_some() {
                todo.push((ev.thread.clone(), turn, p.result));
            }
        }
        let last: Vec<(String, u64, TurnResult, bool)> = self
            .registry
            .list()
            .into_iter()
            .filter_map(|v| {
                v.last_turn
                    .as_ref()
                    .map(|t| (v.id.clone(), t.turn, t.result, !v.is_orchestrator()))
            })
            .filter(|(_, _, r, _)| *r != TurnResult::Interrupted)
            .collect();
        for (thread, turn, result, worker) in last {
            if todo.iter().any(|(t, n, _)| *t == thread && *n == turn) {
                continue;
            }
            if !self.turn_reactions_complete(&thread, turn, result, worker)? {
                todo.push((thread, turn, result));
            }
        }
        for (thread, turn, result) in todo {
            let final_text = self.logged_final_text(&thread, turn)?;
            tracing::info!(%thread, turn, "completing the reactions to a turn that ended before a hub restart");
            self.react_to_turn_end(hub, &thread, turn, result, final_text)?;
        }
        Ok(())
    }

    /// The final text of turn `turn` of `thread` as the log has it (hydrated).
    pub(super) fn logged_final_text(
        &self,
        thread: &str,
        turn: u64,
    ) -> anyhow::Result<Option<String>> {
        Ok(
            match store::last_final_text(self.store.conn(), thread, turn)? {
                None => None,
                Some(ev) => {
                    let ev = hydrated_from_store(self.store.conn(), &ev).unwrap_or(ev);
                    match ev.body {
                        EventBody::AgentText(t) => Some(t.text),
                        _ => None,
                    }
                }
            },
        )
    }

    /// Whether the log holds every reaction to ended turn `turn` (`result`) of `thread`: a
    /// worker turn's report, and its review when one is due; a failed orchestrator turn's
    /// `error` item (`worker` false: nothing else, [`Core::react_to_orchestrator_turn_end`]).
    /// What [`Core::recover_turn_reactions`] checks.
    pub(super) fn turn_reactions_complete(
        &self,
        thread: &str,
        turn: u64,
        result: TurnResult,
        worker: bool,
    ) -> anyhow::Result<bool> {
        let done = store::turn_reactions(self.store.conn(), thread, turn)?;
        if !worker {
            return Ok(result != TurnResult::Error || done.errored);
        }
        if self
            .logged_report(thread, turn)?
            .is_some_and(|r| is_interim(&r))
        {
            return Ok(true);
        }
        Ok(match done.report_grade {
            None => false,
            Some(g) => done.reviewed || !review_due(Some(result), g, &done),
        })
    }

    /// The hub's reactions to a turn its runtime ended: the runtime no longer waits on any
    /// request it raised in that turn (only those: a retry of this that runs after the next
    /// turn started leaves that turn's requests alone, [`AdapterScope`]), the turn gets its
    /// grade / report / review, and the queue moves on. Safe to repeat after a failure (each
    /// step skips what is already recorded). The steps are separate appends on purpose: each
    /// is idempotent and derived from the log, and startup repeats every one of them (idle
    /// threads' adapter items, turn reactions, failure reactions, queued prompts), so a crash
    /// between two loses nothing.
    ///
    /// [`AdapterScope`]: super::AdapterScope
    pub(super) fn react_to_adapter_turn_end(
        &mut self,
        hub: &Arc<Inner>,
        thread: &str,
        t: &EndedTurn,
    ) -> anyhow::Result<()> {
        self.resolve_turn_adapter_attention(
            hub,
            thread,
            t.turn,
            t.runtime_gen,
            ResolvedBy::Timeout,
            true,
        )?;
        self.react_to_turn_end(hub, thread, t.turn, t.result, t.final_text.clone())?;
        if t.result == TurnResult::Error {
            self.react_to_failure(hub, thread)?;
        }
        // A due retry goes first (Core::dispatch_next).
        self.try_dispatch(hub, thread)?;
        Ok(())
    }
}

/// Whether the report that counts for a worker turn (its latest) raises a hub `review` item:
/// its grade is `gave_up` / `needs_verification`, and the turn's failure has no attention of
/// its own. A failed (`error`) turn gets the `error` item or the automatic retry of
/// `hub::retry` instead (one item per failure), and so does any turn with a
/// logged block_kind decision or hub `error` item. `result` is the turn's, when known.
/// Orchestrator turns get no report, so no review ([`Core::react_to_orchestrator_turn_end`]).
pub(super) fn review_due(
    result: Option<TurnResult>,
    grade: Grade,
    done: &store::TurnReactions,
) -> bool {
    grade.needs_review()
        && result != Some(TurnResult::Error)
        && !done.errored
        && done.block_kind.is_none()
}

/// Whether `report` is interim: its turn ended with background tasks still running
/// (`ext.twr.background_tasks`), and the runtime's own turn after them reports again. An
/// interim report raises no review.
pub(super) fn is_interim(report: &ReportReady) -> bool {
    report::interim_tasks(report) > 0
}

/// `ext.twr` key of an interim report: the background tasks its turn ended with, task id →
/// the thread seq that started it ([`twr_core::reduce::BackgroundTask::started_seq`]).
pub(super) const BACKGROUND_TASK_STARTS: &str = "background_task_starts";

/// The background tasks `report`'s turn ended with that `running` still has, the same tasks
/// (an id reused since is another task), as `ext.twr` would list them. A report from before
/// the tasks were listed counts all of `running` while its marker is set.
pub(super) fn interim_tasks_running(
    report: &ReportReady,
    running: &std::collections::BTreeMap<String, twr_core::reduce::BackgroundTask>,
) -> serde_json::Map<String, Value> {
    if !is_interim(report) {
        return serde_json::Map::new();
    }
    let starts = report
        .ext
        .as_ref()
        .and_then(|e| e.get("twr"))
        .and_then(|t| t.get(BACKGROUND_TASK_STARTS))
        .and_then(Value::as_object);
    running
        .iter()
        .filter(|(id, t)| match starts {
            Some(starts) => starts
                .get(id.as_str())
                .and_then(Value::as_u64)
                .is_some_and(|seq| seq == t.started_seq),
            None => true,
        })
        .map(|(id, t)| (id.clone(), json!(t.started_seq)))
        .collect()
}

/// The hub `review` item for `report` of turn `turn` of thread `name` (`ext.twr.report_by`:
/// the report's writer, when it is not the hub).
pub(super) fn review_item(name: &str, turn: u64, report: &ReportReady) -> AttentionRaised {
    AttentionRaised {
        attn_id: new_id(),
        reason: AttentionReason::Review,
        summary: format!(
            "{name}: turn {turn} {}: {}",
            twr_decisions::grade_str(report.grade),
            report.summary
        ),
        options: vec!["dismiss".into()],
        ext: (report.written_by != "hub")
            .then(|| twr_ext_map(json!({ "report_by": report.written_by })))
            .flatten(),
        ..Default::default()
    }
}

/// The options offered at `site`.
pub(super) fn site_options(site: DecisionSite) -> Vec<String> {
    let opts: &[&str] = match site {
        DecisionSite::Grade => &twr_decisions::GRADE_OPTIONS,
        DecisionSite::BlockKind => &twr_decisions::BLOCK_KIND_OPTIONS,
        DecisionSite::Route => &[],
    };
    opts.iter().map(|s| s.to_string()).collect()
}

/// Runs the hub's ladder for a grade or block_kind decision.
pub(super) fn decide(hub: &Inner, ctx: &DecisionContext) -> LadderRun {
    hub.ladder.run(ctx, &site_options(ctx.site()))
}
