//! Scripted runs of implementation loops, and of the commit of either kind. As for review
//! loops, every step is checked against a fold of the log it wrote ([`Run::check_fold`]).

use super::*;
use crate::loops::config::{ImplCommit, PlanCheck, ReviewCommit};
use crate::loops::contract::{Commit, PLAN_BLOCK};
use crate::loops::line_forms;

/// An implementation loop's `loop.started`: profile `profile`, the settings' plan check
/// `plan_check` (`None`: the profile's) and commit `commit`.
fn started_impl(profile: &str, plan_check: Option<PlanCheck>, commit: ImplCommit) -> LoopStarted {
    let mut p = started(Profile::builtin(profile).unwrap(), 25);
    p.kind = LoopKind::Implement;
    p.task = Some("add a CSV export".into());
    p.settings.plan_check = plan_check;
    p.settings.commit = commit;
    p
}

fn implement(profile: &str) -> (Run, Vec<Action>) {
    Run::started(started_impl(profile, None, ImplCommit::End))
}

fn ireview(round: &str, effort: &str, retry: bool) -> Action {
    let Action::RunReview {
        round,
        phase,
        effort,
        retry,
        ..
    } = review(round, effort, retry)
    else {
        unreachable!()
    };
    Action::RunReview {
        round,
        phase,
        effort,
        retry,
        template: ReviewTemplate::ImplReview,
    }
}

fn plan_review(retry: bool) -> Action {
    Action::RunReview {
        round: "p1".into(),
        phase: Phase::PlanCheck,
        effort: "medium".into(),
        retry,
        template: ReviewTemplate::PlanReview,
    }
}

fn plan_text(risk: bool) -> String {
    format!(
        "## Goal\nExport reports as CSV.\n\n## Steps\n1. Add `export_csv`.\n\n```{PLAN_BLOCK}\n\
         {{\"contract_risk\": {risk}, \"reason\": \"a new endpoint\"}}\n```\n"
    )
}

fn implement_text(status: &str, gates: &str) -> String {
    format!(
        "Implemented.\n```twr-implement\n{{\"status\": \"{status}\", \"gates\": \
         {{\"ran\": [\"cargo test\"], \"result\": \"{gates}\"}}}}\n```\n"
    )
}

fn stepped(run: &mut Run, input: Input) -> Vec<Action> {
    run.step(input)
}

impl Run {
    /// The plan turn's reply `text`, the hub writing the plan to `plan.md` as it would.
    fn plan_reply(&mut self, text: &str) -> Vec<Action> {
        let input = match self.state.reply_input(text).expect("waits for the plan") {
            Input::PlanWritten { plan, path: None } => Input::PlanWritten {
                plan,
                path: Some("/data/loops/l1/plan.md".into()),
            },
            Input::PlanTriaged {
                round,
                triage,
                path: None,
            } => {
                let path = triage
                    .plan
                    .as_ref()
                    .map(|_| "/data/loops/l1/plan.md".to_string());
                Input::PlanTriaged {
                    round,
                    triage,
                    path,
                }
            }
            other => other,
        };
        stepped(self, input)
    }

    fn plan(&mut self, risk: bool) -> Vec<Action> {
        self.plan_reply(&plan_text(risk))
    }

    fn implemented(&mut self, status: &str, gates: &str) -> Vec<Action> {
        self.reply(&implement_text(status, gates))
    }

    /// The hub's check of the commit: passed (`ok`), or what is left.
    fn commit_check(&mut self, ok: bool) -> Vec<Action> {
        self.step(if ok {
            Input::CommitChecked {
                commits: vec![Commit {
                    repo: "/w".into(),
                    sha: "abc1234".into(),
                }],
                problems: vec![],
                notes: vec![],
            }
        } else {
            Input::CommitChecked {
                commits: vec![],
                problems: vec!["/w: 1 path is uncommitted beyond the baseline: a.rs".into()],
                notes: vec![],
            }
        })
    }

    fn control(&mut self, action: ControlAction) -> Vec<Action> {
        self.step(Input::Control {
            action,
            by: ControlBy::User,
        })
    }

    /// The recorded `loop.plan` revisions, in order.
    fn plans(&self) -> Vec<(u32, bool)> {
        self.log
            .iter()
            .filter_map(|e| match &e.body {
                EventBody::LoopPlan(p) => Some((p.revision, p.contract_risk)),
                _ => None,
            })
            .collect()
    }

    /// The last `loop.ended`.
    fn ended_event(&self) -> LoopEnded {
        self.log
            .iter()
            .rev()
            .find_map(|e| match &e.body {
                EventBody::LoopEnded(p) => Some(p.clone()),
                _ => None,
            })
            .unwrap()
    }
}

#[test]
fn high_without_contracts_plans_implements_reviews_and_commits() {
    let (mut run, first) = implement("high");
    assert_eq!(first, [Action::AskPlan]);
    assert_eq!(run.state.phase, Phase::Plan);
    assert_eq!(run.state.wait, Wait::Plan);
    assert_eq!(run.state.commit, CommitMode::End);
    assert_eq!(run.state.plan_check, PlanCheck::Contracts);
    assert_eq!(line(&run.state), "high · plan");
    // No contract risk, `contracts`: no plan check.
    assert_eq!(run.plan(false), [Action::AskImplement]);
    assert_eq!(run.plans(), [(1, false)]);
    assert_eq!(run.state.plan_revision, 1);
    assert_eq!(run.state.contract_risk, Some(false));
    assert_eq!(line(&run.state), "high · implementing");
    assert_eq!(run.state.waiting_on, WaitingOn::Thread);
    // Done and green: the ladder, on the impl-review template.
    assert_eq!(
        run.implemented("done", "pass"),
        [ireview("m1", "medium", false)]
    );
    assert_eq!(run.state.review_template(), ReviewTemplate::ImplReview);
    assert_eq!(line(&run.state), "high · ladder m1");
    assert_eq!(run.answer(0), [ireview("x1", "xhigh", false)]);
    // The verification skip holds for an implementation loop too.
    assert_eq!(run.answer(0), [Action::AskReport]);
    assert!(run.verify_skipped());
    assert!(run.state.asks_commit());
    // The commit passes: the hub writes the report, and the loop ends with the commit.
    assert_eq!(run.commit_check(true), [Action::WriteReport]);
    assert_eq!(run.state.wait, Wait::WriteReport);
    assert_eq!(
        run.report(),
        [Action::End {
            outcome: Outcome::Clean
        }]
    );
    assert_eq!(ended(&run), Outcome::Clean);
    let e = run.ended_event();
    assert_eq!(e.commits.len(), 1);
    assert_eq!(e.commits[0].sha, "abc1234");
    assert_eq!(e.commit_problem, None);
    assert_eq!(run.state.commits, e.commits);
    assert_eq!(
        run.phases(),
        [Phase::Plan, Phase::Implement, Phase::Ladder, Phase::Report]
    );
    assert_eq!(
        run.kinds(),
        [
            "loop.started",
            "loop.phase",
            "loop.plan",
            "loop.phase",
            "loop.implemented",
            "loop.phase",
            "loop.review_started",
            "loop.findings",
            "loop.review_started",
            "loop.findings",
            "loop.transition",
            "loop.phase",
            "loop.commit",
            "loop.ended"
        ]
    );
    assert_eq!(run.round_ids(), ["m1", "x1"]);
}

#[test]
fn medium_implements_then_fixes_sweeps_and_verifies_as_a_review_loop() {
    let (mut run, _) = implement("medium");
    run.plan(false);
    run.implemented("done", "pass");
    assert!(matches!(&run.answer(1)[..], [Action::AskTriage { round, .. }] if round == "m1"));
    assert_eq!(
        run.triage(&["fixed:new-class"]),
        [ireview("m2", "medium", false)]
    );
    assert_eq!(run.answer(0), [Action::AskSweep]);
    let sweep =
        "```twr-sweep\n{\"rules\": [{\"rule\": \"r\", \"result\": \"checked clean\"}]}\n```";
    assert_eq!(run.reply(sweep), [ireview("v1", "xhigh", false)]);
    assert_eq!(line(&run.state), "medium · verify 1/2");
    assert_eq!(run.answer(0), [Action::AskReport]);
    run.commit_check(true);
    run.report();
    assert_eq!(ended(&run), Outcome::Clean);
    assert_eq!(
        run.phases(),
        [
            Phase::Plan,
            Phase::Implement,
            Phase::Ladder,
            Phase::Sweep,
            Phase::Verify,
            Phase::Report
        ]
    );
}

#[test]
fn the_plan_check_runs_by_its_setting_and_the_plans_risk() {
    // `contracts` (high's), and the plan touches a contract: one round p1 at medium.
    let (mut run, _) = implement("high");
    assert_eq!(run.plan(true), [plan_review(false)]);
    assert_eq!(run.state.phase, Phase::PlanCheck);
    assert_eq!(run.state.review_template(), ReviewTemplate::PlanReview);
    assert_eq!(run.state.waiting_on, WaitingOn::Review);
    assert_eq!(line(&run.state), "high · plan check");
    assert_eq!(
        line_forms(&run.state),
        [
            "high · plan check".to_string(),
            "plan check".into(),
            "p1".into()
        ]
    );
    // Nothing found: on to implement; the plan check's round stays in the rounds.
    assert_eq!(run.answer(0), [Action::AskImplement]);
    assert_eq!(run.round_ids(), ["p1"]);
    assert_eq!(
        run.phases(),
        [Phase::Plan, Phase::PlanCheck, Phase::Implement]
    );
    // The first ladder round is m1 all the same.
    assert_eq!(
        run.implemented("done", "pass"),
        [ireview("m1", "medium", false)]
    );

    // The settings' plan check overrides the profile's: `always` on a plan with no risk.
    let (mut run, _) = Run::started(started_impl(
        "high",
        Some(PlanCheck::Always),
        ImplCommit::End,
    ));
    assert_eq!(run.state.plan_check, PlanCheck::Always);
    assert_eq!(run.plan(false), [plan_review(false)]);
    // `never` on a plan with one.
    let (mut run, _) = Run::started(started_impl(
        "high",
        Some(PlanCheck::Never),
        ImplCommit::End,
    ));
    assert_eq!(run.plan(true), [Action::AskImplement]);
    // `low`'s own is `never`; the settings' `always` wins over it.
    let (mut run, _) = implement("low");
    assert_eq!(run.plan(true), [Action::AskImplement]);
    let (mut run, _) = Run::started(started_impl(
        "low",
        Some(PlanCheck::Always),
        ImplCommit::End,
    ));
    assert_eq!(run.plan(false), [plan_review(false)]);
}

#[test]
fn the_plan_check_triage_ledgers_and_revises_the_plan_once() {
    let (mut run, _) = implement("high");
    run.plan(true);
    let ask = run.answer(2);
    let [Action::AskPlanTriage { round, findings }] = &ask[..] else {
        panic!("{ask:?}")
    };
    assert_eq!(round, "p1");
    assert_eq!(findings[0].id, "p1.1");
    assert_eq!(findings[1].id, "p1.2");
    assert_eq!(run.state.ask(), Some(ask[0].clone()));
    // A `fixed` item without the revised plan is refused (once).
    let items = "{\"items\": [{\"id\": \"p1.1\", \"verdict\": \"fixed\"}, {\"id\": \"p1.2\", \
                 \"verdict\": \"dismissed\", \"reason\": \"the API is versioned\"}]}";
    let bare = format!("Triaged.\n```twr-triage\n{items}\n```\n");
    let refused = run.plan_reply(&bare);
    let [Action::AskCorrection { step, problems }] = &refused[..] else {
        panic!("{refused:?}")
    };
    assert_eq!(*step, Step::Triage);
    assert!(problems[0].contains("p1.1 fixed, but no ```twr-plan block"));
    // With the revised plan (no gates, no class): `loop.plan` revision 2, then the triage; the
    // dismissal goes into the ledger; then the implement turn.
    let revised = format!(
        "## Goal\nExport reports as CSV, versioned.\n\n```twr-triage\n{items}\n```\n\
         ```{PLAN_BLOCK}\n{{\"contract_risk\": true, \"reason\": \"a new endpoint\"}}\n```\n"
    );
    assert_eq!(run.plan_reply(&revised), [Action::AskImplement]);
    assert_eq!(run.plans(), [(1, true), (2, true)]);
    assert_eq!(run.state.plan_revision, 2);
    assert_eq!(run.state.ledger, 1);
    assert_eq!(run.state.ledger_ids(), ["L1"]);
    let kinds = run.kinds();
    assert_eq!(
        kinds[kinds.len() - 4..],
        ["loop.plan", "loop.triage", "loop.ledger", "loop.phase"]
    );
    // No second plan round, and the plan check's fixes do not call for a sweep.
    run.implemented("done", "pass");
    run.answer(0);
    assert_eq!(run.answer(0), [Action::AskReport]);
    assert_eq!(run.round_ids(), ["p1", "m1", "x1"]);
    assert_eq!(run.state.fixed, 0);

    // A triage without a plan change keeps the plan; a ladder-shaped triage is not it.
    let (mut run, _) = implement("high");
    run.plan(true);
    run.answer(1);
    let ids = run.state.round_finding_ids();
    assert!(matches!(
        run.try_step(Input::TriageAccepted {
            round: "p1".into(),
            triage: Triage::default()
        }),
        Err(StepError::NotWaiting(_))
    ));
    assert_eq!(
        run.reply(&triage_text(&ids, &["dismissed"], "pass")),
        [Action::AskImplement]
    );
    assert_eq!(run.plans(), [(1, true)]);
}

#[test]
fn a_failed_plan_check_run_is_retried_once_then_the_loop_is_stuck() {
    let (mut run, _) = implement("high");
    run.plan(true);
    let fail = |run: &mut Run| {
        run.step(Input::ReviewFailed {
            round: "p1".into(),
            reason: "timeout".into(),
        })
    };
    assert_eq!(fail(&mut run), [plan_review(true)]);
    assert_eq!(
        fail(&mut run),
        [Action::Stuck {
            why: "review round p1 failed twice: timeout".into()
        }]
    );
    assert_eq!(line(&run.state), "high · plan check · stuck");
    assert_eq!(run.control(ControlAction::Resume), [plan_review(false)]);
    assert_eq!(run.answer(0), [Action::AskImplement]);
}

#[test]
fn a_refused_plan_gets_one_correction_then_the_loop_is_stuck() {
    let (mut run, _) = implement("medium");
    let first = run.plan_reply("Here is my plan, no block.");
    let [Action::AskCorrection { step, problems }] = &first[..] else {
        panic!("{first:?}")
    };
    assert_eq!(*step, Step::Plan);
    assert_eq!(step.block(), PLAN_BLOCK);
    assert_eq!(problems, &["no ```twr-plan block: end your reply with one"]);
    assert!(matches!(
        &run.plan_reply("```twr-plan\n{\"contract_risk\": \"maybe\"}\n```")[..],
        [Action::Stuck { why }] if why.starts_with("the plan reply was refused twice: ")
    ));
    assert_eq!(run.state.waiting_on, WaitingOn::User);
    assert_eq!(line(&run.state), "medium · plan · stuck");
    assert_eq!(run.control(ControlAction::Resume), [Action::AskPlan]);
    assert_eq!(run.plan(false), [Action::AskImplement]);
}

#[test]
fn implement_partial_or_red_gets_one_correction_then_the_loop_is_stuck() {
    let (mut run, _) = implement("high");
    run.plan(false);
    let first = run.implemented("partial", "pass");
    assert_eq!(
        first,
        [Action::AskCorrection {
            step: Step::Implement,
            problems: vec![
                "status is partial: finish the plan; gates must be green before review".into()
            ],
        }]
    );
    // Recorded, refused or not.
    assert!(run.kinds().contains(&"loop.implemented"));
    assert_eq!(
        run.implemented("done", "fail"),
        [Action::Stuck {
            why: "the implement reply was refused twice: gates.result is fail: gates must be \
                  green before review"
                .into()
        }]
    );
    assert_eq!(line(&run.state), "high · implementing · stuck");
    assert_eq!(run.control(ControlAction::Resume), [Action::AskImplement]);
    // A reply without the block is refused as well.
    assert!(matches!(
        &run.reply("All done!")[..],
        [Action::AskCorrection {
            step: Step::Implement,
            ..
        }]
    ));
    assert_eq!(
        run.implemented("done", "pass"),
        [ireview("m1", "medium", false)]
    );
    assert_eq!(run.phases().last(), Some(&Phase::Ladder));
}

#[test]
fn pause_holds_each_new_phase_and_a_failed_turn_sticks_it() {
    // The plan: the turn finishes, the plan check is held.
    let (mut run, _) = implement("high");
    run.control(ControlAction::Pause);
    assert_eq!(line(&run.state), "high · plan · paused");
    assert_eq!(run.plan(true), []);
    assert_eq!(run.state.phase, Phase::PlanCheck);
    assert_eq!(run.state.waiting_on, WaitingOn::User);
    assert_eq!(run.control(ControlAction::Resume), [plan_review(false)]);
    // The plan check: its run finishes, the triage is held.
    run.control(ControlAction::Pause);
    assert_eq!(run.answer(1), []);
    assert!(matches!(
        &run.control(ControlAction::Resume)[..],
        [Action::AskPlanTriage { round, .. }] if round == "p1"
    ));
    run.control(ControlAction::Pause);
    let ids = run.state.round_finding_ids();
    assert_eq!(run.reply(&triage_text(&ids, &["dismissed"], "pass")), []);
    assert_eq!(run.state.phase, Phase::Implement);
    assert_eq!(run.control(ControlAction::Resume), [Action::AskImplement]);
    // The implement turn: it finishes, m1 is held.
    run.control(ControlAction::Pause);
    assert_eq!(run.implemented("done", "pass"), []);
    assert_eq!(
        run.control(ControlAction::Resume),
        [ireview("m1", "medium", false)]
    );

    // A failed turn in the plan, the plan check's triage or the implement turn sticks the
    // loop; a resume asks for that turn again.
    let (mut run, _) = implement("high");
    let failed = |run: &mut Run| {
        run.step(Input::TurnFailed {
            why: "the turn crashed".into(),
        })
    };
    assert!(matches!(&failed(&mut run)[..], [Action::Stuck { .. }]));
    assert_eq!(run.control(ControlAction::Resume), [Action::AskPlan]);
    run.plan(true);
    // (Not while the plan check's run runs.)
    assert!(run.try_step(Input::TurnFailed { why: "x".into() }).is_err());
    run.answer(1);
    assert!(matches!(&failed(&mut run)[..], [Action::Stuck { .. }]));
    assert!(matches!(
        &run.control(ControlAction::Resume)[..],
        [Action::AskPlanTriage { .. }]
    ));
    let ids = run.state.round_finding_ids();
    run.reply(&triage_text(&ids, &["dismissed"], "pass"));
    assert!(matches!(&failed(&mut run)[..], [Action::Stuck { .. }]));
    assert_eq!(run.control(ControlAction::Resume), [Action::AskImplement]);
}

#[test]
fn stop_ends_an_implementation_loop_in_each_new_phase_without_a_commit() {
    let stop = |run: &mut Run| run.control(ControlAction::Stop);
    let (mut run, _) = implement("high");
    assert_eq!(
        stop(&mut run),
        [Action::End {
            outcome: Outcome::Stopped
        }]
    );
    let (mut run, _) = implement("high");
    run.plan(true);
    stop(&mut run);
    assert_eq!(ended(&run), Outcome::Stopped);
    let (mut run, _) = implement("high");
    run.plan(true);
    run.answer(2);
    stop(&mut run);
    assert!(run.state.findings.is_empty());
    let (mut run, _) = implement("high");
    run.plan(false);
    stop(&mut run);
    assert_eq!(ended(&run), Outcome::Stopped);
    assert!(!run.state.asks_commit());
    assert_eq!(run.ended_event().commits, vec![]);
}

#[test]
fn skip_to_verify_is_refused_until_the_plan_is_implemented() {
    let skip = Input::Control {
        action: ControlAction::SkipToVerify,
        by: ControlBy::User,
    };
    let refused = Err(StepError::Control(
        "the loop has not implemented its plan yet: skip to verify comes after it".into(),
    ));
    let (mut run, _) = implement("high");
    assert_eq!(run.try_step(skip.clone()), refused);
    run.plan(true);
    assert_eq!(run.try_step(skip.clone()), refused);
    run.answer(1);
    assert_eq!(run.try_step(skip.clone()), refused);
    let ids = run.state.round_finding_ids();
    run.reply(&triage_text(&ids, &["dismissed"], "pass"));
    assert_eq!(run.try_step(skip.clone()), refused);
    // In the ladder, as a review loop's; paused before m1 ran, m1 is dropped and the ladder's
    // last round is none (never the plan check's p1).
    run.control(ControlAction::Pause);
    assert_eq!(run.implemented("done", "pass"), []);
    assert_eq!(run.round_ids(), ["p1", "m1"]);
    assert_eq!(run.step(skip.clone()), []);
    assert_eq!(run.round_ids(), ["p1", "v1"]);
    assert_eq!(
        run.transitions(),
        [(TransitionReason::Skipped, String::new())]
    );
    assert_eq!(
        run.control(ControlAction::Resume),
        [ireview("v1", "xhigh", false)]
    );
    assert_eq!(
        run.try_step(skip),
        Err(StepError::Control("the loop is past its ladder".into()))
    );
}

#[test]
fn a_restart_asks_again_for_each_new_step() {
    let (mut run, _) = implement("high");
    let asks = |run: &Run| LoopState::fold(&run.log).unwrap().ask();
    assert_eq!(asks(&run), Some(Action::AskPlan));
    run.plan(true);
    assert_eq!(asks(&run), Some(plan_review(false)));
    run.answer(1);
    assert!(
        matches!(asks(&run), Some(Action::AskPlanTriage { findings, .. }) if findings.len() == 1)
    );
    let ids = run.state.round_finding_ids();
    run.reply(&triage_text(&ids, &["dismissed"], "pass"));
    assert_eq!(asks(&run), Some(Action::AskImplement));
    run.implemented("done", "pass");
    run.answer(0);
    run.answer(0);
    assert_eq!(asks(&run), Some(Action::AskReport));
    run.commit_check(false);
    assert!(matches!(
        asks(&run),
        Some(Action::AskCorrection {
            step: Step::Commit,
            problems
        }) if problems.len() == 1
    ));
    run.commit_check(true);
    assert_eq!(asks(&run), Some(Action::WriteReport));
    run.report();
    assert_eq!(asks(&run), None);
    // The reply a wait parses, or none: the report and its commit are the hub's to read.
    let (run, _) = implement("high");
    assert!(matches!(
        run.state.reply_input(&plan_text(false)),
        Some(Input::PlanWritten { path: None, .. })
    ));
    let mut report = run.state.clone();
    report.wait = Wait::Report;
    assert_eq!(report.reply_input("anything"), None);
}

#[test]
fn a_commit_check_failing_once_gets_one_correction() {
    let (mut run, _) = implement("low");
    run.plan(false);
    run.implemented("done", "pass");
    assert_eq!(run.answer(0), [Action::AskReport]);
    let first = run.commit_check(false);
    assert_eq!(
        first,
        [Action::AskCorrection {
            step: Step::Commit,
            problems: vec!["/w: 1 path is uncommitted beyond the baseline: a.rs".into()],
        }]
    );
    assert_eq!(run.state.wait, Wait::Commit);
    assert!(run.state.asks_commit());
    // A late report write is not what it waits for.
    assert!(run
        .try_step(Input::ReportWritten {
            report_path: "/r.md".into()
        })
        .is_err());
    assert_eq!(run.commit_check(true), [Action::WriteReport]);
    run.report();
    assert_eq!(ended(&run), Outcome::Clean);
    let e = run.ended_event();
    assert_eq!(e.commits.len(), 1);
    assert_eq!(e.commit_problem, None);
}

#[test]
fn a_commit_check_failing_twice_ends_the_loop_with_its_outcome_and_the_problem() {
    let (mut run, _) = implement("low");
    run.plan(false);
    run.implemented("done", "pass");
    run.answer(0);
    run.commit_check(false);
    // The second failure holds nothing: the report is written, the problem recorded.
    assert_eq!(run.commit_check(false), [Action::WriteReport]);
    assert_eq!(
        run.state.commit_problem.as_deref(),
        Some("/w: 1 path is uncommitted beyond the baseline: a.rs")
    );
    assert_eq!(run.state.stuck, None);
    run.report();
    assert_eq!(ended(&run), Outcome::Clean);
    let e = run.ended_event();
    assert_eq!(e.commits, vec![]);
    assert_eq!(e.commit_problem, run.state.commit_problem);
}

#[test]
fn a_paused_or_failed_commit_correction_is_asked_again() {
    let (mut run, _) = implement("low");
    run.plan(false);
    run.implemented("done", "pass");
    run.answer(0);
    // Paused: the correction is held; a resume sends it.
    run.control(ControlAction::Pause);
    assert_eq!(run.commit_check(false), []);
    assert!(matches!(
        &run.control(ControlAction::Resume)[..],
        [Action::AskCorrection {
            step: Step::Commit,
            ..
        }]
    ));
    // Its turn fails: stuck; a resume asks for the commit again (with a fresh budget).
    assert!(matches!(
        &run.step(Input::TurnFailed { why: "x".into() })[..],
        [Action::Stuck { .. }]
    ));
    assert!(matches!(
        &run.control(ControlAction::Resume)[..],
        [Action::AskCorrection {
            step: Step::Commit,
            ..
        }]
    ));
    assert!(matches!(
        &run.commit_check(false)[..],
        [Action::AskCorrection { .. }]
    ));
    // A pause never holds the report's writing.
    run.control(ControlAction::Pause);
    assert_eq!(run.commit_check(true), [Action::WriteReport]);
    run.report();
    assert_eq!(ended(&run), Outcome::Clean);
}

#[test]
fn per_round_and_none_commits() {
    let (mut run, _) = Run::started(started_impl("low", None, ImplCommit::PerRound));
    assert_eq!(run.state.commit, CommitMode::PerRound);
    run.plan(false);
    run.implemented("done", "pass");
    run.answer(0);
    assert!(run.state.asks_commit());
    assert_eq!(run.commit_check(true), [Action::WriteReport]);
    run.report();
    assert_eq!(run.ended_event().commits.len(), 1);

    // `none`: the report is written from the report turn, nothing is checked.
    let (mut run, _) = Run::started(started_impl("low", None, ImplCommit::None));
    assert_eq!(run.state.commit, CommitMode::None);
    run.plan(false);
    run.implemented("done", "pass");
    assert_eq!(run.answer(0), [Action::AskReport]);
    assert!(!run.state.asks_commit());
    assert!(matches!(
        run.try_step(Input::CommitChecked {
            commits: vec![],
            problems: vec![],
            notes: vec![],
        }),
        Err(StepError::NotWaiting(_))
    ));
    assert_eq!(
        run.report(),
        [Action::End {
            outcome: Outcome::Clean
        }]
    );
    assert!(!run.kinds().contains(&"loop.commit"));
}

#[test]
fn cap_expired_still_commits_and_stopped_or_failed_never_does() {
    let (mut run, _) = implement("low");
    run.plan(false);
    run.implemented("done", "pass");
    run.answer(1);
    run.triage(&["fixed:new-class"]);
    run.answer(1);
    assert_eq!(run.triage(&["fixed:known-site"]), [Action::AskReport]);
    assert_eq!(run.state.outcome, Some(Outcome::CapExpired));
    assert!(run.state.asks_commit());
    run.commit_check(true);
    run.report();
    assert_eq!(ended(&run), Outcome::CapExpired);
    assert_eq!(run.ended_event().commits.len(), 1);

    // Stopped in the report, with the commit asked: it ends stopped, and a check that comes
    // after it is not taken.
    let (mut run, _) = implement("low");
    run.plan(false);
    run.implemented("done", "pass");
    run.answer(0);
    assert!(run.state.asks_commit());
    run.control(ControlAction::Stop);
    assert_eq!(ended(&run), Outcome::Stopped);
    assert!(!run.state.asks_commit());
    assert_eq!(
        run.try_step(Input::CommitChecked {
            commits: vec![],
            problems: vec![],
            notes: vec![],
        }),
        Err(StepError::Ended)
    );
    // Failed: likewise.
    let (mut run, _) = implement("low");
    run.plan(false);
    run.implemented("done", "pass");
    run.answer(0);
    run.step(Input::Fail {
        why: "the worktree is gone".into(),
    });
    assert_eq!(ended(&run), Outcome::Failed);
    assert!(!run.state.asks_commit());
}

#[test]
fn a_review_loop_commits_its_fixes_only_as_match_decided() {
    let review_loop = |commit_fixes: bool, commit: ReviewCommit| {
        let mut p = started(Profile::builtin("low").unwrap(), 25);
        p.commit_fixes = commit_fixes;
        p.settings.review_commit = commit;
        Run::started(p).0
    };
    // `match`, and the thread had been committing: the report turn commits the fixes.
    let mut run = review_loop(true, ReviewCommit::Match);
    assert_eq!(run.state.commit, CommitMode::End);
    run.goal();
    assert_eq!(run.answer(0), [Action::AskReport]);
    assert!(run.state.asks_commit());
    assert_eq!(run.commit_check(true), [Action::WriteReport]);
    run.report();
    assert_eq!(run.ended_event().commits.len(), 1);
    // `match`, and it had not: nothing to commit, as milestone 1's logs read too.
    let mut run = review_loop(false, ReviewCommit::Match);
    assert_eq!(run.state.commit, CommitMode::None);
    run.goal();
    run.answer(0);
    assert!(!run.state.asks_commit());
    run.report();
    assert_eq!(ended(&run), Outcome::Clean);
    // `none` never commits, whatever the start found.
    let run = review_loop(true, ReviewCommit::None);
    assert_eq!(run.state.commit, CommitMode::None);
    // The decision itself.
    assert!(ReviewCommit::Match.commits_fixes(true, true));
    assert!(!ReviewCommit::Match.commits_fixes(true, false));
    assert!(!ReviewCommit::Match.commits_fixes(false, true));
    assert!(!ReviewCommit::None.commits_fixes(true, true));
}

#[test]
fn the_loop_line_of_an_implementation_loop() {
    let (mut run, _) = implement("high");
    assert_eq!(
        line_forms(&run.state),
        ["high · plan".to_string(), "plan".into(), "plan".into()]
    );
    run.plan(false);
    assert_eq!(
        line_forms(&run.state),
        [
            "high · implementing".to_string(),
            "implementing".into(),
            "impl".into()
        ]
    );
    run.control(ControlAction::Pause);
    assert_eq!(
        line_forms(&run.state),
        [
            "high · implementing · paused".to_string(),
            "implementing · paused".into(),
            "impl · paused".into()
        ]
    );
    run.control(ControlAction::Resume);
    run.implemented("done", "pass");
    run.answer(1);
    run.triage(&["dismissed"]);
    // Then as a review loop's.
    assert_eq!(
        line_forms(&run.state),
        [
            "high · ladder m2 · ledger 1".to_string(),
            "ladder m2 · ledger 1".into(),
            "m2 · ldg 1".into()
        ]
    );
    assert_eq!(crate::loops::badge(run.state.kind), "IMPLEMENT LOOP");
}

/// Logs written by milestone 1 still read: a `loop.started` with no `commit_fixes` (no commit)
/// and a `loop.ended` with its old `commit` key.
#[test]
fn milestone_one_logs_still_fold() {
    let body = |v: serde_json::Value| -> EventBody { serde_json::from_value(v).unwrap() };
    let started = Event::new(
        "t1",
        1,
        None,
        Source::Hub,
        body(serde_json::json!({
            "kind": "loop.started",
            "data": {"id": "l1", "kind": "review", "profile": Profile::builtin("low").unwrap(),
                     "max_rounds_per_tier": 25,
                     "settings": {"profile": "low",
                                  "reviewer": {"runtime": "codex", "model": "gpt-6.1-sol"},
                                  "review_commit": "match"},
                     "scope": "uncommitted", "bases": [], "task": null, "folder": null}
        })),
    );
    let ended = Event::new(
        "t1",
        2,
        None,
        Source::Hub,
        body(serde_json::json!({
            "kind": "loop.ended",
            "data": {"id": "l1", "outcome": "stopped", "report_path": null, "commit": null,
                     "reason": null}
        })),
    );
    let s = LoopState::fold([&started]).unwrap();
    assert_eq!(s.commit, CommitMode::None);
    assert_eq!(s.phase, Phase::Scope);
    let s = LoopState::fold([&started, &ended]).unwrap();
    assert!(s.ended);
    assert_eq!(s.commits, vec![]);
    assert_eq!(s.commit_problem, None);
}
