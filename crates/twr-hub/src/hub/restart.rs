//! Restarting a live runtime in its thread's new settings: the permission mode
//! (`thread.mode_changed`, [`super::mode`]), the model and the reasoning effort
//! (`thread.model_changed`, [`super::model`]), and replacing it with another runtime
//! (`thread.model_changed` with a `runtime`, [`super::switch`]).
//!
//! The settings are the thread's (`ThreadView::runtime`, `mode`, `model` and `effort`), and every runtime
//! start uses them ([`super::spawn_config`]), so a thread with no live runtime (idle after a
//! stop, parked, attached, a remote box offline) just starts in the new settings next time.
//! An attach whose runtime is still being stopped made its command with the old settings: the
//! change refuses that attach when it would be granted (`hub::attach`), never the change. A
//! live runtime was started in the old ones ([`ThreadRt::runtime_settings`], stamped as
//! `ext.twr.runtime`, `ext.twr.mode`, `ext.twr.model` and `ext.twr.effort` on its
//! `thread.started` / `thread.resumed`, so a remote runtime taken over after a hub restart knows them too): it is
//! restarted on its session the next time the thread is free to take a turn
//! ([`Core::restart_for_settings`], called first thing in [`Core::dispatch_next`]):
//!
//! - idle: right away (the RPC dispatches);
//! - mid-turn (running, or waiting on a permission or question of that turn): when the turn
//!   ends, before the next queued prompt or retry dispatches. The turn and its items stay the
//!   old process's;
//! - an item the runtime raised between turns (any reason: a permission, a question, an
//!   error) is still open: that process waits for its answer, and its exit would resolve it
//!   unanswered (`thread_stopped`), so a restart that keeps or widens the mode waits until it
//!   is answered (a narrowing one does not wait: see the next item; its stop resolves the
//!   item `thread_stopped`, like any stop). Meanwhile
//!   nothing is dispatched to the old process either (a queued prompt or retry would run in
//!   the old settings): answering the item dispatches again ([`Hub::respond`] for a runtime's
//!   item), as does the next prompt;
//! - the runtime has background tasks of its own running (`ThreadView::runtime_tasks`): a
//!   restart that keeps or widens what the runtime may do without asking waits until they
//!   end (the background ticker dispatches it then, `hub::background`). A narrowing one
//!   ([`RunSettings::narrows_to`]: anything the runtime may do without asking is lost) waits
//!   for neither its tasks nor its open items: a runtime starts turns of its own while its
//!   tasks run, in the old mode, for as long as they run. The restart stops the runtime now
//!   (after the running turn), and its tasks end with its process group; the change's result
//!   says so (`stops_background_tasks`, [`Core::stops_background_tasks`]).
//!
//! The restart goes through the stop machinery: a durable stop request that keeps the queue
//! (and a remote box's `stop` command), the runtime's own graceful stop, and its exit's
//! closing sequence, a `thread.stopped` with `ext.twr.reason` = `mode_change` when the
//! permission mode differs ([`MODE_RESTART_REASON`]), `runtime_change` when the runtime does
//! ([`RUNTIME_RESTART_REASON`]), `account_change` when only the account does
//! ([`ACCOUNT_RESTART_REASON`]: a move between accounts of one runtime, `hub::switch`), else
//! `model_change` ([`MODEL_RESTART_REASON`]); either way
//! the thread does not count as stopped. Then the queue dispatches (the next prompt resumes
//! the session in the new settings, or, after a runtime change, starts the new runtime's first
//! session with the handoff) and, if nothing did, the runtime starts again on its session
//! ([`Core::restart_after_settings_change`]; not after a runtime change: the thread has no
//! session of its new runtime yet, or one it resumes that must first be told what happened
//! since, so that one starts with the next prompt). A
//! `thread.stop` meanwhile wins: the thread stays stopped. Nothing is killed but what the stop
//! machinery kills (identity-checked).
//!
//! Not restarted: the orchestrator (its settings are its runtime's choice, `hub::orch`; the
//! RPCs refuse it) and `pty` threads (they apply no mode; `thread.set_model` refuses them).
//!
//! A runtime move is recorded on the live runtime's settings too ([`RunSettings::moved`]): a
//! move between two agents of the same runtime (two `acp` agents or presets, or back to the
//! runtime the thread had before a move that has not taken effect yet) leaves the names
//! equal, but the live process is still the previous agent's, so it is replaced all the same
//! (`runtime_change`). The mark comes from the `thread.model_changed` in the log, so a replay
//! after a hub restart marks a runtime taken over again; a start recorded after the move (one
//! that was under way) is stamped `ext.twr.runtime_moved`.
//!
//! An `acp` runtime's agent (its argv) is part of its settings too ([`RunSettings::agent`],
//! stamped as `ext.twr.acp_command` by the hub and by the runner that started it): the mark
//! above needs the start recorded before the move, which a remote start that arrives only
//! after a hub restart (the move recorded meanwhile) is not. Its stamp says it runs the
//! previous agent: it is no session of the new one (`ThreadView::handoff_pending`), and it is
//! replaced like a marked one. A stamp without an agent (a runner from before agent stamps)
//! is taken for the thread's agent.
//!
//! The account is part of them too ([`RunSettings::account`], stamped as `ext.twr.account` by
//! the hub, absent for the native account): a start of the account the thread moved away from
//! is no session of the new one (`ThreadView::handoff_pending`), and is replaced
//! (`account_change`).
//!
//! A stamp without `ext.twr.mode` (a runner from before stamping) is recorded as unknown
//! settings ([`RunSettings::unknown`]); one with a mode but without the model or the effort
//! key (a runner from before efforts) has that value unknown. Unknown differs from every
//! setting, so such a runtime is restarted in the thread's. A stamp without
//! `ext.twr.runtime` (from before runtime changes) has the runtime of the event's source
//! (`adapter:<runtime>`), which is what the hub supervises it as.
//!
//! [`ThreadRt::runtime_settings`]: super::ThreadRt::runtime_settings
//! [`Hub::respond`]: super::Hub::respond

use std::sync::Arc;

use serde_json::{json, Value};
use twr_core::event::*;
use twr_core::reduce::ThreadView;

use super::{AdapterScope, Batch, Core, Inner};

/// A stamped setting whose value is unknown (taken over from a runner that did not stamp it):
/// it differs from every setting, so the runtime is restarted in the thread's.
pub(super) const UNKNOWN: &str = "unknown";

/// What a runtime process was started with.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(super) struct RunSettings {
    /// The runtime (adapter).
    pub(super) runtime: String,
    /// The permission mode.
    pub(super) mode: String,
    /// The model (`None`: the runtime's default).
    pub(super) model: Option<String>,
    /// The reasoning effort (`None`: the runtime's default).
    pub(super) effort: Option<String>,
    /// The thread moved to another runtime after this one started (`thread.model_changed`
    /// with a `runtime`): it is not the thread's runtime, whatever its name (see the module
    /// docs). Stamped as `ext.twr.runtime_moved`.
    pub(super) moved: bool,
    /// An `acp` runtime's agent, its argv (`ThreadView::acp_command`; stamped as
    /// `ext.twr.acp_command`). `None`: no agent, or not known (a runner from before agent
    /// stamps), which [`RunSettings::runs_in`] takes for any agent.
    pub(super) agent: Option<Value>,
    /// The account (`ThreadView::account`; `None`: the runtime's native account). Stamped as
    /// `ext.twr.account` (absent for the native account).
    pub(super) account: Option<String>,
    /// The thread moved to another account of the same runtime after this one started
    /// (`thread.model_changed` with `account_changed`; also a move to the same account in
    /// another directory): with [`RunSettings::moved`], its restart is an account change. Not
    /// stamped: a start recorded after such a move restarts as a runtime change.
    pub(super) account_moved: bool,
    /// The named account's directory (links followed) the process was started with: the
    /// thread's when it started (`ThreadRt::account_dir`). Stamped as `ext.twr.account_dir`
    /// (absent for the native account). What the session reports (usage, limits, sign-in
    /// failures) is that directory's: one the account's name no longer leads to (removed and
    /// added again elsewhere) is not the account's now (`hub::usage`, `hub::retry`). `None`
    /// also when not known (a start from before the stamp). Not a setting: no restart.
    pub(super) account_dir: Option<std::path::PathBuf>,
    /// The native account's directory the process was started in: the runtime's own as the
    /// environment it started with selects it (`Core::native_store`: its selector variable
    /// from `threads.env`, the project's `env` or the hub's, else under `HOME`; the process's
    /// environment is not changed). Stamped as `ext.twr.session_store` (absent for a named account, and when
    /// not known: a start from before the stamp, or on another box). Not a setting: no
    /// restart; a move back resumes a native session only in it (`hub::switch`).
    #[serde(default)]
    pub(super) session_store: Option<std::path::PathBuf>,
    /// The login generation of the account the process was started on
    /// (`AccountsView::login_generation`; 0 on another box), or, for a process that resumes a
    /// session an earlier start named, that start's: the login the session began under
    /// (`Core::carry_login`, as the start is recorded). Stamped as
    /// `ext.twr.account_login` (absent when 0). Not a setting: no restart; a move back resumes
    /// a session only under the generation it ran in (`hub::switch`).
    #[serde(default)]
    pub(super) account_login: u64,
    /// The login note of the directory the process's login lived in when it started (the
    /// named account's, or the native account's [`RunSettings::session_store`]:
    /// `twr_core::account::login_note`; on the hub's own box only), `None` when there was
    /// none; carried from the session's earlier start like [`RunSettings::account_login`]. The
    /// native account's directory is not carried: each start's is its own. Stamped as `ext.twr.login_note` (absent: none). Not a setting: no restart; a
    /// move back resumes a session only while the note is the same (`hub::switch`).
    #[serde(default)]
    pub(super) login_note: Option<String>,
}

impl RunSettings {
    /// The settings a runtime of `view` starts with now ([`super::spawn_config`]).
    pub(super) fn of(view: &ThreadView) -> RunSettings {
        RunSettings {
            runtime: view.runtime.clone(),
            mode: view.mode.clone(),
            model: view.model.clone(),
            effort: view.effort.clone(),
            moved: false,
            agent: view.acp_command.clone(),
            account: view.account.clone(),
            account_moved: false,
            account_dir: None,
            session_store: None,
            account_login: 0,
            login_note: None,
        }
    }

    /// Settings nothing is known of ([`UNKNOWN`] each).
    pub(super) fn unknown() -> RunSettings {
        RunSettings {
            runtime: UNKNOWN.into(),
            mode: UNKNOWN.into(),
            model: Some(UNKNOWN.into()),
            effort: Some(UNKNOWN.into()),
            moved: false,
            agent: None,
            account: None,
            account_moved: false,
            account_dir: None,
            session_store: None,
            account_login: 0,
            login_note: None,
        }
    }

    /// Whether a runtime started with these settings runs in `to` (the thread's now,
    /// [`RunSettings::of`]): no restart is due. An agent known on one side only is taken to be
    /// the same.
    pub(super) fn runs_in(&self, to: &RunSettings) -> bool {
        self.runtime == to.runtime
            && self.mode == to.mode
            && self.model == to.model
            && self.effort == to.effort
            && self.moved == to.moved
            && self.same_agent(to)
            && self.account == to.account
    }

    /// Whether a runtime started with these settings is thread `view`'s current runtime, so
    /// that a settings restart would resume its session: not one the thread moved away from
    /// ([`RunSettings::moved`], another runtime, agent or account). A runtime not known (a
    /// runner from before stamps) is taken to be it.
    pub(super) fn is_runtime_of(&self, view: &ThreadView) -> bool {
        let to = RunSettings::of(view);
        !self.moved
            && ((self.runtime == to.runtime && self.account == to.account)
                || self.runtime == UNKNOWN)
            && self.same_agent(&to)
    }

    /// Whether a runtime started in `to` loses anything a runtime started with these settings
    /// may do without asking ([`twr_adapters::mode_narrows`]: the runtimes' effective
    /// policies, not only their levels; a policy twr cannot compare, such as an ACP agent's own
    /// mode id or settings taken over from a runner that did not stamp them, narrows). The
    /// same runtime and mode (a model or effort change) never narrows.
    pub(super) fn narrows_to(&self, to: &RunSettings) -> bool {
        twr_adapters::mode_narrows((&self.runtime, &self.mode), (&to.runtime, &to.mode))
    }

    /// Whether these and `to` name the same agent, or either names none.
    fn same_agent(&self, to: &RunSettings) -> bool {
        match (&self.agent, &to.agent) {
            (Some(a), Some(b)) => a == b,
            _ => true,
        }
    }

    /// The `ext.twr.reason` of the stop that restarts a runtime started with these settings
    /// in `to`: [`RUNTIME_RESTART_REASON`] when the runtime (a known one) or its agent changes
    /// (or the thread moved away from this one), [`ACCOUNT_RESTART_REASON`] when only the
    /// account does (or the thread moved to another account of the runtime, or to its account
    /// in another directory), [`MODE_RESTART_REASON`] when the permission mode does, else
    /// [`MODEL_RESTART_REASON`].
    pub(super) fn restart_reason(&self, to: &RunSettings) -> &'static str {
        // A runtime not known (a runner from before stamps) is no move.
        let other_runtime = self.runtime != to.runtime && self.runtime != UNKNOWN;
        let other_account = self.account != to.account && self.runtime != UNKNOWN;
        if other_runtime || !self.same_agent(to) {
            RUNTIME_RESTART_REASON
        } else if other_account || (self.moved && self.account_moved) {
            ACCOUNT_RESTART_REASON
        } else if self.moved {
            RUNTIME_RESTART_REASON
        } else if self.mode != to.mode {
            MODE_RESTART_REASON
        } else {
            MODEL_RESTART_REASON
        }
    }
}

/// The settings a start event's `ext.twr` stamp records, if it has a mode (see the module
/// docs: a missing model or effort key is unknown, `null` is the runtime's default, a missing
/// runtime is the one of `source`, the event's; `runtime_moved` is [`RunSettings::moved`];
/// `acp_command` is [`RunSettings::agent`], missing when not known; `account` is
/// [`RunSettings::account`], missing for the native account; `account_dir` is
/// [`RunSettings::account_dir`], `session_store` [`RunSettings::session_store`],
/// `account_login` [`RunSettings::account_login`], missing: 0, and `login_note`
/// [`RunSettings::login_note`]).
pub(super) fn stamped(p: &SessionPayload, source: &Source) -> Option<RunSettings> {
    let twr = p.ext.as_ref()?.get("twr")?;
    let mode = twr.get("mode")?.as_str()?.to_string();
    let value = |key: &str| match twr.get(key) {
        None => Some(UNKNOWN.to_string()),
        Some(v) => v.as_str().map(str::to_string),
    };
    let runtime = match (twr.get("runtime").and_then(Value::as_str), source) {
        (Some(r), _) => r.to_string(),
        (None, Source::Adapter(r)) => r.clone(),
        (None, _) => UNKNOWN.to_string(),
    };
    Some(RunSettings {
        runtime,
        mode,
        model: value("model"),
        effort: value("effort"),
        moved: twr.get(RUNTIME_MOVED_KEY) == Some(&Value::Bool(true)),
        agent: acp_command(p.ext.as_ref()).cloned(),
        account: twr
            .get(ACCOUNT_KEY)
            .and_then(Value::as_str)
            .map(str::to_string),
        account_moved: false,
        account_dir: twr
            .get(ACCOUNT_DIR_KEY)
            .and_then(Value::as_str)
            .filter(|d| !d.is_empty())
            .map(std::path::PathBuf::from),
        session_store: twr
            .get(SESSION_STORE_KEY)
            .and_then(Value::as_str)
            .filter(|d| !d.is_empty())
            .map(std::path::PathBuf::from),
        account_login: twr
            .get(ACCOUNT_LOGIN_KEY)
            .and_then(Value::as_u64)
            .unwrap_or(0),
        login_note: twr
            .get(LOGIN_NOTE_KEY)
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// `ext.twr.runtime`, `ext.twr.mode`, `ext.twr.model` and `ext.twr.effort` on a runtime's
/// `thread.started` / `thread.resumed`: what the hub (or, for a runtime it took over, its
/// runner: `twr_runner::lbox`) started it with; `null` for a model or effort left to the
/// runtime. `ext.twr.runtime_moved` (true) when the thread moved away from it meanwhile;
/// `ext.twr.acp_command`, an `acp` runtime's agent, when known (else a runner's stays);
/// `ext.twr.account`, the named account it runs on (absent: the native account), and
/// `ext.twr.account_dir`, its directory ([`RunSettings::account_dir`], when known);
/// `ext.twr.session_store`, the native account's directory ([`RunSettings::session_store`],
/// when known), `ext.twr.account_login`, the account's login generation
/// ([`RunSettings::account_login`], when not 0), and `ext.twr.login_note`, the login note of
/// its directory then ([`RunSettings::login_note`], when there was one).
pub(super) fn stamp(p: &mut SessionPayload, s: &RunSettings) {
    let ext = p.ext.get_or_insert_with(JsonMap::new);
    let twr = ext
        .entry("twr")
        .or_insert_with(|| Value::Object(Default::default()));
    if let Some(twr) = twr.as_object_mut() {
        twr.insert("runtime".into(), json!(s.runtime));
        twr.insert("mode".into(), json!(s.mode));
        twr.insert("model".into(), json!(s.model));
        twr.insert("effort".into(), json!(s.effort));
        if s.moved {
            twr.insert(RUNTIME_MOVED_KEY.into(), json!(true));
        } else {
            twr.remove(RUNTIME_MOVED_KEY);
        }
        if let Some(agent) = &s.agent {
            twr.insert(ACP_COMMAND_KEY.into(), agent.clone());
        }
        match &s.account {
            Some(a) => twr.insert(ACCOUNT_KEY.into(), json!(a)),
            None => twr.remove(ACCOUNT_KEY),
        };
        match s.account_dir.as_ref().filter(|_| s.account.is_some()) {
            Some(d) => twr.insert(ACCOUNT_DIR_KEY.into(), json!(d.to_string_lossy())),
            None => twr.remove(ACCOUNT_DIR_KEY),
        };
        match s.session_store.as_ref().filter(|_| s.account.is_none()) {
            Some(d) => twr.insert(SESSION_STORE_KEY.into(), json!(d.to_string_lossy())),
            None => twr.remove(SESSION_STORE_KEY),
        };
        match s.account_login {
            0 => twr.remove(ACCOUNT_LOGIN_KEY),
            n => twr.insert(ACCOUNT_LOGIN_KEY.into(), json!(n)),
        };
        match &s.login_note {
            Some(n) => twr.insert(LOGIN_NOTE_KEY.into(), json!(n)),
            None => twr.remove(LOGIN_NOTE_KEY),
        };
    }
}

/// Whether runtime `runtime` applies the thread's settings at all (`pty` runs a command line
/// twr does not configure).
pub(super) fn applies_settings(runtime: &str) -> bool {
    runtime != twr_adapters::pty::NAME
}

impl Core {
    /// Whether `thread`'s live runtime runs in other settings than the thread's now and is
    /// not being stopped or restarted already: a restart is due.
    pub(super) fn restart_due(&self, thread: &str) -> bool {
        let Some(view) = self.registry.get(thread) else {
            return false;
        };
        if view.is_orchestrator() || !applies_settings(&view.runtime) {
            return false;
        }
        let Some(rt) = self.rt.get(thread) else {
            return false;
        };
        let Some(live) = rt.live.as_ref() else {
            return false;
        };
        !(rt.restart_reason.is_some() || live.stop_requested || live.given_up.is_some())
            && rt
                .runtime_settings
                .as_ref()
                .is_some_and(|s| !s.runs_in(&RunSettings::of(view)))
    }

    /// Whether the restart due on `thread` ([`Core::restart_due`]) narrows what its runtime
    /// may do without asking ([`RunSettings::narrows_to`]): it does not wait for the runtime's
    /// background tasks (see the module docs).
    pub(super) fn narrowing_due(&self, thread: &str) -> bool {
        if !self.restart_due(thread) {
            return false;
        }
        match (
            self.rt
                .get(thread)
                .and_then(|r| r.runtime_settings.as_ref()),
            self.registry.get(thread),
        ) {
            (Some(from), Some(view)) => from.narrows_to(&RunSettings::of(view)),
            _ => false,
        }
    }

    /// Whether the restart due on `thread` ends background tasks its runtime runs: it narrows
    /// the mode ([`Core::narrowing_due`]), so it does not wait for them, and their processes
    /// end with the runtime's. Read right after a settings change is committed, for its result
    /// (`stops_background_tasks`).
    pub(super) fn stops_background_tasks(&self, thread: &str) -> bool {
        self.registry.get(thread).is_some_and(|v| v.runtime_tasks()) && self.narrowing_due(thread)
    }

    /// Restarts `thread`'s live runtime when it was started in other settings than the
    /// thread's now (see the module docs). Called by [`Core::dispatch_next`] once the thread is
    /// free; returns whether the thread must wait: a restart began (the runtime's exit
    /// dispatches again), or one is due but an item the runtime raised between turns defers it
    /// (nothing may run in the old settings meanwhile).
    pub(super) fn restart_for_settings(
        &mut self,
        hub: &Inner,
        thread: &str,
    ) -> anyhow::Result<bool> {
        if !self.restart_due(thread) {
            return Ok(false);
        }
        // The runtime's own background tasks defer a restart, unless it narrows the mode: the
        // runtime would keep starting turns of its own in the old one while they run.
        if self.registry.get(thread).is_some_and(|v| v.runtime_tasks())
            && !self.narrowing_due(thread)
        {
            return Ok(true);
        }
        // An item the runtime raised between turns (whatever its reason) is still waiting on
        // that process: the restart's closing sequence would resolve it unanswered. Unless the
        // restart narrows the mode: the process could go on working in the old one (a
        // background subagent waiting on its approval while others run), so it is stopped,
        // and its stop resolves the item like any stop.
        let narrowing = self.narrowing_due(thread);
        if !narrowing
            && !self
                .open_adapter_attention(thread, AdapterScope::All, false)
                .is_empty()
        {
            return Ok(true);
        }
        let Some(view) = self.registry.get(thread) else {
            return Ok(false);
        };
        let Some(from) = self.rt.get(thread).and_then(|r| r.runtime_settings.clone()) else {
            return Ok(false);
        };
        let to = RunSettings::of(view);
        let reason = from.restart_reason(&to);
        // Recorded first, like `thread.stop` (but the queue is kept): whenever the exit is
        // recorded, the thread closes as stopped, never crashed.
        let remote_stop = self.remote_stop(hub, thread);
        let batch = Batch {
            stop_requests: vec![thread.to_string()],
            remote_cmds: remote_stop.iter().cloned().collect(),
            ..Batch::default()
        };
        self.append_batch(hub, batch)?;
        if let Some(c) = &remote_stop {
            if let Some(rbox) = hub.remote(&c.box_name) {
                rbox.expect_stop(thread, &c.cmd_id);
            }
        }
        tracing::info!(
            thread,
            ?from,
            ?to,
            reason,
            "restarting the runtime in its new settings"
        );
        let rt = self.rt(thread);
        rt.stop_requested = true;
        rt.restart_reason = Some(reason);
        if let Some(live) = rt.live.as_mut() {
            live.stop_requested = true;
            let _ = live.handle.stop();
        }
        Ok(true)
    }

    /// After the exit of a runtime stopped for a settings restart was recorded and the queue
    /// had its chance: starts the runtime again on its session (no turn), unless a turn
    /// already started it, the thread was stopped meanwhile, or it cannot take a turn now
    /// (then the next prompt resumes it, in the new settings). Nor while a move to another
    /// runtime is pending (`ThreadView::handoff_pending`): the new runtime starts with the
    /// thread's next turn, even when it resumes a session the thread left earlier
    /// (`hub::switch`): started now, it would end the move before that turn told the session
    /// what happened since. A failure is logged: the thread is idle with no runtime, like
    /// after a stop.
    pub(super) fn restart_after_settings_change(&mut self, hub: &Arc<Inner>, thread: &str) {
        let Some(view) = self.registry.get(thread) else {
            return;
        };
        if view.stopped
            || view.handoff_pending
            || view.session_id.is_none()
            || view.is_orchestrator()
        {
            return;
        }
        if self.rt.get(thread).is_some_and(|r| r.live.is_some()) || !self.may_take_turn(thread) {
            return;
        }
        if let Err(e) = self.start_runtime(hub, thread, None) {
            tracing::error!(thread, error = %format!("{e:#}"), "cannot restart the runtime in its new settings; the next prompt starts it");
        }
    }

    /// When a settings change of `thread` committed (its view holds the new settings) takes
    /// effect: `next_start` (no live runtime), `now` (the live runtime runs in them already,
    /// or is being restarted: an idle one is, by the dispatch this runs) or `after_turn` (when
    /// the thread is next free). `dispatch_error` gets why that dispatch failed, if it did.
    pub(super) fn apply_settings(
        &mut self,
        hub: &Arc<Inner>,
        thread: &str,
        dispatch_error: &mut Option<String>,
    ) -> &'static str {
        let rt = self.rt.get(thread);
        if !rt.is_some_and(|r| r.live.is_some()) {
            // No runtime to restart. A card offering to compact the context before a held
            // message (`hub::idle_compact`) may no longer apply (a move to another runtime):
            // the queue is looked at again, which closes it and lets the message go.
            self.recheck_idle_offer(hub, thread);
            return "next_start";
        }
        let running_in_them = match (
            rt.and_then(|r| r.runtime_settings.as_ref()),
            self.registry.get(thread),
        ) {
            (Some(s), Some(view)) => s.runs_in(&RunSettings::of(view)),
            _ => false,
        };
        if running_in_them {
            // Changed back before the runtime was restarted: it runs in them already.
            return "now";
        }
        // Idle: restarted now; otherwise when the thread is next free.
        if let Err(e) = self.try_dispatch(hub, thread) {
            tracing::error!(thread, error = %format!("{e:#}"), "dispatching after a settings change failed");
            *dispatch_error = Some(format!("{e:#}"));
        }
        if self
            .rt
            .get(thread)
            .is_some_and(|r| r.restart_reason.is_some())
        {
            "now"
        } else {
            "after_turn"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use twr_core::config::Config;

    use super::super::{twr_ext_map, Hub, HubOptions};

    /// A move between two agents of one runtime marks the runtime started before it, also when
    /// the log is replayed (a hub restart that takes the runtime over), and a start recorded
    /// after the move stamped as moved away from stays marked and is no session of the new
    /// agent. The mark makes the restart a runtime change.
    #[tokio::test]
    async fn a_move_between_two_agents_of_one_runtime_marks_the_live_runtime() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        let hub = Hub::open(HubOptions::new(&db, Config::default()))
            .await
            .unwrap();
        let agent = |cmd: &str| twr_ext_map(json!({"acp_command": [cmd]}));
        hub.append(
            "t1",
            None,
            Source::User("test".into()),
            EventBody::ThreadCreated(ThreadCreated {
                name: "api".into(),
                box_name: "local".into(),
                runtime: "acp".into(),
                mode: "manual".into(),
                ext: agent("agent-a"),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        let start = |session: &str, moved: bool, cmd: &str| {
            let mut p = SessionPayload {
                session_id: Some(session.into()),
                ..Default::default()
            };
            let s = RunSettings {
                runtime: "acp".into(),
                mode: "manual".into(),
                model: None,
                effort: None,
                moved,
                agent: Some(json!([cmd])),
                account: None,
                account_moved: false,
                account_dir: None,
                session_store: None,
                account_login: 0,
                login_note: None,
            };
            stamp(&mut p, &s);
            EventBody::ThreadStarted(p)
        };
        let source = Source::Adapter("acp".into());
        hub.append(
            "t1",
            None,
            source.clone(),
            start("a-1", false, "agent-a"),
            None,
        )
        .unwrap();
        let settings = |core: &Core| core.rt.get("t1").and_then(|r| r.runtime_settings.clone());
        assert_eq!(settings(&hub.inner.core()).map(|s| s.moved), Some(false));
        hub.append(
            "t1",
            None,
            Source::User("test".into()),
            EventBody::ThreadModelChanged(ThreadModelChanged {
                runtime: Some("acp".into()),
                previous_runtime: Some("acp".into()),
                ext: agent("agent-b"),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        let check = |core: &Core| {
            let s = settings(core).expect("the previous agent's settings");
            let view = core.registry.get("t1").unwrap();
            assert!(s.moved);
            assert!(!s.runs_in(&RunSettings::of(view)));
            assert_eq!(
                s.restart_reason(&RunSettings::of(view)),
                RUNTIME_RESTART_REASON
            );
            assert!(view.handoff_pending && view.session_id.is_none());
        };
        check(&hub.inner.core());
        let (replayed, _) = Core::replay(&db).unwrap();
        check(&replayed);
        drop(replayed);
        // The previous agent's start, recorded after the move: still the previous one.
        hub.append(
            "t1",
            None,
            source.clone(),
            start("a-2", true, "agent-a"),
            None,
        )
        .unwrap();
        check(&hub.inner.core());
        let (replayed, _) = Core::replay(&db).unwrap();
        check(&replayed);
        drop(replayed);
        // The new agent's start: its session, its settings.
        hub.append("t1", None, source, start("b-1", false, "agent-b"), None)
            .unwrap();
        {
            let core = hub.inner.core();
            let view = core.registry.get("t1").unwrap();
            assert_eq!(
                (view.session_id.as_deref(), view.handoff_pending),
                (Some("b-1"), false)
            );
            assert_eq!(settings(&core), Some(RunSettings::of(view)));
        }
        hub.shutdown().await;
    }

    /// A move between two agents of one runtime recorded before the previous agent's first
    /// start (a remote start that arrives after a hub restart: no settings to mark): that
    /// start, stamped with its agent by its runner, is no session of the new agent, and its
    /// settings differ from the thread's, so the runtime is replaced (`runtime_change`).
    #[tokio::test]
    async fn a_late_start_of_the_previous_agent_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        let hub = Hub::open(HubOptions::new(&db, Config::default()))
            .await
            .unwrap();
        let agent = |cmd: &str| twr_ext_map(json!({"acp_command": [cmd]}));
        let user = || Source::User("test".into());
        hub.append(
            "t1",
            None,
            user(),
            EventBody::ThreadCreated(ThreadCreated {
                name: "api".into(),
                box_name: "local".into(),
                runtime: "acp".into(),
                mode: "manual".into(),
                ext: agent("agent-a"),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        hub.append(
            "t1",
            None,
            user(),
            EventBody::ThreadModelChanged(ThreadModelChanged {
                runtime: Some("acp".into()),
                previous_runtime: Some("acp".into()),
                ext: agent("agent-b"),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        assert!(hub
            .inner
            .core()
            .rt
            .get("t1")
            .and_then(|r| r.runtime_settings.as_ref())
            .is_none());
        // The previous agent's start as its runner stamped it (no `runtime_moved`: nothing
        // knew of the move when it started).
        let late = SessionPayload {
            session_id: Some("a-1".into()),
            ext: twr_ext_map(json!({
                "runtime": "acp", "mode": "manual", "model": null, "effort": null,
                "acp_command": ["agent-a"],
            })),
            ..Default::default()
        };
        hub.append(
            "t1",
            None,
            Source::Adapter("acp".into()),
            EventBody::ThreadStarted(late),
            None,
        )
        .unwrap();
        let check = |core: &Core| {
            let view = core.registry.get("t1").unwrap();
            assert_eq!(
                (view.session_id.as_deref(), view.handoff_pending),
                (None, true),
                "the previous agent's start is no session of the new one"
            );
            let s = core.rt.get("t1").and_then(|r| r.runtime_settings.clone());
            let s = s.expect("the previous agent's settings");
            assert_eq!(s.agent, Some(json!(["agent-a"])));
            assert!(!s.runs_in(&RunSettings::of(view)));
            assert_eq!(
                s.restart_reason(&RunSettings::of(view)),
                RUNTIME_RESTART_REASON
            );
        };
        check(&hub.inner.core());
        let (replayed, _) = Core::replay(&db).unwrap();
        check(&replayed);
        drop(replayed);
        hub.shutdown().await;
    }

    /// A move between accounts of one runtime: the live runtime's settings are marked (an
    /// account change), and a start of the previous account recorded after the move (stamped
    /// `ext.twr.account`, as the hub stamps it) is no session of the new account, on replay
    /// too; its settings differ, so it is replaced (`account_change`). The new account's start
    /// ends the handoff.
    #[tokio::test]
    async fn a_start_of_the_previous_account_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        let hub = Hub::open(HubOptions::new(&db, Config::default()))
            .await
            .unwrap();
        let user = || Source::User("test".into());
        hub.append(
            "t1",
            None,
            user(),
            EventBody::ThreadCreated(ThreadCreated {
                name: "api".into(),
                box_name: "local".into(),
                runtime: "claude-code".into(),
                mode: "manual".into(),
                ext: twr_ext_map(json!({ACCOUNT_KEY: "work", ACCOUNT_DIR_KEY: "/acc/work"})),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        let start = |session: &str, account: Option<&str>| {
            let mut p = SessionPayload {
                session_id: Some(session.into()),
                ..Default::default()
            };
            let s = RunSettings {
                runtime: "claude-code".into(),
                mode: "manual".into(),
                model: None,
                effort: None,
                moved: false,
                agent: None,
                account: account.map(str::to_string),
                account_moved: false,
                account_dir: None,
                // The native account's directory, and a login generation (round trip).
                session_store: account.is_none().then(|| "/native/claude".into()),
                account_login: if account.is_some() { 2 } else { 0 },
                login_note: Some("n1".into()),
            };
            stamp(&mut p, &s);
            assert_eq!(stamped(&p, &Source::Hub), Some(s));
            EventBody::ThreadStarted(p)
        };
        let source = Source::Adapter("claude-code".into());
        hub.append("t1", None, source.clone(), start("w-1", Some("work")), None)
            .unwrap();
        hub.append(
            "t1",
            None,
            user(),
            EventBody::ThreadModelChanged(ThreadModelChanged {
                runtime: Some("claude-code".into()),
                previous_runtime: Some("claude-code".into()),
                account_changed: true,
                account: Some("work2".into()),
                previous_account: Some("work".into()),
                ext: twr_ext_map(json!({ACCOUNT_DIR_KEY: "/acc/work2"})),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        {
            let core = hub.inner.core();
            let s = core.rt.get("t1").and_then(|r| r.runtime_settings.clone());
            let s = s.expect("the previous account's settings");
            let view = core.registry.get("t1").unwrap();
            assert!(s.moved && s.account_moved);
            assert_eq!(
                s.restart_reason(&RunSettings::of(view)),
                ACCOUNT_RESTART_REASON
            );
            assert_eq!(
                core.rt.get("t1").and_then(|r| r.account_dir.clone()),
                Some("/acc/work2".into())
            );
        }
        // The previous account's start, recorded after the move.
        hub.append("t1", None, source.clone(), start("w-2", Some("work")), None)
            .unwrap();
        let check = |core: &Core| {
            let view = core.registry.get("t1").unwrap();
            assert_eq!(view.account.as_deref(), Some("work2"));
            assert_eq!(
                (view.session_id.as_deref(), view.handoff_pending),
                (None, true),
                "the previous account's start is no session of the new one"
            );
            let s = core.rt.get("t1").and_then(|r| r.runtime_settings.clone());
            let s = s.expect("the previous account's settings");
            assert_eq!(s.account.as_deref(), Some("work"));
            assert!(!s.runs_in(&RunSettings::of(view)));
            assert!(!s.is_runtime_of(view));
            assert_eq!(
                s.restart_reason(&RunSettings::of(view)),
                ACCOUNT_RESTART_REASON
            );
        };
        check(&hub.inner.core());
        let (replayed, _) = Core::replay(&db).unwrap();
        check(&replayed);
        drop(replayed);
        // The new account's start: its session.
        hub.append("t1", None, source, start("w2-1", Some("work2")), None)
            .unwrap();
        {
            let core = hub.inner.core();
            let view = core.registry.get("t1").unwrap();
            assert_eq!(
                (view.session_id.as_deref(), view.handoff_pending),
                (Some("w2-1"), false)
            );
            let s = core.rt.get("t1").and_then(|r| r.runtime_settings.clone());
            assert!(s.unwrap().runs_in(&RunSettings::of(view)));
        }
        hub.shutdown().await;
    }

    /// A restart begun for a new model whose thread then moved to another runtime (before the
    /// runtime exited) is that move's: its stop says `runtime_change`, and a clear for the new
    /// runtime is not refused because of the runtime being replaced.
    #[tokio::test]
    async fn a_move_during_a_model_restart_makes_it_a_runtime_change() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("twr.db");
        let hub = Hub::open(HubOptions::new(&db, Config::default()))
            .await
            .unwrap();
        let user = || Source::User("test".into());
        hub.append(
            "t1",
            None,
            user(),
            EventBody::ThreadCreated(ThreadCreated {
                name: "api".into(),
                box_name: "local".into(),
                runtime: "fake".into(),
                mode: "manual".into(),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        let mut p = SessionPayload {
            session_id: Some("s1".into()),
            ..Default::default()
        };
        let started = RunSettings {
            runtime: "fake".into(),
            mode: "manual".into(),
            model: Some("m1".into()),
            effort: None,
            moved: false,
            agent: None,
            account: None,
            account_moved: false,
            account_dir: None,
            session_store: None,
            account_login: 0,
            login_note: None,
        };
        stamp(&mut p, &started);
        hub.append(
            "t1",
            None,
            Source::Adapter("fake".into()),
            EventBody::ThreadStarted(p),
            None,
        )
        .unwrap();
        let reason = |core: &Core| {
            let plan = core
                .closing_plan("t1", super::super::Exit::Stopped)
                .unwrap();
            plan.into_iter()
                .find_map(|e| match e.body {
                    EventBody::ThreadStopped(p) => Some(p),
                    _ => None,
                })
                .and_then(|p| p.ext)
                .and_then(|e| e["twr"]["reason"].as_str().map(str::to_string))
        };
        // The model restart is under way (its stop requested).
        hub.inner.core().rt("t1").restart_reason = Some(MODEL_RESTART_REASON);
        assert_eq!(
            reason(&hub.inner.core()).as_deref(),
            Some(MODEL_RESTART_REASON)
        );
        hub.append(
            "t1",
            None,
            user(),
            EventBody::ThreadModelChanged(ThreadModelChanged {
                model: Some("m2".into()),
                runtime: Some("codex".into()),
                previous_runtime: Some("fake".into()),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
        {
            let core = hub.inner.core();
            assert_eq!(reason(&core).as_deref(), Some(RUNTIME_RESTART_REASON));
            let view = core.registry.get("t1").unwrap();
            let live = core.rt.get("t1").and_then(|r| r.runtime_settings.clone());
            assert!(!live.unwrap().is_runtime_of(view));
            assert!(core.rt.get("t1").unwrap().session_settings.is_none());
        }
        hub.shutdown().await;
    }

    /// A narrowing compares what each side's runtime starts its mode with; one that cannot be
    /// told counts as narrowing, and a model or effort change never narrows.
    #[test]
    fn narrowing_compares_the_levels_each_runtime_starts_with() {
        let s = |runtime: &str, mode: &str| RunSettings {
            runtime: runtime.into(),
            mode: mode.into(),
            model: None,
            effort: None,
            moved: false,
            agent: None,
            account: None,
            account_moved: false,
            account_dir: None,
            session_store: None,
            account_login: 0,
            login_note: None,
        };
        assert!(s("claude-code", "auto").narrows_to(&s("claude-code", "plan")));
        assert!(!s("claude-code", "plan").narrows_to(&s("claude-code", "auto")));
        let mut other_model = s("codex", "auto");
        other_model.model = Some("m2".into());
        assert!(!s("codex", "auto").narrows_to(&other_model));
        assert!(s("codex", "auto").narrows_to(&s("claude-code", "manual")));
        assert!(!s("codex", "manual").narrows_to(&s("claude-code", "acceptEdits")));
        assert!(RunSettings::unknown().narrows_to(&s("codex", "auto")));
        assert!(s("acp", "their-own").narrows_to(&s("acp", "another")));
        // Same level, less policy.
        assert!(s("codex", "on-request").narrows_to(&s("codex", "manual")));
        assert!(s("codex", "auto").narrows_to(&s("codex", "never")));
    }

    /// The agent takes part in the comparison only when both sides know it: a stamp without
    /// one (a runner from before agent stamps) runs in the thread's settings.
    #[test]
    fn an_unknown_agent_is_the_threads() {
        let s = |agent: Option<Value>| RunSettings {
            runtime: "acp".into(),
            mode: "manual".into(),
            model: None,
            effort: None,
            moved: false,
            agent,
            account: None,
            account_moved: false,
            account_dir: None,
            session_store: None,
            account_login: 0,
            login_note: None,
        };
        let a = || Some(json!(["agent-a"]));
        assert!(s(a()).runs_in(&s(a())));
        assert!(s(None).runs_in(&s(a())));
        assert!(s(a()).runs_in(&s(None)));
        assert!(!s(a()).runs_in(&s(Some(json!(["agent-b"])))));
        let mut p = SessionPayload::default();
        stamp(&mut p, &s(None));
        assert_eq!(stamped(&p, &Source::Hub).map(|s| s.agent), Some(None));
        stamp(&mut p, &s(a()));
        assert_eq!(stamped(&p, &Source::Hub).map(|s| s.agent), Some(a()));
        // Settings nothing is known of (a runner from before stamps) restart as a mode
        // change, not as a move to another runtime.
        assert_eq!(
            RunSettings::unknown().restart_reason(&s(None)),
            MODE_RESTART_REASON
        );
        let mut moved = s(None);
        moved.moved = true;
        assert_eq!(moved.restart_reason(&s(None)), RUNTIME_RESTART_REASON);
    }
}
