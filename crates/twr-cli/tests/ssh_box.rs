//! End to end: an SSH box set up with `twr box add`, through a fake `ssh`. A real `twr hub`
//! (with `TWR_HOME` in a tempdir) reaches the "box" with the `ssh` that `TWR_SSH` names: a sh
//! script the test writes, which takes ssh's options and destination, requires `--` before
//! the remote command, logs each run, and runs the command with `sh -c` in a fake box
//! environment (`HOME` a directory of the test's, no `XDG_*`, `TWR_BOX_LAUNCHER=setsid` so the
//! relay starts the daemon in a session of its own, never with `systemd-run`, and
//! `TWR_ENABLE_FAKE=1` for the fake runtime, which the threads run). The box is this machine,
//! so the hub uploads this build of twr into the fake home, and the box daemon runs from
//! there.
//!
//! Linux only: the test finds the box's processes in `/proc`.
#![cfg(target_os = "linux")]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use twr_core::paths::Paths;
use twr_core::testkit::git::init_repo;

const TWR: &str = env!("CARGO_BIN_EXE_twr");

/// Long enough for an upload of a debug build of twr and the box coming online.
const WAIT: Duration = Duration::from_secs(90);

/// A user token this test process's hubs and clients share (`TWR_TEST_USER_TOKEN_FILE`), as in
/// `cli.rs`: run from inside a twr thread, the clients would otherwise count as its agent.
fn test_token() -> &'static Path {
    static TOKEN: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    TOKEN.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap().keep();
        let path = dir.join("user.token");
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        use std::io::Write;
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

/// The test's twr home, its fake box and its fake ssh.
struct Setup {
    _dir: tempfile::TempDir,
    /// `TWR_HOME`.
    home: PathBuf,
    /// The box's `HOME`.
    box_home: PathBuf,
    /// The fake ssh.
    ssh: PathBuf,
    /// One line per run of the fake ssh: its pid, the destination and the remote command
    /// (newlines and tabs as spaces).
    log: PathBuf,
}

impl Setup {
    fn new() -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let box_home = dir.path().join("box");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&box_home).unwrap();
        let ssh = dir.path().join("fake-ssh");
        let log = dir.path().join("ssh.log");
        let path = std::env::var("PATH").unwrap_or_default();
        for p in [&box_home, &log] {
            assert!(!p.to_str().unwrap().contains('\''));
        }
        assert!(!path.contains('\''));
        std::fs::write(
            &ssh,
            format!(
                r#"#!/bin/sh
# A fake ssh for twr's tests: ssh's options are taken and ignored, then the destination, then
# `--` and the remote command, which runs with `sh -c` in the fake box's environment.
log='{log}'
dest=
dashdash=
while [ $# -gt 0 ]; do
  case $1 in
    --) dashdash=1; shift; break ;;
    -T|-t|-n|-q|-v|-a|-x|-k) shift ;;
    -o|-p|-l|-i|-F) [ $# -ge 2 ] || {{ echo "fake ssh: $1 needs a value" >&2; exit 255; }}; shift 2 ;;
    -o?*|-p?*|-l?*|-i?*|-F?*) shift ;;
    -*) echo "fake ssh: unknown option $1" >&2; exit 255 ;;
    *) [ -z "$dest" ] || {{ echo "fake ssh: a second destination $1 (the remote command must follow --)" >&2; exit 255; }}
       dest=$1; shift ;;
  esac
done
[ -n "$dest" ] || {{ echo "fake ssh: no destination" >&2; exit 255; }}
[ -n "$dashdash" ] && [ $# -gt 0 ] || {{ echo "fake ssh: no -- before the remote command" >&2; exit 255; }}
cmd="$*"
printf '%s\t%s\t%s\n' "$$" "$dest" "$(printf '%s' "$cmd" | tr '\n\t' '  ')" >> "$log"
cd '{box_home}' || exit 255
exec env -i HOME='{box_home}' PATH='{path}' TWR_BOX_LAUNCHER=setsid TWR_ENABLE_FAKE=1 sh -c "$cmd"
"#,
                log = log.display(),
                box_home = box_home.display(),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
        Setup {
            _dir: dir,
            home,
            box_home,
            ssh,
            log,
        }
    }

    fn paths(&self) -> Paths {
        Paths::with_home(&self.home)
    }

    /// `twr` with this setup's environment (the hub it starts inherits it).
    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(TWR);
        c.args(args)
            .env("TWR_HOME", &self.home)
            .env("TWR_SSH", &self.ssh)
            // The threads run the fake runtime, which the hub offers only on request.
            .env("TWR_ENABLE_FAKE", "1")
            .env("TWR_TEST_USER_TOKEN_FILE", test_token())
            .env_remove("TWR_WORKSPACE")
            .env_remove("RUST_LOG")
            .stdin(Stdio::null());
        c
    }

    fn twr(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("running twr")
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.twr(args);
        assert!(
            out.status.success(),
            "twr {args:?} failed: {}\n{}",
            String::from_utf8_lossy(&out.stderr),
            String::from_utf8_lossy(&out.stdout)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    fn fails(&self, args: &[&str]) -> String {
        let out = self.twr(args);
        assert!(!out.status.success(), "twr {args:?} succeeded");
        String::from_utf8(out.stderr).unwrap()
    }

    /// `twr hub` in the foreground, once it answers.
    fn start_hub(&self, n: usize) -> HubProc {
        let log = std::fs::File::create(self.home.join(format!("hub-{n}.log"))).unwrap();
        let hub = HubProc(
            self.command(&["hub"])
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .unwrap(),
        );
        poll("the hub socket", || {
            self.twr(&["thread", "list"]).status.success().then_some(())
        });
        hub
    }

    /// The fake ssh's runs so far: (pid, destination, remote command).
    fn ssh_runs(&self) -> Vec<(u32, String, String)> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let mut f = l.splitn(3, '\t');
                Some((
                    f.next()?.parse().ok()?,
                    f.next()?.to_string(),
                    f.next()?.to_string(),
                ))
            })
            .collect()
    }

    /// The runs of the relay the hub connects through (not `--stop`).
    fn relays(&self) -> Vec<u32> {
        self.ssh_runs()
            .into_iter()
            .filter(|(_, _, c)| c.contains(" runner --stdio ") && !c.contains("--stop"))
            .map(|(pid, _, _)| pid)
            .collect()
    }

    /// `hub.info`'s entry of box `name` (`twr box status --json`).
    fn box_status(&self, name: &str) -> Value {
        serde_json::from_str(&self.ok(&["box", "status", name, "--json"])).unwrap()
    }

    fn events(&self, thread: &str) -> Vec<Value> {
        self.ok(&["events", thread])
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn count(&self, thread: &str, kind: &str) -> usize {
        self.events(thread)
            .iter()
            .filter(|e| e["kind"] == kind)
            .count()
    }

    /// Sends `prompt` to `thread` and waits for its `n`th report.
    fn turn(&self, thread: &str, prompt: &str, n: usize) {
        self.ok(&["send", thread, prompt]);
        poll(&format!("report {n} of {thread}"), || {
            (self.count(thread, "report.ready") >= n).then_some(())
        });
    }
}

struct HubProc(Child);

impl HubProc {
    /// Waits for the hub to exit (after `twr hub stop`).
    fn exited(mut self) {
        let deadline = Instant::now() + WAIT;
        while self.0.try_wait().unwrap().is_none() {
            assert!(Instant::now() < deadline, "the hub did not exit");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for HubProc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Retries `f` until it returns Some or [`WAIT`] passes.
fn poll<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The command lines of the processes whose arguments mention `needle`, by pid.
fn processes_mentioning(needle: &str) -> Vec<(u32, Vec<String>)> {
    let mut out = Vec::new();
    for e in std::fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|p| p.parse::<u32>().ok()) else {
            continue;
        };
        let Ok(raw) = std::fs::read(e.path().join("cmdline")) else {
            continue;
        };
        let args: Vec<String> = raw
            .split(|b| *b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect();
        if args.iter().any(|a| a.contains(needle)) {
            out.push((pid, args));
        }
    }
    out
}

/// The box daemons running from the fake box (`twr runner --daemon` of a build under it).
fn daemons(box_home: &Path) -> Vec<u32> {
    processes_mentioning(box_home.to_str().unwrap())
        .into_iter()
        .filter(|(_, args)| args.iter().any(|a| a == "--daemon"))
        .map(|(pid, _)| pid)
        .collect()
}

/// Kills, when dropped, every process whose arguments mention the fake box (its daemons and
/// relays): a failed test leaves none behind.
struct BoxProcesses(PathBuf);

impl Drop for BoxProcesses {
    fn drop(&mut self) {
        for (pid, _) in processes_mentioning(self.0.to_str().unwrap()) {
            // SAFETY: kill(2) of a pid; no memory is passed.
            unsafe {
                libc_kill(pid as i32, 9);
            }
        }
    }
}

extern "C" {
    #[link_name = "kill"]
    fn libc_kill(pid: i32, sig: i32) -> i32;
}

/// `box.add` straight to the hub on `socket`, as the user: its error code.
fn box_add_error(socket: &Path, name: &str) -> String {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut c = twr_core::client::Client::connect(socket).await.unwrap();
        let token = std::fs::read_to_string(test_token()).unwrap();
        c.hello(json!({"client": "test", "user_token": token}))
            .await
            .unwrap();
        let e = c
            .call("box.add", json!({ "name": name }))
            .await
            .unwrap_err();
        e.downcast_ref::<twr_core::proto::RpcError>()
            .map(|r| r.code.to_string())
            .unwrap_or_else(|| format!("{e:#}"))
    })
}

/// `twr box add` sets up the box: the hub uploads its own build into the box's builds
/// directory, starts the box daemon there and comes online; `twr box list` and `status` show
/// it, and a thread on the box runs turns. The hub's ssh killed, the same daemon and the
/// thread's runtime run on and the box comes back; a hub restart reconnects to that daemon
/// and the thread goes on. Adding the box again is refused, `box.add` of a box in neither
/// file is `not_found`, `twr box remove` takes it out of boxes.toml, and the relay's `--stop`
/// (through ssh, as `remove` prints it) stops the daemon.
#[test]
fn an_ssh_box_from_add_to_remove() {
    let t = Setup::new();
    let _cleanup = BoxProcesses(t.box_home.clone());
    let paths = t.paths();
    let mut hub = Some(t.start_hub(1));

    let out = t.ok(&["box", "add", "gpu", "fake@box", "--yes"]);
    assert!(out.contains("box gpu: linux/"), "{out}");
    assert!(out.contains("The hub installs its own build"), "{out}");
    assert!(out.contains("box gpu is online: twr "), "{out}");
    assert!(
        out.contains("start a thread on it: twr thread new <project> --box gpu"),
        "{out}"
    );
    let boxes = std::fs::read_to_string(paths.boxes_file()).unwrap();
    assert!(boxes.contains("[boxes.gpu]"), "{boxes}");
    assert!(boxes.contains("transport = \"ssh\""), "{boxes}");
    assert!(boxes.contains("ssh = \"fake@box\""), "{boxes}");
    // The hub's own build, uploaded into the box's builds directory.
    let id = twr_core::build_id_of(Path::new(TWR)).unwrap();
    let exe = t
        .box_home
        .join(".local/share/twr/runner/builds")
        .join(&id)
        .join("twr");
    let meta = std::fs::metadata(&exe).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o700);
    assert_eq!(meta.len(), std::fs::metadata(TWR).unwrap().len());
    assert_eq!(twr_core::build_id_of(&exe).unwrap(), id);
    // The ssh the probes ran in the user's terminal, then the hub's own.
    let runs = t.ssh_runs();
    assert!(
        runs.iter().all(|(_, dest, _)| dest == "fake@box"),
        "{runs:?}"
    );
    assert!(
        runs.iter().any(|(_, _, c)| c.contains("twr-probe 1")),
        "{runs:?}"
    );
    assert!(runs.iter().any(|(_, _, c)| c == "true"), "{runs:?}");
    assert!(
        runs.iter().any(|(_, _, c)| c.contains(".twr.upload.")),
        "{runs:?}"
    );
    assert_eq!(t.relays().len(), 1, "{runs:?}");

    let list = t.ok(&["box", "list"]);
    let row = list.lines().find(|l| l.starts_with("gpu ")).unwrap();
    let cells: Vec<&str> = row.split_whitespace().collect();
    assert_eq!(cells[1..3], ["ssh", "online"], "{list}");
    assert!(cells[4].contains('/'), "{list}");
    let status = t.box_status("gpu");
    assert_eq!(status["state"], "online", "{status}");
    assert_eq!(status["transport"], "ssh", "{status}");
    assert_eq!(status["build"], id.as_str(), "{status}");
    assert_eq!(status["os"], "Linux", "{status}");
    assert_eq!(status["switch_pending"], false, "{status}");
    let text = t.ok(&["box", "status", "gpu"]);
    assert!(
        text.contains(&format!("build:          {id} (launched by the hub)")),
        "{text}"
    );
    assert!(text.contains("started the box daemon"), "{text}");

    // A thread on the box, in a repository there.
    let repo = init_repo(&t.box_home.join("repo"));
    let created = t.ok(&[
        "thread",
        "new",
        repo.to_str().unwrap(),
        "--box",
        "gpu",
        "--runtime",
        "fake",
        "--name",
        "remote",
    ]);
    assert!(created.starts_with("created remote ("), "{created}");
    t.turn("remote", "hello from the box", 1);
    let daemon = daemons(&t.box_home);
    assert_eq!(daemon.len(), 1, "one box daemon: {daemon:?}");

    // The hub's ssh dies: the daemon and the thread's runtime run on; the hub connects again.
    let relay = *t.relays().last().unwrap();
    // SAFETY: kill(2) of the fake ssh's pid (the relay, which it became).
    assert_eq!(unsafe { libc_kill(relay as i32, 9) }, 0);
    poll("a new connection", || (t.relays().len() >= 2).then_some(()));
    poll("the box back online", || {
        (t.box_status("gpu")["state"] == "online").then_some(())
    });
    assert_eq!(daemons(&t.box_home), daemon, "the same daemon");
    t.turn("remote", "after the reconnection", 2);

    // A hub restart: the new hub reconnects to the same daemon, and the thread goes on.
    t.ok(&["hub", "stop"]);
    hub.take().unwrap().exited();
    assert_eq!(daemons(&t.box_home), daemon, "the daemon outlives the hub");
    hub = Some(t.start_hub(2));
    poll("the box online after the restart", || {
        (t.box_status("gpu")["state"] == "online").then_some(())
    });
    assert_eq!(daemons(&t.box_home), daemon, "the same daemon");
    t.turn("remote", "after the restart", 3);
    // One runtime all along: started once, never crashed or stopped.
    assert_eq!(t.count("remote", "thread.started"), 1);
    assert_eq!(t.count("remote", "thread.crashed"), 0);
    assert_eq!(t.count("remote", "thread.stopped"), 0);

    // Added once only; `box.add` of a box in neither file is `not_found`.
    let e = t.fails(&["box", "add", "gpu", "fake@box", "--yes"]);
    assert!(e.contains("there is a box gpu already"), "{e}");
    let socket = paths.socket.clone();
    assert_eq!(box_add_error(&socket, "nope"), "not_found");

    // Removed from boxes.toml; the hub keeps it until it restarts.
    let out = t.ok(&["box", "remove", "gpu"]);
    assert!(out.contains("removed box gpu from"), "{out}");
    assert!(out.contains("keeps the box until it restarts"), "{out}");
    let boxes = std::fs::read_to_string(paths.boxes_file()).unwrap();
    assert!(!boxes.contains("gpu"), "{boxes}");
    let stop = out
        .lines()
        .find(|l| l.contains("--stop"))
        .unwrap_or_else(|| panic!("no stop command: {out}"))
        .trim()
        .to_string();
    // The hub's interactive ssh (its forced options; `TWR_SSH` here), each word quoted.
    assert!(
        stop.starts_with(&format!("{} -T -o ConnectTimeout=20 ", t.ssh.display())),
        "{stop}"
    );
    assert!(stop.contains(" fake@box -a -x -k -- 'sh -c "), "{stop}");
    assert!(
        stop.contains(&format!("/twr/runner/builds/{id}/twr")),
        "{stop}"
    );
    // As the user pastes it.
    let run_stop = || {
        Command::new("sh")
            .arg("-c")
            .arg(&stop)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };

    // With the hub stopped (it would connect again), the relay's --stop ends the daemon.
    t.ok(&["hub", "stop"]);
    hub.take().unwrap().exited();
    let out = run_stop();
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{said}");
    assert!(said.contains("twr-box: stopped the box daemon"), "{said}");
    poll("no box daemon left", || {
        daemons(&t.box_home).is_empty().then_some(())
    });
    // Again: nothing runs, and that is no error.
    let out = run_stop();
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{said}");
    assert!(
        said.contains("twr-box: no box daemon runs for box gpu"),
        "{said}"
    );
}

/// Stops the hub `twr` started in the background for a setup, when dropped: with a signal,
/// as its clients may count as an agent to it (see [`Setup::detached`]), and waits until its
/// process has ended.
struct StopsHub<'a>(&'a Setup);

impl Drop for StopsHub<'_> {
    fn drop(&mut self) {
        let out = self.0.twr(&["info", "--json"]);
        let Ok(info) = serde_json::from_slice::<Value>(&out.stdout) else {
            return;
        };
        if let Some(pid) = info["pid"].as_u64() {
            // SAFETY: kill(2) of a pid; no memory is passed.
            unsafe {
                libc_kill(pid as i32, 15);
            }
            // Gone means its process is gone (not only its socket, which it removes first).
            let deadline = Instant::now() + WAIT;
            while running(pid as u32) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

/// Whether process `pid` runs (exists and is no zombie).
fn running(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| {
            s.rsplit_once(')')
                .map(|(_, r)| !r.trim_start().starts_with('Z'))
        })
        .unwrap_or(false)
}

impl Setup {
    /// `twr args` as you would run it from your own terminal: outside this test's process tree
    /// (`setsid -f`, so it is not this test's descendant) and without `TWR_AGENT`. A hub that
    /// `twr` starts trusts no test user token (`spawn_hub` drops it), so from inside a twr
    /// thread the test's own clients are that thread's agent to such a hub. Whether it
    /// succeeded, and its stdout and stderr.
    fn detached(&self, args: &[&str]) -> (bool, String) {
        let out = self.home.join("detached.out");
        let status = self.home.join("detached.status");
        let _ = std::fs::remove_file(&status);
        let mut c = Command::new("setsid");
        c.args([
            "-f",
            "sh",
            "-c",
            r#""$0" "$@" > "$TWR_TEST_OUT" 2>&1; echo $? > "$TWR_TEST_STATUS""#,
        ])
        .arg(TWR)
        .args(args)
        .env("TWR_TEST_OUT", &out)
        .env("TWR_TEST_STATUS", &status)
        .env("TWR_HOME", &self.home)
        .env("TWR_SSH", &self.ssh)
        .env("TWR_ENABLE_FAKE", "1")
        .env_remove("TWR_AGENT")
        .env_remove("TWR_TEST_USER_TOKEN_FILE")
        .env_remove("TWR_WORKSPACE")
        .env_remove("RUST_LOG")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
        assert!(c.status().unwrap().success(), "setsid -f");
        let code = poll(&format!("twr {args:?}"), || {
            std::fs::read_to_string(&status)
                .ok()
                .filter(|s| s.ends_with('\n'))
        });
        (
            code.trim() == "0",
            std::fs::read_to_string(&out).unwrap_or_default(),
        )
    }
}

/// `twr box add` with no hub running: it writes boxes.toml and starts the hub, which adds the
/// box from the file as it starts; the add's own `box.add` then finds it there (a conflict
/// that is no failure: the hub's box is an SSH box of that name), and it waits for the box
/// online as usual.
#[test]
fn adding_a_box_starts_the_hub_when_none_runs() {
    let t = Setup::new();
    let _cleanup = BoxProcesses(t.box_home.clone());
    let _hub = StopsHub(&t);
    let (ok, out) = t.detached(&["box", "add", "gpu", "fake@box", "--yes"]);
    assert!(ok, "{out}");
    assert!(out.contains("the hub has box gpu from "), "{out}");
    assert!(out.contains("box gpu is online: twr "), "{out}");
    let status = t.box_status("gpu");
    assert_eq!(status["state"], "online", "{status}");
    assert_eq!(status["transport"], "ssh", "{status}");
    assert_eq!(t.relays().len(), 1, "{:?}", t.ssh_runs());
}

/// Without --yes and without a terminal, `twr box add` refuses before it runs ssh or writes
/// anything.
#[test]
fn adding_a_box_needs_a_terminal_or_yes() {
    let t = Setup::new();
    let e = t.fails(&["box", "add", "gpu", "fake@box"]);
    assert!(e.contains("not adding box gpu without confirmation"), "{e}");
    assert!(e.contains("pass --yes"), "{e}");
    assert!(!t.paths().boxes_file().exists());
    assert!(t.ssh_runs().is_empty(), "{:?}", t.ssh_runs());
    // A name that is no box name, and `local`, are refused too.
    let e = t.fails(&["box", "add", "local", "fake@box", "--yes"]);
    assert!(e.contains("the hub's own box"), "{e}");
    let e = t.fails(&["box", "add", ".x", "fake@box", "--yes"]);
    assert!(e.contains("invalid box name"), "{e}");
    assert!(!t.paths().boxes_file().exists());
    // `install` checks the name first too.
    let e = t.fails(&["box", "install", ".x", "--yes"]);
    assert!(e.contains("invalid box name"), "{e}");
    assert!(t.ssh_runs().is_empty(), "{:?}", t.ssh_runs());
}

/// `twr box remove` repairs config files twr cannot load: a box in both config.toml and
/// boxes.toml (the error says to run it) comes out of boxes.toml, and so does a box whose
/// settings twr refuses, the other tables kept; the config loads again. No hub runs.
#[test]
fn box_remove_repairs_boxes_toml() {
    let t = Setup::new();
    let paths = t.paths();
    std::fs::write(
        &paths.config_file,
        "[boxes.gpu]\ntoken = \"0123456789abcdef0123456789abcdef\"\n",
    )
    .unwrap();
    std::fs::write(
        paths.boxes_file(),
        "[boxes.gpu]\ntransport = \"ssh\"\nssh = \"me@gpu\"\n\n\
         [boxes.ok]\ntransport = \"ssh\"\nssh = \"me@ok\"\n\n\
         [boxes.ok.projects.api]\nrepo = \"/srv/api\"\n",
    )
    .unwrap();
    let e = t.fails(&["box", "list"]);
    assert!(e.contains("`twr box remove gpu` removes it"), "{e}");
    let out = t.ok(&["box", "remove", "gpu"]);
    assert!(out.contains("removed box gpu from"), "{out}");
    // A box whose settings twr refuses (another twr version's, say).
    let mut text = std::fs::read_to_string(paths.boxes_file()).unwrap();
    text.push_str("\n[boxes.bad]\ntransport = \"tcp\"\nnot_a_key = 1\n");
    std::fs::write(paths.boxes_file(), text).unwrap();
    let e = t.fails(&["box", "list"]);
    assert!(e.contains("parsing") || e.contains("bad"), "{e}");
    let out = t.ok(&["box", "remove", "bad"]);
    assert!(out.contains("removed box bad from"), "{out}");
    assert!(out.contains("settings were not valid"), "{out}");
    let config = twr_core::config::Config::load_with_boxes(&paths.config_file).unwrap();
    assert_eq!(config.boxes["gpu"].token.as_deref().map(str::len), Some(32));
    assert_eq!(config.boxes["ok"].ssh.as_deref(), Some("me@ok"));
    assert_eq!(
        config.boxes["ok"].projects["api"].repo,
        std::path::PathBuf::from("/srv/api")
    );
    assert!(!config.boxes.contains_key("bad"));
    // One in config.toml only: twr does not edit that file.
    let e = t.fails(&["box", "remove", "gpu"]);
    assert!(e.contains("which twr does not edit"), "{e}");
    let e = t.fails(&["box", "remove", "nope"]);
    assert!(e.contains("there is no box nope"), "{e}");
}
