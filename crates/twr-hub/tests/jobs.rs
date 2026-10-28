//! Jobs (`hub::jobs`): run for a thread, queued on the machine's job slots, tracked as the
//! thread's background work, ended (exit, cancel, timeout), and told to the thread.

use std::os::unix::process::CommandExt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::time::timeout;
use twr_adapters::fake::{FakeAdapter, Step};
use twr_core::adapter::Adapter;
use twr_core::config::Config;
use twr_core::event::*;
use twr_hub::{AgentJobBy, Hub, HubOptions, JobStart, NewThread};

const WAIT: Duration = Duration::from_secs(20);

fn opts(dir: &Path, slots: u64) -> HubOptions {
    let mut config = Config::default();
    config.threads.job_slots = slots;
    let mut o = HubOptions::new(dir.join("data").join("twr.db"), config);
    o.job_gate_exe = Some(env!("CARGO_BIN_EXE_twr-job-gate").into());
    o.adapters = Arc::new(|name: &str| -> Option<Arc<dyn Adapter>> {
        (name == "fake").then(|| Arc::new(FakeAdapter::new()) as Arc<dyn Adapter>)
    });
    o.git_tick = Some(Duration::ZERO);
    o.stall_after = Some(Duration::ZERO);
    o.disk_tick = Some(Duration::ZERO);
    o
}

async fn thread(hub: &Hub, project: &Path, name: &str) -> String {
    std::fs::create_dir_all(project).unwrap();
    let t = hub
        .new_thread(
            NewThread {
                project: project.to_string_lossy().into_owned(),
                runtime: Some("fake".into()),
                name: Some(name.into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap();
    t["id"].as_str().unwrap().to_string()
}

fn start(hub: &Hub, thread: &str, argv: &[&str], extra: Value) -> Value {
    let mut req = json!({"argv": argv});
    for (k, v) in extra.as_object().unwrap() {
        req[k] = v.clone();
    }
    let req: JobStart = serde_json::from_value(req).unwrap();
    hub.start_job(thread, req, Source::User("test".into()))
        .unwrap()
}

/// Waits until `pred` holds of the thread's events; returns them.
async fn until(hub: &Hub, thread: &str, pred: impl Fn(&[Event]) -> bool) -> Vec<Event> {
    timeout(WAIT, async {
        loop {
            let (_, evs) = hub.thread_events(thread, 0, 10_000).unwrap();
            if pred(&evs) {
                return evs;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("timed out")
}

/// A process recorded as thread `t`'s runtime (its `thread.started` with the process's pid
/// and start time, as a runtime of the hub's own box reports itself): what an agent's job
/// request descends from. Kill the process when done.
fn recorded_runtime(hub: &Hub, t: &str) -> (std::process::Child, AgentJobBy) {
    let child = std::process::Command::new("sleep")
        .arg("300")
        .spawn()
        .unwrap();
    let id = twr_runner::reap::ProcessId {
        pid: child.id(),
        start_time: twr_runner::reap::process_start_time(child.id()).unwrap(),
    };
    let ext = json!({"twr": {"pid": id.pid, "start_time": id.start_time}});
    hub.append(
        t,
        None,
        Source::Adapter("fake".into()),
        EventBody::ThreadStarted(SessionPayload {
            session_id: Some("s-recorded".into()),
            ext: Some(serde_json::from_value(ext).unwrap()),
            ..Default::default()
        }),
        None,
    )
    .unwrap();
    (child, AgentJobBy::Runtime(id))
}

fn job_end<'a>(evs: &'a [Event], job: &str) -> Option<&'a Value> {
    evs.iter().find_map(|e| match &e.body {
        EventBody::TaskUpdated(p) if !p.running && p.task_id == format!("job:{job}") => {
            p.ext.as_ref()?.get("twr")?.get("job")
        }
        _ => None,
    })
}

fn woke_with(evs: &[Event], needle: &str) -> bool {
    evs.iter()
        .any(|e| matches!(&e.body, EventBody::TurnStarted(p) if p.from == TurnFrom::Runtime && p.prompt.contains(needle)))
}

#[tokio::test]
async fn a_job_runs_in_the_threads_directory_and_wakes_it_with_its_output() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path(), 2)).await.unwrap();
    let project = dir.path().join("proj");
    let t = thread(&hub, &project, "api").await;
    let r = start(
        &hub,
        &t,
        &[
            "sh",
            "-c",
            "echo hello from $(pwd); echo $TWR_THREAD; exit 3",
        ],
        json!({"name": "check"}),
    );
    let job = r["job"].as_str().unwrap().to_string();
    assert_eq!(r["thread_name"], "api");
    assert_eq!(r["name"], "check");
    let evs = until(&hub, &t, |evs| woke_with(evs, "[twr job")).await;
    let end = job_end(&evs, &job).expect("ended");
    assert_eq!(end["outcome"], "exited");
    assert_eq!(end["exit"], 3);
    let woke = evs
        .iter()
        .find_map(|e| match &e.body {
            EventBody::TurnStarted(p) if p.from == TurnFrom::Runtime => Some(p.prompt.clone()),
            _ => None,
        })
        .unwrap();
    assert!(woke.contains(&format!("[twr job {job} check]")), "{woke}");
    assert!(woke.contains("exited 3 after"), "{woke}");
    let real = std::fs::canonicalize(&project).unwrap();
    assert!(
        woke.contains(&format!("hello from {}", real.display())),
        "{woke}"
    );
    assert!(woke.contains(&t), "its processes carry the thread: {woke}");
    // Its log stays readable after it ended.
    let log = hub.job_log(&job, Some(5)).unwrap();
    assert!(
        log["text"].as_str().unwrap().contains("hello from"),
        "{log}"
    );
    assert!(hub.list_jobs(None).unwrap().as_array().unwrap().is_empty());
    hub.request_shutdown();
}

#[tokio::test]
async fn jobs_wait_for_slots_in_turn_and_count_as_background_work() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path(), 1)).await.unwrap();
    let t = thread(&hub, &dir.path().join("proj"), "api").await;
    let first = start(&hub, &t, &["sleep", "1"], json!({"wake": false}));
    let second = start(&hub, &t, &["true"], json!({"wake": false}));
    assert_eq!(second["position"], 1);
    // A queued job's log is there, empty.
    let log = hub.job_log(second["job"].as_str().unwrap(), None).unwrap();
    assert_eq!(log["text"], "", "{log}");
    // Both are the thread's background work meanwhile.
    let view = hub.thread_view(&t).unwrap();
    assert_eq!(view.background_tasks.len(), 2);
    assert!(!view.runtime_tasks());
    timeout(WAIT, async {
        loop {
            let jobs = hub.list_jobs(Some(&t)).unwrap();
            let states: Vec<_> = jobs
                .as_array()
                .unwrap()
                .iter()
                .map(|j| j["state"].clone())
                .collect();
            if states == [json!("running"), json!("queued")] {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the first runs while the second waits for the one slot");
    // More slots than the machine has are refused.
    let too_many: JobStart = serde_json::from_value(json!({"argv": ["true"], "slots": 2})).unwrap();
    let e = hub
        .start_job(&t, too_many, Source::User("test".into()))
        .unwrap_err();
    assert!(e.message.contains("slots must be 1 to 1"), "{}", e.message);
    let (a, b) = (
        first["job"].as_str().unwrap(),
        second["job"].as_str().unwrap(),
    );
    let evs = until(&hub, &t, |evs| {
        job_end(evs, a).is_some() && job_end(evs, b).is_some()
    })
    .await;
    assert_eq!(job_end(&evs, b).unwrap()["exit"], 0);
    assert!(hub.thread_view(&t).unwrap().background_tasks.is_empty());
    assert!(!woke_with(&evs, "[twr job"), "no wake asked");
    hub.request_shutdown();
}

/// What the hub shows of a job it runs (`thread.get`, the activity line) is what the job does
/// now, not its last renewal (every 5 min): a queued job moves up when one before it goes.
#[tokio::test]
async fn a_jobs_shown_state_is_what_it_does_now() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path(), 1)).await.unwrap();
    let t = thread(&hub, &dir.path().join("proj"), "api").await;
    let job = |name: &str| {
        start(
            &hub,
            &t,
            &["sleep", "30"],
            json!({"wake": false, "name": name}),
        )["job"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let a = job("a");
    until(&hub, &t, |evs| {
        evs.iter().any(|e| {
            matches!(&e.body, EventBody::TaskUpdated(p)
            if p.task_id == format!("job:{a}") && p.description == "job a: running")
        })
    })
    .await;
    let (b, c) = (job("b"), job("c"));
    let shown = |job: &str| {
        hub.get_thread(&t).unwrap()["background_tasks"][format!("job:{job}")]["description"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(shown(&a), "job a: running");
    assert_eq!(shown(&c), "job c: queued (1 ahead)");
    hub.cancel_job(&b, None).unwrap();
    until(&hub, &t, |evs| job_end(evs, &b).is_some()).await;
    assert_eq!(shown(&c), "job c: queued (0 ahead)");
    let activity = hub.thread_activity()[&t]["activity"].clone();
    assert!(
        activity
            .as_str()
            .unwrap()
            .contains("job c: queued (0 ahead)"),
        "{activity}"
    );
    // The log still says what its start said: nothing was recorded for the move.
    let recorded = hub.thread_view(&t).unwrap().background_tasks[&format!("job:{c}")]
        .description
        .clone();
    assert_eq!(recorded, "job c: queued (1 ahead)");
    for j in [&a, &c] {
        hub.cancel_job(j, None).unwrap();
    }
    until(&hub, &t, |evs| {
        job_end(evs, &a).is_some() && job_end(evs, &c).is_some()
    })
    .await;
    hub.request_shutdown();
}

#[tokio::test]
async fn a_cancel_or_a_timeout_ends_its_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path(), 2)).await.unwrap();
    let t = thread(&hub, &dir.path().join("proj"), "api").await;
    let pidfile = dir.path().join("child.pid");
    let script = format!("sleep 60 & echo $! > {}; wait", pidfile.display());
    let r = start(&hub, &t, &["sh", "-c", &script], json!({"wake": false}));
    let job = r["job"].as_str().unwrap().to_string();
    timeout(WAIT, async {
        while !pidfile.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    hub.cancel_job(&job[..6], None).unwrap();
    let evs = until(&hub, &t, |evs| job_end(evs, &job).is_some()).await;
    assert_eq!(job_end(&evs, &job).unwrap()["outcome"], "cancelled");
    // Its background child went with it.
    let pid: u32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    timeout(WAIT, async {
        while Path::new(&format!("/proc/{pid}")).exists()
            && !std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .is_ok_and(|s| s.contains(") Z"))
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the job's child is gone");

    let r = start(
        &hub,
        &t,
        &["sleep", "30"],
        json!({"timeout_secs": 1, "wake": false}),
    );
    let job = r["job"].as_str().unwrap().to_string();
    let evs = until(&hub, &t, |evs| job_end(evs, &job).is_some()).await;
    assert_eq!(job_end(&evs, &job).unwrap()["outcome"], "timed_out");
    hub.request_shutdown();
}

#[tokio::test]
async fn a_job_a_stopped_hub_ran_is_lost_at_the_next_start() {
    let dir = tempfile::tempdir().unwrap();
    let t = {
        let hub = Hub::open(opts(dir.path(), 2)).await.unwrap();
        let t = thread(&hub, &dir.path().join("proj"), "api").await;
        lose(&hub, &t, "J0LOST", true);
        lose(&hub, &t, "J1QUIET", false);
        t
    };
    // What is left of it: a process with the job's marker, which the next hub ends.
    let mut left = std::process::Command::new("sleep")
        .arg("300")
        .env("TWR_AGENT", format!("{t}:job-J0LOST"))
        .spawn()
        .unwrap();
    let hub = Hub::open(opts(dir.path(), 2)).await.unwrap();
    assert!(hub.thread_view(&t).unwrap().background_tasks.is_empty());
    let evs = until(&hub, &t, |evs| job_end(evs, "J0LOST").is_some()).await;
    assert_eq!(job_end(&evs, "J0LOST").unwrap()["outcome"], "lost");
    let evs = until(&hub, &t, |evs| {
        job_end(evs, "J1QUIET").is_some() && woke_with(evs, "[twr job J0LOST]")
    })
    .await;
    assert_eq!(job_end(&evs, "J1QUIET").unwrap()["outcome"], "lost");
    let status = timeout(WAIT, async {
        loop {
            if let Some(s) = left.try_wait().unwrap() {
                break s;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the lost job's process was left running");
    assert!(!status.success());
    // One started not to tell its thread never does.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (_, evs) = hub.thread_events(&t, 0, 10_000).unwrap();
    assert!(
        !woke_with(&evs, "[twr job J1QUIET]"),
        "a quiet job wakes nothing"
    );
    hub.request_shutdown();
}

/// Records job `job` running on thread `t`, as a hub that then stopped would have left it.
fn lose(hub: &Hub, t: &str, job: &str, wake: bool) {
    hub.append(
        t,
        None,
        Source::Hub,
        EventBody::TaskUpdated(TaskUpdated {
            task_id: format!("job:{job}"),
            running: true,
            description: "job build: running".into(),
            ext: Some(
                serde_json::from_value(json!({"twr": {"job": {"id": job, "wake": wake}}})).unwrap(),
            ),
            ..Default::default()
        }),
        None,
    )
    .unwrap();
}

#[tokio::test]
async fn a_lost_job_of_a_stopped_thread_wakes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let t = {
        let hub = Hub::open(opts(dir.path(), 2)).await.unwrap();
        let t = thread(&hub, &dir.path().join("proj"), "api").await;
        lose(&hub, &t, "J0LOST", true);
        // Stopped (the job's cancel was recorded) before the hub died.
        hub.stop_thread_by(&t, &twr_hub::Caller::user("test"))
            .unwrap();
        until(&hub, &t, |evs| {
            evs.iter()
                .any(|e| matches!(e.body, EventBody::ThreadStopped(_)))
        })
        .await;
        t
    };
    let hub = Hub::open(opts(dir.path(), 2)).await.unwrap();
    let evs = until(&hub, &t, |evs| job_end(evs, "J0LOST").is_some()).await;
    assert_eq!(job_end(&evs, "J0LOST").unwrap()["outcome"], "lost");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (_, evs) = hub.thread_events(&t, 0, 10_000).unwrap();
    assert!(
        !woke_with(&evs, "[twr job"),
        "a stopped thread stays stopped"
    );
    assert!(hub.thread_view(&t).unwrap().stopped);
    hub.request_shutdown();
}

#[tokio::test]
async fn stopping_a_thread_cancels_its_jobs_without_waking_it() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path(), 2)).await.unwrap();
    let t = thread(&hub, &dir.path().join("proj"), "api").await;
    let (mut runtime, by) = recorded_runtime(&hub, &t);
    let r = start(&hub, &t, &["sleep", "30"], json!({}));
    let job = r["job"].as_str().unwrap().to_string();
    timeout(WAIT, async {
        loop {
            let jobs = hub.list_jobs(Some(&t)).unwrap();
            if jobs[0]["state"] == "running" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    hub.stop_thread_by(&t, &twr_hub::Caller::user("test"))
        .unwrap();
    let evs = until(&hub, &t, |evs| job_end(evs, &job).is_some()).await;
    assert_eq!(job_end(&evs, &job).unwrap()["outcome"], "cancelled");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (_, evs) = hub.thread_events(&t, 0, 10_000).unwrap();
    assert!(!woke_with(&evs, "[twr job"), "a stop wakes nothing");
    assert!(hub.thread_view(&t).unwrap().stopped);
    // A process of its runtime that outlived the stop can't start another: the stop ended the
    // runtime it descends from.
    let req: JobStart = serde_json::from_value(json!({"argv": ["true"]})).unwrap();
    let err = hub
        .start_agent_job(&t, req, Source::User("agent".into()), by)
        .unwrap_err();
    assert_eq!(err.code, "relay_forbidden", "{err:?}");
    assert!(
        err.message.contains("no longer api's live runtime"),
        "{err:?}"
    );
    let _ = runtime.kill();
    let _ = runtime.wait();
    hub.request_shutdown();
}

#[tokio::test]
async fn bad_requests_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path(), 2)).await.unwrap();
    let t = thread(&hub, &dir.path().join("proj"), "api").await;
    for (req, why) in [
        (json!({"argv": []}), "argv is empty"),
        (
            json!({"argv": ["true"], "timeout_secs": 0}),
            "timeout_secs must be 1 to",
        ),
        (
            json!({"argv": ["true"], "timeout_secs": u64::MAX}),
            "timeout_secs must be 1 to",
        ),
        (
            json!({"argv": ["true"], "slots": 0}),
            "slots must be 1 to 2",
        ),
    ] {
        let req: JobStart = serde_json::from_value(req).unwrap();
        let e = hub
            .start_job(&t, req, Source::User("test".into()))
            .unwrap_err();
        assert!(e.message.contains(why), "{}", e.message);
    }
    // One whose event can't be recorded is no job, and leaves no log behind.
    let db = rusqlite::Connection::open(dir.path().join("data/twr.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER inject BEFORE INSERT ON events WHEN NEW.kind = 'task.updated'
         BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    )
    .unwrap();
    let req: JobStart = serde_json::from_value(json!({"argv": ["true"]})).unwrap();
    let e = hub
        .start_job(&t, req, Source::User("test".into()))
        .unwrap_err();
    assert!(e.message.contains("injected"), "{e:?}");
    let logs: Vec<_> = std::fs::read_dir(dir.path().join("data/jobs"))
        .unwrap()
        .collect();
    assert!(logs.is_empty(), "{logs:?}");
    db.execute_batch("DROP TRIGGER inject").unwrap();
    assert!(hub
        .list_jobs(Some(&t))
        .unwrap()
        .as_array()
        .unwrap()
        .is_empty());
    // A prefix two live jobs share names neither.
    let a = start(&hub, &t, &["sleep", "30"], json!({"wake": false}));
    let b = start(&hub, &t, &["sleep", "30"], json!({"wake": false}));
    let (a, b) = (a["job"].as_str().unwrap(), b["job"].as_str().unwrap());
    let shared = a.chars().zip(b.chars()).take_while(|(x, y)| x == y).count();
    if shared >= 4 {
        let e = hub.job_log(&a[..shared], None).unwrap_err();
        assert_eq!(e.code, "ambiguous", "{e:?}");
    }
    hub.cancel_job(a, None).unwrap();
    hub.cancel_job(b, None).unwrap();
    hub.request_shutdown();
}

/// A thread's agent starts a job only while the thread's mode may run commands: not in `plan`,
/// `dontAsk` or `read-only`, whatever the spelling (`relay_forbidden`, naming the mode). The
/// user's jobs take any mode. Each job records who asked: `agent:<thread id>`, `user:<client>`
/// (`attach:<thread id>`: [`an_attached_clis_job_wakes_no_stopped_thread`]). A process that is
/// not the thread's runtime starts none.
#[tokio::test]
async fn an_agents_job_needs_a_mode_that_runs_commands() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path(), 2)).await.unwrap();
    let t = thread(&hub, &dir.path().join("proj"), "api").await;
    let user = twr_hub::Caller::user("test");
    let (mut runtime, by) = recorded_runtime(&hub, &t);
    let agent_job = |by: AgentJobBy| {
        let req: JobStart =
            serde_json::from_value(json!({"argv": ["true"], "wake": false})).unwrap();
        hub.start_agent_job(&t, req, Source::User("cli".into()), by)
    };
    let AgentJobBy::Runtime(rt) = by else {
        unreachable!()
    };
    // Another process, or one with the runtime's pid started at another time.
    for not in [
        AgentJobBy::Attach(rt),
        AgentJobBy::Runtime(twr_runner::reap::ProcessId {
            pid: rt.pid,
            start_time: rt.start_time + 1,
        }),
    ] {
        let e = agent_job(not).unwrap_err();
        assert_eq!(e.code, "relay_forbidden", "{e:?}");
    }
    for (mode, shown) in [
        ("plan", "plan"),
        ("dontAsk", "dontAsk"),
        ("Read_Only", "read-only"),
    ] {
        hub.set_mode(&t, mode, &user).unwrap();
        let e = agent_job(by).unwrap_err();
        assert_eq!(e.code, "relay_forbidden", "{e:?}");
        assert_eq!(
            e.message,
            format!(
                "api is in {shown} mode: jobs need a mode that may run commands (manual or \
                 wider)"
            )
        );
    }
    // The user's job, in plan mode still.
    start(&hub, &t, &["true"], json!({"wake": false}));
    for mode in ["manual", "acceptEdits", "auto", "bypassPermissions"] {
        hub.set_mode(&t, mode, &user).unwrap();
        agent_job(by).unwrap();
    }
    let by = |evs: &[Event]| -> Vec<String> {
        let mut by: Vec<String> = Vec::new();
        let mut seen = Vec::new();
        for e in evs {
            if let EventBody::TaskUpdated(p) = &e.body {
                let job = p.ext.as_ref().and_then(|x| x.get("twr")?.get("job"));
                if let Some(job) = job.filter(|j| p.running && !seen.contains(&j["id"])) {
                    seen.push(job["id"].clone());
                    by.push(job["by"].as_str().unwrap().to_string());
                }
            }
        }
        by
    };
    let evs = until(&hub, &t, |evs| by(evs).len() == 5).await;
    let agent = format!("agent:{t}");
    assert_eq!(by(&evs), ["user:test", &agent, &agent, &agent, &agent]);
    let _ = runtime.kill();
    let _ = runtime.wait();
    hub.request_shutdown();
}

/// Records job `job` running on thread `t` in process group `pgid` led by a process started
/// at `start_time`, with `members` seen in it, as a hub that then stopped would have left it.
fn lose_group(hub: &Hub, t: &str, job: &str, pgid: u32, start_time: u64, members: &[(u32, u64)]) {
    let members: Vec<[u64; 2]> = members.iter().map(|(p, t)| [u64::from(*p), *t]).collect();
    hub.append(
        t,
        None,
        Source::Hub,
        EventBody::TaskUpdated(TaskUpdated {
            task_id: format!("job:{job}"),
            running: true,
            description: "job build: running".into(),
            ext: Some(
                serde_json::from_value(json!({"twr": {"job": {
                    "id": job, "wake": false, "pgid": pgid, "start_time": start_time,
                    "members": members,
                }}}))
                .unwrap(),
            ),
            ..Default::default()
        }),
        None,
    )
    .unwrap();
}

/// Waits until process `pid` is gone (or a zombie); whether it went within [`WAIT`].
async fn gone(pid: u32) -> bool {
    timeout(WAIT, async {
        while twr_runner::reap::process_alive(pid) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok()
}

/// A job records its process group before its command runs, and its members while it runs;
/// a hub that finds the job lost kills what is left of that group, also a process that
/// cleared the job's markers (here a recorded background child of a leader that exited,
/// started without `TWR_AGENT` and `TWR_THREAD`). A recorded group whose leader's pid now
/// names another process (started at another time) is left alone.
#[tokio::test]
async fn a_lost_jobs_process_group_is_killed_whatever_its_markers() {
    let dir = tempfile::tempdir().unwrap();
    // What a running job records: its group, before its command ran.
    let hub = Hub::open(opts(dir.path(), 2)).await.unwrap();
    let t = thread(&hub, &dir.path().join("proj"), "api").await;
    let pidfile = dir.path().join("pid");
    let childfile = dir.path().join("child");
    let script = format!(
        "sleep 30 & echo $! > {1}.tmp; mv {1}.tmp {1}; echo $$ > {0}.tmp; mv {0}.tmp {0}; \
         exec sleep 30",
        pidfile.display(),
        childfile.display()
    );
    let r = start(&hub, &t, &["sh", "-c", &script], json!({"wake": false}));
    let job = r["job"].as_str().unwrap().to_string();
    let pid: u32 = timeout(WAIT, async {
        loop {
            if let Ok(p) = std::fs::read_to_string(&pidfile) {
                break p.trim().parse().unwrap();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let group = hub.thread_view(&t).unwrap().background_tasks[&format!("job:{job}")]
        .group
        .clone();
    let start_time = twr_runner::reap::process_start_time(pid).unwrap();
    assert_eq!(
        group.map(|g| (g.pgid, g.start_time)),
        Some((pid, start_time))
    );
    // Its other members, as the hub sees them while it runs.
    let child: u32 = std::fs::read_to_string(&childfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let child_id = (child, twr_runner::reap::process_start_time(child).unwrap());
    timeout(WAIT, async {
        loop {
            let view = hub.thread_view(&t).unwrap();
            let task = &view.background_tasks[&format!("job:{job}")];
            if task
                .group
                .as_ref()
                .is_some_and(|g| g.members.contains(&child_id))
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .expect("its member recorded");
    hub.cancel_job(&job, None).unwrap();
    until(&hub, &t, |evs| job_end(evs, &job).is_some()).await;
    hub.request_shutdown();
    drop(hub);

    // What a stopped hub left: a group whose leader exited, its member without markers.
    let left = dir.path().join("left");
    let mut leader = std::process::Command::new("sh")
        .args([
            "-c",
            &format!(
                "env -u TWR_AGENT -u TWR_THREAD sleep 300 & echo $! > {0}.tmp; mv {0}.tmp {0}",
                left.display()
            ),
        ])
        .process_group(0)
        .spawn()
        .unwrap();
    let leader_pid = leader.id();
    let leader_start = twr_runner::reap::process_start_time(leader_pid).unwrap();
    assert!(leader.wait().unwrap().success());
    let member: u32 = std::fs::read_to_string(&left)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let member_id = (
        member,
        twr_runner::reap::process_start_time(member).unwrap(),
    );
    // Another group, whose recorded leader started at another time than its pid's process.
    let mut other = std::process::Command::new("sleep")
        .arg("300")
        .process_group(0)
        .spawn()
        .unwrap();
    let other_start = twr_runner::reap::process_start_time(other.id()).unwrap();
    {
        let hub = Hub::open(opts(dir.path(), 2)).await.unwrap();
        lose_group(&hub, &t, "J0GROUP", leader_pid, leader_start, &[member_id]);
        lose_group(&hub, &t, "J1OTHER", other.id(), other_start + 1, &[]);
        hub.request_shutdown();
    }
    let hub = Hub::open(opts(dir.path(), 2)).await.unwrap();
    assert!(
        gone(member).await,
        "the lost job's process was left running"
    );
    let evs = until(&hub, &t, |evs| job_end(evs, "J0GROUP").is_some()).await;
    assert_eq!(job_end(&evs, "J0GROUP").unwrap()["outcome"], "lost");
    assert!(
        other.try_wait().unwrap().is_none(),
        "another group was killed"
    );
    let _ = other.kill();
    let _ = other.wait();
    hub.request_shutdown();
}

/// A lost job's end and the prompt that tells its thread are recorded together: when the
/// prompt cannot be queued (here a trigger that refuses it), neither is, and the next start
/// records both.
#[tokio::test]
async fn a_lost_jobs_end_and_its_wake_are_recorded_together() {
    let dir = tempfile::tempdir().unwrap();
    let t = {
        let hub = Hub::open(opts(dir.path(), 2)).await.unwrap();
        let t = thread(&hub, &dir.path().join("proj"), "api").await;
        lose(&hub, &t, "J0LOST", true);
        hub.request_shutdown();
        t
    };
    let db = rusqlite::Connection::open(dir.path().join("data/twr.db")).unwrap();
    db.execute_batch(
        "CREATE TRIGGER inject BEFORE INSERT ON pending_prompts
         BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    )
    .unwrap();
    {
        let hub = Hub::open(opts(dir.path(), 2)).await.unwrap();
        let (_, evs) = hub.thread_events(&t, 0, 10_000).unwrap();
        assert!(
            job_end(&evs, "J0LOST").is_none(),
            "recorded without its wake"
        );
        hub.request_shutdown();
    }
    db.execute_batch("DROP TRIGGER inject").unwrap();
    let hub = Hub::open(opts(dir.path(), 2)).await.unwrap();
    let evs = until(&hub, &t, |evs| {
        job_end(evs, "J0LOST").is_some() && woke_with(evs, "[twr job J0LOST]")
    })
    .await;
    assert_eq!(job_end(&evs, "J0LOST").unwrap()["outcome"], "lost");
    hub.request_shutdown();
}

/// An agent's job that waits for its slots while its thread's mode narrows does not start:
/// the mode is checked again when it would, and the job ends `failed_to_start` saying why.
/// The user's job in the same mode runs.
#[tokio::test]
async fn a_queued_agents_job_does_not_start_once_the_mode_narrowed() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path(), 1)).await.unwrap();
    let t = thread(&hub, &dir.path().join("proj"), "api").await;
    let user = twr_hub::Caller::user("test");
    hub.set_mode(&t, "auto", &user).unwrap();
    let flag = dir.path().join("go");
    let wait = format!("while [ ! -e {} ]; do sleep 0.05; done", flag.display());
    let first = start(&hub, &t, &["sh", "-c", &wait], json!({"wake": false}));
    let (mut runtime, by) = recorded_runtime(&hub, &t);
    let req: JobStart =
        serde_json::from_value(json!({"argv": ["touch", "ran"], "wake": false})).unwrap();
    let queued = hub
        .start_agent_job(&t, req, Source::User("cli".into()), by)
        .unwrap();
    assert_eq!(queued["position"], 1, "{queued}");
    let users = start(&hub, &t, &["true"], json!({"wake": false}));
    hub.set_mode(&t, "plan", &user).unwrap();
    std::fs::write(&flag, "").unwrap();
    let (a, b, c) = (
        first["job"].as_str().unwrap(),
        queued["job"].as_str().unwrap(),
        users["job"].as_str().unwrap(),
    );
    let evs = until(&hub, &t, |evs| {
        [a, b, c].iter().all(|j| job_end(evs, j).is_some())
    })
    .await;
    let end = job_end(&evs, b).unwrap();
    assert_eq!(end["outcome"], "failed_to_start", "{end}");
    let refused = evs
        .iter()
        .find_map(|e| match &e.body {
            EventBody::TaskUpdated(p) if !p.running && p.task_id == format!("job:{b}") => {
                Some(p.description.clone())
            }
            _ => None,
        })
        .unwrap();
    assert!(
        refused.contains("api is in plan mode: jobs need a mode that may run commands"),
        "{refused}"
    );
    assert!(!dir.path().join("proj/ran").exists(), "it ran");
    assert_eq!(job_end(&evs, c).unwrap()["outcome"], "exited");
    let _ = runtime.kill();
    let _ = runtime.wait();
    hub.request_shutdown();
}

/// The CLI attached to a stopped thread starts a job (recorded `attach:`), whose end does not
/// wake the thread: the prompt would start its runtime again, with what the job printed. Once
/// the attach is over, its CLI's process starts none.
#[tokio::test]
async fn an_attached_clis_job_wakes_no_stopped_thread() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path(), 2)).await.unwrap();
    let t = thread(&hub, &dir.path().join("proj"), "api").await;
    let user = twr_hub::Caller::user("test");
    hub.set_mode(&t, "auto", &user).unwrap();
    hub.send_by(&t, "hello", &user).unwrap();
    until(&hub, &t, |evs| {
        evs.iter()
            .any(|e| matches!(e.body, EventBody::TurnEnded(_)))
    })
    .await;
    hub.stop_thread_by(&t, &user).unwrap();
    until(&hub, &t, |evs| {
        evs.iter()
            .any(|e| matches!(e.body, EventBody::ThreadStopped(_)))
    })
    .await;
    let grant = hub.attach_begin(&t, false, "tty").await.unwrap();
    let mut cli = std::process::Command::new("sleep")
        .arg("60")
        .env("TWR_ATTACH", grant.env["TWR_ATTACH"].as_str().unwrap())
        .spawn()
        .unwrap();
    let cli_id = twr_runner::reap::ProcessId {
        pid: cli.id(),
        start_time: twr_runner::reap::process_start_time(cli.id()).unwrap(),
    };
    hub.attach_child(&t, grant.lease, cli_id.pid, cli_id.start_time)
        .await
        .unwrap();
    let req: JobStart = serde_json::from_value(json!({"argv": ["echo", "built"]})).unwrap();
    let r = hub
        .start_agent_job(
            &t,
            req,
            Source::User("cli".into()),
            AgentJobBy::Attach(cli_id),
        )
        .unwrap();
    let job = r["job"].as_str().unwrap().to_string();
    let evs = until(&hub, &t, |evs| job_end(evs, &job).is_some()).await;
    assert_eq!(job_end(&evs, &job).unwrap()["outcome"], "exited");
    let by = evs.iter().find_map(|e| match &e.body {
        EventBody::TaskUpdated(p) if p.running => {
            p.ext.as_ref()?.get("twr")?.get("job")?.get("by").cloned()
        }
        _ => None,
    });
    assert_eq!(by, Some(json!(format!("attach:{t}"))));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let view = hub.get_thread(&t).unwrap();
    assert_eq!(view["pending"], 0, "a stopped thread is not woken: {view}");
    let _ = cli.kill();
    let _ = cli.wait();
    hub.attach_end(&t, "tty", grant.lease, None).unwrap();
    let req: JobStart = serde_json::from_value(json!({"argv": ["true"]})).unwrap();
    let e = hub
        .start_agent_job(
            &t,
            req,
            Source::User("cli".into()),
            AgentJobBy::Attach(cli_id),
        )
        .unwrap_err();
    assert_eq!(e.code, "relay_forbidden", "{e:?}");
    assert!(hub.thread_view(&t).unwrap().stopped);
    hub.request_shutdown();
}

/// A job whose process's start time cannot be read does not run: a later hub could not tell
/// what is left of it. It ends `failed_to_start` saying why, its command never run.
#[tokio::test]
async fn a_job_whose_start_time_cannot_be_read_does_not_run() {
    let dir = tempfile::tempdir().unwrap();
    let mut o = opts(dir.path(), 2);
    o.start_time_faults = 1000;
    let hub = Hub::open(o).await.unwrap();
    let t = thread(&hub, &dir.path().join("proj"), "api").await;
    let r = start(&hub, &t, &["touch", "ran"], json!({"wake": false}));
    let job = r["job"].as_str().unwrap().to_string();
    let evs = until(&hub, &t, |evs| job_end(evs, &job).is_some()).await;
    assert_eq!(job_end(&evs, &job).unwrap()["outcome"], "failed_to_start");
    let why = end_description(&evs, &job);
    assert!(why.contains("start time of its process"), "{why}");
    assert!(!dir.path().join("proj/ran").exists(), "it ran");
    hub.request_shutdown();
}

/// The description of job `job`'s end.
fn end_description(evs: &[Event], job: &str) -> String {
    evs.iter()
        .find_map(|e| match &e.body {
            EventBody::TaskUpdated(p) if !p.running && p.task_id == format!("job:{job}") => {
                Some(p.description.clone())
            }
            _ => None,
        })
        .unwrap()
}

/// A gate hook that holds each job's child at its start gate: the child's pid comes out of the
/// returned receiver, and the child goes on once the returned sender is sent a `()`.
fn holding_gate(
    o: &mut HubOptions,
) -> (std::sync::mpsc::Receiver<u32>, std::sync::mpsc::Sender<()>) {
    let (at_tx, at_rx) = std::sync::mpsc::channel();
    let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
    let at_tx = std::sync::Mutex::new(at_tx);
    let go_rx = std::sync::Mutex::new(go_rx);
    o.job_gate_hook = Some(Arc::new(move |pid| {
        let _ = at_tx.lock().unwrap().send(pid);
        let _ = go_rx.lock().unwrap().recv_timeout(WAIT);
    }));
    (at_rx, go_tx)
}

/// A job's child held at its start gate is let go only under the lock a cancel and a mode
/// change take, with both checked then: an agent's job whose thread's mode narrowed while it
/// was held ends `failed_to_start`, and a job cancelled while it was held ends `cancelled`,
/// neither having run its command.
#[tokio::test]
async fn a_job_held_at_its_gate_checks_its_mode_and_cancel_before_it_runs() {
    let dir = tempfile::tempdir().unwrap();
    let mut o = opts(dir.path(), 2);
    let (at, go) = holding_gate(&mut o);
    let hub = Hub::open(o).await.unwrap();
    let t = thread(&hub, &dir.path().join("proj"), "api").await;
    let user = twr_hub::Caller::user("test");
    hub.set_mode(&t, "auto", &user).unwrap();
    let (mut runtime, by) = recorded_runtime(&hub, &t);
    let req: JobStart =
        serde_json::from_value(json!({"argv": ["touch", "agent-ran"], "wake": false})).unwrap();
    let r = hub
        .start_agent_job(&t, req, Source::User("cli".into()), by)
        .unwrap();
    let agents = r["job"].as_str().unwrap().to_string();
    let held = tokio::task::spawn_blocking(move || at.recv_timeout(WAIT).map(|p| (p, at)))
        .await
        .unwrap();
    let (_, at) = held.expect("the child reached its gate");
    hub.set_mode(&t, "plan", &user).unwrap();
    go.send(()).unwrap();
    let evs = until(&hub, &t, |evs| job_end(evs, &agents).is_some()).await;
    assert_eq!(
        job_end(&evs, &agents).unwrap()["outcome"],
        "failed_to_start"
    );
    let why = end_description(&evs, &agents);
    assert!(why.contains("api is in plan mode"), "{why}");
    let r = start(&hub, &t, &["touch", "user-ran"], json!({"wake": false}));
    let users = r["job"].as_str().unwrap().to_string();
    tokio::task::spawn_blocking(move || at.recv_timeout(WAIT))
        .await
        .unwrap()
        .expect("the child reached its gate");
    hub.cancel_job(&users, None).unwrap();
    go.send(()).unwrap();
    let evs = until(&hub, &t, |evs| job_end(evs, &users).is_some()).await;
    assert_eq!(job_end(&evs, &users).unwrap()["outcome"], "cancelled");
    for f in ["agent-ran", "user-ran"] {
        assert!(!dir.path().join("proj").join(f).exists(), "{f}");
    }
    let _ = runtime.kill();
    let _ = runtime.wait();
    hub.request_shutdown();
}

/// A lost job whose process group cannot be verified gone (here a cleanup that reports it
/// unknown) is not recorded lost, and its thread starts no job and no runtime; a later start
/// that verifies it records it lost and lets the thread go on.
#[tokio::test]
async fn a_lost_job_whose_leftovers_cannot_be_verified_gone_holds_its_thread() {
    let dir = tempfile::tempdir().unwrap();
    // A group that is gone: its leader exited, nothing else in it.
    let mut gone = std::process::Command::new("true")
        .process_group(0)
        .spawn()
        .unwrap();
    let (pgid, start_time) = (
        gone.id(),
        twr_runner::reap::process_start_time(gone.id()).unwrap_or(1),
    );
    gone.wait().unwrap();
    let t = {
        let hub = Hub::open(opts(dir.path(), 2)).await.unwrap();
        let t = thread(&hub, &dir.path().join("proj"), "api").await;
        lose_group(&hub, &t, "J0HELD", pgid, start_time, &[]);
        hub.request_shutdown();
        t
    };
    {
        let mut o = opts(dir.path(), 2);
        o.reap_job_group_hook = Some(Arc::new(|_, _| twr_runner::reap::Reaped::Unknown));
        let hub = Hub::open(o).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let (_, evs) = hub.thread_events(&t, 0, 10_000).unwrap();
        assert!(job_end(&evs, "J0HELD").is_none(), "recorded lost");
        let req: JobStart = serde_json::from_value(json!({"argv": ["true"]})).unwrap();
        let e = hub
            .start_job(&t, req, Source::User("test".into()))
            .unwrap_err();
        assert_eq!(e.code, "conflict", "{e:?}");
        assert!(e.message.contains("may still run"), "{e:?}");
        // A prompt's turn fails: its runtime does not start.
        hub.send_by(&t, "hello", &twr_hub::Caller::user("test"))
            .unwrap();
        let evs = until(&hub, &t, |evs| {
            evs.iter()
                .any(|e| matches!(e.body, EventBody::TurnEnded(_)))
        })
        .await;
        let ended = evs
            .iter()
            .find(|e| matches!(e.body, EventBody::TurnEnded(_)))
            .unwrap();
        let ended = serde_json::to_string(ended).unwrap();
        assert!(ended.contains("may still run"), "{ended}");
        assert!(!evs
            .iter()
            .any(|e| matches!(e.body, EventBody::ThreadStarted(_))));
        hub.request_shutdown();
    }
    let hub = Hub::open(opts(dir.path(), 2)).await.unwrap();
    let evs = until(&hub, &t, |evs| job_end(evs, "J0HELD").is_some()).await;
    assert_eq!(job_end(&evs, "J0HELD").unwrap()["outcome"], "lost");
    let r = start(&hub, &t, &["true"], json!({"wake": false}));
    assert!(r["job"].is_string());
    hub.request_shutdown();
}

/// Not a test of its own: a hub process for [`a_hub_that_dies_with_jobs_at_their_gates`]
/// (this binary with `TWR_TEST_GATE_HUB=<dir>`): opens a hub on `<dir>`, starts three jobs at
/// once (each would create `<dir>/proj/ran-<n>`) whose gate hook writes each held child's pid
/// to `<dir>/gated-<pid>` and holds it there, and waits to be killed.
#[test]
fn gate_hub() {
    let Some(dir) = std::env::var_os("TWR_TEST_GATE_HUB") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let mut o = opts(&dir, 3);
        let d = dir.clone();
        o.job_gate_hook = Some(Arc::new(move |pid| {
            std::fs::write(d.join(format!("gated-{pid}")), "").unwrap();
            std::thread::sleep(Duration::from_secs(600));
        }));
        let hub = Hub::open(o).await.unwrap();
        let t = thread(&hub, &dir.join("proj"), "api").await;
        for n in 0..3 {
            start(
                &hub,
                &t,
                &["touch", &format!("ran-{n}")],
                json!({"wake": false}),
            );
        }
        std::fs::write(dir.join("started"), &t).unwrap();
        tokio::time::sleep(Duration::from_secs(600)).await;
    });
}

/// A hub that dies while jobs wait at their gates: each held child sees its gate close and
/// exits without running its command, holding nothing of the hub's open (its lock file
/// included), so a new hub starts on the same data at once and records the jobs lost.
#[tokio::test]
async fn a_hub_that_dies_with_jobs_at_their_gates() {
    let dir = tempfile::tempdir().unwrap();
    let mut hub = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "gate_hub", "--test-threads=1"])
        .env("TWR_TEST_GATE_HUB", dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let gated = |d: &Path| -> Vec<u32> {
        std::fs::read_dir(d)
            .unwrap()
            .filter_map(|e| {
                let name = e.ok()?.file_name().into_string().ok()?;
                name.strip_prefix("gated-")?.parse().ok()
            })
            .collect()
    };
    let reached = timeout(WAIT, async {
        while gated(dir.path()).len() < 3 || !dir.path().join("started").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    let held = gated(dir.path());
    hub.kill().unwrap();
    hub.wait().unwrap();
    assert!(reached.is_ok(), "all three jobs at their gates: {held:?}");
    for pid in &held {
        assert!(gone(*pid).await, "the held child {pid} kept running");
    }
    let t = std::fs::read_to_string(dir.path().join("started")).unwrap();
    let hub = timeout(WAIT, Hub::open(opts(dir.path(), 3)))
        .await
        .expect("the new hub started")
        .unwrap();
    let evs = until(&hub, &t, |evs| {
        evs.iter()
            .filter(|e| matches!(&e.body, EventBody::TaskUpdated(p) if !p.running))
            .count()
            == 3
    })
    .await;
    for e in &evs {
        if let EventBody::TaskUpdated(p) = &e.body {
            if !p.running {
                let job = p.ext.as_ref().unwrap()["twr"]["job"].clone();
                assert_eq!(job["outcome"], "lost", "{job}");
            }
        }
    }
    for n in 0..3 {
        assert!(
            !dir.path().join(format!("proj/ran-{n}")).exists(),
            "ran-{n}"
        );
    }
    hub.request_shutdown();
}

/// A job's child held at its start gate keeps none of the hub's descriptors but its own
/// standard ones: one the hub (here this test) holds close-on-exec, made before the job
/// started, reads end of file once the hub closes its end. (A child held before `exec` kept
/// them all, and could keep a spawn the hub's lock was held across from ever finishing.) A
/// `pty` runtime starts while the job is held, and the job runs once let go.
#[tokio::test]
async fn a_job_held_at_its_gate_holds_none_of_the_hubs_descriptors() {
    let dir = tempfile::tempdir().unwrap();
    let mut o = opts(dir.path(), 2);
    o.adapters = Arc::new(|name: &str| -> Option<Arc<dyn Adapter>> {
        match name {
            "fake" => Some(Arc::new(FakeAdapter::new())),
            "pty" => Some(Arc::new(twr_adapters::pty::PtyAdapter::new())),
            _ => None,
        }
    });
    let (at, go) = holding_gate(&mut o);
    let hub = Hub::open(o).await.unwrap();
    let t = thread(&hub, &dir.path().join("proj"), "api").await;
    let script = dir.path().join("runtime.sh");
    std::fs::write(
        &script,
        "printf '> '\nwhile read l; do echo \"got [$l]\"; printf '> '; done\n",
    )
    .unwrap();
    let w = hub
        .new_thread(
            NewThread {
                project: dir.path().join("proj").to_string_lossy().into_owned(),
                runtime: Some(format!("pty:sh {}", script.display())),
                name: Some("w".into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (probe_r, probe_w) = std::io::pipe().unwrap();
    let r = start(&hub, &t, &["touch", "ran"], json!({"wake": false}));
    let job = r["job"].as_str().unwrap().to_string();
    let (_, at) = tokio::task::spawn_blocking(move || at.recv_timeout(WAIT).map(|p| (p, at)))
        .await
        .unwrap()
        .expect("the child reached its gate");
    drop(probe_w);
    let eof = tokio::task::spawn_blocking(move || {
        let mut b = [0u8; 1];
        std::io::Read::read(&mut &probe_r, &mut b)
    });
    let eof = timeout(Duration::from_secs(5), eof).await;
    assert!(
        matches!(eof, Ok(Ok(Ok(0)))),
        "a held job's child keeps the hub's descriptors: {eof:?}"
    );
    // A runtime starts meanwhile.
    hub.send_by(&w, "hello", &twr_hub::Caller::user("test"))
        .unwrap();
    until(&hub, &w, |evs| {
        evs.iter()
            .any(|e| matches!(e.body, EventBody::ThreadStarted(_)))
    })
    .await;
    go.send(()).unwrap();
    let evs = until(&hub, &t, |evs| job_end(evs, &job).is_some()).await;
    assert_eq!(job_end(&evs, &job).unwrap()["outcome"], "exited");
    assert_eq!(job_end(&evs, &job).unwrap()["exit"], 0);
    assert!(dir.path().join("proj/ran").exists());
    drop(at);
    hub.request_shutdown();
}

/// Through its gate, a job's command still gets its thread's environment (`[threads] env`
/// here) and `/dev/null` for standard input, not the gate's pipe. While it waits at its gate,
/// that environment is its process's own (which only its user may read), never on its command
/// line (which every user may read).
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_job_runs_with_its_environment_and_no_input_after_its_gate() {
    let dir = tempfile::tempdir().unwrap();
    let mut o = opts(dir.path(), 2);
    o.config
        .threads
        .env
        .insert("JOB_TEST_SECRET".into(), "s3cret-value-0451".into());
    let (at, go) = holding_gate(&mut o);
    let hub = Hub::open(o).await.unwrap();
    let t = thread(&hub, &dir.path().join("proj"), "api").await;
    let r = start(
        &hub,
        &t,
        &[
            "sh",
            "-c",
            "readlink /proc/self/fd/0; echo \"var: $JOB_TEST_SECRET\"",
        ],
        json!({"wake": false}),
    );
    let job = r["job"].as_str().unwrap().to_string();
    let pid = tokio::task::spawn_blocking(move || at.recv_timeout(WAIT))
        .await
        .unwrap()
        .expect("the child reached its gate");
    let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap();
    let environ = std::fs::read(format!("/proc/{pid}/environ")).unwrap();
    go.send(()).unwrap();
    let has = |b: &[u8]| b.windows(17).any(|w| w == b"s3cret-value-0451");
    assert!(!has(&cmdline), "{}", String::from_utf8_lossy(&cmdline));
    assert!(has(&environ));
    until(&hub, &t, |evs| job_end(evs, &job).is_some()).await;
    let log = hub.job_log(&job, None).unwrap();
    let text = log["text"].as_str().unwrap();
    assert_eq!(text, "/dev/null\nvar: s3cret-value-0451", "{log}");
    hub.request_shutdown();
}

/// The job's ended outcome, exit code and log text, once it ended.
async fn ran(hub: &Hub, t: &str, argv: &[&str]) -> (Value, String, String) {
    let r = start(hub, t, argv, json!({"wake": false}));
    let job = r["job"].as_str().unwrap().to_string();
    let evs = until(hub, t, |evs| job_end(evs, &job).is_some()).await;
    let end = job_end(&evs, &job).unwrap().clone();
    let log = hub.job_log(&job, None).unwrap()["text"]
        .as_str()
        .unwrap()
        .to_string();
    (end, end_description(&evs, &job), log)
}

/// Creates the executable `path` (mode 755, its directories made) holding `content`, without
/// this process ever holding it open for writing: the bytes go to a side file, `cp` writes
/// `path`, and the side file is removed. A child another test thread forks while this process
/// writes the file would inherit the descriptor, and exec'ing the file would fail with
/// ETXTBSY until that child execs or exits.
fn install(path: &Path, content: impl AsRef<[u8]>) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut src = path.as_os_str().to_owned();
    src.push(".src");
    std::fs::write(&src, content).unwrap();
    let st = std::process::Command::new("cp")
        .arg(&src)
        .arg(path)
        .status()
        .unwrap();
    assert!(st.success(), "cp failed");
    std::fs::remove_file(&src).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// Writes an executable script `body` (with `#!/bin/sh`) at `p`, its directories made.
fn script(p: &Path, body: &str) {
    install(p, format!("#!/bin/sh\n{body}\n"));
}

/// Through its gate a job's program is found as `execvp` finds it and run exactly as named:
/// a program on no `PATH` directory, or a file that cannot be run (not executable, no `#!`),
/// fails to start saying why; one on a relative `PATH` entry (`node_modules/.bin`) is found
/// from the job's directory; a first word starting with `-` is that file, never an option; a
/// command that itself exits 125, 126 or 127 exited with that code.
#[tokio::test]
async fn a_jobs_program_is_found_and_run_as_named() {
    let dir = tempfile::tempdir().unwrap();
    let mut o = opts(dir.path(), 2);
    o.config
        .threads
        .env
        .insert("PATH".into(), "node_modules/.bin:/usr/bin:/bin".into());
    let hub = Hub::open(o).await.unwrap();
    let proj = dir.path().join("proj");
    let t = thread(&hub, &proj, "api").await;
    script(&proj.join("node_modules/.bin/jesty"), "echo jesty ran");
    script(&proj.join("-a/x"), "echo \"x ran $*\"");
    std::fs::write(proj.join("plain"), "echo plain").unwrap();
    install(&proj.join("headless"), [0u8, 1, 2, 3]);
    let (end, why, _) = ran(&hub, &t, &["no-such-program-twr"]).await;
    assert_eq!(end["outcome"], "failed_to_start", "{end}");
    assert!(
        why.contains("`no-such-program-twr` not found on PATH"),
        "{why}"
    );
    let (end, _, log) = ran(&hub, &t, &["jesty"]).await;
    assert_eq!(
        (end["outcome"].clone(), log.as_str()),
        (json!("exited"), "jesty ran")
    );
    let (end, _, log) = ran(&hub, &t, &["-a/x", "/bin/echo", "other"]).await;
    assert_eq!(end["exit"], 0, "{end}");
    assert_eq!(log, "x ran /bin/echo other");
    for (file, error) in [
        ("./plain", "ermission denied"),
        ("./headless", "format error"),
    ] {
        let (end, why, _) = ran(&hub, &t, &[file]).await;
        assert_eq!(end["outcome"], "failed_to_start", "{file}: {end}");
        assert!(why.contains(error), "{file}: {why}");
    }
    for code in [125, 126, 127] {
        let (end, _, _) = ran(&hub, &t, &["sh", "-c", &format!("exit {code}")]).await;
        assert_eq!(
            (end["outcome"].clone(), end["exit"].clone()),
            (json!("exited"), json!(code))
        );
    }
    hub.request_shutdown();
}

/// A runtime crash that failed a turn holds the thread's queue until the user acts: a job that
/// outlived the runtime ends and queues its wake, which waits with the rest, also across a
/// hub restart (the wake is queued after the crash, but is not the user's send); the user's
/// next prompt releases it.
#[tokio::test]
async fn a_jobs_wake_does_not_release_a_queue_a_crash_holds() {
    let dir = tempfile::tempdir().unwrap();
    let crashing = || {
        let mut o = opts(dir.path(), 2);
        let adapter: Arc<dyn Adapter> = Arc::new(FakeAdapter::scripted(vec![
            Step::Emit(EventBody::ThreadStarted(SessionPayload {
                session_id: Some("crash-session".into()),
                ..Default::default()
            })),
            Step::Sleep(300),
            Step::Exit {
                code: Some(3),
                stderr_tail: "segfault".into(),
            },
        ]));
        o.adapters = Arc::new(move |name: &str| (name == "fake").then(|| adapter.clone()));
        o
    };
    let turns = |hub: &Hub, t: &str| {
        let (_, evs) = hub.thread_events(t, 0, 10_000).unwrap();
        evs.iter().filter(|e| e.kind() == "turn.started").count()
    };
    let user = || Source::User("test".into());
    let hub = Hub::open(crashing()).await.unwrap();
    let t = thread(&hub, &dir.path().join("proj"), "api").await;
    let job = start(&hub, &t, &["sleep", "1"], json!({}))["job"]
        .as_str()
        .unwrap()
        .to_string();
    hub.send(&t, "go", TurnFrom::User, user()).unwrap();
    hub.send(&t, "queued", TurnFrom::User, user()).unwrap();
    until(&hub, &t, |evs| {
        evs.iter().any(|e| e.kind() == "thread.crashed")
    })
    .await;
    until(&hub, &t, |evs| job_end(evs, &job).is_some()).await;
    // Past a tick of the background ticker: the wake is queued behind the user's prompt, and
    // neither runs.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(turns(&hub, &t), 1);
    assert_eq!(hub.get_thread(&t).unwrap()["pending"], 2);
    hub.shutdown().await;
    drop(hub);
    // The next hub keeps the hold: no prompt the user sent after the crash is queued.
    // (Retried briefly: a process another test spawns at this very moment holds a copy of
    // every open file, the lock's too, until its exec closes them.)
    let hub = timeout(WAIT, async {
        loop {
            match Hub::open(crashing()).await {
                Ok(h) => return h,
                Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
            }
        }
    })
    .await
    .expect("the hub reopens");
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(turns(&hub, &t), 1);
    assert_eq!(hub.get_thread(&t).unwrap()["pending"], 2);
    // The user's prompt releases it: the oldest queued prompt goes first.
    hub.send(&t, "now", TurnFrom::User, user()).unwrap();
    let evs = until(&hub, &t, |evs| {
        evs.iter().filter(|e| e.kind() == "turn.started").count() == 2
    })
    .await;
    let second = evs
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::TurnStarted(p) => Some(p.prompt.as_str()),
            _ => None,
        })
        .nth(1);
    assert_eq!(second, Some("queued"));
    hub.shutdown().await;
}

/// Reading a running job's process group (a `ps` call per snapshot on macOS, which may take
/// seconds) is off the job's loop: a cancel while it reads ends the job at once.
#[tokio::test]
async fn a_slow_member_snapshot_does_not_hold_up_a_cancel() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let dir = tempfile::tempdir().unwrap();
    let reading = Arc::new(AtomicBool::new(false));
    let released = Arc::new(AtomicBool::new(false));
    let mut o = opts(dir.path(), 2);
    let (r, l) = (reading.clone(), released.clone());
    o.job_members_hook = Some(Arc::new(move |_| {
        if !r.swap(true, Ordering::SeqCst) {
            let until = std::time::Instant::now() + Duration::from_secs(30);
            while !l.load(Ordering::SeqCst) && std::time::Instant::now() < until {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }));
    let hub = Hub::open(o).await.unwrap();
    let t = thread(&hub, &dir.path().join("proj"), "api").await;
    let job = start(&hub, &t, &["sleep", "60"], json!({}))["job"]
        .as_str()
        .unwrap()
        .to_string();
    timeout(WAIT, async {
        while !reading.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("no snapshot was taken");
    hub.cancel_job(&job, None).unwrap();
    let ended = timeout(Duration::from_secs(5), async {
        loop {
            let (_, evs) = hub.thread_events(&t, 0, 10_000).unwrap();
            if job_end(&evs, &job).is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    released.store(true, Ordering::SeqCst);
    ended.expect("the cancel waited for the snapshot");
    hub.request_shutdown();
}

/// The controlling terminal (`tty_nr`, 0 for none) and state of process `pid`, from
/// `/proc/<pid>/stat`.
#[cfg(target_os = "linux")]
fn tty_and_state(pid: u32) -> Option<(i64, String)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // After the command name, which may hold spaces: state, ppid, pgrp, session, tty_nr.
    let rest: Vec<&str> = stat[stat.rfind(')')? + 1..].split_whitespace().collect();
    Some((rest.get(4)?.parse().ok()?, rest.first()?.to_string()))
}

/// Not a test of its own: a hub process for [`a_job_that_reads_the_terminal_ends_with_an_error`]
/// (this binary with `TWR_TEST_TTY_HUB=<dir>`, run with a terminal as its controlling one):
/// writes its own `tty_nr` to `<dir>/tty`, opens a hub on `<dir>` and runs a job that reads
/// the terminal (`cat /dev/tty`). Writes `<dir>/ended`: the job's end (JSON) once it ended, or
/// `stuck <state>` (the state of its process) when it did not end within 15 s, in which case
/// it kills the job's group.
#[cfg(target_os = "linux")]
#[test]
fn tty_hub() {
    let Some(dir) = std::env::var_os("TWR_TEST_TTY_HUB") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let (tty, _) = tty_and_state(std::process::id()).unwrap();
    std::fs::write(dir.join("tty"), tty.to_string()).unwrap();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let hub = Hub::open(opts(&dir, 2)).await.unwrap();
        let t = thread(&hub, &dir.join("proj"), "api").await;
        let r = start(&hub, &t, &["cat", "/dev/tty"], json!({"wake": false}));
        let job = r["job"].as_str().unwrap().to_string();
        let ended = timeout(Duration::from_secs(15), async {
            loop {
                let (_, evs) = hub.thread_events(&t, 0, 10_000).unwrap();
                if let Some(end) = job_end(&evs, &job) {
                    return end.clone();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        let said = match ended {
            Ok(end) => end.to_string(),
            Err(_) => {
                let (_, evs) = hub.thread_events(&t, 0, 10_000).unwrap();
                let pgid = evs.iter().find_map(|e| match &e.body {
                    EventBody::TaskUpdated(p) if p.running => p
                        .ext
                        .as_ref()?
                        .get("twr")?
                        .get("job")?
                        .get("pgid")?
                        .as_u64(),
                    _ => None,
                });
                let state = pgid.and_then(|p| tty_and_state(p as u32)).map(|(_, s)| s);
                if let Some(p) = pgid {
                    // SAFETY: kill(2) of the job's process group; no memory is passed.
                    unsafe { libc::kill(-(p as libc::pid_t), libc::SIGKILL) };
                }
                format!("stuck {state:?}")
            }
        };
        // Whole or not at all: the parent reads it as soon as it exists.
        std::fs::write(dir.join("ended.tmp"), said).unwrap();
        std::fs::rename(dir.join("ended.tmp"), dir.join("ended")).unwrap();
        hub.shutdown().await;
    });
}

/// A job whose command reads the terminal, run by a hub that has one (a pseudo-terminal, its
/// controlling terminal, as when `twr hub` runs in your terminal): the job runs in a session
/// of its own, so opening `/dev/tty` fails and the job ends with that error. (In the hub's
/// session, it was a background process group of that terminal: job control stopped it with
/// SIGTTIN, forever.)
#[cfg(target_os = "linux")]
#[tokio::test]
async fn a_job_that_reads_the_terminal_ends_with_an_error() {
    use std::os::fd::{FromRawFd, OwnedFd};
    let dir = tempfile::tempdir().unwrap();
    let (mut master, mut slave) = (-1, -1);
    // SAFETY: openpty(3) writes the two descriptors; no name, termios or size is passed.
    let r = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null(),
        )
    };
    assert_eq!(r, 0, "openpty: {}", std::io::Error::last_os_error());
    // SAFETY: both are open descriptors of this process's, owned from here on.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    let out = std::fs::File::create(dir.path().join("hub.out")).unwrap();
    let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
    cmd.args(["--exact", "tty_hub", "--test-threads=1", "--nocapture"])
        .env("TWR_TEST_TTY_HUB", dir.path())
        .stdin(slave)
        .stdout(out.try_clone().unwrap())
        .stderr(out);
    // SAFETY: setsid(2) and ioctl(2) are async-signal-safe; nothing is allocated. The new
    // session's controlling terminal becomes the pseudo-terminal on stdin.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut hub = cmd.spawn().unwrap();
    let ended = dir.path().join("ended");
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    while !ended.exists() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let said = std::fs::read_to_string(&ended);
    let _ = hub.kill();
    let _ = hub.wait();
    // Held until the hub is gone: closing it hangs the terminal up.
    drop(master);
    let log = std::fs::read_to_string(dir.path().join("hub.out")).unwrap_or_default();
    let tty = std::fs::read_to_string(dir.path().join("tty")).unwrap_or_default();
    assert!(
        tty.parse::<i64>().is_ok_and(|t| t != 0),
        "the hub had the terminal as its controlling one ({tty:?}): {log}"
    );
    let said = said.unwrap_or_else(|_| panic!("the hub never said how the job went: {log}"));
    let end: Value = serde_json::from_str(&said).unwrap_or_else(|_| panic!("{said}: {log}"));
    assert_eq!(end["outcome"], "exited", "{end}");
    assert_eq!(end["exit"], 1, "{end}");
}
