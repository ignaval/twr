//! Structured inputs to decisions, one struct per site.
//!
//! Contexts hold structured fields plus, for the grading rules, the turn's final text
//! ([`GradeContext::final_text`]).

use std::collections::HashSet;

use twr_core::config::CardOrder;
use twr_core::event::{AttentionReason, JsonMap, TurnResult};
use twr_core::reduce::{
    stack_order, AttentionItem, AttentionStack, Registry, ThreadStatus, ThreadView,
};

use crate::{DecisionSite, NEW_THREAD};

/// Input to a decision.
#[derive(Debug, Clone, PartialEq)]
pub enum DecisionContext {
    /// Routing a user message.
    Route(RouteContext),
    /// Grading a finished turn.
    Grade(GradeContext),
    /// Classifying a failed turn or crash as user-blocked or transient.
    BlockKind(BlockContext),
}

impl DecisionContext {
    /// The decision site.
    pub fn site(&self) -> DecisionSite {
        match self {
            DecisionContext::Route(_) => DecisionSite::Route,
            DecisionContext::Grade(_) => DecisionSite::Grade,
            DecisionContext::BlockKind(_) => DecisionSite::BlockKind,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Route
// ---------------------------------------------------------------------------------------------

/// The most urgent open attention item of a thread.
#[derive(Debug, Clone, PartialEq)]
pub struct AttentionInfo {
    /// Attention id.
    pub attn_id: String,
    /// Reason.
    pub reason: AttentionReason,
    /// Answer options, as listed on the item.
    pub options: Vec<String>,
    /// Whether a reply may answer this item on the user's behalf: the user had seen it when
    /// they wrote the message (it was raised at or before the control position the client
    /// rendered, `say` `seen_gseq`). A strict answer to an item that is not answerable makes
    /// routing abstain.
    pub answerable: bool,
}

impl AttentionInfo {
    /// From an open item; not [`AttentionInfo::answerable`] until the caller says so
    /// ([`RouteContext::set_answerable`]).
    pub fn from_item(item: &AttentionItem) -> Self {
        AttentionInfo {
            attn_id: item.attn_id.clone(),
            reason: item.reason,
            options: item.options.clone(),
            answerable: false,
        }
    }
}

/// What routing knows about one thread.
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadInfo {
    /// Thread id.
    pub id: String,
    /// Name.
    pub name: String,
    /// Aliases.
    pub aliases: Vec<String>,
    /// The names it had before its renames ([`ThreadView::former_names`]): they still name it.
    pub former_names: Vec<String>,
    /// Project.
    pub project: Option<String>,
    /// Derived status.
    pub status: ThreadStatus,
    /// Runtime name.
    pub runtime: String,
    /// The orchestrator thread (never a single-thread routing target).
    pub is_orchestrator: bool,
    /// Stopped (never a single-thread routing target; loses name ties, like
    /// [`Registry::resolve`]).
    pub stopped: bool,
    /// Most urgent open attention item, by the stack ordering.
    pub top_attention: Option<AttentionInfo>,
    /// Summary of the latest report.
    pub last_report: Option<String>,
}

impl ThreadInfo {
    /// From a thread view and its most urgent open item.
    pub fn from_view(view: &ThreadView, top: Option<&AttentionItem>) -> Self {
        ThreadInfo {
            id: view.id.clone(),
            name: view.name.clone(),
            aliases: view.aliases.clone(),
            former_names: view.former_names.clone(),
            project: view.project.clone(),
            status: view.status(),
            runtime: view.runtime.clone(),
            is_orchestrator: view.is_orchestrator(),
            stopped: view.stopped,
            top_attention: top.map(AttentionInfo::from_item),
            last_report: view.last_report.as_ref().map(|r| r.summary.clone()),
        }
    }

    /// Whether the top item blocks the thread (permission / question), i.e. a reply should
    /// answer it rather than be sent as text.
    pub fn blocking_attention(&self) -> Option<&AttentionInfo> {
        self.top_attention
            .as_ref()
            .filter(|a| a.reason.is_blocking())
    }

    fn is_worker(&self) -> bool {
        !self.is_orchestrator && !self.stopped
    }
}

/// Input to the route site.
#[derive(Debug, Clone, PartialEq)]
pub struct RouteContext {
    /// The user's message, verbatim.
    pub message: String,
    /// Every thread (the reserved `hub` thread excluded), oldest first.
    pub threads: Vec<ThreadInfo>,
    /// Focused thread id (the client's focus, else the leftmost in the stack).
    pub focused: Option<String>,
    /// Whether an orchestrator is configured (it may be spawned lazily), so the message can be
    /// delegated.
    pub orchestrator_available: bool,
}

/// Why a thread reference did not resolve (mirrors [`twr_core::reduce::ResolveError`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unresolved {
    /// Nothing matched.
    NotFound,
    /// Several threads matched.
    Ambiguous,
}

impl RouteContext {
    /// A context over `threads` with nothing focused and no orchestrator.
    pub fn new(message: impl Into<String>, threads: Vec<ThreadInfo>) -> Self {
        RouteContext {
            message: message.into(),
            threads,
            focused: None,
            orchestrator_available: false,
        }
    }

    /// Builds the context from the hub's reducers. `focused` (thread id) defaults to the
    /// leftmost thread of [`stack_order`].
    pub fn from_registry(
        registry: &Registry,
        attention: &AttentionStack,
        pins: &HashSet<String>,
        focused: Option<&str>,
        message: &str,
        orchestrator_available: bool,
    ) -> Self {
        let threads = registry
            .list()
            .into_iter()
            .map(|v| ThreadInfo::from_view(v, attention.for_thread(&v.id, pins).first().copied()))
            .collect();
        let focused = match focused {
            Some(f) => Some(f.to_string()),
            // The most urgent, else the most recently active: not the TUI's stable order.
            None => stack_order(registry, attention, pins, CardOrder::Attention)
                .into_iter()
                .next(),
        };
        RouteContext {
            message: message.to_string(),
            threads,
            focused,
            orchestrator_available,
        }
    }

    /// Marks each thread's top item [`AttentionInfo::answerable`] when `answerable(attn_id)`
    /// (and not otherwise). Contexts built by [`RouteContext::from_registry`] start with no
    /// answerable item, so a reply answers nothing unless the caller proved the user saw it.
    pub fn set_answerable(&mut self, answerable: impl Fn(&str) -> bool) {
        for a in self
            .threads
            .iter_mut()
            .filter_map(|t| t.top_attention.as_mut())
        {
            a.answerable = answerable(&a.attn_id);
        }
    }

    /// Thread by id.
    pub fn thread(&self, id: &str) -> Option<&ThreadInfo> {
        self.threads.iter().find(|t| t.id == id)
    }

    /// The focused thread, looked up by id (or exact name).
    pub fn focused_thread(&self) -> Option<&ThreadInfo> {
        let f = self.focused.as_deref()?;
        self.thread(f)
            .or_else(|| self.threads.iter().find(|t| t.name == f))
    }

    /// Single-thread routing candidates: non-orchestrator, non-stopped threads.
    pub fn workers(&self) -> Vec<&ThreadInfo> {
        self.threads.iter().filter(|t| t.is_worker()).collect()
    }

    /// Options offered at the route site, and to the user when routing asks: worker names
    /// and [`NEW_THREAD`].
    pub fn options(&self) -> Vec<String> {
        let mut v: Vec<String> = self.workers().iter().map(|t| t.name.clone()).collect();
        v.push(NEW_THREAD.to_string());
        v
    }

    /// Resolves a user reference with [`Registry::resolve`] semantics: exact id, then exact
    /// name, then alias (several exact matches resolve to the only non-stopped one, else
    /// ambiguous), then a former name (several: ambiguous), then, when `prefix` is set, a
    /// unique prefix of a name or id.
    pub fn resolve(&self, query: &str, prefix: bool) -> Result<&ThreadInfo, Unresolved> {
        if let Some(t) = self.thread(query) {
            return Ok(t);
        }
        let by_name: Vec<&ThreadInfo> = self.threads.iter().filter(|t| t.name == query).collect();
        if let Some(r) = pick_duplicate(by_name) {
            return r;
        }
        let by_alias: Vec<&ThreadInfo> = self
            .threads
            .iter()
            .filter(|t| t.aliases.iter().any(|a| a == query))
            .collect();
        if let Some(r) = pick_duplicate(by_alias) {
            return r;
        }
        let by_former: Vec<&ThreadInfo> = self
            .threads
            .iter()
            .filter(|t| t.former_names.iter().any(|n| n == query))
            .collect();
        match by_former.as_slice() {
            [] => {}
            [one] => return Ok(one),
            _ => return Err(Unresolved::Ambiguous),
        }
        if !prefix || query.is_empty() {
            return Err(Unresolved::NotFound);
        }
        let matches: Vec<&ThreadInfo> = self
            .threads
            .iter()
            .filter(|t| t.name.starts_with(query) || t.id.starts_with(query))
            .collect();
        match matches.as_slice() {
            [one] => Ok(one),
            [] => Err(Unresolved::NotFound),
            _ => Err(Unresolved::Ambiguous),
        }
    }
}

fn pick_duplicate(matches: Vec<&ThreadInfo>) -> Option<Result<&ThreadInfo, Unresolved>> {
    match matches.as_slice() {
        [] => None,
        [one] => Some(Ok(one)),
        many => {
            let live: Vec<&ThreadInfo> = many.iter().copied().filter(|t| !t.stopped).collect();
            Some(match live.as_slice() {
                [one] => Ok(one),
                _ => Err(Unresolved::Ambiguous),
            })
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Grade / block kind
// ---------------------------------------------------------------------------------------------

/// Input to the grade site (on `turn.ended`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct GradeContext {
    /// How the turn ended.
    pub result: TurnResult,
    /// The turn's final agent text (the give-up phrase rule reads it).
    pub final_text: Option<String>,
    /// Tool calls made in the turn.
    pub tool_calls: u32,
    /// Tool results with `ok: false` (the tool failed, or the user denied it).
    pub tool_failures: u32,
    /// Turn duration.
    pub duration_ms: Option<u64>,
    /// Some `turn.ended` `ext.<runtime>.transient` flag is true.
    pub transient: bool,
}

/// Input to the block_kind site (on `turn.ended{error}` or `thread.crashed`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BlockContext {
    /// How the turn ended; `None` for a crash without a turn result.
    pub result: Option<TurnResult>,
    /// The runtime process crashed.
    pub crashed: bool,
    /// Process exit code, for crashes.
    pub exit_code: Option<i32>,
    /// Some `ext.<runtime>.transient` flag is true (rate limit / overloaded / 5xx / network,
    /// as classified by the adapter).
    pub transient: bool,
    /// Some `ext.<runtime>.auth_failed` flag is true: the agent is not signed in.
    pub auth_failed: bool,
    /// The turn hit a usage or spending limit ([`limited_flag`]).
    pub limited: bool,
}

/// Whether any runtime section of an event's `ext` carries `transient: true`
/// (e.g. `ext.claude.transient`).
pub fn transient_flag(ext: Option<&JsonMap>) -> bool {
    ext.is_some_and(|m| {
        m.values()
            .any(|v| v.get("transient").and_then(|t| t.as_bool()) == Some(true))
    })
}

/// Whether any runtime section of an event's `ext` carries `auth_failed: true` (the agent
/// reported that it is not signed in, e.g. `ext.claude.auth_failed`).
pub fn auth_failed_flag(ext: Option<&JsonMap>) -> bool {
    ext.is_some_and(|m| {
        m.values()
            .any(|v| v.get("auth_failed").and_then(|t| t.as_bool()) == Some(true))
    })
}

/// Whether any section of an event's `ext` carries a `limit` object (`ext.twr.limit`, which
/// the adapters set on a turn that hit a usage or spending limit, or a runtime's own
/// `ext.<runtime>.limit`).
pub fn limited_flag(ext: Option<&JsonMap>) -> bool {
    ext.is_some_and(|m| {
        m.values()
            .any(|v| v.get("limit").is_some_and(|l| l.is_object()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use twr_core::event::{Event, EventBody, Grade};
    use twr_core::testkit::ThreadScript;

    fn orchestrator_created(s: &mut ThreadScript) -> Event {
        let mut ev = s.created_with_aliases("orchestrator", &["orch"]);
        if let EventBody::ThreadCreated(p) = &mut ev.body {
            p.ext = json!({"twr": {"role": "orchestrator"}})
                .as_object()
                .cloned();
        }
        ev
    }

    fn registry() -> (Registry, AttentionStack) {
        let mut evs = Vec::new();
        let mut o = ThreadScript::new("t0");
        evs.push(orchestrator_created(&mut o));
        let mut a = ThreadScript::new("t1");
        a.at(1);
        evs.push(a.created_with_aliases("api", &["backend"]));
        evs.push(a.turn_started("go"));
        evs.push(a.raise("p1", AttentionReason::Permission));
        let mut b = ThreadScript::new("t2");
        b.at(2);
        evs.push(b.created("apps"));
        evs.push(b.turn_started("x"));
        evs.push(b.turn_ended(TurnResult::Completed));
        evs.push(b.report(Some(1), Grade::Success));
        let mut c = ThreadScript::new("t3");
        c.at(3);
        evs.push(c.created("web"));
        evs.push(c.stopped());
        let mut d = ThreadScript::new("t4");
        d.at(4);
        evs.push(d.created("web"));
        let mut e = ThreadScript::new("t5");
        e.at(5);
        evs.push(e.created("docs"));
        evs.push(e.stopped());
        let mut f = ThreadScript::new("t6");
        f.at(6);
        evs.push(f.created("docs"));
        evs.push(f.stopped());
        (
            Registry::from_events(&evs),
            AttentionStack::from_events(&evs),
        )
    }

    #[test]
    fn from_registry_builds_structured_threads() {
        let (reg, attn) = registry();
        let pins = HashSet::new();
        let ctx = RouteContext::from_registry(&reg, &attn, &pins, None, "hi", true);
        assert_eq!(ctx.threads.len(), 7);
        let orch = ctx.thread("t0").unwrap();
        assert!(orch.is_orchestrator);
        assert_eq!(orch.aliases, vec!["orch"]);
        let api = ctx.thread("t1").unwrap();
        assert_eq!(api.status, ThreadStatus::Waiting);
        let top = api.blocking_attention().unwrap();
        assert_eq!(top.attn_id, "p1");
        assert_eq!(top.options, vec!["allow", "deny"]);
        assert!(ctx.thread("t2").unwrap().last_report.is_some());
        assert!(ctx.thread("t3").unwrap().stopped);
        // Default focus: the leftmost stack thread, the one with attention.
        assert_eq!(ctx.focused.as_deref(), Some("t1"));
        assert_eq!(ctx.focused_thread().unwrap().name, "api");
        let ctx = RouteContext::from_registry(&reg, &attn, &pins, Some("t2"), "hi", true);
        assert_eq!(ctx.focused_thread().unwrap().name, "apps");
        // Workers: not the orchestrator, not stopped.
        let names: Vec<&str> = ctx.workers().iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["api", "apps", "web"]);
        assert_eq!(ctx.options(), vec!["api", "apps", "web", NEW_THREAD]);
        assert!(ctx.orchestrator_available);
    }

    #[test]
    fn resolve_matches_registry_semantics() {
        let (reg, attn) = registry();
        let ctx = RouteContext::from_registry(&reg, &attn, &HashSet::new(), None, "", false);
        for q in [
            "t1",
            "t",
            "api",
            "backend",
            "orch",
            "orchestrator",
            "ap",
            "app",
            "apps",
            "a",
            "w",
            "web",
            "docs",
            "d",
            "zzz",
            "",
            "orc",
            "t3",
            "back",
        ] {
            let ours = ctx.resolve(q, true).map(|t| t.id.clone());
            let theirs = reg.resolve(q).map(|t| t.id.clone());
            match (&ours, &theirs) {
                (Ok(a), Ok(b)) => assert_eq!(a, b, "{q}"),
                (Err(Unresolved::NotFound), Err(twr_core::reduce::ResolveError::NotFound(_))) => {}
                (
                    Err(Unresolved::Ambiguous),
                    Err(twr_core::reduce::ResolveError::Ambiguous { .. }),
                ) => {}
                _ => panic!("{q}: ours {ours:?} registry {theirs:?}"),
            }
        }
        // Spot checks: alias, duplicate name with one live thread, prefix ambiguity.
        assert_eq!(ctx.resolve("backend", true).unwrap().name, "api");
        assert_eq!(ctx.resolve("web", true).unwrap().id, "t4");
        assert_eq!(ctx.resolve("docs", true), Err(Unresolved::Ambiguous));
        assert_eq!(ctx.resolve("ap", true), Err(Unresolved::Ambiguous));
        assert_eq!(ctx.resolve("orc", true).unwrap().id, "t0");
        // Without prefix matching only exact id/name/alias resolve.
        assert_eq!(ctx.resolve("orc", false), Err(Unresolved::NotFound));
        assert_eq!(ctx.resolve("orch", false).unwrap().id, "t0");
    }

    #[test]
    fn transient_flag_reads_any_runtime_section() {
        let m = |v: serde_json::Value| v.as_object().cloned();
        assert!(transient_flag(
            m(json!({"claude": {"transient": true}})).as_ref()
        ));
        assert!(transient_flag(
            m(json!({"twr": {}, "codex": {"transient": true}})).as_ref()
        ));
        assert!(!transient_flag(
            m(json!({"claude": {"transient": false}})).as_ref()
        ));
        assert!(!transient_flag(
            m(json!({"claude": {"transient": "yes"}})).as_ref()
        ));
        assert!(!transient_flag(m(json!({"transient": true})).as_ref()));
        assert!(!transient_flag(None));
    }

    #[test]
    fn limited_flag_reads_twr_and_runtime_sections() {
        let m = |v: serde_json::Value| v.as_object().cloned();
        assert!(limited_flag(
            m(json!({"twr": {"error": "x", "limit": {"kind": "usage", "message": "x"}}})).as_ref()
        ));
        assert!(limited_flag(
            m(json!({"codex": {"limit": {"kind": "spend"}}})).as_ref()
        ));
        assert!(!limited_flag(
            m(json!({"twr": {"limit": "usage"}})).as_ref()
        ));
        assert!(!limited_flag(m(json!({"twr": {"limit": null}})).as_ref()));
        assert!(!limited_flag(
            m(json!({"claude": {"transient": true}})).as_ref()
        ));
        assert!(!limited_flag(None));
    }
}
