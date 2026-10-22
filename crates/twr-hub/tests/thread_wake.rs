//! A worker's background work ends and no turn follows (`hub::wake`): the hub wakes it, once
//! per interim report and a few times in a row at most, never when the runtime's own follow-up
//! started or the user stopped it. A turn left open while something waits behind it gets a
//! hung-turn card (`hub::stall`), and a stall waits while a job of the thread runs.
//!
//! Workers run the fake adapter: scripted for background work, else its prompt directives.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::time::timeout;
use twr_adapters::fake::{FakeAdapter, Step};
use twr_core::adapter::{Adapter, Answer};
use twr_core::config::Config;
use twr_core::event::*;
use twr_hub::hub::{WAKE_CHAIN_MAX, WAKE_PROMPT_START};
use twr_hub::{Caller, Hub, HubOptions, JobStart, NewThread};

const WAIT: Duration = Duration::from_secs(20);
const WAKE_AFTER: Duration = Duration::from_millis(400);

fn opts(dir: &Path, script: Option<Vec<Step>>) -> HubOptions {
    let mut o = HubOptions::new(dir.join("data").join("twr.db"), Config::default());
    o.job_gate_exe = Some(env!("CARGO_BIN_EXE_twr-job-gate").into());
    let fake: Arc<dyn Adapter> = Arc::new(match script {
        Some(steps) => FakeAdapter::scripted(steps),
        None => FakeAdapter::new(),
    });
    o.adapters = Arc::new(move |name: &str| -> Option<Arc<dyn Adapter>> {
        (name == "fake").then(|| Arc::clone(&fake))
    });
    o.git_tick = Some(Duration::ZERO);
    o.stall_after = Some(Duration::ZERO);
    o.disk_tick = Some(Duration::ZERO);
    o.wake_after = Some(WAKE_AFTER);
    o
}

async fn thread(hub: &Hub, dir: &Path) -> String {
    let project = dir.join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let t = hub
        .new_thread(
            NewThread {
                project: project.to_string_lossy().into_owned(),
                runtime: Some("fake".into()),
                name: Some("api".into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap();
    t["id"].as_str().unwrap().to_string()
}

fn events(hub: &Hub, thread: &str) -> Vec<Event> {
    hub.thread_events(thread, 0, 100_000).unwrap().1
}

async fn until(hub: &Hub, thread: &str, pred: impl Fn(&[Event]) -> bool) -> Vec<Event> {
    timeout(WAIT, async {
        loop {
            let evs = events(hub, thread);
            if pred(&evs) {
                return evs;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out; saw {:#?}", kinds(&events(hub, thread))))
}

fn kinds(evs: &[Event]) -> Vec<String> {
    evs.iter()
        .map(|e| format!("{} {:?} {}", e.kind(), e.turn, e.source))
        .collect()
}

/// The hub's wakes: the prompts of turns it started for the runtime after background work.
fn wakes(evs: &[Event]) -> Vec<String> {
    evs.iter()
        .filter_map(|e| match &e.body {
            EventBody::TurnStarted(p)
                if p.from == TurnFrom::Runtime && p.prompt.starts_with(WAKE_PROMPT_START) =>
            {
                Some(p.prompt.clone())
            }
            _ => None,
        })
        .collect()
}

fn twr_updates(evs: &[Event], key: &str) -> Vec<Value> {
    evs.iter()
        .filter_map(|e| match &e.body {
            EventBody::ConversationUpdate(p) => p.ext.as_ref()?.get("twr")?.get(key).cloned(),
            _ => None,
        })
        .collect()
}

fn ended(evs: &[Event], turn: u64) -> bool {
    evs.iter()
        .any(|e| e.kind() == "turn.ended" && e.turn == Some(turn))
}

fn task(id: &str, running: bool) -> EventBody {
    EventBody::TaskUpdated(TaskUpdated {
        task_id: id.into(),
        running,
        description: format!("background {id}"),
        ..Default::default()
    })
}

fn text(s: &str) -> EventBody {
    EventBody::AgentText(AgentText {
        text: s.into(),
        is_final: true,
        ..Default::default()
    })
}

/// A turn that yields with task `id` running (an interim report).
fn interim(id: &str) -> Vec<Step> {
    vec![
        Step::EmitSession(task(id, true)),
        Step::Emit(text("Running it in the background.")),
        Step::Emit(EventBody::TurnEnded(TurnEnded::default())),
    ]
}

fn session() -> Step {
    Step::EmitSession(EventBody::ThreadStarted(SessionPayload {
        session_id: Some("bg".into()),
        ..Default::default()
    }))
}

/// The tests end, no follow-up turn starts: the hub wakes the worker once with what ended.
/// The woken turn ends with no background work (a final report): nothing wakes it again.
#[tokio::test]
async fn an_idle_worker_after_background_work_is_woken_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut script = vec![session()];
    script.extend(interim("tests"));
    script.extend([
        Step::Sleep(200),
        Step::EmitSession(task("tests", false)),
        Step::AwaitSend,
        Step::Emit(text("All tests passed; committed.")),
        Step::Emit(EventBody::TurnEnded(TurnEnded::default())),
        Step::Sleep(60_000),
    ]);
    let hub = Hub::open(opts(dir.path(), Some(script))).await.unwrap();
    let api = thread(&hub, dir.path()).await;
    hub.send_by(&api, "run the tests", &Caller::user("test"))
        .unwrap();
    let evs = until(&hub, &api, |e| ended(e, 2)).await;
    let ended_at = evs
        .iter()
        .find(|e| matches!(&e.body, EventBody::TaskUpdated(t) if !t.running))
        .unwrap()
        .ts;
    let start = evs
        .iter()
        .find(|e| e.kind() == "turn.started" && e.turn == Some(2))
        .unwrap();
    assert_eq!(start.source, Source::Hub);
    assert!(start.ts - ended_at >= chrono::Duration::from_std(WAKE_AFTER).unwrap());
    let woke = wakes(&evs);
    assert_eq!(woke.len(), 1, "{woke:?}");
    assert!(woke[0].contains("after your turn 1 ended"), "{}", woke[0]);
    assert!(
        woke[0].contains("- background tests (ended)"),
        "{}",
        woke[0]
    );
    let recorded = twr_updates(&evs, "auto_wake");
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0]["turn"], 1);
    // The woken turn's report is final, and a follow-up's.
    let report = evs.iter().rev().find_map(|e| match &e.body {
        EventBody::ReportReady(r) if r.turn == Some(2) => Some(r.clone()),
        _ => None,
    });
    let report = match report {
        Some(r) => r,
        None => until(&hub, &api, |e| {
            e.iter()
                .any(|e| e.kind() == "report.ready" && e.turn == Some(2))
        })
        .await
        .into_iter()
        .find_map(|e| match e.body {
            EventBody::ReportReady(r) if r.turn == Some(2) => Some(r),
            _ => None,
        })
        .unwrap(),
    };
    assert_eq!(report.ext.as_ref().unwrap()["twr"]["turn_from"], "runtime");
    tokio::time::sleep(WAKE_AFTER * 3).await;
    assert_eq!(wakes(&events(&hub, &api)).len(), 1);
    hub.shutdown().await;
}

/// A woken turn that yields again with new background work is woken again when that ends,
/// at most [`WAKE_CHAIN_MAX`] times in a row: then the hub records that it did not wake it.
#[tokio::test]
async fn wakes_in_a_row_are_capped() {
    let dir = tempfile::tempdir().unwrap();
    let mut script = vec![session()];
    let rounds = WAKE_CHAIN_MAX as usize + 1;
    for i in 0..rounds {
        if i > 0 {
            script.push(Step::AwaitSend);
        }
        let id = format!("t{i}");
        script.extend(interim(&id));
        script.push(Step::Sleep(50));
        script.push(Step::EmitSession(task(&id, false)));
    }
    script.push(Step::Sleep(60_000));
    let hub = Hub::open(opts(dir.path(), Some(script))).await.unwrap();
    let api = thread(&hub, dir.path()).await;
    hub.send_by(&api, "loop", &Caller::user("test")).unwrap();
    let evs = until(&hub, &api, |e| {
        !twr_updates(e, "auto_wake_skipped").is_empty()
    })
    .await;
    assert_eq!(wakes(&evs).len(), WAKE_CHAIN_MAX as usize);
    let skipped = twr_updates(&evs, "auto_wake_skipped");
    assert_eq!(skipped[0]["turn"], json!(rounds));
    let last = evs.iter().rev().find_map(|e| match &e.body {
        EventBody::ConversationUpdate(p) => Some(p.text.clone()),
        _ => None,
    });
    assert!(
        last.as_deref()
            .is_some_and(|t| t.contains("did not wake it") && t.contains("Send it a message")),
        "{last:?}"
    );
    // A message of the user's starts the count again.
    assert_eq!(wakes(&events(&hub, &api)).len(), WAKE_CHAIN_MAX as usize);
    hub.shutdown().await;
}

/// The runtime starts its own follow-up turn within the wait: the hub wakes nothing.
#[tokio::test]
async fn the_runtimes_own_follow_up_takes_the_wake() {
    let dir = tempfile::tempdir().unwrap();
    let mut ext = None;
    set_twr_ext(&mut ext, RUNTIME_TURN_KEY, "r1".into());
    let mut script = vec![session()];
    script.extend(interim("tests"));
    script.extend([
        Step::Sleep(100),
        Step::EmitSession(task("tests", false)),
        Step::Sleep(WAKE_AFTER.as_millis() as u64 / 2),
        Step::EmitSession(EventBody::TurnStarted(TurnStarted {
            from: TurnFrom::Runtime,
            prompt: "[runtime resumed after background activity]".into(),
            ext: ext.clone(),
            ..Default::default()
        })),
        Step::EmitSession(text("All tests passed.")),
        Step::EmitSession(EventBody::TurnEnded(TurnEnded {
            ext,
            ..Default::default()
        })),
        Step::Sleep(60_000),
    ]);
    let hub = Hub::open(opts(dir.path(), Some(script))).await.unwrap();
    let api = thread(&hub, dir.path()).await;
    hub.send_by(&api, "run the tests", &Caller::user("test"))
        .unwrap();
    until(&hub, &api, |e| ended(e, 2)).await;
    tokio::time::sleep(WAKE_AFTER * 3).await;
    let evs = events(&hub, &api);
    assert!(wakes(&evs).is_empty(), "{:#?}", kinds(&evs));
    assert!(twr_updates(&evs, "auto_wake").is_empty());
    hub.shutdown().await;
}

/// A prompt already queued for the worker (here the user's) is its next turn: no wake. A
/// thread the user stopped during the wait is not woken either.
#[tokio::test]
async fn a_stopped_thread_is_not_woken() {
    let dir = tempfile::tempdir().unwrap();
    let mut script = vec![session()];
    script.extend(interim("tests"));
    script.extend([
        Step::Sleep(200),
        Step::EmitSession(task("tests", false)),
        Step::Sleep(60_000),
    ]);
    let mut o = opts(dir.path(), Some(script));
    o.wake_after = Some(Duration::from_millis(1500));
    let hub = Hub::open(o).await.unwrap();
    let api = thread(&hub, dir.path()).await;
    hub.send_by(&api, "run the tests", &Caller::user("test"))
        .unwrap();
    until(&hub, &api, |e| {
        e.iter()
            .any(|e| matches!(&e.body, EventBody::TaskUpdated(t) if !t.running))
    })
    .await;
    hub.stop_thread(&api, "test").unwrap();
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let evs = events(&hub, &api);
    assert!(wakes(&evs).is_empty(), "{:#?}", kinds(&evs));
    assert!(twr_updates(&evs, "auto_wake").is_empty());
    hub.shutdown().await;
}

/// A job started not to wake its thread (`--no-wake`), the last of its work: the hub wakes
/// the idle thread with the job's result and its log.
#[tokio::test]
async fn a_quiet_job_ending_last_wakes_an_idle_thread() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path(), None)).await.unwrap();
    let api = thread(&hub, dir.path()).await;
    hub.send_by(
        &api,
        "!sleep 400\nStarted the build.",
        &Caller::user("test"),
    )
    .unwrap();
    until(&hub, &api, |e| e.iter().any(|e| e.kind() == "turn.started")).await;
    let req: JobStart = serde_json::from_value(json!({
        "argv": ["sh", "-c", "sleep 1.2; echo built"],
        "name": "build",
        "wake": false,
    }))
    .unwrap();
    let job = hub
        .start_job(&api, req, Source::User("test".into()))
        .unwrap();
    let job = job["job"]
        .as_str()
        .or(job["id"].as_str())
        .unwrap()
        .to_string();
    let evs = until(&hub, &api, |e| ended(e, 2)).await;
    let woke = wakes(&evs);
    assert_eq!(woke.len(), 1, "{woke:?}");
    assert!(woke[0].contains("job build: exited 0"), "{}", woke[0]);
    assert!(
        woke[0].contains(&format!("twr job {job}; its whole log: ")),
        "{}",
        woke[0]
    );
    // The interim report the wake was for counted the job.
    let interim = evs.iter().find_map(|e| match &e.body {
        EventBody::ReportReady(r) if r.turn == Some(1) => Some(r.clone()),
        _ => None,
    });
    assert_eq!(
        interim.unwrap().ext.unwrap()["twr"]["background_tasks"],
        json!(1)
    );
    hub.shutdown().await;
}

fn hung_card(evs: &[Event]) -> Option<AttentionRaised> {
    evs.iter().find_map(|e| match &e.body {
        EventBody::AttentionRaised(p) if is_hung_card(&p.ext) => Some(p.clone()),
        _ => None,
    })
}

fn stalls(evs: &[Event]) -> Vec<AttentionRaised> {
    evs.iter()
        .filter_map(|e| match &e.body {
            EventBody::AttentionRaised(p) if p.reason == AttentionReason::Stalled => {
                Some(p.clone())
            }
            _ => None,
        })
        .collect()
}

/// A turn stays open after the job it waited on ended, the job's wake waiting behind it: no
/// stall while the job ran, then a hung-turn card. Interrupt ends the turn and the job's
/// wake runs.
#[tokio::test]
async fn a_turn_left_open_behind_a_jobs_wake_gets_a_hung_card() {
    let dir = tempfile::tempdir().unwrap();
    let mut o = opts(dir.path(), None);
    o.stall_after = Some(Duration::from_millis(600));
    let hub = Hub::open(o).await.unwrap();
    let api = thread(&hub, dir.path()).await;
    hub.send_by(&api, "!sleep 600000", &Caller::user("test"))
        .unwrap();
    until(&hub, &api, |e| e.iter().any(|e| e.kind() == "turn.started")).await;
    let req: JobStart = serde_json::from_value(json!({
        "argv": ["sh", "-c", "sleep 2"],
        "name": "gates",
    }))
    .unwrap();
    hub.start_job(&api, req, Source::User("test".into()))
        .unwrap();
    let evs = until(&hub, &api, |e| hung_card(e).is_some()).await;
    // While the job ran, the silent turn was no stall.
    let job_end = evs
        .iter()
        .find(|e| matches!(&e.body, EventBody::TaskUpdated(t) if !t.running))
        .unwrap()
        .ts;
    assert!(evs
        .iter()
        .filter(|e| matches!(&e.body, EventBody::AttentionRaised(p) if p.reason == AttentionReason::Stalled))
        .all(|e| e.ts > job_end));
    let card = hung_card(&evs).unwrap();
    assert_eq!(card.options, [HUNG_INTERRUPT, HUNG_WAIT]);
    assert!(
        card.summary
            .starts_with("api's turn 1 seems hung: no output from its runtime for"),
        "{}",
        card.summary
    );
    assert!(card.summary.contains("the end of job "), "{}", card.summary);
    assert_eq!(
        stalls(&evs)
            .iter()
            .filter(|s| !is_hung_card(&s.ext))
            .count(),
        0
    );
    hub.respond(&card.attn_id, Answer::new(HUNG_INTERRUPT), "test")
        .unwrap();
    let evs = until(&hub, &api, |e| {
        e.iter().any(|e| {
            matches!(&e.body, EventBody::TurnStarted(p) if e.turn == Some(2) && p.prompt.starts_with("[twr job "))
        })
    })
    .await;
    assert!(evs.iter().any(|e| matches!(&e.body,
        EventBody::TurnEnded(t) if e.turn == Some(1) && t.result == TurnResult::Interrupted)));
    hub.shutdown().await;
}

/// Wait: the card closes, and no other hung-turn card comes for that turn (a plain stall may).
#[tokio::test]
async fn waiting_on_a_hung_turn_raises_no_other_card_for_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut o = opts(dir.path(), None);
    o.stall_after = Some(Duration::from_millis(400));
    let hub = Hub::open(o).await.unwrap();
    let api = thread(&hub, dir.path()).await;
    hub.send_by(&api, "!sleep 600000", &Caller::user("test"))
        .unwrap();
    until(&hub, &api, |e| e.iter().any(|e| e.kind() == "turn.started")).await;
    // A message of the user's waits behind the silent turn.
    hub.send_by(&api, "and then this", &Caller::user("test"))
        .unwrap();
    let evs = until(&hub, &api, |e| hung_card(e).is_some()).await;
    let card = hung_card(&evs).unwrap();
    assert!(
        card.summary.contains("while a message of yours waits"),
        "{}",
        card.summary
    );
    hub.respond(&card.attn_id, Answer::new(HUNG_WAIT), "test")
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let evs = events(&hub, &api);
    assert_eq!(
        stalls(&evs).iter().filter(|s| is_hung_card(&s.ext)).count(),
        1,
        "{:#?}",
        stalls(&evs)
    );
    assert!(evs.iter().all(|e| e.kind() != "turn.ended"));
    hub.shutdown().await;
}

/// A plain stall open when the turn turns out hung: the hung-turn card replaces it in one
/// batch, closing it `by: timeout` with `ext.twr.superseded_by` naming the card (its activity
/// did not resume).
#[tokio::test]
async fn a_hung_card_closes_the_plain_stall_it_replaces_as_superseded() {
    let dir = tempfile::tempdir().unwrap();
    let mut o = opts(dir.path(), None);
    o.stall_after = Some(Duration::from_millis(400));
    let hub = Hub::open(o).await.unwrap();
    let api = thread(&hub, dir.path()).await;
    hub.send_by(&api, "!sleep 600000", &Caller::user("test"))
        .unwrap();
    until(&hub, &api, |e| e.iter().any(|e| e.kind() == "turn.started")).await;
    let evs = until(&hub, &api, |e| !stalls(e).is_empty()).await;
    let plain = stalls(&evs)[0].clone();
    assert!(!is_hung_card(&plain.ext));
    // A message of the user's then waits behind the silent turn.
    hub.send_by(&api, "and then this", &Caller::user("test"))
        .unwrap();
    let evs = until(&hub, &api, |e| hung_card(e).is_some()).await;
    let card = hung_card(&evs).unwrap();
    let at = evs
        .iter()
        .position(
            |e| matches!(&e.body, EventBody::AttentionResolved(p) if p.attn_id == plain.attn_id),
        )
        .expect("the plain stall is closed");
    let EventBody::AttentionResolved(closed) = &evs[at].body else {
        unreachable!()
    };
    assert_eq!(closed.by, ResolvedBy::Timeout);
    assert_eq!(superseded_by(&closed.ext), Some(card.attn_id.as_str()));
    assert!(!went_on(&closed.ext), "it did not go on: {closed:?}");
    // In the card's batch, right before it.
    assert!(
        matches!(&evs[at + 1].body, EventBody::AttentionRaised(p) if p.attn_id == card.attn_id),
        "{:#?}",
        &evs[at..]
    );
    hub.shutdown().await;
}

/// A worker that yielded with its runtime's watcher running and a job of its own: no stall
/// while the job runs (the watcher waits on it); once it ended, the silent watcher stalls.
#[tokio::test]
async fn background_work_waiting_on_a_job_does_not_stall() {
    let dir = tempfile::tempdir().unwrap();
    let mut script = vec![session()];
    script.extend(interim("watcher"));
    script.push(Step::Sleep(60_000));
    let mut o = opts(dir.path(), Some(script));
    o.stall_after = Some(Duration::from_millis(500));
    o.wake_after = Some(Duration::ZERO);
    let hub = Hub::open(o).await.unwrap();
    let api = thread(&hub, dir.path()).await;
    hub.send_by(&api, "run the gates", &Caller::user("test"))
        .unwrap();
    until(&hub, &api, |e| ended(e, 1)).await;
    let req: JobStart = serde_json::from_value(json!({
        "argv": ["sh", "-c", "sleep 2"],
        "name": "gates",
        "wake": false,
    }))
    .unwrap();
    hub.start_job(&api, req, Source::User("test".into()))
        .unwrap();
    let evs = until(&hub, &api, |e| !stalls(e).is_empty()).await;
    let job_end = evs
        .iter()
        .find(|e| matches!(&e.body, EventBody::TaskUpdated(t) if !t.running))
        .map(|e| e.ts)
        .expect("no stall before the job ended");
    let stall = evs
        .iter()
        .find(|e| matches!(&e.body, EventBody::AttentionRaised(p) if p.reason == AttentionReason::Stalled))
        .unwrap();
    assert!(stall.ts > job_end);
    assert_eq!(
        stalls(&evs)[0].summary,
        "No activity for 1 s from its background tasks"
    );
    hub.shutdown().await;
}

/// A job the log shows running that this hub does not run (one a stopped hub ran, before it
/// is recorded lost; here one recorded by hand) holds back no stall: only the jobs the hub runs
/// do, however recently their task was renewed.
#[tokio::test]
async fn a_job_this_hub_does_not_run_holds_back_no_stall() {
    let dir = tempfile::tempdir().unwrap();
    let mut script = vec![session()];
    script.extend(interim("watcher"));
    script.push(Step::Sleep(60_000));
    let mut o = opts(dir.path(), Some(script));
    o.stall_after = Some(Duration::from_millis(500));
    o.wake_after = Some(Duration::ZERO);
    let hub = Hub::open(o).await.unwrap();
    let api = thread(&hub, dir.path()).await;
    let mut ext = None;
    set_twr_ext(&mut ext, "job", json!({"id": "ghost", "wake": false}));
    hub.append(
        &api,
        None,
        Source::Hub,
        EventBody::TaskUpdated(TaskUpdated {
            task_id: format!("{JOB_TASK_PREFIX}ghost"),
            running: true,
            description: "job ghost: running".into(),
            ext,
            ..Default::default()
        }),
        None,
    )
    .unwrap();
    hub.send_by(&api, "run the gates", &Caller::user("test"))
        .unwrap();
    let evs = until(&hub, &api, |e| !stalls(e).is_empty()).await;
    assert!(ended(&evs, 1));
    assert!(hub
        .thread_view(&api)
        .unwrap()
        .background_tasks
        .contains_key("job:ghost"));
    hub.shutdown().await;
}

/// A worker whose background task reported progress the hub did not all record
/// (`hub::coalesce`) is still woken once when the task ends.
#[tokio::test]
async fn coalesced_progress_still_wakes_the_worker_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut script = vec![session()];
    script.extend(interim("tests"));
    for i in 0..20 {
        script.push(Step::Sleep(10));
        script.push(Step::EmitSession(EventBody::TaskUpdated(TaskUpdated {
            task_id: "tests".into(),
            running: true,
            description: format!("background tests: {i} passed"),
            ..Default::default()
        })));
    }
    script.extend([
        Step::EmitSession(task("tests", false)),
        Step::AwaitSend,
        Step::Emit(text("All tests passed.")),
        Step::Emit(EventBody::TurnEnded(TurnEnded::default())),
        Step::Sleep(60_000),
    ]);
    let hub = Hub::open(opts(dir.path(), Some(script))).await.unwrap();
    let api = thread(&hub, dir.path()).await;
    hub.send_by(&api, "run the tests", &Caller::user("test"))
        .unwrap();
    let evs = until(&hub, &api, |e| ended(e, 2)).await;
    let updates = evs.iter().filter(|e| e.kind() == "task.updated").count();
    assert_eq!(updates, 2, "{:#?}", kinds(&evs));
    assert_eq!(hub.info()["coalesced_events"], 20);
    let woke = wakes(&evs);
    assert_eq!(woke.len(), 1, "{woke:?}");
    assert!(
        woke[0].contains("- background tests (ended)"),
        "{}",
        woke[0]
    );
    tokio::time::sleep(WAKE_AFTER * 3).await;
    assert_eq!(wakes(&events(&hub, &api)).len(), 1);
    hub.shutdown().await;
}

fn runtime_turn(id: &str, no_run: bool) -> [Step; 2] {
    let mut ext = None;
    set_twr_ext(&mut ext, RUNTIME_TURN_KEY, id.into());
    let mut end_ext = ext.clone();
    if no_run {
        set_twr_ext(&mut end_ext, "no_run", true.into());
    }
    [
        Step::EmitSession(EventBody::TurnStarted(TurnStarted {
            from: TurnFrom::Runtime,
            prompt: "[runtime resumed after background activity]".into(),
            ext,
            ..Default::default()
        })),
        Step::EmitSession(EventBody::TurnEnded(TurnEnded {
            ext: end_ext,
            ..Default::default()
        })),
    ]
}

fn report_of(evs: &[Event], turn: u64) -> Option<ReportReady> {
    evs.iter().rev().find_map(|e| match &e.body {
        EventBody::ReportReady(r) if r.turn == Some(turn) => Some(r.clone()),
        _ => None,
    })
}

/// A turn the runtime opened for a background task's notification that saw no run
/// (`ext.twr.no_run`, the Claude Code adapter's): its report repeats the interim one, and the
/// hub wakes the worker at once for the work that turn handled nothing of.
#[tokio::test]
async fn a_turn_that_saw_no_run_leaves_the_wake_to_the_hub() {
    let dir = tempfile::tempdir().unwrap();
    let mut script = vec![session()];
    script.extend(interim("tests"));
    script.extend([Step::Sleep(100), Step::EmitSession(task("tests", false))]);
    script.extend(runtime_turn("r1", true));
    script.extend([
        Step::AwaitSend,
        Step::Emit(text("The tests passed.")),
        Step::Emit(EventBody::TurnEnded(TurnEnded::default())),
        Step::Sleep(60_000),
    ]);
    let mut o = opts(dir.path(), Some(script));
    // Longer than the test waits: the wake after `no_run` does not wait for it.
    o.wake_after = Some(Duration::from_secs(60));
    let hub = Hub::open(o).await.unwrap();
    let api = thread(&hub, dir.path()).await;
    hub.send_by(&api, "run the tests", &Caller::user("test"))
        .unwrap();
    let evs = until(&hub, &api, |e| ended(e, 3)).await;
    let repeated = report_of(&evs, 2).expect("the no-run turn's report");
    let first = report_of(&evs, 1).unwrap();
    assert_eq!(repeated.summary, first.summary);
    let twr = &repeated.ext.as_ref().unwrap()["twr"];
    assert_eq!(
        (&twr["no_run_of"], &twr["background_tasks"]),
        (&json!(1), &json!(1))
    );
    let woke = wakes(&evs);
    assert_eq!(woke.len(), 1, "{woke:?}");
    assert!(woke[0].contains("after your turn 1 ended"), "{}", woke[0]);
    assert!(
        woke[0].contains("- background tests (ended)"),
        "{}",
        woke[0]
    );
    hub.shutdown().await;
}

/// The same with work of the worker's still running when that turn ends: the wake waits for
/// the last task's end, as after any interim report.
#[tokio::test]
async fn a_no_run_turns_wake_waits_for_the_rest_of_the_work() {
    let dir = tempfile::tempdir().unwrap();
    let mut script = vec![
        session(),
        Step::EmitSession(task("tests", true)),
        Step::EmitSession(task("lint", true)),
        Step::Emit(text("Running them in the background.")),
        Step::Emit(EventBody::TurnEnded(TurnEnded::default())),
        Step::Sleep(100),
        Step::EmitSession(task("tests", false)),
    ];
    script.extend(runtime_turn("r1", true));
    script.extend([
        Step::Sleep(1000),
        Step::EmitSession(task("lint", false)),
        Step::AwaitSend,
        Step::Emit(text("Both passed.")),
        Step::Emit(EventBody::TurnEnded(TurnEnded::default())),
        Step::Sleep(60_000),
    ]);
    let hub = Hub::open(opts(dir.path(), Some(script))).await.unwrap();
    let api = thread(&hub, dir.path()).await;
    hub.send_by(&api, "run the checks", &Caller::user("test"))
        .unwrap();
    let evs = until(&hub, &api, |e| ended(e, 3)).await;
    let lint_end = evs
        .iter()
        .find(|e| matches!(&e.body, EventBody::TaskUpdated(t) if !t.running && t.task_id == "lint"))
        .unwrap()
        .ts;
    let start = evs
        .iter()
        .find(|e| e.kind() == "turn.started" && e.turn == Some(3))
        .unwrap();
    assert!(start.ts - lint_end >= chrono::Duration::from_std(WAKE_AFTER).unwrap());
    let woke = wakes(&evs);
    assert_eq!(woke.len(), 1, "{woke:?}");
    assert!(
        woke[0].contains("- background tests (ended)")
            && woke[0].contains("- background lint (ended)"),
        "{}",
        woke[0]
    );
    hub.shutdown().await;
}

/// A task that ends while a turn that sees no run is open is unhandled too: the wake lists it.
#[tokio::test]
async fn a_task_ending_during_a_no_run_turn_is_in_the_wake() {
    let dir = tempfile::tempdir().unwrap();
    let [start, end] = runtime_turn("r1", true);
    let script = vec![
        session(),
        Step::EmitSession(task("tests", true)),
        Step::EmitSession(task("lint", true)),
        Step::Emit(text("Running them in the background.")),
        Step::Emit(EventBody::TurnEnded(TurnEnded::default())),
        Step::Sleep(100),
        Step::EmitSession(task("tests", false)),
        start,
        Step::Sleep(100),
        Step::EmitSession(task("lint", false)),
        Step::Sleep(100),
        end,
        Step::AwaitSend,
        Step::Emit(text("Both passed.")),
        Step::Emit(EventBody::TurnEnded(TurnEnded::default())),
        Step::Sleep(60_000),
    ];
    let hub = Hub::open(opts(dir.path(), Some(script))).await.unwrap();
    let api = thread(&hub, dir.path()).await;
    hub.send_by(&api, "run the checks", &Caller::user("test"))
        .unwrap();
    let evs = until(&hub, &api, |e| ended(e, 3)).await;
    let woke = wakes(&evs);
    assert_eq!(woke.len(), 1, "{woke:?}");
    assert!(
        woke[0].contains("- background tests (ended)")
            && woke[0].contains("- background lint (ended)"),
        "{}",
        woke[0]
    );
    hub.shutdown().await;
}

/// The user interrupted a silent turn to send the next message now, and its runtime ignores
/// the interrupt: no hung-turn card comes for a turn being interrupted, so the plain stall
/// does.
#[tokio::test]
async fn an_interrupt_the_runtime_ignores_still_stalls() {
    let dir = tempfile::tempdir().unwrap();
    let mut o = opts(dir.path(), None);
    o.stall_after = Some(Duration::from_millis(500));
    let hub = Hub::open(o).await.unwrap();
    let api = thread(&hub, dir.path()).await;
    let user = Caller::user("test");
    hub.send_by(&api, "!deaf\n!sleep 600000", &user).unwrap();
    until(&hub, &api, |e| e.iter().any(|e| e.kind() == "turn.started")).await;
    hub.send_by(&api, "next", &user).unwrap();
    let queue = hub.thread_queue(&api, &user).unwrap();
    let r = queue["items"][0]["ref"].as_i64().unwrap();
    hub.send_now(&api, r, &user).unwrap();
    let evs = until(&hub, &api, |e| !stalls(e).is_empty()).await;
    assert!(evs.iter().any(|e| e.kind() == "turn.interrupted"));
    assert!(stalls(&evs).iter().all(|s| !is_hung_card(&s.ext)));
    hub.shutdown().await;
}

/// A `!` command of the user's run in the wait is no work of the worker's: the wake goes out.
#[tokio::test]
async fn a_users_shell_command_does_not_cancel_a_wake() {
    let dir = tempfile::tempdir().unwrap();
    let mut script = vec![session()];
    script.extend(interim("tests"));
    script.extend([
        Step::Sleep(100),
        Step::EmitSession(task("tests", false)),
        Step::AwaitSend,
        Step::Emit(text("Done.")),
        Step::Emit(EventBody::TurnEnded(TurnEnded::default())),
        Step::Sleep(60_000),
    ]);
    let mut o = opts(dir.path(), Some(script));
    o.wake_after = Some(Duration::from_millis(1500));
    let hub = Hub::open(o).await.unwrap();
    let api = thread(&hub, dir.path()).await;
    hub.send_by(&api, "run the tests", &Caller::user("test"))
        .unwrap();
    until(&hub, &api, |e| {
        e.iter()
            .any(|e| matches!(&e.body, EventBody::TaskUpdated(t) if !t.running))
    })
    .await;
    let run: twr_hub::hub::ShellRun =
        serde_json::from_value(json!({"thread": api, "command": "sleep 3"})).unwrap();
    hub.shell_run(run, &Caller::user("test")).unwrap();
    until(&hub, &api, |e| {
        e.iter()
            .any(|e| matches!(&e.body, EventBody::TaskUpdated(t) if t.running && t.task_id.starts_with("job:")))
    })
    .await;
    let evs = until(&hub, &api, |e| ended(e, 2)).await;
    assert_eq!(wakes(&evs).len(), 1);
    hub.shutdown().await;
}
