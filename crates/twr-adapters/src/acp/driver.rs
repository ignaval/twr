//! The IO driver of an ACP agent process, shared by every ACP adapter: starts the process,
//! writes the translator's lines to its stdin (from a dedicated task, so a runtime that stops
//! reading cannot block `stop`), feeds it stdout lines (capped at the adapter's `max_line`),
//! carries out hub commands, and reports the exit with the last [`STDERR_TAIL`] bytes of
//! stderr.
//!
//! The process runs in its own process group, kill-on-drop, with twr's secrets removed from
//! its environment ([`crate::process::scrub`]); the rest of its group is killed whenever it
//! exits. `stop` cancels what runs, answers pending requests `cancelled`, closes stdin and,
//! after the grace period, kills the process group.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin};
use tokio::sync::mpsc;
use twr_core::adapter::{AdapterCmd, AdapterEnd, AdapterEvent};

use super::translate::{Dialect, Translated, Translator};
use crate::process::{self, BoundedLines, Line};

/// How long stdout is still read after the process exited.
const DRAIN: Duration = Duration::from_secs(1);

/// How long the stderr collector may keep reading after the process exited.
const STDERR_DRAIN: Duration = Duration::from_millis(500);

/// Bytes of stderr kept for `Exited::stderr_tail`.
pub const STDERR_TAIL: usize = 4096;

/// The absolute working directory for `cwd` (the current one when empty).
pub fn absolute_cwd(cwd: &Path) -> anyhow::Result<PathBuf> {
    if cwd.as_os_str().is_empty() {
        std::env::current_dir().context("resolving the working directory")
    } else {
        std::path::absolute(cwd).with_context(|| format!("resolving {}", cwd.display()))
    }
}

/// Starts `program args` (under `wrap`, [`process::wrapped`]) in `cwd` with `env` added
/// (twr's secrets removed), stdio piped, in its own process group, killed on drop. `fix`: what
/// to do when the program is not there ([`process::start_error`]).
pub fn launch(
    wrap: &[String],
    program: &Path,
    args: &[String],
    env: &[(String, String)],
    cwd: &Path,
    fix: &str,
) -> anyhow::Result<Child> {
    let mut cmd = process::wrapped(wrap, program);
    cmd.args(args)
        .envs(env.iter().map(|(k, v)| (k, v)))
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    process::scrub(&mut cmd);
    process::check_wrapped(wrap, program, env, cwd, fix)?;
    cmd.spawn()
        .map_err(|e| process::start_error(e, program, cwd, fix))
}

/// Last [`STDERR_TAIL`] bytes of a stream.
#[derive(Debug, Default)]
struct Tail(VecDeque<u8>);

impl Tail {
    fn push(&mut self, bytes: &[u8]) {
        self.0.extend(bytes);
        let excess = self.0.len().saturating_sub(STDERR_TAIL);
        self.0.drain(..excess);
    }

    fn text(&self) -> String {
        let (a, b) = self.0.as_slices();
        let mut v = Vec::with_capacity(self.0.len());
        v.extend_from_slice(a);
        v.extend_from_slice(b);
        String::from_utf8_lossy(&v).into_owned()
    }
}

async fn collect_tail(mut r: impl AsyncRead + Unpin, tail: Arc<Mutex<Tail>>) {
    let mut buf = [0u8; 4096];
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => tail
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(&buf[..n]),
        }
    }
}

/// Queue of lines for the stdin writer task; None once stdin is closed.
type Stdin = Option<mpsc::UnboundedSender<String>>;

async fn write_stdin(
    mut w: ChildStdin,
    mut lines: mpsc::UnboundedReceiver<String>,
    log: &'static str,
) {
    while let Some(line) = lines.recv().await {
        let mut bytes = line.into_bytes();
        bytes.push(b'\n');
        let res = async {
            w.write_all(&bytes).await?;
            w.flush().await
        }
        .await;
        if let Err(e) = res {
            tracing::warn!(error = %e, "{log}: writing to stdin failed");
            return;
        }
    }
}

fn queue_line(stdin: &mut Stdin, line: String, log: &'static str) {
    let Some(tx) = stdin.as_ref() else {
        tracing::warn!("{log}: stdin already closed; dropping a line");
        return;
    };
    if tx.send(line).is_err() {
        tracing::warn!("{log}: stdin writer is gone; dropping a line");
        *stdin = None;
    }
}

/// Sends `SIGKILL` to process group `pgid` with the `kill` command (no libc in this crate).
fn kill_group(pgid: u32, log: &'static str) {
    let status = std::process::Command::new("kill")
        .args(["-KILL", "--", &format!("-{pgid}")])
        .stderr(Stdio::null())
        .status();
    tracing::debug!(pgid, ?status, "{log}: killed process group");
}

/// Helper tasks of one runtime process.
struct Tasks {
    writer: Option<tokio::task::JoinHandle<()>>,
    stderr: Option<tokio::task::JoinHandle<()>>,
}

/// Translates one stdout line. An oversized line is dropped (warned about); the response to
/// the running prompt still ends the turn ([`Translator::oversized`]).
fn translate_line<D: Dialect>(tr: &mut Translator<D>, line: Line, log: &'static str) -> Translated {
    match line {
        Line::Text(l) => tr.translate(&l),
        Line::Oversized { len, head } => {
            tracing::debug!(len, head = %crate::claude_code::translate::one_line(&head, 200), "{log}: dropping an oversized stdout line");
            tr.oversized(&head, len)
        }
    }
}

/// Queues `out.writes` and emits `out.events`. A fatal translation is recorded in the stderr
/// tail (so `Exited` says why). Returns whether the session must stop: the hub no longer
/// listens, or the translation was fatal. Every path applying translator output (stdout
/// lines, commands, the start) goes through here, so none can leave a failed session running.
fn apply(
    stdin: &mut Stdin,
    end: &AdapterEnd,
    tail: &Mutex<Tail>,
    out: Translated,
    log: &'static str,
) -> bool {
    for w in out.writes {
        queue_line(stdin, w, log);
    }
    let mut stop = false;
    for e in out.events {
        let ev = AdapterEvent::Emit {
            body: e.body,
            raw: e.raw,
            turn: e.turn,
        };
        if end.events.send(ev).is_err() && !stop {
            tracing::warn!("{log}: hub dropped the event stream; stopping");
            stop = true;
        }
    }
    if let Some(why) = out.fatal {
        tracing::warn!(%why, "{log}: session cannot go on; stopping");
        tail.lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(format!("twr: {why}\n").as_bytes());
        stop = true;
    }
    stop
}

async fn sleep_until_opt(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(d) => tokio::time::sleep_until(d).await,
        None => std::future::pending().await,
    }
}

/// Cancels what runs, answers pending requests `cancelled` and closes stdin. Returns the kill
/// deadline, which starts now whether or not the runtime still reads its stdin.
fn begin_stop<D: Dialect>(
    tr: &mut Translator<D>,
    stdin: &mut Stdin,
    grace: Duration,
    log: &'static str,
) -> tokio::time::Instant {
    let deadline = tokio::time::Instant::now() + grace;
    for w in tr.stop().writes {
        queue_line(stdin, w, log);
    }
    *stdin = None;
    deadline
}

/// Drives `child` with `tr` until it exits, then reports the exit on `end`. `first` is the
/// first prompt and its turn.
pub async fn drive<D: Dialect>(
    mut child: Child,
    mut tr: Translator<D>,
    mut end: AdapterEnd,
    first: Option<(String, u64)>,
    grace: Duration,
    max_line: usize,
) {
    let log = tr.dialect().log_name();
    let pid = child.id();
    let (mut stdin, writer): (Stdin, _) = match child.stdin.take() {
        Some(w) => {
            let (tx, rx) = mpsc::unbounded_channel();
            (Some(tx), Some(tokio::spawn(write_stdin(w, rx, log))))
        }
        None => (None, None),
    };
    let stdout = child.stdout.take().expect("stdout is piped");
    let tail = Arc::new(Mutex::new(Tail::default()));
    let stderr_task = child
        .stderr
        .take()
        .map(|e| tokio::spawn(collect_tail(e, Arc::clone(&tail))));
    let mut lines = BoundedLines::new(stdout, max_line);
    let tasks = Tasks {
        writer,
        stderr: stderr_task,
    };

    let mut stop_deadline: Option<tokio::time::Instant> = None;
    let mut out = tr.start();
    if let Some((prompt, turn)) = first {
        let more = tr.send(&prompt, turn);
        out.events.extend(more.events);
        out.writes.extend(more.writes);
    }
    if apply(&mut stdin, &end, &tail, out, log) {
        stop_deadline = Some(begin_stop(&mut tr, &mut stdin, grace, log));
    }

    let mut stdout_open = true;
    loop {
        tokio::select! {
            line = lines.next_line(), if stdout_open => match line {
                Ok(Some(l)) => {
                    let out = translate_line(&mut tr, l, log);
                    if apply(&mut stdin, &end, &tail, out, log) && stop_deadline.is_none() {
                        stop_deadline = Some(begin_stop(&mut tr, &mut stdin, grace, log));
                    }
                }
                Ok(None) => stdout_open = false,
                Err(e) => {
                    tracing::warn!(error = %e, "{log}: reading stdout failed");
                    stdout_open = false;
                }
            },
            status = child.wait() => {
                if let Some(p) = pid {
                    kill_group(p, log);
                }
                if stdout_open {
                    let drain_until = tokio::time::Instant::now() + DRAIN;
                    while let Ok(Ok(Some(l))) =
                        tokio::time::timeout_at(drain_until, lines.next_line()).await
                    {
                        let out = translate_line(&mut tr, l, log);
                        apply(&mut stdin, &end, &tail, out, log);
                    }
                }
                return finish(status, pid, &end, &tail, tasks, log).await;
            }
            cmd = end.cmds.recv(), if stop_deadline.is_none() => match cmd {
                Some(AdapterCmd::Send { text, turn }) => {
                    // A send can fail the session (a mode check before the prompt).
                    if apply(&mut stdin, &end, &tail, tr.send(&text, turn), log) {
                        stop_deadline = Some(begin_stop(&mut tr, &mut stdin, grace, log));
                    }
                }
                Some(AdapterCmd::Respond { attn_id, answer }) => match tr.respond(&attn_id, &answer) {
                    Some(out) => {
                        if apply(&mut stdin, &end, &tail, out, log) {
                            stop_deadline = Some(begin_stop(&mut tr, &mut stdin, grace, log));
                        }
                    }
                    None => tracing::warn!(%attn_id, "{log}: ignoring respond for an attention that is not pending"),
                },
                Some(AdapterCmd::Stop) | None => {
                    stop_deadline = Some(begin_stop(&mut tr, &mut stdin, grace, log));
                }
                Some(AdapterCmd::Interrupt) => {
                    for line in tr.interrupt().writes { queue_line(&mut stdin, line, log); }
                },
            },
            _ = sleep_until_opt(stop_deadline) => {
                tracing::warn!(?pid, "{log}: runtime did not exit after stop; killing its process group");
                if let Some(p) = pid {
                    kill_group(p, log);
                }
                let status = child.wait().await;
                return finish(status, pid, &end, &tail, tasks, log).await;
            }
        }
    }
}

/// Reports the exit. The process group was already killed by the caller.
async fn finish(
    status: std::io::Result<std::process::ExitStatus>,
    pid: Option<u32>,
    end: &AdapterEnd,
    tail: &Arc<Mutex<Tail>>,
    tasks: Tasks,
    log: &'static str,
) {
    if let Some(w) = tasks.writer {
        w.abort();
    }
    if let Some(mut t) = tasks.stderr {
        if tokio::time::timeout(STDERR_DRAIN, &mut t).await.is_err() {
            t.abort();
        }
    }
    let code = match status {
        Ok(s) => s.code(),
        Err(e) => {
            tracing::warn!(error = %e, "{log}: waiting for the runtime failed");
            None
        }
    };
    let text = tail.lock().unwrap_or_else(|e| e.into_inner()).text();
    tracing::info!(?pid, ?code, "{log}: exited");
    end.exited(code, text);
}
