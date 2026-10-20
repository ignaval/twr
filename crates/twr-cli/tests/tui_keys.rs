//! End-to-end: the real TUI in a pty, reading the bytes a terminal sends for its keys, against
//! a real hub. What crosses mosh is these bytes, unchanged (mosh 1.4 passes a key's bytes on
//! in one piece), so this is what the TUI gets over mosh from a terminal set up as
//! `twr terminal-setup` or Claude Code's `/terminal-setup` sets it up.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

const TWR: &str = env!("CARGO_BIN_EXE_twr");
const WAIT: Duration = Duration::from_secs(30);

/// A user token the test's hub and clients share: from inside a twr thread they would count as
/// its agent otherwise (debug builds only).
fn test_token(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("user.token");
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap()
        .write_all(format!("tui-keys-{:x}", std::process::id()).as_bytes())
        .unwrap();
    path
}

fn twr_cmd(home: &Path, token: &Path) -> Command {
    let mut c = Command::new(TWR);
    c.env_remove("TWR_WORKSPACE")
        .env_remove("RUST_LOG")
        .env("TWR_HOME", home)
        .env("TWR_TEST_USER_TOKEN_FILE", token)
        .env("TWR_AGENT_PROBE", "off")
        .env("TWR_BOX_PROBE", "off")
        .env("TWR_ENABLE_FAKE", "1");
    c
}

struct Killed(Child);

impl Drop for Killed {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn poll<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Shift-Enter as the terminal sends it once set up (`ESC CR`, Alt-Enter), as a terminal
/// speaking the keyboard protocol sends it (`CSI 13;2u`), and Ctrl-J (LF) all break the line;
/// a plain CR sends it. xterm's `CSI 27;2;13~`, which the TUI never asks for, types nothing.
#[test]
fn line_break_bytes_from_the_terminal_break_the_line() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let tokens = tempfile::tempdir().unwrap();
    let token = test_token(tokens.path());
    std::fs::write(
        home.join("config.toml"),
        "[orchestrator]\nruntime = \"fake\"\n",
    )
    .unwrap();
    let log = std::fs::File::create(home.join("hub-test.log")).unwrap();
    let _hub = Killed(
        twr_cmd(home, &token)
            .arg("hub")
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    poll("the hub", || {
        twr_cmd(home, &token)
            .args(["thread", "list"])
            .output()
            .unwrap()
            .status
            .success()
            .then_some(())
    });

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut cmd = CommandBuilder::new(TWR);
    cmd.env_clear();
    for (k, v) in [
        ("PATH", std::env::var("PATH").unwrap_or_default()),
        ("HOME", std::env::var("HOME").unwrap_or_default()),
        ("TERM", "xterm-256color".to_string()),
        ("TWR_HOME", home.display().to_string()),
        ("TWR_TEST_USER_TOKEN_FILE", token.display().to_string()),
        ("TWR_AGENT_PROBE", "off".to_string()),
        ("TWR_BOX_PROBE", "off".to_string()),
        ("TWR_ENABLE_FAKE", "1".to_string()),
    ] {
        cmd.env(k, v);
    }
    let mut child = pair.slave.spawn_command(cmd).unwrap();
    drop(pair.slave);
    let screen = Arc::new(Mutex::new(Vec::<u8>::new()));
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();
    {
        let screen = Arc::clone(&screen);
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                screen.lock().unwrap().extend_from_slice(&buf[..n]);
            }
        });
    }
    // The first frame: the input box's placeholder. Keys typed before it, or too soon after
    // it, are dropped as typed ahead; wait past that.
    poll("the TUI's first frame", || {
        let s = String::from_utf8_lossy(&screen.lock().unwrap()).into_owned();
        s.contains("message the orchestrator").then_some(())
    });
    std::thread::sleep(Duration::from_millis(600));
    // Each key in a write of its own, as a terminal sends them (and mosh passes them on).
    for chunk in [
        &b"one"[..],
        b"\x1b\r",
        b"two",
        b"\x1b[13;2u",
        b"three",
        b"\n",
        b"four",
        b"\x1b[27;2;13~",
        b"five",
        b"\r",
    ] {
        writer.write_all(chunk).unwrap();
        writer.flush().unwrap();
        std::thread::sleep(Duration::from_millis(80));
    }
    let said = poll("the message at the hub", || {
        let out = twr_cmd(home, &token)
            .args(["events", "orchestrator"])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .find(|e| e["kind"] == "turn.started")
            .and_then(|e| e["data"]["prompt"].as_str().map(str::to_string))
    });
    assert_eq!(said, "one\ntwo\nthree\nfourfive");
    // Ctrl-C quits.
    writer.write_all(b"\x03").unwrap();
    writer.flush().unwrap();
    let deadline = Instant::now() + WAIT;
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("the TUI did not quit");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
