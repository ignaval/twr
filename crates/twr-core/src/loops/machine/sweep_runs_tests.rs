//! Scripted runs of the sweep with `sweep = "runs"`: the rules turn, the hub's audit runs and
//! the verification turn, in both kinds of loop. Every step is checked against a fold of the
//! log it wrote ([`Run::check_fold`]), so a hub that restarts at any point finds the state the
//! steps reached.

use super::*;
use crate::loops::config::{ImplCommit, SweepMode};
use crate::loops::line_forms;

/// A review loop on `profile` whose sweep is audited by runs.
fn runs(profile: &str) -> (Run, Vec<Action>) {
    let mut p = started(Profile::builtin(profile).unwrap(), 25);
    p.settings.sweep = SweepMode::Runs;
    Run::started(p)
}

fn rules_text(names: &[&str]) -> String {
    let rules: Vec<String> = names
        .iter()
        .map(|n| format!(r#"{{"name": "{n}", "rule": "{n} holds.", "where": "src/**"}}"#))
        .collect();
    format!(
        "The rules.\n```twr-rules\n{{\"rules\": [{}]}}\n```\n",
        rules.join(", ")
    )
}

fn sweep_text(items: &[(&str, &str)]) -> String {
    let items: Vec<String> = items
        .iter()
        .map(|(rule, result)| match *result {
            "accepted residual" | "not audited" => format!(
                r#"{{"rule": "{rule}", "result": "{result}", "reason": "because", "location": "a.rs:1"}}"#
            ),
            r => format!(r#"{{"rule": "{rule}", "result": "{r}"}}"#),
        })
        .collect();
    format!(
        "Verified.\n```twr-sweep\n{{\"rules\": [{}], \"gates\": {{\"ran\": [\"cargo test\"], \
         \"result\": \"pass\"}}}}\n```\n",
        items.join(", ")
    )
}

impl Run {
    /// Runs the medium ladder to its sweep: one round with a new-class fix, then a clean one.
    fn run_to_sweep(&mut self) -> Vec<Action> {
        self.goal();
        self.answer(1);
        self.triage(&["fixed:new-class"]);
        self.answer(0)
    }

    fn audit_start(&mut self, rule: u32) -> Vec<Action> {
        self.step(Input::AuditStarted {
            rule,
            prompt_digest: Some("sha256:aa".into()),
        })
    }

    fn audit_answer(&mut self, rule: u32, n: usize) -> Vec<Action> {
        self.step(Input::AuditAnswered {
            rule,
            findings: findings(n),
            answer_path: Some(format!("/data/loops/l1/audit-s{rule}.answer.json")),
            usage: Some(Usage {
                input: 10,
                output: 5,
                ..Default::default()
            }),
        })
    }

    fn audit_fail(&mut self, rule: u32) -> Vec<Action> {
        self.step(Input::AuditFailed {
            rule,
            reason: "exit status 1".into(),
        })
    }

    fn statuses(&self) -> Vec<AuditStatus> {
        self.state.audits.iter().map(|a| a.status).collect()
    }
}

#[test]
fn the_sweep_names_rules_audits_them_and_verifies_the_reports() {
    let (mut run, _) = runs("medium");
    assert!(run.state.plan.audit_runs);
    assert_eq!(run.state.plan.audit_effort, "xhigh");
    // The ladder fixed something: the sweep's first turn is the rules turn.
    assert_eq!(run.run_to_sweep(), [Action::AskRules]);
    assert_eq!(run.state.phase, Phase::Sweep);
    assert_eq!(run.state.wait, Wait::Rules);
    assert_eq!(line(&run.state), "medium · sweep · rules");
    assert_eq!(run.state.waiting_on, WaitingOn::Thread);
    // A refused block: one correction (as every step).
    assert_eq!(
        run.reply("no block"),
        [Action::AskCorrection {
            step: Step::Rules,
            problems: vec!["no ```twr-rules block: end your reply with one".into()],
        }]
    );
    assert_eq!(
        run.reply(&rules_text(&["identity-gating", "retries", "locks"])),
        [Action::RunAudits]
    );
    assert_eq!(run.state.wait, Wait::Audits);
    assert_eq!(run.state.waiting_on, WaitingOn::Review);
    assert_eq!(run.state.pending_audits().len(), 3);
    assert_eq!(line(&run.state), "medium · sweep · auditing 0/3");
    let [_, _, short] = line_forms(&run.state);
    assert_eq!(short, "aud 0/3");
    // The hub starts them (up to its cap); their ends come in any order.
    for k in 1..=3 {
        assert!(run.audit_start(k).is_empty());
    }
    assert_eq!(run.statuses(), [AuditStatus::Running; 3]);
    assert!(run.state.audits[0].started_at.is_some());
    assert!(run.state.pending_audits().is_empty());
    // A run that answered is not started again, nor answered twice.
    assert!(run.audit_answer(2, 2).is_empty());
    assert!(run
        .try_step(Input::AuditAnswered {
            rule: 2,
            findings: vec![],
            answer_path: None,
            usage: None,
        })
        .is_err());
    assert!(run
        .try_step(Input::AuditStarted {
            rule: 2,
            prompt_digest: None
        })
        .is_err());
    assert_eq!(line(&run.state), "medium · sweep · auditing 1/3");
    let audit = run
        .log
        .iter()
        .find_map(|e| match &e.body {
            EventBody::LoopAudit(a) => Some(a.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(audit.name, "retries");
    assert_eq!(audit.findings[1].id, "s2.2");
    assert!(run.audit_answer(1, 0).is_empty());
    // The last one ends: the verification turn.
    assert_eq!(run.audit_answer(3, 1), [Action::AskSweep]);
    assert_eq!(run.state.wait, Wait::Sweep);
    assert_eq!(line(&run.state), "medium · sweep · verifying");
    assert_eq!(
        run.state
            .audits
            .iter()
            .map(|a| a.findings)
            .collect::<Vec<_>>(),
        [0, 2, 1]
    );
    // Every rule reported, and only those; `not audited` only for a rule not run.
    let missing = sweep_text(&[("identity-gating", "checked clean"), ("retries", "fixed")]);
    let [Action::AskCorrection { step, problems }] = &run.reply(&missing)[..] else {
        panic!()
    };
    assert_eq!(*step, Step::Sweep);
    assert!(
        problems[0].contains("rule locks has no item"),
        "{problems:?}"
    );
    let ok = sweep_text(&[
        ("identity-gating", "checked clean"),
        ("retries", "fixed"),
        ("locks", "accepted residual"),
    ]);
    assert_eq!(run.reply(&ok), [review("v1", "xhigh", false)]);
    assert_eq!(run.state.ledger, 1);
    assert_eq!(run.state.phase, Phase::Verify);
    assert_eq!(
        run.phases(),
        [Phase::Scope, Phase::Ladder, Phase::Sweep, Phase::Verify]
    );
    run.answer(0);
    run.report();
    assert_eq!(ended(&run), Outcome::Clean);
    // The audits stay in the loop's state for `loop.get`.
    assert_eq!(run.state.audits.len(), 3);
}

#[test]
fn zero_rules_end_the_sweep_with_nothing_to_audit() {
    let (mut run, _) = runs("medium");
    run.run_to_sweep();
    assert_eq!(run.reply(&rules_text(&[])), [review("v1", "xhigh", false)]);
    assert!(run.state.audits.is_empty());
    assert_eq!(run.state.phase, Phase::Verify);
    assert!(run.kinds().contains(&"loop.rules"));
}

#[test]
fn a_failed_audit_is_retried_once_then_not_run_and_the_sweep_goes_on() {
    let (mut run, _) = runs("medium");
    run.run_to_sweep();
    run.reply(&rules_text(&["a", "b"]));
    run.audit_start(1);
    run.audit_start(2);
    // A first failure: the rule waits for its retry.
    assert_eq!(run.audit_fail(1), [Action::RunAudits]);
    assert_eq!(run.statuses(), [AuditStatus::Pending, AuditStatus::Running]);
    assert!(run.state.audits[0].retried);
    run.audit_start(1);
    let started = run
        .log
        .iter()
        .rev()
        .find_map(|e| match &e.body {
            EventBody::LoopAuditStarted(a) => Some(a.clone()),
            _ => None,
        })
        .unwrap();
    assert!(started.retry);
    assert_eq!(started.effort, "xhigh");
    // The retry answers: fine.
    assert!(run.audit_answer(1, 1).is_empty());
    // Rule b fails twice: not run, and the loop is never stuck over it.
    assert_eq!(run.audit_fail(2), [Action::RunAudits]);
    // A failure before the run started (the hub could not prepare it) counts the same.
    assert_eq!(run.audit_fail(2), [Action::AskSweep]);
    assert_eq!(run.statuses(), [AuditStatus::Answered, AuditStatus::NotRun]);
    assert!(run.state.stuck.is_none());
    assert!(!run.state.paused);
    assert_eq!(run.state.audited_rules().1, ["b"]);
    // `not audited` for a rule whose audit ran is refused; for b it is an answer, and so is
    // the thread's own audit of b.
    let wrong = sweep_text(&[("a", "not audited"), ("b", "not audited")]);
    let [Action::AskCorrection { problems, .. }] = &run.reply(&wrong)[..] else {
        panic!()
    };
    assert!(problems[0].contains("a is not audited, but its audit ran"));
    let ok = sweep_text(&[("a", "fixed"), ("b", "not audited")]);
    assert_eq!(run.reply(&ok), [review("v1", "xhigh", false)]);
    // Not audited is no accepted residual: nothing goes into the ledger for it.
    assert_eq!(run.state.ledger, 0);
}

#[test]
fn a_refused_rules_turn_twice_sticks_the_loop() {
    let (mut run, _) = runs("medium");
    run.run_to_sweep();
    run.reply("no block");
    let effects = run.reply("```twr-rules\n{\"rules\": [{\"name\": \"A\"}]}\n```");
    assert!(matches!(&effects[..], [Action::Stuck { why }] if why.contains("rules reply")));
    assert_eq!(run.state.waiting_on, WaitingOn::User);
    // A resume asks for the rules turn again.
    let effects = run.step(Input::Control {
        action: ControlAction::Resume,
        by: ControlBy::User,
    });
    assert_eq!(effects, [Action::AskRules]);
    // A failed rules turn sticks it too (it is a thread turn).
    let effects = run.step(Input::TurnFailed {
        why: "the turn errored".into(),
    });
    assert!(matches!(&effects[..], [Action::Stuck { .. }]));
}

#[test]
fn pause_lets_running_audits_finish_and_starts_none() {
    let (mut run, _) = runs("medium");
    run.run_to_sweep();
    run.reply(&rules_text(&["a", "b", "c"]));
    // The hub started two (its cap); the third waits for a slot.
    run.audit_start(1);
    run.audit_start(2);
    assert!(run
        .step(Input::Control {
            action: ControlAction::Pause,
            by: ControlBy::User,
        })
        .is_empty());
    // A running audit ends: nothing new starts while paused.
    assert!(run.audit_answer(1, 0).is_empty());
    assert!(run.state.held);
    // A failed one is not retried while paused either.
    assert!(run.audit_fail(2).is_empty());
    assert_eq!(
        run.statuses(),
        [
            AuditStatus::Answered,
            AuditStatus::Pending,
            AuditStatus::Pending
        ]
    );
    assert_eq!(line(&run.state), "medium · sweep · auditing 1/3 · paused");
    assert_eq!(run.state.waiting_on, WaitingOn::User);
    // The resume starts them.
    assert_eq!(
        run.step(Input::Control {
            action: ControlAction::Resume,
            by: ControlBy::User,
        }),
        [Action::RunAudits]
    );
    run.audit_start(2);
    run.audit_start(3);
    // Paused while all run: their ends hold the verification turn.
    run.step(Input::Control {
        action: ControlAction::Pause,
        by: ControlBy::User,
    });
    run.audit_answer(2, 0);
    assert!(run.audit_answer(3, 0).is_empty());
    assert_eq!(run.state.wait, Wait::Sweep);
    assert!(run.state.held);
    assert_eq!(
        run.step(Input::Control {
            action: ControlAction::Resume,
            by: ControlBy::User,
        }),
        [Action::AskSweep]
    );
}

#[test]
fn a_resume_starts_audits_that_waited_for_a_slot() {
    let (mut run, _) = runs("medium");
    run.run_to_sweep();
    run.reply(&rules_text(&["a", "b"]));
    run.audit_start(1);
    run.step(Input::Control {
        action: ControlAction::Pause,
        by: ControlBy::User,
    });
    // Nothing ended while paused, so nothing was held; b still waits for its run.
    assert!(!run.state.held);
    assert_eq!(
        run.step(Input::Control {
            action: ControlAction::Resume,
            by: ControlBy::User,
        }),
        [Action::RunAudits]
    );
}

#[test]
fn stop_ends_the_loop_with_audits_in_flight() {
    let (mut run, _) = runs("medium");
    run.run_to_sweep();
    run.reply(&rules_text(&["a", "b"]));
    run.audit_start(1);
    let effects = run.step(Input::Control {
        action: ControlAction::Stop,
        by: ControlBy::User,
    });
    assert_eq!(
        effects,
        [Action::End {
            outcome: Outcome::Stopped
        }]
    );
    assert_eq!(ended(&run), Outcome::Stopped);
    // A run's end after the stop changes nothing.
    assert_eq!(
        run.try_step(Input::AuditAnswered {
            rule: 1,
            findings: vec![],
            answer_path: None,
            usage: None,
        }),
        Err(StepError::Ended)
    );
}

#[test]
fn skip_to_verify_during_the_ladder_still_skips_the_sweep() {
    let (mut run, _) = runs("medium");
    run.goal();
    run.answer(1);
    run.step(Input::Control {
        action: ControlAction::SkipToVerify,
        by: ControlBy::User,
    });
    assert_eq!(
        run.triage(&["fixed:new-class"]),
        [review("v1", "xhigh", false)]
    );
    assert!(run.state.audits.is_empty());
    assert!(!run.phases().contains(&Phase::Sweep));
    // And it is refused once the sweep runs.
    let (mut run, _) = runs("medium");
    run.run_to_sweep();
    assert!(run
        .try_step(Input::Control {
            action: ControlAction::SkipToVerify,
            by: ControlBy::User,
        })
        .is_err());
}

#[test]
fn the_fold_rebuilds_every_point_and_a_lost_run_is_a_failed_one() {
    let (mut run, _) = runs("medium");
    run.run_to_sweep();
    // Every step checked its fold; here the points a restarted hub reads.
    let fold = |run: &Run| LoopState::fold(&run.log).unwrap();
    assert_eq!(fold(&run).wait, Wait::Rules);
    assert_eq!(fold(&run).ask(), Some(Action::AskRules));
    run.reply(&rules_text(&["a", "b"]));
    run.audit_start(1);
    let mid = fold(&run);
    assert_eq!(mid.ask(), Some(Action::RunAudits));
    assert_eq!(mid.audits[0].status, AuditStatus::Running);
    assert_eq!(mid.pending_audits().len(), 1);
    // The hub that restarts finds a1 in flight with no run of its own: a failed run, retried.
    assert_eq!(
        run.step(Input::AuditFailed {
            rule: 1,
            reason: "the hub stopped while it ran".into(),
        }),
        [Action::RunAudits]
    );
    run.audit_start(1);
    run.audit_start(2);
    run.audit_answer(1, 1);
    // Between the last audit and the verification prompt: the fold asks for it.
    run.audit_answer(2, 0);
    let last = fold(&run);
    assert_eq!(last.wait, Wait::Sweep);
    assert_eq!(last.ask(), Some(Action::AskSweep));
    assert_eq!(
        last.reply_input("```twr-sweep\n{\"rules\": []}\n```"),
        Some(Input::SweepRejected {
            problems: vec![
                "rule a has no item: report it (fixed, accepted residual, checked clean)".into(),
                "rule b has no item: report it (fixed, accepted residual, checked clean)".into(),
            ]
        })
    );
}

#[test]
fn a_thread_sweep_refuses_not_audited_and_old_logs_read_as_before() {
    // `sweep = "thread"`: one turn, as before, and `not audited` is no answer there.
    let (mut run, _) = Run::new("medium");
    run.goal();
    run.answer(1);
    run.triage(&["fixed:new-class"]);
    assert_eq!(run.answer(0), [Action::AskSweep]);
    assert!(!run.state.plan.audit_runs);
    let [Action::AskCorrection { problems, .. }] =
        &run.reply(&sweep_text(&[("r", "not audited")]))[..]
    else {
        panic!()
    };
    assert!(problems[0].contains("audits its rules itself"));
    assert_eq!(
        run.reply(&sweep_text(&[("r", "fixed")])),
        [review("v1", "xhigh", false)]
    );
    // A state serialized without the new fields reads back (a client's older snapshot).
    let mut json = serde_json::to_value(&run.state).unwrap();
    json["plan"].as_object_mut().unwrap().remove("audit_runs");
    json["plan"].as_object_mut().unwrap().remove("audit_effort");
    let back: LoopState = serde_json::from_value(json).unwrap();
    assert_eq!(back, run.state);
}

#[test]
fn an_implementation_loop_sweeps_with_runs_and_per_round_commits_as_ever() {
    let mut p = started(Profile::builtin("medium").unwrap(), 25);
    p.kind = LoopKind::Implement;
    p.task = Some("add a CSV export".into());
    p.settings.commit = ImplCommit::PerRound;
    p.settings.sweep = SweepMode::Runs;
    let (mut run, _) = Run::started(p);
    assert_eq!(run.state.commit, CommitMode::PerRound);
    run.step(Input::PlanWritten {
        plan: contract::Plan {
            contract_risk: false,
            reason: "no contract".into(),
            text: "the plan".into(),
        },
        path: Some("/data/loops/l1/plan.md".into()),
    });
    run.reply(
        "Implemented.\n```twr-implement\n{\"status\": \"done\", \"gates\": {\"ran\": \
         [\"cargo test\"], \"result\": \"pass\"}}\n```\n",
    );
    run.answer(1);
    run.triage(&["fixed:new-class"]);
    assert_eq!(run.answer(0), [Action::AskRules]);
    run.reply(&rules_text(&["a"]));
    run.audit_start(1);
    assert_eq!(run.audit_answer(1, 1), [Action::AskSweep]);
    let Action::RunReview { template, .. } = &run.reply(&sweep_text(&[("a", "fixed")]))[0] else {
        panic!()
    };
    assert_eq!(*template, ReviewTemplate::ImplReview);
    run.answer(0);
    assert!(run.state.asks_commit());
}
