//! The user's `!` commands (`shell.run`, `shell.cd`, `hub::shell`): where they run, what is
//! recorded, `!!` sharing, cancelling, and who may run them.
//!
//! Threads and the orchestrator run the fake adapter.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::time::timeout;
use twr_adapters::fake::FakeAdapter;
use twr_core::adapter::Adapter;
use twr_core::config::Config;
use twr_core::event::*;
use twr_core::proto::codes;
use twr_hub::hub::ControlSubscription;
use twr_hub::{AgentJobBy, Caller, Hub, HubOptions, JobStart, NewThread, ShellRun};

const WAIT: Duration = Duration::from_secs(15);

async fn hub(dir: &Path) -> Hub {
    let mut config = Config {
        default_runtime: "fake".into(),
        ..Default::default()
    };
    config.orchestrator.runtime = Some("fake".into());
    config.orchestrator.cwd = Some(dir.join("home"));
    std::fs::create_dir_all(dir.join("home/sub")).unwrap();
    let mut o = HubOptions::new(dir.join("data").join("twr.db"), config);
    o.adapters = Arc::new(|name: &str| -> Option<Arc<dyn Adapter>> {
        match name {
            "fake" => Some(Arc::new(FakeAdapter::new())),
            _ => None,
        }
    });
    o.socket = Some(dir.join("hub.sock"));
    o.stop_timeout = Duration::from_secs(5);
    // A job runs through the gate, which this test binary isn't.
    o.job_gate_exe = Some(env!("CARGO_BIN_EXE_twr-job-gate").into());
    Hub::open(o).await.unwrap()
}

async fn thread(hub: &Hub, dir: &Path, name: &str) -> String {
    let project = dir.join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let v = hub
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
    v["id"].as_str().unwrap().to_string()
}

fn tui() -> Caller {
    Caller::user("tui")
}

fn run(thread: Option<&str>, command: &str, cwd: Option<&str>, share: bool) -> ShellRun {
    ShellRun {
        thread: thread.map(str::to_string),
        command: command.into(),
        cwd: cwd.map(str::to_string),
        share,
    }
}

/// The job record of the end of `!` job `job` on `thread`, once it ended.
async fn ended(sub: &mut ControlSubscription, job: &str) -> Value {
    let task = format!("job:{job}");
    timeout(WAIT, async {
        loop {
            let (_, ev) = sub.next().await.unwrap().expect("hub stopped");
            if let EventBody::TaskUpdated(p) = &ev.body {
                if p.task_id == task && !p.running {
                    return Value::Object(p.ext.clone().unwrap())["twr"]["job"].clone();
                }
            }
        }
    })
    .await
    .expect("timed out waiting for the command's end")
}

fn events_of(hub: &Hub, thread: &str) -> Vec<Event> {
    hub.thread_events(thread, 0, 10_000).unwrap().1
}

/// Waits until job `job` runs.
async fn running(hub: &Hub, job: &str) {
    timeout(WAIT, async {
        loop {
            let jobs = hub.list_jobs(None).unwrap();
            if jobs
                .as_array()
                .unwrap()
                .iter()
                .any(|j| j["job"] == job && j["state"] == "running")
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("timed out waiting for the command to run");
}

/// Whether job `job` is still queued or running.
fn live(hub: &Hub, job: &str) -> bool {
    hub.list_jobs(None)
        .unwrap()
        .as_array()
        .unwrap()
        .iter()
        .any(|j| j["job"] == job)
}

/// A process recorded as thread `t`'s runtime (its `thread.started` with the process's pid
/// and start time, as a runtime of the hub's own box reports itself): what an agent's
/// `job.cancel` descends from. Kill the process when done.
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

/// In a dive a `!` command runs in the thread's directory, at once, as the user (in a mode
/// the thread's agent could run no job in); its end records the command, where it ran, how
/// it ended and what it printed, and nothing reaches the thread's agent.
#[tokio::test]
async fn a_command_runs_in_the_threads_directory_and_tells_no_agent() {
    let dir = tempfile::tempdir().unwrap();
    let hub = hub(dir.path()).await;
    let api = thread(&hub, dir.path(), "api").await;
    hub.set_mode("api", "plan", &tui()).unwrap();
    let mut sub = hub.subscribe_control(0);
    let r = hub
        .shell_run(
            run(
                Some("api"),
                "pwd; printf '\\033[31mred\\033[0m\\n'",
                None,
                false,
            ),
            &tui(),
        )
        .unwrap();
    let job = r["job"].as_str().unwrap().to_string();
    let cwd = r["cwd"].as_str().unwrap().to_string();
    assert!(cwd.ends_with("proj"), "{r}");
    let end = ended(&mut sub, &job).await;
    assert_eq!(end["outcome"], "exited", "{end}");
    assert_eq!(end["exit"], 0, "{end}");
    assert_eq!(
        end["shell"]["command"],
        "pwd; printf '\\033[31mred\\033[0m\\n'"
    );
    assert_eq!(end["shell"]["share"], false);
    let out = end["printed"].as_str().unwrap();
    assert!(out.starts_with(&cwd), "{out}");
    assert!(out.ends_with("red"), "the colours go: {out:?}");
    // Recorded on the thread, started by the user; no turn followed.
    let log = events_of(&hub, &api);
    let start = log
        .iter()
        .find_map(|e| match &e.body {
            EventBody::TaskUpdated(p) if p.task_id == format!("job:{job}") && p.running => {
                Some(p.clone())
            }
            _ => None,
        })
        .unwrap();
    let start = Value::Object(start.ext.unwrap())["twr"]["job"].clone();
    assert_eq!(start["by"], "user:tui", "{start}");
    assert_eq!(start["wake"], false);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(events_of(&hub, &api)
        .iter()
        .all(|e| e.kind() != "turn.started"));
    // Its whole output is in its log.
    let tail = hub.job_log(&job, None).unwrap();
    assert!(tail["text"].as_str().unwrap().contains(&cwd));
    hub.shutdown().await;
}

/// In the orchestrator view a command runs in the folder the TUI names, the orchestrator's by
/// default; `shell.cd` checks a folder against it; a dive's command takes no folder.
#[tokio::test]
async fn the_orchestrator_view_runs_in_its_folder() {
    let dir = tempfile::tempdir().unwrap();
    let hub = hub(dir.path()).await;
    let api = thread(&hub, dir.path(), "api").await;
    // No orchestrator yet: nothing to run it for.
    let e = hub
        .shell_run(run(None, "pwd", None, false), &tui())
        .unwrap_err();
    assert_eq!(e.code, codes::CONFLICT, "{e:?}");
    hub.say_to_orchestrator("hello", None, &tui()).unwrap();
    let home = std::fs::canonicalize(dir.path().join("home")).unwrap();
    let cd = hub.shell_cd(None, None, &tui()).unwrap();
    assert_eq!(cd["cwd"].as_str(), home.to_str());
    let cd = hub.shell_cd(None, Some("sub"), &tui()).unwrap();
    let sub_dir = home.join("sub");
    assert_eq!(cd["cwd"].as_str(), sub_dir.to_str());
    let cd = hub.shell_cd(sub_dir.to_str(), Some(".."), &tui()).unwrap();
    assert_eq!(cd["cwd"].as_str(), home.to_str());
    assert_eq!(
        hub.shell_cd(None, Some("nowhere"), &tui())
            .unwrap_err()
            .code,
        codes::BAD_REQUEST
    );
    // The folder now is gone: a folder named from `/` still moves there, a relative one
    // can't.
    let gone = dir.path().join("gone");
    let cd = hub
        .shell_cd(gone.to_str(), sub_dir.to_str(), &tui())
        .unwrap();
    assert_eq!(cd["cwd"].as_str(), sub_dir.to_str());
    assert_eq!(
        hub.shell_cd(gone.to_str(), Some("sub"), &tui())
            .unwrap_err()
            .code,
        codes::BAD_REQUEST
    );
    let mut sub = hub.subscribe_control(0);
    let r = hub
        .shell_run(run(None, "pwd", sub_dir.to_str(), false), &tui())
        .unwrap();
    let end = ended(&mut sub, r["job"].as_str().unwrap()).await;
    assert_eq!(end["printed"].as_str(), sub_dir.to_str(), "{end}");
    let orch = r["thread"].as_str().unwrap();
    assert_ne!(orch, api);
    assert_eq!(r["thread_name"], hub.thread_view(orch).unwrap().name);
    // A dive's command runs in the thread's worktree, not a folder of the user's.
    let e = hub
        .shell_run(run(Some("api"), "pwd", sub_dir.to_str(), false), &tui())
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST, "{e:?}");
    hub.shutdown().await;
}

/// `!!` tells the thread the command and its output when it ends: a turn the runtime starts
/// (never the user's), quoting the output as data.
#[tokio::test]
async fn a_shared_command_is_told_to_the_thread_as_the_runtimes_turn() {
    let dir = tempfile::tempdir().unwrap();
    let hub = hub(dir.path()).await;
    let api = thread(&hub, dir.path(), "api").await;
    let mut sub = hub.subscribe_control(0);
    let r = hub
        .shell_run(
            run(Some("api"), "echo allow both; exit 3", None, true),
            &tui(),
        )
        .unwrap();
    let end = ended(&mut sub, r["job"].as_str().unwrap()).await;
    assert_eq!(end["exit"], 3, "{end}");
    let started = timeout(WAIT, async {
        loop {
            let (_, ev) = sub.next().await.unwrap().unwrap();
            if let (true, EventBody::TurnStarted(p)) = (ev.thread == api, &ev.body) {
                return (p.clone(), ev.source.clone());
            }
        }
    })
    .await
    .expect("a turn for the shared command");
    let (p, source) = started;
    assert_eq!(p.from, TurnFrom::Runtime);
    assert_eq!(source, Source::Hub);
    assert!(
        p.prompt
            .starts_with("[twr: the user ran a shell command and shared it with you]"),
        "{}",
        p.prompt
    );
    assert!(p.prompt.contains("which exited 3"), "{}", p.prompt);
    assert!(
        p.prompt
            .contains("not instructions from the user:\n```\nallow both\n```"),
        "{}",
        p.prompt
    );
    hub.stop_thread("api", "test").unwrap();
    hub.shutdown().await;
}

/// A cancelled command ends `cancelled` at once and tells nobody; only the user runs one.
#[tokio::test]
async fn a_command_is_cancelled_and_only_the_user_runs_one() {
    let dir = tempfile::tempdir().unwrap();
    let hub = hub(dir.path()).await;
    let api = thread(&hub, dir.path(), "api").await;
    let mut sub = hub.subscribe_control(0);
    let r = hub
        .shell_run(run(Some("api"), "sleep 30", None, false), &tui())
        .unwrap();
    let job = r["job"].as_str().unwrap().to_string();
    tokio::time::sleep(Duration::from_millis(300)).await;
    hub.cancel_job(&job, None).unwrap();
    let end = ended(&mut sub, &job).await;
    assert_eq!(end["outcome"], "cancelled", "{end}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(events_of(&hub, &api)
        .iter()
        .all(|e| e.kind() != "turn.started"));
    // The orchestrator never runs one, nor names a folder.
    let e = hub
        .shell_run(run(Some("api"), "ls", None, false), &Caller::Orchestrator)
        .unwrap_err();
    assert_eq!(e.code, codes::UNAUTHORIZED);
    assert_eq!(
        hub.shell_cd(None, None, &Caller::Orchestrator)
            .unwrap_err()
            .code,
        codes::UNAUTHORIZED
    );
    // Nothing to run.
    let e = hub
        .shell_run(run(Some("api"), "   ", None, false), &tui())
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST);
    hub.shutdown().await;
}

/// A worker's `!` command is the user's: when it ends, the orchestrator's conversation hears
/// nothing of it (no "background work finished").
#[tokio::test]
async fn the_orchestrator_hears_nothing_of_a_workers_command() {
    let dir = tempfile::tempdir().unwrap();
    let hub = hub(dir.path()).await;
    let _api = thread(&hub, dir.path(), "api").await;
    hub.say_to_orchestrator("hello", None, &tui()).unwrap();
    let orch = hub
        .shell_run(run(None, "true", None, false), &tui())
        .unwrap()["thread"]
        .as_str()
        .unwrap()
        .to_string();
    let mut sub = hub.subscribe_control(0);
    let r = hub
        .shell_run(run(Some("api"), "echo hi", None, false), &tui())
        .unwrap();
    ended(&mut sub, r["job"].as_str().unwrap()).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let updates: Vec<String> = events_of(&hub, &orch)
        .into_iter()
        .filter_map(|e| match e.body {
            EventBody::ConversationUpdate(p) => Some(p.text),
            _ => None,
        })
        .collect();
    assert!(
        updates.iter().all(|t| !t.contains("background work")),
        "{updates:?}"
    );
    hub.shutdown().await;
}

/// A `!` command is the user's: the thread's agent cannot cancel it (`job.cancel` refused), and
/// it runs on until the user does. The same agent cancels an ordinary job of its thread.
#[tokio::test]
async fn only_the_user_cancels_a_command() {
    let dir = tempfile::tempdir().unwrap();
    let hub = hub(dir.path()).await;
    let api = thread(&hub, dir.path(), "api").await;
    let (mut runtime, by) = recorded_runtime(&hub, &api);
    let mut sub = hub.subscribe_control(0);
    let r = hub
        .shell_run(run(Some("api"), "sleep 30", None, false), &tui())
        .unwrap();
    let job = r["job"].as_str().unwrap().to_string();
    running(&hub, &job).await;
    let e = hub.cancel_agent_job(&job, &api, by).unwrap_err();
    assert_eq!(e.code, codes::RELAY_FORBIDDEN, "{e:?}");
    assert!(e.message.contains("only the user stops it"), "{e:?}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(live(&hub, &job), "the agent's cancel stopped it");
    // An ordinary job of the thread: its agent cancels it.
    let req: JobStart =
        serde_json::from_value(json!({"argv": ["sleep", "30"], "wake": false})).unwrap();
    let other = hub
        .start_job(&api, req, Source::User("tui".into()))
        .unwrap();
    let other = other["job"].as_str().unwrap().to_string();
    hub.cancel_agent_job(&other, &api, by).unwrap();
    assert_eq!(ended(&mut sub, &other).await["outcome"], "cancelled");
    hub.cancel_job(&job, None).unwrap();
    assert_eq!(ended(&mut sub, &job).await["outcome"], "cancelled");
    let _ = runtime.kill();
    let _ = runtime.wait();
    hub.shutdown().await;
}

/// The orchestrator's runtime stopping (here a reset) cancels its jobs, but not the user's `!`
/// command of its view: it runs on in the orchestrator's folder until the user cancels it.
#[tokio::test]
async fn a_reset_leaves_the_orchestrator_views_command_running() {
    let dir = tempfile::tempdir().unwrap();
    let hub = hub(dir.path()).await;
    hub.say_to_orchestrator("hello", None, &tui()).unwrap();
    let mut sub = hub.subscribe_control(0);
    let r = hub
        .shell_run(run(None, "sleep 30", None, false), &tui())
        .unwrap();
    let job = r["job"].as_str().unwrap().to_string();
    let home = std::fs::canonicalize(dir.path().join("home")).unwrap();
    assert_eq!(r["cwd"].as_str(), home.to_str(), "{r}");
    let old = r["thread"].as_str().unwrap().to_string();
    running(&hub, &job).await;
    hub.reset_orchestrator(&tui()).await.unwrap();
    assert_ne!(hub.orchestrator_thread().as_deref(), Some(old.as_str()));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(live(&hub, &job), "the reset cancelled the user's command");
    hub.cancel_job(&job, None).unwrap();
    assert_eq!(ended(&mut sub, &job).await["outcome"], "cancelled");
    hub.shutdown().await;
}

/// Records job `job` running on thread `t` with `shell` as its `!` record (if any), as a hub
/// that then stopped would have left it.
fn lose(hub: &Hub, t: &str, job: &str, shell: Option<Value>) {
    let mut record = json!({"id": job, "wake": true});
    if let Some(s) = shell {
        record["shell"] = s;
    }
    hub.append(
        t,
        None,
        Source::Hub,
        EventBody::TaskUpdated(TaskUpdated {
            task_id: format!("job:{job}"),
            running: true,
            description: format!("{job}: running"),
            ext: Some(serde_json::from_value(json!({"twr": {"job": record}})).unwrap()),
            ..Default::default()
        }),
        None,
    )
    .unwrap();
}

/// A `!!` command a stopped hub ran is recorded lost at the next start and asks the thread's
/// agent nothing (no prompt queued, no turn): the user ran it, and no agent runs it again. A
/// lost job of the same thread still tells it.
#[tokio::test]
async fn a_lost_shared_command_queues_no_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let api = {
        let hub = hub(dir.path()).await;
        let api = thread(&hub, dir.path(), "api").await;
        let shell = json!({"command": "make test", "cwd": dir.path().join("proj"), "share": true});
        lose(&hub, &api, "J0SHARE", Some(shell));
        lose(&hub, &api, "J1JOB", None);
        hub.shutdown().await;
        api
    };
    let hub = hub(dir.path()).await;
    let lost = |evs: &[Event], job: &str| {
        evs.iter().any(|e| {
            matches!(&e.body, EventBody::TaskUpdated(p)
                if !p.running && p.task_id == format!("job:{job}"))
        })
    };
    let woke = |evs: &[Event], job: &str| {
        evs.iter().any(|e| {
            matches!(&e.body, EventBody::TurnStarted(p)
                if p.prompt.contains(&format!("[twr job {job}]")))
        })
    };
    timeout(WAIT, async {
        loop {
            let evs = events_of(&hub, &api);
            if lost(&evs, "J0SHARE") && lost(&evs, "J1JOB") && woke(&evs, "J1JOB") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("both recorded lost, the job's thread told");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!woke(&events_of(&hub, &api), "J0SHARE"));
    let q = hub.thread_queue(&api, &tui()).unwrap();
    assert!(
        q["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| !i["prompt"].as_str().unwrap().contains("J0SHARE")),
        "{q}"
    );
    hub.stop_thread("api", "test").unwrap();
    hub.shutdown().await;
}

/// A worker's turn that ends while a `!` command of the user's runs in its worktree is not
/// interim: the command starts no turn that would report again, so the hub's report is final,
/// and so is one written later, which raises its review at once.
#[tokio::test]
async fn a_command_running_leaves_a_workers_report_final() {
    let dir = tempfile::tempdir().unwrap();
    let hub = hub(dir.path()).await;
    let api = thread(&hub, dir.path(), "api").await;
    let mut sub = hub.subscribe_control(0);
    let r = hub
        .shell_run(run(Some("api"), "sleep 60", None, false), &tui())
        .unwrap();
    let job = r["job"].as_str().unwrap().to_string();
    running(&hub, &job).await;
    hub.send_by(&api, "do it", &tui()).unwrap();
    let report = timeout(WAIT, async {
        loop {
            let (_, ev) = sub.next().await.unwrap().expect("hub stopped");
            if let EventBody::ReportReady(p) = &ev.body {
                if ev.thread == api {
                    return p.clone();
                }
            }
        }
    })
    .await
    .unwrap();
    let twr = Value::Object(report.ext.unwrap_or_default())["twr"].clone();
    assert!(twr["background_tasks"].is_null(), "{twr}");
    let written = hub
        .write_report(
            &api,
            Some(1),
            "Done, untested.",
            Grade::NeedsVerification,
            &Caller::Orchestrator,
        )
        .unwrap();
    assert!(!written["review"].is_null(), "{written}");
    assert!(live(&hub, &job), "still running");
    hub.cancel_job(&job, None).unwrap();
    hub.shutdown().await;
}
