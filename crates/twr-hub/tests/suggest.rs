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
use twr_hub::{Caller, Hub, HubOptions, NewThread, ShellRun};

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
if [ -f "$here/leave" ]; then
  sleep 300 > /dev/null 2>&1 < /dev/null &
  echo $! > "$here/left"
fi
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

/// Whether `pid` is a zombie (ended, not yet reaped by its parent, here init).
fn zombie(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| {
            s.rsplit_once(')')
                .map(|(_, r)| r.trim_start().starts_with('Z'))
        })
        .unwrap_or(true)
}

fn runs(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join("runs"))
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

async fn hub(dir: &Path, bin: PathBuf) -> Hub {
    hub_probing(dir, bin, None).await
}

/// [`hub`] whose own agent probe finds the agents `probe` lists (`None`: it probes none).
/// Its Codex is `codex` in `dir`'s `codex-cli` when there is one there ([`fake_codex`]),
/// else a path that doesn't exist: no test runs the real one.
async fn hub_probing(
    dir: &Path,
    bin: PathBuf,
    probe: Option<Vec<twr_core::runner::AgentInfo>>,
) -> Hub {
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
    o.suggest_codex_bin = Some(dir.join("codex-cli").join("codex"));
    // Not the developer's own Codex home: its global instructions would keep Codex out.
    o.suggest_codex_home = Some(dir.join("codex-home"));
    if let Some(probe) = probe {
        o.agent_probe = Some(Arc::new(move || {
            let probe = probe.clone();
            futures::FutureExt::boxed(async move { probe })
        }));
    }
    // A `!!` command runs through the job gate, which this test binary isn't.
    o.job_gate_exe = Some(env!("CARGO_BIN_EXE_twr-job-gate").into());
    Hub::open(o).await.unwrap()
}

fn tui() -> Caller {
    Caller::user("tui")
}

/// The models a TUI asks with: `claude` for Claude Code, Codex's default.
fn m(claude: &str) -> twr_core::config::SuggestModels {
    twr_core::config::SuggestModels {
        claude_code: claude.into(),
        ..Default::default()
    }
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
    // An API key or another endpoint in the hub's environment: not the CLI's (the user's
    // login only, and the text goes nowhere else). And what it leaves running goes with it.
    std::env::set_var("ANTHROPIC_API_KEY", "sk-sentinel");
    std::env::set_var("ANTHROPIC_BASE_URL", "http://127.0.0.1:9");
    std::fs::write(cli.join("leave"), "").unwrap();
    let hub = hub(dir.path(), bin).await;
    let api = turn_ended(
        &hub,
        dir.path(),
        "fix the login bug\nI fixed the login bug.",
    )
    .await;
    let r = hub
        .suggest_next("api", 1, &m("haiku"), &tui())
        .await
        .unwrap();
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
    assert!(!env.contains("ANTHROPIC_"), "{env}");
    // An `@/path` in the reply inlines no file.
    assert!(
        env.lines()
            .any(|l| l == "CLAUDE_CODE_DISABLE_ATTACHMENTS=1"),
        "{env}"
    );
    let left: i32 = std::fs::read_to_string(cli.join("left"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // SAFETY: signal 0 checks the process exists; nothing is sent.
    let alive = || unsafe { libc::kill(left, 0) } == 0 && !zombie(left);
    for _ in 0..50 {
        if !alive() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !alive(),
        "what the CLI left running ({left}) outlived the call"
    );
    // Asked again (another TUI): the same answer, no second run.
    let again = hub
        .suggest_next("api", 1, &m("haiku"), &tui())
        .await
        .unwrap();
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
    let r = hub
        .suggest_next("api", 1, &m("haiku"), &tui())
        .await
        .unwrap();
    assert_eq!(
        (r["suggestion"].is_null(), r["reason"].as_str()),
        (true, Some("none")),
        "{r}"
    );
    let e = hub
        .suggest_next("api", 1, &m("--dangerously-skip-permissions"), &tui())
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST);
    let e = hub
        .suggest_next("api", 1, &m("haiku"), &Caller::Orchestrator)
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::UNAUTHORIZED);
    let e = hub
        .suggest_next("api", 9, &m("haiku"), &tui())
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::NOT_FOUND);
    // A running turn.
    hub.send_by("api", "!sleep 5000", &tui()).unwrap();
    let r = hub
        .suggest_next("api", 2, &m("haiku"), &tui())
        .await
        .unwrap();
    assert_eq!(r["reason"], "running", "{r}");
    assert_eq!(runs(&cli), 1);
    hub.stop_thread("api", "test").unwrap();
    hub.shutdown().await;
}

/// A failed call (a usage limit) says why, and none follows for a while.
#[tokio::test]
async fn a_failure_holds_suggestions_back() {
    let dir = tempfile::tempdir().unwrap();
    let cli = dir.path().join("cli");
    std::fs::create_dir_all(&cli).unwrap();
    let bin = fake_claude(&cli);
    answer(&cli, "You've hit your limit · resets 3pm", true);
    let hub = hub(dir.path(), bin).await;
    let api = turn_ended(&hub, dir.path(), "one").await;
    let r = hub
        .suggest_next("api", 1, &m("haiku"), &tui())
        .await
        .unwrap();
    assert_eq!(
        r["reason"].as_str(),
        Some("failed: You've hit your limit · resets 3pm"),
        "{r}"
    );
    assert_eq!(r["stats"]["failures"], 1);
    let mut sub = hub.subscribe_control(0);
    hub.send_by(&api, "two", &tui()).unwrap();
    until_ended(&mut sub, &api, 2).await;
    answer(&cli, "fine now", false);
    let r = hub
        .suggest_next("api", 2, &m("haiku"), &tui())
        .await
        .unwrap();
    assert_eq!(r["reason"], "backoff", "{r}");
    assert_eq!(runs(&cli), 1);
    hub.shutdown().await;
}

/// A hub notice turn of the orchestrator (other threads' reports and items) is no
/// conversation with the user: nothing of it goes out for a suggestion.
#[tokio::test]
async fn a_notice_turn_gets_no_suggestion() {
    let dir = tempfile::tempdir().unwrap();
    let cli = dir.path().join("cli");
    std::fs::create_dir_all(&cli).unwrap();
    let bin = fake_claude(&cli);
    answer(&cli, "check the report", false);
    let mut config = Config {
        default_runtime: "fake".into(),
        ..Default::default()
    };
    config.orchestrator.runtime = Some("fake".into());
    config.orchestrator.cwd = Some(dir.path().to_path_buf());
    let mut o = HubOptions::new(dir.path().join("data").join("twr.db"), config);
    o.adapters = Arc::new(|name: &str| -> Option<Arc<dyn Adapter>> {
        match name {
            "fake" => Some(Arc::new(FakeAdapter::new())),
            _ => None,
        }
    });
    o.socket = Some(dir.path().join("hub.sock"));
    o.stop_timeout = Duration::from_secs(5);
    o.suggest_bin = Some(bin);
    let hub = Hub::open(o).await.unwrap();
    let mut sub = hub.subscribe_control(0);
    let r = hub.say_to_orchestrator("hello", None, &tui()).unwrap();
    let orch = r["thread"].as_str().unwrap().to_string();
    until_ended(&mut sub, &orch, 1).await;
    // A worker's failed turn: an error item, a notice turn for the orchestrator.
    turn_ended(&hub, dir.path(), "!error").await;
    until_ended(&mut sub, &orch, 2).await;
    let (_, log) = hub.thread_events(&orch, 0, 10_000).unwrap();
    let notice = log.iter().any(|e| {
        e.turn == Some(2)
            && e.kind() == "turn.started"
            && e.body
                .data()
                .is_ok_and(|d| d["ext"]["twr"]["notice"] == true)
    });
    assert!(notice, "turn 2 is a notice");
    let r = hub
        .suggest_next(&orch, 2, &m("haiku"), &tui())
        .await
        .unwrap();
    assert_eq!(
        (r["suggestion"].is_null(), r["reason"].as_str()),
        (true, Some("notice")),
        "{r}"
    );
    assert_eq!(runs(&cli), 0);
    hub.shutdown().await;
}

/// Two TUIs asking at once share one call: neither is turned away by the rate limit.
#[tokio::test]
async fn asks_at_once_share_one_call() {
    let dir = tempfile::tempdir().unwrap();
    let cli = dir.path().join("cli");
    std::fs::create_dir_all(&cli).unwrap();
    let bin = fake_claude(&cli);
    answer(&cli, "deploy it", false);
    let hub = hub(dir.path(), bin).await;
    turn_ended(&hub, dir.path(), "build it").await;
    let (one, two) = (tui(), Caller::user("tui2"));
    let models = m("haiku");
    let (a, b) = tokio::join!(
        hub.suggest_next("api", 1, &models, &one),
        hub.suggest_next("api", 1, &models, &two)
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_eq!(a["suggestion"], "deploy it", "{a}");
    assert_eq!(b["suggestion"], "deploy it", "{b}");
    assert_eq!(runs(&cli), 1);
    hub.shutdown().await;
}

/// A suggestion the hub would read as the user's command (an answer or a mode change, in any
/// of its words and languages) is dropped: it would be one Tab and Enter from applying.
#[tokio::test]
async fn a_suggestion_that_reads_as_a_command_is_dropped() {
    for (n, text) in ["set all to bypass", "dale a ambos", "approve both"]
        .into_iter()
        .enumerate()
    {
        let dir = tempfile::tempdir().unwrap();
        let cli = dir.path().join("cli");
        std::fs::create_dir_all(&cli).unwrap();
        let bin = fake_claude(&cli);
        answer(&cli, text, false);
        let hub = hub(dir.path(), bin).await;
        turn_ended(&hub, dir.path(), "go").await;
        let r = hub
            .suggest_next("api", 1, &m("haiku"), &tui())
            .await
            .unwrap();
        assert_eq!(
            (r["suggestion"].is_null(), r["reason"].as_str()),
            (true, Some("none")),
            "{n}: {text}: {r}"
        );
        hub.shutdown().await;
    }
}

/// Only a turn the user's own message started is asked about: after one the orchestrator's
/// message started, or a shared `!!` command's output (the hub's prompt, the runtime's turn),
/// there is no suggestion (`not_user`) and the CLI is not run.
#[tokio::test]
async fn a_turn_the_user_did_not_start_gets_no_suggestion() {
    let dir = tempfile::tempdir().unwrap();
    let cli = dir.path().join("cli");
    std::fs::create_dir_all(&cli).unwrap();
    let bin = fake_claude(&cli);
    answer(&cli, "Now run the integration tests", false);
    let hub = hub(dir.path(), bin).await;
    let project = dir.path().join("proj");
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
    let api = v["id"].as_str().unwrap().to_string();
    let mut sub = hub.subscribe_control(0);
    hub.send_by(&api, "fix the login bug", &Caller::Orchestrator)
        .unwrap();
    until_ended(&mut sub, &api, 1).await;
    let shared = ShellRun {
        thread: Some(api.clone()),
        command: "echo the tests pass".into(),
        cwd: None,
        share: true,
    };
    hub.shell_run(shared, &tui()).unwrap();
    until_ended(&mut sub, &api, 2).await;
    for turn in [1, 2] {
        let r = hub
            .suggest_next("api", turn, &m("haiku"), &tui())
            .await
            .unwrap();
        assert_eq!(
            (r["suggestion"].is_null(), r["reason"].as_str()),
            (true, Some("not_user")),
            "{turn}: {r}"
        );
    }
    assert_eq!(runs(&cli), 0);
    hub.shutdown().await;
}

/// No Claude Code to ask: a CLI that isn't there, or that says it has no login (as Claude
/// Code 2.1.304 does without one, or with a refused key). One call per ask: that ask gets
/// nothing (`retry: ...`), Codex may answer the next; with no Codex either, `unavailable:
/// ...` with both reasons, for the TUI to stop asking, and `hub.info` says so. Turned on
/// again (`retry`), what the calls found is tried again.
#[tokio::test]
async fn a_missing_or_signed_out_cli_is_unavailable() {
    for (case, want) in [
        ("missing", "Claude Code's CLI ("),
        (
            "Not logged in · Please run /login",
            "Claude Code is not signed in",
        ),
        (
            "Invalid API key · Please run /login",
            "Claude Code is not signed in",
        ),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let cli = dir.path().join("cli");
        std::fs::create_dir_all(&cli).unwrap();
        let bin = if case == "missing" {
            cli.join("no-such-claude")
        } else {
            let bin = fake_claude(&cli);
            answer(&cli, case, true);
            bin
        };
        let hub = hub(dir.path(), bin).await;
        assert!(hub.info()["suggest"]["unavailable"].is_null());
        let api = turn_ended(&hub, dir.path(), "one").await;
        let r = hub
            .suggest_next("api", 1, &m("haiku"), &tui())
            .await
            .unwrap();
        let reason = r["reason"].as_str().unwrap_or_default();
        assert!(reason.starts_with(&format!("retry: {want}")), "{case}: {r}");
        assert!(r["suggestion"].is_null(), "{r}");
        assert_eq!(r["stats"]["failures"], 1, "{r}");
        assert!(hub.info()["suggest"]["unavailable"].is_null());
        // The next: Codex, missing here too. Then none can, and `hub.info` says so.
        let mut sub = hub.subscribe_control(0);
        hub.send_by(&api, "two", &tui()).unwrap();
        until_ended(&mut sub, &api, 2).await;
        // Calls at least 3 seconds apart (`hub::suggest::MIN_GAP`).
        tokio::time::sleep(Duration::from_millis(3100)).await;
        let r = hub
            .suggest_next("api", 2, &m("haiku"), &tui())
            .await
            .unwrap();
        let reason = r["reason"].as_str().unwrap_or_default();
        assert!(
            reason.starts_with(&format!("unavailable: {want}")),
            "{case}: {r}"
        );
        assert!(reason.contains("; Codex's CLI ("), "{case}: {r}");
        let info = hub.info()["suggest"]["unavailable"]
            .as_str()
            .map(str::to_string);
        assert_eq!(info.as_deref(), Some(reason), "{case}");
        let claude_runs = usize::from(case != "missing");
        assert_eq!(runs(&cli), claude_runs);
        // Turned on again: tried again.
        let ask = twr_hub::SuggestAsk {
            models: m("haiku"),
            claude_only: false,
            retry: true,
        };
        tokio::time::sleep(Duration::from_millis(3100)).await;
        let r = hub.suggest_ask("api", 2, &ask, &tui()).await.unwrap();
        let reason = r["reason"].as_str().unwrap_or_default();
        assert!(reason.starts_with("retry: "), "{case}: {r}");
        assert_eq!(runs(&cli), 2 * claude_runs);
        hub.shutdown().await;
    }
}

/// The stand-in Codex CLI in `dir`'s `codex-cli` ([`hub_probing`]): each run appends to
/// `runs`, writes `argv`, `stdin`, `cwd` and `env`, then prints `answer.jsonl`.
fn fake_codex(dir: &Path) -> PathBuf {
    let cli = dir.join("codex-cli");
    std::fs::create_dir_all(&cli).unwrap();
    let bin = cli.join("codex");
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
cat "$here/answer.jsonl"
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    cli
}

/// What `codex exec --json` prints for a turn whose last message is `text`.
fn codex_answer(cli: &Path, lines: &[serde_json::Value]) {
    let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    std::fs::write(cli.join("answer.jsonl"), text.join("\n") + "\n").unwrap();
}

fn codex_said(text: &str) -> Vec<serde_json::Value> {
    use serde_json::json;
    vec![
        json!({"type": "thread.started", "thread_id": "t"}),
        json!({"type": "turn.started"}),
        json!({"type": "item.completed", "item": {"id": "item_1", "type": "agent_message", "text": text}}),
        json!({"type": "turn.completed", "usage": {"input_tokens": 5153, "cached_input_tokens": 3456, "output_tokens": 5}}),
    ]
}

fn probe(
    runtime: &str,
    found: bool,
    login: twr_core::runner::AgentLogin,
) -> twr_core::runner::AgentInfo {
    twr_core::runner::AgentInfo {
        runtime: runtime.into(),
        found,
        login,
        ..Default::default()
    }
}

/// Codex answers when it is the one that can run: `codex exec` once, with no session, none of
/// the user's config, rules or project docs, a read-only sandbox that asks nothing, every tool
/// feature off, twr's prompt as the instructions and the small model asked for, in the
/// private folder, with the user's login only (no `OPENAI_*` from the hub's environment).
#[tokio::test]
async fn codex_answers_with_its_small_model_and_nothing_of_the_users() {
    use twr_core::runner::AgentLogin;
    let dir = tempfile::tempdir().unwrap();
    let cli = fake_codex(dir.path());
    codex_answer(&cli, &codex_said("Now run the integration tests"));
    std::env::set_var("OPENAI_API_KEY", "sk-sentinel");
    let hub = hub_probing(
        dir.path(),
        dir.path().join("no-claude"),
        Some(vec![
            probe("claude-code", false, AgentLogin::Unknown),
            probe("codex", true, AgentLogin::LoggedIn),
        ]),
    )
    .await;
    timeout(WAIT, async {
        while hub.info()["agents"].as_array().is_none_or(Vec::is_empty) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    assert!(hub.info()["suggest"]["unavailable"].is_null());
    let api = turn_ended(&hub, dir.path(), "fix the login bug").await;
    let r = hub
        .suggest_next("api", 1, &m("haiku"), &tui())
        .await
        .unwrap();
    assert_eq!(r["suggestion"], "Now run the integration tests", "{r}");
    assert_eq!(
        (r["runtime"].as_str(), r["model"].as_str()),
        (Some("codex"), Some("gpt-6-luna"))
    );
    // GPT-6-Luna at list price: 1,697 input, 3,456 cached, 5 output.
    let cost = r["stats"]["cost_usd"].as_f64().unwrap();
    assert!((cost - 0.000_206_76).abs() < 1e-12, "{cost}");
    let argv: Vec<String> = std::fs::read_to_string(cli.join("argv"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    let has = |a: &str| argv.iter().any(|x| x == a);
    let after = |flag: &str| {
        let at = argv
            .iter()
            .position(|a| a == flag)
            .unwrap_or_else(|| panic!("no {flag}"));
        argv[at + 1].clone()
    };
    assert_eq!(argv[0], "exec");
    for flag in [
        "--ephemeral",
        "--ignore-user-config",
        "--ignore-rules",
        "--skip-git-repo-check",
        "--json",
    ] {
        assert!(has(flag), "{flag} in {argv:?}");
    }
    assert_eq!(after("-s"), "read-only");
    assert_eq!(after("-m"), "gpt-6-luna");
    for c in [
        "model_reasoning_effort=\"low\"",
        "approval_policy=\"never\"",
        "web_search=\"disabled\"",
        "project_doc_max_bytes=0",
        "features.shell_tool=false",
        "features.unified_exec=false",
        "features.multi_agent=false",
        "features.plugins=false",
        "skills.include_instructions=false",
    ] {
        assert!(has(c), "{c} in {argv:?}");
    }
    let instructions = argv
        .iter()
        .find_map(|a| a.strip_prefix("model_instructions_file="))
        .unwrap();
    let file: String = serde_json::from_str(instructions).unwrap();
    assert!(std::fs::read_to_string(&file)
        .unwrap()
        .contains("reply exactly NONE"));
    assert_eq!(argv.last().map(String::as_str), Some("-"));
    let stdin = std::fs::read_to_string(cli.join("stdin")).unwrap();
    assert!(
        stdin.contains("<user_message>\nfix the login bug\n</user_message>"),
        "{stdin}"
    );
    let cwd = std::fs::read_to_string(cli.join("cwd")).unwrap();
    assert!(cwd.trim_end().ends_with("data/suggest"), "{cwd}");
    let env = std::fs::read_to_string(cli.join("env")).unwrap();
    assert!(!env.contains("OPENAI_API_KEY"), "{env}");
    assert!(
        env.lines().any(|l| l == format!("TWR_AGENT={api}:suggest")),
        "{env}"
    );
    hub.shutdown().await;
}

/// A call in which the model reaches for any tool is stopped at once: no suggestion, and the
/// runtime is held back as after any failure.
#[tokio::test]
async fn a_codex_call_that_uses_a_tool_is_stopped() {
    use serde_json::json;
    use twr_core::runner::AgentLogin;
    let dir = tempfile::tempdir().unwrap();
    let cli = fake_codex(dir.path());
    codex_answer(
        &cli,
        &[
            json!({"type": "turn.started"}),
            json!({"type": "item.started", "item": {"id": "i", "type": "command_execution", "command": "cat ~/.ssh/id_ed25519"}}),
            json!({"type": "item.completed", "item": {"id": "j", "type": "agent_message", "text": "a key"}}),
            json!({"type": "turn.completed", "usage": {}}),
        ],
    );
    let hub = hub_probing(
        dir.path(),
        dir.path().join("no-claude"),
        Some(vec![
            probe("claude-code", false, AgentLogin::Unknown),
            probe("codex", true, AgentLogin::LoggedIn),
        ]),
    )
    .await;
    timeout(WAIT, async {
        while hub.info()["agents"].as_array().is_none_or(Vec::is_empty) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    turn_ended(&hub, dir.path(), "one").await;
    let r = hub
        .suggest_next("api", 1, &m("haiku"), &tui())
        .await
        .unwrap();
    assert!(r["suggestion"].is_null(), "{r}");
    let reason = r["reason"].as_str().unwrap();
    assert!(
        reason.starts_with("failed: the model tried to use a tool (command_execution)"),
        "{r}"
    );
    hub.shutdown().await;
}

/// Claude Code first for a thread of another runtime; when its call says it has no login,
/// that ask gets nothing (`retry: ...`), and Codex answers the next one: never in the same
/// ask, which the user may have turned off meanwhile. Codex without a login (a 401) is
/// `unavailable` too.
#[tokio::test]
async fn codex_stands_in_when_claude_code_cannot_run() {
    let dir = tempfile::tempdir().unwrap();
    let claude = dir.path().join("cli");
    std::fs::create_dir_all(&claude).unwrap();
    let bin = fake_claude(&claude);
    answer(&claude, "Not logged in · Please run /login", true);
    let cli = fake_codex(dir.path());
    codex_answer(&cli, &codex_said("and the docs"));
    let hub = hub(dir.path(), bin).await;
    let api = turn_ended(&hub, dir.path(), "one").await;
    let r = hub
        .suggest_next("api", 1, &m("haiku"), &tui())
        .await
        .unwrap();
    assert_eq!(r["reason"], "retry: Claude Code is not signed in", "{r}");
    assert_eq!((runs(&claude), runs(&cli)), (1, 0));
    let mut sub = hub.subscribe_control(0);
    hub.send_by(&api, "two", &tui()).unwrap();
    until_ended(&mut sub, &api, 2).await;
    // Calls at least 3 seconds apart (`hub::suggest::MIN_GAP`).
    tokio::time::sleep(Duration::from_millis(3100)).await;
    let r = hub
        .suggest_next("api", 2, &m("haiku"), &tui())
        .await
        .unwrap();
    assert_eq!(r["suggestion"], "and the docs", "{r}");
    assert_eq!(r["runtime"], "codex");
    assert_eq!((runs(&claude), runs(&cli)), (1, 1));
    // Codex signs out too.
    codex_answer(
        &cli,
        &[
            serde_json::json!({"type": "turn.failed", "error": {"message": "unexpected status 401 Unauthorized: Missing bearer or basic authentication in header"}}),
        ],
    );
    hub.send_by(&api, "three", &tui()).unwrap();
    until_ended(&mut sub, &api, 3).await;
    tokio::time::sleep(Duration::from_millis(3100)).await;
    let r = hub
        .suggest_next("api", 3, &m("haiku"), &tui())
        .await
        .unwrap();
    assert_eq!(runs(&claude), 1, "Claude Code left out");
    assert_eq!(
        r["reason"].as_str(),
        Some("unavailable: Claude Code is not signed in; Codex is not signed in"),
        "{r}"
    );
    hub.shutdown().await;
}

/// A client from before Codex answered suggestions (`model` alone) told its user of
/// Anthropic only: Claude Code alone answers it, and without it nothing goes to OpenAI.
#[tokio::test]
async fn a_claude_only_ask_never_reaches_codex() {
    let dir = tempfile::tempdir().unwrap();
    let cli = fake_codex(dir.path());
    codex_answer(&cli, &codex_said("and the docs"));
    let hub = hub(dir.path(), dir.path().join("no-claude")).await;
    turn_ended(&hub, dir.path(), "one").await;
    let ask = twr_hub::SuggestAsk {
        models: m("haiku"),
        claude_only: true,
        retry: false,
    };
    let r = hub.suggest_ask("api", 1, &ask, &tui()).await.unwrap();
    let reason = r["reason"].as_str().unwrap_or_default();
    assert!(
        reason.starts_with("unavailable: Claude Code's CLI ("),
        "{r}"
    );
    assert!(!reason.contains("Codex"), "{r}");
    let r = hub.suggest_ask("api", 1, &ask, &tui()).await.unwrap();
    assert!(
        r["reason"]
            .as_str()
            .unwrap_or_default()
            .starts_with("unavailable: "),
        "{r}"
    );
    assert_eq!(runs(&cli), 0);
    hub.shutdown().await;
}

/// Codex sends the user's global `AGENTS.md` with every request, and has no setting to leave
/// it out: while there is one, Codex answers no suggestion.
#[tokio::test]
async fn codex_with_global_instructions_answers_none() {
    use twr_core::runner::AgentLogin;
    let dir = tempfile::tempdir().unwrap();
    let cli = fake_codex(dir.path());
    codex_answer(&cli, &codex_said("and the docs"));
    let home = dir.path().join("codex-home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("AGENTS.md"), "my private rules").unwrap();
    let hub = hub_probing(
        dir.path(),
        dir.path().join("no-claude"),
        Some(vec![
            probe("claude-code", false, AgentLogin::Unknown),
            probe("codex", true, AgentLogin::LoggedIn),
        ]),
    )
    .await;
    timeout(WAIT, async {
        while hub.info()["agents"].as_array().is_none_or(Vec::is_empty) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    turn_ended(&hub, dir.path(), "one").await;
    let r = hub
        .suggest_next("api", 1, &m("haiku"), &tui())
        .await
        .unwrap();
    assert_eq!(
        r["reason"].as_str(),
        Some("unavailable: Claude Code's CLI is not installed; Codex would send your global AGENTS.md along"),
        "{r}"
    );
    assert_eq!(runs(&cli), 0);
    hub.shutdown().await;
}

/// Only small models answer: one that looks large is never asked, and no other runtime
/// answers instead (no other provider stands in for a setting); the reason says why.
#[tokio::test]
async fn large_models_are_never_asked() {
    let dir = tempfile::tempdir().unwrap();
    let claude = dir.path().join("cli");
    std::fs::create_dir_all(&claude).unwrap();
    let bin = fake_claude(&claude);
    answer(&claude, "run the tests", false);
    let cli = fake_codex(dir.path());
    codex_answer(&cli, &codex_said("and the docs"));
    let hub = hub(dir.path(), bin).await;
    turn_ended(&hub, dir.path(), "one").await;
    let both = twr_core::config::SuggestModels {
        claude_code: "claude-opus-5-5".into(),
        codex: "gpt-6.1-sol".into(),
    };
    for models in [both, m("sonnet")] {
        let r = hub.suggest_next("api", 1, &models, &tui()).await.unwrap();
        let reason = r["reason"].as_str().unwrap();
        assert!(
            reason.starts_with("failed: ") && reason.contains("large model"),
            "{r}"
        );
        assert!(reason.contains(&format!("{:?}", models.claude_code)), "{r}");
        assert_eq!((runs(&claude), runs(&cli)), (0, 0));
    }
    hub.shutdown().await;
}

/// The hub's own agent probe decides before any call: no runtime that can run, nothing is run
/// at all, and `hub.info` says so (`suggest.unavailable`) for a TUI to stay quiet without
/// asking. One that can (or nothing probed): `hub.info` says nothing.
#[tokio::test]
async fn the_probe_says_when_suggestions_cannot_run() {
    use twr_core::runner::AgentLogin::{LoggedIn, LoggedOut, Unknown};
    let cases = [
        (
            vec![
                probe("claude-code", false, Unknown),
                probe("codex", false, Unknown),
            ],
            Some("unavailable: Claude Code's CLI is not installed; Codex's CLI is not installed"),
        ),
        (
            vec![
                probe("claude-code", true, LoggedOut),
                probe("codex", true, LoggedOut),
            ],
            Some("unavailable: Claude Code is not signed in; Codex is not signed in"),
        ),
        (
            vec![
                probe("claude-code", true, LoggedIn),
                probe("codex", false, Unknown),
            ],
            None,
        ),
    ];
    for (agents, want) in cases {
        let dir = tempfile::tempdir().unwrap();
        let cli = dir.path().join("cli");
        std::fs::create_dir_all(&cli).unwrap();
        let bin = fake_claude(&cli);
        answer(&cli, "run the tests", false);
        let hub = hub_probing(dir.path(), bin, Some(agents)).await;
        // The probe runs in the background once the hub is open.
        timeout(WAIT, async {
            while hub.info()["agents"].as_array().is_none_or(Vec::is_empty) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(hub.info()["suggest"]["unavailable"].as_str(), want);
        turn_ended(&hub, dir.path(), "one").await;
        let r = hub
            .suggest_next("api", 1, &m("haiku"), &tui())
            .await
            .unwrap();
        match want {
            Some(why) => {
                assert_eq!(r["reason"].as_str(), Some(why), "{r}");
                assert_eq!(runs(&cli), 0, "nothing runs");
            }
            None => {
                assert_eq!(r["suggestion"], "run the tests", "{r}");
                assert_eq!(runs(&cli), 1);
            }
        }
        hub.shutdown().await;
    }
    // Without a probe (and in tests), nothing is known: `hub.info` says nothing.
    let dir = tempfile::tempdir().unwrap();
    let hub = hub(dir.path(), dir.path().join("claude")).await;
    assert!(hub.info()["suggest"]["unavailable"].is_null());
    hub.shutdown().await;
}
