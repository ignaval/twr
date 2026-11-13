//! Scripted runs of the machine. Every step is checked against a fold of the log it wrote:
//! the state [`LoopState::fold`] rebuilds from the events is the one the steps reached.

use chrono::TimeZone;

use super::*;
use crate::event::Source;
use crate::loops::config::{Profile, ReviewerChoice};
use crate::loops::contract::Severity;
use crate::loops::settings::LoopSettings;
use crate::loops::{line, Phase};

fn at() -> DateTime<Utc> {
    Utc.timestamp_opt(1_800_000_000, 0).unwrap()
}

/// A loop being run: its state and the log of everything it recorded.
struct Run {
    state: LoopState,
    log: Vec<Event>,
}

impl Run {
    fn started(p: LoopStarted) -> (Run, Vec<Action>) {
        let (state, actions) = LoopState::start(&p).unwrap();
        let mut run = Run { state, log: vec![] };
        run.record(EventBody::LoopStarted(Box::new(p)));
        let effects = run.keep(actions);
        run.check_fold();
        (run, effects)
    }

    fn new(profile: &str) -> (Run, Vec<Action>) {
        Run::started(started(Profile::builtin(profile).unwrap(), 25))
    }

    fn record(&mut self, body: EventBody) {
        let mut ev = Event::new("t1", self.log.len() as u64 + 1, None, Source::Hub, body);
        ev.ts = at();
        self.log.push(ev);
    }

    /// Writes the records of `actions` to the log; returns the rest.
    fn keep(&mut self, actions: Vec<Action>) -> Vec<Action> {
        let mut effects = Vec::new();
        for a in actions {
            match a {
                Action::Record(body) => self.record(body),
                other => effects.push(other),
            }
        }
        effects
    }

    fn check_fold(&self) {
        let folded = LoopState::fold(&self.log).unwrap();
        assert_eq!(
            folded, self.state,
            "the fold of the log is not the stepped state"
        );
        // Applying a record again changes nothing.
        let mut again = folded.clone();
        for ev in &self.log {
            if matches!(
                ev.body,
                EventBody::LoopLedger(_) | EventBody::LoopTransition(_)
            ) {
                assert!(again.apply(ev).is_empty());
            }
        }
        assert_eq!(again, self.state);
    }

    /// Steps `input`; returns the effects (records are written to the log).
    fn step(&mut self, input: Input) -> Vec<Action> {
        let actions = self.state.step(input, at()).unwrap();
        let effects = self.keep(actions);
        self.check_fold();
        effects
    }

    fn try_step(&mut self, input: Input) -> Result<Vec<Action>, StepError> {
        let actions = self.state.step(input, at())?;
        let effects = self.keep(actions);
        self.check_fold();
        Ok(effects)
    }

    /// The thread's reply `text`, read as the step it waits for.
    fn reply(&mut self, text: &str) -> Vec<Action> {
        let input = self.state.reply_input(text).expect("waits for a reply");
        self.step(input)
    }

    fn goal(&mut self) -> Vec<Action> {
        self.reply("Gates are green.\n\n```twr-goal\nHarden the exports.\n```\n")
    }

    /// The current round's review answers `n` findings, the worktree unchanged while it ran.
    fn answer(&mut self, n: usize) -> Vec<Action> {
        self.answer_with(n, true)
    }

    /// The current round's review answers `n` findings; `unchanged`: as the hub found the
    /// worktree when the answer came.
    fn answer_with(&mut self, n: usize, unchanged: bool) -> Vec<Action> {
        let round = self.state.round.clone().unwrap();
        self.step(Input::ReviewStarted {
            round: round.clone(),
            diffs: vec![format!("diff-api-{round}.patch")],
            prompt_digest: Some("sha256:00".into()),
        });
        assert!(self.state.review_started_at.is_some());
        self.step(Input::ReviewAnswered {
            round,
            findings: findings(n),
            answer_path: None,
            usage: None,
            unchanged,
        })
    }

    /// The thread triages the current round, one verdict per finding (`fixed:new-class`,
    /// `fixed:known-site`, `fixed:follow-on`, `dismissed`, `dup:L1`).
    fn triage(&mut self, verdicts: &[&str]) -> Vec<Action> {
        let ids = self.state.round_finding_ids();
        assert_eq!(ids.len(), verdicts.len(), "one verdict per finding");
        self.reply(&triage_text(&ids, verdicts, "pass"))
    }

    /// The records of the given kind in the log, as kinds.
    fn kinds(&self) -> Vec<&str> {
        self.log.iter().map(|e| e.kind()).collect()
    }

    fn transitions(&self) -> Vec<(TransitionReason, String)> {
        self.log
            .iter()
            .filter_map(|e| match &e.body {
                EventBody::LoopTransition(t) => Some((t.reason, t.round.clone())),
                _ => None,
            })
            .collect()
    }

    /// Whether the recorded transition skipped the verification (and the state agrees).
    fn verify_skipped(&self) -> bool {
        let recorded = self
            .log
            .iter()
            .any(|e| matches!(&e.body, EventBody::LoopTransition(t) if t.verify_skipped));
        assert_eq!(
            recorded,
            self.state
                .transition
                .as_ref()
                .is_some_and(|t| t.verify_skipped)
        );
        recorded
    }

    /// The phases recorded, in order (repeats of the same phase once).
    fn phases(&self) -> Vec<Phase> {
        let mut out: Vec<Phase> = Vec::new();
        for e in &self.log {
            if let EventBody::LoopPhase(p) = &e.body {
                if p.problems.is_empty() && out.last() != Some(&p.phase) {
                    out.push(p.phase);
                }
            }
        }
        out
    }

    fn round_ids(&self) -> Vec<&str> {
        self.state.rounds.iter().map(|r| r.id.as_str()).collect()
    }

    /// Writes the report: the loop ends.
    fn report(&mut self) -> Vec<Action> {
        self.step(Input::ReportWritten {
            report_path: "/data/loops/l1/report.md".into(),
            commit: None,
        })
    }
}

fn started(profile: Profile, max_rounds_per_tier: u32) -> LoopStarted {
    LoopStarted {
        id: "l1".into(),
        kind: LoopKind::Review,
        profile,
        max_rounds_per_tier,
        settings: LoopSettings {
            profile: "high".into(),
            reviewer: ReviewerChoice::parse("codex/gpt-6.1-sol").unwrap(),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn findings(n: usize) -> Vec<Finding> {
    (0..n)
        .map(|i| Finding {
            id: String::new(),
            severity: Severity::Major,
            location: format!("src/a.rs:{}", i + 1),
            problem: format!("problem {i}"),
            fix: "fix it".into(),
        })
        .collect()
}

fn triage_text(ids: &[String], verdicts: &[&str], gates: &str) -> String {
    let items: Vec<String> = ids
        .iter()
        .zip(verdicts)
        .map(|(id, v)| match v.split_once(':') {
            Some(("fixed", class)) => {
                format!(
                    r#"{{"id": "{id}", "verdict": "fixed", "class": "{class}", "rule": "r-{id}"}}"#
                )
            }
            Some(("dup", of)) => {
                format!(r#"{{"id": "{id}", "verdict": "duplicate", "of": "{of}"}}"#)
            }
            _ => format!(r#"{{"id": "{id}", "verdict": "dismissed", "reason": "intended"}}"#),
        })
        .collect();
    format!(
        "Done.\n```twr-triage\n{{\"items\": [{}], \"gates\": {{\"ran\": [\"cargo test\"], \"result\": \"{gates}\"}}}}\n```\n",
        items.join(", ")
    )
}

fn review(round: &str, effort: &str, retry: bool) -> Action {
    Action::RunReview {
        round: round.into(),
        phase: if round.starts_with('v') {
            Phase::Verify
        } else {
            Phase::Ladder
        },
        effort: effort.into(),
        retry,
    }
}

fn ended(run: &Run) -> Outcome {
    assert!(run.state.ended);
    run.state.outcome.unwrap()
}

#[test]
fn an_implementation_loop_is_refused() {
    let mut p = started(Profile::builtin("high").unwrap(), 25);
    p.kind = LoopKind::Implement;
    assert_eq!(LoopState::start(&p).unwrap_err(), StartError::NotSupported);
    let mut events = vec![Event::new(
        "t1",
        1,
        None,
        Source::Hub,
        EventBody::LoopStarted(Box::new(p)),
    )];
    events.push(Event::new(
        "t1",
        2,
        None,
        Source::Hub,
        EventBody::LoopEnded(LoopEnded::default()),
    ));
    assert_eq!(LoopState::fold(&events), None);
}

#[test]
fn high_clean_from_the_first_round() {
    let (mut run, first) = Run::new("high");
    assert_eq!(first, [Action::AskGoal]);
    assert_eq!(run.state.phase, Phase::Scope);
    assert_eq!(line(&run.state), "high · scope");
    assert_eq!(run.goal(), [review("m1", "medium", false)]);
    assert_eq!(run.state.goal.as_deref(), Some("Harden the exports."));
    assert_eq!(line(&run.state), "high · ladder m1");
    assert_eq!(run.state.waiting_on, WaitingOn::Review);
    // The medium tier converged: the xhigh tier starts.
    assert_eq!(run.answer(0), [review("x1", "xhigh", false)]);
    // The last tier converged on nothing, nothing fixed (no sweep), at the verification's
    // effort, the worktree as x1 read it: v1 could only repeat x1. Straight to the report.
    assert_eq!(run.answer(0), [Action::AskReport]);
    assert_eq!(
        run.transitions(),
        [(TransitionReason::Converged, "x1".into())]
    );
    assert!(run.verify_skipped());
    assert_eq!(run.state.phase, Phase::Report);
    assert_eq!(run.state.outcome, Some(Outcome::Clean));
    assert_eq!(run.state.verify, VerifyProgress { done: 0, cap: 5 });
    assert_eq!(line(&run.state), "high · report");
    assert_eq!(
        run.report(),
        [Action::End {
            outcome: Outcome::Clean
        }]
    );
    assert_eq!(ended(&run), Outcome::Clean);
    assert_eq!(run.round_ids(), ["m1", "x1"]);
    assert_eq!(run.phases(), [Phase::Scope, Phase::Ladder, Phase::Report]);
    assert_eq!(
        run.kinds(),
        [
            "loop.started",
            "loop.phase",
            "loop.phase",
            "loop.review_started",
            "loop.findings",
            "loop.review_started",
            "loop.findings",
            "loop.transition",
            "loop.phase",
            "loop.ended"
        ]
    );
    assert_eq!(
        run.try_step(Input::Fail { why: "x".into() }),
        Err(StepError::Ended)
    );
}

#[test]
fn medium_findings_then_convergence_sweep_and_a_ledger_fixed_point() {
    let (mut run, _) = Run::new("medium");
    run.goal();
    let ask = run.answer(2);
    let Action::AskTriage { round, findings } = &ask[0] else {
        panic!("{ask:?}")
    };
    assert_eq!(round, "m1");
    assert_eq!(findings[1].id, "m1.2");
    assert_eq!(run.state.waiting_on, WaitingOn::Thread);
    assert_eq!(
        run.triage(&["fixed:new-class", "dismissed"]),
        [review("m2", "medium", false)]
    );
    assert_eq!(run.state.ledger, 1);
    let ledger = run
        .log
        .iter()
        .find_map(|e| match &e.body {
            EventBody::LoopLedger(l) => Some(l.entries.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(ledger[0].id, "L1");
    assert_eq!(ledger[0].from, "m1.2");
    assert_eq!(ledger[0].location, "src/a.rs:2");
    assert_eq!(ledger[0].reason, "intended");
    assert_eq!(line(&run.state), "medium · ladder m2 · ledger 1");
    // m2 converges; one tier only: the ladder ends, and it fixed something: sweep.
    assert_eq!(run.answer(0), [Action::AskSweep]);
    assert_eq!(run.state.phase, Phase::Sweep);
    assert_eq!(line(&run.state), "medium · sweep · ledger 1");
    let sweep = "```twr-sweep\n{\"rules\": [{\"rule\": \"r-m1.1\", \"result\": \"fixed\"}, \
                 {\"rule\": \"retries\", \"result\": \"accepted residual\", \"reason\": \"bounded\"}]}\n```";
    assert_eq!(run.reply(sweep), [review("v1", "xhigh", false)]);
    assert_eq!(run.state.ledger, 2);
    assert_eq!(line(&run.state), "medium · verify 1/2 · ledger 2");
    run.answer(1);
    assert_eq!(run.state.verify.done, 1);
    assert_eq!(run.triage(&["dup:L1"]), [Action::AskReport]);
    assert_eq!(run.state.converged, Some(Convergence::AllDuplicate));
    run.report();
    assert_eq!(ended(&run), Outcome::LedgerFixedPoint);
    assert_eq!(run.round_ids(), ["m1", "m2", "v1"]);
    assert_eq!(run.state.rounds[0].findings, 2);
    assert_eq!(run.state.rounds[0].new_class, 1);
}

#[test]
fn the_transition_fires_mid_tier_after_two_rounds_without_a_new_class() {
    let (mut run, _) = Run::new("high");
    run.goal();
    run.answer(1);
    run.triage(&["fixed:new-class"]);
    run.answer(2);
    // Findings, none new: one.
    assert_eq!(
        run.triage(&["fixed:known-site", "dismissed"]),
        [review("m3", "medium", false)]
    );
    assert_eq!(run.state.no_new_class, 1);
    run.answer(1);
    // Two: the ladder ends mid-tier, the xhigh tier skipped; something was fixed: sweep.
    assert_eq!(run.triage(&["fixed:follow-on"]), [Action::AskSweep]);
    assert_eq!(
        run.transitions(),
        [(TransitionReason::NoNewClass, "m3".into())]
    );
    assert!(!run.round_ids().contains(&"x1"));
}

#[test]
fn a_converging_round_neither_counts_nor_keeps_the_count() {
    let (mut run, _) = Run::new("high");
    run.goal();
    run.answer(1);
    run.triage(&["fixed:new-class"]);
    run.answer(1);
    run.triage(&["fixed:known-site"]);
    assert_eq!(run.state.no_new_class, 1);
    // An empty round converges the tier: the next tier, not the transition.
    assert_eq!(run.answer(0), [review("x1", "xhigh", false)]);
    assert_eq!(run.state.no_new_class, 0);
    run.answer(1);
    assert_eq!(
        run.triage(&["fixed:known-site"]),
        [review("x2", "xhigh", false)]
    );
    run.answer(1);
    // A round whose findings are all dismissed has no new class either.
    assert_eq!(run.triage(&["dismissed"]), [Action::AskSweep]);
    assert_eq!(
        run.transitions(),
        [(TransitionReason::NoNewClass, "x2".into())]
    );
    // A round of duplicates only converges, like an empty one.
    let (mut run, _) = Run::new("high");
    run.goal();
    run.answer(1);
    run.triage(&["dismissed"]);
    run.answer(1);
    assert_eq!(run.triage(&["dup:L1"]), [review("x1", "xhigh", false)]);
    assert_eq!(run.state.converged, Some(Convergence::AllDuplicate));
}

#[test]
fn the_backstop_and_a_tier_cap() {
    let (mut run, _) = Run::started(started(Profile::builtin("medium").unwrap(), 3));
    run.goal();
    for round in ["m2", "m3"] {
        run.answer(1);
        assert_eq!(
            run.triage(&["fixed:new-class"]),
            [review(round, "medium", false)]
        );
    }
    run.answer(1);
    assert_eq!(run.triage(&["fixed:new-class"]), [Action::AskSweep]);
    assert_eq!(
        run.transitions(),
        [(TransitionReason::Backstop, "m3".into())]
    );

    // `low`: one medium round with fixes, no sweep, one verify round at medium.
    let (mut run, _) = Run::new("low");
    run.goal();
    run.answer(2);
    assert_eq!(
        run.triage(&["fixed:new-class", "fixed:new-class"]),
        [review("v1", "medium", false)]
    );
    assert_eq!(
        run.transitions(),
        [(TransitionReason::MaxRounds, "m1".into())]
    );
    assert_eq!(line(&run.state), "low · verify 1/1");
    run.answer(1);
    // The cap is spent with findings still arriving.
    assert_eq!(run.triage(&["fixed:known-site"]), [Action::AskReport]);
    run.report();
    assert_eq!(ended(&run), Outcome::CapExpired);

    // A capped first tier hands over to the next.
    let mut two = Profile::builtin("high").unwrap();
    two.tiers[0].max_rounds = Some(1);
    let (mut run, _) = Run::started(started(two, 25));
    run.goal();
    run.answer(1);
    assert_eq!(
        run.triage(&["fixed:new-class"]),
        [review("x1", "xhigh", false)]
    );
    assert!(run.transitions().is_empty());
}

#[test]
fn verify_rounds_count_against_the_cap() {
    let (mut run, _) = Run::new("medium");
    run.goal();
    assert_eq!(run.answer(0), [review("v1", "xhigh", false)]);
    run.answer(1);
    assert_eq!(
        run.triage(&["fixed:new-class"]),
        [review("v2", "xhigh", false)]
    );
    assert_eq!(line(&run.state), "medium · verify 2/2");
    run.answer(1);
    assert_eq!(run.triage(&["dismissed"]), [Action::AskReport]);
    assert_eq!(run.state.outcome, Some(Outcome::CapExpired));
    run.report();
    assert_eq!(ended(&run), Outcome::CapExpired);
    // A verify round that converges ends it at once.
    let (mut run, _) = Run::new("medium");
    run.goal();
    run.answer(0);
    run.answer(1);
    run.triage(&["fixed:new-class"]);
    assert_eq!(run.answer(0), [Action::AskReport]);
    run.report();
    assert_eq!(ended(&run), Outcome::Clean);
}

#[test]
fn a_verification_that_could_only_repeat_the_last_round_is_skipped() {
    // `low`: m1 (medium) finds nothing; v1 would be medium too: skipped.
    let (mut run, _) = Run::new("low");
    run.goal();
    assert_eq!(run.answer(0), [Action::AskReport]);
    assert!(run.verify_skipped());
    assert_eq!(
        run.transitions(),
        [(TransitionReason::Converged, "m1".into())]
    );
    assert_eq!(run.state.verify, VerifyProgress { done: 0, cap: 1 });
    run.report();
    assert_eq!(ended(&run), Outcome::Clean);
    assert_eq!(run.round_ids(), ["m1"]);
    assert_eq!(run.phases(), [Phase::Scope, Phase::Ladder, Phase::Report]);

    // `medium`: m1 is medium, the verification xhigh: it runs.
    let (mut run, _) = Run::new("medium");
    run.goal();
    assert_eq!(run.answer(0), [review("v1", "xhigh", false)]);
    assert!(!run.verify_skipped());

    // `high`, but the worktree changed while x1 ran (a turn of the user's, a job): v1 runs.
    let (mut run, _) = Run::new("high");
    run.goal();
    run.answer(0);
    assert_eq!(run.answer_with(0, false), [review("v1", "xhigh", false)]);
    assert!(!run.verify_skipped());
    assert_eq!(run.phases(), [Phase::Scope, Phase::Ladder, Phase::Verify]);
    assert_eq!(line(&run.state), "high · verify 1/5");
    run.answer(0);
    run.report();
    assert_eq!(ended(&run), Outcome::Clean);
    assert_eq!(run.round_ids(), ["m1", "x1", "v1"]);
}

#[test]
fn the_verification_runs_after_any_other_end_of_the_ladder() {
    // An all-duplicate last round: its triage was a thread turn that could have changed
    // files, so v1 runs (at the same effort as x1).
    let (mut run, _) = Run::new("high");
    run.goal();
    run.answer(1);
    run.triage(&["dismissed"]);
    run.answer(0);
    run.answer(1);
    assert_eq!(run.triage(&["dup:L1"]), [review("v1", "xhigh", false)]);
    assert_eq!(
        run.transitions(),
        [(TransitionReason::Converged, "x1".into())]
    );
    assert!(!run.verify_skipped());
    run.answer(1);
    assert_eq!(run.triage(&["dup:L1"]), [Action::AskReport]);
    run.report();
    assert_eq!(ended(&run), Outcome::LedgerFixedPoint);

    // The last tier converged on nothing, but the ladder fixed something: the sweep, then v1.
    let (mut run, _) = Run::new("high");
    run.goal();
    run.answer(1);
    run.triage(&["fixed:new-class"]);
    run.answer(0);
    assert_eq!(run.answer(0), [Action::AskSweep]);
    assert!(!run.verify_skipped());
    let sweep =
        "```twr-sweep\n{\"rules\": [{\"rule\": \"r\", \"result\": \"checked clean\"}]}\n```";
    assert_eq!(run.reply(sweep), [review("v1", "xhigh", false)]);

    // `no_new_class` in the last tier, at the verification's effort, nothing fixed: v1 runs.
    let (mut run, _) = Run::new("high");
    run.goal();
    run.answer(0);
    for _ in 0..2 {
        run.answer(1);
        run.triage(&["dismissed"]);
    }
    assert_eq!(
        run.transitions(),
        [(TransitionReason::NoNewClass, "x2".into())]
    );
    assert!(!run.verify_skipped());
    assert_eq!(run.state.wait, Wait::Review { round: "v1".into() });

    // The backstop in the last tier, no sweep in the profile: v1 runs.
    let mut no_sweep = Profile::builtin("high").unwrap();
    no_sweep.sweep = false;
    let (mut run, _) = Run::started(started(no_sweep, 2));
    run.goal();
    run.answer(0);
    for _ in 0..2 {
        run.answer(1);
        run.triage(&["fixed:new-class"]);
    }
    assert_eq!(
        run.transitions(),
        [(TransitionReason::Backstop, "x2".into())]
    );
    assert!(!run.verify_skipped());
    assert_eq!(run.state.wait, Wait::Review { round: "v1".into() });

    // `max_rounds` (`low`, m1 with findings, fixed): v1 runs at medium.
    let (mut run, _) = Run::new("low");
    run.goal();
    run.answer(1);
    assert_eq!(
        run.triage(&["fixed:new-class"]),
        [review("v1", "medium", false)]
    );
    assert_eq!(
        run.transitions(),
        [(TransitionReason::MaxRounds, "m1".into())]
    );
    assert!(!run.verify_skipped());

    // `skipped`: a skip pending when the last tier converges on nothing still verifies.
    let (mut run, _) = Run::new("high");
    run.goal();
    run.answer(0);
    run.step(Input::Control {
        action: ControlAction::SkipToVerify,
        by: ControlBy::User,
    });
    assert_eq!(run.answer(0), [review("v1", "xhigh", false)]);
    assert_eq!(
        run.transitions(),
        [(TransitionReason::Skipped, "x1".into())]
    );
    assert!(!run.verify_skipped());
}

#[test]
fn a_failed_round_is_retried_once_then_the_loop_is_stuck() {
    let (mut run, _) = Run::new("high");
    run.goal();
    let fail = |run: &mut Run| {
        run.step(Input::ReviewFailed {
            round: "m1".into(),
            reason: "timeout".into(),
        })
    };
    assert_eq!(fail(&mut run), [review("m1", "medium", true)]);
    assert!(run.state.rounds[0].failed);
    let stuck = fail(&mut run);
    assert_eq!(
        stuck,
        [Action::Stuck {
            why: "review round m1 failed twice: timeout".into()
        }]
    );
    assert!(run.state.paused);
    assert_eq!(run.state.waiting_on, WaitingOn::User);
    assert_eq!(line(&run.state), "high · ladder m1 · stuck");
    // Never converged by failing: still round m1.
    assert_eq!(run.round_ids(), ["m1"]);
    let resumed = run.step(Input::Control {
        action: ControlAction::Resume,
        by: ControlBy::User,
    });
    assert_eq!(resumed, [review("m1", "medium", false)]);
    assert_eq!(run.state.stuck, None);
    assert_eq!(run.answer(0), [review("x1", "xhigh", false)]);
    assert!(!run.state.rounds[0].failed);
    // A late answer for a round it no longer waits for.
    assert_eq!(
        run.try_step(Input::ReviewAnswered {
            round: "m1".into(),
            findings: vec![],
            answer_path: None,
            usage: None,
            unchanged: true,
        }),
        Err(StepError::NotWaiting("review round x1".into()))
    );
}

#[test]
fn a_refused_triage_gets_one_correction_then_the_loop_is_stuck() {
    let (mut run, _) = Run::new("high");
    run.goal();
    run.answer(2);
    let ids = run.state.round_finding_ids();
    // Red gates are refused like a missing item.
    let red = triage_text(&ids, &["fixed:new-class", "dismissed"], "fail");
    let first = run.reply(&red);
    let [Action::AskCorrection { step, problems }] = &first[..] else {
        panic!("{first:?}")
    };
    assert_eq!(*step, Step::Triage);
    assert_eq!(
        problems,
        &["gates.result is fail: gates must be green before the loop goes on"]
    );
    let second = run.reply("I fixed both.");
    assert_eq!(
        second,
        [Action::Stuck {
            why: "the triage reply was refused twice: no ```twr-triage block: end your reply with one"
                .into()
        }]
    );
    assert_eq!(run.state.waiting_on, WaitingOn::User);
    // Resumed: the whole triage is asked again, with a new correction budget.
    let again = run.step(Input::Control {
        action: ControlAction::Resume,
        by: ControlBy::User,
    });
    assert!(
        matches!(&again[..], [Action::AskTriage { round, findings }] if round == "m1" && findings.len() == 2)
    );
    assert!(matches!(
        &run.reply("no block")[..],
        [Action::AskCorrection { .. }]
    ));
    assert_eq!(
        run.triage(&["fixed:new-class", "dismissed"]),
        [review("m2", "medium", false)]
    );
}

#[test]
fn a_refused_goal_and_sweep_get_one_correction() {
    let (mut run, _) = Run::new("medium");
    assert!(matches!(
        &run.reply("Gates green.")[..],
        [Action::AskCorrection {
            step: Step::Goal,
            ..
        }]
    ));
    assert!(matches!(
        &run.reply("Still none.")[..],
        [Action::Stuck { .. }]
    ));
    run.step(Input::Control {
        action: ControlAction::Resume,
        by: ControlBy::User,
    });
    assert_eq!(run.goal(), [review("m1", "medium", false)]);
    run.answer(1);
    run.triage(&["fixed:new-class"]);
    assert_eq!(run.answer(0), [Action::AskSweep]);
    let bad =
        "```twr-sweep\n{\"rules\": [{\"rule\": \"r\", \"result\": \"accepted residual\"}]}\n```";
    assert!(matches!(
        &run.reply(bad)[..],
        [Action::AskCorrection {
            step: Step::Sweep,
            ..
        }]
    ));
    let good = "```twr-sweep\n{\"rules\": [{\"rule\": \"r\", \"result\": \"checked clean\"}]}\n```";
    assert_eq!(run.reply(good), [review("v1", "xhigh", false)]);
}

#[test]
fn pause_holds_the_next_step_until_resume() {
    // In scope: the turn finishes, the first round is held.
    let (mut run, _) = Run::new("high");
    let pause = Input::Control {
        action: ControlAction::Pause,
        by: ControlBy::User,
    };
    let resume = Input::Control {
        action: ControlAction::Resume,
        by: ControlBy::User,
    };
    assert_eq!(run.step(pause.clone()), []);
    assert_eq!(line(&run.state), "high · scope · paused");
    assert_eq!(run.state.waiting_on, WaitingOn::Thread);
    assert_eq!(
        run.try_step(pause.clone()),
        Err(StepError::Control("the loop is already paused".into()))
    );
    assert_eq!(run.goal(), []);
    assert_eq!(run.state.waiting_on, WaitingOn::User);
    assert_eq!(run.step(resume.clone()), [review("m1", "medium", false)]);
    assert_eq!(
        run.try_step(resume.clone()),
        Err(StepError::Control("the loop is not paused".into()))
    );
    // In the ladder, with a review running: the triage is held.
    run.step(pause.clone());
    assert_eq!(run.answer(1), []);
    assert_eq!(run.step(resume.clone()).len(), 1);
    // In the sweep and the verification.
    assert!(matches!(
        &run.triage(&["fixed:new-class"])[..],
        [Action::RunReview { .. }]
    ));
    run.answer(0);
    assert_eq!(run.answer(0), [Action::AskSweep]);
    run.step(pause.clone());
    let sweep = "```twr-sweep\n{\"rules\": [{\"rule\": \"r\", \"result\": \"fixed\"}]}\n```";
    assert_eq!(run.reply(sweep), []);
    assert_eq!(line(&run.state), "high · verify 1/5 · paused");
    assert_eq!(run.step(resume.clone()), [review("v1", "xhigh", false)]);
    run.step(pause.clone());
    assert_eq!(run.answer(0), []);
    assert_eq!(run.state.phase, Phase::Report);
    assert_eq!(run.step(resume.clone()), [Action::AskReport]);
    // In the report: a stop ends it, stopped.
    let stop = run.step(Input::Control {
        action: ControlAction::Stop,
        by: ControlBy::User,
    });
    assert_eq!(
        stop,
        [Action::End {
            outcome: Outcome::Stopped
        }]
    );
    assert_eq!(ended(&run), Outcome::Stopped);
    assert_eq!(
        run.kinds()[run.kinds().len() - 2..],
        ["loop.control", "loop.ended"]
    );
    assert_eq!(run.try_step(resume), Err(StepError::Ended));
}

#[test]
fn stop_ends_the_loop_in_any_phase() {
    let stop = Input::Control {
        action: ControlAction::Stop,
        by: ControlBy::User,
    };
    let (mut run, _) = Run::new("high");
    assert_eq!(
        run.step(stop.clone()),
        [Action::End {
            outcome: Outcome::Stopped
        }]
    );
    let (mut run, _) = Run::new("high");
    run.goal();
    run.answer(1);
    run.step(stop.clone());
    assert_eq!(ended(&run), Outcome::Stopped);
    assert!(run.state.findings.is_empty());
    // A fold that also sees the hub's own `loop.ended` (stop then end) agrees.
    let (mut run, _) = Run::new("high");
    run.goal();
    run.record(EventBody::LoopEnded(LoopEnded {
        id: "l1".into(),
        outcome: Outcome::Stopped,
        ..Default::default()
    }));
    let folded = LoopState::fold(&run.log).unwrap();
    assert!(folded.ended);
    assert_eq!(folded.outcome, Some(Outcome::Stopped));
}

#[test]
fn skip_to_verify_in_scope_and_ladder() {
    let skip = Input::Control {
        action: ControlAction::SkipToVerify,
        by: ControlBy::User,
    };
    // In scope: after the GOAL paragraph, straight to verify.
    let (mut run, _) = Run::new("high");
    assert_eq!(run.step(skip.clone()), []);
    assert_eq!(
        run.try_step(skip.clone()),
        Err(StepError::Control(
            "the loop already skips to verify".into()
        ))
    );
    assert_eq!(run.goal(), [review("v1", "xhigh", false)]);
    assert_eq!(run.round_ids(), ["v1"]);
    assert_eq!(
        run.try_step(skip.clone()),
        Err(StepError::Control("the loop is past its ladder".into()))
    );
    // In the ladder with a round running: at its end, no sweep though it fixed something.
    let (mut run, _) = Run::new("high");
    run.goal();
    run.step(skip.clone());
    run.answer(1);
    assert_eq!(
        run.triage(&["fixed:new-class"]),
        [review("v1", "xhigh", false)]
    );
    assert_eq!(
        run.transitions(),
        [(TransitionReason::Skipped, "m1".into())]
    );
    // A round that converges with a skip pending ends the ladder too.
    let (mut run, _) = Run::new("high");
    run.goal();
    run.step(skip.clone());
    assert_eq!(run.answer(0), [review("v1", "xhigh", false)]);
    // Paused before a round ran: that round is dropped, the skip is at once.
    let (mut run, _) = Run::new("high");
    run.goal();
    run.answer(1);
    run.step(Input::Control {
        action: ControlAction::Pause,
        by: ControlBy::User,
    });
    assert_eq!(run.triage(&["fixed:new-class"]), []);
    assert_eq!(run.round_ids(), ["m1", "m2"]);
    assert_eq!(run.step(skip), []);
    assert_eq!(run.round_ids(), ["m1", "v1"]);
    assert_eq!(
        run.transitions(),
        [(TransitionReason::Skipped, "m1".into())]
    );
    assert_eq!(
        run.step(Input::Control {
            action: ControlAction::Resume,
            by: ControlBy::User
        }),
        [review("v1", "xhigh", false)]
    );
}

#[test]
fn a_failed_turn_sticks_the_loop_and_a_resume_asks_again() {
    let (mut run, _) = Run::new("high");
    run.goal();
    run.answer(1);
    let stuck = run.step(Input::TurnFailed {
        why: "the turn crashed".into(),
    });
    assert_eq!(
        stuck,
        [Action::Stuck {
            why: "the turn crashed".into()
        }]
    );
    assert_eq!(run.state.waiting_on, WaitingOn::User);
    // Once stuck, another failure of the same turn is not news.
    assert!(run
        .try_step(Input::TurnFailed {
            why: "again".into()
        })
        .is_err());
    // Not while a review runs.
    let again = run.step(Input::Control {
        action: ControlAction::Resume,
        by: ControlBy::User,
    });
    assert!(matches!(&again[..], [Action::AskTriage { .. }]));
    run.triage(&["fixed:new-class"]);
    assert_eq!(
        run.try_step(Input::TurnFailed { why: "x".into() }),
        Err(StepError::NotWaiting("review round m2".into()))
    );
    // `failed` ends it.
    assert_eq!(
        run.step(Input::Fail {
            why: "the worktree is gone".into()
        }),
        [Action::End {
            outcome: Outcome::Failed
        }]
    );
}

#[test]
fn the_ask_after_a_restart_and_reply_inputs() {
    let (mut run, _) = Run::new("high");
    assert_eq!(run.state.ask(), Some(Action::AskGoal));
    run.goal();
    assert_eq!(run.state.ask(), Some(review("m1", "medium", false)));
    assert_eq!(run.state.reply_input("anything"), None);
    run.answer(1);
    let folded = LoopState::fold(&run.log).unwrap();
    assert!(matches!(folded.ask(), Some(Action::AskTriage { .. })));
    assert_eq!(folded.round_finding_ids(), ["m1.1"]);
    // Another loop's events change nothing.
    let mut other = run.state.clone();
    let mut ev = Event::new(
        "t1",
        99,
        None,
        Source::Hub,
        EventBody::LoopControl(LoopControl {
            id: "l2".into(),
            action: ControlAction::Stop,
            ..Default::default()
        }),
    );
    ev.ts = at();
    assert!(other.apply(&ev).is_empty());
    assert_eq!(other, run.state);
}
