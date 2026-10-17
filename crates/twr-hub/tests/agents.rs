//! Requests from agents: a connection whose peer is, or descends from, a
//! process twr started for a thread (a runtime of the hub's machine, or an attach's reported
//! process) may only read. The processes that connect here are this test binary started again
//! as a socket client ([`agent_client`]) by a runtime: a `pty` worker's shell script, an
//! orchestrator runtime that is a shell script, or the stand-in for an attach's interactive CLI.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::timeout;
use twr_adapters::pty::{PtyAdapter, Timing};
use twr_core::adapter::{Adapter, AdapterEnd, AdapterHandle, AdapterInfo, SpawnConfig, Tier};
use twr_core::client::{Client, Subscription};
use twr_core::config::Config;
use twr_core::event::{EventBody, SessionPayload};
use twr_core::paths::Paths;
use twr_core::proto::{codes, RpcError, StreamItem, PROTOCOL_VERSION};
use twr_hub::{server, Hub, HubOptions};

const WAIT: Duration = Duration::from_secs(20);

/// Not a test of its own: the socket client the tests' runtime processes run, this test
/// binary started again with `TWR_TEST_CLIENT=<dir>` and `TWR_TEST_SOCKET=<socket>` (without
/// them it returns at once). Once `<dir>/go` exists it connects and sends each line of
/// `<dir>/requests.ndjson`, reading one response per request (`{"reconnect": true}` opens a
/// new connection instead, `{"mark": name}` creates `<dir>/<name>`, `{"wait": name}` waits
/// until `<dir>/<name>` exists, `{"from": name}` sends the request in `<dir>/<name>`, written
/// by the test once it knows it), and writes the responses to `<dir>/responses.ndjson`,
/// renamed into place once complete. With `TWR_TEST_NONDUMPABLE` set it first makes itself
/// not dumpable ([`non_dumpable`]), with `TWR_TEST_SETSID` the leader of a new session
/// ([`new_session`]). `{"handoff": name}` waits until `<dir>/<name>` exists, then
/// hands the connection to a new process, which sends the requests that follow and writes the
/// responses, and exits ([`hand_off`]).
#[test]
fn agent_client() {
    let (Some(dir), Some(socket)) = (
        std::env::var_os("TWR_TEST_CLIENT"),
        std::env::var_os("TWR_TEST_SOCKET"),
    ) else {
        return;
    };
    #[cfg(target_os = "linux")]
    if std::env::var_os("TWR_TEST_NONDUMPABLE").is_some() {
        non_dumpable();
    }
    if std::env::var_os("TWR_TEST_SETSID").is_some() {
        new_session();
    }
    let dir = PathBuf::from(dir);
    // The process a connection was handed to ([`hand_off`]): its descriptor, and the first
    // request it sends.
    let heir = std::env::var("TWR_TEST_HEIR_FD")
        .ok()
        .map(|fd| fd.parse::<i32>().unwrap());
    let first: usize = std::env::var("TWR_TEST_HEIR_FROM").map_or(0, |n| n.parse().unwrap());
    let deadline = Instant::now() + WAIT;
    if heir.is_none() {
        std::fs::write(dir.join("pid.tmp"), std::process::id().to_string()).unwrap();
        std::fs::rename(dir.join("pid.tmp"), dir.join("pid")).unwrap();
        while !dir.join("go").exists() {
            assert!(Instant::now() < deadline, "never told to go");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let open = |s: std::os::unix::net::UnixStream| {
        s.set_read_timeout(Some(WAIT)).unwrap();
        let r = BufReader::new(s.try_clone().unwrap());
        (s, r)
    };
    let connect = || open(std::os::unix::net::UnixStream::connect(&socket).unwrap());
    let requests = std::fs::read_to_string(dir.join("requests.ndjson")).unwrap();
    let (mut s, mut r) = match heir {
        // SAFETY: the descriptor of the connection this process was handed, which nothing
        // else in it owns.
        Some(fd) => open(unsafe {
            <std::os::unix::net::UnixStream as std::os::fd::FromRawFd>::from_raw_fd(fd)
        }),
        None => connect(),
    };
    let mut out = String::new();
    for (i, line) in requests.lines().enumerate().skip(first) {
        let req: Value = serde_json::from_str(line).unwrap();
        if req["reconnect"] == true {
            (s, r) = connect();
            continue;
        }
        if let Some(name) = req["handoff"].as_str() {
            while !dir.join(name).exists() {
                assert!(Instant::now() < deadline, "never told to hand off");
                std::thread::sleep(Duration::from_millis(20));
            }
            hand_off(&s, i + 1);
        }
        if let Some(name) = req["mark"].as_str() {
            std::fs::write(dir.join(name), "").unwrap();
            continue;
        }
        if let Some(name) = req["wait"].as_str() {
            while !dir.join(name).exists() {
                assert!(Instant::now() < deadline, "never told to go on");
                std::thread::sleep(Duration::from_millis(20));
            }
            continue;
        }
        let line = match req["from"].as_str() {
            Some(name) => std::fs::read_to_string(dir.join(name)).unwrap(),
            None => line.to_string(),
        };
        writeln!(s, "{}", line.trim()).unwrap();
        let mut resp = String::new();
        r.read_line(&mut resp).unwrap();
        out.push_str(&resp);
    }
    std::fs::write(dir.join("responses.tmp"), out).unwrap();
    std::fs::rename(dir.join("responses.tmp"), dir.join("responses.ndjson")).unwrap();
}

/// Starts [`agent_client`] again holding the connection `s` (`TWR_TEST_HEIR_FD`, with this
/// process's environment, so its agent marker too) to go on from request `next`, then exits:
/// the process that opened the connection is gone while the connection lives on.
fn hand_off(s: &std::os::unix::net::UnixStream, next: usize) -> ! {
    use std::os::fd::AsRawFd;
    extern "C" {
        fn fcntl(fd: i32, cmd: i32, ...) -> i32;
    }
    const F_SETFD: i32 = 2;
    let fd = s.as_raw_fd();
    // SAFETY: clears the close-on-exec flag of a descriptor this process owns, so the new
    // process inherits it.
    assert_ne!(unsafe { fcntl(fd, F_SETFD, 0) }, -1);
    std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "agent_client", "--test-threads=1"])
        .env("TWR_TEST_HEIR_FD", fd.to_string())
        .env("TWR_TEST_HEIR_FROM", next.to_string())
        .spawn()
        .unwrap();
    std::process::exit(0);
}

/// Makes this process the leader of a new session (`setsid`), still its parent's child: a
/// signal to its runtime's process group (a `pty` runtime's stop) no longer reaches it.
fn new_session() {
    extern "C" {
        fn setsid() -> i32;
    }
    // SAFETY: setsid only moves this process (not a process group leader: a background job of
    // a non-interactive shell) into a new session.
    assert_ne!(unsafe { setsid() }, -1);
}

/// Makes this process not dumpable (`prctl(PR_SET_DUMPABLE, 0)`): its own user can no longer
/// read its `/proc/<pid>/environ`.
#[cfg(target_os = "linux")]
fn non_dumpable() {
    extern "C" {
        fn prctl(option: i32, ...) -> i32;
    }
    const PR_SET_DUMPABLE: i32 = 4;
    // SAFETY: prctl(PR_SET_DUMPABLE, 0) only changes this process's dumpable flag.
    assert_eq!(unsafe { prctl(PR_SET_DUMPABLE, 0u64) }, 0);
}

/// A runtime process's client: [`agent_client`] in `dir`, for the hub on `socket`.
struct AgentClient {
    dir: PathBuf,
    socket: PathBuf,
}

impl AgentClient {
    fn new(base: &Path, name: &str, socket: &Path) -> AgentClient {
        let dir = base.join(name);
        std::fs::create_dir(&dir).unwrap();
        AgentClient {
            dir,
            socket: socket.to_path_buf(),
        }
    }

    /// The shell command line that runs the client (no spaces in the paths of a tempdir).
    fn command(&self) -> String {
        format!(
            "TWR_TEST_CLIENT={} TWR_TEST_SOCKET={} {} --exact agent_client --test-threads=1 > {}/client.log 2>&1",
            self.dir.display(),
            self.socket.display(),
            std::env::current_exe().unwrap().display(),
            self.dir.display()
        )
    }

    /// [`AgentClient::command`] without the agent marker in its environment (a process that
    /// cleared it).
    fn command_without_marker(&self) -> String {
        format!("env -u TWR_AGENT {}", self.command())
    }

    /// [`AgentClient::command`] without the markers a runtime's kill finds its processes by
    /// (`TWR_THREAD`, `TWR_RUNTIME_GEN`): a process that outlives its runtime.
    fn command_surviving(&self) -> String {
        format!("env -u TWR_THREAD -u TWR_RUNTIME_GEN {}", self.command())
    }

    /// A shell script that runs the client and then waits for input like an interactive CLI
    /// (`> ` prompts, for the `pty` adapter).
    fn script(&self) -> PathBuf {
        self.script_running(&self.command())
    }

    /// [`AgentClient::script`] running the shell line `run` instead of the client itself.
    fn script_running(&self, run: &str) -> PathBuf {
        let path = self.dir.join("runtime.sh");
        let script =
            format!("{run}\nprintf '> '\nwhile read l; do echo \"got [$l]\"; printf '> '; done\n");
        std::fs::write(&path, script).unwrap();
        path
    }

    /// The shell line that starts the client as a background job of a subshell that exits at
    /// once (`nohup cmd &` in a short-lived shell): the client is reparented away from the
    /// runtime and its ancestry no longer shows it.
    fn orphan_command(&self) -> String {
        format!("( nohup env {} & )", self.command())
    }

    /// [`AgentClient::orphan_command`] for a client that makes itself not dumpable before it
    /// connects (hiding its environment, and the marker in it, from its own user). Linux only,
    /// like [`non_dumpable`] and the test that uses it.
    #[cfg(target_os = "linux")]
    fn orphan_non_dumpable_command(&self) -> String {
        format!("( nohup env TWR_TEST_NONDUMPABLE=1 {} & )", self.command())
    }

    /// The client's pid, once it runs.
    async fn pid(&self) -> u32 {
        let deadline = Instant::now() + WAIT;
        loop {
            if let Ok(t) = std::fs::read_to_string(self.dir.join("pid")) {
                return t.trim().parse().unwrap();
            }
            assert!(Instant::now() < deadline, "the client never started");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The client's pid once its ancestry no longer contains `pid` (the shell that started
    /// it exited and it was reparented).
    async fn orphaned_from(&self, pid: u64) -> u32 {
        let client = self.pid().await;
        let deadline = Instant::now() + WAIT;
        loop {
            let a = twr_runner::reap::ancestry(client);
            if a.chain.iter().all(|p| u64::from(p.pid) != pid) {
                assert!(!a.chain.is_empty(), "{a:?}");
                return client;
            }
            assert!(Instant::now() < deadline, "never left {pid}: {a:?}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Sends `requests` (and `{"reconnect": true}` lines) once the client runs: the
    /// responses, one per request.
    async fn run(&self, requests: &[Value]) -> Vec<Value> {
        self.start(requests);
        self.responses().await
    }

    /// Tells the client to send `requests` (see [`agent_client`]) without waiting for it.
    fn start(&self, requests: &[Value]) {
        let lines: Vec<String> = requests.iter().map(Value::to_string).collect();
        std::fs::write(self.dir.join("requests.ndjson"), lines.join("\n") + "\n").unwrap();
        std::fs::write(self.dir.join("go"), "").unwrap();
    }

    /// Waits until the client has reached `{"mark": name}`.
    async fn reached(&self, name: &str) {
        let deadline = Instant::now() + WAIT;
        while !self.dir.join(name).exists() {
            let log = std::fs::read_to_string(self.dir.join("client.log")).unwrap_or_default();
            assert!(
                Instant::now() < deadline,
                "the client never got to {name}: {log}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Lets a client waiting on `{"wait": name}` go on.
    fn go_on(&self, name: &str) {
        std::fs::write(self.dir.join(name), "").unwrap();
    }

    /// The client's responses, one per request, once it has sent them all.
    async fn responses(&self) -> Vec<Value> {
        let out = self.dir.join("responses.ndjson");
        let deadline = Instant::now() + WAIT;
        loop {
            if let Ok(text) = std::fs::read_to_string(&out) {
                return text
                    .lines()
                    .map(|l| serde_json::from_str(l).unwrap())
                    .collect();
            }
            let log = std::fs::read_to_string(self.dir.join("client.log")).unwrap_or_default();
            assert!(
                Instant::now() < deadline,
                "the agent's client never answered: {log}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// Spawned by [`ScriptOrch`]: the runtime's configuration, and its process.
struct OrchRt {
    cfg: SpawnConfig,
    child: std::process::Child,
    _end: AdapterEnd,
}

impl OrchRt {
    /// The orchestrator token the runtime's MCP server gets.
    fn token(&self) -> String {
        let path = self.cfg.mcp_config_file.as_ref().unwrap();
        let mcp: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        mcp["mcpServers"]["twr"]["env"]["TWR_ORCH_TOKEN"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

impl Drop for OrchRt {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// An orchestrator runtime whose process is `sh <script>`, reported in `thread.started`
/// `ext.twr.pid` as a real runtime reports its own. Each start runs the next of `scripts`,
/// the last one again once the others ran.
struct ScriptOrch {
    scripts: std::sync::Mutex<std::collections::VecDeque<PathBuf>>,
    spawns: mpsc::UnboundedSender<OrchRt>,
}

impl ScriptOrch {
    fn new(scripts: Vec<PathBuf>, spawns: mpsc::UnboundedSender<OrchRt>) -> ScriptOrch {
        ScriptOrch {
            scripts: std::sync::Mutex::new(scripts.into()),
            spawns,
        }
    }
}

impl Adapter for ScriptOrch {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "claude-code",
            tier: Tier::Headless,
            description: "test orchestrator runtime running a shell script",
        }
    }

    fn spawn(&self, cfg: SpawnConfig, _prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let script = {
            let mut scripts = self.scripts.lock().unwrap();
            match scripts.len() {
                0 => anyhow::bail!("no script"),
                1 => scripts[0].clone(),
                _ => scripts.pop_front().unwrap(),
            }
        };
        // With the hub's environment, as every runtime adapter starts its process.
        let child = std::process::Command::new("sh")
            .arg(&script)
            .envs(cfg.env.iter().map(|(k, v)| (k, v)))
            .stdin(std::process::Stdio::null())
            .spawn()?;
        let (h, end) = AdapterHandle::pair();
        end.emit(
            EventBody::ThreadStarted(SessionPayload {
                session_id: Some("s-orch".into()),
                ext: serde_json::from_value(json!({ "twr": { "pid": child.id() } })).ok(),
                ..Default::default()
            }),
            None,
        );
        let _ = self.spawns.send(OrchRt {
            cfg,
            child,
            _end: end,
        });
        Ok(h)
    }
}

/// Spawned by [`HeldOrch`]: the runtime's configuration, its process, and its side of the
/// session (nothing reported yet).
struct HeldRt {
    cfg: SpawnConfig,
    child: std::process::Child,
    end: AdapterEnd,
}

impl HeldRt {
    /// The orchestrator token the runtime's MCP server gets.
    fn token(&self) -> String {
        let path = self.cfg.mcp_config_file.as_ref().unwrap();
        let mcp: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        mcp["mcpServers"]["twr"]["env"]["TWR_ORCH_TOKEN"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// Reports the process (`thread.started` `ext.twr.pid`), as a runtime does once it has
    /// started its session.
    fn report(&self) {
        self.end.emit(
            EventBody::ThreadStarted(SessionPayload {
                session_id: Some("s-held".into()),
                ext: serde_json::from_value(json!({ "twr": { "pid": self.child.id() } })).ok(),
                ..Default::default()
            }),
            None,
        );
    }

    /// Kills the process (only it: what it started lives on) and reports the exit.
    fn exit(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.end.exited(Some(0), "");
    }
}

/// An orchestrator runtime whose process is `sh <script>` (once: a later start fails), which
/// tells the hub the process it started as soon as it started it
/// ([`AdapterEnd::spawned`]) but reports it (`thread.started`) only when the test says
/// ([`HeldRt::report`]): a runtime whose MCP servers and hooks run before its session starts.
struct HeldOrch {
    script: std::sync::Mutex<Option<PathBuf>>,
    spawns: mpsc::UnboundedSender<HeldRt>,
}

impl Adapter for HeldOrch {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "claude-code",
            tier: Tier::Headless,
            description: "test orchestrator runtime that reports its process late",
        }
    }

    fn spawn(&self, cfg: SpawnConfig, _prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let Some(script) = self.script.lock().unwrap().take() else {
            anyhow::bail!("started once already");
        };
        let child = std::process::Command::new("sh")
            .arg(&script)
            .envs(cfg.env.iter().map(|(k, v)| (k, v)))
            .stdin(std::process::Stdio::null())
            .spawn()?;
        let (h, end) = AdapterHandle::pair();
        end.spawned(child.id());
        let _ = self.spawns.send(HeldRt { cfg, child, end });
        Ok(h)
    }
}

struct TestHub {
    dir: tempfile::TempDir,
    project: PathBuf,
    socket: PathBuf,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
    hub: Hub,
}

/// A hub on a socket with the fake and pty runtimes (fast pty heuristics) and, when given, an
/// orchestrator runtime.
async fn start(orch: Option<Arc<dyn Adapter>>, dir: tempfile::TempDir) -> TestHub {
    start_with(orch, dir, |_| {}).await
}

/// [`start`], with the hub's options changed by `tweak` (test hooks).
async fn start_with(
    orch: Option<Arc<dyn Adapter>>,
    dir: tempfile::TempDir,
    tweak: impl FnOnce(&mut HubOptions),
) -> TestHub {
    let paths = Paths::with_home(dir.path());
    let project = dir.path().join("proj");
    std::fs::create_dir(&project).unwrap();
    let listener = server::bind(&paths.socket).await.unwrap();
    let mut config = Config {
        default_runtime: "fake".into(),
        ..Default::default()
    };
    if orch.is_some() {
        config.orchestrator.runtime = Some("claude-code".into());
    }
    let mut o = HubOptions::new(paths.db_file(), config);
    o.adapters = Arc::new(move |n: &str| -> Option<Arc<dyn Adapter>> {
        match n {
            "fake" => Some(Arc::new(twr_adapters::FakeAdapter::new())),
            "pty" => Some(Arc::new(PtyAdapter::new().timing(Timing {
                idle: Duration::from_millis(400),
                settle: Duration::from_millis(100),
                startup_max: Duration::from_millis(1000),
                flush: Duration::from_millis(50),
            }))),
            "claude-code" => orch.clone(),
            _ => None,
        }
    });
    o.socket = Some(paths.socket.clone());
    o.job_gate_exe = Some(env!("CARGO_BIN_EXE_twr-job-gate").into());
    o.stop_timeout = Duration::from_secs(5);
    tweak(&mut o);
    let hub = Hub::open(o).await.unwrap();
    let task = tokio::spawn(server::serve(
        hub.clone(),
        listener,
        paths.socket.clone(),
        false,
    ));
    TestHub {
        dir,
        project,
        socket: paths.socket,
        task,
        hub,
    }
}

impl TestHub {
    async fn client(&self, name: &str) -> Client {
        let mut c = Client::connect(&self.socket).await.unwrap();
        c.hello(json!({ "client": name })).await.unwrap();
        c
    }

    async fn stop(self) {
        let mut c = self.client("end").await;
        c.call("hub.shutdown", Value::Null).await.unwrap();
        self.task.await.unwrap().unwrap();
    }
}

fn code(e: &anyhow::Error) -> String {
    e.downcast_ref::<RpcError>().unwrap().code.clone()
}

async fn until(sub: &mut Subscription, pred: impl Fn(&StreamItem) -> bool) -> StreamItem {
    timeout(WAIT, async {
        loop {
            let item = sub.next().await.unwrap().expect("stream ended");
            if pred(&item) {
                return item;
            }
        }
    })
    .await
    .expect("timed out waiting for a control item")
}

/// `ext.twr.pid` of a `thread.started` item.
fn runtime_pid(item: &StreamItem) -> u64 {
    item.event.body.data().unwrap()["ext"]["twr"]["pid"]
        .as_u64()
        .unwrap()
}

/// The refusal a thread's agent gets, naming the thread and the process twr started for it.
fn assert_refused(resp: &Value, thread: &str, pid: u64) {
    let e = &resp["error"];
    assert_eq!(e["code"], codes::UNAUTHORIZED, "{resp}");
    let m = e["message"].as_str().unwrap();
    assert!(
        m.starts_with(&format!(
            "this request comes from {thread}'s agent (pid {pid}, a process twr started for it)"
        )),
        "{m}"
    );
    assert!(
        m.contains("only you can, from your own terminal or the TUI"),
        "{m}"
    );
}

/// The refusal a peer gets for its agent marker alone (its ancestry shows no process twr
/// started): the thread the marker names and the peer's own pid.
fn assert_refused_by_marker(resp: &Value, thread: &str, peer: u32) {
    let e = &resp["error"];
    assert_eq!(e["code"], codes::UNAUTHORIZED, "{resp}");
    let m = e["message"].as_str().unwrap();
    assert!(
        m.starts_with(&format!(
            "this request comes from {thread}'s agent (pid {peer}, which carries TWR_AGENT"
        )),
        "{m}"
    );
}

/// A `pty` worker's shell runs a client: it reads (hello, hub.info, thread.list, thread.get,
/// attention.list, control.read, trash.list, thread.transcript, thread.activity, box.agents;
/// of the boxes' agents only whether each is found and signed in) but every other request (the
/// durable spawn's two methods included) is refused with the thread's name and its runtime's
/// pid; a fresh probe of a box's agents is the user's (`unauthorized`). The user's own connection (this test process, which descends from no runtime) does
/// the same requests and gets through. Once the runtime has stopped, nothing is restricted by
/// its old pid.
#[tokio::test]
async fn a_workers_shell_only_reads() {
    // The hub's own agent CLIs, as a fake probe reports them (an agent sees their state only).
    let th = start_with(None, tempfile::tempdir().unwrap(), |o| {
        o.agent_probe = Some(Arc::new(|| {
            futures::FutureExt::boxed(async {
                vec![twr_core::runner::AgentInfo {
                    runtime: "codex".into(),
                    found: true,
                    path: Some("/home/me/.local/bin/codex".into()),
                    version: Some("codex-cli 0.160.0".into()),
                    login: twr_core::runner::AgentLogin::LoggedOut,
                    ..Default::default()
                }]
            })
        }));
    })
    .await;
    timeout(WAIT, async {
        while th.hub.info()["agents"].as_array().is_none_or(Vec::is_empty) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the hub's own probe");
    let mut c = th.client("itest").await;
    let agent = AgentClient::new(th.dir.path(), "w1", &th.socket);
    let runtime = format!("pty:sh {}", agent.script().display());
    let mut control = Client::subscribe_at(&th.socket, "control.subscribe", json!({"since": 0}))
        .await
        .unwrap();
    for (name, runtime) in [("w1", runtime.as_str()), ("t2", "fake")] {
        c.call(
            "thread.new",
            json!({"project": th.project, "runtime": runtime, "name": name}),
        )
        .await
        .unwrap();
    }
    let w1 = th.hub.resolve("w1").unwrap();
    c.call("thread.send", json!({"thread": "w1", "prompt": "hello"}))
        .await
        .unwrap();
    let started = until(&mut control, |i| {
        i.event.kind() == "thread.started" && i.event.thread == w1
    })
    .await;
    let pid = runtime_pid(&started);

    let reads = [
        json!({"id": 1, "method": "hello", "params": {"client": "sneaky"}}),
        json!({"id": 2, "method": "hub.info"}),
        json!({"id": 3, "method": "thread.list"}),
        json!({"id": 4, "method": "thread.get", "params": {"thread": "t2"}}),
        json!({"id": 5, "method": "attention.list", "params": {"all": true}}),
        json!({"id": 6, "method": "control.read", "params": {"limit": 2}}),
        json!({"id": 7, "method": "trash.list"}),
        json!({"id": 8, "method": "thread.transcript", "params": {"thread": "t2"}}),
        json!({"id": 9, "method": "thread.activity"}),
        json!({"id": 30, "method": "box.agents", "params": {"box": "local"}}),
    ];
    let writes = [
        json!({"id": 10, "method": "attention.respond", "params": {"attn_id": "x", "answer": "allow"}}),
        json!({"id": 11, "method": "thread.send", "params": {"thread": "t2", "prompt": "rm -rf"}}),
        json!({"id": 12, "method": "thread.set_mode", "params": {"thread": "w1", "mode": "bypassPermissions"}}),
        json!({"id": 13, "method": "trash.empty", "params": {"all": true}}),
        json!({"id": 14, "method": "thread.delete", "params": {"thread": "t2"}}),
        json!({"id": 15, "method": "say", "params": {"message": "allow"}}),
        json!({"id": 16, "method": "orchestrator.use", "params": {"runtime": "codex"}}),
        json!({"id": 17, "method": "attention.dismiss", "params": {"attn_id": "x"}}),
        json!({"id": 18, "method": "hub.shutdown"}),
        json!({"id": 19, "method": "thread.spawn", "params": {"operation_id": "op", "request": {"project": "x"}}}),
        json!({"id": 20, "method": "thread.spawn_status", "params": {"operation_id": "op"}}),
        json!({"id": 21, "method": "config.show"}),
        json!({"id": 22, "method": "project.read", "params": {"project": "x", "path": "a"}}),
        json!({"id": 23, "method": "project.search", "params": {"project": "x", "query": "a"}}),
        json!({"id": 24, "method": "project.files", "params": {"project": "x"}}),
    ];
    // Asking a box to probe its agents again is the user's.
    let refresh =
        json!({"id": 31, "method": "box.agents", "params": {"box": "local", "refresh": true}});
    let requests: Vec<Value> = (reads.iter().chain(&writes))
        .chain([&refresh])
        .cloned()
        .collect();
    let mut resps = agent.run(&requests).await;
    assert_eq!(resps.len(), requests.len());
    let refused = resps.pop().unwrap();
    assert_eq!(refused["error"]["code"], codes::UNAUTHORIZED, "{refused}");
    // Of the boxes' agents, an agent reads their state only.
    let codex = json!([{"runtime": "codex", "found": true, "login": "logged_out"}]);
    assert_eq!(resps[1]["result"]["agents"], codex, "{}", resps[1]);
    assert_eq!(resps[9]["result"]["agents"], codex, "{}", resps[9]);
    for (req, resp) in reads.iter().zip(&resps) {
        assert!(resp.get("error").is_none(), "{req} -> {resp}");
    }
    assert_eq!(resps[0]["result"]["identity"], "agent:w1");
    assert_eq!(resps[1]["result"]["identity"], "agent:w1");
    let names: Vec<&str> = (resps[2]["result"].as_array().unwrap().iter())
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"w1") && names.contains(&"t2"), "{names:?}");
    for resp in &resps[reads.len()..] {
        assert_refused(resp, "w1", pid);
    }
    // Nothing the refused requests asked for happened.
    let t2 = c.call("thread.get", json!({"thread": "t2"})).await.unwrap();
    assert_eq!(t2["turns"], 0, "{t2}");
    let w = c.call("thread.get", json!({"thread": "w1"})).await.unwrap();
    assert_ne!(w["mode"], "bypassPermissions", "{w}");

    // The user, from a process that descends from no runtime: the same requests go through
    // to the hub (which then judges them as it always does).
    let info = c.call("hub.info", Value::Null).await.unwrap();
    assert_eq!(info["identity"], "user:itest");
    assert_eq!(info["agents"][0]["path"], "/home/me/.local/bin/codex");
    c.call("thread.send", json!({"thread": "t2", "prompt": "hi"}))
        .await
        .unwrap();
    let e = c
        .call(
            "attention.respond",
            json!({"attn_id": "x", "answer": "allow"}),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::NOT_FOUND, "{e:#}");

    // Stopped: the runtime's pid is no longer recorded, and the hub lets go of it.
    c.call("thread.stop", json!({"thread": "w1"}))
        .await
        .unwrap();
    until(&mut control, |i| {
        i.event.kind() == "thread.stopped" && i.event.thread == w1
    })
    .await;
    assert!(th.hub.started_processes().is_empty());
    th.stop().await;
}

/// The orchestrator's MCP server descends from the orchestrator's runtime and presents its
/// token: it stays the orchestrator. Without the token, a process of that runtime is the
/// orchestrator's agent and only reads. Another thread's agent that presents the token (the
/// same OS user can read the MCP server's configuration file) is refused, also when it is a
/// background job that left its shell and carries a forged marker naming the orchestrator's
/// thread (a marker only restricts). So is the orchestrator's own background
/// job that left its shell: its ancestry no longer shows the runtime (accepted: the
/// orchestrator has no shell tools, and its MCP server stays the runtime's child).
#[tokio::test]
async fn the_orchestrator_token_is_the_orchestrators_agent_only() {
    let dir = tempfile::tempdir().unwrap();
    let socket = Paths::with_home(dir.path()).socket;
    let orch_agent = AgentClient::new(dir.path(), "orch", &socket);
    // And a background job of the runtime's that left its shell: its MCP server, as far as
    // the hub can tell (it carries the runtime's marker).
    let orch_bg = AgentClient::new(dir.path(), "orch-bg", &socket);
    let script = orch_agent.dir.join("runtime.sh");
    std::fs::write(
        &script,
        format!(
            "{}\n{}\nsleep 600\n",
            orch_bg.orphan_command(),
            orch_agent.command()
        ),
    )
    .unwrap();
    let (tx, mut spawns) = mpsc::unbounded_channel();
    let orch = Arc::new(ScriptOrch::new(vec![script], tx));
    let th = start(Some(orch), dir).await;
    let mut c = th.client("itest").await;
    let worker = AgentClient::new(th.dir.path(), "w1", &th.socket);
    let forged = AgentClient::new(th.dir.path(), "w1-forged", &th.socket);
    let runtime = format!("pty:sh {}", worker.script().display());
    let mut control = Client::subscribe_at(&th.socket, "control.subscribe", json!({"since": 0}))
        .await
        .unwrap();
    for (name, runtime) in [("w1", runtime.as_str()), ("api", "fake")] {
        c.call(
            "thread.new",
            json!({"project": th.project, "runtime": runtime, "name": name}),
        )
        .await
        .unwrap();
    }
    let r = c
        .call("say", json!({"message": "coordinate the release"}))
        .await
        .unwrap();
    assert_eq!(r["action"], "delegate", "{r}");
    let rt = timeout(WAIT, spawns.recv()).await.unwrap().unwrap();
    let token = rt.token();
    let orch_thread = th.hub.orchestrator_thread().unwrap();
    let orch_name = c
        .call("thread.get", json!({"thread": orch_thread}))
        .await
        .unwrap()["name"]
        .as_str()
        .unwrap()
        .to_string();
    let started = until(&mut control, |i| {
        i.event.kind() == "thread.started" && i.event.thread == orch_thread
    })
    .await;
    let orch_pid = runtime_pid(&started);
    assert_eq!(orch_pid, u64::from(rt.child.id()));

    let resps = orch_agent
        .run(&[
            json!({"id": 1, "method": "hello", "params": {"client": "orchestrator", "token": token, "protocol": PROTOCOL_VERSION}}),
            json!({"id": 2, "method": "hub.info"}),
            json!({"id": 3, "method": "thread.send", "params": {"thread": "api", "prompt": "from the orchestrator"}}),
            json!({"reconnect": true}),
            json!({"id": 4, "method": "hello", "params": {"client": "shell"}}),
            json!({"id": 5, "method": "thread.list"}),
            json!({"id": 6, "method": "thread.send", "params": {"thread": "api", "prompt": "from its shell"}}),
        ])
        .await;
    assert_eq!(
        resps[0]["result"]["identity"], "orchestrator",
        "{}",
        resps[0]
    );
    assert_eq!(resps[1]["result"]["identity"], "orchestrator");
    assert!(resps[2].get("error").is_none(), "{}", resps[2]);
    assert_eq!(resps[3]["result"]["identity"], format!("agent:{orch_name}"));
    assert!(resps[4].get("error").is_none(), "{}", resps[4]);
    assert_refused(&resps[5], &orch_name, orch_pid);

    // The orchestrator's orphaned job: its marker names the orchestrator's thread, but its
    // ancestry does not show the runtime, so the token is refused; with or without it, it is
    // the orchestrator's agent, by the marker.
    let bg = orch_bg.orphaned_from(orch_pid).await;
    let resps = orch_bg
        .run(&[
            json!({"id": 1, "method": "hello", "params": {"client": "orchestrator", "token": token, "protocol": PROTOCOL_VERSION}}),
            json!({"id": 2, "method": "thread.send", "params": {"thread": "api", "prompt": "from the background"}}),
            json!({"reconnect": true}),
            json!({"id": 3, "method": "hello", "params": {"client": "bg"}}),
            json!({"id": 4, "method": "thread.send", "params": {"thread": "api", "prompt": "x"}}),
        ])
        .await;
    assert_token_refused(&resps[0], &format!("{orch_name}'s agent (pid {bg})"));
    assert_refused_by_marker(&resps[1], &orch_name, bg);
    assert_eq!(resps[2]["result"]["identity"], format!("agent:{orch_name}"));
    assert_refused_by_marker(&resps[3], &orch_name, bg);

    // A worker's shell with the orchestrator's token, and a background job of it that left
    // its shell with a marker naming the orchestrator's thread.
    std::fs::write(
        worker.dir.join("runtime.sh"),
        format!(
            "( nohup env TWR_AGENT={orch_thread}:999 {} & )\n{}\nprintf '> '\nwhile read l; do echo \"got [$l]\"; printf '> '; done\n",
            forged.command(),
            worker.command()
        ),
    )
    .unwrap();
    let w1 = th.hub.resolve("w1").unwrap();
    c.call("thread.send", json!({"thread": "w1", "prompt": "hello"}))
        .await
        .unwrap();
    let started = until(&mut control, |i| {
        i.event.kind() == "thread.started" && i.event.thread == w1
    })
    .await;
    let pid = runtime_pid(&started);
    let resps = worker
        .run(&[
            json!({"id": 1, "method": "hello", "params": {"client": "orchestrator", "token": token, "protocol": PROTOCOL_VERSION}}),
            json!({"id": 2, "method": "hub.info"}),
            json!({"id": 3, "method": "thread.send", "params": {"thread": "api", "prompt": "x"}}),
        ])
        .await;
    assert_eq!(
        resps[0]["error"]["code"],
        codes::UNAUTHORIZED,
        "{}",
        resps[0]
    );
    let m = resps[0]["error"]["message"].as_str().unwrap();
    assert!(
        m.contains(&format!("w1's agent (pid {pid})"))
            && m.contains("only the orchestrator's own MCP server"),
        "{m}"
    );
    assert_eq!(resps[1]["result"]["identity"], "agent:w1");
    assert_refused(&resps[2], "w1", pid);
    let job = forged.orphaned_from(pid).await;
    let environ = twr_runner::reap::process_environ(job).unwrap();
    let marker = twr_runner::reap::environ_value(&environ, "TWR_AGENT").unwrap_or_default();
    assert_eq!(marker, format!("{orch_thread}:999"));
    let resps = forged
        .run(&[
            json!({"id": 1, "method": "hello", "params": {"client": "orchestrator", "token": token, "protocol": PROTOCOL_VERSION}}),
            json!({"id": 2, "method": "thread.send", "params": {"thread": "api", "prompt": "forged"}}),
        ])
        .await;
    assert_token_refused(&resps[0], &format!("{orch_name}'s agent (pid {job})"));
    assert_refused_by_marker(&resps[1], &orch_name, job);
    // Nothing a refused connection sent reached the thread: only the orchestrator's message.
    let api = c
        .call("thread.get", json!({"thread": "api"}))
        .await
        .unwrap();
    assert_eq!(api["turns"], 1, "{api}");
    drop(rt);
    th.stop().await;
}

/// The refusal of the orchestrator token from a process whose ancestry does not show the
/// orchestrator's runtime, naming it as `who`.
fn assert_token_refused(resp: &Value, who: &str) {
    let e = &resp["error"];
    assert_eq!(e["code"], codes::UNAUTHORIZED, "{resp}");
    let m = e["message"].as_str().unwrap();
    assert!(
        m.starts_with(&format!(
            "this connection comes from {who}, which is neither the orchestrator's runtime nor \
             a process it started"
        )),
        "{m}"
    );
}

/// The orchestrator token is proven against the runtime it belongs to, with the peer's
/// ancestry walked again each time. A process of the orchestrator's runtime
/// that outlives a restart of it keeps the connection it opened, but presenting the new
/// runtime's token there is refused (it descends from the earlier runtime, not from the new
/// one), and so is every request on a connection that was the orchestrator before the restart;
/// the new runtime's MCP server is the orchestrator. A connection that was the orchestrator
/// and whose peer then left the runtime (its ancestry no longer shows it, the token still
/// valid) can no longer act either.
#[tokio::test]
async fn the_orchestrator_token_is_proven_against_the_current_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let socket = Paths::with_home(dir.path()).socket;
    // Runtime A: a shell that only reads and the MCP server, both outliving A (they cleared
    // the markers its kill goes by), and a client whose parent (a subshell of A) exits once
    // told to.
    let old = AgentClient::new(dir.path(), "old", &socket);
    let mcp_a = AgentClient::new(dir.path(), "mcp-a", &socket);
    let left = AgentClient::new(dir.path(), "left", &socket);
    let leave = left.dir.join("leave");
    let script_a = old.dir.join("runtime.sh");
    std::fs::write(
        &script_a,
        format!(
            "{} &\n{} &\n( {} & while [ ! -e {} ]; do sleep 0.05; done ) &\nexec sleep 600\n",
            old.command_surviving(),
            mcp_a.command_surviving(),
            left.command(),
            leave.display()
        ),
    )
    .unwrap();
    // Runtime B, after the restart: its MCP server.
    let mcp_b = AgentClient::new(dir.path(), "mcp-b", &socket);
    let script_b = mcp_b.dir.join("runtime.sh");
    std::fs::write(
        &script_b,
        format!("{} &\nexec sleep 600\n", mcp_b.command()),
    )
    .unwrap();
    let (tx, mut spawns) = mpsc::unbounded_channel();
    let orch = Arc::new(ScriptOrch::new(vec![script_a, script_b], tx));
    let th = start(Some(orch), dir).await;
    let mut c = th.client("itest").await;
    for name in ["api", "web", "db", "ops"] {
        c.call(
            "thread.new",
            json!({"project": th.project, "runtime": "fake", "name": name}),
        )
        .await
        .unwrap();
    }
    let mut control = Client::subscribe_at(&th.socket, "control.subscribe", json!({"since": 0}))
        .await
        .unwrap();
    let r = c
        .call("say", json!({"message": "coordinate the release"}))
        .await
        .unwrap();
    assert_eq!(r["action"], "delegate", "{r}");
    let rt_a = timeout(WAIT, spawns.recv()).await.unwrap().unwrap();
    let token_a = rt_a.token();
    let orch_thread = th.hub.orchestrator_thread().unwrap();
    let orch_name = c
        .call("thread.get", json!({"thread": orch_thread}))
        .await
        .unwrap()["name"]
        .as_str()
        .unwrap()
        .to_string();
    let started = until(&mut control, |i| {
        i.event.kind() == "thread.started" && i.event.thread == orch_thread
    })
    .await;
    let pid_a = runtime_pid(&started);
    assert_eq!(pid_a, u64::from(rt_a.child.id()));

    let hello_a = json!({"id": 1, "method": "hello", "params": {"client": "orchestrator", "token": token_a, "protocol": PROTOCOL_VERSION}});
    old.start(&[
        json!({"id": 1, "method": "hello", "params": {"client": "shell"}}),
        json!({"mark": "connected"}),
        json!({"wait": "restarted"}),
        json!({"from": "hello-b"}),
        json!({"id": 3, "method": "thread.send", "params": {"thread": "web", "prompt": "old"}}),
        json!({"id": 4, "method": "hub.info"}),
    ]);
    mcp_a.start(&[
        hello_a.clone(),
        json!({"id": 2, "method": "thread.send", "params": {"thread": "api", "prompt": "from A"}}),
        json!({"mark": "authed"}),
        json!({"wait": "restarted"}),
        json!({"id": 3, "method": "thread.send", "params": {"thread": "web", "prompt": "stale"}}),
        json!({"from": "hello-b"}),
        json!({"id": 5, "method": "thread.send", "params": {"thread": "web", "prompt": "replayed"}}),
    ]);
    left.start(&[
        hello_a,
        json!({"id": 2, "method": "thread.send", "params": {"thread": "db", "prompt": "from A"}}),
        json!({"mark": "authed"}),
        json!({"wait": "left"}),
        json!({"id": 3, "method": "thread.send", "params": {"thread": "web", "prompt": "left"}}),
        json!({"id": 4, "method": "thread.list"}),
    ]);
    old.reached("connected").await;
    mcp_a.reached("authed").await;
    left.reached("authed").await;

    // The client's parent exits while runtime A still runs and its token is still valid: the
    // client no longer descends from A, and its connection can no longer act.
    std::fs::write(&leave, "").unwrap();
    left.orphaned_from(pid_a).await;
    assert!(th.hub.orchestrator_token_is(&token_a));
    left.go_on("left");
    let resps = left.responses().await;
    assert_eq!(
        resps[0]["result"]["identity"], "orchestrator",
        "{}",
        resps[0]
    );
    assert!(resps[1].get("error").is_none(), "{}", resps[1]);
    for r in &resps[2..] {
        assert_eq!(r["error"]["code"], codes::UNAUTHORIZED, "{r}");
        let m = r["error"]["message"].as_str().unwrap();
        assert!(
            m.starts_with(
                "this connection no longer comes from the orchestrator's current runtime"
            ),
            "{m}"
        );
    }

    // The orchestrator restarts: runtime A exits (what it started lives on), B starts with a
    // new token.
    c.call("thread.stop", json!({"thread": orch_thread}))
        .await
        .unwrap();
    drop(rt_a);
    until(&mut control, |i| {
        i.event.kind() == "thread.stopped" && i.event.thread == orch_thread
    })
    .await;
    c.call(
        "thread.send",
        json!({"thread": orch_thread, "prompt": "back"}),
    )
    .await
    .unwrap();
    let rt_b = timeout(WAIT, spawns.recv()).await.unwrap().unwrap();
    let token_b = rt_b.token();
    assert_ne!(token_b, token_a);
    let started = until(&mut control, |i| {
        i.event.kind() == "thread.started" && i.event.thread == orch_thread
    })
    .await;
    let pid_b = runtime_pid(&started);
    assert_eq!(pid_b, u64::from(rt_b.child.id()));
    old.orphaned_from(pid_a).await;
    mcp_a.orphaned_from(pid_a).await;

    // The new runtime's MCP server is the orchestrator.
    let resps = mcp_b
        .run(&[
            json!({"id": 1, "method": "hello", "params": {"client": "orchestrator", "token": token_b, "protocol": PROTOCOL_VERSION}}),
            json!({"id": 2, "method": "thread.send", "params": {"thread": "ops", "prompt": "from B"}}),
        ])
        .await;
    assert_eq!(
        resps[0]["result"]["identity"], "orchestrator",
        "{}",
        resps[0]
    );
    assert!(resps[1].get("error").is_none(), "{}", resps[1]);

    // Runtime A's processes present B's token on the connections they opened under A.
    let hello_b = json!({"id": 2, "method": "hello", "params": {"client": "orchestrator", "token": token_b, "protocol": PROTOCOL_VERSION}});
    for client in [&old, &mcp_a] {
        std::fs::write(client.dir.join("hello-b"), hello_b.to_string()).unwrap();
        client.go_on("restarted");
    }
    let only_mcp = "only the orchestrator's own MCP server may present the orchestrator token";
    let resps = old.responses().await;
    assert_eq!(resps[0]["result"]["identity"], format!("agent:{orch_name}"));
    assert_eq!(
        resps[1]["error"]["code"],
        codes::UNAUTHORIZED,
        "{}",
        resps[1]
    );
    let m = resps[1]["error"]["message"].as_str().unwrap();
    assert!(
        m.contains(&format!("{orch_name}'s agent (pid {pid_a})")) && m.contains(only_mcp),
        "{m}"
    );
    assert_refused(&resps[2], &orch_name, pid_a);
    assert_eq!(resps[3]["result"]["identity"], format!("agent:{orch_name}"));
    let resps = mcp_a.responses().await;
    assert_eq!(
        resps[0]["result"]["identity"], "orchestrator",
        "{}",
        resps[0]
    );
    assert!(resps[1].get("error").is_none(), "{}", resps[1]);
    for r in &resps[2..] {
        assert_eq!(r["error"]["code"], codes::UNAUTHORIZED, "{r}");
    }
    let m = resps[3]["error"]["message"].as_str().unwrap();
    assert!(m.contains(only_mcp), "{m}");
    // Only the accepted requests reached a thread.
    for (name, turns) in [("api", 1), ("db", 1), ("ops", 1), ("web", 0)] {
        let t = c.call("thread.get", json!({"thread": name})).await.unwrap();
        assert_eq!(t["turns"], turns, "{name}: {t}");
    }
    drop(rt_b);
    th.stop().await;
}

/// The interactive process of an attach, once its client reported it (`attach.child`), is the
/// thread's agent: it, and what it starts, only read.
#[tokio::test]
async fn an_attached_process_only_reads() {
    let th = start(None, tempfile::tempdir().unwrap()).await;
    let mut c = th.client("itest").await;
    let mut control = Client::subscribe_at(&th.socket, "control.subscribe", json!({"since": 0}))
        .await
        .unwrap();
    c.call(
        "thread.new",
        json!({"project": th.project, "runtime": "fake", "name": "t1"}),
    )
    .await
    .unwrap();
    c.call("thread.send", json!({"thread": "t1", "prompt": "hello"}))
        .await
        .unwrap();
    until(&mut control, |i| i.event.kind() == "report.ready").await;

    let mut lease = th.client("tty").await;
    let grant = lease
        .call("attach.begin", json!({"thread": "t1"}))
        .await
        .unwrap();
    until(&mut control, |i| i.event.kind() == "thread.attached").await;
    // The stand-in for the runtime's interactive CLI: a shell (carrying the grant's marker)
    // whose child is the client.
    let agent = AgentClient::new(th.dir.path(), "cli", &th.socket);
    let mut cli = std::process::Command::new("sh")
        .args(["-c", &format!("{}; sleep 600", agent.command())])
        .env("TWR_ATTACH", grant["env"]["TWR_ATTACH"].as_str().unwrap())
        .spawn()
        .unwrap();
    let pid = cli.id();
    let start_time = twr_runner::reap::process_start_time(pid).unwrap();
    lease
        .call(
            "attach.child",
            json!({"thread": "t1", "pid": pid, "start_time": start_time}),
        )
        .await
        .unwrap();
    let resps = agent
        .run(&[
            json!({"id": 1, "method": "thread.list"}),
            json!({"id": 2, "method": "thread.send", "params": {"thread": "t1", "prompt": "x"}}),
            json!({"id": 3, "method": "attach.end", "params": {"thread": "t1"}}),
        ])
        .await;
    let _ = cli.kill();
    let _ = cli.wait();
    assert!(resps[0].get("error").is_none(), "{}", resps[0]);
    assert_refused(&resps[1], "t1", u64::from(pid));
    assert_refused(&resps[2], "t1", u64::from(pid));
    let t = c.call("thread.get", json!({"thread": "t1"})).await.unwrap();
    assert_eq!(t["pending"], 0, "{t}");
    lease
        .call("attach.end", json!({"thread": "t1"}))
        .await
        .unwrap();
    th.stop().await;
}

/// The reported repro: a worker's shell starts the client as a background
/// job (`nohup ... &`) of a shell that exits at once. Reparented, its ancestry no longer
/// reaches the runtime, but it carries the runtime's agent marker: it only reads, refused by
/// the marker with its own pid. The user's own connection is still the user.
#[tokio::test]
async fn a_background_job_that_left_its_shell_only_reads() {
    let th = start(None, tempfile::tempdir().unwrap()).await;
    let mut c = th.client("itest").await;
    let agent = AgentClient::new(th.dir.path(), "w1", &th.socket);
    let runtime = format!(
        "pty:sh {}",
        agent.script_running(&agent.orphan_command()).display()
    );
    let mut control = Client::subscribe_at(&th.socket, "control.subscribe", json!({"since": 0}))
        .await
        .unwrap();
    for (name, runtime) in [("w1", runtime.as_str()), ("t2", "fake")] {
        c.call(
            "thread.new",
            json!({"project": th.project, "runtime": runtime, "name": name}),
        )
        .await
        .unwrap();
    }
    let w1 = th.hub.resolve("w1").unwrap();
    c.call("thread.send", json!({"thread": "w1", "prompt": "hello"}))
        .await
        .unwrap();
    let started = until(&mut control, |i| {
        i.event.kind() == "thread.started" && i.event.thread == w1
    })
    .await;
    let pid = runtime_pid(&started);
    let client = agent.orphaned_from(pid).await;
    let environ = twr_runner::reap::process_environ(client).unwrap();
    let marker = twr_runner::reap::environ_value(&environ, "TWR_AGENT").unwrap_or_default();
    assert!(marker.starts_with(&format!("{w1}:")), "{marker:?}");
    let resps = agent
        .run(&[
            json!({"id": 1, "method": "hello", "params": {"client": "sneaky"}}),
            json!({"id": 2, "method": "thread.list"}),
            json!({"id": 3, "method": "attention.respond", "params": {"attn_id": "x", "answer": "allow"}}),
            json!({"id": 4, "method": "thread.send", "params": {"thread": "t2", "prompt": "rm -rf"}}),
        ])
        .await;
    assert_eq!(resps[0]["result"]["identity"], "agent:w1", "{}", resps[0]);
    assert!(resps[1].get("error").is_none(), "{}", resps[1]);
    assert_refused_by_marker(&resps[2], "w1", client);
    assert_refused_by_marker(&resps[3], "w1", client);
    let t2 = c.call("thread.get", json!({"thread": "t2"})).await.unwrap();
    assert_eq!(t2["turns"], 0, "{t2}");
    let info = c.call("hub.info", Value::Null).await.unwrap();
    assert_eq!(info["identity"], "user:itest");
    th.stop().await;
}

/// A worker's background job that left its shell and made itself not dumpable: its ancestry
/// no longer reaches the runtime and its environment, which still carries the marker, cannot
/// be read. That is not an absent marker: it only reads, as a
/// process twr cannot trace. The user's own connection is still the user.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_background_job_that_hides_its_environment_only_reads() {
    let th = start(None, tempfile::tempdir().unwrap()).await;
    let mut c = th.client("itest").await;
    let agent = AgentClient::new(th.dir.path(), "w1", &th.socket);
    let runtime = format!(
        "pty:sh {}",
        agent
            .script_running(&agent.orphan_non_dumpable_command())
            .display()
    );
    let mut control = Client::subscribe_at(&th.socket, "control.subscribe", json!({"since": 0}))
        .await
        .unwrap();
    for (name, runtime) in [("w1", runtime.as_str()), ("t2", "fake")] {
        c.call(
            "thread.new",
            json!({"project": th.project, "runtime": runtime, "name": name}),
        )
        .await
        .unwrap();
    }
    let w1 = th.hub.resolve("w1").unwrap();
    c.call("thread.send", json!({"thread": "w1", "prompt": "hello"}))
        .await
        .unwrap();
    let started = until(&mut control, |i| {
        i.event.kind() == "thread.started" && i.event.thread == w1
    })
    .await;
    let pid = runtime_pid(&started);
    let client = agent.orphaned_from(pid).await;
    // It made itself not dumpable before it wrote its pid.
    assert!(twr_runner::reap::process_environ(client).is_err());
    let resps = agent
        .run(&[
            json!({"id": 1, "method": "hello", "params": {"client": "sneaky"}}),
            json!({"id": 2, "method": "thread.list"}),
            json!({"id": 3, "method": "attention.respond", "params": {"attn_id": "x", "answer": "allow"}}),
            json!({"id": 4, "method": "thread.send", "params": {"thread": "t2", "prompt": "rm -rf"}}),
        ])
        .await;
    assert_eq!(
        resps[0]["result"]["identity"], "agent:unknown",
        "{}",
        resps[0]
    );
    assert!(resps[1].get("error").is_none(), "{}", resps[1]);
    for resp in &resps[2..] {
        let e = &resp["error"];
        assert_eq!(e["code"], codes::UNAUTHORIZED, "{resp}");
        let m = e["message"].as_str().unwrap();
        assert!(
            m.starts_with(&format!(
                "this request comes from a process twr cannot trace (pid {client}: its \
                 environment, which would show TWR_AGENT, cannot be read)"
            )),
            "{m}"
        );
    }
    let t2 = c.call("thread.get", json!({"thread": "t2"})).await.unwrap();
    assert_eq!(t2["turns"], 0, "{t2}");
    let info = c.call("hub.info", Value::Null).await.unwrap();
    assert_eq!(info["identity"], "user:itest");
    th.stop().await;
}

/// A worker's background job that left its shell (carrying the marker) connects and, while
/// the hub classifies the connection, after it walked the job's ancestry and before it reads
/// the environments (a test hook holds it there), hands the connection to a child and exits.
/// The job's environment can no longer be read, which is not an absent
/// marker: the connection, now its child's, only reads, as a process twr cannot trace. The
/// user's own connection is still the user.
#[tokio::test]
async fn a_peer_that_hands_its_connection_on_while_classified_only_reads() {
    let target = Arc::new(AtomicU32::new(0));
    let paused = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let hook = {
        let (target, paused, release) = (target.clone(), paused.clone(), release.clone());
        move |peer: u32| {
            if peer != target.load(Ordering::SeqCst) {
                return;
            }
            paused.store(true, Ordering::SeqCst);
            let deadline = Instant::now() + WAIT;
            while !release.load(Ordering::SeqCst) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    };
    let th = start_with(None, tempfile::tempdir().unwrap(), |o| {
        o.marker_read_hook = Some(Arc::new(hook));
    })
    .await;
    let mut c = th.client("itest").await;
    let agent = AgentClient::new(th.dir.path(), "w1", &th.socket);
    let runtime = format!(
        "pty:sh {}",
        agent.script_running(&agent.orphan_command()).display()
    );
    let mut control = Client::subscribe_at(&th.socket, "control.subscribe", json!({"since": 0}))
        .await
        .unwrap();
    for (name, runtime) in [("w1", runtime.as_str()), ("t2", "fake")] {
        c.call(
            "thread.new",
            json!({"project": th.project, "runtime": runtime, "name": name}),
        )
        .await
        .unwrap();
    }
    let w1 = th.hub.resolve("w1").unwrap();
    c.call("thread.send", json!({"thread": "w1", "prompt": "hello"}))
        .await
        .unwrap();
    let started = until(&mut control, |i| {
        i.event.kind() == "thread.started" && i.event.thread == w1
    })
    .await;
    let pid = runtime_pid(&started);
    let client = agent.orphaned_from(pid).await;
    let environ = twr_runner::reap::process_environ(client).unwrap();
    let marker = twr_runner::reap::environ_value(&environ, "TWR_AGENT").unwrap_or_default();
    assert!(marker.starts_with(&format!("{w1}:")), "{marker:?}");
    target.store(client, Ordering::SeqCst);
    agent.start(&[
        json!({"handoff": "heir"}),
        json!({"id": 1, "method": "hello", "params": {"client": "sneaky"}}),
        json!({"id": 2, "method": "thread.list"}),
        json!({"id": 3, "method": "attention.respond", "params": {"attn_id": "x", "answer": "allow"}}),
        json!({"id": 4, "method": "thread.send", "params": {"thread": "t2", "prompt": "rm -rf"}}),
    ]);
    // The hub has walked the job's ancestry (whole: it was reparented) and waits to read the
    // environments; the job hands the connection on and exits.
    let deadline = Instant::now() + WAIT;
    while !paused.load(Ordering::SeqCst) {
        assert!(
            Instant::now() < deadline,
            "the hub never classified the job"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    agent.go_on("heir");
    while twr_runner::reap::process_alive(client) {
        assert!(Instant::now() < deadline, "the job never exited");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    release.store(true, Ordering::SeqCst);
    let resps = agent.responses().await;
    assert_eq!(
        resps[0]["result"]["identity"], "agent:unknown",
        "{}",
        resps[0]
    );
    assert!(resps[1].get("error").is_none(), "{}", resps[1]);
    for resp in &resps[2..] {
        let e = &resp["error"];
        assert_eq!(e["code"], codes::UNAUTHORIZED, "{resp}");
        let m = e["message"].as_str().unwrap();
        assert!(
            m.starts_with(&format!(
                "this request comes from a process twr cannot trace (pid {client}: its \
                 ancestry could not be read to the end)"
            )),
            "{m}"
        );
    }
    let t2 = c.call("thread.get", json!({"thread": "t2"})).await.unwrap();
    assert_eq!(t2["turns"], 0, "{t2}");
    let info = c.call("hub.info", Value::Null).await.unwrap();
    assert_eq!(info["identity"], "user:itest");
    th.stop().await;
}

/// A tool command 70 shells below its runtime, without the marker: the walk
/// goes on to the runtime (no depth of 64 that grants the user's authority).
#[tokio::test]
async fn a_deep_chain_without_the_marker_only_reads() {
    let th = start(None, tempfile::tempdir().unwrap()).await;
    let mut c = th.client("itest").await;
    let agent = AgentClient::new(th.dir.path(), "w1", &th.socket);
    // Each level is a shell running the next; `; true` keeps a shell from exec'ing its last
    // command, so every level stays a process.
    let nest = agent.dir.join("nest.sh");
    std::fs::write(
        &nest,
        format!(
            "if [ \"$1\" -gt 0 ]; then sh \"$0\" $(($1 - 1)); true; else {}; true; fi\n",
            agent.command_without_marker()
        ),
    )
    .unwrap();
    let runtime = format!(
        "pty:sh {}",
        agent
            .script_running(&format!("sh {} 70", nest.display()))
            .display()
    );
    let mut control = Client::subscribe_at(&th.socket, "control.subscribe", json!({"since": 0}))
        .await
        .unwrap();
    c.call(
        "thread.new",
        json!({"project": th.project, "runtime": runtime, "name": "w1"}),
    )
    .await
    .unwrap();
    let w1 = th.hub.resolve("w1").unwrap();
    c.call("thread.send", json!({"thread": "w1", "prompt": "hello"}))
        .await
        .unwrap();
    let started = until(&mut control, |i| {
        i.event.kind() == "thread.started" && i.event.thread == w1
    })
    .await;
    let pid = runtime_pid(&started);
    let client = agent.pid().await;
    let a = twr_runner::reap::ancestry(client);
    let depth = a.chain.iter().position(|p| u64::from(p.pid) == pid);
    assert!(depth.is_some_and(|d| d > 70), "{depth:?}: {a:?}");
    let environ = twr_runner::reap::process_environ(client).unwrap();
    assert!(!String::from_utf8_lossy(&environ).contains("TWR_AGENT="));
    let resps = agent
        .run(&[
            json!({"id": 1, "method": "thread.list"}),
            json!({"id": 2, "method": "thread.send", "params": {"thread": "w1", "prompt": "x"}}),
        ])
        .await;
    assert!(resps[0].get("error").is_none(), "{}", resps[0]);
    assert_refused(&resps[1], "w1", pid);
    th.stop().await;
}

/// A shell between the runtime and the client whose command name is not UTF-8 (a byte
/// that is not, and a multibyte character cut by the 15-byte limit) does not stop the
/// walk: the client, without the marker, is the runtime's agent and only reads.
#[cfg_attr(
    target_os = "macos",
    ignore = "APFS refuses file names that are not UTF-8"
)]
#[tokio::test]
async fn a_command_name_that_is_not_utf8_does_not_hide_the_runtime() {
    use std::os::unix::ffi::OsStrExt;
    let th = start(None, tempfile::tempdir().unwrap()).await;
    let mut c = th.client("itest").await;
    let agent = AgentClient::new(th.dir.path(), "w1", &th.socket);
    let sh = Path::new("/bin/sh");
    let bad = agent.dir.join(std::ffi::OsStr::from_bytes(b"bad\xffsh"));
    std::os::unix::fs::symlink(sh, &bad).unwrap();
    // 14 bytes, then a two-byte character: the command name keeps its first byte only.
    let cut = agent.dir.join("aaaaaaaaaaaaaa\u{e9}sh");
    std::os::unix::fs::symlink(sh, &cut).unwrap();
    let inner = agent.dir.join("inner.sh");
    std::fs::write(
        &inner,
        format!("{}; true\n", agent.command_without_marker()),
    )
    .unwrap();
    let outer = agent.dir.join("outer.sh");
    std::fs::write(
        &outer,
        format!("{} {}; true\n", cut.display(), inner.display()),
    )
    .unwrap();
    // The glob names the shell whose name is not UTF-8 (a script line is text).
    let run = format!("{}/bad*sh {}; true", agent.dir.display(), outer.display());
    let runtime = format!("pty:sh {}", agent.script_running(&run).display());
    let mut control = Client::subscribe_at(&th.socket, "control.subscribe", json!({"since": 0}))
        .await
        .unwrap();
    c.call(
        "thread.new",
        json!({"project": th.project, "runtime": runtime, "name": "w1"}),
    )
    .await
    .unwrap();
    let w1 = th.hub.resolve("w1").unwrap();
    c.call("thread.send", json!({"thread": "w1", "prompt": "hello"}))
        .await
        .unwrap();
    let started = until(&mut control, |i| {
        i.event.kind() == "thread.started" && i.event.thread == w1
    })
    .await;
    let pid = runtime_pid(&started);
    let client = agent.pid().await;
    let a = twr_runner::reap::ancestry(client);
    let pids: Vec<u64> = a.chain.iter().map(|p| u64::from(p.pid)).collect();
    assert_eq!(pids.get(3), Some(&pid), "{a:?}");
    for shell in &a.chain[1..3] {
        let stat = std::fs::read(format!("/proc/{}/stat", shell.pid)).unwrap();
        assert!(std::str::from_utf8(&stat).is_err(), "{stat:?}");
    }
    let environ = twr_runner::reap::process_environ(client).unwrap();
    assert!(!String::from_utf8_lossy(&environ).contains("TWR_AGENT="));
    let resps = agent
        .run(&[
            json!({"id": 1, "method": "thread.list"}),
            json!({"id": 2, "method": "thread.send", "params": {"thread": "w1", "prompt": "x"}}),
        ])
        .await;
    assert!(resps[0].get("error").is_none(), "{}", resps[0]);
    assert_refused(&resps[1], "w1", pid);
    th.stop().await;
}

/// A runtime whose start time cannot be read when it starts: one failed
/// read is retried, and a runtime whose reads keep failing is matched by pid alone while it
/// runs. Either way its child, without the marker, only reads.
#[tokio::test]
async fn a_runtime_whose_start_time_read_failed_still_restricts() {
    for faults in [1, 1000] {
        let th = start_with(None, tempfile::tempdir().unwrap(), |o| {
            o.start_time_faults = faults;
        })
        .await;
        let mut c = th.client("itest").await;
        let agent = AgentClient::new(th.dir.path(), "w1", &th.socket);
        let runtime = format!(
            "pty:sh {}",
            agent
                .script_running(&agent.command_without_marker())
                .display()
        );
        let mut control =
            Client::subscribe_at(&th.socket, "control.subscribe", json!({"since": 0}))
                .await
                .unwrap();
        c.call(
            "thread.new",
            json!({"project": th.project, "runtime": runtime, "name": "w1"}),
        )
        .await
        .unwrap();
        let w1 = th.hub.resolve("w1").unwrap();
        c.call("thread.send", json!({"thread": "w1", "prompt": "hello"}))
            .await
            .unwrap();
        let started = until(&mut control, |i| {
            i.event.kind() == "thread.started" && i.event.thread == w1
        })
        .await;
        let pid = runtime_pid(&started);
        let stamped = started.event.body.data().unwrap()["ext"]["twr"]
            .get("start_time")
            .cloned();
        let table = th.hub.started_processes();
        if faults == 1 {
            assert!(stamped.is_some(), "faults {faults}");
            assert!(
                table.exact.keys().any(|p| u64::from(p.pid) == pid),
                "{table:?}"
            );
        } else {
            assert_eq!(stamped, None, "faults {faults}");
            assert!(table.exact.is_empty(), "{table:?}");
            assert!(
                table.by_pid.keys().any(|p| u64::from(*p) == pid),
                "{table:?}"
            );
        }
        let resps = agent
            .run(&[
                json!({"id": 1, "method": "hub.info"}),
                json!({"id": 2, "method": "thread.send", "params": {"thread": "w1", "prompt": "x"}}),
            ])
            .await;
        assert_eq!(resps[0]["result"]["identity"], "agent:w1", "{}", resps[0]);
        assert_refused(&resps[1], "w1", pid);
        // Once it has stopped, nothing is matched by its pid.
        c.call("thread.stop", json!({"thread": "w1"}))
            .await
            .unwrap();
        until(&mut control, |i| {
            i.event.kind() == "thread.stopped" && i.event.thread == w1
        })
        .await;
        assert!(th.hub.started_processes().is_empty());
        th.stop().await;
    }
}

/// A runtime's process is the thread's from the moment its adapter's spawn returns, before
/// the runtime reports it: a child that cleared its marker and connects
/// before `thread.started` is recorded only reads (refused naming the runtime's pid, as a
/// process twr started), and the runtime's MCP server presenting the token then is the
/// orchestrator. A connection's classification is kept: once the runtime has exited and
/// nothing is recorded any more, the child's connection is still refused.
#[tokio::test]
async fn a_runtime_is_known_from_its_spawn_on() {
    let dir = tempfile::tempdir().unwrap();
    let socket = Paths::with_home(dir.path()).socket;
    let shell = AgentClient::new(dir.path(), "shell", &socket);
    let mcp = AgentClient::new(dir.path(), "mcp", &socket);
    let script = shell.dir.join("runtime.sh");
    std::fs::write(
        &script,
        format!(
            "{} &\n{} &\nsleep 600\n",
            shell.command_without_marker(),
            mcp.command()
        ),
    )
    .unwrap();
    let (tx, mut spawns) = mpsc::unbounded_channel();
    let orch = Arc::new(HeldOrch {
        script: std::sync::Mutex::new(Some(script)),
        spawns: tx,
    });
    let th = start(Some(orch), dir).await;
    let mut c = th.client("itest").await;
    for name in ["api", "web"] {
        c.call(
            "thread.new",
            json!({"project": th.project, "runtime": "fake", "name": name}),
        )
        .await
        .unwrap();
    }
    let r = c
        .call("say", json!({"message": "coordinate the release"}))
        .await
        .unwrap();
    assert_eq!(r["action"], "delegate", "{r}");
    let rt = timeout(WAIT, spawns.recv()).await.unwrap().unwrap();
    let pid = rt.child.id();
    let orch_thread = th.hub.orchestrator_thread().unwrap();
    let orch_name = c
        .call("thread.get", json!({"thread": orch_thread}))
        .await
        .unwrap()["name"]
        .as_str()
        .unwrap()
        .to_string();
    // Nothing reported yet: the hub knows the process from the spawn alone.
    let table = th.hub.started_processes();
    assert!(table.exact.is_empty(), "{table:?}");
    assert_eq!(table.by_pid.keys().collect::<Vec<_>>(), [&pid], "{table:?}");
    let child = shell.pid().await;
    let environ = twr_runner::reap::process_environ(child).unwrap();
    assert!(!String::from_utf8_lossy(&environ).contains("TWR_AGENT="));

    shell.start(&[
        json!({"id": 1, "method": "hello", "params": {"client": "shell"}}),
        json!({"id": 2, "method": "thread.send", "params": {"thread": "api", "prompt": "early"}}),
        json!({"mark": "asked"}),
        json!({"wait": "later"}),
        json!({"id": 3, "method": "thread.send", "params": {"thread": "api", "prompt": "later"}}),
        json!({"id": 4, "method": "hub.info"}),
    ]);
    let resps = mcp
        .run(&[
            json!({"id": 1, "method": "hello", "params": {"client": "orchestrator", "token": rt.token(), "protocol": PROTOCOL_VERSION}}),
            json!({"id": 2, "method": "thread.send", "params": {"thread": "api", "prompt": "from the orchestrator"}}),
        ])
        .await;
    assert_eq!(
        resps[0]["result"]["identity"], "orchestrator",
        "{}",
        resps[0]
    );
    assert!(resps[1].get("error").is_none(), "{}", resps[1]);
    shell.reached("asked").await;
    // Still nothing recorded while those connections were classified.
    assert!(th.hub.started_processes().exact.is_empty());

    // The runtime reports its process, then exits: nothing is recorded any more.
    let mut control = Client::subscribe_at(&th.socket, "control.subscribe", json!({"since": 0}))
        .await
        .unwrap();
    rt.report();
    until(&mut control, |i| {
        i.event.kind() == "thread.started" && i.event.thread == orch_thread
    })
    .await;
    rt.exit();
    let deadline = Instant::now() + WAIT;
    while !th.hub.started_processes().is_empty() {
        assert!(Instant::now() < deadline, "the runtime is still recorded");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    shell.go_on("later");
    let resps = shell.responses().await;
    assert_eq!(resps[0]["result"]["identity"], format!("agent:{orch_name}"));
    assert_refused(&resps[1], &orch_name, u64::from(pid));
    assert_refused(&resps[2], &orch_name, u64::from(pid));
    assert_eq!(resps[3]["result"]["identity"], format!("agent:{orch_name}"));
    let api = c
        .call("thread.get", json!({"thread": "api"}))
        .await
        .unwrap();
    assert_eq!(api["turns"], 1, "{api}");
    let _ = twr_runner::reap::signal(child, false, "KILL");
    th.stop().await;
}

/// An attach's interactive process carries the grant's marker from its start, before its
/// client reports it (`attach.child`): what it starts only reads, also a child that cleared
/// the marker (the nearest carrier of its ancestry counts).
#[tokio::test]
async fn an_attached_process_restricts_before_it_is_reported() {
    let th = start(None, tempfile::tempdir().unwrap()).await;
    let mut c = th.client("itest").await;
    let mut control = Client::subscribe_at(&th.socket, "control.subscribe", json!({"since": 0}))
        .await
        .unwrap();
    c.call(
        "thread.new",
        json!({"project": th.project, "runtime": "fake", "name": "t1"}),
    )
    .await
    .unwrap();
    c.call("thread.send", json!({"thread": "t1", "prompt": "hello"}))
        .await
        .unwrap();
    until(&mut control, |i| i.event.kind() == "report.ready").await;
    let mut lease = th.client("tty").await;
    let grant = lease
        .call("attach.begin", json!({"thread": "t1"}))
        .await
        .unwrap();
    until(&mut control, |i| i.event.kind() == "thread.attached").await;
    // The stand-in for the interactive CLI, with the grant's environment, whose child clears
    // the marker; never reported.
    let agent = AgentClient::new(th.dir.path(), "cli", &th.socket);
    let mut cli = std::process::Command::new("sh")
        .args([
            "-c",
            &format!("{}; sleep 600", agent.command_without_marker()),
        ])
        .env("TWR_ATTACH", grant["env"]["TWR_ATTACH"].as_str().unwrap())
        .env("TWR_AGENT", grant["env"]["TWR_AGENT"].as_str().unwrap())
        .spawn()
        .unwrap();
    let client = agent.pid().await;
    let environ = twr_runner::reap::process_environ(client).unwrap();
    assert!(!String::from_utf8_lossy(&environ).contains("TWR_AGENT="));
    assert!(th.hub.started_processes().is_empty());
    let resps = agent
        .run(&[
            json!({"id": 1, "method": "thread.list"}),
            json!({"id": 2, "method": "thread.send", "params": {"thread": "t1", "prompt": "x"}}),
        ])
        .await;
    let _ = cli.kill();
    let _ = cli.wait();
    assert!(resps[0].get("error").is_none(), "{}", resps[0]);
    let m = resps[1]["error"]["message"].as_str().unwrap_or_default();
    assert!(
        m.starts_with(&format!(
            "this request comes from t1's agent (pid {client}, which descends from pid {}, \
             which carries TWR_AGENT",
            cli.id()
        )),
        "{}",
        resps[1]
    );
    lease
        .call("attach.end", json!({"thread": "t1"}))
        .await
        .unwrap();
    th.stop().await;
}

/// Not a test of its own: a stand-in for another workspace's hub, this test binary started
/// again as `twr hub fake_hub_parent --exact` (so its arguments read as a hub's, its program
/// as `twr`) with `TWR_TEST_FAKE_HUB=<shell command>`: runs that command as its child, without
/// the agent marker, and waits for it.
#[test]
fn fake_hub_parent() {
    let Some(cmd) = std::env::var_os("TWR_TEST_FAKE_HUB") else {
        return;
    };
    let status = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .env_remove("TWR_AGENT")
        .status()
        .unwrap();
    assert!(status.success());
}

/// A process under another hub (another workspace's), which this hub never started and
/// which carries no marker, is an agent here: it can read, not act.
#[tokio::test]
async fn a_process_under_another_hub_is_an_agent() {
    use std::os::unix::process::CommandExt;
    let th = start(None, tempfile::tempdir().unwrap()).await;
    let client = AgentClient::new(th.dir.path(), "under-hub", &th.socket);
    let mut parent = std::process::Command::new(std::env::current_exe().unwrap())
        .arg0("twr")
        .args(["hub", "fake_hub_parent", "--exact", "--test-threads=1"])
        .env("TWR_TEST_FAKE_HUB", client.command())
        .env_remove("TWR_AGENT")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let project = th.project.display().to_string();
    let resps = client
        .run(&[
            json!({"id": 1, "method": "hello", "params": {"client": "shell"}}),
            json!({"id": 2, "method": "thread.list"}),
            json!({"id": 3, "method": "thread.new", "params": {"project": project, "runtime": "fake", "name": "t1"}}),
        ])
        .await;
    // An agent of no thread of this hub: `unknown`, or, when the tests run inside a twr
    // thread or a CLI attached to one, the outer thread whose marker (`<thread>:<...>`) its
    // ancestry carries.
    let identity = resps[0]["result"]["identity"].as_str().unwrap();
    let outer = std::env::var("TWR_AGENT")
        .ok()
        .and_then(|m| m.split_once(':').map(|(t, _)| format!("agent:{t}")));
    assert!(
        identity == "agent:unknown" || outer.as_deref() == Some(identity),
        "{resps:?}"
    );
    assert!(resps[1]["result"].is_array(), "{resps:?}");
    assert_eq!(resps[2]["error"]["code"], codes::UNAUTHORIZED, "{resps:?}");
    let msg = resps[2]["error"]["message"].as_str().unwrap();
    let why = if identity == "agent:unknown" {
        "descends from a twr hub"
    } else {
        "carries TWR_AGENT"
    };
    assert!(msg.contains(why), "{msg}");
    assert!(parent.wait().unwrap().success());
    th.stop().await;
}

/// `job.start` request `id` for a job that runs `true`, waking nothing, with `extra` params.
fn job_start(id: u64, extra: Value) -> Value {
    let mut params = json!({"argv": ["true"], "wake": false});
    for (k, v) in extra.as_object().unwrap() {
        params[k] = v.clone();
    }
    json!({"id": id, "method": "job.start", "params": params})
}

/// Who asked for each job of `thread` (`ext.twr.job.by` of its first task update), in order.
fn jobs_by(hub: &Hub, thread: &str) -> Vec<String> {
    let (_, evs) = hub.thread_events(thread, 0, 10_000).unwrap();
    let mut seen = Vec::new();
    let mut by = Vec::new();
    for e in &evs {
        let EventBody::TaskUpdated(p) = &e.body else {
            continue;
        };
        let Some(job) = p.ext.as_ref().and_then(|x| x.get("twr")?.get("job")) else {
            continue;
        };
        if p.running && !seen.contains(&job["id"]) {
            seen.push(job["id"].clone());
            by.push(job["by"].as_str().unwrap_or_default().to_string());
        }
    }
    by
}

/// A `pty` worker's shell starts jobs for its own thread while the thread's mode may run
/// commands (`auto`, `manual`), and is refused (`relay_forbidden`) in a mode that only plans or
/// reads (`plan`, `dontAsk`, `read-only`), in one twr cannot classify (an agent's own id, which
/// a `pty` thread keeps as given; `twr_adapters::mode_level_for` classifies an ACP agent's the
/// same way), and for another thread. Its jobs are recorded as `agent:<thread id>`, the user's
/// as `user:<client>`. A job's own process (it carries the thread's marker, but descends from
/// the hub, not the runtime) starts none. Every method outside `AGENT_METHODS` is refused.
#[tokio::test]
async fn an_agent_starts_its_own_threads_jobs_from_a_mode_that_runs_commands() {
    let th = start(None, tempfile::tempdir().unwrap()).await;
    let mut c = th.client("itest").await;
    let agent = AgentClient::new(th.dir.path(), "w1", &th.socket);
    let runtime = format!("pty:sh {}", agent.script().display());
    let mut control = Client::subscribe_at(&th.socket, "control.subscribe", json!({"since": 0}))
        .await
        .unwrap();
    for (name, runtime) in [("w1", runtime.as_str()), ("t2", "fake")] {
        c.call(
            "thread.new",
            json!({"project": th.project, "runtime": runtime, "name": name, "mode": "auto"}),
        )
        .await
        .unwrap();
    }
    let w1 = th.hub.resolve("w1").unwrap();
    c.call("thread.send", json!({"thread": "w1", "prompt": "hello"}))
        .await
        .unwrap();
    let started = until(&mut control, |i| {
        i.event.kind() == "thread.started" && i.event.thread == w1
    })
    .await;
    let pid = runtime_pid(&started);

    // In `auto` (the thread's mode), then in each of these, set by the user in between.
    let modes = ["manual", "plan", "dontAsk", "read-only", "architect"];
    let mut requests = vec![
        json!({"id": 1, "method": "hello", "params": {"client": "shell"}}),
        job_start(2, json!({})),
    ];
    for (i, _) in modes.iter().enumerate() {
        requests.push(json!({"mark": format!("at-{i}")}));
        requests.push(json!({"wait": format!("set-{i}")}));
        requests.push(job_start(10 + i as u64, json!({})));
    }
    requests.push(job_start(20, json!({"thread": "t2"})));
    let outside: Vec<&str> = (server::METHODS.iter())
        .filter(|m| !server::AGENT_METHODS.contains(m))
        .copied()
        .collect();
    assert!(outside.contains(&"thread.send") && outside.contains(&"attach.begin"));
    for (i, m) in outside.iter().enumerate() {
        requests.push(json!({"id": 100 + i, "method": m}));
    }
    agent.start(&requests);
    for (i, mode) in modes.iter().enumerate() {
        agent.reached(&format!("at-{i}")).await;
        c.call("thread.set_mode", json!({"thread": "w1", "mode": mode}))
            .await
            .unwrap();
        agent.go_on(&format!("set-{i}"));
    }
    let resps = agent.responses().await;
    assert_eq!(resps.len(), 8 + outside.len(), "{resps:?}");
    assert_eq!(resps[0]["result"]["identity"], "agent:w1", "{}", resps[0]);
    for resp in &resps[1..3] {
        assert_eq!(resp["result"]["thread"], w1.as_str(), "{resp}");
    }
    let refused = |resp: &Value| -> String {
        assert_eq!(resp["error"]["code"], codes::RELAY_FORBIDDEN, "{resp}");
        resp["error"]["message"].as_str().unwrap().to_string()
    };
    for (resp, mode) in resps[3..6].iter().zip(["plan", "dontAsk", "read-only"]) {
        assert_eq!(
            refused(resp),
            format!(
                "w1 is in {mode} mode: jobs need a mode that may run commands (manual or wider)"
            )
        );
    }
    let m = refused(&resps[6]);
    assert!(
        m.starts_with("w1's mode \"architect\" is its agent's own, and twr does not know"),
        "{m}"
    );
    let m = refused(&resps[7]);
    assert!(m.contains("for its own thread only"), "{m}");
    for (resp, m) in resps[8..].iter().zip(&outside) {
        assert!(resp.get("error").is_some(), "{m}: {resp}");
        assert_refused(resp, "w1", pid);
    }

    // A job's own process is the thread's agent by its marker alone: it starts no job, also
    // in a mode that may run commands.
    c.call("thread.set_mode", json!({"thread": "w1", "mode": "auto"}))
        .await
        .unwrap();
    let job_proc = AgentClient::new(th.dir.path(), "job", &th.socket);
    job_proc.start(&[
        json!({"id": 1, "method": "hello", "params": {"client": "job"}}),
        job_start(2, json!({})),
        job_start(3, json!({"thread": "w1"})),
    ]);
    c.call(
        "job.start",
        json!({"thread": "w1", "argv": ["sh", "-c", job_proc.command()], "wake": false}),
    )
    .await
    .unwrap();
    let resps = job_proc.responses().await;
    assert_eq!(resps[0]["result"]["identity"], "agent:w1", "{}", resps[0]);
    for resp in &resps[1..] {
        let m = refused(resp);
        assert!(
            m.contains("jobs are started and cancelled by a thread's runtime or attached CLI"),
            "{m}"
        );
    }
    let agent_by = format!("agent:{w1}");
    assert_eq!(
        jobs_by(&th.hub, &w1),
        [agent_by.as_str(), &agent_by, "user:itest"]
    );
    th.stop().await;
}

/// An agent's connection opened while its thread ran, from a process that outlives the
/// runtime (in a session of its own, without the markers a stop finds processes by), starts
/// no job once the thread is parked (its runtime is gone, so the process no longer runs under
/// it: `relay_forbidden`), though it still reads.
#[tokio::test]
async fn an_agent_starts_no_job_for_its_parked_thread() {
    let th = start(None, tempfile::tempdir().unwrap()).await;
    let mut c = th.client("itest").await;
    let agent = AgentClient::new(th.dir.path(), "w1", &th.socket);
    let run = format!(
        "env -u TWR_THREAD -u TWR_RUNTIME_GEN TWR_TEST_SETSID=1 {} < /dev/null &",
        agent.command()
    );
    let runtime = format!("pty:sh {}", agent.script_running(&run).display());
    let mut control = Client::subscribe_at(&th.socket, "control.subscribe", json!({"since": 0}))
        .await
        .unwrap();
    c.call(
        "thread.new",
        json!({"project": th.project, "runtime": runtime, "name": "w1"}),
    )
    .await
    .unwrap();
    let w1 = th.hub.resolve("w1").unwrap();
    c.call("thread.send", json!({"thread": "w1", "prompt": "hello"}))
        .await
        .unwrap();
    until(&mut control, |i| {
        i.event.kind() == "thread.started" && i.event.thread == w1
    })
    .await;
    agent.start(&[
        json!({"id": 1, "method": "hello", "params": {"client": "shell"}}),
        job_start(2, json!({})),
        json!({"mark": "ran"}),
        json!({"wait": "parked"}),
        job_start(3, json!({})),
        json!({"id": 4, "method": "job.list"}),
    ]);
    agent.reached("ran").await;
    c.call("thread.park", json!({"thread": "w1", "force": true}))
        .await
        .unwrap();
    until(&mut control, |i| {
        i.event.kind() == "thread.parked" && i.event.thread == w1
    })
    .await;
    agent.go_on("parked");
    let resps = agent.responses().await;
    assert_eq!(resps[0]["result"]["identity"], "agent:w1", "{}", resps[0]);
    assert_eq!(resps[1]["result"]["thread"], w1.as_str(), "{}", resps[1]);
    assert_eq!(
        resps[2]["error"]["code"],
        codes::RELAY_FORBIDDEN,
        "{}",
        resps[2]
    );
    let m = resps[2]["error"]["message"].as_str().unwrap();
    assert!(
        m.contains("no longer runs under the runtime or attached CLI twr started for w1"),
        "{m}"
    );
    assert!(resps[3]["result"].is_array(), "{}", resps[3]);
    th.stop().await;
}

/// A runtime's process that opened its connection under the runtime, and then left it (its
/// parent exited: reparented, its ancestry no longer shows the runtime, which still runs),
/// keeps reading on that connection but starts and cancels no job there any more: its
/// ancestry is walked again at each `job.start` and `job.cancel`. A new connection from it
/// starts none either (its marker alone).
#[tokio::test]
async fn a_process_that_left_its_runtime_starts_no_job_on_its_old_connection() {
    let th = start(None, tempfile::tempdir().unwrap()).await;
    let mut c = th.client("itest").await;
    let agent = AgentClient::new(th.dir.path(), "w1", &th.socket);
    let leave = agent.dir.join("leave");
    // A shell of the runtime's runs the client in the background and exits when told to.
    let run = format!(
        "sh -c '{} & while [ ! -e {} ]; do sleep 0.05; done' &",
        agent.command(),
        leave.display()
    );
    let runtime = format!("pty:sh {}", agent.script_running(&run).display());
    let mut control = Client::subscribe_at(&th.socket, "control.subscribe", json!({"since": 0}))
        .await
        .unwrap();
    c.call(
        "thread.new",
        json!({"project": th.project, "runtime": runtime, "name": "w1", "mode": "auto"}),
    )
    .await
    .unwrap();
    let w1 = th.hub.resolve("w1").unwrap();
    c.call("thread.send", json!({"thread": "w1", "prompt": "hello"}))
        .await
        .unwrap();
    let started = until(&mut control, |i| {
        i.event.kind() == "thread.started" && i.event.thread == w1
    })
    .await;
    let pid = runtime_pid(&started);
    agent.start(&[
        json!({"id": 1, "method": "hello", "params": {"client": "shell"}}),
        job_start(2, json!({"argv": ["sleep", "30"]})),
        json!({"mark": "opened"}),
        json!({"wait": "left"}),
        job_start(3, json!({})),
        json!({"id": 4, "method": "job.cancel", "params": {"job": "J0NONE0000"}}),
        json!({"id": 5, "method": "thread.list"}),
        json!({"reconnect": true}),
        json!({"id": 6, "method": "hello", "params": {"client": "shell"}}),
        job_start(7, json!({})),
    ]);
    agent.reached("opened").await;
    std::fs::write(&leave, "").unwrap();
    agent.orphaned_from(pid).await;
    agent.go_on("left");
    let resps = agent.responses().await;
    assert_eq!(resps[0]["result"]["identity"], "agent:w1", "{}", resps[0]);
    assert_eq!(resps[1]["result"]["thread"], w1.as_str(), "{}", resps[1]);
    for resp in &resps[2..4] {
        assert_eq!(resp["error"]["code"], codes::RELAY_FORBIDDEN, "{resp}");
        let m = resp["error"]["message"].as_str().unwrap();
        assert!(
            m.contains("no longer runs under the runtime or attached CLI twr started for w1"),
            "{m}"
        );
    }
    assert!(resps[4]["result"].is_array(), "{}", resps[4]);
    assert_eq!(resps[5]["result"]["identity"], "agent:w1", "{}", resps[5]);
    assert_eq!(
        resps[6]["error"]["code"],
        codes::RELAY_FORBIDDEN,
        "{}",
        resps[6]
    );
    let agent_by = format!("agent:{w1}");
    assert_eq!(jobs_by(&th.hub, &w1), [agent_by.as_str()]);
    th.stop().await;
}

/// A runtime's process whose parent (a shell of the runtime's) exits while a job request of it
/// is checked, after its ancestry was walked again and before that walk is confirmed (a test
/// hook makes it exit there, and stay a zombie of the runtime's): the parent still reads as
/// the runtime's child, but the process no longer descends from it, and its `job.start` is
/// refused.
#[tokio::test]
async fn a_process_that_leaves_its_runtime_while_checked_starts_no_job() {
    let dir = tempfile::tempdir().unwrap();
    let leave = dir.path().join("w1").join("leave");
    let fired = Arc::new(AtomicBool::new(false));
    let (l, f) = (leave.clone(), Arc::clone(&fired));
    let th = start_with(None, dir, move |o| {
        o.peer_confirm_hook = Some(Arc::new(move |peer| {
            if f.swap(true, Ordering::SeqCst) {
                return;
            }
            let parent = |p| twr_runner::reap::ancestry(p).chain.get(1).map(|a| a.pid);
            let before = parent(peer);
            std::fs::write(&l, "").unwrap();
            let deadline = Instant::now() + WAIT;
            while parent(peer) == before && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
        }));
    })
    .await;
    let mut c = th.client("itest").await;
    let agent = AgentClient::new(th.dir.path(), "w1", &th.socket);
    let run = format!(
        "sh -c '{} & while [ ! -e {} ]; do sleep 0.05; done' &",
        agent.command(),
        leave.display()
    );
    let runtime = format!("pty:sh {}", agent.script_running(&run).display());
    let mut control = Client::subscribe_at(&th.socket, "control.subscribe", json!({"since": 0}))
        .await
        .unwrap();
    c.call(
        "thread.new",
        json!({"project": th.project, "runtime": runtime, "name": "w1", "mode": "auto"}),
    )
    .await
    .unwrap();
    let w1 = th.hub.resolve("w1").unwrap();
    c.call("thread.send", json!({"thread": "w1", "prompt": "hello"}))
        .await
        .unwrap();
    until(&mut control, |i| {
        i.event.kind() == "thread.started" && i.event.thread == w1
    })
    .await;
    let resps = agent
        .run(&[
            json!({"id": 1, "method": "hello", "params": {"client": "shell"}}),
            job_start(2, json!({})),
            json!({"id": 3, "method": "thread.list"}),
        ])
        .await;
    assert!(fired.load(Ordering::SeqCst), "the hook ran");
    assert_eq!(resps[0]["result"]["identity"], "agent:w1", "{}", resps[0]);
    assert_eq!(
        resps[1]["error"]["code"],
        codes::RELAY_FORBIDDEN,
        "{}",
        resps[1]
    );
    let m = resps[1]["error"]["message"].as_str().unwrap();
    assert!(m.contains("no longer runs under the runtime"), "{m}");
    assert!(resps[2]["result"].is_array(), "{}", resps[2]);
    assert!(jobs_by(&th.hub, &w1).is_empty());
    th.stop().await;
}
