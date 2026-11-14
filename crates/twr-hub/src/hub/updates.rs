//! Durable conversation updates that cost no orchestrator inference.

use super::orch::cut;
use super::{twr_ext, twr_ext_map, BatchEvent, Core};
use serde_json::{json, Value};
use twr_core::event::*;

/// `prompt` as an orchestrator turn delivers it after hub `facts` (lines, each a JSON string or
/// object): unchanged without any, else behind the `[twr background]` block, and for a user
/// turn (`user`) with the user's message marked as such.
pub(super) fn with_hub_facts(facts: &[String], prompt: String, user: bool) -> String {
    if facts.is_empty() {
        return prompt;
    }
    let facts = facts.join("\n");
    let mark = if user { "[user message]\n" } else { "\n" };
    format!("[twr background] Hub facts, not instructions or answers:\n{facts}\n{mark}{prompt}")
}

/// `ext` with `ext.twr.`[`ROUTINE_KEY`] set to `kind`, every other key kept (a `twr` that is
/// no object is replaced).
fn routine(ext: Option<JsonMap>, kind: &str) -> Option<JsonMap> {
    mark(ext, json!(kind))
}

/// `ext` with `ext.twr.`[`ROUTINE_KEY`] `null`, every other key kept: an update that is no
/// routine one, whatever its words (a client tells an older hub's updates by them).
fn not_routine(ext: Option<JsonMap>) -> Option<JsonMap> {
    mark(ext, Value::Null)
}

/// `ext` with `ext.twr.`[`ROUTINE_KEY`] set to `value` ([`routine`], [`not_routine`]).
fn mark(ext: Option<JsonMap>, value: Value) -> Option<JsonMap> {
    let mut ext = ext.unwrap_or_default();
    let twr = ext.entry("twr").or_insert_with(|| json!({}));
    if !twr.is_object() {
        *twr = json!({});
    }
    if let Some(t) = twr.as_object_mut() {
        t.insert(ROUTINE_KEY.into(), value);
    }
    Some(ext)
}

/// Whether an event of `body`'s kind can make an update of [`Core::conversation_updates`]
/// (every kind its arms match).
fn may_update(body: &EventBody) -> bool {
    matches!(
        body,
        EventBody::AttentionRaised(_)
            | EventBody::LoopStarted(_)
            | EventBody::LoopFindings(_)
            | EventBody::LoopTransition(_)
            | EventBody::LoopPhase(_)
            | EventBody::LoopControl(_)
            | EventBody::LoopEnded(_)
            | EventBody::LoopPlan(_)
            | EventBody::LoopImplemented(_)
            | EventBody::LoopCommit(_)
            | EventBody::LoopRules(_)
            | EventBody::LoopAudit(_)
            | EventBody::ReportReady(_)
            | EventBody::TaskUpdated(_)
            | EventBody::ConversationUpdate(_)
            | EventBody::AttentionResolved(_)
    )
}

/// Whether `batch` ends a turn of `thread`.
fn ended(batch: &[BatchEvent], thread: &str) -> bool {
    batch
        .iter()
        .any(|x| x.thread == thread && matches!(x.body, EventBody::TurnEnded(_)))
}

/// What the orchestrator is told when worker `name`'s hung-turn card closes (`p`), its turn
/// ended in the same batch or not (`turn_ended`): that the alarm is over, and why.
fn hung_closed_text(name: &str, p: &AttentionResolved, turn_ended: bool) -> String {
    let how = match &p.answer {
        Some(a) => format!("answered: {}", cut(&a.decision, 80)),
        None if turn_ended => "closed: its turn ended".to_string(),
        None if twr_ext(&p.ext)
            .is_some_and(|t| t.contains_key(super::stall::NOTHING_WAITS_KEY)) =>
        {
            "closed: nothing waits behind its turn any longer".to_string()
        }
        None if went_on(&p.ext) => "closed: its runtime went on".to_string(),
        None => "closed".to_string(),
    };
    format!(
        "{name}'s seems-hung card {} {how}; it is no longer a current alarm.",
        p.attn_id
    )
}

/// The orchestrator's line when worker `v` starts loop `p` (`hub::loops`): what it runs on,
/// where the settings came from and, when the orchestrator asked for it, that it did: `api
/// started a review loop: high · implementer claude-opus-5-5 high · reviewer codex gpt-6.1-sol
/// (saved settings; /review --setup changes them)`, `api started a review loop, which the
/// orchestrator asked for: …`.
fn loop_started_text(v: &twr_core::reduce::ThreadView, p: &LoopStarted) -> String {
    use twr_core::loops::settings::{SettingsOf, SettingsSource};
    let s = &p.settings;
    let setup = match p.kind {
        twr_core::loops::LoopKind::Review => "/review --setup",
        twr_core::loops::LoopKind::Implement => "/implement --setup",
    };
    let implementer = match &s.implementer {
        Some(m) => [Some(m.model.as_str()), m.effort.as_deref()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" "),
        None => {
            let own = [v.model.as_deref(), v.effort.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(" ");
            if own.is_empty() {
                format!("{} (the thread's own)", v.runtime)
            } else {
                format!("{own} (the thread's own)")
            }
        }
    };
    let from = match (p.settings_from, p.settings_of) {
        (SettingsSource::SetupItem, _) => {
            "settings chosen on its setup card, now saved".to_string()
        }
        (SettingsSource::Saved, Some(SettingsOf::Project)) => format!(
            "{}'s settings; {setup} changes them",
            v.project.as_deref().unwrap_or(&v.name)
        ),
        (SettingsSource::Saved, _) => format!("saved settings; {setup} changes them"),
    };
    let mut run = Vec::new();
    for (what, val) in [
        ("profile", &p.overrides.profile),
        ("implementer", &p.overrides.implementer),
        ("workers", &p.overrides.workers),
        ("reviewer", &p.overrides.reviewer),
        ("commit", &p.overrides.commit),
        ("plan check", &p.overrides.plan_check),
    ] {
        if let Some(val) = val {
            run.push(format!("{what} {val}"));
        }
    }
    let run = if run.is_empty() {
        String::new()
    } else {
        format!("; this run only: {}", run.join(", "))
    };
    let kind = match p.kind {
        twr_core::loops::LoopKind::Review => "review",
        twr_core::loops::LoopKind::Implement => "implementation",
    };
    let asked = match p.requested_by {
        twr_core::loops::ControlBy::Orchestrator => ", which the orchestrator asked for",
        _ => "",
    };
    format!(
        "{} started a {kind} loop{asked}: {} · implementer {implementer} · reviewer {} {} \
         ({from}{run})",
        v.name, p.profile.name, s.reviewer.runtime, s.reviewer.model
    )
}

impl Core {
    /// Whether report `p` (of an event of turn `turn`) of worker `thread` is a failure's: it
    /// is graded `gave_up` (as every failed turn is), or its own turn failed, as worker `v`
    /// (the batch applied) last ended it or else as the log's `turn.ended` of that turn says
    /// (a rewrite of an older turn's report, `report.write`, after later turns ended). Never
    /// routine, whatever ran on after it; a log that cannot be read counts it failed.
    fn failed(
        &self,
        thread: &str,
        p: &ReportReady,
        turn: Option<u64>,
        v: &twr_core::reduce::ThreadView,
    ) -> bool {
        if p.grade == Grade::GaveUp {
            return true;
        }
        let Some(turn) = p.turn.or(turn) else {
            return false;
        };
        if let Some(t) = v.last_turn.as_ref().filter(|t| t.turn == turn) {
            return t.result == TurnResult::Error;
        }
        match self.stored_event(thread, Some(turn), "turn.ended") {
            Ok(Some(Event {
                body: EventBody::TurnEnded(t),
                ..
            })) => t.result == TurnResult::Error,
            Ok(_) => false,
            Err(e) => {
                tracing::warn!(%thread, turn, error = %e, "cannot read how a reported turn ended");
                true
            }
        }
    }

    /// Derive updates before a batch commits, preserving crash/replay consistency. Those that
    /// tell of work going on are marked routine ([`ROUTINE_KEY`]): an interim report, a
    /// worker's background work finishing, the hub's wake of it. Final reports, failures (a
    /// failed turn's report too, though tasks ran on after it: marked `null`), the hub's choice
    /// not to wake a worker (it asks the user to send it a message: marked `null` too), items
    /// and their outcomes, and confirm cards' are not.
    pub(super) fn conversation_updates(&self, events: &[BatchEvent]) -> Vec<BatchEvent> {
        let Some(orch) = self.orch.thread.as_ref() else {
            return Vec::new();
        };
        // Only the threads with an event of a kind that can be said ([`may_update`]) need
        // their view replayed through the batch: most batches (a runtime's text, tool calls
        // and results) have none, and cost no copy of a view or of a body.
        let said: std::collections::HashSet<&str> = events
            .iter()
            .filter(|e| may_update(&e.body))
            .map(|e| e.thread.as_str())
            .collect();
        if said.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut views = std::collections::HashMap::new();
        for e in events {
            if !said.contains(e.thread.as_str()) || self.is_orch(&e.thread) {
                continue;
            }
            let view = views
                .entry(e.thread.clone())
                .or_insert_with(|| self.registry.get(&e.thread).cloned());
            // The user's `!` commands are theirs: the orchestrator hears nothing of them.
            let had_background = view.as_ref().is_some_and(|v| v.own_background());
            // The loop as it was before the event (a round's reviewer, its rounds so far).
            let prev_loop = view
                .as_ref()
                .filter(|_| e.body.kind().starts_with("loop."))
                .and_then(|v| v.r#loop.clone());
            if let Some(view) = view {
                view.apply(&Event::new(
                    &e.thread,
                    0,
                    e.turn,
                    e.source.clone(),
                    e.body.clone(),
                ));
            }
            let worker = view.as_ref().filter(|v| !v.is_orchestrator());
            // `review loop` or `implementation loop`, as the loop was (or, for its start, is).
            let kind = prev_loop
                .as_ref()
                .map(|l| l.kind)
                .or_else(|| worker.and_then(|v| v.r#loop.as_ref().map(|l| l.kind)))
                .unwrap_or_default();
            let loop_words = super::loops::kind_words(kind);
            let (text, about, ext) = match (&e.body, worker) {
                // A loop's setup card (`hub::loops`): the user's to answer, like a question.
                (EventBody::AttentionRaised(p), Some(v))
                    if e.source == Source::Hub && is_loop_setup_card(&p.ext) =>
                {
                    (
                        format!(
                            "{} waits for the user to choose its loop's settings (setup card {}): {}",
                            v.name,
                            p.attn_id,
                            cut(&p.summary, 600)
                        ),
                        Some(e.thread.clone()),
                        twr_ext_map(json!({"attn_id": p.attn_id})),
                    )
                }
                (EventBody::LoopStarted(p), Some(v)) => (
                    loop_started_text(v, p),
                    Some(e.thread.clone()),
                    twr_ext_map(json!({"loop": p.id})),
                ),
                (EventBody::AttentionResolved(p), Some(v)) if self.loop_card(&p.attn_id) => {
                    let outcome = p.answer.as_ref().map_or_else(
                        || "closed with its thread".to_string(),
                        |a| format!("answered: {}", cut(&a.decision, 80)),
                    );
                    (
                        format!("{}'s loop setup card {} {outcome}.", v.name, p.attn_id),
                        Some(e.thread.clone()),
                        twr_ext_map(json!({"attn_id": p.attn_id, "resolved": true})),
                    )
                }
                // A stuck loop's item: the user's to answer; not routine.
                (EventBody::AttentionRaised(p), Some(v))
                    if e.source == Source::Hub && is_loop_stuck_item(&p.ext) =>
                {
                    (
                        format!(
                            "{} waits for the user: {} (stuck item {}: resume or stop)",
                            v.name,
                            cut(&p.summary, 600),
                            p.attn_id
                        ),
                        Some(e.thread.clone()),
                        not_routine(twr_ext_map(json!({"attn_id": p.attn_id}))),
                    )
                }
                (EventBody::AttentionResolved(p), Some(v)) if self.loop_stuck(&p.attn_id) => {
                    let outcome = p.answer.as_ref().map_or_else(
                        || "closed".to_string(),
                        |a| format!("answered: {}", cut(&a.decision, 80)),
                    );
                    (
                        format!("{}'s stuck loop item {} {outcome}.", v.name, p.attn_id),
                        Some(e.thread.clone()),
                        twr_ext_map(json!({"attn_id": p.attn_id, "resolved": true})),
                    )
                }
                // A loop's rounds and phases: routine; a failed round and its end not.
                (EventBody::LoopFindings(p), Some(v)) => {
                    let (model, effort) = prev_loop
                        .as_ref()
                        .map(|l| {
                            (
                                l.reviewer.model.clone(),
                                l.reviewer.effort.clone().unwrap_or_default(),
                            )
                        })
                        .unwrap_or_default();
                    if p.failed {
                        (
                            format!(
                                "{}: review round {} ({model} {effort}) failed: {}",
                                v.name,
                                p.round,
                                cut(p.reason.as_deref().unwrap_or("the run failed"), 300)
                            ),
                            Some(e.thread.clone()),
                            not_routine(twr_ext_map(json!({"loop": p.id, "round": p.round}))),
                        )
                    } else {
                        let n = p.findings.len();
                        (
                            format!(
                                "{}: review round {} · {model} {effort} · {n} finding{}",
                                v.name,
                                p.round,
                                if n == 1 { "" } else { "s" }
                            ),
                            Some(e.thread.clone()),
                            routine(
                                twr_ext_map(json!({"loop": p.id, "round": p.round})),
                                ROUTINE_LOOP,
                            ),
                        )
                    }
                }
                (EventBody::LoopTransition(p), Some(v)) => (
                    if p.round.is_empty() {
                        format!(
                            "{}: its {loop_words} skipped to verification before any ladder round",
                            v.name
                        )
                    } else {
                        format!(
                            "{}: its {loop_words}'s ladder ended after {}: {}{}",
                            v.name,
                            p.round,
                            super::loops::transition_words(p.reason),
                            if p.verify_skipped {
                                "; verification skipped, since it could only repeat that round"
                            } else {
                                ""
                            }
                        )
                    },
                    Some(e.thread.clone()),
                    routine(twr_ext_map(json!({"loop": p.id})), ROUTINE_LOOP),
                ),
                // (Every `loop.phase` without problems is a change of phase; the first, to
                // scope, goes with the start's own line.)
                (EventBody::LoopPhase(p), Some(v))
                    if prev_loop.is_some()
                        && p.problems.is_empty()
                        && !matches!(
                            p.phase,
                            twr_core::loops::Phase::Scope | twr_core::loops::Phase::Plan
                        ) =>
                {
                    (
                        format!("{}: its {loop_words} is in its {} now", v.name, p.phase),
                        Some(e.thread.clone()),
                        routine(twr_ext_map(json!({"loop": p.id})), ROUTINE_LOOP),
                    )
                }
                (EventBody::LoopControl(p), Some(v))
                    if p.by != twr_core::loops::ControlBy::Hub =>
                {
                    (
                        format!(
                            "{}'s {loop_words}: {} by {}",
                            v.name,
                            p.action.as_str(),
                            p.by.words()
                        ),
                        Some(e.thread.clone()),
                        routine(twr_ext_map(json!({"loop": p.id})), ROUTINE_LOOP),
                    )
                }
                // How a loop ended, once: its last report says it when it wrote one.
                (EventBody::LoopEnded(p), Some(v)) if p.report_path.is_none() => {
                    let rounds = prev_loop.as_ref().map_or(0, |l| {
                        l.rounds
                            .iter()
                            .filter(|r| r.id != twr_core::loops::PLAN_CHECK_ROUND)
                            .count()
                            - usize::from(matches!(
                                &l.wait,
                                twr_core::loops::Wait::Review { round }
                                    if round != twr_core::loops::PLAN_CHECK_ROUND
                            ))
                    });
                    let why = p
                        .reason
                        .as_deref()
                        .map(|r| format!(": {}", cut(r, 300)))
                        .unwrap_or_default();
                    (
                        format!(
                            "{}: {}{why}",
                            v.name,
                            super::loops::outcome_words(kind, p.outcome, rounds)
                        ),
                        Some(e.thread.clone()),
                        not_routine(twr_ext_map(json!({"loop": p.id, "outcome": p.outcome}))),
                    )
                }
                // A loop's turn's report: routine but for the loop's last one, which is the
                // thread's report of the loop (its outcome and `report.md`).
                (EventBody::ReportReady(p), Some(v))
                    if twr_ext(&p.ext).is_some_and(|t| t.contains_key(LOOP_REPORT_KEY)) =>
                {
                    let mark = twr_ext(&p.ext)
                        .and_then(|t| t.get(LOOP_REPORT_KEY))
                        .cloned()
                        .unwrap_or_default();
                    let step = mark["step"].as_str().unwrap_or_default().to_string();
                    // (A report turn whose commit is still being checked: the loop's last
                    // report comes once it is.)
                    if step == "report" && mark["commit"] != "checking" {
                        (
                            format!("{}: {}", v.name, p.summary),
                            Some(e.thread.clone()),
                            not_routine(p.ext.clone()),
                        )
                    } else {
                        (
                            format!(
                                "{} answered its {loop_words}'s {step} step ({}): {}",
                                v.name,
                                twr_decisions::grade_str(p.grade),
                                p.summary
                            ),
                            Some(e.thread.clone()),
                            routine(p.ext.clone(), ROUTINE_LOOP),
                        )
                    }
                }
                // An implementation loop's plan, implement turn and commit check: routine.
                (EventBody::LoopPlan(p), Some(v)) => (
                    format!(
                        "{}: its {loop_words}'s plan{} (contract risk: {}: {}){}",
                        v.name,
                        if p.revision >= 2 {
                            ", revised after the plan check"
                        } else {
                            ""
                        },
                        if p.contract_risk { "yes" } else { "no" },
                        cut(&p.reason, 200),
                        p.path
                            .as_deref()
                            .map(|path| format!("; {path}"))
                            .unwrap_or_default()
                    ),
                    Some(e.thread.clone()),
                    routine(
                        twr_ext_map(json!({"loop": p.id, "plan": p.revision})),
                        ROUTINE_LOOP,
                    ),
                ),
                (EventBody::LoopImplemented(p), Some(v)) => (
                    format!(
                        "{}: its {loop_words}'s implement turn: {}, gates {}",
                        v.name,
                        p.status.as_str(),
                        match p.gates.result {
                            twr_core::loops::contract::GateResult::Pass => "pass",
                            twr_core::loops::contract::GateResult::Fail => "fail",
                        }
                    ),
                    Some(e.thread.clone()),
                    routine(twr_ext_map(json!({"loop": p.id})), ROUTINE_LOOP),
                ),
                (EventBody::LoopCommit(p), Some(v)) => (
                    if p.problems.is_empty() {
                        format!(
                            "{}: its {loop_words}'s commit checked: {}",
                            v.name,
                            if p.commits.is_empty() {
                                "nothing to commit".to_string()
                            } else {
                                p.commits
                                    .iter()
                                    .map(|c| format!("{} in {}", cut(&c.sha, 12), c.repo))
                                    .collect::<Vec<_>>()
                                    .join(", ")
                            }
                        )
                    } else {
                        format!(
                            "{}: its {loop_words}'s commit check failed: {}",
                            v.name,
                            cut(&p.problems.join("; "), 300)
                        )
                    },
                    Some(e.thread.clone()),
                    routine(twr_ext_map(json!({"loop": p.id})), ROUTINE_LOOP),
                ),
                // The sweep's rules and audit runs (`sweep = "runs"`): routine, failures too,
                // since a failed audit never holds the loop (it is retried once, then reported
                // to the thread as not run).
                (EventBody::LoopRules(p), Some(v)) => (
                    if p.rules.is_empty() {
                        format!(
                            "{}: its {loop_words}'s sweep named no rule to audit",
                            v.name
                        )
                    } else {
                        format!(
                            "{}: its {loop_words}'s sweep named {} rule{} to audit: {}",
                            v.name,
                            p.rules.len(),
                            if p.rules.len() == 1 { "" } else { "s" },
                            cut(
                                &p.rules
                                    .iter()
                                    .map(|r| r.name.as_str())
                                    .collect::<Vec<_>>()
                                    .join(", "),
                                300
                            )
                        )
                    },
                    Some(e.thread.clone()),
                    routine(twr_ext_map(json!({"loop": p.id})), ROUTINE_LOOP),
                ),
                (EventBody::LoopAudit(p), Some(v)) => {
                    let audit = twr_core::loops::contract::audit_id(p.rule);
                    let (model, effort) = prev_loop
                        .as_ref()
                        .map(|l| (l.reviewer.model.clone(), l.plan.audit_effort.clone()))
                        .unwrap_or_default();
                    let retried = prev_loop.as_ref().is_some_and(|l| {
                        l.audits.iter().any(|a| a.rule == p.rule && a.retried)
                    });
                    let text = if p.failed {
                        format!(
                            "{}: sweep audit {audit} ({}) · {model} {effort} failed{}: {}",
                            v.name,
                            p.name,
                            if retried {
                                " again: not run"
                            } else {
                                "; retrying it"
                            },
                            cut(p.reason.as_deref().unwrap_or("the run failed"), 300)
                        )
                    } else {
                        let n = p.findings.len();
                        format!(
                            "{}: sweep audit {audit} ({}) · {model} {effort} · {n} finding{}",
                            v.name,
                            p.name,
                            if n == 1 { "" } else { "s" }
                        )
                    };
                    (
                        text,
                        Some(e.thread.clone()),
                        routine(
                            twr_ext_map(json!({"loop": p.id, "audit": audit})),
                            ROUTINE_LOOP,
                        ),
                    )
                }
                (EventBody::LoopFindings(_)
                | EventBody::LoopRules(_)
                | EventBody::LoopAudit(_)
                | EventBody::LoopTransition(_)
                | EventBody::LoopPhase(_)
                | EventBody::LoopControl(_)
                | EventBody::LoopPlan(_)
                | EventBody::LoopImplemented(_)
                | EventBody::LoopCommit(_)
                | EventBody::LoopEnded(_), _) => continue,
                (EventBody::AttentionRaised(p), Some(v)) => (
                    format!(
                        "{} {}: {}",
                        v.name,
                        match p.reason {
                            AttentionReason::Permission => "needs permission",
                            AttentionReason::Question => "asks",
                            AttentionReason::Error => "failed",
                            AttentionReason::Review => "needs review",
                            AttentionReason::Stalled if is_hung_card(&p.ext) => "seems hung",
                            AttentionReason::Stalled => "is stalled",
                        },
                        cut(&p.summary, 600)
                    ),
                    Some(e.thread.clone()),
                    twr_ext_map(json!({"attn_id": p.attn_id})),
                ),
                // The report of a turn that saw no run repeats one already said (`hub::wake`).
                (EventBody::ReportReady(p), Some(_)) if super::wake::no_run_of(p).is_some() => {
                    continue;
                }
                // Said from the report's own marks: tasks another turn runs now are not its.
                // An interim one is routine: the result comes later; but never a failure's
                // ([`failed`]), which says so (`routine: null`: no client takes its words for
                // an interim report's).
                (EventBody::ReportReady(p), Some(v)) => {
                    let tasks = crate::report::interim_tasks(p);
                    let how = if tasks > 0 {
                        format!(
                            "yielded with {tasks} background task{} still running \
                             (interim report: the result comes later)",
                            if tasks == 1 { "" } else { "s" }
                        )
                    } else if crate::report::is_follow_up(p) {
                        "finished, in a follow-up turn it started after background work".into()
                    } else {
                        "finished".into()
                    };
                    (
                        format!(
                            "{} {how} ({}): {}",
                            v.name,
                            twr_decisions::grade_str(p.grade),
                            p.summary
                        ),
                        Some(e.thread.clone()),
                        match tasks {
                            0 => p.ext.clone(),
                            _ if self.failed(&e.thread, p, e.turn, v) => not_routine(p.ext.clone()),
                            _ => routine(p.ext.clone(), ROUTINE_INTERIM),
                        },
                    )
                }
                (EventBody::TaskUpdated(p), Some(v))
                    if !p.running
                        && had_background
                        && !v.own_background()
                        && v.current_turn.is_none() =>
                {
                    (
                        format!("{} background work finished.", v.name),
                        Some(e.thread.clone()),
                        routine(None, ROUTINE_BACKGROUND_DONE),
                    )
                }
                // The hub woke the worker after its background work (routine), or chose not to
                // (`hub::wake`), which asks the user to send it a message (not routine: marked
                // `null`): said as the worker's record says it.
                (EventBody::ConversationUpdate(p), Some(_))
                    if twr_ext(&p.ext).is_some_and(|t| {
                        t.contains_key(super::wake::AUTO_WAKE_KEY)
                            || t.contains_key(super::wake::AUTO_WAKE_SKIPPED_KEY)
                    }) =>
                {
                    let skipped = twr_ext(&p.ext)
                        .is_some_and(|t| t.contains_key(super::wake::AUTO_WAKE_SKIPPED_KEY));
                    (
                        p.text.clone(),
                        Some(e.thread.clone()),
                        if skipped {
                            not_routine(None)
                        } else {
                            routine(None, ROUTINE_AUTO_WAKE)
                        },
                    )
                }
                // A hung-turn card closed: said, so that its alarm is not taken for a current
                // one (`hub::stall`).
                (EventBody::AttentionResolved(p), Some(v)) if self.hung_card(&p.attn_id) => (
                    hung_closed_text(&v.name, p, ended(events, &e.thread)),
                    Some(e.thread.clone()),
                    twr_ext_map(json!({"attn_id": p.attn_id, "resolved": true})),
                ),
                (EventBody::AttentionResolved(p), Some(v))
                    if self
                        .attention
                        .get(&p.attn_id)
                        .is_some_and(|i| i.reason.is_blocking()) =>
                {
                    let outcome = p.answer.as_ref().map_or_else(
                        || "closed".to_string(),
                        |a| format!("answered: {}", cut(&a.decision, 600)),
                    );
                    (
                        format!("{} request {} {outcome}.", v.name, p.attn_id),
                        Some(e.thread.clone()),
                        twr_ext_map(json!({"attn_id": p.attn_id, "resolved": true})),
                    )
                }
                (EventBody::AttentionResolved(p), _)
                    if e.thread == HUB_THREAD
                        && (self.hub_cards.contains_key(&p.attn_id)
                            || twr_ext(&p.ext).is_some_and(|t| t.contains_key("confirmed"))) =>
                {
                    let decision = p
                        .answer
                        .as_ref()
                        .map(|a| a.decision.as_str())
                        .unwrap_or("cancel");
                    if decision == super::proposal::SUPERSEDED {
                        continue;
                    }
                    let confirmed = twr_ext(&p.ext).and_then(|t| t.get("confirmed"));
                    let text = match confirmed {
                        Some(c) => {
                            let list = |key| {
                                c.get(key)
                                    .and_then(Value::as_array)
                                    .map(|a| {
                                        a.iter()
                                            .filter_map(Value::as_str)
                                            .collect::<Vec<_>>()
                                            .join(", ")
                                    })
                                    .filter(|s| !s.is_empty())
                                    .unwrap_or_else(|| "nothing".into())
                            };
                            format!(
                                "Confirmed. Applied: {}. Skipped: {}.",
                                list("applied"),
                                list("skipped")
                            )
                        }
                        None => "Confirmation cancelled; nothing applied.".into(),
                    };
                    (text, None, twr_ext_map(json!({"attn_id": p.attn_id})))
                }
                _ => continue,
            };
            out.push(BatchEvent {
                thread: orch.clone(),
                turn: None,
                source: Source::Hub,
                body: EventBody::ConversationUpdate(ConversationUpdate {
                    text,
                    about_thread: about,
                    ext,
                    ..Default::default()
                }),
                raw: None,
            });
        }
        out
    }

    /// The updates of the hung-turn cards `resolutions` close (the resolutions the activity of
    /// `batch` implies, `Core::stall_resolutions`): said as [`Core::conversation_updates`] says
    /// one the batch closes itself.
    pub(super) fn hung_closures(
        &self,
        batch: &[BatchEvent],
        resolutions: &[BatchEvent],
    ) -> Vec<BatchEvent> {
        let Some(orch) = self.orch.thread.as_ref() else {
            return Vec::new();
        };
        resolutions
            .iter()
            .filter_map(|e| {
                let EventBody::AttentionResolved(p) = &e.body else {
                    return None;
                };
                let v = self
                    .registry
                    .get(&e.thread)
                    .filter(|v| !v.is_orchestrator() && self.hung_card(&p.attn_id))?;
                Some(BatchEvent {
                    thread: orch.clone(),
                    turn: None,
                    source: Source::Hub,
                    body: EventBody::ConversationUpdate(ConversationUpdate {
                        text: hung_closed_text(&v.name, p, ended(batch, &e.thread)),
                        about_thread: Some(e.thread.clone()),
                        ext: twr_ext_map(json!({"attn_id": p.attn_id, "resolved": true})),
                        ..Default::default()
                    }),
                    raw: None,
                })
            })
            .collect()
    }

    /// Bounded updates since the preceding turn, supplied as context with a user message. A
    /// thread's routine line ([`ROUTINE_KEY`]) goes only when no later line about that thread
    /// is in them; other lines are never left out for that.
    pub(super) fn update_context(&self, thread: &str) -> anyhow::Result<String> {
        let mut updates =
            crate::store::latest_of_kind(self.store.conn(), thread, "conversation.update", 33)?;
        // Notice and handoff turns do not receive this context. Only the preceding
        // user turn is a delivery watermark; the current user turn is already committed. A
        // turn whose message the user took back is none: what it was told goes again. Nor is
        // a compaction (`hub::compact`): it was told nothing.
        let since: i64 = self.store.conn().query_row(
            "SELECT COALESCE((SELECT seq FROM events WHERE thread=?1 AND kind='turn.started'
             AND json_extract(data, '$.from')='user'
             AND json_extract(data, '$.ext.twr.compact') IS NOT 1
             AND turn NOT IN (SELECT turn FROM events WHERE thread=?1 AND kind='turn.withdrawn'
                              AND turn IS NOT NULL)
             ORDER BY seq DESC LIMIT 1 OFFSET 1),0)",
            [thread],
            |r| r.get(0),
        )?;
        // Find the notice reference independently of the ordinary recent-update cap.
        // A burst of worker events must not hide the background turn the user preempted.
        let interrupted: Option<i64> = self.store.conn().query_row(
            "SELECT MAX(seq) FROM events WHERE thread=?1 AND kind='conversation.update' AND seq>?2
             AND json_extract(data,'$.ext.twr.interrupted_notice_seq') IS NOT NULL",
            rusqlite::params![thread, since],
            |r| r.get(0),
        )?;
        if let Some(seq) = interrupted {
            if !updates.iter().any(|e| e.seq == seq as u64) {
                updates.extend(crate::store::read_thread_events(
                    self.store.conn(),
                    thread,
                    (seq as u64).saturating_sub(1),
                    1,
                )?);
            }
        }
        let since = since as u64;
        updates.retain(|e| e.seq > since);
        let priority = interrupted.map(|s| s as u64);
        updates.sort_by_key(|e| (Some(e.seq) != priority, std::cmp::Reverse(e.seq)));
        let mut more = updates.len() > 32;
        let mut lines = Vec::new();
        let mut bytes = 0;
        // The newest line in the facts about each thread: a routine line of a thread with a
        // later line there says nothing that one doesn't (interim chatter costs no tokens).
        let mut newest: std::collections::HashMap<String, u64> = Default::default();
        for e in updates.into_iter().take(32) {
            if let EventBody::ConversationUpdate(p) = e.body {
                let about = p.about_thread.clone();
                let later = about
                    .as_ref()
                    .and_then(|t| newest.get(t))
                    .is_some_and(|seq| *seq > e.seq);
                if later && routine_of(&p.ext).is_some() {
                    continue;
                }
                let (mut fact, hint) = match twr_ext(&p.ext)
                    .and_then(|t| t.get("interrupted_notice"))
                    .and_then(Value::as_str)
                {
                    Some(notice) => {
                        let seq = twr_ext(&p.ext)
                            .and_then(|t| t.get("interrupted_notice_seq"))
                            .and_then(Value::as_u64)
                            .unwrap_or(e.seq);
                        (cut(notice, 2500), format!(" Full background notice: read_transcript thread={thread} seq={seq}."))
                    }
                    None => (cut(&p.text, 3000), String::new()),
                };
                let mut line = serde_json::to_string(&format!("{fact}{hint}"))?;
                while line.len() > 3500 {
                    fact = cut(&fact, fact.len() / 2);
                    line = serde_json::to_string(&format!("{fact}{hint}"))?;
                }
                if bytes + line.len() + 1 > 3900 {
                    more = true;
                    break;
                }
                bytes += line.len() + 1;
                lines.push((e.seq, line));
                if let Some(t) = about {
                    let seq = newest.entry(t).or_default();
                    *seq = (*seq).max(e.seq);
                }
            }
        }
        lines.sort_by_key(|(seq, _)| *seq);
        let mut lines = lines.into_iter().map(|(_, line)| line).collect::<Vec<_>>();
        if more {
            lines.push(
                "\"More hub updates are available with subscribe_control or read_transcript.\""
                    .into(),
            );
        }
        // Open questions survive the recent-update watermark and burst cap. This is a
        // current-state snapshot, independent of historical updates and notice context.
        let questions = self
            .attention
            .ordered(&Default::default())
            .into_iter()
            .filter(|i| {
                i.reason == AttentionReason::Question
                    && self
                        .registry
                        .get(&i.thread)
                        .is_some_and(|v| !v.is_orchestrator())
            })
            .collect::<Vec<_>>();
        if questions.is_empty() && lines.is_empty() {
            return Ok(String::new());
        }
        let mut pending = vec![serde_json::to_string(&format!(
            "Current open worker questions: {}. These require user answers, not autonomous approval.",
            questions.len()
        ))?];
        let mut bytes = pending[0].len();
        for (n, item) in questions.iter().enumerate() {
            let line = serde_json::to_string(&json!({
                "status": "open_question",
                "attn_id": item.attn_id,
                "revision": item.revision,
                "thread_id": item.thread,
                "thread": self.registry.get(&item.thread).map(|v| &v.name),
                "summary": cut(&item.summary, 160),
                "details": "attention_list or read_transcript"
            }))?;
            if bytes + line.len() + 1 > 3500 {
                pending.push(serde_json::to_string(&format!(
                    "{} more open worker questions: use attention_list for their complete text and options.",
                    questions.len() - n
                ))?);
                break;
            }
            bytes += line.len() + 1;
            pending.push(line);
        }
        pending.extend(lines);
        Ok(pending.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Hub, HubOptions, LOCAL_BOX};
    use super::*;
    use twr_core::config::Config;

    /// A thread's routine lines go from the background facts when a later line about it is
    /// there: only the newest routine line of a thread with nothing after it stays, and no
    /// other line is left out.
    #[tokio::test]
    async fn background_facts_keep_only_a_threads_latest_routine_line() {
        let dir = tempfile::tempdir().unwrap();
        let hub = Hub::open(HubOptions::new(
            dir.path().join("twr.db"),
            Config::default(),
        ))
        .await
        .unwrap();
        hub.inner.core().orch.switching = true;
        hub.append(
            "o1",
            None,
            Source::Hub,
            EventBody::ThreadCreated(ThreadCreated {
                name: "orchestrator".into(),
                box_name: LOCAL_BOX.into(),
                runtime: "fake".into(),
                ext: twr_ext_map(json!({"role": "orchestrator"})),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        let say = |about: &str, text: &str, kind: Option<&str>| {
            hub.append(
                "o1",
                None,
                Source::Hub,
                EventBody::ConversationUpdate(ConversationUpdate {
                    text: text.into(),
                    about_thread: Some(about.into()),
                    ext: kind.and_then(|k| routine(None, k)),
                    ..Default::default()
                }),
                None,
            )
            .unwrap();
        };
        say("api", "api yielded 1", Some(ROUTINE_INTERIM));
        say("api", "api background 1", Some(ROUTINE_BACKGROUND_DONE));
        say("web", "web yielded 1", Some(ROUTINE_INTERIM));
        say("api", "api failed 2", None);
        say("api", "api failed 3", None);
        say("api", "api yielded 4", Some(ROUTINE_INTERIM));
        say("db", "db woke", Some(ROUTINE_AUTO_WAKE));
        say("db", "db background", Some(ROUTINE_BACKGROUND_DONE));
        let facts = hub.inner.core().update_context("o1").unwrap();
        let lines: Vec<&str> = facts.lines().skip(1).collect();
        assert_eq!(
            lines,
            [
                "\"web yielded 1\"",
                "\"api failed 2\"",
                "\"api failed 3\"",
                "\"api yielded 4\"",
                "\"db background\"",
            ],
            "{facts}"
        );
    }

    /// The hub's wake of a worker after its background work is mirrored into the
    /// orchestrator's conversation as a routine update; its choice not to wake it, which asks
    /// the user to send it a message, as no routine one (`routine: null`), whatever its words.
    #[tokio::test]
    async fn a_wake_the_hub_skipped_is_no_routine_update() {
        use super::super::wake::{AUTO_WAKE_KEY, AUTO_WAKE_SKIPPED_KEY};
        let dir = tempfile::tempdir().unwrap();
        let hub = Hub::open(HubOptions::new(
            dir.path().join("twr.db"),
            Config::default(),
        ))
        .await
        .unwrap();
        hub.inner.core().orch.switching = true;
        for (id, name, twr) in [
            ("o1", "orchestrator", json!({"role": "orchestrator"})),
            ("w1", "api", json!({})),
        ] {
            hub.append(
                id,
                None,
                Source::Hub,
                EventBody::ThreadCreated(ThreadCreated {
                    name: name.into(),
                    box_name: LOCAL_BOX.into(),
                    runtime: "fake".into(),
                    ext: twr_ext_map(twr),
                    ..Default::default()
                }),
                None,
            )
            .unwrap();
        }
        // As `hub::wake` records them on the worker.
        for (text, key) in [
            (
                "api's background work finished and no turn started after it: twr woke it to \
                 continue (after its interim report of turn 1).",
                AUTO_WAKE_KEY,
            ),
            (
                "api's background work finished and no turn started after it; twr did not \
                 wake it (woken 5 times in a row already). Send it a message to continue.",
                AUTO_WAKE_SKIPPED_KEY,
            ),
        ] {
            hub.append(
                "w1",
                None,
                Source::Hub,
                EventBody::ConversationUpdate(ConversationUpdate {
                    text: text.into(),
                    about_thread: Some("w1".into()),
                    ext: twr_ext_map(json!({ key: {"turn": 1} })),
                    ..Default::default()
                }),
                None,
            )
            .unwrap();
        }
        let (_, evs) = hub.thread_events("o1", 0, 100).unwrap();
        let marks: Vec<Option<Value>> = evs
            .iter()
            .filter_map(|e| match &e.body {
                EventBody::ConversationUpdate(p) => {
                    Some(twr_ext(&p.ext).and_then(|t| t.get(ROUTINE_KEY)).cloned())
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            marks,
            [Some(json!(ROUTINE_AUTO_WAKE)), Some(Value::Null)],
            "{evs:#?}"
        );
    }

    /// A batch with nothing that can be said makes no update; one that can is read with the
    /// events before it of its own thread (a task started and ended in one batch: its end is
    /// said), whatever other threads' events come between.
    #[tokio::test]
    async fn only_threads_with_something_to_say_are_read() {
        let dir = tempfile::tempdir().unwrap();
        let hub = Hub::open(HubOptions::new(
            dir.path().join("twr.db"),
            Config::default(),
        ))
        .await
        .unwrap();
        hub.inner.core().orch.switching = true;
        for (id, name, twr) in [
            ("o1", "orchestrator", json!({"role": "orchestrator"})),
            ("w1", "api", json!({})),
            ("w2", "web", json!({})),
        ] {
            hub.append(
                id,
                None,
                Source::Hub,
                EventBody::ThreadCreated(ThreadCreated {
                    name: name.into(),
                    box_name: LOCAL_BOX.into(),
                    runtime: "fake".into(),
                    ext: twr_ext_map(twr),
                    ..Default::default()
                }),
                None,
            )
            .unwrap();
        }
        let ev = |thread: &str, body: EventBody| BatchEvent {
            thread: thread.into(),
            turn: None,
            source: Source::Adapter("fake".into()),
            body,
            raw: None,
        };
        let text = || {
            EventBody::AgentText(AgentText {
                text: "hello".into(),
                ..Default::default()
            })
        };
        let task = |running| {
            EventBody::TaskUpdated(TaskUpdated {
                task_id: "bg1".into(),
                running,
                description: "build".into(),
                ..Default::default()
            })
        };
        let core = hub.inner.core();
        assert!(core
            .conversation_updates(&[ev("w1", text()), ev("w2", text())])
            .is_empty());
        let out = core.conversation_updates(&[
            ev("w1", task(true)),
            ev("w2", text()),
            ev("w2", task(true)),
            ev("w1", text()),
            ev("w1", task(false)),
        ]);
        let said: Vec<String> = out
            .iter()
            .map(|e| match &e.body {
                EventBody::ConversationUpdate(p) => p.text.clone(),
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(said, ["api background work finished."]);
    }

    /// The routine mark joins whatever `ext` had.
    #[test]
    fn the_routine_mark_keeps_the_ext() {
        let ext = twr_ext_map(json!({"background_tasks": 2}));
        let marked = routine(ext, ROUTINE_INTERIM);
        assert_eq!(
            serde_json::to_value(&marked).unwrap(),
            json!({"twr": {"background_tasks": 2, "routine": "interim"}})
        );
        assert_eq!(routine_of(&marked), Some(ROUTINE_INTERIM));
        let mut odd = JsonMap::new();
        odd.insert("twr".into(), json!("x"));
        odd.insert("other".into(), json!(1));
        let marked = routine(Some(odd), ROUTINE_AUTO_WAKE);
        assert_eq!(
            serde_json::to_value(&marked).unwrap(),
            json!({"twr": {"routine": "auto_wake"}, "other": 1})
        );
        // Marked no routine update: kept too, and no kind.
        let not = not_routine(twr_ext_map(json!({"background_tasks": 2})));
        assert_eq!(
            serde_json::to_value(&not).unwrap(),
            json!({"twr": {"background_tasks": 2, "routine": null}})
        );
        assert_eq!(routine_of(&not), None);
    }

    /// The orchestrator's line when a loop starts names what it runs on and where its
    /// settings came from.
    #[test]
    fn a_loop_start_is_said_with_its_settings() {
        use twr_core::loops::settings::{LoopSettings, Overrides, SettingsOf, SettingsSource};
        let v = twr_core::reduce::ThreadView {
            name: "api".into(),
            project: Some("api".into()),
            runtime: "claude-code".into(),
            model: Some("claude-opus-5-5".into()),
            effort: Some("high".into()),
            ..twr_core::reduce::ThreadView::new("t1")
        };
        let mut p = LoopStarted {
            profile: twr_core::loops::config::Profile::builtin("high").unwrap(),
            settings: LoopSettings::first_card(&Default::default()),
            settings_from: SettingsSource::Saved,
            settings_of: Some(SettingsOf::Workspace),
            ..Default::default()
        };
        assert_eq!(
            loop_started_text(&v, &p),
            "api started a review loop: high · implementer claude-opus-5-5 high (the thread's \
             own) · reviewer codex gpt-6.1-sol (saved settings; /review --setup changes them)"
        );
        p.settings_of = Some(SettingsOf::Project);
        p.overrides = Overrides {
            profile: Some("high".into()),
            ..Default::default()
        };
        p.settings.implementer = Some(
            twr_core::loops::config::ModelChoice::parse("claude-code/claude-sonnet-5-5").unwrap(),
        );
        assert_eq!(
            loop_started_text(&v, &p),
            "api started a review loop: high · implementer claude-sonnet-5-5 · reviewer codex \
             gpt-6.1-sol (api's settings; /review --setup changes them; this run only: profile high)"
        );
        p.settings_from = SettingsSource::SetupItem;
        assert!(loop_started_text(&v, &p).ends_with(
            "(settings chosen on its setup card, now saved; this run only: profile high)"
        ));
    }
}
