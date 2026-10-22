//! Where a thread's runtime process lives.
//!
//! The hub never spawns adapters directly; it asks the [`ThreadHost`] for the thread's box:
//! [`LocalHost`] for box `local`, [`crate::remote::RemoteBox`] for a box whose runner connects
//! to the hub, which forwards the same calls to that runner.
//!
//! Git: each host exposes its box's [`GitOps`] (worktrees, snapshots) and where
//! the box keeps thread worktrees ([`ThreadHost::worktree_path`]); the local box uses
//! [`twr_runner::local::LocalGit`], the same implementation a remote runner runs.
//!
//! Stale processes: the hub puts [`THREAD_ENV`]`=<thread id>` and
//! [`RUNTIME_GEN_ENV`]`=<runtime generation>` in every runtime's environment and records the
//! process's pid and start time (`ext.twr.pid`, `ext.twr.start_time` on `thread.started` /
//! `thread.resumed`). After a hub crash, [`ThreadHost::reap_stale`] kills a survivor only when
//! all of them still match ([`twr_runner::reap`], shared with runners, which do the same for
//! their own box). A local runtime that never reported its process (the hub died while it
//! started) is found by its marker and start time instead ([`ThreadHost::reap_marked`], the
//! hub's `spawn_intents`). A kill of a live hub's runtime (given up, vanished adapter, stuck at
//! shutdown) matches its generation too, so it never hits the thread's next runtime.
//!
//! Agents: every runtime also gets [`AGENT_ENV`]`=<thread
//! id>:<runtime generation>`, which tells the hub's socket that a connection comes from a
//! thread's agent (`hub::agents`) also where the connecting process's ancestry no longer shows
//! it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::future::{BoxFuture, FutureExt};

use twr_core::adapter::{Adapter, AdapterHandle, AttachApplies, SpawnConfig};
use twr_core::git::GitOps;
use twr_runner::local::LocalGit;
use twr_runner::reap;
pub use twr_runner::reap::{
    argv0_matches, environ_has, is_group_member, is_marked, is_stale_runtime, parse_stat, ProcStat,
    StaleProcess, AGENT_ENV, ATTACH_ENV, RUNTIME_GEN_ENV, THREAD_ENV,
};
use twr_runner::workdirs::WorkdirUse;

/// Looks an adapter up by runtime name.
pub type AdapterLookup = Arc<dyn Fn(&str) -> Option<Arc<dyn Adapter>> + Send + Sync>;

/// The built-in adapter registry as an [`AdapterLookup`].
pub fn builtin_adapters() -> AdapterLookup {
    Arc::new(twr_adapters::find)
}

/// A box that can run thread runtimes.
pub trait ThreadHost: Send + Sync {
    /// Whether `runtime` can run here.
    fn has_runtime(&self, runtime: &str) -> bool;

    /// Starts a new session; with `prompt`, its first turn is `cfg.prompt_turn`.
    fn spawn(
        &self,
        runtime: &str,
        cfg: SpawnConfig,
        prompt: Option<String>,
    ) -> anyhow::Result<AdapterHandle>;

    /// Resumes session `session_id` (no turn starts until the hub sends one).
    fn resume(
        &self,
        runtime: &str,
        cfg: SpawnConfig,
        session_id: String,
    ) -> anyhow::Result<AdapterHandle>;

    /// The command that opens `runtime`'s interactive CLI on `session_id` (`twr attach`), or
    /// `None` when the runtime has none.
    fn attach_command(
        &self,
        runtime: &str,
        cfg: &SpawnConfig,
        session_id: &str,
    ) -> anyhow::Result<Option<std::process::Command>>;

    /// The command that opens `runtime`'s interactive CLI on `session_id` on another box (the
    /// hub's attach to a thread on a runner's box: `Adapter::remote_attach_command`, never
    /// fitted to this box's binaries), or `None` when the runtime has none.
    fn remote_attach_command(
        &self,
        runtime: &str,
        cfg: &SpawnConfig,
        session_id: &str,
    ) -> anyhow::Result<Option<std::process::Command>>;

    /// The box's git (worktrees and snapshots of repositories on this box).
    fn git(&self) -> Arc<dyn GitOps>;

    /// A runtime of thread `holder` is about to start in `dir`: its use of the directory
    /// ([`twr_runner::workdirs`]), which the caller keeps for as long as the runtime lives, so
    /// that no worktree change of this box removes it underneath. An error (the directory's
    /// worktree is being removed or created, e.g. by a park whose caller went away: git runs on
    /// to its end) means the runtime must not start. `None`: nothing to hold (a remote box's
    /// runner holds its runtimes' uses itself).
    fn use_workdir(&self, _dir: &Path, _holder: &str) -> anyhow::Result<Option<WorkdirUse>> {
        Ok(None)
    }

    /// Where this box keeps the worktree of thread `name` of `project`:
    /// `<worktrees>/<project>/<name>`, the project made a safe path component.
    fn worktree_path(&self, project: &str, name: &str) -> PathBuf;

    /// Branch for a new worktree. Local isolated hubs namespace it; remote boxes use
    /// their own worktree layout and the standard branch name.
    fn worktree_branch(&self, name: &str) -> String {
        twr_core::git::thread_branch(name)
    }

    /// Start time of process `pid` ([`twr_runner::reap::process_start_time`]), recorded so
    /// that a later [`ThreadHost::reap_stale`] can tell the process from a reused pid.
    fn process_start_time(&self, pid: u32) -> Option<u64>;

    /// Kills a runtime process that survived a hub crash and waits (bounded)
    /// for it to exit. Only a process whose start time and [`THREAD_ENV`] marker match `p` is
    /// touched. Then kills the surviving members of its process group that carry the same
    /// marker (SIGTERM, then SIGKILL, with bounded waits), also when the runtime itself was
    /// already gone. Returns whether a process was signalled. Blocks; call off the async
    /// runtime.
    fn reap_stale(&self, p: &StaleProcess) -> bool;

    /// Kills every process of this box marked for `thread` ([`THREAD_ENV`]) that started at or
    /// after `min_start`, with its process group: a runtime whose pid was never recorded (its
    /// hub died while it started). With `generation`, only the processes of that runtime
    /// generation ([`RUNTIME_GEN_ENV`]); the thread's other runtimes are left alone. Returns
    /// whether anything was signalled. Blocks; call off the async runtime. A remote box's
    /// runner does this itself.
    fn reap_marked(&self, _thread: &str, _min_start: u64, _generation: Option<u64>) -> bool {
        false
    }

    /// Whether a runtime of `thread` may still run on this box: the recorded process `p`, or a
    /// process marked for `thread` (and `generation`, when given) that started at or after
    /// `min_start` (see [`ThreadHost::reap_marked`]). The hub records an exit it did not
    /// observe (an adapter that vanished, a runtime stuck at shutdown) only once this is false.
    /// A remote box's runner observes its runtimes' exits itself: `false` there. Blocks; call
    /// off the async runtime.
    fn runtime_running(
        &self,
        thread: &str,
        p: Option<&StaleProcess>,
        min_start: Option<u64>,
        generation: Option<u64>,
    ) -> bool;

    /// Whether `runtime` can run here as the orchestrator ([`Adapter::hosts_orchestrator`]).
    /// Only the hub's own box runs the orchestrator: `false` elsewhere.
    fn hosts_orchestrator(&self, _runtime: &str) -> bool {
        false
    }

    /// Whether `runtime` here ends a running turn on `interrupt` and keeps its session
    /// ([`Adapter::interrupts_turns`]): a message whose turn started can be taken back only
    /// then. A remote box's sessions take no interrupt: `false` elsewhere.
    fn interrupts_turns(&self, _runtime: &str) -> bool {
        false
    }

    /// Whether `runtime` here compacts a session's context on [`AdapterCmd::Compact`]
    /// ([`Adapter::compacts`]). A remote box's sessions take no compaction (the runner
    /// protocol has no such command): `false` elsewhere.
    ///
    /// [`AdapterCmd::Compact`]: twr_core::adapter::AdapterCmd::Compact
    fn compacts(&self, _runtime: &str) -> bool {
        false
    }

    /// Which of `cfg`'s settings `runtime`'s attach command for `cfg` runs the session with
    /// ([`Adapter::attach_applies`]). Only the hub's own box is asked (it makes every attach
    /// command, a remote box's too): neither elsewhere.
    fn attach_applies(&self, _runtime: &str, _cfg: &SpawnConfig) -> AttachApplies {
        AttachApplies::default()
    }

    /// The program `runtime` starts here and the hint for when it is missing
    /// ([`Adapter::program`]). Only the hub's own box is asked (`orchestrator.use`): `None`
    /// elsewhere.
    fn program(&self, _runtime: &str) -> Option<(PathBuf, String)> {
        None
    }

    /// Whether the box is a remote one ([`crate::remote::RemoteBox`]): its runner stamps
    /// process start times and reaps its own runtimes.
    fn is_remote(&self) -> bool {
        false
    }

    /// Waits, at most `within`, until no operation of this box that changes its repositories
    /// or worktrees runs any more (whether anyone still waits for it or not): whether none
    /// does. The hub's own box counts its operations ([`LocalGit::changes_done`]); a remote
    /// box's runner runs its own to their end whatever the hub does: `true` at once.
    fn changes_done(&self, _within: std::time::Duration) -> BoxFuture<'static, bool> {
        futures::future::ready(true).boxed()
    }
}

/// Runs adapters as local child processes (box `local`).
pub struct LocalHost {
    adapters: AdapterLookup,
    git: Arc<LocalGit>,
    worktrees: PathBuf,
    branch_namespace: Option<String>,
    /// The box's trash (`<data_dir>/trash`), when set ([`LocalHost::with_trash`]).
    trash: Option<PathBuf>,
    /// How many of the next start-time reads fail on purpose (tests,
    /// [`crate::HubOptions::start_time_faults`]).
    start_time_faults: Arc<std::sync::atomic::AtomicUsize>,
}

impl LocalHost {
    /// A local host using `adapters` to find runtimes and keeping thread worktrees under
    /// `worktrees` (the hub's `<data_dir>/worktrees`).
    pub fn new(adapters: AdapterLookup, worktrees: impl Into<PathBuf>) -> Self {
        let worktrees = worktrees.into();
        LocalHost {
            adapters,
            // Stale worktree records are cleared only under the box's own worktrees.
            git: Arc::new(LocalGit::default().with_managed_root(worktrees.clone())),
            worktrees,
            branch_namespace: None,
            trash: None,
            start_time_faults: Arc::default(),
        }
    }

    /// Namespace for branches of new worktrees (`twr/<namespace>/<name>`).
    pub fn with_branch_namespace(mut self, namespace: Option<String>) -> Self {
        self.branch_namespace = namespace;
        self
    }

    /// The next `n` start-time reads ([`ThreadHost::process_start_time`]) fail, as when the
    /// process table cannot be read for a moment (tests).
    pub fn with_start_time_faults(self, n: usize) -> Self {
        self.start_time_faults
            .store(n, std::sync::atomic::Ordering::Relaxed);
        self
    }

    /// Deleted threads' worktrees and branches go to the trash at `trash` (the hub's
    /// `<data_dir>/trash`, [`LocalGit::with_trash_root`]).
    pub fn with_trash(self, trash: impl Into<PathBuf>) -> Self {
        let trash = trash.into();
        LocalHost {
            git: Arc::new((*self.git).clone().with_trash_root(trash.clone())),
            trash: Some(trash),
            ..self
        }
    }

    /// The ignored paths a park may delete with a worktree (`[threads] park_disposable`,
    /// [`LocalGit::with_park_disposable`]).
    pub fn with_park_disposable(self, disposable: Vec<String>) -> Self {
        LocalHost {
            git: Arc::new((*self.git).clone().with_park_disposable(disposable)),
            ..self
        }
    }

    /// Runs the git at `bin` instead of `git` on `PATH` ([`crate::HubOptions::git_bin`]).
    pub fn with_git_bin(self, bin: PathBuf) -> Self {
        let git = LocalGit::with_bin(bin).with_managed_root(self.worktrees.clone());
        let git = match &self.trash {
            Some(t) => git.with_trash_root(t.clone()),
            None => git,
        };
        LocalHost {
            git: Arc::new(git),
            ..self
        }
    }

    fn adapter(&self, runtime: &str) -> anyhow::Result<Arc<dyn Adapter>> {
        (self.adapters)(runtime).ok_or_else(|| anyhow::anyhow!("unknown runtime {runtime:?}"))
    }
}

impl ThreadHost for LocalHost {
    fn has_runtime(&self, runtime: &str) -> bool {
        (self.adapters)(runtime).is_some()
    }

    fn hosts_orchestrator(&self, runtime: &str) -> bool {
        (self.adapters)(runtime).is_some_and(|a| a.hosts_orchestrator())
    }

    fn program(&self, runtime: &str) -> Option<(PathBuf, String)> {
        (self.adapters)(runtime).and_then(|a| a.program())
    }

    fn interrupts_turns(&self, runtime: &str) -> bool {
        (self.adapters)(runtime).is_some_and(|a| a.interrupts_turns())
    }

    fn compacts(&self, runtime: &str) -> bool {
        (self.adapters)(runtime).is_some_and(|a| a.compacts())
    }

    fn attach_applies(&self, runtime: &str, cfg: &SpawnConfig) -> AttachApplies {
        (self.adapters)(runtime)
            .map(|a| a.attach_applies(cfg))
            .unwrap_or_default()
    }

    fn spawn(
        &self,
        runtime: &str,
        cfg: SpawnConfig,
        prompt: Option<String>,
    ) -> anyhow::Result<AdapterHandle> {
        self.adapter(runtime)?.spawn(cfg, prompt)
    }

    fn resume(
        &self,
        runtime: &str,
        cfg: SpawnConfig,
        session_id: String,
    ) -> anyhow::Result<AdapterHandle> {
        self.adapter(runtime)?.resume(cfg, session_id)
    }

    fn attach_command(
        &self,
        runtime: &str,
        cfg: &SpawnConfig,
        session_id: &str,
    ) -> anyhow::Result<Option<std::process::Command>> {
        Ok(self.adapter(runtime)?.attach_command(cfg, session_id))
    }

    fn remote_attach_command(
        &self,
        runtime: &str,
        cfg: &SpawnConfig,
        session_id: &str,
    ) -> anyhow::Result<Option<std::process::Command>> {
        Ok(self
            .adapter(runtime)?
            .remote_attach_command(cfg, session_id))
    }

    fn git(&self) -> Arc<dyn GitOps> {
        self.git.clone()
    }

    fn use_workdir(&self, dir: &Path, holder: &str) -> anyhow::Result<Option<WorkdirUse>> {
        self.git
            .workdirs()
            .begin_use(dir, holder)
            .map_err(anyhow::Error::msg)
    }

    fn worktree_path(&self, project: &str, name: &str) -> PathBuf {
        self.worktrees.join(path_component(project)).join(name)
    }

    fn worktree_branch(&self, name: &str) -> String {
        match &self.branch_namespace {
            Some(ns) => format!("twr/{ns}/{name}"),
            None => twr_core::git::thread_branch(name),
        }
    }

    fn changes_done(&self, within: std::time::Duration) -> BoxFuture<'static, bool> {
        let git = Arc::clone(&self.git);
        async move { git.changes_done(within).await }.boxed()
    }

    fn process_start_time(&self, pid: u32) -> Option<u64> {
        use std::sync::atomic::Ordering;
        let faulted = self
            .start_time_faults
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
            .is_ok();
        if faulted {
            return None;
        }
        reap::process_start_time(pid)
    }

    fn reap_stale(&self, p: &StaleProcess) -> bool {
        reap::reap_stale(p)
    }

    fn reap_marked(&self, thread: &str, min_start: u64, generation: Option<u64>) -> bool {
        let gen = generation.map(|g| g.to_string());
        reap::reap_marked(thread, min_start, gen.as_deref())
    }

    fn runtime_running(
        &self,
        thread: &str,
        p: Option<&StaleProcess>,
        min_start: Option<u64>,
        generation: Option<u64>,
    ) -> bool {
        let gen = generation.map(|g| g.to_string());
        reap::runtime_running(thread, p, min_start, gen.as_deref())
    }
}

/// Records the start time of the process a runtime of `host` reported (`ext.twr.pid` on a
/// `thread.started` / `thread.resumed`) as `ext.twr.start_time`, so a later hub can recognize
/// it (see the module docs). Only the hub writes `start_time` for a local runtime: a value the
/// adapter sent is replaced or removed. A remote box's runner stamps it itself (only that box
/// can read it): left as it is. The process table is read off the async runtime, and the
/// caller does this before taking the hub's core lock (reading it may take a while: `ps`).
/// A read that fails is retried ([`START_TIME_TRIES`]): without a start time a later hub cannot
/// recognize the process, and while it runs the hub matches it by pid alone
/// (`hub::agents`).
pub async fn stamp_start_time(
    host: Option<Arc<dyn ThreadHost>>,
    thread: &str,
    body: &mut twr_core::event::EventBody,
) {
    use twr_core::event::EventBody;
    let (EventBody::ThreadStarted(p) | EventBody::ThreadResumed(p)) = body else {
        return;
    };
    let Some(twr) = p
        .ext
        .as_mut()
        .and_then(|m| m.get_mut("twr"))
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    if host.as_ref().is_some_and(|h| h.is_remote()) {
        return;
    }
    let pid = twr
        .get("pid")
        .and_then(serde_json::Value::as_u64)
        .and_then(|p| u32::try_from(p).ok());
    let (Some(pid), Some(host)) = (pid, host) else {
        twr.remove("start_time");
        return;
    };
    match read_start_time(host, pid).await {
        Some(t) => {
            twr.insert("start_time".into(), serde_json::json!(t));
        }
        None => {
            tracing::warn!(
                thread,
                pid,
                tries = START_TIME_TRIES,
                "cannot read the runtime's start time: a later hub will not recognize the \
                 process, and until it exits its connections are matched by pid alone"
            );
            twr.remove("start_time");
        }
    }
}

/// How many times [`stamp_start_time`] reads a runtime's start time, and the first pause
/// between reads (doubled after each).
const START_TIME_TRIES: u32 = 4;

/// The start time of process `pid` on `host`'s machine ([`ThreadHost::process_start_time`]),
/// read off the async runtime and tried again ([`START_TIME_TRIES`], waiting longer each
/// time) while the read fails. `None` when every try failed.
pub(crate) async fn read_start_time(host: Arc<dyn ThreadHost>, pid: u32) -> Option<u64> {
    for i in 0..START_TIME_TRIES {
        let h = Arc::clone(&host);
        let read = tokio::task::spawn_blocking(move || h.process_start_time(pid))
            .await
            .ok()
            .flatten();
        if read.is_some() {
            return read;
        }
        if i + 1 < START_TIME_TRIES {
            tokio::time::sleep(START_TIME_PAUSE * (1 << i)).await;
        }
    }
    None
}
const START_TIME_PAUSE: std::time::Duration = std::time::Duration::from_millis(25);

/// `s` as one safe path component: characters outside `[A-Za-z0-9._-]` become `-`, leading
/// dots are dropped (no `.`, `..` or hidden directories), and an empty result is `project`.
pub fn path_component(s: &str) -> String {
    let mapped: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = mapped.trim_start_matches('.');
    if trimmed.is_empty() {
        "project".into()
    } else {
        trimmed.chars().take(64).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A local runtime's `ext.twr.start_time` is the hub's reading (an adapter's value is
    /// replaced, or removed when nothing can be read); a remote one's is the runner's.
    #[tokio::test]
    async fn start_times_are_stamped_by_the_reading_box() {
        use twr_core::event::{EventBody, SessionPayload};
        let started = |pid: u64| {
            EventBody::ThreadStarted(SessionPayload {
                ext: Some(
                    serde_json::json!({ "twr": { "pid": pid, "start_time": 7 } })
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
                ..Default::default()
            })
        };
        let start_time = |b: &EventBody| match b {
            EventBody::ThreadStarted(p) => {
                p.ext.as_ref().unwrap()["twr"].get("start_time").cloned()
            }
            _ => unreachable!(),
        };
        let local: Arc<dyn ThreadHost> = Arc::new(LocalHost::new(builtin_adapters(), "/w"));
        let me = std::process::id();
        let mut b = started(u64::from(me));
        stamp_start_time(Some(local.clone()), "t", &mut b).await;
        assert_eq!(
            start_time(&b),
            Some(serde_json::json!(reap::process_start_time(me).unwrap()))
        );
        let mut b = started(u64::from(u32::MAX - 1));
        stamp_start_time(Some(local.clone()), "t", &mut b).await;
        assert_eq!(start_time(&b), None);
        let mut b = started(u64::from(me));
        stamp_start_time(None, "t", &mut b).await;
        assert_eq!(start_time(&b), None);
        // A read that fails is retried; one that keeps failing leaves no start time.
        let flaky: Arc<dyn ThreadHost> =
            Arc::new(LocalHost::new(builtin_adapters(), "/w").with_start_time_faults(2));
        let mut b = started(u64::from(me));
        stamp_start_time(Some(flaky), "t", &mut b).await;
        assert_eq!(
            start_time(&b),
            Some(serde_json::json!(reap::process_start_time(me).unwrap()))
        );
        let broken: Arc<dyn ThreadHost> = Arc::new(
            LocalHost::new(builtin_adapters(), "/w")
                .with_start_time_faults(START_TIME_TRIES as usize),
        );
        let mut b = started(u64::from(me));
        stamp_start_time(Some(broken), "t", &mut b).await;
        assert_eq!(start_time(&b), None);
    }

    #[test]
    fn worktree_paths_stay_under_the_worktrees_dir() {
        let h = LocalHost::new(builtin_adapters(), "/data/worktrees");
        assert_eq!(
            h.worktree_path("api", "t1"),
            PathBuf::from("/data/worktrees/api/t1")
        );
        assert_eq!(path_component("my project/x"), "my-project-x");
        assert_eq!(path_component(".."), "project");
        assert_eq!(path_component("../../etc"), "-..-etc");
        assert_eq!(path_component(".hidden"), "hidden");
        assert_eq!(path_component(""), "project");
        assert_eq!(path_component("ünï"), "-n-");
        assert_eq!(path_component(&"a".repeat(100)).len(), 64);
    }

    /// A worktree removal on the local box whose caller went away (a `thread.park` whose
    /// request was dropped) runs on to its end as a task of its own (its git is not killed),
    /// and keeps the directory reserved until it is over: a runtime of the box cannot start
    /// there meanwhile, `worktree_exists` answers only then (removed), and the hub's shutdown
    /// waits for it ([`ThreadHost::changes_done`]).
    #[tokio::test]
    async fn a_removal_whose_caller_went_away_runs_to_its_end_and_keeps_runtimes_out() {
        use twr_core::git::WorktreeSpec;
        use twr_core::testkit::git::{init_repo, HeldGit};
        let dir = tempfile::tempdir().unwrap();
        let held = HeldGit::install(&dir.path().join("bin"));
        let worktrees = dir.path().join("worktrees");
        let h = LocalHost::new(builtin_adapters(), &worktrees).with_git_bin(held.bin.clone());
        let repo = init_repo(&dir.path().join("repo"));
        let wt = h.worktree_path("p", "t1");
        h.git()
            .worktree_create(WorktreeSpec {
                repo: repo.clone(),
                path: wt.clone(),
                branch: "twr/t1".into(),
                base: None,
            })
            .await
            .unwrap();
        // A runtime in it keeps a removal out.
        let u = h.use_workdir(&wt, "t1").unwrap();
        let kept = h
            .git()
            .worktree_remove(repo.clone(), wt.clone(), Some("twr/t1".into()))
            .await
            .unwrap();
        assert!(
            matches!(&kept, twr_core::git::RemoveOutcome::Kept { reason } if reason.contains("still runs")),
            "{kept:?}"
        );
        drop(u);
        let removal = tokio::spawn(h.git().worktree_remove(
            repo.clone(),
            wt.clone(),
            Some("twr/t1".into()),
        ));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !held.held() {
            assert!(std::time::Instant::now() < deadline, "git never held");
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        removal.abort();
        let _ = removal.await;
        let e = h.use_workdir(&wt, "t1").unwrap_err();
        assert!(format!("{e:#}").contains("being removed"), "{e:#}");
        let short = std::time::Duration::from_millis(300);
        assert!(!h.changes_done(short).await);
        let exists = tokio::spawn(h.git().worktree_exists(wt.clone()));
        tokio::time::sleep(short).await;
        assert!(!exists.is_finished(), "answered while the removal ran");
        // Released, the removal git runs to its end: the worktree is gone, and only then
        // does anything see it or start there.
        held.release();
        assert!(!exists.await.unwrap().unwrap());
        assert!(held.finished());
        assert!(!wt.exists());
        assert!(h.changes_done(std::time::Duration::from_secs(60)).await);
        assert!(h.use_workdir(&wt, "t1").unwrap().is_some());
    }

    #[test]
    fn local_host_finds_builtin_runtimes() {
        let h = LocalHost::new(builtin_adapters(), "/nonexistent/worktrees");
        assert!(h.has_runtime(twr_adapters::claude_code::NAME));
        assert!(!h.has_runtime("nope"));
        assert!(h.spawn("nope", SpawnConfig::default(), None).is_err());
    }
}
