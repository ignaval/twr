//! Generic ACP runtimes through the hub: `thread new --runtime 'acp:<command line>'` and
//! `[runtimes.<name>] acp = "<command>"` presets. The command is split into argv, recorded in
//! `thread.created` `ext.twr.acp_command` (a preset also `acp_preset` / `acp_attach`), and
//! handed to the `acp` adapter as `TWR_ACP_COMMAND` on spawn and resume; a preset's attach
//! template makes `twr attach` work; `twr orchestrator use <preset>` runs the orchestrator on
//! the agent (its system prompt on the first prompt, the twr MCP server over `mcpServers`,
//! every tool call a permission item for the user). The agent is the scripted ACP stub of the
//! adapter tests (`twr-adapters/tests/fixtures/acp/stub_agent.py`, python3).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::time::timeout;
use twr_adapters::acp::{self, AcpAdapter};
use twr_core::adapter::{Adapter, Answer};
use twr_core::config::{Config, RuntimePreset};
use twr_core::event::*;
use twr_core::proto::codes;
use twr_hub::hub::ThreadSubscription;
use twr_hub::{Caller, Hub, HubOptions, NewThread, SwitchOptions};

const WAIT: Duration = Duration::from_secs(20);

fn stub() -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../twr-adapters/tests/fixtures/acp/stub_agent.py")
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

/// The agent's state directory (its stdin log, sessions).
fn state(dir: &Path) -> PathBuf {
    let s = dir.join("agent");
    std::fs::create_dir_all(&s).unwrap();
    s
}

/// The state directory of preset `stub-b`'s agent (the same stub, a separate state).
fn state_b(dir: &Path) -> PathBuf {
    let s = dir.join("agent-b");
    std::fs::create_dir_all(&s).unwrap();
    s
}

fn opts(dir: &Path) -> HubOptions {
    let mut runtimes = BTreeMap::new();
    runtimes.insert(
        "stub".to_string(),
        RuntimePreset {
            acp: format!("python3 {} {}", stub(), state(dir).display()),
            attach: Some("stub-cli --resume {session}".into()),
        },
    );
    // An agent that takes a model and an effort, attached by a command that hands neither
    // over (`tuned`) or both (`tuned-flags`).
    for (name, attach) in [
        ("tuned", "stub-cli --resume {session}"),
        (
            "tuned-flags",
            "stub-cli --resume {session} --model={model} --effort={effort}",
        ),
    ] {
        runtimes.insert(
            name.to_string(),
            RuntimePreset {
                acp: format!(
                    "python3 {} {} --model-option --effort-option",
                    stub(),
                    state(dir).display()
                ),
                attach: Some(attach.into()),
            },
        );
    }
    runtimes.insert(
        "stub-b".to_string(),
        RuntimePreset {
            acp: format!("python3 {} {}", stub(), state_b(dir).display()),
            attach: None,
        },
    );
    let config = Config {
        default_runtime: "fake".into(),
        runtimes,
        ..Default::default()
    };
    let mut o = HubOptions::new(dir.join("data").join("twr.db"), config);
    o.adapters = Arc::new(|name: &str| -> Option<Arc<dyn Adapter>> {
        (name == acp::NAME).then(|| {
            Arc::new(AcpAdapter::new().stop_grace(Duration::from_secs(1))) as Arc<dyn Adapter>
        })
    });
    o.socket = Some(dir.join("hub.sock"));
    o.twr_exe = Some(PathBuf::from("/opt/twr/bin/twr"));
    o.stop_timeout = Duration::from_secs(5);
    o.git_tick = Some(Duration::ZERO);
    o.stall_after = Some(Duration::ZERO);
    o
}

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
        panic!(
            "timed out; saw {:?}",
            seen.iter().map(|e| e.kind()).collect::<Vec<_>>()
        )
    });
    seen
}

fn final_text(evs: &[Event]) -> String {
    evs.iter()
        .filter_map(|e| match &e.body {
            EventBody::AgentText(t) if t.is_final => Some(t.text.as_str()),
            _ => None,
        })
        .collect()
}

fn result(evs: &[Event]) -> TurnResult {
    match &evs.last().unwrap().body {
        EventBody::TurnEnded(e) => e.result,
        other => panic!("{other:?}"),
    }
}

fn started_ext(evs: &[Event]) -> Value {
    evs.iter()
        .find_map(|e| match &e.body {
            EventBody::ThreadStarted(s) => Some(json!(s.ext)),
            _ => None,
        })
        .expect("thread.started")
}

fn agent_stdin(dir: &Path) -> Vec<Value> {
    stdin_of(&state(dir))
}

/// Every line the agent whose state directory is `state` read.
fn stdin_of(state: &Path) -> Vec<Value> {
    std::fs::read_to_string(state.join("stdin.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// The `method` requests among `lines`.
fn requests<'a>(lines: &'a [Value], method: &str) -> Vec<&'a Value> {
    lines.iter().filter(|l| l["method"] == method).collect()
}

/// The text of the last `session/prompt` among `lines`.
fn last_prompt(lines: &[Value]) -> String {
    requests(lines, "session/prompt").last().expect("a prompt")["params"]["prompt"][0]["text"]
        .as_str()
        .unwrap()
        .to_string()
}

/// A `thread.stopped` that closed a runtime the thread moved away from.
fn runtime_change(e: &Event) -> bool {
    matches!(&e.body, EventBody::ThreadStopped(p)
        if p.ext.as_ref().and_then(|x| x["twr"]["reason"].as_str()) == Some(RUNTIME_RESTART_REASON))
}

fn new(project: &Path, runtime: &str, name: &str) -> NewThread {
    NewThread {
        project: project.to_string_lossy().into_owned(),
        runtime: Some(runtime.into()),
        name: Some(name.into()),
        no_worktree: true,
        ..Default::default()
    }
}

fn user() -> Source {
    Source::User("test".into())
}

#[tokio::test]
async fn an_acp_thread_runs_its_agent_on_spawn_and_resume() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let st = state(dir.path());
    let runtime = format!("acp:python3 '{}' {}", stub(), st.display());
    let t = hub
        .new_thread(new(dir.path(), &runtime, "gem"), "test")
        .await
        .unwrap();
    assert_eq!(t["runtime"], "acp");
    let argv = json!(["python3", stub(), st.to_string_lossy()]);
    assert_eq!(t["acp_command"], argv);
    let id = t["id"].as_str().unwrap().to_string();
    let log = hub.thread_events(&id, 0, 10).unwrap().1;
    let EventBody::ThreadCreated(c) = &log[0].body else {
        panic!("{:?}", log[0])
    };
    assert_eq!(c.runtime, "acp");
    assert_eq!(c.ext.as_ref().unwrap()["twr"]["acp_command"], argv);

    let mut sub = hub.subscribe_thread(&id, 0).unwrap();
    hub.send(&id, "hello", TurnFrom::User, user()).unwrap();
    let evs = until(&mut sub, |e| e.kind() == "turn.ended").await;
    assert_eq!(final_text(&evs), "Hello!");
    assert_eq!(result(&evs), TurnResult::Completed);
    assert_eq!(started_ext(&evs)["acp"]["command"], argv);

    // A permission is the user's: the hub records it, the answer selects `allow_once`.
    hub.send(&id, "perm", TurnFrom::User, user()).unwrap();
    let evs = until(&mut sub, |e| e.kind() == "attention.raised").await;
    let EventBody::AttentionRaised(a) = &evs.last().unwrap().body else {
        panic!()
    };
    assert_eq!(a.reason, AttentionReason::Permission);
    hub.respond(&a.attn_id, Answer::allow(), "test").unwrap();
    let evs = until(&mut sub, |e| e.kind() == "turn.ended").await;
    assert_eq!(result(&evs), TurnResult::Completed);

    // Stopped, then sent again: resumed in a new process with the same command; the
    // session remembers.
    hub.stop_thread(&id, "test").unwrap();
    until(&mut sub, |e| e.kind() == "thread.stopped").await;
    hub.send(&id, "recall", TurnFrom::User, user()).unwrap();
    let evs = until(&mut sub, |e| e.kind() == "turn.ended").await;
    assert_eq!(final_text(&evs), "before: hello | perm");
    let resumed = agent_stdin(dir.path());
    assert!(
        resumed.iter().any(|l| l["method"] == "session/resume"),
        "{resumed:?}"
    );
    // No `attach` without a preset's attach command.
    let e = hub.attach_begin(&id, true, "test").await.unwrap_err();
    assert_eq!(e.code, codes::UNSUPPORTED, "{e:?}");
    hub.shutdown().await;
}

#[tokio::test]
async fn a_preset_names_the_agent_and_its_attach_command() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let t = hub
        .new_thread(new(dir.path(), "stub", "s1"), "test")
        .await
        .unwrap();
    assert_eq!(t["runtime"], "acp");
    let id = t["id"].as_str().unwrap().to_string();
    let log = hub.thread_events(&id, 0, 10).unwrap().1;
    let EventBody::ThreadCreated(c) = &log[0].body else {
        panic!()
    };
    let twr = &c.ext.as_ref().unwrap()["twr"];
    assert_eq!(twr["acp_preset"], "stub");
    assert_eq!(
        twr["acp_attach"],
        json!(["stub-cli", "--resume", "{session}"])
    );
    assert_eq!(twr["acp_command"][0], "python3");

    let mut sub = hub.subscribe_thread(&id, 0).unwrap();
    hub.send(&id, "hello", TurnFrom::User, user()).unwrap();
    let evs = until(&mut sub, |e| e.kind() == "turn.ended").await;
    assert_eq!(result(&evs), TurnResult::Completed);
    let session = evs
        .iter()
        .find_map(|e| match &e.body {
            EventBody::ThreadStarted(s) => s.session_id.clone(),
            _ => None,
        })
        .unwrap();
    let grant = hub.attach_begin(&id, false, "test").await.unwrap();
    assert_eq!(grant.argv, ["stub-cli", "--resume", session.as_str()]);
    assert_eq!(grant.runtime, "acp");
    hub.attach_end(&id, "test", grant.lease, None).unwrap();
    hub.shutdown().await;
}

#[tokio::test]
async fn bad_acp_runtimes_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    for (runtime, says) in [
        ("acp", "needs the agent's command"),
        ("acp:", "names no program"),
        ("acp:  ", "names no program"),
        ("acp:gemini 'unterminated", "unterminated"),
    ] {
        let e = hub
            .new_thread(new(dir.path(), runtime, "x"), "test")
            .await
            .unwrap_err();
        assert_eq!(e.code, codes::BAD_REQUEST, "{runtime}: {e:?}");
        assert!(e.message.contains(says), "{runtime}: {}", e.message);
    }
    let long = format!("acp:{}", "x".repeat(5000));
    let e = hub
        .new_thread(new(dir.path(), &long, "x"), "test")
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST);
    for runtime in ["acp", "acp:", "nope"] {
        let e = hub
            .use_orchestrator(
                runtime,
                None,
                SwitchOptions::default(),
                &Caller::user("tui"),
            )
            .await
            .unwrap_err();
        assert_eq!(e.code, codes::BAD_REQUEST, "{runtime}: {e:?}");
    }
    assert!(hub.list_threads().is_empty());
    hub.shutdown().await;
}

/// `twr orchestrator use stub`: the orchestrator runs on the ACP agent. Its system prompt is
/// on the first prompt of the new session, the twr MCP server (with the token) goes over the
/// standard `mcpServers`, and the agent's tool calls are permission items for the user.
#[tokio::test]
async fn a_preset_hosts_the_orchestrator() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let info = hub.orchestrator_info();
    assert!(
        info["runtimes"]
            .as_array()
            .unwrap()
            .contains(&json!("stub")),
        "{info}"
    );
    let r = hub
        .use_orchestrator("stub", None, SwitchOptions::default(), &Caller::user("tui"))
        .await
        .unwrap();
    assert_eq!(r["runtime"], "acp");
    let id = r["thread"].as_str().unwrap().to_string();
    let mut sub = hub.subscribe_thread(&id, 0).unwrap();
    hub.send(&id, "perm", TurnFrom::User, user()).unwrap();
    let evs = until(&mut sub, |e| e.kind() == "attention.raised").await;
    let ext = started_ext(&evs);
    assert!(ext["acp"]["tool_restriction"]
        .as_str()
        .unwrap()
        .contains("no built-in tools"));
    let EventBody::AttentionRaised(a) = &evs.last().unwrap().body else {
        panic!()
    };
    hub.respond(&a.attn_id, Answer::deny(), "test").unwrap();
    let evs = until(&mut sub, |e| e.kind() == "turn.ended").await;
    assert_eq!(result(&evs), TurnResult::Interrupted);

    let lines = agent_stdin(dir.path());
    let new = lines.iter().find(|l| l["method"] == "session/new").unwrap();
    let servers = new["params"]["mcpServers"].as_array().unwrap();
    assert_eq!(servers.len(), 1, "{servers:?}");
    assert_eq!(servers[0]["name"], "twr");
    assert_eq!(servers[0]["command"], "/opt/twr/bin/twr");
    assert_eq!(servers[0]["env"][0]["name"], "TWR_ORCH_TOKEN");
    let prompt = lines
        .iter()
        .find(|l| l["method"] == "session/prompt")
        .unwrap()["params"]["prompt"][0]["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(prompt.starts_with(acp::SYSTEM_OPEN), "{prompt}");
    assert!(prompt.contains("orchestrator"), "{prompt}");
    assert!(prompt.ends_with("\n\nperm"), "{prompt}");
    hub.shutdown().await;
}

/// An orchestrator switched to a preset records the canonical runtime `acp` (with its command):
/// it stays available (`orchestrator.show`), and `say` delegates to it, also after a restart.
#[tokio::test]
async fn an_acp_orchestrator_stays_available_and_takes_delegations_across_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let r = hub
        .use_orchestrator("stub", None, SwitchOptions::default(), &Caller::user("tui"))
        .await
        .unwrap();
    assert_eq!(r["runtime"], "acp");
    let id = r["thread"].as_str().unwrap().to_string();
    delegates(&hub, &id, "hello before").await;
    hub.shutdown().await;
    drop(hub);
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    delegates(&hub, &id, "hello after").await;
    hub.shutdown().await;
}

/// The orchestrator `id` is available, and `say` (nothing else to route to) hands it `said`.
async fn delegates(hub: &Hub, id: &str, said: &str) {
    let info = hub.orchestrator_info();
    assert_eq!(info["available"], true, "{said}: {info}");
    assert_eq!(
        (&info["runtime"], &info["thread"]),
        (&json!("acp"), &json!(id))
    );
    let mut sub = hub.subscribe_thread(id, 0).unwrap();
    let r = hub.say(said, None, &Caller::user("tui")).await.unwrap();
    assert_eq!(r["action"], "delegate", "{said}: {r}");
    let evs = until(
        &mut sub,
        |e| matches!(&e.body, EventBody::TurnStarted(t) if t.prompt == said),
    )
    .await;
    assert_eq!(evs.last().unwrap().thread, id);
    until(&mut sub, |e| e.kind() == "turn.ended").await;
}

/// A thread in a restricted mode is not attached through a preset's attach
/// template without `{mode}` (the interactive CLI would run in its own default mode): a
/// conflict saying so, before anything is stopped.
#[tokio::test]
async fn a_restricted_thread_needs_mode_in_the_attach_template() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let mut req = new(dir.path(), "stub", "planner");
    req.mode = Some("plan".into());
    let t = hub.new_thread(req, "test").await.unwrap();
    let id = t["id"].as_str().unwrap().to_string();
    let e = hub.attach_begin(&id, true, "test").await.unwrap_err();
    assert_eq!(e.code, codes::CONFLICT, "{e:?}");
    assert!(
        e.message.contains("no {mode}") && e.message.contains("restricted"),
        "{e:?}"
    );
    hub.shutdown().await;
}

/// A `tuned` / `tuned-flags` thread on model `m1` at effort `low` that ran a turn and was
/// stopped, then moved to `m2` / `high` (taken at its next start): its id and session.
async fn moved_while_stopped(hub: &Hub, dir: &Path, runtime: &str) -> (String, String) {
    let mut req = new(dir, runtime, runtime);
    req.model = Some("m1".into());
    req.effort = Some("low".into());
    let t = hub.new_thread(req, "test").await.unwrap();
    let id = t["id"].as_str().unwrap().to_string();
    let mut sub = hub.subscribe_thread(&id, 0).unwrap();
    hub.send(&id, "hello", TurnFrom::User, user()).unwrap();
    let evs = until(&mut sub, |e| e.kind() == "turn.ended").await;
    assert_eq!(result(&evs), TurnResult::Completed);
    let twr = &started_ext(&evs)["twr"];
    assert_eq!(
        (&twr["model"], &twr["effort"]),
        (&json!("m1"), &json!("low"))
    );
    let session = evs
        .iter()
        .find_map(|e| match &e.body {
            EventBody::ThreadStarted(s) => s.session_id.clone(),
            _ => None,
        })
        .unwrap();
    hub.stop_thread(&id, "test").unwrap();
    until(&mut sub, |e| e.kind() == "thread.stopped").await;
    let r = hub
        .set_model(&id, Some("m2"), Some("high"), &Caller::user("test"))
        .unwrap();
    assert_eq!(r["applies"], "next_start", "{r}");
    (id, session)
}

/// An ACP session keeps its model and effort on resume, so an attach command that does not
/// hand them over would resume it in the old ones: a thread whose new model and effort have not
/// reached its session yet is not attached (`conflict`, saying so, before anything is
/// recorded). Once a prompt has resumed the session in them, the same attach is granted.
#[tokio::test]
async fn an_attach_that_cannot_apply_new_settings_is_refused_until_they_ran() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let (id, session) = moved_while_stopped(&hub, dir.path(), "tuned").await;
    let log = hub.thread_events(&id, 0, 1000).unwrap().1;
    let (before, last) = (log.len(), log.last().unwrap().seq);
    let e = hub.attach_begin(&id, false, "test").await.unwrap_err();
    assert_eq!(e.code, codes::CONFLICT, "{e:?}");
    assert!(
        e.message.contains(r#"runs model "m1" and effort "low""#)
            && e.message.contains(r#"model "m2" and effort "high""#)
            && e.message.contains("send tuned a prompt first"),
        "{}",
        e.message
    );
    assert_eq!(hub.thread_events(&id, 0, 1000).unwrap().1.len(), before);

    // A prompt resumes the session in the new settings: now the attach is fine.
    let mut sub = hub.subscribe_thread(&id, last).unwrap();
    hub.send(&id, "hello", TurnFrom::User, user()).unwrap();
    let evs = until(&mut sub, |e| e.kind() == "turn.ended").await;
    assert_eq!(result(&evs), TurnResult::Completed);
    let grant = hub.attach_begin(&id, false, "test").await.unwrap();
    assert_eq!(grant.argv, ["stub-cli", "--resume", session.as_str()]);
    hub.attach_end(&id, "test", grant.lease, None).unwrap();
    hub.shutdown().await;
}

/// A preset whose attach template has `{model}` and `{effort}` hands the thread's new settings
/// to the interactive CLI: no refusal, and the values are in the command. A thread without
/// them leaves those arguments out.
#[tokio::test]
async fn attach_template_placeholders_carry_the_new_settings() {
    let dir = tempfile::tempdir().unwrap();
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let (id, session) = moved_while_stopped(&hub, dir.path(), "tuned-flags").await;
    let grant = hub.attach_begin(&id, false, "test").await.unwrap();
    assert_eq!(
        grant.argv,
        [
            "stub-cli",
            "--resume",
            session.as_str(),
            "--model=m2",
            "--effort=high"
        ]
    );
    hub.attach_end(&id, "test", grant.lease, None).unwrap();

    let t = hub
        .new_thread(new(dir.path(), "tuned-flags", "plain"), "test")
        .await
        .unwrap();
    let plain = t["id"].as_str().unwrap().to_string();
    let mut sub = hub.subscribe_thread(&plain, 0).unwrap();
    hub.send(&plain, "hello", TurnFrom::User, user()).unwrap();
    let evs = until(&mut sub, |e| e.kind() == "turn.ended").await;
    assert_eq!(result(&evs), TurnResult::Completed);
    let grant = hub.attach_begin(&plain, false, "test").await.unwrap();
    assert_eq!(grant.argv.len(), 3, "{:?}", grant.argv);
    hub.attach_end(&plain, "test", grant.lease, None).unwrap();
    hub.shutdown().await;
}

/// `{model}` / `{effort}` alone as an argument would leave its flag behind for a thread without
/// the setting: such a preset is refused when the thread is created.
#[tokio::test]
async fn a_lone_model_placeholder_in_an_attach_template_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut o = opts(dir.path());
    o.config.runtimes.insert(
        "lone".into(),
        RuntimePreset {
            acp: format!("python3 {} {}", stub(), state(dir.path()).display()),
            attach: Some("stub-cli --resume {session} -m {model}".into()),
        },
    );
    let hub = Hub::open(o).await.unwrap();
    let e = hub
        .new_thread(new(dir.path(), "lone", "x"), "test")
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST, "{e:?}");
    assert!(
        e.message.contains("runtimes.lone") && e.message.contains("{model} as a whole argument"),
        "{}",
        e.message
    );
    hub.shutdown().await;
}

/// A thread moves between two ACP presets with the same mode, model and effort.
/// The runtime stays `acp`, yet the previous agent's process is replaced (`runtime_change`):
/// idle at once, mid-turn once the turn ended. Each next turn starts a new session of the other
/// agent (never a resume of the previous agent's), its prompt carrying the handoff.
#[tokio::test]
async fn a_thread_moves_between_two_acp_presets() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (state(dir.path()), state_b(dir.path()));
    let hub = Hub::open(opts(dir.path())).await.unwrap();
    let t = hub
        .new_thread(new(dir.path(), "stub", "mover"), "test")
        .await
        .unwrap();
    let id = t["id"].as_str().unwrap().to_string();
    let mut sub = hub.subscribe_thread(&id, 0).unwrap();
    hub.send(&id, "hello", TurnFrom::User, user()).unwrap();
    until(&mut sub, |e| e.kind() == "turn.ended").await;
    assert_eq!(hub.live_runtimes(), 1);

    // Idle: the agent of `stub` stops now, and nothing starts it again.
    let r = hub
        .set_runtime_model(&id, Some("stub-b"), None, None, &Caller::user("test"))
        .unwrap();
    assert_eq!(
        (&r["runtime"], &r["previous_runtime"], &r["runtime_changed"]),
        (&json!("acp"), &json!("acp"), &json!(true)),
        "{r}"
    );
    assert_eq!(r["applies"], "now", "{r}");
    until(&mut sub, runtime_change).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(hub.live_runtimes(), 0);
    hub.send(&id, "recall", TurnFrom::User, user()).unwrap();
    let evs = until(&mut sub, |e| e.kind() == "turn.ended").await;
    assert_eq!(final_text(&evs), "before: ", "a new session of stub-b");
    let (in_a, in_b) = (stdin_of(&a), stdin_of(&b));
    assert_eq!(
        requests(&in_a, "session/prompt").len(),
        1,
        "stub got no more"
    );
    assert_eq!(requests(&in_b, "session/new").len(), 1);
    assert!(
        requests(&in_b, "session/resume").is_empty() && requests(&in_b, "session/load").is_empty()
    );
    let prompt = last_prompt(&in_b);
    assert!(prompt.starts_with("[twr handoff]"), "{prompt}");
    assert!(prompt.ends_with("</twr-handoff>\n\nrecall"), "{prompt}");
    assert!(prompt.contains("reply: Hello!"), "{prompt}");
    let v = hub.thread_view(&id).unwrap();
    assert!(!v.handoff_pending && v.session_id.is_some());
    assert_eq!(
        hub.get_thread(&id).unwrap()["acp_command"][2],
        json!(b.to_string_lossy())
    );

    // Mid-turn: back to `stub` once the turn (waiting on a permission) ended.
    hub.send(&id, "perm", TurnFrom::User, user()).unwrap();
    let evs = until(&mut sub, |e| e.kind() == "attention.raised").await;
    let EventBody::AttentionRaised(item) = &evs.last().unwrap().body else {
        panic!()
    };
    let r = hub
        .set_runtime_model(&id, Some("stub"), None, None, &Caller::user("test"))
        .unwrap();
    assert_eq!(r["applies"], "after_turn", "{r}");
    hub.respond(&item.attn_id, Answer::allow(), "test").unwrap();
    let evs = until(&mut sub, runtime_change).await;
    let ended = evs.iter().position(|e| e.kind() == "turn.ended");
    assert!(ended.is_some(), "the turn ended first: {:?}", kinds(&evs));
    hub.send(&id, "recall", TurnFrom::User, user()).unwrap();
    let evs = until(&mut sub, |e| e.kind() == "turn.ended").await;
    assert_eq!(final_text(&evs), "before: ", "a new session of stub");
    let in_a = stdin_of(&a);
    assert_eq!(requests(&in_a, "session/new").len(), 2);
    let prompt = last_prompt(&in_a);
    assert!(prompt.starts_with("[twr handoff]"), "{prompt}");
    assert!(prompt.ends_with("recall"), "{prompt}");
    assert_eq!(
        requests(&stdin_of(&b), "session/prompt").len(),
        2,
        "stub-b got no more"
    );
    hub.shutdown().await;
}

fn kinds(evs: &[Event]) -> Vec<&str> {
    evs.iter().map(|e| e.kind()).collect()
}
