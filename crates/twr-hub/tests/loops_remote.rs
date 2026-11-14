//! Loops on a remote box (docs/design/loops.md, "Milestone 4: implementation notes › Remote
//! boxes"): a thread on a runner's box runs a review loop and an implementation loop as on the
//! hub's own machine. The runner here is in-process, behind a WebSocket proxy that can take
//! the box away ([`Proxy`]) or hide the runner's loop support ([`Proxy::strip_loops`], an older
//! runner). The thread's runtime is a scripted fake ([`Brain`]) on the box; the reviewers are
//! fake CLIs installed ON THE BOX only (the box's `TWR_*_BIN`): the hub's own machine has none
//! (its probe finds nothing, its reviewer programs do not exist), so every round that runs
//! proves the box's inventory and the box's reviewer are what count.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::{oneshot, watch};
use tokio::time::timeout;
use tokio_tungstenite::tungstenite::Message;
use twr_adapters::fake::FakeAdapter;
use twr_core::adapter::{Adapter, Answer};
use twr_core::config::{BoxConfig, Config};
use twr_core::event::*;
use twr_core::loops::config::{ImplCommit, ReviewerChoice, SweepMode};
use twr_core::loops::settings::LoopSettings;
use twr_core::loops::Outcome;
use twr_core::runner::{AgentInfo, Secret};
use twr_core::testkit::git as tg;
use twr_hub::hub::{
    AgentProbe, LoopGet, LoopRequest, LoopSettingsFor, LoopSettingsSet, LoopSetupUpdate,
};
use twr_hub::listener::{RunnerListener, RunnerServer};
use twr_hub::{Caller, Hub, HubOptions, NewThread};
use twr_runner::{BoxEnv, LocalBox, LocalBoxOptions, RunnerOptions};

const BOX: &str = "gpu";
const TOKEN: &str = "loops-remote-token-0123456789abcdef";
const WAIT: Duration = Duration::from_secs(40);
const START: &str = "start and save these settings";
const TASK: &str = "add a CSV export";

// ---------------------------------------------------------------------------------------------
// The box's reviewers, and the scripted thread
// ---------------------------------------------------------------------------------------------

/// A fake reviewer CLI on the box (`codex`, `claude` or `grok`, by the name it runs as). The
/// box's agent probe gets a version and a signed-in status; every other call is a review run
/// `n` (from 1, `count`), which writes its argv, environment, stdin and working directory to
/// `<kind>.<n>`; with `hold.<n>` it starts a child (`child.<n>`, itself in `pid.<n>`) and
/// waits until the file goes. It answers `answers/<n>.json`, else no findings.
const FAKE_REVIEWER: &str = r#"#!/bin/sh
here=$(dirname "$0")
case "$1" in --version) echo "fake 1.0"; exit 0;; esac
if [ "$1 $2" = "login status" ]; then echo "Logged in"; exit 0; fi
if [ "$1 $2" = "auth status" ]; then echo '{"loggedIn": true, "authMethod": "claude.ai"}'; exit 0; fi
until mkdir "$here/count.lock" 2>/dev/null; do sleep 0.01; done
n=$(( $(cat "$here/count" 2>/dev/null || echo 0) + 1 ))
echo "$n" > "$here/count"
rmdir "$here/count.lock"
: > "$here/argv.$n"
for a in "$@"; do printf '%s\n' "$a" >> "$here/argv.$n"; done
env > "$here/env.$n"
pwd > "$here/cwd.$n"
cat > "$here/stdin.$n" 2>/dev/null
echo "fake reviewer run $n" >&2
if [ -f "$here/hold.$n" ]; then
  sleep 600 &
  echo $! > "$here/child.$n"
  echo $$ > "$here/pid.$n"
  while [ -f "$here/hold.$n" ]; do sleep 0.05; done
  kill $! 2>/dev/null
fi
ans="$here/answers/$n.json"
[ -f "$ans" ] || { ans="$here/answers/none.json"; echo '{"findings": []}' > "$ans"; }
out=""; prev=""
for a in "$@"; do [ "$prev" = "-o" ] && out="$a"; prev="$a"; done
case "$(basename "$0")" in
  codex)
    cp "$ans" "$out"
    echo '{"type":"turn.completed","usage":{"input_tokens":100,"output_tokens":10}}' ;;
  claude)
    printf '{"type":"result","is_error":false,"result":"done","structured_output":%s,"usage":{"input_tokens":5,"output_tokens":7},"total_cost_usd":0.01}\n' "$(cat "$ans")" ;;
esac
"#;

/// What the scripted thread answers: the first of `replies` whose key its prompt's first line
/// contains (taken once), else, with `auto`, a valid answer to every loop step, editing and
/// committing in `worktree` as a thread would. Every prompt it saw is kept in `seen`.
#[derive(Default)]
struct Brain {
    replies: Vec<(String, String)>,
    auto: bool,
    worktree: Option<PathBuf>,
    /// The rules a `sweep = "runs"` loop names.
    rules: Vec<String>,
    seen: Vec<String>,
    /// The loop folder's files a prompt named that were not on the box when it came.
    missing: Vec<String>,
}

impl Brain {
    fn reply(&mut self, prompt: &str) -> Option<String> {
        self.seen.push(prompt.to_string());
        // What the step reads must be in the box's folder before its prompt comes (the plan
        // step's names `plan.md` as where its plan goes).
        let step = prompt.lines().next().unwrap_or_default().to_string();
        for word in prompt.split_whitespace() {
            let path = word
                .trim_start_matches(['(', '`'])
                .trim_end_matches(['.', ',', ')', ':', ';', '`']);
            if path.contains("/loops/")
                && ["/plan.md", "/ledger.md", "/goal.md", "/task.md"]
                    .iter()
                    .any(|f| path.ends_with(f))
                && !Path::new(path).is_file()
                && !(step.ends_with(" plan]") && path.ends_with("/plan.md"))
            {
                self.missing.push(format!("{step} {path}"));
            }
        }
        let first = prompt.lines().next().unwrap_or_default().to_string();
        if let Some(i) = self
            .replies
            .iter()
            .position(|(k, _)| first.contains(k.as_str()))
        {
            let r = self.replies.remove(i).1;
            if let Some(rest) = r.strip_prefix("@slow ") {
                let auto = self.auto_reply(prompt);
                return Some(format!("!sleep {rest}\n{}", auto.unwrap_or_default()));
            }
            return (!r.is_empty()).then_some(r);
        }
        self.auto_reply(prompt)
    }

    fn auto_reply(&mut self, prompt: &str) -> Option<String> {
        let first = prompt.lines().next().unwrap_or_default().to_string();
        if !first.starts_with("[twr loop ") || !self.auto {
            return None;
        }
        if first.ends_with(" scope]") {
            return Some(
                "Gates are green.\n```twr-goal\nHarden the change against bad input.\n```".into(),
            );
        }
        if first.ends_with(" plan]") {
            return Some(format!(
                "## Goal\nExport reports as CSV.\n\n## Steps\n1. Add `export_csv`.\n\n\
                 ```twr-plan\n{}\n```",
                json!({"contract_risk": false, "reason": "UI only"})
            ));
        }
        if first.ends_with(" implement]") {
            self.write("feature.rs", "pub fn export_csv() {}\n");
            return Some(format!(
                "Implemented.\n```twr-implement\n{}\n```",
                json!({"status": "done", "gates": {"ran": ["cargo test"], "result": "pass"}})
            ));
        }
        if first.contains(" triage ") && !first.contains("correction") {
            let round = first
                .split(" triage ")
                .nth(1)
                .unwrap_or("x")
                .trim_end_matches(']')
                .to_string();
            self.write(&format!("fix-{round}.rs"), "// fixed\n");
            let rule = self.rules.first().cloned().unwrap_or_else(|| "r1".into());
            let items: Vec<Value> = finding_ids(prompt)
                .iter()
                .map(|id| json!({"id": id, "verdict": "fixed", "class": "new-class", "rule": rule}))
                .collect();
            return Some(format!(
                "Triaged.\n```twr-triage\n{}\n```",
                json!({"items": items, "gates": {"ran": ["cargo test"], "result": "pass"}})
            ));
        }
        if first.ends_with(" rules]") {
            let rules: Vec<Value> = self
                .rules
                .iter()
                .map(|r| json!({"name": r, "rule": format!("{r} holds everywhere."), "where": "src/**"}))
                .collect();
            return Some(format!(
                "Named.\n```twr-rules\n{}\n```",
                json!({ "rules": rules })
            ));
        }
        if first.ends_with(" sweep]") {
            let rules: Vec<Value> = if self.rules.is_empty() {
                vec![json!({"rule": "r1", "result": "fixed"})]
            } else {
                self.rules
                    .iter()
                    .map(|r| json!({"rule": r, "result": "checked clean"}))
                    .collect()
            };
            return Some(format!(
                "Swept.\n```twr-sweep\n{}\n```",
                json!({"rules": rules, "gates": {"ran": ["cargo test"], "result": "pass"}})
            ));
        }
        if first.ends_with(" report]") {
            let mut out = String::from("Built and reviewed; nothing is left open.\n");
            if prompt.contains("```twr-commit") {
                out.push_str(&self.commit());
            }
            return Some(out);
        }
        if first.ends_with(" commit correction]") {
            return Some(format!("Committed the rest.\n{}", self.commit()));
        }
        None
    }

    fn write(&self, name: &str, text: &str) {
        if let Some(wt) = &self.worktree {
            std::fs::write(wt.join(name), text).unwrap();
        }
    }

    /// Commits everything in the worktree: the `twr-commit` block of what it did.
    fn commit(&mut self) -> String {
        let Some(wt) = self.worktree.clone() else {
            return String::new();
        };
        tg::run(&wt, &["add", "-A"]);
        let staged = std::process::Command::new("git")
            .arg("-C")
            .arg(&wt)
            .args(["diff", "--cached", "--quiet"])
            .status()
            .unwrap();
        if !staged.success() {
            tg::run(&wt, &["commit", "-q", "-m", "Add the CSV export"]);
        }
        let sha = tg::run(&wt, &["rev-parse", "HEAD"]);
        format!(
            "```twr-commit\n{}\n```",
            json!({"commits": [{"repo": wt.display().to_string(), "sha": sha}]})
        )
    }
}

/// The finding ids a triage prompt wraps (`finding m1.2 (major):` lines, the hub's).
fn finding_ids(prompt: &str) -> Vec<String> {
    prompt
        .lines()
        .filter_map(|l| l.strip_prefix("finding "))
        .filter_map(|l| l.split_once(' ').map(|(id, _)| id.to_string()))
        .collect()
}

/// `n` findings of severity major.
fn findings(n: usize) -> Value {
    Value::Array(
        (1..=n)
            .map(|i| {
                json!({"severity": "major", "location": format!("src/lib.rs:{i}"),
                       "problem": format!("problem {i}"), "fix": format!("fix {i}")})
            })
            .collect(),
    )
}

/// The fake runtime on the box, its prompts rewritten by a [`Brain`] first.
struct Scripted {
    brain: Arc<Mutex<Brain>>,
}

impl Adapter for Scripted {
    fn info(&self) -> twr_core::adapter::AdapterInfo {
        FakeAdapter::new().info()
    }

    fn spawn(
        &self,
        cfg: twr_core::adapter::SpawnConfig,
        prompt: Option<String>,
    ) -> anyhow::Result<twr_core::adapter::AdapterHandle> {
        use twr_core::adapter::{AdapterCmd, AdapterHandle};
        let brain = self.brain.clone();
        let rewrite = move |text: String| -> String {
            let reply = brain.lock().unwrap().reply(&text);
            reply.unwrap_or(text)
        };
        let mut inner = FakeAdapter::new().spawn(cfg, prompt.map(&rewrite))?;
        let (outer, end) = AdapterHandle::pair();
        let to_inner = inner.commander();
        let mut from_inner = inner.take_events().expect("events");
        let twr_core::adapter::AdapterEnd {
            mut cmds, events, ..
        } = end;
        tokio::spawn(async move {
            while let Some(cmd) = cmds.recv().await {
                let cmd = match cmd {
                    AdapterCmd::Send { text, turn } => AdapterCmd::Send {
                        text: rewrite(text),
                        turn,
                    },
                    c => c,
                };
                if to_inner.send(cmd).is_err() {
                    break;
                }
            }
        });
        tokio::spawn(async move {
            let _inner = inner;
            while let Some(ev) = from_inner.recv().await {
                if events.send(ev).is_err() {
                    break;
                }
            }
        });
        Ok(outer)
    }
}

// ---------------------------------------------------------------------------------------------
// A proxy between the runner and the hub that can take the box away
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct ProxyState {
    upstream: String,
    /// The box is away: connections are cut and refused.
    down: bool,
    /// The runner's inventory loses its `loops` (a runner from before them).
    strip_loops: bool,
    /// Cuts the connections going now.
    cuts: Vec<watch::Sender<bool>>,
}

#[derive(Clone)]
struct Proxy {
    url: String,
    state: Arc<Mutex<ProxyState>>,
}

impl Proxy {
    async fn start() -> Proxy {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let p = Proxy {
            url: format!("ws://{}", l.local_addr().unwrap()),
            state: Arc::new(Mutex::new(ProxyState::default())),
        };
        let pp = p.clone();
        tokio::spawn(async move {
            while let Ok((tcp, _)) = l.accept().await {
                let pp = pp.clone();
                tokio::spawn(async move { pp.relay(tcp).await });
            }
        });
        p
    }

    fn st(&self) -> std::sync::MutexGuard<'_, ProxyState> {
        self.state.lock().unwrap()
    }

    /// Takes the box away (its connection cut, new ones refused), or brings it back.
    fn set_down(&self, down: bool) {
        let mut s = self.st();
        s.down = down;
        if down {
            for c in s.cuts.drain(..) {
                let _ = c.send(true);
            }
        }
    }

    async fn relay(&self, tcp: tokio::net::TcpStream) {
        let (upstream, mut cut) = {
            let mut s = self.st();
            if s.down {
                return;
            }
            let (tx, rx) = watch::channel(false);
            s.cuts.push(tx);
            (s.upstream.clone(), rx)
        };
        let Ok(down) = tokio_tungstenite::accept_async(tcp).await else {
            return;
        };
        let Ok((up, _)) = tokio_tungstenite::connect_async(upstream.as_str()).await else {
            return;
        };
        let (mut dsink, mut dstream) = down.split();
        let (mut usink, mut ustream) = up.split();
        loop {
            let m = tokio::select! {
                _ = cut.changed() => return,
                m = dstream.next() => Ok(m),
                m = ustream.next() => Err(m),
            };
            match m {
                Ok(m) => {
                    let Some(Ok(mut m)) = m else { return };
                    if let Message::Text(t) = &m {
                        if self.st().strip_loops {
                            if let Ok(mut v) = serde_json::from_str::<Value>(t.as_str()) {
                                if v["type"] == "inventory" {
                                    v.as_object_mut().unwrap().remove("loops");
                                    m = Message::Text(v.to_string().into());
                                }
                            }
                        }
                    }
                    if usink.send(m).await.is_err() {
                        return;
                    }
                }
                Err(m) => {
                    let Some(Ok(m)) = m else { return };
                    if dsink.send(m).await.is_err() {
                        return;
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The hub (in a runtime of its own, so a test can kill it) and the runner
// ---------------------------------------------------------------------------------------------

struct TestHub {
    rt: Option<tokio::runtime::Runtime>,
    hub: Option<Hub>,
    server: Option<RunnerServer>,
}

impl TestHub {
    fn hub(&self) -> &Hub {
        self.hub.as_ref().unwrap()
    }

    /// Runs the future `f` makes in the hub's runtime.
    async fn run<T, F>(&self, f: impl FnOnce(Hub) -> F) -> T
    where
        F: std::future::Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let fut = f(self.hub().clone());
        self.rt.as_ref().unwrap().spawn(fut).await.unwrap()
    }

    /// Runs `f` in the hub's runtime context.
    fn sync<T>(&self, f: impl FnOnce(&Hub) -> T) -> T {
        let _g = self.rt.as_ref().unwrap().enter();
        f(self.hub())
    }

    async fn shutdown(mut self) {
        let hub = self.hub.take().unwrap();
        self.rt
            .as_ref()
            .unwrap()
            .spawn(async move { hub.shutdown().await })
            .await
            .unwrap();
        self.server.take();
        self.rt.take().unwrap().shutdown_background();
    }

    /// The hub dies: every task of it gone at once, nothing graceful.
    fn crash(mut self) {
        self.server.take();
        self.hub.take();
        self.rt.take().unwrap().shutdown_background();
    }
}

impl Drop for TestHub {
    fn drop(&mut self) {
        self.server.take();
        self.hub.take();
        if let Some(rt) = self.rt.take() {
            rt.shutdown_background();
        }
    }
}

struct Runner {
    stop: Option<oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl Runner {
    async fn shutdown(mut self) {
        let _ = self.stop.take().unwrap().send(());
        let _ = tokio::time::timeout(Duration::from_secs(10), &mut self.task).await;
    }
}

/// A change to the hub's options before it starts.
type Tweak = Arc<dyn Fn(&mut HubOptions) + Send + Sync>;

struct Env {
    dir: tempfile::TempDir,
    brain: Arc<Mutex<Brain>>,
    proxy: Proxy,
    /// The reviewers the box has (`TWR_*_BIN` of the others name nothing).
    on_box: Vec<&'static str>,
    /// Hub options to change before it starts.
    tweak: Option<Tweak>,
}

impl Env {
    async fn new() -> Env {
        let env = Env {
            dir: tempfile::tempdir().unwrap(),
            brain: Arc::new(Mutex::new(Brain {
                auto: true,
                ..Default::default()
            })),
            proxy: Proxy::start().await,
            on_box: vec!["codex", "claude-code"],
            tweak: None,
        };
        let dir = env.reviewers();
        std::fs::create_dir_all(dir.join("answers")).unwrap();
        for name in ["codex", "claude"] {
            let bin = dir.join(name);
            std::fs::write(&bin, FAKE_REVIEWER).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        env
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// The box's fake reviewer CLIs.
    fn reviewers(&self) -> PathBuf {
        self.path().join("box-reviewers")
    }

    /// The runner's data directory.
    fn rdir(&self) -> PathBuf {
        self.path().join("runner")
    }

    /// The `n`th review run's answer (from 1).
    fn answer(&self, n: usize, findings: Value) {
        std::fs::write(
            self.reviewers().join("answers").join(format!("{n}.json")),
            json!({ "findings": findings }).to_string(),
        )
        .unwrap();
    }

    fn hold_run(&self, n: usize) {
        std::fs::write(self.reviewers().join(format!("hold.{n}")), "").unwrap();
    }

    fn release_run(&self, n: usize) {
        let _ = std::fs::remove_file(self.reviewers().join(format!("hold.{n}")));
    }

    /// How many review runs started on the box.
    fn runs(&self) -> usize {
        std::fs::read_to_string(self.reviewers().join("count"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    fn run_file(&self, n: usize, kind: &str) -> String {
        std::fs::read_to_string(self.reviewers().join(format!("{kind}.{n}"))).unwrap_or_default()
    }

    /// The pids of held run `n` and its child, once it wrote them.
    async fn held_pids(&self, n: usize) -> (i32, i32) {
        let read = |k: &str| {
            std::fs::read_to_string(self.reviewers().join(format!("{k}.{n}")))
                .ok()
                .and_then(|s| s.trim().parse::<i32>().ok())
        };
        timeout(WAIT, async {
            loop {
                if let (Some(a), Some(b)) = (read("pid"), read("child")) {
                    return (a, b);
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the held run's pids")
    }

    fn set<F: FnOnce(&mut Brain)>(&self, f: F) {
        f(&mut self.brain.lock().unwrap());
    }

    fn seen(&self, key: &str) -> Vec<String> {
        self.brain
            .lock()
            .unwrap()
            .seen
            .iter()
            .filter(|p| p.lines().next().unwrap_or_default().contains(key))
            .cloned()
            .collect()
    }

    /// A git repository `name`, with one commit (on the box: the in-process runner's machine).
    fn repo(&self, name: &str) -> PathBuf {
        twr_core::testkit::git::init_repo(&self.path().join(name))
    }

    fn hub_opts(&self) -> HubOptions {
        let mut c = Config {
            default_runtime: "fake".into(),
            ..Default::default()
        };
        c.hub.listen = Some("127.0.0.1:0".into());
        c.boxes.insert(
            BOX.into(),
            BoxConfig {
                token: Some(TOKEN.into()),
                ..Default::default()
            },
        );
        let mut o = HubOptions::new(self.path().join("hub").join("twr.db"), c);
        // No reviewer on the hub's own machine: the probe finds none, and no program exists.
        o.reviewer_bins = ["codex", "claude-code", "grok-build"]
            .into_iter()
            .map(|rt| (rt.to_string(), PathBuf::from("/nonexistent/hub").join(rt)))
            .collect();
        let probe: AgentProbe = Arc::new(|| {
            let agents: Vec<AgentInfo> = ["claude-code", "codex", "grok-build"]
                .iter()
                .map(|rt| {
                    serde_json::from_value(
                        json!({"runtime": rt, "found": false, "login": "unknown"}),
                    )
                    .unwrap()
                })
                .collect();
            futures::FutureExt::boxed(async move { agents })
        });
        o.agent_probe = Some(probe);
        let native = self.path().join("native");
        o.native_dir = Some(Arc::new(move |rt: &str| Some(native.join(rt))));
        o.stop_timeout = Duration::from_secs(5);
        o.git_tick = Some(Duration::ZERO);
        o.stall_after = Some(Duration::ZERO);
        o.disk_tick = Some(Duration::ZERO);
        o.checkpoint_tick = Some(Duration::ZERO);
        o.config_file = Some(self.path().join("config.toml"));
        if let Some(t) = &self.tweak {
            t(&mut o);
        }
        o
    }

    /// Starts a hub (on the same data each time) and points the proxy at it.
    async fn hub(&self) -> TestHub {
        let opts = self.hub_opts();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (hub, server, url) = rt
            .spawn(async move {
                let l = RunnerListener::bind(&opts.config.hub)
                    .await
                    .unwrap()
                    .unwrap();
                let url = l.url();
                let hub = Hub::open(opts).await.unwrap();
                let server = l.serve(hub.clone());
                (hub, server, url)
            })
            .await
            .unwrap();
        self.proxy.st().upstream = url;
        TestHub {
            rt: Some(rt),
            hub: Some(hub),
            server: Some(server),
        }
    }

    /// Starts the runner of the box, through the proxy.
    fn runner(&self) -> Runner {
        let rdir = self.rdir();
        let mut ro = RunnerOptions::new(&self.proxy.url, Secret::new(TOKEN), BOX, &rdir);
        ro.backoff_min = Duration::from_millis(20);
        ro.backoff_max = Duration::from_millis(200);
        ro.refused_retry = Duration::from_millis(200);
        ro.ping_interval = Duration::from_millis(500);
        ro.idle_timeout = Duration::from_secs(10);
        ro.welcome_timeout = Duration::from_secs(5);
        ro.shutdown_grace = Duration::from_secs(3);
        let mut o = LocalBoxOptions::new(&rdir, BOX);
        let brain = self.brain.clone();
        o.adapters = Arc::new(move |name: &str| -> Option<Arc<dyn Adapter>> {
            match name {
                "fake" => Some(Arc::new(Scripted {
                    brain: brain.clone(),
                })),
                other => twr_adapters::find(other),
            }
        });
        if !o.runtimes.iter().any(|r| r == "fake") {
            o.runtimes.push("fake".into());
        }
        let bins = self.reviewers();
        let var = |rt: &str, var: &str, bin: &str| {
            let v = if self.on_box.contains(&rt) {
                bins.join(bin)
            } else {
                PathBuf::from("/nonexistent/box").join(bin)
            };
            (var.to_string(), v.into_os_string())
        };
        o.env = BoxEnv {
            login_shell: None,
            probe_agents: true,
            bin_vars: Some(vec![
                var("codex", "TWR_CODEX_BIN", "codex"),
                var("claude-code", "TWR_CLAUDE_BIN", "claude"),
                var("grok-build", "TWR_GROK_BIN", "grok"),
            ]),
        };
        let (tx, rx) = oneshot::channel::<()>();
        let task = tokio::spawn(twr_runner::run_with(
            ro,
            LocalBox::open(o).unwrap(),
            async move {
                let _ = rx.await;
            },
        ));
        Runner {
            stop: Some(tx),
            task,
        }
    }
}

fn user() -> Caller {
    Caller::user("test")
}

/// Waits (at most [`WAIT`]) until `f` holds.
async fn until(what: &str, f: impl Fn() -> bool) {
    if timeout(WAIT, async {
        while !f() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_err()
    {
        panic!("waited for {what} in vain");
    }
}

/// Waits until the box is online and has reported its agents.
async fn online(h: &TestHub) {
    let hub = h.hub().clone();
    until("the box online", || {
        hub.boxes().iter().any(|(n, on, _)| n == BOX && *on)
    })
    .await;
}

fn events(h: &TestHub, t: &str) -> Vec<Event> {
    h.hub().thread_events(t, 0, 100_000).unwrap().1
}

fn of_kind(h: &TestHub, t: &str, kind: &str) -> Vec<Event> {
    events(h, t)
        .into_iter()
        .filter(|e| e.kind() == kind)
        .collect()
}

fn findings_events(h: &TestHub, t: &str) -> Vec<LoopFindings> {
    events(h, t)
        .into_iter()
        .filter_map(|e| match e.body {
            EventBody::LoopFindings(f) => Some(f),
            _ => None,
        })
        .collect()
}

fn get(h: &TestHub, t: &str) -> Value {
    h.hub()
        .loop_get(LoopGet {
            thread: Some(t.into()),
            id: None,
        })
        .unwrap()
}

fn running(h: &TestHub, t: &str) -> Option<twr_core::loops::LoopView> {
    h.hub().thread_view(t).ok().and_then(|v| v.r#loop)
}

async fn ended(h: &TestHub, t: &str) -> twr_core::reduce::LastLoop {
    let hub = h.hub().clone();
    let t2 = t.to_string();
    until("the loop's end", move || {
        hub.thread_view(&t2)
            .is_ok_and(|v| v.r#loop.is_none() && v.last_loop.is_some())
    })
    .await;
    h.hub().thread_view(t).unwrap().last_loop.unwrap()
}

/// A worker thread `name` on the box, in a fresh repository of the same name.
async fn box_thread(env: &Env, h: &TestHub, name: &str) -> String {
    let repo = env.repo(name);
    let req = NewThread {
        project: repo.to_string_lossy().into_owned(),
        runtime: Some("fake".into()),
        name: Some(name.into()),
        box_name: Some(BOX.into()),
        ..Default::default()
    };
    let v = h
        .run(|hub| async move { hub.new_thread(req, "test").await })
        .await
        .unwrap();
    let t = v["id"].as_str().unwrap().to_string();
    let wt = PathBuf::from(h.hub().thread_view(&t).unwrap().worktree.unwrap());
    env.set(|b| b.worktree = Some(wt));
    t
}

/// `loop.request` of `kind` (with `task`) for thread `t`: the setup card (the first loop),
/// its settings set to `settings` by the user, then started: the thread's id.
async fn start(h: &TestHub, t: &str, kind: &str, task: Option<&str>, settings: LoopSettings) {
    let mut p = json!({"thread": t, "kind": kind});
    if let Some(task) = task {
        p["task"] = json!(task);
    }
    let req: LoopRequest = serde_json::from_value(p).unwrap();
    let card = h
        .run(|hub| async move { hub.loop_request(req, &user()).await })
        .await
        .unwrap();
    let card = if card["status"] == "needs_setup" {
        let upd = LoopSetupUpdate {
            attn_id: card["attn_id"].as_str().unwrap().into(),
            revision: card["revision"].as_u64().unwrap(),
            settings: Some(settings),
            save_for: None,
            scope: None,
        };
        h.run(|hub| async move { hub.loop_setup_update(upd, &user()).await })
            .await
            .unwrap()
    } else {
        panic!("{card}");
    };
    let problems = card["setup"]["problems"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(problems.is_empty(), "{card}");
    h.sync(|hub| {
        hub.respond_at(
            card["attn_id"].as_str().unwrap(),
            Answer::new(START),
            card["revision"].as_u64(),
            &user(),
        )
    })
    .unwrap();
    let hub = h.hub().clone();
    let t2 = t.to_string();
    until("the loop's start", move || {
        hub.thread_view(&t2).is_ok_and(|v| v.r#loop.is_some())
    })
    .await;
}

/// The first card's settings on `profile` with reviewer `reviewer`.
fn settings(profile: &str, reviewer: &str) -> LoopSettings {
    let mut s = LoopSettings::first_card(&Default::default());
    s.profile = profile.into();
    s.reviewer = ReviewerChoice::parse(reviewer).unwrap();
    s
}

/// Loop `id`'s folder on the box.
fn box_folder(env: &Env, id: &str) -> PathBuf {
    env.rdir().join("loops").join(id)
}

fn loop_id(h: &TestHub, t: &str) -> String {
    get(h, t)["id"].as_str().unwrap().to_string()
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

/// A whole review loop on the box: the scope, a round with findings, the thread's triage, the
/// verification, the report. Every review run is the box's (its reviewer CLI, in the thread's
/// worktree there, its files in the box's copy of the loop folder, which every prompt names);
/// the hub's machine has no reviewer at all. The hub keeps the record: answers, logs, usage
/// and the report are in its own folder, read with the box away.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_review_loop_runs_on_a_box() {
    let env = Env::new().await;
    let h = env.hub().await;
    let runner = env.runner();
    online(&h).await;
    let t = box_thread(&env, &h, "api").await;
    let wt = PathBuf::from(h.hub().thread_view(&t).unwrap().worktree.unwrap());
    std::fs::write(wt.join("lib.rs"), "fn changed() {}\n").unwrap();
    env.answer(1, findings(2));
    start(&h, &t, "review", None, settings("low", "codex/gpt-6.1-sol")).await;
    let id = loop_id(&h, &t);
    let last = ended(&h, &t).await;
    assert_eq!(last.outcome, Outcome::Clean);
    // Two review runs, both on the box: the round and the verification.
    assert_eq!(env.runs(), 2);
    let f = findings_events(&h, &t);
    assert_eq!(f.len(), 2, "{f:?}");
    assert_eq!(f[0].findings.len(), 2);
    assert!(f[1].findings.is_empty());
    assert!(f[0].usage.is_some());
    // The reviewer ran in the thread's worktree on the box, read-only, its files in the box's
    // copy of the folder.
    let folder = box_folder(&env, &id);
    assert_eq!(
        std::fs::canonicalize(env.run_file(1, "cwd").trim()).unwrap(),
        std::fs::canonicalize(&wt).unwrap()
    );
    let argv = env.run_file(1, "argv");
    assert!(argv.contains("read-only"), "{argv}");
    assert!(
        argv.contains(&folder.join("round-m1.answer.json").display().to_string()),
        "{argv}"
    );
    let stdin = env.run_file(1, "stdin");
    assert!(
        stdin.contains(&folder.join("diff-api-m1.patch").display().to_string()),
        "{stdin}"
    );
    assert!(stdin.contains(&folder.join("ledger.md").display().to_string()));
    // Its environment has none of twr's markers but its own.
    let renv = env.run_file(1, "env");
    assert!(renv.contains(&format!("TWR_AGENT={t}:loop-{id}")), "{renv}");
    assert!(!renv.contains("TWR_HUB_TOKEN"), "{renv}");
    // The box's copy holds what the reviewer and the thread read.
    for f in [
        "findings.schema.json",
        "ledger.md",
        "goal.md",
        "diff-api-m1.patch",
    ] {
        assert!(folder.join(f).is_file(), "{f}");
    }
    // The thread's prompts name the box's folder; twr jobs don't run there.
    let triage = env.seen(" triage m1]").pop().unwrap();
    assert!(
        triage.contains(&folder.join("ledger.md").display().to_string()),
        "{triage}"
    );
    assert!(!triage.contains("twr job run"), "{triage}");
    assert!(triage.contains("twr jobs don't run on box gpu"), "{triage}");
    // The hub's own folder is the record: the answers and logs read back, the report.
    let g = get(&h, &t);
    let hub_folder = PathBuf::from(g["folder"].as_str().unwrap());
    assert_ne!(hub_folder, folder);
    let answer = std::fs::read_to_string(hub_folder.join("round-m1.answer.json")).unwrap();
    assert!(answer.contains("problem 1"), "{answer}");
    let log = std::fs::read_to_string(hub_folder.join("round-m1.log")).unwrap();
    assert!(log.contains("fake reviewer run 1"), "{log}");
    assert!(hub_folder.join("diff-api-m1.patch").is_file());
    // The report turn was told the report goes into the box's folder: it is there, the
    // hub's own, for the thread's later turns to read.
    let report = std::fs::read(hub_folder.join("report.md")).unwrap();
    until("report.md in the box's folder", || {
        std::fs::read(folder.join("report.md")).is_ok_and(|r| r == report)
    })
    .await;
    // With the box away, the report and `loop.get` still read.
    runner.shutdown().await;
    let r = h
        .hub()
        .loop_report(LoopGet {
            thread: Some(t.clone()),
            id: None,
        })
        .unwrap();
    assert!(
        r["text"].as_str().unwrap().contains("# Review loop report"),
        "{r}"
    );
    assert_eq!(get(&h, &t)["outcome"], "clean");
    h.shutdown().await;
}

/// A whole implementation loop on the box: plan, implement, a round, verification, the
/// report and the commit, checked in the box's repository. `plan.md` is in the box's folder
/// for the implement turn to read.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_implementation_loop_runs_on_a_box_and_commits_there() {
    let env = Env::new().await;
    let h = env.hub().await;
    let runner = env.runner();
    online(&h).await;
    let t = box_thread(&env, &h, "export").await;
    let wt = PathBuf::from(h.hub().thread_view(&t).unwrap().worktree.unwrap());
    let head = tg::run(&wt, &["rev-parse", "HEAD"]);
    env.answer(1, findings(1));
    let mut s = settings("low", "claude-code/claude-opus-5-5");
    s.commit = ImplCommit::End;
    start(&h, &t, "implement", Some(TASK), s).await;
    let id = loop_id(&h, &t);
    let last = ended(&h, &t).await;
    assert_eq!(last.outcome, Outcome::Clean);
    // Committed on the box, nothing left.
    let now = tg::run(&wt, &["rev-parse", "HEAD"]);
    assert_ne!(now, head);
    assert_eq!(tg::run(&wt, &["status", "--porcelain"]), "");
    assert_eq!(last.commits.len(), 1);
    assert_eq!(last.commits[0].sha, now);
    // The plan is in the box's folder, and the implement prompt names it there.
    let folder = box_folder(&env, &id);
    let plan = std::fs::read_to_string(folder.join("plan.md")).unwrap();
    assert!(
        plan.starts_with("## Goal\nExport reports as CSV."),
        "{plan}"
    );
    let implement = env.seen(" implement]").pop().unwrap();
    assert!(
        implement.contains(&folder.join("plan.md").display().to_string()),
        "{implement}"
    );
    // It was there when the implement turn's prompt came (no review run between the plan and
    // the implement turn: the plan check was skipped), as was everything else a prompt named.
    let missing = env.brain.lock().unwrap().missing.clone();
    assert!(missing.is_empty(), "named but not on the box: {missing:?}");
    // The reviewer (Claude Code, on the box) read the plan by its path there.
    let review = env.run_file(1, "stdin");
    assert!(
        review.contains(&folder.join("plan.md").display().to_string()),
        "{review}"
    );
    // The report is the hub's, in its own folder, and names the commit.
    let report = h
        .hub()
        .loop_report(LoopGet {
            thread: Some(t.clone()),
            id: None,
        })
        .unwrap();
    let text = report["text"].as_str().unwrap();
    assert!(text.contains(&now), "{text}");
    runner.shutdown().await;
    h.shutdown().await;
}

/// `sweep = "runs"` on a box: the audit runs are the box's reviewer's too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sweep_runs_on_a_box() {
    let env = Env::new().await;
    env.set(|b| b.rules = vec!["bounds".into(), "errors".into()]);
    let h = env.hub().await;
    let runner = env.runner();
    online(&h).await;
    let t = box_thread(&env, &h, "api").await;
    let wt = PathBuf::from(h.hub().thread_view(&t).unwrap().worktree.unwrap());
    std::fs::write(wt.join("lib.rs"), "fn changed() {}\n").unwrap();
    env.answer(1, findings(1));
    let mut s = settings("medium", "codex/gpt-6.1-sol");
    s.sweep = SweepMode::Runs;
    start(&h, &t, "review", None, s).await;
    let id = loop_id(&h, &t);
    let last = ended(&h, &t).await;
    assert_eq!(last.outcome, Outcome::Clean);
    let audits: Vec<LoopAudit> = events(&h, &t)
        .into_iter()
        .filter_map(|e| match e.body {
            EventBody::LoopAudit(a) => Some(a),
            _ => None,
        })
        .collect();
    assert_eq!(audits.len(), 2, "{audits:?}");
    assert!(audits.iter().all(|a| !a.failed));
    let folder = box_folder(&env, &id);
    for k in ["s1", "s2"] {
        assert!(folder.join(format!("audit-{k}.prompt.md")).is_file(), "{k}");
        assert!(
            folder.join(format!("audit-{k}.answer.json")).is_file(),
            "{k}"
        );
    }
    let verify = env.seen(" sweep]").pop().unwrap();
    assert!(verify.contains("audit s1"), "{verify}");
    runner.shutdown().await;
    h.shutdown().await;
}

/// An ACP reviewer on a box: the preset's agent (the hub's config, sent as plain data) runs
/// on the box, in its read-only mode, and answers the round there.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_acp_reviewer_runs_on_a_box() {
    let mut env = Env::new().await;
    let state = env.path().join("agent");
    std::fs::create_dir_all(state.join("answers")).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/acp/reviewer_agent.py")
        .canonicalize()
        .unwrap();
    let agent = format!("python3 {} {}", script.display(), state.display());
    env.tweak = Some(Arc::new(move |o: &mut HubOptions| {
        o.config.runtimes.insert(
            "ex".into(),
            twr_core::config::RuntimePreset {
                acp: agent.clone(),
                attach: None,
                review: Some(twr_core::config::PresetReview {
                    mode: "read-only".into(),
                    models: vec!["m-large".into()],
                    efforts: vec!["medium".into(), "xhigh".into()],
                }),
            },
        );
    }));
    let h = env.hub().await;
    let runner = env.runner();
    online(&h).await;
    let t = box_thread(&env, &h, "api").await;
    let wt = PathBuf::from(h.hub().thread_view(&t).unwrap().worktree.unwrap());
    std::fs::write(wt.join("lib.rs"), "fn changed() {}\n").unwrap();
    start(&h, &t, "review", None, settings("low", "ex/m-large")).await;
    let id = loop_id(&h, &t);
    let last = ended(&h, &t).await;
    assert_eq!(last.outcome, Outcome::Clean);
    // The agent ran (no built-in reviewer did), its answer and log are in both folders.
    assert_eq!(env.runs(), 0);
    let count: usize = std::fs::read_to_string(state.join("count"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(count, 1);
    let folder = box_folder(&env, &id);
    assert!(folder.join("round-m1.answer.json").is_file());
    let log = std::fs::read_to_string(folder.join("round-m1.log")).unwrap();
    assert!(log.contains("acp reviewer ex"), "{log}");
    let hub_folder = PathBuf::from(get(&h, &t)["folder"].as_str().unwrap().to_string());
    assert!(hub_folder.join("round-m1.answer.json").is_file());
    assert!(std::fs::read_to_string(hub_folder.join("round-m1.log"))
        .unwrap()
        .contains("acp reviewer ex"));
    runner.shutdown().await;
    h.shutdown().await;
}

/// The settings are checked against what the thread's box has: a box without Codex gets
/// the setup card with the replacement named for it, though the hub's machine has nothing
/// either; `loop.settings.get {thread}` says the same, and names the box.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_card_replaces_a_reviewer_the_box_lacks_and_names_the_box() {
    let mut env = Env::new().await;
    env.on_box = vec!["claude-code"];
    let h = env.hub().await;
    let runner = env.runner();
    online(&h).await;
    let t = box_thread(&env, &h, "api").await;
    let req: LoopRequest = serde_json::from_value(json!({"thread": t, "kind": "review"})).unwrap();
    let card = h
        .run(|hub| async move { hub.loop_request(req, &user()).await })
        .await
        .unwrap();
    assert_eq!(card["status"], "needs_setup", "{card}");
    let summary = h
        .hub()
        .attention_list(true)
        .into_iter()
        .find(|i| i["attn_id"] == card["attn_id"])
        .unwrap()["summary"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        summary.contains(
            "Warning: codex isn't installed on box gpu: reviewing with claude-code · \
             claude-opus-5-5 instead"
        ),
        "{summary}"
    );
    assert!(
        summary.contains("Reviewer     claude-code · claude-opus-5-5"),
        "{summary}"
    );
    let tt = t.clone();
    let got = h
        .run(|hub| async move {
            hub.loop_settings_get(LoopSettingsFor {
                project: None,
                thread: Some(tt),
            })
            .await
        })
        .await
        .unwrap();
    assert_eq!(got["box"], BOX);
    let codex = got["runtimes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["runtime"] == "codex")
        .unwrap()
        .clone();
    assert_eq!(codex["found"], false, "{codex}");
    let claude = got["runtimes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["runtime"] == "claude-code")
        .unwrap()
        .clone();
    assert_eq!(claude["found"], true, "{claude}");
    // The hub's own machine: nothing is installed there.
    let local = h
        .run(|hub| async move { hub.loop_settings_get(LoopSettingsFor::default()).await })
        .await
        .unwrap();
    assert_eq!(local["box"], Value::Null);
    assert!(local["runtimes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["acp"].is_null())
        .all(|r| r["found"] == false));
    // Started from the card: the replacement runs, on the box.
    h.sync(|hub| {
        hub.respond_at(
            card["attn_id"].as_str().unwrap(),
            Answer::new(START),
            card["revision"].as_u64(),
            &user(),
        )
    })
    .unwrap();
    let started = of_kind(&h, &t, "loop.started");
    let EventBody::LoopStarted(s) = &started[0].body else {
        unreachable!()
    };
    assert_eq!(s.settings.reviewer.runtime, "claude-code");
    runner.shutdown().await;
    h.shutdown().await;
}

/// A runner from before loops (its inventory has none) is refused cleanly, saying to update
/// it; `thread.new` with a loop on it is refused before anything is made.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_old_runner_is_refused_with_a_reason() {
    let env = Env::new().await;
    env.proxy.st().strip_loops = true;
    let h = env.hub().await;
    let runner = env.runner();
    online(&h).await;
    let t = box_thread(&env, &h, "api").await;
    let req: LoopRequest = serde_json::from_value(json!({"thread": t, "kind": "review"})).unwrap();
    let e = h
        .run(|hub| async move { hub.loop_request(req, &user()).await })
        .await
        .unwrap_err();
    assert_eq!(e.code, "unsupported", "{e:?}");
    assert!(
        e.message.contains("can't run loops: update twr on the box"),
        "{e:?}"
    );
    let repo = env.repo("web");
    let req = NewThread {
        project: repo.to_string_lossy().into_owned(),
        runtime: Some("fake".into()),
        name: Some("web".into()),
        box_name: Some(BOX.into()),
        loop_req: Some(serde_json::from_value(json!({"kind": "implement", "task": TASK})).unwrap()),
        ..Default::default()
    };
    let e = h
        .run(|hub| async move { hub.new_thread(req, "test").await })
        .await
        .unwrap_err();
    assert!(e.to_string().contains("update twr on the box"), "{e}");
    assert!(h.hub().thread_view("web").is_err());
    runner.shutdown().await;
    h.shutdown().await;
}

/// The box goes away during a review run: the run is failed (`box gpu disconnected`), the
/// process it ran on the box is killed, the loop waits on the box (its loop line says so)
/// and runs nothing more while it is away; once it is back, the run is retried once, and
/// the loop goes on to its end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_box_that_goes_away_during_a_review_run_fails_it_and_retries_once_back() {
    let env = Env::new().await;
    let h = env.hub().await;
    let runner = env.runner();
    online(&h).await;
    let t = box_thread(&env, &h, "api").await;
    let wt = PathBuf::from(h.hub().thread_view(&t).unwrap().worktree.unwrap());
    std::fs::write(wt.join("lib.rs"), "fn changed() {}\n").unwrap();
    env.hold_run(1);
    start(&h, &t, "review", None, settings("low", "codex/gpt-6.1-sol")).await;
    let (pid, child) = env.held_pids(1).await;
    env.proxy.set_down(true);
    let hub = h.hub().clone();
    let t2 = t.clone();
    until("the failed run", move || {
        hub.thread_events(&t2, 0, 100_000)
            .unwrap()
            .1
            .iter()
            .any(|e| matches!(&e.body, EventBody::LoopFindings(f) if f.failed))
    })
    .await;
    let f = findings_events(&h, &t);
    assert_eq!(f[0].reason.as_deref(), Some("box gpu disconnected"));
    // Its process and its child were killed on the box.
    until("the run's processes gone", || !alive(pid) && !alive(child)).await;
    env.release_run(1);
    // Waiting on the box: no retry while it is away.
    let hub = h.hub().clone();
    let t2 = t.clone();
    until("the loop waiting on the box", move || {
        hub.thread_view(&t2)
            .ok()
            .and_then(|v| v.r#loop)
            .is_some_and(|l| l.box_away.as_deref() == Some(BOX))
    })
    .await;
    let line = twr_core::loops::line(&running(&h, &t).unwrap());
    assert!(line.contains("waiting on box gpu"), "{line}");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(env.runs(), 1);
    assert_eq!(findings_events(&h, &t).len(), 1);
    // Back: the retry runs, and the loop ends as it would have.
    env.proxy.set_down(false);
    let last = ended(&h, &t).await;
    assert_eq!(last.outcome, Outcome::Clean);
    // (The retry answers clean: on `low`, no verification follows a clean first round.)
    assert_eq!(env.runs(), 2);
    let f = findings_events(&h, &t);
    assert_eq!(f.len(), 2, "{f:?}");
    assert!(!f[1].failed);
    assert!(of_kind(&h, &t, "attention.raised").iter().all(
        |e| !matches!(&e.body, EventBody::AttentionRaised(a) if a.reason == AttentionReason::Error)
    ));
    runner.shutdown().await;
    h.shutdown().await;
}

/// The box goes away during a triage turn: the box's runner keeps the turn going and spools
/// its events, and once the box is back the loop takes the turn's answer, once, and goes on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_box_that_goes_away_during_a_triage_turn_answers_once_back() {
    let env = Env::new().await;
    env.set(|b| b.replies.push((" triage m1]".into(), "@slow 1500".into())));
    let h = env.hub().await;
    let runner = env.runner();
    online(&h).await;
    let t = box_thread(&env, &h, "api").await;
    let wt = PathBuf::from(h.hub().thread_view(&t).unwrap().worktree.unwrap());
    std::fs::write(wt.join("lib.rs"), "fn changed() {}\n").unwrap();
    env.answer(1, findings(1));
    start(&h, &t, "review", None, settings("low", "codex/gpt-6.1-sol")).await;
    until("the triage turn", || !env.seen(" triage m1]").is_empty()).await;
    env.proxy.set_down(true);
    tokio::time::sleep(Duration::from_millis(2500)).await;
    env.proxy.set_down(false);
    let last = ended(&h, &t).await;
    assert_eq!(last.outcome, Outcome::Clean);
    assert_eq!(of_kind(&h, &t, "loop.triage").len(), 1);
    assert_eq!(env.seen(" triage m1]").len(), 1);
    assert_eq!(env.runs(), 2);
    runner.shutdown().await;
    h.shutdown().await;
}

/// The box goes away while the commit's check is due: the check waits for it (the loop
/// says so), never counting the box's absence as a commit problem, and passes once back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_commit_check_waits_for_the_box() {
    let mut env = Env::new().await;
    let proxy = env.proxy.clone();
    env.tweak = Some(Arc::new(move |o: &mut HubOptions| {
        let proxy = proxy.clone();
        o.loop_record_hook = Some(Arc::new(move |point: &str| {
            if point == "answer report" {
                proxy.set_down(true);
            }
        }));
    }));
    let h = env.hub().await;
    let runner = env.runner();
    online(&h).await;
    let t = box_thread(&env, &h, "export").await;
    let mut s = settings("low", "codex/gpt-6.1-sol");
    s.commit = ImplCommit::End;
    start(&h, &t, "implement", Some(TASK), s).await;
    let hub = h.hub().clone();
    let t2 = t.clone();
    until("the loop waiting on the box", move || {
        hub.thread_view(&t2)
            .ok()
            .and_then(|v| v.r#loop)
            .is_some_and(|l| l.box_away.is_some())
    })
    .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(of_kind(&h, &t, "loop.commit").is_empty());
    env.proxy.set_down(false);
    let last = ended(&h, &t).await;
    assert_eq!(last.outcome, Outcome::Clean);
    assert_eq!(last.commit_problem, None);
    assert_eq!(last.commits.len(), 1);
    let checks = of_kind(&h, &t, "loop.commit");
    assert_eq!(checks.len(), 1);
    runner.shutdown().await;
    h.shutdown().await;
}

/// The box's copy of the folder deleted mid-loop (the box cleaned): the hub writes it again
/// before the next step reads it, and the loop goes on.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_box_that_lost_the_loop_folder_gets_it_again() {
    let env = Env::new().await;
    env.set(|b| b.replies.push((" triage m1]".into(), "@slow 1000".into())));
    let h = env.hub().await;
    let runner = env.runner();
    online(&h).await;
    let t = box_thread(&env, &h, "export").await;
    env.answer(1, findings(1));
    start(
        &h,
        &t,
        "implement",
        Some(TASK),
        settings("low", "codex/gpt-6.1-sol"),
    )
    .await;
    until("the triage turn", || !env.seen(" triage m1]").is_empty()).await;
    let id = loop_id(&h, &t);
    let folder = box_folder(&env, &id);
    std::fs::remove_dir_all(&folder).unwrap();
    let last = ended(&h, &t).await;
    assert_eq!(last.outcome, Outcome::Clean);
    // Written again: what the verification's reviewer read.
    for f in ["plan.md", "ledger.md", "findings.schema.json", "task.md"] {
        assert!(folder.join(f).is_file(), "{f}");
    }
    let verify = env.run_file(2, "stdin");
    assert!(verify.contains(&folder.join("plan.md").display().to_string()));
    runner.shutdown().await;
    h.shutdown().await;
}

/// The hub restarts with a review run going on the box: whether the box stays connected or
/// is away while the hub is down, the run is failed once (`the hub stopped while it ran`),
/// what the box ran of it is gone, and the retry runs once the box is there; nothing runs
/// twice beside itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hub_restart_mid_run_with_the_box_connected_and_away() {
    for away in [false, true] {
        let env = Env::new().await;
        let h = env.hub().await;
        let runner = env.runner();
        online(&h).await;
        let t = box_thread(&env, &h, "api").await;
        let wt = PathBuf::from(h.hub().thread_view(&t).unwrap().worktree.unwrap());
        std::fs::write(wt.join("lib.rs"), "fn changed() {}\n").unwrap();
        env.hold_run(1);
        start(&h, &t, "review", None, settings("low", "codex/gpt-6.1-sol")).await;
        let (pid, child) = env.held_pids(1).await;
        if away {
            env.proxy.set_down(true);
        }
        h.crash();
        // The runner ends what its connection to the dead hub started.
        until("the run's processes gone", || !alive(pid) && !alive(child)).await;
        env.release_run(1);
        let h = env.hub().await;
        if away {
            let hub = h.hub().clone();
            let t2 = t.clone();
            until("the loop waiting on the box", move || {
                hub.thread_view(&t2)
                    .ok()
                    .and_then(|v| v.r#loop)
                    .is_some_and(|l| l.box_away.is_some())
            })
            .await;
            assert_eq!(env.runs(), 1);
            env.proxy.set_down(false);
        }
        let last = ended(&h, &t).await;
        assert_eq!(last.outcome, Outcome::Clean, "away {away}");
        let f = findings_events(&h, &t);
        assert!(f[0].failed, "{f:?}");
        assert_eq!(
            f[0].reason.as_deref(),
            Some("the hub stopped while it ran"),
            "away {away}"
        );
        assert_eq!(env.runs(), 2, "away {away}");
        runner.shutdown().await;
        h.shutdown().await;
    }
}

/// The hub killed while the thread's scope turn runs on the box: the box spools the turn's
/// events, and the new hub takes its answer once, as after any restart.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_hub_killed_during_a_thread_turn_takes_its_answer_once() {
    let env = Env::new().await;
    env.set(|b| b.replies.push((" scope]".into(), "@slow 2000".into())));
    let h = env.hub().await;
    let runner = env.runner();
    online(&h).await;
    let t = box_thread(&env, &h, "api").await;
    let wt = PathBuf::from(h.hub().thread_view(&t).unwrap().worktree.unwrap());
    std::fs::write(wt.join("lib.rs"), "fn changed() {}\n").unwrap();
    start(&h, &t, "review", None, settings("low", "codex/gpt-6.1-sol")).await;
    until("the scope turn", || !env.seen(" scope]").is_empty()).await;
    h.crash();
    let h = env.hub().await;
    let last = ended(&h, &t).await;
    assert_eq!(last.outcome, Outcome::Clean);
    let goals: Vec<Event> = of_kind(&h, &t, "loop.phase")
        .into_iter()
        .filter(|e| matches!(&e.body, EventBody::LoopPhase(p) if p.goal.is_some()))
        .collect();
    assert_eq!(goals.len(), 1);
    assert_eq!(env.seen(" scope]").len(), 1);
    runner.shutdown().await;
    h.shutdown().await;
}

/// `loop.settings.set` (the editor, `twr loop settings set`) checks the hub's own machine by
/// default, and says so when it refuses; with `box` it checks that box's agent CLIs instead:
/// settings a box thread's card saved can then be changed without the hub's machine having the
/// box's reviewer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn settings_saved_for_a_box_are_checked_against_its_agents() {
    let mut env = Env::new().await;
    env.on_box = vec!["claude-code"];
    let h = env.hub().await;
    let runner = env.runner();
    online(&h).await;
    let t = box_thread(&env, &h, "api").await;
    // The box's agents reported (Claude Code there, no Codex).
    let mut reported = false;
    for _ in 0..200 {
        let tt = t.clone();
        let got = h
            .run(|hub| async move {
                hub.loop_settings_get(LoopSettingsFor {
                    project: None,
                    thread: Some(tt),
                })
                .await
            })
            .await
            .unwrap();
        let found = |rt: &str| {
            got["runtimes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["runtime"] == rt && r["found"] == true)
        };
        if found("claude-code") && !found("codex") {
            reported = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(reported, "the box never reported its agents");
    let hr = &h;
    let set = |s: LoopSettings, b: Option<&str>| {
        let p = LoopSettingsSet {
            project: None,
            settings: s,
            box_name: b.map(str::to_string),
        };
        async move {
            hr.run(|hub| async move { hub.loop_settings_set(p, &user()).await })
                .await
        }
    };
    let claude = settings("low", "claude-code/claude-opus-5-5");
    // The hub's own machine has no reviewer: refused, saying where it looked.
    let e = set(claude.clone(), None).await.unwrap_err();
    assert_eq!(e.code, "bad_request", "{e:?}");
    assert!(e.message.contains("hub's own machine"), "{}", e.message);
    assert!(e.message.contains("--box"), "{}", e.message);
    // Checked against the box: it holds there.
    set(claude, Some(BOX)).await.unwrap();
    // A reviewer the box lacks is refused for the box, by name.
    let e = set(settings("low", "codex/gpt-6.1-sol"), Some(BOX))
        .await
        .unwrap_err();
    assert!(
        e.message.contains("codex isn't installed on box gpu"),
        "{}",
        e.message
    );
    assert!(!e.message.contains("--box"), "{}", e.message);
    // A box the hub does not have.
    let e = set(settings("low", "codex/gpt-6.1-sol"), Some("nope"))
        .await
        .unwrap_err();
    assert_eq!(e.code, "not_found", "{e:?}");
    runner.shutdown().await;
    h.shutdown().await;
}

/// A thread's next loop removes its previous loop's folder from the box when it first writes
/// its own, also when the hub restarted in between (it knows the previous loop from the
/// thread's record, not from what it wrote itself); deleting the thread with the box connected
/// removes the last one's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_next_loop_removes_the_previous_loops_folder_from_the_box() {
    let env = Env::new().await;
    let h = env.hub().await;
    let runner = env.runner();
    online(&h).await;
    let t = box_thread(&env, &h, "api").await;
    let wt = PathBuf::from(h.hub().thread_view(&t).unwrap().worktree.unwrap());
    std::fs::write(wt.join("lib.rs"), "fn changed() {}\n").unwrap();
    start(&h, &t, "review", None, settings("low", "codex/gpt-6.1-sol")).await;
    let first = loop_id(&h, &t);
    ended(&h, &t).await;
    // Kept after the end, for the thread's later turns (its report reaches the box once the
    // loop has ended: written after the end is recorded).
    let report = box_folder(&env, &first).join("report.md");
    until("report.md in the box's folder", || report.is_file()).await;
    h.shutdown().await;
    let h = env.hub().await;
    online(&h).await;
    // The next loop starts on the saved settings.
    let req: LoopRequest = serde_json::from_value(json!({"thread": t, "kind": "review"})).unwrap();
    let r = h
        .run(|hub| async move { hub.loop_request(req, &user()).await })
        .await
        .unwrap();
    assert_eq!(r["status"], "started", "{r}");
    let second = loop_id(&h, &t);
    assert_ne!(second, first);
    until("the previous loop's folder gone from the box", || {
        !box_folder(&env, &first).exists()
    })
    .await;
    ended(&h, &t).await;
    assert!(box_folder(&env, &second).is_dir());
    runner.shutdown().await;
    h.shutdown().await;
}
