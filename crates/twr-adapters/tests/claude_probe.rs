//! Claude Code adapter: spawns while the installed `claude` is probed (`--help` /
//! `--version`, see `claude_code::cli`). No real `claude` runs here: a `sh` stub records each
//! run (`<dir>/calls`: `help` when its `--help` starts, `help-done` when it ends, `run <args>`
//! for a session) and appends what the adapter writes to a session's stdin to
//! `<dir>/stdin.jsonl`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::{timeout, Instant};
use twr_adapters::claude_code::ClaudeCodeAdapter;
use twr_core::adapter::{Adapter, AdapterEvent, AdapterHandle, SpawnConfig};

const WAIT: Duration = Duration::from_secs(20);

/// A `claude` stub in `dir` whose `--help` runs `help` (after recording `help`) and whose
/// session prints `system/init`, then copies its stdin to `<dir>/stdin.jsonl` until EOF.
fn stub(dir: &Path, help: &str) -> PathBuf {
    let d = dir.display();
    cli_script(
        dir,
        &format!(
            "--help) echo help >> {d}/calls; {help};;\n\
             --version) echo '2.1.287 (Claude Code)'; exit 0;;"
        ),
        &format!(
            "echo \"run $*\" >> {d}/calls\n\
             echo '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s\"}}'\n\
             cat >> {d}/stdin.jsonl"
        ),
    )
}

/// A `claude` stub in `dir` that runs `body`, except for the adapter's `--help` / `--version`
/// probe, answered by `probe`: the arms of a `case "$1" in`.
fn cli_script(dir: &Path, probe: &str, body: &str) -> PathBuf {
    let path = dir.join("claude");
    install_executable(
        &path,
        &format!("#!/bin/sh\ncase \"$1\" in\n{probe}\nesac\n{body}\n"),
    );
    path
}

/// Creates the executable `path` with `content` without this process ever holding it open for
/// writing (a fork holding it would make exec'ing it fail with ETXTBSY): `cp` writes it.
fn install_executable(path: &Path, content: &str) {
    use std::os::unix::fs::PermissionsExt;
    let src = path.with_extension("src");
    std::fs::write(&src, content).unwrap();
    let st = std::process::Command::new("cp")
        .arg(&src)
        .arg(path)
        .status()
        .unwrap();
    assert!(st.success(), "cp failed");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

type Rx = UnboundedReceiver<AdapterEvent>;

fn start_with(adapter: ClaudeCodeAdapter, prompt: Option<&str>) -> (AdapterHandle, Rx) {
    let mut h = adapter
        .stop_grace(Duration::from_millis(500))
        .spawn(SpawnConfig::default(), prompt.map(str::to_string))
        .unwrap();
    let rx = h.take_events().unwrap();
    (h, rx)
}

fn start(bin: &Path, prompt: Option<&str>) -> (AdapterHandle, Rx) {
    start_with(ClaudeCodeAdapter::with_bin(bin), prompt)
}

/// Reads events until one matches; returns everything read.
async fn until(rx: &mut Rx, pred: impl Fn(&AdapterEvent) -> bool) -> Vec<AdapterEvent> {
    let mut seen = Vec::new();
    timeout(WAIT, async {
        loop {
            let ev = rx.recv().await.expect("adapter channel closed");
            let done = pred(&ev);
            seen.push(ev);
            if done {
                return;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out; saw {seen:?}"));
    seen
}

fn is_kind(ev: &AdapterEvent, kind: &str) -> bool {
    matches!(ev, AdapterEvent::Emit { body, .. } if body.kind() == kind)
}

async fn exited(rx: &mut Rx) -> (Option<i32>, String) {
    match until(rx, |e| matches!(e, AdapterEvent::Exited { .. }))
        .await
        .pop()
    {
        Some(AdapterEvent::Exited { code, stderr_tail }) => (code, stderr_tail),
        _ => unreachable!(),
    }
}

fn calls(dir: &Path) -> Vec<String> {
    std::fs::read_to_string(dir.join("calls"))
        .unwrap_or_default()
        .lines()
        .map(str::to_string)
        .collect()
}

fn runs(dir: &Path) -> usize {
    calls(dir).iter().filter(|c| c.starts_with("run ")).count()
}

fn helps(dir: &Path) -> usize {
    calls(dir).iter().filter(|c| *c == "help").count()
}

/// The lines written to the sessions' stdin.
fn written_stdin(dir: &Path) -> Vec<Value> {
    std::fs::read_to_string(dir.join("stdin.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// The texts of the user messages written to the sessions' stdin.
fn prompts_written(dir: &Path) -> Vec<String> {
    written_stdin(dir)
        .iter()
        .filter(|v| v["type"] == "user")
        .map(|v| v["message"]["content"].as_str().unwrap().to_string())
        .collect()
}

/// Waits until the stub's `--help` has started.
async fn probing(dir: &Path) {
    timeout(WAIT, async {
        while helps(dir) == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the probe never started");
}

/// A stop while the binary is probed cancels the start: the session never runs, the prompt is
/// never written, the probe is killed, and the exit is a stop's, reported at once.
#[tokio::test]
async fn a_stop_during_the_probe_never_starts_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().display();
    let bin = stub(
        dir.path(),
        &format!("sleep 3; echo help-done >> {d}/calls; exit 0"),
    );
    let t0 = Instant::now();
    let (h, mut rx) = start(&bin, Some("hello"));
    probing(dir.path()).await;
    h.stop().unwrap();
    assert_eq!(exited(&mut rx).await, (Some(0), String::new()));
    assert!(
        t0.elapsed() < Duration::from_millis(2500),
        "{:?}",
        t0.elapsed()
    );
    // Past the end the probe would have had.
    tokio::time::sleep_until(t0 + Duration::from_millis(3800)).await;
    assert_eq!(calls(dir.path()), ["help"]);
    assert!(written_stdin(dir.path()).is_empty());
    assert!(rx.try_recv().is_err());
}

/// The hub dropping the handle while the binary is probed: the same.
#[tokio::test]
async fn a_dropped_handle_during_the_probe_never_starts_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path().display();
    let bin = stub(
        dir.path(),
        &format!("sleep 3; echo help-done >> {d}/calls; exit 0"),
    );
    let t0 = Instant::now();
    let (h, mut rx) = start(&bin, Some("hello"));
    probing(dir.path()).await;
    drop(h);
    assert_eq!(exited(&mut rx).await, (Some(0), String::new()));
    assert!(
        t0.elapsed() < Duration::from_millis(2500),
        "{:?}",
        t0.elapsed()
    );
    tokio::time::sleep_until(t0 + Duration::from_millis(3800)).await;
    assert_eq!(calls(dir.path()), ["help"]);
    assert!(written_stdin(dir.path()).is_empty());
}

/// A prompt sent while the binary is probed is written after the first one, once the
/// session starts.
#[tokio::test]
async fn a_prompt_sent_during_the_probe_is_delivered_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let bin = stub(dir.path(), "sleep 1; exit 0");
    let (h, mut rx) = start(&bin, Some("first"));
    probing(dir.path()).await;
    h.send("second", 2).unwrap();
    h.send("third", 3).unwrap();
    until(&mut rx, |e| is_kind(e, "thread.started")).await;
    assert_eq!(runs(dir.path()), 1);
    h.stop().unwrap();
    exited(&mut rx).await;
    assert_eq!(prompts_written(dir.path()), ["first", "second", "third"]);
}

/// A `--help` that does not answer in time is not cached: the next spawn probes again (and
/// both sessions run, passing every option).
#[tokio::test]
async fn a_timed_out_probe_is_not_cached() {
    let dir = tempfile::tempdir().unwrap();
    let bin = stub(dir.path(), "sleep 30");
    for n in 1..=2 {
        let adapter = ClaudeCodeAdapter::with_bin(&bin).probe_timeout(Duration::from_millis(300));
        let (h, mut rx) = start_with(adapter, None);
        until(&mut rx, |e| is_kind(e, "thread.started")).await;
        h.stop().unwrap();
        exited(&mut rx).await;
        assert_eq!(helps(dir.path()), n);
        assert_eq!(runs(dir.path()), n);
    }
    assert!(calls(dir.path())
        .iter()
        .filter(|c| c.starts_with("run "))
        .all(|c| c.contains("--permission-prompts host")));
}

/// A stop already queued when the driver starts (the binary is known, so the session starts
/// at once) keeps the first prompt from being written.
#[tokio::test]
async fn a_stop_queued_before_the_first_prompt_keeps_it_unwritten() {
    let dir = tempfile::tempdir().unwrap();
    let bin = stub(dir.path(), "exit 0");
    // Probed (and cached) by a first spawn.
    let (h, mut rx) = start(&bin, None);
    until(&mut rx, |e| is_kind(e, "thread.started")).await;
    h.stop().unwrap();
    exited(&mut rx).await;
    assert_eq!(helps(dir.path()), 1);

    // The test runtime runs one task at a time: the driver starts after the stop is queued.
    let (h, mut rx) = start(&bin, Some("hello"));
    h.send("second", 2).unwrap();
    h.stop().unwrap();
    exited(&mut rx).await;
    assert_eq!(helps(dir.path()), 1);
    assert_eq!(runs(dir.path()), 2);
    assert!(prompts_written(dir.path()).is_empty());
}
