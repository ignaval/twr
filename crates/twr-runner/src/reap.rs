//! Stale runtime processes: recognizing and killing a runtime that outlived the
//! process that started it (the hub for box `local`, a runner on its box).
//!
//! Every runtime gets [`THREAD_ENV`]`=<thread id>` in its environment, and its pid and start
//! time are recorded when it starts (the hub: `ext.twr.pid` / `ext.twr.start_time` on
//! `thread.started` / `thread.resumed`; a runner: its spool's runtime table). After a crash,
//! [`reap_stale`] only signals a process whose pid, start time and environment marker all
//! still match, so a reused pid is never killed. Once that process is gone it also kills
//! whatever is left of its process group (descendants that ignored SIGTERM, or outlived a
//! runtime that had already exited), again only processes carrying the thread's marker.
//!
//! Every runtime also gets [`RUNTIME_GEN_ENV`] (which start of the thread's runtime it is): a
//! kill of one runtime while its process may be gone and the thread's next one may already
//! run ([`reap_marked`] / [`runtime_running`] with a generation) touches only its own.
//!
//! The process table is read from `/proc` on Linux and with the `ps` command elsewhere
//! (macOS, [`ps`]); there a start time is seconds since the epoch instead of clock ticks after
//! boot (start times are only compared on the machine that read them, and only `/proc`'s are
//! ordered: a wall clock can be set back). `/proc/<pid>/stat` is read as bytes: a command name
//! need not be UTF-8. Whatever cannot be
//! read (an environment `ps` does not show, an unparseable line) never matches, so it is
//! never signalled. A process table that cannot be read at all (`ps` failed or timed out, or
//! `/proc` is not there: a missing record is "gone" only while this process's own reads) is
//! unknown, never "gone": the process counts as still running ([`runtime_running`],
//! [`attach_process_running`]) and nothing is signalled for it. Signals are sent with the
//! `kill` command (no libc binding).

use std::time::{Duration, Instant};

#[cfg(any(not(target_os = "linux"), test, twr_ps))]
mod ps;

/// The process table from `/proc` (on Linux, unless built with `--cfg twr_ps`, which tests the
/// `ps` backend there).
#[cfg(all(target_os = "linux", not(twr_ps)))]
mod sys {
    use super::{is_gone, parse_stat, proc_faults, Lookup, ProcStat};

    /// `/proc/<pid>/<file>` (`pid` may be `self`), read as bytes, unless [`proc_faults`] fails
    /// it on this thread (a test hook).
    fn read(pid: &str, file: &str) -> std::io::Result<Vec<u8>> {
        proc_faults::check(pid)?;
        std::fs::read(format!("/proc/{pid}/{file}"))
    }

    /// The entries of `/proc` (unless [`proc_faults`] says it is not there).
    fn list() -> std::io::Result<std::fs::ReadDir> {
        proc_faults::check_table()?;
        std::fs::read_dir("/proc")
    }

    /// Whether the process table is there: this process's own record reads and parses. Read
    /// at most once, on first use (one per walk).
    #[derive(Debug, Default)]
    pub struct Table(std::cell::OnceCell<bool>);

    impl Table {
        fn readable(&self) -> bool {
            *self
                .0
                .get_or_init(|| read("self", "stat").is_ok_and(|b| parse_stat(&b).is_some()))
        }

        /// `if_readable` while the process table is there, else [`Lookup::Unknown`].
        fn or_unknown(&self, if_readable: Lookup) -> Lookup {
            if self.readable() {
                if_readable
            } else {
                Lookup::Unknown
            }
        }
    }

    /// [`stat_in`] with the process table checked afresh.
    pub fn stat(pid: u32) -> Lookup {
        stat_in(pid, &Table::default())
    }

    /// `/proc/<pid>/stat`, read as bytes (the command name is what the process was exec'd as,
    /// cut to 15 bytes: not necessarily UTF-8). When it does not exist
    /// (`ENOENT`, `ESRCH`: the process went away while it was read): [`Lookup::Gone`] if the
    /// process table is there (`table`), else [`Lookup::Unknown`]: without `/proc` every
    /// process looks gone. When it cannot be read or parsed:
    /// [`Lookup::Unreadable`] if the table is there (this one record is not), else
    /// [`Lookup::Unknown`].
    pub fn stat_in(pid: u32, table: &Table) -> Lookup {
        match read(&pid.to_string(), "stat") {
            Ok(bytes) => parse_stat(&bytes)
                .map_or_else(|| table.or_unknown(Lookup::Unreadable), Lookup::Found),
            Err(e) if is_gone(&e) => table.or_unknown(Lookup::Gone),
            Err(_) => table.or_unknown(Lookup::Unreadable),
        }
    }

    /// `/proc/<pid>/environ` (NUL-separated).
    pub fn environ(pid: u32) -> std::io::Result<Vec<u8>> {
        read(&pid.to_string(), "environ")
    }

    /// Who runs process `pid` (`self`: this process) and its parent, from the `Uid:`, `Gid:`
    /// and `PPid:` lines of `/proc/<pid>/status` (read as bytes: the `Name:` line need not be
    /// UTF-8). An error when it cannot be read, or lacks one of them.
    pub fn ids(pid: &str) -> std::io::Result<Ids> {
        let status = read(pid, "status")?;
        let field = |key: &str| {
            status
                .split(|b| *b == b'\n')
                .find_map(|l| l.strip_prefix(key.as_bytes()))
                .and_then(|v| std::str::from_utf8(v).ok())
                .map(|v| v.split_whitespace().filter_map(|n| n.parse::<u32>().ok()))
        };
        let four = |key: &str| -> Option<[u32; 4]> {
            let v: Vec<u32> = field(key)?.collect();
            v.try_into().ok()
        };
        let bad = || std::io::Error::other(format!("no ids in /proc/{pid}/status"));
        Ok(Ids {
            uid: four("Uid:").ok_or_else(bad)?,
            gid: four("Gid:").ok_or_else(bad)?,
            ppid: field("PPid:").and_then(|mut v| v.next()).ok_or_else(bad)?,
        })
    }

    /// A process's user and group ids (real, effective, saved, filesystem) and its parent's
    /// pid ([`ids`]).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct Ids {
        pub uid: [u32; 4],
        pub gid: [u32; 4],
        pub ppid: u32,
    }

    impl Ids {
        /// Whether a process with these ids is run by `us` as far as reading its environment
        /// goes: its real, effective and saved user and group ids are all `us`'s filesystem
        /// ones (what Linux checks before it shows `/proc/<pid>/environ` to another process;
        /// a setuid or setgid program the user runs is another user's here).
        pub fn same_owner(&self, us: &Ids) -> bool {
            self.uid[..3].iter().all(|u| *u == us.uid[3])
                && self.gid[..3].iter().all(|g| *g == us.gid[3])
        }

        /// Whether a process with these ids runs as `us`'s user, whatever its groups: its
        /// real, effective or saved user id is `us`'s effective one. A runtime's descendant
        /// that changed its group (`sg`, `newgrp`) is still this user's, and its environment
        /// may then be closed to this process ([`Ids::same_owner`] is false): the recovery
        /// must count it ([`super::super::leftover`]), not leave it out as another user's.
        pub fn same_user(&self, us: &Ids) -> bool {
            self.uid[..3].contains(&us.uid[1])
        }
    }

    /// `/proc/<pid>/cmdline` (NUL-separated, `argv[0]` first).
    pub fn cmdline(pid: u32) -> Option<Vec<u8>> {
        read(&pid.to_string(), "cmdline").ok()
    }

    /// The arguments of process `pid`, `argv[0]` first (`/proc/<pid>/cmdline`).
    pub fn args(pid: u32) -> Option<Vec<String>> {
        let cmdline = cmdline(pid)?;
        Some(
            cmdline
                .split(|b| *b == 0)
                .filter(|a| !a.is_empty())
                .map(|a| String::from_utf8_lossy(a).into_owned())
                .collect(),
        )
    }

    /// Whether process `pid` runs as this process's user, whatever its groups
    /// ([`Ids::same_user`]); `None` when that cannot be read (it went away, or `/proc` hides
    /// it). For the recovery's candidates ([`super::leftover`]).
    pub fn same_user(pid: u32) -> Option<bool> {
        let us = ids("self").ok()?;
        Some(ids(&pid.to_string()).ok()?.same_user(&us))
    }

    /// [`scan`], also listing every process `keep` accepts whose environment cannot be read
    /// although this user runs it (a non-dumpable process: `/proc/<pid>/environ` is then
    /// closed to its own user) and that is in this process's control group, with `None` for
    /// its environment. Another user's process is left out (a runtime runs as this user; its
    /// groups do not matter, [`Ids::same_user`]), and
    /// so is one in another control group: `sshd` makes its session processes non-dumpable,
    /// and the SSH session of a tunnel to this box must not pass for a runtime's descendant
    /// (a runtime's descendants stay in its runner's control group, a systemd service's say,
    /// whatever session or group they move to; a control group that cannot be read counts as
    /// the same). `None` when `/proc` cannot be listed.
    pub fn scan_unresolved(keep: impl Fn(u32, &ProcStat) -> bool) -> Option<Vec<super::Candidate>> {
        let dir = list().ok()?;
        Some(
            dir.filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok())
                .filter_map(|pid| {
                    let Lookup::Found(stat) = stat(pid) else {
                        return None;
                    };
                    keep(pid, &stat).then_some(())?;
                    match environ(pid) {
                        Ok(env) => Some((pid, stat, Some(env))),
                        Err(e) if is_gone(&e) => None,
                        Err(_) => {
                            let ours =
                                match (read("self", "cgroup"), read(&pid.to_string(), "cgroup")) {
                                    (Ok(a), Ok(b)) => a == b,
                                    _ => true,
                                };
                            (ours && same_user(pid)?).then_some((pid, stat, None))
                        }
                    }
                })
                .collect(),
        )
    }

    /// Every process of group `pgid` with its status. `None` when `/proc` cannot be listed.
    pub fn group(pgid: i64) -> Option<Vec<(u32, ProcStat)>> {
        let dir = list().ok()?;
        Some(
            dir.filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok())
                .filter_map(|pid| match stat(pid) {
                    Lookup::Found(s) if s.pgrp == pgid => Some((pid, s)),
                    _ => None,
                })
                .collect(),
        )
    }

    /// Every process `keep` accepts (given its pid and stat) whose environment can be read,
    /// with that environment. `None` when `/proc` cannot be listed.
    pub fn scan(keep: impl Fn(u32, &ProcStat) -> bool) -> Option<Vec<(u32, ProcStat, Vec<u8>)>> {
        let dir = list().ok()?;
        Some(
            dir.filter_map(|e| e.ok()?.file_name().to_str()?.parse::<u32>().ok())
                .filter_map(|pid| {
                    let Lookup::Found(stat) = stat(pid) else {
                        return None;
                    };
                    keep(pid, &stat).then_some(())?;
                    Some((pid, stat, environ(pid).ok()?))
                })
                .collect(),
        )
    }
}

/// The process table from `ps` (see [`ps`]).
#[cfg(any(not(target_os = "linux"), twr_ps))]
use ps as sys;

/// Test hook for the `/proc` backend ([`with_proc_faults`]): what the reads of the current
/// thread find wrong with `/proc` (reads of `/proc/self`, which `hidepid` never hides, fail only
/// when it is not there). Doc-hidden: nothing in twr sets it outside tests.
#[doc(hidden)]
#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcFaults {
    /// `/proc` is not there: every read fails with `ENOENT`, this process's own too.
    pub unavailable: bool,
    /// The records of these processes cannot be read (`EPERM`: what procfs mounted with
    /// `hidepid=1` answers for a process it does not show this user).
    pub denied: Vec<u32>,
    /// These processes are not there (`ENOENT`: procfs mounted with `hidepid=2`).
    pub hidden: Vec<u32>,
}

/// Runs `f` with the `/proc` reads of the current thread failing as `faults` says (a test
/// hook; the `ps` backend ignores it).
#[doc(hidden)]
#[cfg(target_os = "linux")]
pub fn with_proc_faults<R>(faults: ProcFaults, f: impl FnOnce() -> R) -> R {
    let prev = proc_faults::FAULTS.with(|x| x.replace(Some(faults)));
    let out = f();
    proc_faults::FAULTS.with(|x| *x.borrow_mut() = prev);
    out
}

/// The faults [`with_proc_faults`] set for the current thread.
#[cfg(target_os = "linux")]
mod proc_faults {
    use super::ProcFaults;

    thread_local! {
        pub static FAULTS: std::cell::RefCell<Option<ProcFaults>> =
            const { std::cell::RefCell::new(None) };
    }

    /// The error a read of `/proc/<pid>/...` fails with, if any. `self` (this process, which
    /// `hidepid` never hides from itself) fails only when `/proc` is not there.
    #[cfg(not(twr_ps))]
    pub fn check(pid: &str) -> std::io::Result<()> {
        let n: Option<u32> = pid.parse().ok();
        FAULTS.with(|f| match &*f.borrow() {
            Some(f) if f.unavailable || n.is_some_and(|n| f.hidden.contains(&n)) => {
                Err(std::io::ErrorKind::NotFound.into())
            }
            // EPERM.
            Some(f) if n.is_some_and(|n| f.denied.contains(&n)) => {
                Err(std::io::Error::from_raw_os_error(1))
            }
            _ => Ok(()),
        })
    }

    /// The error a listing of `/proc` fails with, if any.
    #[cfg(not(twr_ps))]
    pub fn check_table() -> std::io::Result<()> {
        FAULTS.with(|f| match &*f.borrow() {
            Some(f) if f.unavailable => Err(std::io::ErrorKind::NotFound.into()),
            _ => Ok(()),
        })
    }
}

/// A process a recovery scan found ([`leftover`]): its pid and status, and its environment
/// (`None`: this user's, but it cannot be read).
type Candidate = (u32, ProcStat, Option<Vec<u8>>);

/// Environment variable set on every runtime process: the id of the thread it runs.
pub const THREAD_ENV: &str = "TWR_THREAD";

/// Environment variable set on every runtime process next to [`THREAD_ENV`]: which start of
/// the thread's runtime it belongs to (the hub's runtime generation for a local runtime, the
/// run id on a runner's box). Descendants inherit it. [`reap_marked`] / [`runtime_running`]
/// given a generation only match processes carrying it, so the cleanup of one runtime never
/// touches the thread's next one.
pub const RUNTIME_GEN_ENV: &str = "TWR_RUNTIME_GEN";

/// Environment variable set on the interactive process of an attach (`twr attach`; the hub
/// puts it in the attach command's environment): `<thread id>:<lease id>`. That process never
/// carries [`THREAD_ENV`] / [`RUNTIME_GEN_ENV`], so no runtime kill touches it; the hub uses
/// this marker (with the pid and start time the client reports) to tell whether the attach's
/// process still runs ([`attach_process_running`]).
pub const ATTACH_ENV: &str = "TWR_ATTACH";

/// Environment variable set on every process twr starts for a thread on the hub's machine:
/// each runtime (`<thread id>:<runtime generation>`, next to [`THREAD_ENV`])
/// and each attach's interactive process (`<thread id>:attach.<lease id>`, next to
/// [`ATTACH_ENV`]). Descendants inherit it, also once they have left their parent (a
/// background job whose shell exited, reparented to init), so the hub finds a thread's agent
/// by it where the process's ancestry no longer shows where it came from ([`agent_marker`]).
pub const AGENT_ENV: &str = "TWR_AGENT";

/// How long a stale runtime gets to exit after SIGTERM before SIGKILL.
const TERM_GRACE: Duration = Duration::from_secs(3);

/// How long to wait for a stale runtime to disappear after SIGKILL.
const KILL_GRACE: Duration = Duration::from_secs(2);

/// A runtime process recorded as live when its hub or runner starts (it may have survived a
/// crash of the process that started it, or its pid may belong to something else by now).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleProcess {
    /// Thread the process ran.
    pub thread: String,
    /// `ext.twr.pid`.
    pub pid: u32,
    /// `ext.twr.start_time` ([`process_start_time`]); without it nothing is killed.
    pub start_time: Option<u64>,
    /// `ext.twr.exe`, if the adapter recorded one (then `argv[0]` must match too).
    pub exe: Option<String>,
    /// The runtime generation ([`RUNTIME_GEN_ENV`]) the process was started with, when the
    /// caller means one run of the thread: then the process, and every member of its group
    /// that is swept, must carry that marker too, so a kill meant for one run never reaches
    /// another run of the thread. `None` (a previous hub's or runner's runtime): any
    /// generation.
    pub generation: Option<String>,
}

/// Kills a runtime process that survived the crash of the process that started it and
/// waits (bounded) for it to exit. Only a process whose start time and [`THREAD_ENV`]
/// marker (and [`RUNTIME_GEN_ENV`] marker, when `p` names a generation) match `p` is touched.
/// Then kills the surviving members of its process group that carry the same markers
/// (SIGTERM, then SIGKILL, with bounded waits), also when the runtime itself was already gone.
/// Returns whether a process was signalled. Blocks; call off the async runtime.
pub fn reap_stale(p: &StaleProcess) -> bool {
    let pid = p.pid;
    let Some(start_time) = p.start_time else {
        return false;
    };
    if p.thread.is_empty() {
        return false;
    }
    let current = read_stat(pid);
    let leader = match current {
        Lookup::Found(stat) => Some(stat).filter(|stat| {
            let Ok(environ) = sys::environ(pid) else {
                return false;
            };
            let cmdline = p.exe.as_ref().and_then(|_| sys::cmdline(pid));
            is_stale_runtime(p, stat, &environ, cmdline.as_deref())
        }),
        Lookup::Gone | Lookup::Unknown | Lookup::Unreadable => None,
    };
    // The group to sweep afterwards: the runtime's, or, when no process has its pid any more
    // (or only a zombie), the one it led (adapters start runtimes as group leaders,
    // pgid = pid). A live process that is not the runtime means the pid was reused; its
    // group is left alone. Members must also carry the thread's marker and have started
    // no earlier than the runtime. A process table that cannot be read sweeps nothing.
    let pgid = match (leader, current) {
        (Some(stat), _) => Some(stat.pgrp),
        (None, Lookup::Gone) => Some(i64::from(pid)),
        (None, Lookup::Found(stat)) if matches!(stat.state, 'Z' | 'X' | 'x') => {
            Some(i64::from(pid))
        }
        (None, Lookup::Found(_)) => None,
        (None, Lookup::Unknown | Lookup::Unreadable) => {
            tracing::warn!(
                pid,
                thread = %p.thread,
                "cannot read the process table; leaving the recorded runtime alone"
            );
            None
        }
    };
    let mut signalled = false;
    if let Some(stat) = leader {
        // Signal the whole group only when the runtime still leads it, so an unrelated
        // group is never hit.
        let group = stat.pgrp == i64::from(pid);
        tracing::warn!(
            pid,
            thread = %p.thread,
            group,
            "killing runtime process left over from a previous hub"
        );
        signalled = true;
        signal(pid, group, "TERM");
        match wait_gone(pid, start_time, TERM_GRACE) {
            Alive::No => {}
            Alive::Yes => {
                tracing::warn!(pid, "stale runtime ignored SIGTERM; sending SIGKILL");
                signal(pid, group, "KILL");
                if wait_gone(pid, start_time, KILL_GRACE) != Alive::No {
                    tracing::error!(pid, "stale runtime may still be running after SIGKILL");
                }
            }
            // Only a process seen to still be the runtime is killed.
            Alive::Unknown => tracing::warn!(
                pid,
                "cannot read the process table; not sending SIGKILL to the stale runtime"
            ),
        }
    }
    match pgid {
        Some(pgid) => reap_group(pgid, &p.thread, p.generation.as_deref(), start_time) || signalled,
        None => signalled,
    }
}

/// Kills every process marked for `thread` ([`THREAD_ENV`]) that started at or after
/// `min_start`, with what is left of its process group ([`reap_stale`] on each, so a group is
/// only swept when the process leads it, and only its marked members). With `gen`, only
/// processes of that runtime generation ([`RUNTIME_GEN_ENV`]) are touched: a later runtime of
/// the thread is left alone. For a runtime whose pid was never recorded: its runner died
/// while the runtime initialized. Returns whether anything was signalled. Blocks; call off
/// the async runtime.
pub fn reap_marked(thread: &str, min_start: u64, gen: Option<&str>) -> bool {
    let mut signalled = false;
    // Leaders first: killing one takes its group along.
    let Some(mut found) = marked_processes(thread, min_start, gen) else {
        tracing::warn!(thread, "cannot read the process table; killing nothing");
        return false;
    };
    found.sort_by_key(|(pid, _, pgrp)| i64::from(*pid) != *pgrp);
    for (pid, start_time, _) in found {
        signalled |= reap_stale(&StaleProcess {
            thread: thread.to_string(),
            pid,
            start_time: Some(start_time),
            exe: None,
            generation: gen.map(str::to_string),
        });
    }
    signalled
}

/// Kills every live process whose [`AGENT_ENV`] is one of `markers` (never this one):
/// SIGTERM, a bounded wait, SIGKILL for those still themselves, another bounded wait; rescanned
/// until none is left (at most [`GROUP_SWEEPS`] times, as they may fork meanwhile). For what a
/// stopped hub left running under markers only it gave out, such as its jobs'. Returns whether
/// anything was signalled. Blocks; call off the async runtime.
pub fn reap_agent_marked(markers: &[String]) -> bool {
    let me = std::process::id();
    let mut signalled = false;
    if markers.is_empty() {
        return false;
    }
    for _ in 0..GROUP_SWEEPS {
        let Some(found) =
            sys::scan(|pid, stat| pid != me && !matches!(stat.state, 'Z' | 'X' | 'x'))
        else {
            tracing::warn!("cannot read the process table; killing nothing");
            return signalled;
        };
        let marked: Vec<(u32, u64)> = found
            .into_iter()
            .filter(|(_, _, environ)| {
                environ_value(environ, AGENT_ENV).is_some_and(|m| markers.contains(&m))
            })
            .map(|(pid, stat, _)| (pid, stat.start_time))
            .collect();
        if marked.is_empty() {
            return signalled;
        }
        signalled = true;
        tracing::warn!(pids = ?marked.iter().map(|m| m.0).collect::<Vec<_>>(), "killing processes a stopped hub left running");
        // Each still the process that was matched (its start time), checked right before it
        // is signalled: a pid reused meanwhile is someone else's.
        let signal_same = |pid: u32, start: u64, sig: &str| {
            if liveness(pid, start) == Alive::Yes {
                signal(pid, false, sig);
            }
        };
        for &(pid, start) in &marked {
            signal_same(pid, start, "TERM");
        }
        let left: Vec<(u32, u64)> = wait_all_gone(&marked, TERM_GRACE)
            .into_iter()
            .filter(|(_, _, alive)| *alive == Alive::Yes)
            .map(|(pid, start, _)| (pid, start))
            .collect();
        for &(pid, start) in &left {
            signal_same(pid, start, "KILL");
        }
        wait_all_gone(&left, KILL_GRACE);
    }
    signalled
}

/// What [`reap_job_group`] found of a job's process group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reaped {
    /// None of it runs any more (it was killed, or was gone already).
    Gone,
    /// The group id names another process's group now: the job's group is gone (its id is not
    /// handed out again while any of its processes is in it).
    NotOurs,
    /// Whether any of it still runs cannot be told (the process table could not be read), or
    /// some of it survived SIGKILL.
    Unknown,
}

/// Kills what is left of a job's process group `pgid`, whose leader (process `pgid`) started
/// at `start_time` as the hub that ran the job recorded it before the job's command ran, and
/// of which the hub recorded `members` (pid and start time) while the job ran: every member,
/// whatever its environment says (a job's process may have cleared its markers, or hidden its
/// environment), SIGTERM to the group, a bounded wait, then SIGKILL and another.
///
/// The group is signalled only while it is proven the job's, checked right before each
/// signal: its leader runs with the recorded start time, or, the leader gone, a member known
/// to be the job's still runs in it (a recorded one at first, then any seen in the group
/// while it was proven the job's: a member that forks between the signals keeps it so). A
/// group id is not handed out again while any process is in that group, so a known member
/// still in it proves it. A live process with the group's id that started at another time
/// means the job's group is gone ([`Reaped::NotOurs`]). A group with neither (no leader and
/// no known member, though it has members) cannot be told from another that got the id
/// after the job's was empty: it is left alone and reported [`Reaped::Unknown`], as is one
/// whose state cannot be read or that survives SIGKILL. Never this process's own group.
/// Blocks; call off the async runtime.
pub fn reap_job_group(pgid: u32, start_time: u64, members: &[(u32, u64)]) -> Reaped {
    let me = std::process::id();
    if pgid <= 1 || pgid == me || process_group(me) == Some(i64::from(pgid)) {
        return Reaped::NotOurs;
    }
    let mut known = members.to_vec();
    for (sig, grace) in [("TERM", TERM_GRACE), ("KILL", KILL_GRACE)] {
        #[cfg(test)]
        tests::before_group_signal(pgid, sig);
        match job_group_now(pgid, start_time, &known) {
            GroupNow::Empty => return Reaped::Gone,
            GroupNow::Other => {
                tracing::info!(
                    pgid,
                    "a job's process group id names another process's group now; leaving it alone"
                );
                return Reaped::NotOurs;
            }
            GroupNow::Unproven => {
                tracing::error!(
                    pgid,
                    "a lost job's process group has members none of which is known to be the \
                     job's (its leader is gone): leaving it alone"
                );
                return Reaped::Unknown;
            }
            GroupNow::Unknown => {
                tracing::warn!(
                    pgid,
                    "cannot read the process table; leaving a lost job's process group alone"
                );
                return Reaped::Unknown;
            }
            GroupNow::Ours(now) => known = now,
        }
        tracing::warn!(
            pgid,
            sig,
            "killing what is left of a job's process group a stopped hub ran"
        );
        signal(pgid, true, sig);
        if wait_group_gone(pgid, grace) {
            return Reaped::Gone;
        }
    }
    tracing::error!(
        pgid,
        "a lost job's process group may still run after SIGKILL"
    );
    Reaped::Unknown
}

/// What process group `pgid` is now, for [`reap_job_group`].
#[derive(Debug, PartialEq, Eq)]
enum GroupNow {
    /// No member runs.
    Empty,
    /// The job's (proven): its running members (pid, start time).
    Ours(Vec<(u32, u64)>),
    /// Another's: a running process with the group's id started at another time than the
    /// recorded leader.
    Other,
    /// It has members, but neither its recorded leader nor a member known to be the job's.
    Unproven,
    /// The process table could not be read.
    Unknown,
}

/// What process group `pgid`, recorded as led by a process started at `start_time`, with
/// `known` members known to be the job's, is now.
fn job_group_now(pgid: u32, start_time: u64, known: &[(u32, u64)]) -> GroupNow {
    let leader = match read_stat(pgid) {
        Lookup::Found(stat) if !matches!(stat.state, 'Z' | 'X' | 'x') => Some(stat.start_time),
        Lookup::Found(_) | Lookup::Gone => None,
        Lookup::Unknown | Lookup::Unreadable => return GroupNow::Unknown,
    };
    let Some(members) = group_members_with_start(pgid) else {
        return GroupNow::Unknown;
    };
    group_now(leader, start_time, members, known)
}

/// [`job_group_now`] once read: `leader` the start time of the running process with the
/// group's id, if any; `members` its running members.
fn group_now(
    leader: Option<u64>,
    start_time: u64,
    members: Vec<(u32, u64)>,
    known: &[(u32, u64)],
) -> GroupNow {
    match leader {
        Some(t) if t != start_time => GroupNow::Other,
        _ if members.is_empty() => GroupNow::Empty,
        Some(_) => GroupNow::Ours(members),
        None if members.iter().any(|m| known.contains(m)) => GroupNow::Ours(members),
        None => GroupNow::Unproven,
    }
}

/// Up to `max` running members of process group `pgid` (zombies aside), as (pid, start
/// time), the leader left out: what a hub records of a job's group while the job runs, for
/// [`reap_job_group`]. `None` when the process table cannot be read. Blocks.
pub fn job_group_members(pgid: u32, max: usize) -> Option<Vec<(u32, u64)>> {
    Some(members_kept(sys::group(i64::from(pgid))?, pgid, max))
}

/// [`job_group_members`] from the group's listing.
fn members_kept(listed: Vec<(u32, ProcStat)>, pgid: u32, max: usize) -> Vec<(u32, u64)> {
    let mut members = running_members(listed);
    members.retain(|(pid, _)| *pid != pgid);
    members.sort_by_key(|(_, start)| *start);
    members.truncate(max);
    members
}

/// The running members of process group `pgid` (zombies aside), as (pid, start time); `None`
/// when the process table cannot be read.
fn group_members_with_start(pgid: u32) -> Option<Vec<(u32, u64)>> {
    Some(running_members(sys::group(i64::from(pgid))?))
}

/// The running members (zombies aside) of a group's listing ([`sys::group`]), as (pid, start
/// time): as the one listing read them, with no read per member (on the `ps` backend each
/// would be a `ps` call of its own, for every member of a job's group every
/// few seconds).
fn running_members(listed: Vec<(u32, ProcStat)>) -> Vec<(u32, u64)> {
    listed
        .into_iter()
        .filter(|(_, stat)| !matches!(stat.state, 'Z' | 'X' | 'x'))
        .map(|(pid, stat)| (pid, stat.start_time))
        .collect()
}

/// Waits (bounded) until no member of process group `pgid` runs (zombies aside); whether none
/// does. A process table that cannot be read counts as running.
fn wait_group_gone(pgid: u32, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    loop {
        if group_running(pgid) == Some(false) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Whether `chain` (a process and its ancestors, nearest first, as a walk read them) still
/// holds, read again now: each process still runs (not a zombie) with the start time read,
/// and each one's parent is still the next. A process that exited after the walk read it
/// stays readable as a zombie with its old parent until it is reaped, while its children have
/// a new parent at once: this sees either. `false` for an empty chain, or when a process
/// cannot be read. Blocks.
pub fn chain_holds(chain: &[ProcessId]) -> bool {
    !chain.is_empty()
        && chain
            .iter()
            .enumerate()
            .all(|(i, p)| match read_stat(p.pid) {
                Lookup::Found(stat) => {
                    stat.start_time == p.start_time
                        && !matches!(stat.state, 'Z' | 'X' | 'x')
                        && chain
                            .get(i + 1)
                            .is_none_or(|parent| stat.ppid == parent.pid)
                }
                Lookup::Gone | Lookup::Unknown | Lookup::Unreadable => false,
            })
}

/// Which of `markers` a live process (never this one) still carries as its [`AGENT_ENV`],
/// after [`reap_agent_marked`]: `None` when the process table cannot be read. A process whose
/// environment cannot be read is not seen. Blocks.
pub fn agent_markers_left(markers: &[String]) -> Option<Vec<String>> {
    let me = std::process::id();
    let found = sys::scan(|pid, stat| pid != me && !matches!(stat.state, 'Z' | 'X' | 'x'))?;
    let mut left: Vec<String> = found
        .into_iter()
        .filter_map(|(_, _, environ)| environ_value(&environ, AGENT_ENV))
        .filter(|m| markers.contains(m))
        .collect();
    left.sort();
    left.dedup();
    Some(left)
}

/// Whether a runtime of `thread` may still run: the process `p` describes (the same pid and
/// start time, not a zombie), or any process marked for `thread` ([`THREAD_ENV`]) that started
/// at or after `min_start` (with `gen`: of that runtime generation, [`RUNTIME_GEN_ENV`]).
/// What [`reap_stale`] / [`reap_marked`] leave behind, checked before an exit is recorded
/// that nobody observed. A process table that cannot be read counts as running. Blocks
/// (reads the process table); call off the async runtime.
pub fn runtime_running(
    thread: &str,
    p: Option<&StaleProcess>,
    min_start: Option<u64>,
    gen: Option<&str>,
) -> bool {
    let recorded = p.is_some_and(recorded_running);
    recorded
        || min_start
            .is_some_and(|m| marked_processes(thread, m, gen).is_none_or(|found| !found.is_empty()))
}

/// Whether this backend's start times are ordered: `/proc`'s are clock ticks after boot, so a
/// process that started before a bound read on the same boot is older than it. `ps`'s are
/// wall-clock seconds, which a clock set back reorders: there a start time never proves that a
/// process is not a runtime's ([`leftover`]).
const ORDERED_START_TIMES: bool = cfg!(all(target_os = "linux", not(twr_ps)));

/// Why a runtime of `thread` that a previous runner left behind may still run, once it was
/// killed ([`reap_stale`], [`reap_marked`]); `None` only once its absence is established. For
/// the recovery of a runner's box, which records no exit while this is `Some`:
///
/// - the process table cannot be read;
/// - `p`, the recorded process, still runs: with its start time, the same pid and start time;
///   without it (never recorded), any live process with its pid that is not another user's,
///   marked or not (the runtime may have exec'd without its marker), unless its start time
///   proves it is another process: earlier than `min_start`, on a backend whose start times
///   are ordered ([`ORDERED_START_TIMES`]);
/// - a live process carries the thread's marker ([`THREAD_ENV`]): one that started at or
///   after `min_start` (0 when unknown) where start times are ordered, any where they are not;
/// - with a known `min_start` only, a process run by this user that started at or after it
///   (in this process's control group on Linux), has an environment that cannot be read (a
///   non-dumpable process, see [`sys::scan_unresolved`]), still a moment later, and is tied to
///   the runtime ([`tied_to_runtime`]: its process group, or a parent that is the runtime or
///   marked for the thread): it cannot be told from the runtime's descendant. Without a lower bound every such process since boot would count
///   (an SSH session's, say), so none does. Where start times are not ordered this bound is
///   a best effort (counting every such process would keep a box closed for good).
///
/// Blocks (reads the process table); call off the async runtime.
pub fn leftover(thread: &str, p: Option<&StaleProcess>, min_start: Option<u64>) -> Option<String> {
    if let Some(p) = p {
        let gone = match p.start_time {
            Some(t) => liveness(p.pid, t) == Alive::No,
            None => !recorded_pid_may_run(p.pid, min_start),
        };
        if !gone {
            return Some(format!("process {} may still run", p.pid));
        }
    }
    let me = std::process::id();
    let from = min_start.unwrap_or(0);
    let Some(found) = sys::scan_unresolved(|pid, stat| {
        pid != me
            && (!ORDERED_START_TIMES || stat.start_time >= from)
            && !matches!(stat.state, 'Z' | 'X' | 'x')
    }) else {
        return Some("the process table cannot be read".into());
    };
    let mut left: Vec<String> = Vec::new();
    let mut hidden: Vec<(u32, ProcStat)> = Vec::new();
    for (pid, stat, environ) in found {
        match environ {
            Some(e) if is_marked(&e, thread, None) => {
                left.push(format!("process {pid} still runs"));
            }
            Some(_) => {}
            None if min_start.is_some_and(|m| stat.start_time >= m)
                && tied_to_runtime(&stat, thread, p) =>
            {
                hidden.push((pid, stat));
            }
            None => {}
        }
    }
    // A process caught exiting or inside an execve can read as unreadable for a moment: only
    // one that still is a moment later, unchanged, counts.
    if !hidden.is_empty() {
        std::thread::sleep(HIDDEN_SETTLE);
        left.extend(
            hidden
                .into_iter()
                .filter(|(pid, stat)| still_hidden(*pid, stat))
                .map(|(pid, _)| {
                    format!("process {pid} may be one of it (its environment cannot be read)")
                }),
        );
    }
    left.sort();
    (!left.is_empty()).then(|| left.join(", "))
}

/// How long [`leftover`] waits before it reads a process whose environment could not be read
/// again ([`still_hidden`]).
const HIDDEN_SETTLE: Duration = Duration::from_millis(150);

/// How many parents [`tied_to_runtime`] walks up at most.
const TIE_DEPTH: usize = 64;

/// Whether a process whose environment cannot be read is tied to the recorded runtime `p` of
/// `thread` ([`leftover`]), so that it may be one of its descendants: it is in the runtime's
/// process group (adapters start a runtime as a group leader, and the group outlives its
/// leader while a member is left), or one of its ancestors (up to [`TIE_DEPTH`] parents, as
/// far as they can be read) is the runtime or carries the thread's marker. A process of the
/// user's that is not (an `ssh-agent`, which is setgid and so unreadable, or a `sudo` started
/// in the runner's terminal after it) is no reason to keep the box closed. A descendant that
/// left the runtime's group and whose marked ancestors are all gone cannot be told from
/// those: it does not count either.
fn tied_to_runtime(stat: &ProcStat, thread: &str, p: Option<&StaleProcess>) -> bool {
    if p.is_some_and(|p| stat.pgrp == i64::from(p.pid)) {
        return true;
    }
    let mut ppid = stat.ppid;
    for _ in 0..TIE_DEPTH {
        if ppid <= 1 {
            return false;
        }
        if p.is_some_and(|p| p.pid == ppid) {
            return true;
        }
        let Lookup::Found(parent) = read_stat(ppid) else {
            return false;
        };
        if sys::environ(ppid).is_ok_and(|e| is_marked(&e, thread, None)) {
            return true;
        }
        ppid = parent.ppid;
    }
    false
}

/// Whether process `pid`, found with `stat` and an environment that could not be read, is
/// still the same process (start time), alive, and its environment still cannot be read.
fn still_hidden(pid: u32, stat: &ProcStat) -> bool {
    match read_stat(pid) {
        Lookup::Found(now) if now.start_time == stat.start_time => {
            !matches!(now.state, 'Z' | 'X' | 'x') && sys::environ(pid).is_err()
        }
        Lookup::Found(_) | Lookup::Gone => false,
        Lookup::Unknown | Lookup::Unreadable => true,
    }
}

/// Whether the live process with the recorded `pid` of a runtime whose start time was never
/// recorded may be that runtime ([`leftover`]): yes unless it is gone, a zombie, another user's,
/// or (where start times are ordered, [`ORDERED_START_TIMES`]) started before `min_start`. Its
/// environment proves nothing: the runtime may have exec'd without its marker.
fn recorded_pid_may_run(pid: u32, min_start: Option<u64>) -> bool {
    let stat = match read_stat(pid) {
        Lookup::Gone => return false,
        Lookup::Unknown | Lookup::Unreadable => return true,
        Lookup::Found(stat) => stat,
    };
    let older = ORDERED_START_TIMES && min_start.is_some_and(|m| stat.start_time < m);
    if matches!(stat.state, 'Z' | 'X' | 'x') || older {
        return false;
    }
    sys::same_user(pid) != Some(false)
}

/// Whether the recorded process `p` still runs: the same pid and start time, not a zombie, and,
/// when `p` names a generation, still carrying it (another run's process is not `p`; an
/// environment or process table that cannot be read counts as running).
fn recorded_running(p: &StaleProcess) -> bool {
    let Some(t) = p.start_time else {
        return false;
    };
    if !still_running(p.pid, t) {
        return false;
    }
    match (&p.generation, sys::environ(p.pid)) {
        (Some(g), Ok(environ)) => is_marked(&environ, &p.thread, Some(g)),
        _ => true,
    }
}

/// Whether the NUL-separated `environ` marks a process of `thread` ([`THREAD_ENV`]) and, with
/// `gen`, of that runtime generation ([`RUNTIME_GEN_ENV`]).
pub fn is_marked(environ: &[u8], thread: &str, gen: Option<&str>) -> bool {
    !thread.is_empty()
        && environ_has(environ, THREAD_ENV, thread)
        && gen.is_none_or(|g| environ_has(environ, RUNTIME_GEN_ENV, g))
}

/// Live processes marked for `thread` (and `gen`, when given, see [`is_marked`]) that started
/// at or after `min_start`, as `(pid, start time, process group)` (never this process); `None`
/// when the process table cannot be read.
fn marked_processes(
    thread: &str,
    min_start: u64,
    gen: Option<&str>,
) -> Option<Vec<(u32, u64, i64)>> {
    let me = std::process::id();
    if thread.is_empty() {
        return Some(Vec::new());
    }
    let found = sys::scan(|pid, stat| {
        pid != me && stat.start_time >= min_start && !matches!(stat.state, 'Z' | 'X' | 'x')
    })?;
    Some(
        found
            .into_iter()
            .filter(|(_, _, environ)| is_marked(environ, thread, gen))
            .map(|(pid, stat, _)| (pid, stat.start_time, stat.pgrp))
            .collect(),
    )
}

/// How many times [`reap_group`] rescans the group (members may fork while being killed).
const GROUP_SWEEPS: usize = 4;

/// Kills every live process in group `pgid` whose environment has [`THREAD_ENV`]`=<thread>`
/// (and [`RUNTIME_GEN_ENV`]`=<gen>`, when given) and that started at or after `min_start` (the
/// runtime's start time):
/// SIGTERM, a bounded wait, SIGKILL for the rest, another bounded wait; rescanned until the
/// group is empty of such processes (at most [`GROUP_SWEEPS`] times). Returns whether anything
/// was signalled.
fn reap_group(pgid: i64, thread: &str, gen: Option<&str>, min_start: u64) -> bool {
    let mut signalled = false;
    for _ in 0..GROUP_SWEEPS {
        let Some(members) = group_members(pgid, thread, gen, min_start) else {
            tracing::warn!(
                pgid,
                thread,
                "cannot read the process table; not sweeping the group"
            );
            return signalled;
        };
        if members.is_empty() {
            return signalled;
        }
        signalled = true;
        tracing::warn!(
            pgid,
            thread,
            pids = ?members.iter().map(|m| m.0).collect::<Vec<_>>(),
            "killing processes left in a stale runtime's group"
        );
        for (pid, _) in &members {
            signal(*pid, false, "TERM");
        }
        let left = wait_all_gone(&members, TERM_GRACE);
        if left.is_empty() {
            continue;
        }
        // Only members seen to still be themselves are killed; one the process table could
        // not be read for is left alone.
        let (left, unknown): (Vec<_>, Vec<_>) = left
            .into_iter()
            .partition(|(_, _, alive)| *alive == Alive::Yes);
        if !unknown.is_empty() {
            tracing::warn!(
                pgid,
                ?unknown,
                "cannot read the process table; not sending SIGKILL to these group members"
            );
            return true;
        }
        let left: Vec<(u32, u64)> = left.into_iter().map(|(p, s, _)| (p, s)).collect();
        tracing::warn!(pgid, "stale group members ignored SIGTERM; sending SIGKILL");
        for (pid, _) in &left {
            signal(*pid, false, "KILL");
        }
        let left = wait_all_gone(&left, KILL_GRACE);
        if !left.is_empty() {
            tracing::error!(
                pgid,
                ?left,
                "stale group members still running after SIGKILL"
            );
            return true;
        }
    }
    signalled
}

/// Live processes of group `pgid` marked for `thread` (and `gen`, when given), as `(pid, start
/// time)` (never the hub itself); `None` when the process table cannot be read.
fn group_members(
    pgid: i64,
    thread: &str,
    gen: Option<&str>,
    min_start: u64,
) -> Option<Vec<(u32, u64)>> {
    let me = std::process::id();
    let found = sys::scan(|pid, stat| pid != me && stat.pgrp == pgid)?;
    Some(
        found
            .into_iter()
            .filter(|(_, stat, environ)| {
                is_group_member(stat, environ, pgid, thread, min_start)
                    && gen.is_none_or(|g| environ_has(environ, RUNTIME_GEN_ENV, g))
            })
            .map(|(pid, stat, _)| (pid, stat.start_time))
            .collect(),
    )
}

/// Whether a process (its parsed stat and environment) is a live member of stale group
/// `pgid` that belongs to `thread`: in that group, started no earlier than `min_start` (the
/// runtime it descends from), not a zombie, and [`THREAD_ENV`] set to `thread`. Groups 0 and 1
/// (kernel, init) never match.
pub fn is_group_member(
    stat: &ProcStat,
    environ: &[u8],
    pgid: i64,
    thread: &str,
    min_start: u64,
) -> bool {
    pgid > 1
        && stat.pgrp == pgid
        && stat.start_time >= min_start
        && !matches!(stat.state, 'Z' | 'X' | 'x')
        && !thread.is_empty()
        && environ_has(environ, THREAD_ENV, thread)
}

/// Waits (bounded) for every `(pid, start time)` to be gone; returns those that may still run
/// (with what was last seen of them: running, or unknown).
fn wait_all_gone(procs: &[(u32, u64)], within: Duration) -> Vec<(u32, u64, Alive)> {
    let deadline = Instant::now() + within;
    loop {
        let left: Vec<(u32, u64, Alive)> = procs
            .iter()
            .map(|&(pid, start)| (pid, start, liveness(pid, start)))
            .filter(|(_, _, alive)| *alive != Alive::No)
            .collect();
        if left.is_empty() || Instant::now() >= deadline {
            return left;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// The fields of a process's status that stale-process checks use (`/proc/<pid>/stat` on
/// Linux, `ps` elsewhere).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcStat {
    /// Field 3, e.g. `S`, `R`, `Z` (`ps`: the first letter of `stat`).
    pub state: char,
    /// Field 4, the parent's pid (`ps`: `ppid`).
    pub ppid: u32,
    /// Field 5, the process group id.
    pub pgrp: i64,
    /// Field 22, start time in clock ticks after boot (`ps`: `lstart` in seconds since the
    /// Unix epoch).
    pub start_time: u64,
}

/// Parses the bytes of `/proc/<pid>/stat`. The command name (field 2) sits between the first
/// `(` and the last `)` and may contain any byte but NUL: spaces, parentheses, newlines, bytes
/// that are not UTF-8 (a multibyte character cut by the 15-byte limit). It is skipped, and the
/// fields after its last `)`, all ASCII, are counted.
pub fn parse_stat(stat: &[u8]) -> Option<ProcStat> {
    let open = stat.iter().position(|b| *b == b'(')?;
    let close = stat.iter().rposition(|b| *b == b')')?;
    if close < open {
        return None;
    }
    let rest = std::str::from_utf8(&stat[close + 1..]).ok()?;
    let fields: Vec<&str> = rest.split_ascii_whitespace().collect();
    // `fields[0]` is field 3.
    let field = |n: usize| fields.get(n - 3).copied();
    Some(ProcStat {
        state: field(3)?.chars().next()?,
        ppid: field(4)?.parse().ok()?,
        pgrp: field(5)?.parse().ok()?,
        start_time: field(22)?.parse().ok()?,
    })
}

/// Whether the NUL-separated `environ` contains exactly `key=value`.
pub fn environ_has(environ: &[u8], key: &str, value: &str) -> bool {
    let want = format!("{key}={value}");
    environ.split(|b| *b == 0).any(|e| e == want.as_bytes())
}

/// The arguments of process `pid`, `argv[0]` first (`/proc/<pid>/cmdline`; from `ps`, its
/// `args` column split at spaces). `None` when they cannot be read.
pub fn process_args(pid: u32) -> Option<Vec<String>> {
    sys::args(pid)
}

/// What a twr process an agent can descend from is ([`twr_parent`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TwrParent {
    /// A hub (`twr hub`): its runtimes descend from it.
    Hub,
    /// `twr attach`: the agent's CLI it runs, and what that starts, descend from it.
    Attach,
}

/// Whether the process with arguments `args` (`argv[0]` first) is a hub or an attach of twr:
/// `argv[0]`'s basename is `twr` and its subcommand, past the global options (`--socket`,
/// `-w`/`--workspace` and their values) that may come first, is `hub` (not `hub stop`) or
/// `attach`. As bare `twr` starts a hub (`twr --socket <path> --workspace <name> hub`) and as
/// one is run by hand. A word with a `/` is never the subcommand: it is (part of) a path,
/// which `ps` may have split at a space.
pub fn twr_parent(args: &[String]) -> Option<TwrParent> {
    let (argv0, rest) = args.split_first()?;
    if std::path::Path::new(argv0).file_name() != Some(std::ffi::OsStr::new("twr")) {
        return None;
    }
    let mut words = Vec::new();
    let mut rest = rest.iter();
    while let Some(a) = rest.next() {
        match a.as_str() {
            // Their values follow.
            "--socket" | "--workspace" | "-w" => {
                rest.next();
            }
            o if o.starts_with('-') || o.contains('/') => {}
            w => {
                words.push(w);
                if words.len() == 2 {
                    break;
                }
            }
        }
    }
    match (words.first(), words.get(1)) {
        (Some(&"hub"), Some(&"stop")) => None,
        (Some(&"hub"), _) => Some(TwrParent::Hub),
        (Some(&"attach"), _) => Some(TwrParent::Attach),
        _ => None,
    }
}

/// Whether the program of a NUL-separated `cmdline` (basename of `argv[0]`) is `exe`'s basename,
/// byte for byte (an `argv[0]` that is not UTF-8 is never replaced by one that looks alike).
pub fn argv0_matches(cmdline: &[u8], exe: &str) -> bool {
    let argv0 = cmdline.split(|b| *b == 0).next().unwrap_or_default();
    let base = argv0.rsplit(|b| *b == b'/').next().unwrap_or_default();
    !base.is_empty() && base == exe.rsplit('/').next().unwrap_or(exe).as_bytes()
}

/// Whether a process (its parsed stat, environment and command line) is the runtime `p`
/// describes: same start time, not a zombie, [`THREAD_ENV`] set to `p.thread`, [`RUNTIME_GEN_ENV`]
/// set to `p.generation` when it names one, and, when an exe was recorded, the same program.
pub fn is_stale_runtime(
    p: &StaleProcess,
    stat: &ProcStat,
    environ: &[u8],
    cmdline: Option<&[u8]>,
) -> bool {
    let Some(start_time) = p.start_time else {
        return false;
    };
    if stat.start_time != start_time || matches!(stat.state, 'Z' | 'X' | 'x') {
        return false;
    }
    if !is_marked(environ, &p.thread, p.generation.as_deref()) {
        return false;
    }
    match &p.exe {
        Some(exe) => cmdline.is_some_and(|c| argv0_matches(c, exe)),
        None => true,
    }
}

/// What reading one process's status found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lookup {
    /// A process has the pid (maybe a zombie).
    Found(ProcStat),
    /// No process has the pid (or none this process is shown: procfs `hidepid=2`), while the
    /// process table can be read.
    Gone,
    /// The process table could not be read (`ps` failed or timed out, an unparseable line): the
    /// process may run. Counted as running, and never signalled.
    Unknown,
    /// The process table can be read, but this process's record could not be read or parsed
    /// (`/proc/<pid>/stat`): the process may run. Counted as running, never signalled; an
    /// ancestry walk that meets one is unresolved ([`AncestryEnd::Unreadable`]), not unknown.
    Unreadable,
}

fn read_stat(pid: u32) -> Lookup {
    sys::stat(pid)
}

/// Whether a process still runs: yes, no, or unknown (the process table cannot be read).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Alive {
    /// It runs (found, not a zombie, with the expected start time).
    Yes,
    /// It exited: gone, a zombie, or the pid now names a process with another start time.
    No,
    /// The process table could not be read: it may run (count it as running).
    Unknown,
}

/// Whether process `pid` started at `start_time` is still running (a zombie counts as gone:
/// it has exited and only waits for its parent).
fn liveness(pid: u32, start_time: u64) -> Alive {
    process_liveness(pid, Some(start_time))
}

/// Whether process `pid` still runs, as [`Alive`]: a zombie counts as exited, and so does a
/// process whose start time is not `start_time` (when given: a reused pid is another process).
/// [`Alive::Unknown`] when the process table cannot be read. Blocks (reads the process table).
pub fn process_liveness(pid: u32, start_time: Option<u64>) -> Alive {
    alive_of(read_stat(pid), start_time)
}

/// [`process_liveness`] of what the process table said.
fn alive_of(look: Lookup, start_time: Option<u64>) -> Alive {
    match look {
        Lookup::Found(s)
            if start_time.is_none_or(|t| s.start_time == t)
                && !matches!(s.state, 'Z' | 'X' | 'x') =>
        {
            Alive::Yes
        }
        Lookup::Found(_) | Lookup::Gone => Alive::No,
        Lookup::Unknown | Lookup::Unreadable => Alive::Unknown,
    }
}

/// [`liveness`], with unknown counted as running.
fn still_running(pid: u32, start_time: u64) -> bool {
    liveness(pid, start_time) != Alive::No
}

/// Waits (bounded) for process `pid` started at `start_time` to be gone: [`Alive::No`] once it
/// is, else what was last seen of it.
fn wait_gone(pid: u32, start_time: u64, within: Duration) -> Alive {
    let deadline = Instant::now() + within;
    loop {
        let alive = liveness(pid, start_time);
        if alive == Alive::No || Instant::now() >= deadline {
            return alive;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Sends `SIG<sig>` with the `kill` command (no libc binding in this crate), to the process
/// group `pid` when `group`, else to the process alone.
pub fn signal(pid: u32, group: bool, sig: &str) -> bool {
    let target = if group {
        format!("-{pid}")
    } else {
        pid.to_string()
    };
    let mut cmd = std::process::Command::new("kill");
    cmd.args([format!("-{sig}"), "--".into(), target])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // No terminal: whatever `kill` on `PATH` is.
    twr_adapters::process::new_session_std(&mut cmd);
    cmd.status().is_ok_and(|s| s.success())
}

/// Whether process group `pgid` has a member that has not exited (a zombie has: it only waits
/// for its parent to reap it), whoever started it; `None` when the process table cannot be
/// read.
pub fn group_running(pgid: u32) -> Option<bool> {
    let members = sys::group(i64::from(pgid))?;
    Some(
        members
            .iter()
            .any(|(_, stat)| !matches!(stat.state, 'Z' | 'X' | 'x')),
    )
}

/// Start time of process `pid`: on Linux `/proc/<pid>/stat` field 22 (clock ticks after
/// boot), elsewhere `ps`'s `lstart` (seconds since the Unix epoch). Only comparable with other
/// start times read on the same machine. `None` when there is no such process or the process
/// table cannot be read.
pub fn process_start_time(pid: u32) -> Option<u64> {
    match read_stat(pid) {
        Lookup::Found(s) => Some(s.start_time),
        Lookup::Gone | Lookup::Unknown | Lookup::Unreadable => None,
    }
}

/// How many times [`own_start_time`] reads, and how long it waits between reads.
const OWN_START_TRIES: usize = 3;
const OWN_START_PAUSE: Duration = Duration::from_millis(200);

/// Start time of this process, read when a hub or runner starts: the lower bound of the
/// start times of every runtime it starts (its spawn intents). Read up to
/// [`OWN_START_TRIES`] times; when it still cannot be read, says loudly that spawn intents
/// are off (a runtime whose hub or runner dies before the runtime reports its process is then
/// neither found nor killed at the next start). Blocks; call off the async runtime.
pub fn own_start_time() -> Option<u64> {
    retry_start_time(
        || process_start_time(std::process::id()),
        OWN_START_TRIES,
        OWN_START_PAUSE,
    )
}

/// [`own_start_time`] with its read, tries and pause given.
fn retry_start_time(read: impl Fn() -> Option<u64>, tries: usize, pause: Duration) -> Option<u64> {
    for i in 0..tries {
        if let Some(t) = read() {
            return Some(t);
        }
        if i + 1 < tries {
            std::thread::sleep(pause);
        }
    }
    tracing::error!(
        tries,
        "cannot read this process's start time: spawn intents are OFF. A runtime started by \
         this process whose starter dies before the runtime reports its process will not be \
         found or killed at the next start (it keeps running). Check that the process table \
         is readable (`/proc`, or `ps` on macOS) and restart."
    );
    None
}

/// Process group of process `pid` (`/proc/<pid>/stat` field 5, or `ps`'s `pgid`).
pub fn process_group(pid: u32) -> Option<i64> {
    match read_stat(pid) {
        Lookup::Found(s) => Some(s.pgrp),
        Lookup::Gone | Lookup::Unknown | Lookup::Unreadable => None,
    }
}

/// Whether process `pid` is known to exist and not be a zombie.
pub fn process_alive(pid: u32) -> bool {
    matches!(read_stat(pid), Lookup::Found(s) if !matches!(s.state, 'Z' | 'X' | 'x'))
}

/// The environment of process `pid`, NUL-separated (`/proc/<pid>/environ`, or what `ps`
/// shows); an error when it cannot be read.
pub fn process_environ(pid: u32) -> std::io::Result<Vec<u8>> {
    sys::environ(pid)
}

/// The command line of process `pid`, NUL-separated with `argv[0]` first (`/proc/<pid>/cmdline`;
/// from `ps`, `argv[0]` alone).
pub fn process_cmdline(pid: u32) -> Option<Vec<u8>> {
    sys::cmdline(pid)
}

/// The [`AGENT_ENV`] value in the environment of process `pid` (`/proc/<pid>/environ`, or
/// `ps -E` on macOS / procps `ps e`: only for processes of the same user), `Some` (maybe
/// empty or malformed: whoever set it, the process says it is a thread's agent) when it has
/// one; an error when the environment cannot be read (no such process any more, or `ps`
/// failed or showed none). What a process was started with: Linux and macOS show the
/// environment it was exec'd with, so a process that unsets the variable keeps it there until
/// it execs again without it. Blocks (reads the process table).
pub fn agent_marker(pid: u32) -> std::io::Result<Option<String>> {
    Ok(environ_value(&sys::environ(pid)?, AGENT_ENV))
}

/// What [`first_agent_marker`] found along a chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChainMarker {
    /// Process `pid`, the nearest of the chain whose environment carries [`AGENT_ENV`], with
    /// that value.
    Carried {
        /// The carrier.
        pid: u32,
        /// Its [`AGENT_ENV`] value (maybe empty or malformed).
        value: String,
    },
    /// No environment that counts carries it (also when `ps` could not list the chain: the
    /// process table is unknown).
    Absent,
    /// None that could be read carries it, but the environment of process `pid` could not be
    /// read while it runs, and it counts: the peer itself, or an ancestor
    /// run by this process's user. It may carry the marker.
    Hidden {
        /// The nearest such process.
        pid: u32,
    },
    /// None that could be read carries it, but process `pid` of the chain had exited when its
    /// environment was to be read (gone, or a zombie, while the process table is there): it
    /// may have carried the marker, and the chain is no longer what the process
    /// descends from (its children were reparented). Comes before [`ChainMarker::Hidden`].
    Gone {
        /// The nearest such process.
        pid: u32,
    },
}

/// The first process of `pids` (the peer of a connection, then its ancestors, nearest first:
/// an [`Ancestry`]'s chain) whose environment carries [`AGENT_ENV`], with that value (see
/// [`agent_marker`]). A process of the chain that cleared the variable still descends from
/// one that carries it: any carrier counts.
///
/// A process that has exited by the time its environment is read ([`ChainMarker::Gone`]:
/// gone, or a zombie, whose environment reads empty or not at all) is not one without the
/// marker: it may have exited after the chain was read, a marked process that handed what it
/// holds to a child. A process table that cannot be read at all hides nothing. An environment
/// that cannot be read carries nothing when it is an ancestor this process cannot read for
/// reasons the agent does not control: another user's (on Linux any of its real, effective or
/// saved user or group ids is not this process's: root's `sshd` or `login`, a setuid or setgid
/// program), on Linux one that started before this process did (no agent of this hub is older
/// than the hub: the user's `systemd --user`), or a process of this user whose parent is
/// another user's process (not init): the entry point of a session a privileged daemon started
/// for the user, like `sshd`'s per-connection process, which switched users without exec'ing
/// and is not dumpable.
/// Otherwise ([`ChainMarker::Hidden`]): the peer's (`pids[0]`) whatever the reason, and a
/// same-user ancestor's (`PR_SET_DUMPABLE` 0 hides `/proc/<pid>/environ` from the process's
/// own user). With `ps` an environment it does not show for a process it lists counts as
/// unreadable, an empty one too (`ps` shows nothing for it either), and only the effective
/// user id is compared, and a process `ps` does not list is gone.
///
/// On Linux one `/proc/<pid>/environ` read per process, up to the first carrier (and the
/// `status` of a process whose environment cannot be read, and of its parent; the `stat` of
/// one that is not there, or whose environment is empty); with `ps` two calls for the whole
/// chain. Blocks (reads the process table).
pub fn first_agent_marker(pids: &[u32]) -> ChainMarker {
    #[cfg(all(target_os = "linux", not(twr_ps)))]
    {
        let since = own_start_time();
        let zombie =
            |pid| matches!(sys::stat(pid), Lookup::Found(s) if matches!(s.state, 'Z' | 'X' | 'x'));
        marker_along(pids, |i, pid| match sys::environ(pid) {
            // A zombie's environment reads empty, or cannot be read (`EACCES`).
            Ok(environ) if environ.is_empty() => match exited(pid) {
                Some(true) => EnvRead::Gone,
                _ => EnvRead::Read(environ),
            },
            Ok(environ) => EnvRead::Read(environ),
            // Not there: gone, unless `/proc` itself is not.
            Err(e) if is_gone(&e) => match exited(pid) {
                None => EnvRead::Skipped,
                Some(_) => EnvRead::Gone,
            },
            Err(_) if zombie(pid) => EnvRead::Gone,
            Err(_) if i == 0 => EnvRead::Hidden,
            Err(_) => unreadable_ancestor(pid, since),
        })
    }
    #[cfg(any(not(target_os = "linux"), twr_ps))]
    {
        if pids.is_empty() {
            return ChainMarker::Absent;
        }
        let Some(listed) = ps::chain_environs(pids) else {
            return ChainMarker::Absent;
        };
        marker_along(pids, |i, pid| match listed.procs.get(&pid) {
            None => EnvRead::Gone,
            Some(p) => match &p.environ {
                Some(environ) => EnvRead::Read(environ.clone()),
                None if i == 0 => EnvRead::Hidden,
                None if p.uid != listed.uid => EnvRead::Skipped,
                None if p.ppid > 1
                    && listed
                        .procs
                        .get(&p.ppid)
                        .is_some_and(|parent| parent.uid != listed.uid) =>
                {
                    EnvRead::Skipped
                }
                None => EnvRead::Hidden,
            },
        })
    }
}

/// One process's environment as [`first_agent_marker`] sees it.
enum EnvRead {
    /// Read.
    Read(Vec<u8>),
    /// Not read, and it does not count: the process is gone, or its environment is unreadable
    /// for reasons an agent does not control.
    Skipped,
    /// Not read, and it counts: it may carry the marker.
    Hidden,
    /// Not read: the process has exited. It may have carried the marker.
    Gone,
}

/// [`first_agent_marker`] over `read` (given the index of a process in `pids`, and its pid):
/// the first carrier; else the first process that had exited; else the first hidden
/// environment; else [`ChainMarker::Absent`].
fn marker_along(pids: &[u32], mut read: impl FnMut(usize, u32) -> EnvRead) -> ChainMarker {
    let mut hidden = None;
    let mut gone = None;
    for (i, &pid) in pids.iter().enumerate() {
        match read(i, pid) {
            EnvRead::Read(environ) => {
                if let Some(value) = environ_value(&environ, AGENT_ENV) {
                    return ChainMarker::Carried { pid, value };
                }
            }
            EnvRead::Skipped => {}
            EnvRead::Hidden => {
                hidden.get_or_insert(pid);
            }
            EnvRead::Gone => {
                gone.get_or_insert(pid);
            }
        }
    }
    match (gone, hidden) {
        (Some(pid), _) => ChainMarker::Gone { pid },
        (None, Some(pid)) => ChainMarker::Hidden { pid },
        (None, None) => ChainMarker::Absent,
    }
}

/// Whether process `pid` has exited (gone, or a zombie); `None` when the process table cannot
/// be read. A record that cannot be read while the table can counts as
/// exited: the process was there when its environment was read.
#[cfg(all(target_os = "linux", not(twr_ps)))]
fn exited(pid: u32) -> Option<bool> {
    match sys::stat(pid) {
        Lookup::Found(s) => Some(matches!(s.state, 'Z' | 'X' | 'x')),
        Lookup::Gone | Lookup::Unreadable => Some(true),
        Lookup::Unknown => None,
    }
}

/// Whether a `/proc` read failed because the process is gone (`ENOENT`, `ESRCH`).
#[cfg(all(target_os = "linux", not(twr_ps)))]
fn is_gone(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::NotFound || e.raw_os_error() == Some(3)
}

/// Whether the environment of ancestor `pid`, which could not be read, counts (see
/// [`first_agent_marker`]); `since`: this process's start time.
#[cfg(all(target_os = "linux", not(twr_ps)))]
fn unreadable_ancestor(pid: u32, since: Option<u64>) -> EnvRead {
    let us = sys::ids("self");
    let ids = sys::ids(&pid.to_string());
    let start = match sys::stat(pid) {
        Lookup::Found(s) => Some(s.start_time),
        // It exited after its environment was read.
        Lookup::Gone => return EnvRead::Gone,
        Lookup::Unreadable | Lookup::Unknown => None,
    };
    let parent = |ppid: u32| (ppid > 1).then(|| sys::ids(&ppid.to_string()).ok())?;
    match (us, ids) {
        (_, Err(e)) if is_gone(&e) => EnvRead::Gone,
        (Ok(us), Ok(ids)) => unreadable_counts(&us, &ids, start, since, parent(ids.ppid)),
        _ => EnvRead::Hidden,
    }
}

/// Whether an ancestor's unreadable environment counts ([`first_agent_marker`]), given this
/// process's ids `us`, the ancestor's `ids` and start time, this process's start time `since`
/// and the ids of the ancestor's parent (`None`: init, or unreadable).
#[cfg(all(target_os = "linux", not(twr_ps)))]
fn unreadable_counts(
    us: &sys::Ids,
    ids: &sys::Ids,
    start: Option<u64>,
    since: Option<u64>,
    parent: Option<sys::Ids>,
) -> EnvRead {
    let older = matches!((start, since), (Some(s), Some(us)) if s < us);
    if !ids.same_owner(us) || older || parent.is_some_and(|p| !p.same_owner(us)) {
        EnvRead::Skipped
    } else {
        EnvRead::Hidden
    }
}

/// The value of the first `key=` entry of the NUL-separated `environ`.
pub fn environ_value(environ: &[u8], key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    environ
        .split(|b| *b == 0)
        .find_map(|e| e.strip_prefix(prefix.as_bytes()))
        .map(|v| String::from_utf8_lossy(v).into_owned())
}

/// How many processes [`ancestry`] reads at most before it gives up ([`AncestryEnd::Depth`]):
/// far beyond any real process tree (Linux's default `pid_max` allows deeper ones in
/// principle, which is why a walk that hits it is unresolved, not whole).
pub const ANCESTRY_DEPTH: usize = 4096;

/// A process on this machine, identified so that a reused pid names another process: its pid
/// and its start time ([`process_start_time`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcessId {
    /// The pid.
    pub pid: u32,
    /// The start time, as [`process_start_time`] reads it.
    pub start_time: u64,
}

/// Where [`ancestry`] stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AncestryEnd {
    /// It reached init (pid 1) or pid 0 (no parent visible): the chain is whole.
    Init,
    /// A process of the chain was gone: the process itself, or a parent that exited while the
    /// chain was read.
    Gone,
    /// A parent started after its child: the parent exited and its pid now names another
    /// process (the child was reparented while the chain was read). With `ps`, a child whose
    /// parent was no longer the one read when the chain was read again.
    Reused,
    /// The process table could not be read.
    Unknown,
    /// The process table could be read, but the record of one process of the chain could not
    /// be read or parsed ([`Lookup::Unreadable`]).
    Unreadable,
    /// [`ANCESTRY_DEPTH`] processes were read without reaching init.
    Depth,
    /// The next ancestor, process `pid` (the parent the last process of the chain names), is
    /// one the process table does not show this process: its record cannot
    /// be read while the table can ([`Lookup::Unreadable`]), or it is not there
    /// ([`Lookup::Gone`]) while its child, read again, still names it as its parent (a parent
    /// that exited has left its child reparented before its record goes). procfs mounted with
    /// `hidepid=1` / `hidepid=2` does that for every process this user may not trace: another
    /// user's (root's `sshd` or `login`), and one of this user's that is not dumpable (`sshd`'s
    /// per-connection process, a runtime that made itself not dumpable). Only above the peer:
    /// a peer that cannot be read is [`AncestryEnd::Unreadable`] or [`AncestryEnd::Gone`]. Not
    /// whole ([`AncestryEnd::is_whole`]): what `pid` descends from is not known.
    Concealed {
        /// The ancestor that cannot be seen.
        pid: u32,
    },
    /// A pid came up a second time: the parents loop (pids reused while the chain was read,
    /// with start times too coarse to tell, as `ps`'s seconds can be).
    Loop,
}

impl AncestryEnd {
    /// Whether the chain is whole (it reached init): every process the peer descends from is
    /// in it.
    pub fn is_whole(self) -> bool {
        self == AncestryEnd::Init
    }
}

/// A process and its ancestors, nearest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ancestry {
    /// The process itself, then its parent, its parent's parent, and so on (init not
    /// included).
    pub chain: Vec<ProcessId>,
    /// Why the chain ends there.
    pub end: AncestryEnd,
}

/// The ancestry of process `pid` on this machine: its own pid and start time, then its
/// parent's, and so on up to init, at most [`ANCESTRY_DEPTH`] processes, stopping at a pid
/// seen before ([`AncestryEnd::Loop`]). On Linux one
/// `/proc/<pid>/stat` read per process; with `ps` (macOS) one listing of every process, then
/// one listing of the chain's processes that confirms it ([`ps_ancestry`]). A
/// process that left its parent (a daemon reparented to init) has init as its parent: what it
/// descends from is no longer visible. An ancestor procfs does not show this process
/// (`hidepid`) ends the chain ([`AncestryEnd::Concealed`]). Blocks (reads the process table); call off the async
/// runtime.
pub fn ancestry(pid: u32) -> Ancestry {
    #[cfg(all(target_os = "linux", not(twr_ps)))]
    {
        let table = sys::Table::default();
        walk_ancestry(pid, ANCESTRY_DEPTH, true, |p| sys::stat_in(p, &table))
    }
    #[cfg(any(not(target_os = "linux"), twr_ps))]
    {
        ps_ancestry(pid, ANCESTRY_DEPTH, ps::table(), ps::table_of)
    }
}

/// [`ancestry`] from `ps`: walked over `table` (one listing of every process; `None` when it
/// failed), then confirmed by `again` (a listing of the chain's processes taken after it;
/// `None` when it failed). `ps`'s start times are wall-clock times (`lstart`), which go back
/// when the clock is set back, so a parent that looks younger than its child is not taken for
/// a reused pid. A pid reused while the listing was read is caught by
/// reading the chain again instead ([`confirm_chain`]): a parent read after it had exited (its
/// pid naming another process by then) has left its child reparented when the chain is read
/// again. A second listing that fails leaves the chain as read, its end unknown.
#[cfg(any(not(target_os = "linux"), test, twr_ps))]
fn ps_ancestry(
    pid: u32,
    depth: usize,
    table: Option<std::collections::HashMap<u32, ProcStat>>,
    again: impl FnOnce(&[u32]) -> Option<std::collections::HashMap<u32, ProcStat>>,
) -> Ancestry {
    let Some(table) = table else {
        return walk_ancestry(pid, depth, false, |_| Lookup::Unknown);
    };
    let mut a = walk_ancestry(pid, depth, false, |p| {
        table.get(&p).map_or(Lookup::Gone, |s| Lookup::Found(*s))
    });
    // `ps` lists every process (nothing like `hidepid` on macOS): a parent the listing lacks
    // while its child names it exited while the listing was taken.
    if let AncestryEnd::Concealed { .. } = a.end {
        a.end = AncestryEnd::Gone;
    }
    if a.chain.is_empty() {
        return a;
    }
    let pids: Vec<u32> = a.chain.iter().map(|p| p.pid).collect();
    match again(&pids) {
        Some(now) => confirm_chain(a, &now),
        None => Ancestry {
            chain: a.chain,
            end: AncestryEnd::Unknown,
        },
    }
}

/// `a` checked against `now` (the status of its processes read after it): each process of the
/// chain still has its start time, and each still has the next one as its parent. The chain
/// ends before the first process that is gone or another one ([`AncestryEnd::Gone`]), or
/// after the first one whose parent changed ([`AncestryEnd::Reused`]: its parent exited).
#[cfg(any(not(target_os = "linux"), test, twr_ps))]
fn confirm_chain(a: Ancestry, now: &std::collections::HashMap<u32, ProcStat>) -> Ancestry {
    for (i, id) in a.chain.iter().enumerate() {
        let Some(s) = now.get(&id.pid).filter(|s| s.start_time == id.start_time) else {
            return Ancestry {
                chain: a.chain[..i].to_vec(),
                end: AncestryEnd::Gone,
            };
        };
        if a.chain
            .get(i + 1)
            .is_some_and(|parent| parent.pid != s.ppid)
        {
            return Ancestry {
                chain: a.chain[..=i].to_vec(),
                end: AncestryEnd::Reused,
            };
        }
    }
    a
}

/// [`ancestry`] over `lookup` (one process's status by pid), at most `depth` processes.
/// `ordered`: start times never go back (`/proc`'s clock ticks after boot), so a parent that
/// started after its child is a reused pid ([`AncestryEnd::Reused`]); `ps`'s wall-clock start
/// times are not compared. A parent that cannot be read, or is not there while its child,
/// read again, still names it, ends the chain as [`AncestryEnd::Concealed`].
fn walk_ancestry(
    pid: u32,
    depth: usize,
    ordered: bool,
    mut lookup: impl FnMut(u32) -> Lookup,
) -> Ancestry {
    let mut chain: Vec<ProcessId> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut next = pid;
    let end = loop {
        if next <= 1 {
            break AncestryEnd::Init;
        }
        if chain.len() >= depth {
            break AncestryEnd::Depth;
        }
        if !seen.insert(next) {
            break AncestryEnd::Loop;
        }
        match lookup(next) {
            Lookup::Found(s) => {
                // A parent never starts after its child.
                if ordered && chain.last().is_some_and(|c| s.start_time > c.start_time) {
                    break AncestryEnd::Reused;
                }
                chain.push(ProcessId {
                    pid: next,
                    start_time: s.start_time,
                });
                next = s.ppid;
            }
            // The peer itself.
            Lookup::Gone if chain.is_empty() => break AncestryEnd::Gone,
            Lookup::Unreadable if chain.is_empty() => break AncestryEnd::Unreadable,
            Lookup::Unreadable => break AncestryEnd::Concealed { pid: next },
            // An ancestor that is not there: exited (its child reparented by now), or hidden
            // (`hidepid=2`, the child still names it).
            Lookup::Gone => {
                let named = chain.last().is_some_and(|c| {
                    matches!(lookup(c.pid), Lookup::Found(s)
                        if s.start_time == c.start_time && s.ppid == next)
                });
                break if named {
                    AncestryEnd::Concealed { pid: next }
                } else {
                    AncestryEnd::Gone
                };
            }
            Lookup::Unknown => break AncestryEnd::Unknown,
        }
    };
    Ancestry { chain, end }
}

/// Whether the interactive process of an attach still runs: process `pid` with start time
/// `start_time`, not a zombie, whose environment carries [`ATTACH_ENV`]`=<marker>`. `strict`:
/// an environment that cannot be read does not match (checking what a client reports); else
/// it counts as that process (a pid and start time that still match are the same process, and
/// keeping an attach too long is the safe side), and so does a process table that cannot be
/// read at all. Blocks (reads the process table).
///
/// A client reports its process as soon as its spawn returns, and the child may then still be
/// inside execve: the parent is released before the child has switched to its new address
/// space, so `/proc` shows the parent's environment for a moment, then an empty one until the
/// kernel has recorded the new one (`ps` may show the parent's, or none). `strict` therefore
/// retries a missing marker (or an environment it cannot read) for up to [`EXEC_SETTLE`]
/// while the process runs. Otherwise an empty environment (a recorded process inside a later
/// execve of its own) counts as that process, as an unreadable one does.
pub fn attach_process_running(pid: u32, start_time: u64, marker: &str, strict: bool) -> bool {
    if marker.is_empty() {
        return false;
    }
    let deadline = Instant::now() + EXEC_SETTLE;
    loop {
        let environ = match liveness(pid, start_time) {
            Alive::No => return false,
            Alive::Yes => sys::environ(pid),
            Alive::Unknown => Err(std::io::Error::other("the process table cannot be read")),
        };
        match environ {
            Ok(environ) if environ_has(&environ, ATTACH_ENV, marker) => return true,
            Ok(environ) if !strict => return environ.is_empty(),
            Err(_) if !strict => return true,
            _ if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => return false,
        }
    }
}

/// How long a strict [`attach_process_running`] waits for a just-spawned process to finish
/// its execve before it counts a missing marker as a mismatch.
const EXEC_SETTLE: Duration = Duration::from_secs(2);

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    /// What a test runs right before [`reap_job_group`] signals a group: given the group and
    /// the signal.
    type BeforeSignal = Box<dyn FnMut(u32, &str)>;

    thread_local! {
        static BEFORE_GROUP_SIGNAL: RefCell<Option<BeforeSignal>> = const { RefCell::new(None) };
    }

    /// [`reap_job_group`]'s test hook: runs what [`BEFORE_GROUP_SIGNAL`] holds on this thread.
    pub(super) fn before_group_signal(pgid: u32, sig: &str) {
        BEFORE_GROUP_SIGNAL.with(|h| {
            if let Some(f) = h.borrow_mut().as_mut() {
                f(pgid, sig);
            }
        });
    }

    /// A lost job's group is the job's while its recorded leader runs, or, the leader gone,
    /// while a member known to be the job's runs in it; a running process with the group's id
    /// that started at another time means the job's group is gone; members none of which is
    /// known prove nothing.
    #[test]
    fn a_job_group_is_proven_by_its_leader_or_a_known_member() {
        let known = [(10, 5), (11, 6)];
        assert_eq!(
            group_now(Some(5), 5, vec![(10, 5), (12, 9)], &[]),
            GroupNow::Ours(vec![(10, 5), (12, 9)])
        );
        assert_eq!(
            group_now(Some(7), 5, vec![(10, 7)], &known),
            GroupNow::Other
        );
        assert_eq!(group_now(None, 5, vec![], &known), GroupNow::Empty);
        // A known member still there: new ones (its children) are the job's too.
        assert_eq!(
            group_now(None, 5, vec![(11, 6), (13, 9)], &known),
            GroupNow::Ours(vec![(11, 6), (13, 9)])
        );
        assert_eq!(
            group_now(None, 5, vec![(13, 9)], &known),
            GroupNow::Unproven
        );
        assert_eq!(
            group_now(None, 5, vec![(11, 8)], &known),
            GroupNow::Unproven
        );
        assert_eq!(group_now(None, 5, vec![(11, 6)], &[]), GroupNow::Unproven);
    }

    /// A leaderless group (its leader exited, a child of it left) whose pid and start time
    /// are `(pgid, start_time)` and whose surviving child is `member`; `member_cmd` is the
    /// child's shell command.
    #[cfg(target_os = "linux")]
    fn leaderless_group(member_cmd: &str) -> (u32, u64, u32, tempfile::TempDir) {
        use std::os::unix::process::CommandExt;
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("pid");
        let mut leader = std::process::Command::new("sh")
            .args([
                "-c",
                &format!(
                    "({member_cmd}) & echo $! > {0}.tmp; mv {0}.tmp {0}",
                    f.display()
                ),
            ])
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = leader.id();
        let start_time = process_start_time(pgid).unwrap();
        assert!(leader.wait().unwrap().success());
        let member: u32 = std::fs::read_to_string(&f).unwrap().trim().parse().unwrap();
        (pgid, start_time, member, dir)
    }

    /// A leaderless group none of whose members is known to be the job's (as one that got the
    /// group id of a job's group that was gone, whose own leader exited, would be) is left
    /// alone, reported unknown; with its member known, it is killed.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_leaderless_group_without_a_known_member_is_left_alone() {
        let (pgid, start_time, member, _d) = leaderless_group("exec sleep 300");
        let member_id = (member, process_start_time(member).unwrap());
        let unproven = [
            reap_job_group(pgid, start_time, &[]),
            reap_job_group(pgid, start_time, &[(1, 1)]),
        ];
        let alive = process_alive(member);
        if !alive {
            let _ = signal(member, false, "KILL");
        }
        assert_eq!(unproven, [Reaped::Unknown, Reaped::Unknown]);
        assert!(alive, "an unproven group was signalled");
        assert_eq!(reap_job_group(pgid, start_time, &[member_id]), Reaped::Gone);
        assert!(!process_alive(member));
        // A running process with the group's id that started at another time: not the job's.
        use std::os::unix::process::CommandExt;
        let mut other = std::process::Command::new("sleep")
            .arg("300")
            .process_group(0)
            .spawn()
            .unwrap();
        let t = process_start_time(other.id()).unwrap();
        assert_eq!(reap_job_group(other.id(), t + 1, &[]), Reaped::NotOurs);
        assert!(other.try_wait().unwrap().is_none());
        let _ = other.kill();
        let _ = other.wait();
    }

    /// A known member of a leaderless lost job's group that survives SIGTERM and forks then
    /// (here when it gets SIGTERM, before SIGKILL): the group stays the job's, and SIGKILL
    /// ends both. A process that joins the group meanwhile while a known member is still in
    /// it is the job's too.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_known_member_that_forks_between_the_signals_goes_with_its_child() {
        let dir = tempfile::tempdir().unwrap();
        let child = dir.path().join("child");
        let (pgid, start_time, member, _d) = leaderless_group(&format!(
            "trap 'sleep 300 & echo $! > {}' TERM; while :; do sleep 0.05; done",
            child.display()
        ));
        let member_id = (member, process_start_time(member).unwrap());
        let joiner = std::rc::Rc::new(RefCell::new(None));
        let j = std::rc::Rc::clone(&joiner);
        BEFORE_GROUP_SIGNAL.with(|h| {
            *h.borrow_mut() = Some(Box::new(move |g, sig| {
                if sig == "KILL" {
                    use std::os::unix::process::CommandExt;
                    let c = std::process::Command::new("sleep")
                        .arg("300")
                        .process_group(g as i32)
                        .spawn()
                        .unwrap();
                    *j.borrow_mut() = Some(c);
                }
            }));
        });
        let reaped = reap_job_group(pgid, start_time, &[member_id]);
        BEFORE_GROUP_SIGNAL.with(|h| *h.borrow_mut() = None);
        let mut joiner = joiner.borrow_mut().take().expect("the hook ran");
        let forked: Option<u32> = std::fs::read_to_string(&child)
            .ok()
            .and_then(|t| t.trim().parse().ok());
        let joiner_gone = joiner.try_wait().unwrap().is_some() || {
            std::thread::sleep(Duration::from_millis(200));
            joiner.try_wait().unwrap().is_some()
        };
        if !joiner_gone {
            let _ = joiner.kill();
        }
        let _ = joiner.wait();
        let member_alive = process_alive(member);
        let forked_alive = forked.map(process_alive);
        // Whatever the outcome, nothing is left running.
        for pid in std::iter::once(member).chain(forked) {
            let _ = signal(pid, false, "KILL");
        }
        assert_eq!(reaped, Reaped::Gone);
        assert!(!member_alive);
        assert_eq!(
            forked_alive,
            Some(false),
            "the member forked when it got SIGTERM, and its child went"
        );
        assert!(joiner_gone, "the joiner survived");
    }

    /// Recovery ([`leftover`]): a live process with the pid of a recorded runtime whose start
    /// time was never recorded is that runtime, marked or not (it may have exec'd without its
    /// marker), unless its start time proves otherwise, which it does only where start times
    /// are ordered (`/proc`). And a process marked for the thread counts whatever its start
    /// time where they are not (`ps`: a clock set back puts a survivor below the bound).
    #[test]
    fn leftovers_are_not_ruled_out_by_missing_markers_or_unordered_times() {
        let spawn = |marker: Option<&str>| {
            let mut c = std::process::Command::new("sleep");
            c.arg("30");
            if let Some(t) = marker {
                c.env(THREAD_ENV, t);
            }
            let child = c.spawn().unwrap();
            let pid = child.id();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let exec_d = process_cmdline(pid).is_some_and(|c| c.starts_with(b"sleep"));
                let ready = match marker {
                    Some(t) => exec_d && sys::environ(pid).is_ok_and(|e| is_marked(&e, t, None)),
                    None => exec_d,
                };
                if ready {
                    break;
                }
                assert!(Instant::now() < deadline, "sleep never exec'd");
                std::thread::sleep(Duration::from_millis(5));
            }
            child
        };
        let mut unmarked = spawn(None);
        let pid = unmarked.id();
        let p = StaleProcess {
            thread: "01R3UNMARKED".into(),
            pid,
            start_time: None,
            exe: None,
            generation: None,
        };
        let r = leftover("01R3UNMARKED", Some(&p), None);
        assert!(r.is_some(), "an unmarked live pid was taken for gone");
        // A bound after its start proves it another process only where times are ordered.
        let later = process_start_time(pid).unwrap() + 1_000_000;
        let r = leftover("01R3UNMARKED", Some(&p), Some(later));
        assert_eq!(r.is_some(), !ORDERED_START_TIMES, "{r:?}");
        let _ = unmarked.kill();
        let _ = unmarked.wait();

        let mut marked = spawn(Some("01R3MARKED"));
        let below = process_start_time(marked.id()).unwrap() + 1_000_000;
        let r = leftover("01R3MARKED", None, Some(below));
        assert_eq!(r.is_some(), !ORDERED_START_TIMES, "{r:?}");
        if !ORDERED_START_TIMES {
            let r = r.unwrap();
            assert!(
                r.contains(&format!("process {} still runs", marked.id())),
                "{r}"
            );
        }
        let _ = marked.kill();
        let _ = marked.wait();
    }
    /// The recovery counts an unreadable process as the user's by its user ids alone: one that
    /// changed its group (`sg`, `newgrp`; its environment then closed to the runner) is still
    /// a candidate, another user's is not, whatever its groups; reading an environment still
    /// takes the stricter [`sys::Ids::same_owner`].
    #[cfg(all(target_os = "linux", not(twr_ps)))]
    #[test]
    fn recovery_candidates_are_this_users_whatever_their_groups() {
        let us = sys::Ids {
            uid: [1000; 4],
            gid: [1000; 4],
            ppid: 1,
        };
        let regrouped = sys::Ids {
            gid: [2000; 4],
            ..us
        };
        assert!(regrouped.same_user(&us));
        assert!(!regrouped.same_owner(&us));
        let setgid = sys::Ids {
            gid: [1000, 2000, 2000, 2000],
            ..us
        };
        assert!(setgid.same_user(&us) && !setgid.same_owner(&us));
        let other = sys::Ids {
            uid: [1001; 4],
            ..us
        };
        assert!(!other.same_user(&us));
        // A setuid program of another user the user started still runs with the user's real
        // id: it counts (it could be the runtime's descendant).
        let setuid = sys::Ids {
            uid: [1000, 0, 0, 0],
            ..us
        };
        assert!(setuid.same_user(&us) && !setuid.same_owner(&us));
        assert!(us.same_user(&us) && us.same_owner(&us));
    }

    #[test]
    fn hubs_and_attaches_are_told_by_their_arguments() {
        use super::TwrParent::{Attach, Hub};
        let parent = |args: &[&str]| {
            super::twr_parent(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>())
        };
        assert_eq!(parent(&["twr", "hub"]), Some(Hub));
        assert_eq!(
            parent(&[
                "/home/u/.local/bin/twr",
                "--socket",
                "/s",
                "--workspace",
                "a",
                "hub"
            ]),
            Some(Hub)
        );
        assert_eq!(parent(&["twr", "-w", "a", "hub"]), Some(Hub));
        assert_eq!(parent(&["twr", "--socket=/s", "hub"]), Some(Hub));
        assert_eq!(parent(&["twr", "-w", "a", "attach", "api"]), Some(Attach));
        assert_eq!(parent(&["twr", "attach", "--force", "api"]), Some(Attach));
        assert_eq!(parent(&["twr", "hub", "stop"]), None);
        assert_eq!(
            parent(&["twr", "-w", "hub", "thread", "list"]),
            None,
            "a workspace named hub"
        );
        assert_eq!(parent(&["twr", "thread", "list"]), None);
        assert_eq!(parent(&["twr"]), None);
        assert_eq!(parent(&["mytwr", "hub"]), None);
        assert_eq!(parent(&["sh", "-c", "twr hub"]), None);
        assert_eq!(parent(&[]), None);
        // This process, through the backend.
        let me = super::process_args(std::process::id()).unwrap();
        assert!(!me.is_empty());
        assert_eq!(super::twr_parent(&me), None);
    }

    const STAT: &[u8] = b"4242 (my (odd) prog) S 1 4242 4242 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 987654 1000 200 18446744073709551615 0 0 0 0 0 0 0 0 0 0 0 0 17 3 0 0 0 0 0";

    fn stale(start: Option<u64>, exe: Option<&str>) -> StaleProcess {
        StaleProcess {
            thread: "01THREAD".into(),
            pid: 4242,
            start_time: start,
            exe: exe.map(str::to_string),
            generation: None,
        }
    }

    #[test]
    fn stat_parsing_counts_fields_after_the_command_name() {
        let s = parse_stat(STAT).unwrap();
        assert_eq!(
            s,
            ProcStat {
                state: 'S',
                ppid: 1,
                pgrp: 4242,
                start_time: 987654
            }
        );
        assert!(parse_stat(b"garbage").is_none());
        assert!(parse_stat(b"1 (x) S 1 2").is_none());
        assert!(parse_stat(b"1 )x( S 1 4242 4242").is_none());
        // A command name that is not UTF-8: a byte that is not, a multibyte
        // character cut by the 15-byte limit, a newline and parentheses.
        let fields = &STAT[STAT.iter().rposition(|b| *b == b')').unwrap() + 1..];
        for comm in [
            &b"bad\xffname"[..],
            &"aaaaaaaaaaaaaa\u{e9}".as_bytes()[..15],
            &b"x) (y\n) z"[..],
        ] {
            let mut stat = b"4242 (".to_vec();
            stat.extend_from_slice(comm);
            stat.push(b')');
            stat.extend_from_slice(fields);
            assert_eq!(parse_stat(&stat), Some(s), "{stat:?}");
        }
        // The fields after the name are ASCII.
        let mut stat = STAT.to_vec();
        let at = stat.len() - 2;
        stat[at] = 0xff;
        assert!(parse_stat(&stat).is_none());
    }

    #[test]
    fn stale_matching_needs_start_time_and_marker() {
        let stat = parse_stat(STAT).unwrap();
        let env = b"HOME=/h\0TWR_THREAD=01THREAD\0PATH=/bin\0";
        let cmd = b"/usr/bin/claude\0-p\0";
        assert!(is_stale_runtime(
            &stale(Some(987654), None),
            &stat,
            env,
            None
        ));
        assert!(is_stale_runtime(
            &stale(Some(987654), Some("claude")),
            &stat,
            env,
            Some(cmd)
        ));
        // Pid reused: different start time.
        assert!(!is_stale_runtime(&stale(Some(1), None), &stat, env, None));
        // No recorded start time: never.
        assert!(!is_stale_runtime(&stale(None, None), &stat, env, None));
        // Missing or different marker (prefix matches do not count).
        assert!(!is_stale_runtime(
            &stale(Some(987654), None),
            &stat,
            b"TWR_THREAD=01THREADX\0",
            None
        ));
        assert!(!is_stale_runtime(
            &stale(Some(987654), None),
            &stat,
            b"HOME=/h\0",
            None
        ));
        // Recorded exe must match argv[0].
        assert!(!is_stale_runtime(
            &stale(Some(987654), Some("codex")),
            &stat,
            env,
            Some(cmd)
        ));
        assert!(!is_stale_runtime(
            &stale(Some(987654), Some("claude")),
            &stat,
            env,
            None
        ));
        // Zombies are already dead.
        let zombie = ProcStat { state: 'Z', ..stat };
        assert!(!is_stale_runtime(
            &stale(Some(987654), None),
            &zombie,
            env,
            None
        ));
    }

    #[test]
    fn environ_and_argv0_helpers() {
        assert!(environ_has(b"A=1\0B=2", "B", "2"));
        assert!(!environ_has(b"A=1\0B=22", "B", "2"));
        assert!(!environ_has(b"", "B", "2"));
        assert!(argv0_matches(b"/usr/bin/sleep\x0030", "sleep"));
        assert!(argv0_matches(b"sleep", "/bin/sleep"));
        assert!(!argv0_matches(b"", "sleep"));
    }

    #[test]
    fn runtime_running_sees_the_recorded_process_and_the_marked_ones() {
        use std::os::unix::process::CommandExt;
        let mut c = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .env(THREAD_ENV, "01RUNNING")
            .spawn()
            .unwrap();
        let pid = c.id();
        // Past execve (see `reaps_only_the_marked_process`).
        let deadline = Instant::now() + Duration::from_secs(10);
        while !sys::environ(pid).is_ok_and(|e| environ_has(&e, THREAD_ENV, "01RUNNING")) {
            assert!(Instant::now() < deadline, "sleep never exec'd");
            std::thread::sleep(Duration::from_millis(5));
        }
        let start = process_start_time(pid).unwrap();
        let p = StaleProcess {
            thread: "01RUNNING".into(),
            pid,
            start_time: Some(start),
            exe: None,
            generation: None,
        };
        assert!(runtime_running("01RUNNING", Some(&p), None, None));
        assert!(runtime_running("01RUNNING", None, Some(start), None));
        // Another thread's marker, a later start bound, another start time: not this runtime.
        assert!(!runtime_running("01OTHER", None, Some(start), None));
        assert!(!runtime_running("01RUNNING", None, Some(start + 1), None));
        let other = StaleProcess {
            start_time: Some(start + 1),
            ..p.clone()
        };
        assert!(!runtime_running("01RUNNING", Some(&other), None, None));
        let _ = c.kill();
        let _ = c.wait();
        assert!(!runtime_running("01RUNNING", Some(&p), Some(start), None));
    }

    /// The cleanup of one runtime generation never kills the thread's next
    /// runtime (both carry the thread's marker; only their generations differ).
    #[test]
    fn reap_marked_with_a_generation_spares_the_threads_other_runtimes() {
        use std::os::unix::process::CommandExt;
        let spawn = |gen: &str| {
            std::process::Command::new("sleep")
                .arg("30")
                .process_group(0)
                .env(THREAD_ENV, "01GENS")
                .env(RUNTIME_GEN_ENV, gen)
                .spawn()
                .unwrap()
        };
        let wait_marked = |c: &std::process::Child, gen: &str| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !sys::environ(c.id()).is_ok_and(|e| is_marked(&e, "01GENS", Some(gen))) {
                assert!(Instant::now() < deadline, "sleep never exec'd");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let mut old = spawn("1");
        let mut new = spawn("2");
        wait_marked(&old, "1");
        wait_marked(&new, "2");
        let min = process_start_time(old.id())
            .unwrap()
            .min(process_start_time(new.id()).unwrap());
        assert!(runtime_running("01GENS", None, Some(min), Some("1")));
        assert!(reap_marked("01GENS", min, Some("1")));
        assert!(old.try_wait().unwrap().is_some(), "generation 1 survived");
        assert!(new.try_wait().unwrap().is_none(), "generation 2 was killed");
        assert!(!runtime_running("01GENS", None, Some(min), Some("1")));
        assert!(runtime_running("01GENS", None, Some(min), Some("2")));
        // Without a generation (a previous hub's or runner's runtimes): every marked one.
        assert!(runtime_running("01GENS", None, Some(min), None));
        assert!(reap_marked("01GENS", min, None));
        assert!(new.try_wait().unwrap().is_some());
        let _ = old.wait();
        let _ = new.wait();
    }

    /// A recorded process named with its run's generation is signalled only
    /// while it still carries that generation: a record that turns out to describe another run
    /// of the thread (same thread marker, pid and start time) kills nothing, nor counts as
    /// that run still running.
    #[test]
    fn a_recorded_process_of_another_generation_is_never_signalled() {
        use std::os::unix::process::CommandExt;
        let mut c = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .env(THREAD_ENV, "01GENREC")
            .env(RUNTIME_GEN_ENV, "new")
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !sys::environ(c.id()).is_ok_and(|e| is_marked(&e, "01GENREC", Some("new"))) {
            assert!(Instant::now() < deadline, "sleep never exec'd");
            std::thread::sleep(Duration::from_millis(5));
        }
        let (pid, start) = (c.id(), process_start_time(c.id()));
        let p = |gen: &str| StaleProcess {
            thread: "01GENREC".into(),
            pid,
            start_time: start,
            exe: Some("sleep".into()),
            generation: Some(gen.into()),
        };
        assert!(!reap_stale(&p("old")));
        assert!(
            c.try_wait().unwrap().is_none(),
            "another run's process was killed"
        );
        assert!(!runtime_running("01GENREC", Some(&p("old")), None, None));
        assert!(runtime_running("01GENREC", Some(&p("new")), None, None));
        assert!(reap_stale(&p("new")));
        assert!(c.try_wait().unwrap().is_some());
    }

    #[test]
    fn reaps_only_the_marked_process() {
        use std::os::unix::process::{CommandExt, ExitStatusExt};
        use std::process::{Child, Command};

        let spawn = |marker: Option<&str>| {
            let mut c = Command::new("sleep");
            c.arg("30").process_group(0);
            if let Some(m) = marker {
                c.env(THREAD_ENV, m);
            }
            c.spawn().unwrap()
        };
        let proc = |c: &Child, thread: &str, delta: u64| StaleProcess {
            thread: thread.into(),
            pid: c.id(),
            start_time: Some(process_start_time(c.id()).unwrap() + delta),
            exe: Some("sleep".into()),
            generation: None,
        };
        // A just-spawned process may still be inside execve: for a moment /proc (or ps) shows
        // the parent's cmdline and environ (a vfork parent is woken before the child switches
        // to its new address space), then an empty environ until the kernel has copied it.
        let exec_done = |c: &Child| {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let cmdline = sys::cmdline(c.id()).unwrap_or_default();
                if argv0_matches(&cmdline, "sleep")
                    && sys::environ(c.id()).is_ok_and(|e| !e.is_empty())
                {
                    return;
                }
                assert!(Instant::now() < deadline, "sleep never exec'd");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let mut marked = spawn(Some("01T"));
        let mut decoy = spawn(None);
        exec_done(&marked);
        exec_done(&decoy);
        // Same program and a matching start time, but no marker: what a reused pid looks like.
        assert!(!reap_stale(&proc(&decoy, "01T", 0)));
        // Marker of another thread, or a different start time.
        assert!(!reap_stale(&proc(&marked, "01OTHER", 0)));
        assert!(!reap_stale(&proc(&marked, "01T", 1)));
        assert!(marked.try_wait().unwrap().is_none());
        assert!(decoy.try_wait().unwrap().is_none());

        assert!(reap_stale(&proc(&marked, "01T", 0)));
        // reap_stale returned only once the process was gone.
        let status = marked
            .try_wait()
            .unwrap()
            .expect("reaped process has exited");
        assert_eq!(status.signal(), Some(15));
        assert!(decoy.try_wait().unwrap().is_none());
        let _ = decoy.kill();
        let _ = decoy.wait();
    }

    /// A job's members are read from one listing of the process table: on the `ps` backend
    /// (macOS) one `ps` call per snapshot, not one more per member (every few seconds, for
    /// each running job), with each member's start time from that listing.
    #[test]
    fn a_jobs_members_come_from_one_ps_listing() {
        let dir = tempfile::tempdir().unwrap();
        let calls = dir.path().join("calls");
        let bin = ps::tests::fake_ps(
            dir.path(),
            "listing",
            &format!(
                "echo \"$*\" >> {}\n\
                 echo '  900 Ss       1   900 Wed Oct  1 12:00:00 2026'\n\
                 echo '  903 S      900   900 Wed Oct  1 12:00:03 2026'\n\
                 echo '  901 S      900   900 Wed Oct  1 12:00:01 2026'\n\
                 echo '  902 Z      900   900 Wed Oct  1 12:00:02 2026'\n\
                 echo '  950 S        1   950 Wed Oct  1 12:00:01 2026'",
                calls.display()
            ),
        );
        let members = ps::with_fake_ps(&bin, Duration::from_secs(5), || {
            ps::group(900).map(|listed| members_kept(listed, 900, 8))
        });
        // The leader and the zombie left out, the oldest first, another group's not listed.
        assert_eq!(
            members,
            Some(vec![(901, 1_790_856_001), (903, 1_790_856_003)])
        );
        assert_eq!(std::fs::read_to_string(&calls).unwrap().lines().count(), 1);
    }

    #[test]
    fn group_membership_needs_group_marker_and_life() {
        let stat = parse_stat(STAT).unwrap(); // pgrp 4242
        let env = b"HOME=/h\0TWR_THREAD=01THREAD\0";
        let start = stat.start_time;
        assert!(is_group_member(&stat, env, 4242, "01THREAD", start));
        assert!(is_group_member(&stat, env, 4242, "01THREAD", start - 5));
        // Started before the runtime, another group, another thread, no marker, a zombie,
        // init's group.
        assert!(!is_group_member(&stat, env, 4242, "01THREAD", start + 1));
        assert!(!is_group_member(&stat, env, 4243, "01THREAD", start));
        assert!(!is_group_member(&stat, env, 4242, "01OTHER", start));
        assert!(!is_group_member(
            &stat,
            b"HOME=/h\0",
            4242,
            "01THREAD",
            start
        ));
        assert!(!is_group_member(&stat, env, 4242, "", start));
        let zombie = ProcStat { state: 'Z', ..stat };
        assert!(!is_group_member(&zombie, env, 4242, "01THREAD", start));
        let init = ProcStat { pgrp: 1, ..stat };
        assert!(!is_group_member(&init, env, 1, "01THREAD", start));
    }

    /// Spawns `sh -c <leader script>` as a group leader marked for `thread`; its child (same
    /// group) ignores SIGTERM, writes its pid to `<dir>/child` and sleeps.
    fn stubborn_group(
        dir: &std::path::Path,
        thread: &str,
        leader_tail: &str,
    ) -> (std::process::Child, u32, u64) {
        use std::os::unix::process::CommandExt;
        let pidfile = dir.join("child");
        let script = format!(
            "sh -c 'trap \"\" TERM; echo $$ > {p}.tmp; mv {p}.tmp {p}; exec sleep 30' & {leader_tail}",
            p = pidfile.display()
        );
        let leader = std::process::Command::new("sh")
            .args(["-c", &script])
            .env(THREAD_ENV, thread)
            .process_group(0)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let child: u32 = loop {
            if let Some(pid) = std::fs::read_to_string(&pidfile)
                .ok()
                .and_then(|s| s.trim().parse().ok())
            {
                break pid;
            }
            assert!(Instant::now() < deadline, "the child never started");
            std::thread::sleep(Duration::from_millis(10));
        };
        // Wait for the exec into sleep (the pid file is written just before it).
        while !sys::cmdline(child).is_some_and(|c| argv0_matches(&c, "sleep")) {
            assert!(Instant::now() < deadline, "the child never exec'd sleep");
            std::thread::sleep(Duration::from_millis(10));
        }
        let child_start = process_start_time(child).unwrap();
        assert_eq!(process_group(child), Some(i64::from(leader.id())));
        (leader, child, child_start)
    }

    #[test]
    fn reaps_group_members_that_outlive_the_leader() {
        let dir = tempfile::tempdir().unwrap();

        // The leader dies on SIGTERM; its child ignores it and must still be killed.
        let (mut leader, child, child_start) = stubborn_group(dir.path(), "01GRP", "wait");
        let p = StaleProcess {
            thread: "01GRP".into(),
            pid: leader.id(),
            start_time: process_start_time(leader.id()),
            exe: Some("sh".into()),
            generation: None,
        };
        assert!(reap_stale(&p));
        assert!(!still_running(child, child_start), "the child survived");
        assert!(leader.try_wait().unwrap().is_some());

        // The leader already exited on its own: the group it led is still swept.
        let dir = tempfile::tempdir().unwrap();
        let (mut leader, child, child_start) = stubborn_group(dir.path(), "01GONE", "exit 0");
        let p = StaleProcess {
            thread: "01GONE".into(),
            pid: leader.id(),
            start_time: process_start_time(leader.id()),
            exe: Some("sh".into()),
            generation: None,
        };
        leader.wait().unwrap();
        assert!(still_running(child, child_start));
        // Another thread's marker: untouched.
        assert!(!reap_stale(&StaleProcess {
            thread: "01OTHER".into(),
            ..p.clone()
        }));
        assert!(still_running(child, child_start));
        assert!(reap_stale(&p));
        assert!(!still_running(child, child_start), "the orphan survived");
    }

    #[test]
    fn own_start_time_is_readable() {
        assert!(process_start_time(std::process::id()).is_some());
        assert!(process_start_time(u32::MAX - 1).is_none());
        // Without a start time nothing is killed.
        let me = StaleProcess {
            thread: "x".into(),
            pid: std::process::id(),
            start_time: None,
            exe: None,
            generation: None,
        };
        assert!(!reap_stale(&me));
    }

    #[test]
    fn own_start_time_is_retried() {
        let calls = std::cell::Cell::new(0);
        let flaky = || {
            calls.set(calls.get() + 1);
            (calls.get() >= 3).then_some(42)
        };
        assert_eq!(retry_start_time(flaky, 3, Duration::ZERO), Some(42));
        assert_eq!(calls.get(), 3);
        calls.set(0);
        assert_eq!(retry_start_time(|| None::<u64>, 3, Duration::ZERO), None);
        assert!(own_start_time().is_some());
    }

    /// With the `ps` backend (`--cfg twr_ps`): a `ps` that fails leaves a recorded runtime
    /// unknown, which counts as running and is never signalled; a silent exit 1 (no such
    /// process) is gone.
    #[cfg(twr_ps)]
    #[test]
    fn an_unreadable_process_table_counts_as_running() {
        use std::os::unix::process::CommandExt;
        let mut c = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .env(THREAD_ENV, "01PSFAIL")
            .env(ATTACH_ENV, "01PSFAIL:1")
            .spawn()
            .unwrap();
        let pid = c.id();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !sys::environ(pid).is_ok_and(|e| environ_has(&e, THREAD_ENV, "01PSFAIL")) {
            assert!(Instant::now() < deadline, "sleep never exec'd");
            std::thread::sleep(Duration::from_millis(5));
        }
        let start = process_start_time(pid).unwrap();
        let p = StaleProcess {
            thread: "01PSFAIL".into(),
            pid,
            start_time: Some(start),
            exe: Some("sleep".into()),
            generation: None,
        };
        let dir = tempfile::tempdir().unwrap();
        let t = Duration::from_secs(5);
        for script in ["echo 'ps: broken' >&2; exit 1", "exit 3", "kill -9 $$"] {
            let failing = ps::tests::fake_ps(dir.path(), "failing", script);
            ps::with_fake_ps(&failing, t, || {
                assert!(still_running(pid, start), "{script}");
                assert!(
                    runtime_running("01PSFAIL", Some(&p), None, None),
                    "{script}"
                );
                assert!(
                    runtime_running("01PSFAIL", None, Some(start), None),
                    "{script}"
                );
                assert!(
                    attach_process_running(pid, start, "01PSFAIL:1", false),
                    "{script}"
                );
                assert!(!reap_stale(&p), "{script}");
                assert!(!reap_marked("01PSFAIL", start, None), "{script}");
            });
            assert!(
                c.try_wait().unwrap().is_none(),
                "killed while ps failed: {script}"
            );
        }
        // A hanging ps: the same, after its timeout.
        let hang = ps::tests::fake_ps(dir.path(), "hang", "exec sleep 30");
        ps::with_fake_ps(&hang, Duration::from_millis(200), || {
            assert!(runtime_running("01PSFAIL", Some(&p), Some(start), None));
            assert!(!reap_stale(&p));
        });
        assert!(c.try_wait().unwrap().is_none());
        // "No such process" is gone.
        let none = ps::tests::fake_ps(dir.path(), "none", "exit 1");
        ps::with_fake_ps(&none, t, || {
            assert!(!still_running(pid, start));
            assert!(!runtime_running("01PSFAIL", Some(&p), None, None));
        });
        assert!(reap_stale(&p));
        assert!(c.try_wait().unwrap().is_some());
    }

    /// A client reports its interactive process as soon as its spawn returns, when the child
    /// may still be inside execve (/proc shows the parent's environment, then none): the
    /// strict check waits for the new image rather than refusing it. A wrong marker is still
    /// refused.
    #[test]
    fn a_just_spawned_attach_process_is_recognized() {
        use std::process::{Command, Stdio};
        let spawn = || {
            Command::new("sh")
                .args(["-c", "read x"])
                .env(ATTACH_ENV, "01FRESH:1")
                .stdin(Stdio::piped())
                .spawn()
                .unwrap()
        };
        for _ in 0..200 {
            let mut c = spawn();
            let pid = c.id();
            let start = process_start_time(pid).unwrap();
            let seen = attach_process_running(pid, start, "01FRESH:1", true);
            drop(c.stdin.take());
            c.wait().unwrap();
            assert!(seen, "process {pid} not recognized right after its spawn");
        }
        let mut c = spawn();
        let pid = c.id();
        let start = process_start_time(pid).unwrap();
        // (Refused only after the settle wait: the lenient checks below see the new image.)
        assert!(!attach_process_running(pid, start, "01FRESH:2", true));
        assert!(!attach_process_running(pid, start, "01FRESH:2", false));
        assert!(attach_process_running(pid, start, "01FRESH:1", false));
        drop(c.stdin.take());
        c.wait().unwrap();
        assert!(!attach_process_running(pid, start, "01FRESH:1", false));
    }

    /// [`agent_marker`] reads [`AGENT_ENV`] from a live process's environment (`/proc`, or
    /// `ps` with `--cfg twr_ps`), `None` without it, an error once the process is gone.
    #[test]
    fn the_agent_marker_is_read_from_the_environment() {
        use std::process::Command;
        let spawn = |marker: Option<&str>| {
            let mut c = Command::new("sleep");
            c.arg("30").env("TWR_TEST_SETTLED", "1");
            match marker {
                Some(m) => c.env(AGENT_ENV, m),
                None => c.env_remove(AGENT_ENV),
            };
            c.spawn().unwrap()
        };
        // Until the child has exec'd, its environment may still be this process's.
        let settled = |pid: u32| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !sys::environ(pid).is_ok_and(|e| environ_has(&e, "TWR_TEST_SETTLED", "1")) {
                assert!(Instant::now() < deadline, "process {pid} never exec'd");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let mut marked = spawn(Some("01MARKED:7"));
        let mut plain = spawn(None);
        settled(marked.id());
        settled(plain.id());
        assert_eq!(
            agent_marker(marked.id()).unwrap().as_deref(),
            Some("01MARKED:7")
        );
        assert_eq!(agent_marker(plain.id()).unwrap(), None);
        let pid = marked.id();
        let _ = marked.kill();
        let _ = marked.wait();
        let _ = plain.kill();
        let _ = plain.wait();
        assert!(agent_marker(pid).is_err());
        assert_eq!(
            environ_value(b"A=1\0TWR_AGENT=\0TWR_AGENT=x:1", AGENT_ENV).as_deref(),
            Some("")
        );
        assert_eq!(environ_value(b"TWR_AGENTX=1", AGENT_ENV), None);
    }

    /// [`first_agent_marker`] finds the marker on the nearest process of a chain that carries
    /// it: a child that cleared it is found by its parent's (`/proc`, or `ps` with
    /// `--cfg twr_ps`).
    #[test]
    fn the_agent_marker_is_found_on_an_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let mut sh = std::process::Command::new("sh")
            .args([
                "-c",
                &format!(
                    "env -u {AGENT_ENV} TWR_TEST_SETTLED=1 sleep 30 & echo $! > {p}.tmp; \
                     mv {p}.tmp {p}; wait",
                    p = pidfile.display()
                ),
            ])
            .env(AGENT_ENV, "01CHAIN:3")
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let sleep: u32 = loop {
            if let Ok(t) = std::fs::read_to_string(&pidfile) {
                break t.trim().parse().unwrap();
            }
            assert!(Instant::now() < deadline, "no pid file");
            std::thread::sleep(Duration::from_millis(10));
        };
        while !sys::environ(sleep).is_ok_and(|e| environ_has(&e, "TWR_TEST_SETTLED", "1")) {
            assert!(Instant::now() < deadline, "sleep never exec'd");
            std::thread::sleep(Duration::from_millis(5));
        }
        let chain: Vec<u32> = ancestry(sleep).chain.iter().map(|p| p.pid).collect();
        let found = first_agent_marker(&chain);
        let alone = first_agent_marker(&[sleep]);
        let _ = signal(sleep, false, "KILL");
        let _ = sh.wait();
        assert_eq!(chain[..2], [sleep, sh.id()]);
        assert_eq!(
            found,
            ChainMarker::Carried {
                pid: sh.id(),
                value: "01CHAIN:3".to_string()
            }
        );
        assert_eq!(alone, ChainMarker::Absent);
        assert_eq!(first_agent_marker(&[]), ChainMarker::Absent);
        // Gone processes are not ones without the marker: the nearest one.
        assert_eq!(
            first_agent_marker(&[sleep, sh.id()]),
            ChainMarker::Gone { pid: sleep }
        );
    }

    /// A process of the chain that has exited when its environment is read is reported,
    /// nearest first, as a zombie (its environment reads empty, or cannot be read) and once
    /// reaped; a carrier beyond it still wins. With `ps` a zombie's environment is not shown:
    /// hidden.
    #[test]
    fn an_exited_process_is_not_an_absent_marker() {
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let pid = child.id();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !matches!(read_stat(pid), Lookup::Found(s) if s.state == 'Z') {
            assert!(Instant::now() < deadline, "{pid} never exited");
            std::thread::sleep(Duration::from_millis(5));
        }
        // Beyond it, a process without the marker: not this one, which carries it when the
        // tests run inside a twr thread.
        let mut unmarked = std::process::Command::new("sleep")
            .arg("30")
            .env_remove("TWR_AGENT")
            .spawn()
            .unwrap();
        let beyond = unmarked.id();
        // Past its execve: until then it shows this process's environment (and marker).
        let deadline = Instant::now() + Duration::from_secs(10);
        while !(process_cmdline(beyond).is_some_and(|c| c.starts_with(b"sleep"))
            && sys::environ(beyond).is_ok_and(|e| environ_value(&e, AGENT_ENV).is_none()))
        {
            assert!(Instant::now() < deadline, "sleep never exec'd");
            std::thread::sleep(Duration::from_millis(5));
        }
        let zombie = first_agent_marker(&[pid, beyond]);
        let _ = child.wait();
        let reaped = first_agent_marker(&[pid, beyond]);
        let _ = unmarked.kill();
        let _ = unmarked.wait();
        #[cfg(all(target_os = "linux", not(twr_ps)))]
        assert_eq!(zombie, ChainMarker::Gone { pid });
        #[cfg(any(not(target_os = "linux"), twr_ps))]
        assert_eq!(zombie, ChainMarker::Hidden { pid });
        assert_eq!(reaped, ChainMarker::Gone { pid });
    }

    /// The nearest carrier decides, even beyond an environment that counts as hidden; with no
    /// carrier, the nearest hidden one; else absent.
    #[test]
    fn a_carrier_wins_over_a_hidden_environment() {
        let reads = |kinds: &'static [&'static str]| {
            move |i: usize, _pid: u32| match kinds[i] {
                "plain" => EnvRead::Read(b"A=1".to_vec()),
                "marked" => EnvRead::Read(b"A=1\0TWR_AGENT=01X:1".to_vec()),
                "hidden" => EnvRead::Hidden,
                "gone" => EnvRead::Gone,
                _ => EnvRead::Skipped,
            }
        };
        let carried = |pid| ChainMarker::Carried {
            pid,
            value: "01X:1".into(),
        };
        let pids = [10, 20, 30, 40];
        let along = |kinds| marker_along(&pids, reads(kinds));
        assert_eq!(along(&["plain", "hidden", "marked", "hidden"]), carried(30));
        assert_eq!(
            along(&["plain", "hidden", "skip", "hidden"]),
            ChainMarker::Hidden { pid: 20 }
        );
        assert_eq!(
            along(&["hidden", "plain", "plain", "plain"]),
            ChainMarker::Hidden { pid: 10 }
        );
        assert_eq!(
            along(&["plain", "skip", "skip", "plain"]),
            ChainMarker::Absent
        );
        // A process that had exited comes before a hidden environment, and after a carrier.
        assert_eq!(
            along(&["hidden", "gone", "plain", "gone"]),
            ChainMarker::Gone { pid: 20 }
        );
        assert_eq!(along(&["gone", "plain", "marked", "plain"]), carried(30));
    }

    /// Not a test of its own: this test binary started again with `TWR_TEST_NONDUMPABLE=<dir>`
    /// (without it it returns at once) makes itself not dumpable (`PR_SET_DUMPABLE` 0, which
    /// hides `/proc/<pid>/environ` from its own user), writes `<dir>/pid` and sleeps until
    /// `<dir>/stop` exists (at most 30 s).
    #[cfg(target_os = "linux")]
    #[test]
    fn non_dumpable_process() {
        let Some(dir) = std::env::var_os("TWR_TEST_NONDUMPABLE") else {
            return;
        };
        extern "C" {
            fn prctl(option: i32, ...) -> i32;
        }
        const PR_SET_DUMPABLE: i32 = 4;
        // SAFETY: prctl(PR_SET_DUMPABLE, 0) only changes this process's dumpable flag.
        assert_eq!(unsafe { prctl(PR_SET_DUMPABLE, 0u64) }, 0);
        let dir = std::path::PathBuf::from(dir);
        std::fs::write(dir.join("pid.tmp"), std::process::id().to_string()).unwrap();
        std::fs::rename(dir.join("pid.tmp"), dir.join("pid")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while !dir.join("stop").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// A process of this user, started after this one, that made itself not dumpable keeps
    /// the marker it was started with out of reach: its environment cannot be read, which
    /// counts, as the peer or as an ancestor, where an absent marker would make the peer the
    /// user (`/proc`, or `ps` with `--cfg twr_ps`). Once it is gone, it hides nothing, and is
    /// reported gone.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_hidden_environment_is_not_an_absent_marker() {
        let dir = tempfile::tempdir().unwrap();
        let mut hidden = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "reap::tests::non_dumpable_process",
                "--test-threads=1",
            ])
            .env("TWR_TEST_NONDUMPABLE", dir.path())
            .env(AGENT_ENV, "01HIDDEN:1")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let mut plain = std::process::Command::new("sleep")
            .arg("30")
            .env("TWR_TEST_SETTLED", "1")
            .env_remove(AGENT_ENV)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !dir.path().join("pid").exists() {
            assert!(Instant::now() < deadline, "the helper never started");
            std::thread::sleep(Duration::from_millis(10));
        }
        while !sys::environ(plain.id()).is_ok_and(|e| environ_has(&e, "TWR_TEST_SETTLED", "1")) {
            assert!(Instant::now() < deadline, "sleep never exec'd");
            std::thread::sleep(Duration::from_millis(5));
        }
        let pid = hidden.id();
        let unreadable = sys::environ(pid).is_err();
        let as_peer = first_agent_marker(&[pid]);
        let as_ancestor = first_agent_marker(&[plain.id(), pid]);
        #[cfg(not(twr_ps))]
        let excused = [Some(u64::MAX), Some(0)].map(|since| unreadable_ancestor(pid, since));
        std::fs::write(dir.path().join("stop"), "").unwrap();
        let _ = hidden.wait();
        let gone = first_agent_marker(&[pid]);
        let _ = plain.kill();
        let _ = plain.wait();
        assert!(unreadable);
        assert_eq!(as_peer, ChainMarker::Hidden { pid });
        assert_eq!(as_ancestor, ChainMarker::Hidden { pid });
        // An ancestor older than this process is not one an agent of it started.
        #[cfg(not(twr_ps))]
        assert!(
            matches!(excused, [EnvRead::Skipped, EnvRead::Hidden]),
            "{:?}",
            excused.map(|e| matches!(e, EnvRead::Hidden))
        );
        assert_eq!(gone, ChainMarker::Gone { pid });
    }

    /// This test process's own chain (the terminal, multiplexer or session it runs in) hides
    /// nothing: whatever unreadable environments it has are another user's, older than this
    /// process, or a session's entry point.
    #[test]
    fn this_processs_own_chain_hides_nothing() {
        let a = ancestry(std::process::id());
        assert!(a.end.is_whole(), "{a:?}");
        let pids: Vec<u32> = a.chain.iter().map(|p| p.pid).collect();
        let found = first_agent_marker(&pids);
        assert!(
            !matches!(found, ChainMarker::Hidden { .. }),
            "{found:?}: {a:?}"
        );
    }

    /// Which unreadable ancestors count: one of this user, started after this process, whose
    /// parent is this user's or init. Another user's (any real, effective or saved id differs:
    /// root's, a setuid or setgid program), an older one, or one whose parent is another
    /// user's (`sshd`'s per-connection process under root's) does not.
    #[cfg(all(target_os = "linux", not(twr_ps)))]
    #[test]
    fn which_unreadable_ancestors_count() {
        let ids = |uid: [u32; 4], gid: [u32; 4]| sys::Ids { uid, gid, ppid: 7 };
        let me = ids([1000; 4], [1000; 4]);
        let root = ids([0; 4], [0; 4]);
        let counts = |p: &sys::Ids, start, parent| {
            matches!(
                unreadable_counts(&me, p, start, Some(500), parent),
                EnvRead::Hidden
            )
        };
        assert!(counts(&me, Some(600), Some(me)));
        assert!(counts(&me, Some(600), None));
        assert!(counts(&me, None, None));
        assert!(counts(&me, Some(500), None));
        assert!(!counts(&me, Some(499), None));
        assert!(!counts(&root, Some(600), Some(me)));
        assert!(!counts(&me, Some(600), Some(root)));
        assert!(!counts(&ids([1000, 0, 0, 0], [1000; 4]), Some(600), None));
        assert!(!counts(
            &ids([1000, 1000, 0, 0], [1000; 4]),
            Some(600),
            None
        ));
        assert!(!counts(&ids([1000; 4], [1000, 5, 5, 5]), Some(600), None));
        assert!(!counts(
            &ids([1000; 4], [1000; 4]),
            Some(600),
            Some(ids([1000; 4], [5; 4]))
        ));
        // The filesystem ids of the target do not matter; ours are what Linux compares.
        assert!(counts(
            &ids([1000, 1000, 1000, 0], [1000; 4]),
            Some(600),
            None
        ));
        let ours = sys::ids("self").unwrap();
        assert_eq!(
            ours.ppid,
            sys::ids(&std::process::id().to_string()).unwrap().ppid
        );
        assert!(ours.same_owner(&ours));
    }

    /// With `ps` (`--cfg twr_ps`): an environment `ps` does not show counts for the peer and
    /// for an ancestor of this user, but not for another user's ancestor or one whose parent
    /// is another user's (`sshd`'s per-connection process); a `ps` that fails hides nothing.
    #[cfg(twr_ps)]
    #[test]
    fn a_hidden_environment_from_ps() {
        let dir = tempfile::tempdir().unwrap();
        let me = std::process::id();
        let fake = |name: &str, parent_uid: u32, peer_env: &str| {
            let plain = format!(
                "printf '%s\\n' '   40  1000    30 client' '   30  1000    20 sshd: me@pts/0' \
                 '   20  {parent_uid}     1 sshd: me [priv]' ' {me}  1000     1 test'"
            );
            let with_env = format!(
                "printf '%s\\n' '   40 client{peer_env}' '   30 sshd: me@pts/0' \
                 '   20 sshd: me [priv]' ' {me} test A=1'"
            );
            let script = format!("case \"$*\" in *uid=*) {plain};; *) {with_env};; esac");
            ps::tests::fake_ps(dir.path(), name, &script)
        };
        let t = Duration::from_secs(5);
        let marker =
            |ps: &std::path::Path| ps::with_fake_ps(ps, t, || first_agent_marker(&[40, 30, 20]));
        assert_eq!(marker(&fake("entry", 0, " A=1")), ChainMarker::Absent);
        assert_eq!(
            marker(&fake("same", 1000, " A=1")),
            ChainMarker::Hidden { pid: 30 }
        );
        assert_eq!(
            marker(&fake("peer", 0, "")),
            ChainMarker::Hidden { pid: 40 }
        );
        assert_eq!(
            marker(&fake("marked", 1000, " TWR_AGENT=01X:1")),
            ChainMarker::Carried {
                pid: 40,
                value: "01X:1".into()
            }
        );
        let failing = ps::tests::fake_ps(dir.path(), "failing", "exit 2");
        assert_eq!(marker(&failing), ChainMarker::Absent);
    }

    /// The walk follows parents up to init, and stops at a gone process, a parent that started
    /// after its child (a reused pid), an unreadable table, a pid seen twice, or the depth.
    #[test]
    fn ancestry_walks_parents_until_init() {
        let table = |rows: &[(u32, u32, u64)]| {
            let rows: std::collections::HashMap<u32, ProcStat> = rows
                .iter()
                .map(|&(pid, ppid, start_time)| {
                    let stat = ProcStat {
                        state: 'S',
                        ppid,
                        pgrp: i64::from(pid),
                        start_time,
                    };
                    (pid, stat)
                })
                .collect();
            move |pid: u32| rows.get(&pid).map_or(Lookup::Gone, |s| Lookup::Found(*s))
        };
        let id = |pid, start_time| ProcessId { pid, start_time };
        let tree = table(&[(40, 30, 9), (30, 20, 7), (20, 1, 3)]);
        let a = walk_ancestry(40, ANCESTRY_DEPTH, true, &tree);
        assert_eq!(a.chain, [id(40, 9), id(30, 7), id(20, 3)]);
        assert_eq!(a.end, AncestryEnd::Init);
        // Equal start times (`ps` has second resolution) are a parent.
        let same = table(&[(40, 30, 9), (30, 0, 9)]);
        assert_eq!(walk_ancestry(40, 9, true, same).end, AncestryEnd::Init);
        // The parent's pid now names a younger process: the chain ends before it.
        let reused = table(&[(40, 30, 9), (30, 1, 12)]);
        let a = walk_ancestry(40, ANCESTRY_DEPTH, true, reused);
        assert_eq!((a.chain, a.end), (vec![id(40, 9)], AncestryEnd::Reused));
        // The parent is not there while its child, read again, still names it: hidden
        // (`hidepid=2`); the process itself is gone.
        let orphan = table(&[(40, 30, 9)]);
        let a = walk_ancestry(40, ANCESTRY_DEPTH, true, &orphan);
        assert_eq!(
            (a.chain, a.end),
            (vec![id(40, 9)], AncestryEnd::Concealed { pid: 30 })
        );
        assert!(!a.end.is_whole());
        let a = walk_ancestry(41, ANCESTRY_DEPTH, true, &orphan);
        assert_eq!((a.chain, a.end), (vec![], AncestryEnd::Gone));
        // The parent exited while the chain was read: read again, its child has another parent
        // (or is gone, or another process).
        for again in [
            Lookup::Found(ProcStat {
                state: 'S',
                ppid: 1,
                pgrp: 40,
                start_time: 9,
            }),
            Lookup::Found(ProcStat {
                state: 'S',
                ppid: 30,
                pgrp: 40,
                start_time: 10,
            }),
            Lookup::Gone,
            Lookup::Unknown,
        ] {
            let mut reads = 0;
            let a = walk_ancestry(40, ANCESTRY_DEPTH, true, |pid| {
                reads += 1;
                match (pid, reads) {
                    (40, 1) => orphan(40),
                    (40, _) => again,
                    _ => Lookup::Gone,
                }
            });
            assert_eq!(
                (a.chain, a.end),
                (vec![id(40, 9)], AncestryEnd::Gone),
                "{again:?}"
            );
        }
        let a = walk_ancestry(40, ANCESTRY_DEPTH, true, |_| Lookup::Unknown);
        assert_eq!((a.chain, a.end), (vec![], AncestryEnd::Unknown));
        // A loop (pids reused with equal start times) ends at the first pid seen twice, a
        // chain longer than the depth at the depth: neither is whole.
        let looped = table(&[(40, 30, 9), (30, 40, 9)]);
        let a = walk_ancestry(40, 5, true, looped);
        assert_eq!(
            (a.chain, a.end),
            (vec![id(40, 9), id(30, 9)], AncestryEnd::Loop)
        );
        let looped = table(&[(40, 40, 9)]);
        let a = walk_ancestry(40, 5, true, looped);
        assert_eq!((a.chain.len(), a.end), (1, AncestryEnd::Loop));
        let deep: Vec<(u32, u32, u64)> = (2..100).map(|p| (p, p - 1, 9)).collect();
        let a = walk_ancestry(99, 64, true, table(&deep));
        assert_eq!((a.chain.len(), a.end), (64, AncestryEnd::Depth));
        assert!(!a.end.is_whole());
        let a = walk_ancestry(99, ANCESTRY_DEPTH, true, table(&deep));
        assert_eq!((a.chain.len(), a.end), (98, AncestryEnd::Init));
        assert!(a.end.is_whole());
        assert_eq!(walk_ancestry(1, 5, true, &tree).chain, []);
        assert_eq!(walk_ancestry(0, 5, true, &tree).end, AncestryEnd::Init);
    }

    /// The real process table: a shell's background child descends from the shell, which
    /// descends from this test process (`/proc`, or `ps` with `--cfg twr_ps`).
    #[test]
    fn ancestry_of_a_grandchild_reaches_this_process() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let mut sh = std::process::Command::new("sh")
            .args([
                "-c",
                &format!(
                    "sleep 30 & echo $! > {p}.tmp; mv {p}.tmp {p}; wait",
                    p = pidfile.display()
                ),
            ])
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let sleep: u32 = loop {
            if let Ok(t) = std::fs::read_to_string(&pidfile) {
                break t.trim().parse().unwrap();
            }
            assert!(Instant::now() < deadline, "no pid file");
            std::thread::sleep(Duration::from_millis(10));
        };
        let a = ancestry(sleep);
        let _ = signal(sleep, false, "KILL");
        let _ = sh.wait();
        let pids: Vec<u32> = a.chain.iter().map(|p| p.pid).collect();
        assert_eq!(pids[..3], [sleep, sh.id(), std::process::id()], "{a:?}");
        assert_eq!(
            a.chain[2].start_time,
            process_start_time(std::process::id()).unwrap()
        );
        assert_eq!(a.end, AncestryEnd::Init, "{a:?}");
    }

    /// A process whose command name is not UTF-8 (a byte that is not, a
    /// multibyte character cut by the 15-byte limit) is read like any other: the walk from
    /// its child goes through it to this test process (`/proc`, or `ps` with `--cfg twr_ps`).
    #[cfg_attr(
        target_os = "macos",
        ignore = "APFS refuses file names that are not UTF-8"
    )]
    #[test]
    fn ancestry_goes_through_command_names_that_are_not_utf8() {
        use std::os::unix::ffi::OsStrExt;
        let dir = tempfile::tempdir().unwrap();
        let sh = std::path::Path::new("/bin/sh");
        let outer = dir.path().join(std::ffi::OsStr::from_bytes(b"bad\xffsh"));
        // 14 bytes, then a two-byte character: the command name keeps its first byte only.
        let inner = dir.path().join("aaaaaaaaaaaaaa\u{e9}sh");
        std::os::unix::fs::symlink(sh, &outer).unwrap();
        std::os::unix::fs::symlink(sh, &inner).unwrap();
        let pidfile = dir.path().join("pid");
        let mut outer_sh = std::process::Command::new(&outer)
            .args([
                "-c",
                "\"$0\" -c \"sleep 30 & echo \\$! \\$\\$ > $1.tmp; mv $1.tmp $1; wait\"; true",
            ])
            .arg(&inner)
            .arg(&pidfile)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let (sleep, inner_sh): (u32, u32) = loop {
            if let Ok(t) = std::fs::read_to_string(&pidfile) {
                let mut p = t.split_whitespace().map(|p| p.parse().unwrap());
                break (p.next().unwrap(), p.next().unwrap());
            }
            assert!(Instant::now() < deadline, "no pid file");
            std::thread::sleep(Duration::from_millis(10));
        };
        #[cfg(all(target_os = "linux", not(twr_ps)))]
        for pid in [inner_sh, outer_sh.id()] {
            let stat = std::fs::read(format!("/proc/{pid}/stat")).unwrap();
            assert!(std::str::from_utf8(&stat).is_err(), "{stat:?}");
        }
        let a = ancestry(sleep);
        let _ = signal(sleep, false, "KILL");
        let _ = outer_sh.wait();
        let pids: Vec<u32> = a.chain.iter().map(|p| p.pid).collect();
        assert_eq!(
            pids[..4],
            [sleep, inner_sh, outer_sh.id(), std::process::id()],
            "{a:?}"
        );
        assert_eq!(a.end, AncestryEnd::Init, "{a:?}");
    }

    /// A record that cannot be read or parsed while the table can is not an unknown table: the
    /// peer's makes the walk unresolved, an ancestor's ends it at a process the table does not
    /// show (`hidepid=1`).
    #[test]
    fn an_unreadable_record_is_not_an_unknown_table() {
        let lookup = |pid| match pid {
            40 => Lookup::Found(ProcStat {
                state: 'S',
                ppid: 30,
                pgrp: 40,
                start_time: 9,
            }),
            _ => Lookup::Unreadable,
        };
        let a = walk_ancestry(40, ANCESTRY_DEPTH, true, lookup);
        assert_eq!(a.chain.len(), 1);
        assert_eq!(a.end, AncestryEnd::Concealed { pid: 30 });
        assert!(!a.end.is_whole());
        let a = walk_ancestry(30, ANCESTRY_DEPTH, true, lookup);
        assert_eq!((a.chain, a.end), (vec![], AncestryEnd::Unreadable));
        assert!(!a.end.is_whole());
        assert_eq!(alive_of(Lookup::Unreadable, Some(9)), Alive::Unknown);
    }

    /// The real `/proc` with faults injected: an ancestor whose record cannot
    /// be read (`hidepid=1`) or is not there while its child names it (`hidepid=2`) ends the
    /// chain there, the peer's own does not; without `/proc` nothing is gone, the
    /// table is unknown.
    #[cfg(all(target_os = "linux", not(twr_ps)))]
    #[test]
    fn hidden_and_missing_proc_entries() {
        let mut sleep = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let (pid, me) = (sleep.id(), std::process::id());
        let start = process_start_time(pid).unwrap();
        let faults = |denied: &[u32], hidden: &[u32]| ProcFaults {
            unavailable: false,
            denied: denied.to_vec(),
            hidden: hidden.to_vec(),
        };
        let walked = |f: ProcFaults| with_proc_faults(f, || ancestry(pid));
        let concealed = |a: Ancestry| (a.chain.iter().map(|p| p.pid).collect::<Vec<_>>(), a.end);
        let hidden_me = (vec![pid], AncestryEnd::Concealed { pid: me });
        assert_eq!(concealed(walked(faults(&[me], &[]))), hidden_me);
        assert_eq!(concealed(walked(faults(&[], &[me]))), hidden_me);
        let a = walked(faults(&[pid], &[]));
        assert_eq!((a.chain, a.end), (vec![], AncestryEnd::Unreadable));
        let a = walked(faults(&[], &[pid]));
        assert_eq!((a.chain, a.end), (vec![], AncestryEnd::Gone));
        // No `/proc`: unknown, never gone (a missing record is gone only while this process's
        // own can be read).
        let none = ProcFaults {
            unavailable: true,
            ..ProcFaults::default()
        };
        with_proc_faults(none, || {
            for p in [pid, me, u32::MAX] {
                assert_eq!(read_stat(p), Lookup::Unknown, "{p}");
            }
            assert_eq!(process_liveness(pid, Some(start)), Alive::Unknown);
            assert_eq!(
                ancestry(pid),
                Ancestry {
                    chain: vec![],
                    end: AncestryEnd::Unknown
                }
            );
            assert!(process_environ(pid).is_err());
            assert_eq!(first_agent_marker(&[pid, me]), ChainMarker::Absent);
            assert_eq!(group_running(pid), None);
            assert!(runtime_running("01NOPROC", None, Some(0), None));
        });
        let _ = sleep.kill();
        let _ = sleep.wait();
        // With `/proc` there, a process that is not is gone.
        assert_eq!(read_stat(pid), Lookup::Gone);
        assert_eq!(process_liveness(pid, Some(start)), Alive::No);
    }

    /// With `ps` the walk does not compare wall-clock start times (a clock set back makes a
    /// parent look younger than its child); a second listing of the chain
    /// confirms it instead: a process gone or replaced ends it before that process, a child
    /// whose parent changed (the parent read had exited, its pid reused) after that child.
    #[test]
    fn a_ps_ancestry_is_confirmed_by_a_second_listing() {
        let row = |ppid, start_time| ProcStat {
            state: 'S',
            ppid,
            pgrp: 1,
            start_time,
        };
        let id = |pid, start_time| ProcessId { pid, start_time };
        let table: std::collections::HashMap<u32, ProcStat> =
            [(40, row(30, 100)), (30, row(20, 200)), (20, row(1, 50))].into();
        let same = |_: &[u32]| Some(table.clone());
        let a = ps_ancestry(40, ANCESTRY_DEPTH, Some(table.clone()), same);
        assert_eq!(a.chain, [id(40, 100), id(30, 200), id(20, 50)]);
        assert_eq!(a.end, AncestryEnd::Init);
        // The listing of the chain is asked for exactly its processes.
        let mut asked = Vec::new();
        ps_ancestry(40, ANCESTRY_DEPTH, Some(table.clone()), |p: &[u32]| {
            asked = p.to_vec();
            Some(table.clone())
        });
        assert_eq!(asked, [40, 30, 20]);
        // 30 exited after it was listed: 40 now has init as its parent.
        let mut moved = table.clone();
        moved.insert(40, row(1, 100));
        moved.remove(&30);
        let a = ps_ancestry(40, ANCESTRY_DEPTH, Some(table.clone()), |_| Some(moved));
        assert_eq!((a.chain, a.end), (vec![id(40, 100)], AncestryEnd::Reused));
        // 30 names another process now (its parent left as it was).
        let mut reused = table.clone();
        reused.insert(30, row(20, 300));
        let a = ps_ancestry(40, ANCESTRY_DEPTH, Some(table.clone()), |_| Some(reused));
        assert_eq!((a.chain, a.end), (vec![id(40, 100)], AncestryEnd::Gone));
        // The peer itself is gone.
        let a = ps_ancestry(40, ANCESTRY_DEPTH, Some(table.clone()), |_| {
            Some(Default::default())
        });
        assert_eq!((a.chain, a.end), (vec![], AncestryEnd::Gone));
        // The second listing failed: the chain as read, its end unknown.
        let a = ps_ancestry(40, ANCESTRY_DEPTH, Some(table.clone()), |_| None);
        assert_eq!((a.chain.len(), a.end), (3, AncestryEnd::Unknown));
        // No listing at all, or no such process: nothing to confirm.
        let a = ps_ancestry(40, ANCESTRY_DEPTH, None, |_| panic!("asked"));
        assert_eq!((a.chain, a.end), (vec![], AncestryEnd::Unknown));
        let a = ps_ancestry(41, ANCESTRY_DEPTH, Some(table.clone()), |_| panic!("asked"));
        assert_eq!((a.chain, a.end), (vec![], AncestryEnd::Gone));
        // A parent missing from the listing exited while it was taken (`ps` hides nothing).
        let mut lacking = table;
        lacking.remove(&20);
        let a = ps_ancestry(40, ANCESTRY_DEPTH, Some(lacking.clone()), |_| Some(lacking));
        assert_eq!(
            (a.chain, a.end),
            (vec![id(40, 100), id(30, 200)], AncestryEnd::Gone)
        );
    }

    /// The same through `ps` itself (a fake one): a parent whose `lstart` is later than its
    /// child's (the clock was set back between them) is still the parent; a child the second
    /// listing shows under another parent ends the chain.
    #[cfg(twr_ps)]
    #[test]
    fn a_parent_listed_younger_than_its_child_is_its_parent() {
        let dir = tempfile::tempdir().unwrap();
        let rows = |child_ppid| {
            format!(
                "printf '%s\\n' '   40 S       {child_ppid}    40 Wed Oct  1 12:00:10 2026' \
                 '   30 S        1    30 Wed Oct  1 13:00:00 2026'"
            )
        };
        let t = Duration::from_secs(5);
        let id = |pid, start_time| ProcessId { pid, start_time };
        let older = ps::tests::fake_ps(dir.path(), "older", &rows(30));
        let a = ps::with_fake_ps(&older, t, || ancestry(40));
        assert_eq!(
            a.chain,
            [id(40, 1_790_856_010), id(30, 1_790_859_600)],
            "{a:?}"
        );
        assert_eq!(a.end, AncestryEnd::Init);
        // The listing of the chain (`-p 40,30`) shows 40 reparented to init.
        let script = format!(
            "case \"$*\" in *-A*) {};; *-p\\ 40,30*) {};; *) exit 2;; esac",
            rows(30),
            rows(1)
        );
        let moved = ps::tests::fake_ps(dir.path(), "moved", &script);
        let a = ps::with_fake_ps(&moved, t, || ancestry(40));
        assert_eq!(
            (a.chain, a.end),
            (vec![id(40, 1_790_856_010)], AncestryEnd::Reused)
        );
    }

    /// A zombie, another start time or no process: exited; an unreadable table: unknown (never
    /// "exited"); found with the start time (or none expected): running.
    #[test]
    fn liveness_is_three_valued() {
        let stat = |state, start_time| {
            Lookup::Found(ProcStat {
                state,
                ppid: 1,
                pgrp: 1,
                start_time,
            })
        };
        assert_eq!(alive_of(stat('S', 7), Some(7)), Alive::Yes);
        assert_eq!(alive_of(stat('R', 7), None), Alive::Yes);
        assert_eq!(alive_of(stat('S', 8), Some(7)), Alive::No);
        assert_eq!(alive_of(stat('Z', 7), Some(7)), Alive::No);
        assert_eq!(alive_of(stat('Z', 7), None), Alive::No);
        assert_eq!(alive_of(Lookup::Gone, Some(7)), Alive::No);
        assert_eq!(alive_of(Lookup::Unknown, Some(7)), Alive::Unknown);
        assert_eq!(alive_of(Lookup::Unknown, None), Alive::Unknown);
        assert_eq!(
            process_liveness(std::process::id(), None),
            Alive::Yes,
            "this test process"
        );
    }
}
