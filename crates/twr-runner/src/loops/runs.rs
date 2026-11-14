//! A runner's review runs ([`twr_core::runner::Op::LoopRun`]): each one review run of a loop of
//! the hub's ([`super::review::run`], or an ACP agent's), run in the background on the box with
//! the box's own reviewer CLI and login, in the thread's working directory there, its files in
//! the box's copy of the loop's folder ([`super::folder`]). The hub starts one, waits for it in
//! bounded slices (`Op::LoopRunWait`), reads its answer and log back, and can end it
//! (`Op::LoopRunCancel`).
//!
//! - **Never twice.** A run's id is the hub's: a `loop_run` for an id the box has is answered
//!   with that run's state, never started again. A run of the same loop and stem (`round-m3`,
//!   `audit-s2`) still running when another starts is ended first: a retry never runs beside
//!   the run it replaces.
//! - **Never left running.** A run belongs to the connection to the hub that started it: when
//!   that connection ends (the box went away, the hub stopped or died), its runs end with it
//!   ([`BoxRuns::connection_closed`]), and a `loop_run` that arrives on a connection already over
//!   starts nothing. The hub counts such a run as failed and retries it once the box is back,
//!   so a run is never adopted across a connection, and never counted twice. Each run's process
//!   group, its start time and its `TWR_AGENT` marker are recorded in
//!   `<loops>/.runs/<run>.json` while it runs: a runner that died with runs going (killed, the
//!   machine rebooted) leaves them, and the next one kills what is left of them when it opens
//!   the box ([`reap_left`]).
//! - **Kept a while.** An ended run's state is kept for [`KEEP_ENDED`] (at most [`MAX_ENDED`]
//!   of them), long enough for the hub to read it after a slice of waiting.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use twr_core::loops::contract::{FINDINGS_SCHEMA, MAX_ANSWER_BYTES};
use twr_core::runner::{
    cmd_errors, valid_loop_file, valid_loop_id, valid_run_id, CmdError, LoopRunEnd, LoopRunSpec,
    LoopRunStatus, LOOP_WAIT_MAX_MS,
};

use super::review::{self, Call, Ended};

/// How long an ended run's state is kept for the hub to read.
pub const KEEP_ENDED: Duration = Duration::from_secs(3600);

/// The most ended runs kept.
pub const MAX_ENDED: usize = 256;

/// The most bytes of a run's prompt read from its file.
pub const PROMPT_MAX: u64 = 4 << 20;

/// How long a cancel waits for the run to be over before it answers.
const CANCEL_WAIT: Duration = Duration::from_secs(10);

/// The code of a run refused because the thread's working directory is gone on the box: the
/// loop can't go on (as for a worktree gone on the hub's own box).
pub const GONE: &str = "gone";

tokio::task_local! {
    /// The connection to the hub a command came on ([`BoxRuns::connection_opened`], from 1),
    /// while the runner runs it; 0 or unset: none (a direct call).
    pub static CONNECTION: u64;
}

/// What a run records while it runs, for a later runner to reap ([`reap_left`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Left {
    pgid: u32,
    start_time: Option<u64>,
    marker: String,
}

/// How the box finds a reviewer's program: for runtime `runtime`, the program to run (`None`:
/// not a built-in one) and the `PATH` it runs with.
pub type Resolver<'a> = &'a (dyn Fn(&str) -> (Option<PathBuf>, Option<OsString>) + Send + Sync);

/// The box's review runs (module docs). Cheap to clone.
#[derive(Clone)]
pub struct BoxRuns {
    inner: Arc<Inner>,
}

struct Inner {
    loops: PathBuf,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// The last connection numbered.
    last_conn: u64,
    /// The connections still open.
    open: std::collections::HashSet<u64>,
    runs: HashMap<String, Entry>,
}

struct Entry {
    loop_id: String,
    stem: String,
    /// The connection that started it (`None`: none, a test's direct call).
    conn: Option<u64>,
    end: watch::Receiver<Option<LoopRunEnd>>,
    cancel: watch::Sender<Option<String>>,
    ended_at: Option<Instant>,
}

impl Entry {
    fn status(&self, run: &str) -> LoopRunStatus {
        let ended = self.end.borrow().clone();
        LoopRunStatus {
            run: run.to_string(),
            unknown: false,
            running: ended.is_none(),
            ended,
        }
    }
}

fn unknown(run: &str) -> LoopRunStatus {
    LoopRunStatus {
        run: run.to_string(),
        unknown: true,
        ..Default::default()
    }
}

fn failed(msg: impl Into<String>) -> CmdError {
    CmdError::new(cmd_errors::FAILED, msg)
}

impl BoxRuns {
    /// The runs of a box that keeps loop folders under `loops`.
    pub fn new(loops: PathBuf) -> BoxRuns {
        BoxRuns {
            inner: Arc::new(Inner {
                loops,
                state: Mutex::new(State::default()),
            }),
        }
    }

    /// Where loop folders are kept.
    pub fn loops(&self) -> &Path {
        &self.inner.loops
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A new connection to the hub: its number, for [`CONNECTION`].
    pub fn connection_opened(&self) -> u64 {
        let mut st = self.state();
        st.last_conn += 1;
        let n = st.last_conn;
        st.open.insert(n);
        n
    }

    /// Connection `conn` is over: its runs end (`why`), and a command of it that arrives
    /// later starts none.
    pub fn connection_closed(&self, conn: u64, why: &str) {
        let mut st = self.state();
        st.open.remove(&conn);
        for e in st.runs.values() {
            if e.conn == Some(conn) && e.ended_at.is_none() {
                let _ = e.cancel.send(Some(why.to_string()));
            }
        }
    }

    /// Ends every run (the runner stopping).
    pub fn end_all(&self, why: &str) {
        let st = self.state();
        for e in st.runs.values() {
            if e.ended_at.is_none() {
                let _ = e.cancel.send(Some(why.to_string()));
            }
        }
    }

    /// How many runs run now.
    pub fn running(&self) -> usize {
        self.state()
            .runs
            .values()
            .filter(|e| e.ended_at.is_none())
            .count()
    }

    /// Forgets ended runs past [`KEEP_ENDED`], and the oldest past [`MAX_ENDED`].
    fn gc(st: &mut State) {
        st.runs
            .retain(|_, e| e.ended_at.is_none_or(|t| t.elapsed() < KEEP_ENDED));
        let mut ended: Vec<(Instant, String)> = st
            .runs
            .iter()
            .filter_map(|(r, e)| e.ended_at.map(|t| (t, r.clone())))
            .collect();
        if ended.len() > MAX_ENDED {
            ended.sort();
            for (_, r) in ended.iter().take(ended.len() - MAX_ENDED) {
                st.runs.remove(r);
            }
        }
    }

    /// `Op::LoopRun`: starts run `run` of loop `loop_id` (module docs), or reports the one of
    /// that id; `resolve` finds the reviewer's program on the box.
    pub fn start(
        &self,
        loop_id: &str,
        run: &str,
        spec: LoopRunSpec,
        resolve: Resolver<'_>,
    ) -> Result<LoopRunStatus, CmdError> {
        if !valid_loop_id(loop_id) {
            return Err(failed(format!("{loop_id:?} is no loop id")));
        }
        if !valid_run_id(run) {
            return Err(failed(format!("{run:?} is no run id")));
        }
        let names = ["prompt.md", "answer.json", "log"].map(|x| format!("{}.{x}", spec.stem));
        if spec.stem.is_empty() || names.iter().any(|n| !valid_loop_file(n)) {
            return Err(failed(format!("{:?} is no run stem", spec.stem)));
        }
        let plain = |s: &str| {
            !s.is_empty()
                && s.len() <= 160
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'-' | b'_'))
        };
        if !plain(&spec.marker) {
            return Err(failed("the run's marker is no plain name"));
        }
        let conn = CONNECTION.try_with(|c| *c).ok().filter(|c| *c != 0);
        {
            let mut st = self.state();
            Self::gc(&mut st);
            if let Some(e) = st.runs.get(run) {
                return Ok(e.status(run));
            }
            if conn.is_some_and(|c| !st.open.contains(&c)) {
                return Err(failed(
                    "the connection to the hub that asked for this run is over",
                ));
            }
        }
        if !spec.cwd.is_dir() {
            return Err(CmdError::new(
                GONE,
                format!("{} is not there any more", spec.cwd.display()),
            ));
        }
        let folder = super::folder::make(&self.inner.loops, loop_id)
            .map_err(|e| failed(format!("{e:#}")))?;
        let prompt_file = folder.join(&names[0]);
        let prompt = read_bounded(&prompt_file, PROMPT_MAX)
            .map_err(|_| failed(format!("the run's prompt is not in {}", folder.display())))?;
        let schema_file = folder.join("findings.schema.json");
        std::fs::write(&schema_file, FINDINGS_SCHEMA)
            .map_err(|e| failed(format!("cannot write {}: {e}", schema_file.display())))?;
        let (bin, path) = resolve(&spec.runtime);
        let acp = spec.acp.clone().map(|a| {
            Ok(super::acp_review::AcpReviewer {
                preset: a.preset,
                argv: a.argv,
                mode: a.mode,
                efforts: a.efforts,
            })
        });
        if acp.is_none() && bin.is_none() {
            return Err(failed(format!(
                "{} can't review on this box: it is no built-in reviewer, and no ACP agent was \
                 given",
                spec.runtime
            )));
        }
        let call = Call {
            runtime: spec.runtime.clone(),
            bin: bin.unwrap_or_default(),
            model: spec.model.clone(),
            effort: spec.effort.clone(),
            cwd: spec.cwd.clone(),
            folder: folder.clone(),
            prompt,
            schema_file,
            answer_file: folder.join(&names[1]),
            log_file: folder.join(&names[2]),
            account_env: None,
            marker: spec.marker.clone(),
            timeout: Duration::from_secs(spec.timeout_secs.max(1)),
            acp,
            path,
        };
        let (end_tx, end_rx) = watch::channel(None);
        let (cancel_tx, cancel_rx) = watch::channel(None);
        {
            let mut st = self.state();
            // (Checked again under the lock it is inserted under.)
            if let Some(e) = st.runs.get(run) {
                return Ok(e.status(run));
            }
            if conn.is_some_and(|c| !st.open.contains(&c)) {
                return Err(failed(
                    "the connection to the hub that asked for this run is over",
                ));
            }
            for e in st.runs.values() {
                if e.loop_id == loop_id && e.stem == spec.stem && e.ended_at.is_none() {
                    let _ = e
                        .cancel
                        .send(Some("a new run of the same step replaced it".into()));
                }
            }
            st.runs.insert(
                run.to_string(),
                Entry {
                    loop_id: loop_id.to_string(),
                    stem: spec.stem.clone(),
                    conn,
                    end: end_rx,
                    cancel: cancel_tx,
                    ended_at: None,
                },
            );
        }
        let this = self.clone();
        let (run_id, marker) = (run.to_string(), spec.marker.clone());
        let left_file = self.left_file(run);
        tokio::spawn(async move {
            let record = {
                let left_file = left_file.clone();
                move |pid: u32| {
                    let left = Left {
                        pgid: pid,
                        start_time: crate::reap::process_start_time(pid),
                        marker,
                    };
                    if let Some(f) = &left_file {
                        if let Some(dir) = f.parent() {
                            let _ = twr_core::paths::ensure_private_dir(dir);
                        }
                        if let Ok(b) = serde_json::to_vec(&left) {
                            let _ = std::fs::write(f, b);
                        }
                    }
                }
            };
            let answer_file = call.answer_file.clone();
            let ended = review::run(call, record, cancel_rx).await;
            if let Some(f) = &left_file {
                let _ = std::fs::remove_file(f);
            }
            let end = match ended {
                Ended::Answered { usage, .. } => {
                    match read_bounded(&answer_file, MAX_ANSWER_BYTES as u64 + 1) {
                        Ok(answer) => LoopRunEnd::Answered { answer, usage },
                        Err(e) => LoopRunEnd::Failed {
                            reason: format!("its answer file can't be read: {e}"),
                        },
                    }
                }
                Ended::Failed(reason) => LoopRunEnd::Failed { reason },
                Ended::Cancelled(reason) => LoopRunEnd::Cancelled { reason },
            };
            let mut st = this.state();
            if let Some(e) = st.runs.get_mut(&run_id) {
                e.ended_at = Some(Instant::now());
            }
            let _ = end_tx.send(Some(end));
        });
        Ok(LoopRunStatus {
            run: run.to_string(),
            unknown: false,
            running: true,
            ended: None,
        })
    }

    /// `<loops>/.runs/<run>.json`, where run `run` records its process group.
    fn left_file(&self, run: &str) -> Option<PathBuf> {
        valid_run_id(run).then(|| self.inner.loops.join(".runs").join(format!("{run}.json")))
    }

    /// `Op::LoopRunWait`: waits up to `wait_ms` (at most [`LOOP_WAIT_MAX_MS`]) for run `run` to
    /// end; how it stands then.
    pub async fn wait(&self, run: &str, wait_ms: u64) -> LoopRunStatus {
        let rx = { self.state().runs.get(run).map(|e| e.end.clone()) };
        let Some(mut rx) = rx else {
            return unknown(run);
        };
        let wait = Duration::from_millis(wait_ms.min(LOOP_WAIT_MAX_MS));
        let _ = tokio::time::timeout(wait, rx.wait_for(Option::is_some)).await;
        let end = rx.borrow().clone();
        LoopRunStatus {
            run: run.to_string(),
            unknown: false,
            running: end.is_none(),
            ended: end,
        }
    }

    /// `Op::LoopRunCancel`: ends run `run` with `why`, waiting a moment for it to be over.
    pub async fn cancel(&self, run: &str, why: &str) -> LoopRunStatus {
        {
            let st = self.state();
            match st.runs.get(run) {
                Some(e) => {
                    let _ = e.cancel.send(Some(why.to_string()));
                }
                None => return unknown(run),
            }
        }
        self.wait(run, CANCEL_WAIT.as_millis() as u64).await
    }

    /// `Op::LoopForget`: ends loop `loop_id`'s runs and removes its folder.
    pub async fn forget(&self, loop_id: &str) -> Result<(), CmdError> {
        let mine: Vec<watch::Receiver<Option<LoopRunEnd>>> = {
            let st = self.state();
            st.runs
                .values()
                .filter(|e| e.loop_id == loop_id)
                .map(|e| {
                    let _ = e.cancel.send(Some("the loop's folder was removed".into()));
                    e.end.clone()
                })
                .collect()
        };
        for mut rx in mine {
            let _ = tokio::time::timeout(CANCEL_WAIT, rx.wait_for(Option::is_some)).await;
        }
        self.state().runs.retain(|_, e| e.loop_id != loop_id);
        super::folder::forget(&self.inner.loops, loop_id).map_err(|e| failed(format!("{e:#}")))
    }
}

/// `path`'s text, at most `max` bytes.
fn read_bounded(path: &Path, max: u64) -> std::io::Result<String> {
    use std::io::Read;
    let f = std::fs::File::open(path)?;
    let mut text = String::new();
    f.take(max).read_to_string(&mut text)?;
    Ok(text)
}

/// Kills what is left of the runs a runner that died left behind (their records in
/// `<loops>/.runs`), before the box opens: each run's process group, when its start time
/// still matches, and every process that still carries one of their markers. Best effort;
/// every record read is removed.
pub fn reap_left(loops: &Path) {
    let dir = loops.join(".runs");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut markers = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        if let Some(left) = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Left>(&b).ok())
        {
            if let Some(start) = left.start_time {
                let reaped = crate::reap::reap_job_group(left.pgid, start, &[]);
                tracing::info!(
                    pgid = left.pgid,
                    ?reaped,
                    "what was left of a review run of a runner that died"
                );
            }
            if !left.marker.is_empty() {
                markers.push(left.marker);
            }
        }
        let _ = std::fs::remove_file(&path);
    }
    if !markers.is_empty() {
        crate::reap::reap_agent_marked(&markers);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A fake Codex: answers `answer` (its `-o` file), or with `hold` in its folder sleeps
    /// with a child until the file goes; writes its pid to `pid`.
    const FAKE: &str = r#"#!/bin/sh
here=$(dirname "$0")
echo $$ > "$here/pid"
cat > /dev/null
if [ -f "$here/hold" ]; then
  sleep 600 &
  echo $! > "$here/child"
  while [ -f "$here/hold" ]; do sleep 0.05; done
fi
out=""; prev=""
for a in "$@"; do [ "$prev" = "-o" ] && out="$a"; prev="$a"; done
cat "$here/answer" > "$out"
echo '{"type":"turn.completed","usage":{"input_tokens":3,"output_tokens":4}}'
"#;

    struct TestBox {
        dir: tempfile::TempDir,
        runs: BoxRuns,
    }

    impl TestBox {
        fn new() -> TestBox {
            let dir = tempfile::tempdir().unwrap();
            let bin = dir.path().join("codex");
            std::fs::write(&bin, FAKE).unwrap();
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::write(dir.path().join("answer"), r#"{"findings": []}"#).unwrap();
            std::fs::create_dir_all(dir.path().join("wt")).unwrap();
            let runs = BoxRuns::new(dir.path().join("loops"));
            TestBox { dir, runs }
        }

        fn spec(&self, stem: &str) -> LoopRunSpec {
            LoopRunSpec {
                runtime: "codex".into(),
                model: "m-1".into(),
                effort: "medium".into(),
                cwd: self.dir.path().join("wt"),
                stem: stem.into(),
                marker: "T1:loop-L1".into(),
                timeout_secs: 60,
                acp: None,
            }
        }

        fn prompt(&self, stem: &str) {
            super::super::folder::write_file(
                self.runs.loops(),
                "L1",
                &format!("{stem}.prompt.md"),
                b"review it",
            )
            .unwrap();
        }

        fn start(&self, run: &str, stem: &str) -> Result<LoopRunStatus, CmdError> {
            let bin = self.dir.path().join("codex");
            let resolve = move |_: &str| (Some(bin.clone()), None);
            self.runs.start("L1", run, self.spec(stem), &resolve)
        }

        fn hold(&self, on: bool) {
            let f = self.dir.path().join("hold");
            if on {
                std::fs::write(f, "").unwrap();
            } else {
                let _ = std::fs::remove_file(f);
            }
        }

        async fn pid(&self, name: &str) -> i32 {
            let f = self.dir.path().join(name);
            for _ in 0..500 {
                if let Some(p) = std::fs::read_to_string(&f)
                    .ok()
                    .and_then(|s| s.trim().parse().ok())
                {
                    return p;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("no {name}");
        }
    }

    fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 only checks that the process exists.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    async fn gone(pid: i32) {
        for _ in 0..500 {
            if !alive(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("{pid} still runs");
    }

    #[tokio::test]
    async fn a_run_answers_and_is_never_started_twice() {
        let b = TestBox::new();
        b.prompt("round-m1");
        std::fs::write(
            b.dir.path().join("answer"),
            r#"{"findings": [{"severity": "major", "location": "a:1", "problem": "p", "fix": "f"}]}"#,
        )
        .unwrap();
        let s = b.start("R1", "round-m1").unwrap();
        assert!(s.running && !s.unknown);
        let s = b.runs.wait("R1", 20_000).await;
        let Some(LoopRunEnd::Answered { answer, usage }) = s.ended else {
            panic!("{s:?}")
        };
        assert!(answer.contains("\"problem\": \"p\""), "{answer}");
        assert_eq!(usage.unwrap().input, 3);
        // The same id again: its state, not a second run.
        let pid = b.pid("pid").await;
        let again = b.start("R1", "round-m1").unwrap();
        assert!(!again.running && again.ended.is_some());
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(b.pid("pid").await, pid);
        // Its files are in the box's folder; nothing is left to reap.
        let folder = b.runs.loops().join("L1");
        assert!(folder.join("round-m1.answer.json").is_file());
        assert!(folder.join("round-m1.log").is_file());
        assert!(folder.join("findings.schema.json").is_file());
        assert_eq!(
            std::fs::read_dir(b.runs.loops().join(".runs"))
                .map(|d| d.count())
                .unwrap_or(0),
            0
        );
        // An id the box never had.
        assert!(b.runs.wait("R9", 10).await.unknown);
        assert!(b.runs.cancel("R9", "x").await.unknown);
    }

    #[tokio::test]
    async fn refusals_start_nothing() {
        let b = TestBox::new();
        // No prompt in the folder.
        assert!(b.start("R1", "round-m1").is_err());
        b.prompt("round-m1");
        // Bad ids, stems and markers.
        let bin = b.dir.path().join("codex");
        let resolve = move |_: &str| (Some(bin.clone()), None);
        for (l, r, stem, marker) in [
            ("../x", "R1", "round-m1", "m"),
            ("L1", "a b", "round-m1", "m"),
            ("L1", "R1", "../round", "m"),
            ("L1", "R1", "", "m"),
            ("L1", "R1", "round-m1", "a b"),
        ] {
            let mut spec = b.spec(stem);
            spec.marker = marker.into();
            assert!(
                b.runs.start(l, r, spec, &resolve).is_err(),
                "{l} {r} {stem}"
            );
        }
        // A working directory that is gone.
        let mut spec = b.spec("round-m1");
        spec.cwd = b.dir.path().join("gone");
        let e = b.runs.start("L1", "R1", spec, &resolve).unwrap_err();
        assert_eq!(e.code, GONE);
        // A runtime that is no built-in reviewer, and no ACP agent.
        let none = |_: &str| (None, None);
        let mut spec = b.spec("round-m1");
        spec.runtime = "other".into();
        assert!(b.runs.start("L1", "R1", spec, &none).is_err());
        assert_eq!(b.runs.running(), 0);
    }

    #[tokio::test]
    async fn a_run_ends_with_its_connection_and_a_late_command_starts_nothing() {
        let b = TestBox::new();
        b.prompt("round-m1");
        b.hold(true);
        let conn = b.runs.connection_opened();
        let s = CONNECTION
            .scope(conn, async { b.start("R1", "round-m1") })
            .await
            .unwrap();
        assert!(s.running);
        let (pid, child) = (b.pid("pid").await, b.pid("child").await);
        b.runs
            .connection_closed(conn, "the box's connection to the hub ended");
        let s = b.runs.wait("R1", 20_000).await;
        assert!(
            matches!(&s.ended, Some(LoopRunEnd::Cancelled { reason }) if reason.contains("connection")),
            "{s:?}"
        );
        gone(pid).await;
        gone(child).await;
        // A command of the closed connection that arrives late starts nothing.
        let e = CONNECTION
            .scope(conn, async { b.start("R2", "round-m1") })
            .await
            .unwrap_err();
        assert!(e.message.contains("connection"), "{e}");
        b.hold(false);
    }

    #[tokio::test]
    async fn a_retry_of_a_step_replaces_the_run_still_going_and_cancel_kills() {
        let b = TestBox::new();
        b.prompt("round-m1");
        b.hold(true);
        b.start("R1", "round-m1").unwrap();
        let child = b.pid("child").await;
        let _ = std::fs::remove_file(b.dir.path().join("child"));
        b.start("R2", "round-m1").unwrap();
        let s = b.runs.wait("R1", 20_000).await;
        assert!(
            matches!(&s.ended, Some(LoopRunEnd::Cancelled { reason }) if reason.contains("replaced")),
            "{s:?}"
        );
        gone(child).await;
        let child2 = b.pid("child").await;
        let s = b.runs.cancel("R2", "the loop stopped").await;
        assert!(
            matches!(&s.ended, Some(LoopRunEnd::Cancelled { reason }) if reason == "the loop stopped"),
            "{s:?}"
        );
        gone(child2).await;
        b.hold(false);
        // Forgetting the loop removes its folder.
        b.runs.forget("L1").await.unwrap();
        assert!(!b.runs.loops().join("L1").exists());
        assert!(b.runs.wait("R2", 10).await.unknown);
    }

    #[tokio::test]
    async fn a_dead_runners_runs_are_reaped_when_the_box_opens() {
        let b = TestBox::new();
        b.prompt("round-m1");
        b.hold(true);
        b.start("R1", "round-m1").unwrap();
        let (pid, child) = (b.pid("pid").await, b.pid("child").await);
        let left = b.runs.loops().join(".runs").join("R1.json");
        for _ in 0..500 {
            if left.is_file() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(left.is_file());
        // As if the runner died: its record stays, and the next one reaps what it names.
        let record = std::fs::read(&left).unwrap();
        reap_left(b.runs.loops());
        gone(pid).await;
        gone(child).await;
        assert!(!left.exists());
        // A record of a process long gone (or another's) is just removed.
        std::fs::write(&left, record).unwrap();
        reap_left(b.runs.loops());
        assert!(!left.exists());
        b.hold(false);
    }
}
