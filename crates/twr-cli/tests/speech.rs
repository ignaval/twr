//! End-to-end: reading aloud from the command line (docs/read-aloud.md), against a throwaway
//! hub under a temporary `TWR_HOME` whose orchestrator is the fake runtime (it says back what
//! it is told). `twr speech feed` (the text feed and the proxy `twr speak --ssh` reads),
//! `twr speak` with a `command` engine that records what it is given, `--print-service`, and
//! `twr remote` with recorder scripts for mosh and ssh (`TWR_MOSH`, `TWR_SSH`). No audio, no
//! real ssh or mosh.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use twr_core::paths::Paths;

const TWR: &str = env!("CARGO_BIN_EXE_twr");
const WAIT: Duration = Duration::from_secs(20);

fn command(home: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(TWR);
    c.args(args)
        .env("TWR_HOME", home)
        .env_remove("TWR_WORKSPACE")
        .env_remove("RUST_LOG")
        .env_remove("TWR_SSH")
        .env_remove("TWR_MOSH")
        .env("TWR_TEST_USER_TOKEN_FILE", test_token())
        // No hub or runner of a test runs the user's login shell or real agent CLIs.
        .env("TWR_AGENT_PROBE", "off")
        .env("TWR_BOX_PROBE", "off");
    c
}

/// A user token this test process's hubs and clients share (`TWR_TEST_USER_TOKEN_FILE`): run
/// from inside a twr thread, the tests' clients would otherwise count as that thread's agent
/// to the hubs they start, which descend from it too.
fn test_token() -> &'static Path {
    static TOKEN: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    TOKEN.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap().keep();
        let path = dir.join("user.token");
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // Private to this user, or the hub ignores it.
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

fn twr(home: &Path, args: &[&str]) -> Output {
    command(home, args).output().expect("running twr")
}

fn ok(home: &Path, args: &[&str]) -> String {
    let out = twr(home, args);
    assert!(
        out.status.success(),
        "twr {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// Retries `f` until it returns Some or the deadline passes.
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

/// A child killed and reaped when dropped.
struct Proc(Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A temporary home whose orchestrator is the fake runtime, with `extra` appended to its
/// config.
fn home_with(extra: &str) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        format!("default_runtime = \"fake\"\n{extra}"),
    )
    .unwrap();
    home
}

/// `twr hub` in the foreground, once it answers.
fn start_hub(home: &Path) -> Proc {
    let log = std::fs::File::create(home.join("hub.log")).unwrap();
    let hub = Proc(
        command(home, &["hub"])
            // The orchestrator runs the fake runtime, which the hub offers only on request.
            .env("TWR_ENABLE_FAKE", "1")
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    poll("the hub", || {
        twr(home, &["thread", "list"])
            .status
            .success()
            .then_some(())
    });
    hub
}

/// The lines `r` gives, on a channel (read by a thread of their own).
fn lines(r: impl std::io::Read + Send + 'static) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(r).lines() {
            let Ok(line) = line else { return };
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    rx
}

/// The next line, within [`WAIT`].
fn next_line(rx: &mpsc::Receiver<String>, what: &str) -> String {
    rx.recv_timeout(WAIT)
        .unwrap_or_else(|e| panic!("no line for {what}: {e}"))
}

/// Lines until one contains `needle` (that one), within [`WAIT`].
fn line_with(rx: &mpsc::Receiver<String>, needle: &str) -> String {
    let deadline = Instant::now() + WAIT;
    let mut seen = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(line) if line.contains(needle) => return line,
            Ok(line) => seen.push(line),
            Err(e) => panic!("no line with {needle:?} ({e}); seen: {seen:#?}"),
        }
    }
}

/// `speech.status` of `home`'s default hub, as a client of the user's (the test token).
fn speech_status(home: &Path) -> Value {
    let socket = Paths::with_home(home).socket;
    let token = std::fs::read_to_string(test_token()).unwrap();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let mut c = twr_core::client::Client::connect(&socket).await.unwrap();
        c.hello(json!({"client": "test", "user_token": token.trim()}))
            .await
            .unwrap();
        c.call("speech.status", Value::Null).await.unwrap()
    })
}

/// Waits until a speaker called `name` is attached to `home`'s default hub.
fn wait_for_speaker(home: &Path, name: &str) {
    poll(&format!("speaker {name}"), || {
        let s = speech_status(home);
        s["speakers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|sp| sp["name"] == name)
            .then_some(())
    });
}

/// An executable script `name` in `dir`, holding `body` after a `sh` shebang.
fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    let src = dir.join(format!("{name}.src"));
    std::fs::write(&src, format!("#!/bin/sh\n{body}")).unwrap();
    // Written by `cp`, never held open for writing by this process: a child another test
    // forks meanwhile would inherit the descriptor, and exec'ing the file would fail (ETXTBSY).
    let st = Command::new("cp").arg(&src).arg(&path).status().unwrap();
    assert!(st.success(), "cp failed");
    std::fs::remove_file(&src).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// The text feed prints the sentences of the orchestrator's reply, one per line.
#[test]
fn the_text_feed_prints_a_reply_sentence_by_sentence() {
    let home = home_with("");
    let home = home.path();
    let _hub = start_hub(home);
    let mut feed = Proc(
        command(home, &["speech", "feed", "--text"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let out = lines(feed.0.stdout.take().unwrap());
    wait_for_speaker(home, "speech feed");
    // Two paragraphs too long to be said in one go (300 characters at most).
    let long = "and on ".repeat(25);
    let first = format!("The **first** paragraph goes {long}until it ends.");
    let second = format!("A second one goes {long}as well.");
    ok(home, &["say", &format!("{first}\n\n{second}")]);
    let line = line_with(&out, "paragraph goes");
    // Markup goes; each paragraph is a line of its own here.
    assert_eq!(line, first.replace("**", ""));
    assert_eq!(next_line(&out, "the second paragraph"), second);
}

/// The proxy says hello (its version, host and workspaces), says `up` once the hub is
/// connected, refuses a method other than `speech.attach` and `speech.state` without passing
/// it on, and passes `speech.attach` on, its answer wrapped with the workspace.
#[test]
fn the_proxy_passes_on_speech_attach_only() {
    let home = home_with("");
    let home = home.path();
    let _hub = start_hub(home);
    let mut proxy = Proc(
        command(home, &["speech", "feed"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let mut stdin = proxy.0.stdin.take().unwrap();
    let out = lines(proxy.0.stdout.take().unwrap());
    let hello: Value = serde_json::from_str(&next_line(&out, "hello")).unwrap();
    assert_eq!(hello["proxy"], 1, "{hello}");
    assert_eq!(hello["workspaces"], json!(["default"]), "{hello}");
    assert!(
        hello["host"].as_str().is_some_and(|h| !h.is_empty()),
        "{hello}"
    );
    let up: Value = serde_json::from_str(&next_line(&out, "up")).unwrap();
    assert_eq!(up, json!({"ws": "default", "up": true}));

    let mut send = |v: Value| {
        writeln!(stdin, "{v}").unwrap();
        stdin.flush().unwrap();
    };
    send(json!({"ws": "default", "id": 1, "method": "thread.list", "params": {}}));
    let refused: Value = serde_json::from_str(&next_line(&out, "the refusal")).unwrap();
    assert_eq!(refused["ws"], "default", "{refused}");
    assert_eq!(refused["msg"]["id"], 1, "{refused}");
    assert_eq!(refused["msg"]["error"]["code"], "unauthorized", "{refused}");
    assert!(refused["msg"].get("result").is_none(), "{refused}");

    send(json!({"ws": "elsewhere", "id": 2, "method": "speech.attach", "params": {}}));
    let unknown: Value = serde_json::from_str(&next_line(&out, "an unknown ws")).unwrap();
    assert_eq!(unknown["msg"]["id"], 2, "{unknown}");
    assert!(unknown["msg"]["error"].is_object(), "{unknown}");

    let params = twr_core::speech::SpeechAttachParams {
        device: "proxytest".into(),
        name: "proxy test".into(),
        engine: "text".into(),
        voice: None,
        live: true,
        read: twr_core::speech::default_read(),
        turn_from: twr_core::speech::TurnsRead::All,
        watermarks: Default::default(),
        catchup_secs: twr_core::speech::default_catchup(),
    };
    send(json!({"ws": "default", "id": 3, "method": "speech.attach", "params": params}));
    let ack: Value = serde_json::from_str(&line_with(&out, "\"id\":3")).unwrap();
    assert_eq!(ack["ws"], "default", "{ack}");
    assert!(ack["msg"]["result"].is_object(), "{ack}");
    assert!(ack["msg"].get("error").is_none(), "{ack}");
    wait_for_speaker(home, "proxy test");
}

/// Whether `child` exits within [`WAIT`] (polled), with its status.
fn exits(child: &mut Child) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if let Some(st) = child.try_wait().unwrap() {
            return Some(st);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    None
}

/// A proxy whose speaker pinged it and then went quiet (its laptop slept; its sshd session
/// lives on) ends after the idle limit, its stdin still open; one never pinged (an older
/// speaker) doesn't.
#[test]
fn the_proxy_ends_when_its_pinging_speaker_goes_quiet() {
    let home = home_with("");
    let home = home.path();
    let _hub = start_hub(home);
    let spawn = || {
        Proc(
            command(home, &["speech", "feed", "--proto"])
                .env("TWR_SPEECH_FEED_IDLE_MS", "1500")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
    };
    let mut old = spawn();
    let _old_stdin = old.0.stdin.take().unwrap();
    let old_out = lines(old.0.stdout.take().unwrap());
    next_line(&old_out, "hello");

    let mut proxy = spawn();
    let mut stdin = proxy.0.stdin.take().unwrap();
    let out = lines(proxy.0.stdout.take().unwrap());
    next_line(&out, "hello");
    line_with(&out, "\"up\"");
    let ping = json!({"ws": "", "id": 0, "method": twr_speech::link::PROXY_PING, "params": null});
    writeln!(stdin, "{ping}").unwrap();
    stdin.flush().unwrap();
    // Unanswered: the next line is the end of the output.
    let st = exits(&mut proxy.0).expect("the proxy ended after the idle limit");
    assert!(st.success(), "{st}");
    assert!(
        out.recv_timeout(Duration::from_secs(1)).is_err(),
        "no answer to a ping"
    );
    drop(stdin);
    // Never pinged: still there after the same time and more.
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        old.0.try_wait().unwrap().is_none(),
        "an unpinged proxy stays"
    );
}

/// `twr speech feed` exits once its output is gone, though its stdin stays open (a read of
/// stdin can't be cancelled; the process doesn't wait for it).
#[test]
fn the_proxy_exits_when_its_output_is_gone_with_stdin_open() {
    let home = home_with("");
    let home = home.path();
    let hub = start_hub(home);
    let mut proxy = Proc(
        command(home, &["speech", "feed"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let stdin = proxy.0.stdin.take().unwrap();
    let mut out = BufReader::new(proxy.0.stdout.take().unwrap());
    for what in ["hello", "up"] {
        let mut line = String::new();
        out.read_line(&mut line).unwrap();
        assert!(!line.is_empty(), "no {what}");
    }
    drop(out);
    // The hub goes: its `down` can't be written, so the proxy ends, while it waits on stdin.
    drop(hub);
    exits(&mut proxy.0).expect("the proxy ended with stdin open");
    drop(stdin);
}

/// `twr speak` on this machine, its `command` engine a recorder: once live reading is on (kept
/// in the device's state), the orchestrator's reply reaches the program's stdin. SIGTERM ends
/// it cleanly.
#[test]
fn twr_speak_reads_a_live_reply_to_its_engine() {
    let bin = tempfile::tempdir().unwrap();
    let said = bin.path().join("said.txt");
    let recorder = script(bin.path(), "recorder", "cat >> \"$1\"\n");
    let home = home_with(&format!(
        "[speech]\nengine = \"command\"\ncommand = [{:?}, {:?}]\n",
        recorder.to_str().unwrap(),
        said.to_str().unwrap()
    ));
    let home = home.path();
    // Live reading on for this machine's default workspace, as `/read on` would leave it.
    twr_speech::StateStore::in_data_dir(&Paths::with_home(home).root().data_dir)
        .update(|s| {
            s.live
                .insert(twr_speech::state::live_key("local", "default"), true);
        })
        .unwrap();
    let _hub = start_hub(home);
    let mut speak = Proc(
        command(home, &["speak", "--name", "test speaker"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    );
    let notes = lines(speak.0.stderr.take().unwrap());
    let start = next_line(&notes, "the start note");
    assert!(
        start.contains("reading aloud what every workspace of this machine says, with command"),
        "{start}"
    );
    // No terminal, no keys.
    assert!(!start.contains("q quit"), "{start}");
    line_with(&notes, "default: connected; live reading on");
    wait_for_speaker(home, "test speaker");
    ok(home, &["say", "Read this aloud, please."]);
    poll("the recorder's text", || {
        std::fs::read_to_string(&said)
            .ok()
            .filter(|t| t.contains("Read this aloud, please."))
    });
    // SAFETY: the speaker is this test's child, not reaped yet.
    unsafe {
        libc::kill(speak.0.id() as i32, libc::SIGTERM);
    }
    let status = poll("the speaker to stop", || speak.0.try_wait().unwrap());
    assert!(status.success(), "{status}");
}

/// `--print-service` prints the service that runs this same `twr speak`, with this twr's
/// path, and how to install it, without `--print-service`.
#[test]
fn print_service_runs_the_same_speak() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let text = ok(
        home,
        &[
            "speak",
            "--ssh",
            "me@box",
            "--name",
            "my mac",
            "--rate",
            "200",
            "--print-service",
        ],
    );
    assert!(text.contains(TWR), "{text}");
    assert!(!text.contains("--print-service"), "{text}");
    if cfg!(target_os = "linux") {
        assert!(
            text.contains(&format!(
                "ExecStart={TWR} speak --ssh me@box --name \"my mac\" --rate 200\n"
            )),
            "{text}"
        );
        assert!(text.contains("<<'TWR_UNIT'\n[Unit]\n"), "{text}");
        assert!(
            text.contains("systemctl --user enable twr-speak.service\n"),
            "{text}"
        );
        assert!(
            text.contains("systemctl --user restart twr-speak.service\n"),
            "{text}"
        );
    } else if cfg!(target_os = "macos") {
        for word in [
            TWR, "speak", "--ssh", "me@box", "--name", "my mac", "--rate", "200",
        ] {
            assert!(text.contains(&format!("<string>{word}</string>")), "{text}");
        }
        assert!(text.contains("launchctl bootstrap gui/"), "{text}");
    }
    // Nothing installed, and the home is the test's: said.
    assert!(text.starts_with("# TWR_HOME is set"), "{text}");

    // Bad values are refused before anything is printed.
    let out = twr(
        home,
        &["speak", "--ssh=-oProxyCommand=x", "--print-service"],
    );
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("can't start with `-`"),
        "{out:?}"
    );
    let out = twr(home, &["speak", "--rate", "79", "--print-service"]);
    assert!(!out.status.success());
    // A voice every hub would refuse at each attach.
    let long = "v".repeat(129);
    for voice in [long.as_str(), "a\u{7}b"] {
        let out = twr(home, &["speak", "--voice", voice, "--print-service"]);
        assert!(!out.status.success(), "{voice:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("--voice: 1-128 printable"),
            "{out:?}"
        );
    }
}

/// Fake mosh and ssh for `twr remote`, in `dir`: the session (mosh, or `ssh -t`) records its
/// arguments, waits until the speaker's ssh has started, and exits with `code`; the speaker's
/// ssh records its arguments and the speaker's pid (its parent) and its own, then waits.
fn fake_session(dir: &Path, code: i32) -> (PathBuf, PathBuf) {
    let d = dir.to_str().unwrap();
    let session = format!(
        "printf '%s\\n' \"$@\" > {d}/session.args\n\
         i=0; while [ ! -f {d}/speaker.pids ] && [ $i -lt 200 ]; do sleep 0.05; i=$((i+1)); done\n\
         exit {code}\n"
    );
    let mosh = script(dir, "mosh", &session);
    let ssh = script(
        dir,
        "ssh",
        &format!(
            "if [ \"$1\" = -t ]; then\n{session}fi\n\
             printf '%s\\n' \"$@\" > {d}/speaker-ssh.args\n\
             echo \"$PPID $$\" > {d}/speaker.pids.tmp && mv {d}/speaker.pids.tmp {d}/speaker.pids\n\
             exec sleep 60\n"
        ),
    );
    (mosh, ssh)
}

/// Whether process `pid` has ended (gone, or a zombie nobody reaped yet).
fn ended(pid: &str) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Err(_) => true,
        Ok(stat) => stat
            .rsplit_once(") ")
            .is_some_and(|(_, rest)| rest.starts_with('Z')),
    }
}

/// `twr remote` runs the session with the remote twr's arguments (mosh's as they are, ssh's as
/// shell words), the speaker beside it (`twr speak --ssh HOST` over ssh, its log private), and
/// stops the speaker once the session ends, exiting with the session's code.
#[test]
fn twr_remote_runs_the_session_and_stops_its_speaker() {
    let bin = tempfile::tempdir().unwrap();
    let recorder = script(bin.path(), "recorder", "cat > /dev/null\n");
    let home = home_with(&format!(
        "[speech]\nengine = \"command\"\ncommand = [{:?}]\n",
        recorder.to_str().unwrap()
    ));
    let home = home.path();
    let args = ["thread", "list", "it's mine"];
    for over_ssh in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (mosh, ssh) = fake_session(dir.path(), if over_ssh { 0 } else { 3 });
        let mut words = vec!["remote", "me@box", "--name", "laptop"];
        if over_ssh {
            words.push("--ssh");
        }
        words.push("--");
        words.extend(args);
        let out = command(home, &words)
            .env("TWR_MOSH", &mosh)
            .env("TWR_SSH", &ssh)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(
            out.status.code(),
            Some(if over_ssh { 0 } else { 3 }),
            "{stderr}"
        );
        let session = std::fs::read_to_string(dir.path().join("session.args")).unwrap();
        let expected: Vec<String> = if over_ssh {
            ["-t", "me@box", "--", "twr"]
                .into_iter()
                .map(String::from)
                .chain(args.iter().map(|a| twr_core::paths::sh_word(a)))
                .collect()
        } else {
            ["me@box", "--", "twr"]
                .into_iter()
                .chain(args)
                .map(String::from)
                .collect()
        };
        assert_eq!(session.lines().collect::<Vec<_>>(), expected);
        // The speaker's ssh ran the feed on every workspace there, with no forwarding.
        let feed = std::fs::read_to_string(dir.path().join("speaker-ssh.args")).unwrap();
        let feed: Vec<&str> = feed.lines().collect();
        assert!(
            feed.contains(&"-T") && feed.contains(&"ForwardAgent=no"),
            "{feed:?}"
        );
        assert!(
            feed.ends_with(&[
                "me@box",
                "--",
                "twr",
                "speech",
                "feed",
                "--proto",
                "--all-workspaces"
            ]),
            "{feed:?}"
        );
        // The speaker and what it started are gone.
        let pids = std::fs::read_to_string(dir.path().join("speaker.pids")).unwrap();
        let (speaker, its_ssh) = pids.trim().split_once(' ').unwrap();
        poll("the speaker to end", || ended(speaker).then_some(()));
        poll("the speaker's ssh to end", || ended(its_ssh).then_some(()));
        assert!(!stderr.contains("the speaker stopped"), "{stderr}");
    }
    let log = Paths::with_home(home).root().data_dir.join("speak.log");
    let mode =
        std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&log).unwrap().permissions());
    assert_eq!(mode & 0o777, 0o600);
    let logged = std::fs::read_to_string(&log).unwrap();
    assert!(
        logged.contains("twr remote me@box: starting the speaker"),
        "{logged}"
    );
    assert!(
        logged.contains("twr speak: reading aloud what every workspace of me@box says"),
        "{logged}"
    );
}

/// Without mosh, `twr remote` says to use `--ssh`, and starts nothing.
#[test]
fn twr_remote_without_mosh_says_use_ssh() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let out = command(home, &["remote", "me@box"])
        .env("TWR_MOSH", home.join("no-mosh-here"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("mosh not found; use --ssh"), "{stderr}");
    assert!(!Paths::with_home(home)
        .root()
        .data_dir
        .join("speak.log")
        .exists());
    let out = command(home, &["remote", "me box"]).output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("not a host"), "{stderr}");
}
