//! Thread summaries (`summary.thread`, `hub::summary`): what the CLI reads (routine updates,
//! state and activity, never an item, a prompt or a message), the summary kept for an
//! unchanged basis and for the interval, the hourly cap, the backoff after a failure, the
//! runtimes found unable to run shared with suggestions, refusals, and a call whose client
//! hangs up.
//!
//! The CLI is a script standing in for `claude`, as in `tests/suggest.rs`: it records its
//! arguments, input, folder and environment beside itself and answers what `answer.json`
//! there says; with a `slow` file there, it writes its pid to `pid` and waits first, until the
//! file goes (30 s at most).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::time::timeout;
use twr_core::config::{Config, SuggestModels};
use twr_core::event::*;
use twr_core::proto::{codes, SummaryThreadParams, SummaryThreadResult};
use twr_hub::{Caller, Hub, HubOptions};

const WAIT: Duration = Duration::from_secs(10);

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
if [ -f "$here/slow" ]; then
  echo $$ > "$here/pid"
  n=0
  while [ -f "$here/slow" ] && [ $n -lt 600 ]; do sleep 0.05; n=$((n + 1)); done
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
    let v = json!({"type": "result", "is_error": error, "result": result,
                   "total_cost_usd": 0.0002});
    std::fs::write(dir.join("answer.json"), v.to_string()).unwrap();
}

fn runs(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join("runs"))
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

/// A hub with an orchestrator conversation (`o1`, stopped: no notice turn runs) and a worker
/// `api`, whose Claude Code is `bin` and whose Codex is a path that doesn't exist.
async fn start(dir: &Path, bin: PathBuf) -> Hub {
    let mut o = HubOptions::new(dir.join("data").join("twr.db"), Config::default());
    o.socket = Some(dir.join("hub.sock"));
    o.stop_timeout = Duration::from_secs(5);
    o.suggest_bin = Some(bin);
    o.suggest_codex_bin = Some(dir.join("codex-cli").join("codex"));
    o.suggest_codex_home = Some(dir.join("codex-home"));
    // Nothing the hub does on its own meanwhile: no wake of a worker, no stall.
    o.wake_after = Some(Duration::ZERO);
    o.stall_after = Some(Duration::ZERO);
    let hub = Hub::open(o).await.unwrap();
    thread(&hub, "o1", "orchestrator", json!({"role": "orchestrator"}));
    hub.append(
        "o1",
        None,
        Source::Hub,
        EventBody::ThreadStopped(Default::default()),
        None,
    )
    .unwrap();
    thread(&hub, "api", "api", json!({}));
    hub
}

fn thread(hub: &Hub, id: &str, name: &str, twr: Value) {
    let mut ext = JsonMap::new();
    ext.insert("twr".into(), twr);
    hub.append(
        id,
        None,
        Source::Hub,
        EventBody::ThreadCreated(ThreadCreated {
            name: name.into(),
            box_name: "local".into(),
            runtime: "fake".into(),
            ext: Some(ext),
            ..Default::default()
        }),
        None,
    )
    .unwrap();
}

fn on(hub: &Hub, thread: &str, turn: Option<u64>, body: EventBody) {
    hub.append(thread, turn, Source::Hub, body, None).unwrap();
}

/// Worker `thread`'s turn `turn` on `prompt` yields with a background task running: its interim
/// report says `summary` (a routine update).
fn interim(hub: &Hub, thread: &str, turn: u64, prompt: &str, summary: &str) {
    on(
        hub,
        thread,
        Some(turn),
        EventBody::TurnStarted(TurnStarted {
            prompt: prompt.into(),
            from: TurnFrom::User,
            ..Default::default()
        }),
    );
    on(hub, thread, None, task(&format!("t{turn}"), true));
    on(
        hub,
        thread,
        Some(turn),
        EventBody::TurnEnded(TurnEnded::default()),
    );
    let mut ext = JsonMap::new();
    ext.insert("twr".into(), json!({"background_tasks": 1}));
    on(
        hub,
        thread,
        Some(turn),
        EventBody::ReportReady(ReportReady {
            summary: summary.into(),
            grade: Grade::Success,
            turn: Some(turn),
            written_by: "hub".into(),
            ext: Some(ext),
            ..Default::default()
        }),
    );
}

fn task(id: &str, running: bool) -> EventBody {
    EventBody::TaskUpdated(TaskUpdated {
        task_id: id.into(),
        running,
        description: "cargo test".into(),
        ..Default::default()
    })
}

/// The task of a `!` command of the user's, `id` on its thread, as the hub records it
/// (`hub::shell`): its first words in its description; `share`: run with `!!`.
fn shell_task(id: &str, running: bool, share: bool) -> EventBody {
    let command = "curl -H 'Authorization: SHELL-SECRET' https://example.com";
    EventBody::TaskUpdated(TaskUpdated {
        task_id: format!("job:{id}"),
        running,
        description: "! curl -H 'Authorization: SHELL-SECRET': running".into(),
        ext: Some(
            json!({"twr": {"job": {"id": id, "name": "curl -H 'Authorization:",
                "wake": share, "shell": {"command": command, "share": share}}}})
            .as_object()
            .unwrap()
            .clone(),
        ),
        ..Default::default()
    })
}

fn raise(hub: &Hub, thread: &str, id: &str, reason: AttentionReason, summary: &str) {
    on(
        hub,
        thread,
        None,
        EventBody::AttentionRaised(AttentionRaised {
            attn_id: id.into(),
            reason,
            summary: summary.into(),
            options: vec!["allow".into(), "deny".into()],
            ..Default::default()
        }),
    );
}

fn resolve(hub: &Hub, thread: &str, id: &str) {
    on(
        hub,
        thread,
        None,
        EventBody::AttentionResolved(AttentionResolved {
            attn_id: id.into(),
            by: ResolvedBy::User,
            answer: Some(Answer {
                decision: "allow SECRET-ANSWER".into(),
                ..Default::default()
            }),
            ..Default::default()
        }),
    );
}

fn ask(thread: &str) -> SummaryThreadParams {
    SummaryThreadParams {
        thread: thread.into(),
        models: None,
        every_secs: 300,
        retry: false,
    }
}

async fn summary(hub: &Hub, p: &SummaryThreadParams) -> SummaryThreadResult {
    let v = hub.summary_thread(p, &Caller::user("tui")).await.unwrap();
    serde_json::from_value(v).unwrap()
}

/// The `seq` of the newest update in the orchestrator's conversation (`o1`) that says `text`.
fn update_seq(dir: &Path, text: &str) -> u64 {
    update_seq_in(dir, "o1", text)
}

/// The `seq` of the newest update in orchestrator thread `conv` that says `text`.
fn update_seq_in(dir: &Path, conv: &str, text: &str) -> u64 {
    let conn = rusqlite::Connection::open(dir.join("data").join("twr.db")).unwrap();
    conn.query_row(
        "SELECT seq FROM events WHERE thread = ?2 AND kind = 'conversation.update'
         AND data LIKE ?1 ORDER BY seq DESC LIMIT 1",
        [format!("%{text}%"), conv.to_string()],
        |r| r.get::<_, i64>(0),
    )
    .map(|seq| u64::try_from(seq).unwrap())
    .unwrap()
}

/// How many events the log has, and how many of them say `text`.
fn events(dir: &Path, text: &str) -> (i64, i64) {
    let conn = rusqlite::Connection::open(dir.join("data").join("twr.db")).unwrap();
    let count = |sql: &str| {
        conn.query_row(sql, [format!("%{text}%")], |r| r.get(0))
            .unwrap()
    };
    (
        count("SELECT COUNT(*) FROM events WHERE ?1 = ?1"),
        count("SELECT COUNT(*) FROM events WHERE data LIKE ?1"),
    )
}

/// The CLI reads the thread's name, state and activity and its routine updates since the last
/// update about it that is not routine: nothing of an item (its text or its answer), and no
/// prompt. It runs once as a suggestion's call does, with a system prompt of its own; the same
/// basis gets the same summary without a call, a new one inside the interval the last summary,
/// and nothing goes into the event log.
#[tokio::test]
async fn a_summary_reads_routine_updates_only_and_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let cli = dir.path().join("cli");
    std::fs::create_dir_all(&cli).unwrap();
    let bin = fake_claude(&cli);
    answer(&cli, "\"Two of three fixed; one left\"\n", false);
    let hub = start(dir.path(), bin).await;
    interim(
        &hub,
        "api",
        1,
        "PROMPT-SECRET fix it",
        "ran 120 tests, 3 failing",
    );
    raise(
        &hub,
        "api",
        "api:p1",
        AttentionReason::Permission,
        "PERMISSION-SECRET rm -rf",
    );
    resolve(&hub, "api", "api:p1");
    raise(
        &hub,
        "api",
        "api:q1",
        AttentionReason::Question,
        "QUESTION-SECRET which db?",
    );
    resolve(&hub, "api", "api:q1");
    raise(
        &hub,
        "api",
        "api:e1",
        AttentionReason::Error,
        "ERROR-SECRET it broke",
    );
    resolve(&hub, "api", "api:e1");
    on(&hub, "api", None, task("t1", false));
    interim(
        &hub,
        "api",
        2,
        "PROMPT-SECRET go on",
        "fixed 2 of 3 failing tests",
    );
    on(&hub, "api", None, task("t2", false));
    let said = "Two of three fixed";
    let before = events(dir.path(), said);
    let r = summary(&hub, &ask("api")).await;
    assert_eq!(
        r.summary.as_deref(),
        Some("Two of three fixed; one left"),
        "{r:?}"
    );
    assert!(!r.cached && r.reason.is_none(), "{r:?}");
    assert_eq!(r.runtime.as_deref(), Some("claude-code"));
    assert_eq!(r.model.as_deref(), Some("claude-haiku-5-5"));
    assert_eq!(r.next_after_secs, Some(300));
    assert_eq!((r.stats.calls, r.stats.failures), (1, 0));
    assert_eq!(r.basis.len(), 16, "{r:?}");
    let newest = update_seq(dir.path(), "api background work finished");
    assert_eq!(
        r.covers,
        Some(newest),
        "the newest routine update read: {r:?}"
    );
    let stdin = std::fs::read_to_string(cli.join("stdin")).unwrap();
    assert!(
        stdin.starts_with("Thread: api\nState: done\nNow: done"),
        "{stdin}"
    );
    assert!(stdin.contains("fixed 2 of 3 failing tests"), "{stdin}");
    assert!(stdin.contains("api background work finished."), "{stdin}");
    assert!(
        !stdin.contains("ran 120 tests"),
        "an update before the last one that is not routine: {stdin}"
    );
    for secret in ["SECRET", "rm -rf", "which db", "it broke"] {
        assert!(!stdin.contains(secret), "{secret}: {stdin}");
    }
    let argv: Vec<String> = std::fs::read_to_string(cli.join("argv"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    let at = |flag: &str| argv.iter().position(|a| a == flag).unwrap();
    assert_eq!(argv[0], "-p");
    assert_eq!(argv[at("--model") + 1], "claude-haiku-5-5");
    assert_eq!(argv[at("--tools") + 1], "");
    assert_eq!(argv[at("--setting-sources") + 1], "");
    let system = &argv[at("--system-prompt") + 1];
    assert!(
        system.contains("where a coding agent's work stands"),
        "{system}"
    );
    assert!(system.contains("reply exactly NONE"), "{system}");
    let cwd = std::fs::read_to_string(cli.join("cwd")).unwrap();
    assert!(cwd.trim_end().ends_with("data/suggest"), "{cwd}");
    let env = std::fs::read_to_string(cli.join("env")).unwrap();
    assert!(env.lines().any(|l| l == "TWR_AGENT=api:summary"), "{env}");
    assert!(!env.contains("TWR_ORCH_TOKEN="));
    // The same basis: the same summary, no call.
    let again = summary(&hub, &ask("api")).await;
    assert_eq!(again.summary, r.summary);
    assert_eq!(
        (again.cached, again.basis.as_str(), again.covers),
        (true, r.basis.as_str(), r.covers)
    );
    assert!(
        again.next_after_secs.is_some_and(|n| n > 0 && n <= 300),
        "{again:?}"
    );
    assert_eq!(runs(&cli), 1);
    // Nothing recorded.
    assert_eq!(events(dir.path(), said), before);
    assert_eq!(before.1, 0);
    // Another routine update: a new basis, but inside the interval the last summary stands.
    interim(&hub, "api", 3, "PROMPT-SECRET once more", "all tests pass");
    let soon = summary(&hub, &ask("api")).await;
    assert_eq!(soon.summary, r.summary);
    assert_ne!(soon.basis, r.basis);
    assert_eq!(
        (soon.cached, soon.reason.as_deref()),
        (true, Some("interval"))
    );
    assert_eq!(
        soon.covers, r.covers,
        "of the updates it read, not the newer one"
    );
    assert!(
        soon.next_after_secs.is_some_and(|n| n > 0 && n <= 300),
        "{soon:?}"
    );
    assert_eq!(runs(&cli), 1);
    assert_eq!(events(dir.path(), said).1, 0);
    hub.shutdown().await;
}

/// A worker's live run is read however many updates about other threads came after it: the
/// updates about the thread are picked out before any limit.
#[tokio::test]
async fn a_run_is_read_after_many_updates_about_other_threads() {
    let dir = tempfile::tempdir().unwrap();
    let cli = dir.path().join("cli");
    std::fs::create_dir_all(&cli).unwrap();
    let bin = fake_claude(&cli);
    answer(&cli, "Tests running", false);
    let hub = start(dir.path(), bin).await;
    thread(&hub, "web", "web", json!({}));
    interim(&hub, "api", 1, "fix it", "ran 120 tests, 3 failing");
    let live = update_seq(dir.path(), "ran 120 tests");
    for i in 0..450 {
        on(
            &hub,
            "o1",
            None,
            EventBody::ConversationUpdate(ConversationUpdate {
                text: format!("web update {i}"),
                about_thread: Some("web".into()),
                ..Default::default()
            }),
        );
    }
    let r = summary(&hub, &ask("api")).await;
    assert_eq!(r.covers, Some(live), "{r:?}");
    assert_eq!(r.summary.as_deref(), Some("Tests running"), "{r:?}");
    let stdin = std::fs::read_to_string(cli.join("stdin")).unwrap();
    assert!(stdin.contains("ran 120 tests, 3 failing"), "{stdin}");
    assert!(!stdin.contains("web update"), "{stdin}");
    hub.shutdown().await;
}

/// A failed turn's report is no routine update, though a task of its runs on after it (graded
/// `gave_up`, as a failed turn is, or not): it ends the run before it, and the summary reads
/// none of either (`covers` none, of the conversation it read).
#[tokio::test]
async fn a_failed_turns_report_is_no_routine_update() {
    for grade in [Grade::GaveUp, Grade::Success] {
        let dir = tempfile::tempdir().unwrap();
        let cli = dir.path().join("cli");
        std::fs::create_dir_all(&cli).unwrap();
        let bin = fake_claude(&cli);
        answer(&cli, "NONE", false);
        let hub = start(dir.path(), bin).await;
        interim(&hub, "api", 1, "go", "started the build");
        on(
            &hub,
            "api",
            Some(2),
            EventBody::TurnStarted(TurnStarted {
                prompt: "again".into(),
                from: TurnFrom::User,
                ..Default::default()
            }),
        );
        on(&hub, "api", None, task("t2", true));
        on(
            &hub,
            "api",
            Some(2),
            EventBody::TurnEnded(TurnEnded {
                result: TurnResult::Error,
                ..Default::default()
            }),
        );
        let mut ext = JsonMap::new();
        ext.insert("twr".into(), json!({"background_tasks": 2}));
        on(
            &hub,
            "api",
            Some(2),
            EventBody::ReportReady(ReportReady {
                summary: "Turn 2 failed.".into(),
                grade,
                turn: Some(2),
                written_by: "hub".into(),
                ext: Some(ext),
                ..Default::default()
            }),
        );
        let (_, log) = hub.thread_events("o1", 0, 1000).unwrap();
        let failed = log
            .iter()
            .rev()
            .find_map(|e| match &e.body {
                EventBody::ConversationUpdate(p) => Some(p.clone()),
                _ => None,
            })
            .unwrap();
        assert!(failed.text.contains("Turn 2 failed."), "{failed:?}");
        assert_eq!(routine_of(&failed.ext), None, "{grade:?}: {failed:?}");
        let twr = failed.ext.as_ref().unwrap()["twr"].as_object().unwrap();
        assert_eq!(twr.get("routine"), Some(&Value::Null), "{failed:?}");
        let r = summary(&hub, &ask("api")).await;
        assert_eq!(
            (r.covers, r.covers_in.as_deref()),
            (None, Some("o1")),
            "{r:?}"
        );
        let stdin = std::fs::read_to_string(cli.join("stdin")).unwrap();
        assert!(stdin.contains("No updates about it"), "{stdin}");
        assert!(!stdin.contains("<hub_updates"), "{stdin}");
        assert!(!stdin.contains("started the build"), "{stdin}");
        hub.shutdown().await;
    }
}

/// A rewrite (`report.write`) of an older failed turn's report while that turn's task still
/// runs is no routine update either, graded what it may be: the failure is its turn's, not
/// the worker's last turn's (a later one that completed).
#[tokio::test]
async fn a_rewritten_older_failed_turns_report_is_no_routine_update() {
    let dir = tempfile::tempdir().unwrap();
    let cli = dir.path().join("cli");
    std::fs::create_dir_all(&cli).unwrap();
    let hub = start(dir.path(), fake_claude(&cli)).await;
    on(
        &hub,
        "api",
        Some(1),
        EventBody::TurnStarted(TurnStarted {
            prompt: "go".into(),
            from: TurnFrom::User,
            ..Default::default()
        }),
    );
    on(&hub, "api", None, task("t1", true));
    on(
        &hub,
        "api",
        Some(1),
        EventBody::TurnEnded(TurnEnded {
            result: TurnResult::Error,
            ..Default::default()
        }),
    );
    let mut ext = JsonMap::new();
    ext.insert("twr".into(), json!({"background_tasks": 1}));
    on(
        &hub,
        "api",
        Some(1),
        EventBody::ReportReady(ReportReady {
            summary: "Turn 1 failed.".into(),
            grade: Grade::GaveUp,
            turn: Some(1),
            written_by: "hub".into(),
            ext: Some(ext),
            ..Default::default()
        }),
    );
    on(
        &hub,
        "api",
        Some(2),
        EventBody::TurnStarted(TurnStarted {
            prompt: "again".into(),
            from: TurnFrom::User,
            ..Default::default()
        }),
    );
    on(
        &hub,
        "api",
        Some(2),
        EventBody::TurnEnded(TurnEnded::default()),
    );
    let written = hub
        .write_report(
            "api",
            Some(1),
            "Turn 1 did most of it.",
            Grade::Success,
            &Caller::Orchestrator,
        )
        .unwrap();
    assert_eq!(written["turn"], 1, "{written}");
    let (_, log) = hub.thread_events("o1", 0, 1000).unwrap();
    let rewrite = log
        .iter()
        .rev()
        .find_map(|e| match &e.body {
            EventBody::ConversationUpdate(p) if p.text.contains("Turn 1 did most") => {
                Some(p.clone())
            }
            _ => None,
        })
        .unwrap();
    assert!(rewrite.text.contains("background task"), "{rewrite:?}");
    assert_eq!(routine_of(&rewrite.ext), None, "{rewrite:?}");
    let twr = rewrite.ext.as_ref().unwrap()["twr"].as_object().unwrap();
    assert_eq!(twr.get("routine"), Some(&Value::Null), "{rewrite:?}");
    hub.shutdown().await;
}

/// A thread at work on the user's prompt with requests open: its state goes, its prompt and
/// the requests' text don't (nor what the open items make its activity line say).
#[tokio::test]
async fn open_items_and_the_prompt_never_reach_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let cli = dir.path().join("cli");
    std::fs::create_dir_all(&cli).unwrap();
    let bin = fake_claude(&cli);
    answer(&cli, "NONE", false);
    let hub = start(dir.path(), bin).await;
    interim(&hub, "api", 1, "PROMPT-SECRET one", "first pass done");
    on(
        &hub,
        "api",
        Some(2),
        EventBody::TurnStarted(TurnStarted {
            prompt: "PROMPT-SECRET deploy to prod".into(),
            from: TurnFrom::User,
            ..Default::default()
        }),
    );
    raise(
        &hub,
        "api",
        "api:p1",
        AttentionReason::Permission,
        "PERMISSION-SECRET deploy",
    );
    raise(
        &hub,
        "api",
        "api:q1",
        AttentionReason::Question,
        "QUESTION-SECRET which?",
    );
    raise(
        &hub,
        "api",
        "api:s1",
        AttentionReason::Stalled,
        "STALL-SECRET no activity",
    );
    let r = summary(&hub, &ask("api")).await;
    assert_eq!(
        (r.summary.as_deref(), r.reason.as_deref()),
        (None, Some("none")),
        "{r:?}"
    );
    let stdin = std::fs::read_to_string(cli.join("stdin")).unwrap();
    assert!(stdin.contains("State: running"), "{stdin}");
    assert!(!stdin.contains("SECRET"), "{stdin}");
    assert!(!stdin.contains("deploy"), "{stdin}");
    hub.shutdown().await;
}

/// The user's `!` commands on a thread are theirs, shared (`!!`) or not: no word of one reaches
/// the CLI, nor that the thread waits on one (its state and activity are as without it), on a
/// thread doing nothing else or beside the thread's own background work; and the end of one
/// adds no update (the end of the thread's own work does).
#[tokio::test]
async fn the_users_shell_commands_never_reach_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let cli = dir.path().join("cli");
    std::fs::create_dir_all(&cli).unwrap();
    let bin = fake_claude(&cli);
    answer(&cli, "busy", false);
    let hub = start(dir.path(), bin).await;
    let stdin = || std::fs::read_to_string(cli.join("stdin")).unwrap();
    let clean = |stdin: &str| {
        for word in ["SHELL-SECRET", "curl", "Authorization", "example.com", "!"] {
            assert!(!stdin.contains(word), "{word}: {stdin}");
        }
    };
    // A thread with nothing on but a `!` command.
    on(&hub, "api", None, shell_task("s1", true, false));
    let r = summary(&hub, &ask("api")).await;
    assert_eq!(r.summary.as_deref(), Some("busy"), "{r:?}");
    let s = stdin();
    clean(&s);
    assert!(s.starts_with("Thread: api\nState: idle\n"), "{s}");
    assert!(!s.contains("background"), "{s}");
    // Beside its own background work: that one only.
    thread(&hub, "web", "web", json!({}));
    interim(&hub, "web", 1, "go", "half the pages built");
    on(&hub, "web", None, shell_task("s2", true, false));
    on(&hub, "web", None, shell_task("s3", true, true));
    let r = summary(&hub, &ask("web")).await;
    assert_eq!(r.summary.as_deref(), Some("busy"), "{r:?}");
    let s = stdin();
    clean(&s);
    assert!(
        s.starts_with(
            "Thread: web\nState: waiting on background\nNow: waiting on background: cargo \
             test\n"
        ),
        "{s}"
    );
    assert!(s.contains("half the pages built"), "{s}");
    // A `!` command ending, before and after the thread's own work: no update of its own.
    thread(&hub, "db", "db", json!({}));
    interim(&hub, "db", 1, "go", "migrations half applied");
    on(&hub, "db", None, shell_task("s4", true, false));
    on(&hub, "db", None, shell_task("s4", false, false));
    on(&hub, "db", None, shell_task("s5", true, true));
    on(&hub, "db", None, task("t1", false));
    on(&hub, "db", None, shell_task("s5", false, true));
    let r = summary(&hub, &ask("db")).await;
    assert_eq!(r.summary.as_deref(), Some("busy"), "{r:?}");
    let s = stdin();
    clean(&s);
    assert_eq!(s.matches("background work finished").count(), 1, "{s}");
    assert!(s.starts_with("Thread: db\nState: done\n"), "{s}");
    assert_eq!(runs(&cli), 3);
    hub.shutdown().await;
}

/// An ask that comes while a call made of another basis runs (the thread moved on: a routine
/// update came) waits for it, and gets its summary as the last one (`interval`), of the
/// updates it read (`covers`), not as the summary of its own.
#[tokio::test]
async fn an_ask_on_another_basis_does_not_take_a_running_call_as_its_own() {
    let dir = tempfile::tempdir().unwrap();
    let cli = dir.path().join("cli");
    std::fs::create_dir_all(&cli).unwrap();
    let bin = fake_claude(&cli);
    answer(&cli, "half the docs built", false);
    let hub = start(dir.path(), bin).await;
    interim(&hub, "api", 1, "go", "DOCS-A half built");
    let a_seq = update_seq(dir.path(), "DOCS-A");
    std::fs::write(cli.join("slow"), "").unwrap();
    let h = hub.clone();
    let first = tokio::spawn(async move { summary(&h, &ask("api")).await });
    timeout(WAIT, async {
        while !cli.join("pid").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    // A new run of updates begins while the call runs.
    on(&hub, "api", None, task("t1", false));
    interim(&hub, "api", 2, "go on", "DOCS-B all built");
    let b_seq = update_seq(dir.path(), "DOCS-B");
    assert!(b_seq > a_seq);
    let h = hub.clone();
    let second = tokio::spawn(async move { summary(&h, &ask("api")).await });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!second.is_finished(), "it waits for the call in flight");
    std::fs::remove_file(cli.join("slow")).unwrap();
    let first = timeout(WAIT, first).await.unwrap().unwrap();
    let second = timeout(WAIT, second).await.unwrap().unwrap();
    assert_eq!(runs(&cli), 1, "one call");
    assert_eq!(
        (
            first.summary.as_deref(),
            first.cached,
            first.reason.as_deref()
        ),
        (Some("half the docs built"), false, None),
        "{first:?}"
    );
    assert_eq!(first.covers, Some(a_seq), "{first:?}");
    assert_eq!(
        (
            second.summary.as_deref(),
            second.cached,
            second.reason.as_deref()
        ),
        (Some("half the docs built"), true, Some("interval")),
        "{second:?}"
    );
    assert_eq!(second.covers, Some(a_seq), "of run A, not B: {second:?}");
    assert_ne!(second.basis, first.basis, "{second:?}");
    assert_eq!(second.next_after_secs, Some(300), "{second:?}");
    // Asked again, the same: the last summary is of run A until the interval is over.
    let again = summary(&hub, &ask("api")).await;
    assert_eq!(
        (again.reason.as_deref(), again.covers, again.basis.as_str()),
        (Some("interval"), Some(a_seq), second.basis.as_str()),
        "{again:?}"
    );
    hub.shutdown().await;
}

/// A later run of updates that says what an earlier one said, in the same state, has the
/// earlier summary's basis: it gets that summary without a call, as the summary of its own
/// updates (`covers`, the newest this ask read), not of the earlier run's, so the asker files
/// it under the run it shows. The same across a switch of the orchestrator: a new thread whose
/// seqs start again, which `covers_in` names; the interval's answer names the earlier
/// conversation it read.
#[tokio::test]
async fn the_same_basis_again_covers_the_updates_this_ask_read() {
    let dir = tempfile::tempdir().unwrap();
    let cli = dir.path().join("cli");
    std::fs::create_dir_all(&cli).unwrap();
    let bin = fake_claude(&cli);
    answer(&cli, "half the docs built", false);
    let hub = start(dir.path(), bin).await;
    // A run: the interim report, its background work finishing.
    let run = |turn: u64| {
        interim(&hub, "api", turn, "go", "DOCS half built");
        on(&hub, "api", None, task(&format!("t{turn}"), false));
    };
    // What closes a run: an update about the thread that is not routine.
    let close = |id: &str| {
        raise(&hub, "api", id, AttentionReason::Error, "it broke");
        resolve(&hub, "api", id);
    };
    run(1);
    let a = update_seq(dir.path(), "api background work finished");
    let first = summary(&hub, &ask("api")).await;
    assert_eq!(
        (first.summary.as_deref(), first.cached, first.covers),
        (Some("half the docs built"), false, Some(a)),
        "{first:?}"
    );
    assert_eq!(first.covers_in.as_deref(), Some("o1"), "{first:?}");
    close("api:e1");
    run(2);
    let b = update_seq(dir.path(), "api background work finished");
    assert!(b > a);
    let second = summary(&hub, &ask("api")).await;
    assert_eq!(second.basis, first.basis, "the same words: {second:?}");
    assert_eq!(
        (
            second.summary.as_deref(),
            second.cached,
            second.reason.as_deref()
        ),
        (Some("half the docs built"), true, None),
        "{second:?}"
    );
    assert_eq!(
        (second.covers, second.covers_in.as_deref()),
        (Some(b), Some("o1")),
        "of the later run, which this ask read: {second:?}"
    );
    assert_eq!(runs(&cli), 1);
    // A new orchestrator conversation: its seqs start again.
    thread(&hub, "o2", "orchestrator", json!({"role": "orchestrator"}));
    on(
        &hub,
        "o2",
        None,
        EventBody::ThreadStopped(Default::default()),
    );
    close("api:e2");
    // Inside the interval, a new basis (no updates in the new conversation yet): the last
    // summary, of the updates it read in the old one.
    let soon = summary(&hub, &ask("api")).await;
    assert_ne!(soon.basis, first.basis, "{soon:?}");
    assert_eq!(soon.reason.as_deref(), Some("interval"), "{soon:?}");
    assert_eq!(
        (soon.covers, soon.covers_in.as_deref()),
        (Some(b), Some("o1")),
        "{soon:?}"
    );
    run(3);
    let c = update_seq_in(dir.path(), "o2", "api background work finished");
    let third = summary(&hub, &ask("api")).await;
    assert_eq!(third.basis, first.basis, "the same words: {third:?}");
    assert_eq!(
        (third.cached, third.covers, third.covers_in.as_deref()),
        (true, Some(c), Some("o2")),
        "{third:?}"
    );
    assert_eq!(runs(&cli), 1);
    hub.shutdown().await;
}

/// A failed call (a usage limit) says why, and no call of that runtime follows for a while;
/// at most `SUMMARY_HOURLY_MAX` calls an hour for the hub, whatever thread asks.
#[tokio::test]
async fn a_failure_backs_off_and_the_hour_has_a_cap() {
    let dir = tempfile::tempdir().unwrap();
    let cli = dir.path().join("cli");
    std::fs::create_dir_all(&cli).unwrap();
    let bin = fake_claude(&cli);
    answer(&cli, "You've hit your limit · resets 3pm", true);
    let hub = start(dir.path(), bin).await;
    let r = summary(&hub, &ask("api")).await;
    assert_eq!(
        r.reason.as_deref(),
        Some("failed: You've hit your limit · resets 3pm"),
        "{r:?}"
    );
    assert_eq!(r.stats.failures, 1);
    thread(&hub, "w0", "w0", json!({}));
    let r = summary(&hub, &ask("w0")).await;
    assert_eq!(r.reason.as_deref(), Some("backoff"), "{r:?}");
    assert!(
        r.next_after_secs.is_some_and(|n| n > 500 && n <= 600),
        "{r:?}"
    );
    assert_eq!(runs(&cli), 1);
    hub.shutdown().await;

    let dir = tempfile::tempdir().unwrap();
    let cli = dir.path().join("cli");
    std::fs::create_dir_all(&cli).unwrap();
    let bin = fake_claude(&cli);
    answer(&cli, "working on it", false);
    let hub = start(dir.path(), bin).await;
    let max = twr_hub::hub::SUMMARY_HOURLY_MAX;
    for i in 0..max {
        let name = format!("w{i}");
        thread(&hub, &name, &name, json!({}));
        let r = summary(&hub, &ask(&name)).await;
        assert_eq!(r.summary.as_deref(), Some("working on it"), "{i}: {r:?}");
    }
    let r = summary(&hub, &ask("api")).await;
    assert_eq!(r.reason.as_deref(), Some("limit"), "{r:?}");
    assert!(r.next_after_secs.is_some_and(|n| n > 3500), "{r:?}");
    assert_eq!(runs(&cli), max);
    // What a thread had stays its answer for the same basis.
    let r = summary(&hub, &ask("w0")).await;
    assert_eq!(
        (r.summary.as_deref(), r.cached),
        (Some("working on it"), true)
    );
    hub.shutdown().await;
}

/// Only the user asks: the orchestrator is refused (and agents may not call the method at all),
/// a model name must be one the CLI takes, and the orchestrator's own thread has no summary.
#[tokio::test]
async fn only_the_user_asks_about_a_worker() {
    let dir = tempfile::tempdir().unwrap();
    let cli = dir.path().join("cli");
    std::fs::create_dir_all(&cli).unwrap();
    let bin = fake_claude(&cli);
    answer(&cli, "busy", false);
    let hub = start(dir.path(), bin).await;
    let e = hub
        .summary_thread(&ask("api"), &Caller::Orchestrator)
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::UNAUTHORIZED);
    for list in [
        twr_hub::server::AGENT_METHODS,
        twr_hub::server::ORCHESTRATOR_METHODS,
    ] {
        assert!(!list.contains(&"summary.thread"));
    }
    assert!(twr_hub::server::METHODS.contains(&"summary.thread"));
    let bad = SummaryThreadParams {
        models: Some(SuggestModels {
            claude_code: "--dangerously-skip-permissions".into(),
            ..Default::default()
        }),
        ..ask("api")
    };
    let e = hub
        .summary_thread(&bad, &Caller::user("tui"))
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST);
    let e = hub
        .summary_thread(&ask("nope"), &Caller::user("tui"))
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::NOT_FOUND);
    let r = summary(&hub, &ask("o1")).await;
    assert_eq!(r.reason.as_deref(), Some("not_worker"), "{r:?}");
    assert_eq!(runs(&cli), 0);
    hub.shutdown().await;
}

/// `every_secs` is bounded above, as the config's spans are: after a summary, an ask with the
/// longest interval gets it (`interval`), and one with more is refused, not a panic of the
/// hub's when it adds the interval to the summary's time.
#[tokio::test]
async fn an_interval_past_the_longest_span_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let cli = dir.path().join("cli");
    std::fs::create_dir_all(&cli).unwrap();
    answer(&cli, "Building.", false);
    let hub = start(dir.path(), fake_claude(&cli)).await;
    interim(&hub, "api", 1, "go", "started the build");
    let r = summary(&hub, &ask("api")).await;
    assert_eq!(r.summary.as_deref(), Some("Building."), "{r:?}");
    interim(&hub, "api", 2, "more", "started the tests");
    let longest = SummaryThreadParams {
        every_secs: twr_core::config::MAX_SPAN.as_secs(),
        ..ask("api")
    };
    let r = summary(&hub, &longest).await;
    assert_eq!(r.reason.as_deref(), Some("interval"), "{r:?}");
    for every_secs in [u64::MAX, twr_core::config::MAX_SPAN.as_secs() + 1] {
        let e = hub
            .summary_thread(
                &SummaryThreadParams {
                    every_secs,
                    ..ask("api")
                },
                &Caller::user("tui"),
            )
            .await
            .unwrap_err();
        assert_eq!(e.code, codes::BAD_REQUEST, "{every_secs}: {e:?}");
    }
    assert_eq!(runs(&cli), 1);
    hub.shutdown().await;
}

/// No Claude Code CLI: the summary says another runtime may answer the next ask, then that
/// none can; and suggestions find Claude Code out too (the runtimes found unable to run are
/// shared).
#[tokio::test]
async fn a_missing_cli_is_unavailable_for_suggestions_too() {
    let dir = tempfile::tempdir().unwrap();
    let hub = start(dir.path(), dir.path().join("no-such-claude")).await;
    assert!(hub.info()["suggest"]["unavailable"].is_null());
    let r = summary(&hub, &ask("api")).await;
    let reason = r.reason.clone().unwrap_or_default();
    assert!(reason.starts_with("retry: Claude Code's CLI ("), "{r:?}");
    assert!(!r.unavailable());
    thread(&hub, "web", "web", json!({}));
    let r = summary(&hub, &ask("web")).await;
    assert!(r.unavailable(), "{r:?}");
    assert!(
        r.reason.as_deref().unwrap().contains("; Codex's CLI ("),
        "{r:?}"
    );
    assert_eq!(
        hub.info()["suggest"]["unavailable"].as_str(),
        r.reason.as_deref()
    );
    hub.shutdown().await;
}

/// A runtime a call found unable to run stays out (for suggestions too) after its CLI appears,
/// until an ask with `retry` (the user turned summaries on again): that one tries it again,
/// and it answers.
#[tokio::test]
async fn a_runtime_found_down_is_tried_again_with_retry() {
    let dir = tempfile::tempdir().unwrap();
    let cli = dir.path().join("cli");
    let hub = start(dir.path(), cli.join("claude")).await;
    interim(&hub, "api", 1, "build it", "started the build");
    let r = summary(&hub, &ask("api")).await;
    let reason = r.reason.clone().unwrap_or_default();
    assert!(reason.starts_with("retry: Claude Code's CLI ("), "{r:?}");
    // The CLI is there now (installed, signed in): without a retry, still out.
    std::fs::create_dir_all(&cli).unwrap();
    fake_claude(&cli);
    answer(&cli, "Building the workspace.", false);
    let r = summary(&hub, &ask("api")).await;
    assert!(r.unavailable(), "{r:?}");
    assert_eq!(runs(&cli), 0);
    let retry = SummaryThreadParams {
        retry: true,
        ..ask("api")
    };
    let r = summary(&hub, &retry).await;
    assert_eq!(
        r.summary.as_deref(),
        Some("Building the workspace."),
        "{r:?}"
    );
    assert_eq!(r.runtime.as_deref(), Some("claude-code"));
    assert_eq!(runs(&cli), 1);
    // Tried again for suggestions as well: nothing marks it down any more.
    assert!(hub.info()["suggest"]["unavailable"].is_null());
    // `retry` is left out of the parameters unless set.
    let v = serde_json::to_value(ask("api")).unwrap();
    assert!(v.get("retry").is_none(), "{v}");
    assert_eq!(serde_json::to_value(&retry).unwrap()["retry"], true);
    hub.shutdown().await;
}

/// Two asks at once share one call; an ask whose client hangs up goes with the CLI it
/// started, and the next ask calls afresh.
#[tokio::test]
async fn asks_share_a_call_and_one_whose_client_hangs_up_ends_it() {
    let dir = tempfile::tempdir().unwrap();
    let cli = dir.path().join("cli");
    std::fs::create_dir_all(&cli).unwrap();
    let bin = fake_claude(&cli);
    answer(&cli, "building the docs", false);
    let hub = start(dir.path(), bin).await;
    interim(&hub, "api", 1, "go", "half the docs built");
    let p = ask("api");
    let (a, b) = tokio::join!(summary(&hub, &p), summary(&hub, &p));
    assert_eq!(a.summary.as_deref(), Some("building the docs"), "{a:?}");
    assert_eq!(b.summary, a.summary, "{b:?}");
    assert_eq!(
        (b.basis.as_str(), b.reason.as_deref()),
        (a.basis.as_str(), None)
    );
    assert_eq!(
        a.covers,
        Some(update_seq(dir.path(), "half the docs built"))
    );
    assert_eq!(b.covers, a.covers);
    assert_eq!(runs(&cli), 1);

    thread(&hub, "web", "web", json!({}));
    std::fs::write(cli.join("slow"), "").unwrap();
    let socket = dir.path().join("serve.sock");
    let listener = twr_hub::server::bind(&socket).await.unwrap();
    let serving = tokio::spawn(twr_hub::server::serve(
        hub.clone(),
        listener,
        socket.clone(),
        false,
    ));
    let s = socket.clone();
    let asking = tokio::spawn(async move {
        let mut c = twr_core::client::Client::connect(&s).await.unwrap();
        c.hello(json!({"client": "tui"})).await.unwrap();
        c.summary_thread(&ask("web")).await
    });
    let pid_file = cli.join("pid");
    let pid: i32 = timeout(WAIT, async {
        loop {
            if let Some(pid) = std::fs::read_to_string(&pid_file)
                .ok()
                .and_then(|s| s.trim().parse().ok())
            {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    asking.abort();
    let _ = asking.await;
    // SAFETY: signal 0 checks the process exists; nothing is sent.
    let alive = || unsafe { libc::kill(pid, 0) } == 0 && !zombie(pid);
    timeout(WAIT, async {
        while alive() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the CLI outlived its client");
    std::fs::remove_file(cli.join("slow")).unwrap();
    let r = timeout(WAIT, async {
        loop {
            let r = summary(&hub, &ask("web")).await;
            if r.reason.is_none() {
                return r;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(r.summary.as_deref(), Some("building the docs"), "{r:?}");
    assert_eq!(runs(&cli), 3);
    serving.abort();
    hub.shutdown().await;
}

/// An ask that waits on another's call whose client hangs up (or turned summaries off) is not
/// left with nothing (`failed`, a full interval to wait): it takes the call over, its start
/// counted once, and gets the summary.
#[tokio::test]
async fn an_ask_waiting_on_a_call_whose_client_hangs_up_takes_it_over() {
    let dir = tempfile::tempdir().unwrap();
    let cli = dir.path().join("cli");
    std::fs::create_dir_all(&cli).unwrap();
    let bin = fake_claude(&cli);
    answer(&cli, "building the docs", false);
    let hub = start(dir.path(), bin).await;
    interim(&hub, "api", 1, "go", "half the docs built");
    std::fs::write(cli.join("slow"), "").unwrap();
    let socket = dir.path().join("serve.sock");
    let listener = twr_hub::server::bind(&socket).await.unwrap();
    let serving = tokio::spawn(twr_hub::server::serve(
        hub.clone(),
        listener,
        socket.clone(),
        false,
    ));
    let s = socket.clone();
    let owner = tokio::spawn(async move {
        let mut c = twr_core::client::Client::connect(&s).await.unwrap();
        c.hello(json!({"client": "tui"})).await.unwrap();
        c.summary_thread(&ask("api")).await
    });
    timeout(WAIT, async {
        while !cli.join("pid").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let h = hub.clone();
    let waiter = tokio::spawn(async move { summary(&h, &ask("api")).await });
    tokio::time::sleep(Duration::from_millis(200)).await;
    owner.abort();
    let _ = owner.await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    std::fs::remove_file(cli.join("slow")).unwrap();
    let r = timeout(WAIT, waiter).await.unwrap().unwrap();
    assert_eq!(r.summary.as_deref(), Some("building the docs"), "{r:?}");
    assert_eq!(r.reason, None, "{r:?}");
    assert_eq!(
        r.covers,
        Some(update_seq(dir.path(), "half the docs built"))
    );
    assert_eq!(runs(&cli), 2, "the call taken over runs again");
    serving.abort();
    hub.shutdown().await;
}

/// A stall the thread went on from (its own work closed it: `went_on`) is no update of the
/// run's, nor its end: the summary reads the routine updates before it and covers it, as the
/// TUI folds it into the run. One closed otherwise (here its thread stopped) ends the run:
/// nothing of it read, `covers` none.
#[tokio::test]
async fn a_stall_the_thread_went_on_from_is_read_past() {
    for went in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let cli = dir.path().join("cli");
        std::fs::create_dir_all(&cli).unwrap();
        let bin = fake_claude(&cli);
        answer(&cli, "tests running", false);
        let hub = start(dir.path(), bin).await;
        interim(&hub, "api", 1, "go", "ran 120 tests, 3 failing");
        // Its background work silent: the stall the hub raises (`hub::stall`).
        on(
            &hub,
            "api",
            None,
            EventBody::AttentionRaised(AttentionRaised {
                attn_id: "api:s1".into(),
                reason: AttentionReason::Stalled,
                summary: "No activity for 10 min from its background tasks".into(),
                options: vec!["dismiss".into()],
                ..Default::default()
            }),
        );
        if went {
            // Its task moves: the hub closes the stall in that batch, saying it went on.
            hub.append(
                "api",
                None,
                Source::Adapter("fake".into()),
                task("t1", true),
                None,
            )
            .unwrap();
        } else {
            on(
                &hub,
                "api",
                None,
                EventBody::ThreadStopped(Default::default()),
            );
        }
        let (_, evs) = hub.thread_events("api", 0, 1000).unwrap();
        let closed = evs
            .iter()
            .find_map(|e| match &e.body {
                EventBody::AttentionResolved(p) if p.attn_id == "api:s1" => Some(p.clone()),
                _ => None,
            })
            .expect("the stall is closed");
        assert_eq!(went_on(&closed.ext), went, "{closed:?}");
        let stalled = update_seq(dir.path(), "api is stalled");
        let r = summary(&hub, &ask("api")).await;
        let stdin = std::fs::read_to_string(cli.join("stdin")).unwrap();
        assert!(!stdin.contains("No activity"), "{stdin}");
        if went {
            assert_eq!(r.covers, Some(stalled), "{went}: {r:?}");
            assert!(stdin.contains("ran 120 tests, 3 failing"), "{stdin}");
        } else {
            assert_eq!(r.covers, None, "{r:?}");
            assert_eq!(r.covers_in.as_deref(), Some("o1"), "{r:?}");
            assert!(!stdin.contains("ran 120 tests"), "{stdin}");
        }
        hub.shutdown().await;
    }
}

/// Whether `pid` is a zombie (ended, not yet reaped by its parent).
fn zombie(pid: i32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| {
            s.rsplit_once(')')
                .map(|(_, r)| r.trim_start().starts_with('Z'))
        })
        .unwrap_or(true)
}
