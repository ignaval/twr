//! The orchestrator rung: hands unrouted messages to the orchestrator thread.

use crate::context::DecisionContext;
use crate::provider::DecisionProvider;
use crate::{Decision, ProviderKind, RouteOutcome, DELEGATE};

/// Route: [`RouteOutcome::Delegate`] (confidence 1.0) when
/// [`crate::RouteContext::orchestrator_available`], else abstain. Grade and block_kind:
/// always abstain (the orchestrator grades out of band via `write_report`, which the hub
/// logs itself).
#[derive(Debug, Clone, Copy, Default)]
pub struct OrchestratorRung;

impl OrchestratorRung {
    /// Decides synchronously.
    pub fn decide_now(&self, ctx: &DecisionContext) -> Option<Decision> {
        match ctx {
            DecisionContext::Route(rc) if rc.orchestrator_available => Some(Decision {
                chosen: DELEGATE.to_string(),
                confidence: 1.0,
                provider: ProviderKind::Orchestrator,
                route: Some(RouteOutcome::Delegate),
            }),
            _ => None,
        }
    }
}

impl DecisionProvider for OrchestratorRung {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Orchestrator
    }

    fn decide(&self, ctx: &DecisionContext, _options: &[String]) -> Option<Decision> {
        self.decide_now(ctx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{GradeContext, RouteContext};

    #[test]
    fn delegates_route_only_when_available() {
        let mut rc = RouteContext::new("hi", vec![]);
        assert!(OrchestratorRung
            .decide_now(&DecisionContext::Route(rc.clone()))
            .is_none());
        rc.orchestrator_available = true;
        let d = OrchestratorRung
            .decide_now(&DecisionContext::Route(rc))
            .unwrap();
        assert_eq!(d.route, Some(RouteOutcome::Delegate));
        assert_eq!(d.provider, ProviderKind::Orchestrator);
        assert!(OrchestratorRung
            .decide_now(&DecisionContext::Grade(GradeContext::default()))
            .is_none());
    }
}
