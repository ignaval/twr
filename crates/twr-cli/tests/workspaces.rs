//! End-to-end: workspaces, separate `twr hub` processes inside one `TWR_HOME` (a tempdir),
//! selected with `-w` or `TWR_WORKSPACE`.

use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use twr_core::paths::Paths;
use twr_core::testkit::git::{branch_exists, init_repo};

const TWR: &str = env!("CARGO_BIN_EXE_twr");
const WAIT: Duration = Duration::from_secs(15);

fn command(home: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(TWR);
    c.args(args)
        .env("TWR_HOME", home)
        .env_remove("TWR_WORKSPACE")
        .env_remove("RUST_LOG")
        .env("TWR_TEST_USER_TOKEN_FILE", test_token())
        // No hub or runner of a test runs the user's login shell or real agent CLIs.
        .env("TWR_AGENT_PROBE", "off")
        .env("TWR_BOX_PROBE", "off");
    c
}

/// A user token this test process's hubs and clients share (`TWR_TEST_USER_TOKEN_FILE`): run
/// from inside a twr thread, the tests' clients would otherwise count as that thread's agent
/// to the hubs they start, which descend from it too.
fn test_token() -> &'static std::path::Path {
    static TOKEN: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    TOKEN.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap().keep();
        let path = dir.join("user.token");
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // Private to this user, or the hub ignores it.
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

fn fails(home: &Path, args: &[&str]) -> String {
    let out = twr(home, args);
    assert!(!out.status.success(), "twr {args:?} succeeded");
    String::from_utf8(out.stderr).unwrap()
}

/// Retries `f` until it returns Some or the deadline passes.
fn poll<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn ndjson(s: &str) -> Vec<Value> {
    s.lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}")))
        .collect()
}

struct HubProc(Child);

impl Drop for HubProc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// `twr [-w <workspace>] hub` in the foreground, once it answers.
fn start_hub(home: &Path, workspace: Option<&str>) -> HubProc {
    let w: Vec<&str> = workspace.map(|w| vec!["-w", w]).unwrap_or_default();
    let log_name = format!("hub-{}.log", workspace.unwrap_or("default"));
    let log = std::fs::File::create(home.join(log_name)).unwrap();
    let hub = HubProc(
        command(home, &[w.as_slice(), &["hub"]].concat())
            // The tests' threads run the fake runtime, which the hub offers only on request.
            .env("TWR_ENABLE_FAKE", "1")
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    poll("the hub socket", || {
        twr(home, &[w.as_slice(), &["thread", "list"]].concat())
            .status
            .success()
            .then_some(())
    });
    hub
}

fn workspaces(home: &Path) -> Vec<Value> {
    serde_json::from_str::<Value>(&ok(home, &["ws", "list", "--json"]))
        .unwrap()
        .as_array()
        .unwrap()
        .clone()
}

fn new_thread(home: &Path, w: &[&str], repo: &Path, name: &str, runtime: &str) -> String {
    let args = [
        w,
        &[
            "thread",
            "new",
            repo.to_str().unwrap(),
            "--runtime",
            runtime,
            "--name",
            name,
        ],
    ]
    .concat();
    ok(home, &args)
}

/// The id of thread `name` (of the workspace `w` selects), from its first event.
fn thread_id(home: &Path, w: &[&str], name: &str) -> String {
    let events = ndjson(&ok(home, &[w, &["events", name]].concat()));
    events[0]["thread"].as_str().unwrap().to_string()
}

/// Two workspaces of one home are separate hubs: each has its own threads, worktrees and
/// branches in the same repository (even a thread named like the other workspace), and its
/// reports say how to attach in it.
#[test]
fn workspaces_are_separate_hubs_in_one_repository() {
    let home = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(home.path()).unwrap();
    let home = home.as_path();
    let repo = init_repo(&home.join("proj"));
    let root = Paths::with_home(home);

    // Only `default` exists until a workspace is made.
    let listed = workspaces(home);
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0]["name"], "default");
    assert_eq!(listed[0]["hub"], "stopped");
    assert_eq!(listed[0]["current"], true);
    let e = fails(home, &["-w", "a", "thread", "list"]);
    assert!(
        e.contains("there is no workspace a: create it with `twr ws new a`"),
        "{e}"
    );
    let e = String::from_utf8(
        command(home, &["thread", "list"])
            .env("TWR_WORKSPACE", "a")
            .output()
            .unwrap()
            .stderr,
    )
    .unwrap();
    assert!(
        e.contains("there is no workspace a (from TWR_WORKSPACE)"),
        "{e}"
    );
    assert!(fails(home, &["-w", "Bad", "thread", "list"]).contains("invalid workspace name"));
    assert!(fails(home, &["ws", "new", "default"]).contains("always exists"));
    assert!(fails(home, &["ws", "new", "Bad"]).contains("invalid workspace name"));

    assert_eq!(
        ok(home, &["ws", "new", "a"]).trim(),
        "created workspace a; open it with `twr -w a`"
    );
    assert!(fails(home, &["ws", "new", "a"]).contains("workspace a already exists"));
    let a = root.workspace("a").unwrap();
    assert!(a.data_dir.is_dir());

    let _default_hub = start_hub(home, None);
    let _a_hub = start_hub(home, Some("a"));
    let info: Value = serde_json::from_str(&ok(home, &["-w", "a", "info", "--json"])).unwrap();
    assert_eq!(info["instance"], "a");
    assert_eq!(info["workspace"], "a");
    let info: Value = serde_json::from_str(&ok(home, &["info", "--json"])).unwrap();
    assert_eq!(info["workspace"], "default");
    // The workspace's orchestrator gets a private directory of its own.
    let orch_dir = a.data_dir.join("orchestrator");
    assert!(orch_dir.is_dir());
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&orch_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    // The same thread name in both, on the same repository; and a `default` thread named like
    // the workspace, whose branch would otherwise be the directory the workspace's need.
    let w: &[&str] = &["-w", "a"];
    new_thread(home, &[], &repo, "api", "fake");
    new_thread(home, &[], &repo, "a", "fake");
    let created = new_thread(home, w, &repo, "api", "fake");
    let wt = a.worktrees_dir().join("proj").join("api");
    assert!(
        created.contains(&format!("worktree={}", wt.display())),
        "{created}"
    );
    new_thread(home, w, &repo, "x", "fake");
    let hash = root.worktree_namespace().unwrap();
    for branch in [
        format!("twr/{hash}/api"),
        format!("twr/{hash}/a"),
        format!("twr/{hash}/@a/api"),
        format!("twr/{hash}/@a/x"),
    ] {
        assert!(branch_exists(&repo, &branch), "{branch}");
    }
    assert_ne!(thread_id(home, &[], "api"), thread_id(home, w, "api"));
    let a_threads = ok(home, &["-w", "a", "thread", "list"]);
    assert!(
        !a_threads.lines().any(|l| l.starts_with("a ")),
        "{a_threads}"
    );
    // TWR_WORKSPACE selects the workspace too; `-w` wins over it.
    let by_var = command(home, &["thread", "list"])
        .env("TWR_WORKSPACE", "a")
        .output()
        .unwrap();
    assert_eq!(String::from_utf8(by_var.stdout).unwrap(), a_threads);
    let flag_wins = command(home, &["-w", "default", "thread", "list"])
        .env("TWR_WORKSPACE", "a")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(flag_wins.stdout).unwrap(),
        ok(home, &["thread", "list"])
    );

    // A report in a named workspace is the reply alone: no attach command, with or without `-w`.
    ok(home, &["-w", "a", "send", "api", "hello"]);
    let report = poll("a's report", || {
        ndjson(&ok(home, &["-w", "a", "events", "api"]))
            .into_iter()
            .find(|e| e["kind"] == "report.ready")
    });
    assert_eq!(report["data"]["summary"], "hello");

    // `twr ws` sees both hubs and marks the one the command is in.
    let listed =
        serde_json::from_str::<Value>(&ok(home, &["-w", "a", "ws", "list", "--json"])).unwrap();
    let row = |name: &str| {
        listed
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["name"] == name)
            .unwrap()
            .clone()
    };
    assert_eq!(row("default")["hub"], "running");
    assert_eq!(row("default")["threads"], 2);
    assert_eq!(row("default")["current"], false);
    assert_eq!(row("a")["hub"], "running");
    assert_eq!(row("a")["threads"], 2);
    assert_eq!(row("a")["current"], true);
    let table = ok(home, &["ws"]);
    assert!(table.contains("* default"), "{table}");
    assert!(table.contains("  a "), "{table}");

    // Removing: refused while its hub runs, then while it has threads.
    let e = fails(home, &["ws", "rm", "a"]);
    assert!(e.contains("stop it first with `twr -w a hub stop`"), "{e}");
    ok(home, &["-w", "a", "hub", "stop"]);
    let e = fails(home, &["ws", "rm", "a"]);
    assert!(e.contains("still has 2 threads (api, x)"), "{e}");
    assert!(a.data_dir.is_dir());
    assert!(fails(home, &["ws", "rm", "default"]).contains("cannot be removed"));
    assert!(fails(home, &["ws", "rm", "nope"]).contains("there is no workspace nope"));
}

/// An empty workspace is removed; one whose trash holds a deleted thread is not.
#[test]
fn ws_rm_removes_an_empty_workspace_and_keeps_a_trash() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let root = Paths::with_home(home);
    ok(home, &["ws", "new", "b"]);
    ok(home, &["ws", "new", "c"]);
    let b = root.workspace("b").unwrap();
    // Its hub ran once: a log with an orchestrator but no worker thread.
    {
        let _hub = start_hub(home, Some("b"));
    }
    assert!(b.db_file().exists());
    assert_eq!(ok(home, &["ws", "rm", "b"]).trim(), "removed workspace b");
    assert!(!b.data_dir.exists());
    let names: Vec<Value> = workspaces(home).iter().map(|r| r["name"].clone()).collect();
    assert_eq!(names, ["default", "c"]);

    let c = root.workspace("c").unwrap();
    std::fs::create_dir_all(c.data_dir.join("trash").join("item1")).unwrap();
    let e = fails(home, &["ws", "rm", "c"]);
    assert!(
        e.contains("holds 1 deleted thread") && e.contains("twr -w c trash empty --all"),
        "{e}"
    );
    assert!(c.data_dir.is_dir());
}

/// What a workspace's hub runs inherits `TWR_WORKSPACE`, so an agent's own `twr` reaches
/// that hub, not `default`'s.
#[test]
fn an_agents_twr_reaches_its_own_workspace() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let project = home.join("proj");
    std::fs::create_dir(&project).unwrap();
    ok(home, &["ws", "new", "a"]);
    let _default_hub = start_hub(home, None);
    let _a_hub = start_hub(home, Some("a"));
    new_thread(home, &[], &project, "in-default", "fake");

    let d = home.display();
    let script = home.join("agent.sh");
    std::fs::write(
        &script,
        format!(
            "{TWR} thread list > {d}/list.out 2>&1\n\
             printf '%s' \"$TWR_WORKSPACE\" > {d}/var.out\n\
             touch {d}/done\n\
             printf '> '\n\
             while read l; do printf '> '; done\n"
        ),
    )
    .unwrap();
    let runtime = format!("pty:sh {}", script.display());
    new_thread(home, &["-w", "a"], &project, "agent", &runtime);
    ok(home, &["-w", "a", "send", "agent", "hello"]);
    poll("the agent's commands", || {
        home.join("done").exists().then_some(())
    });
    let listed = std::fs::read_to_string(home.join("list.out")).unwrap();
    assert!(listed.lines().any(|l| l.starts_with("agent ")), "{listed}");
    assert!(!listed.contains("in-default"), "{listed}");
    assert_eq!(std::fs::read_to_string(home.join("var.out")).unwrap(), "a");
}

/// An attached CLI gets its workspace as `TWR_WORKSPACE` (and `default`'s none, even when the
/// user's shell has one), so the agent's own `twr` calls reach the thread's hub.
#[test]
fn an_attached_cli_gets_its_workspace() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let project = home.join("proj");
    std::fs::create_dir(&project).unwrap();
    ok(home, &["ws", "new", "a"]);
    let _default_hub = start_hub(home, None);
    let _a_hub = start_hub(home, Some("a"));
    for w in [["-w", "a"], ["-w", "default"]] {
        new_thread(home, &w, &project, "t1", "fake");
        ok(home, &[w.as_slice(), &["send", "t1", "hello"]].concat());
        poll("t1's report", || {
            ndjson(&ok(home, &[w.as_slice(), &["events", "t1"]].concat()))
                .iter()
                .any(|e| e["kind"] == "report.ready")
                .then_some(())
        });
        let out = home.join(format!("seen-{}", w[1]));
        let attached = command(home, &[w.as_slice(), &["attach", "t1"]].concat())
            .env("TWR_WORKSPACE", "a")
            .env(
                "TWR_FAKE_ATTACH",
                r#"printf '%s' "${TWR_WORKSPACE:-unset}" > "$TWR_TEST_OUT""#,
            )
            .env("TWR_TEST_OUT", &out)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            attached.status.success(),
            "{}",
            String::from_utf8_lossy(&attached.stderr)
        );
        let seen = std::fs::read_to_string(&out).unwrap();
        let want = if w[1] == "a" { "a" } else { "unset" };
        assert_eq!(seen, want, "attached in {}", w[1]);
    }
}

/// Bare `twr -w <name>` starts that workspace's hub when none runs (here the TUI itself then
/// fails: there is no terminal). That hub gets no test user token (`spawn_hub` strips it), so
/// run from inside a twr thread this test's clients are that thread's agent to it: they read
/// it, and the test stops it with a signal.
#[test]
fn bare_twr_starts_the_workspaces_hub() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    ok(home, &["ws", "new", "a"]);
    command(home, &["-w", "a"])
        .env("TWR_ENABLE_FAKE", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    let info: Value = serde_json::from_str(&ok(home, &["-w", "a", "info", "--json"])).unwrap();
    assert_eq!(info["workspace"], "a");
    let a = Paths::with_home(home).workspace("a").unwrap();
    assert!(a.hub_log().exists());
    assert_eq!(
        Path::new(info["db"].as_str().unwrap()),
        a.db_file().as_path()
    );
    // `default` was left alone.
    assert!(fails(home, &["info"]).contains("no hub is running"));
    let pid = info["pid"].as_u64().unwrap().to_string();
    let killed = Command::new("kill").args(["-TERM", &pid]).status().unwrap();
    assert!(killed.success());
    poll("a's hub to stop", || {
        (!twr(home, &["-w", "a", "info"]).status.success()).then_some(())
    });
}

/// `twr ws rm` refuses while the workspace's hub runs, also on a socket of its own: the hub's
/// lock on its data directory says so.
#[test]
fn ws_rm_refuses_a_hub_on_another_socket() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    ok(home, &["ws", "new", "b"]);
    let b = Paths::with_home(home).workspace("b").unwrap();
    let other = home.join("other.sock");
    let other_s = other.to_str().unwrap();
    let log = std::fs::File::create(home.join("hub-b.log")).unwrap();
    let mut hub = HubProc(
        command(home, &["-w", "b", "--socket", other_s, "hub"])
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    poll("b's hub", || {
        twr(home, &["-w", "b", "--socket", other_s, "info"])
            .status
            .success()
            .then_some(())
    });
    let e = fails(home, &["ws", "rm", "b"]);
    assert!(e.contains("the hub of workspace b is running"), "{e}");
    assert!(b.db_file().exists());
    let _ = hub.0.kill();
    let _ = hub.0.wait();
    assert_eq!(ok(home, &["ws", "rm", "b"]).trim(), "removed workspace b");
}

/// A `hub.socket` among the workspaces' sockets is refused where the default workspace uses
/// it, not with an explicit `--socket`; and a command never acts on a hub of another
/// workspace that took its socket.
#[test]
fn a_command_reaches_only_its_workspaces_hub() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    ok(home, &["ws", "new", "a"]);
    ok(home, &["ws", "new", "b"]);
    let a_sock = home.join("ws/a.sock");
    let a_sock_s = a_sock.to_str().unwrap();
    std::fs::write(
        home.join("config.toml"),
        format!("[hub]\nsocket = \"{a_sock_s}\"\n"),
    )
    .unwrap();
    let e = fails(home, &["thread", "list"]);
    assert!(e.contains("where the workspaces' sockets are"), "{e}");
    // An explicit socket is not refused for it.
    let other = home.join("other.sock");
    let e = fails(home, &["--socket", other.to_str().unwrap(), "info"]);
    assert!(!e.contains("where the workspaces' sockets are"), "{e}");
    assert!(e.contains("no hub is running"), "{e}");
    // Workspace b's hub on a's socket: a's commands refuse it.
    std::fs::remove_file(home.join("config.toml")).unwrap();
    let log = std::fs::File::create(home.join("hub-b.log")).unwrap();
    let _b = HubProc(
        command(home, &["-w", "b", "--socket", a_sock_s, "hub"])
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    poll("b's hub", || {
        twr(home, &["--socket", a_sock_s, "info"])
            .status
            .success()
            .then_some(())
    });
    for args in [
        &["-w", "a", "thread", "list"][..],
        &["-w", "a", "hub", "stop"],
        &["-w", "a", "send", "x", "hello"],
    ] {
        let e = fails(home, args);
        assert!(e.contains("serves workspace b, not a"), "{args:?}: {e}");
    }
    // Still running: `hub stop` stopped nothing.
    assert!(twr(home, &["--socket", a_sock_s, "info"]).status.success());
    let out = twr(home, &["hub", "stop", "--all", "--timeout", "1"]);
    assert!(!out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("a: failed:") && stdout.contains("serves workspace b, not a"),
        "{stdout}"
    );
    assert!(twr(home, &["--socket", a_sock_s, "info"]).status.success());
    // `twr ws` doesn't show b's hub as a's.
    let a = workspaces(home)
        .into_iter()
        .find(|r| r["name"] == "a")
        .unwrap();
    assert_eq!(a["hub"], "other", "{a}");
    assert!(a["threads"].is_null(), "{a}");
}

/// A `TWR_WORKSPACE` that is not UTF-8 names no workspace: refused, not taken for default.
#[test]
fn a_workspace_variable_that_is_not_utf8_is_refused() {
    use std::os::unix::ffi::OsStrExt;
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let bad = std::ffi::OsStr::from_bytes(b"a\xff");
    for args in [&["thread", "list"][..], &["hub"]] {
        let out = command(home, args)
            .env("TWR_WORKSPACE", bad)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(!out.status.success(), "{args:?}");
        let e = String::from_utf8_lossy(&out.stderr);
        assert!(e.contains("TWR_WORKSPACE is not UTF-8"), "{args:?}: {e}");
    }
    // `-w` wins over it.
    let out = command(home, &["-w", "default", "ws"])
        .env("TWR_WORKSPACE", bad)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Bulk shutdown covers the home's default and named hubs, regardless of selection, and
/// stops their real local runtime processes before returning. A configured default socket
/// is included; another TWR_HOME is outside the command's scope.
#[test]
fn hub_stop_all_stops_every_workspace_and_its_local_runtimes() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let other_home = tempfile::tempdir().unwrap();
    let mut other_hub = start_hub(other_home.path(), None);
    let project = home.join("proj");
    std::fs::create_dir(&project).unwrap();
    ok(home, &["ws", "new", "a"]);
    ok(home, &["ws", "new", "idle"]);
    let socket = home.join("configured.sock");
    std::fs::write(
        home.join("config.toml"),
        format!("[hub]\nsocket = {:?}\n", socket.to_str().unwrap()),
    )
    .unwrap();
    let mut hubs = [start_hub(home, None), start_hub(home, Some("a"))];
    let mut runtimes = Vec::new();
    for w in ["default", "a"] {
        let script = home.join(format!("runtime-{w}.sh"));
        let pid_file = home.join(format!("runtime-{w}.pid"));
        std::fs::write(
            &script,
            format!(
                "printf '%s' \"$$\" > {}\nprintf '> '\nwhile read l; do printf '> '; done\n",
                pid_file.display()
            ),
        )
        .unwrap();
        new_thread(
            home,
            &["-w", w],
            &project,
            "worker",
            &format!("pty:sh {}", script.display()),
        );
        ok(home, &["-w", w, "send", "worker", "hello"]);
        let pid = poll("the runtime's pid", || {
            std::fs::read_to_string(&pid_file).ok()?.parse::<u32>().ok()
        });
        runtimes.push((pid, twr_runner::reap::process_start_time(pid)));
    }
    let out = command(home, &["-w", "deleted", "hub", "stop", "--all"])
        .env("TWR_WORKSPACE", "a")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let out = String::from_utf8(out.stdout).unwrap();
    for (name, hub) in ["default", "a"].into_iter().zip(hubs.iter_mut()) {
        assert!(
            out.contains(&format!("{name}: stopped (pid {})", hub.0.id())),
            "{out}"
        );
        assert!(hub.0.try_wait().unwrap().expect("hub exited").success());
    }
    assert!(out.contains("idle: not running"), "{out}");
    assert_eq!(out.lines().count(), 3, "{out}");
    for (pid, start) in runtimes {
        assert_eq!(
            twr_runner::reap::process_liveness(pid, start),
            twr_runner::reap::Alive::No
        );
    }
    assert!(other_hub.0.try_wait().unwrap().is_none());
    // The repeated command succeeds, including with a stale selection in the environment.
    let out = command(home, &["hub", "stop", "--all"])
        .env("TWR_WORKSPACE", "deleted")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .all(|l| l.ends_with(": not running")));
}

/// A socket that accepts but never answers times out, while another hub stops immediately.
#[test]
fn hub_stop_all_times_out_without_delaying_other_workspaces() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    ok(home, &["ws", "new", "a"]);
    ok(home, &["ws", "new", "idle"]);
    let _hung = std::os::unix::net::UnixListener::bind(home.join("hub.sock")).unwrap();
    let mut a = start_hub(home, Some("a"));
    // Above poll's WAIT: a sequential stop spends its deadline on default and cannot
    // reach a before the poll fails. A concurrent stop reaches a immediately.
    let mut stop = command(home, &["hub", "stop", "--all", "--timeout", "20"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    poll("a's shutdown while default hangs", || {
        a.0.try_wait().unwrap()
    });
    assert!(
        stop.try_wait().unwrap().is_none(),
        "waited for the hung hub before stopping a"
    );
    let out = stop.wait_with_output().unwrap();
    assert!(!out.status.success(), "{out:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("default: failed:")
            && stdout.contains("accepts connections but did not answer within 20s"),
        "{stdout}"
    );
    assert!(!stdout.contains("another --socket"), "{stdout}");
    assert!(
        stdout.contains("a: stopped") && stdout.contains("idle: not running"),
        "{stdout}"
    );
}

/// Agents are refused before any hubs are contacted, even with no hubs running.
#[test]
fn hub_stop_all_refuses_an_agent() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    for running in [false, true] {
        let mut hubs = Vec::new();
        if running {
            ok(home, &["ws", "new", "a"]);
            hubs.push(start_hub(home, None));
            hubs.push(start_hub(home, Some("a")));
        }
        let out = command(home, &["hub", "stop", "--all"])
            .env("TWR_AGENT", "worker:1")
            // As an agent: without the test's own token.
            .env_remove("TWR_TEST_USER_TOKEN_FILE")
            .output()
            .unwrap();
        assert!(!out.status.success(), "{out:?}");
        let e = String::from_utf8(out.stderr).unwrap();
        assert!(
            e.contains("agents cannot stop hubs") && e.contains("TWR_AGENT"),
            "{e}"
        );
        assert!(out.stdout.is_empty());
        for hub in &mut hubs {
            assert!(hub.0.try_wait().unwrap().is_none());
        }
    }
}

/// A global --socket before the subcommands must conflict too; silently ignoring it
/// would give the bulk operation a wider scope than the caller's explicit socket.
#[test]
fn hub_stop_all_refuses_an_explicit_socket_in_either_flag_order() {
    let home = tempfile::tempdir().unwrap();
    for args in [
        &["--socket", "/tmp/unused.sock", "hub", "stop", "--all"][..],
        &["hub", "stop", "--all", "--socket", "/tmp/unused.sock"],
    ] {
        let out = twr(home.path(), args);
        assert!(!out.status.success(), "{out:?}");
        assert!(
            out.stdout.is_empty(),
            "contacted workspaces before refusing the flag combination"
        );
        let e = String::from_utf8(out.stderr).unwrap();
        assert!(e.contains("--socket") && e.contains("--all"), "{e}");
    }
}

/// An explicit socket outside the documented set is a failure, never "not running".
#[test]
fn hub_stop_all_reports_a_hub_on_another_socket() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    ok(home, &["ws", "new", "b"]);
    let socket = home.join("custom.sock");
    let socket = socket.to_str().unwrap();
    let mut hub = HubProc(
        command(home, &["-w", "b", "--socket", socket, "hub"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    poll("b's custom hub", || {
        twr(home, &["--socket", socket, "info"])
            .status
            .success()
            .then_some(())
    });
    let out = twr(home, &["hub", "stop", "--all", "--timeout", "1"]);
    assert!(!out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("b: failed:") && stdout.contains("another --socket"),
        "{stdout}"
    );
    assert!(hub.0.try_wait().unwrap().is_none());
    assert!(ok(home, &["--socket", socket, "hub", "stop"]).contains("hub stopped"));
}

/// The real daemon claims its data-directory lock before binding, so a contending startup
/// cannot publish a socket while another process is checking the workspace under that lock.
#[test]
fn hub_stop_all_startup_claims_the_workspace_before_binding() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    ok(home, &["ws", "new", "a"]);
    let a = Paths::with_home(home).workspace("a").unwrap();
    let lock = std::fs::File::create(a.data_dir.join(twr_hub::LOCK_FILE)).unwrap();
    lock.lock().unwrap();
    let log_path = home.join("startup.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let hub = HubProc(
        command(home, &["-w", "a", "hub"])
            .env("RUST_LOG", "twr_hub=debug")
            .stdout(Stdio::null())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    // Wait until the hub is demonstrably waiting on our lock, not merely unscheduled.
    // Previously it bound before trying the lock, so the socket would already be present.
    poll("the hub waiting on the workspace lock", || {
        std::fs::read_to_string(&log_path)
            .ok()?
            .contains("waiting for data-directory lock")
            .then_some(())
    });
    let bound_while_locked = a.socket.exists();
    drop(lock);
    assert!(
        !bound_while_locked,
        "bound a socket before claiming the workspace"
    );
    drop(hub);
}

/// A real startup has bound its socket but is blocked opening the database. Bulk stop must
/// wait for hello and shutdown, without assuming that a bound socket means it is ready.
#[test]
fn hub_stop_all_waits_for_a_starting_workspace() {
    use std::io::{BufRead, BufReader, Read, Write};
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    ok(home, &["ws", "new", "a"]);
    let a = Paths::with_home(home).workspace("a").unwrap();
    // Leave the schema uninitialized so the daemon must write its first migration;
    // an already migrated WAL database allows replay readers past a write transaction.
    let db = twr_hub::store::connect(&a.db_file()).unwrap();
    db.execute_batch("BEGIN IMMEDIATE").unwrap();
    // A transparent proxy lets the test observe the stop's hello before releasing the
    // startup barrier. Both frames and workspace identity still come from the real hub.
    std::fs::create_dir_all(a.socket.parent().unwrap()).unwrap();
    let proxy = std::os::unix::net::UnixListener::bind(&a.socket).unwrap();
    proxy.set_nonblocking(true).unwrap();
    let real_socket = home.join("starting.sock");
    let hub = HubProc(
        command(
            home,
            &["-w", "a", "--socket", real_socket.to_str().unwrap(), "hub"],
        )
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap(),
    );
    poll("a's bound socket during startup", || {
        real_socket.exists().then_some(())
    });
    let stop = command(home, &["hub", "stop", "--all"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (mut client, _) = poll("the stop's startup connection", || proxy.accept().ok());
    client.set_nonblocking(false).unwrap();
    client.set_read_timeout(Some(WAIT)).unwrap();
    let mut reader = BufReader::new(client.try_clone().unwrap());
    let mut hello = String::new();
    reader.read_line(&mut hello).unwrap();
    let request: Value = serde_json::from_str(&hello).unwrap();
    assert_eq!(request["method"], "hello");
    drop(reader);
    let mut real = std::os::unix::net::UnixStream::connect(&real_socket).unwrap();
    real.write_all(hello.as_bytes()).unwrap();
    real.set_read_timeout(Some(Duration::from_millis(50)))
        .unwrap();
    let answer = real.read(&mut [0u8; 1]);
    // Release the barrier before assertions, well within SQLite's five-second budget.
    db.execute_batch("ROLLBACK").unwrap();
    drop(db);
    assert!(
        answer.is_err_and(|e| matches!(
            e.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        )),
        "hello answered before startup's first migration"
    );
    client.set_read_timeout(None).unwrap();
    real.set_read_timeout(None).unwrap();
    let mut from_client = client.try_clone().unwrap();
    let mut to_real = real.try_clone().unwrap();
    let upload = std::thread::spawn(move || {
        let result = std::io::copy(&mut from_client, &mut to_real);
        let _ = to_real.shutdown(std::net::Shutdown::Write);
        result
    });
    let download = std::thread::spawn(move || {
        let result = std::io::copy(&mut real, &mut client);
        let _ = client.shutdown(std::net::Shutdown::Write);
        result
    });
    let out = stop.wait_with_output().unwrap();
    let _ = upload.join().unwrap();
    let _ = download.join().unwrap();
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains(&format!("a: stopped (pid {})", hub.0.id())),
        "{stdout}"
    );
}

/// A hub removes its socket before its local runtime exits. If another stop already began,
/// --all waits for the workspace lock to be released before reporting not running.
#[test]
fn hub_stop_all_waits_for_a_workspace_already_shutting_down() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let project = home.join("proj");
    std::fs::create_dir(&project).unwrap();
    ok(home, &["ws", "new", "a"]);
    let a = Paths::with_home(home).workspace("a").unwrap();
    let mut hub = start_hub(home, Some("a"));
    let script = home.join("slow-exit.sh");
    let pid_file = home.join("slow-exit.pid");
    std::fs::write(&script, format!(
        "trap 'sleep 2; exit' HUP\nprintf '%s' \"$$\" > {}\ntouch {}/ready\nprintf '> '\nwhile read l; do printf '> '; done\n", pid_file.display(), home.display()
    )).unwrap();
    new_thread(
        home,
        &["-w", "a"],
        &project,
        "worker",
        &format!("pty:sh {}", script.display()),
    );
    ok(home, &["-w", "a", "send", "worker", "hello"]);
    poll("the slow runtime", || {
        home.join("ready").exists().then_some(())
    });
    let pid = std::fs::read_to_string(&pid_file)
        .unwrap()
        .parse::<u32>()
        .unwrap();
    let start = twr_runner::reap::process_start_time(pid);
    assert!(Command::new("kill")
        .args(["-TERM", &hub.0.id().to_string()])
        .status()
        .unwrap()
        .success());
    poll("the disappearing hub socket", || {
        (!a.socket.exists()).then_some(())
    });
    assert!(hub.0.try_wait().unwrap().is_none());
    let out = ok(home, &["hub", "stop", "--all"]);
    assert!(out.contains("a: not running"), "{out}");
    assert_eq!(
        twr_runner::reap::process_liveness(pid, start),
        twr_runner::reap::Alive::No,
        "returned before the local runtime stopped"
    );
    assert!(poll("the hub exiting after its lock was released", || hub
        .0
        .try_wait()
        .unwrap())
    .success());
}

/// A connection queued just as the listener closes is a shutdown race, not a failed hub.
#[test]
fn hub_stop_all_waits_when_hello_loses_a_shutting_down_connection() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    ok(home, &["ws", "new", "a"]);
    let a = Paths::with_home(home).workspace("a").unwrap();
    std::fs::create_dir_all(a.socket.parent().unwrap()).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&a.socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let lock = std::fs::File::create(a.data_dir.join(twr_hub::LOCK_FILE)).unwrap();
    lock.lock().unwrap();
    let stop = command(home, &["hub", "stop", "--all"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (stream, _) = poll("the queued hello connection", || listener.accept().ok());
    drop(listener);
    std::fs::remove_file(&a.socket).unwrap();
    // Like the real server, stop accepting before resetting existing connections. This
    // keeps a delayed test thread from accepting a second connection during shutdown.
    assert!(!a.socket.exists());
    drop(stream);
    std::thread::sleep(Duration::from_millis(200));
    drop(lock);
    let out = stop.wait_with_output().unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8(out.stdout)
        .unwrap()
        .contains("a: not running"));
}

#[test]
fn hub_stop_all_reports_a_socket_that_keeps_closing_connections() {
    use std::io::{BufRead, BufReader};
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    ok(home, &["ws", "new", "a"]);
    let a = Paths::with_home(home).workspace("a").unwrap();
    std::fs::create_dir_all(a.socket.parent().unwrap()).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&a.socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let stop = command(home, &["hub", "stop", "--all", "--timeout", "10"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    for _ in 0..2 {
        let (stream, _) = poll("a hello on the persistently closing socket", || {
            listener.accept().ok()
        });
        stream.set_nonblocking(false).unwrap();
        stream.set_read_timeout(Some(WAIT)).unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&line).unwrap()["method"],
            "hello"
        );
        // The listener stays available, but every accepted connection loses its reply.
        drop(reader);
    }
    let out = stop.wait_with_output().unwrap();
    assert!(!out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("a: failed:")
            && stdout.contains("keeps closing connections")
            && stdout.contains("closed the connection during hello"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("within 10s"),
        "lost the real failure to a generic timeout"
    );
}

#[test]
fn hub_stop_all_adapts_commands_in_hub_errors_to_the_workspace() {
    use std::io::{BufRead, BufReader, Write};
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    ok(home, &["ws", "new", "a"]);
    let a = Paths::with_home(home).workspace("a").unwrap();
    std::fs::create_dir_all(a.socket.parent().unwrap()).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&a.socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let stop = command(home, &["hub", "stop", "--all"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (mut stream, _) = poll("the refusing hub connection", || listener.accept().ok());
    stream.set_nonblocking(false).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let hello: Value = serde_json::from_str(&line).unwrap();
    writeln!(
        stream,
        "{}",
        serde_json::json!({"id": hello["id"], "result": {"workspace": "a"}})
    )
    .unwrap();
    line.clear();
    reader.read_line(&mut line).unwrap();
    let info: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(info["method"], "hub.info");
    writeln!(stream, "{}", serde_json::json!({"id": info["id"], "error": {"code": "unsupported", "message": "update with `twr update`; `twr --socket /x hub stop` stays explicit"}})).unwrap();
    let out = stop.wait_with_output().unwrap();
    assert!(!out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("a: failed: unsupported: update with `twr -w a update`"),
        "{stdout}"
    );
    assert!(stdout.contains("`twr --socket /x hub stop`"), "{stdout}");
}

/// Identity and pid were already checked when shutdown's reply was lost. Confirming the
/// process exit is sufficient; a still-live process would instead time out.
#[test]
fn hub_stop_all_confirms_exit_when_the_shutdown_reply_is_lost() {
    use std::io::{BufRead, BufReader, Write};
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    ok(home, &["ws", "new", "a"]);
    let a = Paths::with_home(home).workspace("a").unwrap();
    std::fs::create_dir_all(a.socket.parent().unwrap()).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&a.socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let lock = std::fs::File::create(a.data_dir.join(twr_hub::LOCK_FILE)).unwrap();
    lock.lock().unwrap();
    let mut process = HubProc(Command::new("sleep").arg("60").spawn().unwrap());
    let pid = process.0.id();
    let stop = command(home, &["hub", "stop", "--all"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (mut stream, _) = poll("the fake hub connection", || listener.accept().ok());
    stream.set_nonblocking(false).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    for (method, result) in [
        ("hello", serde_json::json!({"workspace": "a"})),
        ("hub.info", serde_json::json!({"pid": pid})),
    ] {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["method"], method);
        writeln!(
            stream,
            "{}",
            serde_json::json!({"id": request["id"], "result": result})
        )
        .unwrap();
    }
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&line).unwrap()["method"],
        "hub.shutdown"
    );
    drop(reader);
    drop(stream);
    drop(listener);
    std::fs::remove_file(&a.socket).unwrap();
    process.0.kill().unwrap();
    process.0.wait().unwrap();
    drop(lock);
    let out = stop.wait_with_output().unwrap();
    assert!(out.status.success(), "{out:?}");
    assert!(String::from_utf8(out.stdout)
        .unwrap()
        .contains(&format!("a: stopped (pid {pid})")));
}

/// A deadline after requesting shutdown is distinct from a hub that never answers. The
/// request can have reached it, so report its still-live pid after a final exit check.
#[test]
fn hub_stop_all_reports_shutdown_progress_when_the_reply_misses_the_deadline() {
    use std::io::{BufRead, BufReader, Write};
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    ok(home, &["ws", "new", "a"]);
    ok(home, &["ws", "new", "idle"]);
    let mut default = start_hub(home, None);
    let a = Paths::with_home(home).workspace("a").unwrap();
    std::fs::create_dir_all(a.socket.parent().unwrap()).unwrap();
    let listener = std::os::unix::net::UnixListener::bind(&a.socket).unwrap();
    listener.set_nonblocking(true).unwrap();
    let process = HubProc(Command::new("sleep").arg("60").spawn().unwrap());
    let pid = process.0.id();
    let stop = command(home, &["hub", "stop", "--all", "--timeout", "2"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (mut stream, _) = poll("the hub connection with a missing reply", || {
        listener.accept().ok()
    });
    stream.set_nonblocking(false).unwrap();
    stream.set_read_timeout(Some(WAIT)).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    for (method, result) in [
        ("hello", serde_json::json!({"workspace": "a"})),
        ("hub.info", serde_json::json!({"pid": pid})),
    ] {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let request: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(request["method"], method);
        writeln!(
            stream,
            "{}",
            serde_json::json!({"id": request["id"], "result": result})
        )
        .unwrap();
    }
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&line).unwrap()["method"],
        "hub.shutdown"
    );
    // Keep the connection and the reported process alive without answering shutdown.
    let out = stop.wait_with_output().unwrap();
    assert!(!out.status.success(), "{out:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains(&format!(
            "a: failed: the hub (pid {pid}) is still shutting down after 2s"
        )),
        "{stdout}"
    );
    assert!(!stdout.contains("accepts connections"), "{stdout}");
    assert!(stdout.contains("default: stopped"), "{stdout}");
    assert!(stdout.contains("idle: not running"), "{stdout}");
    assert!(default.0.try_wait().unwrap().unwrap().success());
}

#[test]
fn hub_stop_all_stops_named_workspaces_with_an_invalid_default_config() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    ok(home, &["ws", "new", "a"]);
    ok(home, &["ws", "new", "idle"]);
    let mut a = start_hub(home, Some("a"));
    std::fs::write(home.join("config.toml"), "broken = [").unwrap();
    let out = twr(home, &["hub", "stop", "--all"]);
    assert!(!out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("default: failed:") && stdout.contains("config.toml"),
        "{stdout}"
    );
    assert!(
        stdout.contains("a: stopped") && stdout.contains("idle: not running"),
        "{stdout}"
    );
    assert!(a
        .0
        .try_wait()
        .unwrap()
        .expect("named hub stopped")
        .success());
}

#[test]
fn hub_stop_all_stops_default_even_when_workspaces_cannot_be_listed() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let mut hub = start_hub(home, None);
    let workspaces = Paths::with_home(home).workspaces_dir();
    std::fs::write(&workspaces, "not a directory").unwrap();
    let out = twr(home, &["hub", "stop", "--all"]);
    assert!(!out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("default: stopped") && stdout.contains("workspaces: failed: listing"),
        "{stdout}"
    );
    assert!(hub
        .0
        .try_wait()
        .unwrap()
        .expect("default hub stopped")
        .success());
}

#[test]
fn hub_stop_all_startup_still_refuses_a_duplicate_hub_on_its_socket() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let mut hub = start_hub(home, None);
    let e = fails(home, &["hub"]);
    assert!(e.contains("a hub is already listening on"), "{e}");
    assert!(hub.0.try_wait().unwrap().is_none());
}

/// The commands a named workspace's output suggests run in that workspace.
#[test]
fn hints_name_the_workspace() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    ok(home, &["ws", "new", "a"]);
    let _a_hub = start_hub(home, Some("a"));
    assert_eq!(
        ok(home, &["-w", "a", "thread", "list"]).trim(),
        "no threads (create one with `twr -w a thread new <project>`)"
    );
    let _default_hub = start_hub(home, None);
    assert_eq!(
        ok(home, &["thread", "list"]).trim(),
        "no threads (create one with `twr thread new <project>`)"
    );
    // The hub's own hints, in its errors and messages, run where the command did.
    let e = fails(home, &["-w", "a", "trash", "restore", "nope"]);
    assert!(e.contains("(`twr -w a trash list` shows them)"), "{e}");
    let e = fails(home, &["trash", "restore", "nope"]);
    assert!(e.contains("(`twr trash list` shows them)"), "{e}");
    // `default` from a shell that selects `a`: named, or the hint would reach a.
    let out = command(home, &["-w", "default", "trash", "restore", "nope"])
        .env("TWR_WORKSPACE", "a")
        .output()
        .unwrap();
    let e = String::from_utf8_lossy(&out.stderr);
    assert!(
        e.contains("(`twr -w default trash list` shows them)"),
        "{e}"
    );

    // A turn that gave up: a review item, the only one, so it doesn't block.
    // In a repository: its worktree goes to the trash when it is deleted.
    let project = init_repo(&home.join("proj"));
    new_thread(home, &["-w", "a"], &project, "t", "fake");
    ok(home, &["-w", "a", "send", "t", "!giveup"]);
    poll("the review item", || {
        ok(home, &["-w", "a", "attention", "--all"])
            .contains("review")
            .then_some(())
    });
    assert_eq!(
        ok(home, &["-w", "a", "attention"]).trim(),
        "nothing blocks you; 1 other item (twr -w a attention --all)"
    );
    // A deletion's outcome names the trash commands of the workspace.
    let out = ok(home, &["-w", "a", "delete", "t", "--yes"]);
    assert!(out.contains("`twr -w a trash restore "), "{out}");
    // The card it confirmed too.
    assert!(!out.contains("`twr trash"), "{out}");
    let shown = ok(home, &["-w", "a", "orchestrator", "show"]);
    assert!(shown.contains("`twr -w a orchestrator use`"), "{shown}");
    assert!(!shown.contains("`twr orchestrator use`"), "{shown}");

    // A socket given explicitly (b's hub on it, a's selected): the commands shown use it.
    ok(home, &["ws", "new", "b"]);
    let spaced = home.join("my b.sock");
    let spaced_s = spaced.to_str().unwrap();
    let log = std::fs::File::create(home.join("hub-spaced.log")).unwrap();
    let _spaced_hub = HubProc(
        command(home, &["-w", "b", "--socket", spaced_s, "hub"])
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    poll("the hub on the spaced socket", || {
        twr(home, &["--socket", spaced_s, "info"])
            .status
            .success()
            .then_some(())
    });
    let e = fails(
        home,
        &["-w", "a", "--socket", spaced_s, "trash", "restore", "nope"],
    );
    assert!(
        e.contains(&format!(
            "(`twr --socket '{spaced_s}' trash list` shows them)"
        )),
        "{e}"
    );
}

/// An agent can't create/remove workspaces or stop every hub, including when it clears its
/// marker: the CLI checks ancestry without depending on any running hub.
#[test]
fn an_agent_cannot_create_or_remove_workspaces() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let project = home.join("proj");
    std::fs::create_dir(&project).unwrap();
    ok(home, &["ws", "new", "a"]);
    ok(home, &["ws", "new", "b"]);
    let _a_hub = start_hub(home, Some("a"));
    let d = home.display();
    let script = home.join("agent.sh");
    std::fs::write(
        &script,
        format!(
            "{TWR} ws rm b > /dev/null 2> {d}/rm.err\n\
             env -u TWR_WORKSPACE {TWR} ws new c > /dev/null 2> {d}/new.err\n\
             env -u TWR_AGENT {TWR} hub stop --all > /dev/null 2> {d}/stop.err\n\
             touch {d}/done\n\
             printf '> '\n\
             while read l; do printf '> '; done\n"
        ),
    )
    .unwrap();
    let runtime = format!("pty:sh {}", script.display());
    new_thread(home, &["-w", "a"], &project, "agent", &runtime);
    ok(home, &["-w", "a", "send", "agent", "hello"]);
    poll("the agent's commands", || {
        home.join("done").exists().then_some(())
    });
    for what in ["rm", "new"] {
        let e = std::fs::read_to_string(home.join(format!("{what}.err"))).unwrap();
        assert!(
            e.contains("agents cannot create or remove workspaces"),
            "{what}: {e}"
        );
    }
    let e = std::fs::read_to_string(home.join("stop.err")).unwrap();
    assert!(e.contains("agents cannot stop hubs"), "{e}");
    assert!(twr(home, &["-w", "a", "info"]).status.success());
    let root = Paths::with_home(home);
    assert!(root.workspace("b").unwrap().exists());
    assert!(!root.workspace("c").unwrap().exists());
}

/// `TWR_WORKSPACE` doesn't move a runner's storage: a runner serves whichever hub it connects
/// to, and keeps its data in the root's data directory.
#[test]
fn a_runner_keeps_its_data_in_the_root() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    ok(home, &["ws", "new", "a"]);
    let mut runner = HubProc(
        command(
            home,
            &["runner", "--hub", "ws://127.0.0.1:9", "--box", "gpu"],
        )
        .env("TWR_WORKSPACE", "a")
        .env("TWR_RUNNER_TOKEN", "a-token-for-the-test")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap(),
    );
    let root = Paths::with_home(home);
    let a = root.workspace("a").unwrap();
    poll("the runner's data", || {
        let made = |dir: &Path| {
            std::fs::read_dir(dir)
                .map(|d| {
                    d.flatten()
                        .any(|e| e.file_name().to_string_lossy().starts_with("runner"))
                })
                .unwrap_or(false)
        };
        assert!(!made(&a.data_dir), "in workspace a's data");
        made(&root.data_dir).then_some(())
    });
    // Stopped as a user stops it (SIGTERM), not killed: it ends what it started, then exits
    // (it starts no shell or agent CLI here: `TWR_BOX_PROBE=off`). A runner that has not
    // listened for the signal yet (just started) ends by it.
    let pid = runner.0.id().to_string();
    assert!(Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .unwrap()
        .success());
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(st) = runner.0.try_wait().unwrap() {
            use std::os::unix::process::ExitStatusExt;
            assert!(
                st.code().is_some() || st.signal() == Some(15),
                "the runner ended by {st}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the runner did not stop on SIGTERM"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// A thread is never started in another workspace's worktree: that workspace's hub removes
/// and moves it with its own thread.
#[test]
fn a_thread_cannot_work_in_another_workspaces_worktree() {
    let home = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(home.path()).unwrap();
    let home = home.as_path();
    let repo = init_repo(&home.join("proj"));
    ok(home, &["ws", "new", "a"]);
    let _default_hub = start_hub(home, None);
    let _a_hub = start_hub(home, Some("a"));
    new_thread(home, &["-w", "a"], &repo, "api", "fake");
    let wt = Paths::with_home(home)
        .workspace("a")
        .unwrap()
        .worktrees_dir()
        .join("proj")
        .join("api");
    assert!(wt.is_dir());
    let wt_s = wt.to_str().unwrap();
    for extra in [&["--no-worktree"][..], &[]] {
        let args = [
            &[
                "thread",
                "new",
                wt_s,
                "--runtime",
                "fake",
                "--name",
                "intruder",
            ][..],
            extra,
        ]
        .concat();
        let e = fails(home, &args);
        assert!(
            e.contains("is in the worktrees of workspace a"),
            "{extra:?}: {e}"
        );
    }
    // Its own workspace refuses it too, as it always did.
    let e = fails(
        home,
        &[
            "-w",
            "a",
            "thread",
            "new",
            wt_s,
            "--runtime",
            "fake",
            "--name",
            "x2",
            "--no-worktree",
        ],
    );
    assert!(e.contains("worktree twr manages for thread api"), "{e}");
}
