//! Hub tests: `say` routing over the decision ladder, the orchestrator thread's
//! lifecycle and spawn config, the relay guard, notices, written reports and annotations.
//!
//! Workers run the fake adapter. The orchestrator's runtime is `claude-code` played by a test
//! adapter that hands every session to the test ([`OrchRt`]), so each test drives its turns.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::timeout;
use twr_adapters::fake::FakeAdapter;
use twr_core::adapter::{
    Adapter, AdapterCmd, AdapterEnd, AdapterHandle, AdapterInfo, Answer, SpawnConfig, Tier,
};
use twr_core::config::Config;
use twr_core::event::*;
use twr_core::proto::{codes, RpcError};
use twr_hub::hub::ControlSubscription;
use twr_hub::{Caller, Hub, HubOptions, NewThread, Relay};

const WAIT: Duration = Duration::from_secs(10);

/// One orchestrator runtime session handed to the test.
struct OrchRt {
    cfg: SpawnConfig,
    end: AdapterEnd,
    first: Option<(String, u64)>,
}

impl OrchRt {
    /// The next turn the hub gives this session: `(prompt, turn)`.
    async fn next_turn(&mut self) -> (String, u64) {
        if let Some(f) = self.first.take() {
            return f;
        }
        timeout(WAIT, async {
            loop {
                match self.end.cmds.recv().await {
                    Some(AdapterCmd::Send { text, turn }) => return (text, turn),
                    Some(_) => continue,
                    None => panic!("the hub dropped the orchestrator runtime"),
                }
            }
        })
        .await
        .expect("timed out waiting for an orchestrator turn")
    }

    /// Ends `turn` with a short reply.
    fn end_turn(&self, turn: u64, reply: &str) {
        self.end.emit(
            EventBody::AgentText(AgentText {
                text: reply.into(),
                is_final: true,
                ..Default::default()
            }),
            Some(turn),
        );
        self.end.emit(
            EventBody::TurnEnded(TurnEnded {
                result: TurnResult::Completed,
                ..Default::default()
            }),
            Some(turn),
        );
    }

    /// The MCP configuration the runtime gets (a file: the token never goes on a command line).
    fn mcp(&self) -> Value {
        let path = self
            .cfg
            .mcp_config_file
            .as_ref()
            .expect("an MCP config file");
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn token(&self) -> String {
        self.mcp()["mcpServers"]["twr"]["env"]["TWR_ORCH_TOKEN"]
            .as_str()
            .unwrap()
            .to_string()
    }
}

/// The model the test orchestrator runtime reports (`thread.started` `ext.claude.model`).
const ORCH_MODEL: &str = "claude-haiku-4-5-20251001";

/// `claude-code` for the tests: every session goes to the test through a channel.
struct OrchAdapter(mpsc::UnboundedSender<OrchRt>);

impl Adapter for OrchAdapter {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "claude-code",
            tier: Tier::Headless,
            description: "test orchestrator runtime",
        }
    }

    fn spawn(&self, cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let (h, end) = AdapterHandle::pair();
        end.emit(
            EventBody::ThreadStarted(SessionPayload {
                session_id: Some("s-orch".into()),
                // What Claude's `system/init` reports, as the real adapter records it.
                ext: json!({"claude": {"model": ORCH_MODEL}})
                    .as_object()
                    .cloned(),
                ..Default::default()
            }),
            None,
        );
        let first = prompt.map(|p| (p, cfg.prompt_turn.unwrap_or(1)));
        let _ = self.0.send(OrchRt { cfg, end, first });
        Ok(h)
    }
}

struct Env {
    dir: tempfile::TempDir,
    spawns: mpsc::UnboundedReceiver<OrchRt>,
    tx: mpsc::UnboundedSender<OrchRt>,
}

impl Env {
    fn new() -> Env {
        let (tx, spawns) = mpsc::unbounded_channel();
        Env {
            dir: tempfile::tempdir().unwrap(),
            spawns,
            tx,
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// `dir` made a git repository with one commit: the orchestrator starts threads only in
    /// one (and not in one holding the hub's data directory, [`Env::path`]).
    fn git_init(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        for args in [
            &["init", "-q"][..],
            &["commit", "-q", "--allow-empty", "-m", "init"][..],
        ] {
            let out = std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t"])
                .args([
                    "-c",
                    "init.defaultBranch=main",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {out:?}");
        }
    }

    /// A git repository in [`Env::path`], made on first use.
    fn repo(&self) -> PathBuf {
        let dir = self.path().join("repo");
        if !dir.join(".git").exists() {
            Env::git_init(&dir);
        }
        dir
    }

    fn opts(&self, edit: impl FnOnce(&mut Config)) -> HubOptions {
        let mut config = Config {
            default_runtime: "fake".into(),
            ..Default::default()
        };
        config.orchestrator.runtime = Some("claude-code".into());
        config.orchestrator.cwd = Some(self.path().to_path_buf());
        config.orchestrator.model = Some("haiku".into());
        edit(&mut config);
        let mut o = HubOptions::new(self.path().join("data").join("twr.db"), config);
        let orch: Arc<dyn Adapter> = Arc::new(OrchAdapter(self.tx.clone()));
        o.adapters = Arc::new(move |name: &str| -> Option<Arc<dyn Adapter>> {
            match name {
                "fake" => Some(Arc::new(FakeAdapter::new())),
                "claude-code" => Some(Arc::clone(&orch)),
                _ => None,
            }
        });
        o.socket = Some(self.path().join("hub.sock"));
        o.twr_exe = Some(PathBuf::from("/opt/twr/bin/twr"));
        // Claude Code's managed policy from this test's own (empty) directories, never the
        // host's.
        o.claude_policy = twr_adapters::claude_code::guard::PolicySources {
            managed_dir: Some(self.path().join("claude-managed")),
            config_dir: Some(self.path().join("claude-config")),
        };
        o.stop_timeout = Duration::from_secs(5);
        o
    }

    async fn hub(&self) -> Hub {
        self.hub_with(|_| {}).await
    }

    async fn hub_with(&self, edit: impl FnOnce(&mut Config)) -> Hub {
        Hub::open(self.opts(edit)).await.unwrap()
    }

    /// Reopens after the previous hub was dropped (the data-dir lock may linger briefly).
    async fn reopen(&self) -> Hub {
        self.reopen_with(|_| {}).await
    }

    async fn reopen_with(&self, edit: impl Fn(&mut Config)) -> Hub {
        timeout(WAIT, async {
            loop {
                match Hub::open(self.opts(&edit)).await {
                    Ok(h) => return h,
                    Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
                }
            }
        })
        .await
        .expect("reopening the hub")
    }

    async fn orch(&mut self) -> OrchRt {
        timeout(WAIT, self.spawns.recv())
            .await
            .expect("timed out waiting for the orchestrator to spawn")
            .unwrap()
    }

    async fn worker(&self, hub: &Hub, name: &str) -> String {
        let v = hub
            .new_thread(
                NewThread {
                    project: self.path().to_string_lossy().into_owned(),
                    runtime: Some("fake".into()),
                    name: Some(name.into()),
                    ..Default::default()
                },
                "test",
            )
            .await
            .unwrap();
        v["id"].as_str().unwrap().to_string()
    }
}

fn tui() -> Caller {
    Caller::user("tui")
}

async fn until(sub: &mut ControlSubscription, pred: impl Fn(&Event) -> bool) -> Vec<Event> {
    let mut seen = Vec::new();
    timeout(WAIT, async {
        loop {
            let (_, ev) = sub.next().await.unwrap().expect("hub stopped");
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
            seen.iter()
                .map(|e| e.kind().to_string())
                .collect::<Vec<_>>()
        )
    });
    seen
}

fn is_kind_on(kind: &'static str, thread: String) -> impl Fn(&Event) -> bool {
    move |e: &Event| e.kind() == kind && e.thread == thread
}

fn raised(e: &Event) -> Option<&AttentionRaised> {
    match &e.body {
        EventBody::AttentionRaised(p) => Some(p),
        _ => None,
    }
}

fn started(e: &Event) -> Option<&TurnStarted> {
    match &e.body {
        EventBody::TurnStarted(p) => Some(p),
        _ => None,
    }
}

/// Every event of every thread, serialized.
fn log_json(hub: &Hub) -> String {
    let mut out = String::new();
    let mut threads: Vec<String> = hub
        .list_threads()
        .iter()
        .map(|t| t["id"].as_str().unwrap().to_string())
        .collect();
    threads.push(HUB_THREAD.into());
    for t in threads {
        let evs = match hub.thread_events(&t, 0, 10_000) {
            Ok((_, evs)) => evs,
            Err(_) => {
                let (items, _) = hub.control_read(0, 10_000).unwrap();
                items
                    .into_iter()
                    .filter(|(_, e)| e.thread == t)
                    .map(|(_, e)| e)
                    .collect()
            }
        };
        out.push_str(&serde_json::to_string(&evs).unwrap());
    }
    out
}

fn hub_events(hub: &Hub) -> Vec<Event> {
    let (items, _) = hub.control_read(0, 10_000).unwrap();
    items
        .into_iter()
        .map(|(_, e)| e)
        .filter(|e| e.thread == HUB_THREAD)
        .collect()
}

// ---------------------------------------------------------------------------------------------
// say
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn say_sends_to_the_single_thread_with_routed_by() {
    let env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    let mut sub = hub.subscribe_control(0);
    let r = hub.say("  run the tests ", None, &tui()).await.unwrap();
    assert_eq!(r["action"], "send");
    assert_eq!(r["thread"], api.as_str());
    assert_eq!(r["thread_name"], "api");
    assert_eq!(r["turn"], 1);
    let ids = r["decision_ids"].as_array().unwrap();
    assert_eq!(ids.len(), 1);
    let evs = until(&mut sub, is_kind_on("turn.started", api.clone())).await;
    let d = evs.iter().find(|e| e.kind() == "decision.made").unwrap();
    assert_eq!(d.thread, HUB_THREAD);
    assert_eq!(d.source, Source::Decision("code".into()));
    assert_eq!(d.id, ids[0].as_str().unwrap());
    let EventBody::DecisionMade(dm) = &d.body else {
        panic!()
    };
    assert_eq!(
        (dm.site, dm.provider, dm.chosen.as_str()),
        (DecisionSite::Route, DecisionProvider::Code, "api")
    );
    assert_eq!(dm.options, vec!["api", "new thread"]);
    let ts = evs.iter().find_map(started).unwrap();
    assert_eq!(ts.prompt, "run the tests");
    assert_eq!(ts.from, TurnFrom::User);
    assert_eq!(ts.routed_by.as_deref(), Some(d.id.as_str()));
    let turn_ev = evs.iter().find(|e| e.kind() == "turn.started").unwrap();
    assert_eq!(turn_ev.source, Source::User("tui".into()));
    // Grading went through the ladder: one code decision per finished turn, as before.
    let evs = until(&mut sub, is_kind_on("report.ready", api.clone())).await;
    let grade = evs
        .iter()
        .find(|e| e.kind() == "decision.made" && e.thread == api)
        .unwrap();
    let EventBody::DecisionMade(g) = &grade.body else {
        panic!()
    };
    assert_eq!(
        (g.site, g.provider, g.chosen.as_str(), g.confidence),
        (DecisionSite::Grade, DecisionProvider::Code, "success", 0.6)
    );
    // Empty messages and the orchestrator identity are refused.
    assert_eq!(
        hub.say("  ", None, &tui()).await.unwrap_err().code,
        codes::BAD_REQUEST
    );
    assert_eq!(
        hub.say("hi", None, &Caller::Orchestrator)
            .await
            .unwrap_err()
            .code,
        codes::BAD_REQUEST
    );
    hub.shutdown().await;
}

#[tokio::test]
async fn say_reply_answers_the_focused_threads_permission() {
    let env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    let web = env.worker(&hub, "web").await;
    let mut sub = hub.subscribe_control(0);
    hub.send_by(&api, "!perm Bash rm -rf build", &tui())
        .unwrap();
    let evs = until(&mut sub, |e| e.kind() == "attention.raised").await;
    let raised_ev = evs.last().unwrap().clone();
    let attn = raised(&raised_ev).unwrap().attn_id.clone();
    let seen = gseq_of(&hub, &raised_ev.id);
    // A client that does not say what it had seen (`twr say`), or had not rendered the item
    // yet, answers nothing: the message is neither an answer nor text for the blocked thread.
    for seen in [None, Some(seen - 1)] {
        let r = hub.say_seen("yes", None, seen, &tui()).await.unwrap();
        assert_ne!(r["action"], "respond", "{seen:?}: {r}");
        assert_ne!(r["thread"], api.as_str(), "{seen:?}: {r}");
        assert_eq!(
            hub.attention_list(false).len(),
            1 + usize::from(r["action"] == "ask")
        );
    }
    // Focus defaults to the leftmost card (api, it has the permission); `web` focused gets
    // the reply as text instead.
    let r = hub.say_seen("yes", None, Some(seen), &tui()).await.unwrap();
    assert_eq!(r["action"], "respond");
    assert_eq!(r["attn_id"], attn.as_str());
    assert_eq!(r["answer"], "allow");
    let evs = until(&mut sub, |e| e.kind() == "attention.resolved").await;
    let EventBody::AttentionResolved(p) = &evs.last().unwrap().body else {
        panic!()
    };
    assert_eq!(p.by, ResolvedBy::User);
    assert_eq!(evs.last().unwrap().source, Source::User("tui".into()));
    let r = hub
        .say_seen("ok", Some("web"), Some(seen), &tui())
        .await
        .unwrap();
    assert_eq!(
        (r["action"].as_str(), r["thread"].as_str()),
        (Some("send"), Some(web.as_str()))
    );
    hub.shutdown().await;
}

/// A message `say` hands to the orchestrator carries what the user had seen: without
/// `seen_gseq` (or before the item) the orchestrator cannot relay it as an answer either.
#[tokio::test]
async fn a_delegated_message_answers_only_what_the_user_had_seen() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    env.worker(&hub, "web").await;
    hub.say("watch the builds", None, &tui()).await.unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    raise(
        &hub,
        &api,
        "p1",
        AttentionReason::Permission,
        &["allow", "deny"],
    );
    let (items, head) = hub.control_read(0, 1000).unwrap();
    let raised_at = items
        .iter()
        .find(|(_, e)| matches!(&e.body, EventBody::AttentionRaised(p) if p.attn_id=="p1"))
        .unwrap()
        .0;
    // Without a frame, an answer by name is never sent: `say` lists what is pending.
    let r = hub
        .say_seen("@orch approve api", None, None, &tui())
        .await
        .unwrap();
    assert_eq!(r["action"], "pending", "{r}");
    // `@orch approve api` addresses the orchestrator: it goes to it as a user turn.
    let mut turn = t1;
    for (seen, relays) in [(Some(raised_at - 1), false), (Some(head), true)] {
        let r = hub
            .say_seen("@orch approve api", None, seen, &tui())
            .await
            .unwrap();
        assert_eq!(r["action"], "send", "{r}");
        rt.end_turn(turn, "ok");
        let (msg, t) = rt.next_turn().await;
        assert!(msg.ends_with("approve api"), "{msg}");
        turn = t;
        let r = relay(&hub, "p1", Answer::allow());
        if relays {
            r.unwrap();
        } else {
            let m = forbidden(r);
            assert!(m.contains("raised after"), "{seen:?}: {m}");
        }
    }
    rt.end_turn(turn, "done");
    drop(rt);
    hub.shutdown().await;
}

#[tokio::test]
async fn say_new_project_creates_a_thread_and_sends() {
    let env = Env::new();
    let hub = env.hub().await;
    let proj = env.path().join("billing");
    std::fs::create_dir(&proj).unwrap();
    let r = hub
        .say(
            &format!("new {}: add invoices", proj.display()),
            None,
            &tui(),
        )
        .await
        .unwrap();
    assert_eq!(r["action"], "new_thread");
    assert_eq!(r["thread_name"], "billing-1");
    assert_eq!(r["turn"], 1);
    let v = hub.thread_view("billing-1").unwrap();
    assert_eq!(v.runtime, "fake");
    assert_eq!(v.project.as_deref(), Some("billing"));
    hub.shutdown().await;
}

#[tokio::test]
async fn without_an_orchestrator_say_asks_which_thread_and_the_answer_routes() {
    let env = Env::new();
    let no_orch = |c: &mut Config| c.orchestrator.runtime = Some("nope".into());
    let hub = env.hub_with(no_orch).await;
    let api = env.worker(&hub, "api").await;
    let web = env.worker(&hub, "web").await;
    let r = hub.say("deploy the thing", None, &tui()).await.unwrap();
    assert_eq!(r["action"], "ask");
    assert_eq!(
        r["options"],
        json!(["api", "web", "new thread", "cancel (don't send)"])
    );
    assert!(r["decision_ids"].as_array().unwrap().is_empty());
    let attn = r["attn_id"].as_str().unwrap().to_string();
    let q = hub_events(&hub)
        .into_iter()
        .find(|e| e.kind() == "attention.raised")
        .unwrap();
    let p = raised(&q).unwrap();
    assert_eq!(p.reason, AttentionReason::Question);
    assert_eq!(p.summary, "Which thread should get: deploy the thing?");
    assert_eq!(
        p.ext.as_ref().unwrap()["twr"]["pending_message"],
        "deploy the thing"
    );
    let listed = hub.attention_list(false);
    assert_eq!(listed[0]["thread"], HUB_THREAD);

    // The question survives a restart (it is in the log).
    hub.shutdown().await;
    drop(hub);
    let hub = env.reopen_with(no_orch).await;

    // `new thread` needs an orchestrator; a non-thread is refused; both leave it open.
    let e = hub
        .respond(&attn, Answer::new("new thread"), "tui")
        .unwrap_err();
    assert_eq!(e.code, codes::CONFLICT, "{e:?}");
    let e = hub.respond(&attn, Answer::new("mars"), "tui").unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST);
    assert_eq!(hub.attention_list(false).len(), 1);

    let mut sub = hub.subscribe_control(0);
    hub.respond_option(&attn, 1, None, "tui").unwrap();
    let evs = until(&mut sub, is_kind_on("turn.started", web.clone())).await;
    let ts = started(evs.last().unwrap()).unwrap();
    assert_eq!(ts.prompt, "deploy the thing");
    assert_eq!(ts.from, TurnFrom::User);
    assert_eq!(evs.last().unwrap().source, Source::User("tui".into()));
    let d = evs
        .iter()
        .find(|e| e.kind() == "decision.made" && e.thread == HUB_THREAD)
        .unwrap();
    assert_eq!(ts.routed_by.as_deref(), Some(d.id.as_str()));
    let EventBody::DecisionMade(dm) = &d.body else {
        panic!()
    };
    assert_eq!(
        (dm.provider, dm.chosen.as_str()),
        (DecisionProvider::User, "web")
    );
    assert!(hub.attention_list(true).is_empty());
    assert_eq!(hub.thread_view(&api).unwrap().turns, 0);
    hub.shutdown().await;
}

// ---------------------------------------------------------------------------------------------
// Orchestrator lifecycle
// ---------------------------------------------------------------------------------------------

/// When Claude Code's managed policy would keep the memory guard's hook from running (here:
/// `allowManagedHooksOnly` in a managed settings file), the orchestrator gets no built-in tools
/// and no guard settings.
#[tokio::test]
async fn a_policy_that_stops_the_guard_leaves_the_orchestrator_no_file_tools() {
    let mut env = Env::new();
    let managed = env.path().join("claude-managed");
    std::fs::create_dir_all(&managed).unwrap();
    std::fs::write(
        managed.join("managed-settings.json"),
        r#"{"allowManagedHooksOnly": true}"#,
    )
    .unwrap();
    let hub = env.hub().await;
    env.worker(&hub, "api").await;
    env.worker(&hub, "web").await;
    hub.say("figure out why the deploy fails", None, &tui())
        .await
        .unwrap();
    let rt = env.orch().await;
    assert_eq!(rt.cfg.builtin_tools, Some(Vec::new()));
    assert_eq!(rt.cfg.settings, None);
    assert_eq!(rt.cfg.allowed_tools, vec!["mcp__twr"]);
    drop(rt);
    hub.shutdown().await;
}

#[tokio::test]
async fn delegation_spawns_the_orchestrator_with_its_tools_and_a_private_token() {
    let mut env = Env::new();
    let hub = env.hub().await;
    env.worker(&hub, "api").await;
    env.worker(&hub, "web").await;
    assert!(hub.orchestrator_thread().is_none());
    let mut sub = hub.subscribe_control(0);
    let r = hub
        .say("figure out why the deploy fails", None, &tui())
        .await
        .unwrap();
    assert_eq!(r["action"], "delegate");
    assert_eq!(r["thread_name"], "orchestrator");
    let orch_id = hub.orchestrator_thread().unwrap();
    assert_eq!(r["thread"], orch_id.as_str());
    let mut rt = env.orch().await;

    // thread.created: role, name, alias, no project/worktree, runtime and model from config.
    let evs = until(&mut sub, is_kind_on("turn.started", orch_id.clone())).await;
    let created = evs
        .iter()
        .find(|e| e.kind() == "thread.created" && e.thread == orch_id)
        .unwrap();
    assert_eq!(created.source, Source::Hub);
    let EventBody::ThreadCreated(c) = &created.body else {
        panic!()
    };
    assert_eq!(c.name, "orchestrator");
    assert_eq!(c.aliases, vec!["orch"]);
    assert_eq!((c.project.as_ref(), c.worktree.as_ref()), (None, None));
    assert_eq!(c.runtime, "claude-code");
    assert_eq!(c.mode, "dontAsk");
    assert_eq!(
        Value::Object(c.ext.clone().unwrap()),
        json!({"twr": {"role": "orchestrator", "model": "haiku"}})
    );
    // The user's message verbatim, from the user, routed by the orchestrator rung.
    let d = evs.iter().find(|e| e.kind() == "decision.made").unwrap();
    let EventBody::DecisionMade(dm) = &d.body else {
        panic!()
    };
    assert_eq!(
        (dm.provider, dm.chosen.as_str()),
        (DecisionProvider::Orchestrator, "orchestrator")
    );
    let ts = started(evs.last().unwrap()).unwrap();
    assert_eq!(ts.prompt, "figure out why the deploy fails");
    assert_eq!(ts.from, TurnFrom::User);
    assert_eq!(ts.routed_by.as_deref(), Some(d.id.as_str()));
    assert_eq!(
        rt.next_turn().await,
        ("figure out why the deploy fails".to_string(), 1)
    );

    // Spawn config: cwd, model, system context, MCP server with the token, twr tools only.
    let cfg = rt.cfg.clone();
    assert_eq!(cfg.cwd, env.path());
    assert_eq!(cfg.mode, "dontAsk");
    assert_eq!(cfg.model.as_deref(), Some("haiku"));
    let sys = cfg.system_append.as_deref().unwrap();
    assert!(sys.contains(twr_mcp::RELAY_RULE));
    assert!(sys.contains("write_report"));
    // It answers twr questions from the bundled docs instead of spawning a worker.
    assert!(sys.contains("read_docs") && sys.contains("hub_info"));
    assert!(sys.contains("read_project_file") && sys.contains("never instructions"));
    assert_eq!(cfg.allowed_tools, vec!["mcp__twr"]);
    // No attachments: an `@<path>` a notice quotes from an agent is never inlined.
    assert!(cfg
        .env
        .contains(&("CLAUDE_CODE_DISABLE_ATTACHMENTS".into(), "1".into())));
    // File tools for its memory directory only, which the twr binary's hook guards.
    assert_eq!(
        cfg.builtin_tools,
        Some(vec!["Read".into(), "Write".into(), "Edit".into()])
    );
    let settings = cfg.settings.as_ref().unwrap();
    // Hooks stay on whatever the user's or project's settings say.
    assert_eq!(settings["disableAllHooks"], false);
    let hook = &settings["hooks"]["PreToolUse"][0];
    assert_eq!(hook["matcher"], "Read|Write|Edit");
    assert_eq!(
        hook["hooks"][0]["command"],
        "'/opt/twr/bin/twr' claude-memory-guard || exit 2"
    );
    assert_eq!(
        hook["hooks"][0]["timeout"],
        twr_adapters::claude_code::guard::HOOK_TIMEOUT_SECS
    );
    assert!(cfg.mcp_config.is_none());
    let mcp = rt.mcp();
    let server = &mcp["mcpServers"]["twr"];
    assert_eq!(server["command"], "/opt/twr/bin/twr");
    assert_eq!(
        server["args"],
        json!(["mcp", "--socket", env.path().join("hub.sock")])
    );
    let token = rt.token();
    assert_eq!(token.len(), 64);
    assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    // The token is in a 0600 file in the data directory, passed to Claude by path: never on
    // the command line (readable by every local user) nor in the runtime's environment.
    let mcp_file = cfg.mcp_config_file.clone().unwrap();
    assert_eq!(mcp_file.parent(), Some(env.path().join("data").as_path()));
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&mcp_file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let argv = twr_adapters::claude_code::build_args(&cfg, "sess");
    assert!(argv.iter().all(|a| !a.contains(&token)), "{argv:?}");
    let at = argv.iter().position(|a| a == "--mcp-config").unwrap();
    assert_eq!(argv[at + 1], mcp_file.to_string_lossy());
    assert!(cfg.env.iter().all(|(_, v)| !v.contains(&token)));
    assert!(hub.orchestrator_token_is(&token));
    assert!(!hub.orchestrator_token_is("nope"));
    assert!(!hub.orchestrator_token_is(&token[..63]));
    // Never in an event, hub.info or thread.get.
    assert!(!log_json(&hub).contains(&token));
    assert!(!hub.info().to_string().contains(&token));
    assert!(!hub.get_thread("orch").unwrap().to_string().contains(&token));
    assert_eq!(hub.info()["orchestrator"]["thread"], orch_id.as_str());

    // A later delegation reuses the thread.
    rt.end_turn(1, "Looking.");
    // (Orchestrator turns get no report: only the end.)
    until(&mut sub, is_kind_on("turn.ended", orch_id.clone())).await;
    // The requested model and the one the runtime reports, side by side.
    let o = &hub.info()["orchestrator"];
    assert_eq!(
        (&o["model"], &o["model_actual"]),
        (&json!("haiku"), &json!(ORCH_MODEL))
    );
    let t = hub.get_thread("orch").unwrap();
    assert_eq!(
        (&t["model"], &t["model_actual"]),
        (&json!("haiku"), &json!(ORCH_MODEL))
    );
    let r = hub.say("and the api?", None, &tui()).await.unwrap();
    assert_eq!(r["thread"], orch_id.as_str());
    assert_eq!(rt.next_turn().await.1, 2);

    // A new runtime process gets a new token; the old one stops working.
    hub.stop_thread("orch", "tui").unwrap();
    drop(rt);
    until(&mut sub, is_kind_on("thread.stopped", orch_id.clone())).await;
    // Its MCP config file went with it.
    assert!(!mcp_file.exists());
    hub.say("status?", None, &tui()).await.unwrap();
    let rt2 = env.orch().await;
    assert_ne!(rt2.cfg.mcp_config_file.as_ref(), Some(&mcp_file));
    let token2 = rt2.token();
    assert_ne!(token, token2);
    assert!(hub.orchestrator_token_is(&token2));
    assert!(!hub.orchestrator_token_is(&token));
    assert_eq!(rt2.cfg.resume_session.as_deref(), Some("s-orch"));
    drop(rt2);
    hub.shutdown().await;
}

#[tokio::test]
async fn autostart_starts_the_orchestrator_with_the_hub() {
    let mut env = Env::new();
    let hub = env.hub_with(|c| c.orchestrator.autostart = true).await;
    let rt = env.orch().await;
    assert!(rt.first.is_none());
    let orch = hub.orchestrator_thread().unwrap();
    assert_eq!(hub.thread_view(&orch).unwrap().name, "orchestrator");
    assert!(hub.orchestrator_token_is(&rt.token()));
    assert_eq!(hub.live_runtimes(), 1);
    drop(rt);
    hub.shutdown().await;
}

/// `thread.set_model` refuses the orchestrator thread (switching it is `twr orchestrator
/// use`), whoever asks, and records nothing.
#[tokio::test]
async fn set_model_refuses_the_orchestrator() {
    let mut env = Env::new();
    let hub = env.hub_with(|c| c.orchestrator.autostart = true).await;
    let rt = env.orch().await;
    let orch = hub.orchestrator_thread().unwrap();
    let head = hub.control_read(0, 1).unwrap().1;
    for caller in [tui(), Caller::Orchestrator] {
        let e = hub
            .set_model(&orch, Some("opus"), Some("high"), &caller)
            .unwrap_err();
        assert_eq!(e.code, codes::BAD_REQUEST, "{e}");
        assert!(e.message.contains("twr orchestrator use"), "{e}");
    }
    assert_eq!(hub.control_read(0, 1).unwrap().1, head);
    assert_eq!(
        hub.thread_view(&orch).unwrap().model.as_deref(),
        Some("haiku")
    );
    drop(rt);
    hub.shutdown().await;
}

// ---------------------------------------------------------------------------------------------
// Caller identity and the relay guard
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn sends_follow_the_callers_identity() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    env.worker(&hub, "web").await;
    hub.say("plan the release", None, &tui()).await.unwrap();
    let rt = env.orch().await;
    let mut sub = hub.subscribe_control(0);
    hub.send_by("api", "from the orchestrator", &Caller::Orchestrator)
        .unwrap();
    let evs = until(&mut sub, is_kind_on("turn.started", api.clone())).await;
    let e = evs.last().unwrap();
    assert_eq!(e.source, Source::Orchestrator);
    assert_eq!(started(e).unwrap().from, TurnFrom::Orchestrator);
    // The orchestrator cannot prompt itself.
    let e = hub
        .send_by("orch", "loop", &Caller::Orchestrator)
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST);
    drop(rt);
    hub.shutdown().await;
}

#[tokio::test]
async fn the_relay_guard() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    env.worker(&hub, "web").await;
    let mut sub = hub.subscribe_control(0);
    hub.send_by(&api, "!perm Bash rm -rf build", &tui())
        .unwrap();
    let evs = until(&mut sub, |e| e.kind() == "attention.raised").await;
    let p1 = raised(evs.last().unwrap()).unwrap().attn_id.clone();
    let orch = || Caller::Orchestrator;
    let forbidden = |r: Result<Event, twr_core::proto::RpcError>| {
        let e = r.unwrap_err();
        assert_eq!(e.code, codes::RELAY_FORBIDDEN, "{e:?}");
        e.message
    };

    let relay = |attn: &str, answer: Answer| relay(&hub, attn, answer);
    // No orchestrator turn at all.
    forbidden(relay(&p1, Answer::allow()));

    // Turn 1: an unrelated instruction from the user, who had seen p1.
    let (_, seen) = hub.control_read(0, 1).unwrap();
    hub.say_seen("look into the failing deploy", None, Some(seen), &tui())
        .await
        .unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    // No command for it: a proposal for the user's card, nothing applied.
    proposed(relay(&p1, Answer::allow()));
    assert!(hub
        .attention_list(false)
        .iter()
        .any(|i| i["attn_id"] == p1.as_str()));
    // Once per item per turn, proposals included.
    let m = forbidden(relay(&p1, Answer::allow()));
    assert!(m.contains("already answered (or proposed"), "{m}");
    rt.end_turn(t1, "It needs permission to delete build/.");
    let orch_id = hub.orchestrator_thread().unwrap();
    until(&mut sub, is_kind_on("turn.ended", orch_id.clone())).await;

    // Turn 2: the user explicitly approves api (typed straight to the orchestrator; a lone
    // "approve." would go on the card: it may answer the orchestrator's own question).
    hub.send_by("orch", "approve api.", &tui()).unwrap();
    let (_, t2) = rt.next_turn().await;
    assert_eq!(t2, 2);
    // A permission raised after the user's message is stale for it.
    hub.send_by("web", "!perm Bash git push", &tui()).unwrap();
    let evs = until(&mut sub, |e| {
        e.kind() == "attention.raised" && e.thread != HUB_THREAD
    })
    .await;
    let p2 = raised(evs.last().unwrap()).unwrap().attn_id.clone();
    let m = forbidden(relay(&p2, Answer::allow()));
    assert!(m.contains("raised after"), "{m}");
    // Success: recorded as the orchestrator's, forwarded to the runtime.
    let ev = relay(&p1, Answer::allow()).unwrap();
    assert_eq!(ev.source, Source::Orchestrator);
    let EventBody::AttentionResolved(r) = &ev.body else {
        panic!()
    };
    assert_eq!(r.by, ResolvedBy::Orchestrator);
    until(&mut sub, |e| {
        matches!(&e.body, EventBody::ReportReady(_)) && e.thread == api
    })
    .await;
    // Double respond: refused.
    let e = relay(&p1, Answer::allow()).unwrap_err();
    assert!(
        [codes::CONFLICT, codes::RELAY_FORBIDDEN].contains(&e.code.as_str()),
        "{e:?}"
    );
    rt.end_turn(t2, "Approved.");

    // An error produces a notice; it never carries authority for p2's permission.
    raise(
        &hub,
        &api,
        "notice-error",
        AttentionReason::Error,
        &["yes", "no"],
    );
    let (notice, t3) = rt.next_turn().await;
    assert_eq!(t3, 3);
    assert!(notice.starts_with("[twr notice]"), "{notice}");
    assert!(notice.contains("notice-error"), "{notice}");
    let m = forbidden(relay(&p2, Answer::allow()));
    assert!(m.contains("not started by a user message"), "{m}");
    // Items that are not permission/question: annotate only.
    let e = hub
        .write_report(
            "api",
            None,
            "Cleaned the build but tests fail.",
            Grade::GaveUp,
            &orch(),
        )
        .unwrap();
    let review = e["review"].as_str().unwrap().to_string();
    let m = forbidden(relay(&review, Answer::new("dismiss")));
    assert!(m.contains("annotate"), "{m}");
    let ann = hub
        .annotate(&review, "tests fail on a flaky snapshot", &orch())
        .unwrap();
    assert_eq!(ann.source, Source::Orchestrator);
    let item = hub
        .attention_list(true)
        .into_iter()
        .find(|i| i["attn_id"] == review.as_str())
        .unwrap();
    assert_eq!(item["note"], "tests fail on a flaky snapshot");
    // The user can still answer anything.
    hub.respond(&p2, Answer::deny(), "tui").unwrap();
    hub.respond(&review, Answer::new("dismiss"), "tui").unwrap();
    rt.end_turn(t3, "Noted.");
    drop(rt);
    hub.shutdown().await;
}

// ---------------------------------------------------------------------------------------------
// Notices
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn notices_go_out_at_once_when_idle_and_coalesce_when_busy() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    let web = env.worker(&hub, "web").await;
    let mut sub = hub.subscribe_control(0);
    hub.say("watch the builds", None, &tui()).await.unwrap();
    let mut rt = env.orch().await;
    let orch_id = hub.orchestrator_thread().unwrap();
    let (_, t1) = rt.next_turn().await;

    // While the orchestrator is busy: a permission on api, a finished turn on web.
    hub.send_by(&api, "!ask Run clean?|yes|no", &tui()).unwrap();
    hub.send_by(&web, "all good", &tui()).unwrap();
    until(&mut sub, is_kind_on("report.ready", web.clone())).await;
    exceptional_report(&hub, &web);
    raise(&hub, &api, "api-error", AttentionReason::Error, &[]);
    let p1 = hub
        .attention_list(false)
        .into_iter()
        .find(|a| a["reason"] == "question")
        .unwrap()["attn_id"]
        .as_str()
        .unwrap()
        .to_string();
    // Nothing is sent to a busy orchestrator.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(hub.thread_view(&orch_id).unwrap().turns, 1);
    rt.end_turn(t1, "Watching.");

    // One notice turn after it: both updates, compact.
    let (notice, t2) = rt.next_turn().await;
    assert_eq!(t2, 2);
    assert!(notice.starts_with("[twr notice] 2 updates"), "{notice}");
    assert!(
        notice.contains("- thread=api reason=error attn_id=api-error options=[] summary="),
        "{notice}"
    );
    assert!(
        notice.contains("- thread=web report turn=1 grade=partial summary=\"all good"),
        "{notice}"
    );
    // The hub's report names the reply it quotes, which read_transcript reads in full.
    let line = notice
        .lines()
        .find(|l| l.starts_with("- thread=web report"))
        .unwrap();
    assert!(line.ends_with(&format!(" thread_id={web}")), "{line}");
    let reply_seq = reply_seq_of(line);
    let reply = hub.thread_events(&web, reply_seq - 1, 1).unwrap().1;
    assert_eq!(reply[0].seq, reply_seq);
    assert!(
        matches!(&reply[0].body, EventBody::AgentText(a) if a.is_final && a.text == "all good"),
        "{:?}",
        reply[0]
    );
    assert!(
        notice.contains("read_transcript with thread=<thread_id> and seq=<reply_seq>"),
        "{notice}"
    );
    // Every line ends with its thread's id (names are reused once a thread is deleted).
    let line = notice
        .lines()
        .find(|l| l.starts_with("- thread=api reason=error"))
        .unwrap();
    assert!(line.ends_with(&format!(" thread_id={api}")), "{line}");
    // Nothing about the orchestrator's own report of turn 1.
    assert!(!notice.contains("orchestrator"), "{notice}");
    let (items, _) = hub.control_read(0, 10_000).unwrap();
    let ts = items
        .iter()
        .find(|(_, e)| e.thread == orch_id && e.turn == Some(2) && e.kind() == "turn.started")
        .map(|(_, e)| e.clone())
        .unwrap();
    assert_eq!(ts.source, Source::Hub);
    let p = started(&ts).unwrap();
    assert_eq!(p.from, TurnFrom::Orchestrator);
    assert_eq!(p.ext.as_ref().unwrap()["twr"]["notice"], true);
    rt.end_turn(t2, "api wants to run make clean.");

    // Idle: an urgent item goes out at once, on its own.
    hub.respond(&p1, Answer::new("yes"), "tui").unwrap();
    until(&mut sub, is_kind_on("report.ready", api.clone())).await;
    // (api's report is a non-urgent notice; it may ride along or come separately.)
    hub.send_by(&web, "!ask Which db?|postgres|sqlite", &tui())
        .unwrap();
    raise(&hub, &web, "idle-error", AttentionReason::Error, &[]);
    let mut seen = String::new();
    while !seen.contains("reason=error") {
        let (n, t) = rt.next_turn().await;
        assert!(n.starts_with("[twr notice]"), "{n}");
        seen.push_str(&n);
        rt.end_turn(t, "ok");
    }
    assert!(seen.contains("attn_id=idle-error"), "{seen}");
    assert!(hub.thread_events(&orch_id, 0, 1000).unwrap().1.iter().any(|e| matches!(&e.body, EventBody::ConversationUpdate(p) if p.about_thread.as_deref()==Some(&api) && p.text.contains("finished"))));
    drop(rt);
    hub.shutdown().await;
}

#[tokio::test]
async fn background_reports_and_completion_updates_do_not_spam_notice_turns() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let worker = env.worker(&hub, "reviewer").await;
    env.worker(&hub, "web").await;
    let routed = hub.say("watch the review", None, &tui()).await.unwrap();
    assert_eq!(routed["action"], "delegate");
    let mut rt = env.orch().await;
    let orch = hub.orchestrator_thread().unwrap();
    let (_, turn) = rt.next_turn().await;
    let append = |body| hub.append(&worker, None, Source::Hub, body, None).unwrap();
    for id in ["review", "tests"] {
        append(EventBody::TaskUpdated(TaskUpdated {
            task_id: id.into(),
            running: true,
            ..Default::default()
        }));
    }
    // Interim, as the hub writes it for a turn that ended with those tasks running.
    append(EventBody::ReportReady(ReportReady {
        summary: "review is running".into(),
        grade: Grade::Partial,
        ext: Some(serde_json::from_value(json!({"twr": {"background_tasks": 2}})).unwrap()),
        ..Default::default()
    }));
    let updates = || {
        hub.thread_events(&orch, 0, 1000)
            .unwrap()
            .1
            .into_iter()
            .filter_map(|e| match e.body {
                EventBody::ConversationUpdate(p) if p.about_thread.as_deref() == Some(&worker) => {
                    Some(p.text)
                }
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    assert!(updates()
        .iter()
        .any(|text| text.contains("yielded with 2 background")));
    assert!(!updates().iter().any(|text| text.contains("finished")));
    append(EventBody::TaskUpdated(TaskUpdated {
        task_id: "tests".into(),
        running: true,
        heartbeat: true,
        ..Default::default()
    }));
    for id in ["review", "tests", "tests"] {
        append(EventBody::TaskUpdated(TaskUpdated {
            task_id: id.into(),
            running: false,
            ..Default::default()
        }));
    }
    assert_eq!(
        updates()
            .iter()
            .filter(|text| text.contains("background work finished"))
            .count(),
        1
    );
    rt.end_turn(turn, "Watching.");
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(hub.thread_view(&orch).unwrap().turns, 1);
    drop(rt);
    hub.shutdown().await;
}

#[tokio::test]
async fn orchestrator_notice_turns_wait_for_its_own_background_work() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let worker = env.worker(&hub, "api").await;
    env.worker(&hub, "web").await;
    let routed = hub.say("watch the build", None, &tui()).await.unwrap();
    assert_eq!(routed["action"], "delegate");
    let mut rt = env.orch().await;
    let orch = hub.orchestrator_thread().unwrap();
    let (_, turn) = rt.next_turn().await;
    let mut startup = hub.subscribe_control(0);
    until(&mut startup, is_kind_on("thread.started", orch.clone())).await;
    drop(startup);
    hub.append(
        &orch,
        None,
        Source::Hub,
        EventBody::TaskUpdated(TaskUpdated {
            task_id: "review".into(),
            running: true,
            ..Default::default()
        }),
        None,
    )
    .unwrap();
    raise(&hub, &worker, "build-error", AttentionReason::Error, &[]);
    rt.end_turn(turn, "Review is running.");
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        hub.thread_view(&orch).unwrap().status().as_str(),
        "background"
    );
    assert_eq!(hub.thread_view(&orch).unwrap().turns, 1);
    hub.append(
        &orch,
        None,
        Source::Hub,
        EventBody::TaskUpdated(TaskUpdated {
            task_id: "review".into(),
            running: false,
            ..Default::default()
        }),
        None,
    )
    .unwrap();
    let (notice, next) = rt.next_turn().await;
    assert!(notice.contains("build-error"));
    rt.end_turn(next, "Build failed.");
    drop(rt);
    hub.shutdown().await;
}

/// Produce an exceptional report so notice tests still exercise interpretation turns.
fn exceptional_report(hub: &Hub, thread: &str) {
    let events = hub.thread_events(thread, 0, 1000).unwrap().1;
    let ev = events
        .iter()
        .rev()
        .find(|e| e.kind() == "report.ready")
        .unwrap();
    let EventBody::ReportReady(mut report) = ev.body.clone() else {
        unreachable!()
    };
    report.grade = Grade::Partial;
    hub.append(
        thread,
        ev.turn,
        Source::Hub,
        EventBody::ReportReady(report),
        None,
    )
    .unwrap();
}

/// The `reply_seq=<seq>` of a report notice line.
fn reply_seq_of(line: &str) -> u64 {
    line.split(' ')
        .find_map(|w| w.strip_prefix("reply_seq="))
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("no reply_seq: {line}"))
}

/// While the orchestrator is busy with its first turn (returned): worker `web` reports, is
/// deleted (the user confirms), and a new thread takes the name `web` and reports too. Returns
/// (the orchestrator's session, its turn, the old web's id, the new web's id).
async fn report_delete_and_reuse_while_busy(
    env: &mut Env,
    hub: &Hub,
) -> (OrchRt, u64, String, String) {
    env.worker(hub, "api").await;
    let old = env.worker(hub, "web").await;
    let mut sub = hub.subscribe_control(0);
    hub.say("watch the builds", None, &tui()).await.unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    hub.send_by(&old, "old result", &tui()).unwrap();
    until(&mut sub, is_kind_on("report.ready", old.clone())).await;
    exceptional_report(hub, &old);
    let card = hub.delete_threads(&["web".into()], &tui()).await.unwrap();
    hub.respond_at(
        card["attn_id"].as_str().unwrap(),
        Answer::new("Delete"),
        card["revision"].as_u64(),
        &tui(),
    )
    .unwrap();
    until(&mut sub, is_kind_on("thread.deleted", old.clone())).await;
    let new = env.worker(hub, "web").await;
    assert_ne!(new, old);
    hub.send_by(&new, "new result", &tui()).unwrap();
    until(&mut sub, is_kind_on("report.ready", new.clone())).await;
    exceptional_report(hub, &new);
    (rt, t1, old, new)
}

/// What the notice about [`report_delete_and_reuse_while_busy`] says: the new `web`'s report
/// only, by its id (its reply_seq reads its own reply), and that the old one was deleted.
fn check_reuse_notice(hub: &Hub, notice: &str, old: &str, new: &str) {
    assert!(notice.starts_with("[twr notice] 2 updates"), "{notice}");
    assert!(!notice.contains("old result"), "{notice}");
    assert!(
        notice.contains(&format!(
            "- thread=web deleted (its updates queued for you were dropped) thread_id={old}"
        )),
        "{notice}"
    );
    let line = notice
        .lines()
        .find(|l| l.starts_with("- thread=web report"))
        .unwrap_or_else(|| panic!("{notice}"));
    assert!(line.contains("summary=\"new result"), "{line}");
    assert!(line.ends_with(&format!(" thread_id={new}")), "{line}");
    let reply_seq = reply_seq_of(line);
    let reply = hub.thread_events(new, reply_seq - 1, 1).unwrap().1;
    assert!(
        matches!(&reply[0].body, EventBody::AgentText(a) if a.is_final && a.text == "new result"),
        "{:?}",
        reply[0]
    );
    assert!(notice.contains("give the tools"), "{notice}");
}

/// Review (item B/G): a deleted thread's queued notices go with it. Before, a busy
/// orchestrator later got both reports named `web` (the deleted thread's and the new one's),
/// and `read_transcript thread=web seq=<reply_seq>` of the old report read the new thread.
#[tokio::test]
async fn a_deleted_threads_notices_go_and_lines_name_thread_ids() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let (mut rt, t1, old, new) = report_delete_and_reuse_while_busy(&mut env, &hub).await;
    rt.end_turn(t1, "Watching.");
    let (notice, t2) = rt.next_turn().await;
    check_reuse_notice(&hub, &notice, &old, &new);
    rt.end_turn(t2, "ok");
    drop(rt);
    hub.shutdown().await;
}

/// The same after a restart: the queue rebuilt from the log drops the deleted thread's notices.
#[tokio::test]
async fn a_deleted_threads_notices_stay_gone_after_a_restart() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let (rt, _t1, old, new) = report_delete_and_reuse_while_busy(&mut env, &hub).await;
    let orch_id = hub.orchestrator_thread().unwrap();
    let mut sub = hub.subscribe_control(0);
    // (The new web's runtime would keep the hub open.)
    hub.stop_thread(&new, "tui").unwrap();
    until(&mut sub, is_kind_on("thread.stopped", new.clone())).await;
    // The orchestrator's runtime dies (its queue is kept), then the hub.
    drop(rt);
    until(&mut sub, is_kind_on("thread.crashed", orch_id.clone())).await;
    drop(sub);
    drop(hub);
    let hub = env.reopen().await;
    hub.say("@orch what is up?", None, &tui()).await.unwrap();
    let mut rt = env.orch().await;
    let (msg, t) = rt.next_turn().await;
    assert!(msg.ends_with("what is up?"), "{msg}");
    rt.end_turn(t, "Checking.");
    let (notice, t) = rt.next_turn().await;
    check_reuse_notice(&hub, &notice, &old, &new);
    rt.end_turn(t, "ok");
    assert!(hub.thread_view(&orch_id).is_ok());
    drop(rt);
    hub.shutdown().await;
}

/// A worker's notice that arrives while the stopped orchestrator's closing
/// sequence cannot be recorded yet (its runtime exited, `thread.stopped` refused for now)
/// starts no replacement runtime: the old closing sequence would mark the thread stopped
/// with that replacement alive.
#[tokio::test]
async fn a_notice_waits_for_an_exiting_orchestrator_to_close() {
    let mut env = Env::new();
    let hub = env.hub().await;
    env.worker(&hub, "api").await;
    let web = env.worker(&hub, "web").await;
    let mut sub = hub.subscribe_control(0);
    hub.say("watch the builds", None, &tui()).await.unwrap();
    let mut rt = env.orch().await;
    let orch_id = hub.orchestrator_thread().unwrap();
    let (_, t1) = rt.next_turn().await;
    rt.end_turn(t1, "Watching.");
    until(&mut sub, is_kind_on("turn.ended", orch_id.clone())).await;
    // The user stops the idle orchestrator; its closing sequence cannot be written for now.
    let db = db(&env);
    db.execute_batch(
        "CREATE TRIGGER inject BEFORE INSERT ON events WHEN NEW.kind = 'thread.stopped'
         BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    )
    .unwrap();
    hub.stop_thread(&orch_id, "tui").unwrap();
    drop(rt);
    timeout(WAIT, async {
        while hub.info()["persist_retries"].as_u64().unwrap() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the closing sequence was never retried");
    // A worker error must not restart an exiting orchestrator.
    hub.send_by(&web, "!ask Which db?|postgres|sqlite", &tui())
        .unwrap();
    raise(&hub, &web, "closing-error", AttentionReason::Error, &[]);
    until(&mut sub, |e| {
        e.thread == web && raised(e).is_some_and(|p| p.reason == AttentionReason::Error)
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        env.spawns.try_recv().is_err(),
        "a notice started a replacement orchestrator while the old one was closing"
    );
    assert_eq!(hub.thread_view(&orch_id).unwrap().turns, 1);
    // The closing sequence goes through: stopped, and no replacement nor notice turn.
    db.execute_batch("DROP TRIGGER inject").unwrap();
    until(&mut sub, is_kind_on("thread.stopped", orch_id.clone())).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(env.spawns.try_recv().is_err());
    let view = hub.thread_view(&orch_id).unwrap();
    assert!(view.stopped);
    assert_eq!(view.turns, 1);
    hub.shutdown().await;
}

#[tokio::test]
async fn pending_notices_are_rebuilt_from_the_log_after_a_crash() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    env.worker(&hub, "web").await;
    let mut sub = hub.subscribe_control(0);
    hub.say("keep an eye on api", None, &tui()).await.unwrap();
    let mut rt = env.orch().await;
    let orch_id = hub.orchestrator_thread().unwrap();
    let (_, _t1) = rt.next_turn().await;
    // A hub-raised error on api while the orchestrator is busy: queued, not sent.
    hub.append(
        &api,
        None,
        Source::Hub,
        EventBody::AttentionRaised(AttentionRaised {
            attn_id: "e-api".into(),
            reason: AttentionReason::Error,
            summary: "api build broke".into(),
            options: vec!["dismiss".into()],
            ..Default::default()
        }),
        None,
    )
    .unwrap();
    // The orchestrator's runtime dies, then the hub (no graceful shutdown).
    drop(rt);
    until(&mut sub, is_kind_on("thread.crashed", orch_id.clone())).await;
    drop(sub);
    drop(hub);
    let hub = env.reopen().await;
    // A crashed orchestrator is not restarted for notices.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(env.spawns.try_recv().is_err());
    // The user talks to it again; after that turn the rebuilt notice goes out.
    hub.say("@orch what is up?", None, &tui()).await.unwrap();
    let mut rt = env.orch().await;
    let (msg, t) = rt.next_turn().await;
    assert!(msg.ends_with("what is up?"), "{msg}");
    rt.end_turn(t, "Checking.");
    let (notice, _) = rt.next_turn().await;
    assert!(notice.contains("attn_id=e-api"), "{notice}");
    assert!(notice.contains("reason=error"), "{notice}");
    // Never the orchestrator's own crash.
    assert!(!notice.contains("orchestrator crashed"), "{notice}");
    drop(rt);
    hub.shutdown().await;
}

// ---------------------------------------------------------------------------------------------
// report.write
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn written_reports_supersede_the_hubs_and_raise_one_review() {
    let env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    let mut sub = hub.subscribe_control(0);
    // Nothing to report on yet.
    let e = hub
        .write_report("api", None, "x", Grade::Success, &tui())
        .unwrap_err();
    assert_eq!(e.code, codes::CONFLICT);
    hub.send_by(&api, "done with it", &tui()).unwrap();
    until(&mut sub, is_kind_on("report.ready", api.clone())).await;

    let long = format!(
        "Fixed the flaky login test.   It was a race in the session store! Tests pass now. \
         Extra sentence that is cut. {}",
        "x".repeat(700)
    );
    let r = hub
        .write_report(
            "api",
            None,
            &long,
            Grade::NeedsVerification,
            &Caller::Orchestrator,
        )
        .unwrap();
    assert_eq!(r["turn"], 1);
    let summary = r["summary"].as_str().unwrap();
    assert_eq!(
        summary,
        "Fixed the flaky login test. It was a race in the session store! Tests pass now."
    );
    let review = r["review"].as_str().unwrap().to_string();
    let evs = until(&mut sub, |e| e.kind() == "attention.raised").await;
    let n = evs.len();
    let (d, rep, rv) = (&evs[n - 3], &evs[n - 2], &evs[n - 1]);
    let EventBody::DecisionMade(dm) = &d.body else {
        panic!("{:?}", d.kind())
    };
    assert_eq!(
        (dm.site, dm.provider, dm.chosen.as_str(), d.source.clone()),
        (
            DecisionSite::Grade,
            DecisionProvider::Orchestrator,
            "needs_verification",
            Source::Orchestrator
        )
    );
    let EventBody::ReportReady(rr) = &rep.body else {
        panic!()
    };
    assert_eq!(rr.written_by, "orchestrator");
    assert_eq!(rr.turn, Some(1));
    assert_eq!(raised(rv).unwrap().attn_id, review);
    assert_eq!(rv.source, Source::Hub);
    // The latest report of the turn is the thread's report.
    let v = hub.thread_view("api").unwrap();
    assert_eq!(v.last_report.as_ref().unwrap().written_by, "orchestrator");

    // A second gave_up report for the same turn does not raise a second review; a user can
    // write one too.
    let r = hub
        .write_report("api", Some(1), "Gave up.", Grade::GaveUp, &tui())
        .unwrap();
    assert!(r["review"].is_null());
    let v = hub.thread_view("api").unwrap();
    let last = v.last_report.as_ref().unwrap();
    assert_eq!(
        (last.written_by.as_str(), last.grade),
        ("user:tui", Grade::GaveUp)
    );
    let reviews = hub
        .attention_list(true)
        .into_iter()
        .filter(|i| i["reason"] == "review")
        .count();
    assert_eq!(reviews, 1);
    // Bad turns and targets.
    assert_eq!(
        hub.write_report("api", Some(7), "x", Grade::Success, &tui())
            .unwrap_err()
            .code,
        codes::CONFLICT
    );
    assert_eq!(
        hub.write_report("api", None, "   ", Grade::Success, &tui())
            .unwrap_err()
            .code,
        codes::BAD_REQUEST
    );
    hub.shutdown().await;
}

#[tokio::test]
async fn a_hub_review_is_not_duplicated_by_a_written_report() {
    let env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    let mut sub = hub.subscribe_control(0);
    hub.send_by(&api, "!giveup", &tui()).unwrap();
    until(&mut sub, |e| e.kind() == "attention.raised").await;
    let r = hub
        .write_report(
            "api",
            None,
            "It gave up.",
            Grade::GaveUp,
            &Caller::Orchestrator,
        )
        .unwrap();
    assert!(r["review"].is_null());
    assert_eq!(hub.attention_list(true).len(), 1);
    hub.shutdown().await;
}

// ---------------------------------------------------------------------------------------------
// Relay guard: arrival order, restarts, addressing, canonical answers, hub questions
// ---------------------------------------------------------------------------------------------

/// A permission (or question) raised by the hub on `thread`: it stays open across restarts
/// and is resolved without a runtime to forward to.
fn raise(hub: &Hub, thread: &str, attn_id: &str, reason: AttentionReason, options: &[&str]) {
    hub.append(
        thread,
        None,
        Source::Hub,
        EventBody::AttentionRaised(AttentionRaised {
            attn_id: attn_id.into(),
            reason,
            summary: format!("{attn_id}?"),
            options: options.iter().map(|o| o.to_string()).collect(),
            ..Default::default()
        }),
        None,
    )
    .unwrap();
}

fn forbidden(r: Result<Event, twr_core::proto::RpcError>) -> String {
    let e = r.unwrap_err();
    assert_eq!(e.code, codes::RELAY_FORBIDDEN, "{e:?}");
    e.message
}

/// The orchestrator answers `attn_id` with `answer`: the event when applied, an error when
/// refused, and an error with code `needs_confirmation` when it became a proposal.
fn relay(hub: &Hub, attn_id: &str, answer: Answer) -> Result<Event, RpcError> {
    match hub.respond_relay(attn_id, answer, None, &Caller::Orchestrator)? {
        Relay::Applied(ev) => Ok(*ev),
        Relay::NeedsConfirmation(v) => Err(RpcError::new("needs_confirmation", v.to_string())),
    }
}

/// A relay the hub recorded as a proposal for the user's confirm card.
fn proposed(r: Result<Event, RpcError>) {
    let e = r.unwrap_err();
    assert_eq!(e.code, "needs_confirmation", "{e:?}");
}

/// Waits for the confirm card raised when the orchestrator's turn ended, and confirms it as
/// the user.
async fn confirm_card(hub: &Hub, sub: &mut ControlSubscription) {
    let evs = until(sub, |e| {
        raised(e).is_some_and(|r| is_confirm_card(&e.thread, &r.ext))
    })
    .await;
    let card = raised(evs.last().unwrap()).unwrap().attn_id.clone();
    hub.respond(&card, Answer::new("Confirm"), "tui").unwrap();
}

/// The answer an `attention.resolved` for `attn_id` among `evs` records, and who gave it.
fn resolution(evs: &[Event], attn_id: &str) -> (ResolvedBy, Answer) {
    evs.iter()
        .find_map(|e| match &e.body {
            EventBody::AttentionResolved(r) if r.attn_id == attn_id => {
                Some((r.by, r.answer.clone().unwrap()))
            }
            _ => None,
        })
        .unwrap()
}

fn resolved_answer(ev: &Event) -> Answer {
    let EventBody::AttentionResolved(r) = &ev.body else {
        panic!("{:?}", ev.kind())
    };
    assert_eq!(r.by, ResolvedBy::Orchestrator);
    r.answer.clone().unwrap()
}

/// The control gseq of the event with id `id`.
fn gseq_of(hub: &Hub, id: &str) -> u64 {
    let (items, _) = hub.control_read(0, 10_000).unwrap();
    items.iter().find(|(_, e)| e.id == id).unwrap().0
}

#[tokio::test]
async fn a_message_queued_before_a_permission_cannot_answer_it() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    env.worker(&hub, "web").await;
    let mut sub = hub.subscribe_control(0);
    // The orchestrator is busy with turn 1.
    hub.say("watch the builds", None, &tui()).await.unwrap();
    let mut rt = env.orch().await;
    let orch_id = hub.orchestrator_thread().unwrap();
    let (_, t1) = rt.next_turn().await;
    // The user says "approve it" now: it waits in the queue.
    let sent = hub.send_by("orch", "approve it", &tui()).unwrap();
    assert_eq!((sent.turn, sent.pending), (None, 1));
    // Then api asks for a permission.
    hub.send_by(&api, "!perm Bash rm -rf build", &tui())
        .unwrap();
    let evs = until(&mut sub, |e| {
        e.kind() == "attention.raised" && e.thread == api
    })
    .await;
    let raised_ev = evs.last().unwrap().clone();
    let p1 = raised(&raised_ev).unwrap().attn_id.clone();
    // Turn 1 ends; the queued message is dispatched after the item was raised.
    rt.end_turn(t1, "Watching.");
    let (msg, t2) = rt.next_turn().await;
    assert_eq!(t2, 2);
    assert!(msg.ends_with("approve it"), "{msg}");
    let evs = until(&mut sub, |e| {
        e.kind() == "turn.started" && e.thread == orch_id && e.turn == Some(2)
    })
    .await;
    let ts = started(evs.last().unwrap()).unwrap();
    let said_after = ts.ext.as_ref().unwrap()["twr"]["said_after"]
        .as_u64()
        .unwrap();
    assert!(said_after < gseq_of(&hub, &raised_ev.id));
    // It was said before the permission existed: not an answer to it.
    let m = forbidden(relay(&hub, &p1, Answer::allow()));
    assert!(m.contains("raised after"), "{m}");
    hub.respond(&p1, Answer::allow(), "tui").unwrap();
    rt.end_turn(t2, "Waiting for you.");
    drop(rt);
    hub.shutdown().await;
}

#[test]
fn a_restart_during_the_users_turn_ends_its_authority() {
    let mut env = Env::new();
    // The first hub runs on its own tokio runtime, dropped with every task (a hub crash) while
    // the orchestrator's user turn is open.
    let first = tokio::runtime::Runtime::new().unwrap();
    let (rt, orch_id, t2) = first.block_on(async {
        let hub = env.hub().await;
        let api = env.worker(&hub, "api").await;
        env.worker(&hub, "web").await;
        hub.say("watch the builds", None, &tui()).await.unwrap();
        let mut rt = env.orch().await;
        let orch_id = hub.orchestrator_thread().unwrap();
        let (_, t1) = rt.next_turn().await;
        // Raised while the orchestrator is busy, then the user's approval is queued after it.
        raise(
            &hub,
            &api,
            "p1",
            AttentionReason::Permission,
            &["allow", "deny"],
        );
        hub.send_by("orch", "approve", &tui()).unwrap();
        rt.end_turn(t1, "Watching.");
        let (msg, t2) = rt.next_turn().await;
        assert!(msg.ends_with("approve"), "{msg}");
        // The guard lets this turn relay (a different answer is only a proposal).
        proposed(relay(&hub, "p1", Answer::deny()));
        (rt, orch_id, t2)
    });
    drop(first);
    let second = tokio::runtime::Runtime::new().unwrap();
    second.block_on(async {
        let hub = env.reopen().await;
        let v = hub.thread_view(&orch_id).unwrap();
        assert_eq!((v.turns, v.current_turn), (t2, None));
        // Fail-closed: that turn is over, so its message answers nothing any more.
        let m = forbidden(relay(&hub, "p1", Answer::allow()));
        assert!(m.contains("no turn in progress"), "{m}");
        assert_eq!(hub.attention_list(false).len(), 1);
        drop(rt);
        hub.shutdown().await;
    });
}

#[tokio::test]
async fn one_relay_per_turn_unless_the_message_addresses_the_thread() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    let web = env.worker(&hub, "web").await;
    hub.say("watch the builds", None, &tui()).await.unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    raise(
        &hub,
        &api,
        "p1",
        AttentionReason::Permission,
        &["allow", "deny"],
    );
    raise(
        &hub,
        &web,
        "p2",
        AttentionReason::Permission,
        &["allow", "deny"],
    );
    // "yes" addresses no thread while two items wait: it names neither.
    hub.send_by("orch", "yes", &tui()).unwrap();
    rt.end_turn(t1, "Watching.");
    let (msg, t2) = rt.next_turn().await;
    assert!(msg.ends_with("yes"), "{msg}");
    for p in ["p1", "p2"] {
        proposed(relay(&hub, p, Answer::allow()));
    }
    rt.end_turn(t2, "Which one?");
    // Routine permission updates do not start another model turn.
    // The user answers p2 themselves: now one item waits when "yes" arrives. It names
    // nothing and may answer the orchestrator's own question, so it goes on the card.
    hub.respond("p2", Answer::allow(), "tui").unwrap();
    hub.send_by("orch", "yes", &tui()).unwrap();
    let (msg, t3) = rt.next_turn().await;
    assert!(msg.ends_with("yes"), "{msg}");
    proposed(relay(&hub, "p1", Answer::allow()));
    // Items raised after the message are not its answer.
    raise(
        &hub,
        &web,
        "p2b",
        AttentionReason::Permission,
        &["allow", "deny"],
    );
    let m = forbidden(relay(&hub, "p2b", Answer::allow()));
    assert!(m.contains("raised after"), "{m}");
    raise(
        &hub,
        &api,
        "p3",
        AttentionReason::Permission,
        &["allow", "deny"],
    );
    // Naming web: it answers web's items only (the user's prompt goes before notices).
    hub.send_by("orch", "yes @web", &tui()).unwrap();
    rt.end_turn(t3, "Allowed api.");
    let (msg, t4) = rt.next_turn().await;
    assert!(msg.ends_with("yes @web"), "{msg}");
    proposed(relay(&hub, "p3", Answer::allow()));
    relay(&hub, "p2b", Answer::allow()).unwrap();
    rt.end_turn(t4, "Allowed web.");
    drop(rt);
    hub.shutdown().await;
}

/// The user's message names threads as they were called when it arrived: a thread renamed
/// after "allow api" is still the one it answers, and a thread that took a name only later
/// is not named by it.
#[tokio::test]
async fn a_message_names_threads_by_their_names_when_it_arrived() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    env.worker(&hub, "web").await;
    hub.say("watch the builds", None, &tui()).await.unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    raise(
        &hub,
        &api,
        "p1",
        AttentionReason::Permission,
        &["allow", "deny"],
    );
    hub.send_by("orch", "allow api", &tui()).unwrap();
    rt.end_turn(t1, "Watching.");
    let (msg, t2) = rt.next_turn().await;
    assert!(msg.ends_with("allow api"), "{msg}");
    // Renamed after the message arrived: the message still meant it.
    hub.rename_thread(&api, "gateway", &tui()).unwrap();
    let ev = relay(&hub, "p1", Answer::allow()).unwrap();
    assert_eq!(ev.thread, api);
    rt.end_turn(t2, "Allowed api.");
    drop(rt);
    hub.shutdown().await;
}

/// Whether item `attn_id` is still open.
fn open(hub: &Hub, attn_id: &str) -> bool {
    hub.attention_list(false)
        .iter()
        .any(|a| a["attn_id"] == attn_id)
}

/// A permission on each of `threads`, `p<i>` in order.
fn permissions(hub: &Hub, threads: &[&str]) {
    for (i, t) in threads.iter().enumerate() {
        let attn = format!("p{i}");
        raise(
            hub,
            t,
            &attn,
            AttentionReason::Permission,
            &["allow", "deny"],
        );
    }
}

/// The user's direct commands are read against the names threads have when the command
/// arrives, and only apply directly when no thread a word of them could name was made or
/// renamed after the frame the client last drew (`seen_gseq`): the user wrote the command
/// against the names they saw, and the orchestrator makes and renames threads without a card.
/// A word one thread has as its name and another as a name part names neither. A thread made
/// since is never counted nor changed.
#[tokio::test]
async fn direct_commands_name_threads_as_they_arrive() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    let web = env.worker(&hub, "web").await;
    let client = env.worker(&hub, "x-client").await;
    hub.say_to_orchestrator("watch", None, &tui()).unwrap();
    let mut rt = env.orch().await;
    let (_, t) = rt.next_turn().await;
    rt.end_turn(t, "Watching.");
    permissions(&hub, &[&api, &web, &client]);
    let seen = hub.control_read(0, 1000).unwrap().1;
    // Renamed after the frame drawn: the command goes to the orchestrator, nothing applies.
    hub.rename_thread(&web, "frontend", &tui()).unwrap();
    for m in ["allow frontend", "allow web"] {
        let r = hub.say_to_orchestrator(m, Some(seen), &tui()).unwrap();
        assert_ne!(r["action"], "command", "{m}: {r}");
        assert!(open(&hub, "p1"), "{m}");
        let (_, t) = rt.next_turn().await;
        rt.end_turn(t, "ok");
    }
    // A command that names no changed thread still applies.
    let r = hub
        .say_to_orchestrator("allow api", Some(seen), &tui())
        .unwrap();
    assert_eq!(r["action"], "command", "{r}");
    assert!(!open(&hub, "p0") && open(&hub, "p1") && open(&hub, "p2"));
    // Drawn after the rename: both names are web's.
    let seen = hub.control_read(0, 1000).unwrap().1;
    let r = hub
        .say_to_orchestrator("allow frontend", Some(seen), &tui())
        .unwrap();
    assert_eq!(r["action"], "command", "{r}");
    assert!(!open(&hub, "p1") && open(&hub, "p2"));
    let r = hub
        .say_to_orchestrator("set web to plan mode", Some(seen), &tui())
        .unwrap();
    assert_eq!(r["action"], "command", "{r}");
    assert_eq!(hub.thread_view(&web).unwrap().mode, "plan");
    // A thread made after the frame with a name that takes a word of the command (`client`
    // was x-client's name part): the command names no thread.
    let seen = hub.control_read(0, 1000).unwrap().1;
    let late = env.worker(&hub, "client").await;
    let r = hub
        .say_to_orchestrator("allow client", Some(seen), &tui())
        .unwrap();
    assert_ne!(r["action"], "command", "{r}");
    assert!(open(&hub, "p2"));
    let (_, t) = rt.next_turn().await;
    rt.end_turn(t, "ok");
    // "all" leaves it out, and naming it changes nothing.
    let mode = hub.thread_view(&late).unwrap().mode;
    let r = hub
        .say_to_orchestrator("set all to read-only mode", Some(seen), &tui())
        .unwrap();
    assert_eq!(r["action"], "command", "{r}");
    assert_eq!(hub.thread_view(&client).unwrap().mode, "read-only");
    assert_eq!(hub.thread_view(&late).unwrap().mode, mode);
    let r = hub
        .say_to_orchestrator("set @client to plan mode", Some(seen), &tui())
        .unwrap();
    assert_ne!(r["action"], "command", "{r}");
    assert_eq!(hub.thread_view(&late).unwrap().mode, mode);
    drop(rt);
    hub.shutdown().await;
}

/// The relay guard and the `set_mode` gate read the user's message with the names threads had
/// when it arrived, and never apply it directly when a thread a word of it could name was made
/// or renamed after the frame the client last drew: those calls go on the card. A word one
/// thread has as its name and another as a name part names neither.
#[tokio::test]
async fn relays_name_threads_as_the_message_arrived() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    let web = env.worker(&hub, "web").await;
    let part = env.worker(&hub, "x-api-y").await;
    hub.say("watch the builds", None, &tui()).await.unwrap();
    let mut rt = env.orch().await;
    let (_, mut turn) = rt.next_turn().await;
    permissions(&hub, &[&api, &web, &part]);
    let seen = hub.control_read(0, 1000).unwrap().1;
    hub.rename_thread(&web, "frontend", &tui()).unwrap();
    let mut say = async |msg: &str, seen: u64| {
        let r = hub
            .say_seen(&format!("@orch {msg}"), None, Some(seen), &tui())
            .await
            .unwrap();
        assert_eq!(r["action"], "send", "{r}");
        rt.end_turn(turn, "ok");
        let (prompt, t) = rt.next_turn().await;
        assert!(prompt.ends_with(msg), "{prompt}");
        turn = t;
    };
    // Renamed after the frame: the card.
    say("allow frontend", seen).await;
    proposed(relay(&hub, "p1", Answer::allow()));
    say("set frontend to plan mode", seen).await;
    let out = hub.set_mode(&web, "plan", &Caller::Orchestrator).unwrap();
    assert_eq!(out["status"], "needs_confirmation", "{out}");
    // `api` is api's name and x-api-y's name part: neither, directly.
    say("allow api", seen).await;
    proposed(relay(&hub, "p0", Answer::allow()));
    say("allow @x-api-y", seen).await;
    assert_eq!(relay(&hub, "p2", Answer::allow()).unwrap().thread, part);
    // Drawn after the rename: direct.
    let seen = hub.control_read(0, 1000).unwrap().1;
    say("allow frontend", seen).await;
    assert_eq!(relay(&hub, "p1", Answer::allow()).unwrap().thread, web);
    say("set frontend to plan mode", seen).await;
    let out = hub.set_mode(&web, "plan", &Caller::Orchestrator).unwrap();
    assert_ne!(out["status"], "needs_confirmation", "{out}");
    assert_eq!(hub.thread_view(&web).unwrap().mode, "plan");
    rt.end_turn(turn, "done");
    drop(rt);
    hub.shutdown().await;
}

/// Names are one name in any case: no thread takes `API` beside `api`, nor beside a thread
/// once called `api`, by creation or rename (a thread may change the case of its own name).
/// A log from before, with `API` beside a thread once named `api`, makes `api` name neither
/// in a user's command: neither permission is approved, directly or by a relay.
#[tokio::test]
async fn case_variants_of_a_name_are_one_name() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let new = async |name: &str| {
        hub.new_thread(
            NewThread {
                project: env.path().to_string_lossy().into_owned(),
                runtime: Some("fake".into()),
                name: Some(name.into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .map(|v| v["id"].as_str().unwrap().to_string())
    };
    let api = new("api").await.unwrap();
    assert_eq!(new("API").await.unwrap_err().code, codes::CONFLICT);
    hub.rename_thread(&api, "gateway", &tui()).unwrap();
    assert_eq!(new("Api").await.unwrap_err().code, codes::CONFLICT);
    let web = new("web").await.unwrap();
    for taken in ["GATEWAY", "API"] {
        let e = hub.rename_thread(&web, taken, &tui()).unwrap_err();
        assert_eq!(e.code, codes::CONFLICT, "{taken}: {e:?}");
    }
    let out = hub.rename_thread(&api, "Gateway", &tui()).unwrap();
    assert_eq!(out["changed"], true);
    hub.rename_thread(&api, "gateway", &tui()).unwrap();

    // An older log: `API` made beside the thread once named `api`.
    hub.append(
        "t-upper",
        None,
        Source::User("tui".into()),
        EventBody::ThreadCreated(ThreadCreated {
            name: "API".into(),
            box_name: "local".into(),
            runtime: "fake".into(),
            mode: "manual".into(),
            ..Default::default()
        }),
        None,
    )
    .unwrap();
    hub.say_to_orchestrator("watch", None, &tui()).unwrap();
    let mut rt = env.orch().await;
    let (_, t) = rt.next_turn().await;
    rt.end_turn(t, "Watching.");
    raise(
        &hub,
        &api,
        "pa",
        AttentionReason::Permission,
        &["allow", "deny"],
    );
    raise(
        &hub,
        "t-upper",
        "pu",
        AttentionReason::Permission,
        &["allow", "deny"],
    );
    let seen = hub.control_read(0, 1000).unwrap().1;
    for (i, m) in ["allow @api", "allow api"].into_iter().enumerate() {
        let r = hub.say_to_orchestrator(m, Some(seen), &tui()).unwrap();
        assert_ne!(r["action"], "command", "{m}: {r}");
        assert!(open(&hub, "pa") && open(&hub, "pu"), "{m}");
        let (prompt, t) = rt.next_turn().await;
        assert!(prompt.ends_with(m), "{prompt}");
        if i == 0 {
            proposed(relay(&hub, "pu", Answer::allow()));
            proposed(relay(&hub, "pa", Answer::allow()));
            assert!(open(&hub, "pa") && open(&hub, "pu"));
        }
        rt.end_turn(t, "Asked.");
    }
    drop(rt);
    hub.shutdown().await;
}

/// The orchestrator renames worker threads directly (recorded as its change); nobody renames
/// an orchestrator thread.
#[tokio::test]
async fn the_orchestrator_renames_workers_but_nobody_renames_it() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let web = env.worker(&hub, "web").await;
    env.worker(&hub, "api").await;
    hub.say("watch the builds", None, &tui()).await.unwrap();
    let rt = env.orch().await;
    let mut sub = hub.subscribe_control(0);
    let out = hub
        .rename_thread("web", "web-ui", &Caller::Orchestrator)
        .unwrap();
    assert_eq!(
        (&out["thread"], &out["changed"]),
        (&json!(web), &json!(true))
    );
    let evs = until(&mut sub, is_kind_on("thread.renamed", web.clone())).await;
    let ev = evs.last().unwrap();
    assert_eq!(ev.source, Source::Orchestrator);
    let EventBody::ThreadRenamed(p) = &ev.body else {
        panic!()
    };
    assert_eq!(p.by, ChangedBy::Orchestrator);
    for caller in [tui(), Caller::Orchestrator] {
        let e = hub.rename_thread("orch", "boss", &caller).unwrap_err();
        assert_eq!(e.code, codes::BAD_REQUEST, "{e:?}");
        // Nor may a worker take the orchestrator's names.
        let e = hub
            .rename_thread(&web, "orchestrator", &caller)
            .unwrap_err();
        assert_eq!(e.code, codes::CONFLICT, "{e:?}");
    }
    drop(rt);
    hub.shutdown().await;
}

/// Two items wait when an unaddressed "yes" arrives; one of them is raised again afterwards
/// (a new revision of the same open item). The message still named neither: the other one
/// is not its answer, and the revised one is not either (it may ask something else now).
#[tokio::test]
async fn raising_one_of_two_waiting_items_again_does_not_single_out_the_other() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    let web = env.worker(&hub, "web").await;
    hub.say("watch the builds", None, &tui()).await.unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    raise(
        &hub,
        &api,
        "p1",
        AttentionReason::Permission,
        &["allow", "deny"],
    );
    raise(
        &hub,
        &web,
        "p2",
        AttentionReason::Permission,
        &["allow", "deny"],
    );
    hub.send_by("orch", "yes", &tui()).unwrap();
    rt.end_turn(t1, "Watching.");
    let (msg, t2) = rt.next_turn().await;
    assert!(msg.ends_with("yes"), "{msg}");
    // web raises p2 again (same id, still open) after the message arrived.
    raise(
        &hub,
        &web,
        "p2",
        AttentionReason::Permission,
        &["allow", "deny"],
    );
    proposed(relay(&hub, "p1", Answer::allow()));
    let m = forbidden(relay(&hub, "p2", Answer::allow()));
    assert!(m.contains("raised after"), "{m}");
    assert_eq!(hub.attention_list(false).len(), 2);
    rt.end_turn(t2, "Which one?");
    drop(rt);
    hub.shutdown().await;
}

/// A `say` reply answers only the revision of an item the user had seen: an open item raised
/// again after `seen_gseq` is not answered by that message.
#[tokio::test]
async fn say_does_not_answer_an_item_raised_again_after_what_the_user_saw() {
    let env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    raise(
        &hub,
        &api,
        "p1",
        AttentionReason::Permission,
        &["allow", "deny"],
    );
    let (_, seen) = hub.control_read(0, 1).unwrap();
    raise(
        &hub,
        &api,
        "p1",
        AttentionReason::Permission,
        &["allow", "deny"],
    );
    let r = hub.say_seen("yes", None, Some(seen), &tui()).await.unwrap();
    assert_ne!(r["action"], "respond", "{r}");
    assert!(hub
        .attention_list(false)
        .iter()
        .any(|i| i["attn_id"] == "p1"));
    // Once the user had seen the new revision, the same reply answers it.
    let (_, seen) = hub.control_read(0, 1).unwrap();
    let r = hub
        .say_seen("yes", Some(&api), Some(seen), &tui())
        .await
        .unwrap();
    assert_eq!(
        (r["action"].as_str(), r["attn_id"].as_str()),
        (Some("respond"), Some("p1"))
    );
    hub.shutdown().await;
}

#[tokio::test]
async fn the_orchestrator_relays_a_question_canonically() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    env.worker(&hub, "web").await;
    let mut sub = hub.subscribe_control(0);
    hub.say("watch the builds", None, &tui()).await.unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    // A question from api's runtime (forwarded to it once answered).
    hub.send_by(&api, "!ask Which db?|Postgres|SQLite", &tui())
        .unwrap();
    let evs = until(&mut sub, |e| {
        e.kind() == "attention.raised" && e.thread == api
    })
    .await;
    let q = raised(evs.last().unwrap()).unwrap().attn_id.clone();
    hub.send_by("orch", "option 2", &tui()).unwrap();
    rt.end_turn(t1, "Watching.");
    let (_, t2) = rt.next_turn().await;
    // Whatever case or extra fields the orchestrator sends, the runtime gets the user's
    // answer: the exact option label and nothing else. A question is never answered directly:
    // it goes on the card, and Confirm applies the canonical label.
    let mut answer = Answer::new(" sqlite ");
    answer.extra.insert(
        "updatedInput".into(),
        json!({"answers": {"Which db?": "Postgres"}}),
    );
    answer.message = Some("the user picked it".into());
    proposed(relay(&hub, &q, answer));
    rt.end_turn(t2, "It is on a card.");
    confirm_card(&hub, &mut sub).await;
    let evs = until(&mut sub, is_kind_on("report.ready", api.clone())).await;
    let (by, a) = resolution(&evs, &q);
    assert_eq!(by, ResolvedBy::User);
    assert_eq!(a.decision, "SQLite");
    assert!(a.extra.is_empty(), "{a:?}");
    // Nor a message of its own making.
    assert_eq!(a.message, None, "{a:?}");
    drop(rt);
    hub.shutdown().await;
}

#[tokio::test]
async fn the_orchestrator_relays_the_users_answer_to_a_hub_question() {
    let mut env = Env::new();
    let hub = env.hub().await;
    env.worker(&hub, "api").await;
    let web = env.worker(&hub, "web").await;
    let mut sub = hub.subscribe_control(0);
    hub.say("watch the builds", None, &tui()).await.unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    // A hub question (as `say` raises when nobody can route) about a message from the TUI.
    hub.append(
        HUB_THREAD,
        None,
        Source::Hub,
        EventBody::AttentionRaised(AttentionRaised {
            attn_id: "hq".into(),
            reason: AttentionReason::Question,
            summary: "Which thread should get: deploy the thing?".into(),
            options: vec!["api".into(), "web".into(), "new_thread".into()],
            ext: Some(
                json!({"twr": {"pending_message": "deploy the thing", "asked_by": "tui"}})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
            ..Default::default()
        }),
        None,
    )
    .unwrap();
    hub.send_by("orch", "option 2", &tui()).unwrap();
    rt.end_turn(t1, "Watching.");
    let (_, t2) = rt.next_turn().await;
    // A question is never answered directly: the answer goes on the card.
    proposed(relay(&hub, "hq", Answer::new("WEB")));
    rt.end_turn(t2, "It is on a card.");
    confirm_card(&hub, &mut sub).await;
    // The message goes to web as the user who said it, routed by the confirmed answer.
    let evs = until(&mut sub, is_kind_on("turn.started", web.clone())).await;
    assert_eq!(resolution(&evs, "hq").1.decision, "web");
    let e = evs.last().unwrap();
    assert_eq!(e.source, Source::User("tui".into()));
    let ts = started(e).unwrap();
    assert_eq!(
        (ts.prompt.as_str(), ts.from),
        ("deploy the thing", TurnFrom::User)
    );
    let d = evs
        .iter()
        .find(|e| e.kind() == "decision.made" && e.thread == HUB_THREAD)
        .unwrap();
    assert_eq!(d.source, Source::User("tui".into()));
    assert_eq!(ts.routed_by.as_deref(), Some(d.id.as_str()));
    drop(rt);
    hub.shutdown().await;
}

#[tokio::test]
async fn a_hub_question_stays_open_when_its_message_cannot_be_routed() {
    let env = Env::new();
    let no_orch = |c: &mut Config| c.orchestrator.runtime = Some("nope".into());
    let hub = env.hub_with(no_orch).await;
    env.worker(&hub, "api").await;
    let web = env.worker(&hub, "web").await;
    let r = hub.say("deploy the thing", None, &tui()).await.unwrap();
    let attn = r["attn_id"].as_str().unwrap().to_string();
    hub.shutdown().await;
    // The hub is shutting down: the message could not be queued, so the answer is refused
    // before it is recorded (the question does not close with its message lost).
    let e = hub.respond(&attn, Answer::new("web"), "tui").unwrap_err();
    assert_eq!(e.code, codes::CONFLICT, "{e:?}");
    assert_eq!(hub.attention_list(false).len(), 1);
    drop(hub);
    let hub = env.reopen_with(no_orch).await;
    let mut sub = hub.subscribe_control(0);
    hub.respond(&attn, Answer::new("web"), "tui").unwrap();
    let evs = until(&mut sub, is_kind_on("turn.started", web.clone())).await;
    assert_eq!(
        started(evs.last().unwrap()).unwrap().prompt,
        "deploy the thing"
    );
    hub.shutdown().await;
}

#[tokio::test]
async fn the_orchestrator_cannot_choose_a_threads_permission_mode() {
    let env = Env::new();
    let hub = env
        .hub_with(|c| {
            c.threads.default_mode = "manual".into();
            c.default_runtime = "claude-code".into();
        })
        .await;
    let req = |name: &str, mode: Option<&str>| NewThread {
        project: env.repo().to_string_lossy().into_owned(),
        name: Some(name.into()),
        mode: mode.map(str::to_string),
        ..Default::default()
    };
    let e = hub
        .new_thread_by(req("a", Some("bypassPermissions")), &Caller::Orchestrator)
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST);
    assert!(e.message.contains("user's setting"), "{e:?}");
    assert!(hub.resolve("a").is_err());
    // Omitted, or the default itself: the default.
    for (name, mode) in [("b", None), ("c", Some("manual"))] {
        let t = hub
            .new_thread_by(req(name, mode), &Caller::Orchestrator)
            .await
            .unwrap();
        assert_eq!(t["mode"], "manual");
    }
    // The user chooses freely.
    let t = hub
        .new_thread_by(req("d", Some("acceptEdits")), &tui())
        .await
        .unwrap();
    assert_eq!(t["mode"], "acceptEdits");
    hub.shutdown().await;
}

/// Thread modes are canonicalized when a thread is created and when the hub opens
/// (`threads.default_mode`): a known id in any case or with `-` / `_` becomes its canonical
/// spelling, a runtime's own id is kept only spelled exactly and only on that runtime, and
/// anything else is refused (`bad_request`, or the hub does not open).
#[tokio::test]
async fn thread_modes_are_canonical_or_refused() {
    let env = Env::new();
    let e = Hub::open(env.opts(|c| c.threads.default_mode = "yolo-mode".into()))
        .await
        .err()
        .expect("an unknown default mode is refused");
    assert!(format!("{e:#}").contains("threads.default_mode"), "{e:#}");
    let hub = env
        .hub_with(|c| {
            c.threads.default_mode = "Read_Only".into();
            c.default_runtime = "claude-code".into();
        })
        .await;
    let req = |name: &str, runtime: &str, mode: Option<&str>| NewThread {
        project: env.repo().to_string_lossy().into_owned(),
        name: Some(name.into()),
        runtime: (!runtime.is_empty()).then(|| runtime.into()),
        mode: mode.map(str::to_string),
        ..Default::default()
    };
    let t = hub
        .new_thread(req("a", "fake", None), "test")
        .await
        .unwrap();
    assert_eq!(t["mode"], "read-only");
    for (name, given, want) in [
        ("b", "BYPASS_PERMISSIONS", "bypassPermissions"),
        ("c", "accept-edits", "acceptEdits"),
        ("d", "Dont_Ask", "dontAsk"),
        ("e", "MANUAL", "manual"),
    ] {
        let t = hub
            .new_thread(req(name, "fake", Some(given)), "test")
            .await
            .unwrap();
        assert_eq!(t["mode"], want, "{given}");
    }
    // Claude Code: twr's ids and its own; codex's `never` is not one of them, nor are the
    // names ACP agents report, and the error lists only what it takes.
    for bad in [
        "bypass",
        "planning",
        "never",
        "untrusted",
        "dangerously-allow-all",
        "yolo",
        "autoEdit",
    ] {
        let e = hub
            .new_thread(req("x", "claude-code", Some(bad)), "test")
            .await
            .unwrap_err();
        assert_eq!(e.code, codes::BAD_REQUEST, "{bad:?}");
        assert!(e.message.contains("unknown permission mode"), "{e:?}");
        assert!(
            e.message.ends_with(
                "use one of acceptEdits, auto, bypassPermissions, default, dontAsk, manual, \
                 plan, read-only"
            ),
            "{e:?}"
        );
    }
    let e = hub
        .new_thread(req("x", "fake", Some(" ")), "test")
        .await
        .unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST);
    assert!(hub.resolve("x").is_err());
    // The orchestrator's threads (the configured runtime): the default, however spelled.
    let t = hub
        .new_thread_by(req("f", "", Some("READ-ONLY")), &Caller::Orchestrator)
        .await
        .unwrap();
    assert_eq!(t["mode"], "read-only");
    hub.shutdown().await;
}

/// The orchestrator gets the project's runtime (else `default_runtime`) and nothing else: no
/// `pty:<command>` (arbitrary commands), no `fake` (scriptable file writes), no other runtime.
#[tokio::test]
async fn the_orchestrator_cannot_choose_a_threads_runtime() {
    let env = Env::new();
    let project = env.repo().to_string_lossy().into_owned();
    let req = |name: &str, runtime: Option<&str>| NewThread {
        project: project.clone(),
        name: Some(name.into()),
        runtime: runtime.map(str::to_string),
        ..Default::default()
    };
    let hub = env
        .hub_with(|c| c.default_runtime = "claude-code".into())
        .await;
    for (i, runtime) in [
        Some("pty:sh -c 'touch /tmp/pwned'"),
        Some("pty"),
        Some("fake"),
        Some("codex"),
    ]
    .into_iter()
    .enumerate()
    {
        let name = format!("x{i}");
        let e = hub
            .new_thread_by(req(&name, runtime), &Caller::Orchestrator)
            .await
            .unwrap_err();
        assert_eq!(e.code, codes::BAD_REQUEST, "{runtime:?}");
        assert!(e.message.contains("runtime"), "{e:?}");
        assert!(hub.resolve(&name).is_err());
    }
    // The configured runtime, omitted or named.
    for (name, runtime) in [("a", None), ("b", Some("claude-code"))] {
        let t = hub
            .new_thread_by(req(name, runtime), &Caller::Orchestrator)
            .await
            .unwrap();
        assert_eq!(t["runtime"], "claude-code");
    }
    // The user chooses freely.
    let t = hub
        .new_thread_by(req("c", Some("fake")), &tui())
        .await
        .unwrap();
    assert_eq!(t["runtime"], "fake");
    hub.shutdown().await;
    drop(hub);
    // A configured `fake` (or `pty:*`) default is still not the orchestrator's to start.
    for default in ["fake", "pty:bash"] {
        let hub = env
            .reopen_with(move |c| c.default_runtime = default.into())
            .await;
        let e = hub
            .new_thread_by(req("d", None), &Caller::Orchestrator)
            .await
            .unwrap_err();
        assert_eq!(e.code, codes::BAD_REQUEST, "{default}");
        hub.shutdown().await;
        drop(hub);
    }
}

// ---------------------------------------------------------------------------------------------
// Atomic multi-event operations: failure injection with SQLite triggers
// ---------------------------------------------------------------------------------------------

/// A second connection to the hub's database (for triggers that fail the hub's writes).
fn db(env: &Env) -> rusqlite::Connection {
    let c = rusqlite::Connection::open(env.path().join("data").join("twr.db")).unwrap();
    c.busy_timeout(Duration::from_secs(5)).unwrap();
    c
}

/// Every event of `thread`.
fn events_of(hub: &Hub, thread: &str) -> Vec<Event> {
    hub.thread_events(thread, 0, 10_000).unwrap().1
}

fn count(evs: &[Event], pred: impl Fn(&Event) -> bool) -> usize {
    evs.iter().filter(|e| pred(e)).count()
}

fn is_review(e: &Event) -> bool {
    raised(e).is_some_and(|p| p.reason == AttentionReason::Review)
}

fn is_grade(e: &Event) -> bool {
    matches!(&e.body, EventBody::DecisionMade(d) if d.site == DecisionSite::Grade)
}

#[tokio::test]
async fn a_hub_answer_is_recorded_with_its_route_or_not_at_all() {
    let env = Env::new();
    let no_orch = |c: &mut Config| c.orchestrator.runtime = Some("nope".into());
    let hub = env.hub_with(no_orch).await;
    env.worker(&hub, "api").await;
    let web = env.worker(&hub, "web").await;
    let r = hub.say("deploy the thing", None, &tui()).await.unwrap();
    let attn = r["attn_id"].as_str().unwrap().to_string();
    let db = db(&env);
    let head = hub.info()["control_head"].as_u64().unwrap();
    // Fail each part of the answer in turn: the resolution, the routing decision, the queued
    // message. Each time nothing is recorded and the question stays open.
    for trigger in [
        "CREATE TRIGGER inject BEFORE INSERT ON events WHEN NEW.kind = 'attention.resolved'
         BEGIN SELECT RAISE(ABORT, 'injected'); END;",
        "CREATE TRIGGER inject BEFORE INSERT ON events WHEN NEW.kind = 'decision.made'
         BEGIN SELECT RAISE(ABORT, 'injected'); END;",
        "CREATE TRIGGER inject BEFORE INSERT ON pending_prompts
         BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    ] {
        db.execute_batch(trigger).unwrap();
        let e = hub.respond(&attn, Answer::new("web"), "tui").unwrap_err();
        assert!(e.message.contains("injected"), "{e:?}");
        db.execute_batch("DROP TRIGGER inject").unwrap();
        assert_eq!(hub.attention_list(false).len(), 1, "{trigger}");
        assert_eq!(hub.info()["control_head"].as_u64().unwrap(), head);
        assert_eq!(hub.get_thread(&web).unwrap()["pending"], 0);
        let hub_evs = hub_events(&hub);
        assert_eq!(count(&hub_evs, |e| e.kind() == "decision.made"), 0);
        assert_eq!(count(&hub_evs, |e| e.kind() == "attention.resolved"), 0);
    }
    // All of it is recorded: the message stays queued (dispatch is blocked) and survives a
    // restart, which dispatches it with the logged decision as routed_by.
    db.execute_batch(
        "CREATE TRIGGER block_dispatch BEFORE DELETE ON pending_prompts
         BEGIN SELECT RAISE(ABORT, 'dispatch blocked'); END;",
    )
    .unwrap();
    hub.respond(&attn, Answer::new("web"), "tui").unwrap();
    assert!(hub.attention_list(true).is_empty());
    assert_eq!(hub.get_thread(&web).unwrap()["pending"], 1);
    let hub_evs = hub_events(&hub);
    let decisions: Vec<&Event> = hub_evs
        .iter()
        .filter(|e| e.kind() == "decision.made")
        .collect();
    assert_eq!(decisions.len(), 1);
    let decision = decisions[0].id.clone();
    hub.shutdown().await;
    drop(hub);
    db.execute_batch("DROP TRIGGER block_dispatch").unwrap();
    let hub = env.reopen_with(no_orch).await;
    let mut sub = hub.subscribe_control(0);
    let evs = until(&mut sub, is_kind_on("turn.started", web.clone())).await;
    let ts = started(evs.last().unwrap()).unwrap();
    assert_eq!(ts.prompt, "deploy the thing");
    assert_eq!(ts.routed_by.as_deref(), Some(decision.as_str()));
    assert!(hub.attention_list(true).is_empty());
    hub.shutdown().await;
}

#[tokio::test]
async fn a_written_report_is_recorded_with_its_review_or_not_at_all() {
    let env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    let mut sub = hub.subscribe_control(0);
    hub.send_by(&api, "done with it", &tui()).unwrap();
    until(&mut sub, is_kind_on("report.ready", api.clone())).await;
    let before = events_of(&hub, &api).len();
    let db = db(&env);
    for kind in ["attention.raised", "report.ready", "decision.made"] {
        db.execute_batch(&format!(
            "CREATE TRIGGER inject BEFORE INSERT ON events WHEN NEW.kind = '{kind}'
             BEGIN SELECT RAISE(ABORT, 'injected'); END;"
        ))
        .unwrap();
        let e = hub
            .write_report(
                "api",
                None,
                "Check the migration.",
                Grade::NeedsVerification,
                &Caller::Orchestrator,
            )
            .unwrap_err();
        assert!(e.message.contains("injected"), "{kind}: {e:?}");
        db.execute_batch("DROP TRIGGER inject").unwrap();
        assert_eq!(events_of(&hub, &api).len(), before, "{kind}");
        assert!(hub.attention_list(true).is_empty(), "{kind}");
    }
    let r = hub
        .write_report(
            "api",
            None,
            "Check the migration.",
            Grade::NeedsVerification,
            &Caller::Orchestrator,
        )
        .unwrap();
    assert!(r["review"].is_string());
    let evs = events_of(&hub, &api);
    assert_eq!(count(&evs, is_review), 1);
    assert_eq!(count(&evs, |e| e.kind() == "report.ready"), 2);
    hub.shutdown().await;
}

#[tokio::test]
async fn the_hubs_turn_reactions_are_recorded_together() {
    let env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    let db = db(&env);
    // The review of a gave_up turn cannot be recorded: neither can its grade or report.
    db.execute_batch(
        "CREATE TRIGGER inject BEFORE INSERT ON events WHEN NEW.kind = 'attention.raised'
         BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    )
    .unwrap();
    let mut sub = hub.subscribe_thread(&api, 0).unwrap();
    hub.send_by(&api, "!giveup", &tui()).unwrap();
    timeout(WAIT, async {
        while hub.info()["persist_retries"].as_u64().unwrap() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the hub never retried the failed reactions");
    let evs = events_of(&hub, &api);
    assert_eq!(count(&evs, |e| e.kind() == "turn.ended"), 1);
    assert_eq!(count(&evs, |e| e.kind() == "report.ready"), 0);
    assert_eq!(count(&evs, is_grade), 0);
    db.execute_batch("DROP TRIGGER inject").unwrap();
    timeout(WAIT, async {
        loop {
            let ev = sub.next().await.unwrap().expect("hub stopped");
            if is_review(&ev) {
                return;
            }
        }
    })
    .await
    .expect("no review");
    let evs = events_of(&hub, &api);
    assert_eq!(count(&evs, |e| e.kind() == "report.ready"), 1);
    assert_eq!(count(&evs, is_review), 1);
    assert!(count(&evs, is_grade) >= 1);
    assert!(hub.fatal_error().is_none());
    hub.shutdown().await;
}

/// A report as a hub from before written reports were one batch left it: recorded, without
/// its review.
fn log_report(hub: &Hub, thread: &str, grade: Grade, by: &str, summary: &str) {
    let source = if by == "orchestrator" {
        Source::Orchestrator
    } else {
        Source::User("tui".into())
    };
    hub.append(
        thread,
        Some(1),
        source,
        EventBody::ReportReady(ReportReady {
            summary: summary.into(),
            grade,
            turn: Some(1),
            written_by: by.into(),
            ..Default::default()
        }),
        None,
    )
    .unwrap();
}

#[tokio::test]
async fn restart_restores_a_missing_review_from_the_latest_logged_report() {
    let env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    let web = env.worker(&hub, "web").await;
    let mut sub = hub.subscribe_control(0);
    for t in [&api, &web] {
        hub.send_by(t, "done with it", &tui()).unwrap();
        until(&mut sub, is_kind_on("report.ready", t.clone())).await;
    }
    // The hub graded both turns a success (no review). api's latest report says
    // needs_verification; web's said so too, but a later one says success.
    log_report(
        &hub,
        &api,
        Grade::NeedsVerification,
        "orchestrator",
        "Check the migration.",
    );
    log_report(&hub, &web, Grade::NeedsVerification, "orchestrator", "Hmm.");
    log_report(&hub, &web, Grade::Success, "user:tui", "Verified it.");
    assert!(hub.attention_list(true).is_empty());
    hub.shutdown().await;
    drop((sub, hub));

    for _ in 0..2 {
        let hub = env.reopen().await;
        // Exactly one review, from the logged report (not a regraded turn), across restarts.
        let evs = events_of(&hub, &api);
        let reviews: Vec<&AttentionRaised> = evs.iter().filter_map(raised).collect();
        assert_eq!(reviews.len(), 1, "{:?}", reviews);
        let review = reviews[0];
        assert_eq!(review.reason, AttentionReason::Review);
        assert_eq!(
            review.summary,
            "api: turn 1 needs_verification: Check the migration."
        );
        assert_eq!(
            review.ext.as_ref().unwrap()["twr"]["report_by"],
            "orchestrator"
        );
        assert_eq!(count(&events_of(&hub, &web), is_review), 0);
        let open: Vec<Value> = hub.attention_list(true);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0]["thread"], json!(api));
        hub.shutdown().await;
        drop(hub);
    }
}

/// A deny-only item (a Codex grant twr cannot recognize or show in full): nothing turns into
/// `allow`. The user's `respond` refuses it; `say` with a "yes" answers nothing (and "no"
/// denies); the orchestrator cannot relay an allow, only the user's explicit deny.
#[tokio::test]
async fn a_deny_only_permission_is_never_allowed() {
    let env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    raise(&hub, &api, "p1", AttentionReason::Permission, &["deny"]);
    let e = hub.respond("p1", Answer::allow(), "tui").unwrap_err();
    assert_eq!(e.code, codes::BAD_REQUEST, "{e:?}");
    assert!(e.message.contains("not one of"), "{e:?}");
    let (_, seen) = hub.control_read(0, 1).unwrap();
    for yes in ["yes", "approve", "allow", "go ahead"] {
        let r = hub.say_seen(yes, None, Some(seen), &tui()).await;
        if let Ok(r) = &r {
            assert_ne!(r["answer"], "allow", "{yes}: {r}");
            assert_ne!(r["action"], "respond", "{yes}: {r}");
        }
        assert!(
            hub.attention_list(false)
                .iter()
                .any(|i| i["attn_id"] == "p1"),
            "{yes}: p1 was answered"
        );
    }
    let r = hub
        .say_seen("no", Some("api"), Some(seen), &tui())
        .await
        .unwrap();
    assert_eq!(
        (r["action"].as_str(), r["answer"].as_str()),
        (Some("respond"), Some("deny")),
        "{r}"
    );
    hub.shutdown().await;
}

#[tokio::test]
async fn the_orchestrator_cannot_relay_an_allow_to_a_deny_only_permission() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    env.worker(&hub, "web").await;
    hub.say("watch the builds", None, &tui()).await.unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    raise(&hub, &api, "p1", AttentionReason::Permission, &["deny"]);
    let (_, head) = hub.control_read(0, 1).unwrap();
    hub.say_seen("@orch approve", None, Some(head), &tui())
        .await
        .unwrap();
    rt.end_turn(t1, "ok");
    let (msg, t2) = rt.next_turn().await;
    assert!(msg.ends_with("approve"), "{msg}");
    let e = relay(&hub, "p1", Answer::allow()).unwrap_err();
    assert!(
        [codes::BAD_REQUEST, codes::RELAY_FORBIDDEN].contains(&e.code.as_str()),
        "{e:?}"
    );
    // "approve" does not answer a deny-only item with deny either: only a proposal.
    proposed(relay(&hub, "p1", Answer::deny()));
    rt.end_turn(t2, "It can only be denied.");
    hub.say_seen("@orch deny api", None, Some(head), &tui())
        .await
        .unwrap();
    let (msg, t3) = rt.next_turn().await;
    assert!(msg.ends_with("deny api"), "{msg}");
    let ev = relay(&hub, "p1", Answer::deny()).unwrap();
    assert_eq!(resolved_answer(&ev).decision, "deny");
    rt.end_turn(t3, "Denied.");
    drop(rt);
    hub.shutdown().await;
}

#[tokio::test]
async fn routine_updates_and_exact_commands_need_no_model_turn() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    let web = env.worker(&hub, "web").await;
    hub.say_to_orchestrator("watch", None, &tui()).unwrap();
    let mut rt = env.orch().await;
    let (_, t) = rt.next_turn().await;
    let orch = hub.orchestrator_thread().unwrap();
    rt.end_turn(t, "Watching.");
    let mut sub = hub.subscribe_control(0);
    until(&mut sub, is_kind_on("turn.ended", orch.clone())).await;
    hub.send_by(&api, "!perm Bash make clean", &tui()).unwrap();
    hub.send_by(&web, "all good", &tui()).unwrap();
    until(&mut sub, is_kind_on("report.ready", web.clone())).await;
    assert!(hub.attention_list(false).iter().any(|a| a["thread"] == api));
    let updates = hub.thread_events(&orch, 0, 1000).unwrap().1;
    assert!(updates.iter().any(|e| matches!(&e.body, EventBody::ConversationUpdate(p) if p.about_thread.as_deref()==Some(&api) && p.text.contains("permission"))));
    assert!(updates.iter().any(|e| matches!(&e.body, EventBody::ConversationUpdate(p) if p.about_thread.as_deref()==Some(&web) && p.text.contains("all good"))));
    assert_eq!(hub.thread_view(&orch).unwrap().turns, 1);
    let seen = hub.control_read(0, 1000).unwrap().1;
    let result = hub
        .say_to_orchestrator("allow api and set web to auto mode", Some(seen), &tui())
        .unwrap();
    assert_eq!(result["action"], "command");
    assert_eq!(hub.thread_view(&web).unwrap().mode, "auto");
    assert_eq!(hub.thread_view(&orch).unwrap().turns, 1);
    assert!(hub.attention_list(false).iter().all(|a| a["thread"] != api));
    assert!(hub.thread_events(&orch, 0, 1000).unwrap().1.iter().any(|e|
        matches!(&e.body, EventBody::ConversationUpdate(p) if p.user_message.as_deref()==Some("allow api and set web to auto mode"))));
    drop(rt);
}

#[tokio::test]
async fn exact_commands_fail_closed_on_revisions_and_conflicts() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    raise(
        &hub,
        &api,
        "permission",
        AttentionReason::Permission,
        &["allow", "deny"],
    );
    let seen = hub.control_read(0, 1000).unwrap().1;
    raise(
        &hub,
        &api,
        "permission",
        AttentionReason::Permission,
        &["allow", "deny"],
    );
    let out = hub
        .say_to_orchestrator("allow api", Some(seen), &tui())
        .unwrap();
    assert_ne!(out["action"], "command");
    assert!(hub
        .attention_list(false)
        .iter()
        .any(|a| a["attn_id"] == "permission"));
    let mut rt = env.orch().await;
    let (_, t) = rt.next_turn().await;
    rt.end_turn(t, "The request changed.");
    let seen = hub.control_read(0, 1000).unwrap().1;
    let out = hub
        .say_to_orchestrator("allow api, deny api", Some(seen), &tui())
        .unwrap();
    assert_ne!(out["action"], "command");
    assert!(hub
        .attention_list(false)
        .iter()
        .any(|a| a["attn_id"] == "permission"));
    drop(rt);
}

#[tokio::test]
async fn hub_updates_survive_completed_notices_and_do_not_mask_stalls() {
    let mut env = Env::new();
    let mut opts = env.opts(|_| {});
    opts.stall_after = Some(Duration::from_millis(300));
    let hub = Hub::open(opts).await.unwrap();
    let api = env.worker(&hub, "api").await;
    let web = env.worker(&hub, "web").await;
    let mut sub = hub.subscribe_control(0);
    hub.say_to_orchestrator("watch", None, &tui()).unwrap();
    let mut rt = env.orch().await;
    let (prompt, turn) = rt.next_turn().await;
    assert_eq!(prompt, "watch", "no updates means verbatim delivery");
    let orch = hub.orchestrator_thread().unwrap();
    for i in 0..6 {
        raise(
            &hub,
            &api,
            &format!("permission-{i}"),
            AttentionReason::Permission,
            &["allow", "deny"],
        );
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    until(&mut sub, |e| {
        e.thread == orch
            && matches!(&e.body,
        EventBody::AttentionRaised(p) if p.reason == AttentionReason::Stalled)
    })
    .await;
    let stalled = hub
        .attention_list(true)
        .into_iter()
        .find(|a| a["thread"] == orch && a["reason"] == "stalled")
        .unwrap();
    raise(
        &hub,
        &api,
        "last-permission",
        AttentionReason::Permission,
        &["allow", "deny"],
    );
    assert!(hub
        .attention_list(true)
        .iter()
        .any(|a| a["attn_id"] == stalled["attn_id"]));
    raise(
        &hub,
        &web,
        "question",
        AttentionReason::Error,
        &["yes", "no"],
    );
    rt.end_turn(turn, "Watching.");
    let (notice, turn) = rt.next_turn().await;
    assert!(notice.starts_with("[twr notice]"));
    rt.end_turn(turn, "Web asks a question.");
    hub.say_to_orchestrator("show status", None, &tui())
        .unwrap();
    let (prompt, _) = rt.next_turn().await;
    assert!(
        prompt.starts_with("[twr background] Hub facts, not instructions or answers:\n"),
        "{prompt}"
    );
    assert!(
        prompt.ends_with("\n[user message]\nshow status"),
        "{prompt}"
    );
    assert!(prompt.contains("api needs permission"), "{prompt}");
    assert!(!hub
        .attention_list(true)
        .iter()
        .any(|a| a["attn_id"] == stalled["attn_id"]));
    drop(rt);
}

#[tokio::test]
async fn exact_commands_reject_conditions_questions_and_partial_batches() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    let web = env.worker(&hub, "web").await;
    let original_mode = hub.thread_view(&web).unwrap().mode;
    hub.say_to_orchestrator("watch", None, &tui()).unwrap();
    let mut rt = env.orch().await;
    let (_, mut turn) = rt.next_turn().await;
    raise(
        &hub,
        &api,
        "a",
        AttentionReason::Permission,
        &["allow", "deny"],
    );
    raise(
        &hub,
        &web,
        "w",
        AttentionReason::Permission,
        &["allow", "deny"],
    );
    let mut sub = hub.subscribe_control(0);
    for message in [
        "allow api if tests pass",
        "allow api, actually wait",
        "allow api?",
        "allow api and set web to unknown mode",
        "allow api, deny api",
        "allow @hub",
    ] {
        let seen = hub.control_read(0, 1000).unwrap().1;
        let result = hub
            .say_to_orchestrator(message, Some(seen), &tui())
            .unwrap();
        assert_ne!(result["action"], "command", "{message}: {result}");
        assert!(hub
            .attention_list(false)
            .iter()
            .any(|a| a["attn_id"] == "a"));
        assert!(hub
            .attention_list(false)
            .iter()
            .any(|a| a["attn_id"] == "w"));
        assert_eq!(hub.thread_view(&web).unwrap().mode, original_mode);
        // A newer message queues without interrupting an ordinary user turn.
        assert_eq!(
            hub.thread_view(&hub.orchestrator_thread().unwrap())
                .unwrap()
                .current_turn,
            Some(turn)
        );
        rt.end_turn(turn, "Please clarify.");
        until(&mut sub, |e| {
            e.kind() == "turn.ended"
                && e.turn == Some(turn)
                && e.thread == hub.orchestrator_thread().unwrap()
        })
        .await;
        let (prompt, next) = rt.next_turn().await;
        assert!(
            prompt.ends_with(&format!("\n[user message]\n{message}")) || prompt == message,
            "{prompt}"
        );
        turn = next;
    }
    raise(
        &hub,
        &api,
        "question",
        AttentionReason::Question,
        &["yes", "no"],
    );
    let seen = hub.control_read(0, 1000).unwrap().1;
    let result = hub
        .say_to_orchestrator("allow api", Some(seen), &tui())
        .unwrap();
    assert_ne!(result["action"], "command");
    assert!(hub
        .attention_list(false)
        .iter()
        .any(|a| a["attn_id"] == "question"));
    drop(rt);
}

#[tokio::test]
async fn new_user_message_interrupts_notice_without_replacing_session() {
    let mut env = Env::new();
    let hub = env
        .hub_with(|c| c.orchestrator.effort = Some("high".into()))
        .await;
    let api = env.worker(&hub, "api").await;
    hub.say_to_orchestrator("watch", None, &tui()).unwrap();
    let mut rt = env.orch().await;
    assert!(rt.cfg.refresh_system_prompt);
    assert_eq!(rt.cfg.effort.as_deref(), Some("high"));
    let (_, t) = rt.next_turn().await;
    for i in 0..40 {
        raise(
            &hub,
            &api,
            &format!("question-{i}-{}", "q".repeat(80)),
            AttentionReason::Error,
            &["yes", "no"],
        );
    }
    rt.end_turn(t, "Watching.");
    let (notice, t2) = rt.next_turn().await;
    assert!(notice.starts_with("[twr notice]"));
    assert!(notice.len() > 4096);
    let orch = hub.orchestrator_thread().unwrap();
    let notice_event = hub
        .thread_events(&orch, 0, 1000)
        .unwrap()
        .1
        .into_iter()
        .find(|e| e.kind() == "turn.started" && e.turn == Some(t2))
        .unwrap();
    let mut data = notice_event.body.data().unwrap();
    twr_core::blob::hydrate_with(&mut data, &mut |r| hub.blob(r), false).unwrap();
    assert_eq!(
        data["prompt"], notice,
        "the complete interrupted notice stays durable"
    );
    hub.say_to_orchestrator("show current status", None, &tui())
        .unwrap();
    let cmd = timeout(WAIT, rt.end.cmds.recv()).await.unwrap().unwrap();
    assert_eq!(cmd, AdapterCmd::Interrupt);
    // A burst of newer facts must not crowd out the complete notice reference.
    for _ in 0..40 {
        hub.append(
            &api,
            None,
            Source::Hub,
            EventBody::ReportReady(ReportReady {
                summary: "newer worker fact ".repeat(80),
                grade: Grade::Success,
                written_by: "hub".into(),
                ..Default::default()
            }),
            None,
        )
        .unwrap();
    }
    rt.end.emit(
        EventBody::TurnEnded(TurnEnded {
            result: TurnResult::Interrupted,
            ..Default::default()
        }),
        Some(t2),
    );
    let (user, t3) = rt.next_turn().await;
    assert_eq!(t3, t2 + 1);
    assert!(user.contains("show current status"));
    assert!(user.contains("reason=error"));
    assert!(
        user.contains(&format!(
            "read_transcript thread={orch} seq={}",
            notice_event.seq
        )),
        "{user}"
    );
    assert!(user.len() < 4300, "background facts stay bounded");
    assert!(env.spawns.try_recv().is_err());
    let events = hub
        .thread_events(&hub.orchestrator_thread().unwrap(), 0, 1000)
        .unwrap()
        .1;
    let turn = events
        .iter()
        .find(|e| e.turn == Some(t3) && e.kind() == "turn.started")
        .unwrap();
    assert!(
        turn.body.data().unwrap()["ext"]["twr"]["prompt_fingerprint"]
            .as_str()
            .is_some()
    );
    drop(rt);
}

#[tokio::test]
async fn spawn_operations_survive_retries_and_restart_without_resending() {
    let env = Env::new();
    let hub = env.hub().await;
    let request = NewThread {
        project: env.path().to_string_lossy().into_owned(),
        runtime: Some("fake".into()),
        name: Some("durable".into()),
        ..Default::default()
    };
    let first = hub
        .begin_spawn(
            "retry-1",
            request.clone(),
            Some("!sleep 50\ndone".into()),
            &tui(),
        )
        .unwrap();
    assert_eq!(first["operation_id"], "retry-1");
    hub.begin_spawn(
        "retry-1",
        request.clone(),
        Some("!sleep 50\ndone".into()),
        &tui(),
    )
    .unwrap();
    let status = timeout(WAIT, async {
        loop {
            let status = hub.spawn_status("retry-1", &tui()).unwrap();
            if status["status"] != "pending" {
                break status;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(status["status"], "complete", "{status}");
    let id = status["thread"]["id"].as_str().unwrap().to_string();
    let mut sub = hub.subscribe_control(0);
    until(&mut sub, is_kind_on("turn.ended", id.clone())).await;
    assert_eq!(hub.thread_view(&id).unwrap().turns, 1);
    hub.set_mode(&id, "plan", &tui()).unwrap();
    assert_eq!(
        hub.spawn_status("retry-1", &tui()).unwrap()["thread"]["mode"],
        "plan"
    );
    let db = rusqlite::Connection::open(env.path().join("data/twr.db")).unwrap();
    let bytes: i64 = db
        .query_row(
            "SELECT length(request) FROM spawn_operations WHERE operation_id='retry-1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        bytes < 100,
        "the retry key retains a digest, not the prompt"
    );
    drop(db);
    assert_eq!(
        hub.begin_spawn("retry-1", request.clone(), Some("different".into()), &tui())
            .unwrap_err()
            .code,
        codes::CONFLICT
    );
    hub.shutdown().await;
    drop(sub);
    drop(hub);
    // Simulate a crash after thread/prompt commit but before the operation result was stored.
    let conn = rusqlite::Connection::open(env.path().join("data/twr.db")).unwrap();
    conn.execute(
        "UPDATE spawn_operations SET result=NULL WHERE operation_id='retry-1'",
        [],
    )
    .unwrap();
    conn.execute("INSERT INTO spawn_operations(operation_id, owner, request, created) VALUES ('abandoned','user','{}','now')", []).unwrap();
    drop(conn);
    let hub = env.reopen().await;
    let status = hub
        .begin_spawn("retry-1", request, Some("!sleep 50\ndone".into()), &tui())
        .unwrap();
    assert_eq!(status["status"], "complete");
    assert_eq!(status["thread"]["id"], id);
    assert_eq!(status["recovered"], true);
    assert_eq!(
        hub.spawn_status("abandoned", &tui()).unwrap()["status"],
        "error"
    );
    assert_eq!(hub.thread_view(&id).unwrap().turns, 1);
}

/// A spawn without a name gets one from its first prompt (made unique with `-<n>`), and one
/// without a prompt the project's `<slug>-<n>`.
#[tokio::test]
async fn unnamed_spawns_are_named_after_their_first_prompt() {
    let env = Env::new();
    let hub = env.hub().await;
    let request = NewThread {
        project: env.path().to_string_lossy().into_owned(),
        runtime: Some("fake".into()),
        ..Default::default()
    };
    let spawn = |op: &'static str, prompt: Option<&'static str>| {
        let hub = hub.clone();
        let request = request.clone();
        async move {
            hub.begin_spawn(op, request, prompt.map(str::to_string), &tui())
                .unwrap();
            timeout(WAIT, async {
                loop {
                    let status = hub.spawn_status(op, &tui()).unwrap();
                    if status["status"] != "pending" {
                        assert_eq!(status["status"], "complete", "{status}");
                        break status["thread"]["name"].as_str().unwrap().to_string();
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap()
        }
    };
    let prompt = "Design the notifications system\ndone";
    assert_eq!(
        spawn("a", Some(prompt)).await,
        "design-notifications-system"
    );
    assert_eq!(
        spawn("b", Some(prompt)).await,
        "design-notifications-system-2"
    );
    let unnamed = spawn("c", None).await;
    assert!(
        unnamed.ends_with("-1") && !unnamed.starts_with("design"),
        "{unnamed}"
    );
    hub.shutdown().await;
}

#[tokio::test]
async fn transcript_pages_filter_deltas_and_scope_historical_turns() {
    let env = Env::new();
    let hub = env.hub().await;
    let id = env.worker(&hub, "api").await;
    let mut sub = hub.subscribe_control(0);
    hub.send_by(&id, &"stream ".repeat(800), &tui()).unwrap();
    until(&mut sub, is_kind_on("report.ready", id.clone())).await;
    hub.send_by(&id, "latest", &tui()).unwrap();
    until(&mut sub, |e| {
        e.thread == id && e.turn == Some(2) && e.kind() == "report.ready"
    })
    .await;
    let (_, _, turn, events) = hub.transcript_events(&id, None, None, 500).unwrap();
    assert_eq!(turn, Some(2));
    assert!(events.iter().all(|e| e.turn != Some(1)));
    let (_, _, turn, events) = hub.transcript_events(&id, None, Some(1), 500).unwrap();
    assert_eq!(turn, Some(1));
    assert!(events.len() < 10);
    assert!(events
        .iter()
        .any(|e| matches!(&e.body, EventBody::AgentText(p) if p.is_final)));
    let cursor = events[0].seq;
    let (_, _, _, tail) = hub
        .transcript_events(&id, Some(cursor), Some(1), 500)
        .unwrap();
    assert!(tail.iter().all(|e| e.seq > cursor && e.turn != Some(2)));
    assert_eq!(
        hub.transcript_events(&id, None, Some(99), 500)
            .unwrap_err()
            .code,
        codes::NOT_FOUND
    );
}

#[tokio::test]
async fn a_direct_command_ends_old_authority_without_interrupting_user_work() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    hub.say_to_orchestrator("watch", None, &tui()).unwrap();
    let mut rt = env.orch().await;
    let (_, turn) = rt.next_turn().await;
    raise(
        &hub,
        &api,
        "permission",
        AttentionReason::Permission,
        &["allow", "deny"],
    );
    let seen = hub.control_read(0, 1000).unwrap().1;
    let command = hub
        .say_to_orchestrator("allow api", Some(seen), &tui())
        .unwrap();
    assert_eq!(command["action"], "command");
    assert_eq!(
        hub.thread_view(&hub.orchestrator_thread().unwrap())
            .unwrap()
            .current_turn,
        Some(turn)
    );
    assert!(matches!(
        rt.end.cmds.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    let error = hub
        .set_mode(&api, "plan", &Caller::Orchestrator)
        .unwrap_err();
    assert_eq!(error.code, codes::RELAY_FORBIDDEN);
    assert!(error.message.contains("the user sent a newer message"));
    assert!(!error.message.contains("your next turn"));
    drop(rt);
}

#[tokio::test]
async fn a_direct_command_keeps_an_unrelated_background_error_running() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    let web = env.worker(&hub, "web").await;
    hub.say_to_orchestrator("watch", None, &tui()).unwrap();
    let mut rt = env.orch().await;
    let (_, user_turn) = rt.next_turn().await;
    raise(
        &hub,
        &web,
        "background-error",
        AttentionReason::Error,
        &["yes", "no"],
    );
    rt.end_turn(user_turn, "Watching.");
    let (notice, notice_turn) = rt.next_turn().await;
    assert!(notice.contains("attn_id=background-error"));
    raise(
        &hub,
        &api,
        "permission",
        AttentionReason::Permission,
        &["allow", "deny"],
    );
    let seen = hub.control_read(0, 1000).unwrap().1;
    let result = hub
        .say_to_orchestrator("allow api", Some(seen), &tui())
        .unwrap();
    assert_eq!(result["action"], "command");
    let orch = hub.orchestrator_thread().unwrap();
    assert_eq!(
        hub.thread_view(&orch).unwrap().current_turn,
        Some(notice_turn)
    );
    assert!(matches!(
        rt.end.cmds.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert!(hub
        .attention_list(true)
        .iter()
        .any(|a| a["attn_id"] == "background-error"));
    assert!(hub
        .attention_list(true)
        .iter()
        .all(|a| a["attn_id"] != "permission"));
    rt.end_turn(notice_turn, "web is waiting for your answer.");
    let mut sub = hub.subscribe_control(0);
    until(&mut sub, |e| {
        e.thread == orch && e.turn == Some(notice_turn) && e.kind() == "turn.ended"
    })
    .await;
    assert_eq!(hub.thread_view(&orch).unwrap().turns, 2);
    assert!(matches!(
        rt.end.cmds.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    drop(rt);
}

#[tokio::test]
async fn worker_questions_cost_no_notice_turn_and_reach_the_next_user_turn() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    hub.say_to_orchestrator("watch", None, &tui()).unwrap();
    let mut rt = env.orch().await;
    let (_, turn) = rt.next_turn().await;
    let orch = hub.orchestrator_thread().unwrap();
    rt.end_turn(turn, "Watching.");
    let mut sub = hub.subscribe_control(0);
    until(&mut sub, is_kind_on("turn.ended", orch.clone())).await;
    hub.send_by(&api, "!ask Keep the --force flag?|keep|cancel", &tui())
        .unwrap();
    let events = until(&mut sub, |e| {
        e.thread == api && raised(e).is_some_and(|a| a.reason == AttentionReason::Question)
    })
    .await;
    let question = raised(events.last().unwrap()).unwrap().attn_id.clone();
    let web = env.worker(&hub, "web").await;
    // A burst removes the original question from both the recent-event and byte windows.
    for n in 0..40 {
        hub.append(
            &web,
            None,
            Source::Hub,
            EventBody::AttentionRaised(AttentionRaised {
                attn_id: format!("noise-{n}"),
                reason: AttentionReason::Permission,
                summary: "large unrelated permission ".repeat(40),
                options: vec!["allow".into(), "deny".into()],
                ..Default::default()
            }),
            None,
        )
        .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(hub.thread_view(&orch).unwrap().turns, 1);
    assert!(matches!(
        rt.end.cmds.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert!(hub.thread_events(&orch, 0, 1000).unwrap().1.iter().any(|e| matches!(&e.body, EventBody::ConversationUpdate(p) if p.about_thread.as_deref() == Some(&api) && p.text.contains("Keep the --force flag?"))));
    hub.say_to_orchestrator("Explain what api is asking", None, &tui())
        .unwrap();
    let (prompt, turn) = rt.next_turn().await;
    assert!(prompt.contains("Keep the --force flag?"), "{prompt}");
    assert!(
        prompt.contains("Current open worker questions: 1"),
        "{prompt}"
    );
    assert!(prompt.len() < 10_000, "bounded context: {}", prompt.len());
    assert!(prompt.ends_with("Explain what api is asking"));
    assert!(hub
        .attention_list(false)
        .iter()
        .any(|i| i["attn_id"] == question));
    rt.end_turn(turn, "api asks whether to keep --force.");
    hub.respond(&question, Answer::new("cancel"), "tui")
        .unwrap();
    until(&mut sub, is_kind_on("report.ready", api)).await;
    assert_eq!(hub.thread_view(&orch).unwrap().turns, 2);
    hub.say_to_orchestrator("What questions remain?", None, &tui())
        .unwrap();
    let (prompt, turn) = rt.next_turn().await;
    assert!(
        prompt.contains("Current open worker questions: 0"),
        "{prompt}"
    );
    assert!(
        prompt.contains(&format!("request {question} answered: cancel")),
        "{prompt}"
    );
    assert!(!prompt.contains("\"status\":\"open_question\""), "{prompt}");
    rt.end_turn(turn, "No worker questions remain.");
    drop(rt);
}

/// A lone answer ("yes") typed to the orchestrator with one permission waiting is never
/// applied directly, by the hub or through the orchestrator's relay: the user may be answering
/// the orchestrator's own question. It reaches the orchestrator; a relay of it goes on the
/// confirm card, which names the item, and only the user's Confirm answers it.
#[tokio::test]
async fn a_lone_answer_goes_on_the_card() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    let mut sub = hub.subscribe_control(0);
    hub.say_to_orchestrator("watch", None, &tui()).unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    rt.end_turn(t1, "Should I merge the branch when the tests pass?");
    raise(
        &hub,
        &api,
        "p1",
        AttentionReason::Permission,
        &["allow", "deny"],
    );
    let seen = hub.control_read(0, 1000).unwrap().1;
    let r = hub.say_to_orchestrator("yes", Some(seen), &tui()).unwrap();
    assert_eq!(r["action"], "delegate", "{r}");
    assert!(open(&hub, "p1"));
    let (msg, t2) = rt.next_turn().await;
    assert!(msg.ends_with("yes"), "{msg}");
    proposed(relay(&hub, "p1", Answer::allow()));
    assert!(open(&hub, "p1"));
    rt.end_turn(t2, "I've put that on a card for you to confirm.");
    let evs = until(&mut sub, |e| {
        raised(e).is_some_and(|r| is_confirm_card(&e.thread, &r.ext))
    })
    .await;
    let card = raised(evs.last().unwrap()).unwrap();
    assert!(card.summary.contains("allow api"), "{}", card.summary);
    hub.respond(&card.attn_id, Answer::new("Confirm"), "tui")
        .unwrap();
    assert!(!open(&hub, "p1"));
    drop(rt);
    hub.shutdown().await;
}

/// The orchestrator starts a thread at a path only inside a git repository, and never in one
/// that is or holds the home folder, or holds or is in twr's data directory or a credentials
/// folder: such a thread's worktree would be that folder itself. The user may.
#[tokio::test]
async fn the_orchestrator_starts_threads_only_in_git_repositories() {
    let env = Env::new();
    let plain = env.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let hub = env
        .hub_with(|c| c.default_runtime = "claude-code".into())
        .await;
    let req = |name: &str, dir: &Path| NewThread {
        project: dir.to_string_lossy().into_owned(),
        name: Some(name.into()),
        ..Default::default()
    };
    let refused = async |dir: &Path, why: &str| {
        let e = hub
            .new_thread_by(req("a", dir), &Caller::Orchestrator)
            .await
            .unwrap_err();
        assert_eq!(e.code, codes::BAD_REQUEST, "{dir:?}");
        assert!(e.message.contains(why), "{e:?}");
        assert!(hub.resolve("a").is_err());
    };
    refused(&plain, "not in a git repository").await;
    refused(Path::new("/"), "not in a git repository").await;
    // The user may start one there.
    hub.new_thread_by(req("b", &plain), &tui()).await.unwrap();
    // In a git repository: fine.
    hub.new_thread_by(req("c", &env.repo()), &Caller::Orchestrator)
        .await
        .unwrap();
    // Not in one that holds the hub's data directory (`<env>/data`).
    Env::git_init(env.path());
    refused(&plain, "holds").await;
    hub.shutdown().await;
}

/// The orchestrator's move of a thread to another runtime can widen what the thread's mode
/// lets it do unasked (runtimes differ in what one mode lets through), so it goes through the
/// `set_mode` gate: in a user turn it becomes a proposal on the confirm card ("move api →
/// claude-code"), once per thread per turn, and only the user's Confirm moves the thread, as
/// the user's change. A model change alone is applied directly.
#[tokio::test]
async fn the_orchestrators_runtime_move_goes_on_the_card() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    let mut sub = hub.subscribe_control(0);
    hub.say_to_orchestrator("watch", None, &tui()).unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    // Outside a user turn: refused.
    rt.end_turn(t1, "Watching.");
    let e = hub
        .set_runtime_model(&api, Some("claude-code"), None, None, &Caller::Orchestrator)
        .unwrap_err();
    assert_eq!(e.code, codes::RELAY_FORBIDDEN, "{e:?}");
    let seen = hub.control_read(0, 1000).unwrap().1;
    let r = hub
        .say_to_orchestrator("move api to claude code", Some(seen), &tui())
        .unwrap();
    assert_eq!(r["action"], "delegate", "{r}");
    let (_, t2) = rt.next_turn().await;
    let r = hub
        .set_runtime_model(
            &api,
            Some("claude-code"),
            None,
            Some("low"),
            &Caller::Orchestrator,
        )
        .unwrap();
    assert_eq!(r["status"], "needs_confirmation", "{r}");
    assert_eq!(hub.thread_view(&api).unwrap().runtime, "fake");
    let e = hub
        .set_runtime_model(&api, Some("claude-code"), None, None, &Caller::Orchestrator)
        .unwrap_err();
    assert!(e.message.contains("already proposed"), "{e:?}");
    // A model change alone needs no card.
    let r = hub
        .set_model(&api, Some("fake-big"), None, &Caller::Orchestrator)
        .unwrap();
    assert_eq!(r["changed"], true, "{r}");
    rt.end_turn(t2, "I've put that on a card for you to confirm.");
    let evs = until(&mut sub, |e| {
        raised(e).is_some_and(|r| is_confirm_card(&e.thread, &r.ext))
    })
    .await;
    let card = raised(evs.last().unwrap()).unwrap();
    assert!(
        card.summary
            .contains("move api → claude-code, effort \"low\""),
        "{}",
        card.summary
    );
    hub.respond(&card.attn_id, Answer::new("Confirm"), "tui")
        .unwrap();
    let v = hub.thread_view(&api).unwrap();
    assert_eq!(v.runtime, "claude-code");
    assert_eq!(v.effort.as_deref(), Some("low"));
    let evs = hub.thread_events(&api, 0, 1000).unwrap().1;
    let by = evs.iter().rev().find_map(|e| match &e.body {
        EventBody::ThreadModelChanged(p) if p.runtime.is_some() => Some(p.by),
        _ => None,
    });
    assert_eq!(by, Some(ChangedBy::User));
    drop(rt);
    hub.shutdown().await;
}

/// A mode on a confirm card is for the runtime the thread ran when it was proposed: the
/// orchestrator may move a thread in a restricted mode to another runtime without a card, and
/// the same mode can let the other runtime do more without asking. Confirm then skips the mode,
/// saying why, and the thread stays in its mode.
#[tokio::test]
async fn a_cards_mode_is_skipped_for_a_thread_moved_to_another_runtime_since() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    hub.set_mode(&api, "plan", &tui()).unwrap();
    let mut sub = hub.subscribe_control(0);
    hub.say_to_orchestrator("watch", None, &tui()).unwrap();
    let mut rt = env.orch().await;
    let (_, t) = rt.next_turn().await;
    rt.end_turn(t, "Watching.");
    let orch = Caller::Orchestrator;
    let seen = hub.control_read(0, 1000).unwrap().1;
    let r = hub
        .say_to_orchestrator(
            "Set api to default mode. What changed on api?",
            Some(seen),
            &tui(),
        )
        .unwrap();
    assert_eq!(r["action"], "delegate", "{r}");
    let (_, t) = rt.next_turn().await;
    let r = hub.set_mode(&api, "default", &orch).unwrap();
    assert_eq!(r["status"], "needs_confirmation", "{r}");
    rt.end_turn(t, "I've put that on a card for you to confirm.");
    let evs = until(&mut sub, |e| {
        raised(e).is_some_and(|r| is_confirm_card(&e.thread, &r.ext))
    })
    .await;
    let card = raised(evs.last().unwrap()).unwrap().attn_id.clone();
    // In plan, a restricted mode on Claude Code too: the orchestrator's move needs no card.
    let r = hub
        .set_runtime_model(&api, Some("claude-code"), None, None, &orch)
        .unwrap();
    assert_ne!(r["status"], "needs_confirmation", "{r}");
    assert_eq!(hub.thread_view(&api).unwrap().runtime, "claude-code");
    let ev = hub.respond(&card, Answer::new("Confirm"), "tui").unwrap();
    let EventBody::AttentionResolved(res) = &ev.body else {
        panic!("{ev:?}")
    };
    let confirmed = &Value::Object(res.ext.clone().unwrap())["twr"]["confirmed"];
    assert_eq!(confirmed["applied"], json!([]), "{confirmed}");
    assert!(
        confirmed["skipped"][0]
            .as_str()
            .unwrap()
            .contains("moved to another runtime"),
        "{confirmed}"
    );
    let v = hub.thread_view(&api).unwrap();
    assert_eq!(
        (v.runtime.as_str(), v.mode.as_str()),
        ("claude-code", "plan")
    );
    drop(rt);
    hub.shutdown().await;
}

/// One confirm card that sets a thread's mode and moves it to another runtime checks each
/// action with the card's earlier ones applied: Confirm never leaves a thread on a runtime in
/// a mode that runtime does not take. `never` (a mode the fake runtime accepts, Claude Code
/// does not) then a move to Claude Code: the mode applies, the move is skipped. The move
/// first, then `never`: the move applies, the mode is skipped. Each skip says why.
#[tokio::test]
async fn a_card_never_moves_a_thread_into_a_mode_its_new_runtime_does_not_take() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    let web = env.worker(&hub, "web").await;
    let mut sub = hub.subscribe_control(0);
    hub.say_to_orchestrator("watch", None, &tui()).unwrap();
    let mut rt = env.orch().await;
    let (_, t) = rt.next_turn().await;
    rt.end_turn(t, "Watching.");
    let orch = Caller::Orchestrator;
    for (thread, mode_first) in [(&api, true), (&web, false)] {
        let seen = hub.control_read(0, 1000).unwrap().1;
        let r = hub
            .say_to_orchestrator("never mode and claude code for it", Some(seen), &tui())
            .unwrap();
        assert_eq!(r["action"], "delegate", "{r}");
        let (_, t) = rt.next_turn().await;
        let set_mode = || hub.set_mode(thread, "never", &orch).unwrap();
        let move_it = || {
            hub.set_runtime_model(thread, Some("claude-code"), None, None, &orch)
                .unwrap()
        };
        let (a, b) = if mode_first {
            (set_mode(), move_it())
        } else {
            (move_it(), set_mode())
        };
        assert_eq!(a["status"], "needs_confirmation", "{a}");
        assert_eq!(b["status"], "needs_confirmation", "{b}");
        rt.end_turn(t, "I've put that on a card for you to confirm.");
        let evs = until(&mut sub, |e| {
            raised(e).is_some_and(|r| is_confirm_card(&e.thread, &r.ext))
        })
        .await;
        let card = raised(evs.last().unwrap()).unwrap().attn_id.clone();
        let ev = hub.respond(&card, Answer::new("Confirm"), "tui").unwrap();
        let EventBody::AttentionResolved(res) = &ev.body else {
            panic!("{ev:?}")
        };
        let confirmed = &Value::Object(res.ext.clone().unwrap())["twr"]["confirmed"];
        assert_eq!(
            confirmed["applied"].as_array().unwrap().len(),
            1,
            "{confirmed}"
        );
        assert_eq!(
            confirmed["skipped"].as_array().unwrap().len(),
            1,
            "{confirmed}"
        );
        let v = hub.thread_view(thread).unwrap();
        if mode_first {
            assert_eq!((v.runtime.as_str(), v.mode.as_str()), ("fake", "never"));
            assert!(
                confirmed["skipped"][0].as_str().unwrap().contains("move"),
                "{confirmed}"
            );
        } else {
            assert_eq!(v.runtime, "claude-code");
            assert_ne!(v.mode, "never");
            assert!(
                confirmed["skipped"][0].as_str().unwrap().contains("never"),
                "{confirmed}"
            );
        }
    }
    drop(rt);
    hub.shutdown().await;
}

/// The hub's own agent CLIs as a fake probe reports them: Claude Code's missing, Codex's
/// there and signed in.
fn fake_local_agents() -> twr_hub::hub::AgentProbe {
    use futures::FutureExt;
    use twr_core::runner::{AgentInfo, AgentLogin};
    Arc::new(|| {
        async {
            vec![
                AgentInfo {
                    runtime: "claude-code".into(),
                    note: Some("not found on PATH".into()),
                    ..Default::default()
                },
                AgentInfo {
                    runtime: "codex".into(),
                    found: true,
                    path: Some("/home/me/.local/bin/codex".into()),
                    version: Some("codex-cli 0.160.0".into()),
                    login: AgentLogin::LoggedIn,
                    ..Default::default()
                },
            ]
        }
        .boxed()
    })
}

/// The orchestrator starts a thread on a box it names (`thread.spawn` `box`, as `thread.new`
/// takes it, with its errors); one on a runtime whose CLI the box reports missing is made,
/// and the spawn's result warns. What it reads of the boxes' agents is their state only.
#[tokio::test]
async fn the_orchestrator_picks_a_box_and_is_warned_of_a_missing_agent() {
    use twr_hub::hub::BoxView;
    let env = Env::new();
    let mut opts = env.opts(|_| {});
    opts.agent_probe = Some(fake_local_agents());
    let hub = Hub::open(opts).await.unwrap();
    // Probed in the background as the hub started.
    timeout(WAIT, async {
        while hub.info()["agents"].as_array().is_none_or(Vec::is_empty) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the hub's own probe");
    assert_eq!(hub.info()["agents"][1]["path"], "/home/me/.local/bin/codex");
    assert_eq!(
        hub.info_for(BoxView::Limited)["agents"],
        json!([
            {"runtime": "claude-code", "found": false, "login": "unknown"},
            {"runtime": "codex", "found": true, "login": "logged_in"},
        ])
    );
    let spawn = |op: &'static str, runtime: &'static str, box_name: &'static str| {
        let hub = hub.clone();
        let request = NewThread {
            project: env.repo().to_string_lossy().into_owned(),
            runtime: Some(runtime.into()),
            name: Some(op.into()),
            box_name: Some(box_name.into()),
            ..Default::default()
        };
        async move {
            hub.begin_spawn(op, request, None, &Caller::Orchestrator)
                .unwrap();
            timeout(WAIT, async {
                loop {
                    let status = hub.spawn_status(op, &Caller::Orchestrator).unwrap();
                    if status["status"] != "pending" {
                        break status;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap()
        }
    };
    let made = spawn("on-local", "claude-code", "local").await;
    assert_eq!(made["status"], "complete", "{made}");
    assert_eq!(made["thread"]["box_name"], "local");
    let warning = "box local (the hub's machine) reports no `claude` on its PATH; the thread may \
                   fail to start";
    assert_eq!(made["warning"], warning, "{made}");
    assert_eq!(made["thread"]["warning"], warning, "{made}");
    let made = spawn("on-gpu", "claude-code", "gpu").await;
    assert_eq!(made["status"], "error", "{made}");
    assert_eq!(made["error"]["code"], codes::BOX_OFFLINE, "{made}");
    assert!(
        made["error"]["message"]
            .as_str()
            .unwrap()
            .contains("no box \"gpu\" is configured"),
        "{made}"
    );
    // `box.agents` of the hub's own box, as the orchestrator sees it; no fresh probe for it.
    let local = hub
        .box_agents("local", false, BoxView::Limited)
        .await
        .unwrap();
    assert_eq!(
        local["agents"][1],
        json!({"runtime": "codex", "found": true, "login": "logged_in"})
    );
    assert_eq!(
        hub.box_agents("local", true, BoxView::Limited)
            .await
            .unwrap_err()
            .code,
        codes::UNAUTHORIZED
    );
    // A user's fresh probe of the hub's own box.
    let fresh = hub.box_agents("local", true, BoxView::Full).await.unwrap();
    assert_eq!(fresh["agents"][1]["version"], "codex-cli 0.160.0");
    hub.shutdown().await;
}

/// A suggestion the user took unedited (`say` with `suggested`) goes to the orchestrator as a
/// message, never applied as a short command at once, however it reads now; the same words
/// typed are applied directly.
#[tokio::test]
async fn a_suggested_message_is_never_applied_as_a_command() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let api = env.worker(&hub, "api").await;
    let web = env.worker(&hub, "web").await;
    hub.say_to_orchestrator("watch", None, &tui()).unwrap();
    let mut rt = env.orch().await;
    let (_, t) = rt.next_turn().await;
    rt.end_turn(t, "Watching.");
    permissions(&hub, &[&api]);
    let mode = hub.thread_view(&web).unwrap().mode;
    assert_ne!(mode, "plan");
    for m in ["allow api", "set web to plan mode"] {
        let seen = hub.control_read(0, 1000).unwrap().1;
        let r = hub
            .say_to_orchestrator_as(m, Some(seen), &tui(), true)
            .unwrap();
        assert_eq!(r["action"], "delegate", "{m}: {r}");
        assert!(r["ref"].as_i64().is_some(), "{m}: {r}");
        let (prompt, t) = rt.next_turn().await;
        assert!(prompt.ends_with(m), "{m}: {prompt}");
        rt.end_turn(t, "ok");
    }
    assert!(open(&hub, "p0"));
    assert_eq!(hub.thread_view(&web).unwrap().mode, mode);
    // Typed, the same words apply.
    for m in ["allow api", "set web to plan mode"] {
        let seen = hub.control_read(0, 1000).unwrap().1;
        let r = hub
            .say_to_orchestrator_as(m, Some(seen), &tui(), false)
            .unwrap();
        assert_eq!(r["action"], "command", "{m}: {r}");
    }
    assert!(!open(&hub, "p0"));
    assert_eq!(hub.thread_view(&web).unwrap().mode, "plan");
    drop(rt);
    hub.shutdown().await;
}
