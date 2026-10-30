//! Worktrees, park / resume and git snapshots. All git work goes through the
//! thread's box ([`ThreadHost::git`](crate::host::ThreadHost::git)), never the hub's own
//! process, so a runner box does the same work for its threads.
//!
//! **Workspaces.** `thread.new` decides where a worker thread works before `thread.created` is
//! appended (nothing is appended when that fails) and records it in `thread.created`
//! `ext.twr`:
//!
//! | case                                   | `repo`        | `worktree`                              | `ext.twr` |
//! |----------------------------------------|---------------|-----------------------------------------|-----------|
//! | git repository (default)               | its top level | `<data>/worktrees/<project>/<name>` on a new branch `twr/<name>` from `base` (default `HEAD`) | `worktree: "twr", branch, base, base_commit` |
//! | git repository, `no_worktree`, or worktrees off for the project | its top level | the project directory | `worktree: "repo"` |
//! | not a git repository                   | the directory | the directory                           | `worktree: "repo", git: false, note` |
//!
//! Worktrees are on by default; `[threads] worktree = false` turns them off, `[projects.<name>]
//! worktree` overrides that per project, and `thread.new` decides last (`worktree: true` or a
//! `base` asks for one, `no_worktree` for none). A thread without a worktree of its own works
//! in the repository checkout: parking never removes it, snapshots and attach work the same,
//! and parallel threads of one repository share that checkout (and its branch).
//!
//! The orchestrator never has a worktree. Thread names are valid git branch components. A
//! hub-managed worktree belongs to its thread: `thread.new` in one (with or without
//! `no_worktree`, also below it) is refused (`conflict`), since parking its thread removes it.
//!
//! **Park** (`thread.park {thread, keep?, force?}`) mirrors attach: refused while
//! attached (or attaching), already parked, being parked or resumed, and while a turn or
//! background tasks run unless `force` (the turn is interrupted, the tasks end with the
//! runtime). The live runtime is stopped and its exit awaited;
//! the exit runs the closing sequence (open turn `interrupted`, adapter attention resolved) and
//! appends `thread.parked{session_id, ext.twr.keep}` instead of `thread.stopped`. Then, unless
//! `keep`, a hub-managed worktree is removed if it is clean (never forced; its branch is
//! kept; the repository's stale worktree records are pruned) and kept when dirty
//! (`kept_dirty`), and kept (`kept_reason`) while a runtime of another thread works in it,
//! attached ones included: an attach holds its directory on its box from before it is
//! granted until it ends (a local box's reservation, `twr_runner::workdirs`; for a remote
//! box, whose runner cannot see an attach, the hub's own check, which also refuses an attach
//! into a worktree being parked or resumed). Queued prompts stay queued, `thread.send` to a
//! parked thread queues and says `parked: true`, and nothing is dispatched until
//! `thread.resume`.
//!
//! **Resume** (`thread.resume {thread}`): recreates a missing hub-managed worktree at the same
//! path from its branch (runtimes such as Claude Code key sessions by directory; a removal of
//! it still running, e.g. of a park whose request went away, is waited for first, bounded, and
//! no runtime starts in a worktree being removed: the box's working-directory reservations,
//! `twr_runner::workdirs`), appends
//! `thread.resumed{session_id}` (source: the caller; it starts no process) and dispatches the
//! queue, which resumes the runtime's session (`adapter.resume(session_id)`).
//!
//! **Snapshots.** After an adapter ends a turn, and on a tick (30 s by default) for threads
//! that are not parked or stopped, the hub runs `git status --porcelain=v2 --branch` in the
//! thread's directory and appends `git.snapshot{branch, dirty_files, ahead, behind}` (source
//! hub, no turn) only when one of those changed since the thread's last snapshot. Failures are
//! logged at debug level; non-git threads and the orchestrator are skipped.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};
use std::time::Duration;

use serde::Serialize;
use serde_json::{json, Map, Value};
use tokio::sync::oneshot;
use twr_core::event::*;
use twr_core::git::{snapshot_changed, thread_branch, RemoveOutcome, WorktreeSpec};
use twr_core::paths::resolve_lenient;
use twr_core::proto::{codes, RpcError};

use super::delete::Reservations;
use super::{
    append_err, internal, rpc, twr_ext, twr_ext_map, Caller, Core, Exit, Hub, HubState, Inner,
    Live, RuntimeExiting, ThreadRt, ATTACH_QUIESCE_TIMEOUT,
};
use crate::host::ThreadHost;

/// Default period of the `git.snapshot` tick.
pub const GIT_TICK: Duration = Duration::from_secs(30);

/// Where a thread works, from its `thread.created` `ext.twr`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
pub(super) enum Workspace {
    /// Nothing recorded (threads created before workspaces were recorded, the orchestrator):
    /// the recorded directory, not managed by the hub.
    #[default]
    Unknown,
    /// A worktree the hub created, on `branch` (`existing_branch`: the branch was there
    /// before the thread, `ext.twr.existing_branch`).
    Worktree {
        branch: String,
        existing_branch: bool,
    },
    /// The repository itself (`no_worktree`).
    Repo,
    /// Not a git repository: no snapshots.
    NotGit,
}

impl Workspace {
    pub(super) fn from_created(p: &ThreadCreated) -> Workspace {
        let Some(twr) = twr_ext(&p.ext) else {
            return Workspace::Unknown;
        };
        match twr.get("worktree").and_then(Value::as_str) {
            Some("twr") => Workspace::Worktree {
                branch: twr
                    .get("branch")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| thread_branch(&p.name)),
                existing_branch: twr.get("existing_branch") == Some(&Value::Bool(true)),
            },
            Some("repo") if twr.get("git") == Some(&Value::Bool(false)) => Workspace::NotGit,
            Some("repo") => Workspace::Repo,
            _ => Workspace::Unknown,
        }
    }
}

/// A `thread.park` in progress.
pub(super) struct ParkLease {
    /// Distinguishes this park from a later one (shares the attach lease counter).
    id: u64,
    /// Who parks (`thread.parked` source).
    source: Source,
    /// `keep`: the worktree stays.
    keep: bool,
    /// Woken once the runtime's exit is recorded.
    waiter: Option<oneshot::Sender<()>>,
    /// `thread.parked` is appended.
    parked: bool,
}

/// Result of `thread.park`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ParkOutcome {
    /// Thread id.
    pub thread: String,
    /// Thread name.
    pub thread_name: String,
    /// Always true.
    pub parked: bool,
    /// The worktree path, when the hub manages one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    /// The worktree was removed.
    pub worktree_removed: bool,
    /// The worktree was kept because it has uncommitted changes or untracked files.
    pub kept_dirty: bool,
    /// Dirty files of a kept worktree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dirty_files: Option<u64>,
    /// Why a clean worktree was kept: its checkout is not the thread's branch (a detached
    /// `HEAD` or another branch; removing it would lose what only its reflog remembers).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kept_reason: Option<String>,
    /// The worktree was kept on request (`keep`).
    pub kept: bool,
    /// There was no worktree at the path any more.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub worktree_missing: bool,
    /// Removing the worktree failed (the thread is parked anyway; the worktree stays).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree_error: Option<String>,
    /// Prompts waiting for `thread.resume`.
    pub pending: u64,
}

/// Result of `thread.resume`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ResumeOutcome {
    /// Thread id.
    pub thread: String,
    /// Thread name.
    pub thread_name: String,
    /// Always true.
    pub resumed: bool,
    /// The worktree path, when the hub manages one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    /// The worktree was missing and was recreated from its branch.
    pub worktree_recreated: bool,
    /// The turn a queued prompt started, if one did.
    pub turn: Option<u64>,
    /// Prompts still queued.
    pub pending: u64,
}

/// A thread's new workspace (see the module table), ready for `thread.created`.
pub(super) struct NewWorkspace {
    pub(super) repo: String,
    pub(super) worktree: String,
    /// `ext.twr` keys.
    pub(super) twr: Map<String, Value>,
    /// The worktree the hub created (repo, path, branch), removed again if the thread is not
    /// created.
    created: Option<(PathBuf, PathBuf, String)>,
}

impl NewWorkspace {
    /// The thread was not created after all: removes the worktree made for it (best effort,
    /// in the background, holding its repository like any change that removes something of
    /// it, `delete::Reservations`; its branch stays). `op`, the box's operation the creation
    /// began with its gate check (`Core::reserve_box`), is held until that removal is over:
    /// the box is not switched under it.
    pub(super) fn discard(
        self,
        hub: &Arc<Inner>,
        box_name: &str,
        host: &Arc<dyn ThreadHost>,
        op: Option<crate::remote::BoxOp>,
    ) {
        if let Some((repo, path, branch)) = self.created {
            let (hub, box_name, git) = (Arc::clone(hub), box_name.to_string(), host.git());
            tokio::spawn(async move {
                let _op = op;
                let held = hub
                    .reservations
                    .reserve_repository(&box_name, git.as_ref(), &repo)
                    .await;
                let removed = held
                    .during(git.worktree_remove(repo.clone(), path.clone(), Some(branch)))
                    .await;
                if let Err(e) = removed {
                    tracing::warn!(path = %path.display(), error = %format!("{e:#}"), "cannot remove the worktree of a thread that was not created");
                }
            });
        }
    }
}

/// The code of a failure that is the box's, not the request's: its runner is not connected
/// (`box_offline`, try again once it is), or it restarted while running the command
/// (`internal`: the effect is unknown). `None` for a failure of the operation itself.
fn box_code(e: &anyhow::Error) -> Option<&'static str> {
    match e
        .downcast_ref::<twr_core::runner::CmdError>()
        .map(|c| c.code.as_str())
    {
        Some(codes::BOX_OFFLINE) => Some(codes::BOX_OFFLINE),
        Some(twr_core::runner::cmd_errors::INTERRUPTED) => Some(codes::INTERNAL),
        _ => None,
    }
}

/// Decides and prepares the workspace of new thread `name` of `project` in `dir` on box
/// `box_name` (`host`; see the module table): creates its worktree unless `no_worktree` or
/// `dir` is not in a git repository, holding its repository meanwhile (`reservations`).
#[allow(clippy::too_many_arguments)]
pub(super) async fn prepare_workspace(
    reservations: &Arc<Reservations>,
    box_name: &str,
    host: &Arc<dyn ThreadHost>,
    dir: PathBuf,
    project: &str,
    name: &str,
    no_worktree: bool,
    base: Option<String>,
) -> Result<NewWorkspace, RpcError> {
    let git = host.git();
    let text = |p: &PathBuf| p.to_string_lossy().into_owned();
    let root = git
        .repo_root(dir.clone())
        .await
        .map_err(|e| rpc(box_code(&e).unwrap_or(codes::BAD_REQUEST), format!("{e:#}")))?;
    let Some(root) = root else {
        if base.is_some() {
            return Err(rpc(
                codes::BAD_REQUEST,
                format!(
                    "{} is not a git repository: a base only applies to a worktree",
                    dir.display()
                ),
            ));
        }
        let mut twr = Map::new();
        twr.insert("worktree".into(), json!("repo"));
        twr.insert("git".into(), json!(false));
        twr.insert(
            "note".into(),
            json!("not a git repository: the thread works in the project directory"),
        );
        return Ok(NewWorkspace {
            repo: text(&dir),
            worktree: text(&dir),
            twr,
            created: None,
        });
    };
    if no_worktree {
        if base.is_some() {
            return Err(rpc(
                codes::BAD_REQUEST,
                "a base only applies to a new worktree (not with no_worktree)",
            ));
        }
        let mut twr = Map::new();
        twr.insert("worktree".into(), json!("repo"));
        return Ok(NewWorkspace {
            repo: text(&root),
            worktree: text(&dir),
            twr,
            created: None,
        });
    }
    let path = host.worktree_path(project, name);
    let branch = host.worktree_branch(name);
    // Creating it clears a stale record at its path when that loses nothing: holding the
    // repository like any change that removes something of it.
    let held = reservations
        .reserve_repository(box_name, git.as_ref(), &root)
        .await;
    let made = held
        .during(git.worktree_create(WorktreeSpec {
            repo: root.clone(),
            path: path.clone(),
            branch: branch.clone(),
            base: base.clone(),
        }))
        .await
        .map_err(|e| {
            let msg = format!("cannot create the worktree {}: {e:#}", path.display());
            if let Some(code) = box_code(&e) {
                return rpc(code, msg);
            }
            let e = e.to_string();
            let code = if e.contains("not a commit") || e.contains("invalid") {
                codes::BAD_REQUEST
            } else {
                codes::CONFLICT
            };
            rpc(code, msg)
        })?;
    drop(held);
    let mut twr = Map::new();
    twr.insert("worktree".into(), json!("twr"));
    twr.insert("branch".into(), json!(branch));
    twr.insert("base".into(), json!(base.as_deref().unwrap_or("HEAD")));
    twr.insert("base_commit".into(), json!(made.head));
    if let Some(b) = &made.base_ref {
        twr.insert("base_ref".into(), json!(b));
    }
    if !made.new_branch {
        twr.insert("existing_branch".into(), json!(true));
    }
    Ok(NewWorkspace {
        repo: text(&root),
        worktree: text(&path),
        twr,
        created: Some((root, path, branch)),
    })
}

/// A thread's name and aliases, reserved together while `thread.new` prepares its
/// workspace (outside the core lock): no other thread can take any of them meanwhile. Released
/// together with [`NameReservation::release`], or when dropped (the request went away).
pub(super) struct NameReservation {
    hub: Arc<Inner>,
    names: Vec<String>,
}

impl NameReservation {
    /// Reserves `names` (the caller holds the core lock and has checked they are free).
    pub(super) fn new(hub: &Arc<Inner>, core: &mut Core, names: Vec<String>) -> Self {
        for n in &names {
            core.reserved_names.insert(n.clone());
        }
        NameReservation {
            hub: Arc::clone(hub),
            names,
        }
    }

    /// Releases every reserved name (the caller holds the core lock).
    pub(super) fn release(&mut self, core: &mut Core) {
        for n in self.names.drain(..) {
            core.reserved_names.remove(&n);
        }
    }
}

impl Drop for NameReservation {
    fn drop(&mut self) {
        if !self.names.is_empty() {
            let mut core = self.hub.core();
            for n in self.names.drain(..) {
                core.reserved_names.remove(&n);
            }
        }
    }
}

/// Clears a park or resume that did not finish normally (its request future was dropped).
struct Pending {
    hub: Arc<Inner>,
    thread: String,
    /// `Some(lease id)` for a park, `None` for a resume.
    park: Option<u64>,
    armed: bool,
}

impl Pending {
    /// Done: nothing to clear. Call with the core lock held, before clearing the state.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut core = self.hub.core();
        let Some(rt) = core.rt.get_mut(&self.thread) else {
            return;
        };
        match self.park {
            Some(id) => {
                if rt.park.as_ref().is_some_and(|p| p.id == id) {
                    rt.park = None;
                }
            }
            None => rt.resuming = false,
        }
    }
}

impl Core {
    /// The park exit of `live`, when it was stopped for a `thread.park` still waiting for it.
    pub(super) fn park_exit(&self, thread: &str, live: &Live) -> Option<Exit> {
        if !live.park {
            return None;
        }
        let p = self.rt.get(thread)?.park.as_ref().filter(|p| !p.parked)?;
        Some(Exit::Park {
            source: p.source.clone(),
            keep: p.keep,
        })
    }

    /// The runtime quiesced for a park has exited and its closing sequence ran (`closed`):
    /// marks the park parked (else drops it) and wakes `thread.park`.
    pub(super) fn park_quiesced(&mut self, thread: &str, closed: bool) {
        let Some(rt) = self.rt.get_mut(thread) else {
            return;
        };
        let Some(p) = rt.park.as_mut() else {
            return;
        };
        if let Some(w) = p.waiter.take() {
            let _ = w.send(());
        }
        if closed {
            p.parked = true;
        } else {
            rt.park = None;
        }
    }

    /// The hub-managed worktree of thread `thread`, when it has one: its box and path.
    pub(super) fn managed_worktree(&self, thread: &str) -> Option<(&str, &Path)> {
        let rt = self.rt.get(thread)?;
        if !matches!(rt.workspace, Workspace::Worktree { .. }) {
            return None;
        }
        let view = self.registry.get(thread)?;
        Some((view.box_name.as_str(), Path::new(view.worktree.as_deref()?)))
    }

    /// The name of the thread whose hub-managed worktree on box `box_name` is `dir` or
    /// contains it. twr's worktrees belong to their thread: another thread working in one
    /// would lose its directory when that thread is parked (the worktree removed). On the
    /// hub's own box (`local`) both are also compared resolved with the filesystem's semantics
    /// (`alias/..` is the parent of alias's target), and a path that cannot be resolved counts
    /// as inside (fail closed).
    pub(super) fn worktree_owner(&self, box_name: &str, dir: &Path, local: bool) -> Option<String> {
        let resolved_dir = local.then(|| resolve_lenient(dir));
        self.rt.keys().find_map(|t| {
            let (b, wt) = self.managed_worktree(t)?;
            if b != box_name {
                return None;
            }
            let inside = dir.starts_with(wt)
                || match &resolved_dir {
                    None => false,
                    Some(d) => match (d, resolve_lenient(wt)) {
                        (Some(d), Some(wt)) => d.starts_with(wt),
                        _ => true,
                    },
                };
            inside
                .then(|| self.registry.get(t).map(|v| v.name.clone()))
                .flatten()
        })
    }

    /// `thread.new` in `dir` on box `box_name`: refused (`conflict`) when `dir` is (in) another
    /// thread's hub-managed worktree ([`Core::worktree_owner`]).
    pub(super) fn refuse_managed_worktree(
        &self,
        box_name: &str,
        dir: &Path,
        local: bool,
    ) -> Result<(), RpcError> {
        match self.worktree_owner(box_name, dir, local) {
            Some(owner) => Err(rpc(
                codes::CONFLICT,
                format!(
                    "{} is in the worktree twr manages for thread {owner}; that worktree \
                     belongs to {owner} (it is removed when {owner} is parked): start the \
                     thread in the repository instead",
                    dir.display()
                ),
            )),
            None => Ok(()),
        }
    }

    /// The name of a thread on box `box_name` whose hub-managed worktree is being parked
    /// (possibly removed), resumed (possibly recreated) or deleted (`hub::delete`, from the
    /// confirmation until the thread is gone or its deletion failed) and is `dir`, contains it
    /// or lies in it (compared as written: a remote box's paths cannot be resolved here).
    pub(super) fn worktree_change_over(&self, box_name: &str, dir: &Path) -> Option<String> {
        self.rt
            .iter()
            .filter(|(_, rt)| rt.park.is_some() || rt.resuming || rt.deleting || rt.trimming)
            .find_map(|(t, _)| {
                let (b, wt) = self.managed_worktree(t)?;
                (b == box_name && (dir.starts_with(wt) || wt.starts_with(dir)))
                    .then(|| self.registry.get(t).map(|v| v.name.clone()))
                    .flatten()
            })
    }

    /// A thread on box `box_name` attached (or being attached) with its directory at `path`
    /// or inside it, other than `except`: its name and client. Guards a remote box's worktree
    /// removal ([`AttachLease::dir`](super::AttachLease)); a local box knows its attaches
    /// itself.
    pub(super) fn attach_inside(
        &self,
        box_name: &str,
        path: &Path,
        except: &str,
    ) -> Option<(String, String)> {
        self.rt.iter().find_map(|(t, rt)| {
            let a = rt.attach.as_ref()?;
            let view = self.registry.get(t)?;
            let inside = a.dir.as_deref().is_some_and(|d| d.starts_with(path));
            (t != except && view.box_name == box_name && inside)
                .then(|| (view.name.clone(), a.client.clone()))
        })
    }

    /// Whether the hub snapshots `thread` now, and in which directory.
    fn snapshot_dir(&self, thread: &str) -> Option<PathBuf> {
        if self.shutting_down {
            return None;
        }
        let view = self.registry.get(thread)?;
        if view.parked || view.stopped || view.is_orchestrator() {
            return None;
        }
        let rt = self.rt.get(thread);
        if rt.is_some_and(|r| r.park.is_some() || r.workspace == Workspace::NotGit) {
            return None;
        }
        view.worktree.as_ref().map(PathBuf::from)
    }
}

impl Hub {
    /// `thread.park` (see the module docs).
    pub async fn park_thread(
        &self,
        query: &str,
        keep: bool,
        force: bool,
        caller: &Caller,
    ) -> Result<ParkOutcome, RpcError> {
        self.park_thread_as(query, keep, force, caller, false).await
    }

    /// [`Hub::park_thread`]; `deleting`: for the thread's deletion (`hub::delete`), the only
    /// park a thread being deleted takes.
    pub(super) async fn park_thread_as(
        &self,
        query: &str,
        keep: bool,
        force: bool,
        caller: &Caller,
        deleting: bool,
    ) -> Result<ParkOutcome, RpcError> {
        let hub = &self.inner;
        let (id, lease, wait) = {
            let mut core = hub.core();
            if core.shutting_down {
                return Err(rpc(codes::CONFLICT, "hub is shutting down"));
            }
            let id = core.resolve(query)?;
            if !deleting {
                core.refuse_deleting(&id)?;
            }
            let view = core.view(&id)?.clone();
            let name = &view.name;
            // The runtime and the worktree are on the box.
            core.require_box(&view)?;
            if view.parked {
                return Err(rpc(codes::CONFLICT, format!("{name} is already parked")));
            }
            if let Some(c) = &view.attached {
                return Err(rpc(
                    codes::CONFLICT,
                    format!("{name} is attached by {c}; {}", core.end_attach_hint(&id)),
                ));
            }
            let rt = core.rt.get(&id);
            if let Some(a) = rt.and_then(|r| r.attach.as_ref()) {
                return Err(rpc(
                    codes::CONFLICT,
                    format!("{name} is being attached by {}", a.client),
                ));
            }
            if rt.is_some_and(|r| r.park.is_some()) {
                return Err(rpc(codes::CONFLICT, format!("{name} is being parked")));
            }
            if rt.is_some_and(|r| r.resuming) {
                return Err(rpc(codes::CONFLICT, format!("{name} is being resumed")));
            }
            if rt.is_some_and(|r| r.trimming) {
                return Err(rpc(
                    codes::CONFLICT,
                    format!("{name}'s build output is being trimmed; try again when that is done"),
                ));
            }
            // A runtime the hub gave up on: its exit records the crash first (and only then
            // may its worktree go).
            if rt.is_some_and(ThreadRt::giving_up) {
                return Err(rpc(codes::CONFLICT, RuntimeExiting(id).to_string()));
            }
            if !force && !view.background_tasks.is_empty() {
                return Err(rpc(codes::CONFLICT, "background work is running; wait for it or use force (stops the runtime and its tasks)"));
            }
            if let Some(turn) = view.current_turn {
                if !force {
                    return Err(rpc(
                        codes::CONFLICT,
                        format!(
                            "turn {turn} of {name} is running; wait for it or force the park \
                             (interrupts the turn)"
                        ),
                    ));
                }
            }
            core.leases += 1;
            let lease = core.leases;
            let (tx, rx) = oneshot::channel();
            core.rt(&id).park = Some(ParkLease {
                id: lease,
                source: caller.source(),
                keep,
                waiter: Some(tx),
                parked: false,
            });
            // Its jobs end too (only a forced park gets here with any): the lease now keeps
            // new ones from starting (`hub::jobs`).
            core.cancel_jobs_of(&id, "cancelled: the thread was parked");
            let stopping = match core.rt.get_mut(&id).and_then(|r| r.live.as_mut()) {
                Some(live) => {
                    live.stop_requested = true;
                    live.park = true;
                    let _ = live.handle.stop();
                    true
                }
                None => false,
            };
            if !stopping {
                // No runtime process: park right away (closing a turn the hub lost the
                // process of, if one is open).
                let closed = core.close_process(
                    hub,
                    &id,
                    Exit::Park {
                        source: caller.source(),
                        keep,
                    },
                );
                core.park_quiesced(&id, closed.is_ok());
                closed.map_err(append_err)?;
            }
            (id, lease, rx)
        };
        let mut pending = Pending {
            hub: Arc::clone(hub),
            thread: id.clone(),
            park: Some(lease),
            armed: true,
        };
        let waited = tokio::time::timeout(ATTACH_QUIESCE_TIMEOUT, wait).await;
        // And its jobs, whose processes work in the worktree. Bounded: one still running holds
        // the worktree's directory on its box, which then keeps it (`twr_runner::workdirs`).
        let deadline = tokio::time::Instant::now() + super::jobs::KILL_AFTER * 3;
        while hub.core().has_jobs(&id) && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let (view, host, workspace, attached_inside) = {
            let mut core = hub.core();
            // (A thread being parked is never deleted: nothing to recreate for one gone.)
            let Some(rt) = core.rt.get_mut(&id) else {
                pending.disarm();
                return Err(rpc(codes::NOT_FOUND, format!("no thread {id:?}")));
            };
            let parked = rt.park.as_ref().is_some_and(|p| p.id == lease && p.parked);
            if !parked {
                // The runtime did not exit in time (or its exit could not be recorded): give
                // up the park. A late exit is then an ordinary stop.
                pending.disarm();
                if rt.park.as_ref().is_some_and(|p| p.id == lease) {
                    rt.park = None;
                }
                if let Some(live) = rt.live.as_mut() {
                    live.park = false;
                }
                return Err(match waited {
                    Err(_) => rpc(
                        codes::INTERNAL,
                        format!(
                            "the runtime did not exit within {}s; it is still being stopped",
                            ATTACH_QUIESCE_TIMEOUT.as_secs()
                        ),
                    ),
                    Ok(_) => rpc(
                        codes::INTERNAL,
                        "recording the park failed; see the hub log",
                    ),
                });
            }
            let workspace = rt.workspace.clone();
            let view = core.view(&id)?.clone();
            let host = hub.host(&view.box_name);
            // A remote box does not know about attaches (their command runs over ssh): the
            // hub keeps a worktree an attached thread works in. Checked under the lock that
            // grants attaches, which refuse one inside a worktree being parked
            // (`Core::worktree_change_over`), so none can begin before the removal is over.
            // A local box refuses the removal itself (the attach holds a use of its
            // directory, `twr_runner::workdirs`).
            let attached_inside = match (&host, &view.worktree) {
                (Some(h), Some(wt)) if h.is_remote() => {
                    core.attach_inside(&view.box_name, Path::new(wt), &id)
                }
                _ => None,
            };
            (view, host, workspace, attached_inside)
        };
        // `thread.parked` is recorded (with the rest of the closing sequence, one batch); the
        // worktree is removed after it and only reported. A crash in between
        // leaves a parked thread whose worktree is still there, which resume handles.
        let mut out = ParkOutcome {
            thread: id.clone(),
            thread_name: view.name.clone(),
            parked: true,
            worktree: None,
            worktree_removed: false,
            kept_dirty: false,
            dirty_files: None,
            kept_reason: None,
            kept: false,
            worktree_missing: false,
            worktree_error: None,
            pending: 0,
        };
        if let (Workspace::Worktree { branch, .. }, Some(repo), Some(path)) =
            (&workspace, &view.repo, &view.worktree)
        {
            out.worktree = Some(path.clone());
            if keep {
                out.kept = true;
            } else if let Some((name, client)) = attached_inside {
                out.kept_reason = Some(format!(
                    "thread {name} is attached (by {client}) and works in it on this box"
                ));
            } else {
                let removed = match &host {
                    Some(h) => {
                        // Holding its repository from the removal's checks through the removal
                        // (`delete::Reservations`): a deletion there runs before (the checks
                        // then see what it removed) or after, never in between.
                        // Held until the box is done, even if this request goes away first.
                        let git = h.git();
                        let held = hub
                            .reservations
                            .reserve_repository(&view.box_name, git.as_ref(), Path::new(repo))
                            .await;
                        held.during(git.worktree_remove(
                            PathBuf::from(repo),
                            PathBuf::from(path),
                            Some(branch.clone()),
                        ))
                        .await
                    }
                    None => Err(anyhow::anyhow!("box {:?} is not available", view.box_name)),
                };
                match removed {
                    Ok(RemoveOutcome::Removed) => out.worktree_removed = true,
                    Ok(RemoveOutcome::Missing) => out.worktree_missing = true,
                    Ok(RemoveOutcome::KeptDirty { dirty_files }) => {
                        out.kept_dirty = true;
                        out.dirty_files = Some(dirty_files);
                    }
                    Ok(RemoveOutcome::KeptCheckout { head }) => {
                        out.kept_reason = Some(format!(
                            "checkout differs from thread branch {branch} ({head})"
                        ));
                    }
                    // Hidden or ignored files, a repository in a tracked directory, or commits
                    // only the worktree's own record has.
                    Ok(RemoveOutcome::Kept { reason }) => out.kept_reason = Some(reason),
                    Err(e) => {
                        tracing::warn!(thread = %id, error = %format!("{e:#}"), "cannot remove the parked thread's worktree");
                        out.worktree_error = Some(format!("{e:#}"));
                    }
                }
            }
        }
        let mut core = hub.core();
        pending.disarm();
        if let Some(rt) = core.rt.get_mut(&id) {
            if rt.park.as_ref().is_some_and(|p| p.id == lease) {
                rt.park = None;
            }
        }
        out.pending = core.store.pending_count(&id).unwrap_or(0);
        Ok(out)
    }

    /// `thread.resume` (see the module docs).
    pub async fn resume_thread(
        &self,
        query: &str,
        caller: &Caller,
    ) -> Result<ResumeOutcome, RpcError> {
        let hub = &self.inner;
        let (id, view, workspace, host) = {
            let mut core = hub.core();
            if core.shutting_down {
                return Err(rpc(codes::CONFLICT, "hub is shutting down"));
            }
            let id = core.resolve(query)?;
            let view = core.view(&id)?.clone();
            core.refuse_retired(&id)?;
            core.refuse_deleting(&id)?;
            // The worktree is on the box.
            core.require_box(&view)?;
            let rt = core.rt(&id);
            if rt.park.is_some() {
                return Err(rpc(
                    codes::CONFLICT,
                    format!("{} is being parked", view.name),
                ));
            }
            if rt.resuming {
                return Err(rpc(
                    codes::CONFLICT,
                    format!("{} is already being resumed", view.name),
                ));
            }
            if !view.parked {
                return Err(rpc(codes::CONFLICT, format!("{} is not parked", view.name)));
            }
            if rt.trimming {
                return Err(rpc(
                    codes::CONFLICT,
                    format!(
                        "{}'s build output is being trimmed; try again when that is done",
                        view.name
                    ),
                ));
            }
            rt.resuming = true;
            let workspace = rt.workspace.clone();
            let host = hub.host(&view.box_name);
            (id, view, workspace, host)
        };
        let mut pending = Pending {
            hub: Arc::clone(hub),
            thread: id.clone(),
            park: None,
            armed: true,
        };
        let mut recreated = false;
        let mut worktree = None;
        if let (Workspace::Worktree { branch, .. }, Some(repo), Some(path)) =
            (&workspace, &view.repo, &view.worktree)
        {
            worktree = Some(path.clone());
            let recreate = async {
                let host = host
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("box {:?} is not available", view.box_name))?;
                let git = host.git();
                if git.worktree_exists(PathBuf::from(path)).await? {
                    return Ok(false);
                }
                // Creating it clears a stale record at its path when that loses nothing:
                // holding the repository like any change that removes something of it.
                let held = hub
                    .reservations
                    .reserve_repository(&view.box_name, git.as_ref(), Path::new(repo))
                    .await;
                held.during(git.worktree_create(WorktreeSpec {
                    repo: PathBuf::from(repo),
                    path: PathBuf::from(path),
                    branch: branch.clone(),
                    base: None,
                }))
                .await?;
                anyhow::Ok(true)
            };
            match recreate.await {
                Ok(r) => recreated = r,
                Err(e) => {
                    return Err(rpc(
                        codes::INTERNAL,
                        format!(
                            "cannot recreate the worktree {path} from {branch}: {e:#}; {} stays \
                             parked",
                            view.name
                        ),
                    ));
                }
            }
        }
        let mut core = hub.core();
        pending.disarm();
        if let Some(rt) = core.rt.get_mut(&id) {
            rt.resuming = false;
        }
        if core.shutting_down {
            return Err(rpc(codes::CONFLICT, "hub is shutting down"));
        }
        let session_id = core.view(&id)?.session_id.clone();
        // The worktree (live git state) comes back before `thread.resumed`: a
        // failure or crash in between leaves the thread parked with its worktree present, and
        // resuming again finds it (`worktree_exists`). The queue is dispatched after the append
        // (and on every later send or hub start).
        core.append(
            hub,
            &id,
            None,
            caller.source(),
            EventBody::ThreadResumed(SessionPayload {
                session_id,
                ext: twr_ext_map(json!({ "worktree_recreated": recreated })),
                ..Default::default()
            }),
        )
        .map_err(append_err)?;
        // The user's resume releases a queue a failure held (`ThreadRt::queue_held`); the
        // orchestrator's does not (nor does it in the log: `hub::append`).
        if !caller.is_orchestrator() {
            core.release_queue(&id);
        }
        let turn = core.try_dispatch(hub, &id).map_err(internal)?;
        let pending_prompts = core.store.pending_count(&id).unwrap_or(0);
        Ok(ResumeOutcome {
            thread: id,
            thread_name: view.name.clone(),
            resumed: true,
            worktree,
            worktree_recreated: recreated,
            turn,
            pending: pending_prompts,
        })
    }
}

/// How long a snapshot waits for the one in progress (a git that hangs until its timeout):
/// after that it is skipped (the next turn end or tick takes one), so snapshots never pile up.
const SNAPSHOT_WAIT: Duration = Duration::from_secs(5);

/// Snapshots `thread` (see the module docs): appends `git.snapshot` when it changed.
pub(super) async fn snapshot(hub: &Arc<Inner>, thread: &str) {
    let Ok(_serial) = tokio::time::timeout(SNAPSHOT_WAIT, hub.snapshots.lock()).await else {
        tracing::debug!(
            thread,
            "skipping a git snapshot: the previous one is still running"
        );
        return;
    };
    let (dir, git, base) = {
        let core = hub.core();
        let Some(dir) = core.snapshot_dir(thread) else {
            return;
        };
        let base = core.readiness_base(hub, thread);
        let Some(host) = core
            .registry
            .get(thread)
            .and_then(|v| hub.host(&v.box_name))
        else {
            return;
        };
        (dir, host.git(), base)
    };
    let snap = match git.snapshot(dir.clone(), base.clone()).await {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!(thread, dir = %dir.display(), error = %format!("{e:#}"), "no git snapshot");
            return;
        }
    };
    let mut core = hub.core();
    // Parked or stopped meanwhile: the snapshot describes a directory it no longer uses.
    if core.snapshot_dir(thread).is_none() {
        return;
    }
    let old = core.registry.get(thread).and_then(|v| v.git.as_ref());
    // A readiness that could not be read this time is unknown: the last one was read at
    // commits that may have moved since, and nothing here says which. (The probe keeps its
    // own results by the commits they were read at, for a while: one whose commits did not
    // move is served from there, [`twr_runner::local::LocalGit`].)
    if !snapshot_changed(old, &snap) {
        return;
    }
    if let Err(e) = core.append(hub, thread, None, Source::Hub, EventBody::GitSnapshot(snap)) {
        tracing::warn!(thread, error = %format!("{e:#}"), "cannot record a git snapshot");
    }
}

impl Core {
    /// The ref thread `id`'s work tree is compared with for merging (merge readiness on its
    /// card): its project's `[projects.<name>] base`, else what its worktree was made from
    /// ([`ThreadView::base`]). `None` for a thread working in the repository's own checkout.
    pub(super) fn readiness_base(&self, hub: &Inner, id: &str) -> Option<String> {
        let v = self.registry.get(id)?;
        if !matches!(
            self.rt.get(id).map(|r| &r.workspace),
            Some(Workspace::Worktree { .. })
        ) {
            return None;
        }
        v.project
            .as_ref()
            .filter(|_| v.configured_project)
            .and_then(|p| hub.config.projects.get(p))
            .and_then(|p| p.base.clone())
            .or_else(|| v.base.clone())
    }
}

/// Snapshots `thread` in the background (after a turn ended).
pub(super) fn snapshot_soon(hub: &Arc<Inner>, thread: &str) {
    let (hub, thread) = (Arc::clone(hub), thread.to_string());
    tokio::spawn(async move { snapshot(&hub, &thread).await });
}

/// The snapshot tick: every `period` until the hub stops or is dropped, snapshots every
/// thread that qualifies (one at a time).
pub(super) async fn git_ticker(hub: Weak<Inner>, period: Duration) {
    let mut tick = tokio::time::interval(period);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let Some(h) = hub.upgrade() else {
            return;
        };
        if *h.state.borrow() != HubState::Running {
            return;
        }
        let threads: Vec<String> = {
            let core = h.core();
            core.registry
                .list()
                .into_iter()
                .filter(|v| core.snapshot_dir(&v.id).is_some())
                .map(|v| v.id.clone())
                .collect()
        };
        for thread in threads {
            snapshot(&h, &thread).await;
        }
    }
}
