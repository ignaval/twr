//! End to end: an SSH box set up with `twr box add`, through a fake `ssh`. A real `twr hub`
//! (with `TWR_HOME` in a tempdir) reaches the "box" with the `ssh` that `TWR_SSH` names: a sh
//! script the test writes, which takes ssh's options and destination, requires `--` before
//! the remote command, logs each run, and runs the command with `sh -c` in a fake box
//! environment (`HOME` a directory of the test's, no `XDG_*`, `TWR_BOX_LAUNCHER=setsid` so the
//! relay starts the daemon in a session of its own, never with `systemd-run`, and
//! `TWR_ENABLE_FAKE=1` for the fake runtime, which the threads run; `PATH` is this test's
//! without the directories that hold an agent CLI (`claude`, `codex`, `grok`), after the box's
//! own `bin`, where a test puts fake ones). A destination `down@...` fails as an unreachable
//! host does. With `-t` and a terminal on its stdin, it runs the command with a terminal of its
//! own (`script`), as ssh does, its output on the fake ssh's stdout; `TWR_TEST_CLONE_ALLOW_FILE`
//! reaches the box when set. The box is this machine, so the hub uploads this build of twr
//! into the fake home, and the box daemon runs from there.
//!
//! A command box (`transport = "command"`, a container) goes through a fake `docker exec -i`
//! instead ([`Setup::exec`]): its first argument is the container (only `box` exists; another
//! fails as docker says), and it runs the rest as it is, with no shell around it, in the same
//! fake box environment; it logs each run as the fake ssh does. Its variants give the box no
//! input (`docker exec` without `-i`) or a terminal (`-t`, through `script`).
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

/// Copies `from` to `to` with `cp`, so this process never holds `to` open for writing: a child
/// another test thread forks while this process writes the file would inherit the descriptor,
/// and exec'ing the file would fail with ETXTBSY until that child execs or exits.
fn cp(from: &Path, to: &Path) {
    let st = Command::new("cp").arg(from).arg(to).status().unwrap();
    assert!(st.success(), "cp failed");
}

/// Creates the executable `path` (mode 755, its directories made) holding `content`, without
/// this process ever holding it open for writing: the bytes go to a side file, `cp` writes
/// `path` ([`cp`]: why), and the side file is removed.
fn install(path: &Path, content: impl AsRef<[u8]>) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut src = path.as_os_str().to_owned();
    src.push(".src");
    std::fs::write(&src, content).unwrap();
    cp(Path::new(&src), path);
    std::fs::remove_file(&src).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// The test's twr home, its fake box and its fake ssh.
struct Setup {
    _dir: tempfile::TempDir,
    /// `TWR_HOME`.
    home: PathBuf,
    /// The box's `HOME`.
    box_home: PathBuf,
    /// The `HOME` of `twr box project` ([`Setup::project`]): the hub machine's home, where
    /// `project_roots` are.
    hub_home: PathBuf,
    /// The fake ssh.
    ssh: PathBuf,
    /// One line per run of the fake ssh: its pid, the destination and the remote command
    /// (newlines and tabs as spaces).
    log: PathBuf,
    /// The fake `docker exec -i`: `<exec> box <argv...>` runs the argv in the fake box.
    exec: PathBuf,
    /// The same without passing its input on (`docker exec` without `-i`).
    exec_noinput: PathBuf,
    /// The same giving the box a terminal (`docker exec -i -t`, through `script`).
    exec_tty: PathBuf,
    /// One line per run of any of them: its pid, the container and the rest of its argv
    /// (newlines and tabs as spaces).
    exec_log: PathBuf,
}

impl Setup {
    fn new() -> Setup {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let box_home = dir.path().join("box");
        let hub_home = dir.path().join("hubhome");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&box_home).unwrap();
        std::fs::create_dir_all(&hub_home).unwrap();
        let ssh = dir.path().join("fake-ssh");
        let log = dir.path().join("ssh.log");
        // The box finds only the agent CLIs a test puts in its `bin`.
        let agents = ["claude", "codex", "grok"];
        let kept: Vec<PathBuf> =
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .filter(|d| !agents.iter().any(|a| d.join(a).exists()))
                .collect();
        for tool in ["sh", "git", "setsid"] {
            assert!(
                kept.iter().any(|d| d.join(tool).exists()),
                "no {tool} on PATH once the agent CLIs' directories are left out"
            );
        }
        let path = std::env::join_paths(std::iter::once(box_home.join("bin")).chain(kept))
            .unwrap()
            .into_string()
            .unwrap();
        for p in [&box_home, &log] {
            assert!(!p.to_str().unwrap().contains('\''));
        }
        assert!(!path.contains('\''));
        install(
            &ssh,
            format!(
                r#"#!/bin/sh
# A fake ssh for twr's tests: ssh's options are taken and ignored, then the destination, then
# `--` and the remote command, which runs with `sh -c` in the fake box's environment.
log='{log}'
dest=
dashdash=
tty=
while [ $# -gt 0 ]; do
  case $1 in
    --) dashdash=1; shift; break ;;
    -t) tty=1; shift ;;
    -T|-n|-q|-v|-a|-x|-k) shift ;;
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
case $dest in down@*) echo "ssh: connect to host ${{dest#*@}} port 22: Connection refused" >&2; exit 255 ;; esac
cd '{box_home}' || exit 255
set -- env -i HOME='{box_home}' PATH='{path}' TWR_BOX_LAUNCHER=setsid TWR_ENABLE_FAKE=1 \
  ${{TWR_TEST_CLONE_ALLOW_FILE:+TWR_TEST_CLONE_ALLOW_FILE=$TWR_TEST_CLONE_ALLOW_FILE}}
# `-t` with a terminal here: the command gets a terminal of its own, as ssh gives it one.
if [ -n "$tty" ] && [ -t 0 ]; then exec "$@" script -qec "$cmd" /dev/null; fi
exec "$@" sh -c "$cmd"
"#,
                log = log.display(),
                box_home = box_home.display(),
            ),
        );
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
        let exec_log = dir.path().join("exec.log");
        let env = format!(
            "env -i HOME='{}' PATH='{path}' TWR_BOX_LAUNCHER=setsid TWR_ENABLE_FAKE=1",
            box_home.display()
        );
        let fake_exec = |file: &str, run: String| {
            let file = dir.path().join(file);
            install(
                &file,
                format!(
                    r#"#!/bin/sh
# A fake `docker exec -i` for twr's tests: `$1` is the container (only `box` exists), the rest
# the command to run there, as it is (no shell around it), in the fake box's environment.
printf '%s\t%s\t%s\n' "$$" "$1" "$(shift; printf '%s ' "$@" | tr '\n\t' '  ')" >> '{log}'
[ "$1" = box ] || {{ echo "Error response from daemon: No such container: $1" >&2; exit 1; }}
shift
cd '{box_home}' || exit 1
{run}
"#,
                    log = exec_log.display(),
                    box_home = box_home.display(),
                ),
            );
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o700)).unwrap();
            file
        };
        let exec = fake_exec("fake-exec", format!("exec {env} \"$@\""));
        let exec_noinput = fake_exec("fake-exec-noinput", format!("exec {env} \"$@\" </dev/null"));
        // (`sh -c <line>`: the line, run with a terminal of its own, as `-t` gives one.)
        let exec_tty = fake_exec(
            "fake-exec-tty",
            format!("exec {env} script -qec \"$3\" /dev/null"),
        );
        Setup {
            _dir: dir,
            home,
            box_home,
            hub_home,
            ssh,
            log,
            exec,
            exec_noinput,
            exec_tty,
            exec_log,
        }
    }

    fn paths(&self) -> Paths {
        Paths::with_home(&self.home)
    }

    /// Puts a fake agent CLI `name` on the box's PATH: a sh script with `body`.
    fn box_agent(&self, name: &str, body: &str) {
        let file = self.box_home.join("bin").join(name);
        install(&file, format!("#!/bin/sh\n{body}"));
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o700)).unwrap();
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
            // Nothing of the user's agent overrides: the hub's own agent probe and what the
            // tests' commands run never pick another program for them.
            .env_remove("TWR_CLAUDE_BIN")
            .env_remove("TWR_CODEX_BIN")
            .env_remove("TWR_GROK_BIN")
            .env("TWR_AGENT_PROBE", "off")
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
        runs(&self.log)
    }

    /// The fake exec's runs so far: (pid, container, the rest of its argv).
    fn exec_runs(&self) -> Vec<(u32, String, String)> {
        runs(&self.exec_log)
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

/// The runs logged in `log` (the fake ssh's, the fake exec's): (pid, destination or container,
/// the rest).
fn runs(log: &Path) -> Vec<(u32, String, String)> {
    std::fs::read_to_string(log)
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
        let Some(args) = cmdline(pid) else {
            continue;
        };
        if args.iter().any(|a| a.contains(needle)) {
            out.push((pid, args));
        }
    }
    out
}

/// The arguments of process `pid` (`/proc/<pid>/cmdline`), when it runs and they can be read.
fn cmdline(pid: u32) -> Option<Vec<String>> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(
        raw.split(|b| *b == 0)
            .filter(|a| !a.is_empty())
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect(),
    )
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
        // Nothing of the user's agent overrides: the hub's own agent probe and what the
        // tests' commands run never pick another program for them.
        .env_remove("TWR_CLAUDE_BIN")
        .env_remove("TWR_CODEX_BIN")
        .env_remove("TWR_GROK_BIN")
        .env("TWR_AGENT_PROBE", "off")
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
    // As the user: this test's own clients may be a thread's agent to that hub, which sees a
    // box's state and agents only.
    let (ok, out) = t.detached(&["box", "status", "gpu", "--json"]);
    assert!(ok, "{out}");
    let status: Value = serde_json::from_str(&out).unwrap();
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

/// `box.agents {box, refresh: true}` straight to the hub on `socket`, as the user.
fn box_agents_refresh(socket: &Path, name: &str) -> Value {
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
        c.call("box.agents", json!({ "box": name, "refresh": true }))
            .await
            .unwrap()
    })
}

/// A fake Claude Code that is logged in: its status names an account, which twr never shows.
const CLAUDE_LOGGED_IN: &str = r#"case "$1 $2" in
  "--version ") echo '2.1.0 (Claude Code)' ;;
  "auth status") echo '{"loggedIn": true, "authMethod": "claude.ai", "email": "someone@example.com", "orgName": "Secret Org"}' ;;
  *) exit 1 ;;
esac
"#;

/// A fake Codex that is logged out.
const CODEX_LOGGED_OUT: &str = r#"case "$1 $2" in
  "--version ") echo 'codex-cli 0.50.0' ;;
  "login status") echo 'Not logged in'; exit 1 ;;
  *) exit 1 ;;
esac
"#;

/// `twr box doctor` of an SSH box: with the hub running and the box online, every check
/// passes but the agents' (Claude logged in, Codex logged out with how to log in, no Grok)
/// and the login shell's (no SHELL on the fake box); `--json` says the same, the build used
/// and the agents; no account detail anywhere. `twr box list` and `status` show the agents
/// once the box reported them. With the hub stopped, doctor still checks the box (its daemon
/// outlives the hub); a project whose repository is missing on the box fails (exit 1), and so
/// does a box ssh cannot reach.
#[test]
fn box_doctor_checks_a_box_with_and_without_the_hub() {
    let t = Setup::new();
    let _cleanup = BoxProcesses(t.box_home.clone());
    t.box_agent("claude", CLAUDE_LOGGED_IN);
    t.box_agent("codex", CODEX_LOGGED_OUT);
    let paths = t.paths();
    let mut hub = Some(t.start_hub(1));
    t.ok(&["box", "add", "gpu", "fake@box", "--yes"]);
    let id = twr_core::build_id_of(Path::new(TWR)).unwrap();
    let bin = t.box_home.join("bin");

    let out = t.twr(&["box", "doctor", "gpu"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let said = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "{text}\n{said}");
    for line in [
        "✓ hub: box gpu is online: twr ".to_string(),
        "✓ ssh without prompting: works".to_string(),
        "✓ ssh in this terminal: reaches the box: linux/".to_string(),
        format!("✓ build: the hub's build {id} is on the box; the checks below run it"),
        format!("✓ daemon: a box daemon runs: build {id}, runner protocol "),
        "! login shell: the login shell's PATH was not read (SHELL is not set)".to_string(),
        format!(
            "✓ claude: logged in (claude.ai), 2.1.0 (Claude Code), at {}/claude",
            bin.display()
        ),
        format!(
            "! codex: logged out, codex-cli 0.50.0, at {}/codex\n    fix: `twr box login gpu codex`",
            bin.display()
        ),
        "! grok: not installed (not found on PATH)".to_string(),
        "✓ git: git version ".to_string(),
        "box gpu: nothing failed".to_string(),
    ] {
        assert!(text.contains(&line), "{line:?} in:\n{text}");
    }
    for secret in ["example.com", "Secret Org"] {
        assert!(!text.contains(secret) && !said.contains(secret), "{text}");
    }

    // A box where the user lingers and logind ends a session's processes, but the daemon was
    // started in a session (here the relay's setsid, as when `systemd-run` fails): lingering
    // is no proof it outlives the hub's ssh. Its unit, once listed, is (fakes of loginctl,
    // busctl and systemctl on the box).
    let key = twr_core::sealed::HubKey::load_or_create(
        &paths.data_dir.join(twr_hub::ssh_box::BOX_KEY_FILE),
    )
    .unwrap();
    let unit = format!(
        "twr-box-{}-1a2b3c4d.service",
        twr_runner::relay::socket_hash(&key.id(), "gpu")
    );
    let listed = t.box_home.join("units");
    t.box_agent("loginctl", "echo Linger=yes\n");
    t.box_agent("busctl", "echo 'b true'\n");
    t.box_agent(
        "systemctl",
        &format!(
            "[ \"$1 $2\" = '--user list-units' ] || exit 1\ncat '{}' 2>/dev/null\nexit 0\n",
            listed.display()
        ),
    );
    let out = t.twr(&["box", "doctor", "gpu"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "{text}");
    assert!(
        text.contains(
            "! logind: you linger, but the box daemon runs outside a systemd user service"
        ),
        "{text}"
    );
    assert!(
        text.contains("systemd-run --user --expand-environment=no"),
        "{text}"
    );
    std::fs::write(
        &listed,
        format!("{unit} loaded active running twr box daemon\n"),
    )
    .unwrap();
    let out = t.twr(&["box", "doctor", "gpu"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        text.contains(&format!(
            "✓ logind: you linger on the box, and the box daemon runs as the systemd user \
             service {unit}"
        )),
        "{text}"
    );
    for fake in ["loginctl", "busctl", "systemctl"] {
        std::fs::remove_file(bin.join(fake)).unwrap();
    }

    let out = t.ok(&["box", "doctor", "gpu", "--json"]);
    let v: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["box"], "gpu", "{v}");
    assert_eq!(v["ok"], true, "{v}");
    assert_eq!(v["build"], json!({"id": id, "source": "hub"}), "{v}");
    assert_eq!(v["login_path"]["error"], "SHELL is not set", "{v}");
    let agents = v["agents"].as_array().unwrap();
    assert_eq!(agents.len(), 3, "{v}");
    assert_eq!(agents[0]["runtime"], "claude-code", "{v}");
    assert_eq!(agents[0]["login"], "logged_in", "{v}");
    assert_eq!(agents[0]["method"], "claude.ai", "{v}");
    assert_eq!(agents[1]["login"], "logged_out", "{v}");
    assert_eq!(agents[2]["found"], false, "{v}");
    for c in v["checks"].as_array().unwrap() {
        let mut keys: Vec<&str> = c.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["check", "fix", "item", "message", "result"], "{c}");
        assert!(
            ["ok", "warn", "fail"].contains(&c["result"].as_str().unwrap()),
            "{c}"
        );
    }
    let codex = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["item"] == "codex")
        .unwrap();
    assert_eq!(codex["check"], "agent");
    assert_eq!(codex["result"], "warn");
    assert_eq!(codex["fix"], "`twr box login gpu codex`");
    assert!(!out.contains("example.com"), "{out}");

    // The agents in `twr box list` and `status`, once the box reported them.
    let refreshed = box_agents_refresh(&paths.socket, "gpu");
    assert_eq!(
        refreshed["agents"].as_array().unwrap().len(),
        3,
        "{refreshed}"
    );
    let list = t.ok(&["box", "list"]);
    assert!(list.lines().next().unwrap().ends_with("AGENTS"), "{list}");
    let row = list.lines().find(|l| l.starts_with("gpu ")).unwrap();
    assert!(row.ends_with("claude ✓ codex ✗ grok -"), "{list}");
    let status = t.ok(&["box", "status", "gpu"]);
    assert!(
        status.contains(&format!(
            "  claude: logged in (claude.ai), 2.1.0 (Claude Code), at {}/claude\n",
            bin.display()
        )),
        "{status}"
    );
    assert!(
        status.contains("  grok: not installed (not found on PATH)\n"),
        "{status}"
    );
    assert_eq!(t.box_status("gpu")["agents"][1]["login"], "logged_out");

    // The hub stopped: the box is still checked, and its daemon still runs.
    t.ok(&["hub", "stop"]);
    hub.take().unwrap().exited();
    let out = t.twr(&["box", "doctor", "gpu"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(out.status.success(), "{text}");
    assert!(text.contains("! hub: no hub runs"), "{text}");
    assert!(
        text.contains(&format!("✓ daemon: a box daemon runs: build {id}")),
        "{text}"
    );
    assert!(text.contains("✓ claude: logged in"), "{text}");
    assert!(text.contains("box gpu: nothing failed"), "{text}");

    // Projects of the box: one that is there, one that is not.
    let repo = init_repo(&t.box_home.join("repo"));
    let mut boxes = std::fs::read_to_string(paths.boxes_file()).unwrap();
    boxes.push_str(&format!(
        "\n[boxes.gpu.projects.api]\nrepo = \"{}\"\n\n[boxes.gpu.projects.gone]\nrepo = \"{}\"\n",
        repo.display(),
        t.box_home.join("gone").display()
    ));
    std::fs::write(paths.boxes_file(), boxes).unwrap();
    let out = t.twr(&["box", "doctor", "gpu"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(
        text.contains(&format!(
            "✓ project api: {} is a git repository",
            repo.display()
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "✗ project gone: {}/gone does not exist on the box",
            t.box_home.display()
        )),
        "{text}"
    );
    assert!(text.contains("box gpu: 1 check failed"), "{text}");

    // A box ssh cannot reach: ssh fails, and nothing on the box is checked.
    let mut boxes = std::fs::read_to_string(paths.boxes_file()).unwrap();
    boxes.push_str("\n[boxes.down]\ntransport = \"ssh\"\nssh = \"down@box\"\n");
    std::fs::write(paths.boxes_file(), boxes).unwrap();
    let out = t.twr(&["box", "doctor", "down", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], false, "{v}");
    assert_eq!(v["agents"], Value::Null, "{v}");
    let failed: Vec<&str> = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["result"] == "fail")
        .map(|c| c["check"].as_str().unwrap())
        .collect();
    assert_eq!(failed, ["ssh_batch", "ssh"], "{v}");
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("Connection refused"), "{said}");
}

/// `twr box login` is the user's alone, and needs a terminal: refused before any ssh.
#[test]
fn box_login_is_refused_to_agents_and_without_a_terminal() {
    let t = Setup::new();
    std::fs::write(
        t.paths().boxes_file(),
        "[boxes.gpu]\ntransport = \"ssh\"\nssh = \"fake@box\"\n",
    )
    .unwrap();
    let out = t
        .command(&["box", "login", "gpu", "claude"])
        .env("TWR_AGENT", "worker:1")
        .env_remove("TWR_TEST_USER_TOKEN_FILE")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let e = String::from_utf8_lossy(&out.stderr);
    assert!(
        e.contains("agents cannot log agent CLIs in on boxes"),
        "{e}"
    );
    // No terminal (stdin is /dev/null here).
    let e = t.fails(&["box", "login", "gpu", "codex"]);
    assert!(e.contains("needs a terminal on stdin and stdout"), "{e}");
    // Not an agent twr logs in.
    let e = t.fails(&["box", "login", "gpu", "acp"]);
    assert!(e.contains("invalid value"), "{e}");
    assert!(t.ssh_runs().is_empty(), "{:?}", t.ssh_runs());
}

/// A fake `uname` for the box: a Mac on Apple silicon (the hub's probe, doctor's and the
/// relay's scripts read it; the box's twr is still this Linux build).
const UNAME_MAC: &str = "case $1 in -s) echo Darwin ;; -m) echo arm64 ;; *) echo Darwin ;; esac\n";

/// A Mac box (a fake `uname` on the box): `twr box add` says what a Mac needs and that it needs
/// a build; `twr box install --from-box` copies a twr already on the box into its builds,
/// checked, records it, and the hub connects with it; `twr box doctor` has a macOS group
/// (whether anyone is logged in at the screen can't be told here: `/bin/launchctl` is not
/// there, which is no "no one"; the daemon runs from ssh) and takes
/// Claude Code's login from the box daemon; `twr box restart` replaces the daemon with a new
/// one on a newer connection (`--restart`) and says it still runs from ssh.
#[test]
fn a_mac_box_from_add_to_restart() {
    let t = Setup::new();
    let _cleanup = BoxProcesses(t.box_home.clone());
    t.box_agent("uname", UNAME_MAC);
    t.box_agent("claude", CLAUDE_LOGGED_IN);
    let paths = t.paths();
    let _hub = t.start_hub(1);

    let out = t.ok(&["box", "add", "mac", "fake@box", "--yes"]);
    for line in [
        "box mac: macos/aarch64 (Darwin arm64)",
        "- The box is a Mac. It needs:",
        "  - someone logged in at the Mac's screen",
        "automatic login",
        "  - its agents logged in at the Mac itself",
        "box mac needs a twr build for macos/aarch64: run `twr box install mac`",
    ] {
        assert!(out.contains(line), "{line:?} in:\n{out}");
    }
    assert_eq!(t.box_status("mac")["state"], "needs_install");

    // A twr built on the box (here: this one, copied), installed from there.
    let built = t.box_home.join(".local/bin/twr");
    std::fs::create_dir_all(built.parent().unwrap()).unwrap();
    cp(Path::new(TWR), &built);
    std::fs::set_permissions(&built, std::fs::Permissions::from_mode(0o755)).unwrap();
    let id = twr_core::build_id_of(&built).unwrap();
    let out = t.ok(&[
        "box",
        "install",
        "mac",
        "--from-box",
        built.to_str().unwrap(),
        "--yes",
    ]);
    let exe = t
        .box_home
        .join(".local/share/twr/runner/builds")
        .join(&id)
        .join("twr");
    for line in [
        format!("installed build {id} on box mac, at {}", exe.display()),
        "box mac runs it as twr ".to_string(),
        format!("recorded build = \"{id}\" for box mac in"),
        "box mac is online: twr ".to_string(),
    ] {
        assert!(out.contains(&line), "{line:?} in:\n{out}");
    }
    assert_eq!(
        std::fs::metadata(&exe).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(twr_core::build_id_of(&exe).unwrap(), id);
    let boxes = std::fs::read_to_string(paths.boxes_file()).unwrap();
    assert!(boxes.contains(&format!("build = \"{id}\"")), "{boxes}");
    let status = t.box_status("mac");
    assert_eq!(
        (&status["os"], &status["running_build"], &status["launch"]),
        (&json!("Darwin"), &json!(id), &json!("detached")),
        "{status}"
    );
    // A file that is not there.
    let e = t.fails(&[
        "box",
        "install",
        "mac",
        "--from-box",
        t.box_home.join("nope").to_str().unwrap(),
        "--yes",
    ]);
    assert!(
        e.contains("is not a regular file on the box") && e.contains("nothing was installed"),
        "{e}"
    );

    // Doctor: the macOS group, and Claude Code's login as the daemon sees it.
    box_agents_refresh(&paths.socket, "mac");
    let out = t.twr(&["box", "doctor", "mac"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert_eq!(out.status.code(), Some(0), "{text}");
    for line in [
        "! macOS GUI session: couldn't tell whether someone is logged in at the Mac's screen \
         (`launchctl print gui/<uid>`: exit 127)",
        "! macOS daemon launch: the daemon runs from ssh, so Claude Code can't read its \
         keychain login\n    fix: log in at the Mac's screen (or turn on automatic login), \
         then `twr box restart mac`",
        "(as the box daemon reported it)",
    ] {
        assert!(text.contains(line), "{line:?} in:\n{text}");
    }
    assert!(text.contains("✓ claude: logged in (claude.ai)"), "{text}");
    assert!(!text.contains("logind"), "{text}");

    // Restart: a new daemon, on a newer connection, through `--restart`.
    let before = daemons(&t.box_home);
    assert_eq!(before.len(), 1, "{before:?}");
    let epoch = t.box_status("mac")["epoch"].as_u64().unwrap();
    let out = t.ok(&["box", "restart", "mac", "--yes"]);
    for line in [
        "This stops box mac's daemon and the agents it runs",
        "restarting box mac's daemon: the hub connects again, and the relay on the box stops \
         the daemon (a repository change it runs is finished first: up to 330 s)",
        // The relay's notes as it goes.
        "\n  twr-box: stopping the box daemon ",
        "\n  twr-box: restarted the box daemon ",
        "box mac is back online with a new daemon: twr ",
        "the daemon runs from ssh, so Claude Code can't read its keychain login: log in at \
         the Mac's screen (or turn on automatic login), then `twr box restart mac`",
    ] {
        assert!(out.contains(line), "{line:?} in:\n{out}");
    }
    let after = daemons(&t.box_home);
    assert_eq!(after.len(), 1, "{after:?}");
    assert_ne!(after, before, "a new daemon");
    let status = t.box_status("mac");
    assert!(status["epoch"].as_u64().unwrap() > epoch, "{status}");
    assert_eq!(status["restart_pending"], false, "{status}");
    let runs = t.ssh_runs();
    let restarts = runs
        .iter()
        .filter(|(_, _, c)| c.contains(" runner --stdio ") && c.ends_with(" --restart"))
        .count();
    assert_eq!(restarts, 1, "{runs:?}");
    let notes = status["notes"].to_string();
    assert!(
        notes.contains("restarting the box daemon (`twr box restart`)")
            && notes.contains("twr-box: restarted the box daemon"),
        "{notes}"
    );
}

/// `twr box restart` and `twr box install --from-box` refuse before any ssh: an agent, no
/// terminal without --yes, a token box, a path that is not absolute (a quoted `~` included:
/// the help and the error say how to get the absolute one), --binary with it.
#[test]
fn box_restart_and_from_box_guards() {
    let t = Setup::new();
    std::fs::write(
        t.paths().boxes_file(),
        "[boxes.gpu]\ntransport = \"ssh\"\nssh = \"fake@box\"\n\n[boxes.lab]\ntoken = \
         \"lab-token-0123456789abcdef0123456789\"\n",
    )
    .unwrap();
    let out = t
        .command(&["box", "restart", "gpu", "--yes"])
        .env("TWR_AGENT", "worker:1")
        .env_remove("TWR_TEST_USER_TOKEN_FILE")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let e = String::from_utf8_lossy(&out.stderr);
    assert!(e.contains("agents cannot restart box daemons"), "{e}");
    let e = t.fails(&["box", "restart", "gpu"]);
    assert!(
        e.contains("without confirmation") && e.contains("--yes"),
        "{e}"
    );
    let e = t.fails(&["box", "restart", "lab", "--yes"]);
    assert!(e.contains("box lab is a token box"), "{e}");
    let e = t.fails(&["box", "install", "gpu", "--from-box", "bin/twr", "--yes"]);
    assert!(
        e.contains("absolute path on box gpu") && e.contains("on the box, `command -v twr`"),
        "{e}"
    );
    // A quoted `~` stays a `~`: not an absolute path either.
    let e = t.fails(&[
        "box",
        "install",
        "gpu",
        "--from-box",
        "~/.local/bin/twr",
        "--yes",
    ]);
    assert!(e.contains("absolute path on box gpu"), "{e}");
    let help = t.ok(&["box", "install", "--help"]);
    assert!(
        help.contains("`command -v twr` or `echo ~/.local/bin/twr` prints it")
            && !help.contains("quote"),
        "{help}"
    );
    let e = t.fails(&[
        "box",
        "install",
        "gpu",
        "--from-box",
        "/x/twr",
        "--binary",
        "/y/twr",
    ]);
    assert!(e.contains("cannot be used with"), "{e}");
    assert!(t.ssh_runs().is_empty(), "{:?}", t.ssh_runs());
}

// -------------------------------------------------------------------------------------------
// Command boxes
// -------------------------------------------------------------------------------------------

impl Setup {
    /// `twr <args>` with a terminal on its stdin and stdout (`script`): whether it succeeded,
    /// and what it printed.
    fn in_terminal(&self, args: &[&str]) -> (bool, String) {
        let line: Vec<String> = std::iter::once(TWR)
            .chain(args.iter().copied())
            .map(twr_core::paths::sh_word)
            .collect();
        let out = Command::new("script")
            .args(["-qec", &line.join(" "), "/dev/null"])
            .env("TWR_HOME", &self.home)
            .env("TWR_TEST_USER_TOKEN_FILE", test_token())
            .env_remove("TWR_WORKSPACE")
            .env_remove("RUST_LOG")
            .stdin(Stdio::null())
            .output()
            .expect("running script");
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        (out.status.success(), text.replace('\r', ""))
    }
}

/// The processes of the box's relays (`twr runner --stdio` of a build under the fake box).
fn box_relays(box_home: &Path) -> Vec<u32> {
    processes_mentioning(box_home.to_str().unwrap())
        .into_iter()
        .filter(|(_, args)| args.iter().any(|a| a == "--stdio"))
        .map(|(pid, _)| pid)
        .collect()
}

/// A command box (a fake `docker exec -i box`) from `twr box add --command` to `twr box
/// remove`: the CLI probes it through the command, its pipe check with it, and the hub
/// installs its own build through it and comes online; boxes.toml has the command as given
/// and no terminal_command (the fake is no docker). No ssh runs. `twr box list` and `status`
/// say `command`, and a thread on the box runs a turn. `twr attach` is refused (the hub's
/// message) before anything is stopped. Doctor checks `command` and `pipe` instead of ssh,
/// then the build, the daemon and logind, whose fix shows an agent no stop command (it holds
/// the command's words). `twr box login` and `--clone` are refused without a
/// terminal_command, naming the box and the key, before anything runs. `twr box restart`
/// brings it back on a new daemon through `--restart`. With the hub stopped, no relay of the
/// box is left and its daemon runs on; `twr box remove` prints the stop command as the box's
/// command runs it, which stops the daemon.
#[test]
fn a_command_box_from_add_to_remove() {
    let t = Setup::new();
    let _cleanup = BoxProcesses(t.box_home.clone());
    let paths = t.paths();
    let mut hub = Some(t.start_hub(1));
    let exec = t.exec.to_str().unwrap();
    let label = format!("{} box", twr_core::paths::sh_word(exec));

    let out = t.ok(&["box", "add", "sb", "--command", exec, "box", "--yes"]);
    for line in [
        format!("probing box sb through `{label}`..."),
        "box sb: linux/".to_string(),
        format!("Adding box sb (command: {label}):"),
        "- The hub installs its own build".to_string(),
        format!("- The hub runs `{label} sh -c ...` itself, from its own environment"),
        "- `twr box login` and `twr box project --clone` need a terminal_command in [boxes.sb]"
            .to_string(),
        "box sb is online: twr ".to_string(),
    ] {
        assert!(out.contains(&line), "{line:?} in:\n{out}");
    }
    let file = twr_core::config::BoxesFile::load(&paths.boxes_file()).unwrap();
    let sb = &file.boxes["sb"];
    assert_eq!(sb.transport, Some(twr_core::config::BoxTransport::Command));
    assert_eq!(sb.command, Some(vec![exec.to_string(), "box".to_string()]));
    assert_eq!(sb.terminal_command, None);
    assert_eq!(sb.ssh, None);
    // Through the command only: the CLI's probe and the hub's, its upload, its relay.
    assert!(t.ssh_runs().is_empty(), "{:?}", t.ssh_runs());
    let runs = t.exec_runs();
    assert!(runs.iter().all(|(_, c, _)| c == "box"), "{runs:?}");
    let probes = runs
        .iter()
        .filter(|(_, _, c)| {
            c.starts_with("sh -c ") && c.contains("twr-probe 1") && c.contains("pipe=")
        })
        .count();
    assert!(probes >= 2, "{runs:?}");
    assert!(
        runs.iter().any(|(_, _, c)| c.contains(".twr.upload.")),
        "{runs:?}"
    );
    let id = twr_core::build_id_of(Path::new(TWR)).unwrap();
    let exe = t
        .box_home
        .join(".local/share/twr/runner/builds")
        .join(&id)
        .join("twr");
    assert_eq!(twr_core::build_id_of(&exe).unwrap(), id);

    let list = t.ok(&["box", "list"]);
    let row = list.lines().find(|l| l.starts_with("sb ")).unwrap();
    let cells: Vec<&str> = row.split_whitespace().collect();
    assert_eq!(cells[1..3], ["command", "online"], "{list}");
    let status = t.box_status("sb");
    assert_eq!(status["transport"], "command", "{status}");
    assert_eq!(status["build"], id.as_str(), "{status}");
    let text = t.ok(&["box", "status", "sb"]);
    assert!(text.contains("transport:      command ("), "{text}");
    assert!(!text.contains(exec), "the command is not shown: {text}");

    // A thread on the box.
    let repo = init_repo(&t.box_home.join("repo"));
    t.ok(&[
        "thread",
        "new",
        repo.to_str().unwrap(),
        "--box",
        "sb",
        "--runtime",
        "fake",
        "--name",
        "contained",
    ]);
    t.turn("contained", "hello from the container", 1);

    // Attach: refused by the hub before the runtime is stopped.
    let e = t.fails(&["attach", "contained"]);
    assert!(
        e.contains("twr can't attach to threads on box sb yet")
            && e.contains("Continue the thread from the TUI or with `twr send`"),
        "{e}"
    );
    assert_eq!(t.count("contained", "thread.attached"), 0);
    assert_eq!(t.count("contained", "thread.stopped"), 0);
    t.turn("contained", "still running", 2);
    assert_eq!(t.count("contained", "thread.started"), 1);

    // Doctor: the command and its pipe instead of ssh.
    let out = t.twr(&["box", "doctor", "sb"]);
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        text.starts_with(&format!("box sb: command (the hub runs `{label}`)\n")),
        "{text}"
    );
    let out = t.twr(&["box", "doctor", "sb", "--json"]);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let checks = v["checks"].as_array().unwrap();
    let result = |name: &str| {
        checks
            .iter()
            .find(|c| c["check"] == name)
            .map(|c| c["result"].as_str().unwrap().to_string())
    };
    for ok in ["command", "pipe", "build", "daemon"] {
        assert_eq!(result(ok).as_deref(), Some("ok"), "{ok}: {v}");
    }
    assert!(result("logind").is_some(), "{v}");
    for none in ["ssh", "ssh_batch"] {
        assert_eq!(result(none), None, "{none}: {v}");
    }
    assert_eq!(v["build"], json!({"id": id, "source": "hub"}), "{v}");

    // logind's fix when the daemon runs outside a systemd user service (fakes of loginctl,
    // busctl and systemctl on the box): the user gets the stop command, an agent `twr box
    // restart` and never the command's words, in text and in JSON. For these runs only, the
    // command (doctor reads it from boxes.toml; the hub keeps its own) holds a secret.
    let file = paths.boxes_file();
    let kept = std::fs::read_to_string(&file).unwrap();
    let secret: String = kept
        .lines()
        .map(|l| match l.strip_prefix("command = [") {
            Some(rest) => format!("command = [\"env\", \"KEY=s3cret\", {rest}\n"),
            None => format!("{l}\n"),
        })
        .collect();
    assert!(secret.contains("KEY=s3cret"), "{kept}");
    std::fs::write(&file, &secret).unwrap();
    t.box_agent("loginctl", "echo Linger=yes\n");
    t.box_agent("busctl", "echo 'b true'\n");
    t.box_agent(
        "systemctl",
        "[ \"$1 $2\" = '--user list-units' ] || exit 1\nexit 0\n",
    );
    let doctor = |json: bool, agent: bool| {
        let mut args = vec!["box", "doctor", "sb"];
        if json {
            args.push("--json");
        }
        let mut c = t.command(&args);
        if agent {
            c.env("TWR_AGENT", "worker:1")
                .env_remove("TWR_TEST_USER_TOKEN_FILE");
        }
        let out = c.output().unwrap();
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let logind_fix = |json: &str| {
        let v: Value = serde_json::from_str(json.trim()).unwrap();
        let c = v["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["check"] == "logind")
            .unwrap_or_else(|| panic!("no logind check: {v}"))
            .clone();
        assert_eq!(c["result"], "warn", "{v}");
        c["fix"].as_str().unwrap().to_string()
    };
    let (out, err) = doctor(false, true);
    let text = format!("{out}{err}");
    assert!(
        text.contains(
            "! logind: you linger, but the box daemon runs outside a systemd user service"
        ),
        "{text}"
    );
    assert!(
        text.contains("then restart the box daemon (`twr box restart sb`)"),
        "{text}"
    );
    for word in ["s3cret", "--stop", "sh -c"] {
        assert!(!text.contains(word), "{word} in:\n{text}");
    }
    let (json, err) = doctor(true, true);
    assert!(
        !json.contains("s3cret") && !err.contains("s3cret"),
        "{json}{err}"
    );
    let fix = logind_fix(&json);
    assert!(
        fix.contains("`twr box restart sb`") && !fix.contains("--stop"),
        "{fix}"
    );
    // The user gets the stop command.
    let (text, _) = doctor(false, false);
    assert!(
        text.contains("then stop the box daemon with `env KEY=s3cret "),
        "{text}"
    );
    let (json, _) = doctor(true, false);
    let fix = logind_fix(&json);
    assert!(
        fix.contains("KEY=s3cret") && fix.contains("--stop"),
        "{fix}"
    );
    std::fs::write(&file, &kept).unwrap();
    for fake in ["loginctl", "busctl", "systemctl"] {
        std::fs::remove_file(t.box_home.join("bin").join(fake)).unwrap();
    }

    // What needs a terminal on the box: refused without a terminal_command, before anything
    // runs on it.
    let before = t.exec_runs().len();
    if Command::new("script")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        let (ok, said) = t.in_terminal(&["box", "login", "sb", "claude"]);
        assert!(!ok, "{said}");
        assert!(
            said.contains(
                "box sb has no terminal_command: set one in [boxes.sb] (its command with `-t`: \
                 `docker exec -i -t <container>`), or log in inside the container with `claude \
                 auth login`"
            ),
            "{said}"
        );
        let (ok, said) = t.in_terminal(&["box", "project", "sb", "api", "--clone", "--yes"]);
        assert!(!ok, "{said}");
        assert!(
            said.contains("box sb has no terminal_command")
                && said.contains("`twr box project sb api --path <path>`"),
            "{said}"
        );
        assert_eq!(t.exec_runs().len(), before, "{:?}", t.exec_runs());
    } else {
        eprintln!("no `script` here: the terminal refusals are skipped");
    }

    // Restart: a new daemon, on a newer connection, through `--restart`.
    let before = daemons(&t.box_home);
    assert_eq!(before.len(), 1, "{before:?}");
    let epoch = t.box_status("sb")["epoch"].as_u64().unwrap();
    let out = t.ok(&["box", "restart", "sb", "--yes"]);
    assert!(
        out.contains("box sb is back online with a new daemon: twr "),
        "{out}"
    );
    let after = daemons(&t.box_home);
    assert_eq!(after.len(), 1, "{after:?}");
    assert_ne!(after, before, "a new daemon");
    assert!(t.box_status("sb")["epoch"].as_u64().unwrap() > epoch);
    let restarts = t
        .exec_runs()
        .iter()
        .filter(|(_, _, c)| c.contains(" runner --stdio ") && c.contains(" --restart"))
        .count();
    assert_eq!(restarts, 1, "{:?}", t.exec_runs());

    // The hub stopped: it ends its command, so no relay of the box is left; the daemon runs on.
    t.ok(&["hub", "stop"]);
    hub.take().unwrap().exited();
    poll("no relay of the box left", || {
        box_relays(&t.box_home).is_empty().then_some(())
    });
    assert_eq!(daemons(&t.box_home), after, "the daemon outlives the hub");

    // Removed: the stop command is the box's command with `sh -c`, and it stops the daemon.
    let out = t.ok(&["box", "remove", "sb"]);
    assert!(out.contains("removed box sb from"), "{out}");
    assert!(
        out.contains("twr never stops or removes box sb's container: it stays as it is."),
        "{out}"
    );
    let stop = out
        .lines()
        .find(|l| l.contains("--stop"))
        .unwrap_or_else(|| panic!("no stop command: {out}"))
        .trim()
        .to_string();
    assert!(stop.starts_with(&format!("{label} sh -c ")), "{stop}");
    assert!(
        stop.contains(&format!("/twr/runner/builds/{id}/twr")),
        "{stop}"
    );
    let out = Command::new("sh")
        .arg("-c")
        .arg(&stop)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{said}");
    assert!(said.contains("twr-box: stopped the box daemon"), "{said}");
    poll("no box daemon left", || {
        daemons(&t.box_home).is_empty().then_some(())
    });
}

/// `twr box add --command` refuses a command that cannot carry the hub's connection, saying
/// how to fix it, and writes nothing: one that does not pass its input on (`docker exec`
/// without `-i`), one that gives the box a terminal (`-t`), a container that does not exist
/// (as docker says it), and an ssh-like program (before anything runs). No hub is started.
#[test]
fn a_command_box_that_cannot_carry_the_connection_is_not_added() {
    let t = Setup::new();
    let paths = t.paths();
    let add = |argv: &[&str]| {
        let mut args = vec!["box", "add", "sb", "--command"];
        args.extend_from_slice(argv);
        args.push("--yes");
        t.fails(&args)
    };
    let e = add(&[t.exec_noinput.to_str().unwrap(), "box"]);
    assert!(
        e.contains(
            "box sb's command does not pass the hub's input to the box: `docker exec` and \
             `kubectl exec` need `-i`"
        ),
        "{e}"
    );
    if Command::new("script")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
    {
        let e = add(&[t.exec_tty.to_str().unwrap(), "box"]);
        assert!(
            e.contains(
                "box sb's command gives the box a terminal (a `-t` in it?): the hub's \
                 connection is binary. Remove `-t` from [boxes.sb] command"
            ),
            "{e}"
        );
    } else {
        eprintln!("no `script` here: the terminal case is skipped");
    }
    let e = add(&[t.exec.to_str().unwrap(), "nosuch"]);
    assert!(
        e.contains(
            "box sb's container does not exist (Error response from daemon: No such container: \
             nosuch): create and start it"
        ),
        "{e}"
    );
    let ran = t.exec_runs().len();
    let e = add(&["tailscale", "ssh", "me@box"]);
    assert!(
        e.contains("`tailscale ssh` passes its arguments to a shell on the box"),
        "{e}"
    );
    assert_eq!(t.exec_runs().len(), ran);
    assert!(!paths.boxes_file().exists());
    assert!(!paths.socket.exists(), "a hub was started");
    assert!(t.ssh_runs().is_empty(), "{:?}", t.ssh_runs());
}

/// `twr box doctor`, which agents may run, never shows an agent a command box's words (one may
/// hold a secret): its header, its progress and the failed command check's fix name the
/// program and how many words it has, in text and in JSON. The user sees the command.
#[test]
fn box_doctor_never_shows_an_agent_a_command_s_words() {
    let t = Setup::new();
    let exec = t.exec.to_str().unwrap();
    std::fs::write(
        t.paths().boxes_file(),
        format!(
            "[boxes.sb]\ntransport = \"command\"\ncommand = [{exec:?}, \"nosuch\", \"-e\", \
             \"KEY=s3cret\"]\n"
        ),
    )
    .unwrap();
    let doctor = |json: bool, agent: bool| {
        let mut args = vec!["box", "doctor", "sb"];
        if json {
            args.push("--json");
        }
        let mut c = t.command(&args);
        if agent {
            c.env("TWR_AGENT", "worker:1")
                .env_remove("TWR_TEST_USER_TOKEN_FILE");
        }
        let out = c.output().unwrap();
        assert!(!out.status.success());
        (
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };
    let shown = format!("{exec} (4 words)");
    let (out, err) = doctor(false, true);
    let text = format!("{out}{err}");
    assert!(
        !text.contains("s3cret") && !text.contains("nosuch -e"),
        "{text}"
    );
    assert!(
        text.starts_with(&format!("box sb: command (the hub runs `{shown}`)\n")),
        "{text}"
    );
    assert!(
        text.contains(&format!("probing box sb through `{shown}`...")),
        "{text}"
    );
    assert!(
        text.contains(
            "box sb's container does not exist (Error response from daemon: No such \
             container: nosuch)"
        ),
        "{text}"
    );
    let (json, err) = doctor(true, true);
    assert!(
        !json.contains("s3cret") && !err.contains("s3cret"),
        "{json}{err}"
    );
    let v: Value = serde_json::from_str(json.trim()).unwrap();
    let check = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["check"] == "command")
        .unwrap_or_else(|| panic!("no command check: {v}"))
        .clone();
    assert_eq!(check["result"], "fail", "{v}");
    assert!(
        check["fix"]
            .as_str()
            .unwrap()
            .contains(&format!("[boxes.sb] command ({shown}) runs here")),
        "{v}"
    );
    // The user sees the command, in its fix too.
    let (text, _) = doctor(false, false);
    assert!(text.contains("nosuch -e KEY=s3cret"), "{text}");
    let (json, _) = doctor(true, false);
    assert!(json.contains("nosuch -e KEY=s3cret true"), "{json}");
}

// -------------------------------------------------------------------------------------------
// Container boxes with a real docker (ignored: `cargo test -p twr-cli --test ssh_box
// container_box -- --ignored`)
// -------------------------------------------------------------------------------------------

/// `docker <args>`, its stdin null.
fn docker(args: &[&str]) -> Output {
    Command::new("docker")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("running docker")
}

/// `docker <args>`, which must succeed: its stdout.
fn docker_ok(args: &[&str]) -> String {
    let out = docker(args);
    assert!(
        out.status.success(),
        "docker {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// The image of docs/tools/container-box/Dockerfile (the docs' recipe), built once as
/// `twr-test-box:<the Dockerfile's hash>[-<base>]` from base `TWR_TEST_BOX_IMAGE` (default
/// `ubuntu:24.04`, whose glibc runs this debug build). Panics, saying so, when docker can't be
/// used here.
fn box_image() -> &'static str {
    static IMAGE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    IMAGE.get_or_init(|| {
        let v = Command::new("docker")
            .args(["version", "--format", "{{.Server.Version}}"])
            .stdin(Stdio::null())
            .output();
        match v {
            Ok(o) if o.status.success() => {}
            Ok(o) => panic!(
                "these tests need docker, and `docker version` failed here: {}",
                String::from_utf8_lossy(&o.stderr).trim()
            ),
            Err(e) => panic!("these tests need docker, and `docker` can't be run here: {e}"),
        }
        let context = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/tools/container-box");
        let hash = twr_core::build_id_of(&context.join("Dockerfile")).unwrap();
        let base = std::env::var("TWR_TEST_BOX_IMAGE").unwrap_or_default();
        let tag = if base.is_empty() {
            format!("twr-test-box:{hash}")
        } else {
            let clean: String = base
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect();
            format!("twr-test-box:{hash}-{clean}")
        };
        if !docker(&["image", "inspect", &tag]).status.success() {
            let mut args = vec!["build", "-q", "-t", &tag];
            let arg = format!("BASE={base}");
            if !base.is_empty() {
                args.extend(["--build-arg", &arg]);
            }
            let dir = context.to_str().unwrap();
            args.push(dir);
            docker_ok(&args);
        }
        tag
    })
}

/// A container of [`box_image`] (`docker run -d --init -e TWR_ENABLE_FAKE=1 <extra>`), removed
/// with `docker rm -f` when dropped.
struct Container(String);

impl Container {
    fn run(extra: &[&str]) -> Container {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let name = format!("twr-test-{}-{nanos}", std::process::id());
        let mut args = vec![
            "run",
            "-d",
            "--name",
            &name,
            "--init",
            "-e",
            "TWR_ENABLE_FAKE=1",
        ];
        args.extend_from_slice(extra);
        args.push(box_image());
        let c = Container(name.clone());
        docker_ok(&args);
        c
    }

    /// `docker exec <container> sh -c <script>` as the container's user: its stdout, when it
    /// succeeded.
    fn sh(&self, script: &str) -> Option<String> {
        let out = docker(&["exec", &self.0, "sh", "-c", script]);
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// The pids of the processes in the container whose command line matches `pattern`
    /// (`pgrep -f`).
    fn pgrep(&self, pattern: &str) -> Vec<u32> {
        // (No shell around it, whose command line would match too.)
        String::from_utf8_lossy(&docker(&["exec", &self.0, "pgrep", "-f", "--", pattern]).stdout)
            .split_whitespace()
            .filter_map(|p| p.parse().ok())
            .collect()
    }
}

impl Container {
    /// Kills the processes in the container whose command line matches `pattern` (`pkill -f`,
    /// as root).
    fn pkill(&self, pattern: &str) {
        let _ = docker(&[
            "exec", "-u", "root", &self.0, "pkill", "-9", "-f", "--", pattern,
        ]);
    }
}

impl Drop for Container {
    fn drop(&mut self) {
        let _ = docker(&["rm", "-f", &self.0]);
    }
}

/// Waits up to `within` for `f` to return Some.
fn poll_for<T>(what: &str, within: Duration, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + within;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

impl Setup {
    /// A hub, and box sb added through `docker exec -i <ctr>` with `extra` exec options before
    /// the container (`twr box add sb --command docker exec -i <extra> <ctr> --yes`): what
    /// `add` printed, once the box is online.
    fn add_container(&self, ctr: &Container, extra: &[&str]) -> (HubProc, String) {
        let hub = self.start_hub(1);
        let mut args = vec!["box", "add", "sb", "--command", "docker", "exec", "-i"];
        args.extend_from_slice(extra);
        args.extend([ctr.0.as_str(), "--yes"]);
        let out = self.ok(&args);
        (hub, out)
    }

    /// `twr <args>` with a terminal (`script`) whose input this test writes: the process, its
    /// output going to `out`.
    fn spawn_in_terminal(&self, args: &[&str], out: &Path) -> Child {
        let line: Vec<String> = std::iter::once(TWR)
            .chain(args.iter().copied())
            .map(twr_core::paths::sh_word)
            .collect();
        Command::new("script")
            .args(["-qec", &line.join(" "), "/dev/null"])
            .env("TWR_HOME", &self.home)
            .env("TWR_TEST_USER_TOKEN_FILE", test_token())
            .env("HOME", &self.hub_home)
            .env_remove("TWR_WORKSPACE")
            .env_remove("RUST_LOG")
            .stdin(Stdio::piped())
            .stdout(std::fs::File::create(out).unwrap())
            .stderr(Stdio::null())
            .spawn()
            .expect("running script")
    }
}

/// Waits up to `within` for `child` to exit: whether it succeeded.
fn exited_within(child: &mut Child, within: Duration) -> bool {
    poll_for("the command to end", within, || {
        child.try_wait().unwrap().map(|s| s.success())
    })
}

/// Whether `args` are those of a `docker exec` client into container `ctr` (a word of them,
/// the test's own unique name) that mention `needle`.
fn docker_client_of(args: &[String], ctr: &Container, needle: &str) -> bool {
    args.first()
        .is_some_and(|a| a == "docker" || a.ends_with("/docker"))
        && args.get(1).is_some_and(|a| a == "exec")
        && args.contains(&ctr.0)
        && args.iter().any(|a| a.contains(needle))
}

/// The host processes of `docker exec` clients into `ctr` whose arguments mention `needle`
/// ([`docker_client_of`]): never another test's, or another box's.
fn docker_clients(ctr: &Container, needle: &str) -> Vec<u32> {
    processes_mentioning(&ctr.0)
        .into_iter()
        .filter(|(_, args)| docker_client_of(args, ctr, needle))
        .map(|(pid, _)| pid)
        .collect()
}

/// SIGKILLs `pid`, one of [`docker_clients`]`(ctr, needle)`, once its arguments read again
/// right before still say it is (a client that ended and whose pid was taken again fails the
/// test instead).
fn kill_docker_client(ctr: &Container, needle: &str, pid: u32) {
    let args = cmdline(pid).unwrap_or_default();
    assert!(
        docker_client_of(&args, ctr, needle),
        "docker exec client {pid} ended before it was killed (now: {args:?})"
    );
    // SAFETY: kill(2) of a pid; no memory is passed.
    unsafe {
        libc_kill(pid as i32, 9);
    }
}

/// A container box from start to end, in the docs' recipe container (`--init`, the image's
/// plain user): `twr box add --command docker exec -i <ctr>` uploads the hub's build through
/// the pipe and the box comes online, with the `terminal_command` it derived; a thread there
/// runs a turn, and attach to it is refused; doctor's command and pipe checks pass, and the
/// daemon runs in a session of its own (no loginctl). `docker restart` brings the box back on
/// a new connection (the thread's next prompt runs); `docker stop` shows as a stopped
/// container, and `docker start` brings it back. The hub's `docker exec` client killed while
/// the relay of a restart waits for the old daemon to end: that relay ends at once, and the
/// next attempt starts the only daemon. `twr hub stop` leaves no relay in the container, and
/// the daemon runs on. Once the container is gone, a new hub says it doesn't exist.
#[test]
#[ignore = "needs docker: cargo test -p twr-cli --test ssh_box container_box -- --ignored"]
fn container_box_from_add_to_removal() {
    let t = Setup::new();
    let ctr = Container::run(&[]);
    let start = Instant::now();
    let (hub, out) = t.add_container(&ctr, &[]);
    let mut hub = Some(hub);
    assert!(out.contains("box sb is online: twr "), "{out}");
    eprintln!("container box online after {:?}", start.elapsed());
    let file = twr_core::config::BoxesFile::load(&t.paths().boxes_file()).unwrap();
    let sb = &file.boxes["sb"];
    let words = |w: &[&str]| w.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(sb.command, Some(words(&["docker", "exec", "-i", &ctr.0])));
    assert_eq!(
        sb.terminal_command,
        Some(words(&["docker", "exec", "-i", "-t", &ctr.0]))
    );
    let status = t.box_status("sb");
    assert_eq!(status["transport"], "command", "{status}");
    assert_eq!(status["launch"], "detached", "{status}");
    assert_eq!(ctr.pgrep("runner --daemon").len(), 1);

    // A thread in a repository of the container's.
    ctr.sh(
        "git init -q ~/p && git -C ~/p -c user.email=t@t -c user.name=t commit -q --allow-empty \
         -m i",
    )
    .unwrap();
    t.ok(&[
        "thread",
        "new",
        "/home/box/p",
        "--box",
        "sb",
        "--runtime",
        "fake",
        "--name",
        "inbox",
    ]);
    t.turn("inbox", "hello from the container", 1);
    let e = t.fails(&["attach", "inbox"]);
    assert!(
        e.contains("twr can't attach to threads on box sb yet"),
        "{e}"
    );

    // Doctor through the command.
    let out = t.twr(&["box", "doctor", "sb", "--json"]);
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let result = |name: &str| {
        v["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["check"] == name)
            .map(|c| {
                (
                    c["result"].as_str().unwrap().to_string(),
                    c["message"].to_string(),
                )
            })
    };
    for ok in ["command", "pipe", "build", "daemon"] {
        assert_eq!(result(ok).map(|r| r.0).as_deref(), Some("ok"), "{ok}: {v}");
    }
    let logind = result("logind").unwrap_or_else(|| panic!("no logind check: {v}"));
    assert!(logind.1.contains("no loginctl"), "{v}");

    // `docker restart`: the box comes back on a new connection, and the thread goes on.
    let epoch = t.box_status("sb")["epoch"].as_u64().unwrap();
    docker_ok(&["restart", &ctr.0]);
    poll("box sb back online after docker restart", || {
        let s = t.box_status("sb");
        (s["state"] == "online" && s["epoch"].as_u64().unwrap() > epoch).then_some(())
    });
    assert_eq!(ctr.pgrep("runner --daemon").len(), 1);
    // Its runtime was lost with the container's processes: a crash card.
    poll("the thread's crash card", || {
        (t.count("inbox", "thread.crashed") == 1).then_some(())
    });
    t.turn("inbox", "after the restart", 2);

    // `docker stop`: a stopped container, said as such; `docker start`: back.
    docker_ok(&["stop", "-t", "1", &ctr.0]);
    poll_for(
        "the stopped container in last_error",
        Duration::from_secs(60),
        || {
            let s = t.box_status("sb");
            s["last_error"]
                .as_str()
                .is_some_and(|e| e.contains("box sb's container is stopped"))
                .then_some(())
        },
    );
    docker_ok(&["start", &ctr.0]);
    poll_for(
        "box sb online after docker start",
        Duration::from_secs(120),
        || (t.box_status("sb")["state"] == "online").then_some(()),
    );
    assert_eq!(ctr.pgrep("runner --daemon").len(), 1);

    // A restart whose relay waits for the old daemon to end (its machine lock, taken over by
    // `flock` as the daemon lets go of it): the hub's client killed, that relay ends, starting
    // nothing; once the lock is free, the hub's next attempt starts the only daemon.
    let locks = ctr
        .sh("ls ~/.local/state/twr/runner-locks/")
        .unwrap_or_default();
    let lock = locks
        .split_whitespace()
        .find(|l| l.ends_with(".lock"))
        .unwrap_or_else(|| panic!("no machine lock in {locks:?}"))
        .to_string();
    docker_ok(&[
        "exec",
        "-d",
        &ctr.0,
        "sh",
        "-c",
        &format!("exec flock ~/.local/state/twr/runner-locks/{lock} sleep 120"),
    ]);
    let old = ctr.pgrep("runner --daemon");
    let mut restart = t
        .command(&["box", "restart", "sb", "--yes"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    poll_for(
        "the restart's relay to wait",
        Duration::from_secs(60),
        || {
            (ctr.pgrep("runner --daemon").is_empty()
                && !ctr.pgrep("runner --stdio.*--restart").is_empty())
            .then_some(())
        },
    );
    let clients = docker_clients(&ctr, "--restart");
    assert_eq!(clients.len(), 1, "{clients:?}");
    kill_docker_client(&ctr, "--restart", clients[0]);
    poll_for("the restart's relay to end", Duration::from_secs(5), || {
        ctr.pgrep("runner --stdio.*--restart")
            .is_empty()
            .then_some(())
    });
    assert!(ctr.pgrep("runner --daemon").is_empty(), "a daemon started");
    ctr.pkill("flock .*sleep 120");
    ctr.pkill("^sleep 120");
    poll_for(
        "box sb online on a new daemon",
        Duration::from_secs(120),
        || (t.box_status("sb")["state"] == "online").then_some(()),
    );
    let daemons = ctr.pgrep("runner --daemon");
    assert_eq!(daemons.len(), 1, "{daemons:?}");
    assert_ne!(daemons, old);
    let _ = restart.kill();
    let _ = restart.wait();

    // The hub stopped: no relay left in the container; the daemon runs on.
    t.ok(&["hub", "stop"]);
    hub.take().unwrap().exited();
    poll_for("no relay in the container", Duration::from_secs(5), || {
        ctr.pgrep("runner --stdio").is_empty().then_some(())
    });
    assert_eq!(ctr.pgrep("runner --daemon"), daemons);

    // The container gone: a new hub says so.
    docker_ok(&["rm", "-f", &ctr.0]);
    let _hub = t.start_hub(2);
    poll_for(
        "the missing container in last_error",
        Duration::from_secs(60),
        || {
            t.box_status("sb")["last_error"]
                .as_str()
                .is_some_and(|e| e.contains("box sb's container does not exist"))
                .then_some(())
        },
    );
}

/// What needs a terminal on a container box, through `docker exec -i -t` in a real terminal
/// (`script`): a clone whose `docker exec` client is killed runs to its end in the container
/// (no git and no clone left after it, the folder absent or a whole clone, nothing recorded),
/// and the box daemon still answers; Ctrl-C during a clone reaches git in the container, and
/// the folder is removed; the detach keys (Ctrl-P Ctrl-Q) during `twr box login` end the CLI
/// and leave the agent's login running in the container, as documented.
#[test]
#[ignore = "needs docker: cargo test -p twr-cli --test ssh_box container_box -- --ignored"]
fn container_box_terminal_paths() {
    let t = Setup::new();
    let ctr = Container::run(&["-e", "TWR_TEST_CLONE_ALLOW_FILE=1"]);
    let (_hub, _) = t.add_container(&ctr, &[]);

    // The hub machine's repository, whose origin the container's git maps to a bare
    // repository there; a post-checkout hook that says it started, then takes its time.
    let url = "https://example.invalid/o/api.git";
    let hub_repo = init_repo(&t.hub_home.join("code/api"));
    twr_core::testkit::git::run(&hub_repo, &["remote", "add", "origin", url]);
    ctr.sh(&format!(
        "set -e; git init -q --bare ~/bare.git; git init -q ~/seed; cd ~/seed; echo hi > \
         README.md; git add README.md; git -c user.email=t@t -c user.name=t commit -qm i; git \
         push -q ~/bare.git HEAD:refs/heads/main; git -C ~/bare.git symbolic-ref HEAD \
         refs/heads/main; mkdir ~/code ~/hooks; printf '#!/bin/sh\\ntouch ~/hook.started\\nsleep \
         15\\n' > ~/hooks/post-checkout; chmod +x ~/hooks/post-checkout; git config --global \
         url.file:///home/box/bare.git.insteadOf {url}; git config --global core.hooksPath \
         ~/hooks"
    ))
    .expect("the container's repositories");
    let boxes_before = std::fs::read_to_string(t.paths().boxes_file()).unwrap();
    let clone_args = ["box", "project", "sb", "api", "--clone", "--yes"];
    let out = t.home.join("clone.out");

    // The client killed once git runs: the clone runs to its end in the container.
    let mut cli = t.spawn_in_terminal(&clone_args, &out);
    poll_for("the clone's hook", Duration::from_secs(60), || {
        ctr.sh("test -e ~/hook.started").map(|_| ())
    });
    let clients = docker_clients(&ctr, "--clone-repo");
    assert_eq!(clients.len(), 1, "{clients:?}");
    kill_docker_client(&ctr, "--clone-repo", clients[0]);
    assert!(!exited_within(&mut cli, Duration::from_secs(30)));
    let said = std::fs::read_to_string(&out).unwrap();
    assert!(
        said.contains("the connection to box sb ended before the clone said how it went"),
        "{said}"
    );
    poll_for(
        "no clone left in the container",
        Duration::from_secs(60),
        || {
            (ctr.pgrep("--clone-repo").is_empty() && ctr.pgrep("(^|/)git( |$)").is_empty())
                .then_some(())
        },
    );
    let whole =
        ctr.sh("git -C ~/code/api rev-parse -q --verify HEAD && test -f ~/code/api/README.md");
    let absent = ctr.sh("test ! -e ~/code/api");
    assert!(whole.is_some() || absent.is_some(), "a partial clone");
    assert_eq!(
        std::fs::read_to_string(t.paths().boxes_file()).unwrap(),
        boxes_before
    );
    assert_eq!(t.box_status("sb")["state"], "online");
    let doctor = t.twr(&["box", "doctor", "sb", "--json"]);
    let v: Value = serde_json::from_slice(&doctor.stdout).unwrap();
    let daemon = v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["check"] == "daemon")
        .unwrap_or_else(|| panic!("no daemon check: {v}"));
    assert_eq!(daemon["result"], "ok", "{v}");

    // Ctrl-C during the clone: git in the container stops, and the folder is removed.
    ctr.sh("rm -rf ~/code/api ~/hook.started").unwrap();
    let mut cli = t.spawn_in_terminal(&clone_args, &out);
    poll_for("the clone's hook", Duration::from_secs(60), || {
        ctr.sh("test -e ~/hook.started").map(|_| ())
    });
    {
        use std::io::Write;
        let stdin = cli.stdin.as_mut().unwrap();
        stdin.write_all(b"\x03").unwrap();
        stdin.flush().unwrap();
    }
    assert!(!exited_within(&mut cli, Duration::from_secs(30)));
    poll_for("the folder removed", Duration::from_secs(30), || {
        ctr.sh("test ! -e ~/code/api").map(|_| ())
    });
    assert!(ctr.pgrep("--clone-repo").is_empty());
    assert_eq!(
        std::fs::read_to_string(t.paths().boxes_file()).unwrap(),
        boxes_before
    );

    // The detach keys during a login: the CLI returns, the login runs on in the container.
    docker_ok(&[
        "exec",
        "-u",
        "root",
        &ctr.0,
        "sh",
        "-c",
        "printf '#!/bin/sh\\ncase \"$*\" in *login*) touch /tmp/login.started; exec sleep 300 ;; \
         esac\\necho 2.0.0\\n' > /usr/local/bin/claude; chmod 755 /usr/local/bin/claude",
    ]);
    let out = t.home.join("login.out");
    let mut cli = t.spawn_in_terminal(&["box", "login", "sb", "claude"], &out);
    poll_for("the fake login", Duration::from_secs(60), || {
        ctr.sh("test -e /tmp/login.started").map(|_| ())
    });
    {
        use std::io::Write;
        let stdin = cli.stdin.as_mut().unwrap();
        stdin.write_all(b"\x10\x11").unwrap();
        stdin.flush().unwrap();
    }
    exited_within(&mut cli, Duration::from_secs(30));
    let said = std::fs::read_to_string(&out).unwrap();
    assert!(said.contains("claude's login on box sb ended"), "{said}");
    let left = ctr.pgrep("^sleep 300");
    assert_eq!(
        left.len(),
        1,
        "the login is still running in the container: {said}"
    );
    ctr.pkill("^sleep 300");
}

/// A container that runs as root, with `HOME=/root` in its environment (`-e HOME`, or an
/// image's `ENV HOME`): `docker exec -u box` keeps that HOME (docker sets the exec user's own
/// only when the container's environment has none), so the command needs `-e HOME=/home/box
/// -w /home/box` too. With them the box comes online, doctor finds its data directory under
/// box's home, the daemon runs as box, and `terminal_command` keeps every word; without them,
/// `add` fails with the hint.
#[test]
#[ignore = "needs docker: cargo test -p twr-cli --test ssh_box container_box -- --ignored"]
fn container_box_in_a_root_container() {
    let t = Setup::new();
    let as_root = ["--user", "root", "-e", "HOME=/root"];
    let ctr = Container::run(&as_root);
    let as_box = ["-u", "box", "-e", "HOME=/home/box", "-w", "/home/box"];
    let (mut hub, out) = t.add_container(&ctr, &as_box);
    assert!(out.contains("box sb is online: twr "), "{out}");
    let file = twr_core::config::BoxesFile::load(&t.paths().boxes_file()).unwrap();
    let mut want: Vec<String> = ["docker", "exec", "-i", "-t"]
        .iter()
        .chain(&as_box)
        .map(|s| s.to_string())
        .collect();
    want.push(ctr.0.clone());
    assert_eq!(file.boxes["sb"].terminal_command, Some(want));
    let doctor = t.twr(&["box", "doctor", "sb"]);
    let text = said(&doctor);
    assert!(text.contains("/home/box/.local/share"), "{text}");
    assert!(!text.contains("/root/"), "{text}");
    let daemons = ctr.sh("ps -o user= -C twr").unwrap_or_default();
    assert!(daemons.split_whitespace().all(|u| u == "box"), "{daemons}");

    // Without HOME: the upload is refused, with the fix.
    t.ok(&["hub", "stop"]);
    hub.exited();
    std::fs::remove_file(t.paths().boxes_file()).unwrap();
    let ctr2 = Container::run(&as_root);
    hub = t.start_hub(2);
    let out = t.twr(&[
        "box",
        "add",
        "sb",
        "--command",
        "docker",
        "exec",
        "-i",
        "-u",
        "box",
        &ctr2.0,
        "--yes",
    ]);
    let text = said(&out);
    assert!(!out.status.success(), "{text}");
    assert!(
        text.contains("box sb's HOME is /root, which its user can't write to"),
        "{text}"
    );
    drop(hub);
}

// -------------------------------------------------------------------------------------------
// twr box project
// -------------------------------------------------------------------------------------------

/// This build of twr, copied once (mode 0700) for the boxes the tests set up without a hub.
fn box_build() -> &'static Path {
    static BUILD: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    BUILD.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap().keep();
        let exe = dir.join("twr");
        cp(Path::new(TWR), &exe);
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
        exe
    })
}

/// A git repository at `dir` with `origin` as its origin: its canonical path.
fn repo_with_origin(dir: &Path, origin: &str) -> PathBuf {
    let repo = init_repo(dir);
    twr_core::testkit::git::run(&repo, &["remote", "add", "origin", origin]);
    repo
}

impl Setup {
    /// Box gpu (`fake@box`) in boxes.toml, with this build of twr in its builds directory
    /// (as the hub would install it), and no hub.
    fn box_without_hub(&self) {
        std::fs::write(
            self.paths().boxes_file(),
            "[boxes.gpu]\ntransport = \"ssh\"\nssh = \"fake@box\"\n",
        )
        .unwrap();
        let id = twr_core::build_id_of(Path::new(TWR)).unwrap();
        let mut dir = self.box_home.clone();
        for part in [".local", "share", "twr", "runner", "builds", id.as_str()] {
            dir.push(part);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let exe = dir.join("twr");
        if std::fs::hard_link(box_build(), &exe).is_err() {
            cp(box_build(), &exe);
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }

    /// `twr box project <args>` with the hub machine's `HOME`.
    fn project(&self, args: &[&str]) -> Output {
        let mut all = vec!["box", "project"];
        all.extend_from_slice(args);
        self.command(&all)
            .env("HOME", &self.hub_home)
            .output()
            .expect("running twr")
    }

    /// `twr box project <args>` with a terminal on its stdin and stdout (`script`), with the
    /// hub machine's `HOME` and `env`: whether it succeeded, and what it printed.
    fn project_in_terminal(&self, args: &[&str], env: &[(&str, &str)]) -> (bool, String) {
        let line: Vec<String> = [TWR, "box", "project"]
            .iter()
            .chain(args)
            .map(|w| twr_core::paths::sh_word(w))
            .collect();
        let mut c = Command::new("script");
        c.args(["-qec", &line.join(" "), "/dev/null"])
            .env("TWR_HOME", &self.home)
            .env("TWR_SSH", &self.ssh)
            .env("TWR_TEST_USER_TOKEN_FILE", test_token())
            .env("HOME", &self.hub_home)
            .env_remove("TWR_WORKSPACE")
            .env_remove("RUST_LOG")
            .stdin(Stdio::null());
        for (k, v) in env {
            c.env(k, v);
        }
        let out = c.output().expect("running script");
        let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
        text.push_str(&String::from_utf8_lossy(&out.stderr));
        (out.status.success(), text.replace('\r', ""))
    }

    /// Box gpu's `[boxes.gpu.projects]` in boxes.toml, as (project, repo).
    fn box_projects(&self) -> Vec<(String, PathBuf)> {
        let file = twr_core::config::BoxesFile::load(&self.paths().boxes_file()).unwrap();
        file.boxes
            .get("gpu")
            .map(|b| {
                b.projects
                    .iter()
                    .map(|(p, c)| (p.clone(), c.repo.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// The text of `out`, stdout then stderr.
fn said(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

/// `twr box project` with a hub: the project (a folder name under the hub's project_roots) is
/// found on the box at the path it has under the hub's home, under the box's home (another
/// repository of that name, with another origin, aside), recorded, and active at once: `twr
/// thread new api --box gpu` makes a thread there. `twr box add` named the projects configured
/// for the box. `--remove` takes it out, and the hub with it: a new thread is refused with the
/// hint again.
#[test]
fn box_project_records_and_the_hub_takes_it() {
    let t = Setup::new();
    let _cleanup = BoxProcesses(t.box_home.clone());
    let paths = t.paths();
    std::fs::write(
        &paths.config_file,
        format!(
            "[projects.web]\nrepo = \"{}\"\nbox = \"gpu\"\n",
            t.hub_home.join("code/web").display()
        ),
    )
    .unwrap();
    let _hub = t.start_hub(1);
    let out = t.ok(&["box", "add", "gpu", "fake@box", "--yes"]);
    assert!(
        out.contains("projects configured for box gpu: find each one's repository on the box"),
        "{out}"
    );
    assert!(out.contains("  twr box project gpu web\n"), "{out}");

    repo_with_origin(&t.hub_home.join("code/api"), "git@github.com:o/api.git");
    let there = repo_with_origin(&t.box_home.join("code/api"), "https://github.com/o/api");
    let other = repo_with_origin(&t.box_home.join("src/api"), "https://github.com/x/api");
    // Before: the box does not have it.
    let e = t.fails(&["thread", "new", "api", "--box", "gpu", "--runtime", "fake"]);
    assert!(e.contains("twr box project gpu api"), "{e}");

    let out = t.project(&["gpu", "api", "--yes"]);
    let text = said(&out);
    assert!(out.status.success(), "{text}");
    assert!(
        text.contains("origin git@github.com:o/api.git"),
        "the hub's origin: {text}"
    );
    assert!(
        text.contains(&format!(
            "  {} (origin https://github.com/o/api, the project's; where it is under your home \
             here)",
            there.display()
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "  {} (origin https://github.com/x/api)",
            other.display()
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "recorded and active: project api on box gpu is {}; `twr thread new api --box gpu` \
             uses it",
            there.display()
        )),
        "{text}"
    );
    assert_eq!(t.box_projects(), [("api".to_string(), there.clone())]);
    // The search ran with the box's build, the hint and the origin (no credentials).
    let runs = t.ssh_runs();
    let find = runs
        .iter()
        .find(|(_, _, c)| c.contains("--find-repo"))
        .unwrap();
    assert!(
        find.2
            .contains(&format!("--hint={}", t.box_home.join("code/api").display())),
        "{find:?}"
    );
    assert!(
        find.2.contains("--origin=git@github.com:o/api.git"),
        "{find:?}"
    );

    let created = t.ok(&[
        "thread",
        "new",
        "api",
        "--box",
        "gpu",
        "--runtime",
        "fake",
        "--name",
        "on-box",
    ]);
    assert!(created.starts_with("created on-box ("), "{created}");

    // Again: it is recorded already, and nothing changes.
    let out = t.project(&["gpu", "api", "--yes"]);
    assert!(said(&out).contains("is recorded as"), "{}", said(&out));

    let out = t.project(&["gpu", "api", "--remove"]);
    let text = said(&out);
    assert!(out.status.success(), "{text}");
    assert!(
        text.contains("removed and active: project api on box gpu is no longer recorded"),
        "{text}"
    );
    assert!(t.box_projects().is_empty());
    let e = t.fails(&["thread", "new", "api", "--box", "gpu", "--runtime", "fake"]);
    assert!(e.contains("twr box project gpu api"), "{e}");
    // Nothing more to remove.
    let out = t.project(&["gpu", "api", "--remove"]);
    assert!(!out.status.success());
    assert!(said(&out).contains("has no [boxes.gpu.projects.api] to remove"));
}

/// `twr box project` without a hub (the box has this build of twr): a configured project not
/// at its home-mapped path is found by its origin under the box's ~/code and recorded; several
/// repositories with the origin are listed, nothing recorded; without --yes and a terminal,
/// nothing is recorded either; `--path` records only a repository's top level; quotes, `;`,
/// `$`, spaces and `$(...)` in paths reach the box as they are and run nothing; a search cut
/// short records nothing.
#[test]
fn box_project_finds_lists_and_refuses() {
    let t = Setup::new();
    let paths = t.paths();
    t.box_without_hub();
    let origin = "git@github.com:o/api.git";
    let hub_repo = repo_with_origin(&t.hub_home.join("work/api"), origin);
    std::fs::write(
        &paths.config_file,
        format!("[projects.api]\nrepo = \"{}\"\n", hub_repo.display()),
    )
    .unwrap();
    let code = repo_with_origin(&t.box_home.join("code/api"), "https://github.com/o/api.git");

    // Found by its origin under ~/code (nothing at ~/work/api on the box).
    let out = t.project(&["gpu", "api", "--yes"]);
    let text = said(&out);
    assert!(out.status.success(), "{text}");
    assert!(
        text.contains(&format!(
            "looking on box gpu for repository folder api: at {}/work/api",
            t.box_home.display()
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "recorded: project api on box gpu is {} (no hub runs",
            code.display()
        )),
        "{text}"
    );
    assert_eq!(t.box_projects(), [("api".to_string(), code.clone())]);
    let out = t.project(&["gpu", "api", "--remove"]);
    assert!(out.status.success(), "{}", said(&out));
    assert!(t.box_projects().is_empty());

    // Without --yes and a terminal: listed, not recorded.
    let out = t.project(&["gpu", "api"]);
    let text = said(&out);
    assert!(!out.status.success(), "{text}");
    assert!(text.contains("without confirmation"), "{text}");
    assert!(text.contains("pass --yes"), "{text}");
    assert!(t.box_projects().is_empty());

    // Several with the origin: listed, nothing recorded.
    let src = repo_with_origin(&t.box_home.join("src/org/api"), origin);
    let out = t.project(&["gpu", "api", "--yes"]);
    let text = said(&out);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(text.contains(&code.display().to_string()), "{text}");
    assert!(text.contains(&src.display().to_string()), "{text}");
    assert!(
        text.contains(
            "nothing was recorded: 2 have the project's origin. Pick one with `twr box project \
             gpu api --path <path>`"
        ),
        "{text}"
    );
    assert!(t.box_projects().is_empty());

    // --path: a directory inside the repository is refused; its top level is recorded.
    std::fs::create_dir_all(src.join("sub")).unwrap();
    let sub = src.join("sub");
    let out = t.project(&["gpu", "api", "--path", sub.to_str().unwrap()]);
    let text = said(&out);
    assert!(!out.status.success(), "{text}");
    assert!(text.contains("not its top level"), "{text}");
    let out = t.project(&["gpu", "api", "--path", "relative/api"]);
    assert!(said(&out).contains("not an absolute path on the box"));
    let nowhere = t.box_home.join("nowhere");
    let out = t.project(&["gpu", "api", "--path", nowhere.to_str().unwrap()]);
    assert!(
        said(&out).contains("is not in a git repository"),
        "{}",
        said(&out)
    );
    assert!(t.box_projects().is_empty());
    let out = t.project(&["gpu", "api", "--path", src.to_str().unwrap()]);
    assert!(out.status.success(), "{}", said(&out));
    assert_eq!(t.box_projects(), [("api".to_string(), src.clone())]);

    // Paths with quotes, `;`, `$`, spaces and `$(...)`: intact, and nothing runs.
    let odd = "it's; $HOME \"q\" $(touch pwned) `touch pwned2` a b";
    let weird = repo_with_origin(&t.box_home.join(odd).join("api"), origin);
    let out = t.project(&["gpu", "api", "--path", weird.to_str().unwrap()]);
    assert!(out.status.success(), "{}", said(&out));
    assert_eq!(t.box_projects(), [("api".to_string(), weird.clone())]);
    // The home-mapped hint too: a project whose repository is under such a folder here.
    let hub_web = repo_with_origin(&t.hub_home.join(odd).join("web"), "https://h.example/o/web");
    let box_web = repo_with_origin(&t.box_home.join(odd).join("web"), "https://h.example/o/web");
    std::fs::write(
        &paths.config_file,
        format!(
            "[projects.api]\nrepo = \"{}\"\n[projects.web]\nrepo = {}\n",
            hub_repo.display(),
            serde_json::to_string(hub_web.to_str().unwrap()).unwrap()
        ),
    )
    .unwrap();
    let out = t.project(&["gpu", "web", "--yes"]);
    assert!(out.status.success(), "{}", said(&out));
    assert!(t
        .box_projects()
        .contains(&("web".to_string(), box_web.clone())));
    for dir in [&t.box_home, &t.hub_home, &t.home] {
        for f in ["pwned", "pwned2"] {
            assert!(!dir.join(f).exists(), "{} ran", dir.join(f).display());
        }
    }

    // A search cut short (too many folders under ~/code): listed, nothing recorded.
    t.project(&["gpu", "api", "--remove"]);
    std::fs::remove_dir_all(&src).unwrap();
    for i in 0..twr_core::project_lookup::MAX_DIRS + 2 {
        std::fs::create_dir(t.box_home.join(format!("code/g{i:04}"))).unwrap();
    }
    let out = t.project(&["gpu", "api", "--yes"]);
    let text = said(&out);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(text.contains(&code.display().to_string()), "{text}");
    assert!(text.contains("the search was incomplete"), "{text}");
    assert!(!t.box_projects().iter().any(|(p, _)| p == "api"));
}

/// A search on the box whose report is incomplete records nothing: a repository there whose
/// origin git cannot read (next to one with the project's origin) is listed as such, and may
/// have been the project's. An explicit `--path` of a repository whose origin is longer than
/// what the CLI reads of ssh is checked and recorded all the same (the origin shown cut), and
/// so is the search's unique match once the unreadable one is gone.
#[test]
fn box_project_records_nothing_from_an_incomplete_search() {
    let t = Setup::new();
    t.box_without_hub();
    repo_with_origin(&t.hub_home.join("code/api"), "git@github.com:o/api.git");
    let code = repo_with_origin(&t.box_home.join("code/api"), "https://github.com/o/api");
    let unread = init_repo(&t.box_home.join("src/api"));
    let config = unread.join(".git/config");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str("[remote \"origin\"]\n\turl\n");
    std::fs::write(&config, text).unwrap();
    let out = t.project(&["gpu", "api", "--yes"]);
    let text = said(&out);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(
        text.contains(&format!(
            "  {} (origin https://github.com/o/api, the project's",
            code.display()
        )),
        "{text}"
    );
    assert!(
        text.contains(&format!(
            "  {} (its origin could not be read)",
            unread.display()
        )),
        "{text}"
    );
    assert!(text.contains("the search was incomplete"), "{text}");
    assert!(t.box_projects().is_empty());

    // --path of one whose origin alone is more than ssh's output may be.
    let huge = format!("https://h.example/o/{}", "a".repeat(100_000));
    let big = repo_with_origin(&t.box_home.join("big/api"), &huge);
    let out = t.project(&["gpu", "api", "--path", big.to_str().unwrap()]);
    let text = said(&out);
    assert!(out.status.success(), "{text}");
    assert!(text.contains("aaa..."), "{text}");
    assert!(text.len() < 50_000, "{}", text.len());
    assert_eq!(t.box_projects(), [("api".to_string(), big.clone())]);
    t.project(&["gpu", "api", "--remove"]);

    // The unreadable one gone: the one match is recorded.
    std::fs::remove_dir_all(&unread).unwrap();
    let out = t.project(&["gpu", "api", "--yes"]);
    assert!(out.status.success(), "{}", said(&out));
    assert_eq!(t.box_projects(), [("api".to_string(), code)]);
}

/// `twr box project` is the user's alone, in the default workspace, and `--clone` needs a
/// terminal: refused before any ssh.
#[test]
fn box_project_guards_come_first() {
    let t = Setup::new();
    t.box_without_hub();
    repo_with_origin(&t.hub_home.join("code/api"), "https://h.example/o/api");
    let out = t
        .command(&["box", "project", "gpu", "api"])
        .env("HOME", &t.hub_home)
        .env("TWR_AGENT", "worker:1")
        .env_remove("TWR_TEST_USER_TOKEN_FILE")
        .output()
        .unwrap();
    assert!(!out.status.success());
    let e = said(&out);
    assert!(e.contains("agents cannot record projects on boxes"), "{e}");
    let out = t.project(&["gpu", "api", "--clone", "--yes"]);
    assert!(!out.status.success());
    let e = said(&out);
    assert!(e.contains("needs a terminal on stdin and stdout"), "{e}");
    t.ok(&["ws", "new", "other"]);
    let out = t
        .command(&["-w", "other", "box", "project", "gpu", "api"])
        .env("HOME", &t.hub_home)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        said(&out).contains("boxes belong to the default workspace's hub"),
        "{}",
        said(&out)
    );
    assert!(t.ssh_runs().is_empty(), "{:?}", t.ssh_runs());
    assert!(t.box_projects().is_empty());
}

/// `twr runner --find-repo --origin` and `--clone-repo --url` (what `twr box project` runs on
/// a box) refuse a URL whose password holds `@`, `:`, `?`, `#` or `/` without showing any
/// part of it: the URL is shown redacted, the reason quotes nothing of it.
#[test]
fn runner_repo_commands_never_show_a_password() {
    let t = Setup::new();
    let parent = t.box_home.join("code");
    std::fs::create_dir_all(&parent).unwrap();
    for (url, shown, why) in [
        (
            "me:PW1@LEAKED-SECRET@host:repo",
            "me:***@host:repo",
            "more than one `@`",
        ),
        (
            "https://u:PW1:LEAKED@SECRET@host/r",
            "https://***@host/r",
            "more than one `@`",
        ),
        (
            "ssh://git:PW1@LEAKED@SECRET@host:22/r",
            "ssh://git:***@host:22/r",
            "more than one `@`",
        ),
        (
            "ssh://git:PW1/LEAKED@SECRET@host/r",
            "ssh://git:***@host/r",
            "an `@` after the host",
        ),
        ("me:PW1?LEAKED#x@SECRET@host:r", "me:***@host:r", "query"),
        (
            "me:PW1@LEAKED:x@host:r",
            "me:***@host:r",
            "an `@` after the host",
        ),
    ] {
        let origin = format!("--origin={url}");
        let find = ["runner", "--find-repo", "--name=api", origin.as_str()];
        let url_arg = format!("--url={url}");
        let parent_arg = format!("--parent={}", parent.display());
        let clone = [
            "runner",
            "--clone-repo",
            url_arg.as_str(),
            parent_arg.as_str(),
            "--name=api",
        ];
        for args in [&find[..], &clone[..]] {
            let out = t.command(args).env("HOME", &t.box_home).output().unwrap();
            assert!(!out.status.success(), "{args:?}");
            let e = said(&out);
            assert!(e.contains(why), "{args:?}: {e}");
            assert!(e.contains(&format!("{shown:?}")), "{args:?}: {e}");
            for secret in ["PW1", "LEAKED", "SECRET"] {
                assert!(!e.contains(secret), "{args:?}: {e}");
            }
        }
    }
    assert_eq!(std::fs::read_dir(&parent).unwrap().count(), 0);
}

/// `twr box project --clone`, in a terminal: an origin with credentials, or a `file://` one,
/// is refused before any ssh (the credentials never shown); an https one is cloned on the box
/// in that terminal (a local bare repository behind it, through the box's git config and the
/// debug-only `file` seam) under the home-mapped folder, and recorded (a hook of the box's that
/// prints without a newline notwithstanding). `--into <dir>` is the
/// clone's own new directory, of any folder name, as `git clone <url> <dir>` takes it.
#[test]
fn box_project_clones_in_the_terminal() {
    let t = Setup::new();
    t.box_without_hub();
    let hub_repo = repo_with_origin(
        &t.hub_home.join("code/api"),
        "https://me:hunter2@example.invalid/o/api.git",
    );
    let (ok, out) = t.project_in_terminal(&["gpu", "api", "--clone", "--yes"], &[]);
    assert!(!ok, "{out}");
    assert!(out.contains("password or a token"), "{out}");
    assert!(out.contains("Clone it on box gpu yourself"), "{out}");
    assert!(!out.contains("hunter2"), "{out}");
    // A password holding `@` (and `:`): refused as ambiguous, no part of it shown.
    for origin in [
        "me:hunter2@SEC2@example.invalid:o/api.git",
        "https://u:hunter2:SEC2@SEC3@example.invalid/o/api.git",
    ] {
        twr_core::testkit::git::run(&hub_repo, &["remote", "set-url", "origin", origin]);
        let (ok, out) = t.project_in_terminal(&["gpu", "api", "--clone", "--yes"], &[]);
        assert!(!ok, "{out}");
        assert!(out.contains("more than one `@`"), "{out}");
        assert!(out.contains("Clone it on box gpu yourself"), "{out}");
        assert!(!out.contains("hunter2") && !out.contains("SEC"), "{out}");
    }
    twr_core::testkit::git::run(
        &hub_repo,
        &["remote", "set-url", "origin", "file:///srv/api.git"],
    );
    let (ok, out) = t.project_in_terminal(&["gpu", "api", "--clone", "--yes"], &[]);
    assert!(!ok, "{out}");
    assert!(out.contains("file://"), "{out}");
    assert!(t.ssh_runs().is_empty(), "{:?}", t.ssh_runs());

    // A clone that works: the box's git reaches a local bare repository for that URL.
    let url = "https://example.invalid/o/api.git";
    twr_core::testkit::git::run(&hub_repo, &["remote", "set-url", "origin", url]);
    let bare = t.box_home.join("bare.git");
    twr_core::testkit::git::run(
        &t.box_home,
        &[
            "clone",
            "-q",
            "--bare",
            hub_repo.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    // A post-checkout hook of the box's that prints without a newline: the mark still starts
    // a line of its own.
    let hooks = t.box_home.join("hooks");
    std::fs::create_dir(&hooks).unwrap();
    install(
        &hooks.join("post-checkout"),
        "#!/bin/sh\nprintf 'hook ready'\n",
    );
    std::fs::write(
        t.box_home.join(".gitconfig"),
        format!(
            "[url \"file://{}\"]\n\tinsteadOf = {url}\n[core]\n\thooksPath = {}\n",
            bare.display(),
            hooks.display()
        ),
    )
    .unwrap();
    let code = t.box_home.join("code");
    std::fs::create_dir(&code).unwrap();
    std::fs::set_permissions(&code, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (ok, out) = t.project_in_terminal(
        &["gpu", "api", "--clone", "--yes"],
        &[("TWR_TEST_CLONE_ALLOW_FILE", "1")],
    );
    let target = code.join("api");
    assert!(ok, "{out}");
    assert!(out.contains("hook ready"), "{out}");
    assert!(
        out.contains(&format!(
            "twr clones {url} into {} on box gpu",
            target.display()
        )),
        "{out}"
    );
    assert!(out.contains("with the box's own git credentials"), "{out}");
    assert!(
        out.contains(&format!("twr-clone-path {}", target.display())),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "recorded: project api on box gpu is {}",
            target.display()
        )),
        "{out}"
    );
    assert!(target.join("README.md").is_file());
    assert_eq!(t.box_projects(), [("api".to_string(), target.clone())]);
    let runs = t.ssh_runs();
    assert!(
        runs.iter().any(|(_, _, c)| c.contains("--clone-repo")
            && c.contains(&format!("--url={url}"))
            && c.contains(&format!("--parent={}", code.display()))
            && c.contains("--name=api")),
        "{runs:?}"
    );
    // The target exists now: a second clone is refused on the box, and nothing changes.
    let (ok, out) = t.project_in_terminal(
        &["gpu", "api", "--clone", "--yes"],
        &[("TWR_TEST_CLONE_ALLOW_FILE", "1")],
    );
    assert!(!ok, "{out}");
    assert!(out.contains("exists already"), "{out}");
    assert_eq!(t.box_projects(), [("api".to_string(), target)]);

    // --into: a new directory of another name, in an existing one.
    let srv = t.box_home.join("srv");
    std::fs::create_dir(&srv).unwrap();
    std::fs::set_permissions(&srv, std::fs::Permissions::from_mode(0o755)).unwrap();
    let copy = srv.join("api-copy");
    let into = format!("{}/", copy.display());
    let (ok, out) = t.project_in_terminal(
        &["gpu", "api", "--clone", "--into", &into, "--yes"],
        &[("TWR_TEST_CLONE_ALLOW_FILE", "1")],
    );
    assert!(ok, "{out}");
    assert!(
        out.contains(&format!(
            "twr clones {url} into {} on box gpu",
            copy.display()
        )),
        "{out}"
    );
    assert!(
        out.contains(&format!(
            "recorded: project api on box gpu is {}",
            copy.display()
        )),
        "{out}"
    );
    assert!(copy.join("README.md").is_file());
    assert!(!copy.join("api").exists());
    assert_eq!(t.box_projects(), [("api".to_string(), copy.clone())]);
    let runs = t.ssh_runs();
    assert!(
        runs.iter().any(|(_, _, c)| c.contains("--clone-repo")
            && c.contains(&format!("--parent={}", srv.display()))
            && c.contains("--name=api-copy")),
        "{runs:?}"
    );
    // It exists now: refused, not cloned into it.
    let (ok, out) = t.project_in_terminal(
        &[
            "gpu",
            "api",
            "--clone",
            "--into",
            copy.to_str().unwrap(),
            "--yes",
        ],
        &[("TWR_TEST_CLONE_ALLOW_FILE", "1")],
    );
    assert!(!ok, "{out}");
    assert!(out.contains("exists already"), "{out}");
    assert!(!copy.join("api").exists());
    // A last component that is not a folder name: refused before any ssh.
    let before = t.ssh_runs().len();
    let dots = format!("{}/..", srv.display());
    let (ok, out) = t.project_in_terminal(
        &["gpu", "api", "--clone", "--into", &dots, "--yes"],
        &[("TWR_TEST_CLONE_ALLOW_FILE", "1")],
    );
    assert!(!ok, "{out}");
    assert!(out.contains("not a folder name twr makes"), "{out}");
    assert_eq!(t.ssh_runs().len(), before);
    assert_eq!(t.box_projects(), [("api".to_string(), copy)]);
}

/// git's message when it cannot read an origin quotes the repository's config: an invalid
/// `remote.origin.fetch` holding a URL with a password, on the hub's repository and on one on
/// the box. Neither `twr box project` nor the box's report shows any of it, and the search
/// is incomplete.
#[test]
fn box_project_never_shows_git_s_message_about_an_origin() {
    let t = Setup::new();
    t.box_without_hub();
    let origin = "https://github.com/o/api";
    let fetch = |repo: &Path, secret: &str| {
        twr_core::testkit::git::run(
            repo,
            &[
                "config",
                "remote.origin.fetch",
                &format!("https://user:{secret}@host/repo"),
            ],
        );
    };
    let hub_repo = repo_with_origin(&t.hub_home.join("code/api"), origin);
    fetch(&hub_repo, "SECRET1");
    let code = repo_with_origin(&t.box_home.join("code/api"), origin);
    let src = repo_with_origin(&t.box_home.join("src/api"), origin);
    fetch(&src, "SECRET2");
    let out = t
        .command(&["box", "project", "gpu", "api", "--yes"])
        .env("HOME", &t.hub_home)
        .env("RUST_LOG", "debug")
        .output()
        .unwrap();
    let text = said(&out);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(
        text.contains(&format!(
            "could not read the origin of {}: git remote get-url origin failed (git exit",
            hub_repo.display()
        )),
        "{text}"
    );
    assert!(text.contains(&code.display().to_string()), "{text}");
    assert!(
        text.contains(&format!(
            "  {} (its origin could not be read)",
            src.display()
        )),
        "{text}"
    );
    assert!(
        !text.contains("SECRET") && !text.contains("refspec"),
        "{text}"
    );
    assert!(t.box_projects().is_empty());

    // The box's report itself, with its logs on.
    let origin_arg = format!("--origin={origin}");
    let out = t
        .command(&["runner", "--find-repo", "--name=api", &origin_arg])
        .env("HOME", &t.box_home)
        .env("RUST_LOG", "debug")
        .output()
        .unwrap();
    let text = said(&out);
    assert!(out.status.success(), "{text}");
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["complete"], false, "{text}");
    assert!(
        !text.contains("SECRET") && !text.contains("refspec"),
        "{text}"
    );
}

/// A repository folder on the box that can be listed but not searched (mode 0644) next to one
/// with the project's origin: the search is incomplete (that one may be the project's), and
/// nothing is recorded.
#[test]
fn box_project_records_nothing_when_a_folder_cannot_be_searched() {
    if twr_core::paths::effective_uid() == 0 {
        eprintln!("root searches any folder: skipped");
        return;
    }
    let t = Setup::new();
    t.box_without_hub();
    let origin = "https://github.com/o/api";
    repo_with_origin(&t.hub_home.join("code/api"), origin);
    let code = repo_with_origin(&t.box_home.join("code/api"), origin);
    let hidden = repo_with_origin(&t.box_home.join("src/api"), origin);
    std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o644)).unwrap();
    let out = t.project(&["gpu", "api", "--yes"]);
    let origin_arg = format!("--origin={origin}");
    let report = t
        .command(&["runner", "--find-repo", "--name=api", &origin_arg])
        .env("HOME", &t.box_home)
        .output()
        .unwrap();
    std::fs::set_permissions(&hidden, std::fs::Permissions::from_mode(0o755)).unwrap();
    let text = said(&out);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(
        text.contains(&format!(
            "  {} (origin {origin}, the project's",
            code.display()
        )),
        "{text}"
    );
    assert!(text.contains("the search was incomplete"), "{text}");
    assert!(t.box_projects().is_empty());
    let report: Value = serde_json::from_slice(&report.stdout).unwrap();
    assert_eq!(report["complete"], false, "{report}");
    assert_eq!(
        report["candidates"].as_array().unwrap().len(),
        1,
        "{report}"
    );

    // Searchable again, and not a repository any more: the one match is recorded.
    std::fs::remove_dir_all(hidden.join(".git")).unwrap();
    let out = t.project(&["gpu", "api", "--yes"]);
    assert!(out.status.success(), "{}", said(&out));
    assert_eq!(t.box_projects(), [("api".to_string(), code)]);
}
