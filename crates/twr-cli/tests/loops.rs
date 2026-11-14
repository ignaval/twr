//! `twr loop` against a real `twr hub` (docs/usage.md, "Loops"): a fake thread in a git
//! repository, a fake Codex reviewer (`TWR_CODEX_BIN`), and a hub told that Codex is installed
//! and logged in (`TWR_TEST_AGENTS`, honoured by a test's throwaway hub only).
//!
//! The fake runtime echoes each prompt it is sent, so the loop's own prompts answer themselves:
//! the templates in `<config dir>/loops/templates/` (which replace the built-in ones) end with
//! the block each step asks for, a GOAL paragraph, a triage that dismisses the round's one
//! finding, and a report.
//!
//! Then the hub killed outright (SIGKILL) at each point of a loop, and started again on the
//! same data: the loop goes on to the end it would have had, with no step lost or repeated
//! and nothing left behind. The points between two writes of the hub's are held at a test gate
//! (`TWR_TEST_LOOP_GATE`, a test's throwaway hub only) until the test kills it there.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

const TWR: &str = env!("CARGO_BIN_EXE_twr");
const WAIT: Duration = Duration::from_secs(30);
const START: &str = "start and save these settings";

/// The `twr` binary, without a `TWR_WORKSPACE` the test's own environment may have.
fn twr_cmd() -> Command {
    let mut c = Command::new(TWR);
    c.env_remove("TWR_WORKSPACE");
    c.env("TWR_TEST_USER_TOKEN_FILE", test_token());
    c.env("TWR_AGENT_PROBE", "off");
    c.env("TWR_BOX_PROBE", "off");
    c
}

/// A user token this test process's hubs and clients share (`TWR_TEST_USER_TOKEN_FILE`).
fn test_token() -> &'static Path {
    static TOKEN: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    TOKEN.get_or_init(|| {
        let dir = tempfile::tempdir().unwrap().keep();
        let path = dir.join("user.token");
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
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

/// What the hub finds of its own agent CLIs.
const AGENTS: &str = r#"[{"runtime": "codex", "found": true, "login": "logged_in"},
    {"runtime": "claude-code", "found": true, "login": "logged_in"},
    {"runtime": "grok-build", "found": false, "login": "unknown"}]"#;

/// A fake `codex exec`: run `n` (counted in `count`) answers one finding on the first run and
/// none after, to its `-o` file; with `hold.<n>` it waits until the file goes.
const FAKE_CODEX: &str = r#"#!/bin/sh
here=$(dirname "$0")
until mkdir "$here/count.lock" 2>/dev/null; do sleep 0.01; done
n=$(( $(cat "$here/count" 2>/dev/null || echo 0) + 1 ))
echo "$n" > "$here/count"
rmdir "$here/count.lock"
cat > /dev/null
while [ -f "$here/hold.$n" ]; do sleep 0.05; done
out=""; prev=""
for a in "$@"; do [ "$prev" = "-o" ] && out="$a"; prev="$a"; done
if [ "$n" = 1 ]; then
  printf '%s' '{"findings": [{"severity": "major", "location": "lib.rs:1", "problem": "it breaks on empty input", "fix": "guard it"}]}' > "$out"
else
  printf '%s' '{"findings": []}' > "$out"
fi
echo '{"type":"turn.completed","usage":{"input_tokens":100,"output_tokens":10}}'
"#;

/// The templates the fake thread echoes: each ends with the block its step asks for.
const GOAL: &str = "Gates are green.\n```twr-goal\nHarden the change against bad input.\n```\n";
const TRIAGE: &str = "{{findings}}\nTriaged.\n```twr-triage\n{\"items\": [{\"id\": \"{{finding_ids}}\", \"verdict\": \"dismissed\", \"reason\": \"empty input never reaches it\"}], \"gates\": {\"ran\": [\"true\"], \"result\": \"pass\"}}\n```\n";
const REPORT: &str = "The loop reviewed the change; nothing is left open.\n";

struct Env {
    _dir: tempfile::TempDir,
    home: PathBuf,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        std::fs::create_dir(home.join("tmp")).unwrap();
        let reviewers = home.join("reviewers");
        std::fs::create_dir(&reviewers).unwrap();
        let codex = reviewers.join("codex");
        std::fs::write(&codex, FAKE_CODEX).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755)).unwrap();
        // `TWR_HOME`'s config is `<home>/config.toml`: its templates are beside it.
        let templates = home.join("loops").join("templates");
        std::fs::create_dir_all(&templates).unwrap();
        for (name, text) in [("goal", GOAL), ("triage", TRIAGE), ("report", REPORT)] {
            std::fs::write(templates.join(format!("{name}.md")), text).unwrap();
        }
        Env { _dir: dir, home }
    }

    fn cmd(&self) -> Command {
        let mut c = twr_cmd();
        c.env("TWR_HOME", &self.home)
            .env("TMPDIR", self.home.join("tmp"))
            .env("TWR_ENABLE_FAKE", "1")
            .env("TWR_TEST_AGENTS", AGENTS)
            .env("TWR_CODEX_BIN", self.home.join("reviewers").join("codex"))
            .env_remove("RUST_LOG")
            .env_remove("VISUAL")
            .stdin(Stdio::null());
        c
    }

    fn twr(&self, args: &[&str]) -> Output {
        self.cmd().args(args).output().expect("running twr")
    }

    /// `twr <args>` that must succeed: its stdout.
    fn ok(&self, args: &[&str]) -> String {
        let out = self.twr(args);
        assert!(out.status.success(), "twr {args:?}: {}", text(&out));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn json(&self, args: &[&str]) -> Value {
        serde_json::from_str(&self.ok(args)).unwrap()
    }

    fn hold(&self, run: usize) {
        std::fs::write(self.home.join("reviewers").join(format!("hold.{run}")), "").unwrap();
    }

    fn release(&self, run: usize) {
        let _ = std::fs::remove_file(self.home.join("reviewers").join(format!("hold.{run}")));
    }

    fn runs(&self) -> usize {
        std::fs::read_to_string(self.home.join("reviewers").join("count"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    fn start_hub(&self) -> HubProc {
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.home.join("hub-test.log"))
            .unwrap();
        let hub = HubProc(
            self.cmd()
                .arg("hub")
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .unwrap(),
        );
        poll("the hub socket", || {
            self.twr(&["thread", "list"]).status.success()
        });
        hub
    }
}

fn text(out: &Output) -> String {
    format!(
        "status {:?}\nstdout:\n{}\nstderr:\n{}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn poll(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct HubProc(Child);

impl Drop for HubProc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The `<attn_id>@<revision>` in a `twr respond <id> "..."` line of `s`.
fn respond_target(s: &str) -> String {
    let at = s.find(" respond ").expect(s) + " respond ".len();
    s[at..].split_whitespace().next().unwrap().to_string()
}

#[test]
fn a_review_loop_from_the_command_line() {
    let env = Env::new();
    let _hub = env.start_hub();
    let repo = twr_core::testkit::git::init_repo(&env.home.join("api"));
    env.ok(&[
        "thread",
        "new",
        repo.to_str().unwrap(),
        "--runtime",
        "fake",
        "--name",
        "api",
        "--mode",
        "auto",
    ]);
    let threads = env.json(&["thread", "list", "--json"]);
    let wt = threads[0]["worktree"].as_str().unwrap().to_string();
    std::fs::write(Path::new(&wt).join("lib.rs"), "fn parse(s: &str) {}\n").unwrap();

    // Nothing saved: the settings say what the first card shows.
    let s = env.ok(&["loop", "settings"]);
    assert!(s.contains("no loop settings saved yet"), "{s}");
    assert!(s.contains("codex: logged in"), "{s}");
    assert!(env.twr(&["loop", "list"]).status.success());
    assert_eq!(env.ok(&["loop", "list"]), "no loop is running\n");

    // The first request, without a terminal: the card stays open, and the line to start it
    // is printed. Its overrides prefill the card.
    let out = env.twr(&["loop", "review", "api", "--profile", "low"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stdout.contains("api: review loop setup (first run: choose its models)"),
        "{stdout}"
    );
    assert!(stdout.contains("Profile      low"), "{stdout}");
    assert!(
        stdout.contains("Reviewer     codex · gpt-6.1-sol"),
        "{stdout}"
    );
    assert!(stderr.contains(&format!("\"{START}\"")), "{stderr}");
    let card = respond_target(&stderr);
    assert!(stdout.contains(&format!("card: {card}")), "{stdout}");
    // Nothing started, nothing saved.
    assert_eq!(env.ok(&["loop", "list"]), "no loop is running\n");
    assert!(env.json(&["loop", "settings", "--json"])["settings"].is_null());
    // `twr wait` says the thread waits on its card.
    let out = env.twr(&["wait", "api", "--timeout", "10s"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    assert!(text(&out).contains("settings card"), "{}", text(&out));

    // Answering it starts the loop on what it showed and saves that. The first review run
    // is held.
    env.hold(1);
    env.ok(&["respond", &card, START]);
    poll("the first review run", || env.runs() == 1);
    let list = env.ok(&["loop", "list"]);
    assert!(list.contains("api     review  low"), "{list}");
    assert!(list.contains("review"), "{list}");
    let loops = env.json(&["loop", "list", "--json"]);
    assert_eq!(loops[0]["phase"], "ladder");
    assert_eq!(loops[0]["round"], "m1");
    let saved = env.json(&["loop", "settings", "--json"]);
    assert_eq!(saved["source"], "workspace");
    assert_eq!(saved["settings"]["profile"], "low");

    // Paused while the run goes on: `twr wait` keeps waiting (the review still runs).
    let paused = env.ok(&["loop", "pause", "api"]);
    assert!(
        paused.starts_with("api's loop paused (low · ladder m1 · paused)"),
        "{paused}"
    );
    let out = env.twr(&["wait", "api", "--timeout", "2s"]);
    assert_eq!(out.status.code(), Some(124), "{}", text(&out));
    let shown = env.ok(&["loop", "show", "api"]);
    assert!(
        shown.contains("api: REVIEW LOOP low · ladder m1 · paused (running"),
        "{shown}"
    );
    assert!(
        shown.contains("settings (the settings card you answered"),
        "{shown}"
    );
    assert!(shown.contains("  scope        uncommitted"), "{shown}");
    assert!(
        shown.contains("goal: Harden the change against bad input."),
        "{shown}"
    );
    let resumed = env.ok(&["loop", "resume", "api", "--json"]);
    let resumed: Value = serde_json::from_str(&resumed).unwrap();
    assert_eq!(resumed["paused"], false);
    // A report before the end: there is none yet.
    let out = env.twr(&["loop", "report", "api"]);
    assert!(!out.status.success());
    assert!(text(&out).contains("still running"), "{}", text(&out));

    // The run answers one finding; the thread dismisses it; the verification is clean; the
    // report is written. `twr wait` returns with it.
    env.release(1);
    let out = env.twr(&["wait", "api", "--timeout", "60s"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out));
    let waited = String::from_utf8_lossy(&out.stdout);
    assert!(
        waited.contains("review loop clean after 2 rounds"),
        "{waited}"
    );
    assert_eq!(env.runs(), 2);
    let shown = env.ok(&["loop", "show", "api"]);
    for want in [
        "(ended: clean)",
        "  m1 medium: 1 finding",
        "    m1.1 major lib.rs:1: it breaks on empty input — dismissed: empty input never \
         reaches it",
        "    gates: pass (true)",
        "  v1 medium: clean",
        "ledger: 1",
        "transition: after m1: the last tier ran its profile's rounds",
        "ended: clean",
        "report: ",
    ] {
        assert!(shown.contains(want), "{want:?} missing:\n{shown}");
    }
    let report = env.ok(&["loop", "report", "api"]);
    assert!(report.starts_with("# Review loop report: api"), "{report}");
    assert!(
        report.contains("The loop reviewed the change; nothing is left open."),
        "{report}"
    );
    let r = env.json(&["loop", "report", "api", "--json"]);
    assert_eq!(r["text"].as_str(), Some(report.as_str()));

    // Saved settings: the next loop starts at once and says what it runs on.
    env.hold(3);
    let started = env.ok(&["loop", "review", "api"]);
    // (The fake thread may have answered the scope step already: the line says where it is.)
    assert!(
        started.starts_with("api: review loop started (low · "),
        "{started}"
    );
    assert!(
        started.contains("on the workspace's saved settings"),
        "{started}"
    );
    assert!(
        started.contains("  reviewer     codex/gpt-6.1-sol"),
        "{started}"
    );
    poll("the next review run", || env.runs() == 3);
    // Stopping it needs a confirmation: no terminal, so --yes.
    let out = env.twr(&["loop", "stop", "api"]);
    assert!(!out.status.success());
    assert!(text(&out).contains("pass --yes"), "{}", text(&out));
    let stopped = env.ok(&["loop", "stop", "api", "--yes"]);
    assert_eq!(stopped, "api's loop ended: stopped\n");
    env.release(3);
    assert_eq!(env.ok(&["loop", "list"]), "no loop is running\n");
    assert!(env
        .ok(&["loop", "show", "api"])
        .contains("(ended: stopped)"));

    // Settings: set, edit, reset.
    let set = env.ok(&[
        "loop",
        "settings",
        "set",
        "--profile",
        "medium",
        "--commit",
        "none",
    ]);
    assert!(
        set.starts_with("saved as the workspace's loop settings:"),
        "{set}"
    );
    let got = env.json(&["loop", "settings", "--json"]);
    assert_eq!(got["settings"]["profile"], "medium");
    assert_eq!(got["settings"]["review_commit"], "none");
    let bad = env.twr(&[
        "loop",
        "settings",
        "set",
        "--reviewer",
        "grok-build/grok-4.7",
    ]);
    assert!(!bad.status.success());
    assert!(
        text(&bad).contains("grok-build isn't installed"),
        "{}",
        text(&bad)
    );
    let editor = env.home.join("editor.sh");
    std::fs::write(
        &editor,
        "printf 'profile = \"high\"\\nimplementer = \"thread\"\\nworkers = \
         \"claude-opus-5-5:high\"\\nreviewer = \"codex/gpt-6.1-sol\"\\ncommit = \"match\"\\n' > \
         \"$1\"\n",
    )
    .unwrap();
    let out = env
        .cmd()
        .args(["loop", "settings", "--edit"])
        .env("EDITOR", format!("sh {}", editor.display()))
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    let got = env.json(&["loop", "settings", "--json"]);
    assert_eq!(got["settings"]["profile"], "high");
    assert_eq!(got["settings"]["review_commit"], "match");
    // An edit that does not read: nothing saved, the edit kept.
    std::fs::write(&editor, "printf 'profile = \"high\"\\n' > \"$1\"\n").unwrap();
    let out = env
        .cmd()
        .args(["loop", "settings", "--edit"])
        .env("EDITOR", format!("sh {}", editor.display()))
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        text(&out).contains("nothing was saved (your edit is kept in"),
        "{}",
        text(&out)
    );
    let out = env.twr(&["loop", "settings", "--reset"]);
    assert!(!out.status.success());
    assert!(text(&out).contains("pass --yes"), "{}", text(&out));
    assert!(env
        .ok(&["loop", "settings", "--reset", "--yes"])
        .starts_with("forgot every saved loop setting"));
    assert!(env
        .ok(&["loop", "settings"])
        .contains("no loop settings saved yet"));
    // So the next loop shows its card again.
    let out = env.twr(&["loop", "review", "api"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
}

// ---------------------------------------------------------------------------------------------
// A hub killed outright (SIGKILL: no shutdown, nothing recorded on the way out) in the middle
// of a review loop, then started again on the same data.
// ---------------------------------------------------------------------------------------------

/// A fake `codex exec` for the crash tests: run `n` (counted in `count`) writes its pid to
/// `pid.<n>`; with `hold.<n>` it starts a child (`child.<n>`) and waits until the file goes;
/// with `fail.<n>` it exits 3; it answers `answers/<n>.json` to its `-o` file, else no
/// findings.
const CRASH_CODEX: &str = r#"#!/bin/sh
here=$(dirname "$0")
until mkdir "$here/count.lock" 2>/dev/null; do sleep 0.01; done
n=$(( $(cat "$here/count" 2>/dev/null || echo 0) + 1 ))
echo "$n" > "$here/count"
rmdir "$here/count.lock"
cat > /dev/null
out=""; prev=""
for a in "$@"; do [ "$prev" = "-o" ] && out="$a"; prev="$a"; done
if [ -f "$here/hold.$n" ]; then
  sleep 600 &
  echo $! > "$here/child.$n"
  echo $$ > "$here/pid.$n"
  while [ -f "$here/hold.$n" ]; do sleep 0.05; done
  kill $! 2>/dev/null
fi
echo $$ > "$here/pid.$n"
if [ -f "$here/fail.$n" ]; then echo "boom: run $n failed" >&2; exit 3; fi
if [ -f "$here/answers/$n.json" ]; then cp "$here/answers/$n.json" "$out"; else printf '%s' '{"findings": []}' > "$out"; fi
echo '{"type":"turn.completed","usage":{"input_tokens":100,"output_tokens":10}}'
"#;

/// The crash tests' templates: a triage that fixes the round's one finding (a new class, so
/// the `medium` profile's sweep runs), and a sweep block.
const FIX_TRIAGE: &str = "{{findings}}\nTriaged.\n```twr-triage\n{\"items\": [{\"id\": \"{{finding_ids}}\", \"verdict\": \"fixed\", \"class\": \"new-class\", \"rule\": \"guard-input\"}], \"gates\": {\"ran\": [\"true\"], \"result\": \"pass\"}}\n```\n";
const SWEEP: &str = "Swept.\n```twr-sweep\n{\"rules\": [{\"rule\": \"guard-input\", \"result\": \"fixed\"}], \"gates\": {\"ran\": [\"true\"], \"result\": \"pass\"}}\n```\n";

/// A template that holds its turn (the fake runtime sleeps ten minutes first), then answers as
/// `then` does.
fn holding(then: &str) -> String {
    format!("!sleep 600000\n{then}")
}

/// One finding, as a reviewer answers it.
const ONE_FINDING: &str = r#"{"findings": [{"severity": "major", "location": "lib.rs:1", "problem": "it breaks on empty input", "fix": "guard it"}]}"#;

impl Env {
    /// An `Env` for the crash tests: [`CRASH_CODEX`], whose first run finds one problem and
    /// every later one none, the [`FIX_TRIAGE`] and [`SWEEP`] templates, and a thread `api` in
    /// a repository with a change, whose loops run on the saved `medium` profile: scope, m1
    /// (one finding, fixed), m2 (clean: the tier converges), the sweep, v1 (clean), the report.
    fn crash() -> Env {
        let env = Env::new();
        let reviewers = env.home.join("reviewers");
        std::fs::write(reviewers.join("codex"), CRASH_CODEX).unwrap();
        std::fs::create_dir(reviewers.join("answers")).unwrap();
        std::fs::write(reviewers.join("answers").join("1.json"), ONE_FINDING).unwrap();
        env.template("triage", FIX_TRIAGE);
        env.template("sweep", SWEEP);
        env
    }

    /// Replaces template `name` (read again for each prompt).
    fn template(&self, name: &str, text: &str) {
        std::fs::write(
            self.home
                .join("loops")
                .join("templates")
                .join(format!("{name}.md")),
            text,
        )
        .unwrap();
    }

    /// A hub that holds what a loop records at `point` ([`twr hub`'s `TWR_TEST_LOOP_GATE`])
    /// until the test kills it: the gate's file. Ready once its socket is bound, or once it is
    /// at the gate: a hub that reaches it in its start-up recovery holds its lock there, and a
    /// request (`twr thread list`) would wait out the gate.
    fn start_hub_gated(&self, point: &str) -> (HubProc, PathBuf) {
        let gate = self.home.join(format!("gate-{}", point.replace(' ', "-")));
        std::fs::write(&gate, "").unwrap();
        let socket = self.home.join("hub.sock");
        // (A killed hub's socket stays behind: only the new one's says it is up.)
        let _ = std::fs::remove_file(&socket);
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.home.join("hub-test.log"))
            .unwrap();
        let mut cmd = self.cmd();
        cmd.arg("hub")
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .env("TWR_TEST_LOOP_GATE", format!("{point}={}", gate.display()));
        let hub = HubProc(cmd.spawn().unwrap());
        let mut at = gate.as_os_str().to_owned();
        at.push(".reached");
        let at = PathBuf::from(at);
        poll("the gated hub's socket", || socket.exists() || at.exists());
        (hub, gate)
    }

    /// The thread `api` in a fresh repository, with a change in its worktree, and the
    /// `medium` profile saved: its id.
    fn api_thread(&self) -> String {
        let id = self.api_thread_unsaved();
        self.ok(&["loop", "settings", "set", "--profile", "medium"]);
        id
    }

    /// [`Env::api_thread`] with no settings saved.
    fn api_thread_unsaved(&self) -> String {
        let repo = twr_core::testkit::git::init_repo(&self.home.join("api"));
        self.ok(&[
            "thread",
            "new",
            repo.to_str().unwrap(),
            "--runtime",
            "fake",
            "--name",
            "api",
            "--mode",
            "auto",
        ]);
        let threads = self.json(&["thread", "list", "--json"]);
        let wt = threads[0]["worktree"].as_str().unwrap().to_string();
        std::fs::write(Path::new(&wt).join("lib.rs"), "fn parse(s: &str) {}\n").unwrap();
        threads[0]["id"].as_str().unwrap().to_string()
    }

    /// `twr loop review api` on the saved settings: the loop's id.
    fn start_loop(&self) -> String {
        let out = self.ok(&["loop", "review", "api"]);
        assert!(out.starts_with("api: review loop started"), "{out}");
        self.json(&["loop", "show", "api", "--json"])["id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    /// `twr loop review api` on the saved settings against `hub`, gated at `gate`
    /// ([`Env::start_hub_gated`]): once the hub reached the gate, it is killed (the command
    /// with it: the hub holds its lock at the gate). The loop's id, from the log.
    fn start_loop_until(&self, hub: HubProc, gate: &Path, thread: &str) -> String {
        self.start_until(&["loop", "review", "api"], hub, gate, thread)
    }

    /// [`Env::start_loop_until`] with the command `args`.
    fn start_until(&self, args: &[&str], hub: HubProc, gate: &Path, thread: &str) -> String {
        let mut cli = self
            .cmd()
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        reached(gate);
        kill(hub);
        let _ = cli.wait();
        self.events(thread)
            .iter()
            .find_map(|e| match &e.body {
                twr_core::event::EventBody::LoopStarted(p) => Some(p.id.clone()),
                _ => None,
            })
            .expect("the loop started")
    }

    fn db(&self) -> PathBuf {
        self.home.join("data").join("twr.db")
    }

    /// Thread `thread`'s events, from the log (the hub may be up or gone).
    fn events(&self, thread: &str) -> Vec<twr_core::event::Event> {
        let conn = twr_hub::store::connect(&self.db()).unwrap();
        twr_hub::store::read_thread_events(&conn, thread, 0, 1_000_000).unwrap()
    }

    /// Thread `thread`'s queued prompts.
    fn queued(&self, thread: &str) -> Vec<String> {
        let conn = twr_hub::store::connect(&self.db()).unwrap();
        let mut st = conn
            .prepare("SELECT prompt FROM pending_prompts WHERE thread = ?1 ORDER BY id")
            .unwrap();
        st.query_map([thread], |r| r.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// `pid.<n>` (and `child.<n>`, when `child`) of review run `n`, once written.
    fn run_pids(&self, n: usize, child: bool) -> Vec<i32> {
        let read = |k: &str| {
            std::fs::read_to_string(self.home.join("reviewers").join(format!("{k}.{n}")))
                .ok()
                .and_then(|s| s.trim().parse::<i32>().ok())
        };
        let mut out = Vec::new();
        poll("the review run's pids", || {
            out = [Some("pid"), child.then_some("child")]
                .into_iter()
                .flatten()
                .filter_map(read)
                .collect();
            out.len() == 1 + usize::from(child)
        });
        out
    }

    /// `twr loop show api --json`.
    fn shown(&self) -> Value {
        self.json(&["loop", "show", "api", "--json"])
    }

    /// Waits until the thread's loop ended: `twr loop show api --json`.
    fn wait_ended(&self) -> Value {
        let mut shown = Value::Null;
        poll("the loop's end", || {
            shown = self.shown();
            !shown["ended"].is_null()
        });
        shown
    }

    /// The open attention items, as `twr attention --all --json` lists them.
    fn items(&self) -> Vec<Value> {
        self.json(&["attention", "--all", "--json"])
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    fn set(&self, file: &str) {
        std::fs::write(self.home.join("reviewers").join(file), "").unwrap();
    }

    /// Review run `n` answers one finding.
    fn finds(&self, n: usize) {
        std::fs::write(
            self.home
                .join("reviewers")
                .join("answers")
                .join(format!("{n}.json")),
            ONE_FINDING,
        )
        .unwrap();
    }

    /// The open item of thread `thread` that is a `key` (`loop_setup`, `loop_stuck`), once
    /// there is one.
    fn item_marked(&self, thread: &str, key: &str) -> Value {
        let mut found = Value::Null;
        poll(key, || {
            found = self
                .items()
                .into_iter()
                .find(|i| i["thread"] == thread && i[key] == true)
                .unwrap_or(Value::Null);
            !found.is_null()
        });
        found
    }
}

/// Kills the hub outright (SIGKILL), as a crash would, and waits for it to be gone.
fn kill(mut hub: HubProc) {
    hub.0.kill().unwrap();
    hub.0.wait().unwrap();
}

/// Waits until the hub reached the gate `gate` ([`Env::start_hub_gated`]).
fn reached(gate: &Path) {
    let mut r = gate.as_os_str().to_owned();
    r.push(".reached");
    let r = PathBuf::from(r);
    poll("the hub at its gate", || r.exists());
}

fn alive(pid: i32) -> bool {
    // (A zombie counts as gone: nobody runs it any more.)
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout);
    let stat = stat.trim();
    !stat.is_empty() && !stat.starts_with('Z')
}

/// The loop's prompts that started a turn of `thread`, by step (`scope`, `triage m1`,
/// `sweep`, `report`, and `correction` ones), in order.
fn asked(events: &[twr_core::event::Event], loop_id: &str) -> Vec<String> {
    let marker = format!("[twr loop {loop_id} ");
    events
        .iter()
        .filter_map(|e| match &e.body {
            twr_core::event::EventBody::TurnStarted(p) => p.prompt.strip_prefix(&marker),
            _ => None,
        })
        .map(|rest| rest.split(']').next().unwrap_or_default().to_string())
        .collect()
}

/// The kinds of `events` that are loop facts (`loop.*`), with their round or phase: what the
/// loop recorded, in order, to compare runs.
fn facts(events: &[twr_core::event::Event]) -> Vec<String> {
    use twr_core::event::EventBody as B;
    events
        .iter()
        .filter_map(|e| {
            Some(match &e.body {
                B::LoopStarted(_) => "started".to_string(),
                B::LoopPhase(p) if p.problems.is_empty() => format!("phase {:?}", p.phase),
                B::LoopPhase(_) => "phase refused".to_string(),
                B::LoopReviewStarted(p) => format!("review {}", p.round),
                B::LoopFindings(p) if p.failed => format!("failed {}", p.round),
                B::LoopFindings(p) => format!("findings {} {}", p.round, p.findings.len()),
                B::LoopTriage(p) => format!("triage {}", p.round),
                B::LoopTransition(p) => format!("transition {}", p.round),
                B::LoopControl(p) => format!("control {:?}", p.action),
                B::LoopEnded(p) => format!("ended {:?}", p.outcome),
                B::LoopPlan(p) => format!("plan {}", p.revision),
                B::LoopImplemented(p) => format!("implemented {:?}", p.status),
                B::LoopCommit(p) if p.problems.is_empty() => "commit passed".to_string(),
                B::LoopCommit(_) => "commit failed".to_string(),
                B::LoopRules(p) => format!("rules {}", p.rules.len()),
                B::LoopAuditStarted(p) => format!("audit s{}", p.rule),
                B::LoopAudit(p) if p.failed => format!("audit failed s{}", p.rule),
                B::LoopAudit(p) => format!("audited s{} {}", p.rule, p.findings.len()),
                _ => return None,
            })
        })
        .collect()
}

/// The facts of a whole `medium` loop that nothing interrupted ([`Env::crash`]).
const CLEAN_RUN: &[&str] = &[
    "started",
    "phase Scope",
    "phase Ladder",
    "review m1",
    "findings m1 1",
    "triage m1",
    "review m2",
    "findings m2 0",
    "transition m2",
    "phase Sweep",
    "phase Verify",
    "review v1",
    "findings v1 0",
    "phase Report",
    "ended Clean",
];

/// What every loop that went to its end after a crash leaves: no review task running, no
/// prompt of the loop queued, no reviewer process left, its report the thread's report.
fn nothing_left(env: &Env, thread: &str, loop_id: &str) {
    nothing_left_of(env, thread, loop_id, "review loop clean after");
}

/// [`nothing_left`], the loop's last report starting with `report`.
fn nothing_left_of(env: &Env, thread: &str, loop_id: &str, report: &str) {
    let threads = env.json(&["thread", "list", "--json"]);
    let t = threads
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["id"] == thread)
        .unwrap();
    let tasks = t["background_tasks"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    assert!(
        !tasks.keys().any(|k| k.starts_with("loop:")),
        "a review task is left: {tasks:?}"
    );
    let marker = format!("[twr loop {loop_id} ");
    assert!(
        !env.queued(thread).iter().any(|p| p.starts_with(&marker)),
        "{:?}",
        env.queued(thread)
    );
    let reviewers = env.home.join("reviewers");
    for entry in std::fs::read_dir(&reviewers).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        if name.starts_with("pid.") || name.starts_with("child.") {
            let pid: i32 = std::fs::read_to_string(reviewers.join(&name))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            poll(&format!("{name} ({pid}) gone"), || !alive(pid));
        }
    }
    // The loop's last report is the thread's last report (what `twr wait` and the card
    // show). (Not `twr wait` itself: a thread whose idle runtime died with a killed hub counts
    // as failed to it, loop or not.)
    let last = env
        .events(thread)
        .into_iter()
        .rev()
        .find_map(|e| match e.body {
            twr_core::event::EventBody::ReportReady(r) => Some(r.summary),
            _ => None,
        })
        .unwrap();
    assert!(last.starts_with(report), "{last}");
    // No review item for a turn of the loop's: the loop judges those.
    assert!(
        !env.items()
            .iter()
            .any(|i| i["thread"] == thread && i["reason"] == "review"),
        "{:?}",
        env.items()
    );
}

/// Killed while a review run is in flight: the reviewer (and its child) outlive the hub, the
/// next hub kills them before anything runs, records the run failed ("the hub stopped while it
/// ran") and runs the round again, once; the loop goes on to its end.
#[test]
fn a_hub_killed_during_a_review_run() {
    let env = Env::crash();
    let hub = env.start_hub();
    let thread = env.api_thread();
    env.hold(1);
    let id = env.start_loop();
    let pids = env.run_pids(1, true);
    kill(hub);
    // Nothing killed the run: it is the next hub's to clean up.
    assert!(pids.iter().all(|p| alive(*p)), "{pids:?}");
    let evs = env.events(&thread);
    assert_eq!(
        facts(&evs),
        ["started", "phase Scope", "phase Ladder", "review m1"]
    );
    let _hub = env.start_hub();
    for p in &pids {
        poll(&format!("reviewer process {p} killed"), || !alive(*p));
    }
    let shown = env.wait_ended();
    assert_eq!(shown["outcome"], "clean", "{shown}");
    // m1 failed with the hub, then ran again; the retry (run 2) answers no findings, so the
    // ladder converges at m1 with nothing fixed: no triage, no sweep, and v1 runs at xhigh
    // (medium's verification).
    let evs = env.events(&thread);
    let got = facts(&evs);
    assert_eq!(
        got,
        [
            "started",
            "phase Scope",
            "phase Ladder",
            "review m1",
            "failed m1",
            "review m1",
            "findings m1 0",
            "transition m1",
            "phase Verify",
            "review v1",
            "findings v1 0",
            "phase Report",
            "ended Clean",
        ],
        "{got:?}"
    );
    assert_eq!(shown["rounds"][0]["runs"].as_array().unwrap().len(), 2);
    assert_eq!(env.runs(), 3);
    assert_eq!(asked(&evs, &id), ["scope", "report"]);
    nothing_left(&env, &thread, &id);
}

/// Killed after a review run answered (its answer file written, its process gone) and before
/// the hub recorded `loop.findings`: the run was in flight as far as the log knows, so the next
/// hub records it failed and runs the round again, once; the stale answer file is not taken.
#[test]
fn a_hub_killed_before_it_records_a_review_answer() {
    let env = Env::crash();
    // (The retry finds the same problem, so the loop goes as one nothing interrupted.)
    env.finds(2);
    let (hub, gate) = env.start_hub_gated("findings m1");
    let thread = env.api_thread();
    let id = env.start_loop_until(hub, &gate, &thread);
    let shown_folder = {
        let evs = env.events(&thread);
        assert_eq!(
            facts(&evs),
            ["started", "phase Scope", "phase Ladder", "review m1"]
        );
        evs.iter()
            .find_map(|e| match &e.body {
                twr_core::event::EventBody::LoopStarted(p) => p.folder.clone(),
                _ => None,
            })
            .unwrap()
    };
    let answer = Path::new(&shown_folder).join("round-m1.answer.json");
    assert!(answer.exists(), "the run answered before the hub died");
    std::fs::remove_file(env.home.join("reviewers").join("answers").join("1.json")).unwrap();
    let _hub = env.start_hub();
    let shown = env.wait_ended();
    assert_eq!(shown["outcome"], "clean", "{shown}");
    let mut want: Vec<&str> = CLEAN_RUN.to_vec();
    want.splice(4..4, ["failed m1", "review m1"]);
    let evs = env.events(&thread);
    assert_eq!(facts(&evs), want);
    assert_eq!(env.runs(), 4);
    assert_eq!(asked(&evs, &id), ["scope", "triage m1", "sweep", "report"]);
    nothing_left(&env, &thread, &id);
}

/// Killed while the thread's triage turn runs: the runtime goes with the hub, and the next hub
/// asks for the step again, once (no automatic retry of the turn besides); the loop goes on to
/// its end, the triage recorded once.
#[test]
fn a_hub_killed_during_a_triage_turn() {
    let env = Env::crash();
    env.template("triage", &holding(FIX_TRIAGE));
    let hub = env.start_hub();
    let thread = env.api_thread();
    let id = env.start_loop();
    poll("the triage turn", || {
        asked(&env.events(&thread), &id).contains(&"triage m1".to_string())
    });
    kill(hub);
    env.template("triage", FIX_TRIAGE);
    let _hub = env.start_hub();
    let shown = env.wait_ended();
    assert_eq!(shown["outcome"], "clean", "{shown}");
    let evs = env.events(&thread);
    assert_eq!(facts(&evs), CLEAN_RUN);
    assert_eq!(
        asked(&evs, &id),
        ["scope", "triage m1", "triage m1", "sweep", "report"]
    );
    assert!(!evs.iter().any(|e| matches!(&e.body,
        twr_core::event::EventBody::TurnStarted(p) if p.prompt.starts_with("Continue where you left off"))));
    assert_eq!(env.runs(), 3);
    nothing_left(&env, &thread, &id);
}

/// A step's turn that ended while no hub reacted to it (the hub killed after it recorded the
/// turn's end and before it recorded what the turn answers): the next hub takes that turn's
/// final message as the step's answer, once, and goes on. Each thread step in turn: scope,
/// triage, sweep, report.
#[test]
fn a_step_answered_while_no_hub_ran_is_taken_once() {
    let env = Env::crash();
    let (hub, gate) = env.start_hub_gated("answer scope");
    let thread = env.api_thread();
    let id = env.start_loop_until(hub, &gate, &thread);
    for step in ["scope", "triage m1", "sweep", "report"] {
        if step != "scope" {
            let (hub, gate) = env.start_hub_gated(&format!("answer {step}"));
            reached(&gate);
            kill(hub);
        }
        let evs = env.events(&thread);
        // Its turn ended, completed, and nothing took it yet.
        let last = asked(&evs, &id);
        assert_eq!(last.last().map(String::as_str), Some(step), "{last:?}");
        assert!(
            matches!(&evs.last().unwrap().body,
            twr_core::event::EventBody::TurnEnded(p) if p.result == twr_core::event::TurnResult::Completed),
            "{:?}",
            evs.last()
        );
    }
    let _hub = env.start_hub();
    let shown = env.wait_ended();
    assert_eq!(shown["outcome"], "clean", "{shown}");
    let evs = env.events(&thread);
    assert_eq!(facts(&evs), CLEAN_RUN);
    assert_eq!(asked(&evs, &id), ["scope", "triage m1", "sweep", "report"]);
    assert_eq!(env.runs(), 3);
    // Each step's turn has one report, the loop's (marked with the step), as at a live end.
    for (step, turn) in loop_turns(&evs, &id) {
        let reports: Vec<_> = evs
            .iter()
            .filter(|e| e.turn == Some(turn))
            .filter_map(|e| match &e.body {
                twr_core::event::EventBody::ReportReady(r) => Some(r),
                _ => None,
            })
            .collect();
        assert_eq!(reports.len(), 1, "{step}: {reports:?}");
        let mark = reports[0]
            .ext
            .as_ref()
            .map(|x| x["twr"]["loop"]["step"].clone());
        assert_eq!(mark, Some(json!(step)), "{step}: {reports:?}");
    }
    nothing_left(&env, &thread, &id);
}

/// The turns a loop's prompts started: `(step, turn)`.
fn loop_turns(events: &[twr_core::event::Event], loop_id: &str) -> Vec<(String, u64)> {
    let marker = format!("[twr loop {loop_id} ");
    events
        .iter()
        .filter_map(|e| match &e.body {
            twr_core::event::EventBody::TurnStarted(p) => {
                let rest = p.prompt.strip_prefix(&marker)?;
                Some((rest.split(']').next()?.to_string(), e.turn?))
            }
            _ => None,
        })
        .collect()
}

/// A step's turn that failed while no hub reacted to it (the hub killed after it recorded the
/// failure and its error item, before the loop took it): the next hub takes it as at its end,
/// the loop paused with its stuck item, which `resume` answers; the step is asked again and the
/// loop goes on. A turn that ended `error`:
#[test]
fn a_step_that_failed_while_no_hub_ran_makes_the_loop_stuck() {
    failed_while_no_hub_ran("!error the gates broke\n", "turn 2 failed");
}

/// ... and a runtime that crashed on its own in the step's turn (not one that died with the
/// hub, whose step is asked again: [`a_hub_killed_during_a_triage_turn`]).
#[test]
fn a_step_whose_runtime_crashed_while_no_hub_ran_makes_the_loop_stuck() {
    failed_while_no_hub_ran("!crash\n", "turn 2 crashed");
}

fn failed_while_no_hub_ran(triage: &str, why: &str) {
    let env = Env::crash();
    env.template("triage", triage);
    let (hub, gate) = env.start_hub_gated("failed triage m1");
    let thread = env.api_thread();
    let id = env.start_loop_until(hub, &gate, &thread);
    let evs = env.events(&thread);
    assert_eq!(asked(&evs, &id), ["scope", "triage m1"]);
    assert!(evs.iter().rev().any(|e| matches!(&e.body,
        twr_core::event::EventBody::TurnEnded(p) if e.turn == Some(2)
            && p.result != twr_core::event::TurnResult::Completed)));
    assert!(!facts(&evs).iter().any(|f| f.starts_with("control")));
    env.template("triage", FIX_TRIAGE);
    let _hub = env.start_hub();
    let stuck = env.item_marked(&thread, "loop_stuck");
    assert!(stuck["summary"].as_str().unwrap().contains(why), "{stuck}");
    let shown = env.shown();
    assert_eq!(shown["state"]["paused"], true, "{shown}");
    // Nothing asks for the step until the user answers.
    std::thread::sleep(Duration::from_secs(2));
    assert_eq!(asked(&env.events(&thread), &id), ["scope", "triage m1"]);
    env.ok(&["respond", stuck["target"].as_str().unwrap(), "resume"]);
    let shown = env.wait_ended();
    assert_eq!(shown["outcome"], "clean", "{shown}");
    let evs = env.events(&thread);
    let mut want: Vec<&str> = CLEAN_RUN.to_vec();
    want.splice(5..5, ["control Pause", "control Resume"]);
    assert_eq!(facts(&evs), want);
    assert_eq!(
        asked(&evs, &id),
        ["scope", "triage m1", "triage m1", "sweep", "report"]
    );
    nothing_left(&env, &thread, &id);
}

/// Killed while the setup card is open: the next hub has the card, at the same revision, and
/// answering it then starts the loop, which runs to its end.
#[test]
fn the_setup_card_outlives_a_killed_hub() {
    let env = Env::crash();
    let hub = env.start_hub();
    let thread = env.api_thread_unsaved();
    let out = env.twr(&["loop", "review", "api", "--profile", "medium"]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    let card = respond_target(&String::from_utf8_lossy(&out.stderr));
    kill(hub);
    let _hub = env.start_hub();
    let item = env.item_marked(&thread, "loop_setup");
    assert_eq!(item["target"], card.as_str(), "{item}");
    assert_eq!(env.ok(&["loop", "list"]), "no loop is running\n");
    env.ok(&["respond", &card, START]);
    let shown = env.wait_ended();
    assert_eq!(shown["outcome"], "clean", "{shown}");
    let evs = env.events(&thread);
    assert_eq!(facts(&evs), CLEAN_RUN);
    let id = shown["id"].as_str().unwrap();
    assert_eq!(asked(&evs, id), ["scope", "triage m1", "sweep", "report"]);
    nothing_left(&env, &thread, id);
}

/// Killed while the loop is paused with its review run in flight: the next hub kills the run,
/// and the loop stays paused (no run starts) until `twr loop resume`, after which the round
/// runs again and the loop goes on to its end.
#[test]
fn a_paused_loop_outlives_a_killed_hub() {
    let env = Env::crash();
    env.finds(2);
    env.hold(1);
    let hub = env.start_hub();
    let thread = env.api_thread();
    let id = env.start_loop();
    let pids = env.run_pids(1, true);
    env.ok(&["loop", "pause", "api"]);
    kill(hub);
    let _hub = env.start_hub();
    for p in &pids {
        poll(&format!("reviewer process {p} killed"), || !alive(*p));
    }
    std::fs::remove_file(env.home.join("reviewers").join("answers").join("1.json")).unwrap();
    // Paused: nothing runs, however long.
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(env.runs(), 1);
    let shown = env.shown();
    assert_eq!(shown["state"]["paused"], true, "{shown}");
    let threads = env.json(&["thread", "list", "--json"]);
    assert!(
        !threads[0]["background_tasks"]
            .as_object()
            .is_some_and(|t| t.keys().any(|k| k.starts_with("loop:"))),
        "{threads}"
    );
    env.ok(&["loop", "resume", "api"]);
    let shown = env.wait_ended();
    assert_eq!(shown["outcome"], "clean", "{shown}");
    let evs = env.events(&thread);
    assert_eq!(asked(&evs, &id), ["scope", "triage m1", "sweep", "report"]);
    let got = facts(&evs);
    assert_eq!(
        got.iter().filter(|f| f.starts_with("triage")).count(),
        1,
        "{got:?}"
    );
    assert_eq!(env.runs(), 4);
    nothing_left(&env, &thread, &id);
}

/// Killed while the stuck item is open (two failed runs of m1): the next hub has the item, at
/// the same revision, and the loop still paused; `resume` runs the round again.
#[test]
fn a_stuck_loop_outlives_a_killed_hub() {
    let env = Env::crash();
    env.set("fail.1");
    env.set("fail.2");
    env.finds(3);
    std::fs::remove_file(env.home.join("reviewers").join("answers").join("1.json")).unwrap();
    let hub = env.start_hub();
    let thread = env.api_thread();
    let id = env.start_loop();
    let stuck = env.item_marked(&thread, "loop_stuck");
    kill(hub);
    let _hub = env.start_hub();
    let again = env.item_marked(&thread, "loop_stuck");
    assert_eq!(again["target"], stuck["target"], "{again}");
    std::thread::sleep(Duration::from_secs(3));
    assert_eq!(env.runs(), 2);
    assert_eq!(env.shown()["state"]["paused"], true);
    env.ok(&["respond", stuck["target"].as_str().unwrap(), "resume"]);
    let shown = env.wait_ended();
    assert_eq!(shown["outcome"], "clean", "{shown}");
    let evs = env.events(&thread);
    let mut want: Vec<&str> = CLEAN_RUN.to_vec();
    want.splice(
        4..4,
        [
            "failed m1",
            "review m1",
            "failed m1",
            "control Resume",
            "review m1",
        ],
    );
    assert_eq!(facts(&evs), want);
    assert_eq!(asked(&evs, &id), ["scope", "triage m1", "sweep", "report"]);
    assert_eq!(env.runs(), 5);
    nothing_left(&env, &thread, &id);
}

// ---------------------------------------------------------------------------------------------
// Implementation loops (docs/design/loops.md, milestone 2): `twr loop implement`, `twr thread
// new --loop implement`, and a hub killed at each of the new points of one.
// ---------------------------------------------------------------------------------------------

/// The plan turn: a plan that touches a contract.
const PLAN: &str = "## Plan\nAdd export_csv in feature.rs.\n```twr-plan\n{\"contract_risk\": true, \"reason\": \"a new public endpoint\"}\n```\n";
/// The plan check's triage: the finding dismissed (the plan is not revised).
const PLAN_TRIAGE: &str = "{{findings}}\nTriaged.\n```twr-triage\n{\"items\": [{\"id\": \"{{finding_ids}}\", \"verdict\": \"dismissed\", \"reason\": \"the plan covers it\"}]}\n```\n";
/// The implement turn: writes `feature.rs`, gates green.
const IMPLEMENT: &str = "!write feature.rs pub fn export_csv() {}\nImplemented the plan.\n```twr-implement\n{\"status\": \"done\", \"gates\": {\"ran\": [\"true\"], \"result\": \"pass\"}}\n```\n";
/// The report turn of an implementation loop.
const IMPL_REPORT: &str = "Built the CSV export; nothing is left open.\n";
/// A commit's correction (the test commits for the thread: the fake runtime has no git).
const COMMITTED: &str = "Committed the rest.\n";

/// `twr loop implement api <task>`.
const IMPL_ARGS: &[&str] = &["loop", "implement", "api", "add", "a", "CSV", "export"];

/// The facts of a whole implementation loop on the `low` profile with the plan check `always`
/// ([`Env::implement`]): the plan, the plan check (p1, one finding, dismissed), the implement
/// turn, m1 (clean: the ladder converges, and the verification, which could only repeat it, is
/// skipped), the report. A loop that commits checks its commit before it ends.
const CLEAN_IMPL: &[&str] = &[
    "started",
    "phase Plan",
    "plan 1",
    "phase PlanCheck",
    "review p1",
    "findings p1 1",
    "triage p1",
    "phase Implement",
    "implemented Done",
    "phase Ladder",
    "review m1",
    "findings m1 0",
    "transition m1",
    "phase Report",
    "ended Clean",
];

/// [`CLEAN_IMPL`] of a loop that commits, its check passed the first time. `verify`: the
/// work was committed while m1 ran (its diff then reads otherwise: `feature.rs` tracked, no
/// longer untracked), so the verification round runs (clean).
fn clean_impl_committed(verify: bool) -> Vec<&'static str> {
    let mut want = CLEAN_IMPL.to_vec();
    if verify {
        let at = want.iter().position(|f| *f == "phase Report").unwrap();
        want.splice(at..at, ["phase Verify", "review v1", "findings v1 0"]);
    }
    want.insert(want.len() - 1, "commit passed");
    want
}

/// The thread steps of a whole implementation loop, in order.
const IMPL_ASKED: &[&str] = &["plan", "triage p1", "implement", "report"];

impl Env {
    /// An `Env` for implementation loops: [`Env::crash`]'s reviewer (its first run, the plan
    /// check, finds one problem, every later one none) and templates for the plan, the plan
    /// check's triage, the implement turn and the report.
    fn implement() -> Env {
        let env = Env::crash();
        for (name, text) in [
            ("plan", PLAN),
            ("plan-triage", PLAN_TRIAGE),
            ("implement", IMPLEMENT),
            ("impl-report", IMPL_REPORT),
        ] {
            env.template(name, text);
        }
        env
    }

    /// The thread `api` ([`Env::api_thread_unsaved`]: its `lib.rs` changed, the loop's
    /// baseline), with the `low` profile saved, the plan check `always` and `commit` (`end`,
    /// `none`): its id.
    fn impl_thread(&self, commit: &str) -> String {
        let id = self.api_thread_unsaved();
        self.ok(&[
            "loop",
            "settings",
            "set",
            "--profile",
            "low",
            "--plan-check",
            "always",
            "--implementation-commit",
            commit,
        ]);
        id
    }

    /// `twr loop implement api add a CSV export` on the saved settings: the loop's id.
    fn start_impl(&self) -> String {
        let out = self.ok(IMPL_ARGS);
        assert!(out.starts_with("api: implementation loop started"), "{out}");
        self.shown()["id"].as_str().unwrap().to_string()
    }

    /// The worktree of thread `api`.
    fn worktree(&self) -> PathBuf {
        let threads = self.json(&["thread", "list", "--json"]);
        let t = threads
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "api")
            .unwrap();
        PathBuf::from(t["worktree"].as_str().unwrap())
    }

    /// Commits `paths` in worktree `wt` ([`Env::worktree`]), as the thread would (the fake
    /// runtime runs no git).
    fn commit_paths(&self, wt: &Path, paths: &[&str]) -> String {
        let mut args = vec!["add", "--"];
        args.extend(paths);
        twr_core::testkit::git::run(wt, &args);
        twr_core::testkit::git::run(wt, &["commit", "-q", "-m", "Add the CSV export"]);
        twr_core::testkit::git::run(wt, &["rev-parse", "HEAD"])
    }

    /// Once the implement turn was taken and review run 2 (m1, held by the caller) has
    /// started, commits the turn's `feature.rs` (and writes `stray.rs` beside it,
    /// uncommitted, when `stray`), then lets the run go: the report turn then finds the work
    /// committed. It waits for the run's start, not only for the implement turn: the run's
    /// start has read m1's diff, so the commit always lands while m1 runs and the
    /// verification round always follows ([`clean_impl_committed`]). A commit that won the
    /// race against that read left the diff as it was, and the verification was skipped.
    fn commit_after_implement(&self, thread: &str, stray: bool) -> String {
        poll("the implement turn taken and m1's review started", || {
            let facts = facts(&self.events(thread));
            facts.contains(&"implemented Done".to_string())
                && facts.contains(&"review m1".to_string())
        });
        let wt = self.worktree();
        let sha = self.commit_paths(&wt, &["feature.rs"]);
        if stray {
            std::fs::write(wt.join("stray.rs"), "// left\n").unwrap();
        }
        self.release(2);
        sha
    }
}

/// Facts without the pause and resume a stuck step adds.
fn without_controls(facts: Vec<String>) -> Vec<String> {
    facts
        .into_iter()
        .filter(|f| !f.starts_with("control"))
        .collect()
}

/// An implementation loop from the command line: the first card holds the task (no terminal:
/// it stays open, with the `twr respond` lines that start it or run the task without a loop,
/// exit 2; `--json` the same); answered, the loop plans, has the plan checked, implements and
/// reviews; `show`, `report` (through `loop.report`) and `wait` read it. Then `twr thread new
/// --loop implement` on the saved settings, a held task run without a loop, and a task read
/// from stdin.
#[test]
fn an_implementation_loop_from_the_command_line() {
    let env = Env::implement();
    let _hub = env.start_hub();
    let thread = env.api_thread_unsaved();
    let first = [
        "loop",
        "implement",
        "api",
        "--profile",
        "low",
        "--plan-check",
        "always",
        "--commit",
        "none",
        "add",
        "a",
        "CSV",
        "export",
    ];
    let out = env.twr(&first);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    for want in [
        "api: implementation loop setup (first run",
        "Task         add a CSV export",
        "Baseline     1 path already changed: lib.rs",
        "Plan check   always",
        "Commit       none",
    ] {
        assert!(stdout.contains(want), "{want:?}:\n{stdout}");
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("\"start and save these settings\""),
        "{stderr}"
    );
    assert!(stderr.contains("\"run without a loop\""), "{stderr}");
    let mut json_args = first.to_vec();
    json_args.insert(3, "--json");
    let out = env.twr(&json_args);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["status"], "needs_setup", "{v}");
    assert_eq!(v["card"]["setup"]["kind"], "implement", "{v}");
    assert_eq!(v["card"]["setup"]["task"], "add a CSV export", "{v}");
    assert!(v["run_without"]
        .as_str()
        .unwrap()
        .ends_with("\"run without a loop\""));
    let card = respond_target(v["respond"].as_str().unwrap());
    env.ok(&["respond", &card, START]);
    let shown = env.wait_ended();
    assert_eq!(shown["outcome"], "clean", "{shown}");
    let evs = env.events(&thread);
    assert_eq!(facts(&evs), CLEAN_IMPL);
    let id = shown["id"].as_str().unwrap().to_string();
    assert_eq!(asked(&evs, &id), IMPL_ASKED);
    // `show`: the task, the baseline, the plan, the plan check, the implement turn.
    let show = env.ok(&["loop", "show", "api"]);
    for want in [
        "api: implementation loop ",
        "  task         add a CSV export",
        "  baseline     1 path already changed: lib.rs (out of scope",
        "plan 1: contract risk: yes — a new public endpoint (",
        "plan.md)",
        "implement: done, gates pass (true)",
        "  p1 medium (plan check): 1 finding",
        "  m1 medium: clean",
    ] {
        assert!(show.contains(want), "{want:?}:\n{show}");
    }
    // `report`, through `loop.report`: the text, wherever this runs.
    let report = env.ok(&["loop", "report", "api"]);
    assert!(report.contains("Built the CSV export"), "{report}");
    let r = env.json(&["loop", "report", "api", "--json"]);
    assert_eq!(r["written"], true, "{r}");
    assert_eq!(r["truncated"], false, "{r}");
    assert_eq!(r["kind"], "implement", "{r}");
    assert!(r["path"].as_str().unwrap().ends_with("report.md"), "{r}");
    assert_eq!(r["text"].as_str(), Some(report.as_str()));
    // `wait`: the loop's report is the thread's.
    let out = env.twr(&["wait", "api"]);
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("implementation loop clean after"),
        "{}",
        text(&out)
    );
    nothing_left_of(&env, &thread, &id, "implementation loop clean after");

    // A new thread with a loop, on the saved settings.
    let web = twr_core::testkit::git::init_repo(&env.home.join("web"));
    let out = env.ok(&[
        "thread",
        "new",
        web.to_str().unwrap(),
        "--runtime",
        "fake",
        "--name",
        "web",
        "--mode",
        "auto",
        "--loop",
        "implement",
        "add",
        "rate",
        "limiting",
    ]);
    assert!(out.starts_with("created web ("), "{out}");
    assert!(
        out.contains("\nweb: implementation loop started ("),
        "{out}"
    );
    assert!(out.contains("  task         add rate limiting"), "{out}");
    let mut web_loop = Value::Null;
    poll("web's loop's end", || {
        web_loop = env.json(&["loop", "show", "web", "--json"]);
        !web_loop["ended"].is_null()
    });
    assert_eq!(web_loop["outcome"], "clean", "{web_loop}");
    assert_eq!(web_loop["started"]["task"], "add rate limiting");

    // A held task run without a loop: the task goes to the thread as the user's message.
    let out = env.twr(&[
        "loop",
        "implement",
        "web",
        "--setup",
        "--json",
        "say",
        "hello",
    ]);
    assert_eq!(out.status.code(), Some(2), "{}", text(&out));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let card = respond_target(v["run_without"].as_str().unwrap());
    env.ok(&["respond", &card, "run without a loop"]);
    let web_id = web_loop["thread"].as_str().unwrap().to_string();
    poll("the task as the user's message", || {
        env.events(&web_id).iter().any(|e| {
            matches!(&e.body,
            twr_core::event::EventBody::TurnStarted(p)
                if p.prompt == "say hello" && p.from == twr_core::event::TurnFrom::User)
        })
    });
    assert_eq!(
        env.json(&["loop", "show", "web", "--json"])["id"],
        web_loop["id"],
        "no new loop"
    );

    // The task from stdin.
    let mut cli = env
        .cmd()
        .args(["loop", "implement", "api", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        let mut stdin = cli.stdin.take().unwrap();
        stdin.write_all(b"add tests\nfor the export\n").unwrap();
    }
    let out = cli.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("(+1 more line)"),
        "{}",
        text(&out)
    );
    let shown = env.wait_ended();
    assert_eq!(
        shown["started"]["task"], "add tests\nfor the export",
        "{shown}"
    );
}

/// Killed while the thread's plan turn, the plan check's triage turn or the implement turn
/// runs: the runtime goes with the hub, and the next hub asks for that step again, once; the
/// loop goes on to the end it would have had.
#[test]
fn an_implementation_loop_killed_during_each_new_turn() {
    for (template, text, step) in [
        ("plan", PLAN, "plan"),
        ("plan-triage", PLAN_TRIAGE, "triage p1"),
        ("implement", IMPLEMENT, "implement"),
    ] {
        let env = Env::implement();
        env.template(template, &holding(text));
        let hub = env.start_hub();
        let thread = env.impl_thread("none");
        let id = env.start_impl();
        poll(step, || {
            asked(&env.events(&thread), &id).contains(&step.to_string())
        });
        kill(hub);
        env.template(template, text);
        let _hub = env.start_hub();
        let shown = env.wait_ended();
        assert_eq!(shown["outcome"], "clean", "{step}: {shown}");
        let evs = env.events(&thread);
        assert_eq!(facts(&evs), CLEAN_IMPL, "{step}");
        let mut want: Vec<&str> = IMPL_ASKED.to_vec();
        let at = want.iter().position(|s| *s == step).unwrap();
        want.insert(at, step);
        assert_eq!(asked(&evs, &id), want, "{step}");
        assert_eq!(env.runs(), 2, "{step}");
        nothing_left_of(&env, &thread, &id, "implementation loop clean after");
    }
}

/// Killed while the plan check's review run is in flight: the next hub kills what is left of
/// it, records it failed and runs it again, once; the loop goes on.
#[test]
fn an_implementation_loop_killed_during_the_plan_check_run() {
    let env = Env::implement();
    env.finds(2);
    env.hold(1);
    let hub = env.start_hub();
    let thread = env.impl_thread("none");
    let id = env.start_impl();
    let pids = env.run_pids(1, true);
    kill(hub);
    let _hub = env.start_hub();
    for p in &pids {
        poll(&format!("reviewer process {p} killed"), || !alive(*p));
    }
    let shown = env.wait_ended();
    assert_eq!(shown["outcome"], "clean", "{shown}");
    let mut want = CLEAN_IMPL.to_vec();
    want.splice(5..5, ["failed p1", "review p1"]);
    let evs = env.events(&thread);
    assert_eq!(facts(&evs), want);
    assert_eq!(asked(&evs, &id), IMPL_ASKED);
    assert_eq!(env.runs(), 3);
    nothing_left_of(&env, &thread, &id, "implementation loop clean after");
}

/// Each new thread step's turn that ended while no hub reacted to it (the hub killed after it
/// recorded the turn's end, before it took the answer): the next hub takes it once, as at its
/// end, and goes on. The plan, the plan check's triage, the implement turn, and the report
/// turn of a loop that commits, whose check the next hub then runs.
#[test]
fn an_implementation_step_answered_while_no_hub_ran_is_taken_once() {
    let env = Env::implement();
    let (hub, gate) = env.start_hub_gated("answer plan");
    let thread = env.impl_thread("end");
    let wt = env.worktree();
    let id = env.start_until(IMPL_ARGS, hub, &gate, &thread);
    let mut sha = String::new();
    for step in IMPL_ASKED {
        if *step != "plan" {
            let (hub, gate) = env.start_hub_gated(&format!("answer {step}"));
            reached(&gate);
            kill(hub);
        }
        let evs = env.events(&thread);
        let last = asked(&evs, &id);
        assert_eq!(
            last.last().map(String::as_str),
            Some(*step),
            "{last:?}\n{}",
            std::fs::read_to_string(env.home.join("hub-test.log")).unwrap_or_default()
        );
        // Its turn ended, completed, and nothing of the loop's came after it.
        let end = evs
            .iter()
            .rposition(|e| matches!(&e.body, twr_core::event::EventBody::TurnEnded(_)))
            .unwrap();
        assert!(
            matches!(&evs[end].body,
            twr_core::event::EventBody::TurnEnded(p) if p.result == twr_core::event::TurnResult::Completed),
            "{step}: {:?}",
            evs[end]
        );
        assert_eq!(
            loop_turns(&evs, &id).last().map(|(_, t)| *t),
            evs[end].turn,
            "{step}"
        );
        assert!(
            !evs[end..].iter().any(|e| e.kind().starts_with("loop.")),
            "{step}: {:?}\n{}",
            evs[end..]
                .iter()
                .map(|e| (e.ts.to_string(), e.kind().to_string()))
                .collect::<Vec<_>>(),
            std::fs::read_to_string(env.home.join("hub-test.log")).unwrap_or_default()
        );
        if *step == "implement" {
            sha = env.commit_paths(&wt, &["feature.rs"]);
        }
    }
    let _hub = env.start_hub();
    let shown = env.wait_ended();
    assert_eq!(shown["outcome"], "clean", "{shown}");
    let evs = env.events(&thread);
    assert_eq!(facts(&evs), clean_impl_committed(false));
    assert_eq!(asked(&evs, &id), IMPL_ASKED);
    assert_eq!(shown["ended"]["commits"][0]["sha"], sha.as_str(), "{shown}");
    assert!(shown["ended"]["commit_problem"].is_null(), "{shown}");
    for (step, turn) in loop_turns(&evs, &id) {
        let reports: Vec<_> = evs
            .iter()
            .filter(|e| e.turn == Some(turn))
            .filter_map(|e| match &e.body {
                twr_core::event::EventBody::ReportReady(r) => Some(r),
                _ => None,
            })
            .collect();
        // (The report turn's: marked `checking`, then the loop's last, the hub's.)
        let n = if step == "report" { 2 } else { 1 };
        assert_eq!(reports.len(), n, "{step}: {reports:?}");
        let mark = reports[0]
            .ext
            .as_ref()
            .map(|x| x["twr"]["loop"]["step"].clone());
        assert_eq!(mark, Some(json!(step)), "{step}: {reports:?}");
    }
    nothing_left_of(&env, &thread, &id, "implementation loop clean after");
}

/// Each new thread step's turn that failed while no hub reacted to it: the next hub takes it
/// as at its end, the loop paused with its stuck item, which `resume` answers; the step is
/// asked again and the loop goes on. The plan, the plan check's triage, the implement turn,
/// and the commit's correction.
#[test]
fn an_implementation_step_that_failed_while_no_hub_ran_makes_the_loop_stuck() {
    for (template, text, step) in [
        ("plan", PLAN, "plan"),
        ("plan-triage", PLAN_TRIAGE, "triage p1"),
        ("implement", IMPLEMENT, "implement"),
        ("correction", COMMITTED, "commit"),
    ] {
        let env = Env::implement();
        env.template(template, "!error the gates broke\n");
        let commit = step == "commit";
        let (hub, gate) = env.start_hub_gated(&format!("failed {step}"));
        let thread = env.impl_thread(if commit { "end" } else { "none" });
        let wt = env.worktree();
        if commit {
            // The first check fails (a stray file), and its correction fails.
            env.hold(2);
            env.ok(IMPL_ARGS);
            env.commit_after_implement(&thread, true);
            reached(&gate);
            kill(hub);
        } else {
            env.start_until(IMPL_ARGS, hub, &gate, &thread);
        }
        let id = env
            .events(&thread)
            .iter()
            .find_map(|e| match &e.body {
                twr_core::event::EventBody::LoopStarted(p) => Some(p.id.clone()),
                _ => None,
            })
            .unwrap();
        assert!(!facts(&env.events(&thread))
            .iter()
            .any(|f| f.starts_with("control")));
        env.template(template, text);
        let _hub = env.start_hub();
        let stuck = env.item_marked(&thread, "loop_stuck");
        assert!(
            stuck["summary"]
                .as_str()
                .unwrap()
                .contains("implementation loop is stuck"),
            "{step}: {stuck}"
        );
        assert_eq!(env.shown()["state"]["paused"], true, "{step}");
        if commit {
            env.commit_paths(&wt, &["stray.rs"]);
        }
        env.ok(&["respond", stuck["target"].as_str().unwrap(), "resume"]);
        let shown = env.wait_ended();
        assert_eq!(shown["outcome"], "clean", "{step}: {shown}");
        let evs = env.events(&thread);
        let got = facts(&evs);
        assert_eq!(
            got.iter().filter(|f| f.starts_with("control")).count(),
            2,
            "{step}: {got:?}"
        );
        let want: Vec<String> = if commit {
            let mut w = clean_impl_committed(true);
            w.splice(w.len() - 2..w.len() - 1, ["commit failed", "commit passed"]);
            w.into_iter().map(String::from).collect()
        } else {
            CLEAN_IMPL.iter().map(|s| s.to_string()).collect()
        };
        assert_eq!(without_controls(got), want, "{step}");
        let mut asked_want: Vec<String> = IMPL_ASKED.iter().map(|s| s.to_string()).collect();
        if commit {
            asked_want.extend(["commit correction".into(), "commit correction".into()]);
        } else {
            let at = asked_want.iter().position(|s| s == step).unwrap();
            asked_want.insert(at, step.to_string());
        }
        assert_eq!(asked(&evs, &id), asked_want, "{step}");
        nothing_left_of(&env, &thread, &id, "implementation loop clean after");
    }
}

/// Killed while the commit's check runs (after it read git, before it recorded what it
/// found), and between the check and `report.md` (the check recorded, the report not yet):
/// the next hub checks again, or writes the report, once; the loop ends as it would have, its
/// commit named, nothing asked again.
#[test]
fn an_implementation_loop_killed_while_its_commit_is_checked_or_reported() {
    for point in ["checked report", "report written"] {
        let env = Env::implement();
        env.hold(2);
        let (hub, gate) = env.start_hub_gated(point);
        let thread = env.impl_thread("end");
        let id = env.start_impl();
        let sha = env.commit_after_implement(&thread, false);
        reached(&gate);
        kill(hub);
        let before = facts(&env.events(&thread));
        assert_eq!(
            before.contains(&"commit passed".to_string()),
            point == "report written",
            "{point}: {before:?}"
        );
        assert!(!before.contains(&"ended Clean".to_string()), "{point}");
        let _hub = env.start_hub();
        let shown = env.wait_ended();
        assert_eq!(shown["outcome"], "clean", "{point}: {shown}");
        let evs = env.events(&thread);
        assert_eq!(facts(&evs), clean_impl_committed(true), "{point}");
        assert_eq!(asked(&evs, &id), IMPL_ASKED, "{point}");
        assert_eq!(shown["ended"]["commits"][0]["sha"], sha.as_str(), "{point}");
        let report = env.ok(&["loop", "report", "api"]);
        assert!(report.contains("## Commits"), "{point}: {report}");
        assert!(report.contains(&sha), "{point}: {report}");
        assert!(report.contains("Built the CSV export"), "{point}: {report}");
        nothing_left_of(&env, &thread, &id, "implementation loop clean after");
    }
}

/// Killed while the commit's correction turn runs (the first check found a file left
/// uncommitted): the next hub asks for the correction again, once, and its check passes.
#[test]
fn an_implementation_loop_killed_during_its_commit_correction() {
    let env = Env::implement();
    env.hold(2);
    env.template("correction", &holding(COMMITTED));
    let hub = env.start_hub();
    let thread = env.impl_thread("end");
    let id = env.start_impl();
    let wt = env.worktree();
    env.commit_after_implement(&thread, true);
    poll("the commit's correction", || {
        asked(&env.events(&thread), &id).contains(&"commit correction".to_string())
    });
    kill(hub);
    env.template("correction", COMMITTED);
    // (What the thread does in the correction: the fake runtime has no git.)
    env.commit_paths(&wt, &["stray.rs"]);
    let _hub = env.start_hub();
    let shown = env.wait_ended();
    assert_eq!(shown["outcome"], "clean", "{shown}");
    let evs = env.events(&thread);
    let mut want = clean_impl_committed(true);
    want.splice(
        want.len() - 2..want.len() - 1,
        ["commit failed", "commit passed"],
    );
    assert_eq!(facts(&evs), want);
    let mut asked_want = IMPL_ASKED.to_vec();
    asked_want.extend(["commit correction", "commit correction"]);
    assert_eq!(asked(&evs, &id), asked_want);
    assert!(shown["ended"]["commit_problem"].is_null(), "{shown}");
    nothing_left_of(&env, &thread, &id, "implementation loop clean after");
}

/// A message of the user's sent as the report turn ends (its turn starts right after it, and
/// runs while the commit is checked), then the hub killed during the check: the next hub runs
/// the check from the report turn's answer, and does not ask for the report again because a
/// turn of the user's was cut after it.
#[test]
fn a_users_message_as_the_report_turn_ends_then_a_kill_during_the_check() {
    let env = Env::implement();
    env.hold(2);
    env.template("impl-report", &format!("!sleep 1500\n{IMPL_REPORT}"));
    let (hub, gate) = env.start_hub_gated("checked report");
    let thread = env.impl_thread("end");
    let id = env.start_impl();
    let sha = env.commit_after_implement(&thread, false);
    poll("the report turn", || {
        asked(&env.events(&thread), &id).contains(&"report".to_string())
    });
    env.ok(&["send", "api", "!sleep 600000\nhello"]);
    reached(&gate);
    // The user's turn runs, the check is held.
    assert!(env.events(&thread).iter().any(|e| matches!(&e.body,
        twr_core::event::EventBody::TurnStarted(p) if p.prompt.ends_with("hello"))));
    kill(hub);
    let _hub = env.start_hub();
    let shown = env.wait_ended();
    assert_eq!(shown["outcome"], "clean", "{shown}");
    let evs = env.events(&thread);
    assert_eq!(facts(&evs), clean_impl_committed(true));
    assert_eq!(asked(&evs, &id), IMPL_ASKED);
    assert_eq!(shown["ended"]["commits"][0]["sha"], sha.as_str(), "{shown}");
    // Nothing of the loop's was withdrawn either: no report prompt was queued again.
    assert!(!evs.iter().any(|e| matches!(&e.body,
        twr_core::event::EventBody::PromptWithdrawn(p) if p.prompt.contains(&format!("[twr loop {id} report")))));
}

// ---------------------------------------------------------------------------------------------
// The sweep audited by runs (`sweep = "runs"`), killed at its points
// ---------------------------------------------------------------------------------------------

/// The rules turn's template: two rules (the fake thread echoes it).
const RULES: &str = "Named.\n```twr-rules\n{\"rules\": [{\"name\": \"guard-input\", \"rule\": \"Every parser guards empty input.\", \"where\": \"lib.rs\"}, {\"name\": \"trim-input\", \"rule\": \"Every parser trims its input.\", \"where\": \"everywhere the diff touches\"}]}\n```\n";
/// The verification turn's template: both rules checked clean.
const SWEEP_VERIFY: &str = "Verified.\n```twr-sweep\n{\"rules\": [{\"rule\": \"guard-input\", \"result\": \"checked clean\"}, {\"rule\": \"trim-input\", \"result\": \"checked clean\"}], \"gates\": {\"ran\": [\"true\"], \"result\": \"pass\"}}\n```\n";

/// The facts of a whole `medium` loop with `sweep = "runs"` and `max_audit_runs = 1` that
/// nothing interrupted ([`Env::crash_runs`]): runs 1 and 2 are m1 and m2, 3 and 4 the audits of
/// the two rules, 5 v1.
const CLEAN_RUNS: &[&str] = &[
    "started",
    "phase Scope",
    "phase Ladder",
    "review m1",
    "findings m1 1",
    "triage m1",
    "review m2",
    "findings m2 0",
    "transition m2",
    "phase Sweep",
    "rules 2",
    "audit s1",
    "audited s1 0",
    "audit s2",
    "audited s2 0",
    "phase Verify",
    "review v1",
    "findings v1 0",
    "phase Report",
    "ended Clean",
];

impl Env {
    /// [`Env::crash`] with the sweep audited by runs, `max_audit_runs` at once: the rules turn
    /// names two rules ([`RULES`]), the verification turn reports both ([`SWEEP_VERIFY`]).
    fn crash_runs(max_audit_runs: u32) -> Env {
        let env = Env::crash();
        env.template("rules", RULES);
        env.template("sweep-verify", SWEEP_VERIFY);
        std::fs::write(
            env.home.join("config.toml"),
            format!("[loops]\nmax_audit_runs = {max_audit_runs}\n"),
        )
        .unwrap();
        env
    }

    /// [`Env::api_thread`], its saved settings auditing the sweep by runs.
    fn api_thread_runs(&self) -> String {
        let id = self.api_thread();
        self.ok(&["loop", "settings", "set", "--sweep", "runs"]);
        id
    }
}

/// Killed with both audit runs in flight: they outlive the hub, the next hub kills them before
/// anything runs, records each failed ("the hub stopped while it ran") and runs each again,
/// once; the loop goes on to its end, its verification prompt asked once.
#[test]
fn a_hub_killed_with_audits_in_flight() {
    let env = Env::crash_runs(2);
    let hub = env.start_hub();
    let thread = env.api_thread_runs();
    env.hold(3);
    env.hold(4);
    let id = env.start_loop();
    let pids: Vec<i32> = [3, 4]
        .into_iter()
        .flat_map(|n| env.run_pids(n, true))
        .collect();
    kill(hub);
    assert!(pids.iter().all(|p| alive(*p)), "{pids:?}");
    let got = facts(&env.events(&thread));
    assert_eq!(got[..11], CLEAN_RUNS[..11], "{got:?}");
    let mut started: Vec<&String> = got[11..].iter().collect();
    started.sort();
    assert_eq!(started, ["audit s1", "audit s2"], "{got:?}");
    let _hub = env.start_hub();
    for p in &pids {
        poll(&format!("audit process {p} killed"), || !alive(*p));
    }
    let shown = env.wait_ended();
    assert_eq!(shown["outcome"], "clean", "{shown}");
    let evs = env.events(&thread);
    let got = facts(&evs);
    let count = |f: &str| got.iter().filter(|g| *g == f).count();
    for rule in ["s1", "s2"] {
        assert_eq!(count(&format!("audit {rule}")), 2, "{got:?}");
        assert_eq!(count(&format!("audit failed {rule}")), 1, "{got:?}");
        assert_eq!(count(&format!("audited {rule} 0")), 1, "{got:?}");
    }
    assert_eq!(got[got.len() - 5..], CLEAN_RUNS[CLEAN_RUNS.len() - 5..]);
    let rules = shown["sweep"]["rules"].as_array().unwrap();
    assert_eq!(
        rules[0]["audit"]["failures"][0],
        "the hub stopped while it ran"
    );
    assert_eq!(rules[1]["audit"]["status"], "answered");
    assert_eq!(env.runs(), 7);
    assert_eq!(
        asked(&evs, &id),
        ["scope", "triage m1", "rules", "sweep", "report"]
    );
    nothing_left(&env, &thread, &id);
}

/// Killed right before the hub records the last audit's answer: that answer and the
/// verification prompt are one batch, so the next hub finds the audit in flight, records it
/// failed and runs it again, once, then asks for the verification turn once.
#[test]
fn a_hub_killed_before_it_records_the_last_audit() {
    let env = Env::crash_runs(1);
    let (hub, gate) = env.start_hub_gated("audit s2");
    let thread = env.api_thread_runs();
    // (`api_thread_runs` saved the settings through this hub before it reached the gate.)
    let id = env.start_loop_until(hub, &gate, &thread);
    let got = facts(&env.events(&thread));
    assert_eq!(got, CLEAN_RUNS[..14], "{got:?}");
    let _hub = env.start_hub();
    let shown = env.wait_ended();
    assert_eq!(shown["outcome"], "clean", "{shown}");
    let evs = env.events(&thread);
    let mut want: Vec<&str> = CLEAN_RUNS.to_vec();
    want.splice(14..14, ["audit failed s2", "audit s2"]);
    assert_eq!(facts(&evs), want);
    assert_eq!(env.runs(), 6);
    assert_eq!(
        asked(&evs, &id),
        ["scope", "triage m1", "rules", "sweep", "report"]
    );
    nothing_left(&env, &thread, &id);
}

/// Killed during the rules turn and during the verification turn: the next hub asks for the
/// step again, once; the loop goes on to its end with every fact once.
#[test]
fn a_hub_killed_during_the_sweeps_thread_turns() {
    for (template, text, step) in [
        ("rules", RULES, "rules"),
        ("sweep-verify", SWEEP_VERIFY, "sweep"),
    ] {
        let env = Env::crash_runs(1);
        env.template(template, &holding(text));
        let hub = env.start_hub();
        let thread = env.api_thread_runs();
        let id = env.start_loop();
        poll(step, || {
            asked(&env.events(&thread), &id).contains(&step.to_string())
        });
        kill(hub);
        env.template(template, text);
        let _hub = env.start_hub();
        let shown = env.wait_ended();
        assert_eq!(shown["outcome"], "clean", "{step}: {shown}");
        let evs = env.events(&thread);
        assert_eq!(facts(&evs), CLEAN_RUNS, "{step}");
        let mut want = vec!["scope", "triage m1", "rules", "sweep", "report"];
        let at = want.iter().position(|s| *s == step).unwrap();
        want.insert(at, step);
        assert_eq!(asked(&evs, &id), want, "{step}");
        assert_eq!(env.runs(), 5, "{step}");
        nothing_left(&env, &thread, &id);
    }
}
