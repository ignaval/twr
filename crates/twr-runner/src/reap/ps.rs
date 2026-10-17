//! The process table through the `ps` command: what [`super`] reads where there is no `/proc`
//! (macOS). It is also built for tests on Linux, where procps `ps` prints the same columns,
//! so the parsing and its agreement with `/proc` are checked there too.
//!
//! What `ps` cannot show is unknown, and unknown never matches (nothing is signalled for it):
//!
//! - Start time: `lstart`, printed in UTC (`TZ=UTC0`, `LC_ALL=C`) with second resolution,
//!   kept as seconds since the Unix epoch. Like `/proc`'s clock ticks, it is only compared
//!   with other start times read on the same machine.
//! - Environment: `ps -E` (macOS; procps: `e`) appends the environment, space-separated, to
//!   the command line, and only for processes of the same user (macOS reads it with
//!   `KERN_PROCARGS2`). It is taken as what follows the command line printed without it, in
//!   a second call; a process that exec'd between the calls, one whose environment is not
//!   shown (another user's) or an empty one is unknown. Entries are split at spaces, so only
//!   markers without spaces (ids) are recognized as whole entries. [`chain_environs`] also
//!   reads each process's effective user id (`uid`) and parent, and this process's own user id
//!   (it lists this process too), so an unknown environment of another user's process can be
//!   told from one of this user's.
//! - Program: `comm`, which macOS prints as the whole `argv[0]`.
//!
//! The marker variables are removed from `ps`'s own environment, so it never shows up as a
//! marked process itself.
//!
//! A `ps` call that fails (it cannot run, prints an error, exits other than 0, or 1 without
//! output, is killed by a signal or takes longer than [`TIMEOUT`], then is killed) leaves the
//! process table unknown ([`super::Lookup::Unknown`], a scan `None`), never "no such process":
//! the caller counts the process as running and signals nothing.

use std::collections::HashMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::{Lookup, ProcStat, AGENT_ENV, ATTACH_ENV, RUNTIME_GEN_ENV, THREAD_ENV};

#[cfg(target_os = "macos")]
const PS: &str = "/bin/ps";
#[cfg(not(target_os = "macos"))]
const PS: &str = "ps";

/// The flag that adds the environment to the `command` column.
#[cfg(target_os = "linux")]
const ENV_FLAG: &str = "e";
#[cfg(not(target_os = "linux"))]
const ENV_FLAG: &str = "-E";

/// Columns of a listing row ([`parse_row`]).
const ROW: &str = "pid=,stat=,ppid=,pgid=,lstart=,command=";

/// How long one `ps` call may take before it is killed and counts as failed.
const TIMEOUT: Duration = Duration::from_secs(5);

/// What one `ps` call found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Ps {
    /// It succeeded: its standard output (empty when no process matched).
    Rows(String),
    /// It exited 1 without any output (what `ps -p <pid>` does when no process has that pid).
    NoSuchProcess,
    /// It could not run, timed out ([`TIMEOUT`], then killed), was killed, printed an error or
    /// exited otherwise: the process table is unknown.
    Failed,
}

/// Classifies a finished `ps` call by its exit code and outputs.
fn classify(code: Option<i32>, out: &[u8], err: &[u8]) -> Ps {
    match code {
        // Apple's ps prints an error and still exits 0 when it cannot read the process
        // table: anything on standard error makes the call fail.
        Some(0) if err.trim_ascii().is_empty() => {
            Ps::Rows(String::from_utf8_lossy(out).into_owned())
        }
        Some(1) if out.trim_ascii().is_empty() && err.trim_ascii().is_empty() => Ps::NoSuchProcess,
        _ => {
            tracing::warn!(
                ?code,
                stderr = %String::from_utf8_lossy(err).trim(),
                "ps failed; the process table is unknown"
            );
            Ps::Failed
        }
    }
}

/// Runs `ps` with `args` (bounded by [`TIMEOUT`]).
pub(super) fn run(args: &[&str]) -> Ps {
    let (bin, timeout) = fake::get().unwrap_or_else(|| (PS.into(), TIMEOUT));
    let mut cmd = Command::new(&bin);
    cmd.args(args)
        .env("TZ", "UTC0")
        .env("LC_ALL", "C")
        .env_remove(THREAD_ENV)
        .env_remove(RUNTIME_GEN_ENV)
        .env_remove(ATTACH_ENV)
        .env_remove(AGENT_ENV)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // No terminal: whatever `ps` this is.
    twr_adapters::process::new_session_std(&mut cmd);
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "cannot run ps; the process table is unknown");
            return Ps::Failed;
        }
    };
    // Standard output can be large (a listing with environments): read while ps runs.
    // Standard error is read once it has exited (an error message fits the pipe; one that
    // does not blocks ps until the timeout, which fails the call anyway).
    let mut stdout = child.stdout.take();
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(s) = stdout.as_mut() {
            let _ = s.read_to_end(&mut buf);
        }
        buf
    });
    let deadline = Instant::now() + timeout;
    let mut pause = Duration::from_millis(1);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(pause);
                pause = (pause * 2).min(Duration::from_millis(10));
            }
            waited => {
                // Not reaped yet, so the pid is still this child's.
                let _ = child.kill();
                tracing::warn!(
                    timeout_ms = timeout.as_millis() as u64,
                    error = ?waited.err(),
                    "ps did not finish in time; killed it, the process table is unknown"
                );
                // Reaped off this thread: a ps stuck in the kernel may not die at once.
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return Ps::Failed;
            }
        }
    };
    let mut err = Vec::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_end(&mut err);
    }
    let Ok(out) = reader.join() else {
        return Ps::Failed;
    };
    classify(status.code(), &out, &err)
}

/// Test hook: `ps` replaced by another program (and its timeout), for the calls of the current
/// thread.
mod fake {
    use std::path::PathBuf;
    use std::time::Duration;

    #[cfg(test)]
    thread_local! {
        static FAKE: std::cell::RefCell<Option<(PathBuf, Duration)>> =
            const { std::cell::RefCell::new(None) };
    }

    #[cfg(test)]
    pub fn get() -> Option<(PathBuf, Duration)> {
        FAKE.with(|f| f.borrow().clone())
    }

    #[cfg(not(test))]
    pub fn get() -> Option<(PathBuf, Duration)> {
        None
    }

    /// Runs `f` with `bin` as `ps` (and `timeout`) on this thread.
    #[cfg(test)]
    pub fn with<R>(bin: &std::path::Path, timeout: Duration, f: impl FnOnce() -> R) -> R {
        let prev = FAKE.with(|x| x.replace(Some((bin.to_path_buf(), timeout))));
        let out = f();
        FAKE.with(|x| *x.borrow_mut() = prev);
        out
    }
}

#[cfg(test)]
pub(super) use fake::with as with_fake_ps;

/// The state, parent, process group and start time of process `pid`: [`Lookup::Gone`] when
/// `ps` lists no such process, [`Lookup::Unknown`] when it failed or printed something
/// unparseable.
pub fn stat(pid: u32) -> Lookup {
    let pid = pid.to_string();
    match run(&["-o", "stat=,ppid=,pgid=,lstart=", "-p", &pid]) {
        Ps::Rows(out) => match out.lines().find(|l| !l.trim().is_empty()) {
            None => Lookup::Gone,
            Some(line) => parse_stat_line(line).map_or(Lookup::Unknown, Lookup::Found),
        },
        Ps::NoSuchProcess => Lookup::Gone,
        Ps::Failed => Lookup::Unknown,
    }
}

/// The output of a successful `ps` call; `None` when there is no such process or it failed.
fn rows(args: &[&str]) -> Option<String> {
    match run(args) {
        Ps::Rows(out) => Some(out),
        Ps::NoSuchProcess | Ps::Failed => None,
    }
}

/// The environment of process `pid`, NUL-separated like `/proc/<pid>/environ`; an error when
/// it is unknown (see the module docs).
pub fn environ(pid: u32) -> std::io::Result<Vec<u8>> {
    let pid = pid.to_string();
    let unknown = || std::io::Error::other("environment not shown by ps");
    let plain = rows(&["-ww", "-o", "command=", "-p", &pid]).ok_or_else(unknown)?;
    let with_env = rows(&["-ww", ENV_FLAG, "-o", "command=", "-p", &pid]).ok_or_else(unknown)?;
    env_suffix(
        plain.lines().next().unwrap_or_default(),
        with_env.lines().next().unwrap_or_default(),
    )
    .ok_or_else(unknown)
}

/// One process of [`chain_environs`]: its effective user id, its parent and its environment
/// ([`environ`]'s format; `None` when `ps` does not show it: another user's, or one it cannot
/// read, an empty one, or one that exec'd between the calls).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    /// Effective user id (`uid`).
    pub uid: u32,
    /// Parent pid.
    pub ppid: u32,
    /// Environment.
    pub environ: Option<Vec<u8>>,
}

/// What [`chain_environs`] read: this process's effective user id and the processes listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainEnvirons {
    /// This process's effective user id.
    pub uid: u32,
    /// Each process of the chain that `ps` listed (a missing one is gone), this one too.
    pub procs: HashMap<u32, Listed>,
}

/// The processes `pids` (and this one, for its user id) with their user, parent and
/// environment, from two calls for all of them. `None` when a call failed or did not list this
/// process (the process table is unknown). A process missing from the second call is left out
/// (gone).
pub fn chain_environs(pids: &[u32]) -> Option<ChainEnvirons> {
    let me = std::process::id();
    let list = pids
        .iter()
        .chain([&me])
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let plain = rows(&["-ww", "-o", "pid=,uid=,ppid=,command=", "-p", &list])?;
    let with_env = rows(&["-ww", ENV_FLAG, "-o", "pid=,command=", "-p", &list])?;
    let with_env: HashMap<u32, &str> = with_env
        .lines()
        .filter_map(|l| {
            let ([pid], command) = fields::<1>(l)?;
            Some((pid.parse().ok()?, command))
        })
        .collect();
    let procs: HashMap<u32, Listed> = plain
        .lines()
        .filter_map(|l| {
            let ([pid, uid, ppid], command) = fields::<3>(l)?;
            let pid: u32 = pid.parse().ok()?;
            let listed = Listed {
                uid: uid.parse().ok()?,
                ppid: ppid.parse().ok()?,
                environ: env_suffix(command, with_env.get(&pid)?),
            };
            Some((pid, listed))
        })
        .collect();
    Some(ChainEnvirons {
        uid: procs.get(&me)?.uid,
        procs,
    })
}

/// The program of process `pid` (`argv[0]`, see the module docs).
pub fn cmdline(pid: u32) -> Option<Vec<u8>> {
    let pid = pid.to_string();
    let comm = rows(&["-ww", "-o", "comm=", "-p", &pid])?;
    let comm = comm.lines().next()?.trim();
    (!comm.is_empty()).then(|| comm.as_bytes().to_vec())
}

/// The arguments of process `pid`: its program (`comm`, the whole `argv[0]` on macOS, spaces
/// and all), then the rest of its `args` split at spaces (`ps` shows no boundaries between
/// arguments: one with a space in it comes out as several).
pub fn args(pid: u32) -> Option<Vec<String>> {
    let pid = pid.to_string();
    let comm = rows(&["-ww", "-o", "comm=", "-p", &pid])?;
    let comm = comm.lines().next()?.trim().to_string();
    let line = rows(&["-ww", "-o", "args=", "-p", &pid])?;
    let line = line.lines().next()?.trim().to_string();
    if comm.is_empty() {
        return None;
    }
    let rest = line.strip_prefix(comm.as_str()).unwrap_or(&line);
    let mut args = vec![comm.clone()];
    args.extend(rest.split_whitespace().map(str::to_string));
    Some(args)
}

/// Every process of group `pgid` with its status (environment or not), from one listing.
/// `None` when the listing failed (the process table is unknown).
pub fn group(pgid: i64) -> Option<Vec<(u32, ProcStat)>> {
    let Ps::Rows(rows) = run(&["-A", "-o", STAT_ROW]) else {
        return None;
    };
    Some(
        rows.lines()
            .filter_map(parse_row)
            .filter(|(_, stat, _)| stat.pgrp == pgid)
            .map(|(pid, stat, _)| (pid, stat))
            .collect(),
    )
}

/// Every process with its status, from one listing (no environments), for walking a
/// process's ancestry ([`super::ancestry`]). `None` when the listing failed (the process table
/// is unknown).
pub fn table() -> Option<HashMap<u32, ProcStat>> {
    let Ps::Rows(rows) = run(&["-A", "-o", STAT_ROW]) else {
        return None;
    };
    Some(stat_rows(&rows))
}

/// The processes `pids` that exist, with their status, from one call: what confirms an
/// ancestry read from [`table`] ([`super::ancestry`]). `None` when the call failed (the
/// process table is unknown).
pub fn table_of(pids: &[u32]) -> Option<HashMap<u32, ProcStat>> {
    let list = pids
        .iter()
        .map(u32::to_string)
        .collect::<Vec<_>>()
        .join(",");
    match run(&["-o", STAT_ROW, "-p", &list]) {
        Ps::Rows(rows) => Some(stat_rows(&rows)),
        Ps::NoSuchProcess => Some(HashMap::new()),
        Ps::Failed => None,
    }
}

/// Columns of a status row ([`table`], [`table_of`]): a listing row without the command.
const STAT_ROW: &str = "pid=,stat=,ppid=,pgid=,lstart=";

/// The rows of a [`STAT_ROW`] listing by pid (unparseable rows left out).
fn stat_rows(rows: &str) -> HashMap<u32, ProcStat> {
    rows.lines()
        .filter_map(parse_row)
        .map(|(pid, stat, _)| (pid, stat))
        .collect()
}

/// Every process `keep` accepts (given its pid and stat) whose environment is known, with that
/// environment ([`environ`]'s format). Two listings of every process; a process missing from
/// either, or whose start time or group differs between them, is left out. `None` when either
/// listing failed (the process table is unknown).
pub fn scan(keep: impl Fn(u32, &ProcStat) -> bool) -> Option<Vec<(u32, ProcStat, Vec<u8>)>> {
    let Ps::Rows(plain) = run(&["-A", "-ww", "-o", ROW]) else {
        return None;
    };
    let Ps::Rows(with_env) = run(&["-A", "-ww", ENV_FLAG, "-o", ROW]) else {
        return None;
    };
    let plain: HashMap<u32, (ProcStat, &str)> = plain
        .lines()
        .filter_map(parse_row)
        .map(|(pid, stat, command)| (pid, (stat, command)))
        .collect();
    Some(
        with_env
            .lines()
            .filter_map(parse_row)
            .filter_map(|(pid, stat, command)| {
                let (before, plain_command) = plain.get(&pid)?;
                if before.start_time != stat.start_time || before.pgrp != stat.pgrp {
                    return None;
                }
                if !keep(pid, &stat) {
                    return None;
                }
                Some((pid, stat, env_suffix(plain_command, command)?))
            })
            .collect(),
    )
}

/// Whether process `pid` runs as this process's effective user; `None` when `ps` cannot tell.
pub fn same_user(pid: u32) -> Option<bool> {
    let out = rows(&["-o", "uid=", "-p", &pid.to_string()])?;
    let uid: u32 = out.trim().parse().ok()?;
    Some(uid == twr_core::paths::effective_uid())
}

/// [`scan`], also listing every process `keep` accepts that this user runs but whose
/// environment `ps` does not show, with `None` for its environment (another user's are left
/// out: a runtime runs as this user). `None` when `ps` fails.
pub fn scan_unresolved(keep: impl Fn(u32, &ProcStat) -> bool) -> Option<Vec<super::Candidate>> {
    let shown = scan(&keep)?;
    let Ps::Rows(plain) = run(&["-A", "-ww", "-o", ROW]) else {
        return None;
    };
    let Ps::Rows(uids) = run(&["-A", "-o", "pid=,uid="]) else {
        return None;
    };
    let me = twr_core::paths::effective_uid();
    let mine: std::collections::HashSet<u32> = uids
        .lines()
        .filter_map(|l| {
            let mut f = l.split_whitespace();
            let pid: u32 = f.next()?.parse().ok()?;
            let uid: u32 = f.next()?.parse().ok()?;
            (uid == me).then_some(pid)
        })
        .collect();
    let seen: std::collections::HashSet<u32> = shown.iter().map(|s| s.0).collect();
    let mut out: Vec<super::Candidate> =
        shown.into_iter().map(|(p, s, e)| (p, s, Some(e))).collect();
    out.extend(
        plain
            .lines()
            .filter_map(parse_row)
            .filter(|(pid, stat, _)| !seen.contains(pid) && mine.contains(pid) && keep(*pid, stat))
            .map(|(pid, stat, _)| (pid, stat, None)),
    );
    Some(out)
}

/// The environment in `with_env` (a command line printed with its environment) after
/// `plain` (the same command line printed without it), NUL-separated; `None` when `with_env`
/// does not continue `plain` with at least one entry.
pub fn env_suffix(plain: &str, with_env: &str) -> Option<Vec<u8>> {
    let rest = with_env.strip_prefix(plain)?.strip_prefix(' ')?;
    let entries: Vec<&str> = rest.split(' ').filter(|e| !e.is_empty()).collect();
    (!entries.is_empty()).then(|| entries.join("\0").into_bytes())
}

/// Splits the first `N` space-separated fields off `line`; the rest, leading spaces removed.
fn fields<const N: usize>(line: &str) -> Option<([&str; N], &str)> {
    let mut out = [""; N];
    let mut rest = line;
    for field in &mut out {
        rest = rest.trim_start();
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        if end == 0 {
            return None;
        }
        *field = &rest[..end];
        rest = &rest[end..];
    }
    Some((out, rest.trim_start()))
}

/// Parses `stat ppid pgid lstart` (`lstart` is five fields).
pub fn parse_stat_line(line: &str) -> Option<ProcStat> {
    let ([stat, ppid, pgid, a, b, c, d, e], rest) = fields::<8>(line)?;
    if !rest.is_empty() {
        return None;
    }
    Some(ProcStat {
        state: stat.chars().next()?,
        ppid: ppid.parse().ok()?,
        pgrp: pgid.parse().ok()?,
        start_time: parse_lstart([a, b, c, d, e])?,
    })
}

/// Parses a listing row: `pid stat ppid pgid lstart command`.
pub fn parse_row(line: &str) -> Option<(u32, ProcStat, &str)> {
    let ([pid, stat, ppid, pgid, a, b, c, d, e], command) = fields::<9>(line)?;
    Some((
        pid.parse().ok()?,
        ProcStat {
            state: stat.chars().next()?,
            ppid: ppid.parse().ok()?,
            pgrp: pgid.parse().ok()?,
            start_time: parse_lstart([a, b, c, d, e])?,
        },
        command,
    ))
}

/// Parses `lstart` in the C locale and UTC (`Wed Oct  1 12:34:56 2026`, split into its five
/// fields) into seconds since the Unix epoch.
pub fn parse_lstart([_weekday, month, day, time, year]: [&str; 5]) -> Option<u64> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let month = MONTHS.iter().position(|m| *m == month)? as i64 + 1;
    let day: i64 = day.parse().ok().filter(|d| (1..=31).contains(d))?;
    let year: i64 = year.parse().ok().filter(|y| *y >= 1970)?;
    let mut hms = time.split(':').map(|t| t.parse::<i64>().ok());
    let (h, m, s) = (hms.next()??, hms.next()??, hms.next()??);
    if hms.next().is_some() || h > 23 || m > 59 || s > 60 || h < 0 || m < 0 || s < 0 {
        return None;
    }
    // Days from the epoch to the civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + h * 3600 + m * 60 + s).ok()
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// macOS's format for `TZ=UTC0 LC_ALL=C ps -o stat=,ppid=,pgid=,lstart= -p <pid>` (its
    /// ps(1) pads columns and prints `lstart` as `%a %b %e %T %Y`).
    const MAC_STAT: &str = "Ss       1 81234 Wed Oct  1 12:34:56 2026";

    #[test]
    fn lstart_is_seconds_since_the_epoch() {
        let t = |s: &str| {
            let f: Vec<&str> = s.split_whitespace().collect();
            parse_lstart([f[0], f[1], f[2], f[3], f[4]])
        };
        assert_eq!(t("Thu Jan  1 00:00:00 1970"), Some(0));
        assert_eq!(t("Wed Oct  1 12:34:56 2026"), Some(1_790_858_096));
        assert_eq!(t("Thu Feb 29 23:59:59 2024"), Some(1_709_251_199));
        assert_eq!(t("Fri Mar  1 00:00:00 2024"), Some(1_709_251_200));
        assert_eq!(t("Fri Dec 31 23:59:59 1999"), Some(946_684_799));
        assert_eq!(t("Wed Okt  1 12:34:56 2026"), None);
        assert_eq!(t("Wed Oct 32 12:34:56 2026"), None);
        assert_eq!(t("Wed Oct  1 12:34 2026"), None);
        assert_eq!(t("Wed Oct  1 24:00:00 2026"), None);
        assert_eq!(t("Wed Oct  1 12:34:56 1969"), None);
    }

    #[test]
    fn stat_lines_and_listing_rows_parse() {
        assert_eq!(
            parse_stat_line(MAC_STAT),
            Some(ProcStat {
                state: 'S',
                ppid: 1,
                pgrp: 81234,
                start_time: 1_790_858_096
            })
        );
        assert_eq!(
            parse_stat_line("Z+       1       1 Wed Oct  1 12:34:56 2026\n").map(|s| s.state),
            Some('Z')
        );
        assert_eq!(parse_stat_line(""), None);
        assert_eq!(parse_stat_line("S 1 1 Wed Oct  1 12:34:56"), None);
        assert_eq!(parse_stat_line("S 1 x Wed Oct  1 12:34:56 2026"), None);

        // A row of `ps -A -ww -o pid=,stat=,ppid=,pgid=,lstart=,command=`: right-aligned
        // pids, a command with spaces.
        let row = "  812 S      640   812 Wed Oct  1 12:34:56 2026 /Applications/My App.app/x --flag a  b";
        let (pid, stat, command) = parse_row(row).unwrap();
        assert_eq!(
            (pid, stat.ppid, stat.pgrp, stat.state),
            (812, 640, 812, 'S')
        );
        assert_eq!(stat.start_time, 1_790_858_096);
        assert_eq!(command, "/Applications/My App.app/x --flag a  b");
        assert_eq!(parse_row("812 S 1 812 Wed Oct  1 12:34:56"), None);
        assert_eq!(parse_row("x S 1 812 Wed Oct  1 12:34:56 2026 sleep"), None);
        // A process without a command line still parses (its environment stays unknown).
        assert_eq!(
            parse_row("1 Ss 0 1 Wed Oct  1 12:34:56 2026").map(|r| r.2),
            Some("")
        );
    }

    #[test]
    fn the_environment_is_what_follows_the_plain_command_line() {
        // `ps -ww -o command=` and `ps -ww -E -o command=` (macOS appends the environment).
        let plain = "sleep 30";
        let with_env =
            "sleep 30 TWR_THREAD=01T TWR_RUNTIME_GEN=7 HOME=/Users/me PATH=/usr/bin:/bin";
        let env = env_suffix(plain, with_env).unwrap();
        assert_eq!(
            env,
            b"TWR_THREAD=01T\0TWR_RUNTIME_GEN=7\0HOME=/Users/me\0PATH=/usr/bin:/bin"
        );
        assert!(super::super::is_marked(&env, "01T", Some("7")));
        // Not shown (another user's process), or empty: unknown.
        assert_eq!(env_suffix(plain, plain), None);
        assert_eq!(env_suffix(plain, "sleep 30 "), None);
        // Another command line (exec'd between the two calls): unknown.
        assert_eq!(env_suffix(plain, "sh -c x TWR_THREAD=01T"), None);
        assert_eq!(env_suffix("sleep 3", "sleep 30 TWR_THREAD=01T"), None);
        // A value with spaces splits into pieces; a marker is only recognized whole.
        let env = env_suffix("x", "x A=b c TWR_THREAD=01T").unwrap();
        assert!(super::super::environ_has(&env, THREAD_ENV, "01T"));
        assert!(!super::super::environ_has(&env, "A", "b c"));
    }

    /// A fake `ps` running `script` (sh), in `dir`.
    pub(in super::super) fn fake_ps(
        dir: &std::path::Path,
        name: &str,
        script: &str,
    ) -> std::path::PathBuf {
        // (Through `cp`: a `ps` that fails to run with ETXTBSY would read as one that failed.)
        let path = dir.join(name);
        crate::test_support::install(&path, format!("#!/bin/sh\n{script}\n"));
        path
    }

    /// A `ps` that fails (an error, another exit code, a signal, a missing program) leaves the
    /// process table unknown; only a silent exit 1 means "no such process".
    #[test]
    fn a_failing_ps_is_unknown_not_gone() {
        let dir = tempfile::tempdir().unwrap();
        let t = Duration::from_secs(5);
        let none = fake_ps(dir.path(), "none", "exit 1");
        with_fake_ps(&none, t, || {
            assert_eq!(run(&["-p", "1"]), Ps::NoSuchProcess);
            assert_eq!(stat(1), Lookup::Gone);
            assert!(environ(1).is_err());
            assert_eq!(cmdline(1), None);
        });
        let empty = fake_ps(dir.path(), "empty", "exit 0");
        with_fake_ps(&empty, t, || assert_eq!(stat(1), Lookup::Gone));
        for (name, script) in [
            ("err", "echo 'ps: something broke' >&2; exit 1"),
            (
                "err0",
                "echo 'ps: sysctl: Operation not permitted' >&2; exit 0",
            ),
            ("code", "exit 2"),
            ("signal", "kill -9 $$"),
            ("garbage", "echo 'not a stat line'"),
        ] {
            let bin = fake_ps(dir.path(), name, script);
            with_fake_ps(&bin, t, || {
                assert_eq!(stat(1), Lookup::Unknown, "{name}");
                assert!(scan(|_, _| true).is_none() || name == "garbage", "{name}");
                assert!(environ(1).is_err(), "{name}");
            });
        }
        with_fake_ps(&dir.path().join("missing"), t, || {
            assert_eq!(run(&["-p", "1"]), Ps::Failed);
            assert_eq!(stat(1), Lookup::Unknown);
            assert!(scan(|_, _| true).is_none());
        });
        // macOS shows no boundaries between arguments: a program and a socket path with spaces.
        let hub = fake_ps(
            dir.path(),
            "hub",
            "case \"$*\" in\n\
             *comm=*) echo '/Users/my user/bin/twr' ;;\n\
             *args=*) echo '/Users/my user/bin/twr --socket /tmp/my home/ws/a.sock hub' ;;\n\
             esac",
        );
        with_fake_ps(&hub, t, || {
            let args = args(7).unwrap();
            assert_eq!(
                args,
                [
                    "/Users/my user/bin/twr",
                    "--socket",
                    "/tmp/my",
                    "home/ws/a.sock",
                    "hub"
                ]
            );
            assert_eq!(
                super::super::twr_parent(&args),
                Some(super::super::TwrParent::Hub)
            );
        });
        // The real thing, through the same path.
        let real = fake_ps(dir.path(), "real", "exec ps \"$@\"");
        with_fake_ps(&real, t, || {
            assert!(matches!(stat(std::process::id()), Lookup::Found(_)));
            assert!(scan(|p, _| p == std::process::id()).is_some());
        });
    }

    /// A `ps` that hangs is killed after the timeout, and the call fails (unknown).
    #[cfg(target_os = "linux")]
    #[test]
    fn a_hanging_ps_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let bin = fake_ps(
            dir.path(),
            "hang",
            &format!("echo $$ > {}; exec sleep 30", pidfile.display()),
        );
        let started = Instant::now();
        let got = with_fake_ps(&bin, Duration::from_millis(300), || stat(1));
        assert_eq!(got, Lookup::Unknown);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        let pid: u32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::path::Path::new(&format!("/proc/{pid}")).exists() {
            assert!(Instant::now() < deadline, "the hanging ps was not killed");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// procps `ps` and `/proc` agree on a live process: the same group and environment, and a
    /// start time of a moment ago.
    #[cfg(target_os = "linux")]
    #[test]
    fn ps_agrees_with_proc() {
        use std::os::unix::process::CommandExt;
        use std::time::{Duration, Instant};
        let mut c = Command::new("sleep")
            .arg("30")
            .process_group(0)
            .env(THREAD_ENV, "01PSCHECK")
            .env(RUNTIME_GEN_ENV, "g1")
            .spawn()
            .unwrap();
        let pid = c.id();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !std::fs::read(format!("/proc/{pid}/environ"))
            .is_ok_and(|e| super::super::is_marked(&e, "01PSCHECK", Some("g1")))
        {
            assert!(Instant::now() < deadline, "sleep never exec'd");
            std::thread::sleep(Duration::from_millis(5));
        }
        let proc_stat =
            super::super::parse_stat(&std::fs::read(format!("/proc/{pid}/stat")).unwrap()).unwrap();
        let Lookup::Found(s) = stat(pid) else {
            panic!("ps does not list {pid}")
        };
        assert_eq!(s.pgrp, proc_stat.pgrp);
        assert_eq!(s.pgrp, i64::from(pid));
        assert_eq!(s.ppid, proc_stat.ppid);
        assert_eq!(s.ppid, std::process::id());
        let listed = table().unwrap()[&pid];
        assert_eq!(
            (listed.ppid, listed.pgrp, listed.start_time),
            (s.ppid, s.pgrp, s.start_time)
        );
        let me = std::process::id();
        let both = table_of(&[pid, me]).unwrap();
        assert_eq!(both.len(), 2, "{both:?}");
        assert_eq!(both[&pid], listed);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(
            s.start_time <= now && now - s.start_time < 60,
            "{s:?} vs {now}"
        );
        assert!(super::super::argv0_matches(&cmdline(pid).unwrap(), "sleep"));
        let env = environ(pid).unwrap();
        assert!(super::super::is_marked(&env, "01PSCHECK", Some("g1")));
        assert!(!super::super::is_marked(&env, "01PSCHECK", Some("g2")));
        // Several processes in one pair of calls, with their user and parent.
        let envs = chain_environs(&[pid, std::process::id()]).unwrap();
        assert_eq!(envs.procs.len(), 2, "{envs:?}");
        assert_eq!(envs.procs[&pid].environ.as_ref(), Some(&env));
        assert_eq!(envs.procs[&pid].ppid, std::process::id());
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
        let euid = status
            .lines()
            .find_map(|l| l.strip_prefix("Uid:"))
            .and_then(|v| v.split_whitespace().nth(1))
            .unwrap();
        assert_eq!(envs.procs[&pid].uid.to_string(), euid);
        assert_eq!(envs.uid.to_string(), euid);
        let found = scan(|p, _| p == pid).unwrap();
        assert_eq!(found.len(), 1, "{found:?}");
        // The recovery's scan finds it with its environment, run by this user.
        let unresolved = scan_unresolved(|p, _| p == pid).unwrap();
        assert_eq!(unresolved.len(), 1, "{unresolved:?}");
        assert_eq!(unresolved[0].2.as_ref(), Some(&env));
        assert_eq!(same_user(pid), Some(true));
        let members = group(i64::from(pid)).unwrap();
        assert_eq!(members.len(), 1, "{members:?}");
        assert_eq!(members[0].0, pid);
        assert_eq!(super::super::group_running(pid), Some(true));
        assert_eq!(found[0].1.start_time, s.start_time);
        assert!(super::super::is_marked(
            &found[0].2,
            "01PSCHECK",
            Some("g1")
        ));
        // ps itself never carries the caller's markers.
        assert!(scan(|p, _| p != pid)
            .unwrap()
            .iter()
            .all(|(_, _, e)| !super::super::environ_has(e, THREAD_ENV, "01PSCHECK")));
        let _ = c.kill();
        let _ = c.wait();
        assert_eq!(stat(pid), Lookup::Gone);
        assert_eq!(table_of(&[pid]), Some(HashMap::new()));
        assert_eq!(group(i64::from(pid)), Some(Vec::new()));
        assert_eq!(super::super::group_running(pid), Some(false));
        assert!(environ(pid).is_err());
        let envs = chain_environs(&[pid]).unwrap();
        assert_eq!(envs.procs.keys().collect::<Vec<_>>(), [&std::process::id()]);
    }
}
