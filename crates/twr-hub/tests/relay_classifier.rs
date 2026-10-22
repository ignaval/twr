//! The relay rule end to end over the socket: the harness never answers a permission or
//! question without an explicit user answer. `say` reply routing reads the user's message with
//! the strict whole-message grammar (`twr_decisions::code::route::explicit_answer`); a
//! reply-shaped message that is not a strict answer ("yes?", "If tests pass, approve.") answers
//! nothing there: `say` abstains (the hub asks the user, or hands it to the orchestrator). The
//! orchestrator reads the message freely; the hub checks provenance (a user turn, the user's
//! latest message, the item waiting and seen when it arrived) and then reads the message itself
//! (`twr_decisions::code::command`): when the whole message is one short command
//! ("allow both", "approve atlas, deny the webapp one", "set api to plan mode", "yes" with one
//! item waiting) that covers the call, it is applied; everything else (a second sentence, a
//! question, courtesy, an option label or number, a typo of a name) goes on a confirm card
//! only the user answers (`hub::proposal`).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::timeout;
use twr_adapters::fake::FakeAdapter;
use twr_core::adapter::{
    Adapter, AdapterCmd, AdapterEnd, AdapterHandle, AdapterInfo, SpawnConfig, Tier,
};
use twr_core::client::Client;
use twr_core::config::Config;
use twr_core::event::*;
use twr_core::paths::Paths;
use twr_core::proto::{codes, RpcError};
use twr_hub::{server, Hub, HubOptions};

const WAIT: Duration = Duration::from_secs(10);

/// Reply-shaped messages that are not strict answers.
const NOT_ANSWERS: &[&str] = &[
    "yes?",
    "approve it?",
    "If tests pass, approve.",
    "approve if tests pass",
    "yes, but not the rm",
    "'yes'",
    "yes. no.",
    "sure, what is it?",
];

/// One orchestrator runtime session handed to the test.
struct OrchRt {
    cfg: SpawnConfig,
    end: AdapterEnd,
    first: Option<(String, u64)>,
}

impl OrchRt {
    fn token(&self) -> String {
        let path = self.cfg.mcp_config_file.as_ref().expect("an MCP config");
        let mcp: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        mcp["mcpServers"]["twr"]["env"]["TWR_ORCH_TOKEN"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// The next turn the hub gives this session: `(prompt, turn)`.
    async fn next_turn(&mut self) -> (String, u64) {
        if let Some(f) = self.first.take() {
            return f;
        }
        timeout(WAIT, async {
            loop {
                match self.end.cmds.recv().await {
                    Some(AdapterCmd::Send { text, turn }) => return (text, turn),
                    Some(_) => continue,
                    None => panic!("the hub dropped the orchestrator runtime"),
                }
            }
        })
        .await
        .expect("timed out waiting for an orchestrator turn")
    }

    /// The next turn carrying the user's `message`, ending hub notice turns on the way.
    async fn user_turn(&mut self, message: &str) -> u64 {
        loop {
            let (text, turn) = self.next_turn().await;
            if text.starts_with("[twr notice]") {
                self.end_turn(turn);
                continue;
            }
            assert!(
                text.ends_with(message),
                "{text:?} does not end in {message:?}"
            );
            return turn;
        }
    }

    fn end_turn(&self, turn: u64) {
        self.end.emit(
            EventBody::AgentText(AgentText {
                text: "ok".into(),
                is_final: true,
                ..Default::default()
            }),
            Some(turn),
        );
        self.end.emit(
            EventBody::TurnEnded(TurnEnded {
                result: TurnResult::Completed,
                ..Default::default()
            }),
            Some(turn),
        );
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

    fn spawn(&self, cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let (h, end) = AdapterHandle::pair();
        end.emit(
            EventBody::ThreadStarted(SessionPayload {
                session_id: Some("s-orch".into()),
                ..Default::default()
            }),
            None,
        );
        let first = prompt.map(|p| (p, cfg.prompt_turn.unwrap_or(1)));
        let _ = self.0.send(OrchRt { cfg, end, first });
        Ok(h)
    }
}

/// Runtime `probe`: started with a prompt, it ends that turn at once and then raises
/// `perms` permissions between turns (`p1`, `p2`, ...); started without one (a restart) it
/// raises nothing. It records, per thread, every command it gets, in order: `respond <id>
/// <decision>`, `send`, `stop`.
#[derive(Default)]
struct Probe {
    perms: AtomicUsize,
    log: Mutex<Vec<(String, String)>>,
}

impl Probe {
    /// The commands thread `thread` got, in order.
    fn commands(&self, thread: &str) -> Vec<String> {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, _)| t == thread)
            .map(|(_, c)| c.clone())
            .collect()
    }
}

struct ProbeAdapter(Arc<Probe>);

impl Adapter for ProbeAdapter {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "probe",
            tier: Tier::Headless,
            description: "records the commands it gets",
        }
    }

    fn spawn(&self, cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let (h, mut end) = AdapterHandle::pair();
        let probe = Arc::clone(&self.0);
        let perms = if prompt.is_some() {
            probe.perms.load(Ordering::SeqCst)
        } else {
            0
        };
        tokio::spawn(async move {
            end.emit(
                EventBody::ThreadStarted(SessionPayload {
                    session_id: Some(cfg.resume_session.clone().unwrap_or("s-probe".into())),
                    ..Default::default()
                }),
                None,
            );
            let ended = || {
                EventBody::TurnEnded(TurnEnded {
                    result: TurnResult::Completed,
                    ..Default::default()
                })
            };
            if prompt.is_some() {
                end.emit(ended(), Some(cfg.prompt_turn.unwrap_or(1)));
            }
            for k in 1..=perms {
                end.emit(
                    EventBody::AttentionRaised(AttentionRaised {
                        attn_id: format!("p{k}"),
                        reason: AttentionReason::Permission,
                        summary: format!("p{k}?"),
                        options: vec!["allow".into(), "deny".into()],
                        ..Default::default()
                    }),
                    None,
                );
            }
            let record = |c: String| probe.log.lock().unwrap().push((cfg.thread.clone(), c));
            while let Some(cmd) = end.cmds.recv().await {
                match cmd {
                    AdapterCmd::Respond { attn_id, answer } => {
                        record(format!("respond {attn_id} {}", answer.decision));
                    }
                    AdapterCmd::Send { turn, .. } => {
                        record("send".into());
                        end.emit(ended(), Some(turn));
                    }
                    AdapterCmd::Interrupt => record("interrupt".into()),
                    AdapterCmd::Compact { .. } => record("compact".into()),
                    AdapterCmd::Stop => {
                        record("stop".into());
                        break;
                    }
                }
            }
            end.exited(Some(0), "");
        });
        Ok(h)
    }
}

struct TestHub {
    dir: tempfile::TempDir,
    socket: std::path::PathBuf,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
    hub: Hub,
    spawns: mpsc::UnboundedReceiver<OrchRt>,
    probe: Arc<Probe>,
}

/// A hub on a socket with workers `api` and `web`; with `orchestrator`, `claude-code` is the
/// test's orchestrator runtime, otherwise none is available.
async fn start(orchestrator: bool) -> TestHub {
    let th = open(tempfile::tempdir().unwrap(), orchestrator).await;
    let mut c = th.client("setup").await;
    for name in ["api", "web"] {
        c.call(
            "thread.new",
            json!({"project": th.dir.path(), "runtime": "fake", "name": name}),
        )
        .await
        .unwrap();
    }
    th
}

/// A hub on a socket over the data in `dir` (see [`start`]).
async fn open(dir: tempfile::TempDir, orchestrator: bool) -> TestHub {
    let paths = Paths::with_home(dir.path());
    let listener = server::bind(&paths.socket).await.unwrap();
    let mut config = Config {
        default_runtime: "fake".into(),
        ..Default::default()
    };
    config.orchestrator.runtime = Some(if orchestrator { "claude-code" } else { "nope" }.into());
    config.orchestrator.cwd = Some(dir.path().to_path_buf());
    let mut opts = HubOptions::new(paths.db_file(), config);
    let (tx, spawns) = mpsc::unbounded_channel();
    let orch: Arc<dyn Adapter> = Arc::new(OrchAdapter(tx));
    let probe = Arc::new(Probe::default());
    let probe_rt: Arc<dyn Adapter> = Arc::new(ProbeAdapter(Arc::clone(&probe)));
    opts.adapters = Arc::new(move |name: &str| -> Option<Arc<dyn Adapter>> {
        match name {
            // `acp`: a generic ACP agent's thread (any mode vocabulary), run by the fake.
            "fake" | "acp" => Some(Arc::new(FakeAdapter::new())),
            "claude-code" => Some(Arc::clone(&orch)),
            "probe" => Some(Arc::clone(&probe_rt)),
            _ => None,
        }
    });
    opts.socket = Some(paths.socket.clone());
    // The orchestrator runtime runs in this process: there is no process for its MCP server
    // to descend from, so the token is taken from this process's connections.
    opts.orchestrator_token_unverified = true;
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
        probe,
    }
}

impl TestHub {
    async fn client(&self, name: &str) -> Client {
        let mut c = Client::connect(&self.socket).await.unwrap();
        c.hello(json!({ "client": name })).await.unwrap();
        c
    }

    async fn orch(&mut self) -> OrchRt {
        timeout(WAIT, self.spawns.recv())
            .await
            .expect("timed out waiting for the orchestrator to spawn")
            .unwrap()
    }

    /// Raises an open permission (or question) on `thread`, as the hub (no runtime to forward
    /// the answer to).
    fn raise(&self, thread: &str, attn_id: &str, reason: AttentionReason, options: &[&str]) {
        let id = self.hub.resolve(thread).unwrap();
        self.hub
            .append(
                &id,
                None,
                Source::Hub,
                EventBody::AttentionRaised(AttentionRaised {
                    attn_id: attn_id.into(),
                    reason,
                    summary: format!("{attn_id}?"),
                    options: options.iter().map(|o| o.to_string()).collect(),
                    ..Default::default()
                }),
                None,
            )
            .unwrap();
    }

    fn permission(&self, thread: &str, attn_id: &str) {
        self.raise(
            thread,
            attn_id,
            AttentionReason::Permission,
            &["allow", "deny"],
        );
    }

    fn question(&self, thread: &str, attn_id: &str) {
        self.raise(
            thread,
            attn_id,
            AttentionReason::Question,
            &["Postgres", "SQLite"],
        );
    }

    fn is_open(&self, attn_id: &str) -> bool {
        self.hub
            .attention_list(false)
            .iter()
            .any(|i| i["attn_id"] == attn_id)
    }

    /// Resolves `attn_id` (its current revision) as the user, tidying up.
    fn user_resolves(&self, attn_id: &str) {
        let revision = self
            .hub
            .attention_list(true)
            .iter()
            .find(|i| i["attn_id"] == attn_id)
            .and_then(|i| i["revision"].as_u64());
        self.hub
            .respond_at(
                attn_id,
                twr_core::adapter::Answer::deny(),
                revision,
                &twr_hub::Caller::user("test"),
            )
            .unwrap();
    }

    async fn stop(self) {
        self.shutdown().await;
    }

    /// Shuts the hub down; returns its data directory.
    async fn shutdown(self) -> tempfile::TempDir {
        let mut c = self.client("end").await;
        c.call("hub.shutdown", Value::Null).await.unwrap();
        self.task.await.unwrap().unwrap();
        self.dir
    }
}

fn code(e: &anyhow::Error) -> String {
    e.downcast_ref::<RpcError>()
        .map(|r| r.code.clone())
        .unwrap_or_else(|| format!("{e:#}"))
}

/// `say` as the TUI sends it: with the control head as what the user had seen.
async fn say(c: &mut Client, message: &str, focused: &str) -> Value {
    let head = c
        .call("control.read", json!({"since": 0, "limit": 1}))
        .await
        .unwrap()["head"]
        .clone();
    c.call(
        "say",
        json!({"message": message, "focused": focused, "seen_gseq": head}),
    )
    .await
    .unwrap()
}

/// The control stream's head.
async fn control_head(c: &mut Client) -> u64 {
    c.call("control.read", json!({"since": 0, "limit": 1}))
        .await
        .unwrap()["head"]
        .as_u64()
        .unwrap()
}

/// `say` without `seen_gseq` (as `twr say` sends it).
async fn say_unseen(c: &mut Client, message: &str, focused: &str) -> Value {
    c.call("say", json!({"message": message, "focused": focused}))
        .await
        .unwrap()
}

#[tokio::test]
async fn say_answers_an_item_only_with_a_strict_answer() {
    let th = start(false).await;
    let mut c = th.client("tui").await;
    let api = th.hub.resolve("api").unwrap();

    // Reply-shaped but not strict: nothing is answered and nothing is sent to the blocked
    // thread; without an orchestrator the hub asks the user.
    th.permission("api", "p0");
    for msg in NOT_ANSWERS {
        for (message, focused) in [(msg.to_string(), "api"), (format!("@api {msg}"), "web")] {
            let r = say(&mut c, &message, focused).await;
            assert_eq!(r["action"], "ask", "{message:?}: {r}");
            assert!(th.is_open("p0"), "{message:?}");
        }
    }
    assert_eq!(th.hub.thread_view(&api).unwrap().turns, 0);
    th.user_resolves("p0");

    // Strict answers resolve the focused (or addressed) thread's item.
    let cases: &[(&str, &str, &str)] = &[
        ("ok thanks", "api", "allow"),
        ("@api yes please", "web", "allow"),
        ("2", "api", "deny"),
        ("option 2", "api", "deny"),
        ("deny.", "api", "deny"),
        ("no thanks", "api", "deny"),
    ];
    for (i, (message, focused, want)) in cases.iter().enumerate() {
        let attn = format!("p{}", i + 1);
        th.permission("api", &attn);
        // Not knowing what the user had seen, `say` answers nothing (and sends nothing): it
        // names the pending item instead.
        let r = say_unseen(&mut c, message, focused).await;
        assert_eq!(r["action"], "pending", "{message:?}: {r}");
        assert_eq!(r["items"][0]["attn_id"], attn.as_str(), "{message:?}: {r}");
        assert!(th.is_open(&attn), "{message:?}");
        let r = say(&mut c, message, focused).await;
        assert_eq!(r["action"], "respond", "{message:?}: {r}");
        assert_eq!(r["thread"], api.as_str(), "{message:?}");
        assert_eq!(r["attn_id"], attn.as_str(), "{message:?}");
        assert_eq!(r["answer"], *want, "{message:?}");
        assert!(!th.is_open(&attn), "{message:?}");
    }
    for (i, message) in ["SQLite", "sqlite.", "2"].iter().enumerate() {
        let attn = format!("q{i}");
        th.question("api", &attn);
        let r = say(&mut c, message, "api").await;
        assert_eq!(r["action"], "respond", "{message:?}: {r}");
        assert_eq!(r["answer"], "SQLite", "{message:?}");
    }
    // Hedged answers to a question do not pick an option either.
    th.question("api", "q9");
    for message in ["sqlite?", "sqlite, if it is installed", "2?"] {
        let r = say(&mut c, message, "api").await;
        assert_eq!(r["action"], "ask", "{message:?}: {r}");
    }
    assert!(th.is_open("q9"));
    assert_eq!(th.hub.thread_view(&api).unwrap().turns, 0);
    th.stop().await;
}

/// Short commands typed to the orchestrator: applied canonical when the orchestrator relays
/// them as given; another answer for the same item is a proposal, never applied. An option
/// number for a question is no command: the orchestrator's answer goes on the card.
#[tokio::test]
async fn the_orchestrator_relays_only_what_the_user_said() {
    let mut th = start(true).await;
    let mut user = th.client("tui").await;
    let (mut rt, mut orch) = orchestrator_up(&mut th, &mut user).await;
    let cases: &[(&str, &str)] = &[
        ("ok api", "allow"),
        ("yes @api", "allow"),
        ("allow api.", "allow"),
        ("deny api.", "deny"),
        ("reject api", "deny"),
        ("block @api!", "deny"),
    ];
    for (i, (msg, want)) in cases.iter().enumerate() {
        let attn = format!("s{i}");
        th.permission("api", &attn);
        let turn = user_says(&mut user, &mut rt, msg).await;
        let other = if *want == "allow" { "deny" } else { "allow" };
        let r = orch
            .call("attention.respond", respond_json(&attn, other))
            .await
            .unwrap();
        assert_eq!(r["status"], "needs_confirmation", "{msg:?}: {r}");
        assert!(th.is_open(&attn), "{msg:?}");
        rt.end_turn(turn);
        // The proposal went on a card; the user cancels it.
        let card = wait_card(&th).await;
        cancel_card(&th, &card);
        let turn = user_says(&mut user, &mut rt, msg).await;
        let ev = orch
            .call("attention.respond", respond_json(&attn, want))
            .await
            .unwrap();
        assert_eq!(ev["source"], "orchestrator", "{msg:?}: {ev}");
        assert_eq!(ev["data"]["answer"]["decision"], *want, "{msg:?}: {ev}");
        assert!(!th.is_open(&attn), "{msg:?}");
        rt.end_turn(turn);
    }
    for (i, msg) in ["option 2", "#2", "2"].iter().enumerate() {
        let attn = format!("q{i}");
        th.question("api", &attn);
        let turn = user_says(&mut user, &mut rt, msg).await;
        let r = proposes(
            orch.call("attention.respond", respond_json(&attn, "sqlite"))
                .await,
        );
        assert_eq!(r["answer"], "SQLite", "{msg:?}: {r}");
        rt.end_turn(turn);
        let card = wait_card(&th).await;
        cancel_card(&th, &card);
        assert!(th.is_open(&attn), "{msg:?}");
        th.user_resolves(&attn);
    }
    drop(rt);
    th.stop().await;
}

/// `twr say "yes"` (no `seen_gseq`) while a permission waits: not routed. No hub question is
/// raised, no decision logged, nothing sent; the result lists the open blocking items with
/// their revisions (for `twr respond <id>@<rev>`). A message that is no answer attempt still
/// routes, and the TUI (with `seen_gseq`) still answers.
#[tokio::test]
async fn an_unseen_answer_is_not_routed_but_names_the_pending_items() {
    let th = start(false).await;
    let mut c = th.client("cli").await;
    let api = th.hub.resolve("api").unwrap();
    th.permission("api", "p1");
    th.question("web", "q1");
    let before = control_head(&mut c).await;
    for message in ["yes", "yes?", "allow", "1", "ok, if the tests pass"] {
        let r = c.call("say", json!({ "message": message })).await.unwrap();
        assert_eq!(r["action"], "pending", "{message:?}: {r}");
        let items = r["items"].as_array().unwrap();
        let ids: Vec<&str> = items
            .iter()
            .map(|i| i["attn_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["p1", "q1"], "{message:?}: {r}");
        assert_eq!(items[0]["thread_name"], "api");
        assert_eq!(items[0]["reason"], "permission");
        assert!(items[0]["revision"].as_u64().unwrap() > 0, "{r}");
        assert_eq!(items[0]["options"], json!(["allow", "deny"]));
    }
    // Addressed: only that thread's item.
    let r = c
        .call("say", json!({ "message": "@web SQLite" }))
        .await
        .unwrap();
    assert_eq!(r["action"], "pending", "{r}");
    assert_eq!(r["items"].as_array().unwrap().len(), 1, "{r}");
    assert_eq!(r["items"][0]["attn_id"], "q1");
    // Nothing was recorded: no hub question, no decision, no turn.
    assert_eq!(control_head(&mut c).await, before, "say recorded something");
    assert!(th
        .hub
        .attention_list(true)
        .iter()
        .all(|i| i["thread"] != HUB_THREAD));
    assert_eq!(th.hub.thread_view(&api).unwrap().turns, 0);
    assert!(th.is_open("p1") && th.is_open("q1"));
    // Not an answer attempt: routed as before (addressed text goes to the thread).
    let r = c
        .call(
            "say",
            json!({ "message": "@api refactor the session store to use sqlite" }),
        )
        .await
        .unwrap();
    assert_eq!(r["action"], "send", "{r}");
    // The TUI (what was on screen) still answers.
    let r = say(&mut c, "yes", "api").await;
    assert_eq!(r["action"], "respond", "{r}");
    assert_eq!(r["attn_id"], "p1");
    th.stop().await;
}

/// `say {to: "orchestrator"}` as the TUI's orchestrator conversation sends it (with what was
/// on screen).
async fn say_to_orch(c: &mut Client, message: &str) -> Value {
    let head = control_head(c).await;
    c.call(
        "say",
        json!({"message": message, "to": "orchestrator", "seen_gseq": head}),
    )
    .await
    .unwrap()
}

/// The orchestrator's own connection (its MCP server's).
async fn orch_client(th: &TestHub, rt: &OrchRt) -> Client {
    let mut orch = Client::connect(&th.socket).await.unwrap();
    orch.hello(json!({"client": "orchestrator", "token": rt.token()}))
        .await
        .unwrap();
    orch
}

/// Starts the orchestrator with a first message and returns its runtime and connection.
async fn orchestrator_up(th: &mut TestHub, user: &mut Client) -> (OrchRt, Client) {
    let r = say_to_orch(user, "hello").await;
    assert_eq!(r["action"], "delegate", "{r}");
    let mut rt = th.orch().await;
    let t = rt.user_turn("hello").await;
    rt.end_turn(t);
    let orch = orch_client(th, &rt).await;
    (rt, orch)
}

fn forbidden(r: anyhow::Result<Value>, why: &str) {
    let e = r.expect_err("the hub should refuse");
    assert_eq!(code(&e), codes::RELAY_FORBIDDEN, "{e:#}");
    assert!(format!("{e:#}").contains(why), "{why:?} not in: {e:#}");
}

/// `say {to: "orchestrator"}` skips the route ladder: even with a single worker thread the
/// message goes to the orchestrator, and an answer in it is relayed like any delegated one.
#[tokio::test]
async fn say_to_the_orchestrator_skips_routing() {
    let mut th = start(true).await;
    let mut user = th.client("tui").await;
    user.call("thread.stop", json!({"thread": "web"}))
        .await
        .unwrap();
    // Without `to`, the only worker gets it.
    let r = say(&mut user, "look at the flaky test", "").await;
    assert_eq!(r["action"], "send", "{r}");
    assert_eq!(r["thread_name"], "api");
    // With it, the orchestrator does.
    let msg = "look at the flaky test";
    let r = say_to_orch(&mut user, msg).await;
    assert_eq!(r["action"], "delegate", "{r}");
    assert_eq!(r["thread_name"], "orchestrator");
    assert!(r["turn"].as_u64().is_some(), "{r}");
    assert_eq!(r["decision_ids"].as_array().unwrap().len(), 1, "{r}");
    let mut rt = th.orch().await;
    let turn = rt.user_turn(msg).await;
    rt.end_turn(turn);
    // A complete command is checked and applied by the hub without another model turn.
    th.permission("api", "a1");
    let r = say_to_orch(&mut user, "allow api").await;
    assert_eq!(r["action"], "command", "{r}");
    assert!(!th.is_open("a1"));
    assert_eq!(th.hub.thread_view("orchestrator").unwrap().turns, turn);
    let events = th.hub.thread_events("api", 0, 1000).unwrap().1;
    assert!(events
        .iter()
        .any(|e| e.kind() == "attention.resolved" && e.source == Source::User("tui".into())));

    // Nothing on screen (no `seen_gseq`): an answer attempt is refused, the items listed.
    th.permission("api", "a2");
    let r = user
        .call("say", json!({"message": "allow all", "to": "orchestrator"}))
        .await
        .unwrap();
    assert_eq!(r["action"], "pending", "{r}");
    assert_eq!(r["items"][0]["attn_id"], "a2");
    // Only the orchestrator is a target.
    let e = user
        .call("say", json!({"message": "hi", "to": "api"}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::BAD_REQUEST, "{e:#}");
    drop(rt);
    th.stop().await;
}

/// No orchestrator available: `say {to: "orchestrator"}` records nothing and says so.
#[tokio::test]
async fn say_to_a_missing_orchestrator_is_a_conflict() {
    let th = start(false).await;
    let mut user = th.client("tui").await;
    let before = control_head(&mut user).await;
    let head = before;
    let e = user
        .call(
            "say",
            json!({"message": "hi", "to": "orchestrator", "seen_gseq": head}),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::CONFLICT, "{e:#}");
    assert!(format!("{e:#}").contains("no orchestrator"), "{e:#}");
    assert_eq!(control_head(&mut user).await, before);
    th.stop().await;
}

/// Creates worker `name` (fake runtime, `mode`).
async fn worker(th: &TestHub, user: &mut Client, name: &str, mode: &str) {
    user.call(
        "thread.new",
        json!({"project": th.dir.path(), "runtime": "fake", "name": name, "mode": mode}),
    )
    .await
    .unwrap();
}

/// Creates worker `name` (fake runtime, `mode`) in its own project folder `project`.
async fn project_worker(th: &TestHub, user: &mut Client, project: &str, name: &str, mode: &str) {
    let dir = th.dir.path().join(project);
    std::fs::create_dir_all(&dir).unwrap();
    user.call(
        "thread.new",
        json!({"project": dir, "runtime": "fake", "name": name, "mode": mode}),
    )
    .await
    .unwrap();
}

/// The user says `msg` to the orchestrator; returns the orchestrator turn carrying it.
async fn user_says(user: &mut Client, rt: &mut OrchRt, msg: &str) -> u64 {
    // Exercise the model relay guard with a verbatim thread.send. Exact conversation
    // commands have their own hub fast-path tests; routing must not change this corpus.
    user.call("thread.send", json!({"thread":"orch", "prompt":msg}))
        .await
        .unwrap();
    rt.user_turn(msg).await
}

/// The orchestrator's `attention.respond`.
fn respond_json(attn: &str, answer: &str) -> Value {
    json!({"attn_id": attn, "answer": answer})
}

/// A `thread.set_mode` (the user's or the orchestrator's).
fn set_json(thread: &str, mode: &str) -> Value {
    json!({"thread": thread, "mode": mode})
}

fn mode_of(th: &TestHub, name: &str) -> String {
    let id = th.hub.resolve(name).unwrap();
    th.hub.thread_view(&id).unwrap().mode
}
// ------------------------------------------------------------------------------------------
// Short commands and the confirm card
// ------------------------------------------------------------------------------------------

/// The user's session: api and web stopped; workers `orbit-webapp-1` (project
/// `orbit-webapp`) and `atlas-1` (project `atlas`) in manual mode; the orchestrator up.
async fn session(th: &mut TestHub, user: &mut Client) -> (OrchRt, Client) {
    for name in ["api", "web"] {
        user.call("thread.stop", json!({"thread": name}))
            .await
            .unwrap();
    }
    project_worker(th, user, "orbit-webapp", "orbit-webapp-1", "manual").await;
    project_worker(th, user, "atlas", "atlas-1", "manual").await;
    orchestrator_up(th, user).await
}

/// The orchestrator answers `attn` with `answer`: applied as its own.
async fn relays(orch: &mut Client, attn: &str, answer: &str) {
    let ev = orch
        .call("attention.respond", respond_json(attn, answer))
        .await
        .unwrap_or_else(|e| panic!("{attn} {answer}: {e:#}"));
    assert_eq!(ev["source"], "orchestrator", "{ev}");
    assert_eq!(ev["data"]["answer"]["decision"], answer, "{ev}");
}

/// The orchestrator sets `name` to `mode`: applied as its change.
async fn sets_mode(th: &TestHub, orch: &mut Client, name: &str, mode: &str) {
    let r = orch
        .call("thread.set_mode", set_json(name, mode))
        .await
        .unwrap_or_else(|e| panic!("{name} {mode}: {e:#}"));
    assert_eq!(r["changed"], true, "{r}");
    assert_eq!(mode_of(th, name), mode);
    let id = th.hub.resolve(name).unwrap();
    let (_, evs) = th.hub.thread_events(&id, 0, 1000).unwrap();
    let changed = evs
        .iter()
        .rev()
        .find(|e| e.kind() == "thread.mode_changed")
        .unwrap();
    assert_eq!(changed.source, Source::Orchestrator);
    let EventBody::ThreadModeChanged(p) = &changed.body else {
        panic!()
    };
    assert_eq!(p.by, ChangedBy::Orchestrator);
}

/// A call the hub recorded as a proposal.
fn proposes(r: anyhow::Result<Value>) -> Value {
    let r = r.expect("a proposal, not an error");
    assert_eq!(r["status"], "needs_confirmation", "{r}");
    assert!(
        r["message"]
            .as_str()
            .unwrap()
            .contains("I've put that on a card for you to confirm."),
        "{r}"
    );
    r
}

/// The open confirm cards (attention items), in stack order.
fn cards(th: &TestHub) -> Vec<Value> {
    th.hub
        .attention_list(false)
        .into_iter()
        .filter(|i| i["proposal"] == true)
        .collect()
}

/// The confirm card the notifier raises once the orchestrator's turn ended.
async fn wait_card(th: &TestHub) -> Value {
    timeout(WAIT, async {
        loop {
            if let Some(c) = cards(th).into_iter().next() {
                return c;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("no confirm card was raised")
}

/// No card shows up, though the notifier had the time to raise one.
async fn no_card(th: &TestHub) {
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(cards(th).is_empty(), "{:?}", cards(th));
}

/// The user answers confirm card `card` with `answer` over the socket.
async fn answer_card(user: &mut Client, card: &Value, answer: &str) -> Value {
    user.call(
        "attention.respond",
        json!({"attn_id": card["attn_id"], "revision": card["revision"], "answer": answer}),
    )
    .await
    .unwrap()
}

/// The user cancels confirm card `card` (a TUI's 2).
fn cancel_card(th: &TestHub, card: &Value) {
    th.hub
        .respond_option_at(
            card["attn_id"].as_str().unwrap(),
            1,
            None,
            card["revision"].as_u64(),
            &twr_hub::Caller::user("tui"),
        )
        .unwrap();
}

/// The control events so far.
fn control(th: &TestHub) -> Vec<Event> {
    let (items, _) = th.hub.control_read(0, 100_000).unwrap();
    items.into_iter().map(|(_, e)| e).collect()
}

/// `ext.twr.proposal` of confirm card `attn`.
fn card_proposal(th: &TestHub, attn: &str) -> Value {
    control(th)
        .into_iter()
        .find_map(|e| match e.body {
            EventBody::AttentionRaised(p) if p.attn_id == attn => p
                .ext
                .and_then(|x| x.get("twr").and_then(|t| t.get("proposal")).cloned()),
            _ => None,
        })
        .expect("the card's proposal")
}

/// The `attention.resolved` of `attn`, with its source.
fn resolution(th: &TestHub, attn: &str) -> (Source, AttentionResolved) {
    control(th)
        .into_iter()
        .find_map(|e| match e.body {
            EventBody::AttentionResolved(p) if p.attn_id == attn => Some((e.source, p)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{attn} was not resolved"))
}

/// `ext.twr.<key>` of `ext`.
fn twr_key(ext: &Option<JsonMap>, key: &str) -> Value {
    ext.as_ref()
        .and_then(|x| x.get("twr"))
        .and_then(|t| t.get(key))
        .cloned()
        .unwrap_or(Value::Null)
}

/// The durable conversation update for a confirmation outcome, without a model turn.
fn card_notice(th: &TestHub, card: &Value) -> String {
    control(th)
        .into_iter()
        .rev()
        .find_map(|e| match e.body {
            EventBody::ConversationUpdate(p) if twr_key(&p.ext, "attn_id") == card["attn_id"] => {
                Some(p.text)
            }
            _ => None,
        })
        .expect("the confirmation outcome is in the conversation")
}

/// The short commands: each action the orchestrator relays from them is applied at once, and
/// no card is raised.
#[tokio::test]
async fn short_commands_apply_directly() {
    let mut th = start(true).await;
    let mut user = th.client("tui").await;
    let (mut rt, mut orch) = session(&mut th, &mut user).await;
    type Case<'a> = (&'a str, &'a [(&'a str, &'a str)], &'a [(&'a str, &'a str)]);
    let both_auto: &[(&str, &str)] = &[("orbit-webapp-1", "auto"), ("atlas-1", "auto")];
    let both: &[(&str, &str)] = &[("w", "allow"), ("d", "allow")];
    let cases: &[Case] = &[
        ("allow both", both, &[]),
        ("approve all", both, &[]),
        ("allow on the webapp", &[("w", "allow")], &[]),
        (
            "approve atlas, deny the webapp one",
            &[("d", "allow"), ("w", "deny")],
            &[],
        ),
        ("dale a ambos", both, &[]),
        (
            "Allow both, and set them both to audo mode",
            both,
            both_auto,
        ),
        ("set both to audo mode", &[], both_auto),
    ];
    for (i, (msg, answers, modes)) in cases.iter().enumerate() {
        let (w, d) = (format!("w{i}"), format!("d{i}"));
        th.permission("orbit-webapp-1", &w);
        th.permission("atlas-1", &d);
        let turn = user_says(&mut user, &mut rt, msg).await;
        for (item, answer) in *answers {
            let attn = if *item == "w" { &w } else { &d };
            relays(&mut orch, attn, answer).await;
            assert!(!th.is_open(attn), "{msg:?}");
        }
        for (name, mode) in *modes {
            sets_mode(&th, &mut orch, name, mode).await;
        }
        rt.end_turn(turn);
        no_card(&th).await;
        for attn in [&w, &d] {
            if th.is_open(attn) {
                th.user_resolves(attn);
            }
        }
        for name in ["orbit-webapp-1", "atlas-1"] {
            user.call("thread.set_mode", set_json(name, "manual"))
                .await
                .unwrap();
        }
    }
    drop(rt);
    th.stop().await;

    // Running workers api and web.
    let mut th = start(true).await;
    let mut user = th.client("tui").await;
    for name in ["api", "web"] {
        user.call("thread.set_mode", set_json(name, "manual"))
            .await
            .unwrap();
    }
    let (mut rt, mut orch) = orchestrator_up(&mut th, &mut user).await;
    th.permission("api", "a1");
    th.permission("web", "w1");
    let turn = user_says(&mut user, &mut rt, "allow api and web").await;
    relays(&mut orch, "a1", "allow").await;
    relays(&mut orch, "w1", "allow").await;
    rt.end_turn(turn);
    let turn = user_says(&mut user, &mut rt, "set api to plan mode").await;
    sets_mode(&th, &mut orch, "api", "plan").await;
    rt.end_turn(turn);
    no_card(&th).await;
    // A lone answer, with exactly one item waiting, names nothing and may answer the
    // orchestrator's own question: it goes on the card, which names the item.
    for (i, msg) in ["allow it", "yes", "yep", "no"].iter().enumerate() {
        let attn = format!("l{i}");
        th.permission("api", &attn);
        let turn = user_says(&mut user, &mut rt, msg).await;
        let answer = if *msg == "no" { "deny" } else { "allow" };
        proposes(
            orch.call("attention.respond", respond_json(&attn, answer))
                .await,
        );
        assert!(th.is_open(&attn), "{msg:?}");
        rt.end_turn(turn);
        let card = wait_card(&th).await;
        cancel_card(&th, &card);
    }
    drop(rt);
    th.stop().await;
}

/// A message that is no short command: every call that passes provenance is a proposal,
/// nothing is applied, and the turn's proposals go on one card once the turn ends.
#[tokio::test]
async fn unusual_wording_goes_on_one_confirm_card() {
    let mut th = start(true).await;
    let mut user = th.client("tui").await;
    let (mut rt, mut orch) = session(&mut th, &mut user).await;
    th.permission("orbit-webapp-1", "w1");
    th.permission("atlas-1", "d1");
    let msg = "please approve the webapp and atlas, then make both of them auto mode, cheers";
    let turn = user_says(&mut user, &mut rt, msg).await;
    for attn in ["w1", "d1"] {
        let r = proposes(
            orch.call("attention.respond", respond_json(attn, "allow"))
                .await,
        );
        assert_eq!(r["answer"], "allow", "{r}");
    }
    for name in ["orbit-webapp-1", "atlas-1"] {
        let r = proposes(orch.call("thread.set_mode", set_json(name, "auto")).await);
        assert_eq!(r["thread_name"], name, "{r}");
    }
    // Once per item and thread per turn, proposals included.
    forbidden(
        orch.call("attention.respond", respond_json("w1", "allow"))
            .await,
        "already answered (or proposed an answer to) w1",
    );
    forbidden(
        orch.call("thread.set_mode", set_json("atlas-1", "plan"))
            .await,
        "already changed (or proposed a change to)",
    );
    // Nothing applied, and no card while the turn runs.
    assert!(th.is_open("w1") && th.is_open("d1"));
    assert_eq!(mode_of(&th, "atlas-1"), "manual");
    assert!(cards(&th).is_empty());
    rt.end_turn(turn);
    let card = wait_card(&th).await;
    assert_eq!(cards(&th).len(), 1);
    assert_eq!(card["thread"], HUB_THREAD);
    assert_eq!(card["reason"], "question");
    assert_eq!(card["options"], json!(["Confirm", "Cancel"]));
    assert_eq!(
        card["summary"],
        "From your message: allow orbit-webapp-1 (w1?) · allow atlas-1 (d1?) · set \
         orbit-webapp-1 → auto · set atlas-1 → auto"
    );
    let p = card_proposal(&th, card["attn_id"].as_str().unwrap());
    assert_eq!(p["turn"], turn, "{p}");
    let actions = p["actions"].as_array().unwrap();
    assert_eq!(actions.len(), 4, "{p}");
    assert_eq!(actions[0]["kind"], "answer");
    assert_eq!(actions[0]["attn_id"], "w1");
    assert!(actions[0]["revision"].as_u64().unwrap() > 0);
    assert_eq!(actions[2]["kind"], "mode");
    assert_eq!(actions[2]["mode"], "auto");
    cancel_card(&th, &card);
    assert!(th.is_open("w1") && th.is_open("d1"));
    drop(rt);
    th.stop().await;
}

/// The confirm path for the user's other phrasings: a typo that names nothing, a condition,
/// an exception, a negation. Each is a proposal; the card shows what the orchestrator
/// proposed (for "don't allow api", "allow api"), and the user cancels it.
#[tokio::test]
async fn phrasings_outside_the_vocabulary_are_proposals() {
    let mut th = start(true).await;
    let mut user = th.client("tui").await;
    let (mut rt, mut orch) = orchestrator_up(&mut th, &mut user).await;
    project_worker(&th, &mut user, "orbit-webapp", "orbit-webapp-1", "manual").await;
    for (i, (msg, thread, answer, shown)) in [
        (
            "allow teh webapp",
            "orbit-webapp-1",
            "allow",
            "allow orbit-webapp-1",
        ),
        (
            "allow api only if the tests pass",
            "api",
            "allow",
            "allow api",
        ),
        ("approve all, except web", "api", "allow", "allow api"),
        ("don't allow api", "api", "allow", "allow api"),
    ]
    .into_iter()
    .enumerate()
    {
        let attn = format!("x{i}");
        th.permission(thread, &attn);
        th.permission("web", &format!("y{i}"));
        let turn = user_says(&mut user, &mut rt, msg).await;
        proposes(
            orch.call("attention.respond", respond_json(&attn, answer))
                .await,
        );
        rt.end_turn(turn);
        let card = wait_card(&th).await;
        let summary = card["summary"].as_str().unwrap();
        assert!(summary.contains(shown), "{msg:?}: {summary}");
        cancel_card(&th, &card);
        assert!(th.is_open(&attn), "{msg:?}");
        th.user_resolves(&attn);
        th.user_resolves(&format!("y{i}"));
    }
    drop(rt);
    th.stop().await;
}

/// Confirm applies every action as the user's, marked with the card; the card's resolution
/// lists them, and the orchestrator hears of it.
#[tokio::test]
async fn confirm_applies_every_action_as_the_user() {
    let mut th = start(true).await;
    let mut user = th.client("tui").await;
    let (mut rt, mut orch) = session(&mut th, &mut user).await;
    th.permission("orbit-webapp-1", "w1");
    th.permission("atlas-1", "d1");
    let msg = "ok the webapp, nah atlas, and pop atlas into auto mode";
    let turn = user_says(&mut user, &mut rt, msg).await;
    proposes(
        orch.call("attention.respond", respond_json("w1", "allow"))
            .await,
    );
    proposes(
        orch.call("attention.respond", respond_json("d1", "deny"))
            .await,
    );
    proposes(
        orch.call("thread.set_mode", set_json("atlas-1", "auto"))
            .await,
    );
    rt.end_turn(turn);
    let card = wait_card(&th).await;
    let id = card["attn_id"].as_str().unwrap().to_string();
    let ev = answer_card(&mut user, &card, "Confirm").await;
    assert_eq!(ev["source"], "user:tui", "{ev}");
    assert_eq!(ev["data"]["answer"]["decision"], "Confirm", "{ev}");
    let done = &ev["data"]["ext"]["twr"]["confirmed"];
    assert_eq!(
        done["applied"],
        json!(["allow orbit-webapp-1", "deny atlas-1", "set atlas-1 → auto"]),
        "{ev}"
    );
    assert_eq!(done["skipped"], json!([]), "{ev}");
    assert!(cards(&th).is_empty());
    for (attn, want) in [("w1", "allow"), ("d1", "deny")] {
        let (source, p) = resolution(&th, attn);
        assert_eq!(source, Source::User("tui".into()), "{attn}");
        assert_eq!(p.by, ResolvedBy::User, "{attn}");
        assert_eq!(p.answer.unwrap().decision, want, "{attn}");
        assert_eq!(twr_key(&p.ext, "proposal"), json!(id), "{attn}");
    }
    assert_eq!(mode_of(&th, "atlas-1"), "auto");
    let atlas = th.hub.resolve("atlas-1").unwrap();
    let (_, evs) = th.hub.thread_events(&atlas, 0, 1000).unwrap();
    let changed = evs
        .iter()
        .rev()
        .find(|e| e.kind() == "thread.mode_changed")
        .unwrap();
    assert_eq!(changed.source, Source::User("tui".into()));
    let EventBody::ThreadModeChanged(p) = &changed.body else {
        panic!()
    };
    assert_eq!(p.by, ChangedBy::User);
    assert_eq!(twr_key(&p.ext, "proposal"), json!(id));
    let notice = card_notice(&th, &card);
    assert!(notice.contains("Confirmed."), "{notice}");
    assert!(notice.contains("Skipped: nothing"), "{notice}");
    drop(rt);
    th.stop().await;
}

/// Cancel (an option, or a dismissal) applies nothing; the orchestrator hears of it.
#[tokio::test]
async fn cancel_applies_nothing() {
    let mut th = start(true).await;
    let mut user = th.client("tui").await;
    let (mut rt, mut orch) = session(&mut th, &mut user).await;
    for how in ["Cancel", "option", "dismiss"] {
        th.permission("atlas-1", "d1");
        let turn = user_says(&mut user, &mut rt, "atlas seems fine, let it through").await;
        proposes(
            orch.call("attention.respond", respond_json("d1", "allow"))
                .await,
        );
        proposes(
            orch.call("thread.set_mode", set_json("atlas-1", "plan"))
                .await,
        );
        rt.end_turn(turn);
        let card = wait_card(&th).await;
        match how {
            "Cancel" => {
                answer_card(&mut user, &card, "cancel").await;
            }
            "option" => cancel_card(&th, &card),
            _ => {
                user.call(
                    "attention.dismiss",
                    json!({"attn_id": card["attn_id"], "revision": card["revision"]}),
                )
                .await
                .unwrap();
            }
        }
        assert!(cards(&th).is_empty(), "{how}");
        assert!(th.is_open("d1"), "{how}");
        assert_eq!(mode_of(&th, "atlas-1"), "manual", "{how}");
        let notice = card_notice(&th, &card);
        assert!(notice.contains("Confirmation cancelled"), "{how}: {notice}");
        th.user_resolves("d1");
    }
    // Anything else is no answer to a card.
    th.permission("atlas-1", "d2");
    let turn = user_says(&mut user, &mut rt, "atlas seems fine, let it through").await;
    proposes(
        orch.call("attention.respond", respond_json("d2", "allow"))
            .await,
    );
    rt.end_turn(turn);
    let card = wait_card(&th).await;
    let e = user
        .call(
            "attention.respond",
            json!({"attn_id": card["attn_id"], "answer": "allow"}),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::BAD_REQUEST, "{e:#}");
    assert_eq!(cards(&th).len(), 1);
    drop(rt);
    th.stop().await;
}

/// What no longer holds when the user confirms is skipped and reported: an item the user
/// answered meanwhile, an item raised again since (it may ask something else now).
#[tokio::test]
async fn a_stale_action_is_skipped_on_confirm() {
    let mut th = start(true).await;
    let mut user = th.client("tui").await;
    let (mut rt, mut orch) = session(&mut th, &mut user).await;
    th.permission("orbit-webapp-1", "w1");
    th.permission("atlas-1", "d1");
    let turn = user_says(&mut user, &mut rt, "let both through and auto-pilot atlas").await;
    for attn in ["w1", "d1"] {
        proposes(
            orch.call("attention.respond", respond_json(attn, "allow"))
                .await,
        );
    }
    proposes(
        orch.call("thread.set_mode", set_json("atlas-1", "auto"))
            .await,
    );
    rt.end_turn(turn);
    let card = wait_card(&th).await;
    th.user_resolves("w1");
    th.permission("atlas-1", "d1");
    let ev = answer_card(&mut user, &card, "Confirm").await;
    let done = &ev["data"]["ext"]["twr"]["confirmed"];
    assert_eq!(done["applied"], json!(["set atlas-1 → auto"]), "{ev}");
    let skipped = done["skipped"].as_array().unwrap();
    assert_eq!(skipped.len(), 2, "{ev}");
    assert!(
        skipped[0]
            .as_str()
            .unwrap()
            .starts_with("allow orbit-webapp-1 ("),
        "{ev}"
    );
    assert!(
        skipped[0].as_str().unwrap().contains("already resolved"),
        "{ev}"
    );
    assert!(
        skipped[1].as_str().unwrap().contains("raised again"),
        "{ev}"
    );
    assert!(th.is_open("d1"));
    assert_eq!(resolution(&th, "w1").1.answer.unwrap().decision, "deny");
    assert_eq!(mode_of(&th, "atlas-1"), "auto");
    let notice = card_notice(&th, &card);
    assert!(notice.contains("Skipped: allow orbit-webapp-1"), "{notice}");
    drop(rt);
    th.stop().await;
}

/// A mode change on a card is for the life of the thread it was proposed in: a thread deleted
/// and restored (here under another name) since is skipped on confirm and keeps its mode.
#[tokio::test]
async fn a_mode_change_skips_a_thread_restored_since() {
    let mut th = start(true).await;
    let mut user = th.client("tui").await;
    let (mut rt, mut orch) = session(&mut th, &mut user).await;
    let turn = user_says(&mut user, &mut rt, "please auto-pilot atlas when you can").await;
    proposes(
        orch.call("thread.set_mode", set_json("atlas-1", "auto"))
            .await,
    );
    rt.end_turn(turn);
    let card = wait_card(&th).await;
    let id = th.hub.resolve("atlas-1").unwrap();
    let proposal = card_proposal(&th, card["attn_id"].as_str().unwrap());
    assert_eq!(
        proposal["actions"][0]["lifecycle"],
        json!(th.hub.thread_view(&id).unwrap().lifecycle_seq),
        "{proposal}"
    );
    // Deleted and restored as atlas-2 meanwhile (as the trash records it).
    th.hub
        .append(
            &id,
            None,
            Source::Hub,
            EventBody::ThreadDeleted(ThreadDeleted {
                name: "atlas-1".into(),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
    th.hub
        .append(
            &id,
            None,
            Source::Hub,
            EventBody::ThreadRestored(ThreadRestored {
                item: "01J00000000000000000000000".into(),
                name: "atlas-2".into(),
                previous_name: Some("atlas-1".into()),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
    let ev = answer_card(&mut user, &card, "Confirm").await;
    let done = &ev["data"]["ext"]["twr"]["confirmed"];
    assert_eq!(done["applied"], json!([]), "{ev}");
    let skipped = done["skipped"].as_array().unwrap();
    assert_eq!(skipped.len(), 1, "{ev}");
    assert!(
        skipped[0]
            .as_str()
            .unwrap()
            .contains("atlas-2 was deleted and restored since the card was raised"),
        "{ev}"
    );
    assert_eq!(mode_of(&th, "atlas-2"), "manual");
    drop(rt);
    th.stop().await;
}

/// Only the user answers a confirm card: the orchestrator can neither answer, pick an option,
/// annotate nor dismiss it.
#[tokio::test]
async fn only_the_user_answers_a_confirm_card() {
    let mut th = start(true).await;
    let mut user = th.client("tui").await;
    let (mut rt, mut orch) = session(&mut th, &mut user).await;
    th.permission("atlas-1", "d1");
    let turn = user_says(&mut user, &mut rt, "atlas seems fine, let it through").await;
    proposes(
        orch.call("attention.respond", respond_json("d1", "allow"))
            .await,
    );
    rt.end_turn(turn);
    let card = wait_card(&th).await;
    let attn = card["attn_id"].clone();
    // Even in a user turn whose message says "confirm".
    let turn = user_says(&mut user, &mut rt, "confirm").await;
    assert_eq!(cards(&th).len(), 0, "a new message supersedes the card");
    rt.end_turn(turn);
    // A fresh card, and the orchestrator tries everything on it.
    let turn = user_says(&mut user, &mut rt, "atlas seems fine, let it through").await;
    proposes(
        orch.call("attention.respond", respond_json("d1", "allow"))
            .await,
    );
    rt.end_turn(turn);
    let card = wait_card(&th).await;
    assert_ne!(card["attn_id"], attn);
    let attn = card["attn_id"].clone();
    for (method, params) in [
        (
            "attention.respond",
            json!({"attn_id": attn, "answer": "Confirm"}),
        ),
        (
            "attention.respond",
            json!({"attn_id": attn, "revision": card["revision"], "answer": "Confirm"}),
        ),
        (
            "attention.respond",
            json!({"attn_id": attn, "option_index": 0}),
        ),
        (
            "attention.respond",
            json!({"attn_id": attn, "answer": "Cancel"}),
        ),
        (
            "attention.annotate",
            json!({"attn_id": attn, "note": "safe"}),
        ),
        ("attention.dismiss", json!({"attn_id": attn})),
    ] {
        let e = orch.call(method, params.clone()).await.unwrap_err();
        // `attention.dismiss` is no orchestrator method at all.
        let want = if method == "attention.dismiss" {
            codes::UNAUTHORIZED
        } else {
            codes::RELAY_FORBIDDEN
        };
        assert_eq!(code(&e), want, "{method} {params}: {e:#}");
    }
    assert_eq!(cards(&th).len(), 1);
    assert!(th.is_open("d1"));
    // The user's annotation is fine; the user confirms.
    user.call(
        "attention.annotate",
        json!({"attn_id": attn, "note": "mine"}),
    )
    .await
    .unwrap();
    answer_card(&mut user, &card, "Confirm").await;
    assert!(!th.is_open("d1"));
    drop(rt);
    th.stop().await;
}

/// A newer user message to the orchestrator resolves an open card as superseded (nothing
/// applied, no notice: the message is the orchestrator's next turn), and drops proposals not
/// on a card yet.
#[tokio::test]
async fn a_newer_user_message_supersedes_the_card() {
    let mut th = start(true).await;
    let mut user = th.client("tui").await;
    let (mut rt, mut orch) = session(&mut th, &mut user).await;
    th.permission("atlas-1", "d1");
    let turn = user_says(&mut user, &mut rt, "atlas seems fine, let it through").await;
    proposes(
        orch.call("attention.respond", respond_json("d1", "allow"))
            .await,
    );
    rt.end_turn(turn);
    let card = wait_card(&th).await;
    let attn = card["attn_id"].as_str().unwrap().to_string();
    let turn = user_says(&mut user, &mut rt, "actually, wait").await;
    assert!(cards(&th).is_empty());
    let (source, p) = resolution(&th, &attn);
    assert_eq!(source, Source::User("tui".into()));
    assert_eq!(p.by, ResolvedBy::User);
    assert_eq!(p.answer.unwrap().decision, "superseded");
    assert!(th.is_open("d1"));
    rt.end_turn(turn);
    // Proposals of a turn whose message a newer one superseded never reach a card.
    let turn = user_says(&mut user, &mut rt, "atlas seems fine, let it through").await;
    proposes(
        orch.call("attention.respond", respond_json("d1", "allow"))
            .await,
    );
    let r = say_to_orch(&mut user, "no, hold on").await;
    assert_eq!(r["pending"], 1, "{r}");
    rt.end_turn(turn);
    no_card(&th).await;
    let turn = rt.user_turn("no, hold on").await;
    rt.end_turn(turn);
    no_card(&th).await;
    assert!(th.is_open("d1"));
    drop(rt);
    th.stop().await;
}

/// Provenance still refuses (`relay_forbidden`) rather than proposes: a notice turn, an older
/// turn once a newer message arrived, an item the user had not seen, an item raised after the
/// message, an orchestrator's item, a thread created after the message, a thread whose mode
/// the user changed since. Nothing goes on a card.
#[tokio::test]
async fn provenance_refusals_refuse_rather_than_propose() {
    let mut th = start(true).await;
    let mut user = th.client("tui").await;
    let (mut rt, mut orch) = session(&mut th, &mut user).await;

    // An exceptional notice turn; routine permissions are shown directly.
    th.permission("atlas-1", "d1");
    th.raise(
        "orbit-webapp-1",
        "notice-error",
        AttentionReason::Error,
        &["Yes", "No"],
    );
    let (notice, turn) = rt.next_turn().await;
    assert!(notice.starts_with("[twr notice]"), "{notice}");
    forbidden(
        orch.call("attention.respond", respond_json("d1", "allow"))
            .await,
        "not started by a user message",
    );
    forbidden(
        orch.call("thread.set_mode", set_json("atlas-1", "auto"))
            .await,
        "not started by a user message",
    );
    rt.end_turn(turn);
    no_card(&th).await;

    // An older turn once the user sent a newer message.
    let turn = user_says(&mut user, &mut rt, "let atlas through, it is fine").await;
    say_to_orch(&mut user, "and the webapp?").await;
    forbidden(
        orch.call("attention.respond", respond_json("d1", "allow"))
            .await,
        "the user sent a newer message",
    );
    forbidden(
        orch.call("thread.set_mode", set_json("atlas-1", "auto"))
            .await,
        "the user sent a newer message",
    );
    rt.end_turn(turn);
    let turn = rt.user_turn("and the webapp?").await;
    rt.end_turn(turn);
    no_card(&th).await;

    // An item the user had not seen, and one raised after the message.
    let seen = control_head(&mut user).await;
    th.permission("orbit-webapp-1", "w1");
    let msg = "let them all through, they are fine";
    let r = user
        .call(
            "say",
            json!({"message": msg, "to": "orchestrator", "seen_gseq": seen}),
        )
        .await
        .unwrap();
    assert_eq!(r["action"], "delegate", "{r}");
    let turn = rt.user_turn(msg).await;
    th.permission("orbit-webapp-1", "w2");
    for attn in ["w1", "w2"] {
        forbidden(
            orch.call("attention.respond", respond_json(attn, "allow"))
                .await,
            "raised after",
        );
    }
    // An orchestrator's own item.
    th.raise(
        "orchestrator",
        "o1",
        AttentionReason::Question,
        &["Yes", "No"],
    );
    forbidden(
        orch.call("attention.respond", respond_json("o1", "Yes"))
            .await,
        "orchestrator thread",
    );
    // A thread created after the message.
    project_worker(&th, &mut user, "late", "late-1", "manual").await;
    forbidden(
        orch.call("thread.set_mode", set_json("late-1", "auto"))
            .await,
        "did not exist when the user's message arrived",
    );
    // A thread whose mode the user changed since.
    user.call("thread.set_mode", set_json("atlas-1", "plan"))
        .await
        .unwrap();
    forbidden(
        orch.call("thread.set_mode", set_json("atlas-1", "auto"))
            .await,
        "the user changed",
    );
    rt.end_turn(turn);
    no_card(&th).await;
    for attn in ["d1", "w1", "w2", "o1"] {
        assert!(th.is_open(attn), "{attn}");
    }
    assert_eq!(mode_of(&th, "late-1"), "manual");
    assert_eq!(mode_of(&th, "atlas-1"), "plan");
    drop(rt);
    th.stop().await;
}

/// An open confirm card outlives a hub restart: it is rebuilt from the log, and Confirm
/// applies it then.
#[tokio::test]
async fn a_confirm_card_survives_a_hub_restart() {
    let mut th = start(true).await;
    let mut user = th.client("tui").await;
    let (mut rt, mut orch) = session(&mut th, &mut user).await;
    th.permission("atlas-1", "d1");
    let turn = user_says(&mut user, &mut rt, "atlas seems fine, autopilot it too").await;
    proposes(
        orch.call("attention.respond", respond_json("d1", "allow"))
            .await,
    );
    proposes(
        orch.call("thread.set_mode", set_json("atlas-1", "auto"))
            .await,
    );
    rt.end_turn(turn);
    let card = wait_card(&th).await;
    drop((user, orch, rt));
    let dir = th.shutdown().await;
    let th = open(dir, true).await;
    let mut user = th.client("tui").await;
    let again = cards(&th);
    assert_eq!(again.len(), 1);
    assert_eq!(again[0]["attn_id"], card["attn_id"]);
    assert!(th.is_open("d1"));
    // The orchestrator still cannot answer it; the user confirms it.
    let ev = answer_card(&mut user, &again[0], "Confirm").await;
    assert_eq!(
        ev["data"]["ext"]["twr"]["confirmed"]["applied"],
        json!(["allow atlas-1", "set atlas-1 → auto"]),
        "{ev}"
    );
    assert!(!th.is_open("d1"));
    assert_eq!(mode_of(&th, "atlas-1"), "auto");
    th.stop().await;
}

// ------------------------------------------------------------------------------------------
// The `set_mode` gate
// ------------------------------------------------------------------------------------------

/// Nobody changes the orchestrator's mode; a notice turn changes nothing; a short command is
/// applied; any other request is a proposal.
#[tokio::test]
async fn set_mode_is_gated_like_the_relay_rule() {
    let mut th = start(true).await;
    let mut user = th.client("tui").await;
    let (mut rt, mut orch) = orchestrator_up(&mut th, &mut user).await;
    for c in [&mut user, &mut orch] {
        let e = c
            .call("thread.set_mode", set_json("orchestrator", "auto"))
            .await
            .unwrap_err();
        assert_eq!(code(&e), codes::BAD_REQUEST, "{e:#}");
        assert!(format!("{e:#}").contains("twr orchestrator use"), "{e:#}");
    }
    for (msg, sets) in [
        (
            "set @web to plan mode and @api to manual mode",
            vec![("web", "plan"), ("api", "manual")],
        ),
        ("set @web to plam mode", vec![("web", "plan")]),
        (
            "switch all to read only mode",
            vec![("api", "read-only"), ("web", "read-only")],
        ),
        ("set web to manual mode", vec![("web", "manual")]),
    ] {
        let turn = user_says(&mut user, &mut rt, msg).await;
        for (thread, mode) in sets {
            let r = orch
                .call("thread.set_mode", set_json(thread, mode))
                .await
                .unwrap();
            assert_eq!(r["mode"], mode, "{msg:?}: {r}");
            assert!(r.get("status").is_none(), "{msg:?}: {r}");
        }
        rt.end_turn(turn);
        no_card(&th).await;
    }
    // Not a command for this thread or this mode: proposals.
    for (msg, thread, mode) in [
        ("allow both", "api", "auto"),
        ("set @web to plan mode", "api", "plan"),
        ("set @web to plan mode", "web", "auto"),
        ("don't set @web to plan mode", "web", "plan"),
        ("set it to plan mode", "web", "plan"),
        ("run the auto tests on web", "web", "auto"),
        (
            "Explain bypass permissions for web",
            "web",
            "bypassPermissions",
        ),
        ("set web to plain mode", "web", "plan"),
        ("fix the auth mode bug in web", "web", "auto"),
    ] {
        let before = mode_of(&th, thread);
        let turn = user_says(&mut user, &mut rt, msg).await;
        proposes(orch.call("thread.set_mode", set_json(thread, mode)).await);
        rt.end_turn(turn);
        let card = wait_card(&th).await;
        cancel_card(&th, &card);
        assert_eq!(mode_of(&th, thread), before, "{msg:?}");
    }
    drop(rt);
    th.stop().await;
}

/// The threads a mode request covers are those of when the message arrived: a thread created
/// later is refused, and "both" counts the workers then.
#[tokio::test]
async fn mode_targets_are_frozen_when_the_message_arrives() {
    let mut th = start(true).await;
    let mut user = th.client("tui").await;
    let (mut rt, mut orch) = orchestrator_up(&mut th, &mut user).await;
    let msg = "set all to bypass permissions";
    let turn = user_says(&mut user, &mut rt, msg).await;
    worker(&th, &mut user, "late", "manual").await;
    forbidden(
        orch.call("thread.set_mode", set_json("late", "bypassPermissions"))
            .await,
        "did not exist when the user's message arrived",
    );
    orch.call("thread.set_mode", set_json("api", "bypassPermissions"))
        .await
        .unwrap();
    rt.end_turn(turn);
    assert_eq!(mode_of(&th, "late"), "manual");
    // Three workers: "both" covers none of them (a proposal).
    let msg = "set both to plan mode";
    let turn = user_says(&mut user, &mut rt, msg).await;
    proposes(orch.call("thread.set_mode", set_json("web", "plan")).await);
    rt.end_turn(turn);
    let card = wait_card(&th).await;
    cancel_card(&th, &card);
    // With late stopped (when the message arrives), "both" is api and web.
    user.call("thread.stop", json!({"thread": "late"}))
        .await
        .unwrap();
    let turn = user_says(&mut user, &mut rt, msg).await;
    for name in ["api", "web"] {
        let r = orch
            .call("thread.set_mode", set_json(name, "plan"))
            .await
            .unwrap();
        assert_eq!(r["changed"], true, "{r}");
    }
    rt.end_turn(turn);
    drop(rt);
    th.stop().await;
}

/// A message's authority to change a thread's mode is used once, and a newer mode change by
/// the user ends it.
#[tokio::test]
async fn mode_authority_is_used_once_and_ends_with_a_user_change() {
    let mut th = start(true).await;
    let mut user = th.client("tui").await;
    let (mut rt, mut orch) = orchestrator_up(&mut th, &mut user).await;
    let msg = "Set @web to bypass permissions";
    let turn = user_says(&mut user, &mut rt, msg).await;
    let set = set_json("web", "bypassPermissions");
    let r = orch.call("thread.set_mode", set.clone()).await.unwrap();
    assert_eq!(r["changed"], true, "{r}");
    // A retry of the same change is a no-op; another mode is refused.
    let r = orch.call("thread.set_mode", set.clone()).await.unwrap();
    assert_eq!(r["changed"], false, "{r}");
    forbidden(
        orch.call("thread.set_mode", set_json("web", "plan")).await,
        "already changed",
    );
    // The user restricts web; the stale message cannot undo that.
    user.call("thread.set_mode", set_json("web", "manual"))
        .await
        .unwrap();
    forbidden(orch.call("thread.set_mode", set).await, "the user changed");
    assert_eq!(mode_of(&th, "web"), "manual");
    rt.end_turn(turn);
    no_card(&th).await;
    drop(rt);
    th.stop().await;
}

/// The user's explicit choice of the mode a thread already has is recorded and ends an earlier
/// message's authority; nothing restarts. The orchestrator's own no-op records nothing.
#[tokio::test]
async fn an_unchanged_user_mode_choice_ends_a_stale_request() {
    let mut th = start(true).await;
    let mut user = th.client("tui").await;
    user.call("thread.set_mode", set_json("web", "manual"))
        .await
        .unwrap();
    let id = th.hub.resolve("web").unwrap();
    let (mut rt, mut orch) = orchestrator_up(&mut th, &mut user).await;
    let msg = "set @web to bypass permissions mode";
    let turn = user_says(&mut user, &mut rt, msg).await;
    let r = user
        .call("thread.set_mode", set_json("web", "manual"))
        .await
        .unwrap();
    assert_eq!(r["changed"], false, "{r}");
    forbidden(
        orch.call("thread.set_mode", set_json("web", "bypassPermissions"))
            .await,
        "the user changed",
    );
    assert_eq!(mode_of(&th, "web"), "manual");
    let (_, evs) = th.hub.thread_events(&id, 0, 1000).unwrap();
    let last = evs
        .iter()
        .rev()
        .find(|e| e.kind() == "thread.mode_changed")
        .unwrap();
    assert_eq!(last.source, Source::User("tui".into()));
    rt.end_turn(turn);
    let msg = "set @api to auto mode";
    let turn = user_says(&mut user, &mut rt, msg).await;
    let api = th.hub.resolve("api").unwrap();
    let count = |th: &TestHub| {
        let (_, evs) = th.hub.thread_events(&api, 0, 1000).unwrap();
        evs.iter()
            .filter(|e| e.kind() == "thread.mode_changed")
            .count()
    };
    let before = count(&th);
    let r = orch
        .call("thread.set_mode", set_json("api", "auto"))
        .await
        .unwrap();
    assert_eq!(r["changed"], false, "{r}");
    assert_eq!(count(&th), before);
    rt.end_turn(turn);
    drop(rt);
    th.stop().await;
}

// ------------------------------------------------------------------------------------------
// What was waiting, and seen, when the message arrived
// ------------------------------------------------------------------------------------------

/// A message is read against the items as they were when it arrived: an item raised again
/// since is refused; a deny-only permission is never allowed.
#[tokio::test]
async fn an_answer_is_checked_against_the_items_as_they_were() {
    let mut th = start(true).await;
    let mut user = th.client("tui").await;
    let (mut rt, mut orch) = orchestrator_up(&mut th, &mut user).await;
    th.permission("api", "a1");
    th.question("web", "q1");
    let msg = "allow both";
    let turn = user_says(&mut user, &mut rt, msg).await;
    th.raise("web", "q1", AttentionReason::Permission, &["allow", "deny"]);
    forbidden(
        orch.call("attention.respond", respond_json("q1", "allow"))
            .await,
        "raised after",
    );
    // "both" counts the two items as they were: a1's allow applies.
    relays(&mut orch, "a1", "allow").await;
    rt.end_turn(turn);
    no_card(&th).await;
    th.user_resolves("q1");

    th.permission("api", "a2");
    th.raise("web", "w2", AttentionReason::Permission, &["deny"]);
    let turn = user_says(&mut user, &mut rt, msg).await;
    let e = orch
        .call("attention.respond", respond_json("w2", "allow"))
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::BAD_REQUEST, "{e:#}");
    assert!(format!("{e:#}").contains("not one of"), "{e:#}");
    // "allow both" gives a deny-only item no answer: its deny is a proposal; a2's allow
    // applies.
    proposes(
        orch.call("attention.respond", respond_json("w2", "deny"))
            .await,
    );
    relays(&mut orch, "a2", "allow").await;
    rt.end_turn(turn);
    let card = wait_card(&th).await;
    cancel_card(&th, &card);
    assert!(th.is_open("w2") && !th.is_open("a2"));
    drop(rt);
    th.stop().await;
}

/// With only api's card on screen, web's item (raised after what
/// was seen) is refused, and "both" counts one item (a proposal); both on screen, both are
/// answered.
#[tokio::test]
async fn a_message_covers_only_what_was_on_screen() {
    let mut th = start(true).await;
    let mut user = th.client("tui").await;
    let (mut rt, mut orch) = orchestrator_up(&mut th, &mut user).await;
    th.permission("api", "a1");
    let seen = control_head(&mut user).await;
    th.permission("web", "w1");
    let msg = "allow both";
    let r = user
        .call(
            "say",
            json!({"message": msg, "to": "orchestrator", "seen_gseq": seen}),
        )
        .await
        .unwrap();
    assert_eq!(r["action"], "delegate", "{r}");
    let turn = rt.user_turn(msg).await;
    forbidden(
        orch.call("attention.respond", respond_json("w1", "allow"))
            .await,
        "raised after",
    );
    proposes(
        orch.call("attention.respond", respond_json("a1", "allow"))
            .await,
    );
    rt.end_turn(turn);
    let card = wait_card(&th).await;
    cancel_card(&th, &card);
    let msg = "deny both";
    let turn = user_says(&mut user, &mut rt, msg).await;
    for attn in ["a1", "w1"] {
        orch.call("attention.respond", respond_json(attn, "deny"))
            .await
            .unwrap();
    }
    rt.end_turn(turn);
    drop(rt);
    th.stop().await;
}

/// On the hub's side, "yes" to a question whose tenth option is `Yes` is refused when the user had
/// not seen the question; seen, it goes on the card (a question is never answered directly) and
/// applies on the user's Confirm.
#[tokio::test]
async fn an_affirmative_option_past_the_ninth_is_relayed_only_when_seen() {
    let mut th = start(true).await;
    let mut user = th.client("tui").await;
    let (mut rt, mut orch) = orchestrator_up(&mut th, &mut user).await;
    let options = [
        "red", "green", "blue", "cyan", "teal", "gray", "pink", "gold", "plum", "Yes",
    ];
    let before = control_head(&mut user).await;
    th.raise("api", "q1", AttentionReason::Question, &options);
    let r = user
        .call(
            "say",
            json!({"message": "yes", "to": "orchestrator", "seen_gseq": before}),
        )
        .await
        .unwrap();
    assert_eq!(r["action"], "delegate", "{r}");
    let turn = rt.user_turn("yes").await;
    forbidden(
        orch.call("attention.respond", respond_json("q1", "Yes"))
            .await,
        "raised after",
    );
    rt.end_turn(turn);
    assert!(th.is_open("q1"));
    let turn = user_says(&mut user, &mut rt, "yes").await;
    proposes(
        orch.call("attention.respond", respond_json("q1", "Yes"))
            .await,
    );
    rt.end_turn(turn);
    let card = wait_card(&th).await;
    assert!(th.is_open("q1"));
    answer_card(&mut user, &card, "Confirm").await;
    assert!(!th.is_open("q1"));
    drop(rt);
    th.stop().await;
}

// ------------------------------------------------------------------------------------------
// Names
// ------------------------------------------------------------------------------------------

/// Names that differ only in case (`api` and `Api`) name neither thread: proposals, for
/// answers and modes alike. The hub takes no such name now; an older log may hold one. A
/// thread named like a vocabulary word (`all`) is named only with `@`.
#[tokio::test]
async fn ambiguous_and_vocabulary_names() {
    let mut th = start(true).await;
    let mut user = th.client("tui").await;
    let e = user
        .call(
            "thread.new",
            json!({"project": th.dir.path(), "runtime": "fake", "name": "Api"}),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::CONFLICT, "{e:#}");
    th.hub
        .append(
            "t-api-upper",
            None,
            Source::User("tui".into()),
            EventBody::ThreadCreated(ThreadCreated {
                name: "Api".into(),
                box_name: "local".into(),
                runtime: "fake".into(),
                mode: "manual".into(),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
    worker(&th, &mut user, "all", "manual").await;
    user.call("thread.set_mode", set_json("api", "manual"))
        .await
        .unwrap();
    let (mut rt, mut orch) = orchestrator_up(&mut th, &mut user).await;
    th.permission("api", "a1");
    th.permission("Api", "A1");
    let turn = user_says(&mut user, &mut rt, "allow api").await;
    for attn in ["a1", "A1"] {
        proposes(
            orch.call("attention.respond", respond_json(attn, "allow"))
                .await,
        );
    }
    proposes(
        orch.call("thread.set_mode", set_json("api", "bypassPermissions"))
            .await,
    );
    rt.end_turn(turn);
    let card = wait_card(&th).await;
    cancel_card(&th, &card);
    th.user_resolves("a1");
    th.user_resolves("A1");
    th.permission("web", "w1");
    th.permission("all", "l1");
    let turn = user_says(&mut user, &mut rt, "allow @all").await;
    proposes(
        orch.call("attention.respond", respond_json("w1", "allow"))
            .await,
    );
    relays(&mut orch, "l1", "allow").await;
    rt.end_turn(turn);
    let card = wait_card(&th).await;
    cancel_card(&th, &card);
    th.permission("all", "l2");
    let turn = user_says(&mut user, &mut rt, "allow all").await;
    for attn in ["w1", "l2"] {
        relays(&mut orch, attn, "allow").await;
    }
    rt.end_turn(turn);
    drop(rt);
    th.stop().await;
}

/// With api and a worker named `cheers` each holding a seen permission, "allow api, cheers" once
/// approved both, though the user approved api and said thanks. A courtesy word never names a
/// thread bare: the message goes on the card, nothing is applied, and only "@cheers" names the
/// worker.
#[tokio::test]
async fn a_courtesy_word_never_authorizes_a_worker_of_that_name() {
    let mut th = start(true).await;
    let mut user = th.client("tui").await;
    worker(&th, &mut user, "cheers", "manual").await;
    let (mut rt, mut orch) = orchestrator_up(&mut th, &mut user).await;
    th.permission("api", "a1");
    th.permission("cheers", "c1");
    let turn = user_says(&mut user, &mut rt, "allow api, cheers").await;
    for attn in ["a1", "c1"] {
        proposes(
            orch.call("attention.respond", respond_json(attn, "allow"))
                .await,
        );
    }
    rt.end_turn(turn);
    let card = wait_card(&th).await;
    cancel_card(&th, &card);
    assert!(th.is_open("a1") && th.is_open("c1"));
    let turn = user_says(&mut user, &mut rt, "allow api, @cheers").await;
    for attn in ["a1", "c1"] {
        relays(&mut orch, attn, "allow").await;
        assert!(!th.is_open(attn));
    }
    rt.end_turn(turn);
    no_card(&th).await;
    drop(rt);
    th.stop().await;
}

/// With api and `conditionally-1` each holding a seen permission, "allow api, conditionally" once
/// approved both ("tentatively" too), and "set either to bypass mode" changed `either-1`. A
/// qualifier or indefinite-choice word never names a thread bare: each goes on the card with
/// nothing applied, and only `@x` names the worker.
#[tokio::test]
async fn a_qualifier_never_authorizes_a_worker_of_that_name() {
    let mut th = start(true).await;
    let mut user = th.client("tui").await;
    for name in [
        "conditionally-1",
        "tentatively-1",
        "either-1",
        "whichever-1",
    ] {
        worker(&th, &mut user, name, "manual").await;
    }
    let (mut rt, mut orch) = orchestrator_up(&mut th, &mut user).await;
    th.permission("api", "a1");
    th.permission("conditionally-1", "c1");
    th.permission("tentatively-1", "t1");
    for (msg, attn) in [
        ("allow api, conditionally", "c1"),
        ("allow api, tentatively", "t1"),
    ] {
        let turn = user_says(&mut user, &mut rt, msg).await;
        for attn in ["a1", attn] {
            proposes(
                orch.call("attention.respond", respond_json(attn, "allow"))
                    .await,
            );
        }
        rt.end_turn(turn);
        let card = wait_card(&th).await;
        cancel_card(&th, &card);
        assert!(th.is_open("a1") && th.is_open(attn), "{msg}");
    }
    for (msg, name) in [
        ("set either to bypass mode", "either-1"),
        ("set whichever to bypass mode", "whichever-1"),
    ] {
        let turn = user_says(&mut user, &mut rt, msg).await;
        proposes(
            orch.call("thread.set_mode", set_json(name, "bypassPermissions"))
                .await,
        );
        rt.end_turn(turn);
        let card = wait_card(&th).await;
        cancel_card(&th, &card);
        assert_eq!(mode_of(&th, name), "manual", "{msg}");
    }
    let turn = user_says(&mut user, &mut rt, "allow api, @conditionally-1").await;
    for attn in ["a1", "c1"] {
        relays(&mut orch, attn, "allow").await;
        assert!(!th.is_open(attn));
    }
    rt.end_turn(turn);
    no_card(&th).await;
    let turn = user_says(&mut user, &mut rt, "set @either-1 to bypass mode").await;
    orch.call("thread.set_mode", set_json("either-1", "bypassPermissions"))
        .await
        .unwrap();
    rt.end_turn(turn);
    no_card(&th).await;
    assert_eq!(mode_of(&th, "either-1"), "bypassPermissions");
    drop(rt);
    th.stop().await;
}

/// A hub question waiting is no worker: with one real worker "both" covers nothing (a
/// proposal), with two it covers them.
#[tokio::test]
async fn the_hub_thread_is_no_worker() {
    let mut th = start(true).await;
    let mut user = th.client("tui").await;
    user.call("thread.set_mode", set_json("api", "manual"))
        .await
        .unwrap();
    let (mut rt, mut orch) = orchestrator_up(&mut th, &mut user).await;
    user.call("thread.stop", json!({"thread": "web"}))
        .await
        .unwrap();
    hub_question(&th, "hq");
    let msg = "set both to bypass permissions";
    let turn = user_says(&mut user, &mut rt, msg).await;
    proposes(
        orch.call("thread.set_mode", set_json("api", "bypassPermissions"))
            .await,
    );
    rt.end_turn(turn);
    let card = wait_card(&th).await;
    cancel_card(&th, &card);
    worker(&th, &mut user, "db", "manual").await;
    let msg = "set both to auto mode";
    let turn = user_says(&mut user, &mut rt, msg).await;
    for name in ["api", "db"] {
        sets_mode(&th, &mut orch, name, "auto").await;
    }
    rt.end_turn(turn);
    drop(rt);
    th.stop().await;
}

/// Raises hub question `attn_id` (which thread gets a routed message: api or a new one).
fn hub_question(th: &TestHub, attn_id: &str) {
    th.hub
        .append(
            HUB_THREAD,
            None,
            Source::Hub,
            EventBody::AttentionRaised(AttentionRaised {
                attn_id: attn_id.into(),
                reason: AttentionReason::Question,
                summary: "Which thread should get: deploy?".into(),
                options: vec!["api".into(), "new_thread".into()],
                ..Default::default()
            }),
            None,
        )
        .unwrap();
}

/// The relay guard and the `set_mode` gate read a message against the same
/// scene, the hub thread in it while a hub question waits. "approve @hub, set api to auto
/// mode" is a command: the mode change applies, while the answer to the hub question is still
/// a proposal. The hub thread is never a mode target, nor counted by both / all.
#[tokio::test]
async fn an_at_hub_answer_clause_does_not_spoil_a_mode_command() {
    let mut th = start(true).await;
    let mut user = th.client("tui").await;
    user.call("thread.set_mode", set_json("api", "manual"))
        .await
        .unwrap();
    let (mut rt, mut orch) = orchestrator_up(&mut th, &mut user).await;
    user.call("thread.stop", json!({"thread": "web"}))
        .await
        .unwrap();
    hub_question(&th, "hq");
    let turn = user_says(&mut user, &mut rt, "approve @hub, set api to auto mode").await;
    sets_mode(&th, &mut orch, "api", "auto").await;
    proposes(
        orch.call("attention.respond", respond_json("hq", "api"))
            .await,
    );
    rt.end_turn(turn);
    let card = wait_card(&th).await;
    cancel_card(&th, &card);
    assert!(th.is_open("hq"));
    // The hub thread is no mode target: a group naming it picks out nothing, and the hub
    // thread has no mode to set.
    for msg in ["set @hub to plan mode", "set @hub and api to plan mode"] {
        let turn = user_says(&mut user, &mut rt, msg).await;
        assert!(
            orch.call("thread.set_mode", set_json(HUB_THREAD, "plan"))
                .await
                .is_err(),
            "{msg}"
        );
        proposes(orch.call("thread.set_mode", set_json("api", "plan")).await);
        rt.end_turn(turn);
        let card = wait_card(&th).await;
        cancel_card(&th, &card);
        assert_eq!(mode_of(&th, "api"), "auto", "{msg}");
    }
    // Both counts the workers alone: api is the only one, so "both" picks out nothing.
    let turn = user_says(&mut user, &mut rt, "approve @hub, set both to plan mode").await;
    proposes(orch.call("thread.set_mode", set_json("api", "plan")).await);
    rt.end_turn(turn);
    let card = wait_card(&th).await;
    cancel_card(&th, &card);
    assert_eq!(mode_of(&th, "api"), "auto");
    // All is every worker, api alone here, never the hub thread.
    let turn = user_says(&mut user, &mut rt, "set all to plan mode").await;
    sets_mode(&th, &mut orch, "api", "plan").await;
    rt.end_turn(turn);
    no_card(&th).await;
    assert!(th.is_open("hq"));
    drop(rt);
    th.stop().await;
}

/// An orchestrator's item is never answered, and never counted by "both" /
/// "all" or the single-candidate rule.
#[tokio::test]
async fn an_orchestrator_item_is_never_covered() {
    let mut th = start(true).await;
    let mut user = th.client("tui").await;
    let (mut rt, mut orch) = orchestrator_up(&mut th, &mut user).await;
    th.permission("api", "a1");
    th.raise(
        "orchestrator",
        "o1",
        AttentionReason::Question,
        &["Yes", "No"],
    );
    let turn = user_says(&mut user, &mut rt, "approve all").await;
    forbidden(
        orch.call("attention.respond", respond_json("o1", "Yes"))
            .await,
        "orchestrator thread",
    );
    relays(&mut orch, "a1", "allow").await;
    rt.end_turn(turn);
    th.permission("api", "a2");
    let turn = user_says(&mut user, &mut rt, "yes api").await;
    relays(&mut orch, "a2", "allow").await;
    rt.end_turn(turn);
    assert!(th.is_open("o1"));
    th.user_resolves("o1");
    drop(rt);
    th.stop().await;
}

// ------------------------------------------------------------------------------------------
// A newer message supersedes an older one
// ------------------------------------------------------------------------------------------

/// Any newer user message to the orchestrator ends the older turn's authority, whatever it
/// says and whichever way it arrives (`say {to: orchestrator}`, `say` delegated by the route
/// ladder, `thread.send` to the orchestrator). In the newer message's turn the older request is
/// no command: a proposal. A hub notice is no user message and ends nothing.
#[tokio::test]
async fn any_newer_user_message_ends_the_older_authority() {
    let mut th = start(true).await;
    let mut user = th.client("tui").await;
    let (mut rt, mut orch) = orchestrator_up(&mut th, &mut user).await;
    for (i, path) in ["say to", "say", "thread.send"].into_iter().enumerate() {
        let (a, w) = (format!("a{i}"), format!("w{i}"));
        th.permission("api", &a);
        let msg = "allow @api, set @web to plan mode";
        let turn = user_says(&mut user, &mut rt, msg).await;
        // A notice (a worker raised something) is no user message.
        th.permission("web", &w);
        orch.call("thread.set_mode", set_json("web", "plan"))
            .await
            .unwrap();
        let newer = "thanks, and how is the release going";
        match path {
            "say to" => {
                say_to_orch(&mut user, newer).await;
            }
            "say" => {
                let r = say(&mut user, "coordinate the release", "").await;
                assert_eq!(r["action"], "delegate", "{r}");
            }
            _ => {
                user.call(
                    "thread.send",
                    json!({"thread": "orchestrator", "prompt": newer}),
                )
                .await
                .unwrap();
            }
        }
        forbidden(
            orch.call("attention.respond", respond_json(&a, "allow"))
                .await,
            "the user sent a newer message or command; read the conversation before acting",
        );
        assert!(th.is_open(&a), "{path}");
        rt.end_turn(turn);
        let (text, turn) = rt.next_turn().await;
        assert_ne!(text, msg, "{path}");
        if !text.starts_with("[twr notice]") {
            proposes(
                orch.call("attention.respond", respond_json(&a, "allow"))
                    .await,
            );
        }
        rt.end_turn(turn);
        th.user_resolves(&a);
        th.user_resolves(&w);
        user.call("thread.set_mode", set_json("web", "manual"))
            .await
            .unwrap();
        while let Ok((_, t)) = timeout(Duration::from_millis(500), rt.next_turn()).await {
            rt.end_turn(t);
        }
        for card in cards(&th) {
            cancel_card(&th, &card);
        }
        while let Ok((_, t)) = timeout(Duration::from_millis(500), rt.next_turn()).await {
            rt.end_turn(t);
        }
    }
    drop(rt);
    th.stop().await;
}

// ------------------------------------------------------------------------------------------
// A confirm card commits with its actions, all or nothing
// ------------------------------------------------------------------------------------------

/// The orchestrator proposes allow orbit-webapp-1 (`w`), deny atlas-1 (`d`) and atlas-1 →
/// auto from one unusual message; returns the card raised at its turn's end.
async fn three_action_card(
    th: &TestHub,
    user: &mut Client,
    rt: &mut OrchRt,
    orch: &mut Client,
    w: &str,
    d: &str,
) -> Value {
    th.permission("orbit-webapp-1", w);
    th.permission("atlas-1", d);
    let msg = "ok the webapp, nah atlas, and pop atlas into auto mode";
    let turn = user_says(user, rt, msg).await;
    proposes(
        orch.call("attention.respond", respond_json(w, "allow"))
            .await,
    );
    proposes(
        orch.call("attention.respond", respond_json(d, "deny"))
            .await,
    );
    proposes(
        orch.call("thread.set_mode", set_json("atlas-1", "auto"))
            .await,
    );
    rt.end_turn(turn);
    wait_card(th).await
}

/// The store refuses the card's own `attention.resolved` (the last event of a Confirm).
const REFUSE_CONFIRMATIONS: &str = "CREATE TRIGGER inject BEFORE INSERT ON events
     WHEN NEW.kind = 'attention.resolved' AND NEW.data LIKE '%confirmed%'
     BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;";

/// A Confirm whose record fails applies nothing and leaves the card open; Confirm again
/// applies everything once; Cancel after a failed Confirm truthfully applies nothing; and a
/// card answered again reports what the user did with it the first time.
#[tokio::test]
async fn a_confirm_commits_with_its_actions_or_not_at_all() {
    let mut th = start(true).await;
    let mut user = th.client("tui").await;
    let (mut rt, mut orch) = session(&mut th, &mut user).await;
    let db = rusqlite::Connection::open(Paths::with_home(th.dir.path()).db_file()).unwrap();
    db.busy_timeout(Duration::from_secs(5)).unwrap();

    // Confirm fails at its last event: nothing was applied, the card is still open.
    let card = three_action_card(&th, &mut user, &mut rt, &mut orch, "w1", "d1").await;
    db.execute_batch(REFUSE_CONFIRMATIONS).unwrap();
    let e = user
        .call(
            "attention.respond",
            json!({"attn_id": card["attn_id"], "revision": card["revision"], "answer": "Confirm"}),
        )
        .await
        .unwrap_err();
    assert!(format!("{e:#}").contains("injected write failure"), "{e:#}");
    db.execute_batch("DROP TRIGGER inject").unwrap();
    assert!(th.is_open("w1") && th.is_open("d1"));
    assert_eq!(mode_of(&th, "atlas-1"), "manual");
    assert_eq!(cards(&th).len(), 1, "the card is still open");
    assert!(th.hub.fatal_error().is_none());
    // Confirm again: everything is applied, once.
    let ev = answer_card(&mut user, &card, "Confirm").await;
    let done = &ev["data"]["ext"]["twr"]["confirmed"];
    assert_eq!(
        done["applied"],
        json!(["allow orbit-webapp-1", "deny atlas-1", "set atlas-1 → auto"]),
        "{ev}"
    );
    assert_eq!(done["skipped"], json!([]), "{ev}");
    assert!(!th.is_open("w1") && !th.is_open("d1"));
    assert_eq!(mode_of(&th, "atlas-1"), "auto");
    let notice = card_notice(&th, &card);
    assert!(notice.contains("Confirmed."), "{notice}");
    // Answered again (a retry, or Cancel), the card reports the original outcome.
    for (answer, method) in [
        ("Confirm", "attention.respond"),
        ("Cancel", "attention.respond"),
        ("", "attention.dismiss"),
    ] {
        let mut params = json!({"attn_id": card["attn_id"]});
        if !answer.is_empty() {
            params["answer"] = json!(answer);
        }
        let e = user.call(method, params).await.unwrap_err();
        assert_eq!(code(&e), codes::CONFLICT, "{e:#}");
        let msg = format!("{e:#}");
        assert!(
            msg.contains("the user confirmed it. Applied: allow orbit-webapp-1, deny atlas-1, set atlas-1 → auto. Skipped: nothing."),
            "{answer}: {msg}"
        );
    }

    // Cancel after a failed Confirm: nothing was applied, and that is what it reports.
    let card = three_action_card(&th, &mut user, &mut rt, &mut orch, "w2", "d2").await;
    db.execute_batch(REFUSE_CONFIRMATIONS).unwrap();
    user.call(
        "attention.respond",
        json!({"attn_id": card["attn_id"], "revision": card["revision"], "answer": "Confirm"}),
    )
    .await
    .unwrap_err();
    db.execute_batch("DROP TRIGGER inject").unwrap();
    answer_card(&mut user, &card, "Cancel").await;
    assert!(cards(&th).is_empty());
    assert!(th.is_open("w2") && th.is_open("d2"));
    assert_eq!(
        mode_of(&th, "atlas-1"),
        "auto",
        "unchanged since the first card"
    );
    let notice = card_notice(&th, &card);
    assert!(
        notice.contains("Confirmation cancelled; nothing applied."),
        "{notice}"
    );
    let e = user
        .call(
            "attention.respond",
            json!({"attn_id": card["attn_id"], "answer": "Confirm"}),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::CONFLICT, "{e:#}");
    assert!(
        format!("{e:#}").contains("the user cancelled it; nothing was applied"),
        "{e:#}"
    );
    drop(rt);
    th.stop().await;
}

// ------------------------------------------------------------------------------------------
// Modes as the hub spells them, confirm ordering, lossless cards
// ------------------------------------------------------------------------------------------

/// One orchestrator call.
enum Call<'a> {
    /// `attention.respond {attn_id, answer}`.
    Answer(&'a str, &'a str),
    /// `thread.set_mode {thread, mode}`.
    Mode(&'a str, &'a str),
}

impl Call<'_> {
    async fn send(&self, orch: &mut Client) -> anyhow::Result<Value> {
        match self {
            Call::Answer(attn, answer) => {
                orch.call("attention.respond", respond_json(attn, answer))
                    .await
            }
            Call::Mode(thread, mode) => orch.call("thread.set_mode", set_json(thread, mode)).await,
        }
    }
}

/// A generic ACP agent's mode id is kept as given by the hub, so "set gem to plan
/// mode" covers only the id the hub spells `plan`. "p'lan" (and other ids the hub keeps as
/// written) goes on the card; case, a `-` or `_` inside, and spaces around are the hub's own
/// canonicalization, so those calls are `plan` and apply directly.
#[tokio::test]
async fn an_acp_mode_counts_only_as_the_hub_spells_it() {
    let mut th = start(true).await;
    let mut user = th.client("tui").await;
    user.call(
        "thread.new",
        json!({"project": th.dir.path(), "runtime": "acp:agent --stdio", "name": "gem",
               "mode": "manual"}),
    )
    .await
    .unwrap();
    let (mut rt, mut orch) = orchestrator_up(&mut th, &mut user).await;
    let msg = "set gem to plan mode";
    for trick in [
        "p'lan",
        "p\u{2019}lan",
        "plan\u{200b}",
        "plan-mode",
        "plann",
    ] {
        let turn = user_says(&mut user, &mut rt, msg).await;
        let r = proposes(orch.call("thread.set_mode", set_json("gem", trick)).await);
        assert_eq!(r["mode"], trick, "{r}");
        assert_eq!(mode_of(&th, "gem"), "manual", "{trick:?}");
        rt.end_turn(turn);
        let card = wait_card(&th).await;
        cancel_card(&th, &card);
        assert_eq!(mode_of(&th, "gem"), "manual", "{trick:?}");
    }
    for spelled in ["plan", "PLAN", "pl-an", "p_lan", "plan ", " Plan"] {
        let turn = user_says(&mut user, &mut rt, msg).await;
        let r = orch
            .call("thread.set_mode", set_json("gem", spelled))
            .await
            .unwrap();
        assert_eq!(
            (r["mode"].as_str(), r["changed"].as_bool()),
            (Some("plan"), Some(true)),
            "{spelled:?}: {r}"
        );
        assert_eq!(mode_of(&th, "gem"), "plan");
        rt.end_turn(turn);
        user.call("thread.set_mode", set_json("gem", "manual"))
            .await
            .unwrap();
    }
    no_card(&th).await;
    drop(rt);
    th.stop().await;
}

/// The worker `name` on the probe runtime, started with a prompt, with `perms` permissions
/// raised between turns; returns its id and their hub ids, in order.
async fn probe_worker(
    th: &TestHub,
    user: &mut Client,
    name: &str,
    perms: usize,
) -> (String, Vec<String>) {
    th.probe.perms.store(perms, Ordering::SeqCst);
    user.call(
        "thread.new",
        json!({"project": th.dir.path(), "runtime": "probe", "name": name, "mode": "manual"}),
    )
    .await
    .unwrap();
    user.call("thread.send", json!({"thread": name, "prompt": "hi"}))
        .await
        .unwrap();
    let id = th.hub.resolve(name).unwrap();
    let items = timeout(WAIT, async {
        loop {
            let mut items: Vec<String> = th
                .hub
                .attention_list(false)
                .iter()
                .filter(|i| i["thread"] == id.as_str())
                .filter_map(|i| i["attn_id"].as_str().map(str::to_string))
                .collect();
            if items.len() == perms {
                items.sort();
                return items;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the probe's permissions");
    (id, items)
}

/// A confirmed card forwards every answer to its runtime before any restart. With a
/// mode change and answers to the worker's permissions raised between turns, in any order of
/// proposal, the runtime gets every answer and only then the stop of its mode restart (at
/// b03e227 a mode change proposed before an answer stopped the runtime first, and the answer
/// never reached it).
#[tokio::test]
async fn a_confirm_forwards_every_answer_before_a_mode_restart() {
    let mut th = start(true).await;
    let mut user = th.client("tui").await;
    let (mut rt, mut orch) = orchestrator_up(&mut th, &mut user).await;
    // (worker, permissions it raises, the proposals in order: `None` the mode change,
    // `Some(k)` allow its k-th permission)
    let cases: &[(&str, usize, &[Option<usize>])] = &[
        ("probe-a", 1, &[None, Some(1)]),
        ("probe-b", 1, &[Some(1), None]),
        ("probe-c", 2, &[Some(1), None, Some(2)]),
        ("probe-d", 2, &[None, Some(2), Some(1)]),
        ("probe-e", 2, &[Some(2), Some(1), None]),
    ];
    for (name, perms, order) in cases {
        let (id, items) = probe_worker(&th, &mut user, name, *perms).await;
        let msg = format!("pop {name} into plan mode and ok its prompts");
        let turn = user_says(&mut user, &mut rt, &msg).await;
        let mut want = Vec::new();
        for step in *order {
            match step {
                None => proposes(orch.call("thread.set_mode", set_json(name, "plan")).await),
                Some(k) => {
                    want.push(format!("respond p{k} allow"));
                    proposes(
                        orch.call("attention.respond", respond_json(&items[k - 1], "allow"))
                            .await,
                    )
                }
            };
        }
        want.push("stop".to_string());
        rt.end_turn(turn);
        let card = wait_card(&th).await;
        let ev = answer_card(&mut user, &card, "Confirm").await;
        assert_eq!(
            ev["data"]["ext"]["twr"]["confirmed"]["skipped"],
            json!([]),
            "{name}: {ev}"
        );
        let cmds = timeout(WAIT, async {
            loop {
                let cmds = th.probe.commands(&id);
                if cmds.iter().any(|c| c == "stop") {
                    return cmds;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{name}: no restart; got {:?}", th.probe.commands(&id)));
        assert_eq!(cmds, want, "{name}");
        assert_eq!(mode_of(&th, name), "plan");
        for attn in &items {
            assert!(!th.is_open(attn), "{name} {attn}");
        }
    }
    drop(rt);
    th.stop().await;
}

/// A confirm card's text is a lossless rendering of exactly the actions it holds.
/// For an ACP worker, the proposal "plan\u{200B}" rendered like "set gem → plan" and Confirm
/// applied the distinct id. Now the card writes the invisible character as a visible escape
/// and marks the id as a custom mode, an option label and an item summary are escaped too, the
/// stored actions render back to the card's summary exactly, and Confirm applies those actions.
#[tokio::test]
async fn a_confirm_card_shows_its_actions_losslessly() {
    use twr_decisions::code::card::{self, CardLine};
    let mut th = start(true).await;
    let mut user = th.client("tui").await;
    user.call(
        "thread.new",
        json!({"project": th.dir.path(), "runtime": "acp:agent --stdio", "name": "gem",
               "mode": "manual"}),
    )
    .await
    .unwrap();
    let (mut rt, mut orch) = orchestrator_up(&mut th, &mut user).await;
    let web = th.hub.resolve("web").unwrap();
    th.hub
        .append(
            &web,
            None,
            Source::Hub,
            EventBody::AttentionRaised(AttentionRaised {
                attn_id: "q1".into(),
                reason: AttentionReason::Question,
                summary: "Which store?\u{200B}".into(),
                options: vec!["Use\u{200B}SQLite".into(), "Postgres".into()],
                ..Default::default()
            }),
            None,
        )
        .unwrap();
    let turn = user_says(
        &mut user,
        &mut rt,
        "set gem to plan mode, and use sqlite on web",
    )
    .await;
    let r = proposes(Call::Mode("gem", "plan\u{200B}").send(&mut orch).await);
    assert_eq!(r["mode"], "plan\u{200B}", "{r}");
    proposes(
        Call::Answer("q1", "Use\u{200B}SQLite")
            .send(&mut orch)
            .await,
    );
    rt.end_turn(turn);
    let card = wait_card(&th).await;
    let summary = card["summary"].as_str().unwrap();
    assert_eq!(
        summary,
        "From your message: set gem → custom mode \"plan\\u{200B}\" · answer web: \
         \"Use\\u{200B}SQLite\" (Which store?\\u{200B})",
    );
    assert!(!summary.contains("set gem → plan"), "{summary:?}");
    assert!(!summary.contains('\u{200B}'), "{summary:?}");
    // The stored actions render back to the summary, and the summary reads back as them.
    let proposal = card_proposal(&th, card["attn_id"].as_str().unwrap());
    let lines: Vec<CardLine> = proposal["actions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            let thread = th
                .hub
                .thread_view(a["thread"].as_str().unwrap())
                .unwrap()
                .name;
            match a["kind"].as_str().unwrap() {
                "mode" => {
                    let mode = a["mode"].as_str().unwrap().to_string();
                    CardLine::Mode {
                        thread,
                        custom: mode != "plan",
                        mode,
                    }
                }
                _ => CardLine::Answer {
                    thread,
                    answer: a["answer"].as_str().unwrap().to_string(),
                    item: Some("Which store?\u{200B}".into()),
                },
            }
        })
        .collect();
    assert_eq!(card::render(&lines), summary);
    assert_eq!(card::parse(summary), Some(lines));
    // Confirm applies exactly those actions: the id as stored, and the answer.
    answer_card(&mut user, &card, "Confirm").await;
    let (_, resolved) = resolution(&th, "q1");
    assert_eq!(resolved.answer.unwrap().decision, "Use\u{200B}SQLite");
    let gem = th.hub.resolve("gem").unwrap();
    let (_, evs) = th.hub.thread_events(&gem, 0, 1000).unwrap();
    let changes: Vec<String> = evs
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::ThreadModeChanged(p) => Some(p.mode.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(changes, ["plan\u{200B}"]);
    let (_, confirmed) = resolution(&th, card["attn_id"].as_str().unwrap());
    assert_eq!(
        twr_key(&confirmed.ext, "confirmed")["applied"],
        json!([
            "set gem → custom mode \"plan\\u{200B}\"",
            "answer web: \"Use\\u{200B}SQLite\""
        ])
    );
    drop(rt);
    th.stop().await;
}

// ------------------------------------------------------------------------------------------
// Everything outside the short commands goes on the card
// ------------------------------------------------------------------------------------------

/// The user says `msg`; each of `calls` is a proposal, nothing is applied, and the card raised
/// at the turn's end is cancelled.
async fn to_the_card(
    th: &TestHub,
    user: &mut Client,
    rt: &mut OrchRt,
    orch: &mut Client,
    msg: &str,
    calls: &[Call<'_>],
) {
    let turn = user_says(user, rt, msg).await;
    for call in calls {
        proposes(call.send(orch).await);
    }
    rt.end_turn(turn);
    let card = wait_card(th).await;
    cancel_card(th, &card);
}

/// A second sentence, a question, courtesy, "si", a noun, a glued quantifier, a typo of a
/// name, an option label or number, a bare number, a word an orchestrator matches: every call
/// is a proposal, the user cancels the card, nothing is applied.
#[tokio::test]
async fn everything_else_goes_on_the_card() {
    let mut th = start(true).await;
    let mut user = th.client("tui").await;
    for name in ["api", "web"] {
        user.call("thread.set_mode", set_json(name, "manual"))
            .await
            .unwrap();
    }
    let (mut rt, mut orch) = orchestrator_up(&mut th, &mut user).await;
    let unchanged = |th: &TestHub, msg: &str| {
        assert!(th.is_open("a1") && th.is_open("w1"), "{msg:?}");
        assert_eq!(mode_of(th, "api"), "manual", "{msg:?}");
        assert_eq!(mode_of(th, "web"), "manual", "{msg:?}");
    };
    th.permission("api", "a1");
    th.permission("web", "w1");
    let allow_api = &[Call::Answer("a1", "allow")][..];
    let both = &[Call::Answer("a1", "allow"), Call::Answer("w1", "allow")][..];
    let auto_api = &[Call::Mode("api", "auto")][..];
    let auto_both = &[Call::Mode("api", "auto"), Call::Mode("web", "auto")][..];
    let cases: &[(&str, &[Call])] = &[
        // The question reaches the orchestrator as its turn's prompt (`user_says` checks it);
        // the mode change goes on the card.
        (
            "Set both agents to auto mode. What were the latest changes on api?",
            auto_both,
        ),
        (
            "set both to audo mode and approve bothrequests",
            &[
                Call::Mode("api", "auto"),
                Call::Mode("web", "auto"),
                Call::Answer("a1", "allow"),
                Call::Answer("w1", "allow"),
            ],
        ),
        ("approve both requests", both),
        ("Allow api. Only if tests pass?", allow_api),
        ("Set api to auto mode. Or keep it manual?", auto_api),
        ("allow api?", allow_api),
        ("allow api\ndeny web", allow_api),
        ("allow both, thanks", both),
        ("please allow api", allow_api),
        ("si, allow api", allow_api),
        ("si api", allow_api),
        ("allow apii", allow_api),
        ("set wbe to plan mode", &[Call::Mode("web", "plan")]),
        ("set api to auto", auto_api),
        ("allow api only if the tests pass", allow_api),
    ];
    for (msg, calls) in cases {
        to_the_card(&th, &mut user, &mut rt, &mut orch, msg, calls).await;
        unchanged(&th, msg);
    }
    th.user_resolves("a1");
    th.user_resolves("w1");

    // An option label or number is no answer word.
    th.question("web", "q1");
    for msg in ["SQLite", "option 2 for web", "2", "use sqlite"] {
        to_the_card(
            &th,
            &mut user,
            &mut rt,
            &mut orch,
            msg,
            &[Call::Answer("q1", "SQLite")],
        )
        .await;
        assert!(th.is_open("q1"), "{msg:?}");
    }
    th.user_resolves("q1");

    // A bare number, and a word an orchestrator matches, name no worker; `@x` does.
    worker(&th, &mut user, "2", "manual").await;
    worker(&th, &mut user, "orchestrator-worker", "manual").await;
    th.permission("2", "n1");
    th.permission("orchestrator-worker", "o1");
    let cases: &[(&str, &[Call])] = &[
        ("allow 2", &[Call::Answer("n1", "allow")]),
        ("set 2 to plan mode", &[Call::Mode("2", "plan")]),
        ("allow orchestrator", &[Call::Answer("o1", "allow")]),
        ("approve orchstrator", &[Call::Answer("o1", "allow")]),
        (
            "set orchestrator-worker to bypass permissions",
            &[Call::Mode("orchestrator-worker", "bypassPermissions")],
        ),
    ];
    for (msg, calls) in cases {
        to_the_card(&th, &mut user, &mut rt, &mut orch, msg, calls).await;
        assert!(th.is_open("n1") && th.is_open("o1"), "{msg:?}");
        assert_eq!(mode_of(&th, "2"), "manual", "{msg:?}");
        assert_eq!(mode_of(&th, "orchestrator-worker"), "manual", "{msg:?}");
    }
    let turn = user_says(&mut user, &mut rt, "allow @2 and @orchestrator-worker").await;
    relays(&mut orch, "n1", "allow").await;
    relays(&mut orch, "o1", "allow").await;
    rt.end_turn(turn);
    let turn = user_says(&mut user, &mut rt, "set @2 to plan mode").await;
    sets_mode(&th, &mut orch, "2", "plan").await;
    rt.end_turn(turn);
    no_card(&th).await;
    drop(rt);
    th.stop().await;
}

/// A question is never answered directly: "approve all api permissions" for api's Yes/No
/// question goes on the card, and "approve all" applies a permission's
/// allow while the question's Yes goes on the card, applied only when the user confirms it.
#[tokio::test]
async fn a_question_is_answered_only_on_the_card() {
    let mut th = start(true).await;
    let mut user = th.client("tui").await;
    let (mut rt, mut orch) = orchestrator_up(&mut th, &mut user).await;
    th.raise("api", "q1", AttentionReason::Question, &["Yes", "No"]);
    th.permission("web", "w1");
    to_the_card(
        &th,
        &mut user,
        &mut rt,
        &mut orch,
        "approve all api permissions",
        &[Call::Answer("q1", "Yes"), Call::Answer("w1", "allow")],
    )
    .await;
    assert!(th.is_open("q1") && th.is_open("w1"));
    th.user_resolves("w1");
    // Alone too, and with "yes".
    for msg in ["approve all api permissions", "yes", "yes api"] {
        to_the_card(
            &th,
            &mut user,
            &mut rt,
            &mut orch,
            msg,
            &[Call::Answer("q1", "Yes")],
        )
        .await;
        assert!(th.is_open("q1"), "{msg:?}");
    }
    th.permission("web", "w2");
    let turn = user_says(&mut user, &mut rt, "approve all").await;
    relays(&mut orch, "w2", "allow").await;
    proposes(Call::Answer("q1", "Yes").send(&mut orch).await);
    assert!(th.is_open("q1"));
    rt.end_turn(turn);
    let card = wait_card(&th).await;
    assert_eq!(
        card["summary"],
        "From your message: answer api: \"Yes\" (q1?)"
    );
    assert!(th.is_open("q1"), "nothing applied before the Confirm");
    let ev = answer_card(&mut user, &card, "Confirm").await;
    assert_eq!(
        ev["data"]["ext"]["twr"]["confirmed"]["applied"],
        json!(["answer api: \"Yes\""]),
        "{ev}"
    );
    let (source, p) = resolution(&th, "q1");
    assert_eq!(source, Source::User("tui".into()));
    assert_eq!(p.answer.unwrap().decision, "Yes");
    drop(rt);
    th.stop().await;
}
