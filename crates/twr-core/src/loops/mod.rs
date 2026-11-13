//! Loops: review and implementation loops that a thread runs (docs/design/loops.md).
//!
//! A loop is a state of one thread. The thread implements and arbitrates; each review round
//! is a headless, read-only reviewer run the hub starts; the hub sequences the rounds with the
//! pure state machine of this module ([`LoopState`]) and records every step as a `loop.*`
//! event on the thread's stream, so a loop survives a hub restart and every client draws it
//! from the log ([`crate::reduce::ThreadView::r#loop`]).
//!
//! - [`config`]: `[loops]` of the config file, the profiles and the model specs.
//! - [`settings`]: the saved settings, presets, the checks against what is installed and the
//!   replacements the setup card shows; overrides and scope.
//! - [`contract`]: the reviewer's findings, the `twr-goal`, `twr-triage`, `twr-sweep`,
//!   `twr-plan`, `twr-implement` and `twr-commit` blocks, finding ids, the ledger and the
//!   check of a loop's commit.
//! - the state machine ([`LoopState`], [`Input`], [`Action`]) of both kinds of loop, and the
//!   loop line ([`line`]).
//!
//! Nothing here does IO, reads the clock or starts a process.

pub mod config;
pub mod contract;
mod machine;
pub mod settings;

use std::fmt;

use serde::{Deserialize, Serialize};

pub use machine::*;

pub use crate::event::{is_loop_setup_card, LoopSetup, SetupWhy, LOOP_SETUP_KEY, ROUTINE_LOOP};

/// The kind of loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoopKind {
    /// Review the thread's work: scope, ladder, sweep, verify, report.
    #[default]
    Review,
    /// Plan, implement and review a task: plan, plan check, implement, ladder, sweep, verify,
    /// report.
    Implement,
}

impl LoopKind {
    /// As written in requests and events.
    pub fn as_str(self) -> &'static str {
        match self {
            LoopKind::Review => "review",
            LoopKind::Implement => "implement",
        }
    }
}

/// The badge a thread's card draws before the loop line.
pub fn badge(kind: LoopKind) -> &'static str {
    match kind {
        LoopKind::Review => "REVIEW LOOP",
        LoopKind::Implement => "IMPLEMENT LOOP",
    }
}

/// A phase of a loop. Review loops go `setup → scope → ladder → sweep → verify → report`;
/// implementation loops `setup → plan → plan_check → implement → ladder → sweep → verify →
/// report`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// The setup card (before `loop.started`).
    #[default]
    Setup,
    /// Implementation: the thread writes the plan.
    Plan,
    /// Implementation: one review run on the plan.
    PlanCheck,
    /// Implementation: the thread executes the plan.
    Implement,
    /// Review: the thread runs the gates and writes the GOAL paragraph.
    Scope,
    /// Review rounds by tier.
    Ladder,
    /// The thread audits this loop's findings as named rules.
    Sweep,
    /// Capped review rounds with the full ledger.
    Verify,
    /// The thread's last turn, joined into `report.md`.
    Report,
}

impl Phase {
    /// Lowercase words, as the loop line writes them.
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Setup => "setup",
            Phase::Plan => "plan",
            Phase::PlanCheck => "plan check",
            Phase::Implement => "implement",
            Phase::Scope => "scope",
            Phase::Ladder => "ladder",
            Phase::Sweep => "sweep",
            Phase::Verify => "verify",
            Phase::Report => "report",
        }
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How a loop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The verification's converging round had no findings.
    #[default]
    Clean,
    /// The verification's converging round had findings, every one a duplicate of the ledger.
    LedgerFixedPoint,
    /// The verification's cap ran out with findings still arriving.
    CapExpired,
    /// Someone stopped it (or its thread was stopped, parked or deleted).
    Stopped,
    /// The hub could not go on (`loop.ended` `reason` says why).
    Failed,
}

impl Outcome {
    /// As written.
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Clean => "clean",
            Outcome::LedgerFixedPoint => "ledger_fixed_point",
            Outcome::CapExpired => "cap_expired",
            Outcome::Stopped => "stopped",
            Outcome::Failed => "failed",
        }
    }
}

/// Why the ladder ended (`loop.transition`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionReason {
    /// The last tier converged.
    #[default]
    Converged,
    /// Two consecutive rounds with findings but none of a new class.
    NoNewClass,
    /// A tier ran `max_rounds_per_tier` rounds.
    Backstop,
    /// The last tier ran its profile's `max_rounds` (the `low` profile's one round).
    MaxRounds,
    /// `skip_to_verify`.
    Skipped,
}

/// A user's (or the hub's) control of a running loop (`loop.control`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlAction {
    /// Hold the next step until `resume` (what is running finishes).
    #[default]
    Pause,
    /// Go on; after a stuck step, ask it again.
    Resume,
    /// End the loop, `stopped`, leaving the thread and its work as they are.
    Stop,
    /// End the ladder (and skip the sweep) at the end of the current round.
    SkipToVerify,
}

impl ControlAction {
    /// As written.
    pub fn as_str(self) -> &'static str {
        match self {
            ControlAction::Pause => "pause",
            ControlAction::Resume => "resume",
            ControlAction::Stop => "stop",
            ControlAction::SkipToVerify => "skip_to_verify",
        }
    }
}

/// Who controlled a loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlBy {
    /// A user's client.
    #[default]
    User,
    /// The orchestrator (milestone 3).
    Orchestrator,
    /// The hub: a `pause` by the hub is a stuck step (a turn of the loop's that failed).
    Hub,
}

impl ControlBy {
    /// Whether it is [`ControlBy::User`] (the default, left out where it is serialized as who
    /// asked for a loop).
    pub fn is_user(&self) -> bool {
        *self == ControlBy::User
    }

    /// `the user`, `the orchestrator` or `the hub`.
    pub fn words(&self) -> &'static str {
        match self {
            ControlBy::User => "the user",
            ControlBy::Orchestrator => "the orchestrator",
            ControlBy::Hub => "the hub",
        }
    }
}

/// What a running loop waits for, as its card says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitingOn {
    /// A turn of the thread.
    #[default]
    Thread,
    /// A review run.
    Review,
    /// The user: paused with nothing running, or stuck.
    User,
}

/// The loop line a card draws after the [`badge`]: the profile, where the loop is and the
/// ledger's size: `high · scope`, `high · ladder m3 · ledger 2`, `high · sweep`,
/// `medium · verify 1/2`, `high · report`, with `· paused` when it is paused (and `· stuck`
/// when a step failed and waits for the user, `· waiting on box devbox` while its thread's
/// remote box is away and the loop needs it for a run). An implementation loop's first phases read
/// `high · plan`, `high · plan check` and `high · implementing`. With `sweep = "runs"` the
/// sweep reads `high · sweep · rules` (the rules turn), `high · sweep · auditing 2/5` (the
/// rules whose audit run ended, of all) and `high · sweep · verifying` (the thread's turn
/// with the reports).
pub fn line(l: &LoopView) -> String {
    let [full, ..] = line_forms(l);
    full
}

/// The loop line in the forms a card too narrow for it falls back on, longest first: the
/// [`line`] itself (`high · ladder m3 · ledger 2`); without the profile (`ladder m3 · ledger
/// 2`); and short (`m3 · ldg 2`, `v1/5`, `sweep · ldg 2 · paused`; `plan`, `p1`, `impl` for an
/// implementation loop's first phases; `rules`, `aud 2/5` for the sweep's audit runs). Where the loop is comes first in each, so a cut at the
/// end keeps it longest.
pub fn line_forms(l: &LoopView) -> [String; 3] {
    let (place, short_place) = match l.phase {
        Phase::Ladder => match &l.round {
            Some(r) => (format!("ladder {r}"), r.clone()),
            None => ("ladder".into(), "ladder".into()),
        },
        Phase::Verify => {
            let n = l
                .round
                .as_deref()
                .and_then(|r| r.strip_prefix(contract::VERIFY_TAG))
                .and_then(|n| n.parse::<u32>().ok())
                .unwrap_or(l.verify.done);
            (
                format!("verify {n}/{}", l.verify.cap),
                format!("{}{n}/{}", contract::VERIFY_TAG, l.verify.cap),
            )
        }
        Phase::PlanCheck => (
            Phase::PlanCheck.as_str().to_string(),
            l.round
                .clone()
                .unwrap_or_else(|| Phase::PlanCheck.as_str().to_string()),
        ),
        Phase::Implement => ("implementing".into(), "impl".into()),
        // `sweep = "runs"`: the rules turn, the audit runs (ended of all), the verification.
        Phase::Sweep if l.wait == Wait::Rules => ("sweep · rules".into(), "rules".into()),
        Phase::Sweep if l.wait == Wait::Audits => {
            let done = l.audits.iter().filter(|a| a.status.ended()).count();
            let all = l.audits.len();
            (
                format!("sweep · auditing {done}/{all}"),
                format!("aud {done}/{all}"),
            )
        }
        Phase::Sweep if !l.audits.is_empty() && l.wait == Wait::Sweep => {
            ("sweep · verifying".into(), "sweep".into())
        }
        p => (p.as_str().to_string(), p.as_str().to_string()),
    };
    let (ledger, short_ledger) = if l.ledger > 0 {
        (
            Some(format!("ledger {}", l.ledger)),
            Some(format!("ldg {}", l.ledger)),
        )
    } else {
        (None, None)
    };
    let state = if l.stuck.is_some() {
        Some("stuck".to_string())
    } else if let Some(b) = &l.box_away {
        Some(format!("waiting on box {b}"))
    } else if l.paused {
        Some("paused".to_string())
    } else {
        None
    };
    let join =
        |parts: Vec<Option<String>>| parts.into_iter().flatten().collect::<Vec<_>>().join(" · ");
    [
        join(vec![
            Some(l.profile.clone()),
            Some(place.clone()),
            ledger.clone(),
            state.clone(),
        ]),
        join(vec![Some(place), ledger, state.clone()]),
        join(vec![Some(short_place), short_ledger, state]),
    ]
}
