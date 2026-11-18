//! The orchestrator's access level as hub state: `[orchestrator] access` when the hub starts,
//! then each change (`orchestrator.access`, [`Hub::orchestrator_access`]; `config.set` of
//! `orchestrator.access`, by the user or by their Confirm on the orchestrator's card), applied
//! without a hub restart.
//!
//! Every orchestrator runtime start reads the level now ([`Core::orchestrator_spawn_config`]:
//! the restrictions of `hub::orch::restrict`, the system prompt and its fingerprint), and the
//! hub remembers which level each runtime started with ([`AccessRt::runtime`], memory only: a
//! local runtime never outlives its hub). A change records the mode the level gives the
//! orchestrator on its thread (`thread.mode_changed` by the user, [`Core::access_mode`]) and
//! restarts a live orchestrator runtime that started with the other level on its session, so
//! the conversation goes on with the new restrictions and system prompt: through the settings
//! restart of `hub::restart` ([`Core::restart_for_settings`], a `thread.stopped` with reason
//! `mode_change`, then [`Core::restart_after_settings_change`] resumes the session), right away
//! when it is idle and when its turn ends otherwise (a running turn is never cut; the last
//! change made meanwhile is what the restart applies, and none happens when the level was
//! changed back). An `orchestrator.use` or `reset` under way replaces the runtime anyway: the
//! new orchestrator starts with the level as it is then.
//!
//! A change is also written to `config.toml` (`[orchestrator] access`, with
//! [`config_edit::set_key`]: every other byte stays), so the next hub starts with it; when it
//! can't be (no config file, an inline `[orchestrator]` table, a file that can't be written),
//! the level still applies until the hub stops, and the result says why it wasn't kept. It is
//! recorded as `config.changed {key: "orchestrator.access"}` on the orchestrator's thread, the
//! line every client shows and the signal to ask `orchestrator.show` again.
//!
//! At hub start, a mode recorded on the orchestrator's thread that the level would not give it
//! (the level changed in the file meanwhile) is recorded anew ([`Core::sync_orchestrator_mode`]).

use std::sync::Arc;

use serde_json::{json, Value};
use twr_core::config::OrchestratorAccess;
use twr_core::config_edit::{self, Current, Scalar};
use twr_core::event::*;
use twr_core::proto::{codes, RpcError};

use super::config_set::Effect;
use super::orch::orchestrator_mode;
use super::{append_err, internal, rpc, Batch, Caller, Core, Hub, Inner};

/// The config key the level is kept under.
pub(super) const ACCESS_KEY: &str = "orchestrator.access";

/// The orchestrator's access level and what its runtime started with. Memory only.
#[derive(Debug, Default)]
pub(super) struct AccessRt {
    /// The level from now on: what the next orchestrator runtime start gets.
    pub(super) level: OrchestratorAccess,
    /// The orchestrator runtime (its generation) that started last, and with which level.
    pub(super) runtime: Option<(u64, OrchestratorAccess)>,
    /// A settings restart for an access change stopped the orchestrator's runtime: its exit
    /// starts it again on its session ([`Core::restart_after_settings_change`]).
    pub(super) restarting: bool,
}

/// `value` as an access level (`"limited"`, `"full"`), if it is one.
pub(super) fn parse(value: &Value) -> Option<OrchestratorAccess> {
    serde_json::from_value(value.clone()).ok()
}

/// What `orchestrator.access` takes.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccessParams {
    /// The level to set; none: the level is only read.
    pub access: Option<String>,
}

/// Writes `[orchestrator] access = <to>` into the config file (a file that has it already is
/// left as it is): `Err` with why it could not be kept.
fn persist(hub: &Inner, to: OrchestratorAccess) -> Result<(), String> {
    let file = hub
        .config_file
        .as_deref()
        .ok_or_else(|| "this hub runs without a config file".to_string())?;
    let text = config_edit::read_text(file)?;
    let value = Scalar::Str(to.as_str().into());
    if matches!(
        config_edit::current(&text, &["orchestrator"], "access")?,
        Current::Scalar(ref v) if *v == value
    ) {
        return Ok(());
    }
    let new = config_edit::set_key(&text, &["orchestrator"], "access", &value)?;
    config_edit::write_text(file, &new, &config_edit::text_hash(&text))
}

impl Hub {
    /// `orchestrator.access {access?}` (see the module docs): without `access`, returns
    /// `{access}`; with it (`limited` or `full`), sets it and returns `{access, previous,
    /// persisted, persist_error?, restart}`, `restart` being when the orchestrator's runtime
    /// picks it up: `now` (restarted at once on its session), `after_turn` (when its running
    /// turn ends) or `none` (no orchestrator runtime runs it, or nothing changed: the next
    /// start uses it). The user only: refused for the orchestrator (`unauthorized`: its
    /// `set_config` puts the key on a confirm card), and for a paired device without `admin`.
    pub async fn orchestrator_access(
        &self,
        access: Option<&str>,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        if caller.is_orchestrator() {
            return Err(rpc(
                codes::UNAUTHORIZED,
                "the orchestrator cannot change its own access: set_config puts \
                 orchestrator.access on a confirm card for the user",
            ));
        }
        self.inner
            .require_scope(caller, "change the orchestrator's access", "admin")?;
        let Some(access) = access else {
            let level = self.inner.core().orch_access.level;
            return Ok(json!({ "access": level.as_str() }));
        };
        let to = parse(&json!(access.trim())).ok_or_else(|| {
            rpc(
                codes::BAD_REQUEST,
                format!("{access:?} is not an access level: limited or full"),
            )
        })?;
        let (hub, caller) = (self.clone(), caller.clone());
        // The file is read, locked and written: off the async threads.
        tokio::task::spawn_blocking(move || {
            let inner = &hub.inner;
            inner.core().set_access(inner, to, &caller)
        })
        .await
        .map_err(internal)?
    }
}

impl Core {
    /// [`Hub::orchestrator_access`] setting `to`, under the core lock.
    fn set_access(
        &mut self,
        hub: &Arc<Inner>,
        to: OrchestratorAccess,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        if self.shutting_down {
            return Err(rpc(codes::CONFLICT, "hub is shutting down"));
        }
        let previous = self.orch_access.level;
        let persisted = persist(hub, to);
        if let Err(e) = &persisted {
            tracing::warn!(error = %e, access = to.as_str(), "the orchestrator's access applies until the hub stops: config.toml was not changed");
        }
        let restart = if to == previous {
            "none"
        } else {
            if let Some(orch) = self.orch.thread.clone() {
                let rec = EventBody::ConfigChanged(ConfigChanged {
                    key: ACCESS_KEY.into(),
                    value: json!(to.as_str()),
                    before: Some(json!(previous.as_str())),
                    by: "user".into(),
                    effect: Effect::OrchestratorAccess.text().into(),
                    ..Default::default()
                });
                self.append(hub, &orch, None, Source::Hub, rec)
                    .map_err(append_err)?;
            }
            self.apply_access(hub, to, caller.source())
        };
        let mut out = json!({
            "access": to.as_str(),
            "previous": previous.as_str(),
            "persisted": persisted.is_ok(),
            "restart": restart,
        });
        if let Err(e) = persisted {
            out["persist_error"] = json!(e);
        }
        Ok(out)
    }

    /// Makes `to` the orchestrator's access level from now on (its change already recorded by
    /// the caller): records the mode it gives the current orchestrator on its thread when that
    /// differs ([`Core::access_mode`], as `source`), and restarts its live runtime when that
    /// started with the other level ([`Core::access_restart`]). Returns when the runtime picks
    /// it up: `now`, `after_turn` or `none`.
    pub(super) fn apply_access(
        &mut self,
        hub: &Arc<Inner>,
        to: OrchestratorAccess,
        source: Source,
    ) -> &'static str {
        self.orch_access.level = to;
        if let Err(e) = self.sync_orchestrator_mode(hub, source) {
            tracing::warn!(error = %format!("{e:#}"), "cannot record the orchestrator's mode for its access");
        }
        self.access_restart(hub)
    }

    /// Records the mode the access level gives the current orchestrator on its thread, as a
    /// `thread.mode_changed` by the user from `source`, when the mode recorded there is not it
    /// ([`Core::access_mode`]). The mode only says what its runtime runs in: every start
    /// clamps it to the level anyway (`hub::orch::restrict`).
    pub(super) fn sync_orchestrator_mode(
        &mut self,
        hub: &Inner,
        source: Source,
    ) -> anyhow::Result<()> {
        let Some(id) = self.orch.thread.clone() else {
            return Ok(());
        };
        let Some(view) = self.registry.get(&id) else {
            return Ok(());
        };
        let Some(mode) = self.access_mode(hub, &view.runtime, &view.mode) else {
            return Ok(());
        };
        let mut batch = Batch::default();
        batch.push(
            &id,
            None,
            source,
            EventBody::ThreadModeChanged(ThreadModeChanged {
                mode,
                previous: view.mode.clone(),
                by: ChangedBy::User,
                ..Default::default()
            }),
        );
        self.append_batch(hub, batch)?;
        Ok(())
    }

    /// The mode to record for an orchestrator on `runtime` that recorded `recorded`, at the
    /// current level, when it is not `recorded`: the level's mode of `recorded`
    /// ([`orchestrator_mode`]) is kept when it is `recorded` itself; otherwise (bypass left by
    /// full access, or a mode the level does not give) the mode a new orchestrator gets, of
    /// `threads.default_mode`.
    pub(super) fn access_mode(&self, hub: &Inner, runtime: &str, recorded: &str) -> Option<String> {
        let level = self.orch_access.level;
        if orchestrator_mode(runtime, recorded, level) == recorded {
            return None;
        }
        let mode = orchestrator_mode(runtime, &hub.config.threads.default_mode, level);
        (mode != recorded).then_some(mode)
    }

    /// The level the orchestrator's live runtime started with; `None` without a live runtime.
    pub(super) fn orch_live_access(&self) -> Option<OrchestratorAccess> {
        let thread = self.orch.thread.as_deref()?;
        let live = self.rt.get(thread)?.live.as_ref()?.generation;
        self.orch_access
            .runtime
            .filter(|(generation, _)| *generation == live)
            .map(|(_, a)| a)
    }

    /// The level the orchestrator's turns run with: its live runtime's, else the level its
    /// next start gets.
    pub(super) fn orch_runtime_access(&self) -> OrchestratorAccess {
        self.orch_live_access().unwrap_or(self.orch_access.level)
    }

    /// Whether the current orchestrator `thread`'s live runtime started with another level
    /// than the current one and is not being stopped or restarted already: a settings
    /// restart is due (`hub::restart`).
    pub(super) fn access_restart_due(&self, thread: &str) -> bool {
        if !self.orch.is_thread(thread) {
            return false;
        }
        let Some(rt) = self.rt.get(thread) else {
            return false;
        };
        let Some(live) = rt.live.as_ref() else {
            return false;
        };
        !(rt.restart_reason.is_some()
            || rt.stop_requested
            || live.stop_requested
            || live.given_up.is_some())
            && self
                .orch_live_access()
                .is_some_and(|a| a != self.orch_access.level)
    }

    /// Whether the access restart due narrows what the orchestrator may do without asking
    /// (full to limited): it does not wait for its runtime's background tasks nor its items
    /// raised between turns (`hub::restart`).
    pub(super) fn access_narrows(&self) -> bool {
        self.orch_live_access() == Some(OrchestratorAccess::Full)
            && self.orch_access.level == OrchestratorAccess::Limited
    }

    /// After the level changed: restarts the orchestrator's live runtime when it started with
    /// the other one, now when it is free and otherwise when it is next free (the dispatch of
    /// `hub::restart`). Returns `now` (being restarted, an earlier restart under way
    /// included), `after_turn`, or `none` (no live runtime, one that runs this level already,
    /// one being stopped, or a switch under way that replaces it).
    fn access_restart(&mut self, hub: &Arc<Inner>) -> &'static str {
        let Some(thread) = self.orch.thread.clone() else {
            return "none";
        };
        let Some(rt) = self.rt.get(&thread) else {
            return "none";
        };
        let Some(live) = rt.live.as_ref() else {
            return "none";
        };
        if rt.restart_reason.is_some() {
            return "now";
        }
        if rt.stop_requested || live.stop_requested || self.orch.switching {
            return "none";
        }
        if self.orch_live_access() == Some(self.orch_access.level) {
            return "none";
        }
        if let Err(e) = self.try_dispatch(hub, &thread) {
            tracing::error!(thread, error = %format!("{e:#}"), "dispatching after an access change failed");
        }
        if self
            .rt
            .get(&thread)
            .is_some_and(|r| r.restart_reason.is_some())
        {
            "now"
        } else {
            "after_turn"
        }
    }
}
