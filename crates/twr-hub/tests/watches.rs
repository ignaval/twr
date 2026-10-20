//! Watches (`hub::watches`): the orchestrator asks to be woken when a worker finishes. A final
//! report fires a watch with a notice turn; an interim one (the turn yielded with background
//! tasks running) does not, its follow-up turn's report does, and a worker gone idle after an
//! interim report fires it once the grace has passed, unless the hub wakes it (`hub::wake`):
//! then the woken turn's report fires it. Watches replay from the log, go with their thread
//! and with a conversation reset.
//!
//! The orchestrator's runtime is a test adapter that hands every session to the test
//! ([`OrchRt`]); workers run the fake adapter, scripted when a test needs background work.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::timeout;
use twr_adapters::fake::{FakeAdapter, Step};
use twr_core::adapter::{Adapter, AdapterCmd, AdapterEnd, AdapterHandle, AdapterInfo, Answer};
use twr_core::adapter::{SpawnConfig, Tier};
use twr_core::config::Config;
use twr_core::event::*;
use twr_core::proto::codes;
use twr_hub::hub::{ControlSubscription, IDLE_GRACE, WATCH_NOTE_MAX};
use twr_hub::{Caller, Hub, HubOptions, NewThread};

const WAIT: Duration = Duration::from_secs(15);

/// One orchestrator runtime session handed to the test.
struct OrchRt {
    end: AdapterEnd,
    first: Option<(String, u64)>,
}

impl OrchRt {
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

    /// No turn comes within `wait`.
    async fn no_turn(&mut self, wait: Duration) {
        let got = timeout(wait, async {
            loop {
                if let Some(AdapterCmd::Send { text, .. }) = self.end.cmds.recv().await {
                    return text;
                }
            }
        })
        .await;
        if let Ok(text) = got {
            panic!("unexpected orchestrator turn: {text}");
        }
    }

    fn end_turn(&self, turn: u64, reply: &str) {
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
        let _ = self.0.send(OrchRt { end, first });
        Ok(h)
    }
}

struct Env {
    dir: tempfile::TempDir,
    spawns: mpsc::UnboundedReceiver<OrchRt>,
    tx: mpsc::UnboundedSender<OrchRt>,
}

impl Env {
    fn new() -> Env {
        let (tx, spawns) = mpsc::unbounded_channel();
        Env {
            dir: tempfile::tempdir().unwrap(),
            spawns,
            tx,
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// Workers play `script` when given, else the fake's prompt directives.
    fn opts(&self, script: Option<Vec<Step>>) -> HubOptions {
        let mut config = Config {
            default_runtime: "fake".into(),
            ..Default::default()
        };
        config.orchestrator.runtime = Some("claude-code".into());
        config.orchestrator.cwd = Some(self.path().to_path_buf());
        let mut o = HubOptions::new(self.path().join("data").join("twr.db"), config);
        let orch: Arc<dyn Adapter> = Arc::new(OrchAdapter(self.tx.clone()));
        let fake: Arc<dyn Adapter> = Arc::new(match script {
            Some(steps) => FakeAdapter::scripted(steps),
            None => FakeAdapter::new(),
        });
        o.adapters = Arc::new(move |name: &str| -> Option<Arc<dyn Adapter>> {
            match name {
                "fake" => Some(Arc::clone(&fake)),
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
        // The hub's own wake of an idle worker (`hub::wake`) is off but where a test says.
        o.wake_after = Some(Duration::ZERO);
        o
    }

    async fn hub(&self, script: Option<Vec<Step>>) -> Hub {
        Hub::open(self.opts(script)).await.unwrap()
    }

    async fn reopen(&self) -> Hub {
        timeout(WAIT, async {
            loop {
                match Hub::open(self.opts(None)).await {
                    Ok(h) => return h,
                    Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
                }
            }
        })
        .await
        .expect("reopening the hub")
    }

    async fn orch(&mut self) -> OrchRt {
        timeout(WAIT, self.spawns.recv())
            .await
            .expect("timed out waiting for the orchestrator to spawn")
            .unwrap()
    }

    async fn worker(&self, hub: &Hub, name: &str) -> String {
        let v = hub
            .new_thread(
                NewThread {
                    project: self.path().to_string_lossy().into_owned(),
                    runtime: Some("fake".into()),
                    name: Some(name.into()),
                    ..Default::default()
                },
                "test",
            )
            .await
            .unwrap();
        v["id"].as_str().unwrap().to_string()
    }

    /// Starts the orchestrator with a user turn; returns its runtime and that turn.
    async fn orchestrator(&mut self, hub: &Hub) -> (OrchRt, u64) {
        hub.say_to_orchestrator("watch the workers", None, &tui())
            .unwrap();
        let mut rt = self.orch().await;
        let (_, turn) = rt.next_turn().await;
        (rt, turn)
    }
}

fn tui() -> Caller {
    Caller::user("tui")
}

/// The next event `pred` matches.
async fn until(sub: &mut ControlSubscription, pred: impl Fn(&Event) -> bool) -> Event {
    timeout(WAIT, async {
        loop {
            let (_, ev) = sub.next().await.unwrap().expect("hub stopped");
            if pred(&ev) {
                return (*ev).clone();
            }
        }
    })
    .await
    .expect("timed out waiting for an event")
}

fn report_of(thread: &str, turn: u64) -> impl Fn(&Event) -> bool + '_ {
    move |e: &Event| e.kind() == "report.ready" && e.thread == thread && e.turn == Some(turn)
}

fn watch(hub: &Hub, thread: &str, note: &str, repeat: bool) -> Value {
    hub.watch_thread(thread, note, repeat, &Caller::Orchestrator)
        .unwrap()
}

fn watch_ids(hub: &Hub) -> Vec<String> {
    hub.watches()["watches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["watch"].as_str().unwrap().to_string())
        .collect()
}

/// The orchestrator's `turn.started` of `turn`.
fn orch_turn(hub: &Hub, turn: u64) -> TurnStarted {
    let orch = hub.orchestrator_thread().unwrap();
    hub.thread_events(&orch, 0, 10_000)
        .unwrap()
        .1
        .into_iter()
        .find_map(|e| match e.body {
            EventBody::TurnStarted(p) if e.turn == Some(turn) => Some(p),
            _ => None,
        })
        .unwrap()
}

/// When the orchestrator's turn `turn` started (log time).
fn started_at(hub: &Hub, turn: u64) -> chrono::DateTime<chrono::Utc> {
    let orch = hub.orchestrator_thread().unwrap();
    hub.thread_events(&orch, 0, 10_000)
        .unwrap()
        .1
        .into_iter()
        .find(|e| e.kind() == "turn.started" && e.turn == Some(turn))
        .unwrap()
        .ts
}

/// The conversation updates about `thread`.
fn updates(hub: &Hub, thread: &str) -> Vec<String> {
    let orch = hub.orchestrator_thread().unwrap();
    hub.thread_events(&orch, 0, 10_000)
        .unwrap()
        .1
        .into_iter()
        .filter_map(|e| match e.body {
            EventBody::ConversationUpdate(p) if p.about_thread.as_deref() == Some(thread) => {
                Some(p.text)
            }
            _ => None,
        })
        .collect()
}

/// The user's request ("when api is done, send its branch to the integrator") as a watch:
/// another worker's report and api's failed turn do not fire it; api's next finished turn
/// wakes the orchestrator with the report and the note, once.
#[tokio::test]
async fn a_final_report_wakes_the_orchestrator_once_with_the_watch_note() {
    let mut env = Env::new();
    let hub = env.hub(None).await;
    let api = env.worker(&hub, "api").await;
    let web = env.worker(&hub, "web").await;
    let (mut rt, t1) = env.orchestrator(&hub).await;
    let w = watch(&hub, "api", "send its branch to twr-integrator", false);
    assert_eq!(w["watch"], "w1");
    assert_eq!(w["thread"], api.as_str());
    assert_eq!(w["thread_name"], "api");
    assert_eq!(w["existing"], false);
    // Asked again: the same watch.
    let again = watch(&hub, "api", "send its branch to twr-integrator", false);
    assert_eq!(
        (&again["watch"], &again["existing"]),
        (&json!("w1"), &json!(true))
    );
    assert!(updates(&hub, &api)
        .iter()
        .any(|u| u.starts_with("Watching api: the orchestrator is woken the next time")));
    rt.end_turn(
        t1,
        "I'll send api's branch to the integrator when it finishes.",
    );

    let mut sub = hub.subscribe_control(0);
    // Another worker's success: no turn.
    hub.send_by(&web, "web is done", &tui()).unwrap();
    until(&mut sub, report_of(&web, 1)).await;
    rt.no_turn(Duration::from_millis(800)).await;

    // A failed turn: its error card notifies, the watch stays armed.
    hub.send_by(&api, "!error the build broke", &tui()).unwrap();
    let (notice, t2) = rt.next_turn().await;
    assert!(notice.contains("reason=error"), "{notice}");
    assert!(!notice.contains("watch="), "{notice}");
    rt.end_turn(t2, "api failed.");
    assert_eq!(watch_ids(&hub), ["w1"]);

    // Its next turn finishes: one notice turn with the report, the watch and its note.
    hub.send_by(&api, "Committed a1e8080 on twr/api. Tests pass.", &tui())
        .unwrap();
    let (notice, t3) = rt.next_turn().await;
    assert!(notice.starts_with("[twr notice] 1 update"), "{notice}");
    let line = notice
        .lines()
        .find(|l| l.starts_with("- thread=api report turn=2 grade=success"))
        .unwrap_or_else(|| panic!("{notice}"));
    assert!(line.contains("summary=\"Committed a1e8080"), "{line}");
    assert!(
        line.contains(" watch=w1 note=\"send its branch to twr-integrator\""),
        "{line}"
    );
    assert!(line.ends_with(&format!(" thread_id={api}")), "{line}");
    assert!(
        notice.contains("marks a watch you set with watch_thread"),
        "{notice}"
    );
    let started = orch_turn(&hub, t3);
    assert_eq!(
        started.ext.as_ref().unwrap()["twr"]["watches_fired"],
        json!(["w1"])
    );
    rt.end_turn(t3, "Sent twr/api to twr-integrator.");
    // Used up: api's next report wakes nobody.
    assert!(watch_ids(&hub).is_empty());
    hub.send_by(&api, "one more thing done", &tui()).unwrap();
    until(&mut sub, report_of(&api, 3)).await;
    rt.no_turn(Duration::from_millis(800)).await;
    drop(rt);
    hub.shutdown().await;
}

/// A worker tracked background work: tasks then the turn's end (an interim report).
fn interim_turn() -> Vec<Step> {
    vec![
        Step::EmitSession(EventBody::ThreadStarted(SessionPayload {
            session_id: Some("bg".into()),
            ..Default::default()
        })),
        Step::EmitSession(task(true)),
        Step::Emit(EventBody::AgentText(AgentText {
            text: "Tests are running in the background; I'll report when they finish.".into(),
            is_final: true,
            ..Default::default()
        })),
        Step::Emit(EventBody::TurnEnded(TurnEnded::default())),
    ]
}

fn task(running: bool) -> EventBody {
    EventBody::TaskUpdated(TaskUpdated {
        task_id: "tests".into(),
        running,
        description: "cargo test".into(),
        ..Default::default()
    })
}

/// The interim report fires nothing; the follow-up turn the runtime starts itself once the
/// tests end gets a final report that does, marked as a follow-up. The conversation tells the
/// user which is which.
#[tokio::test]
async fn a_watch_waits_out_an_interim_report_for_the_follow_up_turn() {
    let mut env = Env::new();
    let mut script = interim_turn();
    let mut ext = None;
    set_twr_ext(&mut ext, RUNTIME_TURN_KEY, "r1".into());
    script.extend([
        Step::Sleep(1000),
        Step::EmitSession(task(false)),
        Step::EmitSession(EventBody::TurnStarted(TurnStarted {
            from: TurnFrom::Runtime,
            prompt: "[runtime resumed after background activity]".into(),
            ext: ext.clone(),
            ..Default::default()
        })),
        Step::EmitSession(EventBody::AgentText(AgentText {
            text: "All tests passed. Committed b2c3d4e on twr/api.".into(),
            is_final: true,
            ..Default::default()
        })),
        Step::EmitSession(EventBody::TurnEnded(TurnEnded {
            ext,
            ..Default::default()
        })),
        Step::Sleep(60_000),
    ]);
    let hub = env.hub(Some(script)).await;
    let api = env.worker(&hub, "api").await;
    let (mut rt, t1) = env.orchestrator(&hub).await;
    watch(&hub, &api, "send its branch to twr-integrator", false);
    rt.end_turn(t1, "Watching api.");

    let mut sub = hub.subscribe_control(0);
    hub.send_by(&api, "run the tests", &tui()).unwrap();
    until(&mut sub, report_of(&api, 1)).await;
    let (notice, t2) = rt.next_turn().await;
    assert!(
        notice.contains("- thread=api report turn=2 follow_up=true grade=success"),
        "{notice}"
    );
    assert!(notice.contains("summary=\"All tests passed."), "{notice}");
    assert!(notice.contains(" watch=w1 "), "{notice}");
    assert!(!notice.contains("turn=1"), "{notice}");
    assert!(!notice.contains("idle:"), "{notice}");
    rt.end_turn(t2, "Sent.");

    let texts = updates(&hub, &api);
    assert!(
        texts.iter().any(|t| t.starts_with(
            "api yielded with 1 background task still running (interim report: the result \
             comes later) (success): Tests are running"
        )),
        "{texts:?}"
    );
    assert!(
        texts.iter().any(|t| t.starts_with(
            "api finished, in a follow-up turn it started after background work (success): \
             All tests passed."
        )),
        "{texts:?}"
    );
    // The idle line the task's end held was dropped by the follow-up turn: no second turn.
    rt.no_turn(IDLE_GRACE.to_std().unwrap() + Duration::from_millis(800))
        .await;
    drop(rt);
    hub.shutdown().await;
}

/// The tasks end and no follow-up turn starts: once the grace has passed, the watch fires on
/// the idle worker, quoting its interim report. A repeating watch stays.
#[tokio::test]
async fn an_idle_worker_after_an_interim_report_fires_its_watch_after_the_grace() {
    let mut env = Env::new();
    let mut script = interim_turn();
    script.extend([
        Step::Sleep(500),
        Step::EmitSession(task(false)),
        Step::Sleep(60_000),
    ]);
    let hub = env.hub(Some(script)).await;
    let api = env.worker(&hub, "api").await;
    let (mut rt, t1) = env.orchestrator(&hub).await;
    watch(&hub, "*", "tell the user what finished", true);
    rt.end_turn(t1, "Watching every worker.");

    let mut sub = hub.subscribe_control(0);
    hub.send_by(&api, "run the tests", &tui()).unwrap();
    let ended = until(&mut sub, |e| {
        matches!(&e.body, EventBody::TaskUpdated(t) if !t.running) && e.thread == api
    })
    .await;
    let (notice, t2) = rt.next_turn().await;
    // Held for the grace, by log time.
    assert!(
        started_at(&hub, t2) - ended.ts >= IDLE_GRACE,
        "held for the grace"
    );
    let line = notice
        .lines()
        .find(|l| l.starts_with("- thread=api idle:"))
        .unwrap_or_else(|| panic!("{notice}"));
    assert!(
        line.contains("its last report (turn 1) was interim"),
        "{line}"
    );
    assert!(line.contains("summary=\"Tests are running"), "{line}");
    assert!(
        line.contains(" watch=w1 note=\"tell the user what finished\""),
        "{line}"
    );
    assert!(line.ends_with(&format!(" thread_id={api}")), "{line}");
    rt.end_turn(t2, "api's tests ended.");
    assert_eq!(watch_ids(&hub), ["w1"]);
    assert!(updates(&hub, &api)
        .iter()
        .any(|t| t == "api background work finished."));
    drop(rt);
    hub.shutdown().await;
}

/// The tasks end and no follow-up turn starts, but the hub wakes the worker (`hub::wake`):
/// the idle line waits for that and goes with the woken turn's start; the watch fires on that
/// turn's report, a follow-up's. The orchestrator's conversation says the hub woke it.
#[tokio::test]
async fn a_worker_the_hub_wakes_fires_its_watch_with_the_woken_turns_report() {
    let mut env = Env::new();
    let mut script = interim_turn();
    script.extend([
        Step::Sleep(300),
        Step::EmitSession(task(false)),
        Step::AwaitSend,
        Step::Emit(EventBody::AgentText(AgentText {
            text: "All tests passed. Committed c3d4e5f.".into(),
            is_final: true,
            ..Default::default()
        })),
        Step::Emit(EventBody::TurnEnded(TurnEnded::default())),
        Step::Sleep(60_000),
    ]);
    let mut o = env.opts(Some(script));
    // Longer than the idle line's own grace: that line waits for the wake all the same.
    o.wake_after = Some(IDLE_GRACE.to_std().unwrap() + Duration::from_secs(1));
    let hub = Hub::open(o).await.unwrap();
    let api = env.worker(&hub, "api").await;
    let (mut rt, t1) = env.orchestrator(&hub).await;
    watch(&hub, &api, "send its branch to twr-integrator", false);
    rt.end_turn(t1, "Watching api.");

    hub.send_by(&api, "run the tests", &tui()).unwrap();
    let (notice, t2) = rt.next_turn().await;
    assert!(!notice.contains("idle:"), "{notice}");
    assert!(
        notice.contains("- thread=api report turn=2 follow_up=true grade=success"),
        "{notice}"
    );
    assert!(notice.contains(" watch=w1 "), "{notice}");
    rt.end_turn(t2, "Sent.");
    assert!(updates(&hub, &api).iter().any(|t| t
        .starts_with("api's background work finished and no turn started after it: twr woke it")));
    rt.no_turn(IDLE_GRACE.to_std().unwrap() + Duration::from_millis(800))
        .await;
    drop(rt);
    hub.shutdown().await;
}

/// The tasks end while the orchestrator is busy, and the idle line comes due behind its turn;
/// before that turn ends, the worker starts another task. It is not idle: the line goes, and
/// the watch fires only a full grace after the new task ends.
#[tokio::test]
async fn a_task_started_before_an_overdue_idle_line_goes_out_cancels_it() {
    let mut env = Env::new();
    let mut script = interim_turn();
    script.extend([
        Step::Sleep(300),
        Step::EmitSession(task(false)),
        Step::Sleep(IDLE_GRACE.num_milliseconds() as u64 + 1500),
        Step::EmitSession(lint(true)),
        Step::Sleep(3000),
        Step::EmitSession(lint(false)),
        Step::Sleep(60_000),
    ]);
    let hub = env.hub(Some(script)).await;
    let api = env.worker(&hub, "api").await;
    let (mut rt, t1) = env.orchestrator(&hub).await;
    watch(&hub, "*", "tell the user what finished", false);

    let mut sub = hub.subscribe_control(0);
    hub.send_by(&api, "run the tests", &tui()).unwrap();
    let is_lint = |running: bool| move |e: &Event| matches!(&e.body, EventBody::TaskUpdated(t) if t.task_id == "lint" && t.running == running);
    until(&mut sub, is_lint(true)).await;
    // The orchestrator's turn ends with the idle line long overdue: nothing to say now.
    rt.end_turn(t1, "Watching.");
    rt.no_turn(Duration::from_millis(1500)).await;
    let ended = until(&mut sub, is_lint(false)).await;
    let (notice, t2) = rt.next_turn().await;
    assert!(
        started_at(&hub, t2) - ended.ts >= IDLE_GRACE,
        "a new grace from the new task's end"
    );
    assert!(notice.contains("- thread=api idle:"), "{notice}");
    assert_eq!(notice.matches(" watch=w1 ").count(), 1, "{notice}");
    rt.end_turn(t2, "api is done.");
    drop(rt);
    hub.shutdown().await;
}

fn lint(running: bool) -> EventBody {
    EventBody::TaskUpdated(TaskUpdated {
        task_id: "lint".into(),
        running,
        description: "cargo clippy".into(),
        ..Default::default()
    })
}

/// Watches are in the log: a restart keeps them (and their numbering), unwatch and the
/// thread's deletion remove them, and a conversation reset drops them all.
#[tokio::test]
async fn watches_replay_and_go_with_unwatch_deletion_and_reset() {
    let mut env = Env::new();
    let hub = env.hub(None).await;
    let api = env.worker(&hub, "api").await;
    // No orchestrator yet: nothing to wake.
    let e = hub
        .watch_thread("api", "send it on", false, &Caller::Orchestrator)
        .unwrap_err();
    assert_eq!(e.code, codes::CONFLICT);
    let (rt, t1) = env.orchestrator(&hub).await;
    let orch = hub.orchestrator_thread().unwrap();
    for (thread, note) in [("api", " "), ("orchestrator", "x")] {
        let e = hub
            .watch_thread(thread, note, false, &Caller::Orchestrator)
            .unwrap_err();
        assert_eq!(e.code, codes::BAD_REQUEST, "{thread}");
    }
    let long = "x".repeat(WATCH_NOTE_MAX + 1);
    assert!(hub
        .watch_thread("api", &long, false, &Caller::Orchestrator)
        .is_err());
    watch(&hub, "api", "send its branch to twr-integrator", false);
    let all = watch(&hub, "*", "tell the user", true);
    assert_eq!((&all["watch"], &all["thread"]), (&json!("w2"), &json!("*")));
    rt.end_turn(t1, "Watching.");
    // The orchestrator's runtime dies, then the hub.
    let mut sub = hub.subscribe_control(0);
    drop(rt);
    until(&mut sub, |e| {
        e.kind() == "thread.crashed" && e.thread == orch
    })
    .await;
    drop(sub);
    drop(hub);

    let hub = env.reopen().await;
    assert_eq!(hub.orchestrator_thread().unwrap(), orch);
    let listed = hub.watches();
    assert_eq!(listed["watches"][0]["watch"], "w1");
    assert_eq!(listed["watches"][0]["thread_name"], "api");
    assert_eq!(
        listed["watches"][0]["note"],
        "send its branch to twr-integrator"
    );
    assert_eq!(listed["watches"][1]["thread"], "*");
    assert_eq!(listed["watches"][1]["repeat"], true);
    let r = hub.unwatch("w2", &Caller::Orchestrator).unwrap();
    assert_eq!(r["removed"], json!(["w2"]));
    assert_eq!(
        hub.unwatch("w2", &Caller::Orchestrator).unwrap_err().code,
        codes::NOT_FOUND
    );
    // Numbering goes on from the log.
    assert_eq!(watch(&hub, "*", "tell the user", true)["watch"], "w3");

    // api's deletion takes its watch (the "*" one stays).
    let mut sub = hub.subscribe_control(0);
    let card = hub.delete_threads(&["api".into()], &tui()).await.unwrap();
    hub.respond_at(
        card["attn_id"].as_str().unwrap(),
        Answer::new("Delete"),
        card["revision"].as_u64(),
        &tui(),
    )
    .unwrap();
    until(&mut sub, |e| {
        e.kind() == "thread.deleted" && e.thread == api
    })
    .await;
    assert_eq!(watch_ids(&hub), ["w3"]);

    // A reset starts a new conversation: its watches were the old one's.
    hub.reset_orchestrator(&tui()).await.unwrap();
    assert!(watch_ids(&hub).is_empty());
    drop(sub);
    drop(hub);
    let hub = env.reopen().await;
    assert!(watch_ids(&hub).is_empty());
    assert_eq!(watch(&hub, "*", "tell the user", true)["watch"], "w4");
    hub.shutdown().await;
}
