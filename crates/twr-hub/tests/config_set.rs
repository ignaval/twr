//! `config.set` (`hub::config_set`): the orchestrator changes twr's configuration only through
//! the hub, in a turn of the user's latest message. Display and voice preferences apply at
//! once; every other key goes on the confirm card, which only the user's Confirm writes, and
//! only while the file still has the value the card was raised on. Boxes, TLS files, `env`
//! entries, command lines and secret-looking values are never changed this way. The file keeps
//! its comments and every other byte; every change is a `config.changed` line in the
//! orchestrator's conversation.
//!
//! The orchestrator's runtime is `claude-code` played by a test adapter that hands every
//! session to the test, as in `tests/memory.rs`.

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
use twr_hub::{server, Caller, Hub, HubOptions, NewThread};

const WAIT: Duration = Duration::from_secs(10);

/// The config file the tests start from: comments, spacing and a secret that must stay as
/// they are.
const CONFIG: &str = "\
# my twr config
default_runtime = \"fake\"   # the test runtime

[orchestrator]
runtime = \"claude-code\"

[threads]
default_mode = \"auto\"     # keep

[boxes.lab]
token = \"secret-token-0123456789abcdef\"   # never share

[tui]
# how the TUI looks
suggest = true            # a small model suggests
mouse = true
";

struct OrchRt {
    end: AdapterEnd,
    first: Option<(String, u64)>,
}

impl OrchRt {
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

    fn end_turn(&self, turn: u64) {
        self.end.emit(
            EventBody::AgentText(AgentText {
                text: "ok".into(),
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
}

struct OrchAdapter(mpsc::UnboundedSender<OrchRt>);

impl Adapter for OrchAdapter {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "claude-code",
            tier: Tier::Headless,
            description: "test orchestrator runtime",
        }
    }

    fn spawn(&self, _cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let (h, end) = AdapterHandle::pair();
        end.emit(
            EventBody::ThreadStarted(SessionPayload {
                session_id: Some("s-orch".into()),
                ..Default::default()
            }),
            None,
        );
        let first = prompt.map(|p| (p, 1));
        let _ = self.0.send(OrchRt { end, first });
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
        let env = Env {
            dir: tempfile::tempdir().unwrap(),
            spawns,
            tx,
        };
        std::fs::write(env.config_file(), CONFIG).unwrap();
        env
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn config_file(&self) -> PathBuf {
        self.path().join("config.toml")
    }

    fn file(&self) -> String {
        std::fs::read_to_string(self.config_file()).unwrap()
    }

    async fn hub(&self) -> Hub {
        let mut config = Config::from_toml_str(CONFIG).unwrap();
        config.orchestrator.cwd = Some(self.path().to_path_buf());
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
        o.claude_policy = twr_adapters::claude_code::guard::PolicySources {
            managed_dir: Some(self.path().join("claude-managed")),
            config_dir: Some(self.path().join("claude-config")),
        };
        o.config_file = Some(self.config_file());
        o.stop_timeout = Duration::from_secs(5);
        Hub::open(o).await.unwrap()
    }

    async fn orch(&mut self) -> OrchRt {
        timeout(WAIT, self.spawns.recv())
            .await
            .expect("timed out waiting for the orchestrator to spawn")
            .unwrap()
    }
}

fn tui() -> Caller {
    Caller::user("tui")
}

/// The user's message `text` to the orchestrator: the turn it starts.
async fn user_turn(hub: &Hub, rt: &mut OrchRt, text: &str) -> u64 {
    let seen = hub.control_read(0, 1000).unwrap().1;
    hub.say_to_orchestrator(text, Some(seen), &tui()).unwrap();
    let (got, turn) = rt.next_turn().await;
    assert!(got.ends_with(text), "{got}");
    turn
}

async fn set(hub: &Hub, key: &str, value: Value) -> Result<Value, RpcError> {
    hub.config_set(key, value, &Caller::Orchestrator).await
}

/// Every `config.changed` on the orchestrator's thread.
fn changes(hub: &Hub) -> Vec<ConfigChanged> {
    let orch = hub.orchestrator_thread().unwrap();
    hub.thread_events(&orch, 0, 10_000)
        .unwrap()
        .1
        .into_iter()
        .filter_map(|e| match e.body {
            EventBody::ConfigChanged(c) => Some(c),
            _ => None,
        })
        .collect()
}

#[track_caller]
fn refused(r: Result<Value, RpcError>, code: &str, says: &str) {
    let e = r.unwrap_err();
    assert_eq!(e.code, code, "{e:?}");
    assert!(e.message.contains(says), "{e:?}");
}

/// The next confirm card raised: its id and summary.
async fn card(sub: &mut ControlSubscription) -> (String, String) {
    timeout(WAIT, async {
        loop {
            let (_, ev) = sub.next().await.unwrap().expect("hub stopped");
            if let EventBody::AttentionRaised(r) = &ev.body {
                if is_confirm_card(&ev.thread, &r.ext) {
                    return (r.attn_id.clone(), r.summary.clone());
                }
            }
        }
    })
    .await
    .expect("timed out waiting for a confirm card")
}

/// What a confirmed card says it applied and skipped.
fn confirmed(ev: &Event) -> (Vec<String>, Vec<String>) {
    let EventBody::AttentionResolved(p) = &ev.body else {
        panic!("not a resolution: {ev:?}");
    };
    let c = &p.ext.as_ref().unwrap()["twr"]["confirmed"];
    let list =
        |k: &str| -> Vec<String> { serde_json::from_value(c[k].clone()).unwrap_or_default() };
    (list("applied"), list("skipped"))
}

/// A display preference the user asks for is applied at once: the one value in the file
/// changes, every other byte (comments, spacing, the box's token) stays, and the change is a
/// line in the orchestrator's conversation saying when it takes effect. A key the file lacks
/// goes under its table's header; a float is written as one.
#[tokio::test]
async fn a_direct_key_is_applied_at_once_and_shown() {
    let mut env = Env::new();
    let hub = env.hub().await;
    hub.say_to_orchestrator("hello", None, &tui()).unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    rt.end_turn(t1);
    let t2 = user_turn(&hub, &mut rt, "turn off suggestions").await;
    let r = set(&hub, "tui.suggest", json!(false)).await.unwrap();
    assert_eq!(r["status"], "applied", "{r}");
    assert_eq!(r["key"], "tui.suggest");
    assert_eq!(r["value"], false);
    assert_eq!(r["before"], true);
    assert!(
        r["effect"]
            .as_str()
            .unwrap()
            .contains("the next time a TUI starts"),
        "{r}"
    );
    // Only the value: its comment, the spacing before it and the box's token stay.
    assert_eq!(
        env.file(),
        CONFIG.replace("suggest = true", "suggest = false")
    );
    let c = changes(&hub);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].key, "tui.suggest");
    assert_eq!(c[0].by, "orchestrator");
    assert_eq!(c[0].before, Some(json!(true)));
    assert_eq!(
        c[0].line(),
        "set tui.suggest = false (was true); takes effect the next time a TUI starts (one \
         already open keeps what it read)"
    );
    // `config.show` says what the file has now; the rest is what the hub started with.
    let shown = hub.config_show();
    assert_eq!(shown["preferences"]["tui.suggest"], false, "{shown}");
    assert_eq!(
        shown["preferences"]["speech.volume"],
        Value::Null,
        "{shown}"
    );
    assert_eq!(shown["tui"]["suggest"], true, "{shown}");
    // The same value again: nothing to change.
    refused(
        set(&hub, "tui.suggest", json!(false)).await,
        codes::BAD_REQUEST,
        "already false",
    );
    // Missing keys: under the header, or in a table of their own; an integer where twr
    // takes a float.
    let before = env.file();
    set(&hub, "tui.colors", json!("16")).await.unwrap();
    let r = set(&hub, "speech.volume", json!(0.5)).await.unwrap();
    assert_eq!(r["before"], Value::Null);
    assert_eq!(
        env.file(),
        before.replace("[tui]\n", "[tui]\ncolors = \"16\"\n") + "\n[speech]\nvolume = 0.5\n"
    );
    Config::from_toml_str(&env.file()).unwrap();
    assert_eq!(changes(&hub).len(), 3);
    rt.end_turn(t2);
    // A direct change raises no card.
    assert!(hub.attention_list(false).is_empty());
    drop(rt);
    hub.shutdown().await;
}

/// A key that changes how twr works goes on the confirm card with what the file has, never
/// into the file by itself; once per key per turn. Confirm writes every config action of the
/// card in order, recorded as the user's with the card's id; Cancel writes nothing.
#[tokio::test]
async fn a_card_key_waits_for_the_users_confirm() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let mut sub = hub.subscribe_control(0);
    hub.say_to_orchestrator("hello", None, &tui()).unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    rt.end_turn(t1);
    let t2 = user_turn(
        &hub,
        &mut rt,
        "set the default mode to plan and let codex never ask",
    )
    .await;
    let r = set(&hub, "threads.default_mode", json!("plan"))
        .await
        .unwrap();
    assert_eq!(r["status"], "needs_confirmation", "{r}");
    assert_eq!(r["before"], "auto");
    assert!(
        r["message"]
            .as_str()
            .unwrap()
            .contains("I've put that on a card for you to confirm."),
        "{r}"
    );
    assert!(
        r["effect"].as_str().unwrap().contains("twr hub stop"),
        "{r}"
    );
    refused(
        set(&hub, "threads.default_mode", json!("manual")).await,
        codes::RELAY_FORBIDDEN,
        "already proposed a change to threads.default_mode",
    );
    let r = set(&hub, "codex.auto", json!("never")).await.unwrap();
    assert_eq!(r["status"], "needs_confirmation", "{r}");
    // Nothing is written yet.
    assert_eq!(env.file(), CONFIG);
    rt.end_turn(t2);
    let (attn, summary) = card(&mut sub).await;
    assert_eq!(
        summary,
        "From your message: config threads.default_mode = \"plan\" (was \"auto\") · config \
         codex.auto = \"never\" (was unset)"
    );
    assert_eq!(env.file(), CONFIG);
    let resolved = hub.respond(&attn, Answer::new("Confirm"), "tui").unwrap();
    let (applied, skipped) = confirmed(&resolved);
    assert_eq!(
        applied,
        vec![
            "config threads.default_mode = \"plan\" (was \"auto\")",
            "config codex.auto = \"never\" (was unset)"
        ]
    );
    assert!(skipped.is_empty(), "{skipped:?}");
    assert_eq!(
        env.file(),
        CONFIG.replace("default_mode = \"auto\"", "default_mode = \"plan\"")
            + "\n[codex]\nauto = \"never\"\n"
    );
    let c = changes(&hub);
    assert_eq!(c.len(), 2);
    for ch in &c {
        assert_eq!(ch.by, "user");
        assert_eq!(
            ch.ext.as_ref().unwrap()["twr"][PROPOSAL_KEY],
            json!(attn),
            "{ch:?}"
        );
    }
    assert_eq!(c[0].key, "threads.default_mode");
    // Cancel writes nothing.
    let t3 = user_turn(&hub, &mut rt, "set the default mode to manual").await;
    let r = set(&hub, "threads.default_mode", json!("manual"))
        .await
        .unwrap();
    assert_eq!(r["status"], "needs_confirmation", "{r}");
    assert_eq!(r["before"], "plan");
    rt.end_turn(t3);
    let (attn, _) = card(&mut sub).await;
    let file = env.file();
    hub.respond(&attn, Answer::new("Cancel"), "tui").unwrap();
    assert_eq!(env.file(), file);
    assert_eq!(changes(&hub).len(), 2);
    drop(rt);
    hub.shutdown().await;
}

/// A card's config action holds only while the file has the value it was raised on: one the
/// file has already by the time the card is raised is left off it, and one whose key changed
/// by hand before Confirm is skipped, the card's other actions applied.
#[tokio::test]
async fn a_key_changed_since_is_left_off_or_skipped() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let mut sub = hub.subscribe_control(0);
    hub.say_to_orchestrator("hello", None, &tui()).unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    rt.end_turn(t1);
    let t2 = user_turn(&hub, &mut rt, "configure twr for me").await;
    for (k, v) in [
        ("threads.default_mode", json!("plan")),
        ("log.retention_days", json!(60)),
        ("trash.keep_days", json!(3)),
    ] {
        let r = set(&hub, k, v).await.unwrap();
        assert_eq!(r["status"], "needs_confirmation", "{r}");
    }
    // The user sets one of them by hand before the turn ends: it is no longer proposed.
    let file = env.config_file();
    twr_core::config_edit::write_key(
        &file,
        &["trash"],
        "keep_days",
        &twr_core::config_edit::Scalar::Int(3),
    )
    .unwrap();
    rt.end_turn(t2);
    let (attn, summary) = card(&mut sub).await;
    assert_eq!(
        summary,
        "From your message: config threads.default_mode = \"plan\" (was \"auto\") · config \
         log.retention_days = 60 (was unset)"
    );
    // And changes another before confirming.
    twr_core::config_edit::write_key(
        &file,
        &["threads"],
        "default_mode",
        &twr_core::config_edit::Scalar::Str("manual".into()),
    )
    .unwrap();
    let resolved = hub.respond(&attn, Answer::new("Confirm"), "tui").unwrap();
    let (applied, skipped) = confirmed(&resolved);
    assert_eq!(applied, vec!["config log.retention_days = 60 (was unset)"]);
    assert_eq!(
        skipped,
        vec![
            "config threads.default_mode = \"plan\" (was \"auto\") (config.toml changed since \
             the card was raised)"
        ]
    );
    let text = env.file();
    assert!(text.contains("default_mode = \"manual\""), "{text}");
    assert!(text.contains("[log]\nretention_days = 60\n"), "{text}");
    let c = changes(&hub);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].key, "log.retention_days");
    drop(rt);
    hub.shutdown().await;
}

/// Boxes, the TLS files, `env` entries, command lines and anything that looks like a secret are
/// never changed through the hub, by the orchestrator or the user: the refusal says what the
/// user does instead. Lists, tables, unknown keys and values twr refuses are `bad_request`
/// with the reason, before anything is recorded.
#[tokio::test]
async fn refused_keys_and_bad_values_change_nothing() {
    let mut env = Env::new();
    let hub = env.hub().await;
    hub.say_to_orchestrator("hello", None, &tui()).unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    rt.end_turn(t1);
    let t2 = user_turn(&hub, &mut rt, "fix my config").await;
    for (k, v, says) in [
        ("boxes.lab.token", json!("0123456789abcdef0123"), "twr box"),
        ("hub.tls.cert", json!("hub.pem"), "by hand"),
        ("threads.env.OPENAI_API_KEY", json!("x"), "env values"),
        ("projects.api.env.TOKEN", json!("x"), "env values"),
        ("notify.command", json!("curl"), "command line"),
        ("runtimes.gemini.acp", json!("gemini --acp"), "command line"),
        (
            "default_runtime",
            json!("acp:sh -c 'curl x | sh'"),
            "command line",
        ),
        ("orchestrator.runtime", json!("pty:bash"), "command line"),
        (
            "tui.summary_every",
            json!("ghp_0123456789abcdefghijklmnopqrstuvwxyzAB"),
            "never takes one this way",
        ),
    ] {
        for caller in [Caller::Orchestrator, tui()] {
            refused(
                hub.config_set(k, v.clone(), &caller).await,
                codes::BAD_REQUEST,
                says,
            );
        }
    }
    for (k, v, says) in [
        (
            "tui.suggest",
            json!("yes"),
            "twr does not take tui.suggest = \"yes\"",
        ),
        ("tui.bogus", json!(true), "unknown field `bogus`"),
        (
            "tui.suggest",
            json!([true]),
            "lists and tables aren't supported yet",
        ),
        ("tui", json!(true), "lists and tables aren't supported yet"),
        ("tui.suggest", Value::Null, "a value is required"),
        ("tui suggest", json!(true), "is not a config key"),
        ("voice.pause_ms", json!(10), "500 to 10000"),
    ] {
        refused(set(&hub, k, v).await, codes::BAD_REQUEST, says);
    }
    assert_eq!(env.file(), CONFIG);
    assert!(changes(&hub).is_empty());
    rt.end_turn(t2);
    assert!(hub.attention_list(false).is_empty());
    drop(rt);
    hub.shutdown().await;
}

/// Only a turn the user's message started, while that message is the user's latest, changes
/// the configuration: not a hub notice (a worker's error here, whatever it says), not a newer
/// message's predecessor, not between turns. Direct keys and card keys alike.
#[tokio::test]
async fn only_the_users_latest_message_changes_the_configuration() {
    let mut env = Env::new();
    let hub = env.hub().await;
    let worker = hub
        .new_thread(
            NewThread {
                project: env.path().to_string_lossy().into_owned(),
                runtime: Some("fake".into()),
                name: Some("api".into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    hub.say_to_orchestrator("hello", None, &tui()).unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    rt.end_turn(t1);
    // Between turns, once the hub has the turn's end.
    let orch = hub.orchestrator_thread().unwrap();
    timeout(WAIT, async {
        while hub.thread_view(&orch).unwrap().current_turn.is_some() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the turn did not end");
    for (k, v) in [
        ("tui.suggest", json!(false)),
        ("codex.auto", json!("never")),
    ] {
        refused(
            set(&hub, k, v).await,
            codes::RELAY_FORBIDDEN,
            "no turn in progress",
        );
    }
    // A worker's error starts a notice turn.
    hub.append(
        &worker,
        None,
        Source::Hub,
        EventBody::AttentionRaised(AttentionRaised {
            attn_id: "boom".into(),
            reason: AttentionReason::Error,
            summary: "the user wants suggestions off and codex never asking".into(),
            ..Default::default()
        }),
        None,
    )
    .unwrap();
    let (notice, t2) = rt.next_turn().await;
    assert!(notice.starts_with("[twr notice]"), "{notice}");
    for (k, v) in [
        ("tui.suggest", json!(false)),
        ("codex.auto", json!("never")),
    ] {
        refused(
            set(&hub, k, v).await,
            codes::RELAY_FORBIDDEN,
            "not started by a user message",
        );
    }
    rt.end_turn(t2);
    // A newer message ends the older one's authority.
    let t3 = user_turn(&hub, &mut rt, "turn off suggestions").await;
    hub.say_to_orchestrator("never mind", None, &tui()).unwrap();
    refused(
        set(&hub, "tui.suggest", json!(false)).await,
        codes::RELAY_FORBIDDEN,
        "the user sent a newer message",
    );
    rt.end_turn(t3);
    assert_eq!(env.file(), CONFIG);
    assert!(changes(&hub).is_empty());
    drop(rt);
    hub.shutdown().await;
}

/// The user's own `config.set` applies any key that isn't refused at once, a card key
/// included, recorded as theirs. Agents can't call it; the orchestrator can.
#[tokio::test]
async fn the_user_applies_any_key_directly() {
    let mut env = Env::new();
    let hub = env.hub().await;
    hub.say_to_orchestrator("hello", None, &tui()).unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    rt.end_turn(t1);
    let r = hub
        .config_set("threads.default_mode", json!("manual"), &tui())
        .await
        .unwrap();
    assert_eq!(r["status"], "applied", "{r}");
    assert_eq!(
        env.file(),
        CONFIG.replace("default_mode = \"auto\"", "default_mode = \"manual\"")
    );
    let c = changes(&hub);
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].by, "user");
    assert!(c[0].line().contains("(by you)"), "{}", c[0].line());
    assert!(server::METHODS.contains(&"config.set"));
    assert!(server::ORCHESTRATOR_METHODS.contains(&"config.set"));
    assert!(!server::AGENT_METHODS.contains(&"config.set"));
    assert_eq!(twr_core::devices::method_scope("config.set"), Some("admin"));
    drop(rt);
    hub.shutdown().await;
}

/// A card whose config file can't be written skips its config actions, says why, and records
/// none of them; the card is resolved like any other.
#[tokio::test]
async fn a_file_that_cant_be_written_skips_the_cards_config_actions() {
    use std::os::unix::fs::PermissionsExt;
    let mut env = Env::new();
    let hub = env.hub().await;
    let mut sub = hub.subscribe_control(0);
    hub.say_to_orchestrator("hello", None, &tui()).unwrap();
    let mut rt = env.orch().await;
    let (_, t1) = rt.next_turn().await;
    rt.end_turn(t1);
    let t2 = user_turn(&hub, &mut rt, "keep the trash for a day").await;
    let r = set(&hub, "trash.keep_days", json!(1)).await.unwrap();
    assert_eq!(r["status"], "needs_confirmation", "{r}");
    rt.end_turn(t2);
    let (attn, _) = card(&mut sub).await;
    let file = env.config_file();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o400)).unwrap();
    let resolved = hub.respond(&attn, Answer::new("Confirm"), "tui");
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
    let (applied, skipped) = confirmed(&resolved.unwrap());
    assert!(applied.is_empty(), "{applied:?}");
    assert_eq!(skipped.len(), 1, "{skipped:?}");
    assert!(
        skipped[0]
            .starts_with("config trash.keep_days = 1 (was unset) (config.toml was not changed"),
        "{skipped:?}"
    );
    assert_eq!(env.file(), CONFIG);
    assert!(changes(&hub).is_empty());
    drop(rt);
    hub.shutdown().await;
}
