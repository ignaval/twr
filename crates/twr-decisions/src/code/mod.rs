//! The `code` provider: deterministic rules, always present, always the first rung.
//!
//! - route: [`route::route`] (addressing, reply routing, single-thread routing)
//! - short commands for the orchestrator's relayed answers and mode changes: [`command`]
//!   (the relay guard and the `set_mode` gate in `twr-hub` `hub::orch`), and the text of the
//!   confirm card for everything else: [`card`] (`twr-hub` `hub::proposal`)
//! - grade: [`grade`] (error → gave_up; interrupted → partial; give-up phrase → gave_up;
//!   every tool call failed or denied → partial)
//! - block_kind: [`block_kind`] (adapter transient flag → transient)
//!
//! A matching rule answers with confidence 1.0 (the give-up phrase heuristic with 0.9, the
//! failed-tools heuristic with 0.8).

pub mod card;
pub mod command;
pub mod reply;
pub mod route;

use twr_core::event::{Grade, TurnResult};

use crate::context::{BlockContext, DecisionContext, GradeContext};
use crate::provider::DecisionProvider;
use crate::{grade_str, BlockKind, Decision, ProviderKind, RouteOutcome, DELEGATE, NEW_THREAD};

/// Phrases in a turn's final text that mean the agent gave up (matched case-insensitively).
pub const GIVE_UP_PHRASES: &[&str] = &[
    "i couldn't finish",
    "i could not finish",
    "i couldn't complete",
    "i could not complete",
    "i was unable to",
    "i'm unable to",
    "i am unable to",
    "i give up",
    "i'm giving up",
    "i am giving up",
    "unable to complete",
];

/// The code grading rule: error → gave_up (1.0); interrupted → partial (1.0); completed with
/// a give-up phrase in the final text → gave_up (0.9); completed with tool calls of which none
/// succeeded (each result `ok: false`: the user denied it or it failed) → partial (0.8: the
/// agent did not get to do what it set out to; `partial` raises no review);
/// otherwise abstain (the caller's default is [`crate::GRADE_DEFAULT`] at
/// [`crate::GRADE_DEFAULT_CONFIDENCE`]).
pub fn grade(ctx: &GradeContext) -> Option<(Grade, f64)> {
    match ctx.result {
        TurnResult::Error => Some((Grade::GaveUp, 1.0)),
        TurnResult::Interrupted => Some((Grade::Partial, 1.0)),
        TurnResult::Completed => {
            let text = ctx
                .final_text
                .as_deref()
                .unwrap_or_default()
                .replace('\u{2019}', "'")
                .to_lowercase();
            if GIVE_UP_PHRASES.iter().any(|p| text.contains(p)) {
                return Some((Grade::GaveUp, 0.9));
            }
            (ctx.tool_calls > 0 && ctx.tool_failures >= ctx.tool_calls)
                .then_some((Grade::Partial, 0.8))
        }
    }
}

/// The code block_kind rule: the adapter flagged the failure as a sign-in failure → user
/// blocked (never retried, whatever a later rung would say); transient → transient;
/// otherwise abstain (the caller's default is [`BlockKind::UserBlocked`]).
pub fn block_kind(ctx: &BlockContext) -> Option<BlockKind> {
    if ctx.auth_failed {
        return Some(BlockKind::UserBlocked);
    }
    ctx.transient.then_some(BlockKind::Transient)
}

/// The code rung.
#[derive(Debug, Clone, Copy, Default)]
pub struct CodeProvider;

impl CodeProvider {
    /// Decides synchronously (the rules never wait).
    pub fn decide_now(&self, ctx: &DecisionContext) -> Option<Decision> {
        match ctx {
            DecisionContext::Route(rc) => {
                let outcome = route::route(rc)?;
                let chosen = match &outcome {
                    RouteOutcome::SendTo { thread, .. }
                    | RouteOutcome::RespondAttention { thread, .. } => rc
                        .thread(thread)
                        .map_or_else(|| thread.clone(), |t| t.name.clone()),
                    RouteOutcome::NewThreadRequest { .. } => NEW_THREAD.to_string(),
                    RouteOutcome::Delegate => DELEGATE.to_string(),
                    RouteOutcome::AskUser { .. } => return None,
                };
                Some(Decision {
                    chosen,
                    confidence: 1.0,
                    provider: ProviderKind::Code,
                    route: Some(outcome),
                })
            }
            DecisionContext::Grade(gc) => {
                let (g, confidence) = grade(gc)?;
                Some(Decision::new(grade_str(g), confidence, ProviderKind::Code))
            }
            DecisionContext::BlockKind(bc) => {
                let k = block_kind(bc)?;
                Some(Decision::new(k.as_str(), 1.0, ProviderKind::Code))
            }
        }
    }
}

impl DecisionProvider for CodeProvider {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Code
    }

    fn decide(&self, ctx: &DecisionContext, _options: &[String]) -> Option<Decision> {
        self.decide_now(ctx)
    }
}

#[cfg(test)]
mod tests;
