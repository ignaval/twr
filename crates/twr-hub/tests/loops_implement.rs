//! Implementation loops in the hub (`hub::loops`, docs/design/loops.md, milestone 2), and the
//! commit of either kind of loop: whole loops run by a scripted thread ([`Brain`]) that edits
//! and commits in its worktree with real git, and fake reviewer CLIs ([`Env::reviewers`]); the
//! setup card that holds a task and its three answers, for an existing thread and for
//! `thread.new {loop}`; the named implementer of a new thread; the refusals; a hub restart in
//! each new phase.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::time::timeout;
use twr_adapters::fake::FakeAdapter;
use twr_core::adapter::{Adapter, Answer};
use twr_core::config::Config;
use twr_core::event::*;
use twr_core::loops::config::{ImplCommit, ModelChoice, PlanCheck, ReviewerChoice};
use twr_core::loops::settings::LoopSettings;
use twr_core::loops::{ControlAction, Outcome};
use twr_core::proto::codes;
use twr_core::runner::AgentInfo;
use twr_core::testkit::git as tg;
use twr_hub::hub::{
    AgentProbe, LoopControlParams, LoopGet, LoopRequest, LoopSettingsSet, LoopSetupUpdate,
    NewThreadLoop,
};
use twr_hub::{Caller, Hub, HubOptions, NewThread};

const WAIT: Duration = Duration::from_secs(30);
const START: &str = "start and save these settings";
const WITHOUT: &str = "run without a loop";
const TASK: &str = "add a CSV export to the reports page";

struct Env {
    dir: tempfile::TempDir,
    config: Config,
    brain: Arc<Mutex<Brain>>,
}

impl Env {
    /// An `Env` whose loops run: the thread answers each step, and the reviewers each round.
    fn new() -> Env {
        let env = Env {
            dir: tempfile::tempdir().unwrap(),
            config: Config {
                default_runtime: "fake".into(),
                ..Default::default()
            },
            brain: Arc::new(Mutex::new(Brain {
                auto: true,
                ..Default::default()
            })),
        };
        let dir = env.reviewers();
        std::fs::create_dir_all(dir.join("answers")).unwrap();
        for name in ["codex", "claude", "grok"] {
            let bin = dir.join(name);
            std::fs::write(&bin, FAKE_REVIEWER).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        env
    }

    fn reviewers(&self) -> PathBuf {
        self.dir.path().join("reviewers")
    }

    /// The `n`th review run's answer (from 1).
    fn answer(&self, n: usize, findings: Value) {
        std::fs::write(
            self.reviewers().join("answers").join(format!("{n}.json")),
            json!({ "findings": findings }).to_string(),
        )
        .unwrap();
    }

    fn hold_run(&self, n: usize) {
        std::fs::write(self.reviewers().join(format!("hold.{n}")), "").unwrap();
    }

    fn release_run(&self, n: usize) {
        let _ = std::fs::remove_file(self.reviewers().join(format!("hold.{n}")));
    }

    fn runs(&self) -> usize {
        std::fs::read_to_string(self.reviewers().join("count"))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    }

    /// What the `n`th review run read on its standard input (its prompt).
    fn run_prompt(&self, n: usize) -> String {
        std::fs::read_to_string(self.reviewers().join(format!("stdin.{n}"))).unwrap_or_default()
    }

    fn opts(&self) -> HubOptions {
        let mut o = HubOptions::new(
            self.dir.path().join("data").join("twr.db"),
            self.config.clone(),
        );
        let brain = self.brain.clone();
        o.adapters = Arc::new(move |name: &str| -> Option<Arc<dyn Adapter>> {
            match name {
                "fake" | "claude-code" => Some(Arc::new(Scripted {
                    brain: brain.clone(),
                }) as Arc<dyn Adapter>),
                _ => None,
            }
        });
        let bins = self.reviewers();
        o.reviewer_bins = [
            ("codex", "codex"),
            ("claude-code", "claude"),
            ("grok-build", "grok"),
        ]
        .into_iter()
        .map(|(rt, b)| (rt.to_string(), bins.join(b)))
        .collect();
        o.job_gate_exe = Some(env!("CARGO_BIN_EXE_twr-job-gate").into());
        let native = self.dir.path().join("native");
        o.native_dir = Some(Arc::new(move |rt: &str| Some(native.join(rt))));
        o.git_tick = Some(Duration::ZERO);
        o.stall_after = Some(Duration::ZERO);
        o.disk_tick = Some(Duration::ZERO);
        o.checkpoint_tick = Some(Duration::ZERO);
        o.config_file = Some(self.dir.path().join("config.toml"));
        let probe: AgentProbe = Arc::new(move || {
            let agents: Vec<AgentInfo> = [
                ("claude-code", "logged_in"),
                ("codex", "logged_in"),
                ("grok-build", "unknown"),
            ]
            .iter()
            .map(|(rt, login)| {
                serde_json::from_value(json!({"runtime": rt, "found": true, "login": login}))
                    .unwrap()
            })
            .collect();
            futures::FutureExt::boxed(async move { agents })
        });
        o.agent_probe = Some(probe);
        o
    }

    async fn hub(&self) -> Hub {
        Hub::open(self.opts()).await.unwrap()
    }

    fn repo(&self, name: &str) -> PathBuf {
        tg::init_repo(&self.dir.path().join(name))
    }

    fn set<F: FnOnce(&mut Brain)>(&self, f: F) {
        f(&mut self.brain.lock().unwrap());
    }

    /// The prompts the thread was sent whose first line contains `key`.
    fn seen(&self, key: &str) -> Vec<String> {
        self.brain
            .lock()
            .unwrap()
            .seen
            .iter()
            .filter(|p| p.lines().next().unwrap_or_default().contains(key))
            .cloned()
            .collect()
    }
}

fn user() -> Caller {
    Caller::user("test")
}

/// A fake reviewer CLI, as `loops.rs` has it: run `n` writes its argv, environment, stdin and
/// working directory; `hold.<n>` holds it; it answers `answers/<n>.json`, else no findings.
const FAKE_REVIEWER: &str = r#"#!/bin/sh
here=$(dirname "$0")
n=$(( $(cat "$here/count" 2>/dev/null || echo 0) + 1 ))
echo "$n" > "$here/count"
: > "$here/argv.$n"
for a in "$@"; do printf '%s\n' "$a" >> "$here/argv.$n"; done
pwd > "$here/cwd.$n"
cat > "$here/stdin.$n" 2>/dev/null
if [ -f "$here/hold.$n" ]; then
  sleep 600 &
  echo $! > "$here/child.$n"
  echo $$ > "$here/pid.$n"
  while [ -f "$here/hold.$n" ]; do sleep 0.05; done
  kill $! 2>/dev/null
fi
ans="$here/answers/$n.json"
[ -f "$ans" ] || { ans="$here/answers/none.json"; echo '{"findings": []}' > "$ans"; }
out=""; prev=""
for a in "$@"; do [ "$prev" = "-o" ] && out="$a"; prev="$a"; done
cp "$ans" "$out"
echo '{"type":"turn.completed","usage":{"input_tokens":100,"output_tokens":10}}'
"#;

/// What the scripted thread does ([`Scripted`]): the first of `replies` whose key the prompt's
/// first line contains (taken once; `@commit` commits and answers the block); else, with
/// `auto`, a valid answer to any loop step, editing and committing in `worktree` as a thread
/// would; else the prompt itself (echoed). Every prompt it saw is kept in `seen`.
#[derive(Default)]
struct Brain {
    replies: Vec<(String, String)>,
    auto: bool,
    /// The plan says it touches a contract.
    risk: bool,
    /// The thread's worktree.
    worktree: Option<PathBuf>,
    /// Paths a commit leaves out (the baseline's).
    keep: Vec<String>,
    /// So many commits leave a stray file behind.
    leave_dirty: usize,
    seen: Vec<String>,
}

impl Brain {
    fn reply(&mut self, prompt: &str) -> Option<String> {
        self.seen.push(prompt.to_string());
        let first = prompt.lines().next().unwrap_or_default().to_string();
        if let Some(i) = self
            .replies
            .iter()
            .position(|(k, _)| first.contains(k.as_str()))
        {
            let r = self.replies.remove(i).1;
            if r == "@commit" {
                return Some(format!("Committed.\n{}", self.commit()));
            }
            // A commit and no `twr-commit` block.
            if r == "@commit-no-block" {
                self.commit();
                return Some("Built the CSV export and committed it.".into());
            }
            // The work stashed away (nothing committed), with a block that names HEAD.
            if r == "@stash" {
                return Some(format!("Committed.\n{}", self.stash(false)));
            }
            // The work stashed away, and an empty commit made.
            if r == "@stash-empty-commit" {
                return Some(format!("Committed.\n{}", self.stash(true)));
            }
            // The baseline's changes undone, then the rest committed.
            if r == "@undo-baseline" {
                if let Some(wt) = &self.worktree {
                    for k in std::mem::take(&mut self.keep) {
                        tg::run(wt, &["checkout", "--", &k]);
                    }
                }
                return Some(format!("Committed.\n{}", self.commit()));
            }
            if let Some(rest) = r.strip_prefix("@slow ") {
                // The automatic answer to that prompt, a while later.
                let auto = self.auto_reply(prompt);
                return Some(format!("!sleep {rest}\n{}", auto.unwrap_or_default()));
            }
            return (!r.is_empty()).then_some(r);
        }
        self.auto_reply(prompt)
    }

    /// The automatic answer to loop prompt `prompt` (with `auto`), as the thread would give it.
    fn auto_reply(&mut self, prompt: &str) -> Option<String> {
        let first = prompt.lines().next().unwrap_or_default().to_string();
        if !first.starts_with("[twr loop ") || !self.auto {
            return None;
        }
        let per_round = prompt.contains("Commits: when the gates are green, commit");
        if first.ends_with(" plan]") {
            return Some(plan_reply(self.risk));
        }
        if first.contains(" triage p1") {
            let items: Vec<Value> = finding_ids(prompt)
                .iter()
                .map(|id| json!({"id": id, "verdict": "dismissed", "reason": "not an issue"}))
                .collect();
            return Some(format!(
                "Triaged.\n```twr-triage\n{}\n```",
                json!({ "items": items })
            ));
        }
        if first.ends_with(" implement]") {
            self.write("feature.rs", "pub fn export_csv() {}\n");
            let mut out = String::from("Implemented the plan.\n");
            if per_round {
                self.commit();
            }
            out.push_str(&implement_reply("done", "pass"));
            return Some(out);
        }
        if first.contains(" triage ") && !first.contains("correction") {
            let ids = finding_ids(prompt);
            let round = first
                .split(" triage ")
                .nth(1)
                .unwrap_or("x")
                .trim_end_matches(']')
                .to_string();
            self.write(&format!("fix-{round}.rs"), "// fixed\n");
            if per_round {
                self.commit();
            }
            let items: Vec<Value> = ids
                .iter()
                .map(|id| json!({"id": id, "verdict": "fixed", "class": "new-class", "rule": "r1"}))
                .collect();
            return Some(format!(
                "Triaged.\n```twr-triage\n{}\n```",
                json!({"items": items, "gates": {"ran": ["cargo test"], "result": "pass"}})
            ));
        }
        if first.ends_with(" scope]") {
            return Some(
                "Gates are green.\n```twr-goal\nHarden the change against bad input.\n```".into(),
            );
        }
        if first.ends_with(" sweep]") {
            return Some(format!(
                "Swept.\n```twr-sweep\n{}\n```",
                json!({"rules": [{"rule": "r1", "result": "fixed"}],
                       "gates": {"ran": ["cargo test"], "result": "pass"}})
            ));
        }
        if first.ends_with(" report]") {
            let mut out = String::from("Built the CSV export; nothing is left open.\n");
            if prompt.contains("```twr-commit") {
                out.push_str(&self.commit());
            }
            return Some(out);
        }
        if first.ends_with(" commit correction]") {
            return Some(format!("Committed the rest.\n{}", self.commit()));
        }
        None
    }

    /// Stashes the worktree's changes (untracked files too), then, with `empty`, makes an
    /// empty commit: the `twr-commit` block of `HEAD`.
    fn stash(&mut self, empty: bool) -> String {
        let Some(wt) = self.worktree.clone() else {
            return String::new();
        };
        tg::run(&wt, &["stash", "push", "-u", "-q"]);
        if empty {
            tg::run(
                &wt,
                &["commit", "-q", "--allow-empty", "-m", "Add the CSV export"],
            );
        }
        let sha = tg::run(&wt, &["rev-parse", "HEAD"]);
        format!(
            "```twr-commit\n{}\n```",
            json!({"commits": [{"repo": wt.display().to_string(), "sha": sha}]})
        )
    }

    /// Writes `name` in the worktree.
    fn write(&self, name: &str, text: &str) {
        if let Some(wt) = &self.worktree {
            std::fs::write(wt.join(name), text).unwrap();
        }
    }

    /// Commits everything in the worktree but the paths it keeps out (leaving a stray file
    /// behind while `leave_dirty` lasts); the `twr-commit` block of what it did.
    fn commit(&mut self) -> String {
        let Some(wt) = self.worktree.clone() else {
            return String::new();
        };
        tg::run(&wt, &["add", "-A"]);
        for k in &self.keep {
            let _ = std::process::Command::new("git")
                .arg("-C")
                .arg(&wt)
                .args(["reset", "-q", "--", k])
                .output();
        }
        let staged = std::process::Command::new("git")
            .arg("-C")
            .arg(&wt)
            .args(["diff", "--cached", "--quiet"])
            .status()
            .unwrap();
        if !staged.success() {
            tg::run(&wt, &["commit", "-q", "-m", "Add the CSV export"]);
        }
        if self.leave_dirty > 0 {
            self.leave_dirty -= 1;
            std::fs::write(wt.join("stray.rs"), format!("// {}\n", self.leave_dirty)).unwrap();
        }
        let sha = tg::run(&wt, &["rev-parse", "HEAD"]);
        format!(
            "```twr-commit\n{}\n```",
            json!({"commits": [{"repo": wt.display().to_string(), "sha": sha}]})
        )
    }
}

fn plan_reply(risk: bool) -> String {
    format!(
        "## Goal\nExport reports as CSV.\n\n## Steps\n1. Add `export_csv` in feature.rs.\n\n\
         ```twr-plan\n{}\n```",
        json!({"contract_risk": risk, "reason": if risk { "a new public endpoint" } else { "UI only" }})
    )
}

fn implement_reply(status: &str, gates: &str) -> String {
    format!(
        "```twr-implement\n{}\n```",
        json!({"status": status, "gates": {"ran": ["cargo test"], "result": gates}})
    )
}

/// The finding ids a triage prompt wraps (`finding p1.2 (major):` lines, the hub's).
fn finding_ids(prompt: &str) -> Vec<String> {
    prompt
        .lines()
        .filter_map(|l| l.strip_prefix("finding "))
        .filter_map(|l| l.split_once(' ').map(|(id, _)| id.to_string()))
        .collect()
}

/// `n` findings of severity major.
fn findings(n: usize) -> Value {
    Value::Array(
        (1..=n)
            .map(|i| {
                json!({"severity": "major", "location": format!("feature.rs:{i}"),
                       "problem": format!("problem {i}"), "fix": format!("fix {i}")})
            })
            .collect(),
    )
}

/// The fake runtime, its prompts rewritten by a [`Brain`] first.
struct Scripted {
    brain: Arc<Mutex<Brain>>,
}

impl Adapter for Scripted {
    fn info(&self) -> twr_core::adapter::AdapterInfo {
        FakeAdapter::new().info()
    }

    fn spawn(
        &self,
        cfg: twr_core::adapter::SpawnConfig,
        prompt: Option<String>,
    ) -> anyhow::Result<twr_core::adapter::AdapterHandle> {
        use twr_core::adapter::{AdapterCmd, AdapterHandle};
        let brain = self.brain.clone();
        let rewrite = move |text: String| -> String {
            let reply = brain.lock().unwrap().reply(&text);
            reply.unwrap_or(text)
        };
        let mut inner = FakeAdapter::new().spawn(cfg, prompt.map(&rewrite))?;
        let (outer, end) = AdapterHandle::pair();
        let to_inner = inner.commander();
        let mut from_inner = inner.take_events().expect("events");
        let twr_core::adapter::AdapterEnd {
            mut cmds, events, ..
        } = end;
        tokio::spawn(async move {
            while let Some(cmd) = cmds.recv().await {
                let cmd = match cmd {
                    AdapterCmd::Send { text, turn } => AdapterCmd::Send {
                        text: rewrite(text),
                        turn,
                    },
                    c => c,
                };
                if to_inner.send(cmd).is_err() {
                    break;
                }
            }
        });
        tokio::spawn(async move {
            let _inner = inner;
            while let Some(ev) = from_inner.recv().await {
                if events.send(ev).is_err() {
                    break;
                }
            }
        });
        Ok(outer)
    }
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

async fn thread_on(hub: &Hub, project: &Path, name: &str, mode: Option<&str>) -> String {
    let t = hub
        .new_thread(
            NewThread {
                project: project.to_string_lossy().into_owned(),
                runtime: Some("fake".into()),
                name: Some(name.into()),
                mode: mode.map(str::to_string),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap();
    t["id"].as_str().unwrap().to_string()
}

fn worktree(hub: &Hub, t: &str) -> PathBuf {
    PathBuf::from(hub.thread_view(t).unwrap().worktree.unwrap())
}

/// `loop.request` for an implementation loop with `extra` params.
async fn request(
    hub: &Hub,
    thread: &str,
    extra: Value,
) -> Result<Value, twr_core::proto::RpcError> {
    let mut p = json!({"thread": thread, "kind": "implement", "task": TASK});
    for (k, v) in extra.as_object().unwrap() {
        p[k] = v.clone();
    }
    let p: LoopRequest = serde_json::from_value(p).unwrap();
    hub.loop_request(p, &user()).await
}

fn respond(hub: &Hub, card: &Value, decision: &str, caller: &Caller) -> Result<Event, String> {
    hub.respond_at(
        card["attn_id"].as_str().unwrap(),
        Answer::new(decision),
        card["revision"].as_u64(),
        caller,
    )
    .map_err(|e| format!("{}: {}", e.code, e.message))
}

fn events(hub: &Hub, thread: &str) -> Vec<Event> {
    hub.thread_events(thread, 0, 100_000).unwrap().1
}

fn of_kind(hub: &Hub, thread: &str, kind: &str) -> Vec<Event> {
    events(hub, thread)
        .into_iter()
        .filter(|e| e.kind() == kind)
        .collect()
}

fn item(hub: &Hub, attn_id: &Value) -> Option<Value> {
    hub.attention_list(true)
        .into_iter()
        .find(|i| i["attn_id"] == *attn_id)
}

async fn until(hub: &Hub, what: &str, f: impl Fn(&Hub) -> bool) {
    if timeout(WAIT, async {
        while !f(hub) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_err()
    {
        panic!("waited for {what} in vain");
    }
}

/// Waits until the thread's loop ended; its last loop.
async fn ended(hub: &Hub, t: &str) -> twr_core::reduce::LastLoop {
    until(hub, "the loop's end", |h| {
        h.thread_view(t)
            .is_ok_and(|v| v.r#loop.is_none() && v.last_loop.is_some())
    })
    .await;
    hub.thread_view(t).unwrap().last_loop.unwrap()
}

fn get(hub: &Hub, t: &str) -> Value {
    hub.loop_get(LoopGet {
        thread: Some(t.into()),
        id: None,
    })
    .unwrap()
}

fn control(
    hub: &Hub,
    thread: &str,
    action: ControlAction,
) -> Result<Value, twr_core::proto::RpcError> {
    hub.loop_control(
        LoopControlParams {
            thread: thread.into(),
            action,
        },
        &user(),
    )
}

async fn save(hub: &Hub, settings: LoopSettings) {
    hub.loop_settings_set(
        LoopSettingsSet {
            project: None,
            settings,
        },
        &user(),
    )
    .await
    .unwrap();
}

/// The first card's settings on `profile`, committing `commit`, the plan check `plan_check`.
fn settings(profile: &str, commit: ImplCommit, plan_check: Option<PlanCheck>) -> LoopSettings {
    let mut s = LoopSettings::first_card(&Default::default());
    s.profile = profile.into();
    s.reviewer = ReviewerChoice::parse("codex/gpt-6.1-sol").unwrap();
    s.commit = commit;
    s.plan_check = plan_check;
    s
}

/// A thread `name` in a fresh repository, the brain working in its worktree: its id.
async fn worker(env: &Env, hub: &Hub, name: &str) -> String {
    let repo = env.repo(name);
    let t = thread_on(hub, &repo, name, None).await;
    let wt = worktree(hub, &t);
    env.set(|b| b.worktree = Some(wt));
    t
}

/// A thread `name` whose implementation loop starts on `s` (saved first): its id.
async fn implementing(env: &Env, hub: &Hub, name: &str, s: LoopSettings) -> String {
    let t = worker(env, hub, name).await;
    save(hub, s).await;
    let r = request(hub, &t, json!({})).await.unwrap();
    assert_eq!(r["status"], "started", "{r}");
    assert_eq!(r["loop"]["kind"], "implement", "{r}");
    t
}

/// The loop's last `loop.ended`.
fn ended_event(hub: &Hub, t: &str) -> LoopEnded {
    of_kind(hub, t, "loop.ended")
        .into_iter()
        .rev()
        .find_map(|e| match e.body {
            EventBody::LoopEnded(p) => Some(p),
            _ => None,
        })
        .unwrap()
}

fn commit_checks(hub: &Hub, t: &str) -> Vec<LoopCommitChecked> {
    of_kind(hub, t, "loop.commit")
        .into_iter()
        .filter_map(|e| match e.body {
            EventBody::LoopCommit(p) => Some(p),
            _ => None,
        })
        .collect()
}

fn report_of(hub: &Hub, t: &str) -> String {
    let g = get(hub, t);
    std::fs::read_to_string(g["files"]["report"].as_str().expect("report.md")).unwrap()
}

/// `git status --porcelain` of the worktree.
fn status(wt: &Path) -> String {
    tg::run(wt, &["status", "--porcelain"])
}

/// The thread's last `report.ready` marked as its loop's.
fn last_loop_report(hub: &Hub, t: &str) -> ReportReady {
    of_kind(hub, t, "report.ready")
        .into_iter()
        .rev()
        .find_map(|e| match e.body {
            EventBody::ReportReady(p) => Some(p),
            _ => None,
        })
        .unwrap()
}

// ---------------------------------------------------------------------------------------------
// Whole loops
// ---------------------------------------------------------------------------------------------

/// An implementation loop runs to its end on each built-in profile: the plan (written to
/// `plan.md`), no plan check (no contract risk under `contracts`), the implement turn, the
/// review rounds, the report turn with its commit, the hub's check of it, `report.md`, and the
/// loop's last report naming the commit. The worktree is clean after it.
#[tokio::test]
async fn an_implementation_loop_runs_on_each_profile() {
    for (profile, rounds) in [
        ("low", vec!["m1"]),
        ("medium", vec!["m1", "v1"]),
        ("high", vec!["m1", "x1"]),
    ] {
        let env = Env::new();
        let hub = env.hub().await;
        let t = implementing(&env, &hub, "api", settings(profile, ImplCommit::End, None)).await;
        let wt = worktree(&hub, &t);
        let head = tg::run(&wt, &["rev-parse", "HEAD"]);
        let last = ended(&hub, &t).await;
        assert_eq!(last.outcome, Outcome::Clean, "{profile}");
        assert_eq!(last.kind, twr_core::loops::LoopKind::Implement);
        // The loop's phases, in order.
        let phases: Vec<String> = of_kind(&hub, &t, "loop.phase")
            .iter()
            .filter_map(|e| match &e.body {
                EventBody::LoopPhase(p) if p.problems.is_empty() => Some(p.phase.to_string()),
                _ => None,
            })
            .collect();
        assert_eq!(phases[..3], ["plan", "implement", "ladder"], "{profile}");
        assert_eq!(phases.last().map(String::as_str), Some("report"));
        let g = get(&hub, &t);
        let ids: Vec<&str> = g["rounds"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, rounds, "{profile}");
        assert_eq!(g["started"]["task"], TASK);
        assert_eq!(g["plans"].as_array().unwrap().len(), 1);
        assert_eq!(g["plans"][0]["contract_risk"], false);
        assert_eq!(g["implemented"][0]["status"], "done");
        assert_eq!(g["implemented"][0]["accepted"], true);
        assert_eq!(g["commit_checks"].as_array().unwrap().len(), 1);
        assert_eq!(g["commit_checks"][0]["passed"], true);
        let plan = std::fs::read_to_string(g["files"]["plan"].as_str().unwrap()).unwrap();
        assert!(
            plan.starts_with("## Goal\nExport reports as CSV."),
            "{plan}"
        );
        assert!(!plan.contains("twr-plan"), "{plan}");
        assert_eq!(g["plan_path"], g["files"]["plan"]);
        assert!(g["files"]["task"].is_string());
        // The commit: one, of the loop's work, and nothing left.
        let now = tg::run(&wt, &["rev-parse", "HEAD"]);
        assert_ne!(now, head);
        assert_eq!(status(&wt), "");
        assert_eq!(last.commits.len(), 1);
        assert_eq!(last.commits[0].sha, now);
        assert_eq!(last.commit_problem, None);
        let e = ended_event(&hub, &t);
        assert_eq!(e.commits, last.commits);
        // The plan prompt and the implement prompt.
        let plan_prompt = env.seen(" plan]").pop().unwrap();
        assert!(plan_prompt.contains(TASK), "{plan_prompt}");
        assert!(plan_prompt.contains("state an interpretation instead of"));
        assert!(plan_prompt.contains("```twr-plan"));
        let implement = env.seen(" implement]").pop().unwrap();
        assert!(implement.contains("plan.md"), "{implement}");
        assert!(implement.contains("Gates must be green before review"));
        assert!(implement.contains("Commits: do not commit yet."));
        assert!(implement.contains("runs no subagents of its own"));
        // The reviewer reads the impl-review template, the plan by path.
        let review = env.run_prompt(1);
        assert!(
            review.contains("reviewing a code DIFF that implements a plan"),
            "{review}"
        );
        assert!(
            review.contains(&format!("GOAL / TASK:\n{TASK}")),
            "{review}"
        );
        assert!(review.contains("plan.md"));
        assert!(!review.contains("OUT OF SCOPE"));
        // The report turn asked for the commit; report.md tells it all.
        let report_prompt = env.seen(" report]").pop().unwrap();
        assert!(report_prompt.contains("The implementation loop is over"));
        assert!(report_prompt.contains("one commit per repository"));
        let report = report_of(&hub, &t);
        assert!(
            report.starts_with("# Implementation loop report: api"),
            "{report}"
        );
        let check = if profile == "low" {
            "never"
        } else {
            "contracts"
        };
        assert!(
            report.contains(&format!(
                "## Plan check\n\nNot run: the plan check is `{check}`, and the plan said \
                 contract risk: no (UI only)."
            )),
            "{report}"
        );
        assert!(report.contains("The implement turn reported done with gates `cargo test` pass."));
        assert!(report.contains(&format!("## Commits\n\n- {}: {now}", wt.display())));
        assert!(report.contains("## What was built (the thread's report)\n\nBuilt the CSV export"));
        assert!(!report.contains("twr-commit"), "{report}");
        // The loop's last report names the commit and the report; the report turn's own was
        // marked as checking.
        let r = last_loop_report(&hub, &t);
        assert!(
            r.summary.contains("implementation loop clean"),
            "{}",
            r.summary
        );
        assert!(
            r.summary.contains(&format!("committed {}", &now[..12])),
            "{}",
            r.summary
        );
        let mark = &r.ext.as_ref().unwrap()["twr"]["loop"];
        assert_eq!(mark["step"], "report");
        assert_eq!(mark["commits"][0]["sha"], now);
        assert!(of_kind(&hub, &t, "report.ready").iter().any(|e| matches!(&e.body,
            EventBody::ReportReady(p) if p.ext.as_ref().is_some_and(|x| x["twr"]["loop"]["commit"] == "checking"))));
        hub.shutdown().await;
    }
}

/// The plan check runs by its setting: `contracts` with contract risk and `always` run it on
/// the plan-review template (round `p1`, at `medium`); `never`, or `contracts` without risk,
/// does not. Findings get the plan check's triage; a fixed one revises the plan (`plan.md`
/// replaced, the first kept as `plan-1.md`), a dismissed one goes into the ledger.
#[tokio::test]
async fn the_plan_check_runs_by_its_setting() {
    for (check, risk, runs) in [
        (Some(PlanCheck::Never), true, false),
        (None, true, true),
        (Some(PlanCheck::Contracts), false, false),
        (Some(PlanCheck::Always), false, true),
    ] {
        let env = Env::new();
        env.set(|b| b.risk = risk);
        let hub = env.hub().await;
        // (`medium`: its plan check is `contracts`.)
        let t = implementing(
            &env,
            &hub,
            "api",
            settings("medium", ImplCommit::None, check),
        )
        .await;
        assert_eq!(ended(&hub, &t).await.outcome, Outcome::Clean);
        let g = get(&hub, &t);
        let first = g["rounds"][0]["id"].as_str().unwrap().to_string();
        assert_eq!(first == "p1", runs, "{check:?} {risk}");
        // `medium`: m1 and v1; the plan check's run before them.
        assert_eq!(env.runs(), 2 + usize::from(runs));
        if runs {
            assert_eq!(g["rounds"][0]["effort"], "medium");
            let p = env.run_prompt(1);
            assert!(p.contains("reviewing an implementation PLAN"), "{p}");
            assert!(p.contains("plan.md"));
            assert!(report_of(&hub, &t).contains("Ran (one review at medium, round p1)"));
        } else {
            assert!(env.run_prompt(1).contains("implements a plan"));
        }
    }

    // A plan check with findings: one fixed (the plan revised), one dismissed (ledgered).
    let env = Env::new();
    env.set(|b| {
        b.risk = true;
        b.replies.push((
            " triage p1".into(),
            format!(
                "## Goal\nExport reports as CSV, streamed.\n\n## Steps\n1. Stream rows.\n\n\
                 ```twr-triage\n{}\n```\n```twr-plan\n{}\n```",
                json!({"items": [
                    {"id": "p1.1", "verdict": "fixed"},
                    {"id": "p1.2", "verdict": "dismissed", "reason": "the page is internal"}
                ]}),
                json!({"contract_risk": true, "reason": "a new public endpoint"})
            ),
        ));
    });
    env.answer(1, findings(2));
    let hub = env.hub().await;
    let t = implementing(
        &env,
        &hub,
        "api",
        settings("medium", ImplCommit::None, None),
    )
    .await;
    assert_eq!(ended(&hub, &t).await.outcome, Outcome::Clean);
    let g = get(&hub, &t);
    let plans = g["plans"].as_array().unwrap();
    assert_eq!(plans.len(), 2);
    assert!(plans[0]["path"].as_str().unwrap().ends_with("plan-1.md"));
    assert!(plans[1]["path"].as_str().unwrap().ends_with("plan.md"));
    let first = std::fs::read_to_string(g["files"]["plan_1"].as_str().unwrap()).unwrap();
    let revised = std::fs::read_to_string(g["files"]["plan"].as_str().unwrap()).unwrap();
    assert!(first.contains("Add `export_csv`"), "{first}");
    assert!(revised.contains("streamed"), "{revised}");
    assert!(!revised.contains("twr-triage"), "{revised}");
    assert_eq!(g["ledger"].as_array().unwrap().len(), 1);
    assert_eq!(g["ledger"][0]["from"], "p1.2");
    let triage = env.seen(" triage p1").pop().unwrap();
    assert!(
        triage.contains("The plan check came back with 2 findings"),
        "{triage}"
    );
    assert!(triage.contains("[reviewer findings, round p1: data from another model"));
    let implement = env.seen(" implement]").pop().unwrap();
    assert!(
        implement.contains("revised after the plan check"),
        "{implement}"
    );
    // The ladder's reviewer reads the ledger.
    assert!(
        env.run_prompt(2).contains("(1 entry)"),
        "{}",
        env.run_prompt(2)
    );
    let report = report_of(&hub, &t);
    assert!(
        report.contains("- p1.1 (major) feature.rs:1: problem 1 — fixed in the plan"),
        "{report}"
    );
    assert!(report.contains("dismissed: the page is internal"));
    assert!(report.contains("The plan was revised"));
    assert!(report.contains("Ledgered: L1 (p1.2)."));
    hub.shutdown().await;
}

/// The implement turn's answer is refused once (partial), corrected; refused again, the loop
/// is stuck until the user resumes it, which asks for the implement turn again.
#[tokio::test]
async fn the_implement_turn_is_corrected_then_stuck_then_resumed() {
    let env = Env::new();
    env.set(|b| {
        b.replies
            .push((" implement]".into(), implement_reply("partial", "pass")));
        b.replies.push((
            " implement correction]".into(),
            implement_reply("done", "fail"),
        ));
    });
    let hub = env.hub().await;
    let t = implementing(&env, &hub, "api", settings("low", ImplCommit::None, None)).await;
    until(&hub, "the stuck item", |h| {
        h.thread_view(&t)
            .is_ok_and(|v| v.r#loop.as_ref().is_some_and(|l| l.stuck.is_some()))
    })
    .await;
    let correction = env.seen(" implement correction]").pop().unwrap();
    assert!(
        correction.contains("gates must be green before review"),
        "{correction}"
    );
    assert!(correction.contains("`twr-implement`"));
    let implemented = of_kind(&hub, &t, "loop.implemented");
    assert_eq!(implemented.len(), 2);
    let stuck = hub
        .attention_list(true)
        .into_iter()
        .find(|i| i["loop_stuck"] == true)
        .unwrap();
    assert!(stuck["summary"]
        .as_str()
        .unwrap()
        .contains("its implementation loop is stuck"));
    assert_eq!(stuck["options"], json!(["resume", "stop"]));
    // Skip to verify is refused before the ladder.
    let e = control(&hub, &t, ControlAction::SkipToVerify).unwrap_err();
    assert_eq!(e.code, codes::CONFLICT);
    assert!(
        e.message.contains("has not implemented its plan yet"),
        "{e:?}"
    );
    hub.respond_at(
        stuck["attn_id"].as_str().unwrap(),
        Answer::new("resume"),
        stuck["revision"].as_u64(),
        &user(),
    )
    .unwrap();
    assert_eq!(ended(&hub, &t).await.outcome, Outcome::Clean);
    assert_eq!(env.seen(" implement]").len(), 2);
    hub.shutdown().await;
}

// ---------------------------------------------------------------------------------------------
// The commit
// ---------------------------------------------------------------------------------------------

/// The commit modes against real git: a worktree left dirty is corrected once; dirty twice,
/// the loop still ends, `not committed`; `per-round` commits as it goes; `none` commits and
/// checks nothing.
#[tokio::test]
async fn the_commit_is_checked() {
    // Dirty once: the correction asks only for the commit, and passes.
    let env = Env::new();
    env.set(|b| b.leave_dirty = 1);
    let hub = env.hub().await;
    let t = implementing(&env, &hub, "api", settings("low", ImplCommit::End, None)).await;
    let last = ended(&hub, &t).await;
    assert_eq!(last.outcome, Outcome::Clean);
    let checks = commit_checks(&hub, &t);
    assert_eq!(checks.len(), 2);
    assert!(
        checks[0].problems[0].contains("stray.rs"),
        "{:?}",
        checks[0]
    );
    assert!(checks[1].problems.is_empty());
    let correction = env.seen(" commit correction]").pop().unwrap();
    assert!(correction.contains("stray.rs"), "{correction}");
    assert!(correction.contains("Only the commit is asked again"));
    assert!(correction.contains("```twr-commit"));
    assert_eq!(status(&worktree(&hub, &t)), "");
    assert_eq!(last.commit_problem, None);
    // The report keeps the report turn's text, not the correction's.
    let report = report_of(&hub, &t);
    assert!(report.contains("Built the CSV export"), "{report}");
    assert!(!report.contains("Committed the rest"), "{report}");
    hub.shutdown().await;

    // Dirty twice: not committed, and the loop ends all the same.
    let env = Env::new();
    env.set(|b| b.leave_dirty = 2);
    let hub = env.hub().await;
    let t = implementing(&env, &hub, "api", settings("low", ImplCommit::End, None)).await;
    let last = ended(&hub, &t).await;
    assert_eq!(last.outcome, Outcome::Clean);
    assert!(last
        .commit_problem
        .as_deref()
        .is_some_and(|p| p.contains("stray.rs")));
    assert_eq!(commit_checks(&hub, &t).len(), 2);
    assert!(report_of(&hub, &t).contains("Not committed: "));
    let r = last_loop_report(&hub, &t);
    assert!(r.summary.contains("not committed: "), "{}", r.summary);
    hub.shutdown().await;

    // Per round: the implement turn commits; the report turn has nothing left.
    let env = Env::new();
    let hub = env.hub().await;
    let t = implementing(
        &env,
        &hub,
        "api",
        settings("low", ImplCommit::PerRound, None),
    )
    .await;
    let last = ended(&hub, &t).await;
    let implement = env.seen(" implement]").pop().unwrap();
    assert!(implement.contains("Commits: when the gates are green, commit"));
    assert_eq!(last.commits.len(), 1);
    assert_eq!(
        last.commits[0].sha,
        tg::run(&worktree(&hub, &t), &["rev-parse", "HEAD"])
    );
    assert!(env
        .seen(" report]")
        .pop()
        .unwrap()
        .contains("what is left of the loop's work"));
    hub.shutdown().await;

    // None: nothing asked, nothing checked, the work left in the worktree.
    let env = Env::new();
    let hub = env.hub().await;
    let t = implementing(&env, &hub, "api", settings("low", ImplCommit::None, None)).await;
    let last = ended(&hub, &t).await;
    assert!(last.commits.is_empty());
    assert!(commit_checks(&hub, &t).is_empty());
    assert!(!env.seen(" report]").pop().unwrap().contains("twr-commit"));
    assert!(env
        .seen(" implement]")
        .pop()
        .unwrap()
        .contains("Commits: do not commit; leave your changes uncommitted."));
    assert!(status(&worktree(&hub, &t)).contains("feature.rs"));
    hub.shutdown().await;
}

/// Changes already in the worktree when the loop starts are its baseline: kept in the loop
/// folder, named out of scope in the reviewer's prompt, left out of the diff it reads while the
/// loop does not touch them, out of the commit and of the report's work.
#[tokio::test]
async fn the_baseline_stays_out() {
    let env = Env::new();
    let hub = env.hub().await;
    let t = worker(&env, &hub, "api").await;
    let wt = worktree(&hub, &t);
    std::fs::write(wt.join("README.md"), "test repo\nbaseline edit\n").unwrap();
    std::fs::write(wt.join("scratch.txt"), "baseline scratch\n").unwrap();
    env.set(|b| b.keep = vec!["README.md".into(), "scratch.txt".into()]);
    save(&hub, settings("low", ImplCommit::End, None)).await;
    request(&hub, &t, json!({})).await.unwrap();
    let last = ended(&hub, &t).await;
    assert_eq!(last.outcome, Outcome::Clean);
    assert_eq!(last.commit_problem, None);
    // The baseline is still there, uncommitted; the loop's work is committed.
    let st = status(&wt);
    assert!(st.contains("M README.md"), "{st}");
    assert!(st.contains("?? scratch.txt"), "{st}");
    assert!(!st.contains("feature.rs"), "{st}");
    let shown = tg::run(&wt, &["show", "--stat", "HEAD"]);
    assert!(
        shown.contains("feature.rs") && !shown.contains("README"),
        "{shown}"
    );
    let g = get(&hub, &t);
    assert_eq!(g["started"]["baseline"]["api"]["changed"], 2);
    assert!(!g["started"]["baseline_digests"]["api"].is_null());
    let patch = std::fs::read_to_string(g["files"]["baselines"][0].as_str().unwrap()).unwrap();
    assert!(patch.contains("baseline edit") && patch.contains("baseline scratch"));
    // The reviewer: told, and the diff without the untouched baseline.
    let prompt = env.run_prompt(1);
    assert!(
        prompt.contains("OUT OF SCOPE — already modified before this task started"),
        "{prompt}"
    );
    assert!(prompt.contains("2 paths already changed: README.md, scratch.txt"));
    let diff =
        std::fs::read_to_string(g["rounds"][0]["files"]["diffs"][0].as_str().unwrap()).unwrap();
    assert!(diff.contains("feature.rs"), "{diff}");
    assert!(
        !diff.contains("baseline edit") && !diff.contains("baseline scratch"),
        "{diff}"
    );
    assert!(diff.contains("# LEFT OUT: 2 paths"), "{diff}");
    let report = report_of(&hub, &t);
    assert!(report.contains("Out of scope in api: what the worktree held before the loop began (2 paths already changed"), "{report}");
    assert!(env
        .seen(" report]")
        .pop()
        .unwrap()
        .contains("Leave the changes that were already in the worktree"));
    hub.shutdown().await;
}

/// A loop stopped commits nothing; one whose verification cap expired commits, and its report
/// says which findings the commit still carries.
#[tokio::test]
async fn stopped_commits_nothing_and_cap_expired_commits() {
    let env = Env::new();
    env.set(|b| b.replies.push((" plan]".into(), "!sleep 600000".into())));
    let hub = env.hub().await;
    let t = implementing(&env, &hub, "api", settings("low", ImplCommit::End, None)).await;
    until(&hub, "the plan turn", |h| {
        h.thread_view(&t).is_ok_and(|v| v.current_turn.is_some())
    })
    .await;
    let wt = worktree(&hub, &t);
    let head = tg::run(&wt, &["rev-parse", "HEAD"]);
    control(&hub, &t, ControlAction::Stop).unwrap();
    let last = ended(&hub, &t).await;
    assert_eq!(last.outcome, Outcome::Stopped);
    assert!(last.commits.is_empty());
    assert!(commit_checks(&hub, &t).is_empty());
    assert_eq!(tg::run(&wt, &["rev-parse", "HEAD"]), head);
    hub.shutdown().await;

    let env = Env::new();
    env.answer(1, findings(1));
    env.answer(2, findings(1));
    let hub = env.hub().await;
    let t = implementing(&env, &hub, "api", settings("low", ImplCommit::End, None)).await;
    let last = ended(&hub, &t).await;
    assert_eq!(last.outcome, Outcome::CapExpired);
    assert_eq!(last.commits.len(), 1);
    let report = report_of(&hub, &t);
    assert!(
        report.contains("The commit still carries the last verification round's 1 finding, listed under How it ended: v1.1."),
        "{report}"
    );
    hub.shutdown().await;
}

/// A review loop under `match` commits its fixes when the thread had been committing its work
/// (commits of its own in the scope, a clean worktree), decided at the start and recorded; its
/// prompts say so. Without commits of its own, it commits nothing and its prompts say so.
#[tokio::test]
async fn a_review_loop_under_match_commits_as_the_thread_did() {
    let env = Env::new();
    env.answer(1, findings(1));
    let hub = env.hub().await;
    let t = worker(&env, &hub, "api").await;
    let wt = worktree(&hub, &t);
    std::fs::write(wt.join("lib.rs"), "fn work() {}\n").unwrap();
    tg::commit_all(&wt, "the thread's own work");
    let mut s = settings("low", ImplCommit::End, None);
    s.review_commit = twr_core::loops::config::ReviewCommit::Match;
    save(&hub, s.clone()).await;
    let p: LoopRequest = serde_json::from_value(json!({"thread": t, "kind": "review"})).unwrap();
    assert_eq!(
        hub.loop_request(p, &user()).await.unwrap()["status"],
        "started"
    );
    let last = ended(&hub, &t).await;
    let started = of_kind(&hub, &t, "loop.started");
    let EventBody::LoopStarted(p) = &started[0].body else {
        unreachable!()
    };
    assert!(p.commit_fixes);
    assert!(env
        .seen(" triage m1]")
        .pop()
        .unwrap()
        .contains("Commits: do not commit yet. The loop commits its fixes once"));
    assert!(env
        .seen(" report]")
        .pop()
        .unwrap()
        .contains("this loop's fixes"));
    assert_eq!(last.commits.len(), 1);
    assert_eq!(status(&wt), "");
    assert!(report_of(&hub, &t).contains("## Commits"));
    hub.shutdown().await;

    // No commits of its own: nothing is committed.
    let env = Env::new();
    env.answer(1, findings(1));
    let hub = env.hub().await;
    let t = worker(&env, &hub, "api").await;
    let wt = worktree(&hub, &t);
    std::fs::write(wt.join("lib.rs"), "fn work() {}\n").unwrap();
    save(&hub, s).await;
    let p: LoopRequest = serde_json::from_value(json!({"thread": t, "kind": "review"})).unwrap();
    hub.loop_request(p, &user()).await.unwrap();
    let last = ended(&hub, &t).await;
    let started = of_kind(&hub, &t, "loop.started");
    let EventBody::LoopStarted(p) = &started[0].body else {
        unreachable!()
    };
    assert!(!p.commit_fixes);
    assert!(env
        .seen(" triage m1]")
        .pop()
        .unwrap()
        .contains("Commits: do not commit; leave your fixes uncommitted."));
    assert!(!env.seen(" report]").pop().unwrap().contains("twr-commit"));
    assert!(last.commits.is_empty());
    assert!(commit_checks(&hub, &t).is_empty());
    hub.shutdown().await;
}

// ---------------------------------------------------------------------------------------------
// The card with a task, and new threads
// ---------------------------------------------------------------------------------------------

/// The first implementation loop raises the card holding its task: three options, the task
/// shown (cut, with a count of the rest), the plan check and commit rows; `loop.setup.update`
/// keeps the task. `cancel` sends and saves nothing; `run without a loop` sends the task once,
/// as the user's, and saves nothing; `start` starts the loop with the task.
#[tokio::test]
async fn the_card_holds_the_task() {
    let env = Env::new();
    let hub = env.hub().await;
    let t = worker(&env, &hub, "api").await;
    let long = "line one\nline two\nline three\nline four\nline five";
    let card = request(&hub, &t, json!({"task": long})).await.unwrap();
    assert_eq!(card["status"], "needs_setup");
    let it = item(&hub, &card["attn_id"]).unwrap();
    assert_eq!(it["options"], json!([START, WITHOUT, "cancel"]));
    let summary = it["summary"].as_str().unwrap();
    assert!(
        summary.starts_with("api: implementation loop setup (first run"),
        "{summary}"
    );
    assert!(summary.contains("Task         line one\n             line two\n             line three\n             (+2 more lines)"), "{summary}");
    assert!(summary.contains("Baseline     clean"), "{summary}");
    assert!(
        summary.contains("Plan check   contracts (the profile's)"),
        "{summary}"
    );
    assert!(summary.contains("Commit       end"), "{summary}");
    // The fake runtime runs no subagents: no Workers row.
    assert!(!summary.contains("Workers"), "{summary}");
    let got = hub
        .loop_setup_get(
            serde_json::from_value(json!({"attn_id": card["attn_id"]})).unwrap(),
            &user(),
        )
        .unwrap();
    assert_eq!(got["setup"]["task"], long);
    assert_eq!(got["setup"]["kind"], "implement");
    assert_eq!(got["setup"]["no_workers"], true);
    // A change keeps the task.
    let mut s = settings("low", ImplCommit::End, None);
    s.plan_check = Some(PlanCheck::Never);
    let up = hub
        .loop_setup_update(
            LoopSetupUpdate {
                attn_id: card["attn_id"].as_str().unwrap().into(),
                revision: card["revision"].as_u64().unwrap(),
                settings: Some(s),
                ..Default::default()
            },
            &user(),
        )
        .await
        .unwrap();
    assert_eq!(up["setup"]["task"], long);
    assert_eq!(up["setup"]["no_workers"], true);
    assert_eq!(up["options"], json!([START, WITHOUT, "cancel"]));
    assert!(up["summary"]
        .as_str()
        .unwrap()
        .contains("Plan check   never"));
    // A scope is refused.
    let e = hub
        .loop_setup_update(
            LoopSetupUpdate {
                attn_id: card["attn_id"].as_str().unwrap().into(),
                revision: up["revision"].as_u64().unwrap(),
                scope: Some("branch".into()),
                ..Default::default()
            },
            &user(),
        )
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST);
    // Cancel: nothing sent, nothing saved.
    respond(&hub, &up, "cancel", &user()).unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(of_kind(&hub, &t, "turn.started").is_empty());
    assert!(hub.thread_view(&t).unwrap().r#loop.is_none());

    // Run without a loop: the task, once, as the user's.
    let card = request(&hub, &t, json!({})).await.unwrap();
    respond(&hub, &card, WITHOUT, &user()).unwrap();
    until(&hub, "the task's turn", |h| {
        !of_kind(h, &t, "turn.ended").is_empty()
    })
    .await;
    let turns = of_kind(&hub, &t, "turn.started");
    assert_eq!(turns.len(), 1);
    let EventBody::TurnStarted(p) = &turns[0].body else {
        unreachable!()
    };
    assert_eq!(p.prompt, TASK);
    assert_eq!(p.from, TurnFrom::User);
    assert!(hub.thread_view(&t).unwrap().r#loop.is_none());
    let saved = hub
        .loop_settings_get(serde_json::from_value(json!({})).unwrap())
        .await
        .unwrap();
    assert!(saved["settings"].is_null());

    // Start: the loop, with the task.
    let card = request(&hub, &t, json!({})).await.unwrap();
    respond(&hub, &card, START, &user()).unwrap();
    assert_eq!(ended(&hub, &t).await.outcome, Outcome::Clean);
    assert_eq!(get(&hub, &t)["started"]["task"], TASK);
    // The orchestrator answers none of it.
    let card = request(&hub, &t, json!({"setup": true})).await.unwrap();
    let e = hub
        .respond_relay(
            card["attn_id"].as_str().unwrap(),
            Answer::new(WITHOUT),
            card["revision"].as_u64(),
            &Caller::Orchestrator,
        )
        .unwrap_err();
    assert_eq!(e.code, codes::RELAY_FORBIDDEN);
    hub.shutdown().await;
}

/// `thread.new {loop}`: the thread is made with its worktree, and the task is its first
/// prompt only as the loop's: on saved settings the plan prompt is its first turn; with the
/// card needed, the thread stays idle with the card open, and stays (idle) after a cancel,
/// the task never sent; `run without a loop` sends it once.
#[tokio::test]
async fn a_new_thread_with_a_loop() {
    let env = Env::new();
    let hub = env.hub().await;
    let repo = env.repo("api");
    let new = |name: &str, setup: bool| NewThread {
        project: repo.to_string_lossy().into_owned(),
        name: Some(name.into()),
        loop_req: Some(NewThreadLoop {
            kind: "implement".into(),
            task: TASK.into(),
            setup,
            ..Default::default()
        }),
        ..Default::default()
    };
    // The card: idle, nothing sent; cancel keeps the thread, idle.
    let out = hub.new_thread(new("a", false), "test").await.unwrap();
    assert_eq!(out["loop"]["status"], "needs_setup", "{out}");
    let a = out["id"].as_str().unwrap().to_string();
    let card = &out["loop"];
    assert_eq!(
        item(&hub, &card["attn_id"]).unwrap()["options"],
        json!([START, WITHOUT, "cancel"])
    );
    respond(&hub, card, "cancel", &user()).unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(of_kind(&hub, &a, "turn.started").is_empty());
    assert_eq!(hub.get_thread(&a).unwrap()["status"], "idle");
    // Run without a loop: sent once.
    let out = hub.new_thread(new("b", false), "test").await.unwrap();
    let b = out["id"].as_str().unwrap().to_string();
    respond(&hub, &out["loop"], WITHOUT, &user()).unwrap();
    until(&hub, "b's turn", |h| {
        !of_kind(h, &b, "turn.ended").is_empty()
    })
    .await;
    let turns = of_kind(&hub, &b, "turn.started");
    assert_eq!(turns.len(), 1);
    assert!(matches!(&turns[0].body, EventBody::TurnStarted(p) if p.prompt == TASK));
    // Saved settings: the plan prompt is the first turn.
    save(&hub, settings("low", ImplCommit::None, None)).await;
    let out = hub.new_thread(new("c", false), "test").await.unwrap();
    assert_eq!(out["loop"]["status"], "started", "{out}");
    let c = out["id"].as_str().unwrap().to_string();
    assert_eq!(ended(&hub, &c).await.outcome, Outcome::Clean);
    let turns = of_kind(&hub, &c, "turn.started");
    let EventBody::TurnStarted(p) = &turns[0].body else {
        unreachable!()
    };
    assert!(
        p.prompt.starts_with("[twr loop ") && p.prompt.lines().next().unwrap().ends_with(" plan]"),
        "{}",
        p.prompt
    );
    assert!(turns
        .iter()
        .all(|e| !matches!(&e.body, EventBody::TurnStarted(p) if p.prompt == TASK)));
    // `setup: true`: the card all the same.
    let out = hub.new_thread(new("d", true), "test").await.unwrap();
    assert_eq!(out["loop"]["status"], "needs_setup");
    assert_eq!(out["loop"]["why"], "asked");
    hub.shutdown().await;
}

/// A new thread asked for with a loop and no runtime or model of its own runs on the saved
/// named implementer's; with `the thread's own`, on its project's.
#[tokio::test]
async fn a_new_thread_runs_on_the_named_implementer() {
    let env = Env::new();
    let hub = env.hub().await;
    let repo = env.repo("api");
    let mut s = settings("low", ImplCommit::None, None);
    s.implementer = Some(ModelChoice::parse("claude-code/claude-sonnet-5-5").unwrap());
    save(&hub, s).await;
    let out = hub
        .new_thread(
            NewThread {
                project: repo.to_string_lossy().into_owned(),
                name: Some("named".into()),
                loop_req: Some(NewThreadLoop {
                    kind: "implement".into(),
                    task: TASK.into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap();
    let id = out["id"].as_str().unwrap().to_string();
    let v = hub.thread_view(&id).unwrap();
    assert_eq!(v.runtime, "claude-code");
    assert_eq!(v.model.as_deref(), Some("claude-sonnet-5-5"));
    assert_eq!(out["loop"]["status"], "started", "{out}");
    control(&hub, &id, ControlAction::Stop).unwrap();
    // The thread's own: the project's runtime.
    save(&hub, settings("low", ImplCommit::None, None)).await;
    let out = hub
        .new_thread(
            NewThread {
                project: repo.to_string_lossy().into_owned(),
                name: Some("own".into()),
                loop_req: Some(NewThreadLoop {
                    kind: "implement".into(),
                    task: TASK.into(),
                    ..Default::default()
                }),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap();
    let v = hub.thread_view(out["id"].as_str().unwrap()).unwrap();
    assert_eq!(v.runtime, "fake");
    hub.shutdown().await;
}

/// What is refused: `thread.new {loop}` for a review, without a task, or in a mode that can't
/// edit (no thread is made); `loop.request` for an implementation loop in a `plan` thread,
/// with a scope, or a review loop with a plan check. (The orchestrator's requests:
/// `loops_orchestrator.rs`.)
#[tokio::test]
async fn refusals() {
    let env = Env::new();
    let hub = env.hub().await;
    let repo = env.repo("api");
    let threads = |h: &Hub| h.list_threads().len();
    let before = threads(&hub);
    let new = |kind: &str, task: &str, mode: Option<&str>| NewThread {
        project: repo.to_string_lossy().into_owned(),
        mode: mode.map(str::to_string),
        loop_req: Some(NewThreadLoop {
            kind: kind.into(),
            task: task.into(),
            ..Default::default()
        }),
        ..Default::default()
    };
    let e = hub
        .new_thread(new("review", TASK, None), "test")
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST);
    assert!(e.message.contains("nothing to review yet"), "{e:?}");
    let e = hub
        .new_thread(new("implement", " ", None), "test")
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST);
    let e = hub
        .new_thread(new("implement", TASK, Some("plan")), "test")
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::CONFLICT);
    assert!(e.message.contains("can't edit"), "{e:?}");
    assert_eq!(threads(&hub), before);
    // loop.request.
    let plan = thread_on(&hub, &repo, "planner", Some("plan")).await;
    let e = request(&hub, &plan, json!({})).await.unwrap_err();
    assert_eq!(e.code, codes::CONFLICT);
    assert!(e.message.contains("plan mode, which can't edit"), "{e:?}");
    let t = thread_on(&hub, &repo, "api", None).await;
    let e = request(&hub, &t, json!({"scope": "uncommitted"}))
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST);
    let p: LoopRequest = serde_json::from_value(
        json!({"thread": t, "kind": "review", "overrides": {"plan_check": "always"}}),
    )
    .unwrap();
    let e = hub.loop_request(p, &user()).await.unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST);
    assert!(!twr_hub::server::AGENT_METHODS.contains(&"thread.new"));
    hub.shutdown().await;
}

// ---------------------------------------------------------------------------------------------
// Restarts
// ---------------------------------------------------------------------------------------------

/// The hub restarts in each new phase: a plan turn, the plan-check run in flight, the
/// implement turn, and the commit's correction; each is asked (or run) again and the loop
/// runs to its end.
#[tokio::test]
async fn implementation_loops_go_on_after_a_restart() {
    for (key, risk) in [
        (" plan]", false),
        ("p1-run", true),
        (" implement]", false),
        (" commit correction]", false),
    ] {
        let env = Env::new();
        env.set(|b| {
            b.risk = risk;
            if key == " commit correction]" {
                b.leave_dirty = 1;
            }
            if key != "p1-run" {
                b.replies.push((key.into(), "!sleep 600000".into()));
            }
        });
        if key == "p1-run" {
            env.hold_run(1);
        }
        let hub = env.hub().await;
        let check = Some(PlanCheck::Contracts);
        let t = implementing(&env, &hub, "api", settings("low", ImplCommit::End, check)).await;
        if key == "p1-run" {
            until(&hub, "the plan check's run", |h| {
                h.thread_view(&t)
                    .is_ok_and(|v| v.background_tasks.keys().any(|k| k.starts_with("loop:")))
            })
            .await;
            assert_eq!(
                hub.thread_view(&t)
                    .unwrap()
                    .r#loop
                    .unwrap()
                    .round
                    .as_deref(),
                Some("p1")
            );
        } else {
            until(&hub, key, |_| !env.seen(key).is_empty()).await;
            until(&hub, "the held turn", |h| {
                h.thread_view(&t).is_ok_and(|v| v.current_turn.is_some())
            })
            .await;
        }
        hub.shutdown().await;
        drop(hub);
        env.release_run(1);
        let hub = env.hub().await;
        let last = ended(&hub, &t).await;
        assert_eq!(last.outcome, Outcome::Clean, "{key}");
        assert!(
            last.commits.len() == 1 && last.commit_problem.is_none(),
            "{key}: {last:?}"
        );
        if key == "p1-run" {
            let g = get(&hub, &t);
            assert_eq!(g["rounds"][0]["id"], "p1");
            assert_eq!(g["rounds"][0]["reason"], "the hub stopped while it ran");
        } else {
            assert!(env.seen(key).len() >= 2, "{key}");
        }
        hub.shutdown().await;
    }
}

// ---------------------------------------------------------------------------------------------
// The commit's check follows git; the Workers row; loop.report; pause in each new phase
// ---------------------------------------------------------------------------------------------

/// The commit's check is what git says, the `twr-commit` block only the thread's note: a
/// commit with no block passes (the report notes it); work stashed away, or an empty commit,
/// fails ("neither in the worktree nor committed"); the baseline committed by mistake, or
/// undone, fails naming its paths. A repository with a problem is never also counted
/// committed.
#[tokio::test]
async fn the_commit_check_follows_git() {
    // Committed, no block: passed, noted.
    let env = Env::new();
    env.set(|b| {
        b.replies
            .push((" report]".into(), "@commit-no-block".into()))
    });
    let hub = env.hub().await;
    let t = implementing(&env, &hub, "api", settings("low", ImplCommit::End, None)).await;
    let last = ended(&hub, &t).await;
    assert_eq!(last.commits.len(), 1, "{last:?}");
    assert_eq!(last.commit_problem, None);
    let checks = commit_checks(&hub, &t);
    assert_eq!(checks.len(), 1);
    assert!(checks[0].problems.is_empty(), "{:?}", checks[0]);
    assert!(
        checks[0].notes[0].contains("the thread wrote no twr-commit block"),
        "{:?}",
        checks[0]
    );
    assert!(env.seen(" commit correction]").is_empty());
    let report = report_of(&hub, &t);
    assert!(
        report.contains("Note: the thread wrote no twr-commit block"),
        "{report}"
    );
    assert!(!report.contains("Not committed"), "{report}");
    assert_eq!(
        get(&hub, &t)["commit_checks"][0]["notes"][0],
        json!(checks[0].notes[0])
    );
    hub.shutdown().await;

    // Stashed away, then stashed again in the correction: not committed, nothing counted.
    for (how, why) in [
        ("@stash", "(HEAD did not move)"),
        (
            "@stash-empty-commit",
            "(HEAD moved, but to a commit that changed nothing)",
        ),
    ] {
        let env = Env::new();
        env.set(|b| {
            b.replies.push((" report]".into(), how.into()));
            b.replies.push((" commit correction]".into(), how.into()));
        });
        let hub = env.hub().await;
        let t = implementing(&env, &hub, "api", settings("low", ImplCommit::End, None)).await;
        let last = ended(&hub, &t).await;
        assert_eq!(last.outcome, Outcome::Clean, "{how}");
        let checks = commit_checks(&hub, &t);
        assert_eq!(checks.len(), 2, "{how}");
        assert!(
            checks[0].problems[0]
                .contains("the loop's changes are neither in the worktree nor committed")
                && checks[0].problems[0].contains(why),
            "{how}: {:?}",
            checks[0]
        );
        assert!(checks[0].commits.is_empty(), "{how}");
        assert!(last.commits.is_empty(), "{how}: {last:?}");
        assert!(
            last.commit_problem
                .as_deref()
                .is_some_and(|p| p.contains("neither in the worktree nor committed")),
            "{how}: {last:?}"
        );
        assert!(last_loop_report(&hub, &t)
            .summary
            .contains("not committed: "));
        hub.shutdown().await;
    }

    // The baseline committed with the work, twice: a problem naming it, and no commit counted
    // although HEAD moved.
    let env = Env::new();
    let hub = env.hub().await;
    let t = worker(&env, &hub, "api").await;
    let wt = worktree(&hub, &t);
    std::fs::write(wt.join("README.md"), "test repo\nbaseline edit\n").unwrap();
    save(&hub, settings("low", ImplCommit::End, None)).await;
    request(&hub, &t, json!({})).await.unwrap();
    let last = ended(&hub, &t).await;
    let checks = commit_checks(&hub, &t);
    assert_eq!(checks.len(), 2);
    assert!(
        checks[0].problems[0].contains("the baseline's changes were committed")
            && checks[0].problems[0].contains("README.md"),
        "{:?}",
        checks[0]
    );
    assert!(checks[0].commits.is_empty());
    assert!(last.commits.is_empty(), "{last:?}");
    assert!(last
        .commit_problem
        .as_deref()
        .is_some_and(|p| p.contains("README.md")));
    let e = ended_event(&hub, &t);
    assert!(e.commits.is_empty() && e.commit_problem.is_some(), "{e:?}");
    let correction = env.seen(" commit correction]").pop().unwrap();
    assert!(
        correction.contains("the baseline's changes were committed"),
        "{correction}"
    );
    hub.shutdown().await;

    // The baseline undone before the commit: a problem naming it.
    let env = Env::new();
    let hub = env.hub().await;
    let t = worker(&env, &hub, "api").await;
    let wt = worktree(&hub, &t);
    std::fs::write(wt.join("README.md"), "test repo\nbaseline edit\n").unwrap();
    env.set(|b| {
        b.keep = vec!["README.md".into()];
        b.replies.push((" report]".into(), "@undo-baseline".into()));
    });
    save(&hub, settings("low", ImplCommit::End, None)).await;
    request(&hub, &t, json!({})).await.unwrap();
    ended(&hub, &t).await;
    let checks = commit_checks(&hub, &t);
    assert!(
        checks[0].problems[0].contains("the baseline's changes are gone")
            && checks[0].problems[0].contains("README.md"),
        "{:?}",
        checks[0]
    );
    hub.shutdown().await;
}

/// The setup card's Workers row is there only where the implementer's runtime runs subagents:
/// a Claude Code thread's card has it, a fake one's (no subagents) does not.
#[tokio::test]
async fn the_workers_row_only_where_workers_apply() {
    let env = Env::new();
    let hub = env.hub().await;
    let repo = env.repo("api");
    let t = hub
        .new_thread(
            NewThread {
                project: repo.to_string_lossy().into_owned(),
                runtime: Some("claude-code".into()),
                name: Some("cc".into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let card = request(&hub, &t, json!({})).await.unwrap();
    let it = item(&hub, &card["attn_id"]).unwrap();
    let summary = it["summary"].as_str().unwrap();
    assert!(summary.contains("\nWorkers      "), "{summary}");
    let got = hub
        .loop_setup_get(
            serde_json::from_value(json!({"attn_id": card["attn_id"]})).unwrap(),
            &user(),
        )
        .unwrap();
    assert!(got["setup"].get("no_workers").is_none(), "{got}");
    hub.shutdown().await;
}

/// `loop.report`: a running loop's is not written yet (said, with where it will be); an ended
/// one's is the text of `report.md`, by thread or by id.
#[tokio::test]
async fn loop_report_reads_the_report() {
    let env = Env::new();
    env.set(|b| b.replies.push((" plan]".into(), "@slow 1500".into())));
    let hub = env.hub().await;
    let t = implementing(&env, &hub, "api", settings("low", ImplCommit::End, None)).await;
    let r = hub
        .loop_report(LoopGet {
            thread: Some("api".into()),
            id: None,
        })
        .unwrap();
    assert_eq!(r["running"], true, "{r}");
    assert_eq!(r["written"], false, "{r}");
    assert!(r["text"].is_null(), "{r}");
    assert!(
        r["note"].as_str().unwrap().contains("not written yet"),
        "{r}"
    );
    assert!(r["path"].as_str().unwrap().ends_with("report.md"), "{r}");
    ended(&hub, &t).await;
    let id = get(&hub, &t)["id"].as_str().unwrap().to_string();
    let r = hub
        .loop_report(LoopGet {
            thread: None,
            id: Some(id.clone()),
        })
        .unwrap();
    assert_eq!(r["written"], true, "{r}");
    assert_eq!(r["id"], id.as_str());
    assert_eq!(r["kind"], "implement");
    assert_eq!(r["outcome"], "clean");
    assert_eq!(r["truncated"], false);
    assert_eq!(r["text"].as_str().unwrap(), report_of(&hub, &t));
    assert_eq!(r["bytes"], report_of(&hub, &t).len());
    // A loop stopped wrote none: said.
    let env = Env::new();
    env.set(|b| b.replies.push((" plan]".into(), "!sleep 600000".into())));
    let hub = env.hub().await;
    let t = implementing(&env, &hub, "web", settings("low", ImplCommit::End, None)).await;
    control(&hub, &t, ControlAction::Stop).unwrap();
    ended(&hub, &t).await;
    let r = hub
        .loop_report(LoopGet {
            thread: Some(t.clone()),
            id: None,
        })
        .unwrap();
    assert!(r["text"].is_null() && r["path"].is_null(), "{r}");
    assert!(
        r["note"].as_str().unwrap().contains("wrote no report"),
        "{r}"
    );
    hub.shutdown().await;
}

/// A pause in each new phase holds what comes next until resume, and the loop then runs to
/// its end: during the plan turn (no plan check run starts), the plan check's run (no triage
/// asked), its triage (no implement turn), the implement turn (no review run), and the report
/// turn whose commit check fails (the correction is held; the check itself is not).
#[tokio::test]
async fn a_pause_in_each_new_phase_holds_the_next_step() {
    for phase in ["plan", "p1-run", "triage p1", "implement", "commit"] {
        let env = Env::new();
        env.set(|b| {
            b.risk = true;
            match phase {
                "plan" => b.replies.push((" plan]".into(), "@slow 1000".into())),
                "triage p1" => b.replies.push((" triage p1]".into(), "@slow 1000".into())),
                "implement" => b.replies.push((" implement]".into(), "@slow 1000".into())),
                "commit" => {
                    b.leave_dirty = 1;
                    b.replies.push((" report]".into(), "@slow 1000".into()));
                }
                _ => {}
            }
        });
        env.answer(1, findings(1));
        if phase == "p1-run" {
            env.hold_run(1);
        }
        let hub = env.hub().await;
        let s = settings("low", ImplCommit::End, Some(PlanCheck::Always));
        let t = implementing(&env, &hub, "api", s).await;
        // In the phase.
        if phase == "p1-run" {
            until(&hub, "the plan check's run", |h| {
                h.thread_view(&t)
                    .is_ok_and(|v| v.background_tasks.keys().any(|k| k.starts_with("loop:")))
            })
            .await;
        } else {
            until(&hub, "the held turn", |h| {
                h.thread_view(&t).is_ok_and(|v| v.current_turn.is_some())
                    && get(h, &t)["state"]["phase"]
                        == match phase {
                            "triage p1" => "plan_check",
                            "commit" => "report",
                            p => p,
                        }
            })
            .await;
        }
        control(&hub, &t, ControlAction::Pause).unwrap();
        env.release_run(1);
        // What it was doing ends; what follows waits.
        let (next, done): (&str, fn(&Value) -> bool) = match phase {
            "plan" => (" triage p1]", |g| {
                g["plans"].as_array().is_some_and(|p| p.len() == 1)
            }),
            "p1-run" => (" triage p1]", |g| {
                g["rounds"][0]["findings"]
                    .as_array()
                    .is_some_and(|f| f.len() == 1)
            }),
            "triage p1" => (" implement]", |g| !g["rounds"][0]["triage"].is_null()),
            "implement" => (" implement]", |g| {
                g["implemented"].as_array().is_some_and(|i| i.len() == 1)
            }),
            _ => (" commit correction]", |g| {
                g["commit_checks"].as_array().is_some_and(|c| c.len() == 1)
            }),
        };
        until(&hub, "the phase's step taken", |h| done(&get(h, &t))).await;
        let before = (env.runs(), env.seen(next).len());
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(
            (env.runs(), env.seen(next).len()),
            before,
            "{phase}: went on paused"
        );
        let g = get(&hub, &t);
        assert_eq!(g["state"]["paused"], true, "{phase}: {g}");
        assert!(g["ended"].is_null(), "{phase}");
        control(&hub, &t, ControlAction::Resume).unwrap();
        let last = ended(&hub, &t).await;
        assert_eq!(last.outcome, Outcome::Clean, "{phase}");
        assert_eq!(last.commits.len(), 1, "{phase}: {last:?}");
        assert_eq!(last.commit_problem, None, "{phase}");
        hub.shutdown().await;
    }
}
