//! The decision ladder: providers in order, first actionable answer wins.

use std::sync::Arc;
use std::time::Instant;

use twr_core::event::{DecisionMade, Grade};

use crate::code::route::answer_shaped;
use crate::code::CodeProvider;
use crate::context::{DecisionContext, RouteContext, ThreadInfo};
use crate::orchestrator::OrchestratorRung;
use crate::provider::DecisionProvider;
use crate::{
    parse_grade, BlockKind, Decision, DecisionSite, ProviderKind, RouteOutcome, GRADE_DEFAULT,
    GRADE_DEFAULT_CONFIDENCE,
};

/// One rung's answer, acted on or not; the hub appends each as `decision.made`.
#[derive(Debug, Clone, PartialEq)]
pub struct DecisionRecord {
    /// Site.
    pub site: DecisionSite,
    /// Provider that answered.
    pub provider: ProviderKind,
    /// Options offered.
    pub options: Vec<String>,
    /// Option chosen.
    pub chosen: String,
    /// Confidence.
    pub confidence: f64,
    /// Time the provider took.
    pub latency_ms: u64,
}

impl From<DecisionRecord> for DecisionMade {
    fn from(r: DecisionRecord) -> Self {
        DecisionMade {
            site: r.site,
            provider: r.provider,
            options: r.options,
            chosen: r.chosen,
            confidence: r.confidence,
            latency_ms: r.latency_ms,
            ..Default::default()
        }
    }
}

/// How a ladder run ended.
#[derive(Debug, Clone, PartialEq)]
pub enum LadderOutcome {
    /// A rung's answer was accepted. Its `chosen` is valid for the site, and a route decision
    /// carries its [`RouteOutcome`] (never [`RouteOutcome::AskUser`]).
    Decided(Decision),
    /// Every rung abstained or answered something not actionable. Route: ask the user;
    /// grade / block_kind: the site default.
    Undecided,
}

/// Result of [`Ladder::run`].
#[derive(Debug, Clone, PartialEq)]
pub struct LadderRun {
    /// The outcome.
    pub outcome: LadderOutcome,
    /// One record per rung that answered, in ladder order (fell-through answers included;
    /// the accepted one, if any, is last).
    pub records: Vec<DecisionRecord>,
}

/// A grade with where it came from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Graded {
    /// The grade.
    pub grade: Grade,
    /// Confidence.
    pub confidence: f64,
    /// Who decided ([`ProviderKind::Code`] for the default).
    pub provider: ProviderKind,
    /// No rung decided; this is [`GRADE_DEFAULT`] at [`GRADE_DEFAULT_CONFIDENCE`], which has
    /// no [`DecisionRecord`] yet.
    pub defaulted: bool,
}

impl LadderRun {
    /// The accepted decision.
    pub fn decision(&self) -> Option<&Decision> {
        match &self.outcome {
            LadderOutcome::Decided(d) => Some(d),
            LadderOutcome::Undecided => None,
        }
    }

    /// Route result: the accepted action, else ask the user with [`RouteContext::options`].
    pub fn route(&self, ctx: &RouteContext) -> RouteOutcome {
        self.decision()
            .and_then(|d| d.route.clone())
            .unwrap_or_else(|| RouteOutcome::AskUser {
                options: ctx.options(),
            })
    }

    /// Grade result, defaulting to [`GRADE_DEFAULT`] at [`GRADE_DEFAULT_CONFIDENCE`].
    pub fn grade(&self) -> Graded {
        match self
            .decision()
            .and_then(|d| Some((d, parse_grade(&d.chosen)?)))
        {
            Some((d, grade)) => Graded {
                grade,
                confidence: d.confidence,
                provider: d.provider,
                defaulted: false,
            },
            None => Graded {
                grade: GRADE_DEFAULT,
                confidence: GRADE_DEFAULT_CONFIDENCE,
                provider: ProviderKind::Code,
                defaulted: true,
            },
        }
    }

    /// Block kind result, defaulting to [`BlockKind::UserBlocked`].
    pub fn block_kind(&self) -> BlockKind {
        self.decision()
            .and_then(|d| BlockKind::parse(&d.chosen))
            .unwrap_or(BlockKind::UserBlocked)
    }
}

/// Providers in order.
#[derive(Clone)]
pub struct Ladder {
    /// Rungs, first to last.
    pub providers: Vec<Arc<dyn DecisionProvider>>,
}

impl std::fmt::Debug for Ladder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let names: Vec<&str> = self.providers.iter().map(|p| p.name()).collect();
        f.debug_struct("Ladder").field("providers", &names).finish()
    }
}

impl Default for Ladder {
    fn default() -> Self {
        Ladder::standard()
    }
}

impl Ladder {
    /// A ladder over `providers`.
    pub fn new(providers: Vec<Arc<dyn DecisionProvider>>) -> Self {
        Ladder { providers }
    }

    /// The standard ladder: code, then the orchestrator.
    pub fn standard() -> Self {
        Ladder::new(vec![Arc::new(CodeProvider), Arc::new(OrchestratorRung)])
    }

    /// Runs the rungs in order until one answer is accepted.
    ///
    /// Each answer is recorded (with the rung's [`DecisionProvider::kind`] as provider). An
    /// answer is accepted when it is actionable: a known grade / block kind, or for route a
    /// [`RouteOutcome`] other than [`RouteOutcome::AskUser`] (and [`RouteOutcome::Delegate`]
    /// only when an orchestrator is available). Other answers fall through to the next rung.
    pub fn run(&self, ctx: &DecisionContext, options: &[String]) -> LadderRun {
        let site = ctx.site();
        let mut records = Vec::new();
        for p in &self.providers {
            let kind = p.kind();
            let start = Instant::now();
            let Some(mut d) = p.decide(ctx, options) else {
                continue;
            };
            let latency_ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
            d.provider = kind;
            records.push(DecisionRecord {
                site,
                provider: kind,
                options: options.to_vec(),
                chosen: d.chosen.clone(),
                confidence: d.confidence,
                latency_ms,
            });
            if let Some(d) = actionable(ctx, d) {
                return LadderRun {
                    outcome: LadderOutcome::Decided(d),
                    records,
                };
            }
        }
        LadderRun {
            outcome: LadderOutcome::Undecided,
            records,
        }
    }
}

fn actionable(ctx: &DecisionContext, d: Decision) -> Option<Decision> {
    match ctx {
        DecisionContext::Route(rc) => match &d.route {
            None | Some(RouteOutcome::AskUser { .. }) => None,
            Some(RouteOutcome::Delegate) if !rc.orchestrator_available => None,
            Some(RouteOutcome::SendTo { thread, message })
                if blocked_on_an_answer(rc, thread, message) =>
            {
                None
            }
            Some(_) => Some(d),
        },
        DecisionContext::Grade(_) => parse_grade(&d.chosen).map(|_| d),
        DecisionContext::BlockKind(_) => BlockKind::parse(&d.chosen).map(|_| d),
    }
}

/// Whether `thread` has an open permission/question and `message` looks like an attempt at
/// answering it ([`answer_shaped`]): such a message is never sent to it as text, whichever rung
/// picked the thread (only the code rules' strict grammar answers items; the user decides
/// otherwise).
fn blocked_on_an_answer(ctx: &RouteContext, thread: &str, message: &str) -> bool {
    ctx.thread(thread)
        .and_then(ThreadInfo::blocking_attention)
        .is_some_and(|attn| answer_shaped(attn, message))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use twr_core::event::TurnResult;
    use twr_core::reduce::ThreadStatus;

    use super::*;
    use crate::context::{BlockContext, GradeContext, ThreadInfo};
    use crate::NEW_THREAD;

    /// Answers `answer` (or abstains), counting calls.
    struct Stub {
        kind: ProviderKind,
        answer: Option<Decision>,
        calls: AtomicUsize,
        sleep_ms: u64,
    }

    impl Stub {
        fn new(kind: ProviderKind, answer: Option<Decision>) -> Arc<Stub> {
            Arc::new(Stub {
                kind,
                answer,
                calls: AtomicUsize::new(0),
                sleep_ms: 0,
            })
        }

        /// An orchestrator-kind rung answering `chosen` with an optional route.
        fn answering(chosen: &str, route: Option<RouteOutcome>) -> Arc<Stub> {
            let mut d = Decision::new(chosen, 1.0, ProviderKind::User);
            d.route = route;
            Stub::new(ProviderKind::Orchestrator, Some(d))
        }
    }

    impl DecisionProvider for Stub {
        fn kind(&self) -> ProviderKind {
            self.kind
        }
        fn decide(&self, _ctx: &DecisionContext, _options: &[String]) -> Option<Decision> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.sleep_ms > 0 {
                std::thread::sleep(std::time::Duration::from_millis(self.sleep_ms));
            }
            // Claims to be `user` to check that the ladder stamps the rung's kind.
            self.answer.clone()
        }
    }

    fn thread(id: &str, name: &str) -> ThreadInfo {
        ThreadInfo {
            id: id.into(),
            name: name.into(),
            aliases: Vec::new(),
            former_names: Vec::new(),
            project: None,
            status: ThreadStatus::Idle,
            runtime: "fake".into(),
            is_orchestrator: false,
            stopped: false,
            top_attention: None,
            last_report: None,
        }
    }

    fn route_ctx(msg: &str, threads: Vec<ThreadInfo>, orch: bool) -> RouteContext {
        let mut c = RouteContext::new(msg, threads);
        c.orchestrator_available = orch;
        c
    }

    fn two() -> Vec<ThreadInfo> {
        vec![thread("t1", "api"), thread("t2", "web")]
    }

    /// Code, then `extra` (instead of the orchestrator rung) when given.
    fn ladder(extra: Option<Arc<Stub>>) -> Ladder {
        match extra {
            Some(s) => Ladder::new(vec![Arc::new(CodeProvider), s]),
            None => Ladder::standard(),
        }
    }

    fn run(l: &Ladder, rc: &RouteContext) -> (LadderRun, RouteOutcome) {
        let run = l.run(&DecisionContext::Route(rc.clone()), &rc.options());
        let outcome = run.route(rc);
        (run, outcome)
    }

    fn send_to(thread: &str, message: &str) -> Option<RouteOutcome> {
        Some(RouteOutcome::SendTo {
            thread: thread.into(),
            message: message.into(),
        })
    }

    #[test]
    fn code_answer_is_accepted_and_stops_the_ladder() {
        let next = Stub::answering("web", send_to("t2", "fix it"));
        let l = ladder(Some(next.clone()));
        let rc = route_ctx("@api fix it", two(), true);
        let (r, outcome) = run(&l, &rc);
        assert_eq!(
            outcome,
            RouteOutcome::SendTo {
                thread: "t1".into(),
                message: "fix it".into()
            }
        );
        assert_eq!(next.calls.load(Ordering::SeqCst), 0);
        assert_eq!(r.records.len(), 1);
        let rec = &r.records[0];
        assert_eq!(rec.site, DecisionSite::Route);
        assert_eq!(rec.provider, ProviderKind::Code);
        assert_eq!(rec.chosen, "api");
        assert_eq!(rec.confidence, 1.0);
        assert_eq!(rec.options, vec!["api", "web", NEW_THREAD]);
    }

    #[test]
    fn unrouted_messages_delegate_or_ask() {
        let rc = route_ctx("deploy the thing", two(), true);
        let (r, outcome) = run(&Ladder::standard(), &rc);
        assert_eq!(outcome, RouteOutcome::Delegate);
        let got: Vec<_> = r
            .records
            .iter()
            .map(|x| (x.provider, x.chosen.as_str(), x.confidence))
            .collect();
        assert_eq!(got, vec![(ProviderKind::Orchestrator, "orchestrator", 1.0)]);
        // Without an orchestrator: ask the user.
        let rc = route_ctx("deploy the thing", two(), false);
        let (r, outcome) = run(&Ladder::standard(), &rc);
        assert_eq!(
            outcome,
            RouteOutcome::AskUser {
                options: vec!["api".into(), "web".into(), NEW_THREAD.into()]
            }
        );
        assert_eq!(r.outcome, LadderOutcome::Undecided);
        assert!(r.records.is_empty());
    }

    #[test]
    fn non_actionable_answers_fall_through_and_are_recorded() {
        // No route, asking the user, or delegating without an orchestrator: recorded, not
        // acted on, the stamped kind recorded.
        for (chosen, route, orch) in [
            ("web", None, false),
            ("x", Some(RouteOutcome::AskUser { options: vec![] }), false),
            ("orchestrator", Some(RouteOutcome::Delegate), false),
        ] {
            let rc = route_ctx("deploy", two(), orch);
            let (r, outcome) = run(&ladder(Some(Stub::answering(chosen, route))), &rc);
            assert!(matches!(outcome, RouteOutcome::AskUser { .. }), "{chosen}");
            assert_eq!(r.outcome, LadderOutcome::Undecided);
            assert_eq!(r.records.len(), 1, "{chosen}");
            assert_eq!(r.records[0].chosen, chosen);
            assert_eq!(r.records[0].provider, ProviderKind::Orchestrator);
        }
        // An actionable answer from a later rung is accepted with the rung's kind.
        let rc = route_ctx("deploy", two(), false);
        let (r, outcome) = run(
            &ladder(Some(Stub::answering("web", send_to("t2", "deploy")))),
            &rc,
        );
        assert_eq!(outcome, send_to("t2", "deploy").unwrap());
        assert_eq!(r.decision().unwrap().provider, ProviderKind::Orchestrator);
    }

    /// Whichever rung picks a thread, an answer-shaped message is never sent as text to a
    /// thread blocked on a permission/question: it falls through.
    #[test]
    fn a_picked_thread_blocked_on_an_item_does_not_get_an_answer_as_text() {
        use crate::context::AttentionInfo;
        use twr_core::event::AttentionReason;
        let mut api = thread("t1", "api");
        api.top_attention = Some(AttentionInfo {
            attn_id: "p1".into(),
            reason: AttentionReason::Permission,
            options: vec!["allow".into(), "deny".into()],
            answerable: false,
        });
        let threads = vec![api, thread("t2", "web")];
        for msg in ["yes?", "allow it", "2", "ok if tests pass"] {
            let rc = route_ctx(msg, threads.clone(), false);
            let (r, outcome) = run(
                &ladder(Some(Stub::answering("api", send_to("t1", msg)))),
                &rc,
            );
            assert!(
                matches!(outcome, RouteOutcome::AskUser { .. }),
                "{msg}: {outcome:?}"
            );
            assert_eq!(r.records.len(), 1, "{msg}");
        }
        // Not an attempt at an answer: sent as usual.
        let msg = "please rebase on main";
        let rc = route_ctx(msg, threads.clone(), false);
        let (_, outcome) = run(
            &ladder(Some(Stub::answering("api", send_to("t1", msg)))),
            &rc,
        );
        assert_eq!(outcome, send_to("t1", msg).unwrap());
        // An unblocked thread gets even a short "yes".
        let rc = route_ctx("yes?", threads, false);
        let (_, outcome) = run(
            &ladder(Some(Stub::answering("web", send_to("t2", "yes?")))),
            &rc,
        );
        assert!(
            matches!(outcome, RouteOutcome::SendTo { .. }),
            "{outcome:?}"
        );
    }

    #[test]
    fn zero_workers_delegate_or_ask() {
        let mut orch = thread("t0", "orchestrator");
        orch.is_orchestrator = true;
        let mut old = thread("t9", "old");
        old.stopped = true;
        let threads = vec![orch, old];
        let rc = route_ctx("build me a thing", threads.clone(), true);
        let (r, outcome) = run(&ladder(None), &rc);
        assert_eq!(outcome, RouteOutcome::Delegate);
        assert_eq!(r.records.len(), 1);
        assert_eq!(r.records[0].provider, ProviderKind::Orchestrator);
        let rc = route_ctx("build me a thing", threads, false);
        let (r, outcome) = run(&ladder(None), &rc);
        assert_eq!(
            outcome,
            RouteOutcome::AskUser {
                options: vec![NEW_THREAD.into()]
            }
        );
        assert!(r.records.is_empty());
    }

    #[test]
    fn grade_ladder_and_default() {
        let ctx = |result| {
            DecisionContext::Grade(GradeContext {
                result,
                final_text: Some("All done.".into()),
                ..Default::default()
            })
        };
        let opts: Vec<String> = crate::GRADE_OPTIONS.iter().map(|s| s.to_string()).collect();
        // Code decides errors.
        let r = Ladder::standard().run(&ctx(TurnResult::Error), &opts);
        let g = r.grade();
        assert_eq!(
            (g.grade, g.provider, g.defaulted),
            (Grade::GaveUp, ProviderKind::Code, false)
        );
        assert_eq!(r.records.len(), 1);
        // Code abstains; the orchestrator abstains on grade → default, nothing recorded.
        let r = Ladder::standard().run(&ctx(TurnResult::Completed), &opts);
        assert_eq!(r.outcome, LadderOutcome::Undecided);
        assert!(r.records.is_empty());
        assert_eq!(
            r.grade(),
            Graded {
                grade: Grade::Success,
                confidence: 0.6,
                provider: ProviderKind::Code,
                defaulted: true
            }
        );
        // Unknown grade from a later rung: recorded, not actionable.
        let r =
            ladder(Some(Stub::answering("great", None))).run(&ctx(TurnResult::Completed), &opts);
        assert!(r.grade().defaulted);
        assert_eq!(r.records.len(), 1);
        assert_eq!(r.records[0].site, DecisionSite::Grade);
    }

    #[test]
    fn block_kind_ladder_and_default() {
        let opts: Vec<String> = crate::BLOCK_KIND_OPTIONS
            .iter()
            .map(|s| s.to_string())
            .collect();
        let transient = DecisionContext::BlockKind(BlockContext {
            result: Some(TurnResult::Error),
            transient: true,
            ..Default::default()
        });
        let plain = DecisionContext::BlockKind(BlockContext {
            result: Some(TurnResult::Error),
            ..Default::default()
        });
        let r = ladder(None).run(&transient, &opts);
        assert_eq!(r.block_kind(), BlockKind::Transient);
        assert_eq!(r.records[0].provider, ProviderKind::Code);
        let r = ladder(None).run(&plain, &opts);
        assert_eq!(r.block_kind(), BlockKind::UserBlocked);
        assert!(r.records.is_empty());
        let limited = DecisionContext::BlockKind(BlockContext {
            result: Some(TurnResult::Error),
            transient: true,
            limited: true,
            ..Default::default()
        });
        let r = ladder(None).run(&limited, &opts);
        assert_eq!(r.block_kind(), BlockKind::Limited);
    }

    #[test]
    fn latency_is_measured_and_debug_names_the_rungs() {
        let slow = Arc::new(Stub {
            kind: ProviderKind::Orchestrator,
            answer: Some(Decision::new("web", 0.8, ProviderKind::User)),
            calls: AtomicUsize::new(0),
            sleep_ms: 20,
        });
        let l = Ladder::new(vec![slow as Arc<dyn DecisionProvider>]);
        let rc = route_ctx("deploy", two(), false);
        let (r, outcome) = run(&l, &rc);
        // No route: not actionable.
        assert!(matches!(outcome, RouteOutcome::AskUser { .. }));
        assert!(r.records[0].latency_ms >= 20, "{:?}", r.records[0]);
        assert_eq!(
            format!("{:?}", Ladder::standard()),
            r#"Ladder { providers: ["code", "orchestrator"] }"#
        );
    }

    #[test]
    fn record_converts_to_decision_made() {
        let rec = DecisionRecord {
            site: DecisionSite::Grade,
            provider: ProviderKind::Code,
            options: vec!["success".into()],
            chosen: "success".into(),
            confidence: 0.8,
            latency_ms: 12,
        };
        let m: DecisionMade = rec.into();
        assert_eq!(m.site, DecisionSite::Grade);
        assert_eq!(m.provider, ProviderKind::Code);
        assert_eq!(m.chosen, "success");
        assert_eq!(m.latency_ms, 12);
        assert!(m.ext.is_none());
    }
}
