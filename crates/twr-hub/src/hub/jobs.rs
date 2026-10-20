//! Jobs (`twr job run`, `job.start`): a command the hub runs for a thread, in the thread's
//! working directory, so the thread's agent need not run it itself and wait.
//!
//! **Who.** A thread's agent starts jobs for its own thread (its connection is restricted to
//! reading otherwise, `hub::agents`), the user for any thread (`thread` names it); the
//! orchestrator does not. An agent's job runs what the agent asks without anyone asking the
//! user, so it is refused (`relay_forbidden`) unless the thread's mode may run commands:
//! `manual` or wider, not one that only plans or reads (`plan`, `dontAsk`, `read-only`) nor
//! one twr cannot classify (an ACP agent's own id), as the thread's runtime runs the mode
//! ([`twr_adapters::mode_level_for`]). The mode is checked at `job.start`, and again when the
//! job leaves the queue (one queued before the mode narrowed ends `failed_to_start`): a job
//! that runs is not stopped when the thread's mode narrows. The user's jobs take any mode. The
//! server checks at each `job.start` and `job.cancel` that an agent's request comes from a
//! process of the thread's live runtime or attach (`server::job_opener`). Each
//! job records who asked (`ext.twr.job.by`): `user:<client>`, `agent:<thread id>` (its
//! runtime) or `attach:<thread id>` (the CLI attached to it). Jobs run on the hub's own box
//! (`local`) only, for now.
//!
//! **Slots.** Every job takes `slots` (1 by default) of the machine's job slots
//! (`[threads] job_slots`, the CPUs by default) while it runs, and waits for them in turn: the
//! hub's jobs in the order they came ([`JobState::queue`]), every hub of the home sharing one
//! pool ([`SlotPool`], a lock file per slot), so builds and test runs of several threads queue
//! instead of loading the machine all at once.
//!
//! **Tracking.** A job is one of its thread's background tasks, `job:<id>` (recorded by the
//! hub, [`JOB_TASK_PREFIX`]): the thread shows `background` while it runs, the turn that
//! started it reports an interim report, and the thread's activity line names it. Unlike a
//! runtime's own tasks it outlives the runtime (a settings restart, a crash), and neither a
//! settings restart nor an automatic retry waits for it. While it runs the hub renews it every
//! [`PROGRESS_EVERY`] (`queued (2 ahead)`, `running 6m`). It ends with a `task.updated`
//! (`running: false`) whose `ext.twr.job` says how: `{id, name, argv, outcome: exited |
//! cancelled | timed_out | failed_to_start | lost, exit?, signal?, duration_ms?, log, why?}`.
//!
//! **Waking.** When a job ends the hub queues a prompt to its thread (`from: runtime`, the
//! thread's follow-up turn), unless the job was started with `wake: false`, or an agent
//! started it and the thread is stopped (the prompt would start its runtime again): `[twr job <id>
//! <name>] `cmd` exited 101 after 14m 12s.` and the end of its output (at most
//! [`TAIL_LINES`] lines), with the path of its whole log. Its output is in
//! `<data>/jobs/<id>.log` (stdout and stderr together).
//!
//! **Ends.** `job.cancel`, `thread.stop`, a forced park and a deletion cancel a thread's
//! jobs: the job's process group gets SIGTERM, then SIGKILL after [`KILL_AFTER`]. An attach,
//! forced or not, leaves them running: a job runs on beside the attached CLI (`hub::attach`).
//! `timeout_secs` does the same once a job ran that long. A job that exits leaves no process
//! of its group behind either (SIGTERM). A hub shutdown cancels every job; a job a stopped hub
//! did not see end (it died) is recorded `lost` by the next hub when it starts, once what is
//! left of its process group is killed: the group is recorded before the job's command runs
//! (the command waits at a gate until then, [`gated_spawn`]), so its processes are found
//! whatever their environments say. A job whose leftovers cannot be verified gone is not
//! recorded lost, and its thread starts no runtime and no job until a later start verifies
//! them ([`Core::hold_unreaped`]).

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::Notify;
use twr_core::event::*;
use twr_core::proto::{codes, RpcError};
use twr_core::reduce::BackgroundTask;

use super::shell::{
    shell_output, shell_prompt, shell_record, SHELL_RECORD_BYTES, SHELL_SHARE_BYTES,
};
use super::{rpc, twr_ext_map, Core, Inner};

/// How often a running or queued job renews its task (liveness, and what it says now).
pub const PROGRESS_EVERY: Duration = Duration::from_secs(60);
/// How often the members of a running job's process group are read, and recorded when they
/// changed ([`record_members`]).
const GROUP_EVERY: Duration = Duration::from_secs(5);
/// How many members of a job's process group are recorded (the oldest).
const GROUP_MEMBERS_KEPT: usize = 8;
/// How long a cancelled job's process group has after SIGTERM before SIGKILL.
pub const KILL_AFTER: Duration = Duration::from_secs(5);
/// Lines of a job's output its wake prompt quotes.
pub const TAIL_LINES: usize = 40;
/// Most bytes of a job's log `job.log` or a wake prompt reads (the end of it).
pub const TAIL_BYTES: u64 = 64 << 10;
/// How many times a job's end is tried before it is given up ([`finish`]).
const FINISH_TRIES: u32 = 5;
/// The longest `timeout_secs` a job takes (30 days).
pub const MAX_TIMEOUT: Duration = Duration::from_secs(30 * 24 * 3600);
/// Most jobs a thread may have queued or running at once.
pub const JOBS_PER_THREAD: usize = 8;
/// How often a job at the head of the queue tries for its slots again.
const SLOT_POLL: Duration = Duration::from_millis(500);
/// Failures in a row to take slots (not slots busy: a lock file that can't be opened) after
/// which the job at the queue's head fails to start.
const SLOT_ERRORS: u32 = 10;

/// `job.start` parameters.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobStart {
    /// The command, as argv (no shell): `["cargo", "test", "--workspace"]`.
    pub argv: Vec<String>,
    /// The thread (the user only; an agent's job is its own thread's).
    #[serde(default)]
    pub thread: Option<String>,
    /// A short name for people (default: the command's first two words).
    #[serde(default)]
    pub name: Option<String>,
    /// Job slots it takes while it runs (default 1, at most the pool's size).
    #[serde(default)]
    pub slots: Option<usize>,
    /// Cancelled once it ran this long.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    /// Prompt the thread when it ends (default true).
    #[serde(default)]
    pub wake: Option<bool>,
    /// A `!` command the user runs from the TUI (`shell.run`, `hub::shell`): never from
    /// `job.start`'s parameters.
    #[serde(skip)]
    pub shell: Option<ShellJob>,
}

/// What makes a job a user's `!` command (`hub::shell`): it runs at once (no slots, no queue),
/// on the orchestrator's thread too, in `cwd` there; its end records the end of its output;
/// and only when the user shared it (`!!`) does its thread get a prompt, one that quotes the
/// command and its output as such.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ShellJob {
    /// The command line, as typed after `!` (run with `$SHELL -c`).
    pub command: String,
    /// Where it runs: the orchestrator view's folder (the thread's worktree when `None`).
    pub cwd: Option<PathBuf>,
    /// Its command and output go to the thread when it ends (`!!`).
    pub share: bool,
}

/// The hub's jobs (memory only: a job the hub did not see end is `lost` at the next start).
#[derive(Default)]
pub(super) struct JobState {
    /// Queued and running jobs, by id.
    jobs: HashMap<String, JobRt>,
    /// Queued job ids, in the order they came: only the first tries for slots.
    queue: VecDeque<String>,
    /// Woken when the queue moves.
    moved: Arc<Notify>,
    /// Jobs a stopped hub ran whose leftovers could not be verified gone at this hub's start
    /// ([`Core::hold_unreaped`]): by job id, their thread and its directory's hold. They stay
    /// running in the log (a later start tries again), and their thread starts no runtime
    /// and no job meanwhile.
    unreaped: HashMap<String, (String, Option<twr_runner::workdirs::WorkdirUse>)>,
}

/// A job a stopped hub ran and did not see end ([`Core::lost_job_leftovers`]): what is left of
/// it to kill.
pub(super) struct LostJob {
    /// Its id.
    pub id: String,
    /// Its `TWR_AGENT` marker ([`job_marker`]).
    pub marker: String,
    /// Its process group, as recorded before its command ran.
    pub group: Option<twr_core::reduce::JobGroup>,
}

/// A queued or running job.
pub(super) struct JobRt {
    thread: String,
    /// Who asked for it.
    by: JobBy,
    name: String,
    argv: Vec<String>,
    slots: usize,
    cwd: PathBuf,
    env: Vec<(String, String)>,
    timeout: Option<Duration>,
    wake: bool,
    log: PathBuf,
    /// Asked to end (`job.cancel`, a stop, a shutdown), and why.
    cancel: Arc<Cancel>,
    /// Since when it runs (`None`: queued).
    started: Option<Instant>,
    /// A user's `!` command ([`ShellJob`]).
    shell: Option<ShellJob>,
    /// Its directory held on the box ([`twr_runner::workdirs`]): no removal or trim of it
    /// runs while the job is queued or runs.
    _workdir: Option<twr_runner::workdirs::WorkdirUse>,
}

/// A job's cancellation, why, and whether its thread is still told (not after its stop, its
/// park or deletion, or a shutdown): set once, though a later cancel can still stop the
/// telling; [`Cancel::cancelled`] sees it whenever it was set.
pub(super) struct Cancel(tokio::sync::watch::Sender<Option<(String, bool)>>);

impl Default for Cancel {
    fn default() -> Self {
        Cancel(tokio::sync::watch::Sender::new(None))
    }
}

impl Cancel {
    /// Sets it, the first time; a later cancel that tells nobody (a stop after a user's
    /// cancel) still keeps the thread from being told.
    fn cancel(&self, why: &str, wake: bool) {
        self.0.send_if_modified(|v| match v {
            None => {
                *v = Some((why.to_string(), wake));
                true
            }
            Some((_, told)) => {
                *told &= wake;
                false
            }
        });
    }
    fn is_set(&self) -> bool {
        self.0.borrow().is_some()
    }
    fn why(&self) -> String {
        self.0
            .borrow()
            .as_ref()
            .map(|c| c.0.clone())
            .unwrap_or_default()
    }
    /// Whether its thread is told of its end: not when its cancel said not to.
    fn wakes(&self) -> bool {
        self.0.borrow().as_ref().is_none_or(|c| c.1)
    }
    /// Resolves once it is set (at once when it is already).
    async fn cancelled(&self) {
        let mut rx = self.0.subscribe();
        let _ = rx.wait_for(Option::is_some).await;
    }
}

/// The machine's job slots: a lock file per slot in one directory, held (advisory `flock`)
/// by the job that took it, whichever hub runs it; released when the job (or its hub) is gone.
/// The directory is made private ([`twr_core::paths::ensure_private_dir`]) by the pool's
/// first take, not on every poll of a job that waits for slots.
pub struct SlotPool {
    dir: PathBuf,
    n: usize,
    /// The directory was made private.
    private: std::sync::atomic::AtomicBool,
}

/// Slots a job holds (their files, locked): released when dropped.
pub struct Slots(#[allow(dead_code)] Vec<std::fs::File>);

impl SlotPool {
    /// `n` slots (at least 1) in `dir`.
    pub fn new(dir: impl Into<PathBuf>, n: usize) -> SlotPool {
        SlotPool {
            dir: dir.into(),
            n: n.max(1),
            private: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// The pool's size.
    pub fn size(&self) -> usize {
        self.n
    }

    /// `k` free slots, all or none.
    pub fn try_take(&self, k: usize) -> std::io::Result<Option<Slots>> {
        use std::os::unix::fs::OpenOptionsExt;
        use std::sync::atomic::Ordering;
        if !self.private.load(Ordering::Relaxed) {
            twr_core::paths::ensure_private_dir(&self.dir).map_err(std::io::Error::other)?;
            self.private.store(true, Ordering::Relaxed);
        }
        let mut held = Vec::new();
        for i in 0..self.n {
            if held.len() == k {
                break;
            }
            // Not through a symbolic link, nor blocking on a FIFO, put at its name.
            let file = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .mode(0o600)
                .custom_flags(super::OPEN_NOFOLLOW_NONBLOCK)
                .open(self.dir.join(format!("slot-{i}")))?;
            match file.try_lock() {
                Ok(()) => held.push(file),
                Err(std::fs::TryLockError::WouldBlock) => {}
                Err(std::fs::TryLockError::Error(e)) => return Err(e),
            }
        }
        Ok((held.len() == k.max(1)).then_some(Slots(held)))
    }
}

/// `n` seconds for people: `45s`, `6m 12s`, `2h 3m`.
fn duration_text(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m {}s", s / 60, s % 60),
        _ => format!("{}h {}m", s / 3600, (s % 3600) / 60),
    }
}

/// A job's name for people: given, else its command's first two words.
fn job_name(argv: &[String], name: Option<&str>) -> String {
    match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => n.chars().take(64).collect(),
        None => argv
            .iter()
            .take(2)
            .map(|a| {
                Path::new(a)
                    .file_name()
                    .map_or_else(|| a.clone(), |f| f.to_string_lossy().into_owned())
            })
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// The command as one line for people (each word quoted when it needs it).
fn command_text(argv: &[String]) -> String {
    argv.iter()
        .map(|a| {
            if !a.is_empty()
                && a.chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c))
            {
                a.clone()
            } else {
                format!("'{}'", a.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A job id is the hub's (a ULID): nothing else names a log file.
fn valid_job_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 32 && id.bytes().all(|b| b.is_ascii_alphanumeric())
}

/// The end of `path`, at most [`TAIL_BYTES`], from a line start: its last `lines` lines, at
/// most [`TAIL_BYTES`] of text too (bytes that are not UTF-8 read as U+FFFD, three bytes each,
/// so the text is cut again, at a character, keeping its end).
pub(super) fn tail(path: &Path, lines: usize) -> std::io::Result<String> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(super::OPEN_NOFOLLOW_NONBLOCK)
        .open(path)?;
    let meta = f.metadata()?;
    if !meta.is_file() {
        return Err(std::io::Error::other(format!(
            "{} is not a file",
            path.display()
        )));
    }
    let len = meta.len();
    let from = len.saturating_sub(TAIL_BYTES);
    f.seek(SeekFrom::Start(from))?;
    let mut buf = Vec::new();
    f.take(TAIL_BYTES).read_to_end(&mut buf)?;
    let text = String::from_utf8_lossy(&buf);
    // A cut first line is no line.
    let text = if from > 0 {
        text.split_once('\n').map_or("", |(_, rest)| rest)
    } else {
        &text
    };
    let all: Vec<&str> = text.lines().collect();
    let text = all[all.len().saturating_sub(lines)..].join("\n");
    Ok(keep_end(text, TAIL_BYTES as usize))
}

/// The end of `text`, at most `max` bytes of it, cut at a character.
fn keep_end(text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_string()
}

/// Opens job log `path` for appending: created with mode 0600, not through a symbolic link
/// nor blocking on a FIFO put at its name, a regular file, restricted to its owner (on the
/// opened file) when others could read it.
fn open_log(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    let f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(super::OPEN_NOFOLLOW_NONBLOCK)
        .open(path)?;
    let meta = f.metadata()?;
    if !meta.is_file() {
        return Err(std::io::Error::other(format!(
            "{} is not a file",
            path.display()
        )));
    }
    if meta.mode() & 0o077 != 0 {
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(f)
}

impl Core {
    /// The jobs directory (`<data>/jobs`).
    fn jobs_dir(hub: &Inner) -> PathBuf {
        hub.db_path
            .parent()
            .map_or_else(|| PathBuf::from("jobs"), |d| d.join("jobs"))
    }

    /// `job.start` for thread `id` by `by` ([`JobStart`]): queued now, run when its slots are
    /// free. `{job, thread, thread_name, name, position, log}`. Who asked is recorded as
    /// `ext.twr.job.by` ([`JobBy::recorded`]; `source` is the user's client).
    pub(super) fn start_job(
        &mut self,
        hub: &Arc<Inner>,
        id: &str,
        req: JobStart,
        source: Source,
        by: JobBy,
    ) -> Result<Value, RpcError> {
        if self.shutting_down {
            return Err(rpc(codes::CONFLICT, "hub is shutting down"));
        }
        if req.argv.is_empty() || req.argv[0].is_empty() {
            return Err(rpc(
                codes::BAD_REQUEST,
                "argv is empty: give the command to run",
            ));
        }
        if req.argv.iter().any(|a| a.contains('\0')) {
            return Err(rpc(codes::BAD_REQUEST, "argv holds a NUL byte"));
        }
        let view = self.view(id)?.clone();
        self.refuse_retired(id)?;
        self.refuse_deleting(id)?;
        let name = view.name.clone();
        let shell = req.shell.clone();
        // A `!` command is the user's, from `shell.run` only.
        if shell.is_some() != (by == JobBy::Shell) {
            return Err(rpc(
                codes::BAD_REQUEST,
                "a `!` command comes from shell.run only",
            ));
        }
        // The orchestrator's view runs the user's `!` commands, recorded on its thread; its
        // agent runs no job.
        if view.is_orchestrator() && shell.is_none() {
            return Err(rpc(codes::BAD_REQUEST, "the orchestrator runs no jobs"));
        }
        if view.box_name != "local" && shell.is_some() {
            return Err(rpc(
                codes::CONFLICT,
                format!(
                    "{name} runs on box {}: `!` runs commands on the hub's own box only; \
                     run it there with `twr attach {name}` or ssh",
                    view.box_name
                ),
            ));
        }
        if view.box_name != "local" {
            return Err(rpc(
                codes::CONFLICT,
                format!(
                    "{name} runs on box {}: jobs run only on the hub's own box for now",
                    view.box_name
                ),
            ));
        }
        if view.parked {
            return Err(rpc(
                codes::CONFLICT,
                format!("{name} is parked: resume it first"),
            ));
        }
        // Not once it was told to stop: its jobs went with the stop, and a new one would
        // outlive it. Its runtime's agent can't start one on a stopped thread either (what is
        // left of a stopped runtime); the CLI attached to it can.
        if self.rt.get(id).is_some_and(|rt| {
            rt.stop_requested || rt.exiting || rt.live.as_ref().is_some_and(|l| l.stop_requested)
        }) {
            return Err(rpc(
                codes::CONFLICT,
                format!("{name}'s runtime is stopping; try again once it is done"),
            ));
        }
        self.refuse_unreaped(id)?;
        if by == JobBy::Runtime && view.stopped {
            return Err(rpc(codes::CONFLICT, format!("{name} is stopped")));
        }
        if !by.is_user() {
            refuse_agent_job_mode(&view)?;
        }
        // Not while its worktree may go or change under it.
        if let Some(rt) = self.rt.get(id) {
            if rt.park.is_some() || rt.resuming {
                return Err(rpc(
                    codes::CONFLICT,
                    format!("{name} is being parked or resumed"),
                ));
            }
            if rt.trimming {
                return Err(rpc(
                    codes::CONFLICT,
                    format!("{name}'s build output is being trimmed"),
                ));
            }
        }
        let timeout = match req.timeout_secs {
            None => None,
            Some(s) if s > 0 && s <= MAX_TIMEOUT.as_secs() => Some(Duration::from_secs(s)),
            Some(_) => {
                return Err(rpc(
                    codes::BAD_REQUEST,
                    format!("timeout_secs must be 1 to {}", MAX_TIMEOUT.as_secs()),
                ))
            }
        };
        let pool = self.slot_pool(hub);
        // A `!` command takes no slots: it runs at once, as typed in a terminal would.
        let slots = if shell.is_some() {
            0
        } else {
            req.slots.unwrap_or(1)
        };
        if shell.is_none() && (slots == 0 || slots > pool.size()) {
            return Err(rpc(
                codes::BAD_REQUEST,
                format!(
                    "slots must be 1 to {} (the machine's job slots)",
                    pool.size()
                ),
            ));
        }
        let mine = self.jobs.jobs.values().filter(|j| j.thread == id).count();
        if mine >= JOBS_PER_THREAD {
            return Err(rpc(
                codes::CONFLICT,
                format!("{name} has {mine} jobs queued or running already"),
            ));
        }
        let mut cfg = super::spawn_config(hub, &view);
        // The orchestrator view's folder, which the user picked (`!cd`): it is no worktree.
        let own_dir = shell.as_ref().and_then(|s| s.cwd.clone());
        if let Some(dir) = &own_dir {
            cfg.cwd = dir.clone();
        }
        if !cfg.cwd.is_dir() {
            return Err(rpc(
                codes::CONFLICT,
                format!("{name}'s directory {} is not there", cfg.cwd.display()),
            ));
        }
        // Held from now until it ends: a removal or trim of its directory waits or is refused.
        let workdir = match hub.host(&view.box_name).filter(|_| own_dir.is_none()) {
            Some(host) => host
                .use_workdir(&cfg.cwd, id)
                .map_err(|e| rpc(codes::CONFLICT, format!("{e:#}")))?,
            None => None,
        };
        let job = new_id();
        let dir = Self::jobs_dir(hub);
        twr_core::paths::ensure_private_dir(&dir).map_err(super::internal)?;
        let log = dir.join(format!("{job}.log"));
        // There from the start, empty while it waits: `job.log` reads it whatever happens.
        open_log(&log).map_err(super::internal)?;
        // Its processes are the thread's agent's, as a runtime's are (`hub::agents`): they
        // read the hub, and change nothing in it.
        let mut env = cfg.env.clone();
        env.splice(
            0..0,
            [
                (super::THREAD_ENV.to_string(), id.to_string()),
                (super::AGENT_ENV.to_string(), job_marker(id, &job)),
            ],
        );
        let display = job_name(&req.argv, req.name.as_deref());
        let ahead = if shell.is_some() {
            0
        } else {
            self.jobs.queue.len()
        };
        let wake = match &shell {
            Some(s) => s.share,
            None => req.wake.unwrap_or(true),
        };
        let mut record = json!({
            "id": job, "name": display, "argv": req.argv, "slots": slots,
            "log": log, "wake": wake, "by": by.recorded(&source, id),
        });
        if let Some(s) = &shell {
            record["shell"] = shell_record(s, &cfg.cwd);
        }
        let body = EventBody::TaskUpdated(TaskUpdated {
            task_id: format!("{JOB_TASK_PREFIX}{job}"),
            running: true,
            description: match &shell {
                Some(_) => format!("! {display}: running"),
                None => format!("job {display}: queued ({ahead} ahead)"),
            },
            ext: twr_ext_map(json!({ "job": record })),
            ..Default::default()
        });
        // Recorded by the hub, which runs it (a job's task is the hub's), saying who asked. Not
        // recorded, it is no job: its log goes.
        if let Err(e) = self.append(hub, id, None, Source::Hub, body) {
            let _ = std::fs::remove_file(&log);
            return Err(super::append_err(e));
        }
        let cancel = Arc::new(Cancel::default());
        self.jobs.jobs.insert(
            job.clone(),
            JobRt {
                thread: id.to_string(),
                by,
                name: display.clone(),
                argv: req.argv.clone(),
                slots,
                cwd: cfg.cwd.clone(),
                env,
                timeout,
                wake,
                log: log.clone(),
                cancel,
                started: None,
                shell: shell.clone(),
                _workdir: workdir,
            },
        );
        // A `!` command waits for nothing: it is in no queue.
        if shell.is_none() {
            self.jobs.queue.push_back(job.clone());
        }
        tokio::spawn(run_job(Arc::clone(hub), job.clone()));
        Ok(json!({
            "job": job,
            "thread": id,
            "thread_name": name,
            "name": display,
            "position": ahead,
            "log": log,
            "cwd": cfg.cwd,
        }))
    }

    /// Who asks for an agent's job of thread `id` (`by`), checked again under the hub's lock:
    /// the process the request's peer descends from must still be the thread's live runtime,
    /// or the attach's reported CLI, as the hub records them now ([`Core::started_table`]).
    /// Else `relay_forbidden`: a runtime that exited or was replaced, an attach that ended.
    fn agent_job_by(&self, id: &str, by: AgentJobBy) -> Result<JobBy, RpcError> {
        use super::agents::StartedKind;
        let (p, kind, job_by) = match by {
            AgentJobBy::Runtime(p) => (p, StartedKind::Runtime, JobBy::Runtime),
            AgentJobBy::Attach(p) => (p, StartedKind::Attach, JobBy::Attach),
        };
        let table = self.started_table();
        match table.lookup(&p) {
            Some(s) if s.thread == id && s.kind == kind => Ok(job_by),
            _ => {
                let name = self.registry.get(id).map_or(id, |v| v.name.as_str());
                Err(rpc(
                    codes::RELAY_FORBIDDEN,
                    format!(
                        "pid {} is no longer {name}'s live runtime or attached CLI: its \
                         processes start and cancel no job",
                        p.pid
                    ),
                ))
            }
        }
    }

    /// The machine's job slots ([`SlotPool`]).
    fn slot_pool(&self, hub: &Inner) -> SlotPool {
        let n = match hub.config.threads.job_slots {
            0 => twr_core::load::machine_cpus(),
            n => usize::try_from(n).unwrap_or(usize::MAX),
        };
        SlotPool::new(hub.job_slots_dir.clone(), n)
    }

    /// `job.list {thread?}`: the running jobs (of `thread`), then the queued ones in order.
    pub(super) fn list_jobs(&self, thread: Option<&str>) -> Value {
        let mut out: Vec<Value> = self
            .jobs
            .jobs
            .iter()
            .filter(|(_, j)| thread.is_none_or(|t| j.thread == t))
            .map(|(id, j)| {
                let position = self.jobs.queue.iter().position(|q| q == id);
                json!({
                    "job": id,
                    "thread": j.thread,
                    "thread_name": self.registry.get(&j.thread).map(|v| v.name.clone()),
                    "name": j.name,
                    "argv": j.argv,
                    "slots": j.slots,
                    "state": if j.started.is_some() { "running" } else { "queued" },
                    "position": position,
                    "running_secs": j.started.map(|s| s.elapsed().as_secs()),
                    "log": j.log,
                })
            })
            .collect();
        // Running ones first, then the queue in its order.
        out.sort_by_key(|j| {
            (
                j["position"].as_u64().map_or(0, |p| p + 1),
                j["job"].as_str().map(str::to_string),
            )
        });
        Value::Array(out)
    }

    /// The job whose id is `query` or starts with it (4 characters at least), and its thread.
    fn find_job(&self, query: &str) -> Result<(String, String), RpcError> {
        let hits: Vec<(&String, &JobRt)> = self
            .jobs
            .jobs
            .iter()
            .filter(|(id, _)| *id == query || (query.len() >= 4 && id.starts_with(query)))
            .collect();
        match hits.as_slice() {
            [(id, j)] => Ok(((*id).clone(), j.thread.clone())),
            [] => Err(rpc(
                codes::NOT_FOUND,
                format!("no queued or running job {query:?}"),
            )),
            _ => Err(rpc(
                codes::AMBIGUOUS,
                format!("{query:?} names several jobs"),
            )),
        }
    }

    /// `job.cancel {job}`: asks it to end (only a job of `only` when given: an agent's own
    /// thread). It ends as soon as its process group is gone.
    pub(super) fn cancel_job(&self, query: &str, only: Option<&str>) -> Result<Value, RpcError> {
        let (id, thread) = self.find_job(query)?;
        if only.is_some_and(|t| t != thread) {
            return Err(rpc(
                codes::RELAY_FORBIDDEN,
                "a thread's agent cancels only its own thread's jobs",
            ));
        }
        // The user's `!` commands: only the user stops them.
        if only.is_some() && self.jobs.jobs.get(&id).is_some_and(|j| j.shell.is_some()) {
            return Err(rpc(
                codes::RELAY_FORBIDDEN,
                "a `!` command is the user's: only the user stops it",
            ));
        }
        if let Some(j) = self.jobs.jobs.get(&id) {
            j.cancel.cancel("cancelled", true);
        }
        Ok(json!({"job": id, "thread": thread, "cancelling": true}))
    }

    /// Cancels every job of `thread` (its stop, a forced park, its deletion), telling it
    /// nothing: whatever stopped it should not start it again.
    pub(super) fn cancel_jobs_of(&self, thread: &str, why: &str) -> usize {
        let mut n = 0;
        // The orchestrator's runtime stopping (a stop, a reset, a switch) leaves the user's `!`
        // commands of its view running: they are no work of its.
        let orch = self
            .registry
            .get(thread)
            .is_some_and(|v| v.is_orchestrator());
        for j in self
            .jobs
            .jobs
            .values()
            .filter(|j| j.thread == thread && !(orch && j.shell.is_some()))
        {
            j.cancel.cancel(why, false);
            n += 1;
        }
        n
    }

    /// Cancels every job (a shutdown), telling no thread.
    pub(super) fn cancel_all_jobs(&self, why: &str) {
        for j in self.jobs.jobs.values() {
            j.cancel.cancel(why, false);
        }
    }

    /// Whether thread `thread` has a job queued or running.
    pub(super) fn has_jobs(&self, thread: &str) -> bool {
        self.jobs.jobs.values().any(|j| j.thread == thread)
    }

    /// Whether any job is queued or running.
    pub(super) fn jobs_running(&self) -> bool {
        !self.jobs.jobs.is_empty()
    }

    /// The jobs the log shows running that no job of this hub is: a hub that stopped without
    /// seeing them end ran them. As `(thread, task id, task)`.
    fn lost_jobs(&self) -> Vec<(String, String, BackgroundTask)> {
        self.registry
            .list()
            .iter()
            .flat_map(|v| {
                v.background_tasks
                    .iter()
                    .filter(|(_, t)| t.job)
                    .map(|(task, t)| (v.id.clone(), task.clone(), t.clone()))
                    .collect::<Vec<_>>()
            })
            .filter(|(_, task, _)| {
                let id = task.trim_start_matches(JOB_TASK_PREFIX);
                !self.jobs.jobs.contains_key(id)
            })
            .collect()
    }

    /// What is left to kill of [`Core::lost_jobs`]: each one's `TWR_AGENT` marker
    /// ([`job_marker`]) and the process group recorded before its command ran
    /// ([`twr_core::reduce::JobGroup`]: a job's process that cleared its markers is still in
    /// its group).
    pub(super) fn lost_job_leftovers(&self) -> Vec<LostJob> {
        self.lost_jobs()
            .into_iter()
            .map(|(thread, task, t)| {
                let id = task.trim_start_matches(JOB_TASK_PREFIX).to_string();
                LostJob {
                    marker: job_marker(&thread, &id),
                    id,
                    group: t.group.clone(),
                }
            })
            .collect()
    }

    /// Keeps lost jobs `ids` (of [`Core::lost_jobs`]) whose leftovers could not be verified
    /// gone from being recorded lost, and their threads from starting a runtime or a job, and
    /// holds their directories against removal, until a later start verifies them gone.
    pub(super) fn hold_unreaped(&mut self, hub: &Arc<Inner>, ids: &[String]) {
        for (thread, task, _) in self.lost_jobs() {
            let id = task.trim_start_matches(JOB_TASK_PREFIX).to_string();
            if !ids.contains(&id) {
                continue;
            }
            let hold = self.registry.get(&thread).cloned().and_then(|view| {
                let cwd = super::spawn_config(hub, &view).cwd;
                let host = hub.host(&view.box_name)?;
                host.use_workdir(&cwd, &thread).ok().flatten()
            });
            tracing::error!(
                thread,
                job = %id,
                "what is left of a job a stopped hub ran could not be verified gone: its thread \
                 starts no runtime and no job until a hub start verifies it (end its processes, \
                 then restart the hub)"
            );
            self.jobs.unreaped.insert(id, (thread, hold));
        }
    }

    /// Refuses to start anything in thread `id` while a lost job of it may still run
    /// ([`Core::hold_unreaped`]).
    pub(super) fn refuse_unreaped(&self, id: &str) -> Result<(), RpcError> {
        let Some(job) = self
            .jobs
            .unreaped
            .iter()
            .find_map(|(job, (t, _))| (t == id).then_some(job))
        else {
            return Ok(());
        };
        let name = self.registry.get(id).map_or(id, |v| v.name.as_str());
        Err(rpc(
            codes::CONFLICT,
            format!(
                "{name}: what is left of job {job}, which a stopped hub ran, may still run (it \
                 could not be verified gone); end its processes and restart the hub"
            ),
        ))
    }

    /// At startup, once what is left of them was killed: [`Core::lost_jobs`] are each
    /// recorded `lost`, and their thread told, unless the job was started not to, or the
    /// thread was stopped or parked meanwhile (what cancelled the job told nobody). The end and
    /// the prompt that tells it are one batch: a job whose end cannot be recorded stays lost
    /// for the next start to record, with its prompt.
    pub(super) fn close_lost_jobs(&mut self, hub: &Arc<Inner>) {
        for (thread, task, job) in self.lost_jobs() {
            let description = job.description;
            let id = task.trim_start_matches(JOB_TASK_PREFIX).to_string();
            if self.jobs.unreaped.contains_key(&id) {
                continue;
            }
            let body = EventBody::TaskUpdated(TaskUpdated {
                task_id: task.clone(),
                running: false,
                description: description.clone(),
                ext: twr_ext_map(json!({"job": {"id": id, "outcome": "lost",
                    "why": "the hub stopped while it ran"}})),
                ..Default::default()
            });
            let mut batch = super::Batch::default();
            batch.push(&thread, None, Source::Hub, body);
            let resting = self
                .registry
                .get(&thread)
                .is_none_or(|v| v.stopped || v.parked);
            // The user's `!` commands are theirs: an agent is never asked to run one again.
            let told = !(job.quiet || job.shell || resting)
                && {
                    let prompt = format!(
                        "[twr job {id}] {description}: lost, the hub stopped while it ran. Run it \
                     again if you still need it."
                    );
                    // A thread that takes no prompt now (being deleted, say) is not told.
                    match self.push_prompt(
                        &mut batch,
                        &thread,
                        &prompt,
                        TurnFrom::Runtime,
                        Source::Hub,
                        None,
                    ) {
                        Ok(()) => true,
                        Err(e) => {
                            tracing::debug!(thread, error = %e.message, "cannot tell a thread its job was lost");
                            false
                        }
                    }
                };
            match self.append_batch(hub, batch) {
                Ok(done) => {
                    if let (true, Some(&q)) = (told, done.queued.first()) {
                        self.dispatch_queued(hub, &thread, q);
                    }
                }
                Err(e) => {
                    tracing::warn!(thread, job = %id, error = %format!("{e:#}"), "cannot record a lost job; the next start tries again");
                }
            }
        }
    }
}

/// How a job ended.
enum Outcome {
    Exited(std::process::ExitStatus),
    Cancelled(String),
    TimedOut,
    FailedToStart(String),
}

/// Which process twr started for a thread an agent's job request descends from
/// ([`super::Hub::start_agent_job`], [`super::Hub::cancel_agent_job`]), as the walk of the
/// request's peer matched it (pid and start time).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentJobBy {
    /// The thread's runtime.
    Runtime(twr_runner::reap::ProcessId),
    /// The CLI attached to the thread, as the attach's client reported it (`attach.child`).
    Attach(twr_runner::reap::ProcessId),
}

/// Who starts a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum JobBy {
    /// A user client.
    User,
    /// A process of the thread's runtime.
    Runtime,
    /// A process of the CLI attached to the thread.
    Attach,
    /// A user client's `!` command (`shell.run`).
    Shell,
}

impl JobBy {
    /// Whether the user asked for it (a job or a `!` command): no agent's mode rules.
    fn is_user(self) -> bool {
        matches!(self, JobBy::User | JobBy::Shell)
    }

    /// Who asked for a job of thread `thread`, as its task records it (`ext.twr.job.by`):
    /// `user:<client>` (`source`) for a user, `agent:<thread id>` for the thread's runtime,
    /// `attach:<thread id>` for the CLI attached to it. An agent's connection says
    /// `user:<client>` as its source too, so its own is not what it is.
    fn recorded(self, source: &Source, thread: &str) -> String {
        match self {
            JobBy::User | JobBy::Shell => source.to_string(),
            JobBy::Runtime => format!("agent:{thread}"),
            JobBy::Attach => format!("attach:{thread}"),
        }
    }
}

/// Refuses a job of `view`'s own agent unless the thread's mode may run commands: `manual` or
/// wider, as its runtime runs the recorded mode ([`twr_adapters::mode_level_for`]; the mode
/// `thread.new` resolved, `threads.default_mode` when it named none). A mode that only plans
/// or reads, or denies what is not allowed (`plan`, `dontAsk`, `read-only`), would otherwise
/// let the agent run through a job what it may not run itself; a mode twr cannot classify (an
/// ACP agent's own id) is refused too.
fn refuse_agent_job_mode(view: &twr_core::reduce::ThreadView) -> Result<(), RpcError> {
    use twr_adapters::acp::translate::ModeLevel;
    let mode = view.mode.trim();
    let why = match twr_adapters::mode_level_for(&view.runtime, mode) {
        Some(ModeLevel::Restricted) => format!("{} is in {mode} mode", view.name),
        Some(_) => return Ok(()),
        None if mode.is_empty() => format!("{}'s mode is its agent's own default", view.name),
        None => format!(
            "{}'s mode {mode:?} is its agent's own, and twr does not know what it allows",
            view.name
        ),
    };
    Err(rpc(
        codes::RELAY_FORBIDDEN,
        format!("{why}: jobs need a mode that may run commands (manual or wider)"),
    ))
}

/// The `TWR_AGENT` marker of job `job` of thread `thread`: its processes are that thread's
/// agent, and a hub after the one that started them finds what is left of them by it.
fn job_marker(thread: &str, job: &str) -> String {
    format!("{thread}:job-{job}")
}

/// Records job `id`'s task as `description`; a `renewal` only says it still runs (a
/// heartbeat, which transcripts leave out).
fn progress(hub: &Arc<Inner>, id: &str, description: String, renewal: bool) {
    let mut core = hub.core();
    let Some(thread) = core.jobs.jobs.get(id).map(|j| j.thread.clone()) else {
        return;
    };
    let body = EventBody::TaskUpdated(TaskUpdated {
        task_id: format!("{JOB_TASK_PREFIX}{id}"),
        running: true,
        description,
        heartbeat: renewal,
        ..Default::default()
    });
    if let Err(e) = core.append(hub, &thread, None, Source::Hub, body) {
        tracing::debug!(job = id, error = %format!("{e:#}"), "cannot renew a job");
    }
}

/// Records the members of job `id`'s process group the hub sees while the job runs (its leader
/// aside, [`GROUP_MEMBERS_KEPT`] of them, the oldest first): with the leader gone, they prove
/// the group the job's to a later hub ([`twr_runner::reap::reap_job_group`]). A renewal, which
/// transcripts leave out.
fn record_members(hub: &Arc<Inner>, id: &str, members: &[(u32, u64)]) {
    let mut core = hub.core();
    let Some(thread) = core.jobs.jobs.get(id).map(|j| j.thread.clone()) else {
        return;
    };
    let members: Vec<[u64; 2]> = members.iter().map(|(p, t)| [u64::from(*p), *t]).collect();
    let body = EventBody::TaskUpdated(TaskUpdated {
        task_id: format!("{JOB_TASK_PREFIX}{id}"),
        running: true,
        heartbeat: true,
        ext: twr_ext_map(json!({"job": {"id": id, "members": members}})),
        ..Default::default()
    });
    if let Err(e) = core.append(hub, &thread, None, Source::Hub, body) {
        tracing::debug!(job = id, error = %format!("{e:#}"), "cannot record a job's group");
    }
}

/// Sends `sig` to process group `pgid`.
fn signal_group(pgid: u32, sig: &str) {
    let _ = twr_runner::reap::signal(pgid, true, sig);
}

/// Runs job `id`: waits for its turn and its slots, runs it, records how it ended, and wakes
/// its thread.
async fn run_job(hub: Arc<Inner>, id: String) {
    let Some((cancel, slots_wanted, name, shell)) = ({
        let core = hub.core();
        core.jobs.jobs.get(&id).map(|j| {
            (
                Arc::clone(&j.cancel),
                j.slots,
                j.name.clone(),
                j.shell.is_some(),
            )
        })
    }) else {
        return;
    };
    // A `!` command runs at once: no turn to wait for, no slots.
    if shell {
        let started = Instant::now();
        let outcome = if cancel.is_set() {
            Outcome::Cancelled(cancel.why())
        } else {
            run_process(&hub, &id, &cancel, started).await
        };
        finish(&hub, &id, outcome, started.elapsed()).await;
        return;
    }
    let pool = hub.core().slot_pool(&hub);
    let moved = Arc::clone(&hub.core().jobs.moved);
    // 1. Its turn, and its slots.
    let mut last_renew = Instant::now();
    let mut errors = 0;
    let held = loop {
        if cancel.is_set() {
            break Ok(None);
        }
        let ahead = {
            let core = hub.core();
            core.jobs.queue.iter().position(|q| *q == id).unwrap_or(0)
        };
        if ahead == 0 {
            match pool.try_take(slots_wanted) {
                Ok(Some(s)) => break Ok(Some(s)),
                Ok(None) => errors = 0,
                Err(e) => {
                    tracing::warn!(job = %id, error = %e, "cannot take job slots");
                    errors += 1;
                    // Not one that passes: it fails, and the queue moves on.
                    if errors >= SLOT_ERRORS {
                        break Err(format!("cannot take its job slots: {e}"));
                    }
                }
            }
        }
        if last_renew.elapsed() >= PROGRESS_EVERY {
            last_renew = Instant::now();
            progress(
                &hub,
                &id,
                format!("job {name}: queued ({ahead} ahead)"),
                true,
            );
        }
        tokio::select! {
            _ = moved.notified() => {}
            _ = cancel.cancelled() => {}
            _ = tokio::time::sleep(SLOT_POLL) => {}
        }
    };
    {
        let mut core = hub.core();
        core.jobs.queue.retain(|q| *q != id);
    }
    moved.notify_waiters();
    let started = Instant::now();
    let outcome = match held {
        Err(e) => Outcome::FailedToStart(e),
        Ok(None) => Outcome::Cancelled(cancel.why()),
        Ok(Some(_slots)) => {
            let outcome = run_process(&hub, &id, &cancel, started).await;
            // `_slots` released here, once the process group is gone.
            outcome
        }
    };
    moved.notify_waiters();
    finish(&hub, &id, outcome, started.elapsed()).await;
}

/// Starts job `id`'s process and waits for its end, renewing its task meanwhile.
async fn run_process(hub: &Arc<Inner>, id: &str, cancel: &Cancel, started: Instant) -> Outcome {
    let Some((argv, cwd, env, log, timeout, name, shell)) = ({
        let core = hub.core();
        core.jobs.jobs.get(id).map(|j| {
            (
                j.argv.clone(),
                j.cwd.clone(),
                j.env.clone(),
                j.log.clone(),
                j.timeout,
                j.name.clone(),
                j.shell.is_some(),
            )
        })
    }) else {
        return Outcome::Cancelled("gone".into());
    };
    let out = match open_log(&log) {
        Ok(f) => f,
        Err(e) => {
            return Outcome::FailedToStart(format!("cannot open its log {}: {e}", log.display()))
        }
    };
    let err = match out.try_clone() {
        Ok(f) => f,
        Err(e) => return Outcome::FailedToStart(format!("cannot open its log: {e}")),
    };
    // Its whole environment, as its thread's runtime gets it: the hub's, with the job's
    // variables over it, without what is scrubbed from a runtime's. A hub started in a thread
    // has its runtime's generation: a job of its own must not match that runtime's cleanup.
    let mut vars: std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString> =
        std::env::vars_os().collect();
    for (k, v) in &env {
        vars.insert(k.into(), v.into());
    }
    for k in twr_adapters::process::SCRUBBED_ENV
        .iter()
        .chain([&twr_runner::reap::RUNTIME_GEN_ENV])
    {
        vars.remove(std::ffi::OsStr::new(k));
    }
    vars.retain(|k, _| !k.is_empty() && !k.as_encoded_bytes().contains(&b'='));
    // Under the workers' limits, as its thread's runtime (`hub::limits`). A program that is
    // not there fails to start, as without them (the gate says so), not as a command of the
    // wrapper's that failed.
    let wrap = hub.limits.wrap_for(&env);
    if !wrap.is_empty() {
        let path = env
            .iter()
            .rev()
            .find(|(k, _)| k == "PATH")
            .map(|(_, v)| std::ffi::OsString::from(v))
            .or_else(|| std::env::var_os("PATH"));
        let program = std::path::Path::new(&argv[0]);
        if let Some(why) = twr_adapters::process::missing_program(program, path.as_deref(), &cwd) {
            return Outcome::FailedToStart(why);
        }
    }
    let command: Vec<std::ffi::OsString> = wrap
        .iter()
        .chain(argv.iter())
        .map(std::ffi::OsString::from)
        .collect();
    // Starting from here, unless a cancel won the race (checked under the lock a cancel
    // takes, here and again before its command runs: a cancelled job never runs it), or the
    // job is an agent's and its thread's mode no longer lets it run commands (checked when it
    // was queued, and again now: the mode may have narrowed while it waited).
    {
        let mut core = hub.core();
        let Some((by, thread)) = core.jobs.jobs.get(id).map(|j| (j.by, j.thread.clone())) else {
            return Outcome::Cancelled("gone".into());
        };
        if cancel.is_set() {
            return Outcome::Cancelled(cancel.why());
        }
        if !by.is_user() {
            if let Some(Err(e)) = core.registry.get(&thread).map(refuse_agent_job_mode) {
                return Outcome::FailedToStart(e.message);
            }
        }
        if let Some(j) = core.jobs.jobs.get_mut(id) {
            j.started = Some(started);
        }
    }
    let spawned = gated_spawn(hub, id, &name, &cwd, &vars, &command, (out, err), cancel).await;
    let mut child = match spawned {
        Ok(c) => c,
        Err(o) => return o,
    };
    let Some(pid) = child.id() else {
        let _ = child.wait().await;
        return Outcome::FailedToStart("it exited before it could be followed".into());
    };
    let deadline = timeout.map(|t| tokio::time::Instant::now() + t);
    let mut renew =
        tokio::time::interval_at(tokio::time::Instant::now() + PROGRESS_EVERY, PROGRESS_EVERY);
    let mut snapshot =
        tokio::time::interval_at(tokio::time::Instant::now() + GROUP_EVERY, GROUP_EVERY);
    snapshot.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut members: Vec<(u32, u64)> = Vec::new();
    // The snapshot being read: off the loop, so the job's exit, a cancel and its timeout are
    // seen while it runs (on the `ps` backend it is a `ps` call, which may take seconds).
    let mut reading: Option<tokio::task::JoinHandle<Option<Members>>> = None;
    let ended = loop {
        tokio::select! {
            r = child.wait() => break match r {
                Ok(status) => Outcome::Exited(status),
                Err(e) => Outcome::FailedToStart(format!("cannot wait for it: {e}")),
            },
            _ = cancel.cancelled() => break Outcome::Cancelled(cancel.why()),
            _ = sleep_until_opt(deadline) => break Outcome::TimedOut,
            _ = renew.tick() => {
                let what = if shell { "!" } else { "job" };
                progress(hub, id, format!("{what} {name}: running {}", duration_text(started.elapsed())), true);
            }
            _ = snapshot.tick(), if reading.is_none() => {
                let hook = hub.job_members_hook.clone();
                reading = Some(tokio::task::spawn_blocking(move || {
                    if let Some(hook) = hook {
                        hook(pid);
                    }
                    twr_runner::reap::job_group_members(pid, GROUP_MEMBERS_KEPT)
                }));
            }
            read = async { reading.as_mut().expect("guarded").await }, if reading.is_some() => {
                reading = None;
                if let Some(now) = read.ok().flatten().filter(|now| *now != members) {
                    record_members(hub, id, &now);
                    members = now;
                }
            }
        }
    };
    match &ended {
        // What it left behind in its group goes too.
        Outcome::Exited(_) => {
            if twr_runner::reap::group_running(pid) == Some(true) {
                end_group(pid, &mut child).await;
            }
        }
        _ => end_group(pid, &mut child).await,
    }
    ended
}

/// A job's process group's members, as (pid, start time) ([`twr_runner::reap::job_group_members`]).
type Members = Vec<(u32, u64)>;

/// How long the hub waits for a job's start gate to say whether it found the program, and,
/// once let go, whether it ran it ([`gated_spawn`]).
const GATE_REPLY: Duration = Duration::from_secs(30);

/// One line of a job's start gate ([`crate::job_gate`]) from `sock`, read off the async runtime
/// (at most [`GATE_REPLY`]); `None` at end of file, on a timeout, or when it cannot be read.
async fn gate_line(sock: &std::os::unix::net::UnixStream) -> Option<String> {
    let mut sock = sock.try_clone().ok()?;
    tokio::task::spawn_blocking(move || {
        use std::io::Read;
        sock.set_read_timeout(Some(GATE_REPLY)).ok()?;
        let mut line = Vec::new();
        let mut b = [0u8; 1];
        while line.len() < 256 {
            match sock.read(&mut b) {
                Ok(1) if b[0] == b'\n' => return String::from_utf8(line).ok(),
                Ok(1) => line.push(b[0]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                _ => return None,
            }
        }
        None
    })
    .await
    .ok()
    .flatten()
}

/// Starts job `id`'s command `command` (its words, the program first) in `cwd` with
/// environment `vars` (all of it) and output to `out` / `err`, as the leader of a session and
/// process group of its own (no controlling terminal: a command that reads the terminal of
/// whoever started the hub gets an error instead of being stopped), held at its start gate ([`crate::job_gate`], run from
/// [`HubOptions::job_gate_exe`]) until its process group is recorded ([`release`]): so a later
/// hub knows the group of every job whose command ran, and kills what is left of it when this
/// hub stops without seeing the job end, also a process of it that cleared the `TWR_AGENT`
/// marker ([`twr_runner::reap::reap_job_group`]).
///
/// The gate is a process of its own that has exec'd, not a wait between `fork` and `exec`: a
/// child that has not exec'd keeps every descriptor the hub had when it forked, the error
/// pipes of other spawns in progress included (one the hub's lock is held across), which would
/// wait for it in turn. It keeps no descriptor of the hub's but its standard ones (the others
/// close on exec), its standard input one end of a socket pair. Its environment is the job's,
/// as its process environment (readable by its user only), never on its command line
/// (readable by every user). It says whether it found the program (`missing`: the job fails
/// to start), then waits; once let go it `execv`s the program, and says so on the socket only
/// when that fails (the job fails to start with the error).
///
/// A gate that does not open (the start time of the process cannot be read, the group cannot
/// be recorded, the job was cancelled or its thread's mode narrowed meanwhile) is killed: it is
/// still the gate (it was never let go, and it is the hub's unreaped child), so nothing of the
/// command ran. A gate this hub dies before letting go reads end of file and exits. On macOS,
/// where the socket pair cannot be made close-on-exec at once, a process another task of the
/// hub spawns in that instant can keep the hub's end too: it could then let the gate go itself,
/// before the group is recorded, and a dead hub's gate would wait for that process to end.
///
/// [`HubOptions::job_gate_exe`]: crate::HubOptions::job_gate_exe
#[allow(clippy::too_many_arguments)]
async fn gated_spawn(
    hub: &Arc<Inner>,
    id: &str,
    name: &str,
    cwd: &Path,
    vars: &std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString>,
    command: &[std::ffi::OsString],
    (out, err): (std::fs::File, std::fs::File),
    cancel: &Cancel,
) -> Result<tokio::process::Child, Outcome> {
    let Some(exe) = hub.job_gate_exe.as_ref() else {
        return Err(Outcome::FailedToStart(
            "this hub has no program to start jobs with (`twr __job-gate`)".into(),
        ));
    };
    let (sock, gate_end) = std::os::unix::net::UnixStream::pair()
        .map_err(|e| Outcome::FailedToStart(format!("cannot make its start gate: {e}")))?;
    let mut cmd = tokio::process::Command::new(exe);
    cmd.env_clear()
        .envs(vars)
        .arg(crate::job_gate::GATE_ARG)
        .arg("--")
        .args(command)
        .current_dir(cwd)
        .stdin(std::os::fd::OwnedFd::from(gate_end))
        .stdout(out)
        .stderr(err)
        .kill_on_drop(false);
    twr_adapters::process::new_session(&mut cmd);
    let spawned = cmd.spawn();
    // The parent's copy of the gate's end goes with the command.
    drop(cmd);
    let mut child = spawned.map_err(|e| Outcome::FailedToStart(format!("{e}")))?;
    // Not opened: killed (still the gate, never let go) and waited for.
    async fn shut(child: &mut tokio::process::Child, o: Outcome) -> Outcome {
        let _ = child.start_kill();
        let _ = child.wait().await;
        o
    }
    let Some(pid) = child.id() else {
        let _ = child.wait().await;
        return Err(Outcome::FailedToStart(
            "it exited before it could be followed".into(),
        ));
    };
    match gate_line(&sock).await.as_deref() {
        Some("ready") => {}
        Some(l) if l.starts_with("missing ") => {
            let why = l["missing ".len()..].to_string();
            return Err(shut(&mut child, Outcome::FailedToStart(why)).await);
        }
        other => {
            let why = format!("its start gate did not answer as it should ({other:?})");
            return Err(shut(&mut child, Outcome::FailedToStart(why)).await);
        }
    }
    if let Some(hook) = hub.job_gate_hook.clone() {
        let _ = tokio::task::spawn_blocking(move || hook(pid)).await;
    }
    let start_time = match hub.host(super::LOCAL_BOX) {
        Some(host) => crate::host::read_start_time(host, pid).await,
        None => None,
    };
    let opened = match start_time {
        Some(t) => release(hub, id, name, pid, t, cancel, &sock),
        None => Err(Outcome::FailedToStart(format!(
            "the start time of its process (pid {pid}) cannot be read: a later hub could not \
             tell what is left of it, so it does not run"
        ))),
    };
    if let Err(o) = opened {
        return Err(shut(&mut child, o).await);
    }
    // Let go: end of file once it exec'd; `exec <errno>` when that failed.
    match gate_line(&sock).await {
        None => Ok(child),
        Some(l) => {
            let e = l
                .strip_prefix("exec ")
                .and_then(|n| n.parse::<i32>().ok())
                .map(|n| std::io::Error::from_raw_os_error(n).to_string())
                .unwrap_or(l);
            let _ = child.wait().await;
            Err(Outcome::FailedToStart(e))
        }
    }
}

/// Lets job `id`'s child, waiting at its gate ([`gated_spawn`]), run its command, under the
/// hub's lock (the one a cancel and a mode change take), in this order: the job is still there
/// and not cancelled; an agent's job's thread still is in a mode that may run commands; its
/// process group, led by `pid` started at `start_time`, is recorded (its task's `running`
/// update says `ext.twr.job.pgid` and `start_time`); the gate's line is written to `go`. A
/// cancel or a mode change that comes after it finds the command running, and ends it as any
/// running job's.
fn release(
    hub: &Arc<Inner>,
    id: &str,
    name: &str,
    pid: u32,
    start_time: u64,
    cancel: &Cancel,
    mut go: &std::os::unix::net::UnixStream,
) -> Result<(), Outcome> {
    use std::io::Write;
    let mut core = hub.core();
    let Some((thread, by)) = core.jobs.jobs.get(id).map(|j| (j.thread.clone(), j.by)) else {
        return Err(Outcome::Cancelled("gone".into()));
    };
    let shell = by == JobBy::Shell;
    if cancel.is_set() {
        return Err(Outcome::Cancelled(cancel.why()));
    }
    if !by.is_user() {
        if let Some(Err(e)) = core.registry.get(&thread).map(refuse_agent_job_mode) {
            return Err(Outcome::FailedToStart(e.message));
        }
    }
    let body = EventBody::TaskUpdated(TaskUpdated {
        task_id: format!("{JOB_TASK_PREFIX}{id}"),
        running: true,
        description: format!("{} {name}: running", if shell { "!" } else { "job" }),
        ext: twr_ext_map(json!({"job": {"id": id, "pgid": pid, "start_time": start_time}})),
        ..Default::default()
    });
    core.append(hub, &thread, None, Source::Hub, body)
        .map_err(|e| Outcome::FailedToStart(format!("cannot record its process group: {e:#}")))?;
    go.write_all(b"go\n")
        .map_err(|e| Outcome::FailedToStart(format!("cannot start it: {e}")))
}

/// Ends process group `pgid`, whose leader is `child`: SIGTERM, then SIGKILL once
/// [`KILL_AFTER`] passed with the leader or any member left (one that ignores SIGTERM after
/// the leader went), and waits for the leader and, bounded, the rest of the group.
pub(super) async fn end_group(pgid: u32, child: &mut tokio::process::Child) {
    let alive = || twr_runner::reap::group_running(pgid) == Some(true);
    signal_group(pgid, "TERM");
    let deadline = Instant::now() + KILL_AFTER;
    let mut leader_gone = false;
    loop {
        if !leader_gone {
            leader_gone = matches!(child.try_wait(), Ok(Some(_)) | Err(_));
        }
        if leader_gone && !alive() {
            return;
        }
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    signal_group(pgid, "KILL");
    let _ = child.wait().await;
    let deadline = Instant::now() + KILL_AFTER;
    while alive() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn sleep_until_opt(at: Option<tokio::time::Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// Records how job `id` ended and, unless it was told not to, prompts its thread: the two in
/// one batch, retried a few times; the job stays listed until they are recorded.
async fn finish(hub: &Arc<Inner>, id: &str, outcome: Outcome, took: Duration) {
    let Some((thread, name, argv, log, wake, shell, cwd)) = ({
        let core = hub.core();
        core.jobs.jobs.get(id).map(|j| {
            (
                j.thread.clone(),
                j.name.clone(),
                j.argv.clone(),
                j.log.clone(),
                j.wake && j.cancel.wakes(),
                j.shell.clone(),
                j.cwd.clone(),
            )
        })
    }) else {
        return;
    };
    let cmd = command_text(&argv);
    let (kind, exit, signal, what) = match &outcome {
        Outcome::Exited(s) => {
            use std::os::unix::process::ExitStatusExt;
            match (s.code(), s.signal()) {
                (Some(c), _) => (
                    "exited",
                    Some(c),
                    None,
                    format!("exited {c} after {}", duration_text(took)),
                ),
                (None, Some(sig)) => (
                    "exited",
                    None,
                    Some(sig),
                    format!("was killed by signal {sig} after {}", duration_text(took)),
                ),
                _ => (
                    "exited",
                    None,
                    None,
                    format!("ended after {}", duration_text(took)),
                ),
            }
        }
        Outcome::Cancelled(why) => (
            "cancelled",
            None,
            None,
            format!("was {why} after {}", duration_text(took)),
        ),
        Outcome::TimedOut => (
            "timed_out",
            None,
            None,
            format!("timed out after {}", duration_text(took)),
        ),
        Outcome::FailedToStart(why) => (
            "failed_to_start",
            None,
            None,
            format!("could not start: {why}"),
        ),
    };
    let mut record = json!({
        "id": id, "name": name, "argv": argv, "outcome": kind,
        "exit": exit, "signal": signal, "duration_ms": took.as_millis() as u64,
        "log": log,
    });
    // A `!` command's end keeps the end of what it printed: what the user saw, in the history.
    if let Some(s) = &shell {
        record["shell"] = shell_record(s, &cwd);
        record["printed"] = json!(shell_output(&log, SHELL_RECORD_BYTES));
    }
    let body = EventBody::TaskUpdated(TaskUpdated {
        task_id: format!("{JOB_TASK_PREFIX}{id}"),
        running: false,
        description: match &shell {
            Some(_) => format!("! {name}: {what}"),
            None => format!("job {name}: {what}"),
        },
        ext: twr_ext_map(json!({ "job": record })),
        ..Default::default()
    });
    let prompt = wake.then(|| {
        if let Some(s) = &shell {
            return shell_prompt(s, &cwd, &what, &shell_output(&log, SHELL_SHARE_BYTES));
        }
        let out = tail(&log, TAIL_LINES).unwrap_or_default();
        let output = if out.trim().is_empty() {
            "It printed nothing.".to_string()
        } else {
            format!("The end of its output:\n```\n{out}\n```")
        };
        format!(
            "[twr job {id} {name}] `{cmd}` {what}. {output}\nIts whole log: {}",
            log.display()
        )
    });
    for attempt in 0..FINISH_TRIES {
        {
            let mut core = hub.core();
            if core.registry.get(&thread).is_none() {
                // Deleted meanwhile: nothing to record on.
                core.jobs.jobs.remove(id);
                core.jobs.queue.retain(|q| q != id);
                return;
            }
            let mut batch = super::Batch::default();
            batch.push(&thread, None, Source::Hub, body.clone());
            // A thread that takes no prompt now (being deleted, the hub stopping), or whose
            // job was cancelled meanwhile by what tells nobody (its stop), is not told; nor
            // is a stopped thread of an agent's job (what is left of an attach's CLI): the
            // prompt would start its runtime again, with what the job printed.
            let stopped = core.registry.get(&thread).is_some_and(|v| v.stopped);
            let wakes = core
                .jobs
                .jobs
                .get(id)
                .is_some_and(|j| j.cancel.wakes() && (j.by.is_user() || !stopped));
            let told = match prompt.as_ref().filter(|_| wakes) {
                Some(p) => core
                    .push_prompt(&mut batch, &thread, p, TurnFrom::Runtime, Source::Hub, None)
                    .is_ok(),
                None => false,
            };
            match core.append_batch(hub, batch) {
                Ok(done) => {
                    core.jobs.jobs.remove(id);
                    core.jobs.queue.retain(|q| q != id);
                    if let (true, Some(&q)) = (told, done.queued.first()) {
                        core.dispatch_queued(hub, &thread, q);
                    }
                    return;
                }
                Err(e) => {
                    tracing::warn!(job = id, attempt, error = %format!("{e:#}"), "cannot record a job's end; trying again")
                }
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    tracing::error!(
        job = id,
        "a job's end could not be recorded; the next hub records it lost"
    );
    let mut core = hub.core();
    core.jobs.jobs.remove(id);
    core.jobs.queue.retain(|q| q != id);
}

impl super::Hub {
    /// Whether `token` is the hub's test user token ([`super::HubOptions::test_user_token`]).
    pub fn test_user_token_is(&self, token: &str) -> bool {
        self.inner
            .test_user_token
            .as_ref()
            .is_some_and(|t| t.ct_eq(token))
    }

    /// `job.start` (see the module docs) for thread `query` by `source`, a user.
    pub fn start_job(&self, query: &str, req: JobStart, source: Source) -> Result<Value, RpcError> {
        let mut core = self.inner.core();
        let id = core.resolve(query)?;
        core.start_job(&self.inner, &id, req, source, JobBy::User)
    }

    /// `job.start` by thread `query`'s own agent (`source`, its connection's): a process of its
    /// runtime, refused on a stopped thread, or of the CLI attached to it ([`AgentJobBy`]:
    /// only while that process still is the thread's live runtime or the attach's reported CLI,
    /// checked here under the hub's lock); recorded as `agent:<thread id>` or
    /// `attach:<thread id>`, and refused unless the thread's mode may run commands (see the
    /// module docs).
    pub fn start_agent_job(
        &self,
        query: &str,
        req: JobStart,
        source: Source,
        by: AgentJobBy,
    ) -> Result<Value, RpcError> {
        let mut core = self.inner.core();
        let id = core.resolve(query)?;
        let by = core.agent_job_by(&id, by)?;
        core.start_job(&self.inner, &id, req, source, by)
    }

    /// `job.cancel {job}` by thread `thread`'s own agent: a job of that thread only, and only
    /// while `by` is still its live runtime or attached CLI (checked under the hub's lock).
    pub fn cancel_agent_job(
        &self,
        job: &str,
        thread: &str,
        by: AgentJobBy,
    ) -> Result<Value, RpcError> {
        let core = self.inner.core();
        let id = core.resolve(thread)?;
        core.agent_job_by(&id, by)?;
        core.cancel_job(job, Some(&id))
    }

    /// `job.list {thread?}`.
    pub fn list_jobs(&self, thread: Option<&str>) -> Result<Value, RpcError> {
        let core = self.inner.core();
        let id = thread.map(|t| core.resolve(t)).transpose()?;
        Ok(core.list_jobs(id.as_deref()))
    }

    /// `job.cancel {job}` (`only`: an agent's own thread).
    pub fn cancel_job(&self, job: &str, only: Option<&str>) -> Result<Value, RpcError> {
        self.inner.core().cancel_job(job, only)
    }

    /// `job.log {job, lines?}`: the end of a job's output (queued, running or ended),
    /// `{job, log, text}` (at most [`TAIL_BYTES`]). Read only, like a transcript: any client
    /// may read any job's.
    pub fn job_log(&self, job: &str, lines: Option<usize>) -> Result<Value, RpcError> {
        if !valid_job_id(job) {
            return Err(rpc(codes::BAD_REQUEST, "a job id is letters and digits"));
        }
        let core = self.inner.core();
        // A live job by prefix; an ended one by its full id.
        let id = match core.find_job(job) {
            Ok((id, _)) => id,
            Err(e) if e.code == codes::NOT_FOUND => job.to_string(),
            Err(e) => return Err(e),
        };
        let path = Core::jobs_dir(&self.inner).join(format!("{id}.log"));
        drop(core);
        let text = match tail(&path, lines.unwrap_or(TAIL_LINES)) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(rpc(codes::NOT_FOUND, format!("no log of job {id}")))
            }
            Err(e) => return Err(super::internal(e)),
        };
        Ok(json!({"job": id, "log": path, "text": text}))
    }

    /// Cancels every job and waits, at most `within`, until each recorded its end (a
    /// shutdown).
    pub(super) async fn end_jobs(&self, within: Duration) {
        self.inner
            .core()
            .cancel_all_jobs("cancelled: the hub stopped");
        let deadline = tokio::time::Instant::now() + within;
        while self.inner.core().jobs_running() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slots_are_all_or_none_and_shared() {
        let dir = tempfile::tempdir().unwrap();
        let pool = SlotPool::new(dir.path().join("slots"), 3);
        let two = pool.try_take(2).unwrap().expect("two free");
        // One left: two more are not there, so none is taken.
        assert!(pool.try_take(2).unwrap().is_none());
        // Another hub's pool on the same directory sees the same slots.
        let other = SlotPool::new(dir.path().join("slots"), 3);
        let one = other.try_take(1).unwrap().expect("one free");
        assert!(other.try_take(1).unwrap().is_none());
        drop(two);
        assert!(pool.try_take(2).unwrap().is_some());
        drop(one);
        assert_eq!(SlotPool::new(dir.path(), 0).size(), 1);
    }

    /// A pool makes its directory private on its first take only: a job waiting for slots
    /// polls every [`SLOT_POLL`], and each check walks the whole path.
    #[test]
    fn a_pool_makes_its_directory_private_once() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let slots = dir.path().join("slots");
        let mode = || std::fs::metadata(&slots).unwrap().permissions().mode() & 0o777;
        let pool = SlotPool::new(&slots, 1);
        drop(pool.try_take(1).unwrap().expect("free"));
        assert_eq!(mode(), 0o700);
        std::fs::set_permissions(&slots, std::fs::Permissions::from_mode(0o750)).unwrap();
        drop(pool.try_take(1).unwrap().expect("free"));
        assert_eq!(mode(), 0o750, "checked again on a later take");
        // A new pool (the next job's) checks it again.
        drop(SlotPool::new(&slots, 1).try_take(1).unwrap().expect("free"));
        assert_eq!(mode(), 0o700);
    }

    #[test]
    fn names_commands_and_durations_for_people() {
        let argv = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            job_name(&argv(&["/usr/bin/cargo", "test", "-q"]), None),
            "cargo test"
        );
        assert_eq!(job_name(&argv(&["make"]), Some(" build ")), "build");
        assert_eq!(
            command_text(&argv(&["sh", "-c", "echo 'hi' && exit 3"])),
            "sh -c 'echo '\\''hi'\\'' && exit 3'"
        );
        assert_eq!(duration_text(Duration::from_secs(45)), "45s");
        assert_eq!(duration_text(Duration::from_secs(372)), "6m 12s");
        assert_eq!(duration_text(Duration::from_secs(7380)), "2h 3m");
        assert!(valid_job_id("01J0ABC"));
        assert!(!valid_job_id("../x") && !valid_job_id(""));
    }

    #[test]
    fn tail_keeps_the_last_whole_lines() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        let text: String = (0..10_000).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&p, text).unwrap();
        let t = tail(&p, 3).unwrap();
        assert_eq!(t, "line 9997\nline 9998\nline 9999");
        std::fs::write(&p, "only\n").unwrap();
        assert_eq!(tail(&p, 40).unwrap(), "only");
    }

    /// Output that is not UTF-8 reads as U+FFFD (three bytes for each byte): the text is cut
    /// again to [`TAIL_BYTES`], at a character, keeping its end. A log that is a symbolic link
    /// or a FIFO is not read.
    #[test]
    fn tail_text_stays_within_its_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        std::fs::write(&p, vec![0xffu8; 64_000]).unwrap();
        let t = tail(&p, TAIL_LINES).unwrap();
        assert!(t.len() <= TAIL_BYTES as usize, "{}", t.len());
        assert!(t.len() > TAIL_BYTES as usize - 3);
        assert!(t.chars().all(|c| c == char::REPLACEMENT_CHARACTER));
        let mut bytes = b"first\n".to_vec();
        bytes.extend(vec![0xfe; 40_000]);
        bytes.extend(b"\nlast line");
        std::fs::write(&p, &bytes).unwrap();
        let t = tail(&p, 2).unwrap();
        assert!(t.ends_with("\nlast line"), "{}", &t[t.len() - 20..]);
        assert!(t.len() <= TAIL_BYTES as usize);
        assert_eq!(keep_end("héllo".into(), 4), "llo");
        assert_eq!(keep_end("abc".into(), 4), "abc");
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&p, &link).unwrap();
        assert!(tail(&link, 1).is_err());
        assert!(open_log(&link).is_err());
    }

    #[test]
    fn a_cancel_that_tells_nobody_wins_over_one_that_tells() {
        let c = Cancel::default();
        assert!(c.wakes());
        c.cancel("cancelled", true);
        assert!(c.is_set() && c.wakes());
        c.cancel("cancelled: the thread was stopped", false);
        assert_eq!(c.why(), "cancelled");
        assert!(!c.wakes());
        c.cancel("cancelled", true);
        assert!(!c.wakes());
    }
}
