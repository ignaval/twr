//! The orchestrator over the socket: caller identity from `hello`, `say`, and
//! an MCP end-to-end run (in-process hub + `twr_mcp` over a duplex pipe with the hub's token).

use std::sync::Arc;
use std::time::Duration;

use rmcp::model::{CallToolRequestParams, CallToolResult};
use rmcp::ServiceExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::timeout;
use twr_adapters::fake::FakeAdapter;
use twr_core::adapter::{Adapter, AdapterEnd, AdapterHandle, AdapterInfo, SpawnConfig, Tier};
use twr_core::client::{Client, Subscription};
use twr_core::config::Config;
use twr_core::event::{EventBody, SessionPayload, Source, TurnFrom};
use twr_core::paths::Paths;
use twr_core::proto::{codes, RpcError, StreamItem};
use twr_hub::{server, Hub, HubOptions};

const WAIT: Duration = Duration::from_secs(10);

struct OrchRt {
    cfg: SpawnConfig,
    /// Kept so the runtime stays up until the test drops it.
    _end: AdapterEnd,
}

impl OrchRt {
    /// The MCP configuration the runtime gets (a file: the token never goes on a command line).
    fn mcp(&self) -> Value {
        let path = self
            .cfg
            .mcp_config_file
            .as_ref()
            .expect("an MCP config file");
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn token(&self) -> String {
        self.mcp()["mcpServers"]["twr"]["env"]["TWR_ORCH_TOKEN"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

struct OrchAdapter(mpsc::UnboundedSender<OrchRt>);

impl Adapter for OrchAdapter {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "claude-code",
            tier: Tier::Headless,
            description: "test orchestrator runtime",
        }
    }

    fn spawn(&self, cfg: SpawnConfig, _prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let (h, end) = AdapterHandle::pair();
        end.emit(
            EventBody::ThreadStarted(SessionPayload {
                session_id: Some("s-orch".into()),
                ..Default::default()
            }),
            None,
        );
        let _ = self.0.send(OrchRt { cfg, _end: end });
        Ok(h)
    }
}

struct TestHub {
    dir: tempfile::TempDir,
    socket: std::path::PathBuf,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
    hub: Hub,
    spawns: mpsc::UnboundedReceiver<OrchRt>,
}

async fn start() -> TestHub {
    let (tx, spawns) = mpsc::unbounded_channel();
    start_with(Arc::new(OrchAdapter(tx)), spawns).await
}

/// An orchestrator runtime that cannot start: it reports the token it was given, then fails.
struct FailingOrch(mpsc::UnboundedSender<String>);

impl Adapter for FailingOrch {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "claude-code",
            tier: Tier::Headless,
            description: "test orchestrator runtime that fails to start",
        }
    }

    fn spawn(&self, cfg: SpawnConfig, _prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let path = cfg.mcp_config_file.as_ref().expect("an MCP config file");
        let mcp: Value = serde_json::from_str(&std::fs::read_to_string(path)?)?;
        let token = mcp["mcpServers"]["twr"]["env"]["TWR_ORCH_TOKEN"]
            .as_str()
            .unwrap()
            .to_string();
        let _ = self.0.send(token);
        anyhow::bail!("the orchestrator's CLI is not installed")
    }
}

async fn start_with(orch: Arc<dyn Adapter>, spawns: mpsc::UnboundedReceiver<OrchRt>) -> TestHub {
    start_tweaked(orch, spawns, |_| {}).await
}

/// [`start_with`], with the hub's options changed by `tweak`.
async fn start_tweaked(
    orch: Arc<dyn Adapter>,
    spawns: mpsc::UnboundedReceiver<OrchRt>,
    tweak: impl FnOnce(&mut HubOptions),
) -> TestHub {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::with_home(dir.path());
    let listener = server::bind(&paths.socket).await.unwrap();
    let mut config = Config {
        default_runtime: "fake".into(),
        ..Default::default()
    };
    config.orchestrator.runtime = Some("claude-code".into());
    let mut opts = HubOptions::new(paths.db_file(), config);
    opts.adapters = Arc::new(move |name: &str| -> Option<Arc<dyn Adapter>> {
        match name {
            "fake" => Some(Arc::new(FakeAdapter::new())),
            "claude-code" => Some(Arc::clone(&orch)),
            _ => None,
        }
    });
    opts.socket = Some(paths.socket.clone());
    // The orchestrator runtime runs in this process: there is no process for its MCP server
    // to descend from, so the token is taken from this process's connections.
    opts.orchestrator_token_unverified = true;
    tweak(&mut opts);
    let hub = Hub::open(opts).await.unwrap();
    let task = tokio::spawn(server::serve(
        hub.clone(),
        listener,
        paths.socket.clone(),
        false,
    ));
    TestHub {
        dir,
        socket: paths.socket,
        task,
        hub,
        spawns,
    }
}

impl TestHub {
    async fn client(&self, name: &str) -> Client {
        let mut c = Client::connect(&self.socket).await.unwrap();
        c.hello(json!({ "client": name })).await.unwrap();
        c
    }

    async fn orch(&mut self) -> OrchRt {
        timeout(WAIT, self.spawns.recv()).await.unwrap().unwrap()
    }

    /// Two fake workers and a delegated message: the orchestrator runs (turn 1 open).
    async fn with_orchestrator(&mut self, c: &mut Client) -> OrchRt {
        for name in ["api", "web"] {
            c.call(
                "thread.new",
                json!({"project": self.dir.path(), "runtime": "fake", "name": name}),
            )
            .await
            .unwrap();
        }
        let r = c
            .call("say", json!({"message": "coordinate the release"}))
            .await
            .unwrap();
        assert_eq!(r["action"], "delegate", "{r}");
        self.orch().await
    }

    async fn stop(self) {
        let mut c = self.client("end").await;
        c.call("hub.shutdown", Value::Null).await.unwrap();
        self.task.await.unwrap().unwrap();
    }
}

fn code(e: &anyhow::Error) -> String {
    e.downcast_ref::<RpcError>()
        .map(|r| r.code.clone())
        .unwrap_or_else(|| format!("{e:#}"))
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

fn turn_started_on(thread: &str) -> impl Fn(&StreamItem) -> bool + '_ {
    move |i| i.event.kind() == "turn.started" && i.event.thread == thread
}

#[tokio::test]
async fn hello_decides_the_callers_identity() {
    let mut th = start().await;
    let mut c = Client::connect(&th.socket).await.unwrap();
    // The orchestrator's name needs its token; a wrong token is refused.
    let e = c
        .hello(json!({"client": "orchestrator"}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::UNAUTHORIZED);
    let e = c
        .hello(json!({"client": "orchestrator", "token": "00ff"}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::UNAUTHORIZED);
    let r = c.hello(json!({"client": "cli"})).await.unwrap();
    assert_eq!(r["identity"], "user:cli");
    let mut rt = th.with_orchestrator(&mut c).await;
    let token = rt.token();
    let api = th.hub.resolve("api").unwrap();
    let web = th.hub.resolve("web").unwrap();

    let mut o = Client::connect(&th.socket).await.unwrap();
    let r = o
        .hello(json!({"client": "orchestrator", "token": token}))
        .await
        .unwrap();
    assert_eq!(r["identity"], "orchestrator");
    let info = o.call("hub.info", Value::Null).await.unwrap();
    assert_eq!(info["identity"], "orchestrator");
    assert!(!info.to_string().contains(&token));
    // Once the orchestrator, a connection cannot say hello again as a user client.
    let e = o.hello(json!({"client": "cli"})).await.unwrap_err();
    assert_eq!(code(&e), codes::UNAUTHORIZED);
    let info = o.call("hub.info", Value::Null).await.unwrap();
    assert_eq!(info["identity"], "orchestrator");

    // `from` follows the identity, whatever the request claims.
    let mut sub = Client::subscribe_at(&th.socket, "control.subscribe", json!({"since": 0}))
        .await
        .unwrap();
    o.call(
        "thread.send",
        json!({"thread": "api", "prompt": "hi from orch", "from": "user"}),
    )
    .await
    .unwrap();
    let item = until(&mut sub, turn_started_on(&api)).await;
    assert_eq!(item.event.source, Source::Orchestrator);
    assert_eq!(item.event.body.data().unwrap()["from"], "orchestrator");
    c.call(
        "thread.send",
        json!({"thread": "web", "prompt": "hi from me", "from": "orchestrator"}),
    )
    .await
    .unwrap();
    let item = until(&mut sub, turn_started_on(&web)).await;
    assert_eq!(item.event.source, Source::User("cli".into()));
    let EventBody::TurnStarted(p) = &item.event.body else {
        panic!()
    };
    assert_eq!(p.from, TurnFrom::User);

    // The orchestrator may not shut the hub down, attach, say for the user or switch the
    // orchestrator.
    for (m, p) in [
        ("hub.shutdown", Value::Null),
        ("attach.begin", json!({"thread": "api"})),
        ("say", json!({"message": "hi"})),
        ("orchestrator.use", json!({"runtime": "claude-code"})),
    ] {
        let e = o.call(m, p).await.unwrap_err();
        assert_eq!(code(&e), codes::UNAUTHORIZED, "{m}");
    }
    // Its answers go through the relay guard.
    o.call(
        "thread.send",
        json!({"thread": "web", "prompt": "!perm Bash ls"}),
    )
    .await
    .unwrap();
    let raised = until(&mut sub, |i| i.event.kind() == "attention.raised").await;
    let attn = raised.event.body.data().unwrap()["attn_id"].clone();
    let e = o
        .call(
            "attention.respond",
            json!({"attn_id": attn, "answer": "allow"}),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::RELAY_FORBIDDEN);
    let ann = o
        .call(
            "attention.annotate",
            json!({"attn_id": attn, "note": "harmless ls"}),
        )
        .await
        .unwrap();
    assert_eq!(ann["source"], "orchestrator");

    // A new orchestrator process gets a new token: the old connection loses its identity.
    let orch_id = th.hub.orchestrator_thread().unwrap();
    c.call("thread.stop", json!({"thread": "orch"}))
        .await
        .unwrap();
    drop(rt);
    until(&mut sub, |i| {
        i.event.kind() == "thread.stopped" && i.event.thread == orch_id
    })
    .await;
    // Its runtime exited: the token died with it, before any new orchestrator starts.
    assert!(!th.hub.orchestrator_token_is(&token));
    let e = o.call("thread.list", Value::Null).await.unwrap_err();
    assert_eq!(code(&e), codes::UNAUTHORIZED);
    let mut late = Client::connect(&th.socket).await.unwrap();
    let e = late
        .hello(json!({"client": "orchestrator", "token": token}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::UNAUTHORIZED);
    c.call("thread.send", json!({"thread": "orch", "prompt": "back"}))
        .await
        .unwrap();
    rt = th.orch().await;
    assert_ne!(rt.token(), token);
    let e = o.call("thread.list", Value::Null).await.unwrap_err();
    assert_eq!(code(&e), codes::UNAUTHORIZED);
    // ... and cannot get a user identity back by dropping the stale token.
    let e = o.hello(json!({"client": "cli"})).await.unwrap_err();
    assert_eq!(code(&e), codes::UNAUTHORIZED);
    let e = o.call("thread.list", Value::Null).await.unwrap_err();
    assert_eq!(code(&e), codes::UNAUTHORIZED);
    drop(rt);
    th.stop().await;
}

/// The orchestrator's connection may call only `ORCHESTRATOR_METHODS`: every other method the
/// hub has is refused (`unauthorized`, "the orchestrator may not call <method>") before
/// anything runs, the hub's shutdown included.
#[tokio::test]
async fn the_orchestrator_calls_only_its_methods() {
    let mut th = start().await;
    let mut c = th.client("cli").await;
    let rt = th.with_orchestrator(&mut c).await;
    let mut o = Client::connect(&th.socket).await.unwrap();
    o.hello(json!({"client": "orchestrator", "token": rt.token()}))
        .await
        .unwrap();
    let outside: Vec<&str> = (server::METHODS.iter())
        .filter(|m| !server::ORCHESTRATOR_METHODS.contains(m))
        .copied()
        .collect();
    for m in [
        "hub.shutdown",
        "say",
        "attach.begin",
        "job.start",
        "trash.empty",
    ] {
        assert!(outside.contains(&m), "{m}");
    }
    for m in &outside {
        let e = o.call(m, Value::Null).await.unwrap_err();
        let e = e.downcast_ref::<RpcError>().unwrap();
        assert_eq!(e.code, codes::UNAUTHORIZED, "{m}: {e:?}");
        assert_eq!(e.message, format!("the orchestrator may not call {m}"));
    }
    // Still the orchestrator, and the hub still runs.
    let info = o.call("hub.info", Value::Null).await.unwrap();
    assert_eq!(info["identity"], "orchestrator");
    drop(rt);
    th.stop().await;
}

/// What the orchestrator reads of the boxes over its connection: each box's name, state,
/// whether it is online, its version and its agents, and of each agent whether it is found
/// and signed in (`hub.info`, `box.agents`), where the user's connection reads everything; a
/// fresh probe of a box's agents is the user's alone.
#[tokio::test]
async fn the_orchestrator_reads_the_boxes_state_only() {
    use futures::FutureExt;
    use twr_core::runner::{AgentInfo, AgentLogin};
    let (tx, spawns) = mpsc::unbounded_channel();
    let mut th = start_tweaked(Arc::new(OrchAdapter(tx)), spawns, |o| {
        o.config.boxes.insert(
            "gpu".into(),
            twr_core::config::BoxConfig {
                token: Some("gpu-token-0123456789abcdef0123456789".into()),
                ssh: Some("me@gpu.invalid".into()),
                ..Default::default()
            },
        );
        o.agent_probe = Some(Arc::new(|| {
            async {
                vec![AgentInfo {
                    runtime: "claude-code".into(),
                    found: true,
                    path: Some("/home/me/.local/bin/claude".into()),
                    version: Some("2.1.302 (Claude Code)".into()),
                    login: AgentLogin::LoggedIn,
                    method: Some("claude.ai".into()),
                    note: None,
                }]
            }
            .boxed()
        }));
    })
    .await;
    let mut c = th.client("cli").await;
    let rt = th.with_orchestrator(&mut c).await;
    let mut o = Client::connect(&th.socket).await.unwrap();
    o.hello(json!({"client": "orchestrator", "token": rt.token()}))
        .await
        .unwrap();
    timeout(WAIT, async {
        while th.hub.info()["agents"].as_array().is_none_or(Vec::is_empty) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the hub's own probe");

    let full = c.call("hub.info", Value::Null).await.unwrap();
    assert_eq!(full["boxes"][0]["transport"], "token", "{full}");
    assert_eq!(full["agents"][0]["path"], "/home/me/.local/bin/claude");
    let info = o.call("hub.info", Value::Null).await.unwrap();
    assert_eq!(info["identity"], "orchestrator");
    assert_eq!(
        info["boxes"],
        json!([{"name": "gpu", "state": "offline", "online": false, "version": null, "agents": []}])
    );
    assert_eq!(
        info["agents"],
        json!([{"runtime": "claude-code", "found": true, "login": "logged_in"}])
    );
    assert!(!info.to_string().contains("/home/me"), "{info}");
    let local = o.call("box.agents", json!({"box": "local"})).await.unwrap();
    assert_eq!(
        local,
        json!({"box": "local", "online": true, "agents": [{"runtime": "claude-code", "found": true, "login": "logged_in"}]})
    );
    let gpu = o.call("box.agents", json!({"box": "gpu"})).await.unwrap();
    assert_eq!(gpu, json!({"box": "gpu", "online": false, "agents": []}));
    let e = o
        .call("box.agents", json!({"box": "local", "refresh": true}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::UNAUTHORIZED);
    // The user's: everything, a fresh probe too (`box_offline` for a box that is not there).
    let mine = c
        .call("box.agents", json!({"box": "local", "refresh": true}))
        .await
        .unwrap();
    assert_eq!(mine["agents"][0]["method"], "claude.ai", "{mine}");
    let e = c
        .call("box.agents", json!({"box": "gpu", "refresh": true}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::BOX_OFFLINE);
    drop(rt);
    th.stop().await;
}

/// A token is valid only once its runtime started: an orchestrator spawn that fails leaves no
/// token anyone can present (it was in a file the failed runtime could read).
#[tokio::test]
async fn a_failed_orchestrator_spawn_leaves_no_valid_token() {
    let (tx, mut tokens) = mpsc::unbounded_channel();
    let (_unused, spawns) = mpsc::unbounded_channel();
    let th = start_with(Arc::new(FailingOrch(tx)), spawns).await;
    let mut c = th.client("cli").await;
    c.call(
        "thread.new",
        json!({"project": th.dir.path(), "runtime": "fake", "name": "api"}),
    )
    .await
    .unwrap();
    c.call(
        "thread.new",
        json!({"project": th.dir.path(), "runtime": "fake", "name": "web"}),
    )
    .await
    .unwrap();
    let r = c
        .call("say", json!({"message": "coordinate the release"}))
        .await
        .unwrap();
    assert_eq!(r["action"], "delegate", "{r}");
    let token = timeout(WAIT, tokens.recv()).await.unwrap().unwrap();
    assert!(!th.hub.orchestrator_token_is(&token));
    let mut o = Client::connect(&th.socket).await.unwrap();
    let e = o
        .hello(json!({"client": "orchestrator", "token": token}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::UNAUTHORIZED);
    th.stop().await;
}

// ---------------------------------------------------------------------------------------------
// MCP end to end
// ---------------------------------------------------------------------------------------------

async fn call_tool(
    client: &rmcp::service::RunningService<rmcp::RoleClient, ()>,
    tool: &str,
    args: Value,
) -> CallToolResult {
    let mut p = CallToolRequestParams::new(tool.to_string());
    if let Value::Object(m) = args {
        p = p.with_arguments(m);
    }
    client.call_tool(p).await.unwrap()
}

fn text(r: &CallToolResult) -> String {
    r.content
        .iter()
        .filter_map(|c| c.as_text())
        .map(|t| t.text.as_str())
        .collect::<Vec<_>>()
        .join("")
}

#[tokio::test]
async fn mcp_tools_drive_the_hub_as_the_orchestrator() {
    let mut th = start().await;
    let mut c = th.client("cli").await;
    let rt = th.with_orchestrator(&mut c).await;
    // What the runtime would launch: the MCP server with the hub's socket and token.
    let mcp = rt.mcp();
    let server = &mcp["mcpServers"]["twr"];
    assert_eq!(server["args"][2], json!(th.socket));
    let (server_io, client_io) = tokio::io::duplex(1 << 16);
    let socket = th.socket.clone();
    let token = rt.token();
    tokio::spawn(async move {
        twr_mcp::serve_with_token(server_io, socket, token)
            .await
            .unwrap();
    });
    let mcp = ().serve(client_io).await.unwrap();
    let api = th.hub.resolve("api").unwrap();
    let mut sub = Client::subscribe_at(&th.socket, "control.subscribe", json!({"since": 0}))
        .await
        .unwrap();

    let r = call_tool(&mcp, "list_threads", json!({})).await;
    assert_ne!(r.is_error, Some(true), "{}", text(&r));
    let threads: Value = serde_json::from_str(&text(&r)).unwrap();
    let names: Vec<&str> = threads
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["api", "web", "orchestrator"]);

    let r = call_tool(
        &mcp,
        "send",
        json!({"thread": "api", "prompt": "ship the fix"}),
    )
    .await;
    assert_ne!(r.is_error, Some(true), "{}", text(&r));
    let item = until(&mut sub, turn_started_on(&api)).await;
    assert_eq!(item.event.source, Source::Orchestrator);
    assert_eq!(item.event.body.data().unwrap()["from"], "orchestrator");
    until(&mut sub, |i| {
        i.event.kind() == "report.ready" && i.event.thread == api
    })
    .await;

    let r = call_tool(
        &mcp,
        "write_report",
        json!({"thread": "api", "summary": "Shipped the fix. Tests pass.", "grade": "success"}),
    )
    .await;
    assert_ne!(r.is_error, Some(true), "{}", text(&r));
    let item = until(&mut sub, |i| {
        i.event.kind() == "report.ready"
            && i.event.thread == api
            && i.event.source == Source::Orchestrator
    })
    .await;
    let data = item.event.body.data().unwrap();
    assert_eq!(data["written_by"], "orchestrator");
    assert_eq!(data["summary"], "Shipped the fix. Tests pass.");

    // Standing context: the orchestrator may read and replace it.
    let project = th
        .dir
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let r = call_tool(
        &mcp,
        "set_context",
        json!({"project": project, "content": "Release on Fridays only."}),
    )
    .await;
    assert_ne!(r.is_error, Some(true), "{}", text(&r));
    let item = until(&mut sub, |i| i.event.kind() == "context.updated").await;
    assert_eq!(item.event.source, Source::Orchestrator);
    let r = call_tool(&mcp, "read_context", json!({"project": project})).await;
    assert_ne!(r.is_error, Some(true), "{}", text(&r));
    let read: Value = serde_json::from_str(&text(&r)).unwrap();
    assert_eq!(read["content"], "Release on Fridays only.");
    assert_eq!(
        std::fs::read_to_string(th.dir.path().join(".twr/context.md")).unwrap(),
        "Release on Fridays only."
    );
    let r = call_tool(&mcp, "read_context", json!({"project": "nope"})).await;
    assert_eq!(r.is_error, Some(true));
    assert!(text(&r).contains("not_found"), "{}", text(&r));

    // Park and resume (a non-git project: no worktree to remove).
    let r = call_tool(&mcp, "park", json!({"thread": "api"})).await;
    assert_ne!(r.is_error, Some(true), "{}", text(&r));
    let out: Value = serde_json::from_str(&text(&r)).unwrap();
    assert_eq!(
        (out["parked"].clone(), out["kept_dirty"].clone()),
        (json!(true), json!(false))
    );
    let item = until(&mut sub, |i| {
        i.event.kind() == "thread.parked" && i.event.thread == api
    })
    .await;
    assert_eq!(item.event.source, Source::Orchestrator);
    let r = call_tool(&mcp, "park", json!({"thread": "api"})).await;
    assert_eq!(r.is_error, Some(true));
    assert!(text(&r).contains("conflict"), "{}", text(&r));
    let r = call_tool(&mcp, "resume", json!({"thread": "api"})).await;
    assert_ne!(r.is_error, Some(true), "{}", text(&r));
    let item = until(&mut sub, |i| {
        i.event.kind() == "thread.resumed" && i.event.thread == api
    })
    .await;
    assert_eq!(item.event.source, Source::Orchestrator);

    // A reply over the payload cap is stored as a preview: the transcript line says so, and
    // read_transcript seq=<reply_seq> reads it in full (blob.get as the orchestrator).
    let web = th.hub.resolve("web").unwrap();
    c.call(
        "thread.send",
        json!({"thread": "web", "prompt": "!long 9000"}),
    )
    .await
    .unwrap();
    let report = until(&mut sub, |i| {
        i.event.kind() == "report.ready" && i.event.thread == web
    })
    .await;
    let reply_seq = report.event.body.data().unwrap()["ext"]["twr"]["reply_seq"]
        .as_u64()
        .unwrap();
    let r = call_tool(&mcp, "read_transcript", json!({"thread": "web"})).await;
    let t = text(&r);
    let line = t
        .lines()
        .find(|l| l.starts_with(&format!("#{reply_seq} text: ")))
        .unwrap_or_else(|| panic!("{t}"));
    assert!(
        line.ends_with(&format!(
            "x… [cut; read_transcript seq={reply_seq} shows it in full]"
        )),
        "{line}"
    );
    assert!(line.len() < 1000, "{line}");
    let r = call_tool(
        &mcp,
        "read_transcript",
        json!({"thread": "web", "seq": reply_seq}),
    )
    .await;
    assert_ne!(r.is_error, Some(true), "{}", text(&r));
    let t = text(&r);
    let lines: Vec<&str> = t.lines().collect();
    assert_eq!(lines.len(), 3, "{t}");
    assert!(
        lines[0].starts_with(&format!("thread {web} #{reply_seq} agent.text turn ")),
        "{t}"
    );
    assert!(lines[0].ends_with(": text (9000 bytes)"), "{t}");
    assert_eq!(lines[1], "x".repeat(9000));
    assert_eq!(
        lines[2],
        format!("[bytes 0-9000 of 9000; end of #{reply_seq}]")
    );

    // A relay the user did not give is refused, as a tool error with the hub's code.
    c.call(
        "thread.send",
        json!({"thread": "web", "prompt": "!perm Bash ls"}),
    )
    .await
    .unwrap();
    let raised = until(&mut sub, |i| i.event.kind() == "attention.raised").await;
    let attn = raised.event.body.data().unwrap()["attn_id"].clone();
    let r = call_tool(
        &mcp,
        "respond_attention",
        json!({"attn_id": attn, "answer": "allow"}),
    )
    .await;
    assert_eq!(r.is_error, Some(true));
    assert!(text(&r).contains("relay_forbidden"), "{}", text(&r));

    mcp.cancel().await.unwrap();
    drop(rt);
    th.stop().await;
}

#[tokio::test]
async fn reset_over_the_socket_changes_only_the_selected_instance() {
    let mut a = start().await;
    let mut b = start().await;
    let mut ca = a.client("cli").await;
    let mut cb = b.client("cli").await;
    let old_rt = a.with_orchestrator(&mut ca).await;
    let b_rt = b.with_orchestrator(&mut cb).await;
    let old = a.hub.orchestrator_thread().unwrap();
    let b_old = b.hub.orchestrator_thread().unwrap();
    let mut b_sub = b
        .client("watch")
        .await
        .subscribe("control.subscribe", json!({"since": 0}))
        .await
        .unwrap();
    until(&mut b_sub, |i| {
        i.event.thread == b_old && i.event.kind() == "thread.started"
    })
    .await;
    let b_head = b.hub.info()["control_head"].clone();
    let old_token = old_rt.token();
    let mut orch = Client::connect(&a.socket).await.unwrap();
    orch.hello(json!({"client": "orchestrator", "token": old_token}))
        .await
        .unwrap();
    let error = orch
        .call("orchestrator.reset", json!({}))
        .await
        .unwrap_err();
    assert_eq!(code(&error), codes::UNAUTHORIZED);
    let error = ca
        .call("orchestrator.reset", json!({"runtime": "codex"}))
        .await
        .unwrap_err();
    assert_eq!(code(&error), codes::BAD_REQUEST);
    assert_eq!(a.hub.orchestrator_thread().as_deref(), Some(old.as_str()));
    let r = ca.call("orchestrator.reset", Value::Null).await.unwrap();
    assert!(r["handoff"].is_null());
    assert_ne!(r["thread"], old);
    assert_eq!(r["previous"]["thread"], old);
    let fresh_rt = a.orch().await;
    assert!(fresh_rt.cfg.resume_session.is_none());
    assert!(!a.hub.orchestrator_token_is(&old_token));
    assert!(b.hub.orchestrator_token_is(&b_rt.token()));
    assert_eq!(b.hub.orchestrator_thread().as_deref(), Some(b_old.as_str()));
    assert_eq!(b.hub.info()["control_head"], b_head);
    assert_ne!(
        ca.call("hub.info", Value::Null).await.unwrap()["db"],
        cb.call("hub.info", Value::Null).await.unwrap()["db"]
    );
    drop(b_sub);
    drop((old_rt, fresh_rt, b_rt));
    a.stop().await;
    b.stop().await;
}

/// A suggested message goes to the orchestrator only: routed (no `to`, or `to: null`), an
/// `@thread yes` in it would answer as the user, so it is refused and nothing is sent.
#[tokio::test]
async fn a_suggested_say_must_go_to_the_orchestrator() {
    let mut th = start().await;
    let mut c = th.client("tui").await;
    for params in [
        json!({"message": "@api yes", "suggested": true}),
        json!({"message": "@api yes", "suggested": true, "to": null}),
    ] {
        let e = c.call("say", params.clone()).await.unwrap_err();
        assert_eq!(code(&e), "bad_request", "{params}: {e:#}");
    }
    let r = c
        .call(
            "say",
            json!({"message": "and the docs", "suggested": true, "to": "orchestrator"}),
        )
        .await
        .unwrap();
    assert_eq!(r["action"], "delegate", "{r}");
    th.stop().await;
}
