//! The orchestrator's loops (`hub::loops`, docs/design/loops.md, milestone 3): what it may ask
//! for (`loop.request`, `thread.new` / `thread.spawn` with `loop`), read (`loop.get`,
//! `loop.list`) and stop (`loop.control stop`), the rule that it never makes a loop start on
//! settings the user did not choose, what it may not do (answer or annotate the setup card,
//! read or write the settings, pause, resume or skip), and what it hears of a loop it started:
//! a start line saying it asked, routine rounds, and a notice turn with the loop's report when
//! it ends, or a stuck item when it sticks.
//!
//! A scripted thread ([`Brain`]) holds every loop step (it sleeps) unless the test drives the
//! loop ([`Env::drive`]); fake reviewer CLIs answer each review run with no findings unless a
//! run is told to fail. The orchestrator runs on the same script, which echoes its prompts.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::time::timeout;
use twr_adapters::fake::FakeAdapter;
use twr_core::adapter::{Adapter, Answer};
use twr_core::config::Config;
use twr_core::event::*;
use twr_core::loops::settings::LoopSettings;
use twr_core::loops::{ControlAction, ControlBy, Outcome};
use twr_core::proto::{codes, RpcError};
use twr_core::runner::AgentInfo;
use twr_hub::hub::{
    AgentProbe, BoxView, LoopControlParams, LoopGet, LoopRequest, LoopSettingsFor, LoopSettingsSet,
    LoopSetupGet, LoopSetupUpdate, NewThreadLoop,
};
use twr_hub::{Caller, Hub, HubOptions, NewThread};

const WAIT: Duration = Duration::from_secs(30);
const START: &str = "start and save these settings";
const TASK: &str = "add rate limiting to the webhook endpoints";

/// What the hub's agent probe finds: each runtime's `(found, login)`.
type Found = Arc<Mutex<Vec<(&'static str, bool, &'static str)>>>;

struct Env {
    dir: tempfile::TempDir,
    found: Found,
    config: Config,
    brain: Arc<Mutex<Brain>>,
}

impl Env {
    /// Loops whose steps the thread holds; an orchestrator on the scripted runtime.
    fn new() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config {
            default_runtime: "fake".into(),
            ..Default::default()
        };
        config.orchestrator.runtime = Some("claude-code".into());
        config.orchestrator.cwd = Some(dir.path().to_path_buf());
        let env = Env {
            dir,
            found: Arc::new(Mutex::new(vec![
                ("claude-code", true, "logged_in"),
                ("codex", true, "logged_in"),
                ("grok-build", true, "unknown"),
            ])),
            config,
            brain: Arc::new(Mutex::new(Brain::default())),
        };
        let bins = env.reviewers();
        std::fs::create_dir_all(bins.join("answers")).unwrap();
        for name in ["codex", "claude", "grok"] {
            let bin = bins.join(name);
            std::fs::write(&bin, FAKE_REVIEWER).unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        env
    }

    /// An `Env` whose loops run: the thread answers each step, the reviewers each round.
    fn drive() -> Env {
        let env = Env::new();
        env.brain.lock().unwrap().auto = true;
        env
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// The fake reviewer CLIs' folder.
    fn reviewers(&self) -> PathBuf {
        self.path().join("reviewers")
    }

    /// The `n`th review run fails (exits 3).
    fn fail_run(&self, n: usize) {
        std::fs::write(self.reviewers().join(format!("fail.{n}")), "").unwrap();
    }

    fn opts(&self) -> HubOptions {
        let mut o = HubOptions::new(self.path().join("data").join("twr.db"), self.config.clone());
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
        let native = self.path().join("native");
        o.native_dir = Some(Arc::new(move |rt: &str| Some(native.join(rt))));
        o.git_tick = Some(Duration::ZERO);
        o.stall_after = Some(Duration::ZERO);
        o.disk_tick = Some(Duration::ZERO);
        o.checkpoint_tick = Some(Duration::ZERO);
        o.config_file = Some(self.path().join("config.toml"));
        let found = self.found.clone();
        let probe: AgentProbe = Arc::new(move || {
            let agents: Vec<AgentInfo> = found
                .lock()
                .unwrap()
                .iter()
                .map(|(rt, f, login)| {
                    serde_json::from_value(json!({"runtime": rt, "found": f, "login": login}))
                        .unwrap()
                })
                .collect();
            futures::FutureExt::boxed(async move { agents })
        });
        o.agent_probe = Some(probe);
        o
    }

    /// The hub, with its orchestrator started and idle.
    async fn hub(&self) -> Hub {
        let hub = Hub::open(self.opts()).await.unwrap();
        hub.say_to_orchestrator("hello", None, &user()).unwrap();
        until(&hub, "the orchestrator's first turn", |h| {
            h.orchestrator_thread().is_some_and(|o| {
                events(h, &o)
                    .iter()
                    .any(|e| matches!(e.body, EventBody::TurnEnded(_)))
            })
        })
        .await;
        hub
    }

    /// A git repository `name` beside the hub's data, with one commit.
    fn repo(&self, name: &str) -> PathBuf {
        twr_core::testkit::git::init_repo(&self.path().join(name))
    }

    /// Sets what the probe finds of `runtime` and has the hub probe again.
    async fn set_found(&self, hub: &Hub, runtime: &str, found: bool, login: &'static str) {
        for r in self.found.lock().unwrap().iter_mut() {
            if r.0 == runtime {
                r.1 = found;
                r.2 = login;
            }
        }
        hub.box_agents("local", true, BoxView::Full).await.unwrap();
    }
}

/// A fake reviewer CLI (see `loops.rs`): run `n` (from 1) exits 3 with `fail.<n>`, else
/// answers no findings.
const FAKE_REVIEWER: &str = r#"#!/bin/sh
here=$(dirname "$0")
n=$(( $(cat "$here/count" 2>/dev/null || echo 0) + 1 ))
echo "$n" > "$here/count"
cat > /dev/null 2>/dev/null
if [ -f "$here/fail.$n" ]; then echo "boom: run $n failed" >&2; exit 3; fi
ans="$here/answers/none.json"
echo '{"findings": []}' > "$ans"
out=""; prev=""
for a in "$@"; do [ "$prev" = "-o" ] && out="$a"; prev="$a"; done
case "$(basename "$0")" in
  codex)
    cp "$ans" "$out"
    echo '{"type":"turn.completed","usage":{"input_tokens":100,"output_tokens":10}}' ;;
  claude)
    printf '{"type":"result","is_error":false,"result":"done","structured_output":%s,"usage":{"input_tokens":5,"output_tokens":7},"total_cost_usd":0.01}\n' "$(cat "$ans")" ;;
  grok)
    printf '{"text":"done","structuredOutput":%s,"usage":{"input_tokens":50,"output_tokens":5},"total_cost_usd":0.02}\n' "$(cat "$ans")" ;;
esac
"#;

/// What the scripted runtime answers: a loop step's prompt (`[twr loop …]`) sleeps, or, with
/// `auto`, gets a valid answer; anything else (the orchestrator's turns, ordinary prompts) is
/// echoed. Every prompt is kept in `seen`.
#[derive(Default)]
struct Brain {
    auto: bool,
    seen: Vec<String>,
}

impl Brain {
    fn reply(&mut self, prompt: &str) -> Option<String> {
        self.seen.push(prompt.to_string());
        let first = prompt.lines().next().unwrap_or_default().to_string();
        if !first.starts_with("[twr loop ") {
            return None;
        }
        if !self.auto {
            return Some("!sleep 600000".into());
        }
        if first.contains(" scope]") {
            return Some(
                "Gates are green.\n```twr-goal\nHarden the change against bad input.\n```".into(),
            );
        }
        if first.contains(" report]") {
            return Some("The loop hardened the change; nothing is left open.".into());
        }
        None
    }
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

fn user() -> Caller {
    Caller::user("test")
}

fn orch() -> Caller {
    Caller::Orchestrator
}

/// A worker thread `name` of the project in `repo`, made by the user.
async fn thread(hub: &Hub, repo: &Path, name: &str) -> String {
    let t = hub
        .new_thread(
            NewThread {
                project: repo.to_string_lossy().into_owned(),
                name: Some(name.into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap();
    t["id"].as_str().unwrap().to_string()
}

/// A worker thread `name` in a fresh repository, with a change in its worktree to review.
async fn changed(env: &Env, hub: &Hub, name: &str) -> String {
    let t = thread(hub, &env.repo(name), name).await;
    let wt = hub.thread_view(&t).unwrap().worktree.unwrap();
    std::fs::write(Path::new(&wt).join("lib.rs"), "fn changed() {}\n").unwrap();
    t
}

/// `loop.request` of a review loop for `thread`, with `extra` params, as `caller`.
async fn request(
    hub: &Hub,
    thread: &str,
    extra: Value,
    caller: &Caller,
) -> Result<Value, RpcError> {
    let mut p = json!({"thread": thread, "kind": "review"});
    for (k, v) in extra.as_object().unwrap() {
        p[k] = v.clone();
    }
    let p: LoopRequest = serde_json::from_value(p).unwrap();
    hub.loop_request(p, caller).await
}

/// Answers the card `card` (a `needs_setup` result) names with `decision`, as `caller`.
fn answer(hub: &Hub, card: &Value, decision: &str, caller: &Caller) -> Result<Event, RpcError> {
    hub.respond_at(
        card["attn_id"].as_str().unwrap(),
        Answer::new(decision),
        card["revision"].as_u64(),
        caller,
    )
}

fn events(hub: &Hub, thread: &str) -> Vec<Event> {
    hub.thread_events(thread, 0, 100_000).unwrap().1
}

fn started(hub: &Hub, thread: &str) -> Vec<LoopStarted> {
    events(hub, thread)
        .into_iter()
        .filter_map(|e| match e.body {
            EventBody::LoopStarted(p) => Some(*p),
            _ => None,
        })
        .collect()
}

/// The open item `attn_id`, from `attention.list`.
fn item(hub: &Hub, attn_id: &Value) -> Option<Value> {
    hub.attention_list(true)
        .into_iter()
        .find(|i| i["attn_id"] == *attn_id)
}

/// The card's `ext.twr.loop_setup`, as last raised.
fn setup(hub: &Hub, thread: &str, attn_id: &Value) -> LoopSetup {
    let ev = events(hub, thread)
        .into_iter()
        .rev()
        .find(|e| matches!(&e.body, EventBody::AttentionRaised(p) if p.attn_id == *attn_id))
        .unwrap();
    let EventBody::AttentionRaised(p) = ev.body else {
        unreachable!()
    };
    serde_json::from_value(p.ext.unwrap()["twr"][LOOP_SETUP_KEY].clone()).unwrap()
}

/// How many times the card was raised.
fn raises(hub: &Hub, thread: &str, attn_id: &Value) -> usize {
    events(hub, thread)
        .iter()
        .filter(|e| matches!(&e.body, EventBody::AttentionRaised(p) if p.attn_id == *attn_id))
        .count()
}

fn control(
    hub: &Hub,
    thread: &str,
    action: ControlAction,
    caller: &Caller,
) -> Result<Value, RpcError> {
    hub.loop_control(
        LoopControlParams {
            thread: thread.into(),
            action,
        },
        caller,
    )
}

async fn save(hub: &Hub, project: Option<&str>, settings: LoopSettings) {
    hub.loop_settings_set(
        LoopSettingsSet {
            project: project.map(str::to_string),
            settings,
        },
        &user(),
    )
    .await
    .unwrap();
}

/// The first card's settings on `profile` with reviewer `reviewer`.
fn settings(profile: &str, reviewer: &str) -> LoopSettings {
    let mut s = LoopSettings::first_card(&Default::default());
    s.profile = profile.into();
    s.reviewer = twr_core::loops::config::ReviewerChoice::parse(reviewer).unwrap();
    s
}

/// Waits (at most [`WAIT`]) until `f` holds of the hub.
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

/// The orchestrator conversation's updates, as `(text, routine)`.
fn lines(hub: &Hub) -> Vec<(String, Option<String>)> {
    let orch = hub.orchestrator_thread().unwrap();
    events(hub, &orch)
        .into_iter()
        .filter_map(|e| match e.body {
            EventBody::ConversationUpdate(p) => {
                let routine = p
                    .ext
                    .as_ref()
                    .and_then(|x| x["twr"]["routine"].as_str().map(str::to_string));
                Some((p.text, routine))
            }
            _ => None,
        })
        .collect()
}

/// The prompts of the orchestrator's notice turns.
fn notices(hub: &Hub) -> Vec<String> {
    let orch = hub.orchestrator_thread().unwrap();
    events(hub, &orch)
        .into_iter()
        .filter_map(|e| match e.body {
            EventBody::TurnStarted(p)
                if p.ext
                    .as_ref()
                    .is_some_and(|x| x["twr"]["notice"] == json!(true)) =>
            {
                Some(p.prompt)
            }
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------------------------
// The rule
// ---------------------------------------------------------------------------------------------

/// The orchestrator's request starts a loop only on saved settings that hold and when it names
/// nothing that differs from them: overrides equal to the saved ones start it (recorded as no
/// override); any that differs, no saved settings, saved settings that no longer hold, and
/// `setup` raise the card for the user. The card and the start say the orchestrator asked.
#[tokio::test]
async fn the_orchestrator_starts_a_loop_only_on_the_users_saved_settings() {
    let env = Env::new();
    let hub = env.hub().await;
    let repo = env.repo("api");
    let t = thread(&hub, &repo, "api").await;

    // No saved settings: the first-run card, the user's to answer; nothing starts.
    let r = request(&hub, &t, json!({}), &orch()).await.unwrap();
    assert_eq!(r["status"], "needs_setup", "{r}");
    assert_eq!(r["why"], "first_run");
    assert_eq!(r["thread_name"], "api");
    let card = setup(&hub, &t, &r["attn_id"]);
    assert_eq!(card.requested_by, ControlBy::Orchestrator);
    let summary = item(&hub, &r["attn_id"]).unwrap()["summary"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        summary.starts_with(
            "api: review loop setup (first run: choose its models; the orchestrator asked for \
             this loop). Start and save these settings?"
        ),
        "{summary}"
    );
    assert!(started(&hub, &t).is_empty());
    answer(&hub, &r, "cancel", &user()).unwrap();

    // Saved settings, and overrides that name just them: it starts, on them, with no
    // override recorded.
    save(&hub, None, settings("high", "codex/gpt-6.1-sol")).await;
    let r = request(
        &hub,
        &t,
        json!({"overrides": {"profile": "high", "reviewer": "codex/gpt-6.1-sol",
                             "implementer": "thread", "commit": "match"}}),
        &orch(),
    )
    .await
    .unwrap();
    assert_eq!(r["status"], "started", "{r}");
    assert_eq!(r["thread_name"], "api");
    assert_eq!(r["settings"]["profile"], "high");
    assert_eq!(r["settings"]["reviewer"]["model"], "gpt-6.1-sol");
    let s = started(&hub, &t);
    assert_eq!(s.len(), 1);
    assert_eq!(s[0].requested_by, ControlBy::Orchestrator);
    assert!(s[0].overrides.is_empty());
    assert_eq!(
        s[0].settings_from,
        twr_core::loops::settings::SettingsSource::Saved
    );
    let v = hub.thread_view(&t).unwrap();
    assert_eq!(
        v.r#loop.as_ref().unwrap().requested_by,
        ControlBy::Orchestrator
    );
    // The start's line says the orchestrator asked.
    assert!(
        lines(&hub).iter().any(|(l, _)| l.starts_with(
            "api started a review loop, which the orchestrator asked for: high · implementer"
        )),
        "{:?}",
        lines(&hub)
    );
    // It stops it; the line says who did.
    let stopped = control(&hub, &t, ControlAction::Stop, &orch()).unwrap();
    assert_eq!(stopped["running"], false, "{stopped}");
    let last = hub.thread_view(&t).unwrap().last_loop.unwrap();
    assert_eq!(last.outcome, Outcome::Stopped);
    assert_eq!(last.requested_by, ControlBy::Orchestrator);
    assert!(lines(&hub)
        .iter()
        .any(|(l, r)| l == "api's review loop: stop by the orchestrator"
            && r.as_deref() == Some("loop")));

    // An override that differs: the card, with the override as this run's; nothing starts.
    let r = request(&hub, &t, json!({"overrides": {"profile": "low"}}), &orch())
        .await
        .unwrap();
    assert_eq!(r["status"], "needs_setup", "{r}");
    assert_eq!(r["why"], "overrides");
    let card = setup(&hub, &t, &r["attn_id"]);
    assert_eq!(card.overrides.profile.as_deref(), Some("low"));
    assert_eq!(
        card.settings.profile, "high",
        "the card shows the saved ones"
    );
    let summary = item(&hub, &r["attn_id"]).unwrap()["summary"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        summary.contains("the request carries settings you did not type; the orchestrator asked")
            && summary.contains("This run     profile low"),
        "{summary}"
    );
    assert_eq!(started(&hub, &t).len(), 1);
    // Asked again, while the card is open: it neither starts nor raises the card again.
    for extra in [
        json!({}),
        json!({"setup": true}),
        json!({"overrides": {"profile": "medium"}}),
    ] {
        let again = request(&hub, &t, extra, &orch()).await.unwrap();
        assert_eq!(again["status"], "needs_setup");
        assert_eq!(again["existing"], true);
        assert_eq!(again["attn_id"], r["attn_id"]);
        assert_eq!(again["revision"], r["revision"]);
    }
    assert_eq!(raises(&hub, &t, &r["attn_id"]), 1);
    assert_eq!(started(&hub, &t).len(), 1);
    // The user starts it: on the card's settings and this run's override, saving the card's.
    answer(&hub, &r, START, &user()).unwrap();
    let s = started(&hub, &t);
    assert_eq!(s.len(), 2);
    assert_eq!(s[1].requested_by, ControlBy::Orchestrator);
    assert_eq!(s[1].settings.profile, "low");
    assert_eq!(
        s[1].settings_from,
        twr_core::loops::settings::SettingsSource::SetupItem
    );
    let saved = hub
        .loop_settings_get(LoopSettingsFor { project: None })
        .await
        .unwrap();
    assert_eq!(saved["settings"]["profile"], "high");
    control(&hub, &t, ControlAction::Stop, &user()).unwrap();

    // `setup`: the card, whatever is saved.
    let r = request(&hub, &t, json!({"setup": true}), &orch())
        .await
        .unwrap();
    assert_eq!(r["status"], "needs_setup");
    assert_eq!(r["why"], "asked");
    answer(&hub, &r, "cancel", &user()).unwrap();

    // Saved settings that no longer hold: the card.
    env.set_found(&hub, "codex", false, "missing").await;
    let r = request(&hub, &t, json!({}), &orch()).await.unwrap();
    assert_eq!(r["status"], "needs_setup", "{r}");
    assert_eq!(r["why"], "no_longer_holds");
    answer(&hub, &r, "cancel", &user()).unwrap();
    env.set_found(&hub, "codex", true, "logged_in").await;
    assert_eq!(started(&hub, &t).len(), 2);
    hub.shutdown().await;
}

/// A project with its own settings: those are what the orchestrator's request is compared
/// with, whole; another project's threads use the workspace's.
#[tokio::test]
async fn a_projects_own_settings_are_the_ones_compared() {
    let env = Env::new();
    let hub = env.hub().await;
    let api = env.repo("api");
    let web = env.repo("web");
    let a = thread(&hub, &api, "a1").await;
    let b = thread(&hub, &api, "a2").await;
    let w = thread(&hub, &web, "w1").await;
    assert_eq!(hub.thread_view(&a).unwrap().project.as_deref(), Some("api"));
    save(&hub, None, settings("high", "codex/gpt-6.1-sol")).await;
    save(&hub, Some("api"), settings("low", "codex/gpt-6.1-sol")).await;
    // `low` is api's: it starts, on api's settings.
    let r = request(&hub, &a, json!({"overrides": {"profile": "low"}}), &orch())
        .await
        .unwrap();
    assert_eq!(r["status"], "started", "{r}");
    assert_eq!(r["settings_of"], "api");
    assert_eq!(
        started(&hub, &a)[0].settings_of,
        Some(twr_core::loops::settings::SettingsOf::Project)
    );
    // `high` is the workspace's, not api's: the card.
    let r = request(&hub, &b, json!({"overrides": {"profile": "high"}}), &orch())
        .await
        .unwrap();
    assert_eq!(r["status"], "needs_setup", "{r}");
    assert_eq!(r["why"], "overrides");
    assert_eq!(
        setup(&hub, &b, &r["attn_id"]).save_for,
        twr_core::loops::settings::SaveFor::Project
    );
    // web has none of its own: the workspace's `high` starts.
    let r = request(&hub, &w, json!({"overrides": {"profile": "high"}}), &orch())
        .await
        .unwrap();
    assert_eq!(r["status"], "started", "{r}");
    assert!(r["settings_of"].is_null());
    hub.shutdown().await;
}

/// What stays the user's: the card (answer, annotate, dismiss, read, change), the settings,
/// pausing, resuming and skipping. The orchestrator's methods are `loop.request`, `loop.get`,
/// `loop.list` and `loop.control` (stop); an agent has none. A user's change of a card the
/// orchestrator asked for drops the orchestrator's picks: what the user sets is what runs.
#[tokio::test]
async fn the_card_the_settings_and_the_other_controls_stay_the_users() {
    let loop_methods: Vec<&str> = twr_hub::server::ORCHESTRATOR_METHODS
        .iter()
        .copied()
        .filter(|m| m.starts_with("loop."))
        .collect();
    assert_eq!(
        loop_methods,
        ["loop.request", "loop.get", "loop.list", "loop.control"]
    );
    assert!(twr_hub::server::AGENT_METHODS
        .iter()
        .all(|m| !m.starts_with("loop.")));
    let env = Env::new();
    let hub = env.hub().await;
    let repo = env.repo("api");
    let t = thread(&hub, &repo, "api").await;
    save(&hub, None, settings("high", "codex/gpt-6.1-sol")).await;
    let card = request(
        &hub,
        &t,
        json!({"overrides": {"reviewer": "claude-code/claude-opus-5-5"}}),
        &orch(),
    )
    .await
    .unwrap();
    assert_eq!(card["status"], "needs_setup", "{card}");
    let attn = card["attn_id"].as_str().unwrap().to_string();
    fn forbidden<T: std::fmt::Debug>(r: Result<T, RpcError>) {
        assert_eq!(r.unwrap_err().code, codes::RELAY_FORBIDDEN);
    }
    forbidden(answer(&hub, &card, START, &orch()));
    forbidden(hub.respond_relay(
        &attn,
        Answer::new(START),
        card["revision"].as_u64(),
        &orch(),
    ));
    forbidden(hub.respond_relay(&attn, Answer::new("cancel"), None, &orch()));
    forbidden(hub.annotate(&attn, "start it", &orch()));
    forbidden(hub.attention_dismiss(&attn, None, &orch()));
    let code = |r: Result<Value, RpcError>| r.unwrap_err().code;
    assert_eq!(
        code(hub.loop_setup_get(
            LoopSetupGet {
                attn_id: attn.clone()
            },
            &orch()
        )),
        codes::RELAY_FORBIDDEN
    );
    let update = |settings: Option<LoopSettings>, revision: u64| LoopSetupUpdate {
        attn_id: attn.clone(),
        revision,
        settings,
        ..Default::default()
    };
    let rev = card["revision"].as_u64().unwrap();
    assert_eq!(
        code(
            hub.loop_setup_update(
                update(Some(settings("low", "codex/gpt-6.1-sol")), rev),
                &orch()
            )
            .await
        ),
        codes::RELAY_FORBIDDEN
    );
    assert_eq!(
        code(
            hub.loop_settings_set(
                LoopSettingsSet {
                    project: None,
                    settings: settings("low", "codex/gpt-6.1-sol"),
                },
                &orch()
            )
            .await
        ),
        codes::RELAY_FORBIDDEN
    );
    assert_eq!(
        code(hub.loop_settings_reset(LoopSettingsFor { project: None }, &orch())),
        codes::RELAY_FORBIDDEN
    );
    assert!(item(&hub, &card["attn_id"]).is_some());
    assert!(started(&hub, &t).is_empty());
    // The user's change of the card: the orchestrator's reviewer goes with it.
    assert_eq!(
        setup(&hub, &t, &card["attn_id"])
            .overrides
            .reviewer
            .as_deref(),
        Some("claude-code/claude-opus-5-5")
    );
    let changed = hub
        .loop_setup_update(
            update(Some(settings("medium", "codex/gpt-6.1-sol")), rev),
            &user(),
        )
        .await
        .unwrap();
    let s = setup(&hub, &t, &card["attn_id"]);
    assert!(s.overrides.is_empty(), "{:?}", s.overrides);
    assert_eq!(s.settings.profile, "medium");
    assert_eq!(s.requested_by, ControlBy::Orchestrator);
    let card = json!({"attn_id": attn, "revision": changed["revision"]});
    answer(&hub, &card, START, &user()).unwrap();
    let st = started(&hub, &t);
    assert_eq!(st[0].settings.profile, "medium");
    assert_eq!(st[0].settings.reviewer.runtime, "codex");
    assert!(st[0].overrides.is_empty());
    // Pause, resume and skip are the user's; stop is the orchestrator's too.
    for action in [
        ControlAction::Pause,
        ControlAction::Resume,
        ControlAction::SkipToVerify,
    ] {
        let e = control(&hub, &t, action, &orch()).unwrap_err();
        assert_eq!(e.code, codes::RELAY_FORBIDDEN, "{action:?}");
        assert!(e.message.contains("only the user may"), "{}", e.message);
    }
    control(&hub, &t, ControlAction::Pause, &user()).unwrap();
    assert!(hub.thread_view(&t).unwrap().r#loop.unwrap().paused);
    // It reads the loop (`get_loop`), and the running loops.
    let list = hub.loop_list();
    assert_eq!(list[0]["thread_name"], "api");
    let got = hub
        .loop_get(LoopGet {
            thread: Some(t.clone()),
            id: None,
        })
        .unwrap();
    assert_eq!(got["running"], true);
    assert_eq!(got["started"]["requested_by"], "orchestrator");
    control(&hub, &t, ControlAction::Stop, &orch()).unwrap();
    assert_eq!(
        control(&hub, &t, ControlAction::Stop, &orch())
            .unwrap_err()
            .code,
        codes::CONFLICT
    );
    // A loop the user asked for says so (`requested_by` left out).
    let r = request(&hub, &t, json!({}), &user()).await.unwrap();
    assert_eq!(r["status"], "started", "{r}");
    let got = hub
        .loop_get(LoopGet {
            thread: Some(t.clone()),
            id: None,
        })
        .unwrap();
    assert_eq!(got["started"]["requested_by"], "user");
    assert_eq!(started(&hub, &t)[1].requested_by, ControlBy::User);
    let user_started = serde_json::to_value(&started(&hub, &t)[1]).unwrap();
    assert!(user_started.get("requested_by").is_none(), "{user_started}");
    let orch_started = serde_json::to_value(&started(&hub, &t)[0]).unwrap();
    assert_eq!(orch_started["requested_by"], "orchestrator");
    hub.shutdown().await;
}

// ---------------------------------------------------------------------------------------------
// A new thread with a loop
// ---------------------------------------------------------------------------------------------

/// `thread.spawn` as the orchestrator: the operation's result, once complete.
async fn spawn(hub: &Hub, op: &str, name: &str, repo: &Path, l: NewThreadLoop) -> Value {
    let req = NewThread {
        project: repo.to_string_lossy().into_owned(),
        name: Some(name.into()),
        // (Not `fake`: the orchestrator starts agent runtimes only; this one is scripted.)
        runtime: Some("claude-code".into()),
        loop_req: Some(l),
        ..Default::default()
    };
    let mut r = hub.begin_spawn(op, req, None, &orch()).unwrap();
    timeout(WAIT, async {
        while r["status"] == "pending" {
            tokio::time::sleep(Duration::from_millis(20)).await;
            r = hub.spawn_status(op, &orch()).unwrap();
        }
    })
    .await
    .expect("the spawn completes");
    assert_eq!(r["status"], "complete", "{r}");
    r
}

fn implement(setup: bool, overrides: Value) -> NewThreadLoop {
    NewThreadLoop {
        kind: "implement".into(),
        task: TASK.into(),
        setup,
        overrides: serde_json::from_value(overrides).unwrap(),
    }
}

/// The turns a thread started, by prompt.
fn turn_prompts(hub: &Hub, t: &str) -> Vec<String> {
    events(hub, t)
        .into_iter()
        .filter_map(|e| match e.body {
            EventBody::TurnStarted(p) => Some(p.prompt),
            _ => None,
        })
        .collect()
}

/// `spawn_thread` with a loop: without saved settings the thread is made, its task held on the
/// card; cancelled, the thread stays idle and the task is never sent; answered by the user,
/// the loop starts (the orchestrator's, as it asked) with the task in its plan prompt. With
/// saved settings it starts at once, and an override that differs raises the card.
#[tokio::test]
async fn spawn_thread_with_a_loop() {
    let env = Env::new();
    let hub = env.hub().await;
    let repo = env.repo("api");
    // The card, cancelled: the thread stays, the task is not sent.
    let r = spawn(&hub, "op-a", "limits-a", &repo, implement(false, json!({}))).await;
    let a = r["thread"]["id"].as_str().unwrap().to_string();
    let l = &r["thread"]["loop"];
    assert_eq!(l["status"], "needs_setup", "{r}");
    assert_eq!(l["why"], "first_run");
    let card = setup(&hub, &a, &l["attn_id"]);
    assert_eq!(card.task.as_deref(), Some(TASK));
    assert_eq!(card.requested_by, ControlBy::Orchestrator);
    forbidden_answer(&hub, l);
    answer(&hub, l, "cancel", &user()).unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(turn_prompts(&hub, &a).is_empty());
    assert_eq!(hub.get_thread(&a).unwrap()["status"], "idle");
    assert!(hub.thread_view(&a).unwrap().last_loop.is_none());
    // The card, answered by the user: the loop starts, with the task in its plan prompt.
    let r = spawn(&hub, "op-b", "limits-b", &repo, implement(false, json!({}))).await;
    let b = r["thread"]["id"].as_str().unwrap().to_string();
    answer(&hub, &r["thread"]["loop"], START, &user()).unwrap();
    let s = started(&hub, &b);
    assert_eq!(s[0].requested_by, ControlBy::Orchestrator);
    assert_eq!(s[0].task.as_deref(), Some(TASK));
    until(&hub, "b's plan turn", |h| !turn_prompts(h, &b).is_empty()).await;
    let p = &turn_prompts(&hub, &b)[0];
    assert!(
        p.lines().next().unwrap().ends_with(" plan]") && p.contains(TASK),
        "{p}"
    );
    control(&hub, &b, ControlAction::Stop, &orch()).unwrap();
    // Saved now: the next starts at once; the task is never an ordinary prompt.
    let r = spawn(&hub, "op-c", "limits-c", &repo, implement(false, json!({}))).await;
    let c = r["thread"]["id"].as_str().unwrap().to_string();
    assert_eq!(r["thread"]["loop"]["status"], "started", "{r}");
    assert_eq!(started(&hub, &c)[0].requested_by, ControlBy::Orchestrator);
    until(&hub, "c's plan turn", |h| !turn_prompts(h, &c).is_empty()).await;
    assert!(turn_prompts(&hub, &c).iter().all(|p| p != TASK));
    control(&hub, &c, ControlAction::Stop, &orch()).unwrap();
    // An override that differs, or `setup`: the card.
    let r = spawn(
        &hub,
        "op-d",
        "limits-d",
        &repo,
        implement(false, json!({"profile": "low"})),
    )
    .await;
    assert_eq!(r["thread"]["loop"]["status"], "needs_setup", "{r}");
    assert_eq!(r["thread"]["loop"]["why"], "overrides");
    let r = spawn(&hub, "op-e", "limits-e", &repo, implement(true, json!({}))).await;
    assert_eq!(r["thread"]["loop"]["why"], "asked", "{r}");
    // The same operation asked again answers the same, its loop's result kept.
    let again = hub.spawn_status("op-e", &orch()).unwrap();
    assert_eq!(again["thread"]["loop"]["why"], "asked", "{again}");
    hub.shutdown().await;
}

fn forbidden_answer(hub: &Hub, card: &Value) {
    let e = answer(hub, card, START, &orch()).unwrap_err();
    assert_eq!(e.code, codes::RELAY_FORBIDDEN);
    let e = answer(hub, card, "run without a loop", &orch()).unwrap_err();
    assert_eq!(e.code, codes::RELAY_FORBIDDEN);
}

// ---------------------------------------------------------------------------------------------
// What the orchestrator hears
// ---------------------------------------------------------------------------------------------

/// A loop the orchestrator started ends with a notice turn carrying its report; one the user
/// started ends with a line and no turn. Rounds are routine lines; a setup card wakes it not.
#[tokio::test]
async fn a_loop_the_orchestrator_started_wakes_it_with_its_report() {
    let env = Env::drive();
    let hub = env.hub().await;
    save(&hub, None, settings("low", "codex/gpt-6.1-sol")).await;
    // A card it asked for: no notice turn (the result told it, and the card is the user's).
    let carded = changed(&env, &hub, "carded").await;
    let card = request(
        &hub,
        &carded,
        json!({"overrides": {"profile": "high"}}),
        &orch(),
    )
    .await
    .unwrap();
    assert_eq!(card["status"], "needs_setup");
    // The user's loop.
    let theirs = changed(&env, &hub, "theirs").await;
    request(&hub, &theirs, json!({}), &user()).await.unwrap();
    assert_eq!(ended(&hub, &theirs).await.outcome, Outcome::Clean);
    // The orchestrator's.
    let mine = changed(&env, &hub, "mine").await;
    let r = request(&hub, &mine, json!({}), &orch()).await.unwrap();
    assert_eq!(r["status"], "started", "{r}");
    let last = ended(&hub, &mine).await;
    assert_eq!(last.outcome, Outcome::Clean);
    assert_eq!(last.requested_by, ControlBy::Orchestrator);
    until(&hub, "the notice of mine's report", |h| {
        notices(h).iter().any(|n| n.contains("thread=mine report"))
    })
    .await;
    let n = notices(&hub);
    let mine_notice = n.iter().find(|n| n.contains("thread=mine report")).unwrap();
    assert!(
        mine_notice.contains("grade=success")
            && mine_notice.contains("review loop clean after 1 round"),
        "{mine_notice}"
    );
    assert!(mine_notice.contains("report.md"), "{mine_notice}");
    assert!(n.iter().all(|n| !n.contains("thread=theirs")), "{n:?}");
    assert!(
        n.iter()
            .all(|n| !n.contains(card["attn_id"].as_str().unwrap())),
        "{n:?}"
    );
    // The lines: the start says who asked, the round is routine, the end is not.
    let ls = lines(&hub);
    type Line = (String, Option<String>);
    let has = |pred: &dyn Fn(&Line) -> bool| ls.iter().any(pred);
    assert!(
        has(&|(l, _)| l
            .starts_with("mine started a review loop, which the orchestrator asked for: low")),
        "{ls:?}"
    );
    assert!(
        has(&|(l, _)| l.starts_with("theirs started a review loop: low")),
        "{ls:?}"
    );
    assert!(
        has(
            &|(l, r)| l.starts_with("mine: review round m1 · gpt-6.1-sol medium · 0 findings")
                && r.as_deref() == Some("loop")
        ),
        "{ls:?}"
    );
    assert!(
        has(&|(l, r)| l.starts_with("mine: review loop clean after 1 round") && r.is_none()),
        "{ls:?}"
    );
    assert!(
        has(
            &|(l, _)| l.starts_with("carded waits for the user to choose its loop's settings")
                && l.contains("the orchestrator asked for this loop")
        ),
        "{ls:?}"
    );
    hub.shutdown().await;
}

/// A loop the orchestrator started that sticks (its review run failed twice) wakes it: the
/// failed rounds' lines are not routine, and the stuck item is a notice; still only the user
/// answers it.
#[tokio::test]
async fn a_stuck_loop_the_orchestrator_started_is_no_routine_update() {
    let env = Env::drive();
    env.fail_run(1);
    env.fail_run(2);
    let hub = env.hub().await;
    save(&hub, None, settings("low", "codex/gpt-6.1-sol")).await;
    let t = changed(&env, &hub, "flaky").await;
    let r = request(&hub, &t, json!({}), &orch()).await.unwrap();
    assert_eq!(r["status"], "started", "{r}");
    until(&hub, "the stuck item's notice", |h| {
        notices(h)
            .iter()
            .any(|n| n.contains("thread=flaky reason=error"))
    })
    .await;
    let ls = lines(&hub);
    let failed: Vec<_> = ls
        .iter()
        .filter(|(l, _)| l.starts_with("flaky: review round m1 (gpt-6.1-sol medium) failed"))
        .collect();
    assert!(!failed.is_empty(), "{ls:?}");
    assert!(failed.iter().all(|(_, r)| r.is_none()), "{failed:?}");
    let stuck = hub
        .attention_list(true)
        .into_iter()
        .find(|i| i["thread"] == t.as_str() && i["reason"] == "error")
        .unwrap();
    let e = hub
        .respond_at(
            stuck["attn_id"].as_str().unwrap(),
            Answer::new("resume"),
            stuck["revision"].as_u64(),
            &orch(),
        )
        .unwrap_err();
    assert_eq!(e.code, codes::RELAY_FORBIDDEN);
    let got = hub
        .loop_get(LoopGet {
            thread: Some(t.clone()),
            id: None,
        })
        .unwrap();
    assert_eq!(got["state"]["waiting_on"], "user", "{got}");
    hub.shutdown().await;
}
