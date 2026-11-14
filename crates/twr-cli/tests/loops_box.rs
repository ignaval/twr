//! `twr loop` on a remote box, process against process: a real `twr hub` (its own `TWR_HOME`)
//! with its runner listener on loopback, and a real dial-in `twr runner` of box `lab` (another
//! `TWR_HOME`), on this one machine. The thread runs the fake runtime on the box, which echoes
//! each prompt, so the loop's prompts answer themselves (the templates end with each step's
//! block, as in `loops.rs`). The reviewer is a fake Codex that only the box's runner has
//! (`TWR_CODEX_BIN` in its environment); the hub is told it has no Codex at all
//! (`TWR_TEST_AGENTS`).
//!
//! Then the hub killed outright (SIGKILL) while the box runs a review run: the runner ends the
//! run with the connection, the next hub records it failed and runs it again once the box is
//! back, and the loop ends as it would have.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

const TWR: &str = env!("CARGO_BIN_EXE_twr");
const WAIT: Duration = Duration::from_secs(60);
const TOKEN: &str = "5a1c0d2e3f405162738495a6b7c8d9e0f1a2b3c4d5e6f708192a3b4c5d6e7f80";
const START: &str = "start and save these settings";

/// What the hub finds of its own agent CLIs: none.
const NO_AGENTS: &str = r#"[{"runtime": "codex", "found": false, "login": "unknown"},
    {"runtime": "claude-code", "found": false, "login": "unknown"},
    {"runtime": "grok-build", "found": false, "login": "unknown"}]"#;

/// A fake `codex exec` on the box: run `n` (`count`) answers one finding on the first run and
/// none after, to its `-o` file; with `hold.<n>` it starts a child and waits until the file
/// goes (its pid in `pid.<n>`, the child's in `child.<n>`).
const FAKE_CODEX: &str = r#"#!/bin/sh
here=$(dirname "$0")
n=$(( $(cat "$here/count" 2>/dev/null || echo 0) + 1 ))
echo "$n" > "$here/count"
pwd > "$here/cwd.$n"
cat > "$here/stdin.$n"
if [ -f "$here/hold.$n" ]; then
  sleep 600 &
  echo $! > "$here/child.$n"
  echo $$ > "$here/pid.$n"
  while [ -f "$here/hold.$n" ]; do sleep 0.05; done
  kill $! 2>/dev/null
fi
out=""; prev=""
for a in "$@"; do [ "$prev" = "-o" ] && out="$a"; prev="$a"; done
if [ "$n" = 1 ]; then
  printf '%s' '{"findings": [{"severity": "major", "location": "lib.rs:1", "problem": "it breaks on empty input", "fix": "guard it"}]}' > "$out"
else
  printf '%s' '{"findings": []}' > "$out"
fi
echo '{"type":"turn.completed","usage":{"input_tokens":100,"output_tokens":10}}'
"#;

const GOAL: &str = "Gates are green.\n```twr-goal\nHarden the change against bad input.\n```\n";
const TRIAGE: &str = "{{findings}}\nTriaged.\n```twr-triage\n{\"items\": [{\"id\": \"{{finding_ids}}\", \"verdict\": \"dismissed\", \"reason\": \"empty input never reaches it\"}], \"gates\": {\"ran\": [\"true\"], \"result\": \"pass\"}}\n```\n";
const REPORT: &str = "The loop reviewed the change; nothing is left open.\n";

/// A child process killed and reaped when dropped.
struct Proc(Option<Child>);

impl Proc {
    fn kill(&mut self) {
        if let Some(mut c) = self.0.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        self.kill();
    }
}

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    url: String,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let (hub, boxh) = (root.join("hub"), root.join("box"));
        for d in [&hub, &boxh, &root.join("tmp"), &root.join("reviewers")] {
            std::fs::create_dir_all(d).unwrap();
        }
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        std::fs::write(
            hub.join("config.toml"),
            format!("[hub]\nlisten = \"127.0.0.1:{port}\"\n\n[boxes.lab]\ntoken = \"{TOKEN}\"\n"),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            hub.join("config.toml"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let templates = hub.join("loops").join("templates");
        std::fs::create_dir_all(&templates).unwrap();
        for (name, text) in [("goal", GOAL), ("triage", TRIAGE), ("report", REPORT)] {
            std::fs::write(templates.join(format!("{name}.md")), text).unwrap();
        }
        let codex = root.join("reviewers").join("codex");
        std::fs::write(&codex, FAKE_CODEX).unwrap();
        std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755)).unwrap();
        let token = boxh.join("lab.token");
        std::fs::write(&token, format!("{TOKEN}\n")).unwrap();
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
        Env {
            _dir: dir,
            url: format!("ws://127.0.0.1:{port}"),
            root,
        }
    }

    fn cmd(&self, home: &Path) -> Command {
        let mut c = Command::new(TWR);
        c.env_remove("TWR_WORKSPACE")
            .env_remove("RUST_LOG")
            .env_remove("VISUAL")
            .env_remove("TWR_CODEX_BIN")
            .env_remove("TWR_RUNNER_TOKEN")
            .env("TWR_HOME", home)
            .env("TMPDIR", self.root.join("tmp"))
            .env("TWR_TEST_USER_TOKEN_FILE", home.join("user.token"))
            .env("TWR_AGENT_PROBE", "off")
            .env("TWR_BOX_PROBE", "off")
            .env("TWR_ENABLE_FAKE", "1")
            .stdin(Stdio::null());
        c
    }

    fn hub_home(&self) -> PathBuf {
        self.root.join("hub")
    }

    /// `twr <args>` against the hub.
    fn twr(&self, args: &[&str]) -> Output {
        self.cmd(&self.hub_home())
            .env("TWR_TEST_AGENTS", NO_AGENTS)
            .args(args)
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.twr(args);
        assert!(out.status.success(), "twr {args:?}: {}", text(&out));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn json(&self, args: &[&str]) -> Value {
        serde_json::from_str(&self.ok(args)).unwrap()
    }

    fn start_hub(&self) -> Proc {
        let home = self.hub_home();
        let _ = std::fs::remove_file(home.join("hub.sock"));
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("hub.log"))
            .unwrap();
        let hub = Proc(Some(
            self.cmd(&home)
                .env("TWR_TEST_AGENTS", NO_AGENTS)
                .arg("hub")
                .stdout(log.try_clone().unwrap())
                .stderr(log)
                .spawn()
                .unwrap(),
        ));
        poll("the hub", || self.twr(&["thread", "list"]).status.success());
        hub
    }

    /// The box's runner: a real `twr runner` of its own home, the only one with the fake
    /// Codex.
    fn start_runner(&self) -> Proc {
        let home = self.root.join("box");
        let log = std::fs::File::create(self.root.join("runner.log")).unwrap();
        Proc(Some(
            self.cmd(&home)
                .env("TWR_CODEX_BIN", self.root.join("reviewers").join("codex"))
                .env("XDG_STATE_HOME", self.root.join("state"))
                .args([
                    "runner",
                    "--hub",
                    &self.url,
                    "--box=lab",
                    &format!("--token-file={}", home.join("lab.token").display()),
                    &format!("--data-dir={}", home.join("data").display()),
                ])
                .stdout(Stdio::null())
                .stderr(log)
                .spawn()
                .unwrap(),
        ))
    }

    fn online(&self) {
        poll("the box online", || {
            let out = self.twr(&["box", "status", "lab", "--json"]);
            out.status.success()
                && serde_json::from_slice::<Value>(&out.stdout)
                    .is_ok_and(|v| v["state"] == "online")
        });
    }

    fn runs(&self) -> usize {
        std::fs::read_to_string(self.root.join("reviewers").join("count"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    fn pid(&self, name: &str) -> i32 {
        let path = self.root.join("reviewers").join(name);
        let mut pid = 0;
        poll(name, || {
            pid = std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
            pid != 0
        });
        pid
    }

    /// Thread `api` on the box, in a fresh repository, with a change; its loop asked for
    /// (`--profile low`) and its card answered: the loop's id.
    fn start_loop(&self) -> String {
        let repo = twr_core::testkit::git::init_repo(&self.root.join("api"));
        let created = self.ok(&[
            "thread",
            "new",
            repo.to_str().unwrap(),
            "--box",
            "lab",
            "--runtime",
            "fake",
            "--name",
            "api",
            "--mode",
            "auto",
        ]);
        assert!(created.starts_with("created api ("), "{created}");
        let threads = self.json(&["thread", "list", "--json"]);
        let wt = threads[0]["worktree"].as_str().unwrap().to_string();
        std::fs::write(Path::new(&wt).join("lib.rs"), "fn parse(s: &str) {}\n").unwrap();
        // The first loop: its card, answered from another command (no terminal here).
        let out = self.twr(&["loop", "review", "api", "--profile", "low"]);
        assert_eq!(out.status.code(), Some(2), "{}", text(&out));
        let said = String::from_utf8_lossy(&out.stdout).into_owned();
        let at = said.find("card: ").expect(&said) + "card: ".len();
        let target = said[at..].split_whitespace().next().unwrap().to_string();
        self.ok(&["respond", &target, START]);
        let mut id = String::new();
        poll("the loop's start", || {
            let out = self.twr(&["loop", "show", "api", "--json"]);
            id = serde_json::from_slice::<Value>(&out.stdout)
                .ok()
                .and_then(|v| v["id"].as_str().map(str::to_string))
                .unwrap_or_default();
            !id.is_empty()
        });
        id
    }

    fn wait_ended(&self) -> Value {
        let mut shown = Value::Null;
        poll("the loop's end", || {
            shown = self.json(&["loop", "show", "api", "--json"]);
            !shown["ended"].is_null()
        });
        shown
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

fn alive(pid: i32) -> bool {
    let out = Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout);
    let stat = stat.trim();
    !stat.is_empty() && !stat.starts_with('Z')
}

/// A review loop on a dial-in box from the command line: the round runs the box's Codex in
/// the thread's worktree there, its files in the box's copy of the loop folder; the hub's
/// report reads with the box gone.
#[test]
fn a_review_loop_on_a_dial_in_box() {
    let env = Env::new();
    let _hub = env.start_hub();
    let mut runner = env.start_runner();
    env.online();
    let id = env.start_loop();
    let shown = env.wait_ended();
    assert_eq!(shown["outcome"], "clean", "{shown}");
    // The round (one finding, dismissed) and its verification ran on the box.
    assert_eq!(env.runs(), 2);
    let folder = env.root.join("box").join("data").join("loops").join(&id);
    let prompt = std::fs::read_to_string(env.root.join("reviewers").join("stdin.1")).unwrap();
    assert!(
        prompt.contains(&folder.join("diff-api-m1.patch").display().to_string()),
        "{prompt}"
    );
    assert!(folder.join("ledger.md").is_file());
    assert!(std::fs::read_to_string(folder.join("ledger.md"))
        .unwrap()
        .contains("empty input never reaches it"));
    // The report, with the box gone.
    runner.kill();
    let report = env.ok(&["loop", "report", "api"]);
    assert!(report.contains("# Review loop report"), "{report}");
}

/// The hub killed (SIGKILL) while the box runs a review run: the runner ends the run with its
/// connection (the reviewer and its child are gone), the next hub records the run failed and
/// runs it again once the box is back, and the loop ends clean.
#[test]
fn a_hub_killed_during_a_review_run_on_a_box() {
    let env = Env::new();
    let mut hub = env.start_hub();
    let _runner = env.start_runner();
    env.online();
    std::fs::write(env.root.join("reviewers").join("hold.1"), "").unwrap();
    let _id = env.start_loop();
    let (pid, child) = (env.pid("pid.1"), env.pid("child.1"));
    hub.kill();
    poll("the box's review run ended", || {
        !alive(pid) && !alive(child)
    });
    let _ = std::fs::remove_file(env.root.join("reviewers").join("hold.1"));
    let _hub = env.start_hub();
    let shown = env.wait_ended();
    assert_eq!(shown["outcome"], "clean", "{shown}");
    let failed = shown["rounds"][0]["runs"]
        .as_array()
        .map(|r| r.len())
        .unwrap_or(0);
    assert_eq!(failed, 2, "{shown}");
    // m1 failed with the hub and ran again (the fake's second run: clean, so `low` skips its
    // verification, which could only repeat it).
    assert_eq!(env.runs(), 2);
}
