//! Next-message suggestions (`suggest.next`, `hub::suggest`): what the CLI is run with and
//! reads, one call per turn shared by every ask, refusals and the backoff after a failure.
//!
//! The CLI is a script standing in for `claude`: it records its arguments, input, folder and
//! environment beside itself and answers what `answer.json` there says.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::time::timeout;
use twr_adapters::fake::FakeAdapter;
use twr_core::adapter::Adapter;
use twr_core::config::Config;
use twr_core::proto::codes;
use twr_hub::hub::ControlSubscription;
use twr_hub::{Caller, Hub, HubOptions, NewThread};

const WAIT: Duration = Duration::from_secs(10);

/// The stand-in CLI in `dir`: each run appends a line to `runs`, writes `argv` (one per
/// line), `stdin`, `cwd` and `env`, then prints `answer.json`.
fn fake_claude(dir: &Path) -> PathBuf {
    let bin = dir.join("claude");
    std::fs::write(
        &bin,
        r#"#!/bin/sh
here=$(dirname "$0")
echo run >> "$here/runs"
: > "$here/argv"
for a in "$@"; do printf '%s\n' "$a" >> "$here/argv"; done
cat > "$here/stdin"
pwd > "$here/cwd"
env > "$here/env"
cat "$here/answer.json"
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

fn answer(dir: &Path, result: &str, error: bool) {
    let v = serde_json::json!({"type": "result", "is_error": error, "result": result,
                               "total_cost_usd": 0.0001});
    std::fs::write(dir.join("answer.json"), v.to_string()).unwrap();
}

fn runs(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join("runs"))
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

async fn hub(dir: &Path, bin: PathBuf) -> Hub {
    let config = Config {
        default_runtime: "fake".into(),
        ..Default::default()
    };
    let mut o = HubOptions::new(dir.join("data").join("twr.db"), config);
    o.adapters = Arc::new(|name: &str| -> Option<Arc<dyn Adapter>> {
        match name {
            "fake" => Some(Arc::new(FakeAdapter::new())),
            _ => None,
        }
    });
    o.socket = Some(dir.join("hub.sock"));
    o.stop_timeout = Duration::from_secs(5);
    o.suggest_bin = Some(bin);
    Hub::open(o).await.unwrap()
}

fn tui() -> Caller {
    Caller::user("tui")
}

/// A thread `api` that ran turn 1 on `prompt` to its end.
async fn turn_ended(hub: &Hub, dir: &Path, prompt: &str) -> String {
    let project = dir.join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let v = hub
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
    let id = v["id"].as_str().unwrap().to_string();
    let mut sub = hub.subscribe_control(0);
    hub.send_by(&id, prompt, &tui()).unwrap();
    until_ended(&mut sub, &id, 1).await;
    id
}

async fn until_ended(sub: &mut ControlSubscription, thread: &str, turn: u64) {
    timeout(WAIT, async {
        loop {
            let (_, ev) = sub.next().await.unwrap().expect("hub stopped");
            if ev.thread == thread && ev.kind() == "turn.ended" && ev.turn == Some(turn) {
                return;
            }
        }
    })
    .await
    .expect("timed out waiting for the turn's end");
}

/// After a turn ended, the CLI is run once, with no tools, settings or MCP servers, the
/// model asked for and a system prompt of twr's, in a private folder, without twr's tokens,
/// reading the turn's prompt and the end of the reply; its answer is the suggestion, kept
/// for the next ask.
#[tokio::test]
async fn a_suggestion_comes_from_one_bare_cli_call_per_turn() {
    let dir = tempfile::tempdir().unwrap();
    let cli = dir.path().join("cli");
    std::fs::create_dir_all(&cli).unwrap();
    let bin = fake_claude(&cli);
    answer(&cli, "\"Now run the integration tests\"\n", false);
    let hub = hub(dir.path(), bin).await;
    let api = turn_ended(
        &hub,
        dir.path(),
        "fix the login bug\nI fixed the login bug.",
    )
    .await;
    let r = hub.suggest_next("api", 1, "haiku", &tui()).await.unwrap();
    assert_eq!(r["suggestion"], "Now run the integration tests", "{r}");
    assert_eq!(r["cached"], false);
    assert_eq!(r["stats"]["calls"], 1);
    let argv: Vec<String> = std::fs::read_to_string(cli.join("argv"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    let at = |flag: &str| {
        argv.iter()
            .position(|a| a == flag)
            .unwrap_or_else(|| panic!("no {flag} in {argv:?}"))
    };
    assert_eq!(argv[0], "-p");
    assert_eq!(argv[at("--model") + 1], "haiku");
    assert_eq!(argv[at("--output-format") + 1], "json");
    assert_eq!(argv[at("--tools") + 1], "");
    assert_eq!(argv[at("--setting-sources") + 1], "");
    at("--strict-mcp-config");
    at("--no-session-persistence");
    at("--disable-slash-commands");
    assert!(argv[at("--system-prompt") + 1].contains("reply exactly NONE"));
    let stdin = std::fs::read_to_string(cli.join("stdin")).unwrap();
    assert!(
        stdin.starts_with("Conversation: api, with a coding agent"),
        "{stdin}"
    );
    assert!(
        stdin
            .contains("<user_message>\nfix the login bug\nI fixed the login bug.\n</user_message>"),
        "{stdin}"
    );
    assert!(stdin.contains("<agent_reply_end>\n"), "{stdin}");
    let cwd = std::fs::read_to_string(cli.join("cwd")).unwrap();
    assert!(cwd.trim_end().ends_with("data/suggest"), "{cwd}");
    let env = std::fs::read_to_string(cli.join("env")).unwrap();
    assert!(
        env.lines().any(|l| l == format!("TWR_AGENT={api}:suggest")),
        "{env}"
    );
    assert!(!env.contains("TWR_ORCH_TOKEN="));
    assert!(!env.contains("TWR_RUNNER_TOKEN="));
    // Asked again (another TUI): the same answer, no second run.
    let again = hub.suggest_next("api", 1, "haiku", &tui()).await.unwrap();
    assert_eq!(
        (again["suggestion"].as_str(), again["cached"].as_bool()),
        (Some("Now run the integration tests"), Some(true))
    );
    assert_eq!(runs(&cli), 1);
    // Nothing recorded on the thread.
    let (_, log) = hub.thread_events("api", 0, 10_000).unwrap();
    assert!(log.iter().all(|e| {
        e.body
            .data()
            .map(|d| !d.to_string().contains("integration tests"))
            .unwrap_or(true)
    }));
    hub.shutdown().await;
}

/// No suggestion that reads as an answer or a short command; `NONE` is none; a turn still
/// running is not asked about; only the user asks, with a model name the CLI takes.
#[tokio::test]
async fn answers_none_running_turns_and_callers_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let cli = dir.path().join("cli");
    std::fs::create_dir_all(&cli).unwrap();
    let bin = fake_claude(&cli);
    answer(&cli, "allow both", false);
    let hub = hub(dir.path(), bin).await;
    turn_ended(&hub, dir.path(), "check it").await;
    let r = hub.suggest_next("api", 1, "haiku", &tui()).await.unwrap();
    assert_eq!(
        (r["suggestion"].is_null(), r["reason"].as_str()),
        (true, Some("none")),
        "{r}"
    );
    let e = hub
        .suggest_next("api", 1, "--dangerously-skip-permissions", &tui())
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST);
    let e = hub
        .suggest_next("api", 1, "haiku", &Caller::Orchestrator)
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::UNAUTHORIZED);
    let e = hub
        .suggest_next("api", 9, "haiku", &tui())
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::NOT_FOUND);
    // A running turn.
    hub.send_by("api", "!sleep 5000", &tui()).unwrap();
    let r = hub.suggest_next("api", 2, "haiku", &tui()).await.unwrap();
    assert_eq!(r["reason"], "running", "{r}");
    assert_eq!(runs(&cli), 1);
    hub.stop_thread("api", "test").unwrap();
    hub.shutdown().await;
}

/// A failed call (not signed in, a usage limit) says why, and none follows for a while.
#[tokio::test]
async fn a_failure_holds_suggestions_back() {
    let dir = tempfile::tempdir().unwrap();
    let cli = dir.path().join("cli");
    std::fs::create_dir_all(&cli).unwrap();
    let bin = fake_claude(&cli);
    answer(&cli, "Invalid API key · Please run /login", true);
    let hub = hub(dir.path(), bin).await;
    let api = turn_ended(&hub, dir.path(), "one").await;
    let r = hub.suggest_next("api", 1, "haiku", &tui()).await.unwrap();
    assert_eq!(
        r["reason"].as_str(),
        Some("failed: Invalid API key · Please run /login"),
        "{r}"
    );
    assert_eq!(r["stats"]["failures"], 1);
    let mut sub = hub.subscribe_control(0);
    hub.send_by(&api, "two", &tui()).unwrap();
    until_ended(&mut sub, &api, 2).await;
    answer(&cli, "fine now", false);
    let r = hub.suggest_next("api", 2, "haiku", &tui()).await.unwrap();
    assert_eq!(r["reason"], "backoff", "{r}");
    assert_eq!(runs(&cli), 1);
    hub.shutdown().await;
}
