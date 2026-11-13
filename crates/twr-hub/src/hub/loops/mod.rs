//! Loops in the hub (docs/design/loops.md, "Milestone 1: implementation notes"): who may
//! start a review loop, on which settings, and the socket methods around it. The loop itself
//! is the pure state machine of `twr_core::loops`; this module records the facts that start
//! and control it, and [`driver`] carries out what the machine asks for (prompts to the
//! thread, review runs, the stuck item, the report) and steps what happens back into it.
//!
//! - [`driver`]: the driver, which turn answers a step, review runs, restart.
//! - [`review`]: one review run (the reviewer's CLI, read-only).
//! - [`prompts`]: the templates and the texts sent.
//! - [`report`]: a loop as its log tells it, and `report.md`.
//! - [`git`]: scopes and round diffs.
//!
//! **The loop folder** is `<data>/loops/<id>/`, private: `findings.schema.json`, `goal.md`,
//! `ledger.md`, per round `diff-<slug>-<round>.patch`, `round-<round>.prompt.md`,
//! `round-<round>.log` (the reviewer's output, for people) and `round-<round>.answer.json`
//! (its answer), and at the end `report.md`.
//!
//! **The stuck item** (`Action::Stuck`) is an `attention.raised` with reason `error` on the
//! loop's thread, source `hub`, marked `ext.twr.loop_stuck = {loop, why}`, with the options
//! `resume` and `stop`. Its reason is `error`, not `question`, for the setup card's reasons
//! (below): a blocking hub item would hold the thread's other work and be dropped when its
//! runtime crashes or the hub's shutdown stops it; and a stuck loop is a step that failed. Only
//! the user answers it, with the setup card's guards: the orchestrator gets `relay_forbidden`
//! and may not annotate it, a thread's agent reaches neither `attention.respond` nor
//! `attention.dismiss`, a paired device needs `operate`. `resume` and `stop` step
//! `loop.control` in the batch of the item's resolution; a dismissal closes it and leaves the
//! loop paused. The hub closes it when the loop resumes, stops or ends.
//!
//! **Saved settings** (`loop.settings_saved` / `loop.settings_reset` on the `hub` thread,
//! source the user) are folded into [`LoopsState::saved`]: the workspace's set and each
//! project's own. Only a user saves them: an answer to a setup card, or `loop.settings.set`.
//! `[loops]` in the config file only sets what the first card shows; it never counts as saved
//! settings. Settings are checked against what the hub found of its own box's agent CLIs
//! (`hub::box_agents`: installed, signed in; a CLI that cannot say whether it is signed in,
//! Grok Build, counts as signed in once found) and the models and efforts twr's pickers know
//! (`twr_adapters::models`, `efforts`).
//!
//! **The rule** (`loop.request`): the thread must be a worker on the hub's own box, not
//! stopped, parked or being deleted, with a git repository to diff, in a mode that can edit
//! (`twr_adapters::mode_level_for`: not `Restricted`, and a mode twr can classify; the hub
//! never widens a mode), running no loop; `kind: "implement"` is `unsupported` in this
//! version. The hub then starts at once on saved settings only when the caller is the user,
//! `setup` is not set, the thread's project has saved settings (its own, else the
//! workspace's), they still hold, a named implementer is on the thread's runtime, and every
//! override is one the caller may apply (the user's own: all of them). Overrides apply to that
//! run only (`loop.started.overrides`); before the first save they prefill the card instead.
//! Anything else raises the setup card (`needs_setup`), replacing a card the thread already
//! has (same attention id, a new revision).
//!
//! **The setup card** is an `attention.raised` on the loop's thread, source `hub`, marked
//! `ext.twr.loop_setup` ([`LoopSetup`]), with the options [`START`] and [`CANCEL`]. Its reason
//! is `review`, not `question`: a hub-raised question on a worker blocks it (status
//! `waiting`, no stall or wake, no background timeout), and the reducer drops a thread's
//! blocking items when its runtime crashes or the hub's shutdown stops it, which would lose
//! the card across a restart. A non-blocking item stays until the hub resolves it, the thread
//! keeps working (a turn in flight, queued prompts) while it is open, and clients order it
//! with the questions by its marker (`AttentionItem::loop_setup`,
//! `twr_core::reduce::compare_attention`); notifications count it as a question. The hub
//! resolves it (`by: thread_stopped`) in the batch that stops, parks or deletes its thread
//! ([`Core::loop_closing`]), and [`LoopsState::cards`] is folded from the log like the
//! attention stack, so both survive a restart and a checkpoint in step.
//!
//! Only the user answers it: the orchestrator gets `relay_forbidden` (and may not annotate
//! it), a thread's agent reaches neither `attention.respond` nor `attention.dismiss`
//! (`AGENT_METHODS`), and a paired device needs `operate`. [`START`] checks the revision the
//! client showed (`attention.respond`'s own check), then the settings against what is
//! installed and the thread's eligibility now, and writes in one batch the card's
//! `attention.resolved`, `loop.settings_saved` (for `save_for`), a `thread.model_changed`
//! when a named implementer on the thread's runtime runs another model (the user's, as
//! `thread.set_model` records it), `loop.started` (models by value, `settings_from:
//! setup_item`) and the records [`LoopState::start`] returns; on any failure nothing is
//! written and the card stays. [`CANCEL`] resolves it and writes nothing else, and so does
//! `attention.dismiss`: a card is never dismissed into a start. `loop.setup.update` (the
//! user's `change…`) raises the card again with the new values under the same id, for the
//! revision the client showed.

mod driver;
mod git;
mod prompts;
mod report;
mod review;

pub(super) use driver::{reap_reviews, watch, LoopRuns};
pub(super) use report::{outcome_words, transition_words};

use std::collections::BTreeMap;
use std::sync::Arc;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use twr_core::adapter::Answer;
use twr_core::event::*;
use twr_core::loops::config::{
    check_profile_name, ModelChoice, ReviewerChoice, SweepMode, WorkersChoice, REVIEWER_RUNTIMES,
};
use twr_core::loops::settings::{
    presets, propose, validate, Availability, LoopSettings, Overrides, ProblemKind,
    RuntimeAvailability, SaveFor, SavedBy, SavedFrom, Scope, SettingsOf, SettingsSource,
    ThreadModel,
};
use twr_core::loops::{
    line, ControlAction, ControlBy, Input, LoopKind, LoopState, StartError, StepError,
};
use twr_core::proto::{codes, RpcError};
use twr_core::reduce::{AttentionItem, ThreadView};
use twr_core::runner::AgentLogin;

use super::model::ModelRequest;
use super::{
    append_err, internal, rpc, twr_ext, twr_ext_map, Batch, Caller, Core, Hub, Inner, Response,
    LOCAL_BOX,
};

/// The setup card's option that starts the loop and saves the settings it shows.
pub const START: &str = "start and save these settings";

/// The setup card's option that starts nothing and saves nothing.
pub const CANCEL: &str = "cancel";

/// What the hub folds of loops beyond each thread's own state (`ThreadView::loop`): the saved
/// settings and the open setup cards. Part of the hub's checkpoints.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct LoopsState {
    /// The saved settings.
    pub(super) saved: twr_core::loops::settings::SavedSettings,
    /// The open setup cards, by attention id.
    pub(super) cards: BTreeMap<String, SetupCard>,
    /// Per thread, the latest turn a loop's prompt started (`hub::loops::driver`).
    pub(super) asks: BTreeMap<String, Ask>,
    /// Per thread, the latest turn its loop took (an answer, or a failure): the turn stamped
    /// on that step's `loop.*` events.
    pub(super) consumed: BTreeMap<String, u64>,
    /// The open stuck items, by attention id: their thread.
    pub(super) stuck: BTreeMap<String, String>,
}

/// A turn a loop's prompt started: its loop, the step its marker names, and the turn.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Ask {
    /// The loop.
    pub(super) loop_id: String,
    /// The step (`scope`, `triage m3`, `sweep`, `report`).
    pub(super) step: String,
    /// The turn.
    pub(super) turn: u64,
}

/// An open setup card: its thread and what it shows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct SetupCard {
    /// The loop's thread.
    pub(super) thread: String,
    /// The card's payload, as last raised.
    pub(super) setup: LoopSetup,
}

// ---------------------------------------------------------------------------------------------
// Parameters
// ---------------------------------------------------------------------------------------------

/// `loop.request`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoopRequest {
    /// The thread (a name, alias or id).
    pub thread: String,
    /// `review` (`implement` is `unsupported` in this version).
    pub kind: String,
    /// Raise the setup card even when saved settings hold.
    pub setup: bool,
    /// `branch`, `uncommitted`, `since <ref>`, `last <n>`; default: `branch` when the thread
    /// has commits beyond its base, else `uncommitted`.
    pub scope: Option<String>,
    /// What this run changes, as the user typed it.
    pub overrides: Overrides,
}

/// `loop.setup.update`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SetupUpdate {
    /// The setup card.
    pub attn_id: String,
    /// The revision the client edited.
    pub revision: u64,
    /// The settings, whole.
    pub settings: Option<LoopSettings>,
    /// Where an answer saves them.
    pub save_for: Option<SaveFor>,
    /// The scope.
    pub scope: Option<String>,
}

/// `loop.setup.get`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SetupGet {
    /// The setup card.
    pub attn_id: String,
}

/// `loop.settings.get` and `loop.settings.reset`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SettingsFor {
    /// A project; none: the workspace's.
    pub project: Option<String>,
}

/// `loop.settings.set`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SettingsSet {
    /// A project; none: the workspace's.
    pub project: Option<String>,
    /// The settings, whole.
    pub settings: LoopSettings,
}

/// `loop.get`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoopGet {
    /// The thread: its running loop, else its last.
    pub thread: Option<String>,
    /// A loop by id.
    pub id: Option<String>,
}

/// `loop.control`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoopControlParams {
    /// The thread.
    pub thread: String,
    /// `pause`, `resume`, `stop` or `skip_to_verify`.
    pub action: ControlAction,
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// `relay_forbidden` for the orchestrator: no loop method is its own in this version.
fn users_only(caller: &Caller, what: &str) -> Result<(), RpcError> {
    if caller.is_orchestrator() {
        return Err(rpc(
            codes::RELAY_FORBIDDEN,
            format!("only the user may {what}: loops are not the orchestrator's in this version"),
        ));
    }
    Ok(())
}

/// The loop kind `kind` names: `review`; `implement` is `unsupported` in this version.
fn parse_kind(kind: &str) -> Result<LoopKind, RpcError> {
    match kind.trim() {
        "review" | "" => Ok(LoopKind::Review),
        "implement" => Err(rpc(
            codes::UNSUPPORTED,
            "not supported: implementation loops come in a later version; only review loops \
             run in this one",
        )),
        other => Err(rpc(
            codes::BAD_REQUEST,
            format!("{other:?} is no kind of loop (review, implement)"),
        )),
    }
}

/// The scope `scope` names, if any.
fn parse_scope(scope: Option<&str>) -> Result<Option<Scope>, RpcError> {
    scope
        .map(|s| s.parse::<Scope>().map_err(|e| rpc(codes::BAD_REQUEST, e)))
        .transpose()
}

/// Whether `s` is well formed: the strings a card or the editor sends parse as their
/// `[loops]` spelling would, and the sweep is the thread's (`runs` comes later).
fn check_shape(s: &LoopSettings) -> Result<(), RpcError> {
    let bad = |e: String| rpc(codes::BAD_REQUEST, e);
    check_profile_name(&s.profile).map_err(|e| bad(format!("profile: {e}")))?;
    if let Some(m) = &s.implementer {
        ModelChoice::parse(&m.to_string()).map_err(|e| bad(format!("implementer: {e}")))?;
    }
    WorkersChoice::parse(&s.workers.to_string()).map_err(|e| bad(format!("workers: {e}")))?;
    ReviewerChoice::parse(&s.reviewer.to_string()).map_err(|e| bad(format!("reviewer: {e}")))?;
    if s.sweep == SweepMode::Runs {
        return Err(bad(
            "sweep \"runs\" comes in a later version: the thread audits the rules itself \
             (\"thread\")"
                .into(),
        ));
    }
    Ok(())
}

/// The names of `runtime`'s accounts that are not removed, as the hub lists them for threads
/// (`account.list`), the native one's first; none for a runtime that takes no accounts.
fn account_names(accounts: &twr_core::account::AccountsView, runtime: &str) -> Vec<String> {
    if !twr_core::account::takes_accounts(runtime) {
        return Vec::new();
    }
    accounts
        .accounts(runtime)
        .into_iter()
        .map(|a| a.name)
        .collect()
}

/// What the hub found of its own box's agent CLIs, as loop settings are checked against it,
/// with `view` the thread a loop would run in. Each runtime's `accounts` are all its accounts
/// that are not removed, the native one included, so that an account a setting names and the
/// runtime does not have is a problem of the card and of `loop.settings.set`, not only of the
/// round that would run on it. A hub that does not probe its agent CLIs
/// (`TWR_AGENT_PROBE=off`) does not know: every runtime counts as installed and logged in
/// (`probed: false`), so no setting is refused or replaced for a runtime it never looked
/// for; a run whose CLI is truly missing fails, as any failed run does.
fn availability(
    hub: &Inner,
    accounts: &twr_core::account::AccountsView,
    view: Option<&ThreadView>,
) -> Availability {
    let probed = hub.local_agents.probes();
    let agents = hub.local_agents.known();
    let runtimes = REVIEWER_RUNTIMES
        .iter()
        .map(|rt| {
            let a = agents.iter().find(|a| a.runtime == *rt);
            let found = !probed || a.is_some_and(|a| a.found);
            RuntimeAvailability {
                runtime: rt.to_string(),
                found,
                probed,
                // A CLI with no status command (Grok Build) cannot say: found, it counts as
                // signed in.
                login: found && (!probed || a.is_some_and(|a| a.login != AgentLogin::LoggedOut)),
                models: twr_adapters::models(rt)
                    .iter()
                    .map(|m| m.id.to_string())
                    .collect(),
                efforts: twr_adapters::efforts(rt)
                    .unwrap_or(&[])
                    .iter()
                    .map(|e| e.to_string())
                    .collect(),
                accounts: account_names(accounts, rt),
                subagents: *rt == twr_adapters::claude_code::NAME,
            }
        })
        .collect();
    Availability {
        runtimes,
        thread: view.map(|v| ThreadModel {
            runtime: v.runtime.clone(),
            model: v.model.clone(),
            effort: v.effort.clone(),
        }),
    }
}

/// The problems `problems` as one message.
fn problem_words(problems: &[twr_core::loops::settings::Problem]) -> String {
    problems
        .iter()
        .map(|p| p.message.clone())
        .collect::<Vec<_>>()
        .join("; ")
}

/// `runtime · model · effort`.
fn model_words(runtime: &str, model: Option<&str>, effort: Option<&str>) -> String {
    let mut parts = vec![runtime.to_string()];
    parts.push(model.unwrap_or("its default model").to_string());
    if let Some(e) = effort {
        parts.push(e.to_string());
    }
    parts.join(" · ")
}

/// The card's summary: one line a generic client shows, then its rows (profile, scope,
/// implementer, workers, reviewer, this run's overrides, where it saves, warnings and what
/// has to change first).
fn card_text(name: &str, setup: &LoopSetup, view: &ThreadView) -> String {
    let kind = match setup.kind {
        LoopKind::Review => "review loop",
        LoopKind::Implement => "implementation loop",
    };
    let why = match setup.why {
        SetupWhy::FirstRun => "first run: choose its models".to_string(),
        SetupWhy::Asked => "check its settings".to_string(),
        SetupWhy::NoLongerHolds => "a saved setting no longer holds".to_string(),
        SetupWhy::OtherRuntime => {
            format!("{name} runs on another runtime than the saved implementer")
        }
        SetupWhy::Overrides => "the request carries settings you did not type".to_string(),
    };
    let s = &setup.settings;
    let mut rows = vec![format!(
        "{name}: {kind} setup ({why}). Start and save these settings?"
    )];
    rows.push(format!(
        "Profile      {}",
        setup
            .profile
            .as_ref()
            .map_or_else(|| s.profile.clone(), |p| p.summary())
    ));
    if let Some(scope) = &setup.scope {
        rows.push(format!(
            "Scope        {}",
            setup
                .scope_summary
                .clone()
                .unwrap_or_else(|| scope.to_string())
        ));
    }
    rows.push(format!(
        "Implementer  {}",
        match &s.implementer {
            Some(m) => model_words(&m.runtime, Some(&m.model), m.effort.as_deref()),
            None => format!(
                "{} (the thread's own)",
                model_words(&view.runtime, view.model.as_deref(), view.effort.as_deref())
            ),
        }
    ));
    let workers = match &s.workers.effort {
        Some(e) => format!("{} · {e}", s.workers.model),
        None => s.workers.model.clone(),
    };
    rows.push(format!("Workers      {workers}"));
    let mut efforts: Vec<String> = Vec::new();
    if let Some(p) = &setup.profile {
        for e in p.tiers.iter().map(|t| &t.effort).chain([&p.verify.effort]) {
            if !efforts.contains(e) {
                efforts.push(e.clone());
            }
        }
    }
    let mut reviewer = format!("{} · {}", s.reviewer.runtime, s.reviewer.model);
    if !efforts.is_empty() {
        reviewer.push_str(&format!(" · {}", efforts.join(" → ")));
    }
    rows.push(format!("Reviewer     {reviewer}"));
    let o = &setup.overrides;
    let mut run = Vec::new();
    for (what, v) in [
        ("profile", &o.profile),
        ("implementer", &o.implementer),
        ("workers", &o.workers),
        ("reviewer", &o.reviewer),
        ("commit", &o.commit),
    ] {
        if let Some(v) = v {
            run.push(format!("{what} {v}"));
        }
    }
    if !run.is_empty() {
        rows.push(format!("This run     {}", run.join(", ")));
    }
    rows.push(format!(
        "Save for     {}",
        match (setup.save_for, &setup.project) {
            (SaveFor::Project, Some(p)) => format!("{p} only"),
            _ => "all projects".into(),
        }
    ));
    rows.extend(setup.warnings.iter().map(|w| format!("Warning: {w}")));
    rows.extend(setup.problems.iter().map(|p| format!("Change first: {p}")));
    rows.join("\n")
}

/// The `loop.setup` marker of a card's `ext`.
fn setup_of(ext: &Option<JsonMap>) -> Option<LoopSetup> {
    let v = twr_ext(ext)?.get(LOOP_SETUP_KEY)?;
    serde_json::from_value(v.clone()).ok()
}

/// A start the machine refused.
fn start_err(e: StartError) -> RpcError {
    match e {
        StartError::NotSupported => rpc(codes::UNSUPPORTED, format!("not supported: {e}")),
        StartError::EmptyProfile(_) => rpc(codes::BAD_REQUEST, e.to_string()),
    }
}

/// The resolution of setup card `attn_id` with `answer` (none: closed by the hub).
fn resolved(attn_id: &str, by: ResolvedBy, answer: Option<&str>) -> EventBody {
    EventBody::AttentionResolved(AttentionResolved {
        attn_id: attn_id.into(),
        by,
        answer: answer.map(Answer::new),
        ..Default::default()
    })
}

/// What a loop starts from, besides its settings: the scope and the bases it found.
#[derive(Debug, Clone, Default)]
struct StartFrom {
    scope: Option<Scope>,
    bases: Vec<LoopBase>,
    baseline_digests: BTreeMap<String, String>,
}

impl StartFrom {
    /// From what git found of `target`.
    fn of(target: &git::Target, found: &git::ScopeInfo) -> StartFrom {
        StartFrom {
            scope: Some(found.scope.clone()),
            bases: vec![LoopBase {
                slug: target.slug.clone(),
                repo: target.dir.to_string_lossy().into_owned(),
                base: found.base.clone(),
            }],
            baseline_digests: [(target.slug.clone(), found.dirty_digest.clone())].into(),
        }
    }

    /// From a setup card.
    fn card(setup: &LoopSetup) -> StartFrom {
        StartFrom {
            scope: setup.scope.clone(),
            bases: setup.bases.clone(),
            baseline_digests: setup.baseline_digests.clone(),
        }
    }
}

/// Where a loop's settings come from when it starts.
struct Origin {
    from: SettingsSource,
    id: String,
    of: Option<SettingsOf>,
    overrides: Overrides,
}

// ---------------------------------------------------------------------------------------------
// The hub's methods
// ---------------------------------------------------------------------------------------------

impl Hub {
    /// The hub's own box's agent CLIs, probed once before the first answer that needs them (a
    /// hub just started has not finished its first probe yet).
    async fn loop_agents(&self) {
        let agents = &self.inner.local_agents;
        if agents.probes() && agents.probed_at().is_none() {
            agents.probe().await;
        }
    }

    /// Where thread `id` works, for [`git::scope`].
    fn loop_target(&self, id: &str) -> Result<git::Target, RpcError> {
        let core = self.inner.core();
        let v = core.view(id)?;
        let dir = v.worktree.clone().ok_or_else(|| {
            rpc(
                codes::UNSUPPORTED,
                format!("{} has no git workspace to diff", v.name),
            )
        })?;
        Ok(git::Target {
            name: v.name.clone(),
            dir: dir.into(),
            base_ref: v.base.clone(),
            slug: git::slug(v.project.as_deref().unwrap_or(&v.name)),
        })
    }

    /// The hub's own box's git.
    fn loop_git(&self) -> Result<Arc<twr_runner::local::LocalGit>, RpcError> {
        self.inner
            .host(LOCAL_BOX)
            .and_then(|h| h.local_git())
            .ok_or_else(|| rpc(codes::UNSUPPORTED, "the hub's own box has no git"))
    }

    /// `loop.request {thread, kind, setup?, scope?, overrides?}` (see the module docs):
    /// `{status: "started", loop}` or `{status: "needs_setup", attn_id, revision, why}`.
    pub async fn loop_request(&self, p: LoopRequest, caller: &Caller) -> Result<Value, RpcError> {
        users_only(caller, "start a loop")?;
        let kind = parse_kind(&p.kind)?;
        let scope = parse_scope(p.scope.as_deref())?;
        let id = {
            let core = self.inner.core();
            let id = core.resolve(&p.thread)?;
            core.loop_eligible(&id)?;
            id
        };
        let target = self.loop_target(&id)?;
        self.loop_agents().await;
        let found = git::scope(&*self.loop_git()?, &target, scope).await?;
        let mut core = self.inner.core();
        // Checked again: the thread may have changed while git ran.
        core.loop_eligible(&id)?;
        core.decide_loop(&self.inner, &id, kind, &p, &target, &found, caller)
    }

    /// `loop.setup.get {attn_id}`: open setup card `attn_id` as last raised, read only (no new
    /// revision): `{attn_id, revision, thread, thread_name, setup, summary, options}`.
    /// `not_found` when `attn_id` is no open setup card (none, another kind of item, or one
    /// resolved since).
    pub fn loop_setup_get(&self, p: SetupGet, caller: &Caller) -> Result<Value, RpcError> {
        users_only(caller, "read a loop's setup card")?;
        let core = self.inner.core();
        let card = core
            .loops
            .cards
            .get(&p.attn_id)
            .filter(|_| core.attention.get(&p.attn_id).is_some())
            .cloned()
            .ok_or_else(|| {
                rpc(
                    codes::NOT_FOUND,
                    format!("{} is no open loop setup card", p.attn_id),
                )
            })?;
        let item = core
            .attention
            .get(&p.attn_id)
            .cloned()
            .ok_or_else(|| internal("the card's item is gone"))?;
        Ok(json!({
            "attn_id": p.attn_id,
            "revision": item.revision,
            "thread": card.thread,
            "thread_name": core.thread_name(&card.thread),
            "setup": card.setup,
            "summary": item.summary,
            "options": item.options,
        }))
    }

    /// `loop.setup.update {attn_id, revision, settings?, save_for?, scope?}`: the user's
    /// change of setup card `attn_id` as shown at `revision`; raises it again with the new
    /// values (same id, a new revision): `{attn_id, revision, setup, summary, options}`.
    /// `conflict` when the card was raised again since `revision`, or is closed.
    pub async fn loop_setup_update(
        &self,
        p: SetupUpdate,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        users_only(caller, "change a loop's settings")?;
        if let Some(s) = &p.settings {
            check_shape(s)?;
        }
        let scope = parse_scope(p.scope.as_deref())?;
        let thread = {
            let core = self.inner.core();
            core.open_setup_card(&p.attn_id, p.revision)?.thread
        };
        let found = match scope {
            Some(scope) => {
                let target = self.loop_target(&thread)?;
                let found = git::scope(&*self.loop_git()?, &target, Some(scope)).await?;
                Some((target, found))
            }
            None => None,
        };
        self.loop_agents().await;
        let mut core = self.inner.core();
        let card = core.open_setup_card(&p.attn_id, p.revision)?;
        let view = core.view(&card.thread)?.clone();
        let avail = availability(&self.inner, &core.accounts, Some(&view));
        let config = &self.inner.config.loops;
        let mut setup = card.setup.clone();
        if let Some(s) = p.settings {
            let proposal = propose(&s, &avail, config);
            setup.settings = proposal.settings;
            setup.warnings = proposal.warnings;
            setup.problems = proposal
                .problems
                .iter()
                .map(|p| p.message.clone())
                .collect();
            setup.profile = config.profile(&setup.settings.profile);
        }
        if let Some(save_for) = p.save_for {
            if save_for == SaveFor::Project && setup.project.is_none() {
                return Err(rpc(
                    codes::BAD_REQUEST,
                    format!("{} has no project to save settings for", view.name),
                ));
            }
            setup.save_for = save_for;
        }
        if let Some((target, found)) = found {
            let from = StartFrom::of(&target, &found);
            setup.scope = from.scope;
            setup.scope_summary = Some(found.summary());
            setup.bases = from.bases;
            setup.baseline_digests = from.baseline_digests;
        }
        let (attn_id, revision) = core.raise_setup_card(&self.inner, &view, setup.clone())?;
        let item = core.attention.get(&attn_id).cloned();
        Ok(json!({
            "attn_id": attn_id,
            "revision": revision,
            "setup": setup,
            "summary": item.as_ref().map(|i| i.summary.clone()),
            "options": item.map(|i| i.options).unwrap_or_default(),
        }))
    }

    /// `loop.settings.get {project?}`: the saved set that applies to `project` (its own, else
    /// the workspace's) with `id` and `source` (`project`, `workspace`, or null before the
    /// first save), what the first card shows (`first_card`, from `[loops]`), the projects
    /// with settings of their own, the presets, each reviewer runtime as found (`runtimes`:
    /// `runtime, found, login, probed, models, efforts, subagents, accounts`; `accounts` empty
    /// when it has only its native one; `probed: false` when the hub does not probe its agent
    /// CLIs, `found` and `login` then true for not knowing), and the warnings and problems of
    /// the set that applies (else of the first card's).
    pub async fn loop_settings_get(&self, p: SettingsFor) -> Result<Value, RpcError> {
        self.loop_agents().await;
        let core = self.inner.core();
        let config = &self.inner.config.loops;
        let avail = availability(&self.inner, &core.accounts, None);
        let saved = core.loops.saved.for_project(p.project.as_deref());
        let first = LoopSettings::first_card(config);
        let shown = saved.map_or(&first, |(s, _)| &s.settings);
        let proposal = propose(shown, &avail, config);
        let runtimes: Vec<Value> = avail
            .runtimes
            .iter()
            .map(|r| {
                json!({
                    "runtime": r.runtime,
                    "found": r.found,
                    "login": r.login,
                    // `false`: the hub does not probe its agent CLIs; it counts this one as
                    // installed and logged in, not knowing.
                    "probed": r.probed,
                    "models": r.models,
                    "efforts": r.efforts,
                    "subagents": r.subagents,
                    // A runtime with only its native account has none to pick.
                    "accounts": if r.accounts.len() > 1 { r.accounts.clone() } else { Vec::new() },
                })
            })
            .collect();
        Ok(json!({
            "settings": saved.map(|(s, _)| &s.settings),
            "id": saved.map(|(s, _)| &s.id),
            "source": saved.map(|(_, of)| of),
            "first_card": first,
            "projects": core.loops.saved.projects.keys().collect::<Vec<_>>(),
            "presets": presets(),
            "profiles": config.profile_names(),
            "runtimes": runtimes,
            "warnings": proposal.warnings,
            "problems": proposal.problems.iter().map(|p| &p.message).collect::<Vec<_>>(),
        }))
    }

    /// `loop.settings.set {project?, settings}`: the user saves loop settings for every
    /// project (none) or for `project` (`loop.settings_saved`, `from: editor`): `{id}`.
    /// `bad_request` for settings that do not hold, naming each problem.
    pub async fn loop_settings_set(
        &self,
        p: SettingsSet,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        users_only(caller, "save loop settings")?;
        check_shape(&p.settings)?;
        self.loop_agents().await;
        let mut core = self.inner.core();
        let project = core.loop_project(&self.inner, p.project.as_deref())?;
        let avail = availability(&self.inner, &core.accounts, None);
        let problems = validate(&p.settings, &avail, &self.inner.config.loops);
        if !problems.is_empty() {
            return Err(rpc(
                codes::BAD_REQUEST,
                format!("these settings do not hold: {}", problem_words(&problems)),
            ));
        }
        let id = new_id();
        core.append(
            &self.inner,
            HUB_THREAD,
            None,
            caller.source(),
            EventBody::LoopSettingsSaved(Box::new(LoopSettingsSaved {
                id: id.clone(),
                project,
                settings: p.settings,
                by: SavedBy::User,
                from: SavedFrom::Editor,
                ..Default::default()
            })),
        )
        .map_err(append_err)?;
        Ok(json!({ "id": id }))
    }

    /// `loop.settings.reset {project?}`: the user forgets the saved settings of `project`
    /// (it uses the workspace's again), or, without one, every saved set (the next loop shows
    /// the first-run card): `{ok, reset}` (`reset`: whether there was anything to forget).
    pub fn loop_settings_reset(&self, p: SettingsFor, caller: &Caller) -> Result<Value, RpcError> {
        users_only(caller, "reset loop settings")?;
        let mut core = self.inner.core();
        let project = core.loop_project(&self.inner, p.project.as_deref())?;
        let saved = &core.loops.saved;
        let any = match &project {
            Some(p) => saved.projects.contains_key(p),
            None => saved.workspace.is_some() || !saved.projects.is_empty(),
        };
        if any {
            core.append(
                &self.inner,
                HUB_THREAD,
                None,
                caller.source(),
                EventBody::LoopSettingsReset(LoopSettingsReset {
                    project,
                    ..Default::default()
                }),
            )
            .map_err(append_err)?;
        }
        Ok(json!({ "ok": true, "reset": any }))
    }

    /// `loop.get {thread}` or `{id}`: the thread's running loop (else its last one), or loop
    /// `id`, from its thread's state and its log: `{thread, thread_name, id, kind, running,
    /// state, line, last, started, phases, rounds, ledger, controls, folder, files,
    /// report_path, outcome}`, each round with the `files` of it the folder has.
    pub fn loop_get(&self, p: LoopGet) -> Result<Value, RpcError> {
        let (view, id) = {
            let core = self.inner.core();
            match (&p.thread, &p.id) {
                (Some(t), None) => {
                    let id = core.resolve(t)?;
                    let v = core.view(&id)?.clone();
                    let loop_id = v
                        .r#loop
                        .as_ref()
                        .map(|l| l.id.clone())
                        .or_else(|| v.last_loop.as_ref().map(|l| l.id.clone()))
                        .ok_or_else(|| {
                            rpc(codes::NOT_FOUND, format!("{} has run no loop", v.name))
                        })?;
                    (v, loop_id)
                }
                (None, Some(id)) => {
                    let v = core
                        .registry
                        .list()
                        .into_iter()
                        .find(|v| {
                            v.r#loop.as_ref().is_some_and(|l| l.id == *id)
                                || v.last_loop.as_ref().is_some_and(|l| l.id == *id)
                        })
                        .cloned()
                        .ok_or_else(|| rpc(codes::NOT_FOUND, format!("no loop {id}")))?;
                    (v, id.clone())
                }
                _ => {
                    return Err(rpc(
                        codes::BAD_REQUEST,
                        "give either thread or id, not both",
                    ))
                }
            }
        };
        let events = {
            let conn = self.inner.reader();
            crate::store::loop_events(&conn, &view.id)
                .map_err(internal)?
                .into_iter()
                .map(|ev| super::append::hydrated_from_store(&conn, &ev).unwrap_or(ev))
                .collect::<Vec<_>>()
        };
        let mut detail = loop_detail(&view, &id, &events);
        add_files(&mut detail);
        Ok(detail)
    }

    /// `loop.list`: the running loops, `[{thread, thread_name, id, kind, profile, phase,
    /// round, ledger, paused, stuck, waiting_on, line}]`.
    pub fn loop_list(&self) -> Value {
        let core = self.inner.core();
        let mut out: Vec<Value> = core
            .registry
            .list()
            .into_iter()
            .filter_map(|v| {
                let l = v.r#loop.as_ref()?;
                Some(json!({
                    "thread": v.id,
                    "thread_name": v.name,
                    "id": l.id,
                    "kind": l.kind,
                    "profile": l.profile,
                    "phase": l.phase,
                    "round": l.round,
                    "ledger": l.ledger,
                    "paused": l.paused,
                    "stuck": l.stuck,
                    "waiting_on": l.waiting_on,
                    "line": line(l),
                }))
            })
            .collect();
        out.sort_by(|a, b| a["thread_name"].as_str().cmp(&b["thread_name"].as_str()));
        Value::Array(out)
    }

    /// `loop.control {thread, action}`: pauses, resumes, stops or skips to verify the thread's
    /// running loop (`loop.control` and the records its step decided, one batch): the loop as
    /// `loop.list` shows one, with `running: false` once it ended. `conflict` when the thread
    /// runs no loop or the action does nothing now.
    pub fn loop_control(&self, p: LoopControlParams, caller: &Caller) -> Result<Value, RpcError> {
        users_only(caller, "control a loop")?;
        let mut core = self.inner.core();
        let id = core.resolve(&p.thread)?;
        let view = core.view(&id)?.clone();
        let Some(mut state) = view.r#loop.clone() else {
            return Err(rpc(codes::CONFLICT, format!("{} runs no loop", view.name)));
        };
        let actions = state
            .step(
                Input::Control {
                    action: p.action,
                    by: ControlBy::User,
                },
                Utc::now(),
            )
            .map_err(|e| match e {
                StepError::Control(why) => rpc(
                    codes::CONFLICT,
                    format!("{} {}: {why}", p.action.as_str(), view.name),
                ),
                e => rpc(codes::CONFLICT, format!("{}'s loop: {e}", view.name)),
            })?;
        let mut batch = Batch::default();
        // The control itself is the user's; what it decided, the hub's.
        let staged = core.stage(
            &self.inner,
            &mut batch,
            &id,
            &state,
            actions,
            None,
            Some(caller.source()),
            None,
        );
        core.append_batch(&self.inner, batch).map_err(append_err)?;
        // The user's resume lets the step's prompt go, though a crash held the thread's queue
        // (as the user's own message would).
        if p.action == ControlAction::Resume {
            core.release_queue(&id);
        }
        core.after_loop_batch(&self.inner, &id, staged);
        let after = core.view(&id)?;
        Ok(match &after.r#loop {
            Some(l) => json!({
                "thread": id,
                "thread_name": after.name,
                "id": l.id,
                "running": true,
                "phase": l.phase,
                "paused": l.paused,
                "stuck": l.stuck,
                "waiting_on": l.waiting_on,
                "line": line(l),
                "state": l,
            }),
            None => json!({
                "thread": id,
                "thread_name": after.name,
                "id": state.id,
                "running": false,
                "last": after.last_loop,
            }),
        })
    }
}

/// The loop folder's files that exist, into `loop.get`'s `detail`: `files` (`goal`, `ledger`,
/// `schema`, `report`) and each round's `files` (`diffs`, `prompt`, `answer`, `log`), as
/// absolute paths.
fn add_files(detail: &mut Value) {
    let Some(folder) = detail["folder"].as_str().map(std::path::PathBuf::from) else {
        return;
    };
    let names: Vec<String> = std::fs::read_dir(&folder)
        .map(|d| {
            d.filter_map(|e| e.ok())
                .filter_map(|e| e.file_name().into_string().ok())
                .collect()
        })
        .unwrap_or_default();
    let has = |n: &str| names.iter().any(|x| x == n);
    let path = |n: &str| json!(folder.join(n));
    let mut files = serde_json::Map::new();
    for (key, name) in [
        ("goal", "goal.md"),
        ("ledger", "ledger.md"),
        ("schema", "findings.schema.json"),
        ("report", "report.md"),
    ] {
        if has(name) {
            files.insert(key.into(), path(name));
        }
    }
    detail["files"] = Value::Object(files);
    if let Some(rounds) = detail["rounds"].as_array_mut() {
        for r in rounds {
            let Some(id) = r["id"].as_str().map(str::to_string) else {
                continue;
            };
            let mut diffs: Vec<&String> = names
                .iter()
                .filter(|n| n.starts_with("diff-") && n.ends_with(&format!("-{id}.patch")))
                .collect();
            diffs.sort();
            let mut f = json!({ "diffs": diffs.iter().map(|n| path(n)).collect::<Vec<_>>() });
            for (key, name) in [
                ("prompt", format!("round-{id}.prompt.md")),
                ("answer", format!("round-{id}.answer.json")),
                ("log", format!("round-{id}.log")),
            ] {
                if has(&name) {
                    f[key] = path(&name);
                }
            }
            r["files"] = f;
        }
    }
}

/// `loop.get`'s answer for loop `id` of thread `view`, from its `loop.*` `events`.
fn loop_detail(view: &ThreadView, id: &str, events: &[Event]) -> Value {
    let mine: Vec<&Event> = events
        .iter()
        .filter(|e| match &e.body {
            EventBody::LoopStarted(p) => p.id == id,
            EventBody::LoopPhase(p) => p.id == id,
            EventBody::LoopReviewStarted(p) => p.id == id,
            EventBody::LoopFindings(p) => p.id == id,
            EventBody::LoopTriage(p) => p.id == id,
            EventBody::LoopLedger(p) => p.id == id,
            EventBody::LoopTransition(p) => p.id == id,
            EventBody::LoopControl(p) => p.id == id,
            EventBody::LoopEnded(p) => p.id == id,
            _ => false,
        })
        .collect();
    let mut started = Value::Null;
    let mut folder = Value::Null;
    let mut phases = Vec::new();
    let mut rounds: Vec<Value> = Vec::new();
    let mut ledger = Vec::new();
    let mut controls = Vec::new();
    let mut transition = Value::Null;
    let mut ended = Value::Null;
    let round = |rounds: &mut Vec<Value>, r: &str| -> usize {
        match rounds.iter().position(|v| v["id"] == r) {
            Some(i) => i,
            None => {
                rounds.push(json!({ "id": r, "runs": [], "findings": [], "triage": null }));
                rounds.len() - 1
            }
        }
    };
    for e in mine {
        match &e.body {
            EventBody::LoopStarted(p) => {
                folder = json!(p.folder);
                started = json!({
                    "at": e.ts,
                    "kind": p.kind,
                    "profile": p.profile,
                    "settings": p.settings,
                    "settings_from": p.settings_from,
                    "settings_id": p.settings_id,
                    "settings_of": p.settings_of,
                    "overrides": p.overrides,
                    "scope": p.scope,
                    "bases": p.bases,
                    "baseline_digests": p.baseline_digests,
                    "task": p.task,
                });
            }
            EventBody::LoopPhase(p) => phases.push(json!({
                "at": e.ts,
                "phase": p.phase,
                "goal": p.goal,
                "sweep": p.sweep,
                "problems": p.problems,
            })),
            EventBody::LoopReviewStarted(p) => {
                let i = round(&mut rounds, &p.round);
                rounds[i]["tier"] = json!(p.tier);
                rounds[i]["effort"] = json!(p.effort);
                if let Some(runs) = rounds[i]["runs"].as_array_mut() {
                    runs.push(json!({
                        "at": e.ts,
                        "retry": p.retry,
                        "diffs": p.diffs,
                        "prompt_digest": p.prompt_digest,
                    }));
                }
            }
            EventBody::LoopFindings(p) => {
                let i = round(&mut rounds, &p.round);
                if p.failed {
                    rounds[i]["failed"] = json!(true);
                    rounds[i]["reason"] = json!(p.reason);
                } else {
                    rounds[i]["failed"] = json!(false);
                    rounds[i]["findings"] = json!(p.findings);
                    rounds[i]["answer_path"] = json!(p.answer_path);
                    rounds[i]["usage"] = json!(p.usage);
                }
            }
            EventBody::LoopTriage(p) => {
                let i = round(&mut rounds, &p.round);
                rounds[i]["triage"] = json!({
                    "items": p.items,
                    "gates": p.gates,
                    "problems": p.problems,
                });
            }
            EventBody::LoopLedger(p) => ledger.extend(p.entries.iter().map(|e| json!(e))),
            EventBody::LoopTransition(p) => {
                transition = json!({
                    "reason": p.reason,
                    "round": p.round,
                    "verify_skipped": p.verify_skipped,
                    "at": e.ts,
                });
            }
            EventBody::LoopControl(p) => controls.push(json!({
                "at": e.ts,
                "action": p.action,
                "by": p.by,
                "why": p.why,
            })),
            EventBody::LoopEnded(p) => {
                ended = json!({
                    "at": e.ts,
                    "outcome": p.outcome,
                    "report_path": p.report_path,
                    "commit": p.commit,
                    "reason": p.reason,
                });
            }
            _ => {}
        }
    }
    let running = view.r#loop.as_ref().filter(|l| l.id == id);
    let last = view.last_loop.as_ref().filter(|l| l.id == id);
    json!({
        "thread": view.id,
        "thread_name": view.name,
        "id": id,
        "kind": running.map(|l| l.kind).or(last.map(|l| l.kind)),
        "running": running.is_some(),
        "state": running,
        "line": running.map(line),
        "last": last,
        "started": started,
        "phases": phases,
        "rounds": rounds,
        "ledger": ledger,
        "transition": transition,
        "controls": controls,
        "ended": ended,
        "folder": folder,
        "report_path": ended.get("report_path").cloned().unwrap_or(Value::Null),
        "outcome": ended
            .get("outcome")
            .cloned()
            .or_else(|| last.map(|l| json!(l.outcome)))
            .unwrap_or(Value::Null),
    })
}

// ---------------------------------------------------------------------------------------------
// Under the core lock
// ---------------------------------------------------------------------------------------------

impl Core {
    /// Folds `ev` into [`LoopsState`] (live and on replay): the saved settings, and the open
    /// setup cards (a raise by the hub marked `loop_setup` on a worker thread opens or
    /// replaces one, a resolution closes it, and a deleted thread's go with it, as the
    /// attention stack drops them).
    pub(super) fn track_loops(&mut self, ev: &Event) {
        self.loops.saved.apply(ev);
        match &ev.body {
            EventBody::AttentionRaised(p)
                if ev.source == Source::Hub
                    && ev.thread != HUB_THREAD
                    && is_loop_setup_card(&p.ext) =>
            {
                if let Some(setup) = setup_of(&p.ext) {
                    self.loops.cards.insert(
                        p.attn_id.clone(),
                        SetupCard {
                            thread: ev.thread.clone(),
                            setup,
                        },
                    );
                }
            }
            EventBody::AttentionRaised(p)
                if ev.source == Source::Hub
                    && ev.thread != HUB_THREAD
                    && is_loop_stuck_item(&p.ext) =>
            {
                self.loops
                    .stuck
                    .insert(p.attn_id.clone(), ev.thread.clone());
            }
            EventBody::AttentionResolved(p) => {
                self.loops.cards.remove(&p.attn_id);
                self.loops.stuck.remove(&p.attn_id);
            }
            EventBody::ThreadDeleted(_) => {
                self.loops.cards.retain(|_, c| c.thread != ev.thread);
                self.loops.stuck.retain(|_, t| *t != ev.thread);
                self.loops.asks.remove(&ev.thread);
                self.loops.consumed.remove(&ev.thread);
                self.loop_runs.cancel(&ev.thread, "its thread was deleted");
            }
            // The turns a loop's prompts start (`hub::loops::driver`).
            EventBody::TurnStarted(p) => {
                if let (Some((loop_id, step)), Some(turn)) =
                    (prompts::parse_marker(&p.prompt), ev.turn)
                {
                    self.loops.asks.insert(
                        ev.thread.clone(),
                        Ask {
                            loop_id,
                            step,
                            turn,
                        },
                    );
                }
            }
            _ => {}
        }
        // The turn a loop's step took: the turn its facts are stamped with.
        if let (true, Some(turn)) = (ev.kind().starts_with("loop."), ev.turn) {
            let c = self.loops.consumed.entry(ev.thread.clone()).or_default();
            *c = (*c).max(turn);
        }
        // A loop that ended (stopped, its thread stopped or parked, or at its end): its review
        // run goes with it.
        if matches!(ev.body, EventBody::LoopEnded(_)) {
            self.loop_runs.cancel(&ev.thread, "the loop ended");
        }
    }

    /// Thread `thread`'s open setup card, if it has one: its attention id and card.
    fn setup_card_of(&self, thread: &str) -> Option<(String, SetupCard)> {
        self.loops
            .cards
            .iter()
            .find(|(a, c)| c.thread == thread && self.attention.get(a).is_some())
            .map(|(a, c)| (a.clone(), c.clone()))
    }

    /// Setup card `attn_id` while it is open at `revision`: `not_found` for no card,
    /// `conflict` when it is closed or was raised again since.
    fn open_setup_card(&self, attn_id: &str, revision: u64) -> Result<SetupCard, RpcError> {
        let item = self.open_item(attn_id)?;
        let card = self
            .loops
            .cards
            .get(attn_id)
            .cloned()
            .ok_or_else(|| rpc(codes::NOT_FOUND, format!("{attn_id} is no loop setup card")))?;
        if item.revision != revision {
            return Err(rpc(
                codes::CONFLICT,
                format!(
                    "the setup card changed; look again: {attn_id} was raised again since \
                     revision {revision} (it is {attn_id}@{})",
                    item.revision
                ),
            ));
        }
        Ok(card)
    }

    /// The project `project` names for saved settings (`None`: the workspace's): a configured
    /// project, or one a thread was made for; `not_found` otherwise.
    fn loop_project(&self, hub: &Inner, project: Option<&str>) -> Result<Option<String>, RpcError> {
        let Some(p) = project.map(str::trim).filter(|p| !p.is_empty()) else {
            return Ok(None);
        };
        let known = hub.config.projects.contains_key(p)
            || self.loops.saved.projects.contains_key(p)
            || self
                .registry
                .list()
                .into_iter()
                .any(|v| v.project.as_deref() == Some(p));
        if !known {
            return Err(rpc(
                codes::NOT_FOUND,
                format!("no project {p:?}: name a configured project or one a thread works in"),
            ));
        }
        Ok(Some(p.to_string()))
    }

    /// Whether thread `id` may run a loop now (see the module docs), with the precise refusal
    /// when it may not.
    pub(super) fn loop_eligible(&self, id: &str) -> Result<(), RpcError> {
        if self.shutting_down {
            return Err(rpc(codes::CONFLICT, "hub is shutting down"));
        }
        let v = self.view(id)?;
        let name = &v.name;
        if v.is_orchestrator() {
            return Err(rpc(
                codes::BAD_REQUEST,
                format!("{name} is the orchestrator: a loop runs in a worker thread"),
            ));
        }
        if v.box_name != LOCAL_BOX {
            return Err(rpc(
                codes::UNSUPPORTED,
                format!(
                    "not supported: {name} runs on box {}, and loops run only on the hub's own \
                     machine in this version",
                    v.box_name
                ),
            ));
        }
        self.refuse_deleting(id)?;
        if v.parked {
            return Err(rpc(
                codes::CONFLICT,
                format!("{name} is parked: resume it first (`twr resume {name}`)"),
            ));
        }
        if v.stopped && self.stopped_by_someone(id) {
            return Err(rpc(
                codes::CONFLICT,
                format!("{name} is stopped: send it a message to start it again first"),
            ));
        }
        if v.worktree.is_none() {
            return Err(rpc(
                codes::UNSUPPORTED,
                format!("{name} has no git workspace to diff"),
            ));
        }
        use twr_adapters::acp::translate::ModeLevel;
        let mode = v.mode.trim();
        let why = match twr_adapters::mode_level_for(&v.runtime, mode) {
            Some(ModeLevel::Restricted) => {
                Some(format!("{name} is in {mode} mode, which can't edit"))
            }
            Some(_) => None,
            None if mode.is_empty() => Some(format!("{name}'s mode is its agent's own default")),
            None => Some(format!(
                "{name}'s mode {mode:?} is its agent's own, and twr does not know what it allows"
            )),
        };
        if let Some(why) = why {
            return Err(rpc(
                codes::CONFLICT,
                format!(
                    "{why}: a loop has to edit; change the mode first (`twr mode {name} \
                     <mode>`): the hub never widens it for a loop"
                ),
            ));
        }
        if let Some(l) = &v.r#loop {
            return Err(rpc(
                codes::CONFLICT,
                format!(
                    "{name} already runs a {} loop ({}): a thread runs one loop at a time; stop \
                     it first",
                    l.kind.as_str(),
                    line(l)
                ),
            ));
        }
        Ok(())
    }

    /// Whether thread `id`, which counts as stopped, was stopped by someone: not only by the
    /// hub's own shutdown (`thread.stopped` `ext.twr.reason`), after which a thread counts as
    /// stopped too, yet goes on (and so does its loop) as soon as it is sent something. A log
    /// that cannot be read says it was.
    fn stopped_by_someone(&self, id: &str) -> bool {
        match crate::store::latest_of_kind(self.store.conn(), id, "thread.stopped", 1) {
            Ok(evs) => !evs.first().is_some_and(|e| match &e.body {
                EventBody::ThreadStopped(p) => {
                    twr_ext(&p.ext)
                        .and_then(|t| t.get("reason"))
                        .and_then(Value::as_str)
                        == Some(super::HUB_SHUTDOWN)
                }
                _ => false,
            }),
            Err(e) => {
                tracing::warn!(thread = %id, error = %format!("{e:#}"), "cannot read how a thread stopped");
                true
            }
        }
    }

    /// `loop.request`'s decision for eligible thread `id` (module docs): start on saved
    /// settings, or raise the setup card.
    #[allow(clippy::too_many_arguments)]
    fn decide_loop(
        &mut self,
        hub: &Arc<Inner>,
        id: &str,
        kind: LoopKind,
        req: &LoopRequest,
        target: &git::Target,
        found: &git::ScopeInfo,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        let view = self.view(id)?.clone();
        let config = &hub.config.loops;
        let avail = availability(hub, &self.accounts, Some(&view));
        let saved = self
            .loops
            .saved
            .for_project(view.project.as_deref())
            .map(|(s, of)| (s.clone(), of));
        // The user's overrides apply to one run: checked now, whatever comes next.
        let overrides = req.overrides.clone();
        let base = match &saved {
            Some((s, _)) => s.settings.clone(),
            None => LoopSettings::first_card(config),
        };
        let run = overrides
            .apply(&base, kind)
            .map_err(|e| rpc(codes::BAD_REQUEST, e))?;
        let user = !caller.is_orchestrator();
        let why = match &saved {
            None => Some(SetupWhy::FirstRun),
            Some(_) if req.setup => Some(SetupWhy::Asked),
            Some((s, _)) => {
                let problems = validate(&s.settings, &avail, config);
                if problems.iter().any(|p| p.kind == ProblemKind::OtherRuntime) {
                    Some(SetupWhy::OtherRuntime)
                } else if !problems.is_empty() {
                    Some(SetupWhy::NoLongerHolds)
                } else if !user && !overrides.is_empty() {
                    Some(SetupWhy::Overrides)
                } else {
                    None
                }
            }
        };
        let from = StartFrom::of(target, found);
        let Some(why) = why else {
            // Saved settings that hold: the run's own overrides must hold too.
            let problems = validate(&run, &avail, config);
            if !problems.is_empty() {
                return Err(rpc(
                    codes::BAD_REQUEST,
                    format!(
                        "this run's settings do not hold: {}",
                        problem_words(&problems)
                    ),
                ));
            }
            let (saved, of) = saved.ok_or_else(|| internal("no saved settings"))?;
            let mut batch = Batch::default();
            // A card the thread still has (asked for before, never answered) goes: this
            // start supersedes it.
            if let Some((attn, _)) = self.setup_card_of(id) {
                batch.push(
                    id,
                    None,
                    caller.source(),
                    resolved(&attn, ResolvedBy::User, Some(CANCEL)),
                );
            }
            let (rest, model) = self.stage_start(
                hub,
                &mut batch,
                &view,
                kind,
                run,
                Origin {
                    from: SettingsSource::Saved,
                    id: saved.id.clone(),
                    of: Some(of),
                    overrides,
                },
                from,
                caller,
            )?;
            self.append_batch(hub, batch).map_err(append_err)?;
            self.after_start(hub, id, rest, model);
            return Ok(json!({ "status": "started", "loop": self.loop_json(id) }));
        };
        // The card: saved settings (or, the first time, the first card's with the overrides
        // in them) with what is missing replaced; this run's overrides beside them.
        let (shown, card_overrides) = match &saved {
            Some(_) => (base, overrides),
            None => (run, Overrides::default()),
        };
        let mut proposal = propose(&shown, &avail, config);
        if proposal.settings.sweep == SweepMode::Runs {
            proposal.settings.sweep = SweepMode::Thread;
            proposal.warnings.push(
                "sweep \"runs\" comes in a later version: the thread audits the rules itself"
                    .into(),
            );
        }
        let setup = LoopSetup {
            kind,
            first_run: saved.is_none(),
            why,
            profile: config.profile(&proposal.settings.profile),
            settings: proposal.settings,
            save_for: match &saved {
                Some((_, SettingsOf::Project)) => SaveFor::Project,
                _ => SaveFor::All,
            },
            project: view.project.clone(),
            scope: from.scope.clone(),
            scope_summary: Some(found.summary()),
            bases: from.bases,
            baseline_digests: from.baseline_digests,
            task: None,
            overrides: card_overrides,
            requested_by: if user {
                ControlBy::User
            } else {
                ControlBy::Orchestrator
            },
            warnings: proposal.warnings,
            problems: proposal
                .problems
                .iter()
                .map(|p| p.message.clone())
                .collect(),
            held_prompt: None,
        };
        let (attn_id, revision) = self.raise_setup_card(hub, &view, setup)?;
        Ok(json!({
            "status": "needs_setup",
            "attn_id": attn_id,
            "revision": revision,
            "why": why,
        }))
    }

    /// Raises `setup` as the setup card of thread `view`: again, under the same id and as a
    /// new revision, when the thread has one open. Returns its id and revision.
    fn raise_setup_card(
        &mut self,
        hub: &Inner,
        view: &ThreadView,
        setup: LoopSetup,
    ) -> Result<(String, u64), RpcError> {
        let attn_id = self.setup_card_of(&view.id).map_or_else(new_id, |(a, _)| a);
        let summary = card_text(&view.name, &setup, view);
        let ext = twr_ext_map(json!({ LOOP_SETUP_KEY: setup }));
        self.append(
            hub,
            &view.id,
            None,
            Source::Hub,
            EventBody::AttentionRaised(AttentionRaised {
                attn_id: attn_id.clone(),
                reason: AttentionReason::Review,
                summary,
                options: vec![START.into(), CANCEL.into()],
                ext,
                ..Default::default()
            }),
        )
        .map_err(append_err)?;
        let revision = self
            .attention
            .get(&attn_id)
            .map(|i| i.revision)
            .unwrap_or_default();
        Ok((attn_id, revision))
    }

    /// Stages a loop's start into `batch` (module docs): a `thread.model_changed` of the
    /// caller's when a named implementer on the thread's runtime runs another model or effort,
    /// then `loop.started`, the records [`LoopState::start`] returns and the scope turn's
    /// prompt ([`Core::stage`]). Returns what follows once the batch committed, and whether
    /// the thread's settings changed.
    #[allow(clippy::too_many_arguments)]
    fn stage_start(
        &mut self,
        hub: &Arc<Inner>,
        batch: &mut Batch,
        view: &ThreadView,
        kind: LoopKind,
        settings: LoopSettings,
        origin: Origin,
        from: StartFrom,
        caller: &Caller,
    ) -> Result<(driver::Staged, bool), RpcError> {
        let config = &hub.config.loops;
        let profile = config.profile(&settings.profile).ok_or_else(|| {
            rpc(
                codes::CONFLICT,
                format!("there is no profile {:?} any more", settings.profile),
            )
        })?;
        let mut model = false;
        if let Some(m) = settings
            .implementer
            .as_ref()
            .filter(|m| m.runtime == view.runtime)
        {
            let other_model = view.model.as_deref() != Some(m.model.as_str());
            let other_effort = m.effort.is_some() && m.effort != view.effort;
            if other_model || other_effort {
                let req = ModelRequest {
                    model: Some(&m.model),
                    effort: m.effort.as_deref(),
                    ..Default::default()
                };
                let change = self.model_change(hub, &view.id, &req, None, caller)?;
                if let Some(ev) = change.event {
                    batch.push(&view.id, None, caller.source(), ev);
                    model = true;
                }
            }
        }
        let id = new_id();
        let folder = hub
            .db_path
            .parent()
            .map(|d| d.join("loops").join(&id).to_string_lossy().into_owned());
        let started = LoopStarted {
            id,
            kind,
            profile,
            max_rounds_per_tier: config.max_rounds_per_tier,
            settings,
            settings_from: origin.from,
            settings_id: origin.id,
            settings_of: origin.of,
            overrides: origin.overrides,
            scope: from.scope,
            bases: from.bases,
            baseline_digests: from.baseline_digests,
            task: None,
            folder,
            ..Default::default()
        };
        let (state, actions) = LoopState::start(&started).map_err(start_err)?;
        let log = report::LoopLog {
            started: Some(started.clone()),
            ..Default::default()
        };
        batch.push(
            &view.id,
            None,
            Source::Hub,
            EventBody::LoopStarted(Box::new(started)),
        );
        let staged = self.stage(hub, batch, &view.id, &state, actions, None, None, Some(log));
        Ok((staged, model))
    }

    /// What follows a committed start: the thread's new model applied (a restart of its
    /// runtime once it is free, `hub::restart`), then the scope turn's prompt dispatched.
    fn after_start(&mut self, hub: &Arc<Inner>, thread: &str, staged: driver::Staged, model: bool) {
        if model {
            let mut dispatch_error = None;
            self.apply_settings(hub, thread, &mut dispatch_error);
            if let Some(e) = dispatch_error {
                tracing::warn!(%thread, error = %e, "applying a loop's implementer failed");
            }
        }
        self.after_loop_batch(hub, thread, staged);
    }

    /// The running loop of `thread` as `loop.request` answers it.
    fn loop_json(&self, thread: &str) -> Value {
        match self.registry.get(thread).and_then(|v| v.r#loop.as_ref()) {
            Some(l) => {
                let mut j = json!(l);
                j["line"] = json!(line(l));
                j["thread"] = json!(thread);
                j
            }
            None => Value::Null,
        }
    }

    /// Answers setup card `item` (`card`) with `response` (see the module docs): only the
    /// user; [`START`] starts and saves, in one batch; [`CANCEL`], or a dismissal, closes it.
    pub(super) fn answer_loop_card(
        &mut self,
        hub: &Arc<Inner>,
        item: &AttentionItem,
        card: &SetupCard,
        response: Response,
        caller: &Caller,
    ) -> Result<Event, RpcError> {
        let attn_id = item.attn_id.as_str();
        if caller.is_orchestrator() {
            return Err(rpc(
                codes::RELAY_FORBIDDEN,
                format!(
                    "{attn_id} is a loop's setup card: only the user answers it ({START} or \
                     {CANCEL}); tell the user it is waiting for them"
                ),
            ));
        }
        let decision = match response {
            Response::Dismiss => CANCEL.to_string(),
            r => super::attention::answer_of(r, item, attn_id)?.decision,
        };
        let decision = decision.trim().to_ascii_lowercase();
        let start = match decision.as_str() {
            START | "start" => true,
            CANCEL | "dismiss" => false,
            other => {
                return Err(rpc(
                    codes::BAD_REQUEST,
                    format!("{other:?} is no option of {attn_id}: {START} or {CANCEL}"),
                ))
            }
        };
        let thread = card.thread.clone();
        if !start {
            let turn = self.registry.get(&thread).and_then(|v| v.current_turn);
            return self
                .append(
                    hub,
                    &thread,
                    turn,
                    caller.source(),
                    resolved(attn_id, ResolvedBy::User, Some(CANCEL)),
                )
                .map_err(append_err);
        }
        // A paired device starts loops and saves their settings with `operate`.
        hub.require_scope(caller, "start a loop and save its settings", "operate")?;
        self.loop_eligible(&thread)?;
        let setup = &card.setup;
        if !setup.problems.is_empty() {
            return Err(rpc(
                codes::CONFLICT,
                format!(
                    "{attn_id} can't start as it is: {}; change it first (loop.setup.update)",
                    setup.problems.join("; ")
                ),
            ));
        }
        let view = self.view(&thread)?.clone();
        let config = &hub.config.loops;
        let avail = availability(hub, &self.accounts, Some(&view));
        let problems = validate(&setup.settings, &avail, config);
        if !problems.is_empty() {
            return Err(rpc(
                codes::CONFLICT,
                format!(
                    "the settings on {attn_id} no longer hold: {}; change them first \
                     (loop.setup.update)",
                    problem_words(&problems)
                ),
            ));
        }
        let run = setup
            .overrides
            .apply(&setup.settings, setup.kind)
            .map_err(|e| rpc(codes::BAD_REQUEST, e))?;
        let problems = validate(&run, &avail, config);
        if !problems.is_empty() {
            return Err(rpc(
                codes::CONFLICT,
                format!(
                    "this run's settings no longer hold: {}",
                    problem_words(&problems)
                ),
            ));
        }
        let project = match setup.save_for {
            SaveFor::All => None,
            SaveFor::Project => Some(setup.project.clone().ok_or_else(|| {
                rpc(
                    codes::BAD_REQUEST,
                    format!("{} has no project to save settings for", view.name),
                )
            })?),
        };
        let mut batch = Batch::default();
        let answered = batch.push(
            &thread,
            view.current_turn,
            caller.source(),
            resolved(attn_id, ResolvedBy::User, Some(START)),
        );
        batch.push(
            HUB_THREAD,
            None,
            caller.source(),
            EventBody::LoopSettingsSaved(Box::new(LoopSettingsSaved {
                id: new_id(),
                project,
                settings: setup.settings.clone(),
                by: SavedBy::User,
                from: SavedFrom::SetupItem,
                ..Default::default()
            })),
        );
        let (rest, model) = self.stage_start(
            hub,
            &mut batch,
            &view,
            setup.kind,
            run,
            Origin {
                from: SettingsSource::SetupItem,
                id: attn_id.to_string(),
                of: None,
                overrides: setup.overrides.clone(),
            },
            StartFrom::card(setup),
            caller,
        )?;
        let done = self.append_batch(hub, batch).map_err(append_err)?;
        self.after_start(hub, &thread, rest, model);
        done.events
            .get(answered)
            .cloned()
            .ok_or_else(|| internal("the answer was not recorded"))
    }

    /// What closes with thread `thread` when it is stopped, parked or deleted (`what`: how,
    /// for the record): its running loop ends `stopped` (`loop.ended`, which cancels its review
    /// run), and its open setup card and stuck item are resolved (`by: thread_stopped`). Recorded in the batch of the thread's own
    /// event, before it (`hub::closing`, `hub::delete`).
    pub(super) fn loop_closing(&self, thread: &str, what: &str) -> Vec<EventBody> {
        let mut out = Vec::new();
        let Some(view) = self.registry.get(thread) else {
            return out;
        };
        if let Some(l) = view.r#loop.as_ref().filter(|l| !l.ended) {
            out.push(EventBody::LoopEnded(LoopEnded {
                id: l.id.clone(),
                outcome: twr_core::loops::Outcome::Stopped,
                reason: Some(format!("{} was {what}", view.name)),
                ..Default::default()
            }));
        }
        let mut cards: Vec<&String> = self
            .loops
            .cards
            .iter()
            .filter(|(a, c)| c.thread == thread && self.attention.get(a).is_some())
            .map(|(a, _)| a)
            .chain(
                self.loops
                    .stuck
                    .iter()
                    .filter(|(a, t)| *t == thread && self.attention.get(a).is_some())
                    .map(|(a, _)| a),
            )
            .collect();
        cards.sort();
        for a in cards {
            out.push(resolved(a, ResolvedBy::ThreadStopped, None));
        }
        out
    }

    /// Whether `attn_id` is an open setup card.
    pub(super) fn loop_card(&self, attn_id: &str) -> bool {
        self.loops.cards.contains_key(attn_id)
    }

    /// Whether `attn_id` is an open stuck loop's item.
    pub(super) fn loop_stuck(&self, attn_id: &str) -> bool {
        self.loops.stuck.contains_key(attn_id)
    }

    /// Answers stuck item `item` of thread `thread` with `response` (module docs): only the
    /// user; `resume` or `stop` controls the loop in the batch of the item's resolution, a
    /// dismissal only closes it (the loop stays paused).
    pub(super) fn answer_stuck_item(
        &mut self,
        hub: &Arc<Inner>,
        item: &AttentionItem,
        thread: &str,
        response: Response,
        caller: &Caller,
    ) -> Result<Event, RpcError> {
        let attn_id = item.attn_id.as_str();
        if caller.is_orchestrator() {
            return Err(rpc(
                codes::RELAY_FORBIDDEN,
                format!(
                    "{attn_id} is a stuck loop's item: only the user answers it ({LOOP_STUCK_RESUME} \
                     or {LOOP_STUCK_STOP}); tell the user it is waiting for them"
                ),
            ));
        }
        let decision = match response {
            Response::Dismiss => "dismiss".to_string(),
            r => super::attention::answer_of(r, item, attn_id)?.decision,
        };
        let action = match decision.trim().to_ascii_lowercase().as_str() {
            LOOP_STUCK_RESUME => Some(ControlAction::Resume),
            LOOP_STUCK_STOP => Some(ControlAction::Stop),
            "dismiss" | "cancel" => None,
            other => {
                return Err(rpc(
                    codes::BAD_REQUEST,
                    format!(
                        "{other:?} is no option of {attn_id}: {LOOP_STUCK_RESUME} or \
                         {LOOP_STUCK_STOP}"
                    ),
                ))
            }
        };
        if action.is_some() {
            hub.require_scope(caller, "control a loop", "operate")?;
        }
        let mut batch = Batch::default();
        let answered = batch.push(
            thread,
            None,
            caller.source(),
            resolved(
                attn_id,
                ResolvedBy::User,
                Some(action.map_or("dismiss", |a| a.as_str())),
            ),
        );
        let mut staged = driver::Staged::default();
        if let (Some(action), Some(mut state)) = (action, self.loop_state(thread)) {
            match state.step(
                Input::Control {
                    action,
                    by: ControlBy::User,
                },
                Utc::now(),
            ) {
                Ok(actions) => {
                    staged = self.stage(
                        hub,
                        &mut batch,
                        thread,
                        &state,
                        actions,
                        None,
                        Some(caller.source()),
                        None,
                    );
                }
                Err(e) => {
                    tracing::info!(%thread, error = %e, "a stuck item's answer controls nothing now")
                }
            }
        }
        let done = self.append_batch(hub, batch).map_err(append_err)?;
        if action == Some(ControlAction::Resume) {
            self.release_queue(thread);
        }
        self.after_loop_batch(hub, thread, staged);
        done.events
            .get(answered)
            .cloned()
            .ok_or_else(|| internal("the answer was not recorded"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The card's text reads as rows a generic client shows whole.
    #[test]
    fn a_card_reads_well() {
        let config = twr_core::loops::config::LoopsConfig::default();
        let settings = LoopSettings::first_card(&config);
        let view = ThreadView {
            name: "api".into(),
            runtime: "claude-code".into(),
            model: Some("claude-opus-5-5".into()),
            effort: Some("high".into()),
            ..ThreadView::new("t1")
        };
        let setup = LoopSetup {
            first_run: true,
            profile: config.profile(&settings.profile),
            settings,
            scope: Some(Scope::Branch),
            scope_summary: Some("branch · 7 commits, 23 files".into()),
            overrides: Overrides {
                profile: Some("low".into()),
                ..Default::default()
            },
            warnings: vec!["codex isn't installed".into()],
            ..Default::default()
        };
        let text = card_text("api", &setup, &view);
        assert_eq!(
            text,
            "api: review loop setup (first run: choose its models). Start and save these \
             settings?\n\
             Profile      high · medium tier, then xhigh tier · sweep · 5 verify\n\
             Scope        branch · 7 commits, 23 files\n\
             Implementer  claude-code · claude-opus-5-5 · high (the thread's own)\n\
             Workers      claude-opus-5-5 · high\n\
             Reviewer     codex · gpt-6.1-sol · medium → xhigh\n\
             This run     profile low\n\
             Save for     all projects\n\
             Warning: codex isn't installed"
        );
    }

    #[test]
    fn kinds_and_shapes() {
        assert_eq!(parse_kind("review").unwrap(), LoopKind::Review);
        assert_eq!(
            parse_kind("implement").unwrap_err().code,
            codes::UNSUPPORTED
        );
        assert_eq!(parse_kind("x").unwrap_err().code, codes::BAD_REQUEST);
        let ok = LoopSettings::first_card(&Default::default());
        check_shape(&ok).unwrap();
        let mut runs = ok.clone();
        runs.sweep = SweepMode::Runs;
        assert_eq!(check_shape(&runs).unwrap_err().code, codes::BAD_REQUEST);
        let mut bad = ok;
        bad.reviewer.model = String::new();
        assert!(check_shape(&bad)
            .unwrap_err()
            .message
            .starts_with("reviewer"));
    }
}
