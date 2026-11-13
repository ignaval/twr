//! A loop as its log tells it ([`LoopLog`], read from its thread's `loop.*` events): what the
//! driver needs beyond the folded state (the bases and folder of `loop.started`, the GOAL, the
//! ledger's entries, each round's findings and triage, the sweep) and what `report.md` says
//! ([`report`]).

use std::fmt::Write as _;

use twr_core::event::*;
use twr_core::loops::contract::{
    render_ledger_entries, Finding, FindingClass, LedgerEntry, SweepItem, TriageItem, Verdict,
};
use twr_core::loops::{Outcome, TransitionReason};

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
    /// Controls, as `(action, by, why)`.
    pub(in crate::hub) controls: Vec<(String, String, Option<String>)>,
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

    /// Rounds answered.
    pub(in crate::hub) fn answered_rounds(&self) -> usize {
        self.rounds.iter().filter(|r| r.answered).count()
    }
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

/// How a loop that ended `outcome` after `rounds` answered rounds is said:
/// `review loop clean after 6 rounds`.
pub(in crate::hub) fn outcome_words(outcome: Outcome, rounds: usize) -> String {
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
    format!("review loop {how} {after}")
}

/// The text of `report.md` for loop `log` of thread `name`, that ended `outcome`, the
/// thread's own part being `reply` (its report turn's final message).
pub(in crate::hub) fn report(
    name: &str,
    log: &LoopLog,
    outcome: Outcome,
    folder: &str,
    reply: Option<&str>,
) -> String {
    let mut out = String::new();
    let started = log.started.clone().unwrap_or_default();
    let _ = writeln!(out, "# Review loop report: {name}\n");
    let _ = writeln!(
        out,
        "{}.\n",
        capitalized(&outcome_words(outcome, log.answered_rounds()))
    );
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
    for b in &started.bases {
        let _ = writeln!(out, "- Repository {}: {} from {}", b.slug, b.repo, b.base);
    }
    let _ = writeln!(out, "- Files: {folder}");
    if let Some(goal) = &log.goal {
        let _ = writeln!(out, "\n## Goal\n\n{}", goal.trim());
    }
    let _ = writeln!(out, "\n## Rounds\n");
    if log.rounds.is_empty() {
        let _ = writeln!(out, "No round ran.");
    }
    // Per tier: its rounds, findings and new classes.
    let mut tiers: Vec<(String, Vec<&RoundLog>)> = Vec::new();
    for r in &log.rounds {
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
                if let Some(l) = &r.location {
                    let _ = write!(line, " at {}", super::prompts::one_line(l));
                }
                if let Some(why) = &r.reason {
                    let _ = write!(line, " ({})", super::prompts::one_line(why));
                }
                let _ = writeln!(out, "{line}");
            }
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
    let _ = writeln!(out, "\n## The thread's report\n");
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

fn capitalized(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}
