//! `twr runner` and an SSH box's daemon (`twr runner --daemon`) listen for SIGTERM and SIGINT
//! from the start: one that comes while they start (opening the box can take seconds) stops
//! them cleanly, exit 0, before they bind their socket or connect, instead of killing them
//! with the signal's default action. A debug build's start hold
//! (`TWR_TEST_RUNNER_START_HOLD`) writes its marker once the signals are listened for, as the
//! box starts opening, and holds the opening blocking the thread (as a slow opening does), so
//! the signal comes while nothing in the runner waits for it. The hold is inert in a release
//! build: these tests run in debug builds only.
#![cfg(debug_assertions)]

use std::net::TcpListener;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

const TWR: &str = env!("CARGO_BIN_EXE_twr");
const WAIT: Duration = Duration::from_secs(20);

/// `twr runner args` on a test home in `dir`, held as it starts, its stderr to `dir/stderr`.
fn runner(dir: &Path, args: &[&str]) -> Child {
    Command::new(TWR)
        .arg("runner")
        .args(args)
        .env("TWR_HOME", dir.join("home"))
        .env("TWR_TEST_RUNNER_START_HOLD", dir.join("held"))
        .env("TWR_BOX_PROBE", "off")
        .env("TWR_AGENT_PROBE", "off")
        .env_remove("TWR_RUNNER_TOKEN")
        .env_remove("RUST_LOG")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(dir.join("stderr")).unwrap())
        .spawn()
        .unwrap()
}

/// Waits for the start hold's marker, sends SIGTERM, and waits for the runner to exit (it is
/// killed after [`WAIT`]).
fn terminated_while_starting(dir: &Path, mut child: Child) -> ExitStatus {
    let until = Instant::now() + WAIT;
    while !dir.join("held").exists() {
        if Instant::now() > until || child.try_wait().unwrap().is_some() {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "the runner never reached its start hold: {}",
                std::fs::read_to_string(dir.join("stderr")).unwrap_or_default()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let sent = Command::new("kill")
        .args(["-TERM", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(sent.success());
    let until = Instant::now() + WAIT;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if Instant::now() > until {
            let _ = child.kill();
            let _ = child.wait();
            panic!("the runner did not stop on SIGTERM");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A box daemon's `twr runner` arguments on `d`, its socket and its log.
fn daemon_args(d: &Path) -> (Vec<String>, PathBuf, PathBuf) {
    let (socket, log) = (d.join("run/daemon.sock"), d.join("daemon.log"));
    let args = vec![
        "--daemon".to_string(),
        "--box=gpu".into(),
        format!("--hub-key={}", "ab".repeat(32)),
        format!("--daemon-socket={}", socket.display()),
        format!("--data-dir={}", d.join("data").display()),
        format!("--lock={}", d.join("locks/lock").display()),
        format!("--log={}", log.display()),
    ];
    (args, socket, log)
}

#[test]
fn a_box_daemon_terminated_while_it_starts_stops_cleanly_before_it_binds() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let (args, socket, log) = daemon_args(d);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let status = terminated_while_starting(d, runner(d, &args));
    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    assert_eq!(status.code(), Some(0), "{status}: {logged}");
    assert!(logged.contains("stopped before it started"), "{logged}");
    assert!(!socket.exists(), "the daemon bound its socket: {logged}");
}

/// Terminated while it waits for the box's lock (held here, so the opening fails after the
/// wait): asked to stop, the daemon stops cleanly, exit 0, rather than failing on the lock.
#[test]
fn a_box_daemon_terminated_while_it_waits_for_the_lock_stops_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(d.join("locks"))
        .unwrap();
    let held = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(d.join("locks/lock"))
        .unwrap();
    held.lock().unwrap();
    let (args, socket, log) = daemon_args(d);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let status = terminated_while_starting(d, runner(d, &args));
    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    assert_eq!(status.code(), Some(0), "{status}: {logged}");
    assert!(logged.contains("stopped before it started"), "{logged}");
    assert!(!socket.exists(), "the daemon bound its socket: {logged}");
    drop(held);
}

#[test]
fn a_token_runner_terminated_while_it_starts_stops_cleanly_before_it_connects() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let hub = TcpListener::bind("127.0.0.1:0").unwrap();
    hub.set_nonblocking(true).unwrap();
    let token = d.join("token");
    std::io::Write::write_all(
        &mut std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&token)
            .unwrap(),
        "0123456789abcdef".repeat(4).as_bytes(),
    )
    .unwrap();
    let child = runner(
        d,
        &[
            &format!("--hub=ws://{}", hub.local_addr().unwrap()),
            "--box=gpu",
            &format!("--token-file={}", token.display()),
            &format!("--data-dir={}", d.join("data").display()),
        ],
    );
    let status = terminated_while_starting(d, child);
    let said = std::fs::read_to_string(d.join("stderr")).unwrap_or_default();
    assert_eq!(status.code(), Some(0), "{status}: {said}");
    assert!(said.contains("stopped before it started"), "{said}");
    assert!(
        matches!(hub.accept(), Err(e) if e.kind() == std::io::ErrorKind::WouldBlock),
        "the runner connected: {said}"
    );
}
