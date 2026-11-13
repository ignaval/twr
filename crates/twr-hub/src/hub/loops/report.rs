//! A loop as its log tells it ([`LoopLog`], read from its thread's `loop.*` events): what the
//! driver needs beyond the folded state (the bases and folder of `loop.started`, the GOAL, an
//! implementation loop's plans and implement turns, the ledger's entries, each round's findings
//! and triage, the sweep, the commit's checks) and what `report.md` says ([`report`]).

use std::fmt::Write as _;

use twr_core::event::*;
use twr_core::loops::contract::{
    render_ledger_entries, Commit, Finding, FindingClass, Gates, ImplementStatus, LedgerEntry,
    SweepItem, SweepResult, TriageItem, Verdict, PLAN_CHECK_TAG,
};
use twr_core::loops::{CommitMode, LoopKind, Outcome, TransitionReason, PLAN_CHECK_ROUND};

/// One round as the log tells it.
#[derive(Debug, Clone, Default)]
pub(in crate::hub) struct RoundLog {
    /// `m3`, `v1`.
    pub(in crate::hub) id: String,
    /// The reviewer's effort.
    pub(in crate::hub) effort: String,
    /// Its findings, numbered (empty: none, or no answer yet).
    pub(in crate::hub) findings: Vec<Finding>,
    /// It was answered.
    pub(in crate::hub) answered: bool,
    /// Why its runs failed, in order.
    pub(in crate::hub) failures: Vec<String>,
    /// Its accepted triage.
    pub(in crate::hub) triage: Vec<TriageItem>,
    /// The gates its triage reported.
    pub(in crate::hub) gates: Option<twr_core::loops::contract::Gates>,
}

/// One rule of a sweep audited by runs (`sweep = "runs"`) as the log tells it.
#[derive(Debug, Clone, Default)]
pub(in crate::hub) struct AuditLog {
    /// The rule, as the rules turn named it.
    pub(in crate::hub) rule: twr_core::loops::contract::AuditRule,
    /// Its run's id (`s2`).
    pub(in crate::hub) id: String,
    /// The reviewer's effort.
    pub(in crate::hub) effort: String,
    /// Its findings, numbered (empty: none, or no answer yet).
    pub(in crate::hub) findings: Vec<Finding>,
    /// It was answered.
    pub(in crate::hub) answered: bool,
    /// Why its runs failed, in order (two: not run).
    pub(in crate::hub) failures: Vec<String>,
    /// The answer file.
    pub(in crate::hub) answer_path: Option<String>,
}

impl AuditLog {
    /// Its run failed twice: not run.
    pub(in crate::hub) fn not_run(&self) -> bool {
        !self.answered && self.failures.len() >= 2
    }
}

/// A loop as its log tells it.
#[derive(Debug, Clone, Default)]
pub(in crate::hub) struct LoopLog {
    /// Its `loop.started`.
    pub(in crate::hub) started: Option<LoopStarted>,
    /// The GOAL paragraph.
    pub(in crate::hub) goal: Option<String>,
    /// The ledger, in order.
    pub(in crate::hub) ledger: Vec<LedgerEntry>,
    /// The rounds, in order.
    pub(in crate::hub) rounds: Vec<RoundLog>,
    /// When and why the ladder ended.
    pub(in crate::hub) transition: Option<(TransitionReason, String)>,
    /// The ladder's end skipped the verification (`loop.transition` `verify_skipped`).
    pub(in crate::hub) verify_skipped: bool,
    /// The sweep's results.
    pub(in crate::hub) sweep: Option<Vec<SweepItem>>,
    /// `sweep = "runs"`: the rules the rules turn named (`loop.rules`), with their audits, in
    /// order; `None` before it named them (or with the thread's sweep).
    pub(in crate::hub) audits: Option<Vec<AuditLog>>,
    /// Controls, as `(action, by, why)`.
    pub(in crate::hub) controls: Vec<(String, String, Option<String>)>,
    /// An implementation loop's plans (`loop.plan`), in order.
    pub(in crate::hub) plans: Vec<LoopPlanned>,
    /// An implementation loop's implement turns' blocks (`loop.implemented`), in order.
    pub(in crate::hub) implemented: Vec<(ImplementStatus, Gates)>,
    /// The commit's checks (`loop.commit`), in order, with the turn each took.
    pub(in crate::hub) commit_checks: Vec<(Option<u64>, LoopCommitChecked)>,
}

impl LoopLog {
    /// Loop `id`'s story in `events` (its thread's `loop.*` events, hydrated, oldest first).
    pub(in crate::hub) fn of(id: &str, events: &[Event]) -> LoopLog {
        let mut log = LoopLog::default();
        let round = |log: &mut LoopLog, r: &str| -> usize {
            match log.rounds.iter().position(|x| x.id == r) {
                Some(i) => i,
                None => {
                    log.rounds.push(RoundLog {
                        id: r.to_string(),
                        ..Default::default()
                    });
                    log.rounds.len() - 1
                }
            }
        };
        for e in events {
            match &e.body {
                EventBody::LoopStarted(p) if p.id == id => log.started = Some((**p).clone()),
                EventBody::LoopPhase(p) if p.id == id => {
                    if p.goal.is_some() {
                        log.goal.clone_from(&p.goal);
                    }
                    if let Some(s) = &p.sweep {
                        log.sweep = Some(s.clone());
                    }
                }
                EventBody::LoopReviewStarted(p) if p.id == id => {
                    let i = round(&mut log, &p.round);
                    log.rounds[i].effort.clone_from(&p.effort);
                }
                EventBody::LoopFindings(p) if p.id == id => {
                    let i = round(&mut log, &p.round);
                    if p.failed {
                        log.rounds[i]
                            .failures
                            .push(p.reason.clone().unwrap_or_default());
                    } else {
                        log.rounds[i].findings.clone_from(&p.findings);
                        log.rounds[i].answered = true;
                    }
                }
                EventBody::LoopTriage(p) if p.id == id && p.problems.is_empty() => {
                    let i = round(&mut log, &p.round);
                    log.rounds[i].triage.clone_from(&p.items);
                    log.rounds[i].gates.clone_from(&p.gates);
                }
                EventBody::LoopLedger(p) if p.id == id => {
                    log.ledger.extend(p.entries.iter().cloned());
                }
                EventBody::LoopTransition(p) if p.id == id => {
                    log.transition = Some((p.reason, p.round.clone()));
                    log.verify_skipped = p.verify_skipped;
                }
                EventBody::LoopRules(p) if p.id == id => {
                    log.audits = Some(
                        p.rules
                            .iter()
                            .enumerate()
                            .map(|(i, r)| AuditLog {
                                rule: r.clone(),
                                id: twr_core::loops::contract::audit_id(
                                    u32::try_from(i + 1).unwrap_or(u32::MAX),
                                ),
                                ..Default::default()
                            })
                            .collect(),
                    )
                }
                EventBody::LoopAuditStarted(p) if p.id == id => {
                    if let Some(a) = log.audit_mut(p.rule) {
                        a.effort.clone_from(&p.effort);
                    }
                }
                EventBody::LoopAudit(p) if p.id == id => {
                    if let Some(a) = log.audit_mut(p.rule) {
                        if p.failed {
                            a.failures.push(p.reason.clone().unwrap_or_default());
                        } else {
                            a.findings.clone_from(&p.findings);
                            a.answered = true;
                            a.answer_path.clone_from(&p.answer_path);
                        }
                    }
                }
                EventBody::LoopPlan(p) if p.id == id => log.plans.push(p.clone()),
                EventBody::LoopImplemented(p) if p.id == id => {
                    log.implemented.push((p.status, p.gates.clone()))
                }
                EventBody::LoopCommit(p) if p.id == id => {
                    log.commit_checks.push((e.turn, p.clone()))
                }
                EventBody::LoopControl(p) if p.id == id => log.controls.push((
                    p.action.as_str().to_string(),
                    serde_json::to_value(p.by)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_string))
                        .unwrap_or_default(),
                    p.why.clone(),
                )),
                _ => {}
            }
        }
        log
    }

    /// Rule `rule`'s audit (1-based).
    fn audit_mut(&mut self, rule: u32) -> Option<&mut AuditLog> {
        let i = usize::try_from(rule).ok()?.checked_sub(1)?;
        self.audits.as_mut()?.get_mut(i)
    }

    /// The findings this loop fixed, with the class and rule the thread gave each.
    pub(in crate::hub) fn fixed(&self) -> Vec<(Finding, String, Option<String>)> {
        let mut out = Vec::new();
        for r in &self.rounds {
            for t in r.triage.iter().filter(|t| t.verdict == Verdict::Fixed) {
                if let Some(f) = r.findings.iter().find(|f| f.id == t.id) {
                    out.push((
                        f.clone(),
                        t.class.unwrap_or_default().as_str().to_string(),
                        t.rule.clone(),
                    ));
                }
            }
        }
        out
    }

    /// The rules the thread named for fixed findings, each once, in order.
    pub(in crate::hub) fn rules(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for (_, _, rule) in self.fixed() {
            if let Some(r) = rule {
                let r = super::prompts::one_line(&r);
                if !out.contains(&r) {
                    out.push(r);
                }
            }
        }
        out
    }

    /// Review rounds answered (the plan check's not counted).
    pub(in crate::hub) fn answered_rounds(&self) -> usize {
        self.rounds
            .iter()
            .filter(|r| r.answered && r.id != PLAN_CHECK_ROUND)
            .count()
    }

    /// The loop's kind.
    pub(in crate::hub) fn kind(&self) -> LoopKind {
        self.started.as_ref().map(|s| s.kind).unwrap_or_default()
    }

    /// The turn whose reply is the loop's report text, when a commit was asked: the turn the
    /// first commit check took (the report turn's; a correction asks only for the commit).
    pub(in crate::hub) fn report_turn(&self) -> Option<u64> {
        self.commit_checks.first().and_then(|(t, _)| *t)
    }

    /// The latest plan's path.
    pub(in crate::hub) fn plan_path(&self) -> Option<String> {
        self.plans.iter().rev().find_map(|p| p.path.clone())
    }
}

/// What a loop committed, as its report and end say it: the commits, or why it did not.
#[derive(Debug, Clone, Default)]
pub(in crate::hub) struct Committed {
    /// What the loop commits ([`CommitMode`]).
    pub(in crate::hub) mode: CommitMode,
    /// The commits its check found.
    pub(in crate::hub) commits: Vec<Commit>,
    /// Its check failed twice: what is left.
    pub(in crate::hub) problem: Option<String>,
}

impl Committed {
    /// The commit in a few words, when one was asked: `committed abc1234 (/w)`, `committed
    /// nothing (there was nothing to commit)`, `not committed: …`.
    pub(in crate::hub) fn words(&self) -> Option<String> {
        if self.mode == CommitMode::None {
            return None;
        }
        // (A repository is committed or has a problem, never both: `check_commit`.)
        let mut parts = Vec::new();
        if !self.commits.is_empty() {
            parts.push(format!(
                "committed {}",
                self.commits
                    .iter()
                    .map(|c| format!("{} ({})", short_sha(&c.sha), c.repo))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if let Some(p) = &self.problem {
            parts.push(format!("not committed: {}", super::prompts::one_line(p)));
        }
        if parts.is_empty() {
            return Some("committed nothing (there was nothing to commit)".into());
        }
        Some(parts.join("; "))
    }
}

/// `sha`'s first 12 digits.
fn short_sha(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

/// `reason` in words.
pub(in crate::hub) fn transition_words(reason: TransitionReason) -> &'static str {
    match reason {
        TransitionReason::Converged => "the last tier converged",
        TransitionReason::NoNewClass => "two rounds in a row found nothing of a new class",
        TransitionReason::Backstop => "a tier reached the backstop (max_rounds_per_tier)",
        TransitionReason::MaxRounds => "the last tier ran its profile's rounds",
        TransitionReason::Skipped => "skipped to verification",
    }
}

/// `review loop` or `implementation loop`.
pub(in crate::hub) fn kind_words(kind: LoopKind) -> &'static str {
    match kind {
        LoopKind::Review => "review loop",
        LoopKind::Implement => "implementation loop",
    }
}

/// How a loop of `kind` that ended `outcome` after `rounds` answered review rounds is said:
/// `review loop clean after 6 rounds`.
pub(in crate::hub) fn outcome_words(kind: LoopKind, outcome: Outcome, rounds: usize) -> String {
    let after = match rounds {
        0 => "before any round".to_string(),
        1 => "after 1 round".to_string(),
        n => format!("after {n} rounds"),
    };
    let how = match outcome {
        Outcome::Clean => "clean",
        Outcome::LedgerFixedPoint => "at a ledger fixed point (every finding already ledgered)",
        Outcome::CapExpired => "with its verification cap spent and findings still arriving",
        Outcome::Stopped => "stopped",
        Outcome::Failed => "failed",
    };
    format!("{} {how} {after}", kind_words(kind))
}

/// The text of `report.md` for loop `log` of thread `name`, that ended `outcome`, the
/// thread's own part being `reply` (its report turn's final message, without its
/// `twr-commit` block), and its commit `committed`. An implementation loop's adds the skill's
/// Phase 5 parts: the plan check (ran or not and why, findings, plan changes, ledgered items),
/// the implement turn, and the commits (after `cap_expired`, which findings they still carry).
/// A baseline that had changes is named only as out of scope, never reported as work.
pub(in crate::hub) fn report(
    name: &str,
    log: &LoopLog,
    outcome: Outcome,
    folder: &str,
    reply: Option<&str>,
    committed: &Committed,
) -> String {
    let mut out = String::new();
    let started = log.started.clone().unwrap_or_default();
    let kind = started.kind;
    let title = match kind {
        LoopKind::Review => "Review loop report",
        LoopKind::Implement => "Implementation loop report",
    };
    let _ = writeln!(out, "# {title}: {name}\n");
    let _ = write!(
        out,
        "{}",
        capitalized(&outcome_words(kind, outcome, log.answered_rounds()))
    );
    if let Some(c) = committed.words() {
        let _ = write!(out, "; {c}");
    }
    let _ = writeln!(out, ".\n");
    let _ = writeln!(
        out,
        "- Loop: {} (profile {})",
        started.id, started.profile.name
    );
    let _ = writeln!(
        out,
        "- Reviewer: {} {}",
        started.settings.reviewer.runtime, started.settings.reviewer.model
    );
    if let Some(scope) = &started.scope {
        let _ = writeln!(out, "- Scope: {scope}");
    }
    if kind == LoopKind::Implement {
        let _ = writeln!(
            out,
            "- Task: {}",
            match started
                .task
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
            {
                Some(t) => super::prompts::one_line(&cut_words(t, 300)),
                None => "what the conversation had settled (the plan states it)".into(),
            }
        );
        if let Some(p) = log.plan_path() {
            let _ = writeln!(out, "- Plan: {p}");
        }
    }
    for b in &started.bases {
        let _ = writeln!(out, "- Repository {}: {} from {}", b.slug, b.repo, b.base);
    }
    for (slug, summary) in baseline_notes(&started) {
        let _ = writeln!(
            out,
            "- Out of scope in {slug}: what the worktree held before the loop began ({summary}; \
             {folder}/baseline-{slug}.patch)"
        );
    }
    let _ = writeln!(out, "- Files: {folder}");
    if let Some(goal) = &log.goal {
        let _ = writeln!(out, "\n## Goal\n\n{}", goal.trim());
    }
    if kind == LoopKind::Implement {
        plan_check_section(&mut out, log);
        let _ = writeln!(out, "\n## Implementation\n");
        match log.implemented.last() {
            Some((status, gates)) => {
                let _ = writeln!(
                    out,
                    "The implement turn reported {} with gates {}{}.",
                    status.as_str(),
                    gate_words(gates),
                    match log.implemented.len() {
                        1 => String::new(),
                        n => format!(
                            " (after {} refused answer{})",
                            n - 1,
                            if n == 2 { "" } else { "s" }
                        ),
                    }
                );
            }
            None => {
                let _ = writeln!(out, "The implement turn did not report.");
            }
        }
    }
    let _ = writeln!(out, "\n## Rounds\n");
    let review_rounds: Vec<&RoundLog> = log
        .rounds
        .iter()
        .filter(|r| r.id != PLAN_CHECK_ROUND)
        .collect();
    if review_rounds.is_empty() {
        let _ = writeln!(out, "No round ran.");
    }
    // Per tier: its rounds, findings and new classes.
    let mut tiers: Vec<(String, Vec<&RoundLog>)> = Vec::new();
    for r in review_rounds.iter().copied() {
        let tag: String =
            r.id.chars()
                .take_while(|c| c.is_ascii_lowercase())
                .collect();
        match tiers.iter_mut().find(|(t, _)| *t == tag) {
            Some((_, v)) => v.push(r),
            None => tiers.push((tag, vec![r])),
        }
    }
    for (tag, rounds) in &tiers {
        let findings: usize = rounds.iter().map(|r| r.findings.len()).sum();
        let new_class: usize = rounds
            .iter()
            .flat_map(|r| &r.triage)
            .filter(|t| t.verdict == Verdict::Fixed && t.class == Some(FindingClass::NewClass))
            .count();
        let effort = rounds.first().map(|r| r.effort.as_str()).unwrap_or("");
        let _ = writeln!(
            out,
            "- Tier {tag} ({effort}): {} round{}, {findings} finding{}, {new_class} of a new \
             class",
            rounds.len(),
            if rounds.len() == 1 { "" } else { "s" },
            if findings == 1 { "" } else { "s" },
        );
        for r in rounds {
            let mut verdicts = [0usize; 3];
            for t in &r.triage {
                verdicts[match t.verdict {
                    Verdict::Fixed => 0,
                    Verdict::Dismissed => 1,
                    Verdict::Duplicate => 2,
                }] += 1;
            }
            let mut line = format!("  - {}: ", r.id);
            if r.answered {
                let _ = write!(
                    line,
                    "{} finding{}",
                    r.findings.len(),
                    if r.findings.len() == 1 { "" } else { "s" }
                );
                if !r.triage.is_empty() {
                    let _ = write!(
                        line,
                        " ({} fixed, {} dismissed, {} duplicate)",
                        verdicts[0], verdicts[1], verdicts[2]
                    );
                }
            } else {
                line.push_str("no answer");
            }
            if !r.failures.is_empty() {
                let _ = write!(
                    line,
                    "; failed run{}: {}",
                    if r.failures.len() == 1 { "" } else { "s" },
                    r.failures
                        .iter()
                        .map(|f| super::prompts::one_line(f))
                        .collect::<Vec<_>>()
                        .join("; ")
                );
            }
            let _ = writeln!(out, "{line}");
        }
    }
    let _ = writeln!(out, "\n## Transition\n");
    match &log.transition {
        Some((_, round)) if round.is_empty() => {
            let _ = writeln!(out, "Skipped to verification before any ladder round.");
        }
        Some((reason, round)) => {
            let _ = write!(out, "After {round}: {}.", transition_words(*reason));
            if log.verify_skipped {
                let _ = write!(
                    out,
                    " The loop went straight to its report: a verification round could only \
                     have repeated {round}."
                );
            }
            let _ = writeln!(out);
        }
        None => {
            let _ = writeln!(out, "The ladder did not end.");
        }
    }
    let _ = writeln!(out, "\n## Sweep\n");
    let audits = log.audits.as_deref().unwrap_or_default();
    if log.audits.is_some() {
        let _ = writeln!(
            out,
            "{}",
            match audits.len() {
                0 => "Audited by runs of the reviewer's model: the thread named no rule worth an \
                      audit."
                    .to_string(),
                n => format!(
                    "Audited by runs of the reviewer's model, one per rule ({n} rule{}), the \
                     thread verifying their reports.",
                    if n == 1 { "" } else { "s" }
                ),
            }
        );
        let _ = writeln!(out);
    } else if log.sweep.as_ref().is_some_and(|r| !r.is_empty()) {
        let _ = writeln!(out, "Audited by the thread.\n");
    }
    match &log.sweep {
        Some(rules) if rules.is_empty() => {
            let _ = writeln!(out, "No rule to audit.");
        }
        Some(rules) => {
            for r in rules {
                let mut line = format!(
                    "- {}: {}",
                    super::prompts::one_line(&r.rule),
                    r.result.as_str()
                );
                // Who audited it: its run (what it found), or the thread.
                if log.audits.is_some() {
                    match audits.iter().find(|a| a.rule.name == r.rule) {
                        Some(a) if a.answered => {
                            let _ = write!(
                                line,
                                " (audit {}: {})",
                                a.id,
                                match a.findings.len() {
                                    0 => "no finding".to_string(),
                                    1 => "1 finding".to_string(),
                                    n => format!("{n} findings"),
                                }
                            );
                        }
                        Some(a) => {
                            let _ = write!(
                                line,
                                " (audit {} not run: {}; {})",
                                a.id,
                                a.failures
                                    .last()
                                    .map(|f| super::prompts::one_line(f))
                                    .unwrap_or_default(),
                                if r.result == SweepResult::NotAudited {
                                    "nobody audited it"
                                } else {
                                    "the thread audited it"
                                }
                            );
                        }
                        None => {}
                    }
                }
                if let Some(l) = &r.location {
                    let _ = write!(line, " at {}", super::prompts::one_line(l));
                }
                if let Some(why) = &r.reason {
                    let _ = write!(line, " ({})", super::prompts::one_line(why));
                }
                let _ = writeln!(out, "{line}");
            }
        }
        None if log.audits.as_ref().is_some_and(Vec::is_empty) => {
            let _ = writeln!(out, "No rule to audit.");
        }
        None => {
            let _ = writeln!(
                out,
                "Skipped: {}.",
                match &log.transition {
                    Some((TransitionReason::Skipped, _)) => "the loop skipped to verification",
                    _ if log.fixed().is_empty() => "the ladder fixed nothing",
                    _ => "the profile has no sweep",
                }
            );
        }
    }
    let _ = writeln!(out, "\n## How it ended\n");
    let verify: Vec<&RoundLog> = log
        .rounds
        .iter()
        .filter(|r| r.id.starts_with(twr_core::loops::contract::VERIFY_TAG))
        .collect();
    let how = capitalized(match outcome {
        Outcome::Clean => "clean: the last round found nothing",
        Outcome::LedgerFixedPoint => {
            "ledger fixed point: every finding of the last round was already in the ledger"
        }
        Outcome::CapExpired => "cap expired: findings were still arriving",
        Outcome::Stopped => "stopped",
        Outcome::Failed => "failed",
    });
    match &log.transition {
        Some((_, round)) if log.verify_skipped && verify.is_empty() => {
            let effort = log
                .rounds
                .iter()
                .find(|r| r.id == *round)
                .map(|r| r.effort.as_str())
                .filter(|e| !e.is_empty())
                .unwrap_or("the same");
            let _ = writeln!(
                out,
                "{how}. Verification skipped: {round}, the last tier's round, found nothing at \
                 the verification's effort ({effort}), with no sweep after it and nothing \
                 changed in the worktree since, so a verification round could only have \
                 repeated it."
            );
        }
        _ => {
            let _ = writeln!(
                out,
                "{how} after {} verification round{}.",
                verify.len(),
                if verify.len() == 1 { "" } else { "s" }
            );
        }
    }
    if outcome == Outcome::CapExpired {
        if let Some(last) = verify.last() {
            let _ = writeln!(out, "\nThe last round's findings, verbatim:\n");
            for f in &last.findings {
                let _ = writeln!(
                    out,
                    "- {} ({}) {}: {}\n  Fix: {}",
                    f.id,
                    f.severity.as_str(),
                    super::prompts::one_line(&f.location),
                    super::prompts::one_line(&f.problem),
                    super::prompts::one_line(&f.fix)
                );
            }
        }
    }
    let _ = writeln!(out, "\n## Gates\n");
    let mut any = false;
    if let Some((_, g)) = log.implemented.last() {
        any = true;
        let _ = writeln!(out, "- implement: {}", gate_words(g));
    }
    for r in &log.rounds {
        if let Some(g) = &r.gates {
            any = true;
            let _ = writeln!(
                out,
                "- {}: {} ({})",
                r.id,
                if g.ran.is_empty() {
                    "no command named".to_string()
                } else {
                    g.ran
                        .iter()
                        .map(|c| format!("`{}`", super::prompts::one_line(c).replace('`', "'")))
                        .collect::<Vec<_>>()
                        .join(", ")
                },
                match g.result {
                    twr_core::loops::contract::GateResult::Pass => "pass",
                    twr_core::loops::contract::GateResult::Fail => "fail",
                }
            );
        }
    }
    if !any {
        let _ = writeln!(out, "No triage reported gates.");
    }
    // The entries only: `ledger.md`'s title and its note to the reviewer stay in that file.
    let _ = writeln!(out, "\n## Ledger\n");
    if log.ledger.is_empty() {
        let _ = writeln!(out, "None.");
    } else {
        out.push_str(&render_ledger_entries(&log.ledger));
    }
    if committed.mode != CommitMode::None {
        let _ = writeln!(out, "\n## Commits\n");
        for c in &committed.commits {
            let _ = writeln!(out, "- {}: {}", c.repo, c.sha);
        }
        match &committed.problem {
            Some(p) => {
                if !committed.commits.is_empty() {
                    out.push('\n');
                }
                let _ = writeln!(
                    out,
                    "Not committed: {}. The loop's work there is left as it is in the worktree.",
                    super::prompts::one_line(p)
                );
            }
            None if committed.commits.is_empty() => {
                let _ = writeln!(out, "Nothing to commit: the loop left no change.");
            }
            None => {}
        }
        // What the check noted (the thread's `twr-commit` block missing, say): no problem.
        if let Some((_, last)) = log.commit_checks.last() {
            for n in &last.notes {
                let _ = writeln!(out, "\nNote: {}.", super::prompts::one_line(n));
            }
        }
        if outcome == Outcome::CapExpired && !committed.commits.is_empty() {
            if let Some(last) = verify.last().filter(|r| !r.findings.is_empty()) {
                let _ = writeln!(
                    out,
                    "\nThe commit still carries the last verification round's {} finding{}, \
                     listed under How it ended: {}.",
                    last.findings.len(),
                    if last.findings.len() == 1 { "" } else { "s" },
                    last.findings
                        .iter()
                        .map(|f| f.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
    }
    let _ = writeln!(
        out,
        "\n## {}\n",
        match kind {
            LoopKind::Review => "The thread's report",
            LoopKind::Implement => "What was built (the thread's report)",
        }
    );
    match reply.map(str::trim).filter(|r| !r.is_empty()) {
        Some(r) => {
            let _ = writeln!(out, "{r}");
        }
        None => {
            let _ = writeln!(out, "(The thread wrote nothing.)");
        }
    }
    out
}

/// The plan check's part of an implementation loop's report: whether it ran and why, its
/// findings and their triage, whether the plan changed, and what it ledgered.
fn plan_check_section(out: &mut String, log: &LoopLog) {
    let _ = writeln!(out, "\n## Plan check\n");
    let started = log.started.clone().unwrap_or_default();
    let setting = started
        .settings
        .plan_check
        .unwrap_or(started.profile.plan_check);
    let first = log.plans.iter().find(|p| p.revision == 1);
    let risk = first.map(|p| {
        format!(
            "the plan said contract risk: {} ({})",
            if p.contract_risk { "yes" } else { "no" },
            super::prompts::one_line(&p.reason)
        )
    });
    let Some(round) = log.rounds.iter().find(|r| r.id == PLAN_CHECK_ROUND) else {
        let _ = writeln!(
            out,
            "Not run: the plan check is `{}`{}.",
            setting.as_str(),
            match (&risk, first) {
                (Some(r), _) => format!(", and {r}"),
                (None, None) => ", and no plan was written".to_string(),
                (None, Some(_)) => String::new(),
            }
        );
        return;
    };
    let _ = writeln!(
        out,
        "Ran (one review at {}, round {}): the plan check is `{}`{}.",
        if round.effort.is_empty() {
            twr_core::loops::PLAN_CHECK_EFFORT
        } else {
            round.effort.as_str()
        },
        PLAN_CHECK_ROUND,
        setting.as_str(),
        risk.map(|r| format!(", and {r}")).unwrap_or_default()
    );
    if !round.answered {
        let _ = writeln!(out, "It gave no answer.");
        return;
    }
    if round.findings.is_empty() {
        let _ = writeln!(out, "It found nothing.");
    }
    for f in &round.findings {
        let verdict = round
            .triage
            .iter()
            .find(|t| t.id == f.id)
            .map(|t| match t.verdict {
                Verdict::Fixed => "fixed in the plan".to_string(),
                Verdict::Dismissed => format!(
                    "dismissed: {}",
                    super::prompts::one_line(t.reason.as_deref().unwrap_or(""))
                ),
                Verdict::Duplicate => format!(
                    "duplicate of {}",
                    t.of.as_deref()
                        .map(super::prompts::one_line)
                        .unwrap_or_default()
                ),
            })
            .unwrap_or_else(|| "not triaged".into());
        let _ = writeln!(
            out,
            "- {} ({}) {}: {} — {verdict}",
            f.id,
            f.severity.as_str(),
            super::prompts::one_line(&f.location),
            super::prompts::one_line(&f.problem)
        );
    }
    match log.plans.iter().find(|p| p.revision >= 2) {
        Some(p) => {
            let _ = writeln!(
                out,
                "\nThe plan was revised{}; the first one is kept as plan-1.md.",
                p.path
                    .as_deref()
                    .map(|path| format!(" ({path})"))
                    .unwrap_or_default()
            );
        }
        None => {
            let _ = writeln!(out, "\nThe plan did not change.");
        }
    }
    let ledgered: Vec<&LedgerEntry> = log
        .ledger
        .iter()
        .filter(|e| e.from.starts_with(PLAN_CHECK_TAG))
        .collect();
    if !ledgered.is_empty() {
        let _ = writeln!(
            out,
            "Ledgered: {}.",
            ledgered
                .iter()
                .map(|e| format!("{} ({})", e.id, e.from))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

/// `gates` in words: `` `cargo test` pass ``.
fn gate_words(gates: &Gates) -> String {
    format!(
        "{} {}",
        if gates.ran.is_empty() {
            "(no command named)".to_string()
        } else {
            gates
                .ran
                .iter()
                .map(|c| format!("`{}`", super::prompts::one_line(c).replace('`', "'")))
                .collect::<Vec<_>>()
                .join(", ")
        },
        match gates.result {
            twr_core::loops::contract::GateResult::Pass => "pass",
            twr_core::loops::contract::GateResult::Fail => "fail",
        }
    )
}

/// The baseline notes `loop.started` of an implementation loop carries
/// (`ext.twr.loop_start.baseline`): per repository slug that had changes, their summary.
pub(in crate::hub) fn baseline_notes(started: &LoopStarted) -> Vec<(String, String)> {
    let Some(b) = started
        .ext
        .as_ref()
        .and_then(|e| e.get("twr"))
        .and_then(|t| t.get(super::LOOP_START_KEY))
        .and_then(|s| s.get("baseline"))
        .and_then(serde_json::Value::as_object)
    else {
        return Vec::new();
    };
    b.iter()
        .filter_map(|(slug, v)| {
            let summary = v["summary"].as_str()?;
            (v["changed"].as_u64().unwrap_or(0) > 0).then(|| (slug.clone(), summary.to_string()))
        })
        .collect()
}

/// `s`, at most `max` bytes of it, `…` when cut.
fn cut_words(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

fn capitalized(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}
