//! A single pass (`loop.started.once`): scope, one round at the profile's strongest tier, its
//! triage, the report; never a second round, a sweep or a verification.

use super::*;
use crate::loops::config::ReviewCommit;

fn once(profile: &str) -> (Run, Vec<Action>) {
    let mut p = started(Profile::builtin(profile).unwrap(), 25);
    p.once = true;
    Run::started(p)
}

#[test]
fn a_clean_pass_reports_clean_after_its_one_round() {
    let (mut run, first) = once("high");
    assert_eq!(first, [Action::AskGoal]);
    assert!(run.state.once);
    assert_eq!(line(&run.state), "high · single pass · scope");
    assert_eq!(
        crate::loops::line_forms(&run.state)[2],
        "pass · scope",
        "the short form"
    );
    // `high`'s strongest tier is xhigh: the one round runs there.
    assert_eq!(run.goal(), [review("x1", "xhigh", false)]);
    assert_eq!(line(&run.state), "high · single pass · x1");
    assert_eq!(crate::loops::line_forms(&run.state)[2], "pass x1");
    assert_eq!(run.answer(0), [Action::AskReport]);
    assert_eq!(run.state.outcome, Some(Outcome::Clean));
    assert_eq!(run.state.verify, VerifyProgress { done: 0, cap: 0 });
    assert_eq!(line(&run.state), "high · single pass · report");
    assert_eq!(
        run.report(),
        [Action::End {
            outcome: Outcome::Clean
        }]
    );
    assert_eq!(ended(&run), Outcome::Clean);
    assert_eq!(run.round_ids(), ["x1"]);
    assert_eq!(run.phases(), [Phase::Scope, Phase::Ladder, Phase::Report]);
    // No transition, sweep or verification was recorded.
    assert!(run.transitions().is_empty());
    assert!(!run.kinds().contains(&"loop.transition"));
}

#[test]
fn findings_get_one_triage_then_the_report() {
    let (mut run, _) = once("medium");
    assert_eq!(run.goal(), [review("m1", "medium", false)]);
    let asked = run.answer(3);
    assert!(
        matches!(&asked[..], [Action::AskTriage { round, findings }]
            if round == "m1" && findings.len() == 3),
        "{asked:?}"
    );
    // A pass that fixed nothing of a new class still ends after its triage: no second round.
    let out = run.triage(&["fixed:known-site", "fixed:new-class", "dismissed"]);
    assert!(out.contains(&Action::AskReport), "{out:?}");
    assert!(!out.iter().any(|a| matches!(a, Action::RunReview { .. })));
    assert_eq!(run.state.outcome, Some(Outcome::Triaged));
    assert_eq!(run.state.phase, Phase::Report);
    // Its dismissal is in the ledger, as in a ladder round.
    assert_eq!(run.state.ledger, 1);
    assert_eq!(
        run.report(),
        [Action::End {
            outcome: Outcome::Triaged
        }]
    );
    assert_eq!(run.round_ids(), ["m1"]);
    assert!(run.transitions().is_empty());
    assert_eq!(run.phases(), [Phase::Scope, Phase::Ladder, Phase::Report]);
}

#[test]
fn a_refused_triage_and_a_failed_run_are_handled_as_in_a_loop() {
    let (mut run, _) = once("low");
    run.goal();
    let round = run.state.round.clone().unwrap();
    assert_eq!(
        run.step(Input::ReviewFailed {
            round: round.clone(),
            reason: "exit 1".into()
        }),
        [review("m1", "medium", true)]
    );
    run.answer(1);
    let ids = run.state.round_finding_ids();
    let out = run.reply(&triage_text(&ids, &["fixed:new-class"], "fail"));
    assert!(
        matches!(
            &out[..],
            [Action::AskCorrection {
                step: Step::Triage,
                ..
            }]
        ),
        "{out:?}"
    );
    assert_eq!(run.triage(&["fixed:new-class"]), [Action::AskReport]);
    assert_eq!(run.state.outcome, Some(Outcome::Triaged));
}

#[test]
fn skip_to_verify_is_refused_and_stop_works() {
    let (mut run, _) = once("high");
    assert_eq!(
        run.try_step(Input::Control {
            action: ControlAction::SkipToVerify,
            by: ControlBy::User,
        }),
        Err(StepError::Control(
            "a single pass has no verification to skip to".into()
        ))
    );
    run.goal();
    assert!(run
        .try_step(Input::Control {
            action: ControlAction::SkipToVerify,
            by: ControlBy::User,
        })
        .is_err());
    run.step(Input::Control {
        action: ControlAction::Stop,
        by: ControlBy::User,
    });
    assert_eq!(ended(&run), Outcome::Stopped);
}

#[test]
fn a_pass_that_commits_asks_for_the_commit_in_its_report() {
    let mut p = started(Profile::builtin("high").unwrap(), 25);
    p.once = true;
    p.commit_fixes = true;
    p.settings.review_commit = ReviewCommit::Match;
    let (mut run, _) = Run::started(p);
    assert_eq!(run.state.commit, CommitMode::End);
    run.goal();
    run.answer(1);
    run.triage(&["fixed:new-class"]);
    assert!(run.state.asks_commit());
    let out = run.step(Input::CommitChecked {
        commits: vec![],
        problems: vec![],
        notes: vec![],
    });
    assert_eq!(out, [Action::WriteReport]);
    assert_eq!(
        run.report(),
        [Action::End {
            outcome: Outcome::Triaged
        }]
    );
}

#[test]
fn an_implementation_loop_runs_no_single_pass() {
    let mut p = started(Profile::builtin("high").unwrap(), 25);
    p.once = true;
    p.kind = LoopKind::Implement;
    assert_eq!(LoopState::start(&p).unwrap_err(), StartError::OnceImplement);
}

/// The strongest tier: by effort, the later on a tie, capped at one round.
#[test]
fn the_pass_tier_is_the_profiles_strongest() {
    let tier = |p: &str| Profile::builtin(p).unwrap().pass_tier().unwrap();
    assert_eq!(tier("high").effort, "xhigh");
    assert_eq!(tier("medium").effort, "medium");
    assert_eq!(tier("low").effort, "medium");
    assert_eq!(tier("high").max_rounds, Some(1));
    let mut odd = Profile::builtin("high").unwrap();
    odd.tiers.reverse();
    assert_eq!(odd.pass_tier().unwrap().effort, "xhigh");
    odd.tiers.clear();
    assert_eq!(odd.pass_tier(), None);
}

/// A log without `once` reads as a full loop; one with it folds back into a single pass.
#[test]
fn once_round_trips_and_old_logs_read_as_full_loops() {
    let old: LoopStarted = serde_json::from_value(serde_json::json!({
        "id": "l1", "kind": "review",
        "profile": serde_json::to_value(Profile::builtin("high").unwrap()).unwrap(),
        "settings": {}
    }))
    .unwrap();
    assert!(!old.once);
    let mut p = started(Profile::builtin("high").unwrap(), 25);
    p.once = true;
    let v = serde_json::to_value(&p).unwrap();
    assert_eq!(v["once"], true);
    let back: LoopStarted = serde_json::from_value(v).unwrap();
    assert!(back.once);
    // Mid-pass, after a restart: the fold is the single pass where it was.
    let (mut run, _) = Run::started(back);
    run.goal();
    let folded = LoopState::fold(&run.log).unwrap();
    assert!(folded.once);
    assert_eq!(folded.round.as_deref(), Some("x1"));
    assert_eq!(folded.plan.tiers.len(), 1);
    assert!(!folded.plan.sweep);
}
