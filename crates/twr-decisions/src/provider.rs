//! The decision provider trait.

use crate::context::DecisionContext;
use crate::{Decision, ProviderKind};

/// One rung of a decision ladder.
///
/// `decide` returns `None` to abstain (the ladder moves to the next rung). The site is
/// [`DecisionContext::site`]; `options` are the choices offered at that site (thread names +
/// [`crate::NEW_THREAD`] for route, [`crate::GRADE_OPTIONS`], [`crate::BLOCK_KIND_OPTIONS`]).
/// Rungs answer at once: every decision site runs its ladder where the hub reacts.
pub trait DecisionProvider: Send + Sync {
    /// Which provider this is (`decision.made.provider`). The ladder stamps it on every
    /// decision this rung returns.
    fn kind(&self) -> ProviderKind;

    /// Short name for logs.
    fn name(&self) -> &'static str {
        self.kind().as_str()
    }

    /// Decides, or abstains with `None`.
    fn decide(&self, ctx: &DecisionContext, options: &[String]) -> Option<Decision>;
}
