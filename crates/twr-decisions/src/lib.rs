//! twr decision layer: one [`DecisionProvider`] trait, a [`Ladder`] that runs providers in
//! order, and the rungs: [`CodeProvider`] and [`OrchestratorRung`].
//!
//! Every decision site (route, grade, block_kind) is a ladder: code rule, then the orchestrator,
//! then the user (route) or the site default (grade: `success` at 0.6; block_kind:
//! `user_blocked`). [`Ladder::run`] returns the outcome plus one [`DecisionRecord`] per rung
//! that answered, which the hub appends as `decision.made`.
//!
//! The crate is pure: no IO, no hub dependency, nothing that waits. Contexts are built by the
//! caller (see [`RouteContext::from_registry`]).

pub mod code;
pub mod context;
pub mod ladder;
pub mod noise;
pub mod orchestrator;
pub mod provider;

use serde::{Deserialize, Serialize};
pub use twr_core::event::DecisionProvider as ProviderKind;
pub use twr_core::event::DecisionSite;
use twr_core::event::Grade;

pub use code::CodeProvider;
pub use context::{
    AttentionInfo, BlockContext, DecisionContext, GradeContext, RouteContext, ThreadInfo,
};
pub use ladder::{DecisionRecord, Graded, Ladder, LadderOutcome, LadderRun};
pub use orchestrator::OrchestratorRung;
pub use provider::DecisionProvider;

/// Route option meaning "spawn a new thread for this" (offered to the user). Route options
/// that are not threads have a space in them, which no thread name has, so a worker can't
/// shadow them or be shadowed by them.
pub const NEW_THREAD: &str = "new thread";
/// `chosen` of a route decision that hands the message to the orchestrator.
pub const DELEGATE: &str = "orchestrator";

/// Every grade, as offered to the grading decision.
pub const GRADE_OPTIONS: [&str; 4] = ["success", "partial", "gave_up", "needs_verification"];
/// Grade used when every grading rung abstains (the code rule's default).
pub const GRADE_DEFAULT: Grade = Grade::Success;
/// Confidence of [`GRADE_DEFAULT`].
pub const GRADE_DEFAULT_CONFIDENCE: f64 = 0.6;
/// Every block kind, as offered to the block_kind decision.
pub const BLOCK_KIND_OPTIONS: [&str; 2] = ["user_blocked", "transient"];

/// One rung's answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    /// The option chosen (logged as `decision.made.chosen`): a thread name, [`NEW_THREAD`],
    /// [`DELEGATE`], a grade or a block kind.
    pub chosen: String,
    /// Confidence in [0, 1]; code rules answer 1.0 unless stated otherwise.
    pub confidence: f64,
    /// Who decided.
    pub provider: ProviderKind,
    /// Route site only: the structured action. A route answer without one is not actionable;
    /// always `Some` on a route decision the ladder accepted.
    pub route: Option<RouteOutcome>,
}

impl Decision {
    /// A decision without a structured route action.
    pub fn new(chosen: impl Into<String>, confidence: f64, provider: ProviderKind) -> Self {
        Decision {
            chosen: chosen.into(),
            confidence,
            provider,
            route: None,
        }
    }
}

/// What to do with a user message (the route site's result).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum RouteOutcome {
    /// Send `message` (addressing prefix stripped) to thread `thread` (id) as a user turn.
    SendTo {
        /// Target thread id.
        thread: String,
        /// Message to send.
        message: String,
    },
    /// The message answers an open permission/question item: resolve it via respond.
    RespondAttention {
        /// Thread the item belongs to (id).
        thread: String,
        /// Attention id.
        attn_id: String,
        /// The answer.
        answer: ReplyAnswer,
    },
    /// Hand the message, verbatim, to the orchestrator as a user turn.
    Delegate,
    /// Nobody decided: ask the user, offering `options` (thread names + [`NEW_THREAD`]).
    AskUser {
        /// Options for the hub's question.
        options: Vec<String>,
    },
    /// Explicit `new <project>: <message>`: spawn a thread for `project`. `message` may be
    /// empty (spawn without a first prompt).
    NewThreadRequest {
        /// Project name as typed.
        project: String,
        /// First prompt.
        message: String,
    },
}

/// An answer to an attention item derived from a reply.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplyAnswer {
    /// An answer label: `allow`, `deny`, or one of the item's options as listed.
    Label(String),
    /// 0-based index into the item's options (the user typed the 1-based number).
    OptionIndex(usize),
}

/// Why a thread is blocked after a failed turn or crash (the block_kind site's result).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    /// Needs the user (raise an error attention).
    UserBlocked,
    /// Rate limit / overload / network: eligible for one automatic retry.
    Transient,
}

impl BlockKind {
    /// Wire name (`user_blocked` / `transient`).
    pub fn as_str(self) -> &'static str {
        match self {
            BlockKind::UserBlocked => "user_blocked",
            BlockKind::Transient => "transient",
        }
    }

    /// Parses a wire name.
    pub fn parse(s: &str) -> Option<BlockKind> {
        match s {
            "user_blocked" => Some(BlockKind::UserBlocked),
            "transient" => Some(BlockKind::Transient),
            _ => None,
        }
    }
}

/// The wire name of a grade (`gave_up`, ...).
pub fn grade_str(g: Grade) -> &'static str {
    match g {
        Grade::Success => "success",
        Grade::Partial => "partial",
        Grade::GaveUp => "gave_up",
        Grade::NeedsVerification => "needs_verification",
    }
}

/// Parses a grade's wire name.
pub fn parse_grade(s: &str) -> Option<Grade> {
    match s {
        "success" => Some(Grade::Success),
        "partial" => Some(Grade::Partial),
        "gave_up" => Some(Grade::GaveUp),
        "needs_verification" => Some(Grade::NeedsVerification),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grade_names_round_trip() {
        for g in [
            Grade::Success,
            Grade::Partial,
            Grade::GaveUp,
            Grade::NeedsVerification,
        ] {
            assert_eq!(
                serde_json::to_value(g).unwrap(),
                serde_json::json!(grade_str(g))
            );
            assert!(GRADE_OPTIONS.contains(&grade_str(g)));
            assert_eq!(parse_grade(grade_str(g)), Some(g));
        }
        assert_eq!(parse_grade("great"), None);
    }

    #[test]
    fn block_kind_names_round_trip() {
        for k in [BlockKind::UserBlocked, BlockKind::Transient] {
            assert_eq!(BlockKind::parse(k.as_str()), Some(k));
            assert!(BLOCK_KIND_OPTIONS.contains(&k.as_str()));
            assert_eq!(
                serde_json::to_value(k).unwrap(),
                serde_json::json!(k.as_str())
            );
        }
        assert_eq!(BlockKind::parse("stuck"), None);
    }
}
