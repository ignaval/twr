//! PTY fallback adapter. Tier: pty. **DEGRADED.**
//!
//! Runs any interactive CLI under a pseudo-terminal (portable-pty) and guesses what it is
//! doing from its terminal output. This is the one place in twr that scrapes a terminal; use a
//! headless adapter whenever the runtime has one. Everything here is a heuristic
//! ([`session`] lists them) and every event carries `ext.pty.degraded = true`:
//!
//! | terminal                                           | twr event                                 |
//! |----------------------------------------------------|-------------------------------------------|
//! | process started                                    | `thread.started{session_id: pty-<ulid>}` + `ext.twr.pid`, `ext.pty.warning` |
//! | output (ANSI stripped, spinners / blank lines and the echo of typed text dropped, coalesced ~200 ms) | `agent.text{delta}` |
//! | idle ≥ `idle` with a question-like unfinished line or a menu | `agent.text{final}` + `attention.raised{question}` (`[y/N]`-style → options `y`/`n`) |
//! | out of a turn: no input prompt shown (consent prompt, menu, unknown line, no output) while a prompt waits | `attention.raised{question}` with `ext.pty.held`; the prompt waits |
//! | idle ≥ `idle` otherwise (own `>` prompt, or nothing) | `agent.text{final}` + `turn.ended{completed, ext.pty.heuristic}` |
//! | new output before a question is answered           | `attention.resolved{by: timeout}` (withdrawal) |
//! | process exit during a turn (not stopped by twr)    | `turn.ended` (`completed` on code 0, else `error`) |
//!
//! No tool calls, no thinking, no usage, no permissions: a CLI's own permission prompts show
//! up as questions at best. **The thread's permission mode (`SpawnConfig::mode`) is not
//! applied**: twr cannot pass or check a mode for an arbitrary CLI, so the program runs in
//! whatever mode its own command line and configuration give it (put the CLI's own mode flag
//! in the `pty:<command>`). This is part of what DEGRADED means here. There is no session to resume (a resume starts the command
//! afresh; `ext.pty.resume_unsupported`) and no `attach` command.
//!
//! **Command.** The runtime name is `pty`; the command to run comes from
//! [`PtyAdapter::with_command`], or from the `TWR_PTY_COMMAND` entry of `SpawnConfig::env`
//! ([`COMMAND_ENV`]): a JSON array of strings (argv), or any other string run with `sh -c`.
//! The hub sets it for threads made with `thread new --runtime 'pty:<command line>'`: the
//! command line is split into argv by [`split_command_line`] (shell-style quoting, no
//! expansion), recorded as `thread.created` `ext.twr.pty_command` (a JSON array) and passed
//! as that JSON array whenever the thread's runtime is spawned or resumed.
//! `TWR_PTY_IDLE_MS` in `SpawnConfig::env` overrides the idle time ([`IDLE_ENV`]). Both are
//! removed from the program's environment, and so are twr's secrets
//! ([`crate::process::SCRUBBED_ENV`], inherited or not); the rest of `SpawnConfig::env` is
//! passed, plus `TERM=dumb` and `NO_COLOR=1` unless given (fewer escape sequences to strip).
//!
//! **Process.** The program runs in `cwd` (which must exist) as a session leader of a new
//! 50x200 terminal (so pgid = pid). Its output is read by a tokio task from a non-blocking
//! duplicate of the terminal's master side (readiness-driven, no blocking reads), input is
//! written by an OS thread, and another OS thread waits for the exit. `send` types the prompt
//! only into the program's own input prompt with no turn or question open (otherwise it is
//! held behind a question; the input gate in [`session`]), `respond` types the answer (one
//! submit each); `stop` sends `SIGHUP` to the process group (what closing a terminal
//! does), and after [`STOP_GRACE`] `SIGKILL`. Whenever the program exits, the rest of its
//! process group is killed and the output is drained for up to 500 ms; then the reader task is
//! dropped and every master descriptor closed, so a leftover that escaped the group (e.g. a
//! `setsid` child still holding the terminal) gets a hangup instead of keeping twr reading
//! forever. `Exited::stderr_tail` is the last 4 KB of cleaned output (stderr and stdout share
//! the terminal).

pub mod screen;
pub mod session;

use std::ffi::OsString;
use std::io::{Read, Write};
use std::os::fd::{BorrowedFd, OwnedFd};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use serde_json::{json, Map};
use tokio::io::unix::AsyncFd;
use tokio::sync::{mpsc, oneshot};
use twr_core::adapter::{
    Adapter, AdapterCmd, AdapterEnd, AdapterHandle, AdapterInfo, SpawnConfig, Tier,
};
use twr_core::event::{EventBody, JsonMap, SessionPayload};

pub use self::session::{Core, Emitted, Out, Timing};

/// Adapter name (`source = adapter:pty`).
pub const NAME: &str = "pty";

/// `SpawnConfig::env` entry holding the command (JSON argv array, or a `sh -c` string).
pub const COMMAND_ENV: &str = "TWR_PTY_COMMAND";

/// `SpawnConfig::env` entry overriding the idle time, in milliseconds.
pub const IDLE_ENV: &str = "TWR_PTY_IDLE_MS";

/// How long `stop` waits after `SIGHUP` before killing the process group.
pub const STOP_GRACE: Duration = Duration::from_secs(3);

/// How long output is still read after the process exited.
const DRAIN: Duration = Duration::from_millis(500);

/// Warning carried by `thread.started` (`ext.pty.warning`).
pub const WARNING: &str = "DEGRADED: pty runtime. twr reads this program's terminal output and \
guesses turn ends and questions from idle time and prompt-like last lines. Text may be \
garbled, turns may end early or late, and the program's own permission prompts are only \
relayed if they look like questions.";

/// The PTY fallback adapter (DEGRADED).
#[derive(Debug, Clone)]
pub struct PtyAdapter {
    command: Option<Vec<String>>,
    timing: Timing,
    stop_grace: Duration,
}

impl Default for PtyAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl PtyAdapter {
    /// Takes the command from `SpawnConfig::env` ([`COMMAND_ENV`]).
    pub fn new() -> Self {
        PtyAdapter {
            command: None,
            timing: Timing::default(),
            stop_grace: STOP_GRACE,
        }
    }

    /// Always runs `argv`.
    pub fn with_command<S: Into<String>>(argv: impl IntoIterator<Item = S>) -> Self {
        PtyAdapter {
            command: Some(argv.into_iter().map(Into::into).collect()),
            ..Self::new()
        }
    }

    /// Changes the heuristics' timing.
    pub fn timing(mut self, timing: Timing) -> Self {
        self.timing = timing;
        self
    }

    /// Changes how long `stop` waits before killing the process group.
    pub fn stop_grace(mut self, grace: Duration) -> Self {
        self.stop_grace = grace;
        self
    }

    /// The argv to run for `cfg`.
    pub fn command_for(&self, cfg: &SpawnConfig) -> anyhow::Result<Vec<String>> {
        if let Some(c) = &self.command {
            if c.is_empty() {
                bail!("the pty command is empty");
            }
            return Ok(c.clone());
        }
        let Some(raw) = env_of(cfg, COMMAND_ENV) else {
            bail!(
                "the pty runtime needs a command: set {COMMAND_ENV} (a JSON argv array or a \
                 shell command), e.g. via `--runtime pty:<command>`"
            );
        };
        parse_command(raw)
    }
}

/// Prefix of a runtime spec that names the pty runtime with its command (`pty:<command>`).
pub const RUNTIME_PREFIX: &str = "pty:";

/// Splits a command line into argv the way a POSIX shell splits words, without running a
/// shell: whitespace separates words; `'...'` is literal; `"..."` is literal except that `\`
/// escapes `"`, `\`, `$` and `` ` ``; outside quotes `\` escapes any character; quoted parts
/// join the word around them (`''` is an empty word). Nothing is expanded: `$VAR`, `~`, globs,
/// `;`, `|` and redirections are plain text. For shell features run the shell itself, e.g.
/// `pty:sh -c 'aider --model "$MODEL"'`. Errors on an unterminated quote, a trailing `\` or an
/// empty command.
pub fn split_command_line(line: &str) -> anyhow::Result<Vec<String>> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            c if c.is_whitespace() => {
                if let Some(w) = word.take() {
                    words.push(w);
                }
            }
            '\\' => match chars.next() {
                Some(n) => word.get_or_insert_with(String::new).push(n),
                None => bail!("the command ends with a lone backslash"),
            },
            '\'' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(n) => w.push(n),
                        None => bail!("unterminated ' quote in the command"),
                    }
                }
            }
            '"' => {
                let w = word.get_or_insert_with(String::new);
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(n @ ('"' | '\\' | '$' | '`')) => w.push(n),
                            Some(n) => {
                                w.push('\\');
                                w.push(n);
                            }
                            None => bail!("unterminated \" quote in the command"),
                        },
                        Some(n) => w.push(n),
                        None => bail!("unterminated \" quote in the command"),
                    }
                }
            }
            c => word.get_or_insert_with(String::new).push(c),
        }
    }
    if let Some(w) = word {
        words.push(w);
    }
    if words.first().is_none_or(|w| w.is_empty()) {
        bail!("the command names no program");
    }
    Ok(words)
}

/// A [`COMMAND_ENV`] value: a JSON array of strings, or a string run with `sh -c`.
pub fn parse_command(raw: &str) -> anyhow::Result<Vec<String>> {
    let t = raw.trim();
    if t.is_empty() {
        bail!("{COMMAND_ENV} is empty");
    }
    if t.starts_with('[') {
        let argv: Vec<String> =
            serde_json::from_str(t).with_context(|| format!("parsing {COMMAND_ENV} as JSON"))?;
        if argv.is_empty() || argv[0].is_empty() {
            bail!("{COMMAND_ENV} names no program");
        }
        return Ok(argv);
    }
    Ok(vec!["sh".into(), "-c".into(), t.into()])
}

fn env_of<'a>(cfg: &'a SpawnConfig, key: &str) -> Option<&'a str> {
    cfg.env
        .iter()
        .rev()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
}

/// Environment for the program: `cfg.env` without twr's `TWR_PTY_*` entries and twr's
/// secrets ([`crate::process::SCRUBBED_ENV`], also removed from what the program inherits),
/// plus `TERM=dumb` and `NO_COLOR=1` unless set there.
pub fn program_env(cfg: &SpawnConfig) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> = cfg
        .env
        .iter()
        .filter(|(k, _)| k != COMMAND_ENV && k != IDLE_ENV && !crate::process::is_scrubbed(k))
        .cloned()
        .collect();
    for (k, v) in [("TERM", "dumb"), ("NO_COLOR", "1")] {
        if !env.iter().any(|(ek, _)| ek == k) {
            env.push((k.into(), v.into()));
        }
    }
    env
}

impl Adapter for PtyAdapter {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: NAME,
            tier: Tier::Pty,
            description: "DEGRADED: any interactive CLI under a pseudo-terminal (heuristic text, turns and questions)",
        }
    }

    fn spawn(&self, cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let argv = self.command_for(&cfg)?;
        let mut timing = self.timing;
        if let Some(ms) = env_of(&cfg, IDLE_ENV) {
            let ms: u64 = ms
                .trim()
                .parse()
                .with_context(|| format!("parsing {IDLE_ENV}={ms:?}"))?;
            timing.idle = Duration::from_millis(ms.max(100));
        }
        let cwd = if cfg.cwd.as_os_str().is_empty() {
            std::env::current_dir().context("resolving the working directory")?
        } else {
            cfg.cwd.clone()
        };
        // portable-pty silently falls back to $HOME for a missing directory.
        if !cwd.is_dir() {
            bail!("working directory {} does not exist", cwd.display());
        }
        let pair = native_pty_system()
            .openpty(PtySize {
                rows: 50,
                cols: 200,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("opening a pseudo-terminal")?;
        // Under `cfg.wrap` ([`SpawnConfig::wrap`]) when it has one.
        let mut cmd = CommandBuilder::from_argv(
            cfg.wrap
                .iter()
                .chain(argv.iter())
                .map(OsString::from)
                .collect(),
        );
        cmd.cwd(&cwd);
        for (k, v) in program_env(&cfg) {
            cmd.env(k, v);
        }
        crate::process::scrub_pty(&mut cmd);
        // portable-pty's own error for a missing program lists every PATH entry it tried.
        let path = std::env::var_os("PATH");
        if let Some(why) =
            crate::process::missing_program(std::path::Path::new(&argv[0]), path.as_deref(), &cwd)
        {
            bail!("{why} (install it, or fix the command after `pty:`)");
        }
        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("starting {:?} in {}", argv, cwd.display()))?;
        // Only the program may hold the terminal's slave side, or reads never see EOF.
        drop(pair.slave);
        let pid = child.process_id();
        let reader = AsyncFd::new(std::fs::File::from(
            nonblocking_dup(&*pair.master).context("pty reader")?,
        ))
        .context("pty reader")?;
        let writer = pair.master.take_writer().context("pty writer")?;

        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let reader = tokio::spawn(read_task(reader, out_tx));
        let (in_tx, in_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let closed = Arc::new(AtomicBool::new(false));
        let writer_closed = Arc::clone(&closed);
        std::thread::Builder::new()
            .name("twr-pty-write".into())
            .spawn(move || write_loop(writer, in_rx, &writer_closed))
            .context("starting the pty writer")?;
        let (exit_tx, exit_rx) = oneshot::channel();
        let mut child = child;
        std::thread::Builder::new()
            .name("twr-pty-wait".into())
            .spawn(move || {
                let _ = exit_tx.send(child.wait());
            })
            .context("starting the pty waiter")?;

        let session_id = format!("pty-{}", ulid::Ulid::generate());
        let mut pty = Map::new();
        pty.insert("command".into(), json!(argv));
        pty.insert("heuristic".into(), json!(true));
        pty.insert("idle_ms".into(), json!(timing.idle.as_millis() as u64));
        pty.insert("warning".into(), json!(WARNING));
        if let Some(old) = &cfg.resume_session {
            tracing::warn!(session = %old, "pty: sessions cannot be resumed; starting the command afresh");
            pty.insert("resume_unsupported".into(), json!(true));
            pty.insert("previous_session".into(), json!(old));
        }
        let mut ext: JsonMap = self::session::ext(pty).unwrap_or_default();
        let mut twr = Map::new();
        if let Some(p) = pid {
            twr.insert("pid".into(), json!(p));
        }
        twr.insert("exe".into(), json!(argv[0]));
        ext.insert("twr".into(), serde_json::Value::Object(twr));
        let started = EventBody::ThreadStarted(SessionPayload {
            session_id: Some(session_id),
            ext: Some(ext),
            ..Default::default()
        });
        tracing::info!(thread = %cfg.thread, ?pid, ?argv, "pty: started (DEGRADED)");

        let (handle, end) = AdapterHandle::pair();
        if let Some(p) = pid {
            end.spawned(p);
        }
        end.emit(started, None);
        let first = prompt.map(|p| (p, cfg.prompt_turn.unwrap_or(1)));
        let io = Io {
            out_rx,
            reader,
            in_tx: Some(in_tx),
            closed,
            exit_rx,
            pid,
            _master: pair.master,
        };
        tokio::spawn(drive(
            io,
            Core::new(timing, Instant::now()),
            end,
            first,
            self.stop_grace,
        ));
        Ok(handle)
    }
}

// ---------------------------------------------------------------------------------------------
// IO driver
// ---------------------------------------------------------------------------------------------

/// A non-blocking duplicate of the terminal's master descriptor.
///
/// `O_NONBLOCK` is set with `ioctl(FIONBIO)` through `UnixStream::set_nonblocking` (std has no
/// `fcntl`, and this crate uses no libc); the ioctl works on any descriptor, not only
/// sockets. The flag belongs to the open file, so the writer's descriptor is non-blocking too
/// ([`write_loop`] retries).
fn nonblocking_dup(master: &(dyn portable_pty::MasterPty + Send)) -> anyhow::Result<OwnedFd> {
    let raw = master
        .as_raw_fd()
        .context("the pty master has no descriptor")?;
    // SAFETY: `raw` is the master's own descriptor, open for as long as `master` is borrowed
    // here; it is only duplicated, never closed or kept.
    let fd = unsafe { BorrowedFd::borrow_raw(raw) }
        .try_clone_to_owned()
        .context("duplicating the pty master")?;
    let s = std::os::unix::net::UnixStream::from(fd);
    s.set_nonblocking(true)
        .context("making the pty master non-blocking")?;
    Ok(OwnedFd::from(s))
}

/// Reads the terminal until EOF / `EIO` (everything holding the slave side is gone). The
/// driver aborts it after the exit drain, which closes its descriptor even while a leftover
/// process still holds the slave side.
async fn read_task(fd: AsyncFd<std::fs::File>, tx: mpsc::UnboundedSender<Vec<u8>>) {
    let mut buf = vec![0u8; 8192];
    loop {
        let Ok(mut ready) = fd.readable().await else {
            return;
        };
        match ready.try_io(|f| f.get_ref().read(&mut buf)) {
            // EIO once the program and everything holding the terminal is gone.
            Ok(Ok(0) | Err(_)) => return,
            Ok(Ok(n)) => {
                if tx.send(buf[..n].to_vec()).is_err() {
                    return;
                }
            }
            Err(_would_block) => {}
        }
    }
}

/// How long the writer waits before retrying a write the terminal did not take.
const WRITE_RETRY: Duration = Duration::from_millis(10);

/// Writes queued input. The descriptor is non-blocking (see [`nonblocking_dup`]): a full
/// input queue is retried every [`WRITE_RETRY`] until the driver sets `closed`.
fn write_loop(
    mut w: Box<dyn Write + Send>,
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    closed: &AtomicBool,
) {
    for bytes in rx {
        let mut rest = &bytes[..];
        while !rest.is_empty() {
            match w.write(rest) {
                Ok(0) => {
                    tracing::warn!("pty: the terminal takes no more input");
                    return;
                }
                Ok(n) => rest = &rest[n..],
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if closed.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(WRITE_RETRY);
                }
                Err(e) => {
                    tracing::warn!(error = %e, "pty: writing to the terminal failed");
                    return;
                }
            }
        }
    }
}

struct Io {
    out_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    /// The reader task; aborted (closing its descriptor) once the exit is drained.
    reader: tokio::task::JoinHandle<()>,
    in_tx: Option<std::sync::mpsc::Sender<Vec<u8>>>,
    /// Tells the writer thread to give up on input the terminal does not take.
    closed: Arc<AtomicBool>,
    exit_rx: oneshot::Receiver<std::io::Result<portable_pty::ExitStatus>>,
    pid: Option<u32>,
    /// Kept open for the session (the reader and writer use their own descriptors); closed
    /// with them when the driver ends.
    _master: Box<dyn portable_pty::MasterPty + Send>,
}

/// Sends `signal` to process group `pgid` with the `kill` command.
fn signal_group(pgid: u32, signal: &str) {
    let status = std::process::Command::new("kill")
        .args([&format!("-{signal}"), "--", &format!("-{pgid}")])
        .stderr(Stdio::null())
        .status();
    tracing::debug!(pgid, signal, ?status, "pty: signalled process group");
}

/// Writes `out.writes` and emits `out.events`. Returns true if the hub no longer listens.
fn apply(io: &mut Io, end: &AdapterEnd, out: Out) -> bool {
    for w in out.writes {
        match &io.in_tx {
            Some(tx) => {
                if tx.send(w).is_err() {
                    tracing::warn!("pty: writer is gone; dropping input");
                    io.in_tx = None;
                }
            }
            None => tracing::warn!("pty: input closed; dropping input"),
        }
    }
    let mut gone = false;
    for e in out.events {
        gone |= !end.emit(e.body, e.turn);
    }
    gone
}

async fn sleep_until_opt(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

fn begin_stop(io: &mut Io, core: &mut Core, grace: Duration) -> tokio::time::Instant {
    core.stop();
    if let Some(p) = io.pid {
        signal_group(p, "HUP");
    }
    tokio::time::Instant::now() + grace
}

fn exit_code(status: &std::io::Result<portable_pty::ExitStatus>) -> Option<i32> {
    match status {
        Ok(s) if s.signal().is_none() => Some(s.exit_code() as i32),
        Ok(_) => None,
        Err(e) => {
            tracing::warn!(error = %e, "pty: waiting for the program failed");
            None
        }
    }
}

/// Output chunks already read that are handled before an answer is typed.
const RESPOND_DRAIN_CHUNKS: usize = 256;

async fn drive(
    mut io: Io,
    mut core: Core,
    mut end: AdapterEnd,
    first: Option<(String, u64)>,
    grace: Duration,
) {
    let mut stop_deadline: Option<tokio::time::Instant> = None;
    if let Some((text, turn)) = first {
        let out = core.send(&text, turn, Instant::now());
        if apply(&mut io, &end, out) {
            stop_deadline = Some(begin_stop(&mut io, &mut core, grace));
        }
    }
    let mut reader_open = true;
    loop {
        let tick_at = core.next_deadline().map(tokio::time::Instant::from_std);
        let mut hub_gone = false;
        tokio::select! {
            chunk = io.out_rx.recv(), if reader_open => match chunk {
                Some(bytes) => hub_gone = apply(&mut io, &end, core.on_output(&bytes, Instant::now())),
                None => reader_open = false,
            },
            status = &mut io.exit_rx => {
                let status = status.unwrap_or_else(|_| Err(std::io::Error::other("waiter gone")));
                return finish(io, core, &end, exit_code(&status)).await;
            }
            cmd = end.cmds.recv(), if stop_deadline.is_none() => match cmd {
                Some(AdapterCmd::Send { text, turn }) => {
                    hub_gone = apply(&mut io, &end, core.send(&text, turn, Instant::now()));
                }
                Some(AdapterCmd::Respond { attn_id, answer }) => {
                    // Output already read goes first: a question the program has moved on from
                    // is withdrawn before an answer meant for it could be typed into what the
                    // program shows now (bounded, so a flood cannot hold the answer forever).
                    for _ in 0..RESPOND_DRAIN_CHUNKS {
                        let Ok(bytes) = io.out_rx.try_recv() else { break };
                        hub_gone |= apply(&mut io, &end, core.on_output(&bytes, Instant::now()));
                    }
                    match core.respond(&attn_id, &answer, Instant::now()) {
                        Some(out) => hub_gone |= apply(&mut io, &end, out),
                        None => tracing::warn!(%attn_id, "pty: ignoring respond for a question that is not open"),
                    }
                }
                Some(AdapterCmd::Interrupt) => tracing::warn!("pty: turn interruption is unsupported; session retained"),
                Some(AdapterCmd::Stop) | None => {
                    stop_deadline = Some(begin_stop(&mut io, &mut core, grace));
                }
            },
            _ = sleep_until_opt(tick_at) => {
                hub_gone = apply(&mut io, &end, core.tick(Instant::now()));
            }
            _ = sleep_until_opt(stop_deadline) => {
                tracing::warn!(pid = ?io.pid, "pty: program did not exit after SIGHUP; killing its process group");
                if let Some(p) = io.pid {
                    signal_group(p, "KILL");
                }
                // Stop waiting at the next deadline if the kill does not take (no busy loop).
                stop_deadline = Some(tokio::time::Instant::now() + grace);
            }
        }
        if hub_gone && stop_deadline.is_none() {
            tracing::warn!("pty: hub dropped the event stream; stopping");
            stop_deadline = Some(begin_stop(&mut io, &mut core, grace));
        }
    }
}

/// Kills leftovers, drains the output, ends an open turn and reports the exit.
async fn finish(mut io: Io, mut core: Core, end: &AdapterEnd, code: Option<i32>) {
    if let Some(p) = io.pid {
        signal_group(p, "KILL");
    }
    let drain_until = tokio::time::Instant::now() + DRAIN;
    while let Ok(Some(bytes)) = tokio::time::timeout_at(drain_until, io.out_rx.recv()).await {
        let out = core.on_output(&bytes, Instant::now());
        apply(&mut io, end, out);
    }
    let out = core.on_exit(code);
    apply(&mut io, end, out);
    // Close every master descriptor: a leftover outside the process group (e.g. a `setsid`
    // child) that still holds the slave side gets a hangup, and nothing of twr keeps reading.
    io.reader.abort();
    io.in_tx = None;
    io.closed.store(true, Ordering::Relaxed);
    tracing::info!(pid = ?io.pid, ?code, "pty: exited");
    let recent = core.recent_output().to_string();
    drop(io);
    end.exited(code, recent);
}
