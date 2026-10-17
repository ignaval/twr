//! The thread RPCs: `thread.new` (box, project, runtime, mode and name checks, then the
//! thread's workspace before `thread.created`), `thread.list` / `thread.get`, `thread.send` and
//! `thread.stop`.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{json, Value};
use twr_core::config::Config;
use twr_core::event::*;
use twr_core::proto::{codes, RpcError};

use super::lifecycle::data_dir_of;
use super::{
    append_err, box_agents, find_project, git, internal, orch, project_read, rpc, runtime_spec,
    twr_ext_map, Batch, BatchPrompt, Caller, Core, Exit, Hub, Inner, NewThread, Planned,
    SendOutcome, LOCAL_BOX, MAX_ALIASES,
};
use crate::host::ThreadHost;
use crate::remote::BoxOp;

impl Core {
    /// `thread.stop` of thread `id` for `caller` (see [`Hub::stop_thread_by`]).
    pub(super) fn stop_thread(
        &mut self,
        hub: &Inner,
        id: &str,
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        self.stop_thread_with(hub, id, caller, None)
    }

    /// [`Core::stop_thread`], with `why`: the hub's own item saying why it stops the thread,
    /// recorded in the same transaction as the stop (`hub::background`).
    pub(super) fn stop_thread_with(
        &mut self,
        hub: &Inner,
        id: &str,
        caller: &Caller,
        why: Option<EventBody>,
    ) -> Result<Value, RpcError> {
        let id = id.to_string();
        if let Some(a) = self.rt.get(&id).and_then(|r| r.attach.as_ref()) {
            return Err(rpc(
                codes::CONFLICT,
                format!(
                    "the thread is attached by {}; {}",
                    a.client,
                    self.end_attach_hint(&id)
                ),
            ));
        }
        if self
            .rt
            .get(&id)
            .is_some_and(|r| r.park.is_some() || r.resuming)
        {
            return Err(rpc(
                codes::CONFLICT,
                "the thread is being parked or resumed; try again when that is done",
            ));
        }
        // Dropping the queue and recording the stop request are the first half of the stop,
        // committed (one transaction) before the runtime is told to stop: if what follows
        // fails (or the hub dies), the queue is already what the user asked for, the runtime's
        // exit closes the thread as stopped whenever it is recorded (by this hub, the next one
        // at startup, or after taking a remote session over again, which also asks it to stop
        // again), and `thread.stop` can be repeated.
        let (live, exiting) = self
            .rt
            .get(&id)
            .map_or((false, false), |r| (r.live.is_some(), r.exiting));
        let view = self.view(&id)?.clone();
        let box_offline = hub.has_remote(&view.box_name) && !self.box_online(&view.box_name);
        // After a hub restart a remote runtime is taken over only when its box connects
        // (`hub::runners`): until then the log is all the hub knows, and a runtime it shows
        // running may well still run there. Once the box is online, a runtime the hub does
        // not run is gone (reconciliation closed it, or took it over).
        let unadopted = !live && !exiting && box_offline && self.unadopted_remote_runtime(&view);
        if !(live || exiting || unadopted) {
            // No runtime to wait for: the queue drop and the closing sequence (the open turn
            // ended, `thread.stopped`) are one batch, so the answer always says what was
            // dropped, and nothing is dropped unless the thread is stopped.
            let plan = if view.current_turn.is_some() {
                self.closing_plan(&id, Exit::Stopped).map_err(append_err)?
            } else if !view.stopped {
                VecDeque::from([Planned {
                    turn: None,
                    source: caller.source(),
                    body: EventBody::ThreadStopped(SessionPayload {
                        session_id: view.session_id.clone(),
                        ..Default::default()
                    }),
                }])
            } else {
                VecDeque::new()
            };
            let mut batch = Batch {
                drop_queues: vec![id.clone()],
                ..Batch::default()
            };
            for p in plan {
                batch.push(&id, p.turn, p.source, p.body);
            }
            if let Some(why) = why {
                batch.push(&id, None, Source::Hub, why);
            }
            // Prompts held for it after other threads go with its queue (`hub::after`).
            let held = self.stage_held_drops(&mut batch, &id, "the thread was stopped");
            let dropped = self.append_batch(hub, batch).map_err(append_err)?.dropped + held;
            // Its jobs end with it (`hub::jobs`), once the stop is recorded.
            self.cancel_jobs_of(&id, "cancelled: the thread was stopped");
            return Ok(json!({
                "thread": id,
                "thread_name": self.thread_name(&id),
                "stopping": false,
                "dropped_prompts": dropped,
            }));
        }
        // A remote runtime's stop is recorded too (`remote_outbox`): a hub that dies before it
        // reaches the runner, or a box that is away, gets it again at the next connection
        // (the runner runs it once); a runtime taken over then is asked again.
        let remote_stop = if live || unadopted {
            self.remote_stop(hub, &id)
        } else {
            None
        };
        let mut batch = Batch {
            drop_queues: vec![id.clone()],
            stop_requests: vec![id.clone()],
            remote_cmds: remote_stop.iter().cloned().collect(),
            ..Batch::default()
        };
        if let Some(why) = why {
            batch.push(&id, None, Source::Hub, why);
        }
        let held = self.stage_held_drops(&mut batch, &id, "the thread was stopped");
        let dropped = self.append_batch(hub, batch).map_err(append_err)?.dropped + held;
        self.cancel_jobs_of(&id, "cancelled: the thread was stopped");
        // The live session's forwarder issues the stop below under the recorded id. (Not for
        // a runtime not taken over yet: no forwarder would consume the id, and a later
        // session of the thread must not inherit it.)
        if let (Some(c), true) = (&remote_stop, live) {
            if let Some(rbox) = hub.remote(&c.box_name) {
                rbox.expect_stop(&id, &c.cmd_id);
            }
        }
        let rt = self.rt(&id);
        rt.stop_requested = true;
        // A stop wins over a settings restart in progress: the thread stays stopped.
        rt.restart_reason = None;
        if let Some(live) = rt.live.as_mut() {
            live.stop_requested = true;
            let _ = live.handle.stop();
        }
        let mut out = json!({
            "thread": id,
            "thread_name": self.thread_name(&id),
            "stopping": true,
            "dropped_prompts": dropped,
        });
        if box_offline {
            out["box_offline"] = json!(true);
        }
        Ok(out)
    }
}

impl Hub {
    /// `thread.new` by user client `client`.
    pub async fn new_thread(&self, req: NewThread, client: &str) -> Result<Value, RpcError> {
        self.new_thread_by(req, &Caller::user(client)).await
    }

    /// `thread.new` for `caller`: validates, then prepares the thread's workspace (its own
    /// worktree in a git repository, see `hub::git`) before appending `thread.created`; when
    /// that fails nothing is appended. The name and aliases are reserved meanwhile.
    ///
    /// The box is `box`, else the project's `box`, else `local`. A remote box must be
    /// configured (`[boxes.<name>]`) and online: its runner makes the worktree, so a thread on
    /// an offline box is refused (`box_offline`, nothing appended). On a remote box the project
    /// is `[boxes.<name>.projects.<project>]`, else `[projects.<project>]` (its `repo` taken as
    /// a path on that box), else an absolute path on that box. On the hub's own box a plain
    /// name that is none of these is looked up under `project_roots` ([`find_project`]); the
    /// repository found is then the project's absolute path, and the result says so
    /// (`project_found`).
    pub async fn new_thread_by(&self, req: NewThread, caller: &Caller) -> Result<Value, RpcError> {
        let cfg = &self.inner.config;
        // Held until the thread is recorded (or refused): its box is not switched meanwhile.
        let (box_name, host, remote, mut op) = self.thread_box(&req)?;
        let found = self.find_project(&req, remote).await?;
        let target = self.thread_project(&req, &box_name, remote, found.as_deref())?;
        // A twr-managed worktree unless asked otherwise: `no_worktree`, or the project's
        // `worktree` (else `threads.worktree`) off and neither `worktree` nor a `base` (which
        // only a new worktree has) asking for one.
        if req.worktree && req.no_worktree {
            return Err(rpc(
                codes::BAD_REQUEST,
                "give worktree or no_worktree, not both",
            ));
        }
        let no_worktree = req.no_worktree
            || !(req.worktree || req.base.is_some() || cfg.project_worktree(&req.project));
        let spec = self.thread_runtime(&req, caller, target.runtime.as_deref(), &host)?;
        let runtime = spec.runtime.clone();
        let mode = self.thread_mode(&req, caller, &runtime)?;
        let model = thread_model(&req, cfg)?;
        let local = !host.is_remote();
        let effort = thread_effort(&req, &runtime)?;
        if caller.is_orchestrator() && !target.configured {
            self.refuse_orchestrator_dir(&host, &target.repo, remote)
                .await?;
        }

        let (name, aliases, mut reservation) =
            self.reserve_thread_names(&req, &box_name, &target.repo, &target.project, local)?;
        let workspace = git::prepare_workspace(
            &self.inner.reservations,
            &box_name,
            &host,
            target.repo,
            &target.project,
            &name,
            no_worktree,
            req.base.clone(),
        )
        .await;
        let mut core = self.inner.core();
        reservation.release(&mut core);
        let workspace = workspace?;
        if core.shutting_down {
            drop(core);
            workspace.discard(&self.inner, &box_name, &host, op.take());
            return Err(rpc(codes::CONFLICT, "hub is shutting down"));
        }
        // Again with the repository git found (the top level, symlinks resolved), under the
        // lock the thread is recorded with.
        let refused = [&workspace.repo, &workspace.worktree]
            .into_iter()
            .try_for_each(|d| {
                core.refuse_managed_worktree(&box_name, Path::new(d), local)?;
                match local {
                    true => self.refuse_other_workspace(Path::new(d)),
                    false => Ok(()),
                }
            })
            .and_then(|()| {
                // The effort against the `claude` the thread's spawn will run: resolved with
                // its environment in its directory, as the spawn resolves it.
                let env = cfg.thread_env(target.configured.then_some(target.project.as_str()));
                match effort.as_ref().and_then(|_| {
                    runtime_spec::effort_refusal(
                        &runtime,
                        local,
                        &env,
                        Path::new(&workspace.worktree),
                    )
                }) {
                    Some(why) => Err(rpc(codes::BAD_REQUEST, why)),
                    None => Ok(()),
                }
            });
        if let Err(e) = refused {
            drop(core);
            workspace.discard(&self.inner, &box_name, &host, op.take());
            return Err(e);
        }
        let mut twr = workspace.twr.clone();
        if let Some(m) = model {
            twr.insert("model".into(), json!(m));
        }
        if let Some(e) = effort {
            twr.insert("effort".into(), json!(e));
        }
        spec.record(&mut twr);
        // The worktree is made before `thread.created` and removed again if that append
        // fails. They cannot be one transaction (git is not in the log). A hub that dies in
        // between leaves an unused worktree (and its branch) but no thread: nothing was
        // acknowledged or done in it. Accepted residual: a new thread of that name is refused
        // (`conflict`, the path is taken) until that worktree is removed.
        let id = new_id();
        let context_root = target.configured_local.then(|| target.project.clone());
        if let Some(op) = &req.spawn_operation {
            twr.insert("spawn_operation".into(), json!(op));
        }
        if target.configured {
            twr.insert("configured_project".into(), json!(true));
        }
        if let Some(path) = &found {
            twr.insert(
                "project_found".into(),
                json!({"name":req.project, "repo":path.to_string_lossy()}),
            );
        }
        let mut batch = Batch::default();
        batch.push(
            &id,
            None,
            caller.source(),
            EventBody::ThreadCreated(ThreadCreated {
                name,
                project: Some(target.project),
                repo: Some(workspace.repo.clone()),
                worktree: Some(workspace.worktree.clone()),
                box_name: box_name.clone(),
                runtime,
                mode,
                aliases,
                ext: twr_ext_map(Value::Object(twr)),
                ..Default::default()
            }),
        );
        if let Some(prompt) = req.first_prompt.clone() {
            batch.prompts.push(BatchPrompt {
                thread: id.clone(),
                prompt,
                from: caller.from(),
                source: caller.source(),
                routed_by: None,
                said_after: None,
                user_send: !caller.is_orchestrator(),
            });
        }
        let created = core.append_batch(&self.inner, batch);
        if let Err(e) = created {
            drop(core);
            workspace.discard(&self.inner, &box_name, &host, op.take());
            return Err(append_err(e));
        }
        if let Some(project) = context_root {
            core.context_roots
                .insert(project, PathBuf::from(&workspace.repo));
        }
        let view = core.view(&id)?.clone();
        let mut out = core.thread_json(&view);
        // Advisory: the box's last report says its agent CLI is not there (`hub::box_agents`).
        if let Some(w) = box_agents::agent_warning(&self.inner, &box_name, &view.runtime) {
            tracing::info!(thread = %id, box_name = %box_name, "{w}");
            out["warning"] = json!(w);
        }
        if let Ok(done) = created {
            if let Some(row) = done.queued.first() {
                let sent = core.dispatch_queued(&self.inner, &id, *row);
                out["send"] = serde_json::to_value(sent).map_err(internal)?;
            }
        }
        if let Some(path) = found {
            tracing::info!(
                thread = %id,
                project = %req.project,
                repo = %path.display(),
                "found the project by its folder name"
            );
            out["project_found"] = json!({
                "name": req.project,
                "repo": path.to_string_lossy(),
            });
        }
        Ok(out)
    }

    /// Refuses (`bad_request`) the orchestrator a thread in `dir` (on `host`; `remote`: another
    /// box) that is not a configured project: `dir` must be inside a git repository, and that
    /// repository must not be, or hold, the home folder, nor hold or be in twr's data directory,
    /// `TWR_HOME` or a credentials folder of the home folder (the folders the orchestrator's
    /// project reads refuse, [`project_read::protected_dirs`]). Otherwise it could start a
    /// worker in `$HOME` or `/` (a folder outside git is the thread's worktree as it is). On
    /// another box, whose home folder twr does not know, a repository at `/`, at a home folder
    /// by its shape (`/root`, `/home/<user>`, `/Users/<user>`) or in a credentials folder by its
    /// name is refused. The user starts such a thread themselves.
    async fn refuse_orchestrator_dir(
        &self,
        host: &Arc<dyn ThreadHost>,
        dir: &Path,
        remote: bool,
    ) -> Result<(), RpcError> {
        let refuse = |why: String| {
            Err(rpc(
                codes::BAD_REQUEST,
                format!(
                    "{why}: the orchestrator starts threads only in a configured project or a \
                     git repository outside the home folder's private folders; ask the user to \
                     start this thread"
                ),
            ))
        };
        let root = host
            .git()
            .repo_root(dir.to_path_buf())
            .await
            .map_err(|e| rpc(codes::BAD_REQUEST, format!("{e:#}")))?;
        let Some(root) = root else {
            return refuse(format!("{} is not in a git repository", dir.display()));
        };
        let why = if remote {
            project_read::remote_repo_refusal(&root)
        } else {
            let home = std::env::var_os("HOME")
                .filter(|h| !h.is_empty())
                .map(PathBuf::from);
            let protected =
                project_read::protected_dirs(home.as_deref(), &data_dir_of(&self.inner.db_path));
            project_read::repo_refusal(&root, home.as_deref(), &protected)
        };
        match why {
            Some(why) => refuse(why),
            None => Ok(()),
        }
    }

    /// The repository a plain project name stands for on the hub's own box: `None` unless
    /// `project` is a plain folder name ([`find_project::is_plain_name`]) that is not a
    /// configured project, the thread is local, and `project_roots` is not empty. Exactly one
    /// git repository of that name under the roots is the answer; several are `ambiguous`,
    /// none is `not_found`, each saying what to do.
    async fn find_project(
        &self,
        req: &NewThread,
        remote: bool,
    ) -> Result<Option<PathBuf>, RpcError> {
        let cfg = &self.inner.config;
        let name = req.project.clone();
        // A remote box's disk is not the hub's: the name would be looked up in the wrong place.
        if remote || cfg.projects.contains_key(&name) || !find_project::is_plain_name(&name) {
            return Ok(None);
        }
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let roots = cfg.project_roots(home.as_deref());
        if roots.is_empty() {
            return Ok(None);
        }
        let lookup = {
            let (roots, name) = (roots.clone(), name.clone());
            tokio::task::spawn_blocking(move || find_project::find(&roots, &name))
                .await
                .map_err(internal)?
        };
        let list = |paths: &[PathBuf]| {
            paths
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        match lookup.repos.as_slice() {
            [one] if !lookup.truncated => Ok(Some(one.clone())),
            [] => {
                let looked = if lookup.searched.is_empty() {
                    format!("none of the project_roots folders exist ({})", list(&roots))
                } else {
                    format!(
                        "looked in {} and one folder below each{}",
                        list(&lookup.searched),
                        if lookup.truncated {
                            " (stopped early: too many folders)"
                        } else {
                            ""
                        }
                    )
                };
                let not_git = if lookup.not_git.is_empty() {
                    String::new()
                } else {
                    format!(
                        "; not git repositories, so not used: {}",
                        list(&lookup.not_git)
                    )
                };
                Err(rpc(
                    codes::NOT_FOUND,
                    format!(
                        "{name:?} is not a configured project, and no git repository named \
                         {name:?} was found ({looked}{not_git}). Give the repository's \
                         absolute path, or add [projects.{name}] repo = \"/path/to/{name}\" \
                         (or its folder to project_roots) to the twr config"
                    ),
                ))
            }
            [one] => Err(rpc(
                codes::AMBIGUOUS,
                format!(
                    "found the git repository {} for {name:?}, but the search stopped early \
                     (too many folders under {}), so another of that name may exist: ask the \
                     user whether that is the one they mean before opening it by its path",
                    one.display(),
                    list(&lookup.searched)
                ),
            )),
            many => Err(rpc(
                codes::AMBIGUOUS,
                format!(
                    "{name:?} names several git repositories: {}. Give the absolute path of the \
                     one you mean",
                    list(many)
                ),
            )),
        }
    }

    /// The box of a new thread (`box`, else the project's `box`, else `local`) with its host,
    /// whether it is a remote one, and for one the operation on it begun with its gate check
    /// ([`Core::reserve_box`]: the box is not switched while the thread's workspace is being
    /// made): `box_offline` when it is not configured, its runner is not connected, or it is
    /// being switched.
    #[allow(clippy::type_complexity)]
    fn thread_box(
        &self,
        req: &NewThread,
    ) -> Result<(String, Arc<dyn ThreadHost>, bool, Option<BoxOp>), RpcError> {
        let cfg = &self.inner.config;
        let box_name = req
            .box_name
            .clone()
            .or_else(|| {
                cfg.projects
                    .get(&req.project)
                    .and_then(|p| p.box_name.clone())
            })
            .unwrap_or_else(|| LOCAL_BOX.into());
        let Some(host) = self.inner.host(&box_name) else {
            return Err(rpc(
                codes::BOX_OFFLINE,
                format!("no box {box_name:?} is configured ([boxes.{box_name}])"),
            ));
        };
        let remote = self.inner.remote(&box_name);
        if remote.as_ref().is_some_and(|r| !r.is_ready()) {
            return Err(rpc(
                codes::BOX_OFFLINE,
                format!("box {box_name:?} is offline (its runner is not connected)"),
            ));
        }
        let op = match remote {
            Some(_) => self.inner.core().reserve_box(&self.inner, &box_name)?,
            None => None,
        };
        Ok((box_name, host, remote.is_some(), op))
    }

    /// The project of a new thread on box `box_name` (`remote`: a remote box): its name, its
    /// repository, and its configured runtime. A configured project of the box, else of the
    /// hub, else an absolute path (checked on the remote box by its runner; here it must be
    /// a directory, and is canonicalized): `found` ([`Hub::find_project`]) when given, taken
    /// exactly like an absolute path.
    fn thread_project(
        &self,
        req: &NewThread,
        box_name: &str,
        remote: bool,
        found: Option<&Path>,
    ) -> Result<ThreadProject, RpcError> {
        let cfg = &self.inner.config;
        if req.project.trim().is_empty() {
            return Err(rpc(codes::BAD_REQUEST, "project is required"));
        }
        // The box's configuration as it was added (a box added live is not in `cfg`).
        let box_cfg = remote.then(|| self.inner.box_config(box_name)).flatten();
        let box_project = box_cfg.as_ref().and_then(|b| b.projects.get(&req.project));
        // A configured project on the hub's own box: the root its workspace resolves to is where
        // its standing context lives (`hub::context`).
        let configured_local =
            box_project.is_none() && !remote && cfg.projects.contains_key(&req.project);
        let configured = box_project.is_some() || cfg.projects.contains_key(&req.project);
        let (project, repo, runtime) = match (box_project, cfg.projects.get(&req.project)) {
            (Some(bp), p) => (
                req.project.clone(),
                bp.repo.clone(),
                bp.runtime
                    .clone()
                    .or_else(|| p.and_then(|p| p.runtime.clone())),
            ),
            (None, Some(p)) => (req.project.clone(), p.repo.clone(), p.runtime.clone()),
            (None, None) if remote => {
                // A path on the remote box: checked there (the runner's `repo_root`).
                let path = PathBuf::from(&req.project);
                if !path.is_absolute() {
                    return Err(rpc(
                        codes::NOT_FOUND,
                        format!(
                            "{:?} is neither a project configured for box {box_name:?} \
                             ([boxes.{box_name}.projects.<name>] or [projects.<name>]) nor an \
                             absolute path on it (folder names are looked up only on the hub's \
                             own machine)",
                            req.project
                        ),
                    ));
                }
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "root".into());
                (name, path, None)
            }
            (None, None) => {
                let path = found.map_or_else(|| PathBuf::from(&req.project), Path::to_path_buf);
                if !path.is_absolute() {
                    let off = if cfg.project_roots.as_ref().is_some_and(Vec::is_empty)
                        && find_project::is_plain_name(&req.project)
                    {
                        " (project_roots is empty: folder names are not looked up)"
                    } else {
                        ""
                    };
                    return Err(rpc(
                        codes::NOT_FOUND,
                        format!(
                            "{:?} is neither a configured project nor an absolute path{off}",
                            req.project
                        ),
                    ));
                }
                let path = std::fs::canonicalize(&path)
                    .map_err(|e| rpc(codes::NOT_FOUND, format!("{}: {e}", path.display())))?;
                if !path.is_dir() {
                    return Err(rpc(
                        codes::BAD_REQUEST,
                        format!("{} is not a directory", path.display()),
                    ));
                }
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "root".into());
                (name, path, None)
            }
        };
        Ok(ThreadProject {
            project,
            repo,
            runtime,
            configured,
            configured_local,
        })
    }

    /// The runtime of a new thread: `runtime`, else the project's (`project_runtime`), else
    /// `default_runtime`, resolved ([`runtime_spec::resolve`]) and one `host` has. The
    /// orchestrator may only pick what [`orch::check_spawn_runtime`] allows.
    fn thread_runtime(
        &self,
        req: &NewThread,
        caller: &Caller,
        project_runtime: Option<&str>,
        host: &Arc<dyn ThreadHost>,
    ) -> Result<runtime_spec::RuntimeSpec, RpcError> {
        let cfg = &self.inner.config;
        let runtime = req
            .runtime
            .clone()
            .or(project_runtime.map(str::to_string))
            .unwrap_or_else(|| cfg.default_runtime.clone());
        if caller.is_orchestrator() {
            let configured = project_runtime.unwrap_or(&cfg.default_runtime);
            orch::check_spawn_runtime(req.runtime.as_deref(), configured, cfg)?;
        }
        let spec = runtime_spec::resolve(&runtime, cfg)?;
        let runtime = spec.runtime.clone();
        if !host.has_runtime(&runtime) {
            return Err(rpc(
                codes::BAD_REQUEST,
                format!("unknown runtime {runtime:?}"),
            ));
        }
        Ok(spec)
    }

    /// The permission mode of a new thread on `runtime`, in its canonical spelling: `mode`,
    /// else `threads.default_mode`. The orchestrator may only use the default.
    fn thread_mode(
        &self,
        req: &NewThread,
        caller: &Caller,
        runtime: &str,
    ) -> Result<String, RpcError> {
        let default_mode = &self.inner.config.threads.default_mode;
        // Canonical spelling, or refused (`runtime_spec::canonical_mode`).
        let req_mode = match &req.mode {
            Some(m) => Some(
                runtime_spec::canonical_mode(m, Some(runtime))
                    .map_err(|e| rpc(codes::BAD_REQUEST, e))?,
            ),
            None => None,
        };
        // The permission mode decides what a worker may do without asking the user: it is the
        // user's setting, never the orchestrator's (it could otherwise spawn threads that
        // skip the relay rule).
        if caller.is_orchestrator() && req_mode.as_ref().is_some_and(|m| m != default_mode) {
            return Err(rpc(
                codes::BAD_REQUEST,
                format!(
                    "the thread's permission mode is the user's setting: the orchestrator's \
                     threads use threads.default_mode ({default_mode:?}); omit mode"
                ),
            ));
        }
        Ok(match req_mode {
            Some(m) => m,
            // Canonical since the hub opened; a runtime-specific id must be this runtime's.
            None => runtime_spec::canonical_mode(default_mode, Some(runtime))
                .map_err(|e| rpc(codes::BAD_REQUEST, format!("threads.default_mode: {e}")))?,
        })
    }

    /// The name (`name`, else `<project slug>-<n>`) and aliases of a new thread, checked and
    /// reserved together while its workspace is made. Refused while the hub shuts down, and
    /// for a `repo` inside a twr-managed worktree.
    fn reserve_thread_names(
        &self,
        req: &NewThread,
        box_name: &str,
        repo: &Path,
        project: &str,
        local: bool,
    ) -> Result<(String, Vec<String>, git::NameReservation), RpcError> {
        let mut core = self.inner.core();
        if core.shutting_down {
            return Err(rpc(codes::CONFLICT, "hub is shutting down"));
        }
        core.refuse_managed_worktree(box_name, repo, local)?;
        if local {
            self.refuse_other_workspace(repo)?;
        }
        let name = match &req.name {
            Some(n) => {
                validate_thread_name(n)?;
                if core.name_taken(n) {
                    return Err(rpc(codes::CONFLICT, format!("name {n:?} is taken")));
                }
                n.clone()
            }
            // A name from the first prompt (`thread.spawn`: the orchestrator should give one,
            // this is the fallback), else `<project slug>-<n>`.
            None => match req.first_prompt.as_deref().and_then(prompt_name) {
                Some(base) => std::iter::once(base.clone())
                    .chain((2..).map(|i| format!("{base}-{i}")))
                    .find(|n| !core.name_taken(n))
                    .expect("unbounded range"),
                None => {
                    let base = slug(project);
                    (1..)
                        .map(|i| format!("{base}-{i}"))
                        .find(|n| !core.name_taken(n))
                        .expect("unbounded range")
                }
            },
        };
        if req.aliases.len() > MAX_ALIASES {
            return Err(rpc(
                codes::BAD_REQUEST,
                format!(
                    "{} aliases given; at most {MAX_ALIASES} are allowed",
                    req.aliases.len()
                ),
            ));
        }
        let mut aliases = Vec::new();
        for a in &req.aliases {
            validate_name(a)?;
            let same = |b: &String| super::same_name(a, b);
            if same(&name) || aliases.iter().any(same) || core.name_taken(a) {
                return Err(rpc(codes::CONFLICT, format!("alias {a:?} is taken")));
            }
            aliases.push(a.clone());
        }
        // The name and every alias are reserved together while the worktree is made
        // (outside the lock); released together by the caller, or by the guard if the request
        // is dropped.
        let mut names = vec![name.clone()];
        names.extend(aliases.iter().cloned());
        let reservation = git::NameReservation::new(&self.inner, &mut core, names);
        Ok((name, aliases, reservation))
    }

    /// `thread.list`: every thread (oldest first) with its derived `status`.
    pub fn list_threads(&self) -> Vec<Value> {
        let core = self.inner.core();
        core.registry
            .list()
            .into_iter()
            .map(|v| core.thread_json(v))
            .collect()
    }

    /// `thread.activity`: what each listed thread is doing now (`hub::activity`), by id.
    pub fn thread_activity(&self) -> Value {
        self.inner.core().activity_json()
    }

    /// `thread.get`: the thread plus its open attention items.
    pub fn get_thread(&self, query: &str) -> Result<Value, RpcError> {
        let core = self.inner.core();
        let id = core.resolve(query)?;
        let view = core.view(&id)?;
        let mut j = core.thread_json(view);
        let items = core.attention.for_thread(&id, &HashSet::new());
        j["attention"] = serde_json::to_value(items).map_err(internal)?;
        Ok(j)
    }

    /// `thread.send`: queues `prompt` and dispatches it if the thread is free.
    pub fn send(
        &self,
        query: &str,
        prompt: &str,
        from: TurnFrom,
        source: Source,
    ) -> Result<SendOutcome, RpcError> {
        let mut core = self.inner.core();
        let id = core.resolve(query)?;
        core.queue_prompt(&self.inner, &id, prompt, from, source, None)
    }

    /// `thread.send` for `caller`: `from` and the source follow the caller's identity (a user
    /// client can never start an orchestrator turn, and the orchestrator never a user one).
    /// The orchestrator cannot send to itself.
    pub fn send_by(
        &self,
        query: &str,
        prompt: &str,
        caller: &Caller,
    ) -> Result<SendOutcome, RpcError> {
        let mut core = self.inner.core();
        let id = core.resolve(query)?;
        if caller.is_orchestrator() && core.orch.thread.as_deref() == Some(id.as_str()) {
            return Err(rpc(
                codes::BAD_REQUEST,
                "the orchestrator cannot send prompts to itself",
            ));
        }
        core.queue_prompt(
            &self.inner,
            &id,
            prompt,
            caller.from(),
            caller.source(),
            None,
        )
    }

    /// `thread.send {after}` for `caller`: `prompt` goes to thread `query` once each of
    /// `after` has finished its next turn (`hub::after`).
    pub fn send_after_by(
        &self,
        query: &str,
        prompt: &str,
        after: &[String],
        caller: &Caller,
    ) -> Result<Value, RpcError> {
        let mut core = self.inner.core();
        let id = core.resolve(query)?;
        if caller.is_orchestrator() && core.orch.thread.as_deref() == Some(id.as_str()) {
            return Err(rpc(
                codes::BAD_REQUEST,
                "the orchestrator cannot send prompts to itself",
            ));
        }
        core.send_after(&self.inner, &id, prompt, after, caller)
    }

    /// `thread.send_cancel {id}`: drops a prompt queued after other threads.
    pub fn cancel_send_after(&self, id: &str) -> Result<Value, RpcError> {
        self.inner.core().cancel_gated(&self.inner, id)
    }

    /// `thread.stop`: stops the runtime (the supervisor appends the closing events) and drops
    /// queued prompts.
    pub fn stop_thread(&self, query: &str, client: &str) -> Result<Value, RpcError> {
        self.stop_thread_by(query, &Caller::user(client))
    }

    /// `thread.stop` for `caller`. A runtime on a remote box that is offline (live, or one the
    /// log shows running that no connection has handed over yet, after a hub restart) is
    /// stopped durably: the request is recorded (with its `stop` command) and the answer is
    /// `stopping` with `box_offline`; the stop goes out when the box reconnects, and the
    /// thread closes as stopped when the runtime's exit arrives (or reconciliation finds it
    /// gone). Nothing is closed before that.
    pub fn stop_thread_by(&self, query: &str, caller: &Caller) -> Result<Value, RpcError> {
        let mut core = self.inner.core();
        let id = core.resolve(query)?;
        core.stop_thread(&self.inner, &id, caller)
    }
}

/// Where a new thread works ([`Hub::thread_project`]).
struct ThreadProject {
    /// The project's name (`thread.created` `project`).
    project: String,
    /// Its repository, on the thread's box.
    repo: PathBuf,
    /// Its configured runtime.
    runtime: Option<String>,
    /// A configured project (`[boxes.<box>.projects.<name>]` or `[projects.<name>]`), not a
    /// repository path whose folder name `project` is (`ext.twr.configured_project`).
    configured: bool,
    /// A configured project on the hub's own box: its context root is recorded
    /// (`hub::context`).
    configured_local: bool,
}

/// The model of a new thread: `model`, else `threads.model` (blank: the runtime's default).
/// At most 128 bytes, and no control characters (it is shown in thread lists).
fn thread_model(req: &NewThread, cfg: &Config) -> Result<Option<String>, RpcError> {
    let model = req
        .model
        .clone()
        .or_else(|| cfg.threads.model.clone())
        .filter(|m| !m.trim().is_empty());
    // Shown in thread lists: no terminal control characters.
    if let Some(e) = model.as_deref().and_then(runtime_spec::model_error) {
        return Err(rpc(codes::BAD_REQUEST, e));
    }
    Ok(model)
}

/// The reasoning effort of a new thread on `runtime`: `effort` canonicalized
/// (`runtime_spec::canonical_effort`; unset, blank or `default`: the runtime's default). Once
/// its directory is made, `thread.new` refuses it for a Claude Code thread on the hub's own box
/// whose `claude` is known not to take one (`runtime_spec::effort_refusal`).
fn thread_effort(req: &NewThread, runtime: &str) -> Result<Option<String>, RpcError> {
    let Some(effort) = &req.effort else {
        return Ok(None);
    };
    runtime_spec::canonical_effort(effort, runtime).map_err(|e| rpc(codes::BAD_REQUEST, e))
}

impl Hub {
    /// `thread.new` in local directory `dir`: refused (`conflict`) when `dir` is in another
    /// workspace's worktrees ([`HubOptions::workspaces_root`]): that workspace's hub removes
    /// and moves them with their threads, and knows nothing of this one's. Compared as written
    /// and resolved with the filesystem's semantics, like [`Core::refuse_managed_worktree`].
    ///
    /// [`HubOptions::workspaces_root`]: crate::HubOptions::workspaces_root
    fn refuse_other_workspace(&self, dir: &Path) -> Result<(), RpcError> {
        let Some(root) = &self.inner.workspaces_root else {
            return Ok(());
        };
        let resolve = |p: &Path| twr_core::paths::resolve_lenient(p);
        let own = resolve(&super::lifecycle::data_dir_of(&self.inner.db_path).join("worktrees"));
        let mut roots = vec![(
            twr_core::paths::DEFAULT_WORKSPACE.to_string(),
            root.join("worktrees"),
        )];
        if let Ok(entries) = std::fs::read_dir(root.join("workspaces")) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                roots.push((name, e.path().join("worktrees")));
            }
        }
        let resolved = resolve(dir);
        let inside = |r: &Path| {
            dir.starts_with(r)
                || match (&resolved, resolve(r)) {
                    (Some(d), Some(r)) => d.starts_with(r),
                    _ => false,
                }
        };
        let other = roots
            .into_iter()
            .filter(|(_, r)| own.is_none() || resolve(r) != own)
            .find(|(_, r)| inside(r));
        match other {
            Some((name, _)) => Err(rpc(
                codes::CONFLICT,
                format!(
                    "{} is in the worktrees of workspace {name}, whose hub removes and moves \
                     them with their threads: start the thread in the repository instead",
                    dir.display()
                ),
            )),
            None => Ok(()),
        }
    }
}

/// A thread name: [`validate_name`] and a valid git branch component (it names the branch
/// `twr/<name>` of the thread's worktree).
pub(super) fn validate_thread_name(n: &str) -> Result<(), RpcError> {
    validate_name(n)?;
    match twr_core::git::branch_component_error(n) {
        None => Ok(()),
        Some(why) => Err(rpc(
            codes::BAD_REQUEST,
            format!("invalid name {n:?}: it names the git branch twr/{n}, and it {why}"),
        )),
    }
}

/// A new thread name or alias: 1-64 of `[A-Za-z0-9._-]`, starting and ending with a letter or
/// digit, and not digits and punctuation only unless digits only (`1.2` and `2-3` read like
/// options or numbers). The relay guard reads a quote's words without the punctuation at their
/// ends ("allow @api." is `api`), so a name ending with it (`api.`) could be named by a quote
/// meaning another thread. Checked only when a thread is created: an old log's
/// names still load (a quote's word that could mean two threads is refused).
pub(super) fn validate_name(n: &str) -> Result<(), RpcError> {
    let chars_ok = !n.is_empty()
        && n.len() <= 64
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    let ends_ok = n.starts_with(|c: char| c.is_ascii_alphanumeric())
        && n.ends_with(|c: char| c.is_ascii_alphanumeric());
    let numeric_ok =
        n.chars().any(|c| c.is_ascii_alphabetic()) || n.chars().all(|c| c.is_ascii_digit());
    if chars_ok && ends_ok && numeric_ok {
        Ok(())
    } else {
        Err(rpc(
            codes::BAD_REQUEST,
            format!(
                "invalid name {n:?}: use 1-64 of [A-Za-z0-9._-], starting and ending with a \
                 letter or digit, with a letter unless it is digits only"
            ),
        ))
    }
}

/// Most words of the first prompt a name made from it takes ([`prompt_name`]).
const PROMPT_NAME_WORDS: usize = 3;

/// Longest name made from a first prompt, in bytes (before a `-<n>` that makes it unique).
const PROMPT_NAME_MAX: usize = 32;

/// Words [`prompt_name`] leaves out: articles, pronouns, prepositions, courtesy and filler
/// (English, and a few Spanish ones), which say nothing of the task.
const PROMPT_FILLER: &[&str] = &[
    "a", "about", "after", "al", "all", "also", "an", "and", "any", "are", "as", "at", "be",
    "before", "both", "by", "can", "con", "could", "de", "del", "do", "does", "el", "en", "es",
    "for", "from", "go", "have", "hey", "hi", "how", "i", "if", "in", "into", "is", "it", "its",
    "just", "la", "las", "let", "lets", "los", "me", "my", "need", "now", "of", "on", "or", "our",
    "para", "please", "por", "que", "se", "should", "so", "some", "su", "that", "the", "then",
    "there", "these", "this", "those", "to", "un", "una", "up", "us", "we", "what", "when",
    "which", "why", "will", "with", "would", "y", "you", "your",
];

/// A short kebab-case name describing a thread's task from its first prompt, for a
/// `thread.spawn` that gives none: the first [`PROMPT_NAME_WORDS`] words of the prompt that
/// are not [`PROMPT_FILLER`] (ASCII letters and digits only, lowercased, at most 16 bytes
/// each), joined with `-`, at most [`PROMPT_NAME_MAX`] bytes ("Design the notifications
/// system" is `design-notifications-system`). `None` when that is no valid thread name.
fn prompt_name(prompt: &str) -> Option<String> {
    let mut name = String::new();
    let mut words = 0;
    for w in prompt.split(|c: char| !c.is_alphanumeric()) {
        if w.is_empty() || w.len() > 16 || !w.is_ascii() {
            continue;
        }
        let w = w.to_ascii_lowercase();
        // A lone letter is no word ("don't" splits into "don" and "t").
        if PROMPT_FILLER.contains(&w.as_str())
            || (w.len() == 1 && !w.as_bytes()[0].is_ascii_digit())
        {
            continue;
        }
        let len = name.len() + usize::from(!name.is_empty()) + w.len();
        if len > PROMPT_NAME_MAX {
            break;
        }
        if !name.is_empty() {
            name.push('-');
        }
        name.push_str(&w);
        words += 1;
        if words == PROMPT_NAME_WORDS {
            break;
        }
    }
    validate_thread_name(&name).ok().map(|()| name)
}

/// Lowercase slug of a project name for generated thread names.
fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_matches('-');
    let out = if out.len() > 24 { &out[..24] } else { out };
    let out = out.trim_matches('-');
    if out.is_empty() {
        "thread".into()
    } else {
        out.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_and_names() {
        assert_eq!(slug("My Project!"), "my-project");
        assert_eq!(slug("..."), "thread");
        assert_eq!(slug("a".repeat(40).as_str()).len(), 24);
        assert!(validate_name("api-1").is_ok());
        assert!(validate_name("a.b_c").is_ok());
        assert!(validate_name("").is_err());
        assert!(validate_name("-x").is_err());
        assert!(validate_name("has space").is_err());
        // Punctuation at either end, or digits and punctuation only.
        for bad in [
            "api.", "api..", ".api", "api-", "-api", "api_", "_api", "api:", "1.2", "2-3", "1_0",
            ".", "..", "-", "_", "2.",
        ] {
            assert!(validate_name(bad).is_err(), "{bad:?}");
            assert!(validate_thread_name(bad).is_err(), "{bad:?}");
        }
        for good in ["2", "12", "api.v2", "svc-a", "a_b", "t1", "v1.2", "x-1"] {
            assert!(validate_name(good).is_ok(), "{good:?}");
        }
        assert!(validate_thread_name("api-1").is_ok());
        assert!(validate_thread_name("a.b_c").is_ok());
        for bad in [".x", "x.", "x.lock", "a..b", "-x", ""] {
            assert!(validate_thread_name(bad).is_err(), "{bad:?}");
        }
    }

    /// A thread spawned without a name is named after its first prompt's task.
    #[test]
    fn names_from_first_prompts() {
        for (prompt, name) in [
            (
                "Design the notifications system for the webapp",
                Some("design-notifications-system"),
            ),
            ("Review the last 3 commits", Some("review-last-3")),
            ("please fix the flaky CI job", Some("fix-flaky-ci")),
            ("Don't touch main; rebase it", Some("don-touch-main")),
            ("Añade un test de integración", Some("test")),
            (
                "Investigate deadlock01234567890123456789 in scheduler",
                Some("investigate-scheduler"),
            ),
            (
                "refactor authentication middleware internals",
                Some("refactor-authentication"),
            ),
            ("!sleep 50\ndone", Some("sleep-50-done")),
            ("the a of to", None),
            ("1 2 3", None),
            ("¿Qué pasó?", None),
            ("", None),
        ] {
            assert_eq!(prompt_name(prompt).as_deref(), name, "{prompt:?}");
        }
    }
}
