//! `twr loop` against a real `twr hub` (docs/usage.md, "Loops"): a fake thread in a git
//! repository, a fake Codex reviewer (`TWR_CODEX_BIN`), and a hub told that Codex is installed
//! and logged in (`TWR_TEST_AGENTS`, honoured by a test's throwaway hub only).
//!
//! The fake runtime echoes each prompt it is sent, so the loop's own prompts answer themselves:
//! the templates in `<config dir>/loops/templates/` (which replace the built-in ones) end with
//! the block each step asks for, a GOAL paragraph, a triage that dismisses the round's one
//! finding, and a report.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

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
n=$(( $(cat "$here/count" 2>/dev/null || echo 0) + 1 ))
echo "$n" > "$here/count"
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
