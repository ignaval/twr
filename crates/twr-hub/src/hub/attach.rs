//! Attach: `attach.begin` hands a thread to its runtime's interactive CLI
//! (locally, or over ssh for a remote box), `attach.end` / `attach.child` and the lease
//! connection closing end or extend it, and the watch over an attach whose process outlived its
//! lease connection.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Map, Value};
use tokio::sync::oneshot;
use twr_core::adapter::{AttachApplies, SpawnConfig};
use twr_core::event::*;
use twr_core::proto::{codes, RpcError};
use twr_core::reduce::ThreadView;
use twr_runner::workdirs::WorkdirUse;

use super::{
    append_err, git, internal, rpc, runtime_spec, twr_ext_map, AttachLease, Core, Exit, Hub,
    HubState, Inner, Live, RuntimeExiting, ThreadRt, ATTACH_QUIESCE_TIMEOUT, LOCAL_BOX,
};
use crate::host::{ThreadHost, AGENT_ENV, ATTACH_ENV};
use crate::store;

/// `ext.twr.reason` of the `thread.detached` that ends an attach whose thread changed (its
/// mode, model, effort or session) while its runtime was being stopped: the command it was
/// granted was made from the old settings ([`Hub::attach_begin`]).
const CHANGED_WHILE_ATTACHING: &str = "changed_while_attaching";

/// How often the hub checks whether the interactive process of an attach whose lease connection
/// is gone still runs.
const ATTACH_PROCESS_POLL: Duration = Duration::from_millis(250);

/// What `attach.begin` grants: the runtime's interactive command for the CLI to run.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct AttachGrant {
    /// Thread id.
    pub thread: String,
    /// Lease id: only the holder of this lease can end the attach (see [`Hub::attach_end`]).
    pub lease: u64,
    /// Program and arguments.
    pub argv: Vec<String>,
    /// Working directory.
    pub cwd: Option<String>,
    /// Environment changes: a string sets the variable, `null` removes it. Always sets
    /// [`ATTACH_ENV`] (the marker the client's process carries, see `attach.child`) and
    /// [`AGENT_ENV`] (`<thread>:attach.<lease>`: what it and its descendants start is the
    /// thread's agent, `hub::agents`); on the hub's own box also `TWR_WORKSPACE`, the hub's
    /// workspace (`null` for `default`).
    pub env: Map<String, Value>,
    /// Session the command resumes.
    pub session_id: String,
    /// Runtime name.
    pub runtime: String,
    /// The thread's box when it is a remote one: `argv` is then `ssh -t <no forwarding>
    /// <boxes.NAME.ssh> -a -x -k -- <command>` (`crate::remote::ssh_command`; the runtime's
    /// own command runs on that box, in its directory there), `cwd`
    /// is empty and `env` has only [`ATTACH_ENV`] and [`AGENT_ENV`] (for the local `ssh`).
    #[serde(rename = "box", skip_serializing_if = "Option::is_none")]
    pub box_name: Option<String>,
    /// The first attach of a Claude Code thread to the worktree twr made for it (the log has
    /// no `thread.attached` of the thread): Claude asks whether to trust the folder, and its
    /// default answer (No) exits. The CLI tells the user to answer Yes; twr never edits
    /// Claude's own settings to skip the question.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub trust_prompt: bool,
}

impl AttachGrant {
    fn new(
        thread: &str,
        view: &ThreadView,
        cfg: &SpawnConfig,
        session_id: &str,
        cmd: &std::process::Command,
    ) -> Result<AttachGrant, RpcError> {
        let text = |s: &std::ffi::OsStr| {
            s.to_str().map(str::to_string).ok_or_else(|| {
                rpc(
                    codes::INTERNAL,
                    format!("the attach command has a non-UTF-8 part: {s:?}"),
                )
            })
        };
        let mut argv = vec![text(cmd.get_program())?];
        for a in cmd.get_args() {
            argv.push(text(a)?);
        }
        let cwd = match cmd.get_current_dir() {
            Some(d) => Some(text(d.as_os_str())?),
            None if !cfg.cwd.as_os_str().is_empty() => Some(text(cfg.cwd.as_os_str())?),
            None => None,
        };
        let mut env = Map::new();
        for (k, v) in cmd.get_envs() {
            let v = match v {
                Some(v) => Value::String(text(v)?),
                None => Value::Null,
            };
            env.insert(text(k)?, v);
        }
        Ok(AttachGrant {
            thread: thread.to_string(),
            lease: 0,
            argv,
            cwd,
            env,
            session_id: session_id.to_string(),
            runtime: view.runtime.clone(),
            box_name: None,
            trust_prompt: false,
        })
    }
}

/// A remote thread's attach command and what it was made for (see
/// [`Hub::remote_attach_command`]).
struct RemoteAttach {
    thread: String,
    box_name: String,
    session_id: String,
    runtime: String,
    /// The runtime configuration it was made from (mode, model, effort, directory, the
    /// orchestrator's restrictions): a change of any while the runner was asked refuses it.
    cfg: SpawnConfig,
    cmd: std::process::Command,
    /// The directory it runs in on the box.
    dir: Option<PathBuf>,
}

/// A thread's interactive command, as [`Hub::interactive_command`] made it.
struct AttachCommand {
    /// The thread's box.
    host: Arc<dyn ThreadHost>,
    /// The session the command resumes.
    session: String,
    /// The runtime configuration it was made from.
    cfg: SpawnConfig,
    cmd: std::process::Command,
    /// The directory it runs in on a remote box.
    remote_dir: Option<PathBuf>,
}

impl Core {
    /// The thread `query` names, with its view, when it can be attached: the hub is not
    /// shutting down, and the thread is not retired, attached or being attached, parked (or
    /// being parked or resumed), or given up on, and its runtime can attach in its mode.
    fn attachable(&self, query: &str) -> Result<(String, ThreadView), RpcError> {
        if self.shutting_down {
            return Err(rpc(codes::CONFLICT, "hub is shutting down"));
        }
        let id = self.resolve(query)?;
        let view = self.view(&id)?.clone();
        self.refuse_retired(&id)?;
        self.refuse_deleting(&id)?;
        if let Some(c) = &view.attached {
            return Err(rpc(
                codes::CONFLICT,
                format!("{} is already attached by {c}", view.name),
            ));
        }
        if let Some(a) = self.rt.get(&id).and_then(|r| r.attach.as_ref()) {
            return Err(rpc(
                codes::CONFLICT,
                format!("{} is already being attached by {}", view.name, a.client),
            ));
        }
        // A runtime the hub gave up on: its exit records the crash first.
        if self.rt.get(&id).is_some_and(ThreadRt::giving_up) {
            return Err(rpc(codes::CONFLICT, RuntimeExiting(id).to_string()));
        }
        // A parked thread's worktree may be gone: resume first (it recreates it).
        if view.parked
            || self
                .rt
                .get(&id)
                .is_some_and(|r| r.park.is_some() || r.resuming)
        {
            return Err(rpc(
                codes::CONFLICT,
                format!(
                    "{} is parked (or being parked or resumed); resume it first",
                    view.name
                ),
            ));
        }
        if self.rt.get(&id).is_some_and(|r| r.trimming) {
            return Err(rpc(
                codes::CONFLICT,
                format!(
                    "{}'s build output is being trimmed; try again when that is done",
                    view.name
                ),
            ));
        }
        if let Some(why) = self
            .rt
            .get(&id)
            .and_then(|r| runtime_spec::acp_attach_refusal(&view.name, &view.mode, &r.acp_env))
        {
            return Err(rpc(codes::CONFLICT, why));
        }
        Ok((id, view))
    }

    /// The runtime configuration thread `id`'s (`view`) interactive command is made from: its
    /// start configuration ([`Core::spawn_config`]: mode, model, effort, directory, its
    /// account's environment), an `acp` thread's preset attach command (`TWR_ACP_ATTACH`), and
    /// the current orchestrator's restrictions (or the attach is refused). Refused too on an
    /// account that cannot run its session ([`Core::refuse_account`]).
    fn attach_config(
        &self,
        hub: &Inner,
        id: &str,
        view: &ThreadView,
    ) -> Result<SpawnConfig, RpcError> {
        self.refuse_account(view)?;
        let mut cfg = self.spawn_config(hub, view);
        if let Some(r) = self.rt.get(id) {
            cfg.env.extend(r.acp_env.iter().cloned());
        }
        self.orchestrator_attach_config(id, &view.runtime, &mut cfg, hub)?;
        Ok(cfg)
    }

    /// Why thread `id` (`view`) cannot be attached through the command made from `cfg`, which
    /// runs its session with the settings `applies` says ([`ThreadHost::attach_applies`]): the
    /// session last ran with another model or effort than `cfg`'s (a change that has not
    /// reached it yet: the thread was stopped, or its runtime is restarted only once idle,
    /// `hub::restart`) and the command does not apply that setting, so the interactive CLI
    /// would resume the session in the old one. None when the session runs `cfg`'s settings,
    /// or nothing is known of what it runs.
    fn attach_settings_refusal(
        &self,
        id: &str,
        view: &ThreadView,
        cfg: &SpawnConfig,
        applies: AttachApplies,
    ) -> Option<String> {
        let rt = self.rt.get(id)?;
        let ran = rt
            .runtime_settings
            .as_ref()
            .or(rt.session_settings.as_ref())?;
        let stale: Vec<_> = [
            ("model", applies.model, &ran.model, &cfg.model),
            ("effort", applies.effort, &ran.effort, &cfg.effort),
        ]
        .into_iter()
        .filter(|(_, applied, from, to)| !applied && from != to)
        .collect();
        if stale.is_empty() {
            return None;
        }
        let shown = |v: &Option<String>| match v {
            Some(v) => format!("{v:?}"),
            None => "the runtime's default".to_string(),
        };
        let (from, to): (Vec<_>, Vec<_>) = stale
            .iter()
            .map(|(what, _, from, to)| {
                (
                    format!("{what} {}", shown(from)),
                    format!("{what} {}", shown(to)),
                )
            })
            .unzip();
        Some(format!(
            "cannot attach {name}: its session still runs {from}, and the {runtime} runtime's \
             attach command cannot switch it to the thread's {to}; send {name} a prompt first \
             (its runtime resumes the session with them), then attach",
            name = view.name,
            from = from.join(" and "),
            runtime = view.runtime,
            to = to.join(" and "),
        ))
    }

    /// Reserves `dir`, where thread `id`'s interactive runtime works on `host`, until the
    /// lease ends: no worktree removal of the box (a park of the thread that owns the worktree)
    /// takes it from under the interactive runtime. A worktree change of it in progress
    /// refuses the attach. A local box keeps the reservation itself (as for a runtime start,
    /// `twr_runner::workdirs`); for a remote box (`remote`) the hub does ([`AttachLease`]).
    fn attach_workdir(
        &self,
        host: &Arc<dyn ThreadHost>,
        id: &str,
        view: &ThreadView,
        dir: Option<&Path>,
        remote: bool,
        client: &str,
    ) -> Result<Option<WorkdirUse>, RpcError> {
        // A worktree being parked, resumed or deleted: refused on any box (a local box's
        // reservation only starts once git runs, and a deletion goes on after its park).
        if let Some(other) = dir.and_then(|d| self.worktree_change_over(&view.box_name, d)) {
            return Err(rpc(
                codes::CONFLICT,
                format!(
                    "cannot attach {}: the worktree of {other} it works in is being parked, \
                     resumed or deleted; try again once that has finished",
                    view.name
                ),
            ));
        }
        Ok(match dir {
            Some(_) if remote => None,
            Some(d) => host
                .use_workdir(d, &format!("{id} (attached by {client})"))
                .map_err(|e| {
                    rpc(
                        codes::CONFLICT,
                        format!("cannot attach {}: {e:#}", view.name),
                    )
                })?,
            None => None,
        })
    }

    /// Takes a new attach lease on thread `id` for `client` (its marker goes into `grant`'s
    /// environment) and quiesces the thread: the live runtime is stopped (its exit appends
    /// `thread.attached`), or with none the closing sequence is recorded now. Returns the lease
    /// and what is woken once `thread.attached` is appended.
    fn take_attach_lease(
        &mut self,
        hub: &Inner,
        id: &str,
        client: &str,
        grant: &mut AttachGrant,
        dir: Option<PathBuf>,
        workdir: Option<WorkdirUse>,
    ) -> Result<(u64, oneshot::Receiver<()>), RpcError> {
        self.leases += 1;
        let lease = self.leases;
        let (tx, rx) = oneshot::channel();
        // The marker the interactive process carries: how the hub recognizes it later.
        grant
            .env
            .insert(ATTACH_ENV.to_string(), json!(attach_marker(id, lease)));
        grant
            .env
            .insert(AGENT_ENV.to_string(), json!(format!("{id}:attach.{lease}")));
        // An agent's own `twr` calls from the attached CLI reach this hub's workspace (`null`
        // removes a `TWR_WORKSPACE` the client has for `default`). A remote box's CLI runs
        // there, not under the local `ssh`.
        if grant.box_name.is_none() {
            grant.env.insert(
                twr_core::paths::WORKSPACE_ENV.to_string(),
                json!(hub.workspace),
            );
        }
        self.rt(id).attach = Some(AttachLease {
            lease,
            client: client.to_string(),
            waiter: Some(tx),
            attached: false,
            dir,
            _workdir: workdir,
            child: None,
            orphaned: false,
        });
        let stopping = match self.rt.get_mut(id).and_then(|r| r.live.as_mut()) {
            Some(live) => {
                live.stop_requested = true;
                live.attach = true;
                let _ = live.handle.stop();
                true
            }
            None => false,
        };
        if !stopping {
            // No runtime process: nothing to wait for (a turn can only be open here if the
            // hub lost the process, and the closing sequence ends it).
            let closed = self.close_process(
                hub,
                id,
                Exit::Attach {
                    client: client.to_string(),
                },
            );
            self.attach_granted(id, closed.is_ok());
            closed.map_err(append_err)?;
        }
        Ok((lease, rx))
    }
}

impl Core {
    /// The attaching client when `live` was stopped for `attach.begin` (and that lease still
    /// waits for the exit).
    pub(super) fn attach_client(&self, thread: &str, live: &Live) -> Option<String> {
        if !live.attach {
            return None;
        }
        self.rt
            .get(thread)
            .and_then(|r| r.attach.as_ref())
            .filter(|a| !a.attached)
            .map(|a| a.client.clone())
    }

    /// The runtime quiesced for an attach has exited and its closing sequence ran: marks the
    /// lease attached (when `ok`, else drops it) and wakes `attach.begin`.
    pub(super) fn attach_granted(&mut self, thread: &str, ok: bool) {
        let Some(rt) = self.rt.get_mut(thread) else {
            return;
        };
        let Some(a) = rt.attach.as_mut() else {
            return;
        };
        if let Some(w) = a.waiter.take() {
            let _ = w.send(());
        }
        if ok {
            a.attached = true;
        } else {
            rt.attach = None;
        }
    }

    /// How to end `thread`'s attach, for a refusal while it lasts: the client that holds it, or,
    /// once that client is gone, the process the hub waits for.
    pub(super) fn end_attach_hint(&self, thread: &str) -> String {
        let orphan = self
            .rt
            .get(thread)
            .and_then(|r| r.attach.as_ref())
            .filter(|a| a.orphaned)
            .and_then(|a| a.child.as_ref());
        match orphan {
            Some(c) => format!(
                "its `twr attach` is gone but the attached process {} still runs; the attach \
                 ends when it exits",
                c.pid
            ),
            None => "end the attach first".to_string(),
        }
    }

    /// At startup: the thread was attached when the previous hub stopped and the interactive
    /// process `child` of that attach still runs. Takes the attach over as a lease without a
    /// connection (its directory held again on a local box) and watches the process
    /// ([`watch_attach_process`]).
    pub(super) fn adopt_attach(
        &mut self,
        hub: &Arc<Inner>,
        thread: &str,
        child: store::AttachProcess,
    ) {
        let Some(view) = self.registry.get(thread) else {
            return;
        };
        let client = view.attached.clone().unwrap_or_default();
        let box_name = view.box_name.clone();
        let dir = child.dir.as_ref().map(PathBuf::from);
        let workdir = match (&dir, hub.has_remote(&box_name)) {
            (Some(d), false) => hub.host(&box_name).and_then(|h| {
                h.use_workdir(d, &format!("{thread} (attached by {client})"))
                    .unwrap_or_else(|e| {
                        tracing::warn!(thread, error = %format!("{e:#}"), "cannot hold the directory of an attach the hub took over");
                        None
                    })
            }),
            _ => None,
        };
        self.leases += 1;
        let lease = self.leases;
        self.rt(thread).attach = Some(AttachLease {
            lease,
            client,
            waiter: None,
            attached: true,
            dir,
            _workdir: workdir,
            child: Some(child.clone()),
            orphaned: true,
        });
        tokio::spawn(watch_attach_process(
            Arc::downgrade(hub),
            thread.to_string(),
            lease,
            child,
        ));
    }

    /// Appends `thread.detached` for the thread's attached client and lets its queued prompts
    /// run (on a lazily resumed session). `reason` goes to `ext.twr.reason`.
    pub(super) fn detach(
        &mut self,
        hub: &Arc<Inner>,
        thread: &str,
        source: Source,
        reason: Option<&str>,
    ) -> anyhow::Result<()> {
        let client = match self.rt.get(thread).and_then(|r| r.attach.as_ref()) {
            Some(a) => a.client.clone(),
            None => match self.registry.get(thread).and_then(|v| v.attached.clone()) {
                Some(c) => c,
                None => return Ok(()),
            },
        };
        // The lease goes only once `thread.detached` is recorded: a failed append leaves the
        // attach (and its lease) as it was, so `attach.end` (or the lease connection closing)
        // can try again. Dispatching is not part of the detach: the queue stays in
        // `pending_prompts` and is dispatched on the next send, turn end or hub start.
        self.append(
            hub,
            thread,
            None,
            source,
            EventBody::ThreadDetached(ClientPayload {
                client,
                ext: reason.and_then(|r| twr_ext_map(json!({ "reason": r }))),
                ..Default::default()
            }),
        )?;
        if let Some(rt) = self.rt.get_mut(thread) {
            rt.attach = None;
        }
        // The detach is done: a dispatch failure is the queue's (it stays queued for the next
        // send, turn end or hub start), not the detach's.
        if let Err(e) = self.try_dispatch(hub, thread) {
            tracing::error!(thread, error = %format!("{e:#}"), "dispatching after a detach failed");
        }
        Ok(())
    }
}

/// The [`ATTACH_ENV`] value of the interactive process of `thread`'s attach under `lease`.
fn attach_marker(thread: &str, lease: u64) -> String {
    format!("{thread}:{lease}")
}

/// An attach whose lease connection is gone (or that a restarted hub took over) while its
/// interactive process `child` ran: waits until that process has exited, then appends
/// `thread.detached` (reason `attach process exited`) and dispatches, if the thread is still
/// attached under `lease`. Ends with the hub (a restarted hub picks the attach up again from
/// `attach_processes`); a failed append is tried again on the next check.
pub(super) async fn watch_attach_process(
    hub: std::sync::Weak<Inner>,
    thread: String,
    lease: u64,
    child: store::AttachProcess,
) {
    loop {
        let c = child.clone();
        let running = tokio::task::spawn_blocking(move || {
            twr_runner::reap::attach_process_running(c.pid, c.start_time, &c.marker, false)
        })
        .await
        // A check that could not run keeps the attach (the safe side) until the next one.
        .unwrap_or(true);
        let Some(inner) = hub.upgrade() else {
            return;
        };
        if !running {
            let mut core = inner.core();
            if core.shutting_down {
                return;
            }
            let ours = core
                .rt
                .get(&thread)
                .and_then(|r| r.attach.as_ref())
                .is_some_and(|a| a.lease == lease && a.orphaned);
            if !ours {
                return;
            }
            match core.detach(&inner, &thread, Source::Hub, Some("attach process exited")) {
                Ok(()) => {
                    tracing::info!(%thread, pid = child.pid, "the attached process exited; detached");
                    return;
                }
                Err(e) => {
                    tracing::error!(%thread, error = %format!("{e:#}"), "detaching after the attached process exited failed; trying again");
                }
            }
        }
        let mut state = inner.state.subscribe();
        drop(inner);
        tokio::select! {
            _ = tokio::time::sleep(ATTACH_PROCESS_POLL) => {}
            _ = state.wait_for(|s| *s != HubState::Running) => return,
        }
    }
}

impl Core {
    /// Whether attaching `thread` (runtime `runtime`) is the first interactive Claude Code
    /// session in the worktree twr made for it ([`AttachGrant::trust_prompt`]): the log has no
    /// `thread.attached` of the thread. When the log cannot be read, `false` (it is a hint).
    fn first_claude_attach(&self, thread: &str, runtime: &str) -> bool {
        runtime == "claude-code"
            && self
                .rt
                .get(thread)
                .is_some_and(|r| matches!(r.workspace, git::Workspace::Worktree { .. }))
            && matches!(
                store::last_of_kind(self.store.conn(), thread, None, "thread.attached"),
                Ok(None)
            )
    }

    /// The grant point of [`Hub::attach_begin`], under the lock that recorded the attach:
    /// `grant` stands when the thread would still get the command made from `cfg` (its mode,
    /// model and effort, session, runtime and the orchestrator's restrictions, unchanged while
    /// the runtime was being stopped). Otherwise the attach is ended again
    /// (`thread.detached`, reason [`CHANGED_WHILE_ATTACHING`]: the lease and its directory go,
    /// and the queued prompts run in the new settings) and refused (`conflict`, try again).
    fn recheck_attach(
        &mut self,
        hub: &Arc<Inner>,
        grant: AttachGrant,
        cfg: &SpawnConfig,
    ) -> Result<AttachGrant, RpcError> {
        let view = self.view(&grant.thread)?.clone();
        let unchanged = view.session_id.as_deref() == Some(grant.session_id.as_str())
            && view.runtime == grant.runtime
            && self
                .attach_config(hub, &grant.thread, &view)
                .is_ok_and(|now| now == *cfg);
        if unchanged {
            return Ok(grant);
        }
        tracing::info!(thread = %grant.thread, "the thread changed while attaching; ending that attach");
        match self.detach(
            hub,
            &grant.thread,
            Source::Hub,
            Some(CHANGED_WHILE_ATTACHING),
        ) {
            Ok(()) => Err(rpc(
                codes::CONFLICT,
                format!("{} changed while attaching; try again", view.name),
            )),
            // The attach stays recorded (with a lease no client holds) until the hub restarts,
            // which ends it: never handed out with the old settings.
            Err(e) => {
                tracing::error!(thread = %grant.thread, error = %format!("{e:#}"), "ending an attach the thread changed under failed");
                Err(rpc(
                    codes::INTERNAL,
                    format!(
                        "{} changed while attaching, and ending that attach failed; see the hub \
                         log",
                        view.name
                    ),
                ))
            }
        }
    }
}

impl Hub {
    /// `attach.begin`: quiesces the thread for `client` and returns the
    /// runtime's interactive command. Refuses (`conflict`) when the thread is already attached
    /// or a turn is running (unless `force`, which interrupts it); `unsupported` when the
    /// runtime has no interactive CLI or the thread has no session yet (also while it moves
    /// to another runtime, `hub::switch`: the previous runtime's session is not its own any
    /// more, and the new one has none until its first turn; refused before anything is
    /// stopped, `force` or not; a move is refused while an attach is in flight, and the grant's
    /// recheck compares the runtime and the session too); `box_offline` for
    /// threads on other boxes. Stops the live runtime and waits for its exit, which closes the
    /// open turn and adapter attention and appends `thread.attached`. The interactive
    /// runtime's directory is held on its box for the lease's life (see `hub::git`, Park);
    /// `conflict` while a worktree change of it is in progress.
    ///
    /// The command is made before the runtime is stopped, from the thread's mode, model and
    /// effort then, so a change of any of them (or of the session or the orchestrator's
    /// restrictions) before the grant refuses the attach (`conflict`, try again): the attach
    /// already recorded is ended again (`thread.detached`, reason `changed_while_attaching`),
    /// and a new attach makes the command anew. A mode or model change is never refused for an
    /// attach in flight. A thread whose session last ran with another model or effort than
    /// the thread's, which its runtime's attach command cannot apply, is refused (`conflict`)
    /// before anything is stopped ([`Core::attach_settings_refusal`]).
    pub async fn attach_begin(
        &self,
        query: &str,
        force: bool,
        client: &str,
    ) -> Result<AttachGrant, RpcError> {
        // A remote box's command needs its runner: asked for before the lease
        // is taken; the checks below run again under the lock, and the command is used only
        // for the thread, session, box and runtime configuration it was made for.
        let mut remote_cmd = self.remote_attach_command(query).await?;
        let (grant, cfg, wait) = {
            let mut core = self.inner.core();
            self.lease_attach(&mut core, query, force, client, &mut remote_cmd)?
        };
        let waited = tokio::time::timeout(ATTACH_QUIESCE_TIMEOUT, wait).await;
        self.attach_outcome(grant, &cfg, waited.is_err())
    }

    /// The locked part of [`Hub::attach_begin`]: checks the thread, builds its interactive
    /// command (`remote_cmd` for a remote box), reserves the directory it works in, takes the
    /// lease, and stops the live runtime (or, with none, records the closing sequence now).
    /// Returns the grant, the runtime configuration its command was made from, and what is
    /// woken once `thread.attached` is appended.
    fn lease_attach(
        &self,
        core: &mut Core,
        query: &str,
        force: bool,
        client: &str,
        remote_cmd: &mut Option<RemoteAttach>,
    ) -> Result<(AttachGrant, SpawnConfig, oneshot::Receiver<()>), RpcError> {
        let (id, view) = core.attachable(query)?;
        let remote = self.inner.has_remote(&view.box_name);
        let AttachCommand {
            host,
            session,
            cfg,
            cmd,
            remote_dir,
        } = self.interactive_command(core, &id, &view, remote, remote_cmd)?;
        let mut grant = AttachGrant::new(&id, &view, &cfg, &session, &cmd)?;
        grant.trust_prompt = core.first_claude_attach(&id, &view.runtime);
        // Where the interactive runtime works, on the thread's box.
        let dir = if remote {
            remote_dir.or_else(|| (!cfg.cwd.as_os_str().is_empty()).then(|| cfg.cwd.clone()))
        } else {
            grant.cwd.as_ref().map(PathBuf::from)
        };
        if remote {
            // The directory is on the box (inside the ssh command), not here.
            grant.cwd = None;
            grant.box_name = Some(view.box_name.clone());
        }
        // Its runtime's own work, that is: a job (`hub::jobs`) runs on beside the attach.
        if !force && view.runtime_tasks() {
            return Err(rpc(codes::CONFLICT, "background work is running; wait for it or use force (stops the runtime and its tasks)"));
        }
        if let Some(turn) = view.current_turn {
            if !force {
                return Err(rpc(
                    codes::CONFLICT,
                    format!(
                        "turn {turn} of {} is running; wait for it or force the attach \
                         (interrupts the turn)",
                        view.name
                    ),
                ));
            }
        }
        let workdir = core.attach_workdir(&host, &id, &view, dir.as_deref(), remote, client)?;
        let (lease, rx) =
            core.take_attach_lease(&self.inner, &id, client, &mut grant, dir, workdir)?;
        Ok((AttachGrant { lease, ..grant }, cfg, rx))
    }

    /// The interactive command of thread `id` (`view`): for a remote box (`remote`) the one
    /// [`Hub::remote_attach_command`] made (`remote_cmd`), only if it was made for this thread,
    /// session, runtime and configuration (a mode, model or effort change while the runner
    /// was asked refuses it, `conflict`); else the local host's, without twr's secrets.
    fn interactive_command(
        &self,
        core: &Core,
        id: &str,
        view: &ThreadView,
        remote: bool,
        remote_cmd: &mut Option<RemoteAttach>,
    ) -> Result<AttachCommand, RpcError> {
        if remote {
            core.require_box(view)?;
        }
        let Some(host) = self.inner.host(&view.box_name) else {
            return Err(rpc(
                codes::BOX_OFFLINE,
                format!("box {:?} is not available", view.box_name),
            ));
        };
        // While a move to another runtime is pending, the thread has no session of it yet,
        // even one the move resumes: its next turn tells that one what happened since
        // (`hub::switch`).
        let Some(session) = view.confirmed_session().map(str::to_string) else {
            return Err(rpc(
                codes::UNSUPPORTED,
                format!(
                    "{} has no runtime session yet; send it a prompt first",
                    view.name
                ),
            ));
        };
        refuse_session_id(view, &session)?;
        let cfg = core.attach_config(&self.inner, id, view)?;
        self.refuse_stale_settings(core, id, view, &cfg, remote)?;
        let (cmd, remote_dir) = if remote {
            remote_cmd
                .take()
                .filter(|r| {
                    r.thread == id
                        && r.box_name == view.box_name
                        && r.session_id == session
                        && r.runtime == view.runtime
                        && r.cfg == cfg
                })
                .map(|r| (r.cmd, r.dir))
                .ok_or_else(|| {
                    rpc(
                        codes::CONFLICT,
                        format!("{} changed while attaching; try again", view.name),
                    )
                })?
        } else {
            let mut cmd = host
                .attach_command(&view.runtime, &cfg, &session)
                .map_err(|e| rpc(codes::UNSUPPORTED, format!("{e:#}")))?
                .ok_or_else(|| {
                    rpc(
                        codes::UNSUPPORTED,
                        format!(
                            "runtime {:?} has no interactive CLI to attach",
                            view.runtime
                        ),
                    )
                })?;
            // None of twr's secrets reaches the user's process.
            twr_adapters::process::scrub_std(&mut cmd);
            (cmd, None)
        };
        Ok(AttachCommand {
            host,
            session,
            cfg,
            cmd,
            remote_dir,
        })
    }

    /// Refuses (`conflict`) to attach thread `id` (`view`) with the command made from `cfg`
    /// while its session runs another model or effort the command cannot apply
    /// ([`Core::attach_settings_refusal`]), as the box that makes the command says: the
    /// thread's own for a local box, the hub's for a remote one (`remote`).
    fn refuse_stale_settings(
        &self,
        core: &Core,
        id: &str,
        view: &ThreadView,
        cfg: &SpawnConfig,
        remote: bool,
    ) -> Result<(), RpcError> {
        let maker = if remote {
            LOCAL_BOX
        } else {
            view.box_name.as_str()
        };
        let applies = self
            .inner
            .host(maker)
            .map(|h| h.attach_applies(&view.runtime, cfg))
            .unwrap_or_default();
        match core.attach_settings_refusal(id, view, cfg, applies) {
            Some(why) => Err(rpc(codes::CONFLICT, why)),
            None => Ok(()),
        }
    }

    /// The end of [`Hub::attach_begin`], once the runtime's exit was waited for (`timed_out`:
    /// not within [`ATTACH_QUIESCE_TIMEOUT`]): the grant when `thread.attached` is appended
    /// under its lease and the thread would still get the command made from `cfg`, else the
    /// lease is given up (a late exit is then an ordinary stop).
    fn attach_outcome(
        &self,
        grant: AttachGrant,
        cfg: &SpawnConfig,
        timed_out: bool,
    ) -> Result<AttachGrant, RpcError> {
        let mut core = self.inner.core();
        // (A thread with an attach lease is never deleted: nothing to recreate for one gone.)
        let Some(rt) = core.rt.get_mut(&grant.thread) else {
            return Err(rpc(
                codes::NOT_FOUND,
                format!("no thread {:?}", grant.thread),
            ));
        };
        let granted = rt
            .attach
            .as_ref()
            .is_some_and(|a| a.lease == grant.lease && a.attached);
        if granted {
            return core.recheck_attach(&self.inner, grant, cfg);
        }
        // The runtime did not exit in time (or its exit could not be recorded): give up the
        // lease. A late exit is then an ordinary stop.
        if rt.attach.as_ref().is_some_and(|a| a.lease == grant.lease) {
            rt.attach = None;
        }
        if let Some(live) = rt.live.as_mut() {
            live.attach = false;
        }
        Err(if timed_out {
            rpc(
                codes::INTERNAL,
                format!(
                    "the runtime did not exit within {}s; it is still being stopped",
                    ATTACH_QUIESCE_TIMEOUT.as_secs()
                ),
            )
        } else {
            rpc(
                codes::INTERNAL,
                "recording the attach failed; see the hub log",
            )
        })
    }

    /// The ssh-wrapped interactive command of a thread on a remote box (`None` for a local
    /// thread, or one without a session yet: `attach_begin` reports those), with the thread,
    /// box, session, runtime and configuration it was made for. The command is the hub's own
    /// adapter's for the runtime on another box (`Adapter::remote_attach_command`, see
    /// [`RemoteBox::attach_command`]).
    ///
    /// [`RemoteBox::attach_command`]: crate::remote::RemoteBox::attach_command
    async fn remote_attach_command(&self, query: &str) -> Result<Option<RemoteAttach>, RpcError> {
        // `_op`: the box's operation, begun with its gate check and held while the runner is
        // asked, so the box is not switched meanwhile (`Core::reserve_box`; the attach lease
        // that follows is checked against the gate again).
        let (rbox, view, cfg, session, local, _op) =
            {
                let core = self.inner.core();
                let id = core.resolve(query)?;
                let view = core.view(&id)?.clone();
                let Some(rbox) = self.inner.remote(&view.box_name) else {
                    return Ok(None);
                };
                core.require_box(&view)?;
                let op = core.reserve_box(&self.inner, &view.box_name)?;
                if let Some(why) = core.rt.get(&id).and_then(|r| {
                    runtime_spec::acp_attach_refusal(&view.name, &view.mode, &r.acp_env)
                }) {
                    return Err(rpc(codes::CONFLICT, why));
                }
                let Some(session) = view.confirmed_session().map(str::to_string) else {
                    return Ok(None);
                };
                refuse_session_id(&view, &session)?;
                let cfg = core.attach_config(&self.inner, &id, &view)?;
                self.refuse_stale_settings(&core, &id, &view, &cfg, true)?;
                let unknown = || {
                    rpc(
                        codes::UNSUPPORTED,
                        format!(
                            "runtime {:?} has no interactive CLI to attach",
                            view.runtime
                        ),
                    )
                };
                let mut local = self
                    .inner
                    .host(LOCAL_BOX)
                    .ok_or_else(unknown)?
                    // Built for the box: nothing the hub knows of its own binaries applies.
                    .remote_attach_command(&view.runtime, &cfg, &session)
                    .map_err(|_| unknown())?
                    .ok_or_else(unknown)?;
                // None of twr's secrets goes along (on the box: `env -u`).
                twr_adapters::process::scrub_std(&mut local);
                (rbox, view, cfg, session, local, op)
            };
        let repo = view.repo.as_deref().map(Path::new);
        match rbox
            .attach_command(&view.runtime, &cfg, &session, &local, repo)
            .await
        {
            Ok(Some((cmd, dir))) => Ok(Some(RemoteAttach {
                thread: view.id.clone(),
                box_name: view.box_name.clone(),
                session_id: session,
                runtime: view.runtime.clone(),
                cfg,
                cmd,
                dir,
            })),
            Ok(None) => Err(rpc(
                codes::UNSUPPORTED,
                format!(
                    "runtime {:?} has no interactive CLI to attach",
                    view.runtime
                ),
            )),
            Err(e) => {
                let code = match e.code.as_str() {
                    codes::BOX_OFFLINE => codes::BOX_OFFLINE,
                    codes::UNSUPPORTED | twr_core::runner::cmd_errors::UNKNOWN_RUNTIME => {
                        codes::UNSUPPORTED
                    }
                    _ => codes::INTERNAL,
                };
                Err(rpc(code, e.message))
            }
        }
    }

    /// `attach.end`: appends `thread.detached` and dispatches the prompts queued while
    /// attached, but only for the current holder of `lease` (the id `attach.begin` returned):
    /// ending another client's attach would hand its session back to the headless runtime
    /// while that client's interactive CLI still runs on it. Returns `Ok(false)` (nothing
    /// ended) when the thread is not attached under `lease`, and `conflict` while that attach
    /// is still starting. `reason` is recorded as `ext.twr.reason`.
    pub fn attach_end(
        &self,
        query: &str,
        client: &str,
        lease: u64,
        reason: Option<&str>,
    ) -> Result<bool, RpcError> {
        let mut core = self.inner.core();
        let id = core.resolve(query)?;
        match core.rt.get(&id).and_then(|r| r.attach.as_ref()) {
            None => return Ok(false),
            Some(a) if a.lease != lease => return Ok(false),
            Some(a) if !a.attached => {
                return Err(rpc(
                    codes::CONFLICT,
                    "the attach is still starting (the runtime is being stopped)",
                ))
            }
            Some(_) => {}
        }
        core.detach(&self.inner, &id, Source::User(client.into()), reason)
            .map_err(append_err)?;
        Ok(true)
    }

    /// `attach.child`: the client of the attach under `lease` runs its interactive process as
    /// `pid` (started at `start_time`, as `twr_runner::reap::process_start_time` reads it).
    /// Checked against the process table (that
    /// process, not a zombie, carrying the lease's [`ATTACH_ENV`] marker: `bad_request`
    /// otherwise) and recorded (`store` `attach_processes`): from now on the attach lasts while
    /// that process runs, even when the lease connection goes away or the hub restarts.
    /// `conflict` when the thread is not attached under `lease`.
    pub async fn attach_child(
        &self,
        query: &str,
        lease: u64,
        pid: u32,
        start_time: u64,
    ) -> Result<(), RpcError> {
        let id = self.inner.core().resolve(query)?;
        let marker = attach_marker(&id, lease);
        let m = marker.clone();
        let running = tokio::task::spawn_blocking(move || {
            twr_runner::reap::attach_process_running(pid, start_time, &m, true)
        })
        .await
        .map_err(internal)?;
        if !running {
            return Err(rpc(
                codes::BAD_REQUEST,
                format!(
                    "process {pid} (start time {start_time}) is not running with {ATTACH_ENV}={marker}"
                ),
            ));
        }
        let mut core = self.inner.core();
        let dir = match core.rt.get(&id).and_then(|r| r.attach.as_ref()) {
            Some(a) if a.lease == lease && a.attached => a.dir.clone(),
            _ => {
                return Err(rpc(
                    codes::CONFLICT,
                    format!("{query} is not attached under this lease"),
                ))
            }
        };
        let child = store::AttachProcess {
            pid,
            start_time,
            marker,
            dir: dir.map(|d| d.to_string_lossy().into_owned()),
        };
        core.store
            .set_attach_process(&id, &child)
            .map_err(append_err)?;
        if let Some(a) = core.rt.get_mut(&id).and_then(|r| r.attach.as_mut()) {
            a.child = Some(child);
        }
        Ok(())
    }

    /// The connection holding `lease` on the thread closed. Without a reported interactive
    /// process the attach ends now (`thread.detached` reason `connection_closed`); with one,
    /// the thread stays attached and the hub watches the process, detaching once it has exited
    /// (the client may have been killed while the user's CLI still runs on the session).
    pub fn attach_connection_closed(
        &self,
        query: &str,
        client: &str,
        lease: u64,
    ) -> Result<(), RpcError> {
        let child = {
            let mut core = self.inner.core();
            let id = core.resolve(query)?;
            match core.rt.get_mut(&id).and_then(|r| r.attach.as_mut()) {
                Some(a) if a.lease == lease && a.attached => match &a.child {
                    Some(c) => {
                        a.orphaned = true;
                        Some((id, c.clone()))
                    }
                    None => None,
                },
                _ => return Ok(()),
            }
        };
        match child {
            Some((id, c)) => {
                tracing::warn!(thread = %id, pid = c.pid, "the attach's connection closed while its process runs; the thread stays attached until that process exits");
                tokio::spawn(watch_attach_process(
                    Arc::downgrade(&self.inner),
                    id,
                    lease,
                    c,
                ));
                Ok(())
            }
            None => self
                .attach_end(query, client, lease, Some("connection_closed"))
                .map(|_| ()),
        }
    }
}

/// Refuses (`bad_request`) to attach thread `view` on session id `session` when the id is not
/// one twr puts on a command line ([`twr_adapters::valid_session_id`]): every runtime's
/// interactive CLI takes it as an argument (`claude --resume <id>`, `codex resume <id>`,
/// `grok --resume <id>`), and an id a runner reported for its thread is the runner's word.
/// `--dangerously-skip-permissions` as an id would be a flag of the user's own process.
fn refuse_session_id(view: &ThreadView, session: &str) -> Result<(), RpcError> {
    if twr_adapters::valid_session_id(session) {
        return Ok(());
    }
    Err(rpc(
        codes::BAD_REQUEST,
        format!(
            "{}'s session id {:?} is not a session id twr passes to a command line (1 to 256 \
             letters, digits, `.`, `_`, `:` or `-`, not starting with `-`); it cannot be \
             attached",
            view.name,
            super::orch::cut(session, 80)
        ),
    ))
}
