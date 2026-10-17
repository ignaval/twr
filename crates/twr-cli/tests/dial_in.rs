//! End-to-end: dial-in boxes. The box's side of the setup, `twr runner install-token`, run as
//! the box's user would, with a `TWR_HOME` of its own: the token at a hidden prompt on a
//! terminal, or from a pipe. And the whole way on this machine: a hub with its runner listener
//! on loopback (`ws://`), `twr box add --dial-in`, the token piped into `install-token` on the
//! box's side, a real `twr runner` of the box running a thread's turns with the fake runtime,
//! a rotation the running runner picks up from its file, and a removal that revokes the box.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

const TWR: &str = env!("CARGO_BIN_EXE_twr");
const TOKEN: &str = "3f9a0c1d2e3f405162738495a6b7c8d9e0f1a2b3c4d5e6f708192a3b4c5dc2e1";
const OTHER: &str = "0aa9b8c7d6e5f4031221304f5e6d7c8b9aa1b2c3d4e5f60718293a4b5c6d7e8f";

fn command(home: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(TWR);
    c.args(args)
        .env("TWR_HOME", home)
        .env_remove("TWR_WORKSPACE")
        .env_remove("TWR_RUNNER_TOKEN")
        .env_remove("RUST_LOG")
        .env("TWR_TEST_USER_TOKEN_FILE", test_token())
        .env("TWR_AGENT_PROBE", "off")
        .env("TWR_BOX_PROBE", "off");
    c
}

/// A user token this test process's clients share (`TWR_TEST_USER_TOKEN_FILE`): run from
/// inside a twr thread, they would otherwise count as that thread's agent, which may not
/// install a token.
fn test_token() -> &'static Path {
    static TOKEN: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    TOKEN.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap().keep();
        let path = dir.join("user.token");
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap()
            .write_all(format!("{nanos:x}{:x}", std::process::id()).as_bytes())
            .unwrap();
        path
    })
}

/// A child process killed and reaped when dropped (a test that fails or times out leaves none
/// behind).
struct Reaped(Option<Child>);

impl Reaped {
    fn child(&mut self) -> &mut Child {
        self.0.as_mut().expect("not reaped yet")
    }

    /// Waits for it to exit, at most `limit`: its status; killed and reaped, then a panic
    /// naming `what`, past that.
    fn wait(&mut self, limit: Duration, what: &str) -> ExitStatus {
        let deadline = Instant::now() + limit;
        loop {
            if let Some(status) = self.child().try_wait().unwrap() {
                self.0 = None;
                return status;
            }
            if Instant::now() >= deadline {
                let mut child = self.0.take().unwrap();
                let _ = child.kill();
                let _ = child.wait();
                panic!("{what} did not finish within {} s", limit.as_secs());
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Reaped {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Reads all of `r` on a thread of its own: joined for what it read.
fn read_all(mut r: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = r.read_to_end(&mut buf);
        buf
    })
}

/// `twr <args>` with `input` piped to its stdin, at most 60 s (then killed, and a panic).
fn piped(home: &Path, args: &[&str], input: &str) -> Output {
    let mut child = Reaped(Some(
        command(home, args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    ));
    let stdout = read_all(child.child().stdout.take().unwrap());
    let stderr = read_all(child.child().stderr.take().unwrap());
    // It may leave without reading (its arguments refused).
    let mut stdin = child.child().stdin.take().unwrap();
    let _ = stdin.write_all(input.as_bytes());
    drop(stdin);
    let status = child.wait(Duration::from_secs(60), &format!("twr {args:?}"));
    Output {
        status,
        stdout: stdout.join().unwrap(),
        stderr: stderr.join().unwrap(),
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn mode(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

/// From a pipe: the token is stored privately in the default file, and the commands printed
/// name it by its absolute path and hold no token. The same token again is "unchanged";
/// another one is refused without --yes (another hub's box of the same name, maybe) and
/// replaced with it.
#[test]
fn a_piped_token_is_stored_privately() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let file = home.join("runner-tokens/lab.token");
    let args = [
        "runner",
        "install-token",
        "lab",
        "--hub",
        "ws://127.0.0.1:7878",
    ];
    let out = piped(home, &args, &format!("  {TOKEN}\n"));
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        format!("{TOKEN}\n")
    );
    assert_eq!(mode(&file), 0o600);
    assert_eq!(mode(file.parent().unwrap()), 0o700);
    assert!(
        !stdout.contains(TOKEN) && !stderr.contains(TOKEN),
        "{stdout}\n{stderr}"
    );
    assert!(
        stdout.contains(&format!(
            "stored box lab's token in {} (mode 0600)",
            file.display()
        )),
        "{stdout}"
    );
    // TWR_HOME is not the standard layout: the data directory is named too.
    let run = format!(
        "twr runner --hub ws://127.0.0.1:7878 --box=lab --token-file={} --data-dir={}",
        file.display(),
        home.join("data").display()
    );
    assert!(stdout.contains(&run), "{stdout}");
    let systemctl = std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("systemctl").is_file()));
    if systemctl {
        assert!(
            stdout.contains(&format!(
                "ExecStart={TWR} runner --hub ws://127.0.0.1:7878 --box=lab --token-file={}",
                file.display()
            )),
            "{stdout}"
        );
    }

    let out = piped(home, &args, TOKEN);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains("token in"),
        "{}",
        text(&out.stdout)
    );
    assert!(
        text(&out.stdout).contains("is unchanged"),
        "{}",
        text(&out.stdout)
    );

    let out = piped(home, &args, OTHER);
    let stderr = text(&out.stderr);
    assert!(!out.status.success());
    assert!(
        stderr.contains("box lab already has another token in")
            && stderr.contains("pass --yes")
            && stderr.contains("Two hubs need two box names"),
        "{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        format!("{TOKEN}\n")
    );
    let out = piped(home, &[&args[..3], &["--yes"]].concat(), OTHER);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        format!("{OTHER}\n")
    );
    // Without --hub: the command to complete, no service.
    let stdout = text(&out.stdout);
    assert!(
        stdout.contains(&format!(
            "Run it with `twr runner --hub <the hub's URL> --box=lab --token-file={}",
            file.display()
        )),
        "{stdout}"
    );
    assert!(!stdout.contains("[Unit]"), "{stdout}");

    // Refused, nothing stored: a token with a space, a short one, an insecure hub URL.
    for (input, extra, want) in [
        ("two words-0123456789abcdef", &[][..], "holds a space"),
        ("short", &[], "too short"),
        (
            TOKEN,
            &["--hub", "ws://10.0.0.5:7878"],
            "refusing plain ws:// to 10.0.0.5",
        ),
    ] {
        let other = home.join("other");
        let out = piped(&other, &[&args[..3], extra].concat(), input);
        let stderr = text(&out.stderr);
        assert!(!out.status.success(), "{input}");
        assert!(stderr.contains(want), "{stderr}");
        assert!(!other.join("runner-tokens/lab.token").exists());
    }
}

#[cfg(target_os = "linux")]
extern "C" {
    fn posix_openpt(flags: i32) -> i32;
    fn grantpt(fd: i32) -> i32;
    fn unlockpt(fd: i32) -> i32;
    fn ptsname_r(fd: i32, buf: *mut std::ffi::c_char, len: usize) -> i32;
    fn setsid() -> i32;
    fn ioctl(fd: i32, request: std::ffi::c_ulong, ...) -> i32;
}

/// A new pseudo-terminal: its master, and its slave opened (not as this process's
/// controlling terminal).
#[cfg(target_os = "linux")]
fn open_pty() -> (std::fs::File, std::fs::File) {
    use std::os::fd::FromRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    const O_RDWR: i32 = 2;
    const O_NOCTTY: i32 = 0o400;
    // SAFETY: plain calls on the descriptor posix_openpt returned; `name` outlives ptsname_r.
    unsafe {
        let master = posix_openpt(O_RDWR | O_NOCTTY);
        assert!(
            master >= 0,
            "posix_openpt: {}",
            std::io::Error::last_os_error()
        );
        let master = std::fs::File::from_raw_fd(master);
        let fd = std::os::fd::AsRawFd::as_raw_fd(&master);
        assert_eq!(grantpt(fd), 0, "grantpt");
        assert_eq!(unlockpt(fd), 0, "unlockpt");
        let mut name = [0 as std::ffi::c_char; 128];
        assert_eq!(ptsname_r(fd, name.as_mut_ptr(), name.len()), 0, "ptsname_r");
        let name = std::ffi::CStr::from_ptr(name.as_ptr()).to_str().unwrap();
        let slave = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(O_NOCTTY)
            .open(name)
            .unwrap();
        (master, slave)
    }
}

/// `twr runner install-token lab --hub ws://127.0.0.1:7878` on a new pseudo-terminal, as its
/// controlling terminal (so `/dev/tty` is it): the child, the master, the slave, and what the
/// terminal shows, chunk by chunk, from a reader thread (joined once the child's side closes).
#[cfg(target_os = "linux")]
fn on_a_pty(
    home: &Path,
) -> (
    Reaped,
    std::fs::File,
    std::fs::File,
    std::sync::mpsc::Receiver<Vec<u8>>,
    std::thread::JoinHandle<()>,
) {
    use std::os::unix::process::CommandExt;
    let (master, slave) = open_pty();
    let mut c = command(
        home,
        &[
            "runner",
            "install-token",
            "lab",
            "--hub",
            "ws://127.0.0.1:7878",
        ],
    );
    c.stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave.try_clone().unwrap());
    // SAFETY: setsid(2) and ioctl(2) are async-signal-safe and nothing is allocated: a new
    // session whose controlling terminal is the pseudo-terminal on stdin (TIOCSCTTY), so
    // `/dev/tty` is it.
    unsafe {
        c.pre_exec(|| {
            if setsid() == -1 || ioctl(0, 0x540E, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = Reaped(Some(c.spawn().unwrap()));
    drop(c);
    // What the terminal shows, read on another thread (a read blocks), until the child's end
    // closes its side (EIO) and the test drops its slave.
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    let mut reader = master.try_clone().unwrap();
    let thread = std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    (child, master, slave, rx, thread)
}

/// Whether the terminal of `master` has echo, line editing and signals on, as a shell has them
/// (`stty -a`).
#[cfg(target_os = "linux")]
fn terminal_restored(master: &std::fs::File) -> String {
    let out = Command::new("stty")
        .arg("-a")
        .stdin(open_slave_of(master))
        .output()
        .unwrap();
    let modes = text(&out.stdout);
    assert!(out.status.success(), "{}", text(&out.stderr));
    for mode in ["echo", "icanon", "isig"] {
        assert!(
            modes.split_whitespace().any(|w| w == mode),
            "{mode} is off: {modes}"
        );
    }
    modes
}

/// On a terminal: the prompt is shown only once echo is off, so a token typed the moment the
/// prompt's first byte appears is never echoed; the file holds it, and the terminal's echo is
/// back on afterwards. Also typed: an arrow key and a correction, which the prompt edits out.
#[cfg(target_os = "linux")]
#[test]
fn the_prompt_never_shows_the_token() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let (mut child, mut master, slave, rx, reader) = on_a_pty(home);
    let first = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("no prompt within 30 s");
    master
        .write_all(format!("{}\x1b[Dx\x7f{}\r", &TOKEN[..10], &TOKEN[10..]).as_bytes())
        .unwrap();
    let status = child.wait(Duration::from_secs(30), "install-token");
    drop(slave);
    let mut shown = first;
    while let Ok(b) = rx.recv_timeout(Duration::from_millis(500)) {
        shown.extend(b);
    }
    let shown = text(&shown);
    assert!(status.success(), "{shown}");
    assert!(
        shown.starts_with("Token for box lab (from `twr box add` on the hub; it is not shown): "),
        "{shown}"
    );
    assert!(
        !shown.contains(&TOKEN[..10]),
        "the token was echoed: {shown}"
    );
    assert!(
        !shown.contains(&TOKEN[10..]),
        "the token was echoed: {shown}"
    );
    assert!(shown.contains("stored box lab's token"), "{shown}");
    assert_eq!(
        std::fs::read_to_string(home.join("runner-tokens/lab.token")).unwrap(),
        format!("{TOKEN}\n")
    );
    // The terminal's settings are back: echo on, as the shell had them.
    terminal_restored(&master);
    drop(master);
    reader.join().unwrap();
}

/// A termination signal sent while the prompt waits (SIGTERM, SIGINT, SIGHUP: not typed, the
/// keyboard's are bytes) ends install-token by that signal, with the terminal's settings put
/// back first, and nothing stored.
#[cfg(target_os = "linux")]
#[test]
fn a_signal_at_the_prompt_restores_the_terminal() {
    use std::os::unix::process::ExitStatusExt;
    for (sig, name) in [(15, "TERM"), (2, "INT"), (1, "HUP")] {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        let (mut child, master, slave, rx, reader) = on_a_pty(home);
        rx.recv_timeout(Duration::from_secs(30))
            .expect("no prompt within 30 s");
        // Echo is off once the prompt shows: the signal comes while the read waits.
        let pid = child.child().id().to_string();
        let sent = Command::new("kill")
            .args([&format!("-{name}"), &pid])
            .status()
            .unwrap();
        assert!(sent.success());
        let status = child.wait(Duration::from_secs(30), "install-token");
        assert_eq!(status.signal(), Some(sig), "SIG{name}: {status:?}");
        terminal_restored(&master);
        assert!(!home.join("runner-tokens/lab.token").exists());
        drop(slave);
        drop(master);
        reader.join().unwrap();
    }
}

/// The slave of pseudo-terminal `master`, opened again (not as a controlling terminal).
#[cfg(target_os = "linux")]
fn open_slave_of(master: &std::fs::File) -> std::fs::File {
    use std::os::unix::fs::OpenOptionsExt;
    let fd = std::os::fd::AsRawFd::as_raw_fd(master);
    let mut name = [0 as std::ffi::c_char; 128];
    // SAFETY: `name` outlives the call, which writes at most its length.
    let name = unsafe {
        assert_eq!(ptsname_r(fd, name.as_mut_ptr(), name.len()), 0, "ptsname_r");
        std::ffi::CStr::from_ptr(name.as_ptr())
            .to_str()
            .unwrap()
            .to_string()
    };
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(0o400)
        .open(name)
        .unwrap()
}

/// `twr <args>` on `home` (no input): its stdout, once it succeeded.
fn ok(home: &Path, args: &[&str]) -> String {
    let out = piped(home, args, "");
    assert!(
        out.status.success(),
        "twr {args:?} failed: {}\n{}",
        text(&out.stderr),
        text(&out.stdout)
    );
    text(&out.stdout)
}

/// `twr <args>` on `home` (no input): its stderr, once it failed.
fn fails(home: &Path, args: &[&str]) -> String {
    let out = piped(home, args, "");
    assert!(
        !out.status.success(),
        "twr {args:?} succeeded: {}",
        text(&out.stdout)
    );
    text(&out.stderr)
}

/// Retries `f` every 100 ms until it gives something, at most `limit` (then a panic naming
/// `what`).
fn poll<T>(what: &str, limit: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// `hub.info`'s entry of box `name`, from the hub of `home`.
fn box_status(home: &Path, name: &str) -> serde_json::Value {
    serde_json::from_str(&ok(home, &["box", "status", name, "--json"])).unwrap()
}

/// The token of `[boxes.<name>]` in `boxes.toml` of `home` (as `twr box add` writes it).
fn file_token(home: &Path, name: &str) -> String {
    let text = std::fs::read_to_string(home.join("boxes.toml")).unwrap();
    let table = format!("[boxes.{name}]");
    let rest = &text[text
        .find(&table)
        .unwrap_or_else(|| panic!("no {table}: {text}"))..];
    let line = rest
        .lines()
        .find(|l| l.starts_with("token = "))
        .unwrap_or_else(|| panic!("no token: {text}"));
    line.trim_start_matches("token = ")
        .trim_matches('"')
        .to_string()
}

/// The `report.ready` events of `thread` so far.
fn reports(home: &Path, thread: &str) -> usize {
    ok(home, &["events", thread])
        .lines()
        .filter(|l| l.contains("\"report.ready\""))
        .count()
}

/// Without `[hub] listen`, `twr box add --dial-in` is refused before anything is written (and
/// no hub is started), saying how to turn the listener on; `--hub` without `--dial-in` is
/// refused too.
#[test]
fn without_a_listener_a_dial_in_box_is_refused() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    std::fs::write(home.join("config.toml"), "").unwrap();
    let e = fails(
        home,
        &[
            "box",
            "add",
            "lab",
            "--dial-in",
            "--hub",
            "ws://127.0.0.1:7878",
            "--yes",
        ],
    );
    assert!(
        e.contains("runner listener, which is off") && e.contains("listen = \"127.0.0.1:7878\""),
        "{e}"
    );
    assert!(!home.join("boxes.toml").exists());
    assert!(
        !home.join("data").join("hub.log").exists(),
        "a hub was started"
    );
    let e = fails(
        home,
        &[
            "box",
            "add",
            "lab",
            "--hub",
            "ws://127.0.0.1:7878",
            "me@host",
        ],
    );
    assert!(e.contains("--hub and --ca are for a dial-in box"), "{e}");
    assert!(!home.join("boxes.toml").exists());
}

/// A dial-in box from `add` to `remove`, with a real hub and a real `twr runner` of the box on
/// this machine (each with a `TWR_HOME` of its own; the listener on loopback): the token goes
/// from `twr box token` on the hub into `install-token` on the box through a pipe, never on
/// stdout of `add`; the runner finds it in its default file and runs a thread's turns; `twr
/// box token --rotate` drops it at once and the same runner process is online again within
/// seconds of the new token's install (its file is watched while it waits after the refusal);
/// `twr box remove` revokes it live, though its table's transport was edited to ssh since. A
/// box in both files (`both`): `remove` takes it out of boxes.toml and says the hub still
/// accepts it from config.toml.
#[cfg(target_os = "linux")]
#[test]
fn a_dial_in_box_from_add_to_remove() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let (hub_home, box_home) = (dir.join("hub"), dir.join("box"));
    std::fs::create_dir_all(&hub_home).unwrap();
    std::fs::create_dir_all(&box_home).unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let url = format!("ws://127.0.0.1:{port}");
    let both = "b0".repeat(32);
    std::fs::write(
        hub_home.join("config.toml"),
        format!("[hub]\nlisten = \"127.0.0.1:{port}\"\n\n[boxes.both]\ntoken = \"{both}\"\n"),
    )
    .unwrap();
    let log = std::fs::File::create(dir.join("hub.log")).unwrap();
    let mut hub = Reaped(Some(
        command(&hub_home, &["hub"])
            // The thread runs the fake runtime, which twr offers only on request.
            .env("TWR_ENABLE_FAKE", "1")
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    ));
    let wait = Duration::from_secs(60);
    poll("the hub", wait, || {
        piped(&hub_home, &["thread", "list"], "")
            .status
            .success()
            .then_some(())
    });

    // On the hub: the box, its token in boxes.toml (private), not on stdout (no terminal).
    let out = ok(
        &hub_home,
        &["box", "add", "lab", "--dial-in", "--hub", &url, "--yes"],
    );
    let token = file_token(&hub_home, "lab");
    assert_eq!(token.len(), 64, "{token}");
    assert!(token.bytes().all(|b| b.is_ascii_hexdigit()), "{token}");
    assert!(!out.contains(&token), "{out}");
    assert!(out.contains("added [boxes.lab] to"), "{out}");
    assert!(
        out.contains(&format!("  twr runner install-token lab --hub {url}\n")),
        "{out}"
    );
    assert!(out.contains("The token is not printed"), "{out}");
    let boxes = std::fs::read_to_string(hub_home.join("boxes.toml")).unwrap();
    assert!(boxes.contains("transport = \"token\""), "{boxes}");
    assert_eq!(mode(&hub_home.join("boxes.toml")), 0o600);
    let status = box_status(&hub_home, "lab");
    assert_eq!(status["state"], "offline", "{status}");

    // On the box: the token piped in, stored privately.
    let install = |home: &Path| {
        let token = ok(&hub_home, &["box", "token", "lab"]);
        let out = piped(
            home,
            &["runner", "install-token", "lab", "--hub", &url, "--yes"],
            &token,
        );
        assert!(out.status.success(), "{}", text(&out.stderr));
        assert!(!text(&out.stdout).contains(token.trim()));
        token.trim().to_string()
    };
    assert_eq!(install(&box_home), token);
    let file = box_home.join("runner-tokens/lab.token");
    assert_eq!(mode(&file), 0o600);
    assert_eq!(mode(file.parent().unwrap()), 0o700);

    // The box's runner, with its default token file; online.
    let runner_log = dir.join("runner.log");
    let mut runner = Reaped(Some(
        command(&box_home, &["runner", "--hub", &url, "--box=lab"])
            .env("TWR_ENABLE_FAKE", "1")
            .env("XDG_STATE_HOME", dir.join("state"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&runner_log).unwrap())
            .spawn()
            .unwrap(),
    ));
    let runner_said = || std::fs::read_to_string(&runner_log).unwrap_or_default();
    poll("the box online", wait, || {
        (box_status(&hub_home, "lab")["state"] == "online").then_some(())
    });
    let status = ok(&hub_home, &["box", "status", "lab"]);
    assert!(status.contains("token, dial-in"), "{status}");

    // A thread on the box runs a turn.
    let repo = twr_core::testkit::git::init_repo(&dir.join("repo"));
    let repo = repo.to_str().unwrap();
    let created = ok(
        &hub_home,
        &[
            "thread",
            "new",
            repo,
            "--box",
            "lab",
            "--runtime",
            "fake",
            "--name",
            "remote",
        ],
    );
    assert!(created.starts_with("created remote ("), "{created}");
    ok(&hub_home, &["send", "remote", "hello from the box"]);
    poll("the first report", wait, || {
        (reports(&hub_home, "remote") >= 1).then_some(())
    });
    // Its project recorded without looking, and the hub takes it.
    let out = ok(&hub_home, &["box", "project", "lab", "api", "--path", repo]);
    assert!(out.contains("recorded without checking"), "{out}");
    assert!(out.contains("recorded and active"), "{out}");
    // Doctor: the hub's view, the listener and the token.
    let out = ok(&hub_home, &["box", "doctor", "lab"]);
    assert!(out.contains("✓ listener: the running hub listens"), "{out}");
    assert!(
        out.contains("✓ token: box lab has a token of 64 characters"),
        "{out}"
    );
    assert!(!out.contains(&token), "{out}");

    // Rotation: the hub drops the runner at once and refuses the old token...
    let before = box_status(&hub_home, "lab")["refused"].clone();
    let out = ok(&hub_home, &["box", "token", "lab", "--rotate", "--yes"]);
    let new = file_token(&hub_home, "lab");
    assert_ne!(new, token);
    assert!(!out.contains(&new), "{out}");
    assert!(out.contains("The running hub took the new token"), "{out}");
    let refused = poll("the old token refused", wait, || {
        let s = box_status(&hub_home, "lab");
        (s["state"] != "online" && s["refused"].is_object() && s["refused"] != before).then(|| {
            s["refused"]["reason"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
    });
    assert!(refused.contains("token"), "{refused}");
    // ...until the box has the new one: the same runner, online again well before its 60 s
    // wait after a refusal is over.
    assert_eq!(install(&box_home), new);
    let installed = Instant::now();
    poll("the box online with the new token", wait, || {
        (box_status(&hub_home, "lab")["state"] == "online").then_some(())
    });
    assert!(
        installed.elapsed() < Duration::from_secs(30),
        "{:?}: {}",
        installed.elapsed(),
        runner_said()
    );
    assert!(
        runner.child().try_wait().unwrap().is_none(),
        "the same runner"
    );
    assert!(
        runner_said().contains("the token file changed"),
        "{}",
        runner_said()
    );
    assert!(!runner_said().contains(&new) && !runner_said().contains(&token));
    ok(&hub_home, &["send", "remote", "after the rotation"]);
    poll("the second report", wait, || {
        (reports(&hub_home, "remote") >= 2).then_some(())
    });
    // The hub compares the box's settings with the new token: its projects are no conflict.
    let out = ok(&hub_home, &["box", "project", "lab", "api", "--path", repo]);
    assert!(out.contains("recorded and active"), "{out}");

    // Removal revokes it live; the runner keeps trying. Also once the table's transport was
    // edited since the hub read it: the hub decides from the box it runs.
    let before = box_status(&hub_home, "lab")["refused"].clone();
    let boxes = std::fs::read_to_string(hub_home.join("boxes.toml")).unwrap();
    assert_eq!(boxes.matches("transport = \"token\"").count(), 1, "{boxes}");
    std::fs::write(
        hub_home.join("boxes.toml"),
        boxes.replace("transport = \"token\"", "transport = \"ssh\""),
    )
    .unwrap();
    let out = ok(&hub_home, &["box", "remove", "lab"]);
    assert!(
        out.contains("The running hub no longer accepts box lab's runner."),
        "{out}"
    );
    assert!(
        out.contains("systemctl --user disable --now twr-runner-lab.service")
            && out.contains("rm ~/.config/twr/runner-tokens/lab.token"),
        "{out}"
    );
    poll("the removed box refused", wait, || {
        let s = box_status(&hub_home, "lab");
        (s["state"] != "online" && s["refused"].is_object() && s["refused"] != before).then_some(())
    });
    assert!(
        runner.child().try_wait().unwrap().is_none(),
        "it keeps trying"
    );

    // A box in both files: out of boxes.toml, still accepted from config.toml.
    let mut boxes = std::fs::read_to_string(hub_home.join("boxes.toml")).unwrap();
    boxes.push_str(&format!("\n[boxes.both]\ntoken = \"{both}\"\n"));
    std::fs::write(hub_home.join("boxes.toml"), boxes).unwrap();
    let out = ok(&hub_home, &["box", "remove", "both"]);
    assert!(
        out.contains("box both is still configured in")
            && out.contains("the hub keeps accepting its runner")
            && !out.contains("systemctl"),
        "{out}"
    );
    assert_eq!(ok(&hub_home, &["box", "token", "both"]).trim(), both);
    let e = fails(&hub_home, &["box", "token", "both", "--rotate", "--yes"]);
    assert!(e.contains("which twr does not edit"), "{e}");
    assert!(ok(&hub_home, &["box", "token", "both", "--reload"]).contains("nothing changed"));

    drop(runner);
    ok(&hub_home, &["hub", "stop"]);
    hub.wait(wait, "the hub");
}

/// `twr hub` of `home` in the background, its output in `log`, paused at `gate` when given:
/// the gate's variable and file (`TWR_TEST_HUB_START_GATE`: right after it read its config;
/// `TWR_TEST_HUB_STOP_GATE`: right after it removed its socket; until that file is removed).
fn start_hub(home: &Path, log: &Path, gate: Option<(&str, &Path)>) -> Reaped {
    let log = std::fs::File::create(log).unwrap();
    let mut c = command(home, &["hub"]);
    if let Some((env, gate)) = gate {
        c.env(env, gate);
    }
    Reaped(Some(
        c.stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    ))
}

/// `twr <args>` on `home` started in the background: the child, and its stdout and stderr
/// readers.
type Running = (
    Reaped,
    std::thread::JoinHandle<Vec<u8>>,
    std::thread::JoinHandle<Vec<u8>>,
);

fn spawn(home: &Path, args: &[&str]) -> Running {
    let mut child = Reaped(Some(
        command(home, args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    ));
    let stdout = read_all(child.child().stdout.take().unwrap());
    let stderr = read_all(child.child().stderr.take().unwrap());
    (child, stdout, stderr)
}

/// [`spawn`]'s command once it ends (at most 60 s): its output.
fn finish((mut child, stdout, stderr): Running, what: &str) -> Output {
    let status = child.wait(Duration::from_secs(60), what);
    Output {
        status,
        stdout: stdout.join().unwrap(),
        stderr: stderr.join().unwrap(),
    }
}

/// A free loopback port, and a config.toml in `home` whose hub listens on it.
fn listening_config(home: &Path) {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    std::fs::write(
        home.join("config.toml"),
        format!("[hub]\nlisten = \"127.0.0.1:{port}\"\n"),
    )
    .unwrap();
}

/// A hub that is starting, its config read but its socket not bound yet, still takes a
/// removal and a rotation made meanwhile: it reads the files under boxes.toml's lock and holds
/// it until the socket is bound, so `twr box remove` and `twr box token --rotate` wait for it,
/// then find the hub and have it take their change. Without the lock they would find no hub,
/// and it would start with the removed and the replaced token.
#[test]
fn a_starting_hub_takes_a_removal_or_rotation_made_meanwhile() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let home = dir.join("hub");
    std::fs::create_dir_all(&home).unwrap();
    listening_config(&home);
    std::fs::write(
        home.join("boxes.toml"),
        format!(
            "[boxes.lab]\ntransport = \"token\"\ntoken = \"{TOKEN}\"\n\n[boxes.gpu]\ntransport \
             = \"token\"\ntoken = \"{OTHER}\"\n"
        ),
    )
    .unwrap();
    let gate = dir.join("gate");
    std::fs::write(&gate, "").unwrap();
    let mut hub = start_hub(
        &home,
        &dir.join("hub.log"),
        Some(("TWR_TEST_HUB_START_GATE", &gate)),
    );
    let wait = Duration::from_secs(60);
    poll("the hub's read of its config", wait, || {
        dir.join("gate.reached").exists().then_some(())
    });

    let mut remove = spawn(&home, &["box", "remove", "lab"]);
    let mut rotate = spawn(&home, &["box", "token", "gpu", "--rotate", "--yes"]);
    std::thread::sleep(Duration::from_secs(1));
    assert!(
        remove.0.child().try_wait().unwrap().is_none(),
        "the removal waits for the starting hub"
    );
    assert!(
        rotate.0.child().try_wait().unwrap().is_none(),
        "the rotation waits for the starting hub"
    );
    std::fs::remove_file(&gate).unwrap();

    let out = finish(remove, "twr box remove");
    assert!(out.status.success(), "{}", text(&out.stderr));
    let said = text(&out.stdout);
    assert!(
        said.contains("The running hub no longer accepts box lab's runner."),
        "{said}"
    );
    let out = finish(rotate, "twr box token --rotate");
    assert!(out.status.success(), "{}", text(&out.stderr));
    let said = text(&out.stdout);
    assert!(
        said.contains("The running hub took the new token"),
        "{said}"
    );
    assert_ne!(file_token(&home, "gpu"), OTHER);
    // The hub has what the files say: no token for lab, gpu's new one.
    let out = ok(&home, &["box", "token", "lab", "--reload"]);
    assert!(out.contains("accepts no runner of it"), "{out}");
    let out = ok(&home, &["box", "token", "gpu", "--reload"]);
    assert!(out.contains("nothing changed"), "{out}");

    ok(&home, &["hub", "stop"]);
    hub.wait(wait, "the hub");
}

/// A hub that is shutting down, its socket removed but its runner listener still up, is
/// waited for: `twr box remove` and `twr box token --rotate`, finding no socket, wait for the
/// hub's data-directory lock, so they say no hub runs only once it exited (or was killed),
/// never while it may still accept the removed or the old token.
#[test]
fn a_stopping_hub_is_waited_for_until_it_exits() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let home = dir.join("hub");
    std::fs::create_dir_all(&home).unwrap();
    listening_config(&home);
    std::fs::write(
        home.join("boxes.toml"),
        format!(
            "[boxes.lab]\ntransport = \"token\"\ntoken = \"{TOKEN}\"\n\n[boxes.gpu]\ntransport \
             = \"token\"\ntoken = \"{OTHER}\"\n"
        ),
    )
    .unwrap();
    let (gate, reached) = (dir.join("gate"), dir.join("gate.reached"));
    let wait = Duration::from_secs(60);
    let stopping_hub = || {
        let _ = std::fs::remove_file(&reached);
        std::fs::write(&gate, "").unwrap();
        let hub = start_hub(
            &home,
            &dir.join("hub.log"),
            Some(("TWR_TEST_HUB_STOP_GATE", &gate)),
        );
        poll("the hub", wait, || {
            text(&piped(&home, &["box", "token", "gpu", "--reload"], "").stdout)
                .contains("nothing changed")
                .then_some(())
        });
        let stop = spawn(&home, &["hub", "stop"]);
        poll("the hub's socket removed", wait, || {
            reached.exists().then_some(())
        });
        (hub, stop)
    };

    // A hub that exits: the removal waits for it.
    let (mut hub, stop) = stopping_hub();
    let mut remove = spawn(&home, &["box", "remove", "lab"]);
    std::thread::sleep(Duration::from_secs(1));
    assert!(
        remove.0.child().try_wait().unwrap().is_none(),
        "the removal waits for the stopping hub"
    );
    std::fs::remove_file(&gate).unwrap();
    let out = finish(remove, "twr box remove");
    assert!(out.status.success(), "{}", text(&out.stderr));
    let said = text(&out.stdout);
    assert!(
        said.contains("No hub runs: none accepts the box's runner."),
        "{said}"
    );
    let out = finish(stop, "twr hub stop");
    assert!(out.status.success(), "{}", text(&out.stderr));
    hub.wait(wait, "the hub");

    // A hub killed while it shuts down lets its lock go too: the rotation is done then.
    let (mut hub, stop) = stopping_hub();
    let mut rotate = spawn(&home, &["box", "token", "gpu", "--rotate", "--yes"]);
    std::thread::sleep(Duration::from_secs(1));
    assert!(
        rotate.0.child().try_wait().unwrap().is_none(),
        "the rotation waits for the stopping hub"
    );
    hub.child().kill().unwrap();
    hub.wait(wait, "the killed hub");
    let out = finish(rotate, "twr box token --rotate");
    assert!(out.status.success(), "{}", text(&out.stderr));
    let said = text(&out.stdout);
    assert!(
        said.contains("No hub runs: one started later reads the new token."),
        "{said}"
    );
    assert_ne!(file_token(&home, "gpu"), OTHER);
    finish(stop, "twr hub stop");
}

/// A removal or rotation the running hub refuses to take fails, its edit of boxes.toml kept,
/// and says how to have the hub take it (`--reload`, or a restart); `--reload` then does.
#[test]
fn a_change_the_running_hub_did_not_take_fails() {
    let dir = tempfile::tempdir().unwrap();
    let dir = dir.path();
    let home = dir.join("hub");
    std::fs::create_dir_all(&home).unwrap();
    listening_config(&home);
    // gpu is a command box to the running hub, which has no token to change for it.
    std::fs::write(
        home.join("boxes.toml"),
        format!(
            "[boxes.lab]\ntransport = \"token\"\ntoken = \"{TOKEN}\"\n\n[boxes.gpu]\ntransport \
             = \"command\"\ncommand = [\"false\"]\n"
        ),
    )
    .unwrap();
    let mut hub = start_hub(&home, &dir.join("hub.log"), None);
    let wait = Duration::from_secs(60);
    poll("the hub", wait, || {
        text(&piped(&home, &["box", "token", "lab", "--reload"], "").stdout)
            .contains("nothing changed")
            .then_some(())
    });

    // Rotation: gpu is a token box in the file now, not to the hub.
    std::fs::write(
        home.join("boxes.toml"),
        format!(
            "[boxes.lab]\ntransport = \"token\"\ntoken = \"{TOKEN}\"\n\n[boxes.gpu]\ntransport \
             = \"token\"\ntoken = \"{OTHER}\"\n"
        ),
    )
    .unwrap();
    let e = fails(&home, &["box", "token", "gpu", "--rotate", "--yes"]);
    assert!(
        e.contains(
            "but the running hub did not take the change and may still accept the old token: \
             run `twr box token gpu --reload`, or restart the hub (`twr hub stop`, then `twr`)"
        ),
        "{e}"
    );
    assert_ne!(file_token(&home, "gpu"), OTHER, "the new token stays");

    // Removal: an entry the hub can't read makes it refuse to read the files again.
    let mut boxes = std::fs::read_to_string(home.join("boxes.toml")).unwrap();
    boxes.push_str("\n[boxes.bad]\ntransport = 3\n");
    std::fs::write(home.join("boxes.toml"), &boxes).unwrap();
    let out = piped(&home, &["box", "remove", "lab"], "");
    assert!(!out.status.success(), "{}", text(&out.stdout));
    let (said, e) = (text(&out.stdout), text(&out.stderr));
    assert!(said.contains("The running hub answered:"), "{said}");
    assert!(
        e.contains("box lab is out of")
            && e.contains("may still accept its runner: run `twr box token lab --reload`"),
        "{e}"
    );
    let boxes = std::fs::read_to_string(home.join("boxes.toml")).unwrap();
    assert!(!boxes.contains("[boxes.lab]"), "{boxes}");
    // Once the files read, `--reload` revokes it.
    std::fs::write(
        home.join("boxes.toml"),
        boxes.replace("[boxes.bad]\ntransport = 3\n", ""),
    )
    .unwrap();
    let out = ok(&home, &["box", "token", "lab", "--reload"]);
    assert!(
        out.contains("the running hub no longer accepts its runner"),
        "{out}"
    );

    ok(&home, &["hub", "stop"]);
    hub.wait(wait, "the hub");
}
