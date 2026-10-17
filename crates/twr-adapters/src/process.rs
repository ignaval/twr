//! Process plumbing shared by the adapters: the bounded stdout line reader of the headless
//! adapters and the environment scrubbing every runtime (and `attach`) command gets.

use std::io;

use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};

/// twr secrets a runtime process must never inherit: the runner's hub token, the
/// orchestrator token, and a test's user token (`TWR_TEST_USER_TOKEN_FILE`, which makes a
/// test's clients the user of its throwaway hub; an agent of that hub is no such client). The
/// orchestrator's MCP server gets its token through the MCP configuration (a file or stdin),
/// never through the runtime's environment.
pub const SCRUBBED_ENV: &[&str] = &[
    "TWR_RUNNER_TOKEN",
    "TWR_ORCH_TOKEN",
    twr_core::client::TEST_USER_TOKEN_ENV,
];

/// Removes [`SCRUBBED_ENV`] from a runtime command (whether inherited or set on it).
pub fn scrub(cmd: &mut tokio::process::Command) {
    for k in SCRUBBED_ENV {
        cmd.env_remove(k);
    }
}

/// A command that runs `program` under `wrap` ([`twr_core::adapter::SpawnConfig::wrap`]):
/// `wrap`'s first word with the rest of it, then `program`; `program` itself when `wrap` is
/// empty. The process started keeps its id when the wrapper runs `program` in its place
/// (`systemd-run --scope`, `nice` do).
pub fn wrapped(wrap: &[String], program: impl AsRef<std::ffi::OsStr>) -> tokio::process::Command {
    match wrap.split_first() {
        Some((first, rest)) => {
            let mut cmd = tokio::process::Command::new(first);
            cmd.args(rest).arg(program);
            cmd
        }
        None => tokio::process::Command::new(program),
    }
}

/// Before starting `program` in `cwd` with `env` added under a non-empty `wrap`
/// ([`wrapped`]): the error [`start_error`] gives for a program that is not there, which the
/// wrapper would otherwise hide (it starts, then fails to run the program, and the runtime
/// reads as crashed). A bare name is looked up on `env`'s `PATH` (its last), else the hub's,
/// as the program itself would be. `Ok` without a wrap, or when the program is there.
pub fn check_wrapped(
    wrap: &[String],
    program: &std::path::Path,
    env: &[(String, String)],
    cwd: &std::path::Path,
    fix: &str,
) -> anyhow::Result<()> {
    if wrap.is_empty() || !(cwd.as_os_str().is_empty() || cwd.is_dir()) {
        return Ok(());
    }
    let base = if cwd.as_os_str().is_empty() {
        std::env::current_dir().unwrap_or_default()
    } else {
        cwd.to_path_buf()
    };
    let path = search_path(env);
    match missing_program(program, path.as_deref(), &base) {
        Some(why) => Err(anyhow::anyhow!("{why} ({fix})")),
        None => Ok(()),
    }
}

/// The `PATH` `env` sets (its last entry, as the process started with it sees), if it sets one.
pub fn env_path(env: &[(String, String)]) -> Option<&str> {
    env.iter()
        .rev()
        .find(|(k, _)| k == "PATH")
        .map(|(_, v)| v.as_str())
}

/// Where a program started with `env` added is looked up: `env`'s own `PATH`
/// ([`env_path`]), else the runner's (this process's).
pub fn search_path(env: &[(String, String)]) -> Option<std::ffi::OsString> {
    env_path(env)
        .map(std::ffi::OsString::from)
        .or_else(|| std::env::var_os("PATH"))
}

/// [`scrub`] for a `std` command (`attach`).
pub fn scrub_std(cmd: &mut std::process::Command) {
    for k in SCRUBBED_ENV {
        cmd.env_remove(k);
    }
}

/// The variables of `cfg.env` an interactive command (`attach`) runs with, as its runtime
/// does: the configured ones of a worker thread (`[threads] env`, `[projects.<name>] env`), not
/// twr's own `TWR_*` entries (a runtime's command, the markers of a runtime process). Set before
/// [`scrub_std`].
pub fn attach_env(cfg: &twr_core::adapter::SpawnConfig) -> impl Iterator<Item = (&str, &str)> {
    cfg.env
        .iter()
        .filter(|(k, _)| !k.to_ascii_uppercase().starts_with("TWR_"))
        .map(|(k, v)| (k.as_str(), v.as_str()))
}

/// [`scrub`] for a PTY command (portable-pty copies the whole environment into it).
pub fn scrub_pty(cmd: &mut portable_pty::CommandBuilder) {
    for k in SCRUBBED_ENV {
        cmd.env_remove(k);
    }
}

/// Why runtime program `program` (an argv[0]) cannot be run, when it cannot: a bare name
/// (`claude`) that is on no directory of `path` (a `PATH` value) as an executable file, or a
/// path (`./bin/agent`, `/opt/agent`; a relative one from `cwd`, where the program starts)
/// that is not an executable file. Leads with the program: "`claude` not found on PATH". `None`
/// when it can be run, or `program` is empty.
pub fn missing_program(
    program: &std::path::Path,
    path: Option<&std::ffi::OsStr>,
    cwd: &std::path::Path,
) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    if program.as_os_str().is_empty() {
        return None;
    }
    let runnable = |p: &std::path::Path| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    let shown = program.display();
    if program.components().count() > 1 || program.is_absolute() {
        let p = cwd.join(program);
        return match std::fs::metadata(&p) {
            _ if runnable(&p) => None,
            Ok(_) => Some(format!("`{shown}` is not an executable file")),
            Err(_) => Some(format!("`{shown}` not found")),
        };
    }
    let found = find_program(program, path, cwd).is_some();
    (!found).then(|| format!("`{shown}` not found on PATH"))
}

/// The executable file runtime program `program` (an argv[0]) names: a bare name looked up on
/// the directories of `path` (a `PATH` value), a path taken from `cwd`. A relative directory
/// of `path` is taken from `cwd` too, and an empty one is `cwd` itself: where `execvp` in the
/// program's working directory looks. `None` when there is no such executable file, or
/// `program` is empty.
pub fn find_program(
    program: &std::path::Path,
    path: Option<&std::ffi::OsStr>,
    cwd: &std::path::Path,
) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    if program.as_os_str().is_empty() {
        return None;
    }
    let runnable = |p: &std::path::Path| {
        std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if program.components().count() > 1 || program.is_absolute() {
        let p = cwd.join(program);
        return runnable(&p).then_some(p);
    }
    std::env::split_paths(path?)
        .map(|d| cwd.join(d).join(program))
        .find(|p| runnable(p))
}

/// The error of starting runtime program `program` in `cwd` with `env` added (`e`, from
/// spawning it). A program that is not there ([`missing_program`] on `env`'s `PATH`, else the
/// hub's: [`search_path`]) leads with its name and `fix`,
/// how to install it or point twr at another one: "`claude` not found on PATH (install Claude
/// Code, or set TWR_CLAUDE_BIN)". Anything else, a missing working directory included, is
/// "starting <program> in <cwd>: <e>".
pub fn start_error(
    e: io::Error,
    program: &std::path::Path,
    env: &[(String, String)],
    cwd: &std::path::Path,
    fix: &str,
) -> anyhow::Error {
    let cwd_ok = cwd.as_os_str().is_empty() || cwd.is_dir();
    let base = if cwd.as_os_str().is_empty() {
        std::env::current_dir().unwrap_or_default()
    } else {
        cwd.to_path_buf()
    };
    let path = search_path(env);
    if let Some(why) = (e.kind() == io::ErrorKind::NotFound && cwd_ok)
        .then(|| missing_program(program, path.as_deref(), &base))
        .flatten()
    {
        return anyhow::anyhow!("{why} ({fix})");
    }
    anyhow::Error::new(e).context(format!(
        "starting {} in {}",
        program.display(),
        cwd.display()
    ))
}

/// Starts `cmd` in a session of its own (`setsid`): without a controlling terminal, so a
/// process that tries to use one (an interactive login shell, a CLI asking a question, a
/// command reading `/dev/tty`) gets an error instead of being stopped by the terminal of
/// whoever started twr (SIGTTIN, SIGTTOU), and in a process group of its own (its process id,
/// as `process_group(0)` makes it), which [`kill_group`] ends with whatever it started. Not
/// together with `process_group(0)`: a group leader cannot start a session (`EPERM`). A
/// process it starts that makes a session of its own (`setsid`, a daemon) leaves that group.
pub fn new_session(cmd: &mut tokio::process::Command) {
    // SAFETY: `setsid` is async-signal-safe, and nothing else runs between fork and exec.
    unsafe {
        cmd.pre_exec(setsid);
    }
}

/// [`new_session`] for a `std` command.
pub fn new_session_std(cmd: &mut std::process::Command) {
    use std::os::unix::process::CommandExt;
    // SAFETY: `setsid` is async-signal-safe, and nothing else runs between fork and exec.
    unsafe {
        cmd.pre_exec(setsid);
    }
}

/// `setsid(2)`, between fork and exec ([`new_session`]): it allocates nothing (an `io::Error`
/// of an OS error does not).
fn setsid() -> io::Result<()> {
    // SAFETY: setsid(2) takes nothing and touches no memory of this process.
    if unsafe { libc::setsid() } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Sends `SIGKILL` to process group `pgid`. Only while the group's leader is not reaped (it
/// is this process's child): until then its id names no other process or group.
pub fn kill_group(pgid: u32) {
    // SAFETY: kill(2) with a negative pid signals that process group; no memory is passed.
    let r = unsafe { libc::kill(-(pgid as libc::pid_t), libc::SIGKILL) };
    tracing::debug!(pgid, signalled = r == 0, "killed a process group");
}

/// Waits for `child`, spawned as the leader of a process group of its own (`process_group(0)`
/// or [`new_session`]), to exit, kills what is left of that group (what it started in the
/// background, which would otherwise outlive it), then reaps it: its exit status. The group
/// is killed while the child is not reaped yet (it waits with `waitid(WEXITED | WNOWAIT)`,
/// [`exited_unreaped`]): until then its pid, which is the group's id, cannot name another
/// process or group. That wait only says the child has exited: what `waitid` reports of it is
/// not read (macOS does not fill it in reliably); the status is the reap's. Should the wait
/// fail:
///
/// - with `ECHILD`, the child is no longer this process's to wait for (something else reaped
///   it): its id may name another process by now, so nothing is killed, and that error is
///   returned at once (no reap is waited for), so the caller lets go of the id before anything
///   of its own (a timeout that drops this, a guard) could signal it;
/// - otherwise, the child is still this process's and not reaped (only the reap below reaps
///   it), so its id still names its group: the group is killed, the child with it (whether it
///   had exited or not), so that the reap that follows cannot wait forever.
///
/// An error for which [`ownership_lost`] holds says the child was reaped by something else:
/// whoever holds its id (a group guard, a `Drop` that kills the group, the `Child`'s own
/// `kill_on_drop`) must not signal it again.
///
/// Cancel safe as far as the group goes: a caller dropped while waiting leaves the child to
/// its own kill (`kill_on_drop`, or the caller's group guard).
pub async fn finish_group(
    child: &mut tokio::process::Child,
) -> io::Result<std::process::ExitStatus> {
    finish_group_with(child, exited_unreaped).await
}

/// [`finish_group`], `wait` standing for [`exited_unreaped`] (tests make it fail).
async fn finish_group_with(
    child: &mut tokio::process::Child,
    wait: fn(u32) -> io::Result<()>,
) -> io::Result<std::process::ExitStatus> {
    if let Some(pid) = child.id() {
        match tokio::task::spawn_blocking(move || wait(pid)).await {
            Ok(Ok(())) => kill_group(pid),
            Ok(Err(e)) if e.raw_os_error() == Some(libc::ECHILD) => {
                tracing::warn!(
                    pid,
                    "waitid: the process is not this one's child any more; its group is not killed"
                );
                // At once, not after a reap: the caller clears what holds the id (its group
                // guard) before any timeout of its own can drop this and leave the id armed.
                return Err(e);
            }
            // Not reaped (only the reap below reaps it): its id still names its group.
            Ok(Err(e)) => {
                tracing::warn!(pid, error = %e, "waitid failed: killing the process group before reaping its leader");
                kill_group(pid);
            }
            Err(e) => {
                tracing::warn!(pid, error = %e, "waiting with waitid failed: killing the process group before reaping its leader");
                kill_group(pid);
            }
        }
    }
    child.wait().await
}

/// Whether `e`, from waiting for a child ([`finish_group`], `Child::wait`), says it is not
/// this process's child any more (`ECHILD`: something else reaped it). Its id may name another
/// process, or group, by then: nothing may signal it again, the `Child`'s `kill_on_drop`
/// included (a holder of such a child spawns it without `kill_on_drop` and kills its group
/// itself while it still owns it, as [`run_bounded`] and the hub's `Ssh` do).
pub fn ownership_lost(e: &io::Error) -> bool {
    e.raw_os_error() == Some(libc::ECHILD)
}

/// Blocks until process `pid` (a child of this process) has exited, without reaping it
/// (`waitid` with `WNOWAIT`; interrupted calls are made again). Only that it has exited: the
/// `siginfo_t` it fills is not read.
fn exited_unreaped(pid: u32) -> io::Result<()> {
    loop {
        // SAFETY: an all-zero `siginfo_t` is a valid value; waitid(2) writes into it.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: waitid(2) with a valid pointer to `info`; WNOWAIT leaves the child
        // waitable (tokio reaps it).
        let r = unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOWAIT,
            )
        };
        if r == 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Kills process group `.0` when dropped, unless cleared first (its leader was reaped, by
/// this process or, [`ownership_lost`], by something else: the id may be another process's by
/// then).
struct GroupGuard(Option<u32>);

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(p) = self.0 {
            kill_group(p);
        }
    }
}

/// How a bounded run of a probe command went ([`run_bounded`]).
#[derive(Debug)]
pub enum Bounded {
    /// It exited within the limit: its exit code (`None`: killed by a signal) and its stdout.
    Exited(Option<i32>, Vec<u8>),
    /// Its stdout so far satisfied the caller before it ended (the rest is not waited for).
    Enough(Vec<u8>),
    /// It could not be started (not there, not executable).
    NotStarted,
    /// It printed more than the cap.
    TooLarge,
    /// It did not finish within the limit.
    TimedOut,
    /// Reading its output or waiting for it failed.
    Failed,
}

/// Runs `cmd` as a probe: stdin `/dev/null`, stderr discarded, twr's secrets removed
/// ([`scrub`]), in a session of its own ([`new_session`]), for at most `limit`, reading at
/// most `max` bytes of its stdout. `enough` is asked after every read: once it says the
/// output so far suffices, the run stops there ([`Bounded::Enough`]). Whatever is left of its
/// process group is killed unless it exited by itself in time (also when this is dropped
/// first; once it exited, [`finish_group`] kills what it left in the background before it is
/// reaped). When it is stopped early (enough, too large, timed out, a failed read), its group
/// is killed and its leader reaped (waiting at most [`REAP_LIMIT`]) before this returns, so a
/// caller that `exec`s next leaves no zombie child to what it runs. Should something else
/// reap it ([`ownership_lost`]), nothing of it is signalled again ([`Bounded::Failed`]). The
/// group, not the `Child`'s `kill_on_drop` (turned off: it would signal the leader's id
/// whoever reaped it), is what is killed, the leader with it. What it printed is never logged.
pub async fn run_bounded(
    cmd: tokio::process::Command,
    limit: std::time::Duration,
    max: usize,
    enough: impl Fn(&[u8]) -> bool,
) -> Bounded {
    run_bounded_with(cmd, limit, max, enough, exited_unreaped).await
}

/// [`run_bounded`], `wait` standing for [`exited_unreaped`] (tests reap the child elsewhere
/// first).
async fn run_bounded_with(
    mut cmd: tokio::process::Command,
    limit: std::time::Duration,
    max: usize,
    enough: impl Fn(&[u8]) -> bool,
    wait: fn(u32) -> io::Result<()>,
) -> Bounded {
    use tokio::io::AsyncReadExt;
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(false);
    scrub(&mut cmd);
    new_session(&mut cmd);
    let Ok(mut child) = cmd.spawn() else {
        return Bounded::NotStarted;
    };
    // Dropped before `child`: the group is killed while its leader is not reaped yet.
    let mut group = GroupGuard(child.id());
    let Some(mut stdout) = child.stdout.take() else {
        return Bounded::Failed;
    };
    let run = async {
        let mut out = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = match stdout.read(&mut buf).await {
                Ok(n) => n,
                Err(_) => return Bounded::Failed,
            };
            if n == 0 {
                break;
            }
            if out.len() + n > max {
                return Bounded::TooLarge;
            }
            out.extend_from_slice(&buf[..n]);
            if enough(&out) {
                return Bounded::Enough(out);
            }
        }
        match finish_group_with(&mut child, wait).await {
            Ok(status) => {
                group.0 = None;
                Bounded::Exited(status.code(), out)
            }
            Err(e) => {
                if ownership_lost(&e) {
                    // Reaped by something else: its id is not this process's to signal.
                    group.0 = None;
                }
                Bounded::Failed
            }
        }
    };
    let outcome = tokio::time::timeout(limit, run)
        .await
        .unwrap_or(Bounded::TimedOut);
    // Not reaped yet (stopped early): kill the group while its leader is unreaped, then reap
    // the leader, bounded (one stuck in the kernel is left to tokio's reaping of dropped
    // children; it was sent SIGKILL with its group).
    if let Some(pgid) = group.0.take() {
        kill_group(pgid);
        let _ = tokio::time::timeout(REAP_LIMIT, child.wait()).await;
    }
    outcome
}

/// How long [`run_bounded`] waits for a probe it killed to be reaped.
pub const REAP_LIMIT: std::time::Duration = std::time::Duration::from_secs(1);

/// Whether `key` is one of [`SCRUBBED_ENV`].
pub fn is_scrubbed(key: &str) -> bool {
    SCRUBBED_ENV.contains(&key)
}

/// Longest stdout line a headless adapter buffers (32 MiB). A longer line is skipped up to its
/// newline and reported as [`Line::Oversized`].
pub const MAX_LINE: usize = 32 * 1024 * 1024;

/// Bytes kept of an oversized line (enough to tell what kind of message it was).
pub const OVERSIZED_HEAD: usize = 4096;

/// One stdout line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    /// A complete line (without `\n` / `\r\n`; invalid UTF-8 replaced).
    Text(String),
    /// A line longer than the cap: only its first [`OVERSIZED_HEAD`] bytes were kept.
    Oversized {
        /// Its full length in bytes.
        len: usize,
        /// Its start.
        head: String,
    },
}

/// Reads newline-delimited lines without ever buffering more than `max` bytes of one line.
///
/// Cancel safe: everything read so far is kept in `self`, so dropping a pending
/// [`BoundedLines::next_line`] (as `tokio::select!` does) loses nothing.
#[derive(Debug)]
pub struct BoundedLines<R> {
    reader: BufReader<R>,
    max: usize,
    buf: Vec<u8>,
    /// Bytes of the current line so far.
    len: usize,
    oversized: u64,
}

impl<R: AsyncRead + Unpin> BoundedLines<R> {
    /// Lines of `r`, each at most `max` bytes.
    pub fn new(r: R, max: usize) -> Self {
        BoundedLines {
            reader: BufReader::new(r),
            max,
            buf: Vec::new(),
            len: 0,
            oversized: 0,
        }
    }

    /// How many oversized lines were skipped so far.
    pub fn oversized_count(&self) -> u64 {
        self.oversized
    }

    /// The next line, or None at EOF.
    pub async fn next_line(&mut self) -> io::Result<Option<Line>> {
        loop {
            let avail = self.reader.fill_buf().await?;
            if avail.is_empty() {
                if self.len == 0 {
                    return Ok(None);
                }
                return Ok(Some(self.take()));
            }
            let (chunk, found) = match avail.iter().position(|b| *b == b'\n') {
                Some(i) => (&avail[..i], true),
                None => (avail, false),
            };
            let n = chunk.len();
            let was_over = self.len > self.max;
            self.len += n;
            if self.len <= self.max {
                self.buf.extend_from_slice(chunk);
            } else if !was_over {
                // Just crossed the cap: keep the head only and drop the rest of the line.
                let keep = OVERSIZED_HEAD.saturating_sub(self.buf.len()).min(n);
                self.buf.extend_from_slice(&chunk[..keep]);
                self.buf.truncate(OVERSIZED_HEAD);
                self.buf.shrink_to_fit();
            }
            self.reader.consume(n + usize::from(found));
            if found {
                return Ok(Some(self.take()));
            }
        }
    }

    fn take(&mut self) -> Line {
        let len = std::mem::take(&mut self.len);
        let mut bytes = std::mem::take(&mut self.buf);
        if len > self.max {
            self.oversized += 1;
            tracing::warn!(
                len,
                max = self.max,
                oversized_lines = self.oversized,
                "skipping an oversized runtime stdout line"
            );
            return Line::Oversized {
                len,
                head: String::from_utf8_lossy(&bytes).into_owned(),
            };
        }
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
        Line::Text(match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        })
    }
}

/// Whether the head of an oversized JSON line has `"key":"value"` (compact or with a space
/// after the colon).
pub fn head_has(head: &str, key: &str, value: &str) -> bool {
    head.contains(&format!("\"{key}\":\"{value}\""))
        || head.contains(&format!("\"{key}\": \"{value}\""))
}

/// The first numeric `"id"` of the head of an oversized JSON-RPC message, when no `"method"`
/// comes before it (so it is a response to one of twr's requests).
pub fn head_response_id(head: &str) -> Option<u64> {
    let at = head.find("\"id\":")?;
    if head[..at].contains("\"method\"") {
        return None;
    }
    let rest = head[at + 5..].trim_start();
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Under a wrapper, a runtime program that is not there is still said to be missing,
    /// with what to do.
    #[test]
    fn a_wrapped_missing_program_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let wrap = ["nice".to_string()];
        let e = check_wrapped(
            &wrap,
            Path::new("./no-agent"),
            &[],
            dir.path(),
            "install it",
        )
        .unwrap_err()
        .to_string();
        assert_eq!(e, "`./no-agent` not found (install it)");
        assert!(check_wrapped(&[], Path::new("./no-agent"), &[], dir.path(), "x").is_ok());
        assert!(check_wrapped(&wrap, Path::new("sh"), &[], dir.path(), "x").is_ok());
        // A bare name on the worker's own PATH (`[threads] env`) is there.
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        std::fs::write(bin.join("only-here"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(
            bin.join("only-here"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let env = [("PATH".to_string(), bin.to_string_lossy().into_owned())];
        assert!(check_wrapped(&wrap, Path::new("only-here"), &env, dir.path(), "x").is_ok());
        let e = check_wrapped(&wrap, Path::new("only-here"), &[], dir.path(), "x").unwrap_err();
        assert_eq!(e.to_string(), "`only-here` not found on PATH (x)");
    }

    /// A runtime's command under a wrapper is the wrapper's, the runtime's program last.
    #[test]
    fn a_wrapped_command_runs_the_wrapper_first() {
        let wrap = ["nice".to_string(), "-n".to_string(), "7".to_string()];
        let cmd = wrapped(&wrap, "/usr/bin/agent");
        let std = cmd.as_std();
        assert_eq!(std.get_program(), "nice");
        let args: Vec<_> = std.get_args().collect();
        assert_eq!(args, ["-n", "7", "/usr/bin/agent"]);
        let bare = wrapped(&[], "/usr/bin/agent");
        assert_eq!(bare.as_std().get_program(), "/usr/bin/agent");
        assert_eq!(bare.as_std().get_args().count(), 0);
    }

    /// A program that is not there is named first (bare names looked up on
    /// PATH, paths checked from the program's directory).
    #[test]
    fn missing_programs_are_named() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let agent = bin.join("agent");
        std::fs::write(&agent, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(bin.join("plain"), "").unwrap();
        let path = std::env::join_paths(["/nonexistent", bin.to_str().unwrap()]).unwrap();
        let path = Some(path.as_os_str());
        let cwd = dir.path();
        let missing = |p: &str, path| missing_program(Path::new(p), path, cwd);
        assert_eq!(missing("agent", path), None);
        assert_eq!(missing("bin/agent", path), None);
        assert_eq!(missing("./bin/agent", None), None);
        assert_eq!(missing(agent.to_str().unwrap(), None), None);
        assert_eq!(missing("", path), None);
        assert_eq!(
            missing("no-such-agent", path).as_deref(),
            Some("`no-such-agent` not found on PATH")
        );
        assert!(missing("agent", None).is_some());
        // Not executable, or a directory: not runnable either.
        assert!(missing("plain", path).is_some());
        let dirs = Some(dir.path().as_os_str());
        assert!(missing_program(Path::new("bin"), dirs, cwd).is_some());
        assert_eq!(
            missing("./missing/agent", path).as_deref(),
            Some("`./missing/agent` not found")
        );
        assert_eq!(
            missing("bin/plain", path).as_deref(),
            Some("`bin/plain` is not an executable file")
        );
        // The file a program names.
        assert_eq!(
            find_program(Path::new("agent"), path, cwd),
            Some(agent.clone())
        );
        assert_eq!(
            find_program(Path::new("bin/agent"), None, cwd),
            Some(cwd.join("bin/agent"))
        );
        assert_eq!(find_program(Path::new("plain"), path, cwd), None);
        assert_eq!(find_program(Path::new("agent"), None, cwd), None);
        assert_eq!(find_program(Path::new(""), path, cwd), None);
    }

    /// The spawn error of a missing program says what to do; others keep the
    /// program, directory and cause.
    #[test]
    fn start_errors_lead_with_a_missing_program() {
        let dir = tempfile::tempdir().unwrap();
        let not_found = || io::Error::from(io::ErrorKind::NotFound);
        let fix = "install Claude Code, or set TWR_CLAUDE_BIN";
        let e = start_error(
            not_found(),
            Path::new("twr-test-no-such-cli"),
            &[],
            dir.path(),
            fix,
        );
        assert_eq!(
            format!("{e:#}"),
            "`twr-test-no-such-cli` not found on PATH (install Claude Code, or set \
             TWR_CLAUDE_BIN)"
        );
        let e = start_error(
            not_found(),
            Path::new("/nonexistent/claude"),
            &[],
            dir.path(),
            fix,
        );
        assert!(format!("{e:#}").starts_with("`/nonexistent/claude` not found ("));
        // A missing working directory is not a missing program.
        let gone = dir.path().join("gone");
        let e = start_error(
            not_found(),
            Path::new("twr-test-no-such-cli"),
            &[],
            &gone,
            fix,
        );
        let e = format!("{e:#}");
        assert!(e.starts_with("starting twr-test-no-such-cli in "), "{e}");
        assert!(!e.contains(fix), "{e}");
        // Nor is a program that is there (a missing interpreter, say), or another error.
        let e = start_error(not_found(), Path::new("sh"), &[], dir.path(), fix);
        // `sh` on the runtime's own PATH (`[threads] env`) when that has none is missing.
        let empty = [("PATH".to_string(), dir.path().display().to_string())];
        let e2 = start_error(not_found(), Path::new("sh"), &empty, dir.path(), fix);
        assert_eq!(format!("{e2:#}"), format!("`sh` not found on PATH ({fix})"));
        assert!(format!("{e:#}").starts_with("starting sh in "));
        let denied = io::Error::from(io::ErrorKind::PermissionDenied);
        let e = start_error(
            denied,
            Path::new("twr-test-no-such-cli"),
            &[],
            dir.path(),
            fix,
        );
        assert!(format!("{e:#}").starts_with("starting twr-test-no-such-cli in "));
    }

    async fn all(input: &[u8], max: usize) -> Vec<Line> {
        let mut r = BoundedLines::new(input, max);
        let mut out = Vec::new();
        while let Some(l) = r.next_line().await.unwrap() {
            out.push(l);
        }
        out
    }

    fn text(s: &str) -> Line {
        Line::Text(s.into())
    }

    #[tokio::test]
    async fn lines_within_the_cap() {
        assert_eq!(
            all(b"a\nbb\r\n\nlast", 8).await,
            [text("a"), text("bb"), text(""), text("last")]
        );
        assert_eq!(all(b"", 8).await, []);
        // Invalid UTF-8 does not end the stream.
        assert_eq!(
            all(b"a\xffb\nok\n", 8).await,
            [text("a\u{fffd}b"), text("ok")]
        );
        // Exactly the cap is fine.
        assert_eq!(all(b"12345678\n", 8).await, [text("12345678")]);
    }

    #[tokio::test]
    async fn an_oversized_line_is_skipped_to_its_newline() {
        let mut input = b"{\"type\":\"result\"".to_vec();
        input.extend(std::iter::repeat_n(b'x', 10_000));
        input.extend(b"\nnext\n");
        let got = all(&input, 64).await;
        assert_eq!(got.len(), 2);
        match &got[0] {
            Line::Oversized { len, head } => {
                assert_eq!(*len, 10_016);
                assert!(head.starts_with("{\"type\":\"result\""), "{head}");
                assert!(head.len() <= OVERSIZED_HEAD);
                assert!(head_has(head, "type", "result"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(got[1], text("next"));
        // Unterminated at EOF: still reported, never buffered past the head.
        let got = all(&[b'y'; 5000], 16).await;
        assert!(matches!(&got[..], [Line::Oversized { len: 5000, .. }]));
    }

    #[tokio::test]
    async fn memory_stays_bounded_while_skipping() {
        // A reader fed in small pieces keeps at most the head of a huge line.
        let (mut w, r) = tokio::io::duplex(64);
        let mut lines = BoundedLines::new(r, 100);
        let (go, wait) = tokio::sync::oneshot::channel::<()>();
        let writer = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            for _ in 0..1000 {
                w.write_all(&[b'z'; 1000]).await.unwrap();
            }
            wait.await.unwrap();
            w.write_all(b"\nok\n").await.unwrap();
        });
        // Read (and cancel, as `select!` does) until the whole unterminated line went by.
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while lines.len < 1_000_000 {
                let r =
                    tokio::time::timeout(std::time::Duration::from_millis(5), lines.next_line())
                        .await;
                assert!(r.is_err(), "no line yet");
            }
        })
        .await
        .unwrap();
        assert!(
            lines.buf.capacity() <= OVERSIZED_HEAD,
            "{}",
            lines.buf.capacity()
        );
        go.send(()).unwrap();
        let first = lines.next_line().await.unwrap().unwrap();
        assert!(matches!(first, Line::Oversized { len: 1_000_000, .. }));
        assert_eq!(lines.next_line().await.unwrap(), Some(text("ok")));
        assert_eq!(lines.oversized_count(), 1);
        writer.await.unwrap();
    }

    #[test]
    fn response_ids() {
        assert_eq!(
            head_response_id(r#"{"jsonrpc":"2.0","id":7,"result":{"#),
            Some(7)
        );
        assert_eq!(
            head_response_id(r#"{"jsonrpc":"2.0","method":"x","id":7"#),
            None
        );
        assert_eq!(head_response_id(r#"{"id":"s","result":1"#), None);
    }

    /// Whether process `pid` still runs (a zombie does not).
    fn alive(pid: &str) -> bool {
        let out = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", pid])
            .output()
            .unwrap();
        let stat = String::from_utf8_lossy(&out.stdout);
        let stat = stat.trim();
        !stat.is_empty() && !stat.starts_with('Z')
    }

    /// A probe that exits successfully leaves nothing behind: what it started in the
    /// background (its streams redirected, so its output ends with the probe) is killed with
    /// its group before the probe is reaped.
    #[tokio::test]
    async fn a_successful_probe_leaves_no_background_process() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("pid");
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.args([
            "-c",
            &format!(
                "sleep 30 </dev/null >/dev/null 2>&1 & echo $! > '{}'; echo done",
                pidfile.display()
            ),
        ]);
        let out = run_bounded(cmd, std::time::Duration::from_secs(10), 1024, |_| false).await;
        assert!(
            matches!(&out, Bounded::Exited(Some(0), o) if o == b"done\n"),
            "{out:?}"
        );
        let pid = std::fs::read_to_string(&pidfile).unwrap();
        let pid = pid.trim();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while alive(pid) {
            assert!(
                std::time::Instant::now() < deadline,
                "the background sleeper {pid} outlived its probe"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Whether process `pid` is there at all (a zombie, not reaped yet, still is).
    fn present(pid: &str) -> bool {
        let out = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", pid])
            .output()
            .unwrap();
        !String::from_utf8_lossy(&out.stdout).trim().is_empty()
    }

    /// A probe stopped early (enough output, too much, too slow) is killed and reaped before
    /// `run_bounded` returns: a caller that `exec`s next inherits no zombie child.
    #[tokio::test]
    async fn a_probe_stopped_early_is_reaped_before_the_return() {
        let dir = tempfile::tempdir().unwrap();
        for (i, (body, enough, limit)) in [
            ("echo ready; exec sleep 30", true, 10_000),
            ("yes flood", false, 10_000),
            ("exec sleep 30", false, 300),
        ]
        .into_iter()
        .enumerate()
        {
            let pidfile = dir.path().join(format!("pid{i}"));
            let mut cmd = tokio::process::Command::new("/bin/sh");
            cmd.args(["-c", &format!("echo $$ > '{}'; {body}", pidfile.display())]);
            let out = run_bounded(
                cmd,
                std::time::Duration::from_millis(limit),
                1024,
                move |o| enough && o.ends_with(b"\n"),
            )
            .await;
            match i {
                0 => assert!(
                    matches!(&out, Bounded::Enough(o) if o == b"ready\n"),
                    "{out:?}"
                ),
                1 => assert!(matches!(out, Bounded::TooLarge), "{out:?}"),
                _ => assert!(matches!(out, Bounded::TimedOut), "{out:?}"),
            }
            let pid = std::fs::read_to_string(&pidfile).unwrap();
            let pid = pid.trim();
            assert!(!present(pid), "probe {pid} ({body}) was left unreaped");
        }
    }

    /// A process group leader (`sh`) that starts a sleeper in the background, writes its pid
    /// into `pidfile`, then runs `then`.
    fn leader_with_helper(pidfile: &Path, then: &str) -> tokio::process::Child {
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.args([
            "-c",
            &format!(
                "sleep 30 </dev/null >/dev/null 2>&1 & echo $! > '{}.tmp' && mv '{0}.tmp' '{0}'; \
                 {then}",
                pidfile.display()
            ),
        ])
        .stdin(std::process::Stdio::null())
        .process_group(0);
        cmd.spawn().unwrap()
    }

    /// [`exited_unreaped`] after something else (here) reaped child `pid` first: `waitid`
    /// then fails with `ECHILD`, as when another waiter of this process got to it.
    fn reaped_elsewhere(pid: u32) -> io::Result<()> {
        let mut status = 0;
        // SAFETY: waitpid(2) of this test's child, with a valid pointer to `status`.
        let r = unsafe { libc::waitpid(pid as libc::pid_t, &mut status, 0) };
        assert_eq!(r, pid as libc::pid_t, "{}", io::Error::last_os_error());
        exited_unreaped(pid)
    }

    /// A child reaped by something else before [`finish_group`] waits for it: its group (a
    /// helper it left running) is not killed, and the error says the ownership was lost; the
    /// same through [`run_bounded`], whose group guard then signals nothing either.
    #[tokio::test]
    async fn a_child_reaped_elsewhere_is_not_signalled() {
        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("finish");
        let mut child = leader_with_helper(&pidfile, "exit 0");
        let helper = written(&pidfile);
        let e = finish_group_with(&mut child, reaped_elsewhere)
            .await
            .unwrap_err();
        assert!(ownership_lost(&e), "{e}");
        drop(child);
        assert!(
            alive(&helper),
            "the group of a child reaped elsewhere was killed"
        );
        // SAFETY: kill(2) of the helper this test started.
        unsafe { libc::kill(helper.parse().unwrap(), libc::SIGKILL) };

        let pidfile = dir.path().join("bounded");
        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.args([
            "-c",
            &format!(
                "sleep 30 </dev/null >/dev/null 2>&1 & echo $! > '{}.tmp' && mv '{0}.tmp' '{0}'; \
                 exit 0",
                pidfile.display()
            ),
        ]);
        let out = run_bounded_with(
            cmd,
            std::time::Duration::from_secs(20),
            1 << 16,
            |_| false,
            reaped_elsewhere,
        )
        .await;
        assert!(matches!(out, Bounded::Failed), "{out:?}");
        let helper = written(&pidfile);
        assert!(
            alive(&helper),
            "run_bounded signalled a group it no longer owned"
        );
        // SAFETY: kill(2) of the helper this test started.
        unsafe { libc::kill(helper.parse().unwrap(), libc::SIGKILL) };
    }

    /// The pid written into `pidfile`, once it is.
    fn written(pidfile: &Path) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Ok(p) = std::fs::read_to_string(pidfile) {
                return p.trim().to_string();
            }
            assert!(std::time::Instant::now() < deadline, "no pid written");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// `waitid` failing with anything but `ECHILD` (injected): the leader is still this
    /// process's, so its group is killed, a leader still running included, and the reap
    /// returns at once with the status it reaped (killed). With `ECHILD` (the child is not
    /// this process's any more) nothing is killed: what the leader left runs on, and the
    /// error comes back at once, without waiting for a reap (the leader here still runs).
    #[tokio::test]
    async fn a_failed_waitid_kills_the_group_unless_the_child_is_gone() {
        use std::os::unix::process::ExitStatusExt;
        let dir = tempfile::tempdir().unwrap();
        let einval = |_| Err(io::Error::from_raw_os_error(libc::EINVAL));
        let pidfile = dir.path().join("running");
        let mut child = leader_with_helper(&pidfile, "exec sleep 30");
        let helper = written(&pidfile);
        let start = std::time::Instant::now();
        let status = finish_group_with(&mut child, einval).await.unwrap();
        assert_eq!(status.signal(), Some(libc::SIGKILL), "{status:?}");
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while alive(&helper) {
            assert!(
                std::time::Instant::now() < deadline,
                "the helper outlived its group"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let echild = |_| Err(io::Error::from_raw_os_error(libc::ECHILD));
        let pidfile = dir.path().join("gone");
        let mut child = leader_with_helper(&pidfile, "exec sleep 30");
        let helper = written(&pidfile);
        let start = std::time::Instant::now();
        let e = finish_group_with(&mut child, echild).await.unwrap_err();
        assert!(ownership_lost(&e), "{e}");
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
        assert!(
            child.try_wait().unwrap().is_none(),
            "the leader was waited for after ECHILD"
        );
        assert!(
            alive(&helper),
            "a group no longer known to be this process's was killed"
        );
        // SAFETY: kill(2) of the helper this test started.
        unsafe { libc::kill(helper.parse().unwrap(), libc::SIGKILL) };
        // (Still this test's child, in truth: ended and reaped here.)
        child.start_kill().unwrap();
        child.wait().await.unwrap();
    }

    /// A relative `PATH` directory is taken from the program's working directory, an empty one
    /// is that directory: as `execvp` there sees them, whatever this process's directory is.
    #[test]
    fn relative_path_directories_are_taken_from_the_programs_directory() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        for p in [bin.join("agent"), dir.path().join("here")] {
            std::fs::write(&p, "#!/bin/sh\n").unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_ne!(std::env::current_dir().unwrap(), dir.path());
        let find = |prog: &str, path: &str| {
            find_program(
                Path::new(prog),
                Some(std::ffi::OsStr::new(path)),
                dir.path(),
            )
        };
        assert_eq!(
            find("agent", "bin:/usr/bin"),
            Some(dir.path().join("bin/agent"))
        );
        assert_eq!(find("agent", "./bin"), Some(dir.path().join("./bin/agent")));
        assert_eq!(
            find("here", "/nonexistent::/usr/bin"),
            Some(dir.path().join("here"))
        );
        assert_eq!(find("here", "/nonexistent:/usr/bin"), None);
        assert_eq!(find("agent", "nope"), None);
    }

    #[test]
    fn scrubbing() {
        let mut cmd = tokio::process::Command::new("true");
        cmd.env("TWR_ORCH_TOKEN", "x").env("KEEP", "1");
        scrub(&mut cmd);
        let envs: Vec<_> = cmd.as_std().get_envs().collect();
        for k in SCRUBBED_ENV {
            assert!(envs.contains(&(std::ffi::OsStr::new(k), None)), "{k}");
        }
        assert!(envs.contains(&(
            std::ffi::OsStr::new("KEEP"),
            Some(std::ffi::OsStr::new("1"))
        )));
        assert!(is_scrubbed("TWR_RUNNER_TOKEN") && !is_scrubbed("TWR_THREAD"));
    }
}
