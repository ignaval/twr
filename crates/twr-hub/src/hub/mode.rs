//! Changing a thread's permission mode: `thread.set_mode` (`twr mode`, the orchestrator's
//! `set_mode` tool), recorded as `thread.mode_changed`.
//!
//! The mode is the thread's (`ThreadView::mode`: `thread.created`'s, then the latest
//! `thread.mode_changed`'s), and every runtime start uses it ([`super::spawn_config`]). A live
//! runtime started in another mode is restarted on its session once the thread is free
//! ([`super::restart`], shared with model and effort changes).
//!
//! Not restarted: the orchestrator (its mode is clamped, `hub::orch`; `thread.set_mode`
//! refuses it) and `pty` threads (they apply no mode; the change is recorded only).

use std::sync::Arc;

use serde_json::{json, Value};
use twr_core::event::*;
use twr_core::proto::{codes, RpcError};
use twr_core::reduce::ThreadView;

use super::orch::Gate;
use super::restart::applies_settings;
use super::{append_err, rpc, runtime_spec, twr_ext_map, Batch, Caller, Core, Hub, Inner};

impl Hub {
    /// `thread.set_mode {thread, mode}`: sets the thread's permission mode (see the module
    /// docs). `mode` is canonicalized and checked like `thread.new`'s
    /// (`runtime_spec::canonical_mode`, `twr_adapters::check_mode`): `bad_request` otherwise.
    /// Refused for the orchestrator (`bad_request`: its mode is clamped; `twr orchestrator
    /// use` switches it) and a retired orchestrator (`conflict`). The orchestrator's call is
    /// held to the `set_mode` gate (see `hub::orch`): applied when the user's latest message
    /// in its current turn is a command that asks for it, else recorded as a proposal for
    /// the user to confirm (`{status: "needs_confirmation", ...}`), and `relay_forbidden` when
    /// provenance fails.
    ///
    /// Returns `{thread, thread_name, mode, previous, changed, applies}`: `changed: false`
    /// (and `message`) when the thread is in that mode already. A user's call is still
    /// recorded then, as a `thread.mode_changed` with `mode == previous` (no restart): the
    /// user's explicit choice ends the authority of any earlier message of theirs to the
    /// orchestrator over that thread's mode. `applies` is `next_start`
    /// (no live runtime: the next start, a resume included, uses it), `now` (the runtime is
    /// being restarted on its session), `after_turn` (when the thread is next free: the
    /// running turn and its pending items stay the old process's) or `never` (a `pty` thread
    /// applies no mode). `stops_background_tasks: true` when the change narrows the mode
    /// while the runtime runs background tasks of its own: the restart does not wait for them,
    /// and they end with the runtime (`hub::restart`).
    pub fn set_mode(&self, query: &str, mode: &str, caller: &Caller) -> Result<Value, RpcError> {
        let mut core = self.inner.core();
        core.set_mode_locked(&self.inner, query, mode, caller, None)
    }
}

impl Core {
    /// [`Hub::set_mode`] under the core lock. `proposal`: the confirm card whose Confirm
    /// applies this change as the user's (recorded as `ext.twr.proposal`).
    pub(super) fn set_mode_locked(
        &mut self,
        hub: &Arc<Inner>,
        query: &str,
        mode: &str,
        caller: &Caller,
        proposal: Option<&str>,
    ) -> Result<Value, RpcError> {
        let (id, view, canonical) = self.mode_target(query, mode)?;
        if caller.is_orchestrator() {
            if let Gate::Propose(_) = self.mode_check(&id, &canonical)? {
                let mut out = self.propose_mode(&id, &canonical);
                if let Some(w) = self.start_warning(hub, &id) {
                    out["warning"] = json!(w);
                }
                return Ok(out);
            }
        }
        let mut batch = Batch::default();
        let staged = self.stage_mode(&mut batch, &id, &view, &canonical, caller, proposal);
        if !batch.events.is_empty() {
            self.append_batch(hub, batch).map_err(append_err)?;
        }
        Ok(self.finish_mode(hub, &staged))
    }

    /// The thread `query` names and `mode` canonicalized for it, when its mode can be set:
    /// not while the hub shuts down, not a retired thread, not the orchestrator, and a mode
    /// its runtime takes.
    pub(super) fn mode_target(
        &self,
        query: &str,
        mode: &str,
    ) -> Result<(String, ThreadView, String), RpcError> {
        self.mode_target_on(query, mode, None)
    }

    /// [`Core::mode_target`], with `mode` checked against runtime `on` instead of the
    /// thread's when given (a confirm card that also moves the thread there).
    pub(super) fn mode_target_on(
        &self,
        query: &str,
        mode: &str,
        on: Option<&str>,
    ) -> Result<(String, ThreadView, String), RpcError> {
        if self.shutting_down {
            return Err(rpc(codes::CONFLICT, "hub is shutting down"));
        }
        let id = self.resolve(query)?;
        self.refuse_retired(&id)?;
        self.refuse_deleting(&id)?;
        let view = self.view(&id)?.clone();
        if view.is_orchestrator() {
            return Err(rpc(
                codes::BAD_REQUEST,
                format!(
                    "{} is the orchestrator: its permission mode is fixed so that it only asks \
                     (dontAsk with only the twr tools on Claude Code, else at most manual); to \
                     change its runtime or model, switch it with `twr orchestrator use \
                     <runtime>`",
                    view.name
                ),
            ));
        }
        let canonical = runtime_spec::canonical_mode(mode, Some(on.unwrap_or(&view.runtime)))
            .map_err(|e| rpc(codes::BAD_REQUEST, e))?;
        Ok((id, view, canonical))
    }

    /// Adds to `batch` the change of thread `id` (`view`, checked by [`Core::mode_target`]) to
    /// `canonical` by `caller` (`ext.twr.proposal` = the confirm card `proposal` that applies
    /// it). A thread already in that mode gets a `thread.mode_changed` with `mode ==
    /// previous` only from the user: their explicit choice ends the authority of any earlier
    /// message of theirs to the orchestrator over that thread's mode (`hub::orch`,
    /// `OrchState::user_modes`). What follows the commit is [`Core::finish_mode`]'s.
    pub(super) fn stage_mode(
        &self,
        batch: &mut Batch,
        id: &str,
        view: &ThreadView,
        canonical: &str,
        caller: &Caller,
        proposal: Option<&str>,
    ) -> StagedMode {
        let by = if caller.is_orchestrator() {
            ChangedBy::Orchestrator
        } else {
            ChangedBy::User
        };
        let changed = canonical != view.mode;
        if changed || by == ChangedBy::User {
            batch.push(
                id,
                None,
                caller.source(),
                EventBody::ThreadModeChanged(ThreadModeChanged {
                    mode: canonical.to_string(),
                    previous: view.mode.clone(),
                    by,
                    ext: proposal.and_then(|card| twr_ext_map(json!({ PROPOSAL_KEY: card }))),
                    ..Default::default()
                }),
            );
        }
        StagedMode {
            thread: id.to_string(),
            name: view.name.clone(),
            runtime: view.runtime.clone(),
            mode: canonical.to_string(),
            previous: view.mode.clone(),
        }
    }

    /// What follows a mode change [`Core::stage_mode`] staged, once its batch committed: the
    /// runtime is restarted in it when idle (otherwise when the thread is next free). Returns
    /// `set_mode`'s result.
    pub(super) fn finish_mode(&mut self, hub: &Arc<Inner>, staged: &StagedMode) -> Value {
        let StagedMode {
            thread: id,
            name,
            runtime,
            mode: canonical,
            previous,
        } = staged;
        let mut out = json!({
            "thread": id,
            "thread_name": name,
            "mode": canonical,
            "previous": previous,
        });
        if canonical == previous {
            // Nothing else changes: no restart, no dispatch.
            out["changed"] = json!(false);
            out["message"] = json!(format!("{name} is already in {canonical} mode"));
            return out;
        }
        out["changed"] = json!(true);
        let mut dispatch_error = None;
        // Read before the dispatch, which begins the restart.
        let stops_tasks = self.stops_background_tasks(id);
        let applies = if applies_settings(runtime) {
            self.apply_settings(hub, id, &mut dispatch_error)
        } else {
            "never"
        };
        out["applies"] = json!(applies);
        if stops_tasks {
            out["stops_background_tasks"] = json!(true);
        }
        if let Some(e) = dispatch_error {
            out["dispatch_error"] = json!(e);
        }
        // Advisory: the runtime the change (re)starts is one its box reports missing.
        if applies != "never" {
            if let Some(w) = self.start_warning(hub, id) {
                out["warning"] = json!(w);
            }
        }
        out
    }
}

/// A mode change staged into a batch ([`Core::stage_mode`]), for [`Core::finish_mode`].
pub(super) struct StagedMode {
    thread: String,
    name: String,
    runtime: String,
    mode: String,
    previous: String,
}
