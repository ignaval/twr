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
//! applied directly. The orchestrator may only choose an agent runtime or a configured preset,
//! never a command line. Refused for the orchestrator itself (`twr orchestrator use <runtime>
//! --model M` switches it) and, without a new runtime, `pty` threads (their command line is
//! their own).

use std::sync::Arc;

use serde_json::{json, Value};
use twr_core::event::*;
use twr_core::proto::{codes, RpcError};

use super::runtime_spec::RuntimeSpec;
use super::{
    append_err, box_agents, rpc, runtime_spec, twr_ext_map, Batch, Caller, Core, Hub, Inner,
};

/// A checked `thread.set_model` ([`Core::model_change`]).
pub(super) struct ModelChange {
    /// The thread.
    pub(super) id: String,
    /// The result so far.
    pub(super) out: Value,
    /// The `thread.model_changed` to record; `None`: nothing changes.
    pub(super) event: Option<EventBody>,
    /// The runtime the thread moves to, if another one.
    pub(super) switch: Option<RuntimeSpec>,
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
    /// The result also has `runtime`, `previous_runtime` and `runtime_changed`; with a move,
    /// `message` says that the new runtime's session starts with the next turn, with the
    /// handoff.
    pub fn set_runtime_model(
        &self,
        query: &str,
        runtime: Option<&str>,
        model: Option<&str>,
        effort: Option<&str>,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        let mut core = self.inner.core();
        core.set_model_locked(&self.inner, query, runtime, model, effort, caller)
    }
}

impl Core {
    /// [`Hub::set_runtime_model`] under the core lock. The orchestrator's move to another
    /// runtime goes on the confirm card unless the thread's mode is restricted there (module
    /// docs).
    fn set_model_locked(
        &mut self,
        hub: &Arc<Inner>,
        query: &str,
        runtime: Option<&str>,
        model: Option<&str>,
        effort: Option<&str>,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        let change = self.model_change(hub, query, runtime, model, effort, None, caller)?;
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
                .map_or(view.runtime.as_str(), |new| new.runtime.as_str());
            box_agents::agent_warning(hub, &view.box_name, target)
        });
        if let Some(new) = switch.as_ref().filter(|_| caller.is_orchestrator()) {
            if self.move_needs_card(&id, &new.runtime) {
                self.move_check(&id)?;
                let spec = runtime.map(str::trim).unwrap_or_default();
                let mut out = self.propose_move(&id, spec, model, effort);
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
        let change = self.model_change(hub, thread, Some(runtime), model, effort, mode, caller)?;
        let Some(target) = change.switch.as_ref().map(|s| s.runtime.clone()) else {
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
    #[allow(clippy::too_many_arguments)]
    pub(super) fn model_change(
        &mut self,
        hub: &Arc<Inner>,
        query: &str,
        runtime: Option<&str>,
        model: Option<&str>,
        effort: Option<&str>,
        mode: Option<&str>,
        caller: &Caller,
    ) -> Result<ModelChange, RpcError> {
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
        let switch = match runtime.map(str::trim).filter(|r| !r.is_empty()) {
            Some(r) => self.runtime_switch(hub, &view, r, caller)?,
            None => None,
        };
        let target = switch
            .as_ref()
            .map_or(view.runtime.clone(), |s| s.runtime.clone());
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
        // Its own runtime named alone: nothing changes (reported below).
        let named = runtime.is_some_and(|r| !r.trim().is_empty());
        if model.is_none() && effort.is_none() && switch.is_none() && !named {
            return Err(rpc(
                codes::BAD_REQUEST,
                "give a model, an effort or both, or another runtime (`default` returns a model \
                 or effort to the runtime's own)",
            ));
        }
        // On another runtime, one left out is the new runtime's default.
        let new_model = match model {
            None if switch.is_some() => None,
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
            None if switch.is_some() => None,
            None => view.effort.clone(),
            Some(e) => runtime_spec::canonical_effort(e, &target)
                .map_err(|e| rpc(codes::BAD_REQUEST, e))?,
        };
        // A runtime that keeps a session's model or effort on resume cannot be cleared back to
        // its default once the thread's next start resumes a session: the session would go on
        // with the old value while the thread said the default. A session the thread has, or
        // one a live runtime started with a value (or an unknown one) may still report: the
        // settings restart that applies the clear would resume it. A thread moving to another
        // runtime by this call resumes nothing: its next start is a new session of the new
        // runtime. Nor does a live runtime the thread moved away from by an earlier call
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
        if new_effort.is_some() && (new_effort != view.effort || switch.is_some()) {
            let local = hub.host(&view.box_name).is_some_and(|h| !h.is_remote());
            // Resolved as the thread's next spawn resolves it (its environment and directory).
            let spawn = super::spawn_config(hub, &view);
            if let Some(why) = runtime_spec::effort_refusal(&target, local, &spawn.env, &spawn.cwd)
            {
                return Err(rpc(codes::BAD_REQUEST, why));
            }
        }
        let mut out = json!({
            "thread": id,
            "thread_name": view.name,
            "model": new_model,
            "effort": new_effort,
            "previous_model": view.model,
            "previous_effort": view.effort,
            "runtime": target,
            "previous_runtime": view.runtime,
            "runtime_changed": switch.is_some(),
        });
        if switch.is_none() && new_model == view.model && new_effort == view.effort {
            // Nothing changes: nothing is recorded, nothing restarts.
            out["changed"] = json!(false);
            out["message"] = json!(format!(
                "{} already runs {}with {}",
                view.name,
                if named {
                    format!("on {} ", view.runtime)
                } else {
                    String::new()
                },
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
        // What the new runtime needs (as `thread.created` records it) and the session it
        // replaces.
        let ext = switch.as_ref().and_then(|s| {
            let mut twr = serde_json::Map::new();
            s.record(&mut twr);
            twr.insert("previous_session".into(), json!(view.session_id));
            twr_ext_map(Value::Object(twr))
        });
        let event = EventBody::ThreadModelChanged(ThreadModelChanged {
            model: new_model,
            effort: new_effort,
            previous_model: view.model.clone(),
            previous_effort: view.effort.clone(),
            runtime: switch.as_ref().map(|s| s.runtime.clone()),
            previous_runtime: switch.as_ref().map(|_| view.runtime.clone()),
            by,
            ext,
            ..Default::default()
        });
        out["changed"] = json!(true);
        if switch.is_some() {
            out["message"] = json!(format!(
                "{} moves from {} to {target}: a new {target} session starts with its next \
                 turn, given a handoff of what the previous session did (its task, reports, \
                 recent turns, worktree and branch)",
                view.name, view.runtime
            ));
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
