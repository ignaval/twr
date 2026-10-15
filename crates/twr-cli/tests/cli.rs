//! End-to-end: a real `twr hub` process with `TWR_HOME` in a tempdir, driven by the `twr`
//! subcommands.

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const TWR: &str = env!("CARGO_BIN_EXE_twr");

/// The `twr` binary, without a `TWR_WORKSPACE` the test's own environment may have (the tests
/// here run in the `default` workspace of their `TWR_HOME`).
fn twr_cmd() -> Command {
    let mut c = Command::new(TWR);
    c.env_remove("TWR_WORKSPACE");
    c.env("TWR_TEST_USER_TOKEN_FILE", test_token());
    c
}

/// A user token this test process's hubs and clients share (`TWR_TEST_USER_TOKEN_FILE`): run
/// from inside a twr thread, the tests' clients would otherwise count as that thread's agent
/// to the hubs they start, which descend from it too.
fn test_token() -> &'static std::path::Path {
    static TOKEN: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    TOKEN.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap().keep();
        let path = dir.join("user.token");
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // Private to this user, or the hub ignores it.
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .unwrap()
            .write_all(format!("{nanos:x}{:x}", std::process::id()).as_bytes())
            .unwrap();
        path
    })
}
const WAIT: Duration = Duration::from_secs(15);

fn twr(home: &Path, args: &[&str]) -> Output {
    twr_cmd()
        .args(args)
        .env("TWR_HOME", home)
        .env_remove("RUST_LOG")
        .output()
        .expect("running twr")
}

fn ok(home: &Path, args: &[&str]) -> String {
    let out = twr(home, args);
    assert!(
        out.status.success(),
        "twr {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// Retries `f` until it returns Some or the deadline passes.
fn poll<T>(what: &str, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(25));
    }
}

fn ndjson(s: &str) -> Vec<Value> {
    s.lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{e}: {l}")))
        .collect()
}

struct HubProc(Child);

impl Drop for HubProc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn hub_thread_send_events() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let project = home.join("proj");
    std::fs::create_dir(&project).unwrap();
    let log = std::fs::File::create(home.join("hub-test.log")).unwrap();
    let mut hub = HubProc(
        twr_cmd()
            .arg("hub")
            .env("TWR_HOME", home)
            // The tests' threads run the fake runtime, which the hub offers only on request.
            .env("TWR_ENABLE_FAKE", "1")
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    poll("the hub socket", || {
        twr(home, &["thread", "list"])
            .status
            .success()
            .then_some(())
    });
    assert!(ok(home, &["thread", "list"]).contains("no threads"));

    let created = ok(
        home,
        &[
            "thread",
            "new",
            project.to_str().unwrap(),
            "--runtime",
            "fake",
            "--name",
            "t1",
            "--alias",
            "first",
        ],
    );
    assert!(created.starts_with("created t1 ("), "{created}");
    assert!(created.contains(" model=(runtime default) "), "{created}");

    // A follower started now sees everything that follows.
    let mut follower = twr_cmd()
        .args(["control", "--follow"])
        .env("TWR_HOME", home)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    let sent = ok(home, &["send", "first", "hello", "from", "cli"]);
    assert_eq!(sent.trim(), "turn 1 started on first");

    let events = poll("the turn's report", || {
        let evs = ndjson(&ok(home, &["events", "t1"]));
        evs.iter()
            .any(|e| e["kind"] == "report.ready")
            .then_some(evs)
    });
    let kinds: Vec<&str> = events.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds[0], "thread.created");
    assert_eq!(kinds[1], "turn.started");
    assert_eq!(events[1]["source"], "user:cli");
    assert_eq!(events[1]["data"]["prompt"], "hello from cli");
    let final_text = events
        .iter()
        .find(|e| e["kind"] == "agent.text" && e["data"]["final"] == true)
        .unwrap();
    assert_eq!(final_text["data"]["text"], "hello from cli");
    assert!(kinds.contains(&"turn.ended"));
    let seqs: Vec<u64> = events.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
    assert_eq!(seqs, (1..=seqs.len() as u64).collect::<Vec<_>>());

    // --since pages from the middle; --pretty prints one line per event, the streamed deltas
    // of a reply (non-final agent.text of one turn) merged into one.
    let tail = ndjson(&ok(home, &["events", "t1", "--since", "2", "--hydrate"]));
    assert_eq!(tail[0]["seq"], 3);
    let pretty = ok(home, &["events", "t1", "--pretty"]);
    let delta = |e: &Value| e["kind"] == "agent.text" && e["data"]["final"] != true;
    let merged = events
        .windows(2)
        .filter(|w| delta(&w[0]) && delta(&w[1]) && w[0]["turn"] == w[1]["turn"])
        .count();
    assert!(merged > 0, "the fake runtime streams its reply");
    assert_eq!(pretty.lines().count(), events.len() - merged, "{pretty}");
    assert!(pretty.contains("report.ready"));
    assert!(pretty.contains("hello from cli"));

    // A final text larger than one socket frame (16 MiB) hydrates through chunked blob.get.
    let before = events.last().unwrap()["seq"].as_u64().unwrap();
    ok(home, &["send", "t1", "!long 17000000"]);
    let since = before.to_string();
    poll("the long turn's report", || {
        ndjson(&ok(home, &["events", "t1", "--since", &since]))
            .iter()
            .any(|e| e["kind"] == "report.ready")
            .then_some(())
    });
    let long = ndjson(&ok(home, &["events", "t1", "--since", &since, "--hydrate"]));
    let text = long
        .iter()
        .find(|e| e["kind"] == "agent.text" && e["data"]["final"] == true)
        .unwrap();
    let text = text["data"]["text"].as_str().unwrap();
    assert_eq!(text.len(), 17_000_000);
    assert!(text.bytes().all(|b| b == b'x'));
    assert!(long
        .iter()
        .all(|e| !e["data"].to_string().contains("_capped")));

    let control = ndjson(&ok(home, &["control"]));
    let report = control
        .iter()
        .find(|i| i["event"]["kind"] == "report.ready")
        .unwrap();
    assert_eq!(report["stream"], "control");
    assert_eq!(report["event"]["data"]["summary"], "hello from cli");
    assert!(control
        .iter()
        .all(|i| !i["event"]["kind"].as_str().unwrap().starts_with("agent.")));
    let gseqs: Vec<u64> = control
        .iter()
        .map(|i| i["gseq"].as_u64().unwrap())
        .collect();
    assert_eq!(gseqs, (1..=gseqs.len() as u64).collect::<Vec<_>>());

    let list = ok(home, &["thread", "list"]);
    let row = list.lines().find(|l| l.starts_with("t1 ")).unwrap();
    assert!(row.contains(" done "), "{list}");
    // Last, what it is doing now: its turn is done.
    assert!(
        list.lines().next().unwrap().ends_with(" ACTIVITY"),
        "{list}"
    );
    let activity = row.rsplit("  ").next().unwrap();
    assert!(activity.starts_with("done"), "{list}");
    // Threads run in twr's default mode, `auto`, until `twr mode` changes it.
    assert!(list.lines().next().unwrap().contains(" MODE "), "{list}");
    assert!(row.contains(" auto "), "{list}");
    let out = ok(home, &["mode", "t1", "plan"]);
    assert!(out.starts_with("t1: auto -> plan ("), "{out}");
    assert_eq!(
        ok(home, &["mode", "t1", "plan"]).trim(),
        "t1 is already in plan mode"
    );
    let list = ok(home, &["thread", "list"]);
    let row = list.lines().find(|l| l.starts_with("t1 ")).unwrap();
    assert!(row.contains(" plan "), "{list}");
    let bad = twr(home, &["mode", "t1", "  "]);
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("bad_request"));
    // The model the fake runtime reports it runs (none was asked for).
    assert!(list.lines().next().unwrap().contains(" MODEL "), "{list}");
    assert!(
        row.contains(&format!(" {} ", twr_adapters::fake::DEFAULT_MODEL)),
        "{list}"
    );
    // `twr model` sets the model and effort (the fake runtime takes any effort), says so
    // once, and shows them with only the thread.
    let out = ok(home, &["model", "t1", "fake-big", "--effort", "deep"]);
    assert!(
        out.starts_with(
            "t1: model (runtime default), effort (runtime default) -> model fake-big, effort \
             deep ("
        ),
        "{out}"
    );
    assert_eq!(
        ok(home, &["model", "t1", "fake-big"]).trim(),
        "t1 already runs with model fake-big, effort deep"
    );
    let shown = ok(home, &["model", "t1"]);
    assert!(
        shown.starts_with("t1: runtime fake, model fake-big, effort deep"),
        "{shown}"
    );
    let out = ok(home, &["model", "t1", "--effort", "default"]);
    assert!(
        out.contains("-> model fake-big, effort (runtime default) ("),
        "{out}"
    );

    // A second hub on the same data dir (even on another socket) refuses to start and leaves
    // no socket behind.
    let other = home.join("other.sock");
    let second = twr(home, &["--socket", other.to_str().unwrap(), "hub"]);
    assert!(!second.status.success());
    let err = String::from_utf8_lossy(&second.stderr);
    assert!(err.contains("another hub is using"), "{err}");
    assert!(!other.exists());

    let bad = twr(home, &["send", "nope", "x"]);
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("not_found"));

    // SIGTERM: the hub stops the runtime, ends streams and removes its socket.
    let pid = hub.0.id().to_string();
    assert!(Command::new("kill")
        .args(["-TERM", &pid])
        .status()
        .unwrap()
        .success());
    let status = poll("the hub to exit", || hub.0.try_wait().unwrap());
    assert!(status.success(), "{status:?}");
    assert!(!home.join("hub.sock").exists());

    let lines: Vec<Value> = BufReader::new(follower.stdout.take().unwrap())
        .lines()
        .map(|l| serde_json::from_str(&l.unwrap()).unwrap())
        .collect();
    let status = poll("the follower to exit", || follower.try_wait().unwrap());
    assert!(status.success());
    assert!(lines.iter().any(|i| i["event"]["kind"] == "report.ready"));
    assert_eq!(lines.last().unwrap()["event"]["kind"], "thread.stopped");

    let down = twr(home, &["thread", "list"]);
    assert!(!down.status.success());
    assert!(String::from_utf8_lossy(&down.stderr).contains("no hub is running"));
}

/// Starts `twr hub` in `home` and waits until it answers.
fn start_hub(home: &Path) -> HubProc {
    start_hub_with(home, &[])
}

/// [`start_hub`] with `env` added to the hub's environment.
fn start_hub_with(home: &Path, env: &[(&str, &str)]) -> HubProc {
    let log = std::fs::File::create(home.join("hub-test.log")).unwrap();
    let hub = HubProc(
        twr_cmd()
            .arg("hub")
            .envs(env.iter().copied())
            .env("TWR_HOME", home)
            // The tests' threads run the fake runtime, which the hub offers only on request.
            .env("TWR_ENABLE_FAKE", "1")
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    poll("the hub socket", || {
        twr(home, &["thread", "list"])
            .status
            .success()
            .then_some(())
    });
    hub
}

fn events_of(home: &Path, thread: &str) -> Vec<Value> {
    ndjson(&ok(home, &["events", thread]))
}

fn reports(evs: &[Value]) -> usize {
    evs.iter().filter(|e| e["kind"] == "report.ready").count()
}

#[test]
fn say_routes_to_the_only_thread_and_asks_when_it_cannot_tell() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let project = home.join("proj");
    std::fs::create_dir(&project).unwrap();
    // No orchestrator runtime: undecided messages become a question for the user.
    std::fs::write(
        home.join("config.toml"),
        "[orchestrator]\nruntime = \"none\"\n",
    )
    .unwrap();
    let _hub = start_hub(home);
    ok(
        home,
        &[
            "thread",
            "new",
            project.to_str().unwrap(),
            "--runtime",
            "fake",
            "--name",
            "t1",
        ],
    );
    let said = ok(home, &["say", "run", "the", "tests"]);
    assert_eq!(said.trim(), "sent to t1 (turn 1 started)");
    let evs = poll("the routed turn's report", || {
        let evs = events_of(home, "t1");
        (reports(&evs) == 1).then_some(evs)
    });
    let started = evs.iter().find(|e| e["kind"] == "turn.started").unwrap();
    assert_eq!(started["data"]["prompt"], "run the tests");
    assert_eq!(started["data"]["from"], "user");
    assert_eq!(started["source"], "user:cli");
    let control = ndjson(&ok(home, &["control"]));
    let decision = control
        .iter()
        .find(|i| i["event"]["kind"] == "decision.made" && i["event"]["thread"] == "hub")
        .unwrap();
    assert_eq!(decision["event"]["data"]["site"], "route");
    assert_eq!(decision["event"]["data"]["chosen"], "t1");
    assert_eq!(started["data"]["routed_by"], decision["event"]["id"]);

    // Two threads and no orchestrator: the hub asks, and `twr respond` answers.
    ok(
        home,
        &[
            "thread",
            "new",
            project.to_str().unwrap(),
            "--runtime",
            "fake",
            "--name",
            "t2",
        ],
    );
    let said = ok(home, &["say", "deploy", "it"]);
    assert!(said.starts_with("which thread should get it?"), "{said}");
    assert!(
        said.contains("1 t1, 2 t2, 3 new thread, 4 cancel (don't send)"),
        "{said}"
    );
    let attn = said.split('`').nth(1).unwrap().split(' ').nth(2).unwrap();
    ok(home, &["respond", attn, "2"]);
    poll("the answered message's turn on t2", || {
        events_of(home, "t2")
            .iter()
            .any(|e| e["kind"] == "turn.started" && e["data"]["prompt"] == "deploy it")
            .then_some(())
    });
}

#[test]
fn mcp_keeps_stdout_for_the_protocol() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    // No hub: the server cannot connect; the error goes to stderr and stdout stays empty.
    let out = twr_cmd()
        .arg("mcp")
        .env("TWR_HOME", home)
        .env("TWR_ORCH_TOKEN", "0123")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        out.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("connecting to hub"), "{stderr}");
}

/// `twr mcp` exists only for the orchestrator: without its token it does not start, whether or
/// not a hub is running.
#[test]
fn mcp_refuses_to_start_without_the_orchestrator_token() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let _hub = start_hub(home);
    for token in [None, Some("")] {
        let mut cmd = twr_cmd();
        cmd.arg("mcp")
            .env("TWR_HOME", home)
            .env_remove("TWR_ORCH_TOKEN")
            .stdin(Stdio::null());
        if let Some(t) = token {
            cmd.env("TWR_ORCH_TOKEN", t);
        }
        let out = cmd.output().unwrap();
        assert!(!out.status.success(), "{token:?}");
        assert!(out.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("TWR_ORCH_TOKEN is not set"), "{stderr}");
    }
}

#[test]
fn respond_and_attention_work_without_a_screen() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let project = home.join("proj");
    std::fs::create_dir(&project).unwrap();
    let _hub = start_hub(home);
    let created = ok(
        home,
        &[
            "thread",
            "new",
            project.to_str().unwrap(),
            "--runtime",
            "fake",
            "--name",
            "t1",
            "--model",
            "opus-x",
        ],
    );
    assert!(created.contains("model=opus-x"), "{created}");
    assert_eq!(ok(home, &["attention"]).trim(), "nothing needs you");

    // A permission, answered by number on the thread.
    ok(home, &["send", "t1", "!perm Bash ls"]);
    let listing = poll("the permission item", || {
        let out = ok(home, &["attention"]);
        out.contains("permission").then_some(out)
    });
    let row = listing.lines().nth(1).unwrap();
    assert!(row.contains(" t1 "), "{listing}");
    assert!(row.ends_with("1:allow 2:deny"), "{listing}");
    // With a thread and no terminal to confirm on: the top item is shown, nothing is
    // answered, and the error names the exact `<attn_id>@<revision>` to answer.
    let refused = twr(home, &["respond", "t1", "1"]);
    assert!(!refused.status.success());
    let shown = String::from_utf8_lossy(&refused.stdout).to_string();
    let err = String::from_utf8_lossy(&refused.stderr).to_string();
    let listed_id = row.split_whitespace().next().unwrap();
    assert!(shown.starts_with(listed_id), "{shown}");
    assert!(shown.contains("options: 1:allow 2:deny"), "{shown}");
    assert!(err.contains("unseen"), "{err}");
    assert!(
        err.contains(&format!("`twr respond {listed_id} 1`")),
        "{err}"
    );
    assert!(ok(home, &["attention"]).contains(listed_id));
    // --yes: the question it answers is shown first (with its revision), then answered.
    let answered = ok(home, &["respond", "t1", "1", "--yes"]);
    let shown = answered.lines().next().unwrap();
    assert!(
        shown.contains("@") && shown.ends_with("on t1 (permission): Allow Bash?"),
        "{answered}"
    );
    let answered = answered.lines().last().unwrap();
    assert!(answered.starts_with("answered "), "{answered}");
    assert!(answered.trim().ends_with("on t1: allow"), "{answered}");
    poll("turn 1's report", || {
        (reports(&events_of(home, "t1")) == 1).then_some(())
    });

    // A question, answered by attention id with a case-insensitive label and a message.
    ok(home, &["send", "t1", "!ask Which db?|postgres|sqlite"]);
    let listing = poll("the question item", || {
        let out = ok(home, &["attention"]);
        out.contains("question").then_some(out)
    });
    let attn_id = listing
        .lines()
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_string();
    let bad = twr(home, &["respond", &attn_id, "3"]);
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("pick 1-2"));
    let answered = ok(
        home,
        &["respond", &attn_id, "SQLite", "--message", "smaller"],
    );
    assert!(answered.trim().ends_with(": sqlite"), "{answered}");
    let evs = poll("turn 2's report", || {
        let evs = events_of(home, "t1");
        (reports(&evs) == 2).then_some(evs)
    });
    let resolved = evs
        .iter()
        .rfind(|e| e["kind"] == "attention.resolved")
        .unwrap();
    assert_eq!(resolved["data"]["answer"]["decision"], "sqlite");
    assert_eq!(resolved["data"]["answer"]["message"], "smaller");
    assert!(evs
        .iter()
        .any(|e| e["kind"] == "agent.text" && e["data"]["text"] == "answer: sqlite"));

    // Nothing left to answer; --all also shows the non-blocking items (none here).
    let none = twr(home, &["respond", "t1", "yes", "-y"]);
    assert!(!none.status.success());
    assert!(String::from_utf8_lossy(&none.stderr).contains("t1 has no open attention item"));
    // A failed turn's `error` item does not block: `twr attention` says it
    // is there, and `twr respond <thread>` does not offer to answer it (only dismiss does).
    ok(home, &["send", "t1", "!error it broke"]);
    let error_row = poll("the error item", || {
        let out = ok(home, &["attention", "--all"]);
        out.lines()
            .find(|l| l.contains(" error "))
            .map(str::to_string)
    });
    assert_eq!(
        ok(home, &["attention"]).trim(),
        "nothing blocks you; 1 other item (twr attention --all)"
    );
    let refused = twr(home, &["respond", "t1", "allow", "-y"]);
    assert!(!refused.status.success());
    let err = String::from_utf8_lossy(&refused.stderr).to_string();
    assert!(
        err.contains("t1 has no open permission or question to answer (1 other item(s)"),
        "{err}"
    );
    assert!(String::from_utf8_lossy(&refused.stdout).is_empty());
    let error_id = error_row.split_whitespace().next().unwrap();
    assert!(!err.contains(error_id), "{err}");
    ok(home, &["attention", "dismiss", error_id]);
    assert_eq!(
        ok(home, &["attention", "--all"]).trim(),
        "nothing needs you"
    );
    let unknown = twr(home, &["respond", "no-such-thing", "yes"]);
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("neither a thread"));

    // `twr say` cannot tell what the user had seen: it never answers an item.
    ok(home, &["send", "t1", "!perm Bash rm -rf build"]);
    poll("the second permission", || {
        ok(home, &["attention"])
            .contains("permission")
            .then_some(())
    });
    // A message that looks like an answer is not sent at all (no "which thread?" question):
    // it fails, naming the pending item and the `twr respond` that answers it.
    let said = twr(home, &["say", "yes"]);
    assert!(!said.status.success());
    let err = String::from_utf8_lossy(&said.stderr).to_string();
    assert!(err.contains("not sent"), "{err}");
    assert!(err.contains("a permission is pending on t1"), "{err}");
    assert_eq!(
        ok(home, &["attention", "--all"]).lines().count(),
        2,
        "only the permission"
    );
    let target = err
        .split("`twr respond ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap()
        .to_string();
    assert!(target.contains('@'), "{target}");
    let answered = ok(home, &["respond", &target, "deny"]);
    assert!(answered.contains("deny"), "{answered}");
    assert_eq!(
        ok(home, &["attention", "--all"]).trim(),
        "nothing needs you"
    );
}

/// A worker's shell is not the user: a `pty` thread's program runs `twr respond`, `twr mode` and
/// `twr trash empty --all`, and the hub refuses each with the thread's name and its runtime's
/// pid, while `twr attention` (a read) works. A background job its shell left behind
/// (reparented away from the runtime) is refused by its agent marker.
/// The permission stays pending for the user, who answers it from the test's own process.
#[test]
fn a_workers_shell_cannot_answer_for_the_user() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let project = home.join("proj");
    std::fs::create_dir(&project).unwrap();
    let _hub = start_hub(home);
    ok(
        home,
        &[
            "thread",
            "new",
            project.to_str().unwrap(),
            "--runtime",
            "fake",
            "--name",
            "t1",
        ],
    );
    ok(home, &["send", "t1", "!perm Bash ls"]);
    poll("the permission item", || {
        ok(home, &["attention"])
            .contains("permission")
            .then_some(())
    });

    let d = home.display();
    // A background job the shell leaves behind (`nohup ... &` from a shell that exits at
    // once): reparented away from the runtime, it still carries its marker.
    let bg = home.join("bg.sh");
    std::fs::write(
        &bg,
        format!(
            "while [ ! -e {d}/go ]; do sleep 0.05; done\n\
             TWR_HOME={d} {TWR} respond t1 allow -y > /dev/null 2> {d}/bg.err\n\
             touch {d}/bg.done\n"
        ),
    )
    .unwrap();
    let script = home.join("agent.sh");
    std::fs::write(
        &script,
        format!(
            "( nohup sh {b} > /dev/null 2>&1 & )\n\
             while [ ! -e {d}/go ]; do sleep 0.05; done\n\
             TWR_HOME={d} {TWR} respond t1 allow -y > /dev/null 2> {d}/respond.err\n\
             TWR_HOME={d} {TWR} mode t1 bypassPermissions > /dev/null 2> {d}/mode.err\n\
             TWR_HOME={d} {TWR} trash empty --all -y > /dev/null 2> {d}/trash.err\n\
             TWR_HOME={d} {TWR} attention > {d}/attention.out 2>&1\n\
             touch {d}/done\n\
             printf '> '\n\
             while read l; do printf '> '; done\n",
            b = bg.display()
        ),
    )
    .unwrap();
    let runtime = format!("pty:sh {}", script.display());
    ok(
        home,
        &[
            "thread",
            "new",
            project.to_str().unwrap(),
            "--runtime",
            &runtime,
            "--name",
            "w1",
        ],
    );
    ok(home, &["send", "w1", "hello"]);
    let pid = poll("w1's runtime", || {
        events_of(home, "w1")
            .iter()
            .find(|e| e["kind"] == "thread.started")
            .and_then(|e| e["data"]["ext"]["twr"]["pid"].as_u64())
    });
    std::fs::write(home.join("go"), "").unwrap();
    poll("w1's commands", || home.join("done").exists().then_some(()));
    poll("w1's background job", || {
        home.join("bg.done").exists().then_some(())
    });
    let err = std::fs::read_to_string(home.join("bg.err")).unwrap();
    assert!(
        err.contains("this request comes from w1's agent (pid ")
            && err.contains("which carries TWR_AGENT"),
        "{err}"
    );
    let refusal =
        format!("this request comes from w1's agent (pid {pid}, a process twr started for it)");
    for what in ["respond", "mode", "trash"] {
        let err = std::fs::read_to_string(home.join(format!("{what}.err"))).unwrap();
        assert!(err.contains(&refusal), "{what}: {err}");
        assert!(err.contains("only you can"), "{what}: {err}");
    }
    let listed = std::fs::read_to_string(home.join("attention.out")).unwrap();
    assert!(listed.contains("permission"), "{listed}");

    // Still pending, and the user can answer it.
    assert!(ok(home, &["attention"]).contains("permission"));
    let t1 = thread_get(home, "t1");
    assert_ne!(t1["mode"], "bypassPermissions", "{t1}");
    ok(home, &["respond", "t1", "allow", "-y"]);
    poll("the answered turn's report", || {
        (reports(&events_of(home, "t1")) == 1).then_some(())
    });
}

#[test]
fn attach_hands_the_thread_to_the_runtime_cli_and_back() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let project = home.join("proj");
    std::fs::create_dir(&project).unwrap();
    let _hub = start_hub(home);
    ok(
        home,
        &[
            "thread",
            "new",
            project.to_str().unwrap(),
            "--runtime",
            "fake",
            "--name",
            "t1",
        ],
    );
    // No session yet.
    let early = twr(home, &["attach", "t1"]);
    assert!(!early.status.success());
    assert!(String::from_utf8_lossy(&early.stderr).contains("unsupported"));

    ok(home, &["send", "t1", "hello"]);
    poll("turn 1's report", || {
        (reports(&events_of(home, "t1")) == 1).then_some(())
    });

    // The attached CLI, once the hub knows its process, tries to queue a prompt (refused: it
    // is the thread's agent now), looks at the thread list, and exits 3. The
    // user queues a prompt from another terminal meanwhile.
    let out_file = home.join("attached.txt");
    let script = r#"while [ ! -e "$TWR_TEST_DIR/go" ]; do sleep 0.05; done; "$TWR_TEST_BIN" send t1 from the attached cli 2> "$TWR_TEST_DIR/send.err"; echo "send=$?" > "$TWR_TEST_OUT"; "$TWR_TEST_BIN" thread list >> "$TWR_TEST_OUT" && pwd >> "$TWR_TEST_OUT" && echo "session=$1 marker=${TWR_THREAD:-none}" >> "$TWR_TEST_OUT"; exit 3"#;
    let client = twr_cmd()
        .args(["attach", "t1"])
        .env("TWR_HOME", home)
        .env("TWR_FAKE_ATTACH", script)
        .env("TWR_TEST_BIN", TWR)
        .env("TWR_TEST_OUT", &out_file)
        .env("TWR_TEST_DIR", home)
        .env("TWR_THREAD", "inherited-marker")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = poll("the hub to know the attached process", || {
        thread_get(home, "t1")["attach_process"]["pid"].as_u64()
    });
    ok(home, &["send", "t1", "during attach"]);
    std::fs::write(home.join("go"), "").unwrap();
    let attached = client.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&attached.stderr);
    assert_eq!(attached.status.code(), Some(3), "{stderr}");
    assert!(stderr.contains("attached to t1"), "{stderr}");
    assert!(stderr.contains("detached from t1"), "{stderr}");
    let refused = std::fs::read_to_string(home.join("send.err")).unwrap();
    assert!(
        refused.contains(&format!(
            "this request comes from t1's agent (pid {pid}, a process twr started for it)"
        )),
        "{refused}"
    );
    let seen = std::fs::read_to_string(&out_file).unwrap();
    assert!(
        seen.starts_with("send=") && !seen.starts_with("send=0"),
        "{seen}"
    );
    let row = seen.lines().find(|l| l.starts_with("t1 ")).unwrap();
    assert!(row.contains(" attached "), "{seen}");
    assert!(row.contains(" fake "), "{seen}");
    let canon = std::fs::canonicalize(&project).unwrap();
    assert!(
        seen.contains(canon.to_str().unwrap()),
        "ran in the project: {seen}"
    );
    assert!(
        seen.contains("marker=none"),
        "no TWR_THREAD for the user's CLI: {seen}"
    );

    // After the detach, the prompt queued meanwhile runs on the resumed session.
    let evs = poll("the queued turn's report", || {
        let evs = events_of(home, "t1");
        (reports(&evs) == 2).then_some(evs)
    });
    let session =
        evs.iter().find(|e| e["kind"] == "thread.started").unwrap()["data"]["session_id"].clone();
    assert!(seen.contains(&format!("session={}", session.as_str().unwrap())));
    let kinds: Vec<&str> = evs.iter().map(|e| e["kind"].as_str().unwrap()).collect();
    let at = kinds.iter().position(|k| *k == "thread.attached").unwrap();
    assert_eq!(
        kinds[at..at + 3],
        ["thread.attached", "thread.detached", "turn.started"]
    );
    assert_eq!(evs[at + 2]["data"]["prompt"], "during attach");
    assert!(!kinds.contains(&"thread.stopped"));
    let started = evs.iter().rfind(|e| e["kind"] == "thread.started").unwrap();
    assert_eq!(started["data"]["session_id"], session);
    assert_eq!(started["data"]["ext"]["fake"]["resumed"], true);
}

/// `thread.get` over the hub socket (the thread object, with `attach_process`).
fn thread_get(home: &Path, thread: &str) -> Value {
    use std::io::Write;
    let mut s = std::os::unix::net::UnixStream::connect(home.join("hub.sock")).unwrap();
    s.set_read_timeout(Some(WAIT)).unwrap();
    writeln!(
        s,
        r#"{{"id":1,"method":"hello","params":{{"client":"cli-test"}}}}"#
    )
    .unwrap();
    writeln!(
        s,
        r#"{{"id":2,"method":"thread.get","params":{{"thread":"{thread}"}}}}"#
    )
    .unwrap();
    let mut lines = BufReader::new(s).lines();
    lines.next().unwrap().unwrap();
    let resp: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
    resp["result"].clone()
}

/// A thread `t1` of the fake runtime in `home`'s project, after its first turn (it has a
/// session to attach to).
fn thread_with_a_session(home: &Path) {
    let project = home.join("proj");
    std::fs::create_dir(&project).unwrap();
    ok(
        home,
        &[
            "thread",
            "new",
            project.to_str().unwrap(),
            "--runtime",
            "fake",
            "--name",
            "t1",
        ],
    );
    ok(home, &["send", "t1", "hello"]);
    poll("turn 1's report", || {
        (reports(&events_of(home, "t1")) == 1).then_some(())
    });
}

/// Kills the user's CLI a test attached (by the pid it wrote) when the test ends, however it
/// ends: a failing test must not leave it looping.
struct KillAttached(std::path::PathBuf);

impl Drop for KillAttached {
    fn drop(&mut self) {
        if let Ok(pid) = std::fs::read_to_string(&self.0) {
            let _ = Command::new("kill")
                .args(["-KILL", pid.trim()])
                .stderr(Stdio::null())
                .status();
        }
    }
}

/// `twr attach t1` running `script` as the user's CLI (stdio not a terminal; stderr goes to
/// `<home>/attach.err`). The script's loops also end once the test's directory is gone.
fn spawn_attach(home: &Path, script: &str, leader: bool) -> Child {
    use std::os::unix::process::CommandExt;
    let mut cmd = twr_cmd();
    cmd.args(["attach", "t1"])
        .env("TWR_HOME", home)
        .env("TWR_FAKE_ATTACH", script)
        .env("TWR_TEST_DIR", home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(home.join("attach.err")).unwrap());
    if leader {
        // As a shell starts a job: `twr attach` leads its own process group.
        cmd.process_group(0);
    }
    cmd.spawn().unwrap()
}

/// The pid of `t1`'s attached process, once the hub has it. Fails at once, with what `client`
/// wrote, if it exits first or says the hub did not record the process.
fn attached_pid(home: &Path, client: &mut Child) -> u64 {
    poll("the hub to know the attached process", || {
        let err = std::fs::read_to_string(home.join("attach.err")).unwrap_or_default();
        let exited = client.try_wait().unwrap();
        if exited.is_some() || err.contains("did not record") {
            panic!("the attached process was not reported ({exited:?}): {err}");
        }
        thread_get(home, "t1")["attach_process"]["pid"].as_u64()
    })
}

/// Kinds of `t1`'s events from its last `thread.attached` on.
fn since_attached(home: &Path) -> Vec<Value> {
    let evs = events_of(home, "t1");
    let at = evs
        .iter()
        .rposition(|e| e["kind"] == "thread.attached")
        .unwrap();
    evs[at..].to_vec()
}

/// `twr attach` killed outright (SIGKILL: it cannot hand anything back) while the user's CLI
/// runs on: the thread stays attached (park refused, queued prompts wait), also across a hub
/// restart, until that CLI exits; then the hub detaches and dispatches.
#[test]
fn a_killed_attach_client_leaves_the_thread_attached_while_its_cli_runs() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let hub = start_hub(home);
    thread_with_a_session(home);

    let script = r#"echo $$ > "$TWR_TEST_DIR/cli.pid"; while [ ! -e "$TWR_TEST_DIR/release" ] && [ -d "$TWR_TEST_DIR" ]; do sleep 0.05; done"#;
    let _cleanup = KillAttached(home.join("cli.pid"));
    let mut client = spawn_attach(home, script, false);
    let pid = attached_pid(home, &mut client);
    let written = poll("the CLI's pid file", || {
        std::fs::read_to_string(home.join("cli.pid")).ok()
    });
    assert_eq!(written.trim(), pid.to_string());
    // It carries the attach and agent markers, never the runtime markers.
    let environ = twr_runner::reap::process_environ(pid as u32).unwrap();
    let environ = String::from_utf8_lossy(&environ);
    assert!(environ.contains("TWR_ATTACH="), "{environ}");
    assert!(environ.contains("TWR_AGENT="), "{environ}");
    assert!(!environ.contains("TWR_THREAD="), "{environ}");

    client.kill().unwrap();
    client.wait().unwrap();
    poll("the hub to notice the client is gone", || {
        (thread_get(home, "t1")["attach_process"]["orphaned"] == true).then_some(())
    });
    ok(home, &["send", "t1", "after the kill"]);
    let check_still_attached = |when: &str| {
        let park = twr(home, &["park", "t1"]);
        assert!(!park.status.success(), "{when}: park went through");
        let err = String::from_utf8_lossy(&park.stderr).to_string();
        assert!(
            err.contains("attached") && err.contains(&format!("process {pid} still runs")),
            "{when}: {err}"
        );
        std::thread::sleep(Duration::from_millis(800));
        let t = thread_get(home, "t1");
        assert_eq!(t["status"], "attached", "{when}: {t}");
        assert_eq!(t["pending"], 1, "{when}: {t}");
        let kinds: Vec<Value> = since_attached(home)
            .into_iter()
            .map(|e| e["kind"].clone())
            .collect();
        assert_eq!(kinds, ["thread.attached"], "{when}");
    };
    check_still_attached("client killed");

    // A hub restart does not end it either.
    drop(hub);
    let _hub = start_hub(home);
    check_still_attached("hub restarted");
    assert_eq!(thread_get(home, "t1")["attach_process"]["pid"], pid);

    // The CLI exits: detached, and the prompt queued meanwhile runs.
    std::fs::write(home.join("release"), "").unwrap();
    poll("the CLI to exit", || {
        twr_runner::reap::process_start_time(pid as u32)
            .is_none()
            .then_some(())
    });
    std::fs::remove_file(home.join("cli.pid")).unwrap();
    let evs = poll("the queued turn", || {
        let evs = since_attached(home);
        evs.iter()
            .any(|e| e["kind"] == "turn.started")
            .then_some(evs)
    });
    assert_eq!(evs[1]["kind"], "thread.detached");
    assert_eq!(
        evs[1]["data"]["ext"]["twr"]["reason"],
        "attach process exited"
    );
    assert_eq!(evs[2]["kind"], "turn.started");
    assert_eq!(evs[2]["data"]["prompt"], "after the kill");
}

/// SIGTERM (or SIGHUP) to `twr attach` goes to the user's CLI; `twr attach` waits for it to
/// exit and only then ends the attach. As a job leader it signals its process group, else the
/// CLI alone.
#[test]
fn sigterm_to_attach_reaches_its_cli_before_the_attach_ends() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let _hub = start_hub(home);
    thread_with_a_session(home);

    for (leader, sig) in [(true, "TERM"), (false, "TERM"), (false, "HUP")] {
        let got = home.join(format!("got-{leader}-{sig}"));
        let _ = std::fs::remove_file(home.join("cli.pid"));
        let script = format!(
            r#"trap 'echo {sig} > "{}"; exit 7' {sig}; echo $$ > "$TWR_TEST_DIR/cli.pid"; while [ -d "$TWR_TEST_DIR" ]; do sleep 0.05; done"#,
            got.display()
        );
        let _cleanup = KillAttached(home.join("cli.pid"));
        let mut client = spawn_attach(home, &script, leader);
        attached_pid(home, &mut client);
        poll("the CLI's trap", || {
            std::fs::read_to_string(home.join("cli.pid")).ok()
        });
        assert!(Command::new("kill")
            .args([format!("-{sig}"), client.id().to_string()])
            .status()
            .unwrap()
            .success());
        let status = poll("`twr attach` to exit", || client.try_wait().unwrap());
        // Exited (the checks below say how): nothing left to kill.
        std::fs::remove_file(home.join("cli.pid")).unwrap();
        let stderr = std::fs::read_to_string(home.join("attach.err")).unwrap();
        assert_eq!(
            status.code(),
            Some(7),
            "{leader} {sig}: {status:?} {stderr}"
        );
        assert_eq!(
            std::fs::read_to_string(&got).unwrap().trim(),
            sig,
            "{leader} {sig}"
        );
        assert!(
            stderr.contains("detached from t1"),
            "{leader} {sig}: {stderr}"
        );
        let evs = since_attached(home);
        let detached = evs.iter().find(|e| e["kind"] == "thread.detached").unwrap();
        assert!(detached["data"].get("ext").is_none(), "{detached}");
    }
}

#[test]
fn socket_paths_too_long_are_refused_with_a_hint() {
    let home = tempfile::tempdir().unwrap();
    let long = home.path().join("s".repeat(120));
    let out = twr(
        home.path(),
        &["--socket", long.to_str().unwrap(), "thread", "list"],
    );
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    let most = format!("at most {} bytes", twr_core::paths::MAX_SOCKET_PATH);
    assert!(err.contains(&most), "{err}");
    assert!(
        err.contains("--socket") && err.contains("TWR_HOME"),
        "{err}"
    );
    assert!(!err.contains("no hub is running"), "{err}");
}

fn mode(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .unwrap_or_else(|e| panic!("{}: {e}", p.display()))
        .permissions()
        .mode()
        & 0o777
}

#[test]
fn the_data_at_rest_is_private_under_umask_022() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let project = home.join("proj");
    std::fs::create_dir(&project).unwrap();
    // A public parent, and a data directory an older twr left public.
    std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o755)).unwrap();
    let data = home.join("data");
    std::fs::create_dir(&data).unwrap();
    std::fs::set_permissions(&data, std::fs::Permissions::from_mode(0o755)).unwrap();
    let log = std::fs::File::create(home.join("hub-test.log")).unwrap();
    let _hub = HubProc(
        Command::new("sh")
            .arg("-c")
            .arg("umask 022 && exec \"$0\" hub")
            .arg(TWR)
            .env("TWR_HOME", home)
            .env_remove("TWR_WORKSPACE")
            .env("TWR_TEST_USER_TOKEN_FILE", test_token())
            .env("TWR_ENABLE_FAKE", "1")
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap(),
    );
    poll("the hub socket", || {
        twr(home, &["thread", "list"])
            .status
            .success()
            .then_some(())
    });
    ok(
        home,
        &[
            "thread",
            "new",
            project.to_str().unwrap(),
            "--runtime",
            "fake",
            "--name",
            "t1",
        ],
    );
    ok(home, &["send", "t1", "hello"]);
    poll("the turn's report", || {
        (reports(&events_of(home, "t1")) > 0).then_some(())
    });
    assert_eq!(mode(home), 0o755, "the parent is not ours to change");
    assert_eq!(mode(&data), 0o700);
    for f in ["twr.db", "twr.db-wal", "twr.db-shm", "twr.lock"] {
        assert_eq!(mode(&data.join(f)), 0o600, "{f}");
    }
    let log = std::fs::read_to_string(home.join("hub-test.log")).unwrap();
    assert!(log.contains("restricted it to its owner"), "{log}");
}

/// `twr context edit <project>` with `EDITOR` set to `editor`. Its temporary copy goes under
/// `home` (`TMPDIR`): a failing editor keeps it on purpose, and the test's directory removes it.
fn context_edit(home: &Path, project: &str, editor: &str) -> Output {
    twr_cmd()
        .args(["context", "edit", project])
        .env("TWR_HOME", home)
        .env("TMPDIR", home)
        .env("EDITOR", editor)
        .env_remove("VISUAL")
        .env_remove("RUST_LOG")
        .output()
        .expect("running twr")
}

#[test]
fn context_edit_runs_the_editor_and_saves_only_changes() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let project = home.join("proj");
    std::fs::create_dir(&project).unwrap();
    let _hub = start_hub(home);
    ok(
        home,
        &[
            "thread",
            "new",
            project.to_str().unwrap(),
            "--runtime",
            "fake",
            "--name",
            "t1",
        ],
    );
    let file = project.join(".twr").join("context.md");
    let out = twr(home, &["context", "show", "proj"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("no standing context"));

    // The editor gets a temporary copy (run through sh, so it may carry arguments).
    let script = home.join("editor.sh");
    std::fs::write(
        &script,
        "case \"$1\" in *.md) ;; *) exit 3 ;; esac\nprintf 'Always run the tests.\\n' >> \"$1\"\n",
    )
    .unwrap();
    let editor = format!("sh {}", script.display());
    let out = context_edit(home, "proj", &editor);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("standing context saved"));
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "Always run the tests.\n"
    );
    assert_eq!(
        ok(home, &["context", "show", "proj"]),
        "Always run the tests.\n"
    );
    let control = ndjson(&ok(home, &["control"]));
    let updates: Vec<&Value> = control
        .iter()
        .filter(|i| i["event"]["kind"] == "context.updated")
        .collect();
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0]["event"]["source"], "user:cli");

    // Unchanged text, or a failing editor: nothing is sent.
    let out = context_edit(home, "proj", "true");
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("no changes"));
    std::fs::write(&script, "echo scratch >> \"$1\"; exit 1\n").unwrap();
    let out = context_edit(home, "proj", &editor);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("the context is unchanged"));
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        "Always run the tests.\n"
    );
    let control = ndjson(&ok(home, &["control"]));
    assert_eq!(
        control
            .iter()
            .filter(|i| i["event"]["kind"] == "context.updated")
            .count(),
        1
    );
    let out = twr(home, &["context", "show", "nope"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not_found"));
}

#[test]
fn park_and_resume_free_and_recreate_the_worktree() {
    use twr_core::testkit::git::{branch_exists, init_repo};
    let home = tempfile::tempdir().unwrap();
    let home = std::fs::canonicalize(home.path()).unwrap();
    let home = home.as_path();
    let repo = init_repo(&home.join("proj"));
    let _hub = start_hub(home);

    let created = ok(
        home,
        &[
            "thread",
            "new",
            repo.to_str().unwrap(),
            "--runtime",
            "fake",
            "--name",
            "wt",
        ],
    );
    let wt = home.join("data").join("worktrees").join("proj").join("wt");
    assert!(
        created.contains(&format!("worktree={}", wt.display())),
        "{created}"
    );
    assert!(wt.join("README.md").exists());
    ok(home, &["send", "wt", "hello"]);
    poll("the first report", || {
        (reports(&events_of(home, "wt")) == 1).then_some(())
    });

    let parked = ok(home, &["park", "wt"]);
    assert_eq!(
        parked.trim(),
        format!("parked wt; removed worktree {} (branch kept)", wt.display())
    );
    assert!(!wt.exists());
    let namespace = twr_core::paths::Paths::with_home(home)
        .worktree_namespace()
        .unwrap();
    let branch = format!("twr/{namespace}/wt");
    assert!(branch_exists(&repo, &branch));
    assert!(!branch_exists(&repo, "twr/wt"));
    let queued = ok(home, &["send", "wt", "after", "resume"]);
    assert!(queued.contains("it is parked"), "{queued}");
    // `thread list` leaves it out and says so; `--all` lists it.
    assert_eq!(
        ok(home, &["thread", "list"]).trim(),
        "1 parked thread hidden: wt (twr thread list --all)"
    );
    let all = ok(home, &["thread", "list", "--all"]);
    assert!(all.contains("parked") && !all.contains("hidden"), "{all}");
    // Parking twice is a conflict.
    let again = twr(home, &["park", "wt"]);
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("already parked"));

    let resumed = ok(home, &["resume", "wt"]);
    assert_eq!(
        resumed.trim(),
        format!(
            "resumed wt; recreated worktree {}; turn 2 started",
            wt.display()
        )
    );
    poll("the second report", || {
        (reports(&events_of(home, "wt")) == 2).then_some(())
    });
    let kinds: Vec<String> = events_of(home, "wt")
        .iter()
        .map(|e| e["kind"].as_str().unwrap().to_string())
        .collect();
    assert!(kinds.contains(&"thread.parked".to_string()));
    assert!(kinds.contains(&"thread.resumed".to_string()));
    assert!(!kinds.contains(&"thread.stopped".to_string()));

    // Dirty worktree: kept, with --keep not needed.
    ok(home, &["send", "wt", "!write scratch.txt wip"]);
    poll("the third report", || {
        (reports(&events_of(home, "wt")) == 3).then_some(())
    });
    let parked = ok(home, &["park", "wt"]);
    assert!(parked.contains("1 uncommitted file(s)"), "{parked}");
    assert!(wt.join("scratch.txt").exists());
}

#[test]
fn info_prints_the_hub_and_its_orchestrator() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    std::fs::write(home.join("config.toml"), "default_runtime = \"fake\"\n").unwrap();
    let _hub = start_hub(home);
    let out = ok(home, &["info"]);
    assert!(out.contains("orchestrator: runtime fake"), "{out}");
    let instance = twr_core::paths::Paths::with_home(home).instance_name();
    assert!(out.contains(&format!("instance:     {instance}")), "{out}");
    let v: Value = serde_json::from_str(&ok(home, &["info", "--json"])).unwrap();
    assert_eq!(v["orchestrator"]["runtime"], "fake");
    assert_eq!(v["instance"], instance);
}

#[test]
fn orchestrator_reset_is_immediate_and_reports_a_fresh_session() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let _hub = start_hub(home);
    ok(
        home,
        &[
            "orchestrator",
            "use",
            "codex",
            "--model",
            "test-model",
            "--no-summary",
        ],
    );
    let before: Value =
        serde_json::from_str(&ok(home, &["orchestrator", "show", "--json"])).unwrap();
    let out = ok(home, &["orchestrator", "reset"]);
    assert!(out.contains("Conversation cleared; no handoff."), "{out}");
    assert!(
        out.contains("History stays in the log; workers stay."),
        "{out}"
    );
    assert_eq!(out.matches("handoff").count(), 1, "{out}");
    let after: Value =
        serde_json::from_str(&ok(home, &["orchestrator", "show", "--json"])).unwrap();
    assert_ne!(before["thread"], after["thread"]);
    assert_eq!(after["runtime"], "codex");
    assert_eq!(after["model"], "test-model");
    assert_eq!(after["retired"], 1);
    let events = events_of(home, after["thread"].as_str().unwrap());
    assert!(events.iter().all(|e| e["kind"] != "turn.started"));
}

#[test]
fn orchestrator_show_and_use_switch_the_runtime() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    std::fs::write(
        home.join("config.toml"),
        "default_runtime = \"fake\"\n[orchestrator]\nruntime = \"claude-code\"\nmodel = \"haiku\"\n",
    )
    .unwrap();
    let _hub = start_hub(home);
    let out = ok(home, &["orchestrator", "show"]);
    assert!(
        out.starts_with("orchestrator: claude-code model haiku\n"),
        "{out}"
    );
    assert!(out.contains("none yet"), "{out}");
    assert!(
        out.contains("can run on:   claude-code, codex, grok-build\n"),
        "{out}"
    );

    // Runtimes that cannot attach the twr tools are refused.
    for bad in ["fake", "pty:bash"] {
        let o = twr(home, &["orchestrator", "use", bad]);
        assert!(!o.status.success(), "{bad}");
        let err = String::from_utf8_lossy(&o.stderr);
        assert!(err.contains("cannot host the orchestrator"), "{bad}: {err}");
    }

    let out = ok(home, &["orchestrator", "use", "codex", "--model", "o5"]);
    assert!(
        out.starts_with("orchestrator is now codex model o5: thread orchestrator ("),
        "{out}"
    );
    assert!(
        out.contains("starts with the next message you say"),
        "{out}"
    );
    assert!(out.contains("new session"), "{out}");
    let out = ok(home, &["orchestrator", "show"]);
    assert!(out.starts_with("orchestrator: codex model o5\n"), "{out}");
    assert!(
        out.contains("configured:   claude-code model haiku"),
        "{out}"
    );
    assert!(ok(home, &["info"]).contains("orchestrator: runtime codex model o5"));

    // The codex orchestrator never ran: nothing to wait for, and the new one starts with the
    // next message (its handoff first).
    let out = ok(home, &["orchestrator", "use", "claude-code"]);
    assert!(
        out.contains("The previous orchestrator (codex model o5, "),
        "{out}"
    );
    assert!(out.contains("stopped and retired"), "{out}");
    assert!(
        out.contains("starts with the next message you say"),
        "{out}"
    );
    assert!(
        out.contains("Handed over as its first turn: 0 recent exchange(s)")
            && out.contains("no summary (the previous orchestrator was not running)"),
        "{out}"
    );
    let list = ok(home, &["thread", "list"]);
    let rows: Vec<&str> = list
        .lines()
        .filter(|l| l.starts_with("orchestrator"))
        .collect();
    assert_eq!(rows.len(), 2, "{list}");
    assert!(
        rows.iter()
            .any(|r| r.contains("retired") && r.contains("codex")),
        "{list}"
    );
    assert!(
        rows.iter()
            .any(|r| !r.contains("retired") && r.contains("claude-code")),
        "{list}"
    );
    let v: Value = serde_json::from_str(&ok(home, &["orchestrator", "show", "--json"])).unwrap();
    assert_eq!(v["runtime"], "claude-code");
    assert_eq!(v["model"], Value::Null);
    assert_eq!(v["retired"], 1);
}

/// Text agents (or prompts) control reaches the terminal with its control characters escaped:
/// `--pretty`, the attention listing, `thread list` and the JSON lines of `events` (where a C1
/// control, which JSON leaves raw, is written as `\u009b`: still the same JSON).
#[test]
fn printed_text_never_carries_control_characters() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let project = home.join("proj");
    std::fs::create_dir(&project).unwrap();
    let _hub = start_hub(home);
    ok(
        home,
        &[
            "thread",
            "new",
            project.to_str().unwrap(),
            "--runtime",
            "fake",
            "--name",
            "t1",
        ],
    );
    let hostile = "go \u{1b}]0;pwned\u{7}\u{1b}[2J\u{9b}31m\r now";
    ok(home, &["send", "t1", hostile]);
    ok(
        home,
        &["send", "t1", &format!("!ask Which{hostile}?|a{hostile}|b")],
    );
    let listing = poll("the question item", || {
        let out = ok(home, &["attention"]);
        out.contains("question").then_some(out)
    });
    let pretty = poll("both turns", || {
        let out = ok(home, &["events", "t1", "--pretty"]);
        (out.matches("turn.started").count() == 2).then_some(out)
    });
    let json = ok(home, &["events", "t1"]);
    let threads = ok(home, &["thread", "list"]);
    for (what, out) in [
        ("attention", &listing),
        ("--pretty", &pretty),
        ("events", &json),
        ("thread list", &threads),
    ] {
        let controls: Vec<char> = out
            .chars()
            .filter(|c| c.is_control() && *c != '\n' && *c != '\t')
            .collect();
        assert!(controls.is_empty(), "{what}: {controls:?} in {out}");
    }
    assert!(pretty.contains("\\u001b]0;pwned\\u0007"), "{pretty}");
    // An error naming a thread that isn't there quotes it escaped too.
    let trim = twr(home, &["trim", hostile]);
    assert!(!trim.status.success());
    let stderr = String::from_utf8(trim.stderr).unwrap();
    let controls: Vec<char> = stderr
        .chars()
        .filter(|c| c.is_control() && *c != '\n' && *c != '\t')
        .collect();
    assert!(controls.is_empty(), "trim: {controls:?} in {stderr}");
    assert!(stderr.contains("go \\u001b]0;pwned"), "{stderr}");
    // The JSON lines still parse, to the same text.
    let prompts: Vec<String> = ndjson(&json)
        .iter()
        .filter(|e| e["kind"] == "turn.started")
        .map(|e| e["data"]["prompt"].as_str().unwrap_or_default().to_string())
        .collect();
    assert_eq!(prompts.first().map(String::as_str), Some(hostile), "{json}");
}

/// `twr hub stop` asks the hub to shut down and returns once its process has exited; with no
/// hub running it has nothing to do. `twr hub` alone still runs the hub.
#[test]
fn hub_stop_waits_for_the_hub_to_exit() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let project = home.join("proj");
    std::fs::create_dir(&project).unwrap();
    let out = ok(home, &["hub", "stop"]);
    assert!(out.starts_with("no hub is running at "), "{out}");

    let mut hub = start_hub(home);
    let pid = hub.0.id();
    // A live runtime for the shutdown to stop first.
    ok(
        home,
        &[
            "thread",
            "new",
            project.to_str().unwrap(),
            "--runtime",
            "fake",
            "--name",
            "t1",
        ],
    );
    ok(home, &["send", "t1", "hello"]);
    poll("the turn's report", || {
        (reports(&events_of(home, "t1")) == 1).then_some(())
    });

    let out = ok(home, &["hub", "stop"]);
    assert_eq!(out.trim(), format!("hub stopped (pid {pid})"));
    // It had exited by the time `hub stop` returned (a zombie until reaped here).
    let status = hub.0.try_wait().unwrap().expect("the hub had exited");
    assert!(status.success(), "{status:?}");
    assert!(!home.join("hub.sock").exists());
    let out = ok(home, &["hub", "stop"]);
    assert!(out.starts_with("no hub is running at "), "{out}");

    // The runtime was stopped by the shutdown, and the log says so.
    let _hub = start_hub(home);
    let evs = events_of(home, "t1");
    let stopped = evs.iter().rev().find(|e| e["kind"] == "thread.stopped");
    assert!(stopped.is_some(), "{evs:?}");
}

/// A deadline preserves the diagnostic for the phase that timed out, including for the
/// existing single stop. The outer bulk deadline must not replace it with a socket hint.
#[test]
fn hub_stop_timeout_reports_an_unresponsive_hub() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let _hung = std::os::unix::net::UnixListener::bind(home.join("hub.sock")).unwrap();
    for args in [
        &["hub", "stop", "--timeout", "1"][..],
        &["hub", "stop", "--all", "--timeout", "1"],
    ] {
        let out = twr_cmd().env("TWR_HOME", home).args(args).output().unwrap();
        assert!(!out.status.success(), "{out:?}");
        let message = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            message.contains("accepts connections but did not answer within 1s"),
            "{message}"
        );
        assert!(!message.contains("another --socket"), "{message}");
    }
}

#[test]
fn hub_stop_timeout_reports_a_runtime_still_shutting_down() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let project = home.join("proj");
    std::fs::create_dir(&project).unwrap();
    let mut hub = start_hub(home);
    let script = home.join("ignore-hup.sh");
    std::fs::write(
        &script,
        format!(
            "trap '' HUP\ntouch {}/ready\nprintf '> '\nwhile read l; do printf '> '; done\n",
            home.display()
        ),
    )
    .unwrap();
    ok(
        home,
        &[
            "thread",
            "new",
            project.to_str().unwrap(),
            "--name",
            "worker",
            "--runtime",
            &format!("pty:sh {}", script.display()),
        ],
    );
    ok(home, &["send", "worker", "hello"]);
    poll("the runtime ignoring SIGHUP", || {
        home.join("ready").exists().then_some(())
    });
    let out = twr_cmd()
        .env("TWR_HOME", home)
        .args(["hub", "stop", "--timeout", "1"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "{out:?}");
    let e = String::from_utf8(out.stderr).unwrap();
    assert!(
        e.contains(&format!(
            "the hub (pid {}) is still shutting down after 1s",
            hub.0.id()
        )),
        "{e}"
    );
    assert!(
        e.contains("it stops its runtimes first") && !e.contains("another --socket"),
        "{e}"
    );
    poll("the hub completing its shutdown", || {
        hub.0.try_wait().unwrap()
    });
}

/// A file of the test signing key pair (tests/fixtures/signing): the update tests sign with it,
/// and a debug twr verifies with its public half when `TWR_TEST_RELEASE_KEY_FILE` names it.
fn signing_fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/signing")
        .join(name)
}

/// The test key pair, to sign with.
fn test_signer() -> ring::signature::Ed25519KeyPair {
    let pem = std::fs::read_to_string(signing_fixture("test-key.pem")).unwrap();
    let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
    let der = twr_core::b64::decode(&body).unwrap();
    ring::signature::Ed25519KeyPair::from_pkcs8_maybe_unchecked(&der).unwrap()
}

/// Writes `<file>.sig` next to `file`, as the release workflow does: base64 of the raw Ed25519
/// signature by `signer`.
fn sign_file(signer: &ring::signature::Ed25519KeyPair, file: &Path) {
    let data = std::fs::read(file).unwrap();
    let mut sig = file.as_os_str().to_owned();
    sig.push(".sig");
    std::fs::write(sig, twr_core::b64::encode(signer.sign(&data).as_ref())).unwrap();
}

/// The SHA-256 of `data` in hex, as sha256sums.txt lists it.
fn sha256_hex(data: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, data)
        .as_ref()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// A release of a fake twr 9.9.9 for this machine, as install.sh expects it, in `dir`: the
/// archive and its sha256sums.txt, which names the release v9.9.9 as the release workflow
/// writes it. Returns the archive's name.
fn fake_release(dir: &Path, stage: &Path) -> String {
    fake_release_of(dir, stage, "#!/bin/sh\necho 'twr 9.9.9'\n")
}

/// [`fake_release`] whose twr is the shell script `twr`.
fn fake_release_of(dir: &Path, stage: &Path, twr: &str) -> String {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).unwrap();
    std::fs::create_dir_all(stage).unwrap();
    std::fs::write(stage.join("twr"), twr).unwrap();
    std::fs::set_permissions(stage.join("twr"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let os = match std::env::consts::OS {
        "macos" => "apple-darwin",
        _ => "unknown-linux-musl",
    };
    let asset = format!("twr-{}-{os}.tar.gz", std::env::consts::ARCH);
    let packed = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "tar -czf \"$1/{asset}\" -C \"$2\" twr && cd \"$1\" && \
             {{ echo '# twr release v9.9.9'; sha256sum {asset} 2>/dev/null || \
             shasum -a 256 {asset}; }} > sha256sums.txt"
        ))
        .arg("pack")
        .arg(dir)
        .arg(stage)
        .status()
        .unwrap();
    assert!(packed.success());
    asset
}

/// The repository's install.sh.
fn install_sh() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../install.sh")
        .canonicalize()
        .unwrap()
}

/// `twr update` runs the release's installer for its own directory (here the repository's
/// install.sh and a fake release, both from file:// URLs) with the checksums it verified, only
/// when both signatures verify; refuses a `twr` in cargo's bin directory, a download that is not
/// the installer and a failed installer; and then names a hub still running the old version.
#[test]
fn update_reinstalls_the_running_executable() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = |name: &str| {
        let d = tmp.path().join(name);
        std::fs::create_dir_all(&d).unwrap();
        d
    };
    let (home, bin, cargo_bin, temp) = (dir("home"), dir("bin"), dir("cargo/bin"), dir("tmp"));
    // The binary under test, at a path the update may replace (a hard link: renaming the new
    // one over it leaves the build's binary alone).
    let link = |to: &Path| {
        if std::fs::hard_link(TWR, to).is_err() {
            std::fs::copy(TWR, to).unwrap();
        }
    };
    link(&bin.join("twr"));
    link(&cargo_bin.join("twr"));

    // The archive of a fake twr 9.9.9 and its checksums, which every release below holds.
    let archives = tmp.path().join("archives");
    let asset = fake_release(&archives, &tmp.path().join("stage"));
    let sums = std::fs::read_to_string(archives.join("sha256sums.txt")).unwrap();
    // The checksums of v9.9.9 with `script` as its install.sh, as the release workflow writes
    // them.
    let sums_with = |script: &[u8]| format!("{sums}{}  install.sh\n", sha256_hex(script));
    // A signed mirror release (the directory of TWR_INSTALL_SCRIPT_URL): `script` as the
    // installer `file` (signed in `<file>.sig`), the archive and the signed checksums. Returns
    // the installer's path.
    let signer = test_signer();
    let release_as = |name: &str, file: &str, script: &[u8]| {
        let d = dir(name);
        std::fs::write(d.join(file), script).unwrap();
        std::fs::copy(archives.join(&asset), d.join(&asset)).unwrap();
        std::fs::write(d.join("sha256sums.txt"), sums_with(script)).unwrap();
        sign_file(&signer, &d.join(file));
        sign_file(&signer, &d.join("sha256sums.txt"));
        d.join(file)
    };
    let release = |name: &str, script: &[u8]| release_as(name, "install.sh", script);
    let real = release("release", &std::fs::read(install_sh()).unwrap());

    // No TWR_DOWNLOAD_URL: `twr update` names the mirror to install.sh. gh is signed out and
    // https goes nowhere, so a download from GitHub would fail.
    let gh_config = dir("gh-config");
    let update_with = |exe: &Path,
                       installer: &Path,
                       socket: Option<&Path>,
                       test_key: bool,
                       version: Option<&str>| {
        let mut cmd = Command::new(exe);
        if let Some(socket) = socket {
            cmd.arg("--socket").arg(socket);
        }
        cmd.arg("update");
        if let Some(version) = version {
            cmd.args(["--version", version]);
        }
        cmd.env("TWR_HOME", &home)
            .env("TMPDIR", &temp)
            .env("CARGO_HOME", tmp.path().join("cargo"))
            .env(
                "TWR_INSTALL_SCRIPT_URL",
                format!("file://{}", installer.display()),
            )
            .env("GH_CONFIG_DIR", &gh_config)
            .env("https_proxy", "http://127.0.0.1:9")
            .env("HTTPS_PROXY", "http://127.0.0.1:9")
            .env_remove("GH_TOKEN")
            .env_remove("GITHUB_TOKEN")
            .env_remove("TWR_DOWNLOAD_URL")
            .env_remove("TWR_VERSION")
            .env_remove("TWR_INSTALL_DIR")
            .env_remove("TWR_SHA256SUMS_FILE")
            .env_remove("RUST_LOG");
        if test_key {
            cmd.env("TWR_TEST_RELEASE_KEY_FILE", signing_fixture("test-key.pub"));
        } else {
            cmd.env_remove("TWR_TEST_RELEASE_KEY_FILE");
        }
        cmd.output().unwrap()
    };
    let update =
        |exe: &Path, installer: &Path| update_with(exe, installer, None, true, Some("9.9.9"));
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();

    // cargo's binary is left to cargo.
    let out = update(&cargo_bin.join("twr"), &real);
    assert!(!out.status.success());
    let err = text(&out.stderr);
    assert!(err.contains("cargo's bin directory"), "{err}");
    assert!(err.contains("cargo uninstall twr-cli"), "{err}");

    // Something that is not the installer is never run, signed or not.
    let page = release("page", b"<!doctype html>\n<p>not found</p>\n");
    let out = update(&bin.join("twr"), &page);
    assert!(!out.status.success());
    let err = text(&out.stderr);
    assert!(err.contains("does not start with #!/bin/sh"), "{err}");

    // Nothing runs unless both signatures verify with the key: a changed installer or
    // checksums file, a missing signature, or a release signed with another key (the test key
    // when twr checks with the release key).
    let marker = bin.join("ran");
    let script = format!("#!/bin/sh\ntouch '{}'\n", marker.display());
    let refused = |name: &str, change: &dyn Fn(&Path), test_key: bool, expected: &str| {
        let installer = release(name, script.as_bytes());
        change(installer.parent().unwrap());
        let out = update_with(&bin.join("twr"), &installer, None, test_key, Some("9.9.9"));
        assert!(!out.status.success(), "{name}: {out:?}");
        let err = text(&out.stderr);
        assert!(err.contains(expected), "{name}: {err}");
        assert!(err.contains("nothing was run"), "{name}: {err}");
        assert!(!marker.exists(), "{name}: the installer ran");
    };
    let append = |file: &Path, more: &str| {
        let mut data = std::fs::read(file).unwrap();
        data.extend_from_slice(more.as_bytes());
        std::fs::write(file, data).unwrap();
    };
    refused(
        "changed-installer",
        &|d| append(&d.join("install.sh"), "echo more\n"),
        true,
        "install.sh does not match its signature",
    );
    refused(
        "changed-sums",
        &|d| {
            append(
                &d.join("sha256sums.txt"),
                &format!("{}  x\n", "1".repeat(64)),
            )
        },
        true,
        "sha256sums.txt does not match its signature",
    );
    refused(
        "unsigned-sums",
        &|d| std::fs::remove_file(d.join("sha256sums.txt.sig")).unwrap(),
        true,
        "sha256sums.txt.sig: no signature",
    );
    refused(
        "other-key",
        &|_| {},
        false,
        "install.sh does not match its signature",
    );
    // Signed files that are not of the release asked for (9.9.9): another release's
    // checksums, and checksums that list another installer (one release's install.sh with
    // another's checksums).
    let resign = |d: &Path, text: &str| {
        std::fs::write(d.join("sha256sums.txt"), text).unwrap();
        sign_file(&signer, &d.join("sha256sums.txt"));
    };
    refused(
        "other-release",
        &|d| resign(d, &sums_with(script.as_bytes()).replace("v9.9.9", "v9.9.8")),
        true,
        "is the checksums of v9.9.8, not of v9.9.9",
    );
    refused(
        "other-installer",
        &|d| resign(d, &sums_with(b"#!/bin/sh\necho older\n")),
        true,
        "is not the install.sh of v9.9.9",
    );
    // A bad version never reaches a URL.
    let out = Command::new(bin.join("twr"))
        .args(["update", "--version", "v1.0.0/../../x"])
        .env("TWR_HOME", &home)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        text(&out.stderr).contains("not a release version"),
        "{out:?}"
    );

    // A failing installer fails the update, its own message shown.
    let failing = release(
        "failing",
        b"#!/bin/sh\necho \"twr install: no release for $TWR_VERSION in $TWR_INSTALL_DIR\" >&2\nexit 3\n",
    );
    let out = update(&bin.join("twr"), &failing);
    assert!(!out.status.success());
    let err = text(&out.stderr);
    assert!(
        err.contains(&format!(
            "twr install: no release for v9.9.9 in {}",
            bin.canonicalize().unwrap().display()
        )),
        "{err}"
    );
    assert!(err.contains("the installer failed"), "{err}");

    // A mirror's release is the one installed, archive included, with or without a version:
    // install.sh downloads from the mirror's directory, and gets the release its checksums
    // name as TWR_VERSION. A differently named installer is signed beside it, in `<name>.sig`
    // (there is no install.sh.sig here).
    let env_dump = release_as(
        "custom",
        "custom-installer.sh",
        b"#!/bin/sh\necho \"url=${TWR_DOWNLOAD_URL-unset} version=${TWR_VERSION-unset}\"\n",
    );
    let mirror = format!("file://{}", env_dump.parent().unwrap().display());
    for (version, expected) in [
        (None, "v9.9.9"),
        (Some("latest"), "v9.9.9"),
        (Some("9.9.9"), "v9.9.9"),
    ] {
        let out = update_with(&bin.join("twr"), &env_dump, None, true, version);
        assert!(out.status.success(), "{version:?}: {out:?}");
        assert_eq!(
            text(&out.stdout).lines().next(),
            Some(format!("url={mirror} version={expected}").as_str()),
            "{version:?}"
        );
    }
    let out = update_with(&bin.join("twr"), &real, None, true, None);
    assert!(out.status.success(), "{out:?}");
    let stdout = text(&out.stdout);
    assert!(stdout.contains("Updating twr to v9.9.9"), "{stdout}");
    assert!(stdout.contains("Installed twr 9.9.9"), "{stdout}");
    std::fs::remove_file(bin.join("twr")).unwrap();
    link(&bin.join("twr"));

    // The real installer replaces bin/twr; the running hub is named as still on the old version.
    let _hub = start_hub(&home);
    let out = update(&bin.join("twr"), &real);
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert!(out.status.success(), "{stdout}\n{stderr}");
    assert!(!stderr.contains("not signature-checked"), "{stderr}");
    let bin = bin.canonicalize().unwrap();
    assert!(
        stdout.contains(&format!("Installed twr 9.9.9 to {}/twr", bin.display())),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "is still twr {}. To restart it on 9.9.9, run `twr hub stop`, then `twr`",
            env!("CARGO_PKG_VERSION")
        )),
        "{stdout}"
    );
    assert!(stdout.contains("twr hub stop --all"), "{stdout}");
    let custom_bin = dir("custom-bin");
    link(&custom_bin.join("twr"));
    let explicit_socket = home.join("hub.sock");
    let out = update_with(
        &custom_bin.join("twr"),
        &real,
        Some(&explicit_socket),
        true,
        Some("9.9.9"),
    );
    let stdout = text(&out.stdout);
    assert!(out.status.success(), "{out:?}");
    assert!(
        stdout.contains(&format!(
            "twr --socket {} hub stop",
            explicit_socket.display()
        )),
        "{stdout}"
    );
    assert!(!stdout.contains("twr hub stop --all"), "{stdout}");
    let installed = std::fs::read_to_string(bin.join("twr")).unwrap();
    assert!(installed.contains("twr 9.9.9"), "{installed}");
    // The build's binary is untouched, and no temporary file is left behind.
    assert!(twr_cmd()
        .arg("--version")
        .output()
        .unwrap()
        .status
        .success());
    let left: Vec<_> = std::fs::read_dir(&temp).unwrap().collect();
    assert!(left.is_empty(), "{left:?}");
}

/// install.sh checks sha256sums.txt against its signature when openssl here can: run against a
/// file:// release signed with the test key, by a copy of install.sh whose release key block
/// holds the test key. Skipped where openssl can't verify Ed25519.
#[test]
fn install_sh_checks_the_signature_of_the_checksums() {
    let tmp = tempfile::tempdir().unwrap();
    let pub_pem = std::fs::read_to_string(signing_fixture("test-key.pub")).unwrap();
    let probe = std::fs::read_to_string(signing_fixture("probe.sig")).unwrap();
    let probe_ok = Command::new("sh")
        .arg("-c")
        .arg(
            "printf 'twr signature probe' > \"$1/msg\" && \
             openssl base64 -d -A -in \"$2\" -out \"$1/sig\" && \
             openssl pkeyutl -verify -rawin -pubin -inkey \"$3\" -sigfile \"$1/sig\" -in \"$1/msg\"",
        )
        .arg("probe")
        .arg(tmp.path())
        .arg(signing_fixture("probe.sig"))
        .arg(signing_fixture("test-key.pub"))
        .output()
        .is_ok_and(|o| o.status.success());
    if !probe_ok {
        eprintln!("skipped: openssl here does not verify Ed25519 signatures");
        return;
    }

    // install.sh with the test key in its release key block.
    let original = std::fs::read_to_string(install_sh()).unwrap();
    let (head, rest) = original
        .split_once("# --- release key ---\n")
        .expect("install.sh has a release key block");
    let (_, tail) = rest
        .split_once("# --- end release key ---\n")
        .expect("install.sh's release key block ends");
    let script = tmp.path().join("install.sh");
    std::fs::write(
        &script,
        format!(
            "{head}# --- release key ---\nRELEASE_KEY='{}'\nPROBE_MESSAGE='twr signature probe'\n\
             PROBE_SIGNATURE='{}'\n# --- end release key ---\n{tail}",
            pub_pem.trim_end(),
            probe.trim()
        ),
    )
    .unwrap();

    let release = tmp.path().join("release");
    fake_release(&release, &tmp.path().join("stage"));
    let signer = test_signer();
    sign_file(&signer, &release.join("sha256sums.txt"));
    let good_sums = std::fs::read(release.join("sha256sums.txt")).unwrap();
    let good_sig = std::fs::read(release.join("sha256sums.txt.sig")).unwrap();
    let reset = || {
        std::fs::write(release.join("sha256sums.txt"), &good_sums).unwrap();
        std::fs::write(release.join("sha256sums.txt.sig"), &good_sig).unwrap();
    };
    // An openssl that can't do Ed25519 (as macOS's LibreSSL), first on PATH when asked.
    let no_openssl = tmp.path().join("no-openssl");
    std::fs::create_dir(&no_openssl).unwrap();
    std::fs::write(no_openssl.join("openssl"), "#!/bin/sh\nexit 1\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        let exe = std::fs::Permissions::from_mode(0o755);
        std::fs::set_permissions(no_openssl.join("openssl"), exe).unwrap();
    }

    let mut n = 0;
    let mut run = |env: &[(&str, &str)], verifier: bool| {
        n += 1;
        let dir = tmp.path().join(format!("bin{n}"));
        let mut cmd = Command::new("sh");
        cmd.arg(&script)
            .env("HOME", tmp.path())
            .env("TWR_INSTALL_DIR", &dir)
            .env("TWR_VERSION", "v9.9.9")
            .env("TWR_DOWNLOAD_URL", format!("file://{}", release.display()));
        for k in [
            "TWR_SHA256SUMS_FILE",
            "TWR_ALLOW_UNSIGNED",
            "TWR_REQUIRE_SIGNATURE",
            "TWR_UPDATE",
        ] {
            cmd.env_remove(k);
        }
        if !verifier {
            let path = std::env::var("PATH").unwrap_or_default();
            cmd.env("PATH", format!("{}:{path}", no_openssl.display()));
        }
        for (k, v) in env {
            cmd.env(k, v);
        }
        let out = cmd.output().unwrap();
        let installed = dir.join("twr").exists();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert_eq!(out.status.success(), installed, "{env:?}: {out:?}");
        (installed, stderr)
    };

    // A good signature installs, without a warning.
    let (installed, stderr) = run(&[], true);
    assert!(installed, "{stderr}");
    assert!(stderr.is_empty(), "{stderr}");
    // ... the release asked for, or the latest: the checksums name their release (v9.9.9),
    // and signed checksums of another release than the one asked for are refused.
    let (installed, stderr) = run(&[("TWR_VERSION", "")], true);
    assert!(installed, "{stderr}");
    let (installed, stderr) = run(&[("TWR_VERSION", "9.9.8")], true);
    assert!(!installed);
    assert!(
        stderr.contains("is the checksums of v9.9.9, not of v9.9.8")
            && stderr.contains("nothing was installed"),
        "{stderr}"
    );
    // So are signed checksums that name no release.
    let unnamed = String::from_utf8(good_sums.clone())
        .unwrap()
        .replace("# twr release v9.9.9\n", "");
    std::fs::write(release.join("sha256sums.txt"), &unnamed).unwrap();
    sign_file(&signer, &release.join("sha256sums.txt"));
    let (installed, stderr) = run(&[("TWR_VERSION", "")], true);
    assert!(!installed);
    assert!(stderr.contains("does not name one release"), "{stderr}");
    reset();

    // Changed checksums are refused (the archive still matches them).
    let mut more = good_sums.clone();
    more.extend_from_slice(format!("{}  other.tar.gz\n", "1".repeat(64)).as_bytes());
    std::fs::write(release.join("sha256sums.txt"), &more).unwrap();
    let (installed, stderr) = run(&[], true);
    assert!(!installed);
    assert!(
        stderr.contains("the signature of sha256sums.txt does not verify"),
        "{stderr}"
    );
    assert!(stderr.contains("nothing was installed"), "{stderr}");
    reset();

    // So is another key's signature, and one that is no signature.
    let rng = ring::rand::SystemRandom::new();
    let other = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
    let other = ring::signature::Ed25519KeyPair::from_pkcs8(other.as_ref()).unwrap();
    sign_file(&other, &release.join("sha256sums.txt"));
    let (installed, stderr) = run(&[], true);
    assert!(!installed);
    assert!(stderr.contains("does not verify"), "{stderr}");
    std::fs::write(release.join("sha256sums.txt.sig"), "c2lnbmVk\n").unwrap();
    let (installed, stderr) = run(&[], true);
    assert!(!installed);
    assert!(stderr.contains("is not an Ed25519 signature"), "{stderr}");

    // So is a missing signature: every release is signed. Nothing turns that off (the
    // TWR_ALLOW_UNSIGNED=1 that older scripts read is ignored), for a missing signature as for
    // a bad one.
    let (installed, stderr) = run(&[("TWR_ALLOW_UNSIGNED", "1")], true);
    assert!(!installed);
    assert!(stderr.contains("is not an Ed25519 signature"), "{stderr}");
    sign_file(&other, &release.join("sha256sums.txt"));
    let (installed, stderr) = run(&[("TWR_ALLOW_UNSIGNED", "1")], true);
    assert!(!installed);
    assert!(stderr.contains("does not verify"), "{stderr}");
    std::fs::remove_file(release.join("sha256sums.txt.sig")).unwrap();
    for env in [&[][..], &[("TWR_ALLOW_UNSIGNED", "1")][..]] {
        let (installed, stderr) = run(env, true);
        assert!(!installed, "{env:?}");
        assert!(
            stderr.contains("could not download sha256sums.txt.sig")
                && stderr.contains("nothing was installed"),
            "{stderr}"
        );
        assert!(!stderr.contains("TWR_ALLOW_UNSIGNED"), "{stderr}");
    }

    // Without a verifier: a warning, and the install goes on, unless TWR_REQUIRE_SIGNATURE=1.
    let (installed, stderr) = run(&[], false);
    assert!(installed, "{stderr}");
    assert!(
        stderr.contains(
            "twr install: the checksums were not signature-checked (no openssl with Ed25519 support)"
        ),
        "{stderr}"
    );
    let (installed, stderr) = run(&[("TWR_REQUIRE_SIGNATURE", "1")], false);
    assert!(!installed);
    assert!(stderr.contains("TWR_REQUIRE_SIGNATURE=1"), "{stderr}");
    reset();

    // TWR_SHA256SUMS_FILE: that file, not a download, and no signature check here (the caller
    // checked it): the release's own checksums and signature may be anything, or missing.
    let given = tmp.path().join("verified-sums.txt");
    std::fs::write(&given, &good_sums).unwrap();
    std::fs::remove_file(release.join("sha256sums.txt")).unwrap();
    std::fs::remove_file(release.join("sha256sums.txt.sig")).unwrap();
    let given = given.to_str().unwrap();
    let (installed, stderr) = run(&[("TWR_SHA256SUMS_FILE", given)], true);
    assert!(installed, "{stderr}");
    assert!(stderr.is_empty(), "{stderr}");
    let (installed, stderr) = run(
        &[
            ("TWR_SHA256SUMS_FILE", given),
            ("TWR_REQUIRE_SIGNATURE", "1"),
        ],
        false,
    );
    assert!(installed, "{stderr}");
    // ... the release it names must still be the one asked for ...
    let (installed, stderr) = run(
        &[("TWR_SHA256SUMS_FILE", given), ("TWR_VERSION", "v9.9.8")],
        true,
    );
    assert!(!installed);
    assert!(stderr.contains("not of v9.9.8"), "{stderr}");
    // ... and it is still the checksum the archive is checked against.
    let mut other_archive = b"# twr release v9.9.9\n".to_vec();
    other_archive.extend_from_slice(&more[good_sums.len()..]);
    std::fs::write(tmp.path().join("verified-sums.txt"), &other_archive).unwrap();
    let (installed, stderr) = run(&[("TWR_SHA256SUMS_FILE", given)], true);
    assert!(!installed);
    assert!(
        stderr.contains("is not listed in sha256sums.txt"),
        "{stderr}"
    );
}

/// A thread whose runtime failed to start keeps the rest of its queue held, but a prompt held
/// for it with `--after` still goes out by itself once the thread it waits for finishes, and
/// its dispatch releases that queue: `twr wait` waits (here until its timeout) instead of
/// calling the failure final.
#[test]
fn wait_after_a_start_failure_waits_for_a_held_prompt() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let project = home.join("proj");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(
        home.join("config.toml"),
        "[orchestrator]\nruntime = \"none\"\n",
    )
    .unwrap();
    let _hub = start_hub_with(home, &[("TWR_CODEX_BIN", "/nonexistent/codex")]);
    for name in ["x", "dep"] {
        ok(
            home,
            &[
                "thread",
                "new",
                project.to_str().unwrap(),
                "--runtime",
                "fake",
                "--name",
                name,
            ],
        );
    }
    ok(home, &["send", "dep", "!sleep", "120000"]);
    // x runs a turn while two more prompts queue, and moves to a runtime that cannot start:
    // the next prompt fails to start it, and the last one stays queued behind the failure.
    ok(home, &["send", "x", "!sleep", "2000"]);
    ok(home, &["send", "x", "second"]);
    ok(home, &["send", "x", "third"]);
    ok(home, &["model", "x", "--runtime", "codex"]);
    poll("x's start failure", || {
        let t = thread_get(home, "x");
        (t["last_turn"]["result"] == "error" && t["pending"] == 1).then_some(())
    });
    ok(home, &["send", "x", "--after", "dep", "after", "dep"]);
    let out = twr(home, &["wait", "x", "--timeout", "12s"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(124), "{stdout}");
}

/// install.sh installs only a twr that runs here: one whose `--version` prints a version and
/// then fails is refused, the twr already installed stays as it was, and no staged copy is
/// left behind.
#[test]
fn install_sh_keeps_the_old_twr_when_the_new_one_fails_to_run() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let release = tmp.path().join("release");
    fake_release_of(
        &release,
        &tmp.path().join("stage"),
        "#!/bin/sh\necho 'twr 9.9.9'\nexit 1\n",
    );
    let bin = tmp.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let old = "#!/bin/sh\necho 'twr 0.1.0'\n";
    std::fs::write(bin.join("twr"), old).unwrap();
    std::fs::set_permissions(bin.join("twr"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = Command::new("sh")
        .arg(install_sh())
        .env("HOME", tmp.path())
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("TWR_INSTALL_DIR", &bin)
        .env("TWR_VERSION", "v9.9.9")
        .env("TWR_DOWNLOAD_URL", format!("file://{}", release.display()))
        .env("TWR_SHA256SUMS_FILE", release.join("sha256sums.txt"))
        .env_remove("TWR_UPDATE")
        .env_remove("TWR_REQUIRE_SIGNATURE")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!out.status.success(), "{out:?}");
    assert!(
        stderr.contains("does not run on this machine") && stderr.contains("nothing was installed"),
        "{stderr}"
    );
    assert_eq!(std::fs::read_to_string(bin.join("twr")).unwrap(), old);
    let left: Vec<_> = std::fs::read_dir(&bin)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(left, ["twr"], "{left:?}");
}

/// install.sh's advice for an install directory that is not on PATH works pasted as shown,
/// whatever the directory's name holds (a space, quotes, `$`, a backquote, a backslash): for
/// sh-family shells a line appended to the profile that puts the directory on PATH once
/// sourced, for fish a `fish_add_path` of the directory itself.
#[test]
fn install_sh_path_advice_works_as_shown() {
    let tmp = tempfile::tempdir().unwrap();
    let release = tmp.path().join("release");
    fake_release(&release, &tmp.path().join("stage"));
    let odd = tmp.path().join(r#"my bin's "x" $HOME `id` a\b"#);
    let run = |shell: &str| {
        let out = Command::new("sh")
            .arg(install_sh())
            .env("HOME", tmp.path())
            .env("SHELL", shell)
            .env("PATH", "/usr/bin:/bin")
            .env("TWR_INSTALL_DIR", &odd)
            .env("TWR_VERSION", "v9.9.9")
            .env("TWR_DOWNLOAD_URL", format!("file://{}", release.display()))
            .env("TWR_SHA256SUMS_FILE", release.join("sha256sums.txt"))
            .env_remove("TWR_UPDATE")
            .env_remove("TWR_REQUIRE_SIGNATURE")
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        assert!(odd.join("twr").exists());
        let stdout = String::from_utf8(out.stdout).unwrap();
        let advice = stdout
            .lines()
            .skip_while(|l| !l.ends_with("is not on your PATH. Add it with:"))
            .nth(2)
            .unwrap_or_else(|| panic!("{stdout}"))
            .trim()
            .to_string();
        advice
    };

    // sh, bash, zsh: run as shown, the profile line puts the directory on PATH.
    let advice = run("/bin/sh");
    assert!(advice.ends_with(" >> ~/.profile"), "{advice}");
    let ran = Command::new("sh")
        .arg("-c")
        .arg(&advice)
        .env("HOME", tmp.path())
        .status()
        .unwrap();
    assert!(ran.success(), "{advice}");
    let path = Command::new("sh")
        .arg("-c")
        .arg(r#". "$HOME/.profile"; printf %s "$PATH""#)
        .env("HOME", tmp.path())
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(path.stdout).unwrap(),
        format!("{}:/usr/bin:/bin", odd.display()),
        "{advice}"
    );

    // fish: the directory in single quotes, `\` and `'` escaped as fish reads them.
    let advice = run("/usr/bin/fish");
    let quoted = advice
        .strip_prefix("fish_add_path '")
        .and_then(|a| a.strip_suffix('\''))
        .unwrap_or_else(|| panic!("{advice}"));
    let mut read = String::new();
    let mut chars = quoted.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(e @ ('\\' | '\'')) => read.push(e),
                Some(other) => {
                    read.push('\\');
                    read.push(other);
                }
                None => read.push('\\'),
            },
            '\'' => panic!("an unescaped quote ends the word early: {advice}"),
            c => read.push(c),
        }
    }
    assert_eq!(read, odd.to_str().unwrap(), "{advice}");
}

/// docs/tools/make-signing-key.sh, run from a scratch copy of the repository (its install.sh,
/// no release/): it makes the key exclusively (never through a link or anything else at its
/// name), writes the public key and install.sh's block, and prints a `gh secret set` that reads
/// the key pasted as shown. A run that fails after making the key, or is hung up, removes the
/// key; a run again makes a new one and writes both files. Skipped without OpenSSL 3.
#[test]
fn make_signing_key_makes_a_key_only_where_nothing_is() {
    use std::os::unix::fs::PermissionsExt;
    let version = Command::new("openssl").arg("version").output();
    if !version.is_ok_and(|o| String::from_utf8_lossy(&o.stdout).starts_with("OpenSSL 3")) {
        eprintln!("skipped: no OpenSSL 3 here");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("repo");
    std::fs::create_dir_all(repo.join("docs/tools")).unwrap();
    let script = repo.join("docs/tools/make-signing-key.sh");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/tools/make-signing-key.sh");
    std::fs::copy(&source, &script).unwrap();
    std::fs::copy(install_sh(), repo.join("install.sh")).unwrap();
    let block = |installer: &str| {
        let (_, rest) = installer.split_once("# --- release key ---\n").unwrap();
        rest.split_once("# --- end release key ---\n")
            .unwrap()
            .0
            .to_string()
    };
    let original = std::fs::read_to_string(repo.join("install.sh")).unwrap();
    let keys = tmp.path().join("my key's dir");
    let key = keys.join("signing-key.pem");
    let run = |path: Option<&Path>| {
        let mut cmd = Command::new("sh");
        cmd.arg(&script).arg(&keys);
        if let Some(p) = path {
            let inherited = std::env::var("PATH").unwrap_or_default();
            cmd.env("PATH", format!("{}:{inherited}", p.display()));
        }
        cmd
    };
    let leftovers = || {
        std::fs::read_dir(&keys)
            .map(|d| d.map(|e| e.unwrap().file_name()).collect::<Vec<_>>())
            .unwrap_or_default()
    };

    // Something at the key's name, even a dangling link: refused, nothing written.
    std::fs::create_dir_all(&keys).unwrap();
    std::os::unix::fs::symlink(tmp.path().join("nowhere"), &key).unwrap();
    let out = run(None).output().unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("already exists"),
        "{out:?}"
    );
    assert!(!tmp.path().join("nowhere").exists());
    assert!(!repo.join("release").exists());
    std::fs::remove_file(&key).unwrap();

    // A failure after the key was made (release/ can't be made): the key goes, and it says so.
    std::fs::write(repo.join("release"), "").unwrap();
    let out = run(None).output().unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("the new key") && stderr.contains("was removed"),
        "{stderr}"
    );
    assert!(leftovers().is_empty(), "{:?}", leftovers());
    // (install.sh, written before, holds the removed key's block: a run again rewrites it.)
    std::fs::remove_file(repo.join("release")).unwrap();
    let failed = std::fs::read_to_string(repo.join("install.sh")).unwrap();
    assert_ne!(block(&failed), block(&original));

    // Hung up while making the key: the key and its temporary file go.
    let slow = tmp.path().join("slow-openssl");
    std::fs::create_dir(&slow).unwrap();
    let real = String::from_utf8(
        Command::new("sh")
            .args(["-c", "command -v openssl"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let started = tmp.path().join("started");
    std::fs::write(
        slow.join("openssl"),
        format!(
            "#!/bin/sh\nif [ \"$1\" = genpkey ]; then touch '{}'; sleep 2; fi\nexec {} \"$@\"\n",
            started.display(),
            real.trim()
        ),
    )
    .unwrap();
    std::fs::set_permissions(slow.join("openssl"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut child = run(Some(&slow))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..500 {
        if started.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(started.exists());
    let hup = Command::new("kill")
        .args(["-HUP", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(hup.success());
    assert!(!child.wait().unwrap().success());
    assert!(leftovers().is_empty(), "{:?}", leftovers());
    assert_eq!(
        std::fs::read_to_string(repo.join("install.sh")).unwrap(),
        failed
    );

    // Made: the key private, both files written, and the printed command reads the key.
    let out = run(None).output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let mode = std::fs::metadata(&key).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
    let names = leftovers();
    assert_eq!(names, ["signing-key.pem"], "{names:?}");
    let public = std::fs::read_to_string(repo.join("release/signing-key.pub")).unwrap();
    let installer = std::fs::read_to_string(repo.join("install.sh")).unwrap();
    assert_ne!(block(&installer), block(&failed));
    assert!(block(&installer).contains(public.trim_end()), "{installer}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    let gh = stdout
        .lines()
        .find(|l| l.trim_start().starts_with("gh secret set"))
        .unwrap();
    let word = gh.split_once(" < ").unwrap().1;
    let read = Command::new("sh")
        .args(["-c", &format!("printf %s {word}")])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(read.stdout).unwrap(),
        key.to_str().unwrap()
    );

    // An existing key is never replaced.
    let before = std::fs::read(&key).unwrap();
    let out = run(None).output().unwrap();
    assert!(!out.status.success());
    assert_eq!(std::fs::read(&key).unwrap(), before);
}

/// `twr claude-memory-guard` (the Claude Code orchestrator's `PreToolUse` hook): no output for
/// a file in the session's memory directory, a deny decision for anything else, input that is
/// no JSON included. It needs no hub or config: even an unreadable config must not let a call
/// through.
#[test]
fn the_memory_guard_hook_keeps_file_tools_in_the_memory_directory() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let project = root.join(".claude/projects/-home-me");
    std::fs::create_dir_all(project.join("memory")).unwrap();
    // A config this twr cannot read.
    std::fs::write(root.join("config.toml"), "not = [valid").unwrap();
    let guard = |input: &str| {
        let mut child = twr_cmd()
            .arg("claude-memory-guard")
            .env("TWR_HOME", &root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };
    let call = |path: &Path| {
        serde_json::json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Write",
            "tool_input": {"file_path": path, "content": "x"},
            "cwd": root,
            "transcript_path": project.join("s.jsonl"),
        })
        .to_string()
    };
    let out = guard(&call(&project.join("memory/MEMORY.md")));
    assert!(out.status.success(), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
    let out = guard(&call(&root.join("src/main.rs")));
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
    let out = guard("{");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
    // The command the hub configures, run by `sh` as Claude Code runs it: 0 lets the call go
    // on, 2 blocks it.
    let settings = twr_adapters::claude_code::guard::settings(Path::new(TWR));
    let command = settings["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap()
        .to_string();
    let hook = |input: &str| {
        let mut child = Command::new("sh")
            .args(["-c", &command])
            .env("TWR_HOME", &root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        stdin.write_all(input.as_bytes()).unwrap();
        drop(stdin);
        child.wait_with_output().unwrap()
    };
    assert_eq!(
        hook(&call(&project.join("memory/a.md"))).status.code(),
        Some(0)
    );
    assert_eq!(
        hook(&call(&root.join("src/main.rs"))).status.code(),
        Some(2)
    );
}

/// A guard that cannot decide within its deadline (here: input that never ends) denies, with
/// exit 2, before Claude Code's hook timeout would let the call through.
#[test]
fn a_memory_guard_past_its_deadline_denies() {
    let start = std::time::Instant::now();
    let mut child = twr_cmd()
        .arg("claude-memory-guard")
        .env(twr_adapters::claude_code::guard::DEADLINE_ENV, "300")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Stdin stays open: the guard never gets its whole input.
    let stdin = child.stdin.take().unwrap();
    let out = child.wait_with_output().unwrap();
    drop(stdin);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
    assert!(v["hookSpecificOutput"]["permissionDecisionReason"]
        .as_str()
        .unwrap()
        .contains("within 300 ms"));
    assert!(
        start.elapsed() < Duration::from_secs(20),
        "{:?}",
        start.elapsed()
    );
}

/// `twr wait`: finished (0, with its report), waiting on you (2), failed (3), timed out (124),
/// and a prompt sent with `--after` that goes out once the thread it waits for finished.
#[test]
fn wait_tells_how_threads_ended_and_send_after_follows() {
    let home = tempfile::tempdir().unwrap();
    let home = home.path();
    let project = home.join("proj");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(
        home.join("config.toml"),
        "[orchestrator]\nruntime = \"none\"\n",
    )
    .unwrap();
    let _hub = start_hub(home);
    let new = |name: &str| {
        ok(
            home,
            &[
                "thread",
                "new",
                project.to_str().unwrap(),
                "--runtime",
                "fake",
                "--name",
                name,
            ],
        );
    };
    for name in ["done", "asks", "fails", "slow", "api", "fails2", "later"] {
        new(name);
    }
    ok(
        home,
        &[
            "send",
            "api",
            "--after",
            "done",
            "go",
            "on:",
            "{report:done}",
        ],
    );
    ok(home, &["send", "done", "all", "good"]);
    ok(home, &["send", "asks", "!perm", "Bash", "rm"]);
    ok(home, &["send", "fails", "!error", "boom"]);
    ok(home, &["send", "slow", "!sleep", "60000"]);
    let wait = |args: &[&str]| {
        let mut all = vec!["wait"];
        all.extend_from_slice(args);
        let out = twr(home, &all);
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).to_string(),
        )
    };
    let (code, out) = wait(&["done", "--timeout", "20s"]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("done finished turn 1:\nall good"), "{out}");
    let (code, out) = wait(&["asks", "--timeout", "20s"]);
    assert_eq!(code, Some(2), "{out}");
    assert!(out.contains("asks waits on you"), "{out}");
    let (code, out) = wait(&["fails", "--timeout", "20s"]);
    assert_eq!(code, Some(3), "{out}");
    assert!(out.contains("fails: turn 1 failed"), "{out}");
    let (code, out) = wait(&["slow", "--timeout", "1s", "--json"]);
    assert_eq!(code, Some(124), "{out}");
    assert_eq!(ndjson(&out)[0]["state"], "timed_out");
    // All of them: the worst code, each one printed.
    let (code, out) = wait(&["done", "fails", "--all", "--timeout", "20s"]);
    assert_eq!(code, Some(3), "{out}");
    assert!(
        out.contains("done finished") && out.contains("fails: turn 1 failed"),
        "{out}"
    );
    // A timeout too long to count is refused, not a crash.
    let (code, _) = wait(&["done", "--timeout", "18446744073709551615s"]);
    assert_eq!(code, Some(1));
    // A thread named twice is one thread.
    let (code, out) = wait(&["done", "done", "--all", "--timeout", "20s"]);
    assert_eq!(code, Some(0), "{out}");
    // A prompt held after a thread that failed: its card waits on you.
    ok(
        home,
        &["send", "later", "--after", "fails2", "never", "mind"],
    );
    ok(home, &["send", "fails2", "!error", "boom"]);
    let (code, out) = wait(&["later", "--timeout", "25s"]);
    assert_eq!(code, Some(2), "{out}");
    // The prompt queued after `done` went out once it finished.
    let (code, out) = wait(&["api", "--timeout", "25s"]);
    assert_eq!(code, Some(0), "{out}");
    assert!(
        out.contains("api finished turn 1:\ngo on: all good"),
        "{out}"
    );
}
