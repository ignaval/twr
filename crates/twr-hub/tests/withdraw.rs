//! Taking back a message (`message.withdraw`, `hub::withdraw`): still queued, its turn
//! running and not acted yet, too late; a withdrawn orchestrator turn's lost authority, the
//! handoff and conversation updates without it, and the fact its next turn gets.
//!
//! Workers run the fake adapter (it interrupts a turn and keeps its session). The
//! orchestrator's runtime is `claude-code` played by a test adapter that hands every session to
//! the test ([`OrchRt`]), so each test drives its turns and sees the interrupt.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::timeout;
use twr_adapters::fake::FakeAdapter;
use twr_core::adapter::{
    Adapter, AdapterCmd, AdapterEnd, AdapterHandle, AdapterInfo, Answer, SpawnConfig, Tier,
};
use twr_core::config::Config;
use twr_core::event::*;
use twr_core::proto::codes;
use twr_hub::hub::ControlSubscription;
use twr_hub::{Caller, Hub, HubOptions, NewThread, ShellRun, SwitchOptions};

const WAIT: Duration = Duration::from_secs(10);

/// One orchestrator runtime session handed to the test.
struct OrchRt {
    end: AdapterEnd,
    first: Option<(String, u64)>,
}

impl OrchRt {
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

    /// The next command that is not a prompt (an interrupt, a stop).
    async fn next_cmd(&mut self) -> AdapterCmd {
        timeout(WAIT, self.end.cmds.recv())
            .await
            .expect("timed out waiting for a command")
            .expect("the hub dropped the orchestrator runtime")
    }

    /// Ends `turn` with a short reply, reported as `result`.
    fn end_turn_as(&self, turn: u64, reply: &str, result: TurnResult) {
        self.end.emit(
            EventBody::AgentText(AgentText {
                text: reply.into(),
                is_final: true,
                ..Default::default()
            }),
            Some(turn),
        );
        self.end.emit(
            EventBody::TurnEnded(TurnEnded {
                result,
                ..Default::default()
            }),
            Some(turn),
        );
    }

    fn end_turn(&self, turn: u64, reply: &str) {
        self.end_turn_as(turn, reply, TurnResult::Completed);
    }
}

/// `claude-code` for the tests: every session goes to the test through a channel; while its
/// flag is set, it cannot start.
struct OrchAdapter(
    mpsc::UnboundedSender<OrchRt>,
    Arc<std::sync::atomic::AtomicBool>,
);

impl Adapter for OrchAdapter {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "claude-code",
            tier: Tier::Headless,
            description: "test orchestrator runtime",
        }
    }

    /// The user's compaction (`hub::compact`) reaches it as `AdapterCmd::Compact`.
    fn compacts(&self) -> bool {
        true
    }

    fn spawn(&self, cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        if self.1.load(std::sync::atomic::Ordering::SeqCst) {
            anyhow::bail!("claude is not installed");
        }
        let (h, end) = AdapterHandle::pair();
        end.emit(
            EventBody::ThreadStarted(SessionPayload {
                session_id: Some("s-orch".into()),
                ..Default::default()
            }),
            None,
        );
        let first = prompt.map(|p| (p, cfg.prompt_turn.unwrap_or(1)));
        let _ = self.0.send(OrchRt { end, first });
        Ok(h)
    }
}

/// A runtime that cannot interrupt a turn (a PTY's tier), otherwise the fake.
struct PtyLike;

impl Adapter for PtyLike {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "ptylike",
            tier: Tier::Pty,
            description: "test runtime without interrupts",
        }
    }

    fn spawn(&self, cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        FakeAdapter::new().spawn(cfg, prompt)
    }
}

struct Env {
    dir: tempfile::TempDir,
    spawns: mpsc::UnboundedReceiver<OrchRt>,
    tx: mpsc::UnboundedSender<OrchRt>,
    /// The orchestrator's runtime cannot start while set.
    broken: Arc<std::sync::atomic::AtomicBool>,
}

impl Env {
    fn new() -> Env {
        let (tx, spawns) = mpsc::unbounded_channel();
        Env {
            dir: tempfile::tempdir().unwrap(),
            spawns,
            tx,
            broken: Arc::default(),
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    async fn hub(&self) -> Hub {
        Hub::open(self.opts()).await.unwrap()
    }

    /// Opens the hub again after the previous one was dropped (its lock may linger briefly).
    async fn reopen(&self) -> Hub {
        timeout(WAIT, async {
            loop {
                match Hub::open(self.opts()).await {
                    Ok(h) => return h,
                    Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
                }
            }
        })
        .await
        .expect("reopening the hub")
    }

    fn opts(&self) -> HubOptions {
        let mut config = Config {
            default_runtime: "fake".into(),
            ..Default::default()
        };
        config.orchestrator.runtime = Some("claude-code".into());
        config.orchestrator.cwd = Some(self.path().to_path_buf());
        config.orchestrator.model = Some("haiku".into());
        let mut o = HubOptions::new(self.path().join("data").join("twr.db"), config);
        let orch: Arc<dyn Adapter> = Arc::new(OrchAdapter(self.tx.clone(), self.broken.clone()));
        o.adapters = Arc::new(move |name: &str| -> Option<Arc<dyn Adapter>> {
            match name {
                "fake" => Some(Arc::new(FakeAdapter::new())),
                "ptylike" => Some(Arc::new(PtyLike)),
                "claude-code" => Some(Arc::clone(&orch)),
                _ => None,
            }
        });
        o.socket = Some(self.path().join("hub.sock"));
        o.twr_exe = Some(PathBuf::from("/opt/twr/bin/twr"));
        o.claude_policy = twr_adapters::claude_code::guard::PolicySources {
            managed_dir: Some(self.path().join("claude-managed")),
            config_dir: Some(self.path().join("claude-config")),
        };
        o.stop_timeout = Duration::from_secs(5);
        // A `!!` command runs through the job gate, which this test binary isn't.
        o.job_gate_exe = Some(env!("CARGO_BIN_EXE_twr-job-gate").into());
        o
    }

    async fn orch(&mut self) -> OrchRt {
        timeout(WAIT, self.spawns.recv())
            .await
            .expect("timed out waiting for the orchestrator to spawn")
            .unwrap()
    }

    async fn thread(&self, hub: &Hub, name: &str, runtime: &str) -> String {
        let v = hub
            .new_thread(
                NewThread {
                    project: self.path().to_string_lossy().into_owned(),
                    runtime: Some(runtime.into()),
                    name: Some(name.into()),
                    ..Default::default()
                },
                "test",
            )
            .await
            .unwrap();
        v["id"].as_str().unwrap().to_string()
    }
}

fn tui() -> Caller {
    Caller::user("tui")
}

async fn until(sub: &mut ControlSubscription, pred: impl Fn(&Event) -> bool) -> Vec<Event> {
    let mut seen = Vec::new();
    timeout(WAIT, async {
        loop {
            let (_, ev) = sub.next().await.unwrap().expect("hub stopped");
            let done = pred(&ev);
            seen.push((*ev).clone());
            if done {
                return;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out; saw {:?}", kinds(&seen)));
    seen
}

fn kinds(evs: &[Event]) -> Vec<String> {
    evs.iter().map(|e| e.kind().to_string()).collect()
}

fn is_kind_on(kind: &'static str, thread: &str) -> impl Fn(&Event) -> bool {
    let thread = thread.to_string();
    move |e: &Event| e.kind() == kind && e.thread == thread
}

fn events_of(hub: &Hub, thread: &str) -> Vec<Event> {
    hub.thread_events(thread, 0, 10_000).unwrap().1
}

fn head(hub: &Hub) -> u64 {
    hub.control_read(0, 0).unwrap().1
}

fn withdraw(hub: &Hub, thread: &str, msg_ref: i64) -> Value {
    hub.withdraw_message(thread, msg_ref, &tui()).unwrap()
}

/// Polls `check` until it holds.
async fn eventually(mut check: impl FnMut() -> bool) {
    timeout(WAIT, async {
        while !check() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("timed out waiting for a condition");
}

/// A queued message goes from the queue with its record and never starts a turn; taking it
/// back again, or a message the thread never had, is refused with a reason. `say` and
/// `thread.send` both return the ref, and the turn a queued message starts records it.
#[tokio::test]
async fn a_queued_message_is_taken_back_and_never_runs() {
    let env = Env::new();
    let hub = env.hub().await;
    let api = env.thread(&hub, "api", "fake").await;
    let mut sub = hub.subscribe_control(0);
    let first = hub.send_by(&api, "!sleep 5000", &tui()).unwrap();
    assert_eq!((first.turn, first.msg_ref), (Some(1), first.queued));
    let r = hub.say("@api and then this", None, &tui()).await.unwrap();
    assert_eq!(r["action"], "send", "{r}");
    assert_eq!(r["turn"], Value::Null, "{r}");
    let queued = r["ref"].as_i64().expect("say returns the ref");

    let w = withdraw(&hub, "api", queued);
    assert_eq!(w["withdrawn"], true, "{w}");
    assert_eq!(
        (w["state"].as_str(), w["prompt"].as_str()),
        (Some("queued"), Some("and then this"))
    );
    let evs = until(&mut sub, is_kind_on("prompt.withdrawn", &api)).await;
    let EventBody::PromptWithdrawn(p) = &evs.last().unwrap().body else {
        panic!()
    };
    assert_eq!((p.msg_ref, p.prompt.as_str()), (queued, "and then this"));
    assert_eq!(evs.last().unwrap().source, Source::User("tui".into()));
    let again = withdraw(&hub, "api", queued);
    assert_eq!(
        (again["withdrawn"].as_bool(), again["reason"].as_str()),
        (Some(false), Some("withdrawn")),
        "{again}"
    );
    let none = withdraw(&hub, "api", queued + 100);
    assert_eq!(none["reason"], "unknown", "{none}");

    // The running turn recorded its own ref; once it ends nothing else runs.
    let started = events_of(&hub, &api)
        .into_iter()
        .find_map(|e| match e.body {
            EventBody::TurnStarted(p) => Some(p),
            _ => None,
        })
        .unwrap();
    assert_eq!(message_ref(&started), Some(first.msg_ref));
    assert_eq!(
        hub.thread_view("api").unwrap().current_turn_ref,
        Some(first.msg_ref)
    );
    let w = withdraw(&hub, "api", first.msg_ref);
    assert_eq!(w["state"], "running", "{w}");
    until(&mut sub, is_kind_on("turn.ended", &api)).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let v = hub.thread_view("api").unwrap();
    assert_eq!((v.turns, v.current_turn), (1, None));
    let ended = withdraw(&hub, "api", first.msg_ref);
    assert_eq!(ended["reason"], "withdrawn", "{ended}");
    hub.shutdown().await;
}

/// A running worker turn that has not acted is withdrawn and interrupted: it ends
/// `interrupted` with its session kept, gets no report, and what came after the interrupt
/// point never runs. One that already called a tool is too late.
#[tokio::test]
async fn a_running_worker_turn_is_interrupted_unless_it_acted() {
    let env = Env::new();
    let hub = env.hub().await;
    let api = env.thread(&hub, "api", "fake").await;
    let mut sub = hub.subscribe_control(0);
    let sent = hub
        .send_by(&api, "!sleep 5000\n!tool Bash ls", &tui())
        .unwrap();
    let w = withdraw(&hub, "api", sent.msg_ref);
    assert_eq!(
        (
            w["withdrawn"].as_bool(),
            w["state"].as_str(),
            w["turn"].as_u64()
        ),
        (Some(true), Some("running"), Some(1)),
        "{w}"
    );
    let evs = until(&mut sub, is_kind_on("turn.ended", &api)).await;
    let EventBody::TurnEnded(end) = &evs.last().unwrap().body else {
        panic!()
    };
    assert_eq!(end.result, TurnResult::Interrupted);
    let log = events_of(&hub, &api);
    let at = |kind: &str| log.iter().position(|e| e.kind() == kind);
    assert!(at("turn.withdrawn") < at("turn.ended"), "{:?}", kinds(&log));
    assert_eq!(at("agent.tool_call"), None, "{:?}", kinds(&log));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(events_of(&hub, &api)
        .iter()
        .all(|e| e.kind() != "report.ready"));

    // The session goes on; a turn that called a tool cannot be taken back.
    let sent = hub
        .send_by(&api, "!tool Bash ls\n!sleep 5000", &tui())
        .unwrap();
    eventually(|| hub.thread_view("api").unwrap().current_turn_acted).await;
    let w = withdraw(&hub, "api", sent.msg_ref);
    assert_eq!(
        (w["withdrawn"].as_bool(), w["reason"].as_str()),
        (Some(false), Some("acted")),
        "{w}"
    );
    assert!(
        w["message"]
            .as_str()
            .unwrap()
            .contains("api already started on it"),
        "{w}"
    );
    hub.stop_thread("api", "test").unwrap();
    hub.shutdown().await;
}

/// A runtime that ignores the interrupt because it came before the turn began (`!deaf`) is
/// interrupted again once it shows it runs the turn: what came after never runs.
#[tokio::test]
async fn a_taken_back_turn_its_runtime_runs_anyway_is_interrupted_again() {
    let env = Env::new();
    let hub = env.hub().await;
    let api = env.thread(&hub, "api", "fake").await;
    let mut sub = hub.subscribe_control(0);
    let sent = hub
        .send_by(
            &api,
            "!deaf\nworking on it\n!sleep 3000\n!tool Bash ls",
            &tui(),
        )
        .unwrap();
    assert_eq!(withdraw(&hub, "api", sent.msg_ref)["state"], "running");
    let evs = until(&mut sub, is_kind_on("turn.ended", &api)).await;
    let EventBody::TurnEnded(end) = &evs.last().unwrap().body else {
        panic!()
    };
    assert_eq!(end.result, TurnResult::Interrupted);
    let log = events_of(&hub, &api);
    assert!(
        log.iter().any(|e| e.kind() == "agent.text"),
        "{:?}",
        kinds(&log)
    );
    assert!(
        log.iter().all(|e| e.kind() != "agent.tool_call"),
        "{:?}",
        kinds(&log)
    );
    hub.shutdown().await;
}

/// A runtime that crashes after its turn's message was taken back fails no turn: the crash
/// is reported, but no failed-turn decision, error item for that turn, or retry of the
/// message taken back follows.
#[tokio::test]
async fn a_crash_after_a_withdrawal_retries_nothing() {
    let env = Env::new();
    let hub = env.hub().await;
    let api = env.thread(&hub, "api", "fake").await;
    let mut sub = hub.subscribe_control(0);
    let sent = hub
        .send_by(&api, "!deaf\n!sleep 300\n!crash", &tui())
        .unwrap();
    assert_eq!(withdraw(&hub, "api", sent.msg_ref)["state"], "running");
    until(&mut sub, is_kind_on("thread.crashed", &api)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let v = hub.thread_view("api").unwrap();
    assert_eq!((v.turns, v.current_turn, v.crashed_turn), (1, None, None));
    let (items, _) = hub.control_read(0, 10_000).unwrap();
    let block_kind = items.iter().any(
        |(_, e)| matches!(&e.body, EventBody::DecisionMade(d) if d.site == DecisionSite::BlockKind),
    );
    assert!(
        !block_kind,
        "{:?}",
        items.iter().map(|(_, e)| e.kind()).collect::<Vec<_>>()
    );
    hub.shutdown().await;
}

/// While a taken-back turn is open, each kind of sign that its runtime runs it interrupts it
/// again, once: its text, its thinking, its tool calls, its requests. A request it raises is
/// declined at once (`deny` to the runtime), never left for the user.
#[tokio::test]
async fn signs_of_a_taken_back_turn_interrupt_it_again_and_its_requests_are_declined() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let r = hub
        .say_to_orchestrator("look into the flaky test", Some(head(&hub)), &tui())
        .unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    withdraw(&hub, "orchestrator", r["ref"].as_i64().unwrap());
    assert_eq!(rt.next_cmd().await, AdapterCmd::Interrupt);
    let text = |s: &str| {
        EventBody::AgentText(AgentText {
            text: s.into(),
            delta: true,
            ..Default::default()
        })
    };
    // The runtime ignored it and runs the turn: its first text interrupts it again, the
    // next deltas do not.
    rt.end.emit(text("Let me"), Some(t1));
    assert_eq!(rt.next_cmd().await, AdapterCmd::Interrupt);
    rt.end.emit(text(" look"), Some(t1));
    rt.end.emit(
        EventBody::AttentionRaised(AttentionRaised {
            attn_id: "p1".into(),
            reason: AttentionReason::Permission,
            summary: "Read ~/notes".into(),
            options: vec!["allow".into(), "deny".into()],
            ..Default::default()
        }),
        Some(t1),
    );
    match rt.next_cmd().await {
        AdapterCmd::Respond { attn_id, answer } => {
            assert_eq!((attn_id.as_str(), answer.decision.as_str()), ("p1", "deny"));
        }
        other => panic!("expected the request declined first, got {other:?}"),
    }
    assert_eq!(rt.next_cmd().await, AdapterCmd::Interrupt);
    let orch = hub.orchestrator_thread().unwrap();
    let resolved = events_of(&hub, &orch)
        .into_iter()
        .find_map(|e| match e.body {
            EventBody::AttentionResolved(p) => Some(p),
            _ => None,
        })
        .expect("the request was resolved");
    assert_eq!(resolved.by, ResolvedBy::Timeout);
    assert_eq!(resolved.answer.map(|a| a.decision).as_deref(), Some("deny"));
    assert_eq!(resolved.ext.unwrap()["twr"]["taken_back"], true);
    assert!(
        hub.attention_list(true).is_empty(),
        "nothing waits for the user"
    );
    rt.end_turn_as(t1, "", TurnResult::Interrupted);
    until(
        &mut hub.subscribe_control(0),
        is_kind_on("turn.ended", &orch),
    )
    .await;
    // The next turn is no taken-back one: its signs interrupt nothing.
    hub.say_to_orchestrator("now look", Some(head(&hub)), &tui())
        .unwrap();
    let (_, t2) = rt.next_turn().await;
    rt.end.emit(text("Looking"), Some(t2));
    rt.end_turn(t2, "Done.");
    let cmd = timeout(Duration::from_millis(300), rt.end.cmds.recv()).await;
    assert!(cmd.is_err(), "no command: {cmd:?}");
    drop(rt);
    hub.shutdown().await;
}

/// On a runtime that cannot interrupt a turn, a queued message can still be taken back, a
/// running one cannot.
#[tokio::test]
async fn an_uninterruptible_runtime_gives_back_only_queued_messages() {
    let env = Env::new();
    let hub = env.hub().await;
    let pty = env.thread(&hub, "term", "ptylike").await;
    let first = hub.send_by(&pty, "!sleep 5000", &tui()).unwrap();
    assert_eq!(first.turn, Some(1));
    let second = hub.send_by(&pty, "later", &tui()).unwrap();
    assert_eq!(second.turn, None);
    assert_eq!(withdraw(&hub, "term", second.msg_ref)["state"], "queued");
    let w = withdraw(&hub, "term", first.msg_ref);
    assert_eq!(
        (w["withdrawn"].as_bool(), w["reason"].as_str()),
        (Some(false), Some("uninterruptible")),
        "{w}"
    );
    assert!(hub.thread_view("term").unwrap().current_turn.is_some());
    // Moved to a runtime that can interrupt, the thread still runs this turn on the one that
    // cannot: what runs decides.
    let moved = hub
        .set_runtime_model("term", Some("fake"), None, None, &tui())
        .unwrap();
    assert_eq!(moved["applies"], "after_turn", "{moved}");
    assert_eq!(hub.thread_view("term").unwrap().runtime, "fake");
    let w = withdraw(&hub, "term", first.msg_ref);
    assert_eq!(w["reason"], "uninterruptible", "{w}");
    // Only a user takes a message back.
    let e = hub
        .withdraw_message("term", first.msg_ref, &Caller::Orchestrator)
        .unwrap_err();
    assert_eq!(e.code, codes::UNAUTHORIZED);
    hub.stop_thread("term", "test").unwrap();
    hub.shutdown().await;
}

/// The orchestrator's turn is withdrawn before it acted: the turn is recorded withdrawn before
/// the runtime gets the interrupt; from then on it relays nothing, changes no mode and asks
/// the hub for nothing else; its end is recorded `interrupted` whatever the runtime says, and
/// its next turn is told to disregard the message.
#[tokio::test]
async fn a_withdrawn_orchestrator_turn_loses_its_authority() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let api = env.thread(&hub, "api", "fake").await;
    let mut sub = hub.subscribe_control(0);
    hub.send_by(&api, "!perm Bash rm -rf build", &tui())
        .unwrap();
    let evs = until(&mut sub, |e| e.kind() == "attention.raised").await;
    let EventBody::AttentionRaised(p) = &evs.last().unwrap().body else {
        panic!()
    };
    let attn = p.attn_id.clone();
    let r = hub
        .say_to_orchestrator("approve api's request", Some(head(&hub)), &tui())
        .unwrap();
    assert_eq!(r["action"], "delegate", "{r}");
    let msg_ref = r["ref"].as_i64().unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;

    let w = withdraw(&hub, "orchestrator", msg_ref);
    assert_eq!(
        (w["state"].as_str(), w["turn"].as_u64()),
        (Some("running"), Some(t1))
    );
    assert_eq!(rt.next_cmd().await, AdapterCmd::Interrupt);
    let orch = hub.orchestrator_thread().unwrap();
    let log = events_of(&hub, &orch);
    assert!(
        log.iter()
            .any(|e| e.kind() == "turn.withdrawn" && e.turn == Some(t1)),
        "{:?}",
        kinds(&log)
    );
    assert!(log.iter().all(|e| e.kind() != "turn.ended"));

    let Err(relay) = hub.respond_relay(&attn, Answer::allow(), None, &Caller::Orchestrator) else {
        panic!("the relay went through");
    };
    assert_eq!(relay.code, codes::RELAY_FORBIDDEN);
    assert!(relay.message.contains("took back"), "{}", relay.message);
    let mode = hub
        .set_mode(&api, "plan", &Caller::Orchestrator)
        .unwrap_err();
    assert_eq!(mode.code, codes::RELAY_FORBIDDEN);
    assert!(mode.message.contains("took back"), "{}", mode.message);
    for method in ["thread.send", "thread.new", "orchestrator.watch"] {
        let e = hub
            .orchestrator_acts(method, hub.orchestrator_call().as_ref())
            .unwrap_err();
        assert_eq!(e.code, codes::RELAY_FORBIDDEN, "{method}");
    }
    assert_eq!(hub.attention_list(false).len(), 1, "the item is still open");

    // It reports a completed turn; the hub records it interrupted, with no error or card.
    rt.end_turn(t1, "Approved.");
    let evs = until(&mut sub, is_kind_on("turn.ended", &orch)).await;
    let EventBody::TurnEnded(end) = &evs.last().unwrap().body else {
        panic!()
    };
    assert_eq!(end.result, TurnResult::Interrupted);
    assert_eq!(
        end.ext.as_ref().unwrap()["twr"]["runtime_result"],
        "completed"
    );
    let again = withdraw(&hub, "orchestrator", msg_ref);
    assert_eq!(again["reason"], "withdrawn");

    // The next turn is told, once.
    hub.say_to_orchestrator("what is new?", Some(head(&hub)), &tui())
        .unwrap();
    let (prompt, t2) = rt.next_turn().await;
    assert!(prompt.starts_with("[twr background]"), "{prompt}");
    assert!(
        prompt.contains("The user took back their message «approve api's request»"),
        "{prompt}"
    );
    assert!(prompt.ends_with("[user message]\nwhat is new?"), "{prompt}");
    // A message of the user's own turn has its authority again.
    assert!(hub
        .orchestrator_acts("orchestrator.watch", hub.orchestrator_call().as_ref())
        .is_ok());
    rt.end_turn(t2, "Nothing.");
    hub.say_to_orchestrator("and now?", Some(head(&hub)), &tui())
        .unwrap();
    let (prompt, t3) = rt.next_turn().await;
    assert!(!prompt.contains("took back"), "{prompt}");
    rt.end_turn(t3, "Still nothing.");
    hub.shutdown().await;
}

/// A change the orchestrator asked for in a turn whose message the user took back stays
/// refused when the hub gets to it after that turn ended (its peer is checked first, which
/// takes time), and so does one asked between that turn and the next. The next turn has its
/// authority again.
#[tokio::test]
async fn a_change_asked_in_a_taken_back_turn_stays_refused_after_it_ends() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let r = hub
        .say_to_orchestrator("rename api to backend", Some(head(&hub)), &tui())
        .unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    let received = hub.orchestrator_call();
    assert_eq!(received.as_ref().and_then(|c| c.turn()), Some(t1));
    withdraw(&hub, "orchestrator", r["ref"].as_i64().unwrap());
    assert_eq!(rt.next_cmd().await, AdapterCmd::Interrupt);
    rt.end_turn_as(t1, "", TurnResult::Interrupted);
    eventually(|| hub.orchestrator_call().is_some_and(|c| c.turn().is_none())).await;
    let late = hub
        .orchestrator_acts("thread.rename", received.as_ref())
        .unwrap_err();
    assert_eq!(late.code, codes::RELAY_FORBIDDEN);
    // Received between that turn and the next, checked once the next one runs.
    let idle = hub.orchestrator_call();
    assert_eq!(idle.as_ref().and_then(|c| c.turn()), None);
    assert_eq!(
        hub.orchestrator_acts("thread.rename", idle.as_ref())
            .unwrap_err()
            .code,
        codes::RELAY_FORBIDDEN
    );
    let r2 = hub
        .say_to_orchestrator("what is new?", Some(head(&hub)), &tui())
        .unwrap();
    let (_, t2) = rt.next_turn().await;
    let e = hub
        .orchestrator_acts("thread.rename", idle.as_ref())
        .unwrap_err();
    assert_eq!(e.code, codes::RELAY_FORBIDDEN);
    assert!(hub
        .orchestrator_acts("thread.rename", received.as_ref())
        .is_err());
    // Neither marked the new turn: its message can still be taken back.
    assert_eq!(
        withdraw(&hub, "orchestrator", r2["ref"].as_i64().unwrap())["state"],
        "running"
    );
    assert_eq!(rt.next_cmd().await, AdapterCmd::Interrupt);
    rt.end_turn_as(t2, "", TurnResult::Interrupted);
    drop(rt);
    hub.shutdown().await;
}

/// A turn the orchestrator's runtime starts on its own right after a taken-back one goes on
/// with that work: it is interrupted, changes nothing and ends `interrupted`. The message
/// taken back is told to the next turn the hub delivers a prompt for, not lost to the turn
/// the hub never prompted. A user message ends the taint.
#[tokio::test]
async fn a_runtime_turn_after_a_taken_back_one_is_taken_back_too() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let mut sub = hub.subscribe_control(0);
    let r = hub
        .say_to_orchestrator("rewrite the release plan", Some(head(&hub)), &tui())
        .unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    withdraw(&hub, "orchestrator", r["ref"].as_i64().unwrap());
    assert_eq!(rt.next_cmd().await, AdapterCmd::Interrupt);
    // (No text: its first text would be interrupted again, as a sign it runs.)
    rt.end.emit(
        EventBody::TurnEnded(TurnEnded {
            result: TurnResult::Interrupted,
            ..Default::default()
        }),
        Some(t1),
    );
    let orch = hub.orchestrator_thread().unwrap();
    until(&mut sub, is_kind_on("turn.ended", &orch)).await;
    let runtime_turn = |key: &str| {
        serde_json::json!({"twr": {"runtime_turn": key}})
            .as_object()
            .cloned()
    };
    // The runtime goes on by itself.
    rt.end.emit(
        EventBody::TurnStarted(TurnStarted {
            prompt: "continuing".into(),
            from: TurnFrom::Runtime,
            ext: runtime_turn("rt-1"),
            ..Default::default()
        }),
        None,
    );
    assert_eq!(rt.next_cmd().await, AdapterCmd::Interrupt);
    let call = hub.orchestrator_call();
    let t2 = call
        .as_ref()
        .and_then(|c| c.turn())
        .expect("its turn is open");
    assert_eq!(t2, t1 + 1);
    let e = hub
        .orchestrator_acts("thread.send", call.as_ref())
        .unwrap_err();
    assert_eq!(e.code, codes::RELAY_FORBIDDEN);
    rt.end.emit(
        EventBody::TurnEnded(TurnEnded {
            result: TurnResult::Completed,
            ext: runtime_turn("rt-1"),
            ..Default::default()
        }),
        None,
    );
    let evs = until(&mut sub, is_kind_on("turn.ended", &orch)).await;
    let EventBody::TurnEnded(end) = &evs.last().unwrap().body else {
        panic!()
    };
    assert_eq!(end.result, TurnResult::Interrupted);
    // The next prompt the hub delivers carries the fact.
    hub.say_to_orchestrator("what now?", Some(head(&hub)), &tui())
        .unwrap();
    let (prompt, t3) = rt.next_turn().await;
    assert!(
        prompt.contains("The user took back their message «rewrite the release plan»"),
        "{prompt}"
    );
    rt.end_turn(t3, "Nothing.");
    until(&mut sub, is_kind_on("turn.ended", &orch)).await;
    // After a user message, the runtime's own turn is its own again.
    rt.end.emit(
        EventBody::TurnStarted(TurnStarted {
            prompt: "follow-up".into(),
            from: TurnFrom::Runtime,
            ext: runtime_turn("rt-2"),
            ..Default::default()
        }),
        None,
    );
    eventually(|| {
        hub.orchestrator_call()
            .is_some_and(|c| c.turn() == Some(t3 + 1))
    })
    .await;
    assert!(hub
        .orchestrator_acts("thread.send", hub.orchestrator_call().as_ref())
        .is_ok());
    let cmd = timeout(Duration::from_millis(300), rt.end.cmds.recv()).await;
    assert!(cmd.is_err(), "no interrupt: {cmd:?}");
    drop(rt);
    hub.shutdown().await;
}

/// The fact that a message was taken back is told only once the runtime got the prompt that
/// carries it: a turn whose runtime could not start told nothing, and the next prompt tells
/// it.
#[tokio::test]
async fn a_prompt_that_never_reached_the_runtime_tells_nothing() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let mut sub = hub.subscribe_control(0);
    let r = hub
        .say_to_orchestrator("delete the old branches", Some(head(&hub)), &tui())
        .unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    withdraw(&hub, "orchestrator", r["ref"].as_i64().unwrap());
    assert_eq!(rt.next_cmd().await, AdapterCmd::Interrupt);
    let orch = hub.orchestrator_thread().unwrap();
    // Its runtime goes away; the next one cannot start.
    hub.stop_thread(&orch, "test").unwrap();
    drop(rt);
    until(&mut sub, is_kind_on("thread.stopped", &orch)).await;
    env.broken.store(true, std::sync::atomic::Ordering::SeqCst);
    hub.say_to_orchestrator("anything new?", Some(head(&hub)), &tui())
        .unwrap();
    let evs = until(&mut sub, is_kind_on("turn.ended", &orch)).await;
    let EventBody::TurnEnded(end) = &evs.last().unwrap().body else {
        panic!()
    };
    assert_eq!(end.result, TurnResult::Error);
    env.broken.store(false, std::sync::atomic::Ordering::SeqCst);
    hub.say_to_orchestrator("and now?", Some(head(&hub)), &tui())
        .unwrap();
    let mut rt = env.orch().await;
    let (prompt, t3) = rt.next_turn().await;
    assert_eq!(t3, t1 + 2);
    assert!(prompt.contains("«delete the old branches»"), "{prompt}");
    rt.end_turn(t3, "Nothing.");
    // Told: the next prompt does not repeat it.
    until(&mut sub, is_kind_on("turn.ended", &orch)).await;
    hub.say_to_orchestrator("thanks", Some(head(&hub)), &tui())
        .unwrap();
    let (prompt, _) = rt.next_turn().await;
    assert!(!prompt.contains("took back"), "{prompt}");
    drop(rt);
    hub.shutdown().await;
}

/// The fact that a message was taken back rides on every prompt the hub delivers until a
/// turn that carried it ends `completed` (as the runtime reported it). Any other end leaves it
/// pending, whatever came before it: an adapter that could not start the turn (an `error` end
/// with nothing before it, as Codex refusing `turn/start`), one that wrote a text of its own
/// and then failed (ACP failing to load the session), a prompt cancelled before it started
/// (Codex: `interrupted`), and a carrying turn taken back too whose runtime said `error`.
/// Once one completes, it is not repeated.
#[tokio::test]
async fn a_fact_is_told_once_a_turn_that_carried_it_completes() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let mut sub = hub.subscribe_control(0);
    let r = hub
        .say_to_orchestrator("drop the staging db", Some(head(&hub)), &tui())
        .unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    withdraw(&hub, "orchestrator", r["ref"].as_i64().unwrap());
    assert_eq!(rt.next_cmd().await, AdapterCmd::Interrupt);
    let orch = hub.orchestrator_thread().unwrap();
    let end = |rt: &OrchRt, turn: u64, result: TurnResult| {
        rt.end.emit(
            EventBody::TurnEnded(TurnEnded {
                result,
                ..Default::default()
            }),
            Some(turn),
        );
    };
    end(&rt, t1, TurnResult::Interrupted);
    until(&mut sub, is_kind_on("turn.ended", &orch)).await;
    let ask = |text: &str| {
        hub.say_to_orchestrator(text, Some(head(&hub)), &tui())
            .unwrap();
    };
    // Could not start: an `error` end and nothing else.
    ask("status?");
    let (prompt, t) = rt.next_turn().await;
    assert!(prompt.contains("«drop the staging db»"), "{prompt}");
    end(&rt, t, TurnResult::Error);
    until(&mut sub, is_kind_on("turn.ended", &orch)).await;
    // A text of the adapter's own, then an `error` end.
    ask("status, again?");
    let (prompt, t) = rt.next_turn().await;
    assert!(prompt.contains("«drop the staging db»"), "{prompt}");
    rt.end.emit(
        EventBody::AgentText(AgentText {
            text: "could not load the session".into(),
            is_final: true,
            ..Default::default()
        }),
        Some(t),
    );
    end(&rt, t, TurnResult::Error);
    until(&mut sub, is_kind_on("turn.ended", &orch)).await;
    // Cancelled before it started.
    ask("and now?");
    let (prompt, t) = rt.next_turn().await;
    assert!(prompt.contains("«drop the staging db»"), "{prompt}");
    end(&rt, t, TurnResult::Interrupted);
    until(&mut sub, is_kind_on("turn.ended", &orch)).await;
    // Taken back too: recorded `interrupted`, the runtime said `error`.
    let r2 = hub
        .say_to_orchestrator("never mind that", Some(head(&hub)), &tui())
        .unwrap();
    let (prompt, t) = rt.next_turn().await;
    assert!(prompt.contains("«drop the staging db»"), "{prompt}");
    withdraw(&hub, "orchestrator", r2["ref"].as_i64().unwrap());
    assert_eq!(rt.next_cmd().await, AdapterCmd::Interrupt);
    end(&rt, t, TurnResult::Error);
    let evs = until(&mut sub, is_kind_on("turn.ended", &orch)).await;
    assert!(matches!(&evs.last().unwrap().body,
        EventBody::TurnEnded(e) if e.result == TurnResult::Interrupted));
    // A turn that completes tells both.
    ask("anything?");
    let (prompt, t) = rt.next_turn().await;
    assert!(prompt.contains("«drop the staging db»"), "{prompt}");
    assert!(prompt.contains("«never mind that»"), "{prompt}");
    rt.end_turn(t, "Nothing.");
    until(&mut sub, is_kind_on("turn.ended", &orch)).await;
    ask("and?");
    let (prompt, _) = rt.next_turn().await;
    assert!(!prompt.contains("took back"), "not repeated: {prompt}");
    drop(rt);
    hub.shutdown().await;
}

/// A turn that a hub restart closed before its runtime showed it got the prompt told
/// nothing either: after the restart the next prompt tells it, as the log says.
#[tokio::test]
async fn a_prompt_cut_short_by_a_restart_tells_nothing() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let mut sub = hub.subscribe_control(0);
    let r = hub
        .say_to_orchestrator("archive the logs", Some(head(&hub)), &tui())
        .unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    withdraw(&hub, "orchestrator", r["ref"].as_i64().unwrap());
    assert_eq!(rt.next_cmd().await, AdapterCmd::Interrupt);
    let orch = hub.orchestrator_thread().unwrap();
    rt.end.emit(
        EventBody::TurnEnded(TurnEnded {
            result: TurnResult::Interrupted,
            ..Default::default()
        }),
        Some(t1),
    );
    until(&mut sub, is_kind_on("turn.ended", &orch)).await;
    hub.say_to_orchestrator("what about the logs?", Some(head(&hub)), &tui())
        .unwrap();
    let (prompt, _) = rt.next_turn().await;
    assert!(prompt.contains("«archive the logs»"), "{prompt}");
    // The hub goes before the runtime showed anything of that turn.
    drop(rt);
    hub.shutdown().await;
    drop((sub, hub));
    let hub = env.reopen().await;
    hub.say_to_orchestrator("still there?", Some(head(&hub)), &tui())
        .unwrap();
    let mut rt = env.orch().await;
    let (prompt, _) = rt.next_turn().await;
    assert!(prompt.contains("«archive the logs»"), "{prompt}");
    drop(rt);
    hub.shutdown().await;
}

/// A call from an orchestrator that was replaced while its request was checked changes
/// nothing, even with a turn number the new orchestrator's turn has too, and marks nothing of
/// the new one.
#[tokio::test]
async fn a_call_from_a_replaced_orchestrator_changes_nothing() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let r = hub
        .say_to_orchestrator("rename api to backend", Some(head(&hub)), &tui())
        .unwrap();
    let mut old = env.orch().await;
    let (_, t1) = old.next_turn().await;
    let received = hub.orchestrator_call();
    withdraw(&hub, "orchestrator", r["ref"].as_i64().unwrap());
    assert_eq!(old.next_cmd().await, AdapterCmd::Interrupt);
    old.end_turn_as(t1, "", TurnResult::Interrupted);
    hub.reset_orchestrator(&tui()).await.unwrap();
    drop(old);
    let r2 = hub
        .say_to_orchestrator("hello again", Some(head(&hub)), &tui())
        .unwrap();
    let mut new = env.orch().await;
    let (_, n1) = new.next_turn().await;
    assert_eq!(n1, t1, "the same turn number");
    let e = hub
        .orchestrator_acts("thread.rename", received.as_ref())
        .unwrap_err();
    assert_eq!(e.code, codes::UNAUTHORIZED);
    assert_eq!(
        withdraw(&hub, "orchestrator", r2["ref"].as_i64().unwrap())["state"],
        "running"
    );
    drop(new);
    hub.shutdown().await;
}

/// Whichever comes first wins: an orchestrator change asked before the withdrawal makes it too
/// late, and so does a tool call its runtime reported; a short command is applied at once and
/// has no ref to take back.
#[tokio::test]
async fn the_first_action_and_short_commands_make_it_too_late() {
    let mut env = Env::new();
    let hub = env.hub().await;
    env.thread(&hub, "api", "fake").await;
    let r = hub
        .say_to_orchestrator("start a thread for the docs", Some(head(&hub)), &tui())
        .unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    hub.orchestrator_acts("thread.new", hub.orchestrator_call().as_ref())
        .unwrap();
    let w = withdraw(&hub, "orchestrator", r["ref"].as_i64().unwrap());
    assert_eq!(
        (w["withdrawn"].as_bool(), w["reason"].as_str()),
        (Some(false), Some("acted")),
        "{w}"
    );
    assert_eq!(
        w["message"],
        "too late: the orchestrator already acted on it"
    );
    // What the orchestrator did is its own turn's: a worker's turn 1 is still untouched.
    let api = hub.send_by("api", "!sleep 5000", &tui()).unwrap();
    assert_eq!(api.turn, Some(t1));
    assert_eq!(withdraw(&hub, "api", api.msg_ref)["state"], "running");
    rt.end_turn(t1, "Started docs.");

    let r = hub
        .say_to_orchestrator("what is api doing?", Some(head(&hub)), &tui())
        .unwrap();
    let (_, t2) = rt.next_turn().await;
    rt.end.emit(
        EventBody::AgentToolCall(AgentToolCall {
            call_id: "c1".into(),
            tool: "mcp__twr__get_thread".into(),
            ..Default::default()
        }),
        Some(t2),
    );
    let orch = hub.orchestrator_thread().unwrap();
    eventually(|| hub.thread_view(&orch).unwrap().current_turn_acted).await;
    let w = withdraw(&hub, "orchestrator", r["ref"].as_i64().unwrap());
    assert_eq!(w["reason"], "acted", "{w}");
    rt.end_turn(t2, "Idle.");
    eventually(|| hub.thread_view(&orch).unwrap().current_turn.is_none()).await;
    let ended = withdraw(&hub, "orchestrator", r["ref"].as_i64().unwrap());
    assert_eq!(ended["reason"], "ended", "{ended}");

    let r = hub
        .say_to_orchestrator("set api to plan mode", Some(head(&hub)), &tui())
        .unwrap();
    assert_eq!(r["action"], "command", "{r}");
    assert!(r.get("ref").is_none(), "{r}");
    hub.shutdown().await;
}

/// A withdrawn user turn is left out of the orchestrator's conversation: what it was told goes
/// again with the next user turn, and a switch's handoff does not list its message.
#[tokio::test]
async fn updates_and_the_handoff_leave_a_withdrawn_turn_out() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let api = env.thread(&hub, "api", "fake").await;
    let mut sub = hub.subscribe_control(0);
    hub.say_to_orchestrator("hello", Some(head(&hub)), &tui())
        .unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    rt.end_turn(t1, "Hi.");
    hub.send_by(&api, "build it", &tui()).unwrap();
    until(&mut sub, is_kind_on("report.ready", &api)).await;

    let r = hub
        .say_to_orchestrator("first message", Some(head(&hub)), &tui())
        .unwrap();
    let (prompt, t2) = rt.next_turn().await;
    assert!(prompt.contains("api finished"), "{prompt}");
    withdraw(&hub, "orchestrator", r["ref"].as_i64().unwrap());
    assert_eq!(rt.next_cmd().await, AdapterCmd::Interrupt);
    rt.end_turn_as(t2, "", TurnResult::Interrupted);
    let orch = hub.orchestrator_thread().unwrap();
    until(&mut sub, is_kind_on("turn.ended", &orch)).await;

    hub.say_to_orchestrator("second message", Some(head(&hub)), &tui())
        .unwrap();
    let (prompt, t3) = rt.next_turn().await;
    assert!(prompt.contains("api finished"), "told again: {prompt}");
    assert!(prompt.contains("«first message»"), "{prompt}");
    rt.end_turn(t3, "Done.");
    until(&mut sub, is_kind_on("turn.ended", &orch)).await;

    hub.use_orchestrator(
        "claude-code",
        Some("sonnet"),
        SwitchOptions::no_summary(),
        &tui(),
    )
    .await
    .unwrap();
    let mut next = env.orch().await;
    let (handoff, _) = next.next_turn().await;
    assert!(handoff.contains("user: hello"), "{handoff}");
    assert!(handoff.contains("user: second message"), "{handoff}");
    assert!(!handoff.contains("first message"), "{handoff}");
    hub.shutdown().await;
}

/// The orchestrator's turn after a withdrawn one is told even when it is a hub notice.
#[tokio::test]
async fn a_notice_turn_after_a_withdrawn_one_is_told() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let api = env.thread(&hub, "api", "fake").await;
    let mut sub = hub.subscribe_control(0);
    let r = hub
        .say_to_orchestrator("deploy everything", Some(head(&hub)), &tui())
        .unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    withdraw(&hub, "orchestrator", r["ref"].as_i64().unwrap());
    assert_eq!(rt.next_cmd().await, AdapterCmd::Interrupt);
    rt.end_turn_as(t1, "", TurnResult::Interrupted);
    let orch = hub.orchestrator_thread().unwrap();
    until(&mut sub, is_kind_on("turn.ended", &orch)).await;
    // A worker's failure is an urgent notice: it goes out as the next turn.
    hub.send_by(&api, "!error boom", &tui()).unwrap();
    let (notice, _) = rt.next_turn().await;
    assert!(notice.starts_with("[twr background]"), "{notice}");
    assert!(notice.contains("«deploy everything»"), "{notice}");
    assert!(notice.contains("[twr notice]"), "{notice}");
    hub.shutdown().await;
}

/// A turn the hub started for the orchestrator (a shared `!!` command's output) that the
/// user stops is no message of theirs taken back: the next prompt says the user stopped a turn
/// the hub had started, and quotes none of its text as the user's.
#[tokio::test]
async fn a_hub_started_turn_stopped_is_not_a_message_taken_back() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let mut sub = hub.subscribe_control(0);
    hub.say_to_orchestrator("watch", Some(head(&hub)), &tui())
        .unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    rt.end_turn(t1, "Watching.");
    let orch = hub.orchestrator_thread().unwrap();
    until(&mut sub, is_kind_on("turn.ended", &orch)).await;
    let shared = ShellRun {
        thread: None,
        command: "echo deploy everything".into(),
        cwd: None,
        share: true,
    };
    hub.shell_run(shared, &tui()).unwrap();
    let (prompt, t2) = rt.next_turn().await;
    assert!(
        prompt.contains("[twr: the user ran a shell command and shared it with you]"),
        "{prompt}"
    );
    let started = events_of(&hub, &orch)
        .into_iter()
        .find(|e| e.kind() == "turn.started" && e.turn == Some(t2))
        .unwrap();
    assert_eq!(started.source, Source::Hub);
    let msg_ref = hub.thread_view(&orch).unwrap().current_turn_ref.unwrap();
    let w = withdraw(&hub, "orchestrator", msg_ref);
    assert_eq!(w["state"], "running", "{w}");
    assert_eq!(rt.next_cmd().await, AdapterCmd::Interrupt);
    rt.end_turn_as(t2, "", TurnResult::Interrupted);
    until(&mut sub, is_kind_on("turn.ended", &orch)).await;
    hub.say_to_orchestrator("what now?", Some(head(&hub)), &tui())
        .unwrap();
    let (prompt, t3) = rt.next_turn().await;
    assert!(
        prompt.contains("The user stopped a turn the hub had started (not a message of theirs)"),
        "{prompt}"
    );
    assert!(!prompt.contains("took back"), "{prompt}");
    assert!(!prompt.contains("deploy everything"), "{prompt}");
    rt.end_turn(t3, "Nothing.");
    hub.shutdown().await;
}

/// Adapters never write a withdrawal: only the user's `message.withdraw` does.
#[test]
fn adapters_cannot_emit_withdrawals() {
    assert!(!twr_hub::hub::adapter_may_emit(&EventBody::TurnWithdrawn(
        TurnWithdrawn::default()
    )));
    assert!(!twr_hub::hub::adapter_may_emit(
        &EventBody::PromptWithdrawn(PromptWithdrawn::default())
    ));
    // Both are on the control stream, for clients that track a message they sent.
    assert!(is_control_kind("turn.withdrawn") && is_control_kind("prompt.withdrawn"));
}

/// A compaction of the orchestrator's context that failed by its own report, its turn ended
/// `completed`, gets its error card from startup recovery when the hub stopped before it
/// recorded the card.
#[tokio::test]
async fn a_failed_orchestrator_compaction_gets_its_card_after_a_restart() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let mut sub = hub.subscribe_control(0);
    hub.say_to_orchestrator("hello", Some(head(&hub)), &tui())
        .unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    rt.end_turn(t1, "Hi.");
    let orch = hub.orchestrator_thread().unwrap();
    until(&mut sub, is_kind_on("turn.ended", &orch)).await;
    // The card cannot be recorded: as a hub that stops between the end and its card.
    let db = rusqlite::Connection::open(env.path().join("data").join("twr.db")).unwrap();
    db.busy_timeout(Duration::from_secs(5)).unwrap();
    db.execute_batch(
        "CREATE TRIGGER inject BEFORE INSERT ON events
         WHEN NEW.kind = 'attention.raised' AND NEW.data LIKE '%compacting its context failed%'
         BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;",
    )
    .unwrap();
    hub.compact_thread(&orch, &tui()).unwrap();
    let AdapterCmd::Compact { turn } = rt.next_cmd().await else {
        panic!("a compaction")
    };
    rt.end.emit(
        EventBody::ContextCompacted(ContextCompacted {
            trigger: "manual".into(),
            error: Some("conversation too short".into()),
            ..Default::default()
        }),
        Some(turn),
    );
    rt.end.emit(
        EventBody::TurnEnded(TurnEnded {
            result: TurnResult::Completed,
            ..Default::default()
        }),
        Some(turn),
    );
    until(&mut sub, |e| {
        e.kind() == "turn.ended" && e.turn == Some(turn)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let card = |hub: &Hub| {
        hub.attention_list(true).iter().any(|a| {
            a["summary"]
                .as_str()
                .is_some_and(|s| s.contains("compacting its context failed"))
        })
    };
    assert!(!card(&hub));
    drop(rt);
    hub.shutdown().await;
    drop((sub, hub));
    db.execute_batch("DROP TRIGGER inject").unwrap();
    drop(db);
    let hub = env.reopen().await;
    eventually(|| card(&hub)).await;
    hub.shutdown().await;
}

/// A compaction of the orchestrator's context that was cancelled (its compaction reports
/// `cancelled`, its turn ends `interrupted`) is no failure: no card, live or after a restart.
#[tokio::test]
async fn a_cancelled_orchestrator_compaction_gets_no_card_after_a_restart() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let mut sub = hub.subscribe_control(0);
    hub.say_to_orchestrator("hello", Some(head(&hub)), &tui())
        .unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    rt.end_turn(t1, "Hi.");
    let orch = hub.orchestrator_thread().unwrap();
    until(&mut sub, is_kind_on("turn.ended", &orch)).await;
    hub.compact_thread(&orch, &tui()).unwrap();
    let AdapterCmd::Compact { turn } = rt.next_cmd().await else {
        panic!("a compaction")
    };
    rt.end.emit(
        EventBody::ContextCompacted(ContextCompacted {
            trigger: "manual".into(),
            error: Some("cancelled".into()),
            ..Default::default()
        }),
        Some(turn),
    );
    rt.end.emit(
        EventBody::TurnEnded(TurnEnded {
            result: TurnResult::Interrupted,
            ..Default::default()
        }),
        Some(turn),
    );
    until(&mut sub, |e| {
        e.kind() == "turn.ended" && e.turn == Some(turn)
    })
    .await;
    let card = |hub: &Hub| {
        hub.attention_list(true).iter().any(|a| {
            a["summary"]
                .as_str()
                .is_some_and(|s| s.contains("compacting its context failed"))
        })
    };
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!card(&hub));
    drop(rt);
    hub.shutdown().await;
    drop((sub, hub));
    let hub = env.reopen().await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!card(&hub), "a cancelled compaction is no failure");
    hub.shutdown().await;
}

/// A compaction between a taken-back turn and what follows changes nothing of its taint: a
/// call made between them is refused, and the runtime's own turn after the compaction
/// continues the taken-back work (it is interrupted, and changes nothing).
#[tokio::test]
async fn a_compaction_keeps_a_taken_back_turns_taint() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let mut sub = hub.subscribe_control(0);
    let r = hub
        .say_to_orchestrator("rewrite the release plan", Some(head(&hub)), &tui())
        .unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    withdraw(&hub, "orchestrator", r["ref"].as_i64().unwrap());
    assert_eq!(rt.next_cmd().await, AdapterCmd::Interrupt);
    rt.end.emit(
        EventBody::TurnEnded(TurnEnded {
            result: TurnResult::Interrupted,
            ..Default::default()
        }),
        Some(t1),
    );
    let orch = hub.orchestrator_thread().unwrap();
    until(&mut sub, is_kind_on("turn.ended", &orch)).await;
    assert_eq!(
        hub.compact_thread(&orch, &tui()).unwrap()["state"],
        "started"
    );
    let AdapterCmd::Compact { turn: t2 } = rt.next_cmd().await else {
        panic!("a compaction")
    };
    rt.end.emit(
        EventBody::TurnEnded(TurnEnded {
            result: TurnResult::Completed,
            ..Default::default()
        }),
        Some(t2),
    );
    until(&mut sub, |e| e.kind() == "turn.ended" && e.turn == Some(t2)).await;
    // Between turns: bound to the compaction, which is the taken-back turn's.
    eventually(|| hub.orchestrator_call().is_some_and(|c| c.turn().is_none())).await;
    let idle = hub.orchestrator_call();
    assert_eq!(
        hub.orchestrator_acts("thread.rename", idle.as_ref())
            .unwrap_err()
            .code,
        codes::RELAY_FORBIDDEN
    );
    // The runtime goes on by itself: the taken-back work again.
    rt.end.emit(
        EventBody::TurnStarted(TurnStarted {
            prompt: "continuing".into(),
            from: TurnFrom::Runtime,
            ext: serde_json::json!({"twr": {"runtime_turn": "rt-1"}})
                .as_object()
                .cloned(),
            ..Default::default()
        }),
        None,
    );
    assert_eq!(rt.next_cmd().await, AdapterCmd::Interrupt);
    let call = hub.orchestrator_call();
    assert_eq!(call.as_ref().and_then(|c| c.turn()), Some(t2 + 1));
    assert_eq!(
        hub.orchestrator_acts("thread.send", call.as_ref())
            .unwrap_err()
            .code,
        codes::RELAY_FORBIDDEN
    );
    drop(rt);
    hub.shutdown().await;
}

/// A compaction of the orchestrator's context carries no prompt of the hub's: it tells no
/// fact of a message taken back, and its end leaves the fact to the next prompt.
#[tokio::test]
async fn a_compaction_tells_no_withdrawal_fact() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let mut sub = hub.subscribe_control(0);
    let r = hub
        .say_to_orchestrator("drop the staging db", Some(head(&hub)), &tui())
        .unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    withdraw(&hub, "orchestrator", r["ref"].as_i64().unwrap());
    assert_eq!(rt.next_cmd().await, AdapterCmd::Interrupt);
    let orch = hub.orchestrator_thread().unwrap();
    let end = |rt: &OrchRt, turn: u64| {
        rt.end.emit(
            EventBody::TurnEnded(TurnEnded {
                result: TurnResult::Completed,
                ..Default::default()
            }),
            Some(turn),
        );
    };
    end(&rt, t1);
    until(&mut sub, is_kind_on("turn.ended", &orch)).await;
    let c = hub.compact_thread(&orch, &tui()).unwrap();
    assert_eq!(c["state"], "started");
    let AdapterCmd::Compact { turn } = rt.next_cmd().await else {
        panic!("a compaction")
    };
    end(&rt, turn);
    until(&mut sub, |e| {
        e.kind() == "turn.ended" && e.turn == Some(turn)
    })
    .await;
    hub.say_to_orchestrator("status?", Some(head(&hub)), &tui())
        .unwrap();
    let (prompt, _) = rt.next_turn().await;
    assert!(prompt.contains("«drop the staging db»"), "{prompt}");
    drop(rt);
    hub.shutdown().await;
}
