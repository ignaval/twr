//! Changing a thread's model and reasoning effort: `thread.set_model` (`twr model`, the
//! orchestrator's `set_model` tool), recorded as `thread.model_changed`.
//!
//! The model and effort are the thread's (`ThreadView::model` / `effort`: `thread.created`'s
//! `ext.twr.model` / `ext.twr.effort`, then the latest `thread.model_changed`'s), and every
//! runtime start uses them ([`super::spawn_config`]). A live runtime started with others is
//! restarted on its session once the thread is free ([`super::restart`], shared with mode
//! changes: idle now, mid-turn when the turn ends, an open between-turn item once answered).
//!
//! One convention for clearing: a `model` or `effort` that is blank or `default` (any case)
//! returns that setting to the runtime's default ([`runtime_spec::is_default`]); one left out
//! keeps the thread's. A runtime that keeps a session's setting when it resumes it without one
//! ([`twr_adapters::resume_keeps`]: the model on every runtime but the fake, the effort on
//! Codex, Grok Build and generic ACP agents) refuses that clear once the thread's next start
//! would resume a session ([`runtime_spec::clear_refusal`]): the session would go on with the
//! old value while the thread, and the restart's stamp, said the runtime's default. That is
//! once the thread has a session, and also while a runtime started with a value for that
//! setting is live but has not reported its session yet (its `thread.started` names one, which
//! the restart that applies the clear resumes). Otherwise it is fine: the next start is a new
//! session, or the live runtime runs on the default already. The effort is checked against
//! what the runtime takes ([`runtime_spec::canonical_effort`]: the allowed values are listed
//! in the error), and a local Claude Code whose `--help` does not list `--effort` refuses it
//! up front ([`runtime_spec::effort_refusal`]). The model is the runtime's own id or alias,
//! passed as given (Claude Code takes aliases such as `fable` or full ids such as
//! `claude-fable-5-1`); the runtime says whether it knows it when it starts.
//!
//! With a `runtime` (`twr model --runtime`), the thread moves to that runtime too
//! ([`super::switch`]): a new session of it starts with the thread's next turn, which carries a
//! handoff from the previous one. A model or effort left out then is the new runtime's default.
//! Both are checked against the new runtime (its efforts, a local Claude Code's `--effort`),
//! and going back to its default is never refused: its first session is a new one. Neither is
//! a later clear refused because of a live runtime the thread moved away from (it is replaced,
//! not resumed); one of the new runtime that has not reported its session yet counts as above.
//! A move back to a runtime the thread left a session of earlier resumes that session instead
//! (not for `acp`): a model or effort left out is then the one the thread had when it left it,
//! and a `default` that runtime keeps a session's value of makes the move start a new session
//! (with the full handoff) rather than being refused. (The session is resumed with the
//! thread's settings, as any resume is: a value other than the session's applies.) Until the
//! resumed session starts, a clear of a setting the runtime keeps is refused as for any
//! session the thread has.
//!
//! With an `account` (`twr model --account`, or `--runtime <runtime>:<account>`), the thread
//! moves to that account (`hub::switch`, `hub::thread_account`), on its runtime or with a new
//! one: a new session there, with the handoff, like a runtime move. A move between accounts of
//! one runtime keeps the thread's model and effort unless given, also when it moves back to
//! an account the thread left a session of: that session resumes with the thread's settings
//! now, not the ones it had when the thread left it, unless the thread's default for a
//! setting the runtime keeps on resume would leave the session on its own value (as for a
//! `default` given above): then the move starts a new session there. A move back resumes
//! only a session the thread left on that runtime and that same account, in the directory
//! the account has now and through the path its runtime was given then
//! ([`Core::departure`]): never another account's session.
//!
//! Unlike a mode change, the orchestrator's change of a model or effort is applied directly,
//! recorded with `by: orchestrator`: choosing a model is no permission answer (like
//! `thread.new`'s `model`), so it needs no confirm card. A move to another runtime is
//! different: runtimes differ in what one permission mode lets through (Codex asks for the
//! edits `acceptEdits` lets Claude Code make on its own; grok's `default` runs some commands it
//! deems safe unasked), so a move can widen what the thread does without asking. The
//! orchestrator's move goes through the `set_mode` gate's provenance (`hub::orch`: a user
//! turn whose message is the user's latest, a thread that existed when it arrived, once per
//! thread per turn; `relay_forbidden` otherwise) and onto the confirm card
//! (`{status: "needs_confirmation"}`; Confirm makes the move as the user's), unless the
//! thread's mode is a restricted one (plan, read-only, dontAsk) on the new runtime
//! ([`twr_adapters::mode_level_for`]), which no runtime lets do more than read: that move is
//! applied directly; so is its move between accounts of one runtime (within the pins: the
//! thread may do the same there). The orchestrator may only choose an agent runtime or a
//! configured preset, never a command line. Refused for the orchestrator itself (`twr
//! orchestrator use <runtime> --model M` switches it) and, without a new runtime, `pty` threads
//! (their command line is their own).

use std::sync::Arc;

use serde_json::{json, Value};
use twr_core::event::*;
use twr_core::proto::{codes, RpcError};

use super::switch::Switch;
use super::withdraw::OrchCall;
use super::{
    append_err, box_agents, internal, rpc, runtime_spec, twr_ext_map, Batch, Caller, Core, Hub,
    Inner,
};

/// What `thread.set_model` asks for (see [`Hub::set_thread_model`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct ModelRequest<'a> {
    /// The runtime to move to (`<runtime>:<account>` picks the account too); `None` or blank:
    /// the thread's own.
    pub runtime: Option<&'a str>,
    /// The account to move to (a name, a glob or a comma-separated list, as `thread.new`
    /// takes it).
    pub account: Option<&'a str>,
    /// Move to `account` although it is outside the thread's or its project's pin (the user
    /// only).
    pub outside_pin: bool,
    /// The model (`default`: the runtime's).
    pub model: Option<&'a str>,
    /// The reasoning effort (`default`: the runtime's).
    pub effort: Option<&'a str>,
}

/// A checked `thread.set_model` ([`Core::model_change`]).
pub(super) struct ModelChange {
    /// The thread.
    pub(super) id: String,
    /// The result so far.
    pub(super) out: Value,
    /// The `thread.model_changed` to record; `None`: nothing changes.
    pub(super) event: Option<EventBody>,
    /// The runtime or account the thread moves to, if another one.
    pub(super) switch: Option<Switch>,
}

impl Hub {
    /// `thread.set_model {thread, model?, effort?}`: sets the thread's model and/or reasoning
    /// effort (see the module docs; at least one must be given, `bad_request` otherwise).
    /// Refused while the hub shuts down (`conflict`), for a retired orchestrator (`conflict`),
    /// the orchestrator (`bad_request`), a `pty` thread (`bad_request`), a model over 128
    /// bytes or with control characters, an effort the runtime does not take
    /// (`bad_request`, listing the ones it does), and a clear back to the runtime's default
    /// that a resumed session would not take (`bad_request`, see the module docs).
    ///
    /// Returns `{thread, thread_name, model, effort, previous_model, previous_effort,
    /// changed, applies?, message?, dispatch_error?}` (`model` / `effort` null: the runtime's
    /// default). `changed: false` (and `message`) when both are the thread's already: nothing
    /// is recorded and nothing restarts. Otherwise a `thread.model_changed` is appended and
    /// `applies` says when the runtime takes it: `next_start` (no live runtime), `now` (it is
    /// being restarted on its session) or `after_turn` (when the thread is next free);
    /// `stops_background_tasks: true` when a move to another runtime narrows what the thread
    /// may do without asking while the runtime runs background tasks of its own (they end with
    /// it, `hub::restart`).
    pub fn set_model(
        &self,
        query: &str,
        model: Option<&str>,
        effort: Option<&str>,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        self.set_runtime_model(query, None, model, effort, caller)
    }

    /// `thread.set_model {thread, runtime?, model?, effort?}`: [`Hub::set_model`], also moving
    /// the thread to `runtime` when it is another one ([`super::switch`]; blank: none). Also
    /// refused: a runtime the thread's box lacks, a `pty` one, one whose permission modes do not
    /// include the thread's (`bad_request`), an attached thread (`conflict`), and for the
    /// orchestrator anything but an agent runtime or a configured preset (`bad_request`).
    ///
    /// The result also has `runtime`, `previous_runtime`, `runtime_changed` and `resumes` (the
    /// session of the new runtime the move resumes, else null); with a move, `message` says
    /// that the new runtime's session starts (or the earlier one resumes) with the next turn,
    /// with the handoff. (The project of a thread its log names none for is not looked up here,
    /// as [`Hub::set_thread_model`] does: what an earlier lookup found applies.)
    pub fn set_runtime_model(
        &self,
        query: &str,
        runtime: Option<&str>,
        model: Option<&str>,
        effort: Option<&str>,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        let req = ModelRequest {
            runtime,
            model,
            effort,
            ..Default::default()
        };
        let mut core = self.inner.core();
        core.set_model_locked(&self.inner, query, &req, caller, None)
    }

    /// `thread.set_model {thread, runtime?, account?, outside_pin?, model?, effort?}`:
    /// [`Hub::set_runtime_model`], also moving the thread to account `account` (or the account
    /// part of `runtime`, `<runtime>:<account>`) when it is another one, on the same runtime or
    /// with it (`hub::switch`, `hub::thread_account`). The account is checked like a new
    /// thread's (one the runtime does not have, a removed or logged-out one, any named account
    /// on a remote box: refused), and must match the thread's own pin and its project's pin
    /// and exclusions: outside them, refused for the orchestrator, and for the user unless
    /// `outside_pin` (recorded as `ext.twr.account_reason: "override"`). A move between
    /// accounts of one runtime keeps the thread's model and effort unless given.
    ///
    /// A move (a runtime or an account given) of a thread made for a repository path whose
    /// log names no project for its pins (made before they were recorded, or before its
    /// project was configured) looks the repository's configured project up first, before the
    /// core is locked ([`Hub::look_up_pins_project`]): its pins apply. The move is then made
    /// to the thread `query` named before the lookup, whatever it names once the core is
    /// locked again (another thread renamed meanwhile); `not_found` if that thread is gone.
    ///
    /// The result also has `account`, `previous_account` (names: the native account's current
    /// name for the native one) and `account_changed`.
    ///
    /// The orchestrator's call is bound to the orchestrator as it is when the call begins
    /// ([`Hub::set_thread_model_bound`]).
    pub async fn set_thread_model(
        &self,
        query: &str,
        req: ModelRequest<'_>,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        self.set_thread_model_bound(query, req, caller, None).await
    }

    /// [`Hub::set_thread_model`] of the orchestrator's call admitted as `bound`
    /// ([`Hub::orchestrator_acts`]: the server's, as the request came in; `None`: the
    /// orchestrator as it is now). A move that looks the thread's project up first is checked
    /// again once the core is locked after the lookup, as admission checked it: refused
    /// (`unauthorized`) when that orchestrator was replaced or restarted meanwhile, and
    /// (`relay_forbidden`) when the user took its turn's message back. The `set_mode` gate of
    /// its runtime move is that of the turn the call came in: refused when that turn ended
    /// (another one may have begun meanwhile; its message is not this call's).
    pub async fn set_thread_model_bound(
        &self,
        query: &str,
        req: ModelRequest<'_>,
        caller: &Caller,
        bound: Option<&OrchCall>,
    ) -> Result<Value, RpcError> {
        let moves = req.account.is_some() || req.runtime.is_some_and(|r| !r.trim().is_empty());
        let (id, bound) = {
            let mut core = self.inner.core();
            let bound = match caller.is_orchestrator() {
                true => bound.cloned().or_else(|| core.orchestrator_call()),
                false => None,
            };
            if !moves {
                return core.set_model_locked(&self.inner, query, &req, caller, bound.as_ref());
            }
            (core.resolve(query)?, bound)
        };
        self.look_up_pins_project(&id).await;
        let mut core = self.inner.core();
        if let Some(b) = &bound {
            core.orchestrator_admitted("thread.set_model", Some(b))?;
        }
        // The thread looked up, by its id (which resolves to it alone).
        core.view(&id)?;
        core.set_model_locked(&self.inner, &id, &req, caller, bound.as_ref())
    }
}

impl Core {
    /// [`Hub::set_runtime_model`] under the core lock. The orchestrator's move to another
    /// runtime goes on the confirm card unless the thread's mode is restricted there (module
    /// docs), its gate that of the turn of `bound` (the call as admitted, when known:
    /// [`Core::move_check`]).
    fn set_model_locked(
        &mut self,
        hub: &Arc<Inner>,
        query: &str,
        req: &ModelRequest<'_>,
        caller: &Caller,
        bound: Option<&OrchCall>,
    ) -> Result<Value, RpcError> {
        let change = self.model_change(hub, query, req, None, caller)?;
        let ModelChange {
            id,
            mut out,
            event,
            switch,
        } = change;
        let Some(event) = event else {
            return Ok(out);
        };
        // Advisory: the thread's box reports the CLI the change starts missing
        // (`hub::box_agents`): the new runtime's on a move, else the thread's own (a new
        // model or effort restarts it). The change goes ahead either way.
        let warning = self.registry.get(&id).and_then(|view| {
            let target = switch
                .as_ref()
                .map_or(view.runtime.as_str(), |new| new.spec.runtime.as_str());
            box_agents::agent_warning(hub, &view.box_name, target)
        });
        // A move between accounts of one runtime keeps what the thread may do: no card.
        let runtime_move = switch
            .as_ref()
            .filter(|s| s.runtime_changed && caller.is_orchestrator());
        if let Some(new) = runtime_move {
            if self.move_needs_card(&id, &new.spec.runtime) {
                self.move_check(&id, bound)?;
                let spec = req.runtime.map(str::trim).unwrap_or_default();
                // The card's move names the account it was checked with, as `<runtime>:<account>`.
                let spec = match req.account.map(str::trim).filter(|a| !a.is_empty()) {
                    Some(a) if new.spec.account.is_none() => format!("{spec}:{a}"),
                    _ => spec.to_string(),
                };
                let mut out = self.propose_move(&id, &spec, req.model, req.effort);
                if let Some(w) = warning {
                    out["warning"] = json!(w);
                }
                return Ok(out);
            }
        }
        if let Some(w) = warning {
            out["warning"] = json!(w);
        }
        self.append(hub, &id, None, caller.source(), event)
            .map_err(append_err)?;
        let mut dispatch_error = None;
        // Read before the dispatch, which begins the restart.
        let stops_tasks = self.stops_background_tasks(&id);
        let applies = self.apply_settings(hub, &id, &mut dispatch_error);
        out["applies"] = json!(applies);
        if stops_tasks {
            out["stops_background_tasks"] = json!(true);
        }
        if let Some(e) = dispatch_error {
            out["dispatch_error"] = json!(e);
        }
        Ok(out)
    }

    /// Whether the orchestrator's move of thread `id` to runtime `runtime` goes on the confirm
    /// card: unless the thread's mode, as `runtime` takes it, is a restricted one there
    /// ([`twr_adapters::mode_level_for`]; unknown counts as not restricted).
    fn move_needs_card(&self, id: &str, runtime: &str) -> bool {
        use twr_adapters::acp::translate::ModeLevel;
        let Some(view) = self.registry.get(id) else {
            return true;
        };
        let Ok(mode) = runtime_spec::canonical_mode(&view.mode, Some(runtime)) else {
            return true;
        };
        twr_adapters::mode_level_for(runtime, &mode) != Some(ModeLevel::Restricted)
    }

    /// Stages the move a confirm card holds into `batch`, as the user's (`caller`), checked
    /// again like the user's own `thread.set_model`, with the thread in `mode` when an earlier
    /// action of the card sets one (the new runtime must take the mode the thread will be in):
    /// the thread to apply its settings to once the batch committed
    /// ([`Core::apply_settings`]) and the runtime it moves to, or why it no longer holds.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn stage_move(
        &mut self,
        hub: &Arc<Inner>,
        batch: &mut Batch,
        thread: &str,
        runtime: &str,
        model: Option<&str>,
        effort: Option<&str>,
        mode: Option<&str>,
        caller: &Caller,
        card: &str,
    ) -> Result<(String, String), RpcError> {
        let req = ModelRequest {
            runtime: Some(runtime),
            model,
            effort,
            ..Default::default()
        };
        let change = self.model_change(hub, thread, &req, mode, caller)?;
        let Some(target) = change.switch.as_ref().map(|s| s.spec.runtime.clone()) else {
            return Err(rpc(
                codes::CONFLICT,
                format!("{} runs on {runtime} already", self.thread_name(&change.id)),
            ));
        };
        let Some(mut event) = change.event else {
            return Err(rpc(codes::CONFLICT, "nothing to change"));
        };
        if let EventBody::ThreadModelChanged(p) = &mut event {
            let ext = p.ext.get_or_insert_with(Default::default);
            if let Some(twr) = ext
                .entry("twr".to_string())
                .or_insert_with(|| json!({}))
                .as_object_mut()
            {
                twr.insert(PROPOSAL_KEY.into(), json!(card));
            }
        }
        batch.push(&change.id, None, caller.source(), event);
        Ok((change.id, target))
    }

    /// Checks `thread.set_model` for `caller` (see [`Hub::set_runtime_model`]): the result
    /// so far and the `thread.model_changed` to record, if anything changes. `mode`: the mode
    /// the thread is in when the change applies, if not its recorded one (a confirm card that
    /// also sets it).
    pub(super) fn model_change(
        &mut self,
        hub: &Arc<Inner>,
        query: &str,
        req: &ModelRequest<'_>,
        mode: Option<&str>,
        caller: &Caller,
    ) -> Result<ModelChange, RpcError> {
        let ModelRequest {
            runtime,
            account,
            outside_pin,
            model,
            effort,
        } = *req;
        if self.shutting_down {
            return Err(rpc(codes::CONFLICT, "hub is shutting down"));
        }
        let id = self.resolve(query)?;
        self.refuse_retired(&id)?;
        self.refuse_deleting(&id)?;
        let mut view = self.view(&id)?.clone();
        if let Some(m) = mode {
            view.mode = m.to_string();
        }
        if view.is_orchestrator() {
            return Err(rpc(
                codes::BAD_REQUEST,
                format!(
                    "{} is the orchestrator: to change its runtime or model, switch it with \
                     `twr orchestrator use <runtime> --model <model>`",
                    view.name
                ),
            ));
        }
        let runtime = runtime.map(str::trim).filter(|r| !r.is_empty());
        let account = account.map(str::trim);
        let switch = if runtime.is_some() || account.is_some() {
            self.runtime_switch(hub, &view, runtime, account, outside_pin, caller)?
        } else {
            None
        };
        // A move between accounts of one runtime keeps the model and effort (left out).
        let runtime_changed = switch.as_ref().is_some_and(|s| s.runtime_changed);
        let target = switch
            .as_ref()
            .map_or(view.runtime.clone(), |s| s.spec.runtime.clone());
        // The account it runs on after this (unchanged: its own).
        let account_move = switch.as_ref().and_then(|s| s.account.as_ref());
        let target_account = account_move.map_or(view.account.clone(), |a| a.account.clone());
        if view.runtime == twr_adapters::pty::NAME && switch.is_none() {
            return Err(rpc(
                codes::BAD_REQUEST,
                format!(
                    "{} is a pty thread: it runs its own command line, so give the model or \
                     effort there (`--runtime 'pty:<command>'`)",
                    view.name
                ),
            ));
        }
        // A move back to a runtime and account the thread left a session of resumes that
        // session (`hub::switch`): found before the settings are checked, which then start from
        // the session's on a move to another runtime (or agent). Only that account's own
        // session, in the directory the account has now and given it by the path its runtime
        // had then ([`Core::departure`]). Unless the move's settings would clear one the
        // runtime keeps on resume (below): the session would go on with its own, so the move
        // starts a new one.
        let (target_dir, target_env) = match account_move {
            Some(a) => (a.dir.clone(), a.env_dir.clone()),
            None => (self.account_dir(&view), self.account_env_dir(&view)),
        };
        let mut departure = match &switch {
            Some(s) if s.spec.acp.is_none() && s.spec.runtime != twr_adapters::pty::NAME => self
                .departure(
                    hub,
                    &id,
                    &s.spec.runtime,
                    target_account.as_deref(),
                    target_dir.as_deref(),
                    target_env.as_deref(),
                )
                .map_err(internal)?,
            _ => None,
        };
        // Its own runtime (or account) named alone: nothing changes (reported below).
        let named = runtime.is_some() || account.is_some();
        if model.is_none() && effort.is_none() && switch.is_none() && !named {
            return Err(rpc(
                codes::BAD_REQUEST,
                "give a model, an effort or both, or another runtime (`default` returns a model \
                 or effort to the runtime's own)",
            ));
        }
        // On another runtime (or agent), one left out is the new runtime's default, or the
        // session's when the move resumes one. A move between accounts of one runtime keeps
        // the thread's, also when it resumes the session the thread left there: that session
        // runs on with them (a resume is given the thread's settings).
        let settings = |restores: Option<&super::switch::Departure>| {
            let restores = restores.filter(|_| runtime_changed);
            let new_model = match model {
                None if restores.is_some() => restores.and_then(|d| d.model.clone()),
                None if runtime_changed => None,
                None => view.model.clone(),
                Some(m) if runtime_spec::is_default(m) => None,
                Some(m) => {
                    let m = m.trim();
                    if let Some(e) = runtime_spec::model_error(m) {
                        return Err(rpc(codes::BAD_REQUEST, e));
                    }
                    Some(m.to_string())
                }
            };
            let new_effort = match effort {
                None if restores.is_some() => restores.and_then(|d| d.effort.clone()),
                None if runtime_changed => None,
                None => view.effort.clone(),
                Some(e) => runtime_spec::canonical_effort(e, &target)
                    .map_err(|e| rpc(codes::BAD_REQUEST, e))?,
            };
            Ok((new_model, new_effort))
        };
        let (mut new_model, mut new_effort) = settings(departure.as_ref())?;
        // A resumed session given the runtime's default for a setting the runtime keeps on
        // resume would go on with its own (`runtime_spec::clear_refusal`): a `default` asked
        // for, or, between accounts of one runtime, the thread's default kept while the
        // session had a value. The move starts a new session there instead, with the full
        // handoff (and, on another runtime, that runtime's defaults for what was left out).
        if let Some(d) = &departure {
            let cleared = [
                ("model", &d.model, &new_model),
                ("effort", &d.effort, &new_effort),
            ];
            if cleared.into_iter().any(|(what, from, to)| {
                runtime_spec::clear_refusal(&target, &view.name, true, what, from, to).is_some()
            }) {
                departure = None;
                (new_model, new_effort) = settings(None)?;
            }
        }
        // A runtime that keeps a session's model or effort on resume cannot be cleared back to
        // its default once the thread's next start resumes a session: the session would go on
        // with the old value while the thread said the default. A session the thread has, or
        // one a live runtime started with a value (or an unknown one) may still report: the
        // settings restart that applies the clear would resume it. A thread moving to another
        // runtime (or account) by this call resumes nothing: its next start is a new session
        // there. Nor does a live runtime the thread moved away from by an earlier call
        // count (`RunSettings::is_runtime_of`): it is replaced, never resumed, and its
        // session is not the thread's (`ThreadView::handoff_pending`); a live runtime of the
        // new runtime that has not reported its session yet does.
        let fresh = switch.is_some();
        let started = self
            .rt
            .get(&id)
            .and_then(|r| r.runtime_settings.as_ref())
            .filter(|s| s.is_runtime_of(&view))
            .map(|s| (s.model.is_some(), s.effort.is_some()));
        let has_session = view.session_id.is_some();
        for (what, from, to, started_with_value) in [
            (
                "model",
                &view.model,
                &new_model,
                started.is_some_and(|s| s.0),
            ),
            (
                "effort",
                &view.effort,
                &new_effort,
                started.is_some_and(|s| s.1),
            ),
        ] {
            let resumes = !fresh && (has_session || started_with_value);
            if let Some(why) =
                runtime_spec::clear_refusal(&target, &view.name, resumes, what, from, to)
            {
                return Err(rpc(codes::BAD_REQUEST, why));
            }
        }
        // Only a new effort (or one for a new runtime) is checked against the binary: one the
        // thread has runs already.
        if new_effort.is_some() && (new_effort != view.effort || runtime_changed) {
            let local = hub.host(&view.box_name).is_some_and(|h| !h.is_remote());
            // Resolved as the thread's next spawn resolves it (its environment and directory).
            let spawn = self.spawn_config(hub, &view);
            if let Some(why) = runtime_spec::effort_refusal(&target, local, &spawn.env, &spawn.cwd)
            {
                return Err(rpc(codes::BAD_REQUEST, why));
            }
        }
        // Accounts by name (the native account's current one), as `thread_json` has them.
        let account_name = |rt: &str, a: &Option<String>| {
            a.clone().unwrap_or_else(|| self.accounts.native_name(rt))
        };
        let label = |rt: &str, a: &Option<String>| format!("{rt}:{}", account_name(rt, a));
        let mut out = json!({
            "thread": id,
            "thread_name": view.name,
            "model": new_model,
            "effort": new_effort,
            "previous_model": view.model,
            "previous_effort": view.effort,
            "runtime": target,
            "previous_runtime": view.runtime,
            "runtime_changed": runtime_changed,
            "resumes": departure.as_ref().map(|d| &d.session),
            "account": account_name(&target, &target_account),
            "previous_account": account_name(&view.runtime, &view.account),
            "account_changed": account_move.is_some(),
        });
        if switch.is_none() && new_model == view.model && new_effort == view.effort {
            // Nothing changes: nothing is recorded, nothing restarts.
            out["changed"] = json!(false);
            let on = match (account.is_some(), runtime.is_some()) {
                (true, _) => format!("on {} ", label(&view.runtime, &view.account)),
                (false, true) => format!("on {} ", view.runtime),
                (false, false) => String::new(),
            };
            out["message"] = json!(format!(
                "{} already runs {on}with {}",
                view.name,
                describe(&new_model, &new_effort)
            ));
            return Ok(ModelChange {
                id,
                out,
                event: None,
                switch,
            });
        }
        let by = if caller.is_orchestrator() {
            ChangedBy::Orchestrator
        } else {
            ChangedBy::User
        };
        // What the new runtime needs (as `thread.created` records it: a move between accounts
        // of one runtime records no agent, keeping the thread's), the new account's directory
        // (resolved, and as its runtimes get it) and why it was chosen, the session it replaces
        // (one its runtime confirmed, never a pending move's) and the one it resumes.
        let ext = switch.as_ref().and_then(|s| {
            let mut twr = serde_json::Map::new();
            if s.runtime_changed {
                s.spec.record(&mut twr);
            }
            if let Some(a) = &s.account {
                if let Some(d) = &a.dir {
                    twr.insert(ACCOUNT_DIR_KEY.into(), json!(d.to_string_lossy()));
                }
                // The path its runtimes get, kept while it leads there (as `thread.created`
                // records it: `Core::account_env_dir`).
                if let Some(d) = &a.env_dir {
                    twr.insert(ACCOUNT_ENV_DIR_KEY.into(), json!(d.to_string_lossy()));
                }
                // (Absent: the native account nothing else chose.)
                if !a.reason.is_empty() && a.reason != super::thread_account::REASON_NATIVE {
                    twr.insert(ACCOUNT_REASON_KEY.into(), json!(a.reason));
                }
            }
            twr.insert("previous_session".into(), json!(view.confirmed_session()));
            if let Some(d) = &departure {
                twr.insert(RESUME_SESSION_KEY.into(), json!(d.session));
            }
            twr_ext_map(Value::Object(twr))
        });
        let event = EventBody::ThreadModelChanged(ThreadModelChanged {
            model: new_model,
            effort: new_effort,
            previous_model: view.model.clone(),
            previous_effort: view.effort.clone(),
            runtime: switch.as_ref().map(|s| s.spec.runtime.clone()),
            previous_runtime: switch.as_ref().map(|_| view.runtime.clone()),
            account_changed: account_move.is_some(),
            account: account_move.and_then(|a| a.account.clone()),
            previous_account: account_move.and(view.account.clone()),
            by,
            ext,
            ..Default::default()
        });
        out["changed"] = json!(true);
        if switch.is_some() {
            // A move of the account names both ends with their accounts.
            let (from, to) = match account_move {
                Some(_) => (
                    label(&view.runtime, &view.account),
                    label(&target, &target_account),
                ),
                None => (view.runtime.clone(), target.clone()),
            };
            out["message"] = json!(match &departure {
                Some(d) => format!(
                    "{} moves from {from} back to {to}: its earlier {to} session ({}) resumes \
                     with its next turn, told what the thread did since it left it",
                    view.name, d.session
                ),
                None if account_move.is_some() => format!(
                    "{} moves to {to}; its next turn starts a new session there with a handoff",
                    view.name
                ),
                None => format!(
                    "{} moves from {from} to {to}: a new {to} session starts with its next \
                     turn, given a handoff of what the previous session did (its task, reports, \
                     recent turns, worktree and branch)",
                    view.name
                ),
            });
        }
        Ok(ModelChange {
            id,
            out,
            event: Some(event),
            switch,
        })
    }
}

/// `model M and effort E` for a message, each `the runtime's default ...` when unset.
fn describe(model: &Option<String>, effort: &Option<String>) -> String {
    let model = model
        .as_deref()
        .map_or("the runtime's default model".to_string(), |m| {
            format!("model {m}")
        });
    let effort = effort
        .as_deref()
        .map_or("its default effort".to_string(), |e| format!("effort {e}"));
    format!("{model} and {effort}")
}
