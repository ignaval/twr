use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use twr_adapters::fake::{FakeAdapter, Step};
use twr_core::adapter::Adapter;
use twr_core::config::Config;
use twr_core::event::*;
use twr_core::reduce::ThreadStatus;
use twr_hub::{Caller, Hub, HubOptions, NewThread};

fn task(id: &str, running: bool) -> EventBody {
    EventBody::TaskUpdated(TaskUpdated {
        task_id: id.into(),
        running,
        description: "review and tests".into(),
        ..Default::default()
    })
}

fn script(tail: Vec<Step>) -> Vec<Step> {
    let mut steps = vec![
        Step::EmitSession(EventBody::ThreadStarted(SessionPayload {
            session_id: Some("background-session".into()),
            ..Default::default()
        })),
        Step::EmitSession(task("review", true)),
        Step::EmitSession(task("tests", true)),
        Step::Emit(EventBody::AgentText(AgentText {
            text: "Review is running; I will pick it up when it finishes.".into(),
            is_final: true,
            ..Default::default()
        })),
        Step::Emit(EventBody::TurnEnded(TurnEnded::default())),
    ];
    steps.extend(tail);
    steps
}

fn options(dir: &Path, steps: Vec<Step>) -> HubOptions {
    options_as(dir, steps, "fake")
}

/// [`options`] with the scripted fake standing in for runtime `runtime` (its modes are that
/// runtime's: what a restart compares).
fn options_as(dir: &Path, steps: Vec<Step>, runtime: &'static str) -> HubOptions {
    let mut opts = HubOptions::new(dir.join("hub.db"), Config::default());
    let adapter: Arc<dyn Adapter> = Arc::new(FakeAdapter::scripted(steps));
    opts.adapters = Arc::new(move |name: &str| (name == runtime).then(|| adapter.clone()));
    opts.git_tick = Some(Duration::ZERO);
    opts.trash_tick = Some(Duration::ZERO);
    opts
}

async fn start(hub: &Hub, dir: &Path) -> String {
    start_as(hub, dir, "fake", None).await
}

/// [`start`] on runtime `runtime` in mode `mode` (none: the default).
async fn start_as(hub: &Hub, dir: &Path, runtime: &str, mode: Option<&str>) -> String {
    let result = hub
        .new_thread(
            NewThread {
                project: dir.to_string_lossy().into(),
                name: Some("worker".into()),
                runtime: Some(runtime.into()),
                mode: mode.map(str::to_string),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap();
    let id = result["id"].as_str().unwrap().to_string();
    hub.send(
        &id,
        "review and test",
        TurnFrom::User,
        Source::User("test".into()),
    )
    .unwrap();
    wait(hub, &id, |e| e.kind() == "report.ready").await;
    id
}

/// An orchestrator thread, stopped (no notice turns), so the hub writes its conversation
/// updates.
fn orchestrator(hub: &Hub) -> String {
    let id = "orch".to_string();
    for body in [
        EventBody::ThreadCreated(ThreadCreated {
            name: "orchestrator".into(),
            box_name: "local".into(),
            runtime: "fake".into(),
            ext: Some(
                serde_json::from_value(serde_json::json!({"twr": {"role": "orchestrator"}}))
                    .unwrap(),
            ),
            ..Default::default()
        }),
        EventBody::ThreadStopped(Default::default()),
    ] {
        hub.append(&id, None, Source::Hub, body, None).unwrap();
    }
    id
}

/// The conversation's line about the report of `thread` whose summary starts with `summary`.
fn report_update(hub: &Hub, orch: &str, thread: &str, summary: &str) -> String {
    hub.thread_events(orch, 0, 10_000)
        .unwrap()
        .1
        .into_iter()
        .rev()
        .find_map(|e| match e.body {
            EventBody::ConversationUpdate(p)
                if p.about_thread.as_deref() == Some(thread)
                    && p.text.contains(&format!("): {summary}")) =>
            {
                Some(p.text)
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("no update about the report {summary:?}"))
}

async fn wait(hub: &Hub, id: &str, predicate: impl Fn(&Event) -> bool) -> Event {
    let mut sub = hub.subscribe_thread(id, 0).unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = sub.next().await.unwrap().unwrap();
            if predicate(&event) {
                return (*event).clone();
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn fake_background_remains_active_then_resumes_without_a_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(options(
        dir.path(),
        script(vec![
            Step::Sleep(1500),
            Step::EmitSession(task("review", false)),
            Step::EmitSession(task("tests", false)),
            Step::EmitSession(EventBody::TurnStarted(TurnStarted {
                from: TurnFrom::Runtime,
                prompt: "review completed".into(),
                ..Default::default()
            })),
            Step::Sleep(1500),
            Step::EmitSession(EventBody::AgentText(AgentText {
                text: "Review and tests passed.".into(),
                is_final: true,
                ..Default::default()
            })),
            Step::EmitSession(EventBody::TurnEnded(TurnEnded::default())),
        ]),
    ))
    .await
    .unwrap();
    let id = start(&hub, dir.path()).await;
    let view = hub.thread_view(&id).unwrap();
    assert_eq!(view.status(), ThreadStatus::Background);
    assert!(view.current_turn.is_none());
    assert_eq!(hub.get_thread(&id).unwrap()["background_task_count"], 2);
    assert_eq!(hub.list_threads()[0]["status"], "background");
    assert!(hub.thread_activity()[&id]["doing"]
        .as_str()
        .unwrap()
        .contains("working in background"));
    assert_eq!(
        view.last_report.unwrap().ext.unwrap()["twr"]["background_tasks"],
        2
    );
    let event = wait(
        &hub,
        &id,
        |e| matches!(&e.body, EventBody::TurnStarted(t) if t.from == TurnFrom::Runtime),
    )
    .await;
    assert_eq!(event.turn, Some(2));
    assert_eq!(
        hub.thread_view(&id).unwrap().status(),
        ThreadStatus::Running
    );
    wait(&hub, &id, |e| {
        e.kind() == "report.ready" && e.turn == Some(2)
    })
    .await;
    assert_eq!(hub.thread_view(&id).unwrap().status(), ThreadStatus::Done);
    assert_eq!(hub.get_thread(&id).unwrap()["background_task_count"], 0);
    hub.shutdown().await;
}

#[tokio::test]
async fn a_report_written_while_background_tasks_run_stays_interim() {
    use twr_core::notify::{NotifyEvent, Policy};
    use twr_core::reduce::{AttentionStack, Registry};

    for grade in [Grade::Success, Grade::GaveUp, Grade::NeedsVerification] {
        let dir = tempfile::tempdir().unwrap();
        let hub = Hub::open(options(dir.path(), script(vec![Step::Sleep(60000)])))
            .await
            .unwrap();
        let id = start(&hub, dir.path()).await;
        let written = hub
            .write_report(&id, Some(1), "Looks done.", grade, &Caller::Orchestrator)
            .unwrap();
        // No review either: the runtime's turn after the tasks reports again.
        assert!(written["review"].is_null(), "{grade:?}");
        let report = hub.thread_view(&id).unwrap().last_report.unwrap();
        assert_eq!(report.written_by, "orchestrator");
        assert_eq!(report.ext.unwrap()["twr"]["background_tasks"], 2);
        // Nothing tells the user about the worker: its tasks still run.
        let (items, _) = hub.control_read(0, 1000).unwrap();
        let (mut registry, mut attention) = (Registry::new(), AttentionStack::new());
        let mut policy = Policy::new(NotifyEvent::DEFAULTS);
        let mut reports = 0;
        for (gseq, ev) in &items {
            reports += usize::from(ev.kind() == "report.ready");
            registry.apply(ev);
            attention.apply_at(ev, Some(*gseq));
            // At most a revision of the interim report, which refreshes only what is held.
            let c = policy.observe(ev, Some(*gseq), &registry, &attention);
            assert!(c.as_ref().is_none_or(|c| c.revision), "{grade:?} {c:?}");
        }
        assert_eq!(reports, 2);
        hub.shutdown().await;
    }
}

#[tokio::test]
async fn stop_park_crash_and_hub_restart_clear_background_work() {
    for action in ["stop", "park", "crash", "shutdown"] {
        let dir = tempfile::tempdir().unwrap();
        let tail = if action == "crash" {
            vec![
                Step::Sleep(1500),
                Step::Exit {
                    code: Some(1),
                    stderr_tail: "runtime crashed".into(),
                },
            ]
        } else {
            vec![Step::Sleep(60000)]
        };
        let steps = script(tail);
        let hub = Hub::open(options(dir.path(), steps.clone())).await.unwrap();
        let id = start(&hub, dir.path()).await;
        assert_eq!(
            hub.thread_view(&id).unwrap().status(),
            ThreadStatus::Background
        );
        match action {
            "stop" => {
                hub.stop_thread(&id, "test").unwrap();
                wait(&hub, &id, |e| e.kind() == "thread.stopped").await;
            }
            "park" => {
                assert!(hub
                    .park_thread(&id, true, false, &Caller::user("test"))
                    .await
                    .is_err());
                assert_eq!(
                    hub.thread_view(&id).unwrap().status(),
                    ThreadStatus::Background
                );
                hub.park_thread(&id, true, true, &Caller::user("test"))
                    .await
                    .unwrap();
            }
            "crash" => {
                wait(&hub, &id, |e| e.kind() == "thread.crashed").await;
            }
            _ => {
                hub.shutdown().await;
            }
        }
        let view = hub.thread_view(&id).unwrap();
        assert!(view.background_tasks.is_empty(), "{action}");
        assert!(!view.has_work());
        hub.shutdown().await;
        drop(hub);
        let replayed = Hub::open(options(dir.path(), steps)).await.unwrap();
        assert!(
            replayed
                .thread_view(&id)
                .unwrap()
                .background_tasks
                .is_empty(),
            "replay after {action}"
        );
        replayed.shutdown().await;
    }
}

#[tokio::test]
async fn startup_clears_tasks_left_in_an_unclosed_runtime_log() {
    let dir = tempfile::tempdir().unwrap();
    let opts = || options(dir.path(), Vec::new());
    let hub = Hub::open(opts()).await.unwrap();
    let created = hub
        .new_thread(
            NewThread {
                project: dir.path().to_string_lossy().into(),
                runtime: Some("fake".into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap();
    // Simulate the durable state left by a hub dying after a task launch, without a
    // runtime exit frame or an open turn. There is no process left to adopt or reap.
    hub.append(
        id,
        None,
        Source::Adapter("fake".into()),
        EventBody::ThreadStarted(SessionPayload::default()),
        None,
    )
    .unwrap();
    hub.append(
        id,
        None,
        Source::Adapter("fake".into()),
        task("orphan", true),
        None,
    )
    .unwrap();
    assert_eq!(
        hub.thread_view(id).unwrap().status(),
        ThreadStatus::Background
    );
    hub.shutdown().await;
    drop(hub);
    let replayed = Hub::open(opts()).await.unwrap();
    assert!(!replayed.thread_view(id).unwrap().has_work());
    assert!(replayed
        .thread_view(id)
        .unwrap()
        .background_tasks
        .is_empty());
    replayed.shutdown().await;
}

#[tokio::test]
async fn stale_background_tracking_stops_the_runtime_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut opts = options(dir.path(), script(vec![Step::Sleep(60000)]));
    opts.config.background_timeout_secs = 1;
    let hub = Hub::open(opts).await.unwrap();
    let id = start(&hub, dir.path()).await;
    wait(&hub, &id, |e| e.kind() == "thread.stopped").await;
    assert!(!hub.thread_view(&id).unwrap().has_work());
    let (_, events) = hub.thread_events(&id, 0, 1000).unwrap();
    let warnings = events.iter().filter(|e| matches!(&e.body, EventBody::AttentionRaised(a) if a.summary.contains("liveness signal"))).count();
    assert_eq!(warnings, 1);
    hub.shutdown().await;
}

fn db_exec(dir: &Path, sql: &str) {
    let c = rusqlite::Connection::open(dir.join("hub.db")).unwrap();
    c.busy_timeout(Duration::from_secs(5)).unwrap();
    c.execute_batch(sql).unwrap();
}

/// The timeout's stop and the error item saying why are one transaction: when the item cannot
/// be recorded, nothing of the stop is, and a later check stops the thread with its item.
#[tokio::test]
async fn a_timeout_stop_is_never_recorded_without_its_error_item() {
    let dir = tempfile::tempdir().unwrap();
    let mut opts = options(dir.path(), script(vec![Step::Sleep(60000)]));
    opts.config.background_timeout_secs = 1;
    let hub = Hub::open(opts).await.unwrap();
    db_exec(
        dir.path(),
        "CREATE TRIGGER inject BEFORE INSERT ON events
         WHEN NEW.kind = 'attention.raised' AND NEW.data LIKE '%liveness signal%'
         BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;",
    );
    let id = start(&hub, dir.path()).await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    let (_, events) = hub.thread_events(&id, 0, 1000).unwrap();
    assert!(!events.iter().any(|e| e.kind() == "thread.stopped"));
    assert_eq!(
        hub.thread_view(&id).unwrap().status(),
        ThreadStatus::Background
    );
    assert_eq!(hub.get_thread(&id).unwrap()["live"], true);
    let c = rusqlite::Connection::open(dir.path().join("hub.db")).unwrap();
    let stops: i64 = c
        .query_row("SELECT COUNT(*) FROM stop_requests", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stops, 0);
    db_exec(dir.path(), "DROP TRIGGER inject");
    wait(&hub, &id, |e| e.kind() == "thread.stopped").await;
    let (_, events) = hub.thread_events(&id, 0, 1000).unwrap();
    let warning = events
        .iter()
        .position(|e| matches!(&e.body, EventBody::AttentionRaised(a) if a.summary.contains("liveness signal")))
        .expect("the stop's error item");
    let stopped = events
        .iter()
        .position(|e| e.kind() == "thread.stopped")
        .unwrap();
    assert!(warning < stopped);
    hub.shutdown().await;
}

/// A runtime's own turn that raced a prompt the hub had already started as turn 2 (its
/// `turn.started` reached the hub after the prompt's) is not recorded: its output goes to the
/// open turn, and its end, which names it (`ext.twr.runtime_turn`), closes nothing; its usage
/// counts for turn 2, once, which ends with the prompt's end. The next turn the runtime starts
/// on its own is turn 3: only its own end closes it, and only its own usage counts.
#[tokio::test]
async fn a_runtime_turn_that_raced_a_dispatched_prompt_never_takes_its_number() {
    let dir = tempfile::tempdir().unwrap();
    let text = |t: &str| {
        EventBody::AgentText(AgentText {
            text: t.into(),
            is_final: true,
            ..Default::default()
        })
    };
    let tagged = |id: &str| {
        let mut ext = None;
        set_twr_ext(&mut ext, RUNTIME_TURN_KEY, id.into());
        ext
    };
    let runtime_turn = |id: &str| {
        EventBody::TurnStarted(TurnStarted {
            from: TurnFrom::Runtime,
            prompt: "[runtime resumed after background activity]".into(),
            ext: tagged(id),
            ..Default::default()
        })
    };
    let end = |id: Option<&str>, input, output, cost| {
        EventBody::TurnEnded(TurnEnded {
            usage: Some(Usage {
                input,
                output,
                cost: Some(cost),
            }),
            ext: id.and_then(tagged),
            ..Default::default()
        })
    };
    let hub = Hub::open(options(
        dir.path(),
        script(vec![
            Step::AwaitSend,
            Step::EmitSession(runtime_turn("r1")),
            Step::EmitSession(text("review finished")),
            Step::EmitSession(end(Some("r1"), 10, 5, 0.25)),
            Step::Emit(text("answer to the prompt")),
            Step::Emit(end(None, 7, 3, 0.5)),
            Step::EmitSession(runtime_turn("r2")),
            Step::EmitSession(end(Some("r1"), 1, 1, 0.0)),
            Step::EmitSession(end(Some("r2"), 2, 2, 0.0)),
            Step::Sleep(60000),
        ]),
    ))
    .await
    .unwrap();
    let id = start(&hub, dir.path()).await;
    hub.send(&id, "next", TurnFrom::User, Source::User("test".into()))
        .unwrap();
    let ended = wait(&hub, &id, |e| e.kind() == "turn.ended" && e.turn == Some(3)).await;
    assert_eq!(ended.turn, Some(3));
    let (_, events) = hub.thread_events(&id, 0, 1000).unwrap();
    let turns: Vec<(&str, Option<u64>)> = events
        .iter()
        .filter(|e| matches!(e.kind(), "turn.started" | "turn.ended"))
        .map(|e| (e.kind(), e.turn))
        .collect();
    assert_eq!(
        turns,
        vec![
            ("turn.started", Some(1)),
            ("turn.ended", Some(1)),
            ("turn.started", Some(2)),
            ("turn.ended", Some(2)),
            ("turn.started", Some(3)),
            ("turn.ended", Some(3)),
        ]
    );
    let at = |p: &dyn Fn(&Event) -> bool| events.iter().position(p).unwrap();
    let raced = at(&|e| matches!(&e.body, EventBody::AgentText(t) if t.text == "review finished"));
    let answer =
        at(&|e| matches!(&e.body, EventBody::AgentText(t) if t.text == "answer to the prompt"));
    let ended = at(&|e| e.kind() == "turn.ended" && e.turn == Some(2));
    assert_eq!(events[raced].turn, Some(2));
    assert!(
        raced < answer && answer < ended,
        "turn 2 ends after its answer"
    );
    let usage = |turn| {
        events
            .iter()
            .find_map(|e| match &e.body {
                EventBody::TurnEnded(t) if e.turn == Some(turn) => Some(t.usage.clone()),
                _ => None,
            })
            .unwrap()
    };
    assert_eq!(
        usage(2),
        Some(Usage {
            input: 17,
            output: 8,
            cost: Some(0.75)
        })
    );
    // r1's late second end counts for nothing: r1 was counted in turn 2.
    assert_eq!(
        usage(3),
        Some(Usage {
            input: 2,
            output: 2,
            cost: Some(0.0)
        })
    );
    hub.shutdown().await;
}

/// A request of the runtime's session rather than of its turn (a Codex child agent's,
/// `ext.twr.session`), raised while a turn runs, is recorded outside that turn: the turn's end
/// leaves it open, the thread waits on it, and its answer still reaches the runtime.
#[tokio::test]
async fn a_session_request_raised_in_a_turn_outlives_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut ext = None;
    set_twr_ext(&mut ext, SESSION_SCOPE_KEY, true.into());
    let hub = Hub::open(options(
        dir.path(),
        vec![
            Step::EmitSession(EventBody::ThreadStarted(SessionPayload::default())),
            Step::EmitSession(EventBody::AttentionRaised(AttentionRaised {
                attn_id: "child-rm".into(),
                reason: AttentionReason::Permission,
                summary: "Allow shell: rm -rf target".into(),
                options: vec!["allow".into(), "deny".into()],
                ext,
                ..Default::default()
            })),
            Step::Emit(EventBody::TurnEnded(TurnEnded::default())),
            Step::AwaitRespond("child-rm".into()),
            Step::EmitSession(EventBody::AgentText(AgentText {
                text: "child went on".into(),
                is_final: true,
                ..Default::default()
            })),
            Step::Sleep(60000),
        ],
    ))
    .await
    .unwrap();
    let id = start(&hub, dir.path()).await;
    let (_, events) = hub.thread_events(&id, 0, 1000).unwrap();
    let raised = events
        .iter()
        .find(|e| e.kind() == "attention.raised")
        .unwrap();
    assert_eq!(raised.turn, None);
    assert!(!events.iter().any(|e| e.kind() == "attention.resolved"));
    assert_eq!(
        hub.thread_view(&id).unwrap().status(),
        ThreadStatus::Waiting
    );
    let EventBody::AttentionRaised(item) = &raised.body else {
        unreachable!()
    };
    hub.respond(&item.attn_id, Answer::allow(), "test").unwrap();
    wait(
        &hub,
        &id,
        |e| matches!(&e.body, EventBody::AgentText(t) if t.text == "child went on"),
    )
    .await;
    hub.shutdown().await;
}

/// What the Codex adapter reports when twr's turn 2 fails (`turn/start` refused) while a turn
/// Codex started on its own runs and asks for an approval: turn 2's end, the runtime turn's
/// start, then its approval, with no turn. The approval belongs to the runtime turn (turn 3),
/// stays open past turn 2's end, and its answer reaches the runtime.
#[tokio::test]
async fn a_runtime_turn_approval_outlives_the_failed_turn_it_raced() {
    let dir = tempfile::tempdir().unwrap();
    let mut tag = None;
    set_twr_ext(&mut tag, RUNTIME_TURN_KEY, "auto".into());
    let hub = Hub::open(options(
        dir.path(),
        vec![
            Step::EmitSession(EventBody::ThreadStarted(SessionPayload::default())),
            Step::Emit(EventBody::TurnEnded(TurnEnded::default())),
            Step::AwaitSend,
            Step::Emit(EventBody::TurnEnded(TurnEnded {
                result: TurnResult::Error,
                ..Default::default()
            })),
            Step::EmitSession(EventBody::TurnStarted(TurnStarted {
                from: TurnFrom::Runtime,
                ext: tag.clone(),
                ..Default::default()
            })),
            Step::EmitSession(EventBody::AttentionRaised(AttentionRaised {
                attn_id: "rm".into(),
                reason: AttentionReason::Permission,
                summary: "Allow shell: rm -rf target".into(),
                options: vec!["allow".into(), "deny".into()],
                ..Default::default()
            })),
            Step::AwaitRespond("rm".into()),
            Step::EmitSession(EventBody::AgentText(AgentText {
                text: "approved".into(),
                is_final: true,
                ..Default::default()
            })),
            Step::EmitSession(EventBody::TurnEnded(TurnEnded {
                ext: tag,
                ..Default::default()
            })),
            Step::Sleep(60000),
        ],
    ))
    .await
    .unwrap();
    let id = start(&hub, dir.path()).await;
    hub.send(&id, "next", TurnFrom::User, Source::User("test".into()))
        .unwrap();
    // (Turn 2's failure raises the hub's own error item first.)
    let raised = wait(
        &hub,
        &id,
        |e| matches!(&e.body, EventBody::AttentionRaised(a) if a.attn_id.ends_with(":rm")),
    )
    .await;
    assert_eq!(raised.turn, Some(3));
    let (_, events) = hub.thread_events(&id, 0, 1000).unwrap();
    assert!(events
        .iter()
        .any(|e| e.kind() == "turn.ended" && e.turn == Some(2)));
    assert!(!events.iter().any(|e| e.kind() == "attention.resolved"));
    assert_eq!(
        hub.thread_view(&id).unwrap().status(),
        ThreadStatus::Waiting
    );
    let EventBody::AttentionRaised(item) = &raised.body else {
        unreachable!()
    };
    hub.respond(&item.attn_id, Answer::allow(), "test").unwrap();
    wait(
        &hub,
        &id,
        |e| matches!(&e.body, EventBody::AgentText(t) if t.text == "approved"),
    )
    .await;
    wait(&hub, &id, |e| e.kind() == "turn.ended" && e.turn == Some(3)).await;
    hub.shutdown().await;
}

/// A runtime turn taken into twr's open turn 2 (its start dropped) asks a question and
/// completes before it is answered: the adapter withdraws the question with the turn's end
/// (which closes nothing), so the question closes and turn 2 goes on to its own end.
#[tokio::test]
async fn an_absorbed_runtime_turn_completing_closes_its_question_not_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let mut tag = None;
    set_twr_ext(&mut tag, RUNTIME_TURN_KEY, "auto".into());
    let hub = Hub::open(options(
        dir.path(),
        vec![
            Step::EmitSession(EventBody::ThreadStarted(SessionPayload::default())),
            Step::Emit(EventBody::TurnEnded(TurnEnded::default())),
            Step::AwaitSend,
            Step::EmitSession(EventBody::TurnStarted(TurnStarted {
                from: TurnFrom::Runtime,
                ext: tag.clone(),
                ..Default::default()
            })),
            Step::EmitSession(EventBody::AttentionRaised(AttentionRaised {
                attn_id: "q1".into(),
                reason: AttentionReason::Question,
                summary: "Keep the cache?".into(),
                options: vec!["yes".into(), "no".into()],
                ..Default::default()
            })),
            Step::EmitSession(EventBody::AttentionResolved(AttentionResolved {
                attn_id: "q1".into(),
                by: ResolvedBy::Timeout,
                ..Default::default()
            })),
            Step::EmitSession(EventBody::TurnEnded(TurnEnded {
                ext: tag,
                ..Default::default()
            })),
            Step::Emit(EventBody::AgentText(AgentText {
                text: "answer to the prompt".into(),
                is_final: true,
                ..Default::default()
            })),
            Step::Emit(EventBody::TurnEnded(TurnEnded::default())),
            Step::Sleep(60000),
        ],
    ))
    .await
    .unwrap();
    let id = start(&hub, dir.path()).await;
    hub.send(&id, "next", TurnFrom::User, Source::User("test".into()))
        .unwrap();
    wait(&hub, &id, |e| e.kind() == "turn.ended" && e.turn == Some(2)).await;
    let (_, events) = hub.thread_events(&id, 0, 1000).unwrap();
    let at = |p: &dyn Fn(&Event) -> bool| events.iter().position(p).unwrap();
    let raised =
        at(&|e| matches!(&e.body, EventBody::AttentionRaised(a) if a.attn_id.ends_with(":q1")));
    let resolved =
        at(&|e| matches!(&e.body, EventBody::AttentionResolved(r) if r.attn_id.ends_with(":q1")));
    let answer =
        at(&|e| matches!(&e.body, EventBody::AgentText(t) if t.text == "answer to the prompt"));
    let ended = at(&|e| e.kind() == "turn.ended" && e.turn == Some(2));
    assert_eq!(events[raised].turn, Some(2));
    assert!(raised < resolved && resolved < answer && answer < ended);
    assert_eq!(
        events
            .iter()
            .filter(|e| e.kind() == "turn.ended" && e.turn == Some(2))
            .count(),
        1
    );
    hub.shutdown().await;
}

/// A settings change that does not narrow the mode (here the model) waits for the runtime's
/// background tasks before it restarts the runtime.
#[tokio::test]
async fn settings_restart_waits_for_background_tasks_to_finish() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(options(dir.path(), script(vec![Step::Sleep(60000)])))
        .await
        .unwrap();
    let id = start(&hub, dir.path()).await;
    let r = hub
        .set_model(&id, Some("m2"), None, &Caller::user("test"))
        .unwrap();
    assert!(r.get("stops_background_tasks").is_none(), "{r}");
    assert_eq!(
        hub.thread_view(&id).unwrap().status(),
        ThreadStatus::Background
    );
    assert!(!hub
        .thread_events(&id, 0, 1000)
        .unwrap()
        .1
        .iter()
        .any(|e| e.kind() == "thread.stopped"));
    for task_id in ["review", "tests"] {
        hub.append(
            &id,
            None,
            Source::Adapter("fake".into()),
            task(task_id, false),
            None,
        )
        .unwrap();
    }
    let stopped = wait(
        &hub,
        &id,
        |e| matches!(&e.body, EventBody::ThreadStopped(p) if is_settings_restart(p)),
    )
    .await;
    // The restarted fake replays its script and can immediately start new work. Check
    // that both old tasks finished before the durable settings stop, not its later view.
    assert_eq!(
        hub.thread_events(&id, 0, 1000)
            .unwrap()
            .1
            .iter()
            .filter(|e| e.seq < stopped.seq)
            .filter(|e| matches!(&e.body, EventBody::TaskUpdated(t) if !t.running))
            .count(),
        2
    );
    hub.shutdown().await;
}

/// A mode change that narrows the mode does not wait for the runtime's background tasks:
/// while they ran, the runtime could start turns of its own in the old, wider mode. It
/// restarts the runtime right away (its tasks end with it), and says so.
#[tokio::test]
async fn a_narrowing_mode_change_does_not_wait_for_background_tasks() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(options(dir.path(), script(vec![Step::Sleep(60000)])))
        .await
        .unwrap();
    let id = start(&hub, dir.path()).await;
    assert_eq!(hub.thread_view(&id).unwrap().mode, "auto");
    assert!(hub.thread_view(&id).unwrap().runtime_tasks());
    let r = hub.set_mode(&id, "manual", &Caller::user("test")).unwrap();
    assert_eq!(r["applies"], "now", "{r}");
    assert_eq!(r["stops_background_tasks"], true, "{r}");
    let stopped = wait(
        &hub,
        &id,
        |e| matches!(&e.body, EventBody::ThreadStopped(p) if is_settings_restart(p)),
    )
    .await;
    // No task ended before the restart's stop: it did not wait for them.
    assert_eq!(
        hub.thread_events(&id, 0, 1000)
            .unwrap()
            .1
            .iter()
            .filter(|e| e.seq < stopped.seq)
            .filter(|e| matches!(&e.body, EventBody::TaskUpdated(t) if !t.running))
            .count(),
        0
    );
    hub.shutdown().await;
}

/// The task ends recorded before the settings restart's stop of `id`.
fn tasks_ended_before(hub: &Hub, id: &str, stopped: &Event) -> usize {
    hub.thread_events(id, 0, 1000)
        .unwrap()
        .1
        .iter()
        .filter(|e| e.seq < stopped.seq)
        .filter(|e| matches!(&e.body, EventBody::TaskUpdated(t) if !t.running))
        .count()
}

/// A narrowing is any capability lost, not only a lower level: Codex `on-request` ->
/// `manual` (approval on-request -> untrusted) and `auto` -> `never` (no network, no extra
/// roots) are one level each, yet narrower, so they do not wait for background tasks.
#[tokio::test]
async fn a_same_level_narrowing_does_not_wait_for_background_tasks() {
    for (from, to) in [("on-request", "manual"), ("auto", "never")] {
        let dir = tempfile::tempdir().unwrap();
        let steps = script(vec![Step::Sleep(60000)]);
        let hub = Hub::open(options_as(dir.path(), steps, "codex"))
            .await
            .unwrap();
        let id = start_as(&hub, dir.path(), "codex", Some(from)).await;
        assert!(hub.thread_view(&id).unwrap().runtime_tasks());
        let r = hub.set_mode(&id, to, &Caller::user("test")).unwrap();
        assert_eq!(r["applies"], "now", "{from} -> {to}: {r}");
        assert_eq!(r["stops_background_tasks"], true, "{from} -> {to}: {r}");
        let stopped = wait(
            &hub,
            &id,
            |e| matches!(&e.body, EventBody::ThreadStopped(p) if is_settings_restart(p)),
        )
        .await;
        assert_eq!(tasks_ended_before(&hub, &id, &stopped), 0, "{from} -> {to}");
        hub.shutdown().await;
    }
    // Same policy (manual and acceptEdits are both untrusted + workspace-write), or a
    // widening to full access under any of its names: it waits.
    for (from, to) in [
        ("auto", "bypassPermissions"),
        ("auto", "danger-full-access"),
        ("auto", "yolo"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let steps = script(vec![Step::Sleep(60000)]);
        let hub = Hub::open(options_as(dir.path(), steps, "codex"))
            .await
            .unwrap();
        let id = start_as(&hub, dir.path(), "codex", Some(from)).await;
        let r = hub.set_mode(&id, to, &Caller::user("test")).unwrap();
        assert!(r.get("stops_background_tasks").is_none(), "{to}: {r}");
        assert_eq!(r["applies"], "after_turn", "{to}: {r}");
        hub.shutdown().await;
    }
    let dir = tempfile::tempdir().unwrap();
    let steps = script(vec![Step::Sleep(60000)]);
    let hub = Hub::open(options_as(dir.path(), steps, "codex"))
        .await
        .unwrap();
    let id = start_as(&hub, dir.path(), "codex", Some("manual")).await;
    let r = hub
        .set_mode(&id, "acceptEdits", &Caller::user("test"))
        .unwrap();
    assert!(r.get("stops_background_tasks").is_none(), "{r}");
    assert_eq!(r["applies"], "after_turn", "{r}");
    hub.shutdown().await;
}

/// An item the runtime raised between turns (a background subagent's approval) defers a
/// restart that keeps the mode, but not a narrowing one: the stop resolves it, like any stop.
#[tokio::test]
async fn a_narrowing_restart_does_not_wait_for_a_background_approval() {
    let dir = tempfile::tempdir().unwrap();
    let steps = script(vec![
        Step::EmitSession(EventBody::AttentionRaised(AttentionRaised {
            attn_id: "child-approval".into(),
            reason: AttentionReason::Permission,
            summary: "Allow shell: cargo test".into(),
            options: vec!["allow".into(), "deny".into()],
            ..Default::default()
        })),
        Step::Sleep(60000),
    ]);
    let hub = Hub::open(options(dir.path(), steps)).await.unwrap();
    let id = start(&hub, dir.path()).await;
    wait(&hub, &id, |e| e.kind() == "attention.raised").await;
    let item = hub
        .thread_view(&id)
        .unwrap()
        .open_attention
        .keys()
        .find(|k| k.ends_with(":child-approval"))
        .cloned()
        .expect("the approval is open");
    let r = hub.set_mode(&id, "manual", &Caller::user("test")).unwrap();
    assert_eq!(r["applies"], "now", "{r}");
    wait(
        &hub,
        &id,
        |e| matches!(&e.body, EventBody::ThreadStopped(p) if is_settings_restart(p)),
    )
    .await;
    let resolved = hub
        .thread_events(&id, 0, 1000)
        .unwrap()
        .1
        .into_iter()
        .find_map(|e| match e.body {
            EventBody::AttentionResolved(p) if p.attn_id == item => Some(p.by),
            _ => None,
        });
    assert_eq!(resolved, Some(ResolvedBy::ThreadStopped));
    hub.shutdown().await;
}

/// After a crash its block kind does not make transient, the queued prompt waits for the user:
/// the background ticker dispatches only a due retry or restart, not any failed turn's queue.
#[tokio::test]
async fn a_crash_holds_the_queue_against_the_background_ticker() {
    let dir = tempfile::tempdir().unwrap();
    let steps = vec![
        Step::Emit(EventBody::ThreadStarted(SessionPayload {
            session_id: Some("crash-session".into()),
            ..Default::default()
        })),
        Step::Sleep(300),
        Step::Exit {
            code: Some(3),
            stderr_tail: "segfault".into(),
        },
    ];
    let hub = Hub::open(options(dir.path(), steps)).await.unwrap();
    let result = hub
        .new_thread(
            NewThread {
                project: dir.path().to_string_lossy().into(),
                name: Some("worker".into()),
                runtime: Some("fake".into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap();
    let id = result["id"].as_str().unwrap().to_string();
    let user = || Source::User("test".into());
    hub.send(&id, "go", TurnFrom::User, user()).unwrap();
    hub.send(&id, "queued", TurnFrom::User, user()).unwrap();
    wait(&hub, &id, |e| e.kind() == "thread.crashed").await;
    // Several ticks of the background ticker (at most a second apart).
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let events = hub.thread_events(&id, 0, 1000).unwrap().1;
    assert_eq!(
        events.iter().filter(|e| e.kind() == "turn.started").count(),
        1,
        "{:?}",
        events.iter().map(Event::kind).collect::<Vec<_>>()
    );
    assert_eq!(hub.get_thread(&id).unwrap()["pending"], 1);
    hub.shutdown().await;
}

/// Only the user releases a queue a crash holds: the orchestrator's `thread.send` and
/// `thread.resume` leave it held, live and in the log a restarted hub folds.
#[tokio::test]
async fn the_orchestrators_send_or_resume_does_not_release_a_crash_hold() {
    let dir = tempfile::tempdir().unwrap();
    let steps = || {
        vec![
            Step::Emit(EventBody::ThreadStarted(SessionPayload {
                session_id: Some("crash-session".into()),
                ..Default::default()
            })),
            Step::Sleep(300),
            Step::Exit {
                code: Some(3),
                stderr_tail: "segfault".into(),
            },
        ]
    };
    let turns = |hub: &Hub, id: &str| {
        hub.thread_events(id, 0, 1000)
            .unwrap()
            .1
            .iter()
            .filter(|e| e.kind() == "turn.started")
            .count()
    };
    let hub = Hub::open(options(dir.path(), steps())).await.unwrap();
    let result = hub
        .new_thread(
            NewThread {
                project: dir.path().to_string_lossy().into(),
                name: Some("worker".into()),
                runtime: Some("fake".into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap();
    let id = result["id"].as_str().unwrap().to_string();
    let user = || Source::User("test".into());
    hub.send(&id, "go", TurnFrom::User, user()).unwrap();
    hub.send(&id, "queued", TurnFrom::User, user()).unwrap();
    wait(&hub, &id, |e| e.kind() == "thread.crashed").await;
    let sent = hub
        .send(
            &id,
            "from the orchestrator",
            TurnFrom::Orchestrator,
            Source::Orchestrator,
        )
        .unwrap();
    assert_eq!(sent.turn, None);
    hub.park_thread(&id, true, false, &Caller::user("test"))
        .await
        .unwrap();
    hub.resume_thread(&id, &Caller::Orchestrator).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(turns(&hub, &id), 1);
    assert_eq!(hub.get_thread(&id).unwrap()["pending"], 2);
    hub.shutdown().await;
    drop(hub);
    // A restarted hub folds the orchestrator's `thread.resumed`, and counts no prompt of its
    // as the user's.
    let hub = Hub::open(options(dir.path(), steps())).await.unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(turns(&hub, &id), 1);
    assert_eq!(hub.get_thread(&id).unwrap()["pending"], 2);
    hub.send(&id, "now", TurnFrom::User, user()).unwrap();
    wait(
        &hub,
        &id,
        |e| matches!(&e.body, EventBody::TurnStarted(p) if p.prompt == "queued"),
    )
    .await;
    hub.shutdown().await;
}

#[tokio::test]
async fn automatic_retry_waits_for_background_work_to_finish() {
    let dir = tempfile::tempdir().unwrap();
    let mut steps = script(vec![Step::AwaitSend, Step::Sleep(60000)]);
    *steps.get_mut(4).unwrap() = Step::Emit(EventBody::TurnEnded(TurnEnded {
        result: TurnResult::Error,
        ext: Some(
            serde_json::from_value(serde_json::json!({
                "fake": {"transient": true, "message": "temporary server failure"}
            }))
            .unwrap(),
        ),
        ..Default::default()
    }));
    let hub = Hub::open(options(dir.path(), steps)).await.unwrap();
    let id = start(&hub, dir.path()).await;
    assert_eq!(hub.thread_view(&id).unwrap().turns, 1);
    assert!(!hub.thread_view(&id).unwrap().retry_eligible());
    for task_id in ["review", "tests"] {
        hub.append(
            &id,
            None,
            Source::Adapter("fake".into()),
            task(task_id, false),
            None,
        )
        .unwrap();
    }
    let retry = wait(&hub, &id, |e| matches!(&e.body, EventBody::TurnStarted(t) if t.ext.as_ref().is_some_and(|x| x["twr"]["retry_of"] == 1))).await;
    assert_eq!(retry.turn, Some(2));
    assert!(hub.thread_view(&id).unwrap().background_tasks.is_empty());
    hub.shutdown().await;
}

#[tokio::test]
async fn silent_background_work_stalls_once_and_a_heartbeat_does_not_resolve_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut opts = options(dir.path(), script(vec![Step::Sleep(60000)]));
    opts.stall_after = Some(Duration::from_millis(100));
    let hub = Hub::open(opts).await.unwrap();
    let id = start(&hub, dir.path()).await;
    wait(&hub, &id, |e| matches!(&e.body, EventBody::AttentionRaised(a) if a.reason == AttentionReason::Stalled)).await;
    hub.append(
        &id,
        None,
        Source::Adapter("fake".into()),
        EventBody::TaskUpdated(TaskUpdated {
            task_id: "review".into(),
            running: true,
            heartbeat: true,
            ..Default::default()
        }),
        None,
    )
    .unwrap();
    assert!(hub
        .thread_view(&id)
        .unwrap()
        .open_attention
        .values()
        .any(|r| *r == AttentionReason::Stalled));
    hub.append(
        &id,
        None,
        Source::Adapter("fake".into()),
        task("review", false),
        None,
    )
    .unwrap();
    assert!(!hub
        .thread_view(&id)
        .unwrap()
        .open_attention
        .values()
        .any(|r| *r == AttentionReason::Stalled));
    // Its background work went on (a task ended): the resolution says so.
    let (_, evs) = hub.thread_events(&id, 0, 1000).unwrap();
    let closed = evs
        .iter()
        .find_map(|e| match &e.body {
            EventBody::AttentionResolved(p) => Some(p.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(closed.by, ResolvedBy::Timeout);
    assert!(went_on(&closed.ext), "{closed:?}");
    hub.stop_thread(&id, "test").unwrap();
    wait(&hub, &id, |e| e.kind() == "thread.stopped").await;
    hub.shutdown().await;
}

/// A report rewritten while a later turn runs (and has started background tasks) is the
/// earlier turn's final report: no interim marker, and its review is raised.
#[tokio::test]
async fn a_report_rewritten_while_a_later_turn_runs_is_not_interim() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(options(
        dir.path(),
        vec![
            Step::EmitSession(EventBody::ThreadStarted(SessionPayload {
                session_id: Some("background-session".into()),
                ..Default::default()
            })),
            Step::Emit(EventBody::AgentText(AgentText {
                text: "Done.".into(),
                is_final: true,
                ..Default::default()
            })),
            Step::Emit(EventBody::TurnEnded(TurnEnded::default())),
            Step::AwaitSend,
            Step::EmitSession(task("tests", true)),
            Step::Sleep(60000),
        ],
    ))
    .await
    .unwrap();
    let id = start(&hub, dir.path()).await;
    hub.send(&id, "and more", TurnFrom::User, Source::User("test".into()))
        .unwrap();
    wait(&hub, &id, |e| e.kind() == "task.updated").await;
    assert_eq!(hub.thread_view(&id).unwrap().current_turn, Some(2));
    let written = hub
        .write_report(
            &id,
            Some(1),
            "Not verified.",
            Grade::NeedsVerification,
            &Caller::Orchestrator,
        )
        .unwrap();
    assert!(!written["review"].is_null(), "{written}");
    let report = hub.thread_view(&id).unwrap().last_report.unwrap();
    assert!(report.ext.is_none(), "{:?}", report.ext);
    hub.shutdown().await;
}

/// A report rewritten while the tasks its turn ended with still run stays interim, also when
/// the user started another turn meanwhile.
#[tokio::test]
async fn a_report_rewritten_with_its_turns_tasks_running_stays_interim_past_a_new_turn() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(options(
        dir.path(),
        script(vec![Step::AwaitSend, Step::Sleep(60000)]),
    ))
    .await
    .unwrap();
    let id = start(&hub, dir.path()).await;
    hub.send(
        &id,
        "meanwhile",
        TurnFrom::User,
        Source::User("test".into()),
    )
    .unwrap();
    wait(&hub, &id, |e| {
        e.kind() == "turn.started" && e.turn == Some(2)
    })
    .await;
    let written = hub
        .write_report(
            &id,
            Some(1),
            "Not verified.",
            Grade::NeedsVerification,
            &Caller::Orchestrator,
        )
        .unwrap();
    assert!(written["review"].is_null(), "{written}");
    let report = hub.thread_view(&id).unwrap().last_report.unwrap();
    assert_eq!(report.ext.unwrap()["twr"]["background_tasks"], 2);
    hub.shutdown().await;
}

/// Rewritten after a later turn reported, a turn whose tasks still run stays interim: what
/// counts is that turn's own report, not the thread's latest.
#[tokio::test]
async fn a_report_rewritten_after_a_later_turn_reported_stays_interim_while_its_tasks_run() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(options(
        dir.path(),
        script(vec![
            Step::AwaitSend,
            Step::Emit(EventBody::AgentText(AgentText {
                text: "Answered while the review runs.".into(),
                is_final: true,
                ..Default::default()
            })),
            Step::Emit(EventBody::TurnEnded(TurnEnded::default())),
            Step::Sleep(60000),
        ]),
    ))
    .await
    .unwrap();
    let orch = orchestrator(&hub);
    let id = start(&hub, dir.path()).await;
    hub.send(
        &id,
        "meanwhile",
        TurnFrom::User,
        Source::User("test".into()),
    )
    .unwrap();
    wait(&hub, &id, |e| {
        e.kind() == "report.ready" && e.turn == Some(2)
    })
    .await;
    let written = hub
        .write_report(
            &id,
            Some(1),
            "Not verified.",
            Grade::NeedsVerification,
            &Caller::Orchestrator,
        )
        .unwrap();
    assert!(written["review"].is_null(), "{written}");
    // The thread's latest report is turn 2's; turn 1's rewrite is its latest of turn 1.
    let (items, _) = hub.control_read(0, 1000).unwrap();
    let rewrite = items
        .iter()
        .rev()
        .find_map(|(_, ev)| match &ev.body {
            EventBody::ReportReady(r) if r.turn == Some(1) => Some(r.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(rewrite.written_by, "orchestrator");
    assert_eq!(rewrite.ext.unwrap()["twr"]["background_tasks"], 2);
    let update = report_update(&hub, &orch, &id, "Not verified.");
    assert!(
        update.contains("yielded with 2 background tasks still running (interim report"),
        "{update}"
    );
    hub.shutdown().await;
}

/// Once the tasks a turn ended with are done, its rewrite is final even while a task a later
/// turn started runs: its review is raised.
#[tokio::test]
async fn a_report_rewritten_after_its_tasks_ended_is_final_despite_later_tasks() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(options(
        dir.path(),
        script(vec![
            Step::AwaitSend,
            Step::EmitSession(task("review", false)),
            Step::EmitSession(task("tests", false)),
            Step::EmitSession(task("other", true)),
            Step::Sleep(60000),
        ]),
    ))
    .await
    .unwrap();
    let orch = orchestrator(&hub);
    let id = start(&hub, dir.path()).await;
    hub.send(
        &id,
        "meanwhile",
        TurnFrom::User,
        Source::User("test".into()),
    )
    .unwrap();
    wait(
        &hub,
        &id,
        |e| matches!(&e.body, EventBody::TaskUpdated(t) if t.task_id == "other"),
    )
    .await;
    let written = hub
        .write_report(
            &id,
            Some(1),
            "Not verified.",
            Grade::NeedsVerification,
            &Caller::Orchestrator,
        )
        .unwrap();
    assert!(!written["review"].is_null(), "{written}");
    let report = hub.thread_view(&id).unwrap().last_report.unwrap();
    assert!(report.ext.is_none(), "{:?}", report.ext);
    // Its line says it finished: the task running now is another turn's, or another task.
    let update = report_update(&hub, &orch, &id, "Not verified.");
    assert!(
        update.contains(" finished (needs_verification): Not verified."),
        "{update}"
    );
    hub.shutdown().await;
}

/// A task id reused by new work after the turn's own task ended is another task: the turn's
/// rewrite is final and its review is raised.
#[tokio::test]
async fn a_task_id_reused_after_the_turns_task_ended_does_not_keep_it_interim() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(options(
        dir.path(),
        script(vec![
            Step::AwaitSend,
            Step::EmitSession(task("review", false)),
            Step::EmitSession(task("tests", false)),
            Step::EmitSession(task("review", true)),
            Step::Sleep(60000),
        ]),
    ))
    .await
    .unwrap();
    let orch = orchestrator(&hub);
    let id = start(&hub, dir.path()).await;
    hub.send(
        &id,
        "meanwhile",
        TurnFrom::User,
        Source::User("test".into()),
    )
    .unwrap();
    // The relaunch: the third `task.updated` of `review`.
    let seen = std::cell::Cell::new(0);
    wait(&hub, &id, |e| {
        if matches!(&e.body, EventBody::TaskUpdated(t) if t.task_id == "review") {
            seen.set(seen.get() + 1);
        }
        seen.get() == 3
    })
    .await;
    assert!(hub
        .thread_view(&id)
        .unwrap()
        .background_tasks
        .contains_key("review"));
    let written = hub
        .write_report(
            &id,
            Some(1),
            "Not verified.",
            Grade::NeedsVerification,
            &Caller::Orchestrator,
        )
        .unwrap();
    assert!(!written["review"].is_null(), "{written}");
    let report = hub.thread_view(&id).unwrap().last_report.unwrap();
    assert!(report.ext.is_none(), "{:?}", report.ext);
    // Its line says it finished: the task running now is another turn's, or another task.
    let update = report_update(&hub, &orch, &id, "Not verified.");
    assert!(
        update.contains(" finished (needs_verification): Not verified."),
        "{update}"
    );
    hub.shutdown().await;
}

/// A running update of task `id`: progress saying `description` (`heartbeat: false`), or a
/// heartbeat.
fn update(id: &str, description: &str, heartbeat: bool) -> EventBody {
    EventBody::TaskUpdated(TaskUpdated {
        task_id: id.into(),
        running: true,
        description: description.into(),
        heartbeat,
        ..Default::default()
    })
}

/// The recorded updates of task `id`: (running, description, heartbeat).
fn recorded(hub: &Hub, thread: &str, id: &str) -> Vec<(bool, String, bool)> {
    hub.thread_events(thread, 0, 10_000)
        .unwrap()
        .1
        .into_iter()
        .filter_map(|e| match e.body {
            EventBody::TaskUpdated(p) if p.task_id == id => {
                Some((p.running, p.description, p.heartbeat))
            }
            _ => None,
        })
        .collect()
}

fn coalesced(hub: &Hub) -> u64 {
    hub.info()["coalesced_events"].as_u64().unwrap()
}

/// The script's start, then `steps` after its turn ended, then a long sleep.
fn started_then(steps: Vec<Step>) -> Vec<Step> {
    let mut script = vec![
        Step::EmitSession(EventBody::ThreadStarted(SessionPayload {
            session_id: Some("background-session".into()),
            ..Default::default()
        })),
        Step::EmitSession(update("build", "build", false)),
        Step::Emit(EventBody::AgentText(AgentText {
            text: "Building in the background.".into(),
            is_final: true,
            ..Default::default()
        })),
        Step::Emit(EventBody::TurnEnded(TurnEnded::default())),
    ];
    script.extend(steps);
    script.push(Step::Sleep(60_000));
    script
}

/// A runtime's task updates are recorded at most so often (`hub::coalesce`): progress once per
/// interval (a tenth of the hung-turn threshold here: 1 s), heartbeats once per interval (a
/// quarter of the background timeout here: 1 s) after the task's last recorded update; its
/// start, an update with `ext` and its end always.
#[tokio::test]
async fn a_runtimes_task_updates_are_recorded_at_most_so_often() {
    let dir = tempfile::tempdir().unwrap();
    let with_ext = EventBody::TaskUpdated(TaskUpdated {
        task_id: "build".into(),
        running: true,
        description: "step 1b".into(),
        ext: Some(serde_json::from_value(serde_json::json!({"x": 1})).unwrap()),
        ..Default::default()
    });
    let script = started_then(vec![
        Step::EmitSession(update("build", "step 1", false)),
        Step::EmitSession(update("build", "", true)),
        Step::EmitSession(with_ext),
        Step::Sleep(1300),
        Step::EmitSession(update("build", "step 2", false)),
        Step::EmitSession(update("build", "step 3", false)),
        Step::EmitSession(update("build", "", true)),
        // A heartbeat with a new description is progress.
        Step::EmitSession(update("build", "step 3", true)),
        Step::Sleep(1300),
        Step::EmitSession(update("build", "", true)),
        Step::EmitSession(EventBody::TaskUpdated(TaskUpdated {
            task_id: "build".into(),
            description: "build".into(),
            ..Default::default()
        })),
    ]);
    let mut opts = options(dir.path(), script);
    opts.stall_after = Some(Duration::from_secs(10));
    opts.config.background_timeout_secs = 4;
    let hub = Hub::open(opts).await.unwrap();
    let id = start(&hub, dir.path()).await;
    wait(
        &hub,
        &id,
        |e| matches!(&e.body, EventBody::TaskUpdated(p) if !p.running),
    )
    .await;
    let s = |d: &str| d.to_string();
    assert_eq!(
        recorded(&hub, &id, "build"),
        [
            (true, s("build"), false),
            (true, s("step 1b"), false),
            (true, s("step 2"), false),
            (true, s(""), true),
            (false, s("build"), false),
        ]
    );
    assert_eq!(coalesced(&hub), 5);
    hub.shutdown().await;
}

/// A task whose progress is coalesced keeps its thread from stalling while it moves; once it
/// goes silent, the thread stalls, `stall_after` after its last recorded progress.
#[tokio::test]
async fn coalesced_progress_keeps_a_thread_from_stalling() {
    let dir = tempfile::tempdir().unwrap();
    let mut steps = Vec::new();
    for i in 0..40 {
        steps.push(Step::Sleep(50));
        steps.push(Step::EmitSession(update(
            "build",
            &format!("step {i}"),
            false,
        )));
    }
    let mut opts = options(dir.path(), started_then(steps));
    let after = Duration::from_secs(2);
    opts.stall_after = Some(after);
    let hub = Hub::open(opts).await.unwrap();
    let id = start(&hub, dir.path()).await;
    let stalled = wait(&hub, &id, |e| matches!(&e.body, EventBody::AttentionRaised(a) if a.reason == AttentionReason::Stalled)).await;
    let (_, evs) = hub.thread_events(&id, 0, 10_000).unwrap();
    let last_progress = evs
        .iter()
        .rfind(|e| matches!(&e.body, EventBody::TaskUpdated(p) if p.running && !p.heartbeat))
        .unwrap();
    // Once: no stall while it moved (nothing resolved one).
    assert!(
        !evs.iter().any(|e| e.kind() == "attention.resolved"),
        "{evs:#?}"
    );
    assert!(stalled.seq > last_progress.seq);
    assert!(stalled.ts - last_progress.ts >= chrono::Duration::from_std(after).unwrap());
    let progress = recorded(&hub, &id, "build").len();
    assert!(progress < 41, "{progress} of 41 updates recorded");
    assert_eq!(coalesced(&hub), 41 - progress as u64);
    hub.shutdown().await;
}

/// Coalesced heartbeats still renew a task's lease: the background timeout stops the runtime
/// only once they stop, after the last recorded one.
#[tokio::test]
async fn coalesced_heartbeats_still_renew_the_background_lease() {
    let dir = tempfile::tempdir().unwrap();
    let mut steps = Vec::new();
    for _ in 0..30 {
        steps.push(Step::Sleep(100));
        steps.push(Step::EmitSession(update("build", "", true)));
    }
    let mut opts = options(dir.path(), started_then(steps));
    opts.config.background_timeout_secs = 2;
    let hub = Hub::open(opts).await.unwrap();
    let id = start(&hub, dir.path()).await;
    let stopped = wait(&hub, &id, |e| e.kind() == "thread.stopped").await;
    let beats = recorded(&hub, &id, "build");
    assert!(beats.len() < 31, "{beats:?}");
    assert_eq!(coalesced(&hub), 31 - beats.len() as u64);
    let (_, evs) = hub.thread_events(&id, 0, 10_000).unwrap();
    let last = evs.iter().rfind(|e| e.kind() == "task.updated").unwrap();
    assert!(stopped.ts - last.ts >= chrono::Duration::seconds(2));
    // Renewed until its heartbeats stopped (3 s in): never stopped before.
    let first = evs.iter().find(|e| e.kind() == "task.updated").unwrap();
    assert!(
        last.ts - first.ts >= chrono::Duration::milliseconds(2500),
        "{beats:?}"
    );
    hub.shutdown().await;
}

/// Progress renews a task's lease whatever its own interval: with a background timeout
/// shorter than the progress interval (30 s here), moving work is never stopped as silent.
#[tokio::test]
async fn coalesced_progress_still_renews_a_short_background_lease() {
    let dir = tempfile::tempdir().unwrap();
    let mut steps = Vec::new();
    for i in 0..30 {
        steps.push(Step::Sleep(100));
        steps.push(Step::EmitSession(update(
            "build",
            &format!("step {i}"),
            false,
        )));
    }
    let mut opts = options(dir.path(), started_then(steps));
    opts.config.background_timeout_secs = 2;
    let hub = Hub::open(opts).await.unwrap();
    let id = start(&hub, dir.path()).await;
    let stopped = wait(&hub, &id, |e| e.kind() == "thread.stopped").await;
    let progress = recorded(&hub, &id, "build");
    assert!(progress.len() < 31, "{progress:?}");
    assert_eq!(coalesced(&hub), 31 - progress.len() as u64);
    let (_, evs) = hub.thread_events(&id, 0, 10_000).unwrap();
    let updates: Vec<&Event> = evs.iter().filter(|e| e.kind() == "task.updated").collect();
    let (first, last) = (updates[0], updates[updates.len() - 1]);
    assert!(stopped.ts - last.ts >= chrono::Duration::seconds(2));
    // Renewed until its progress stopped (3 s in): never stopped before.
    assert!(
        last.ts - first.ts >= chrono::Duration::milliseconds(2500),
        "{progress:?}"
    );
    hub.shutdown().await;
}

/// A heartbeat that says what a stalled task does now is recorded, but is no progress: the
/// real progress right after it is recorded too, and resolves the stall (its work went on).
#[tokio::test]
async fn progress_after_a_heartbeat_with_news_resolves_a_stall() {
    let dir = tempfile::tempdir().unwrap();
    let script = started_then(vec![
        Step::Sleep(3500),
        Step::EmitSession(update("build", "linking", true)),
        Step::EmitSession(update("build", "linked", false)),
    ]);
    let mut opts = options(dir.path(), script);
    opts.stall_after = Some(Duration::from_secs(2));
    let hub = Hub::open(opts).await.unwrap();
    let id = start(&hub, dir.path()).await;
    let closed = wait(&hub, &id, |e| e.kind() == "attention.resolved").await;
    let s = |d: &str| d.to_string();
    assert_eq!(
        recorded(&hub, &id, "build"),
        [
            (true, s("build"), false),
            (true, s("linking"), true),
            (true, s("linked"), false),
        ]
    );
    let EventBody::AttentionResolved(p) = &closed.body else {
        unreachable!()
    };
    assert_eq!(p.by, ResolvedBy::Timeout);
    assert!(went_on(&p.ext), "{p:?}");
    hub.shutdown().await;
}

/// The same with a heartbeat that carries `ext` (always recorded): it is no progress either,
/// so the progress right after it is recorded and resolves the stall.
#[tokio::test]
async fn progress_after_a_heartbeat_with_ext_resolves_a_stall() {
    let dir = tempfile::tempdir().unwrap();
    let mut beat = update("build", "build", true);
    if let EventBody::TaskUpdated(p) = &mut beat {
        let mut ext = serde_json::Map::new();
        ext.insert("fake".into(), serde_json::json!({"inventory": true}));
        p.ext = Some(ext);
    }
    let script = started_then(vec![
        Step::Sleep(3500),
        Step::EmitSession(beat),
        Step::EmitSession(update("build", "linked", false)),
    ]);
    let mut opts = options(dir.path(), script);
    opts.stall_after = Some(Duration::from_secs(2));
    let hub = Hub::open(opts).await.unwrap();
    let id = start(&hub, dir.path()).await;
    let closed = wait(&hub, &id, |e| e.kind() == "attention.resolved").await;
    let s = |d: &str| d.to_string();
    assert_eq!(
        recorded(&hub, &id, "build"),
        [
            (true, s("build"), false),
            (true, s("build"), true),
            (true, s("linked"), false),
        ]
    );
    let EventBody::AttentionResolved(p) = &closed.body else {
        unreachable!()
    };
    assert_eq!(p.by, ResolvedBy::Timeout);
    assert!(went_on(&p.ext), "{p:?}");
    hub.shutdown().await;
}
