//! `thread.set_model` with a `runtime`: a worker thread moves to another runtime (`hub::switch`).
//! The old runtime stops (`runtime_change`, no stop of the thread), nothing restarts it, and the
//! thread's next turn starts a new session of the new runtime whose prompt carries the handoff.
//! The `rec` runtime records what each of its sessions was started with and sent.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::time::timeout;
use twr_adapters::fake::FakeAdapter;
use twr_core::adapter::{Adapter, AdapterCmd, AdapterHandle, AdapterInfo, SpawnConfig, Tier};
use twr_core::config::Config;
use twr_core::event::*;
use twr_core::proto::codes;
use twr_hub::hub::ThreadSubscription;
use twr_hub::{Caller, Hub, HubOptions, NewThread};

const WAIT: Duration = Duration::from_secs(10);

/// One session of `rec`: what it was started with, and every prompt it got.
#[derive(Debug, Clone, Default)]
struct Session {
    resumed: Option<String>,
    first: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    sent: Vec<String>,
}

/// Runtime `rec`: reports session `rec-<n>` (or the one it resumes), ends every turn at once
/// with the reply `done`, and records its sessions.
#[derive(Clone, Default)]
struct Recorder(Arc<Mutex<Vec<Session>>>);

impl Recorder {
    fn sessions(&self) -> Vec<Session> {
        self.0.lock().unwrap().clone()
    }
}

impl Adapter for Recorder {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "rec",
            tier: Tier::Headless,
            description: "records its sessions",
        }
    }

    fn spawn(&self, cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let (h, mut end) = AdapterHandle::pair();
        let index = {
            let mut s = self.0.lock().unwrap();
            s.push(Session {
                resumed: cfg.resume_session.clone(),
                first: prompt.clone(),
                model: cfg.model.clone(),
                effort: cfg.effort.clone(),
                sent: Vec::new(),
            });
            s.len() - 1
        };
        let sessions = Arc::clone(&self.0);
        tokio::spawn(async move {
            let session = cfg
                .resume_session
                .clone()
                .unwrap_or(format!("rec-{}", index + 1));
            end.emit(
                EventBody::ThreadStarted(SessionPayload {
                    session_id: Some(session.clone()),
                    ..Default::default()
                }),
                None,
            );
            let reply = |end: &mut twr_core::adapter::AdapterEnd, turn: u64| {
                end.emit(
                    EventBody::AgentText(AgentText {
                        text: "done".into(),
                        is_final: true,
                        ..Default::default()
                    }),
                    Some(turn),
                );
                end.emit(
                    EventBody::TurnEnded(TurnEnded {
                        result: TurnResult::Completed,
                        session_id: Some(session.clone()),
                        ..Default::default()
                    }),
                    Some(turn),
                );
            };
            if prompt.is_some() {
                reply(&mut end, cfg.prompt_turn.unwrap_or(1));
            }
            while let Some(cmd) = end.cmds.recv().await {
                match cmd {
                    AdapterCmd::Send { text, turn } => {
                        sessions.lock().unwrap()[index].sent.push(text);
                        reply(&mut end, turn);
                    }
                    AdapterCmd::Stop | AdapterCmd::Interrupt => break,
                    _ => {}
                }
            }
            end.exited(Some(0), "");
        });
        Ok(h)
    }
}

fn opts(dir: &Path, rec: &Recorder) -> HubOptions {
    let mut o = HubOptions::new(dir.join("data").join("twr.db"), Config::default());
    let rec = rec.clone();
    o.adapters = Arc::new(move |name: &str| -> Option<Arc<dyn Adapter>> {
        match name {
            "fake" => Some(Arc::new(FakeAdapter::new()) as Arc<dyn Adapter>),
            "rec" => Some(Arc::new(rec.clone())),
            // Never spawned here: threads of these only check what they take.
            "codex" => Some(Arc::new(twr_adapters::codex::CodexAdapter::new())),
            "claude-code" => Some(Arc::new(twr_adapters::ClaudeCodeAdapter::new())),
            "pty" => Some(Arc::new(twr_adapters::PtyAdapter::new())),
            _ => None,
        }
    });
    o.stop_timeout = Duration::from_secs(5);
    o
}

fn user() -> Caller {
    Caller::user("test")
}

async fn new_thread(hub: &Hub, dir: &Path, req: NewThread) -> String {
    let req = NewThread {
        project: dir.to_string_lossy().into_owned(),
        ..req
    };
    let v = hub.new_thread(req, "test").await.unwrap();
    v["id"].as_str().unwrap().to_string()
}

fn named(name: &str, runtime: &str) -> NewThread {
    NewThread {
        runtime: Some(runtime.into()),
        name: Some(name.into()),
        ..Default::default()
    }
}

fn send(hub: &Hub, thread: &str, prompt: &str) -> twr_hub::SendOutcome {
    hub.send(thread, prompt, TurnFrom::User, Source::User("test".into()))
        .unwrap()
}

/// Reads the thread's events until one matches `pred`; returns everything read.
async fn until(sub: &mut ThreadSubscription, pred: impl Fn(&Event) -> bool) -> Vec<Event> {
    let mut seen = Vec::new();
    timeout(WAIT, async {
        loop {
            let ev = sub.next().await.unwrap().expect("hub stopped");
            let done = pred(&ev);
            seen.push((*ev).clone());
            if done {
                return;
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        let kinds: Vec<&str> = seen.iter().map(|e: &Event| e.kind()).collect();
        panic!("timed out; saw {kinds:?}")
    });
    seen
}

fn kinds(evs: &[Event]) -> Vec<&str> {
    evs.iter().map(|e| e.kind()).collect()
}

/// A `report.ready` of turn `turn`.
fn report_of(turn: u64) -> impl Fn(&Event) -> bool {
    move |e| matches!(&e.body, EventBody::ReportReady(r) if r.turn.or(e.turn) == Some(turn))
}

fn restart_reason(ev: &Event) -> Option<String> {
    match &ev.body {
        EventBody::ThreadStopped(p) if is_settings_restart(p) => p.ext.as_ref()?["twr"]["reason"]
            .as_str()
            .map(str::to_string),
        _ => None,
    }
}

fn twr_ext(ext: &Option<JsonMap>) -> Value {
    ext.as_ref()
        .and_then(|e| e.get("twr"))
        .cloned()
        .unwrap_or(Value::Null)
}

/// An idle thread moves at once: its runtime stops for good (`runtime_change`), the next prompt
/// starts a new session of the new runtime (never a resume of the old session) with the handoff
/// before the prompt, recorded on the turn; the turn after that is a plain prompt.
#[tokio::test]
async fn an_idle_thread_moves_and_its_next_turn_gets_the_handoff() {
    let dir = tempfile::tempdir().unwrap();
    let rec = Recorder::default();
    let hub = Hub::open(opts(dir.path(), &rec)).await.unwrap();
    let t = new_thread(&hub, dir.path(), named("web", "fake")).await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "build the login page");
    until(&mut sub, report_of(1)).await;
    let before = hub.thread_view(&t).unwrap();
    let old_session = before.session_id.clone().expect("a session");

    let r = hub
        .set_runtime_model("web", Some("rec"), Some("rec-big"), Some("deep"), &user())
        .unwrap();
    assert_eq!(
        (&r["runtime"], &r["previous_runtime"], &r["runtime_changed"]),
        (&json!("rec"), &json!("fake"), &json!(true)),
        "{r}"
    );
    assert_eq!(
        (&r["model"], &r["effort"]),
        (&json!("rec-big"), &json!("deep"))
    );
    assert_eq!(
        (&r["changed"], &r["applies"]),
        (&json!(true), &json!("now"))
    );
    assert!(r["message"].as_str().unwrap().contains("handoff"), "{r}");

    let evs = until(&mut sub, |e| restart_reason(e).is_some()).await;
    assert_eq!(kinds(&evs), ["thread.model_changed", "thread.stopped"]);
    let EventBody::ThreadModelChanged(m) = &evs[0].body else {
        panic!()
    };
    assert_eq!(
        (m.runtime.as_deref(), m.previous_runtime.as_deref()),
        (Some("rec"), Some("fake"))
    );
    assert_eq!((m.previous_model.as_deref(), m.by), (None, ChangedBy::User));
    assert_eq!(twr_ext(&m.ext)["previous_session"], json!(old_session));
    assert_eq!(
        restart_reason(&evs[1]).as_deref(),
        Some(RUNTIME_RESTART_REASON)
    );
    // Not started again: there is no session of the new runtime to resume.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(hub.live_runtimes(), 0);
    assert!(rec.sessions().is_empty());
    let v = hub.thread_view(&t).unwrap();
    assert_eq!(v.runtime, "rec");
    assert_eq!((v.session_id.as_deref(), v.handoff_pending), (None, true));
    assert!(!v.stopped, "a runtime change is no stop");
    assert_eq!(
        (v.model.as_deref(), v.effort.as_deref()),
        (Some("rec-big"), Some("deep"))
    );
    assert_eq!((v.worktree, v.name), (before.worktree, before.name));

    // The next prompt: a new `rec` session, given the handoff first.
    send(&hub, &t, "now add a logout button");
    let evs = until(&mut sub, report_of(2)).await;
    let s = rec.sessions();
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].resumed, None, "never the old runtime's session");
    assert_eq!(
        (s[0].model.as_deref(), s[0].effort.as_deref()),
        (Some("rec-big"), Some("deep"))
    );
    let first = s[0].first.clone().expect("the turn came with the start");
    assert!(first.starts_with("[twr handoff]"), "{first}");
    assert!(
        first.ends_with("</twr-handoff>\n\nnow add a logout button"),
        "{first}"
    );
    for part in [
        "name: web",
        &format!("previous runtime: fake (model default, effort default, session {old_session})"),
        "now: rec (model rec-big, effort deep)",
        "## Task (the thread's first prompt)\nbuild the login page\n",
        "### turn 1 (user, completed,",
        "prompt: build the login page\nreply: build the login page\n",
    ] {
        assert!(first.contains(part), "{part:?} missing from:\n{first}");
    }
    let started = evs
        .iter()
        .find_map(|e| match &e.body {
            EventBody::TurnStarted(p) => Some(p.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(
        started.prompt, "now add a logout button",
        "the message as given"
    );
    let handoff = twr_ext(&started.ext)["runtime_handoff"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(format!("{handoff}now add a logout button"), first);
    let start = evs
        .iter()
        .find_map(|e| match &e.body {
            EventBody::ThreadStarted(p) => Some((e.source.clone(), p.clone())),
            _ => None,
        })
        .unwrap();
    assert_eq!(start.0, Source::Adapter("rec".into()));
    assert_eq!(twr_ext(&start.1.ext)["runtime"], "rec");
    let v = hub.thread_view(&t).unwrap();
    assert_eq!(
        (v.session_id.as_deref(), v.handoff_pending),
        (Some("rec-1"), false)
    );

    // Then the session goes on as usual: no handoff again.
    send(&hub, &t, "and a test");
    let evs = until(&mut sub, report_of(3)).await;
    assert_eq!(rec.sessions()[0].sent, ["and a test"]);
    assert!(evs.iter().all(|e| match &e.body {
        EventBody::TurnStarted(p) => twr_ext(&p.ext).get("runtime_handoff").is_none(),
        _ => true,
    }));
    hub.shutdown().await;
}

/// Mid-turn, the old runtime finishes its turn first; the prompt queued meanwhile then starts
/// the new runtime with a handoff that covers that last turn.
#[tokio::test]
async fn a_running_thread_moves_after_its_turn() {
    let dir = tempfile::tempdir().unwrap();
    let rec = Recorder::default();
    let hub = Hub::open(opts(dir.path(), &rec)).await.unwrap();
    let t = new_thread(&hub, dir.path(), named("api", "fake")).await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "!sleep 400\nmigrated the schema");
    until(&mut sub, |e| e.kind() == "thread.started").await;
    let r = hub
        .set_runtime_model("api", Some("rec"), None, None, &user())
        .unwrap();
    assert_eq!(r["applies"], "after_turn", "{r}");
    let queued = send(&hub, &t, "continue");
    assert_eq!(queued.turn, None, "queued behind the running turn");
    let evs = until(&mut sub, report_of(2)).await;
    let order: Vec<&str> = kinds(&evs)
        .into_iter()
        .filter(|k| {
            [
                "turn.ended",
                "thread.stopped",
                "turn.started",
                "thread.started",
            ]
            .contains(k)
        })
        .collect();
    assert_eq!(
        order,
        [
            "turn.ended",
            "thread.stopped",
            "turn.started",
            "thread.started",
            "turn.ended"
        ]
    );
    let first = rec.sessions()[0].first.clone().unwrap();
    assert!(
        first.contains("reply: migrated the schema\n"),
        "the last turn of the old runtime is in the handoff:\n{first}"
    );
    assert!(first.ends_with("continue"));
    hub.shutdown().await;
}

/// An attach while a move waits for the new runtime's first session is refused before anything
/// is stopped (`unsupported`: the thread has no session of its new runtime to open), even with
/// `force`: the previous runtime's session is not the thread's any more, and its turn runs on.
#[tokio::test]
async fn a_thread_moving_to_another_runtime_is_not_attached() {
    let dir = tempfile::tempdir().unwrap();
    let rec = Recorder::default();
    let hub = Hub::open(opts(dir.path(), &rec)).await.unwrap();
    let t = new_thread(&hub, dir.path(), named("api", "fake")).await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "!sleep 400\nworking");
    until(&mut sub, |e| e.kind() == "thread.started").await;
    let r = hub
        .set_runtime_model("api", Some("rec"), None, None, &user())
        .unwrap();
    assert_eq!(r["applies"], "after_turn", "{r}");
    for force in [false, true] {
        let e = hub.attach_begin("api", force, "test").await.unwrap_err();
        assert_eq!(e.code, codes::UNSUPPORTED, "{e:?}");
        assert!(e.message.contains("no runtime session yet"), "{e:?}");
    }
    let evs = until(&mut sub, |e| e.kind() == "turn.ended").await;
    assert!(
        evs.iter().any(|e| matches!(
            &e.body,
            EventBody::TurnEnded(p) if p.result == TurnResult::Completed
        )),
        "the previous runtime's turn was not interrupted: {:?}",
        kinds(&evs)
    );
    assert!(!kinds(&evs).contains(&"thread.attached"));
    hub.shutdown().await;
}

/// A move recorded while no runtime runs survives a hub restart: the new runtime still starts
/// with the handoff, and the thread's other settings stay.
#[tokio::test]
async fn a_pending_handoff_survives_a_hub_restart() {
    let dir = tempfile::tempdir().unwrap();
    let rec = Recorder::default();
    let t = {
        let hub = Hub::open(opts(dir.path(), &rec)).await.unwrap();
        let t = new_thread(&hub, dir.path(), named("docs", "fake")).await;
        let r = hub
            .set_runtime_model(&t, Some("rec"), None, Some("high"), &user())
            .unwrap();
        assert_eq!(r["applies"], "next_start", "{r}");
        hub.shutdown().await;
        t
    };
    let hub = Hub::open(opts(dir.path(), &rec)).await.unwrap();
    let v = hub.thread_view(&t).unwrap();
    assert_eq!((v.runtime.as_str(), v.handoff_pending), ("rec", true));
    assert_eq!(v.effort.as_deref(), Some("high"));
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "write the README");
    until(&mut sub, report_of(1)).await;
    let first = rec.sessions()[0].first.clone().unwrap();
    assert!(
        first.contains("## Task (the thread's first prompt)\n(no turn yet)\n"),
        "{first}"
    );
    assert!(first.contains("previous runtime: fake"), "{first}");
    assert!(first.ends_with("write the README"));
    hub.shutdown().await;
}

/// What a move takes and refuses: the orchestrator picks only agent runtimes; nothing moves to
/// `pty`; the thread's mode must be one the new runtime takes; a model or effort left out is the
/// new runtime's default; the thread's own runtime is no move.
#[tokio::test]
async fn moves_are_checked() {
    let dir = tempfile::tempdir().unwrap();
    let rec = Recorder::default();
    let hub = Hub::open(opts(dir.path(), &rec)).await.unwrap();
    let t = new_thread(&hub, dir.path(), named("web", "fake")).await;
    let orch = Caller::Orchestrator;
    for spec in ["pty:sh", "acp:sh -c x", "acp", "fake", "rec"] {
        let e = hub
            .set_runtime_model(&t, Some(spec), None, None, &orch)
            .unwrap_err();
        assert_eq!(e.code, codes::BAD_REQUEST, "{spec}");
        assert!(
            e.message.contains("the orchestrator cannot choose"),
            "{spec}: {e:?}"
        );
    }
    for spec in ["pty:sh", "nope"] {
        let e = hub
            .set_runtime_model(&t, Some(spec), None, None, &user())
            .unwrap_err();
        assert_eq!(e.code, codes::BAD_REQUEST, "{spec}");
    }
    // The thread's own runtime: an ordinary model change.
    let r = hub
        .set_runtime_model(&t, Some("fake"), Some("fake-big"), None, &user())
        .unwrap();
    assert_eq!(
        (&r["runtime_changed"], &r["model"]),
        (&json!(false), &json!("fake-big"))
    );
    // Named alone, nothing changes and nothing is recorded.
    let r = hub
        .set_runtime_model(&t, Some("fake"), None, None, &user())
        .unwrap();
    assert_eq!(r["changed"], json!(false), "{r}");
    assert!(
        r["message"]
            .as_str()
            .is_some_and(|m| m.contains("already runs on fake")),
        "{r}"
    );
    // A move can widen what the thread's mode lets it do unasked (runtimes differ in what one
    // mode lets through), so the orchestrator's move of a thread in a mode that is not a
    // restricted one goes through the `set_mode` gate: with no user turn of its own, refused.
    let e = hub
        .set_runtime_model(&t, Some("codex"), None, Some("ultra"), &orch)
        .unwrap_err();
    assert_eq!(e.code, codes::RELAY_FORBIDDEN, "{e:?}");
    assert_eq!(hub.thread_view(&t).unwrap().runtime, "fake");
    // In plan mode (no runtime lets it do more than read), the orchestrator moves it to an
    // agent runtime directly, recorded as its own; the model it had does not come along.
    hub.set_mode(&t, "plan", &user()).unwrap();
    let r = hub
        .set_runtime_model(&t, Some("codex"), None, Some("ultra"), &orch)
        .unwrap();
    assert_eq!(
        (&r["runtime"], &r["model"], &r["effort"]),
        (&json!("codex"), &Value::Null, &json!("ultra")),
        "{r}"
    );
    let evs = hub.thread_events(&t, 0, 1000).unwrap().1;
    let by = evs.iter().rev().find_map(|e| match &e.body {
        EventBody::ThreadModelChanged(p) => Some(p.by),
        _ => None,
    });
    assert_eq!(by, Some(ChangedBy::Orchestrator));
    // Efforts are checked against the new runtime.
    let e = hub
        .set_runtime_model(&t, Some("claude-code"), None, Some("ultra"), &user())
        .unwrap_err();
    assert!(e.message.contains("unknown reasoning effort"), "{e:?}");

    // A Codex-only permission mode does not move to Claude Code.
    let c = new_thread(
        &hub,
        dir.path(),
        NewThread {
            mode: Some("untrusted".into()),
            ..named("cx", "codex")
        },
    )
    .await;
    let e = hub
        .set_runtime_model(&c, Some("claude-code"), None, None, &user())
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST);
    assert!(e.message.contains("permission mode"), "{e:?}");

    // A pty thread may move away from its command line (to an agent).
    let p = new_thread(&hub, dir.path(), named("term", "pty:sh")).await;
    assert!(hub.set_model(&p, Some("x"), None, &user()).is_err());
    let r = hub
        .set_runtime_model(&p, Some("rec"), None, None, &user())
        .unwrap();
    assert_eq!(r["runtime_changed"], true, "{r}");
    let mut sub = hub.subscribe_thread(&p, 0).unwrap();
    send(&hub, &p, "hello");
    until(&mut sub, report_of(1)).await;
    assert_eq!(
        rec.sessions().len(),
        1,
        "a rec session, not the pty command"
    );

    // Nor is the orchestrator moved this way, by anyone.
    hub.shutdown().await;
}

/// The orchestrator may open a thread on an agent runtime it names (not a command line).
#[tokio::test]
async fn the_orchestrator_opens_threads_on_agent_runtimes() {
    let dir = tempfile::tempdir().unwrap();
    let rec = Recorder::default();
    let hub = Hub::open(opts(dir.path(), &rec)).await.unwrap();
    // A git repository: the orchestrator starts threads only in one (not the one holding the
    // hub's data directory).
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    for args in [
        &["init", "-q"][..],
        &["commit", "-q", "--allow-empty", "-m", "init"][..],
    ] {
        let out = std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t"])
            .args([
                "-c",
                "init.defaultBranch=main",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {out:?}");
    }
    let req = |runtime: &str| NewThread {
        project: repo.to_string_lossy().into_owned(),
        ..named(&format!("t-{}", runtime.len()), runtime)
    };
    let v = hub
        .new_thread_by(req("codex"), &Caller::Orchestrator)
        .await
        .unwrap();
    assert_eq!(v["runtime"], "codex");
    for spec in ["pty:sh", "acp:sh -c x", "fake"] {
        let e = hub
            .new_thread_by(req(spec), &Caller::Orchestrator)
            .await
            .unwrap_err();
        assert_eq!(e.code, codes::BAD_REQUEST, "{spec}");
    }
    hub.shutdown().await;
}

/// Two moves before the new runtime started (fake → codex → rec): the handoff names the
/// runtime and session that did the work (fake), not the one in between that never ran.
#[tokio::test]
async fn a_second_move_before_a_start_keeps_the_previous_session_in_the_handoff() {
    let dir = tempfile::tempdir().unwrap();
    let rec = Recorder::default();
    let hub = Hub::open(opts(dir.path(), &rec)).await.unwrap();
    let t = new_thread(&hub, dir.path(), named("web", "fake")).await;
    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    send(&hub, &t, "build the login page");
    until(&mut sub, report_of(1)).await;
    let old_session = hub.thread_view(&t).unwrap().session_id.expect("a session");
    hub.set_runtime_model("web", Some("codex"), None, None, &user())
        .unwrap();
    until(&mut sub, |e| restart_reason(e).is_some()).await;
    let r = hub
        .set_runtime_model("web", Some("rec"), None, None, &user())
        .unwrap();
    assert_eq!(r["previous_runtime"], json!("codex"), "{r}");
    send(&hub, &t, "go on");
    until(&mut sub, report_of(2)).await;
    let first = rec.sessions()[0].first.clone().expect("the handoff");
    assert!(
        first.contains(&format!(
            "previous runtime: fake (model default, effort default, session {old_session})"
        )),
        "{first}"
    );
    hub.shutdown().await;
}

/// A long thread: the handoff is built from indexed lookups of the newest turns that fit, not
/// by scanning the log once per turn (it is built under the hub's lock), and it still carries
/// those newest turns, each cut, with the task and the count of the older ones dropped.
#[tokio::test]
async fn the_handoff_of_a_long_thread_is_built_quickly() {
    let dir = tempfile::tempdir().unwrap();
    let rec = Recorder::default();
    let hub = Hub::open(opts(dir.path(), &rec)).await.unwrap();
    let t = new_thread(&hub, dir.path(), named("long", "fake")).await;
    const TURNS: u64 = 600;
    let append = |turn: u64, body: EventBody| {
        hub.append(&t, Some(turn), Source::Adapter("fake".into()), body, None)
            .unwrap();
    };
    for turn in 1..=TURNS {
        let prompt = if turn == TURNS {
            // Far past what a handoff quotes of one prompt (and past the payload cap).
            format!("task {turn} {}", "x".repeat(200 * 1024))
        } else {
            format!("task {turn}")
        };
        append(
            turn,
            EventBody::TurnStarted(TurnStarted {
                prompt,
                from: TurnFrom::User,
                ..Default::default()
            }),
        );
        for i in 0..5 {
            append(
                turn,
                EventBody::AgentText(AgentText {
                    text: format!("thinking {i}"),
                    ..Default::default()
                }),
            );
        }
        // Odd turns record no final reply: the lookup must not walk the log for one.
        if turn % 2 == 0 {
            append(
                turn,
                EventBody::AgentText(AgentText {
                    text: format!("reply {turn}"),
                    is_final: true,
                    ..Default::default()
                }),
            );
        }
        append(
            turn,
            EventBody::TurnEnded(TurnEnded {
                result: TurnResult::Completed,
                ..Default::default()
            }),
        );
    }
    assert_eq!(hub.thread_view(&t).unwrap().turns, TURNS);
    hub.set_runtime_model(&t, Some("rec"), None, None, &user())
        .unwrap();

    let mut sub = hub.subscribe_thread(&t, 0).unwrap();
    let start = std::time::Instant::now();
    send(&hub, &t, "carry on");
    let took = start.elapsed();
    until(&mut sub, report_of(TURNS + 1)).await;
    assert!(took < Duration::from_secs(2), "the handoff took {took:?}");

    let first = rec.sessions()[0].first.clone().expect("the handoff");
    for part in [
        "## Task (the thread's first prompt)\ntask 1\n",
        &format!("### turn {TURNS} (user, completed,"),
        &format!("reply: reply {TURNS}\n"),
        &format!("### turn {} (user, completed,", TURNS - 1),
        "reply: (no reply recorded)\n",
        "older turns dropped to fit the handoff)\n",
    ] {
        assert!(first.contains(part), "{part:?} missing from:\n{first}");
    }
    assert!(!first.contains("### turn 1 ("), "{first}");
    assert!(first.len() < 32 * 1024, "{} bytes", first.len());
    // Oldest first: the newest turn comes last.
    let older = first.find(&format!("### turn {} (", TURNS - 1)).unwrap();
    let newest = first.find(&format!("### turn {TURNS} (")).unwrap();
    assert!(older < newest, "{first}");
    hub.shutdown().await;
}
