//! The loop state machine (docs/design/loops.md, "Phases"): pure, event in, actions out.
//!
//! # Facts and records
//!
//! A loop's state is the fold of its `loop.*` events, and nothing else: [`LoopState::apply`]
//! is the one function that changes it, and the reducer ([`crate::reduce::ThreadView`]), a hub
//! restart ([`LoopState::fold`]) and the hub's driver ([`LoopState::step`]) all go through it.
//! The events are of two sorts:
//!
//! - **facts**, what happened that the machine could not know: `loop.started`, a review run
//!   started (`loop.review_started`) or answered (`loop.findings`), the thread's triage
//!   (`loop.triage`), its GOAL paragraph or sweep results or a refused reply (`loop.phase`
//!   with `goal`, `sweep` or `problems`), an implementation loop's plan (`loop.plan`) and
//!   implement turn (`loop.implemented`), the hub's check of a commit (`loop.commit`), a
//!   control (`loop.control`), the end (`loop.ended`, also written by the hub for a stopped
//!   thread or a report written);
//! - **records**, what the machine decided from them: a phase change (a plain `loop.phase`),
//!   the ledger's new entries (`loop.ledger`), the ladder's end (`loop.transition`) and the
//!   end it reached itself (`loop.ended`).
//!
//! Applying a fact changes the state and returns [`Action::Record`]s of the records it
//! decided and the effects the hub carries out (ask the thread, run a review, raise a stuck
//! item). Applying a record changes nothing (a `loop.ended` the state already has is a
//! record). So the hub writes, in one batch, each fact and the records its application
//! returned, and a fold of the log gives back the state it stepped to.
//!
//! [`LoopState::step`] is the driver's entry: it turns an [`Input`] into its fact, checks
//! that the loop waits for it, applies it and returns the fact's [`Action::Record`] first. (One
//! input is two facts: a plan-check triage that revised the plan is its `loop.plan`, then its
//! `loop.triage`, each recorded and applied in turn.)
//!
//! # The rules (review loops)
//!
//! - **scope**: one thread turn; its `twr-goal` block's GOAL paragraph starts the ladder.
//! - **ladder**: rounds by tier, in the profile's order. A round is a review run; findings
//!   go to the thread for triage. A tier **converges** on a round with no findings, or one
//!   whose findings are all triaged `duplicate`: the next tier starts, or, after the last,
//!   the ladder ends (`converged`).
//! - **transition**: after two consecutive rounds that had findings but none triaged
//!   `fixed` with class `new-class`, the ladder ends (`no_new_class`), mid-tier or not.
//!   A converging round is not such a round: convergence is checked first, ends the tier
//!   and resets the count, and so does a round with a new-class finding and the start of a
//!   tier. So the count only ever measures rounds that kept finding things without finding
//!   anything new, and an empty round after one such round moves to the next tier rather
//!   than skipping it.
//! - **caps**: a tier with `max_rounds` that ran them hands over to the next tier, or ends the
//!   ladder after the last (`max_rounds`); a tier that ran `max_rounds_per_tier` rounds ends
//!   the ladder (`backstop`). The tier's own cap is checked first.
//! - **failed review run**: retried once; a second failure pauses the loop, stuck. It never
//!   counts as converged, and the retry is the same round.
//! - **refused reply** (a missing or invalid `twr-goal`, `twr-triage` or `twr-sweep` block,
//!   red gates): one correction prompt; a second refusal pauses the loop, stuck.
//! - **sweep**: after the ladder, when the profile has one and the ladder had at least one
//!   finding triaged `fixed`. Its accepted residuals go into the ledger.
//! - **verify**: up to the profile's `verify.rounds` rounds at its effort. A converging round
//!   ends it: `clean` when it had no findings, `ledger_fixed_point` when all were duplicates.
//!   Every answered round counts against the cap; a cap spent with findings still arriving
//!   ends it `cap_expired`. Findings are triaged as in the ladder.
//! - **verify skipped**: a verification whose first round could only repeat the round before
//!   it does not run. That is when the ladder ends `converged` on a round of its last tier with
//!   no findings, no sweep runs (the profile has none, or the ladder fixed nothing), the tier's
//!   effort is the verification's, and the hub checked that the worktree did not change while
//!   that round ran (`loop.findings` `unchanged`). Then v1 would be the same reviewer at the
//!   same effort on the same diff with the same ledger, so the loop goes from the ladder
//!   straight to its report, `clean`; its `loop.transition` says `verify_skipped`, and the
//!   verification reads 0 of its cap. The invariant is that nothing changed in the working tree
//!   between that round and the verification v1 would have been: no loop turn runs between
//!   them (an empty round has no triage), and the hub's check covers anything else (a turn
//!   you start yourself during the round, a job of the thread's). An all-duplicate round does
//!   not qualify: its triage is a turn of the thread's, which runs the gates and can edit
//!   files even when it fixes nothing, so the verification runs after it. Every other end of
//!   the ladder (`no_new_class`, `backstop`, `max_rounds`, `skipped`), a sweep, or another
//!   effort runs the verification as usual.
//! - **report**: one thread turn; the hub writes `report.md` and `loop.ended`.
//! - **controls**: `pause` holds the next step (what runs finishes) until `resume`, which
//!   asks for a held or stuck step again with fresh retry and correction budgets; `stop` ends
//!   the loop `stopped`; `skip_to_verify` (scope or ladder only) ends the ladder at the end of
//!   the current round (after the GOAL paragraph in scope), skipping the sweep, and at once
//!   when the loop is paused before a round has run.
//!
//! # The rules (implementation loops)
//!
//! An implementation loop has no scope phase and no GOAL paragraph: the task and the plan
//! take their place. It goes `plan → plan_check → implement`, then the ladder, sweep,
//! verification and report exactly as a review loop, its review runs on the impl-review
//! template ([`ReviewTemplate`]).
//!
//! - **plan**: one thread turn; its `twr-plan` block (and the message without it, the plan the
//!   hub writes to `plan.md`) is `loop.plan` revision 1. A missing or invalid block gets the
//!   one correction prompt, then the loop is stuck.
//! - **plan check**: it runs when the plan check setting (the settings' `plan_check`, else the
//!   profile's) is `always`, or `contracts` and the plan said `contract_risk: true`. It is one
//!   review run, round `p1`, at `medium`, on the plan-review template, retried once when it
//!   fails as any round is. No findings: on to implement. Findings: one triage turn
//!   ([`contract::parse_plan_triage`]: no gates, class optional) whose dismissals go into the
//!   ledger and whose revised plan, if any, is `loop.plan` revision 2. There is no second plan
//!   round, and the plan check's findings neither count toward the sweep nor the transition.
//! - **implement**: one thread turn; its `twr-implement` block is `loop.implemented`. Anything
//!   but `done` with gates that passed (or a missing block) gets the one correction prompt,
//!   "gates must be green before review", then the loop is stuck. `done` and green start the
//!   ladder.
//! - **skip to verify** is refused until the implement phase is done: there is nothing to
//!   verify before it. In the ladder it is a review loop's.
//!
//! # The commit
//!
//! Whether a loop commits is fixed when it starts ([`CommitMode`]): an implementation loop by
//! its Commit setting (`end`, `per-round`, `none`), a review loop under `match` by the hub's
//! decision `loop.started.commit_fixes` (`end` when it holds, else none). A loop that ends
//! `stopped` or `failed` never reaches its report and never commits. When it commits
//! ([`LoopState::asks_commit`]), the report turn asks for the commit and a `twr-commit` block;
//! the hub checks it ([`contract::check_commit`], which follows git: the block is the thread's
//! note, never the check) and gives the result ([`Input::CommitChecked`], `loop.commit`). A passed check, or a second failed one, has the hub write `report.md`
//! ([`Action::WriteReport`]), and then the loop ends with its outcome, its `loop.ended` naming
//! the commits and, after two failures, `commit_problem`. A first failure gets one correction
//! prompt ([`Step::Commit`]); a loop that commits never gets stuck over it.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::config::{ImplCommit, PlanCheck, ReviewCommit, Tier, Verify};
use super::contract::{
    self, ledger_id, number_findings, parse_goal, parse_implement, parse_plan, parse_plan_triage,
    parse_sweep, parse_triage, tier_tags, Commit, Finding, FindingClass, Implemented, LedgerEntry,
    LedgerKind, Plan, PlanTriage, Sweep, SweepResult, Triage, Verdict, PLAN_CHECK_TAG, VERIFY_TAG,
};
use super::{ControlAction, ControlBy, LoopKind, Outcome, Phase, TransitionReason, WaitingOn};
use crate::event::{
    Event, EventBody, LoopCommitChecked, LoopControl, LoopEnded, LoopFindings, LoopImplemented,
    LoopLedger, LoopPhaseChanged, LoopPlanned, LoopReviewStarted, LoopStarted, LoopTransition,
    LoopTriage, Usage,
};

/// The round of an implementation loop's plan check (its findings are `p1.1`…).
pub const PLAN_CHECK_ROUND: &str = "p1";

/// The reviewer's effort in the plan check.
pub const PLAN_CHECK_EFFORT: &str = "medium";

/// The loop as clients see it: the machine's state itself, so a client's fold and the hub's
/// driver never disagree ([`crate::reduce::ThreadView::r#loop`]).
pub type LoopView = LoopState;

/// One round, as the loop line and `loop.get` show it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RoundView {
    /// Its id (`m3`, `v1`).
    pub id: String,
    /// How many findings its answer had.
    pub findings: u32,
    /// How many of them were fixed as a new class.
    pub new_class: u32,
    /// Its latest review run failed (it has no answer yet).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub failed: bool,
}

/// The verification's progress: answered rounds of the cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct VerifyProgress {
    /// Answered verification rounds.
    pub done: u32,
    /// The profile's cap.
    pub cap: u32,
}

/// The reviewer, as the card's activity line names it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ReviewerView {
    /// Its runtime.
    pub runtime: String,
    /// Its model.
    pub model: String,
    /// The current round's effort.
    pub effort: Option<String>,
}

/// What the ladder runs: the resolved profile's tiers with their tags, the sweep, the
/// verification and the backstop, fixed when the loop started.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LoopPlan {
    /// The tiers, in order.
    pub tiers: Vec<Tier>,
    /// Their tags ([`contract::tier_tags`]).
    pub tags: Vec<String>,
    /// Whether the profile sweeps.
    pub sweep: bool,
    /// The capped verification.
    pub verify: Verify,
    /// The backstop.
    pub max_rounds_per_tier: u32,
}

/// The step a loop waits for.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum Wait {
    /// The scope turn's GOAL paragraph.
    #[default]
    Goal,
    /// An implementation loop's plan turn (`twr-plan`).
    Plan,
    /// Round `round`'s review run (`p1`: the plan check's).
    Review {
        /// The round.
        round: String,
    },
    /// The thread's triage of round `round` (`p1`: the plan check's).
    Triage {
        /// The round.
        round: String,
    },
    /// An implementation loop's implement turn (`twr-implement`).
    Implement,
    /// The sweep turn.
    Sweep,
    /// The report turn, then (with no commit asked) the hub's `report.md`, or (with one) the
    /// hub's check of the commit.
    Report,
    /// The commit's correction turn, then the hub's check of it.
    Commit,
    /// The commit is settled: the hub writes `report.md` ([`Action::WriteReport`]).
    WriteReport,
    /// Nothing: the loop is over.
    Ended,
}

/// The thread turn a correction prompt is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// The scope turn (`twr-goal`).
    Goal,
    /// A triage turn (`twr-triage`; the plan check's may add `twr-plan`).
    Triage,
    /// The sweep turn (`twr-sweep`).
    Sweep,
    /// The plan turn (`twr-plan`).
    Plan,
    /// The implement turn (`twr-implement`).
    Implement,
    /// The report turn's commit (`twr-commit`): its check failed once.
    Commit,
}

impl Step {
    /// In words.
    pub fn as_str(self) -> &'static str {
        match self {
            Step::Goal => "scope",
            Step::Triage => "triage",
            Step::Sweep => "sweep",
            Step::Plan => "plan",
            Step::Implement => "implement",
            Step::Commit => "commit",
        }
    }

    /// The fenced block its reply ends with.
    pub fn block(self) -> &'static str {
        match self {
            Step::Goal => contract::GOAL_BLOCK,
            Step::Triage => contract::TRIAGE_BLOCK,
            Step::Sweep => contract::SWEEP_BLOCK,
            Step::Plan => contract::PLAN_BLOCK,
            Step::Implement => contract::IMPLEMENT_BLOCK,
            Step::Commit => contract::COMMIT_BLOCK,
        }
    }
}

/// The template a review run's prompt is built from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReviewTemplate {
    /// A review loop's rounds: the thread's work in its scope.
    Review,
    /// An implementation loop's rounds: the diff against the plan, the baseline out of scope.
    ImplReview,
    /// An implementation loop's plan check: the plan, before any code.
    PlanReview,
}

impl ReviewTemplate {
    /// The template's name (`templates/<name>.md`).
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewTemplate::Review => "review",
            ReviewTemplate::ImplReview => "impl-review",
            ReviewTemplate::PlanReview => "plan-review",
        }
    }
}

/// What a loop commits, fixed when it starts (module docs, "The commit").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CommitMode {
    /// Nothing: no commit is asked for and nothing is checked.
    #[default]
    None,
    /// The report turn commits the loop's work.
    End,
    /// The implement turn and every triage turn that fixed something commit their work; the
    /// report turn commits what is left. The same check runs at the end.
    PerRound,
}

impl CommitMode {
    /// What loop `p` commits: an implementation loop by its Commit setting; a review loop
    /// under `match` at the end when the hub decided so at the start
    /// (`loop.started.commit_fixes`), else nothing.
    pub fn of(p: &LoopStarted) -> CommitMode {
        match p.kind {
            LoopKind::Implement => match p.settings.commit {
                ImplCommit::End => CommitMode::End,
                ImplCommit::PerRound => CommitMode::PerRound,
                ImplCommit::None => CommitMode::None,
            },
            LoopKind::Review
                if p.commit_fixes && p.settings.review_commit == ReviewCommit::Match =>
            {
                CommitMode::End
            }
            LoopKind::Review => CommitMode::None,
        }
    }
}

/// How a round converged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Convergence {
    /// No findings.
    Empty,
    /// Every finding a duplicate of the ledger.
    AllDuplicate,
}

/// When and why the ladder ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transition {
    /// Why.
    pub reason: TransitionReason,
    /// After which round.
    pub round: String,
    /// The verification was skipped: it could only repeat `round` (module docs, "verify
    /// skipped").
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub verify_skipped: bool,
}

/// What the hub does next.
// `Record` is the common variant (most steps record something) and actions live only until
// the driver carries them out, so boxing its body would only add an allocation per record.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Write this event (in order, in one batch with the fact it follows).
    Record(EventBody),
    /// Send the scope prompt: run the gates once, write the GOAL paragraph in a `twr-goal`
    /// block.
    AskGoal,
    /// Send an implementation loop's plan prompt: the task, the skill's plan sections, "state
    /// an interpretation instead of asking", and a `twr-plan` block.
    AskPlan,
    /// Run the review of round `round` at `effort` (and record `loop.review_started`).
    RunReview {
        /// The round (`m3`, `v1`; `p1`: the plan check).
        round: String,
        /// The phase it is of (ladder, verify or plan check).
        phase: Phase,
        /// The reviewer's effort.
        effort: String,
        /// It retries a failed run.
        retry: bool,
        /// The template its prompt is built from.
        template: ReviewTemplate,
    },
    /// Send the plan check's findings for triage, wrapped as data: a `twr-triage` block (no
    /// gates, class optional) and, when the plan changes, the revised plan and a `twr-plan`
    /// block.
    AskPlanTriage {
        /// The round (`p1`).
        round: String,
        /// Its findings, numbered.
        findings: Vec<Finding>,
    },
    /// Send an implementation loop's implement prompt: execute `plan.md`, delegate as the
    /// skill says, run the gates, and a `twr-implement` block.
    AskImplement,
    /// Send round `round`'s findings for triage, wrapped as data.
    AskTriage {
        /// The round.
        round: String,
        /// Its findings, numbered.
        findings: Vec<Finding>,
    },
    /// Send the one correction prompt of `step`, naming `problems`.
    AskCorrection {
        /// The turn it corrects.
        step: Step,
        /// What was wrong.
        problems: Vec<String>,
    },
    /// Send the sweep prompt (this loop's findings, distilled into rules).
    AskSweep,
    /// Send the report prompt (with a commit to make when [`LoopState::asks_commit`]).
    AskReport,
    /// Write `report.md` from the report turn's reply: the commit is settled (made, or its
    /// check failed twice); then [`Input::ReportWritten`].
    WriteReport,
    /// Raise a stuck item on the thread's card: the loop is paused until the user resumes it.
    Stuck {
        /// What went wrong.
        why: String,
    },
    /// The loop is over: drop its line, notify once.
    End {
        /// How it ended.
        outcome: Outcome,
    },
}

/// What the driver tells the machine.
#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    /// The scope turn's GOAL paragraph ([`contract::parse_goal`]).
    GoalWritten {
        /// The paragraph.
        goal: String,
    },
    /// The scope turn's reply had no valid `twr-goal` block.
    GoalRejected {
        /// Why.
        problems: Vec<String>,
    },
    /// The plan turn's plan ([`contract::parse_plan`]), which the hub wrote to `path`.
    PlanWritten {
        /// The plan.
        plan: Plan,
        /// Where the hub wrote it (`plan.md`); `None` from [`LoopState::reply_input`], which
        /// the hub fills in once it wrote the file.
        path: Option<String>,
    },
    /// The plan turn's reply had no valid `twr-plan` block, or no plan.
    PlanRejected {
        /// Why.
        problems: Vec<String>,
    },
    /// The plan check's triage ([`contract::parse_plan_triage`]); a revised plan the hub wrote
    /// to `path`. (Its refusal is [`Input::TriageRejected`].)
    PlanTriaged {
        /// The round (`p1`).
        round: String,
        /// The triage.
        triage: PlanTriage,
        /// Where the hub wrote the revised plan (`plan.md`, the first kept as `plan-1.md`);
        /// `None` from [`LoopState::reply_input`], or when the plan did not change.
        path: Option<String>,
    },
    /// The implement turn's `twr-implement` block ([`contract::parse_implement`]), whatever
    /// it says: the machine refuses anything but `done` and green.
    Implemented {
        /// The block.
        implemented: Implemented,
    },
    /// The implement turn's reply had no valid `twr-implement` block.
    ImplementRejected {
        /// Why.
        problems: Vec<String>,
    },
    /// Round `round`'s review run started.
    ReviewStarted {
        /// The round.
        round: String,
        /// Its diffs, by path.
        diffs: Vec<String>,
        /// Its prompt's digest.
        prompt_digest: Option<String>,
    },
    /// Round `round`'s review run answered ([`contract::parse_findings`]); the machine
    /// numbers the findings.
    ReviewAnswered {
        /// The round.
        round: String,
        /// The findings, as read.
        findings: Vec<Finding>,
        /// The answer file.
        answer_path: Option<String>,
        /// What the run spent.
        usage: Option<Usage>,
        /// No findings, and the worktree's diff when the answer came was the one the round
        /// reviewed (`loop.findings` `unchanged`).
        unchanged: bool,
    },
    /// Round `round`'s review run failed: a non-zero exit, a timeout, no answer, an answer
    /// that does not parse, or a run in flight when the hub stopped.
    ReviewFailed {
        /// The round.
        round: String,
        /// Why.
        reason: String,
    },
    /// Round `round`'s triage, validated ([`contract::parse_triage`]).
    TriageAccepted {
        /// The round.
        round: String,
        /// The triage.
        triage: Triage,
    },
    /// Round `round`'s triage was refused.
    TriageRejected {
        /// The round.
        round: String,
        /// Why.
        problems: Vec<String>,
    },
    /// The sweep's results ([`contract::parse_sweep`]).
    SweepDone {
        /// The results.
        sweep: Sweep,
    },
    /// The sweep turn's reply had no valid `twr-sweep` block.
    SweepRejected {
        /// Why.
        problems: Vec<String>,
    },
    /// The hub checked the commit the report turn (or its correction) made
    /// ([`contract::parse_commit`], [`contract::check_commit`]).
    CommitChecked {
        /// The commits it found (of the repositories that passed).
        commits: Vec<Commit>,
        /// What is wrong; empty: the check passed.
        problems: Vec<String>,
        /// What it noted that is no problem (the `twr-commit` block missing or invalid).
        notes: Vec<String>,
    },
    /// The hub wrote `report.md` from the report turn's reply.
    ReportWritten {
        /// Its path.
        report_path: String,
    },
    /// The loop's thread turn failed (an error, a crash, an interrupt): the loop is stuck.
    TurnFailed {
        /// Why.
        why: String,
    },
    /// Pause, resume, stop or skip to verify.
    Control {
        /// What.
        action: ControlAction,
        /// Who.
        by: ControlBy,
    },
    /// The hub can't go on (the worktree is gone, a diff can't be built): the loop ends
    /// `failed`.
    Fail {
        /// Why.
        why: String,
    },
}

/// Why a loop does not start.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StartError {
    /// The profile has no tier, no verification round, or a tier capped at zero rounds.
    #[error("profile {0} has no ladder or no verification round to run")]
    EmptyProfile(String),
}

/// Why the machine did not take an input.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StepError {
    /// The loop is over.
    #[error("the loop has ended")]
    Ended,
    /// The loop waits for something else (a late or repeated input).
    #[error("the loop is not waiting for that (it waits for {0})")]
    NotWaiting(String),
    /// A control that does nothing now.
    #[error("{0}")]
    Control(String),
}

/// A loop's state: what [`crate::reduce::ThreadView::r#loop`] shows and what the driver steps.
/// The first fields are its view (the implementation notes' `thread.loop`); the rest is what
/// the rules need ([`LoopState`] module docs).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LoopState {
    /// The loop's id.
    pub id: String,
    /// Review or implementation.
    pub kind: LoopKind,
    /// The profile's name.
    pub profile: String,
    /// Where it is.
    pub phase: Phase,
    /// The current round's tier tag (`m`, `x`; `v` in the verification).
    pub tier: Option<String>,
    /// The current round (`m3`, `v1`).
    pub round: Option<String>,
    /// Every round so far, in order.
    pub rounds: Vec<RoundView>,
    /// The ledger's size.
    pub ledger: u32,
    /// The verification's progress.
    pub verify: VerifyProgress,
    /// Paused: the next step is held until a resume.
    pub paused: bool,
    /// Why it is stuck (paused by a step that failed), if it is.
    pub stuck: Option<String>,
    /// What it waits for.
    pub waiting_on: WaitingOn,
    /// The reviewer.
    pub reviewer: ReviewerView,
    /// When the current round's review run started, while it runs.
    pub review_started_at: Option<DateTime<Utc>>,
    /// How it ended, once it did; while the report is being written, how it will.
    pub outcome: Option<Outcome>,
    /// What it commits ([`CommitMode`]).
    pub commit: CommitMode,
    /// The commits the hub's check found.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub commits: Vec<Commit>,
    /// The commit's check failed twice: what is left (`not committed`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_problem: Option<String>,
    /// An implementation loop's plan: 0 before the plan turn answered, 1 once it did, 2 once
    /// the plan check revised it.
    #[serde(skip_serializing_if = "is_zero")]
    pub plan_revision: u32,
    /// Who asked for it (`loop.started`'s `requested_by`): left out for the user.
    #[serde(skip_serializing_if = "ControlBy::is_user")]
    pub requested_by: ControlBy,

    /// An implementation loop's plan check setting: the settings' `plan_check`, else the
    /// profile's.
    pub plan_check: PlanCheck,
    /// What the latest plan said of contracts.
    pub contract_risk: Option<bool>,
    /// The last failed commit check's problems (what a resumed correction names again).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub commit_problems: Vec<String>,
    /// What the ladder runs.
    pub plan: LoopPlan,
    /// The current tier's place in [`LoopPlan::tiers`].
    pub tier_index: usize,
    /// Rounds begun in the current tier.
    pub tier_rounds: u32,
    /// Consecutive rounds with findings and none fixed as a new class.
    pub no_new_class: u32,
    /// Findings the ladder triaged `fixed` (zero: no sweep).
    pub fixed: u32,
    /// The step it waits for.
    pub wait: Wait,
    /// The step's ask was not sent (paused, or stuck): a resume sends it.
    pub held: bool,
    /// The current round's review run was retried.
    pub retried: bool,
    /// The current step's reply was refused once (its correction prompt went out).
    pub corrected: bool,
    /// `skip_to_verify` is pending.
    pub skip: bool,
    /// The findings of the round being triaged.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<Finding>,
    /// The GOAL paragraph.
    pub goal: Option<String>,
    /// How the last converging round converged.
    pub converged: Option<Convergence>,
    /// When and why the ladder ended.
    pub transition: Option<Transition>,
    /// The loop is over.
    pub ended: bool,
}

impl LoopState {
    /// The state `loop.started` `p` starts, and what to do first: record the first phase and
    /// ask for its turn (a review loop's scope turn, an implementation loop's plan turn). (The
    /// hub writes `p` itself.) A profile with nothing to run is refused.
    pub fn start(p: &LoopStarted) -> Result<(LoopState, Vec<Action>), StartError> {
        let profile = &p.profile;
        if profile.tiers.is_empty()
            || profile.verify.rounds == 0
            || profile.tiers.iter().any(|t| t.max_rounds == Some(0))
        {
            return Err(StartError::EmptyProfile(profile.name.clone()));
        }
        let efforts: Vec<&str> = profile.tiers.iter().map(|t| t.effort.as_str()).collect();
        let (phase, wait) = match p.kind {
            LoopKind::Review => (Phase::Scope, Wait::Goal),
            LoopKind::Implement => (Phase::Plan, Wait::Plan),
        };
        let mut s = LoopState {
            id: p.id.clone(),
            kind: p.kind,
            profile: profile.name.clone(),
            phase,
            verify: VerifyProgress {
                done: 0,
                cap: profile.verify.rounds,
            },
            reviewer: ReviewerView {
                runtime: p.settings.reviewer.runtime.clone(),
                model: p.settings.reviewer.model.clone(),
                effort: None,
            },
            commit: CommitMode::of(p),
            requested_by: p.requested_by,
            plan_check: p.settings.plan_check.unwrap_or(profile.plan_check),
            plan: LoopPlan {
                tiers: profile.tiers.clone(),
                tags: tier_tags(&efforts),
                sweep: profile.sweep,
                verify: profile.verify.clone(),
                // (0: a `loop.started` that does not say; the default backstop.)
                max_rounds_per_tier: match p.max_rounds_per_tier {
                    0 => super::config::LoopsConfig::default().max_rounds_per_tier,
                    n => n,
                },
            },
            wait,
            ..Default::default()
        };
        let mut out = vec![s.record_phase()];
        out.extend(s.issue());
        s.refresh();
        Ok((s, out))
    }

    /// The state of the last loop in `events` (a thread's, in order): started by its
    /// `loop.started`, then every event applied. `None` when no loop started there (or the
    /// last one could not start).
    pub fn fold<'a>(events: impl IntoIterator<Item = &'a Event>) -> Option<LoopState> {
        let mut state: Option<LoopState> = None;
        for ev in events {
            match &ev.body {
                EventBody::LoopStarted(p) => state = LoopState::start(p).ok().map(|(s, _)| s),
                _ => {
                    if let Some(s) = &mut state {
                        s.apply(ev);
                    }
                }
            }
        }
        state
    }

    /// Applies event `ev` of this loop (another loop's, or any other kind, changes nothing):
    /// the one function that changes the state (module docs). Returns the records it decided
    /// and the effects to carry out; a fold discards them.
    pub fn apply(&mut self, ev: &Event) -> Vec<Action> {
        self.evolve(&ev.body, ev.ts)
    }

    /// Takes `input` at `at` (the time the hub records it): its fact, if the loop waits for
    /// it, applied. The actions start with [`Action::Record`] of the fact, then what applying
    /// it returned (an input of two facts: each, in turn); the hub writes every record in one
    /// batch, with `at` as their time.
    pub fn step(&mut self, input: Input, at: DateTime<Utc>) -> Result<Vec<Action>, StepError> {
        let facts = self.facts(input)?;
        let mut out = Vec::new();
        for fact in facts {
            out.push(Action::Record(fact.clone()));
            out.extend(self.evolve(&fact, at));
        }
        Ok(out)
    }

    /// What the thread's final message `text` brings to the step the loop waits for: a GOAL
    /// paragraph, a plan, a triage, the implement turn's block or sweep results, or why the
    /// block was refused. `None` when the loop waits for no reply of the thread's it parses (a
    /// review run, the report and its commit, which the hub reads itself, the end). A plan
    /// comes back without its path: the hub writes the plan, then steps the input with it.
    pub fn reply_input(&self, text: &str) -> Option<Input> {
        let problems = |v: Vec<contract::BlockProblem>| v.iter().map(ToString::to_string).collect();
        match &self.wait {
            Wait::Goal => Some(match parse_goal(text) {
                Ok(goal) => Input::GoalWritten { goal },
                Err(p) => Input::GoalRejected {
                    problems: problems(p),
                },
            }),
            Wait::Plan => Some(match parse_plan(text) {
                Ok(plan) => Input::PlanWritten { plan, path: None },
                Err(p) => Input::PlanRejected {
                    problems: problems(p),
                },
            }),
            Wait::Triage { round } if self.phase == Phase::PlanCheck => Some(
                match parse_plan_triage(text, &self.round_finding_ids(), &self.ledger_ids()) {
                    Ok(triage) => Input::PlanTriaged {
                        round: round.clone(),
                        triage,
                        path: None,
                    },
                    Err(p) => Input::TriageRejected {
                        round: round.clone(),
                        problems: problems(p),
                    },
                },
            ),
            Wait::Triage { round } => Some(
                match parse_triage(text, &self.round_finding_ids(), &self.ledger_ids()) {
                    Ok(triage) => Input::TriageAccepted {
                        round: round.clone(),
                        triage,
                    },
                    Err(p) => Input::TriageRejected {
                        round: round.clone(),
                        problems: problems(p),
                    },
                },
            ),
            Wait::Implement => Some(match parse_implement(text) {
                Ok(implemented) => Input::Implemented { implemented },
                Err(p) => Input::ImplementRejected {
                    problems: problems(p),
                },
            }),
            Wait::Sweep => Some(match parse_sweep(text) {
                Ok(sweep) => Input::SweepDone { sweep },
                Err(p) => Input::SweepRejected {
                    problems: problems(p),
                },
            }),
            _ => None,
        }
    }

    /// The ask of the step the loop waits for: what a resume sends, and what the hub sends
    /// again after a restart for a step whose turn never ran. `None` once it ended.
    pub fn ask(&self) -> Option<Action> {
        Some(match &self.wait {
            Wait::Goal => Action::AskGoal,
            Wait::Plan => Action::AskPlan,
            Wait::Review { round } => Action::RunReview {
                round: round.clone(),
                phase: self.phase,
                effort: self.reviewer.effort.clone().unwrap_or_default(),
                retry: self.retried,
                template: self.review_template(),
            },
            Wait::Triage { round } if self.phase == Phase::PlanCheck => Action::AskPlanTriage {
                round: round.clone(),
                findings: self.findings.clone(),
            },
            Wait::Triage { round } => Action::AskTriage {
                round: round.clone(),
                findings: self.findings.clone(),
            },
            Wait::Implement => Action::AskImplement,
            Wait::Sweep => Action::AskSweep,
            Wait::Report => Action::AskReport,
            Wait::Commit => Action::AskCorrection {
                step: Step::Commit,
                problems: self.commit_problems.clone(),
            },
            Wait::WriteReport => Action::WriteReport,
            Wait::Ended => return None,
        })
    }

    /// The template the current review run's prompt is built from: the plan check's, an
    /// implementation loop's rounds' or a review loop's.
    pub fn review_template(&self) -> ReviewTemplate {
        match (self.kind, self.phase) {
            (_, Phase::PlanCheck) => ReviewTemplate::PlanReview,
            (LoopKind::Implement, _) => ReviewTemplate::ImplReview,
            (LoopKind::Review, _) => ReviewTemplate::Review,
        }
    }

    /// Whether the report turn asks for a commit (and the hub checks it): the loop commits
    /// ([`CommitMode`]) and it is reporting, which a loop that ends `stopped` or `failed` never
    /// does.
    pub fn asks_commit(&self) -> bool {
        !self.ended
            && self.commit != CommitMode::None
            && matches!(self.wait, Wait::Report | Wait::Commit | Wait::WriteReport)
    }

    /// The ids of the findings being triaged.
    pub fn round_finding_ids(&self) -> Vec<String> {
        self.findings.iter().map(|f| f.id.clone()).collect()
    }

    /// The ids of the ledger's entries (`L1`…).
    pub fn ledger_ids(&self) -> Vec<String> {
        (1..=self.ledger as usize).map(ledger_id).collect()
    }

    // -----------------------------------------------------------------------------------------
    // Inputs into facts
    // -----------------------------------------------------------------------------------------

    /// The facts `input` is, if the loop waits for it: one, but for a plan-check triage that
    /// revised the plan (its `loop.plan`, then its `loop.triage`).
    fn facts(&self, input: Input) -> Result<Vec<EventBody>, StepError> {
        if self.ended {
            return Err(StepError::Ended);
        }
        let id = self.id.clone();
        let plan_check = self.phase == Phase::PlanCheck;
        let waits_triage =
            |round: &str| matches!(&self.wait, Wait::Triage { round: r } if r == round);
        Ok(match input {
            Input::PlanWritten { plan, path } if self.wait == Wait::Plan => {
                vec![self.plan_fact(&plan, path)]
            }
            Input::PlanRejected { problems } if self.wait == Wait::Plan => {
                vec![self.refusal_fact(problems)]
            }
            Input::PlanTriaged {
                round,
                triage,
                path,
            } if plan_check && waits_triage(&round) => {
                let mut out = Vec::new();
                if let Some(plan) = &triage.plan {
                    out.push(self.plan_fact(plan, path));
                }
                out.push(EventBody::LoopTriage(LoopTriage {
                    id,
                    round,
                    items: triage.items,
                    ..Default::default()
                }));
                out
            }
            Input::Implemented { implemented } if self.wait == Wait::Implement => {
                vec![EventBody::LoopImplemented(LoopImplemented {
                    id,
                    status: implemented.status,
                    gates: implemented.gates,
                    ..Default::default()
                })]
            }
            Input::ImplementRejected { problems } if self.wait == Wait::Implement => {
                vec![self.refusal_fact(problems)]
            }
            Input::CommitChecked {
                commits,
                problems,
                notes,
            } if self.asks_commit() && matches!(self.wait, Wait::Report | Wait::Commit) => {
                vec![EventBody::LoopCommit(LoopCommitChecked {
                    id,
                    commits,
                    problems,
                    notes,
                    ..Default::default()
                })]
            }
            Input::ReportWritten { report_path }
                if self.wait == Wait::WriteReport
                    || (self.wait == Wait::Report && !self.asks_commit()) =>
            {
                vec![EventBody::LoopEnded(LoopEnded {
                    id,
                    outcome: self.outcome.unwrap_or_default(),
                    report_path: Some(report_path),
                    commits: self.commits.clone(),
                    commit_problem: self.commit_problem.clone(),
                    ..Default::default()
                })]
            }
            Input::TriageAccepted { round, .. } if plan_check && waits_triage(&round) => {
                return Err(StepError::NotWaiting(self.wait_words()))
            }
            other => vec![self.fact(other)?],
        })
    }

    /// The `loop.plan` of `plan`, written to `path`: the next revision.
    fn plan_fact(&self, plan: &Plan, path: Option<String>) -> EventBody {
        EventBody::LoopPlan(LoopPlanned {
            id: self.id.clone(),
            revision: self.plan_revision + 1,
            contract_risk: plan.contract_risk,
            reason: plan.reason.clone(),
            path,
            ..Default::default()
        })
    }

    /// The `loop.phase` of a reply refused for `problems`, on the phase the loop stays in.
    fn refusal_fact(&self, problems: Vec<String>) -> EventBody {
        EventBody::LoopPhase(LoopPhaseChanged {
            id: self.id.clone(),
            phase: self.phase,
            problems: nonempty(problems),
            ..Default::default()
        })
    }

    /// The fact of an input a review loop has too, if the loop waits for it.
    fn fact(&self, input: Input) -> Result<EventBody, StepError> {
        let id = self.id.clone();
        let not_waiting = || StepError::NotWaiting(self.wait_words());
        let waits_review =
            |round: &str| matches!(&self.wait, Wait::Review { round: r } if r == round);
        let waits_triage =
            |round: &str| matches!(&self.wait, Wait::Triage { round: r } if r == round);
        Ok(match input {
            Input::GoalWritten { goal } if self.wait == Wait::Goal => {
                EventBody::LoopPhase(LoopPhaseChanged {
                    id,
                    phase: if self.skip {
                        Phase::Verify
                    } else {
                        Phase::Ladder
                    },
                    goal: Some(goal),
                    ..Default::default()
                })
            }
            Input::GoalRejected { problems } if self.wait == Wait::Goal => {
                self.refusal_fact(problems)
            }
            Input::ReviewStarted {
                round,
                diffs,
                prompt_digest,
            } if waits_review(&round) => EventBody::LoopReviewStarted(LoopReviewStarted {
                id,
                round,
                tier: self.tier.clone().unwrap_or_default(),
                effort: self.reviewer.effort.clone().unwrap_or_default(),
                diffs,
                prompt_digest,
                retry: self.retried,
                ..Default::default()
            }),
            Input::ReviewAnswered {
                round,
                findings,
                answer_path,
                usage,
                unchanged,
            } if waits_review(&round) => EventBody::LoopFindings(LoopFindings {
                id,
                unchanged: unchanged && findings.is_empty(),
                findings: number_findings(&round, findings),
                round,
                answer_path,
                usage,
                ..Default::default()
            }),
            Input::ReviewFailed { round, reason } if waits_review(&round) => {
                EventBody::LoopFindings(LoopFindings {
                    id,
                    round,
                    failed: true,
                    reason: Some(reason),
                    ..Default::default()
                })
            }
            Input::TriageAccepted { round, triage } if waits_triage(&round) => {
                EventBody::LoopTriage(LoopTriage {
                    id,
                    round,
                    items: triage.items,
                    gates: Some(triage.gates),
                    ..Default::default()
                })
            }
            Input::TriageRejected { round, problems } if waits_triage(&round) => {
                EventBody::LoopTriage(LoopTriage {
                    id,
                    round,
                    problems: nonempty(problems),
                    ..Default::default()
                })
            }
            Input::SweepDone { sweep } if self.wait == Wait::Sweep => {
                EventBody::LoopPhase(LoopPhaseChanged {
                    id,
                    phase: Phase::Verify,
                    sweep: Some(sweep.rules),
                    ..Default::default()
                })
            }
            Input::SweepRejected { problems } if self.wait == Wait::Sweep => {
                self.refusal_fact(problems)
            }
            Input::TurnFailed { why } if self.waits_on_thread() && !self.held => {
                EventBody::LoopControl(LoopControl {
                    id,
                    action: ControlAction::Pause,
                    by: ControlBy::Hub,
                    why: Some(why),
                    ..Default::default()
                })
            }
            Input::Control { action, by } => {
                self.check_control(action)?;
                EventBody::LoopControl(LoopControl {
                    id,
                    action,
                    by,
                    ..Default::default()
                })
            }
            Input::Fail { why } => EventBody::LoopEnded(LoopEnded {
                id,
                outcome: Outcome::Failed,
                reason: Some(why),
                ..Default::default()
            }),
            _ => return Err(not_waiting()),
        })
    }

    /// Whether `action` does something now.
    fn check_control(&self, action: ControlAction) -> Result<(), StepError> {
        let no = |why: &str| Err(StepError::Control(why.to_string()));
        match action {
            ControlAction::Pause if self.paused => no("the loop is already paused"),
            ControlAction::Resume if !self.paused => no("the loop is not paused"),
            ControlAction::SkipToVerify
                if matches!(
                    self.phase,
                    Phase::Plan | Phase::PlanCheck | Phase::Implement
                ) =>
            {
                no("the loop has not implemented its plan yet: skip to verify comes after it")
            }
            ControlAction::SkipToVerify if !matches!(self.phase, Phase::Scope | Phase::Ladder) => {
                no("the loop is past its ladder")
            }
            ControlAction::SkipToVerify if self.skip => no("the loop already skips to verify"),
            _ => Ok(()),
        }
    }

    /// What the loop waits for, in words.
    fn wait_words(&self) -> String {
        match &self.wait {
            Wait::Goal => "the scope turn".into(),
            Wait::Plan => "the plan turn".into(),
            Wait::Review { round } => format!("review round {round}"),
            Wait::Triage { round } => format!("the triage of {round}"),
            Wait::Implement => "the implement turn".into(),
            Wait::Sweep => "the sweep turn".into(),
            Wait::Report => "the report".into(),
            Wait::Commit => "the commit's correction".into(),
            Wait::WriteReport => "the report file".into(),
            Wait::Ended => "nothing".into(),
        }
    }

    /// Whether the step it waits for is a turn of the thread's.
    fn waits_on_thread(&self) -> bool {
        matches!(
            self.wait,
            Wait::Goal
                | Wait::Plan
                | Wait::Triage { .. }
                | Wait::Implement
                | Wait::Sweep
                | Wait::Report
                | Wait::Commit
        )
    }

    // -----------------------------------------------------------------------------------------
    // Facts into state
    // -----------------------------------------------------------------------------------------

    /// Applies `body` (recorded at `ts`): module docs.
    fn evolve(&mut self, body: &EventBody, ts: DateTime<Utc>) -> Vec<Action> {
        if self.ended {
            return vec![];
        }
        let mine = |id: &str| id == self.id;
        let out = match body {
            EventBody::LoopPhase(p) if mine(&p.id) => {
                if !p.problems.is_empty() {
                    match self.wait {
                        Wait::Goal => self.refused(Step::Goal, &p.problems),
                        Wait::Plan => self.refused(Step::Plan, &p.problems),
                        Wait::Implement => self.refused(Step::Implement, &p.problems),
                        Wait::Sweep => self.refused(Step::Sweep, &p.problems),
                        _ => vec![],
                    }
                } else if p.goal.is_some() && self.wait == Wait::Goal {
                    self.goal.clone_from(&p.goal);
                    self.corrected = false;
                    if self.skip {
                        self.skip = false;
                        self.enter_verify(false)
                    } else {
                        self.enter_ladder(false)
                    }
                } else if p.sweep.is_some() && self.wait == Wait::Sweep {
                    self.corrected = false;
                    let mut out = self.residuals(p.sweep.as_deref().unwrap_or_default());
                    out.extend(self.enter_verify(false));
                    out
                } else {
                    vec![]
                }
            }
            EventBody::LoopPlan(p) if mine(&p.id) => self.planned(p),
            EventBody::LoopImplemented(p) if mine(&p.id) && self.wait == Wait::Implement => {
                let problems = Implemented {
                    status: p.status,
                    gates: p.gates.clone(),
                }
                .problems();
                if problems.is_empty() {
                    self.corrected = false;
                    self.enter_ladder(true)
                } else {
                    self.refused(Step::Implement, &problems)
                }
            }
            EventBody::LoopReviewStarted(p)
                if mine(&p.id)
                    && matches!(&self.wait, Wait::Review { round } if *round == p.round) =>
            {
                self.review_started_at = Some(ts);
                self.reviewer.effort = Some(p.effort.clone());
                vec![]
            }
            EventBody::LoopFindings(p)
                if mine(&p.id)
                    && matches!(&self.wait, Wait::Review { round } if *round == p.round) =>
            {
                self.review_started_at = None;
                if p.failed {
                    self.review_failed(p.reason.as_deref().unwrap_or("the review run failed"))
                } else {
                    self.answered(&p.findings, p.unchanged)
                }
            }
            EventBody::LoopTriage(p)
                if mine(&p.id)
                    && matches!(&self.wait, Wait::Triage { round } if *round == p.round) =>
            {
                if !p.problems.is_empty() {
                    self.refused(Step::Triage, &p.problems)
                } else if self.phase == Phase::PlanCheck {
                    self.plan_triaged(&p.items)
                } else {
                    self.triaged(&p.items)
                }
            }
            EventBody::LoopCommit(p)
                if mine(&p.id) && matches!(self.wait, Wait::Report | Wait::Commit) =>
            {
                self.commit_checked(p)
            }
            EventBody::LoopControl(p) if mine(&p.id) => self.control(p),
            EventBody::LoopEnded(p) if mine(&p.id) => {
                if !p.commits.is_empty() {
                    self.commits.clone_from(&p.commits);
                }
                if p.commit_problem.is_some() {
                    self.commit_problem.clone_from(&p.commit_problem);
                }
                self.end(p.outcome, None)
            }
            // Records (plain phases, ledger entries, transitions) and other loops' events.
            _ => vec![],
        };
        self.refresh();
        out
    }

    /// Updates [`LoopState::waiting_on`].
    fn refresh(&mut self) {
        self.waiting_on = if self.paused && self.held {
            WaitingOn::User
        } else if matches!(self.wait, Wait::Review { .. }) {
            WaitingOn::Review
        } else {
            WaitingOn::Thread
        };
    }

    /// The ask of the current step, unless the loop is paused (then it is held).
    fn issue(&mut self) -> Vec<Action> {
        if self.paused {
            self.held = true;
            return vec![];
        }
        self.held = false;
        self.ask().into_iter().collect()
    }

    /// Pauses the loop, stuck, with the step held.
    fn stick(&mut self, why: String) -> Vec<Action> {
        self.paused = true;
        self.held = true;
        self.stuck = Some(why.clone());
        vec![Action::Stuck { why }]
    }

    /// The record of the current phase.
    fn record_phase(&self) -> Action {
        Action::Record(EventBody::LoopPhase(LoopPhaseChanged {
            id: self.id.clone(),
            phase: self.phase,
            ..Default::default()
        }))
    }

    /// A refused reply of `step`: the one correction, or stuck.
    fn refused(&mut self, step: Step, problems: &[String]) -> Vec<Action> {
        if self.corrected {
            return self.stick(format!(
                "the {} reply was refused twice: {}",
                step.as_str(),
                problems.join("; ")
            ));
        }
        self.corrected = true;
        if self.paused {
            // (Held: a resume asks for the whole step again.)
            self.held = true;
            return vec![];
        }
        vec![Action::AskCorrection {
            step,
            problems: problems.to_vec(),
        }]
    }

    /// An implementation loop's plan (`loop.plan`): the plan turn's starts the plan check or
    /// the implement turn; the plan check's revision only replaces the plan.
    fn planned(&mut self, p: &LoopPlanned) -> Vec<Action> {
        let revision = p.revision.max(self.plan_revision + 1);
        match &self.wait {
            Wait::Plan => {
                self.plan_revision = revision;
                self.contract_risk = Some(p.contract_risk);
                self.corrected = false;
                if self.plan_check.runs(p.contract_risk) {
                    self.enter_plan_check()
                } else {
                    self.enter_implement()
                }
            }
            Wait::Triage { .. } if self.phase == Phase::PlanCheck => {
                self.plan_revision = revision;
                self.contract_risk = Some(p.contract_risk);
                vec![]
            }
            _ => vec![],
        }
    }

    /// Starts the plan check: its one review run, round `p1` at `medium`.
    fn enter_plan_check(&mut self) -> Vec<Action> {
        self.phase = Phase::PlanCheck;
        let round = PLAN_CHECK_ROUND.to_string();
        self.tier = Some(PLAN_CHECK_TAG.to_string());
        self.round = Some(round.clone());
        self.rounds.push(RoundView {
            id: round.clone(),
            ..Default::default()
        });
        self.reviewer.effort = Some(PLAN_CHECK_EFFORT.to_string());
        self.review_started_at = None;
        self.retried = false;
        self.corrected = false;
        self.wait = Wait::Review { round };
        let mut out = vec![self.record_phase()];
        out.extend(self.issue());
        out
    }

    /// The plan check's triage, accepted: its dismissals into the ledger, then the implement
    /// turn (a revised plan was its own `loop.plan`, before this).
    fn plan_triaged(&mut self, items: &[contract::TriageItem]) -> Vec<Action> {
        self.corrected = false;
        let mut out = self.ledger_dismissals(items);
        self.findings.clear();
        out.extend(self.enter_implement());
        out
    }

    /// Starts the implement turn.
    fn enter_implement(&mut self) -> Vec<Action> {
        self.phase = Phase::Implement;
        self.tier = None;
        self.round = None;
        self.review_started_at = None;
        self.findings.clear();
        self.corrected = false;
        self.wait = Wait::Implement;
        let mut out = vec![self.record_phase()];
        out.extend(self.issue());
        out
    }

    /// Starts the ladder at its first tier (`record`: with a record of the phase; a fact that
    /// already says `ladder` needs none).
    fn enter_ladder(&mut self, record: bool) -> Vec<Action> {
        self.phase = Phase::Ladder;
        self.tier_index = 0;
        self.tier_rounds = 0;
        self.no_new_class = 0;
        let mut out = Vec::new();
        if record {
            out.push(self.record_phase());
        }
        out.extend(self.begin_round());
        out
    }

    /// Starts the verification (`record`: with a record of the phase; a fact that already
    /// says `verify` needs none).
    fn enter_verify(&mut self, record: bool) -> Vec<Action> {
        self.phase = Phase::Verify;
        let mut out = Vec::new();
        if record {
            out.push(self.record_phase());
        }
        out.extend(self.begin_round());
        out
    }

    /// Begins the next round of the current tier, or of the verification.
    fn begin_round(&mut self) -> Vec<Action> {
        let (tag, n, effort) = match self.phase {
            Phase::Verify => (
                VERIFY_TAG.to_string(),
                self.verify.done + 1,
                self.plan.verify.effort.clone(),
            ),
            _ => {
                self.tier_rounds += 1;
                (
                    self.plan
                        .tags
                        .get(self.tier_index)
                        .cloned()
                        .unwrap_or_default(),
                    self.tier_rounds,
                    self.plan
                        .tiers
                        .get(self.tier_index)
                        .map(|t| t.effort.clone())
                        .unwrap_or_default(),
                )
            }
        };
        let round = format!("{tag}{n}");
        self.tier = Some(tag);
        self.round = Some(round.clone());
        self.rounds.push(RoundView {
            id: round.clone(),
            ..Default::default()
        });
        self.reviewer.effort = Some(effort);
        self.review_started_at = None;
        self.retried = false;
        self.corrected = false;
        self.wait = Wait::Review { round };
        self.issue()
    }

    /// A failed review run: retried once, then stuck.
    fn review_failed(&mut self, reason: &str) -> Vec<Action> {
        if let Some(r) = self.rounds.last_mut() {
            r.failed = true;
        }
        if self.retried {
            let round = self.round.clone().unwrap_or_default();
            return self.stick(format!("review round {round} failed twice: {reason}"));
        }
        self.retried = true;
        self.issue()
    }

    /// An answered review run (`unchanged`: the worktree did not change while it ran).
    fn answered(&mut self, findings: &[Finding], unchanged: bool) -> Vec<Action> {
        if let Some(r) = self.rounds.last_mut() {
            r.failed = false;
            r.findings = u32::try_from(findings.len()).unwrap_or(u32::MAX);
        }
        if self.phase == Phase::Verify {
            self.verify.done += 1;
        }
        if findings.is_empty() {
            if self.phase == Phase::PlanCheck {
                return self.enter_implement();
            }
            return self.converged(Convergence::Empty, unchanged);
        }
        self.findings = findings.to_vec();
        self.corrected = false;
        self.wait = Wait::Triage {
            round: self.round.clone().unwrap_or_default(),
        };
        self.issue()
    }

    /// The ledger entries of the `dismissed` items of a triage of the round being triaged.
    fn ledger_dismissals(&mut self, items: &[contract::TriageItem]) -> Vec<Action> {
        let entries: Vec<LedgerEntry> = items
            .iter()
            .filter(|i| i.verdict == Verdict::Dismissed)
            .enumerate()
            .map(|(k, i)| {
                let finding = self.findings.iter().find(|f| f.id == i.id);
                LedgerEntry {
                    id: ledger_id(self.ledger as usize + k + 1),
                    kind: LedgerKind::Dismissed,
                    from: i.id.clone(),
                    location: finding.map(|f| f.location.clone()).unwrap_or_default(),
                    problem: finding.map(|f| f.problem.clone()).unwrap_or_default(),
                    reason: i.reason.clone().unwrap_or_default(),
                }
            })
            .collect();
        self.append_ledger(entries)
    }

    /// A triage the hub accepted.
    fn triaged(&mut self, items: &[contract::TriageItem]) -> Vec<Action> {
        self.corrected = false;
        let mut out = self.ledger_dismissals(items);
        let fixed = items.iter().filter(|i| i.verdict == Verdict::Fixed).count();
        let new_class = items
            .iter()
            .filter(|i| i.verdict == Verdict::Fixed && i.class == Some(FindingClass::NewClass))
            .count();
        if let Some(r) = self.rounds.last_mut() {
            r.new_class = u32::try_from(new_class).unwrap_or(u32::MAX);
        }
        self.findings.clear();
        let all_duplicate =
            !items.is_empty() && items.iter().all(|i| i.verdict == Verdict::Duplicate);
        if all_duplicate {
            // (Never `unchanged`: the triage turn could have edited files.)
            out.extend(self.converged(Convergence::AllDuplicate, false));
            return out;
        }
        if self.phase == Phase::Verify {
            if self.verify.done >= self.verify.cap {
                out.extend(self.enter_report(Outcome::CapExpired));
            } else {
                out.extend(self.begin_round());
            }
            return out;
        }
        self.fixed += u32::try_from(fixed).unwrap_or(u32::MAX);
        if new_class > 0 {
            self.no_new_class = 0;
        } else {
            self.no_new_class += 1;
        }
        let tier_cap = self
            .plan
            .tiers
            .get(self.tier_index)
            .and_then(|t| t.max_rounds);
        out.extend(if self.no_new_class >= 2 {
            self.end_ladder(TransitionReason::NoNewClass)
        } else if self.skip {
            self.end_ladder(TransitionReason::Skipped)
        } else if tier_cap.is_some_and(|cap| self.tier_rounds >= cap) {
            self.next_tier_or_end(TransitionReason::MaxRounds)
        } else if self.tier_rounds >= self.plan.max_rounds_per_tier {
            self.end_ladder(TransitionReason::Backstop)
        } else {
            self.begin_round()
        });
        out
    }

    /// A round converged (`how`; `unchanged`: with no thread turn after it and the worktree
    /// as it reviewed it).
    fn converged(&mut self, how: Convergence, unchanged: bool) -> Vec<Action> {
        self.converged = Some(how);
        self.no_new_class = 0;
        match self.phase {
            Phase::Verify => self.enter_report(match how {
                Convergence::Empty => Outcome::Clean,
                Convergence::AllDuplicate => Outcome::LedgerFixedPoint,
            }),
            _ if self.skip => self.end_ladder(TransitionReason::Skipped),
            _ if self.tier_index + 1 >= self.plan.tiers.len() => {
                let repeat = how == Convergence::Empty && unchanged && self.verify_repeats();
                self.end_ladder_then(TransitionReason::Converged, repeat)
            }
            _ => self.next_tier_or_end(TransitionReason::Converged),
        }
    }

    /// Whether a verification round now would repeat the round that just ended the ladder
    /// (module docs, "verify skipped"), but for what changed in the worktree: no sweep runs
    /// and the current tier's effort is the verification's.
    fn verify_repeats(&self) -> bool {
        !self.sweeps(TransitionReason::Converged)
            && self
                .plan
                .tiers
                .get(self.tier_index)
                .is_some_and(|t| t.effort == self.plan.verify.effort)
    }

    /// Whether the ladder ending for `reason` now runs the sweep.
    fn sweeps(&self, reason: TransitionReason) -> bool {
        self.plan.sweep && self.fixed > 0 && reason != TransitionReason::Skipped
    }

    /// The next tier, or the end of the ladder (`reason`) after the last.
    fn next_tier_or_end(&mut self, reason: TransitionReason) -> Vec<Action> {
        if self.tier_index + 1 < self.plan.tiers.len() {
            self.tier_index += 1;
            self.tier_rounds = 0;
            self.no_new_class = 0;
            self.begin_round()
        } else {
            self.end_ladder(reason)
        }
    }

    /// The ladder ends: the sweep if the profile has one and the ladder fixed something (and
    /// it was not skipped), else the verification.
    fn end_ladder(&mut self, reason: TransitionReason) -> Vec<Action> {
        self.end_ladder_then(reason, false)
    }

    /// The ladder ends (`reason`); `skip_verify`: straight to the report, `clean`, since the
    /// verification could only repeat the last round (module docs, "verify skipped").
    fn end_ladder_then(&mut self, reason: TransitionReason, skip_verify: bool) -> Vec<Action> {
        let round = self.round.clone().unwrap_or_default();
        self.transition = Some(Transition {
            reason,
            round: round.clone(),
            verify_skipped: skip_verify,
        });
        self.skip = false;
        let mut out = vec![Action::Record(EventBody::LoopTransition(LoopTransition {
            id: self.id.clone(),
            reason,
            round,
            verify_skipped: skip_verify,
            ..Default::default()
        }))];
        if skip_verify {
            out.extend(self.enter_report(Outcome::Clean));
        } else if self.sweeps(reason) {
            self.phase = Phase::Sweep;
            self.tier = None;
            self.round = None;
            self.corrected = false;
            self.wait = Wait::Sweep;
            out.push(self.record_phase());
            out.extend(self.issue());
        } else {
            out.extend(self.enter_verify(true));
        }
        out
    }

    /// The sweep's accepted residuals, into the ledger.
    fn residuals(&mut self, sweep: &[contract::SweepItem]) -> Vec<Action> {
        let entries = sweep
            .iter()
            .filter(|r| r.result == SweepResult::AcceptedResidual)
            .enumerate()
            .map(|(k, r)| LedgerEntry {
                id: ledger_id(self.ledger as usize + k + 1),
                kind: LedgerKind::AcceptedResidual,
                from: r.rule.clone(),
                location: r.location.clone().unwrap_or_default(),
                problem: String::new(),
                reason: r.reason.clone().unwrap_or_default(),
            })
            .collect();
        self.append_ledger(entries)
    }

    /// `entries` into the ledger: a record, when there are any.
    fn append_ledger(&mut self, entries: Vec<LedgerEntry>) -> Vec<Action> {
        if entries.is_empty() {
            return vec![];
        }
        self.ledger += u32::try_from(entries.len()).unwrap_or(u32::MAX);
        vec![Action::Record(EventBody::LoopLedger(LoopLedger {
            id: self.id.clone(),
            entries,
            ..Default::default()
        }))]
    }

    /// The report turn, the loop to end `outcome`.
    fn enter_report(&mut self, outcome: Outcome) -> Vec<Action> {
        self.outcome = Some(outcome);
        self.phase = Phase::Report;
        self.tier = None;
        self.round = None;
        self.corrected = false;
        self.wait = Wait::Report;
        let mut out = vec![self.record_phase()];
        out.extend(self.issue());
        out
    }

    /// The hub's check of the commit (`loop.commit`): passed, the report is written; failed
    /// the first time, the one correction; failed again, the report is written all the same,
    /// with what is left (module docs, "The commit").
    fn commit_checked(&mut self, p: &LoopCommitChecked) -> Vec<Action> {
        self.commits.clone_from(&p.commits);
        if p.problems.is_empty() || self.corrected {
            if !p.problems.is_empty() {
                self.commit_problem = Some(p.problems.join("; "));
            }
            self.commit_problems.clear();
            self.wait = Wait::WriteReport;
            // (Not held by a pause: it is the hub's own last step, no turn of the thread's.)
            self.held = false;
            return vec![Action::WriteReport];
        }
        self.corrected = true;
        self.commit_problems.clone_from(&p.problems);
        self.wait = Wait::Commit;
        self.issue()
    }

    /// The loop ends `outcome`; `record`: with its `loop.ended` record (an end the machine
    /// decided itself, not one a fact said).
    fn end(&mut self, outcome: Outcome, record: Option<LoopEnded>) -> Vec<Action> {
        self.ended = true;
        self.outcome = Some(outcome);
        self.wait = Wait::Ended;
        self.paused = false;
        self.held = false;
        self.stuck = None;
        self.review_started_at = None;
        self.findings.clear();
        self.commit_problems.clear();
        let mut out: Vec<Action> = record
            .map(|r| Action::Record(EventBody::LoopEnded(r)))
            .into_iter()
            .collect();
        out.push(Action::End { outcome });
        out
    }

    /// A control.
    fn control(&mut self, p: &LoopControl) -> Vec<Action> {
        match p.action {
            ControlAction::Pause if p.by == ControlBy::Hub => {
                // A step that failed: the turn is gone, the step is held.
                let why = p.why.clone().unwrap_or_else(|| "a loop turn failed".into());
                self.stick(why)
            }
            ControlAction::Pause => {
                self.paused = true;
                vec![]
            }
            ControlAction::Resume => {
                self.paused = false;
                self.stuck = None;
                if !self.held {
                    return vec![];
                }
                self.retried = false;
                self.corrected = false;
                self.issue()
            }
            ControlAction::Stop => self.end(
                Outcome::Stopped,
                Some(LoopEnded {
                    id: self.id.clone(),
                    outcome: Outcome::Stopped,
                    commits: self.commits.clone(),
                    ..Default::default()
                }),
            ),
            ControlAction::SkipToVerify => {
                if !matches!(self.phase, Phase::Scope | Phase::Ladder) {
                    return vec![];
                }
                self.skip = true;
                // Paused before a ladder round ran: that round is not run.
                let unrun = self.rounds.last().is_some_and(|r| {
                    r.findings == 0
                        && self.wait
                            == Wait::Review {
                                round: r.id.clone(),
                            }
                });
                if self.phase == Phase::Ladder && self.held && unrun {
                    self.rounds.pop();
                    self.tier_rounds = self.tier_rounds.saturating_sub(1);
                    // (The ladder's rounds only: not the plan check's `p1`.)
                    self.round = self
                        .rounds
                        .last()
                        .filter(|r| r.id != PLAN_CHECK_ROUND)
                        .map(|r| r.id.clone());
                    return self.end_ladder(TransitionReason::Skipped);
                }
                vec![]
            }
        }
    }
}

/// `problems`, or one saying the block was refused when it is empty (a refusal is never
/// recorded without a reason).
fn nonempty(problems: Vec<String>) -> Vec<String> {
    if problems.is_empty() {
        vec!["the block was refused".into()]
    } else {
        problems
    }
}

/// Whether `n` is 0 (serde's `skip_serializing_if`).
fn is_zero(n: &u32) -> bool {
    *n == 0
}

#[cfg(test)]
mod tests;
