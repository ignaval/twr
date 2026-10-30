//! Limits and sign-in failures on accounts (`hub::retry`): a turn that ends on a usage or
//! spending limit (`ext.twr.limit`) is classified `limited` and never retried, its card naming
//! the account, the window and the reset (else the runtime's words); a transient failure is
//! still retried once; a sign-in failure on a named account says `twr account login` instead of
//! the adapter's hint and has that account's login checked again; on the native account the
//! hint stays as the adapter wrote it, and its login is checked again where the runtime has
//! named accounts. Both name the account the failed turn ran on, not the one
//! the thread moved to while it ran.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Local, SecondsFormat, TimeDelta, Utc};
use futures::FutureExt;
use serde_json::{json, Value};
use tokio::time::timeout;
use twr_core::account::Login;
use twr_core::adapter::{
    Adapter, AdapterCmd, AdapterEnd, AdapterHandle, AdapterInfo, SpawnConfig, Tier,
};
use twr_core::config::{AccountConfig, Config, ProjectConfig};
use twr_core::event::*;
use twr_core::paths::Paths;
use twr_core::runner::{AgentInfo, AgentLogin};
use twr_hub::hub::{AccountProbe, ModelRequest, NativeDirFn, RETRY_PROMPT};
use twr_hub::{Caller, Hub, HubOptions, NewThread};

const WAIT: Duration = Duration::from_secs(20);

/// Every prompt the runtime received, in order.
type Prompts = Arc<Mutex<Vec<String>>>;

/// A runtime played under `claude-code`: a prompt that is a JSON object ends its turn `error`
/// with that object as `turn.ended`'s `ext` (once the file its `hold` names exists, when it
/// names one); any other prompt completes.
struct LimitAdapter(Prompts);

impl Adapter for LimitAdapter {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "claude-code",
            tier: Tier::Headless,
            description: "limit test runtime",
        }
    }

    fn spawn(&self, cfg: SpawnConfig, prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let (h, end) = AdapterHandle::pair();
        let first = prompt.map(|p| (p, cfg.prompt_turn.unwrap_or(1)));
        tokio::spawn(serve(end, Arc::clone(&self.0), first));
        Ok(h)
    }

    fn resume(&self, cfg: SpawnConfig, _session_id: String) -> anyhow::Result<AdapterHandle> {
        self.spawn(cfg, None)
    }
}

async fn serve(mut end: AdapterEnd, prompts: Prompts, first: Option<(String, u64)>) {
    end.emit(
        EventBody::ThreadStarted(SessionPayload {
            session_id: Some("s-limit".into()),
            ..Default::default()
        }),
        None,
    );
    let turn = |text: String, turn: u64, end: &AdapterEnd| {
        let ext = serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|v| v.as_object().cloned())
            .map(|mut o| {
                o.remove("hold");
                o
            });
        let result = if ext.is_some() {
            TurnResult::Error
        } else {
            TurnResult::Completed
        };
        end.emit(
            EventBody::TurnEnded(TurnEnded {
                result,
                ext,
                ..Default::default()
            }),
            Some(turn),
        );
    };
    if let Some((p, t)) = first {
        prompts.lock().unwrap().push(p.clone());
        hold(&p).await;
        turn(p, t, &end);
    }
    while let Some(cmd) = end.cmds.recv().await {
        match cmd {
            AdapterCmd::Send { text, turn: t } => {
                prompts.lock().unwrap().push(text.clone());
                hold(&text).await;
                turn(text, t, &end)
            }
            AdapterCmd::Stop | AdapterCmd::Interrupt => break,
            _ => {}
        }
    }
    end.exited(Some(0), "");
}

/// Waits until the file prompt `text`'s `hold` names exists (none named: at once).
async fn hold(text: &str) {
    let path = serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v.get("hold")?.as_str().map(PathBuf::from));
    if let Some(p) = path {
        while !p.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

/// The injected login check: each check's `<runtime>:<directory's name, or native>`, and
/// what it answers (logged in, unless `logged_out` names it).
#[derive(Clone, Default)]
struct Logins {
    checks: Arc<Mutex<Vec<String>>>,
    logged_out: Arc<Mutex<Vec<String>>>,
}

impl Logins {
    fn probe(&self) -> AccountProbe {
        let me = self.clone();
        Arc::new(move |rt: String, dir: Option<PathBuf>| {
            let me = me.clone();
            async move {
                let name = dir
                    .as_deref()
                    .and_then(Path::file_name)
                    .map_or("native".to_string(), |n| n.to_string_lossy().into_owned());
                let key = format!("{rt}:{name}");
                me.checks.lock().unwrap().push(key.clone());
                let login = if me.logged_out.lock().unwrap().contains(&key) {
                    AgentLogin::LoggedOut
                } else {
                    AgentLogin::LoggedIn
                };
                AgentInfo {
                    runtime: rt,
                    found: true,
                    login,
                    ..Default::default()
                }
            }
            .boxed()
        })
    }

    fn checks(&self) -> Vec<String> {
        self.checks.lock().unwrap().clone()
    }
}

/// A scratch home with project `api`, `[accounts.claude-code.work]` when `named`, the
/// prompts the runtime saw and the login checks.
struct Env {
    home: tempfile::TempDir,
    prompts: Prompts,
    logins: Logins,
}

impl Env {
    fn new() -> Env {
        let home = tempfile::tempdir().unwrap();
        for d in ["api", "acc/work"] {
            std::fs::create_dir_all(home.path().join(d)).unwrap();
        }
        Env {
            home,
            prompts: Prompts::default(),
            logins: Logins::default(),
        }
    }

    fn config(&self, named: bool) -> Config {
        let mut c = Config {
            default_runtime: "claude-code".into(),
            ..Default::default()
        };
        if named {
            c.accounts.entry("claude-code".into()).or_default().insert(
                "work".into(),
                AccountConfig {
                    dir: Some(self.home.path().join("acc/work")),
                    ..Default::default()
                },
            );
        }
        c.projects.insert(
            "api".into(),
            ProjectConfig {
                repo: self.home.path().join("api"),
                ..Default::default()
            },
        );
        c
    }

    async fn open(&self, named: bool) -> Hub {
        self.open_with(self.config(named), named).await
    }

    /// [`Env::open`] with `config` (`named`: it has a named account).
    async fn open_with(&self, config: Config, named: bool) -> Hub {
        // (The data directory's lock may linger briefly after a shutdown.)
        let hub = timeout(WAIT, async {
            loop {
                match Hub::open(self.opts(config.clone())).await {
                    Ok(h) => return h,
                    Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
                }
            }
        })
        .await
        .expect("opening the hub");
        if named {
            // The check at open (the native account and `work`) is done before a test counts.
            eventually("the login checks at open", || {
                self.logins.checks().len() >= 2
            })
            .await;
        }
        hub
    }

    fn opts(&self, config: Config) -> HubOptions {
        let paths = Paths::with_home(self.home.path());
        let mut o = HubOptions::new(paths.db_file(), config);
        o.accounts_root = Some(paths.accounts_dir());
        let native = self.home.path().join("native");
        let native: NativeDirFn = Arc::new(move |rt: &str| Some(native.join(rt)));
        o.native_dir = Some(native);
        o.account_probe = Some(self.logins.probe());
        o.git_tick = Some(Duration::ZERO);
        o.disk_tick = Some(Duration::ZERO);
        o.trash_tick = Some(Duration::ZERO);
        o.stall_after = Some(Duration::ZERO);
        let rt: Arc<dyn Adapter> = Arc::new(LimitAdapter(Arc::clone(&self.prompts)));
        o.adapters = Arc::new(move |name: &str| -> Option<Arc<dyn Adapter>> {
            (name == "claude-code").then(|| Arc::clone(&rt))
        });
        o.stop_timeout = Duration::from_secs(5);
        o
    }

    fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }
}

async fn eventually(what: &str, mut f: impl FnMut() -> bool) {
    timeout(WAIT, async {
        while !f() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

/// Thread `name` of project `api`, on `account` (`None`: the native one).
async fn create(hub: &Hub, name: &str, account: Option<&str>) -> String {
    let v = hub
        .new_thread(
            NewThread {
                project: "api".into(),
                name: Some(name.into()),
                account: account.map(str::to_string),
                no_worktree: true,
                ..Default::default()
            },
            "itest",
        )
        .await
        .unwrap();
    v["id"].as_str().unwrap().to_string()
}

fn send(hub: &Hub, thread: &str, prompt: &str) {
    hub.send(thread, prompt, TurnFrom::User, Source::User("itest".into()))
        .unwrap();
}

fn events(hub: &Hub, thread: &str) -> Vec<Event> {
    hub.thread_events(thread, 0, 10_000).unwrap().1
}

fn raised(evs: &[Event]) -> Vec<(Option<u64>, AttentionRaised)> {
    evs.iter()
        .filter_map(|e| match &e.body {
            EventBody::AttentionRaised(a) => Some((e.turn, a.clone())),
            _ => None,
        })
        .collect()
}

fn block_kinds(evs: &[Event]) -> Vec<(Option<u64>, String)> {
    evs.iter()
        .filter_map(|e| match &e.body {
            EventBody::DecisionMade(d) if d.site == DecisionSite::BlockKind => {
                Some((e.turn, d.chosen.clone()))
            }
            _ => None,
        })
        .collect()
}

/// Sends `ext` as the failing prompt and waits for the error item; returns it.
async fn fail_with(hub: &Hub, thread: &str, ext: Value) -> AttentionRaised {
    send(hub, thread, &ext.to_string());
    let mut out = None;
    eventually("the error item", || {
        out = raised(&events(hub, thread)).pop().map(|(_, a)| a);
        out.is_some()
    })
    .await;
    out.unwrap()
}

/// `resets_at` as the adapters write it (RFC 3339, seconds).
fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

#[tokio::test]
async fn a_limit_on_a_named_account_is_not_retried_and_its_card_names_the_account() {
    let env = Env::new();
    let hub = env.open(true).await;
    let t = create(&hub, "api", Some("work")).await;
    // Within a day: the reset is written `HH:MM` in the hub's local time.
    let at = Utc::now() + TimeDelta::minutes(13 * 60 + 30);
    let limit = json!({"kind": "usage", "window": "seven_day", "resets_at": rfc3339(at),
                       "message": "You've hit your weekly limit"});
    // A runtime section that also says transient does not make it retried.
    let ext = json!({
        "claude": {"transient": true, "message": "429"},
        "twr": {"error": "You've hit your weekly limit", "limit": limit},
    });
    let a = fail_with(&hub, &t, ext.clone()).await;
    let local = DateTime::parse_from_rfc3339(&rfc3339(at))
        .unwrap()
        .with_timezone(&Local);
    assert_eq!(
        a.summary,
        format!(
            "api: claude-code:work hit its weekly limit · resets {} (in 13h)",
            local.format("%H:%M")
        )
    );
    assert_eq!(a.reason, AttentionReason::Error);
    assert_eq!(a.options, ["dismiss"]);
    let twr = &a.ext.as_ref().unwrap()["twr"];
    assert_eq!(twr["block_kind"], "limited");
    assert_eq!(twr["limit"], limit);
    assert_eq!(twr["account"], "work");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let evs = events(&hub, &t);
    assert_eq!(block_kinds(&evs), [(Some(1), "limited".to_string())]);
    assert_eq!(env.prompts(), [ext.to_string()], "no retry");
    assert_eq!(raised(&evs).len(), 1);
    assert_eq!(hub.thread_view(&t).unwrap().turns, 1);

    // A spending limit on it.
    let s = create(&hub, "spend", Some("work")).await;
    let a = fail_with(
        &hub,
        &s,
        json!({"twr": {"error": "x", "limit": {"kind": "spend", "message": "Credit balance is too low"}}}),
    )
    .await;
    assert_eq!(
        a.summary,
        "spend: claude-code:work hit its spending limit: Credit balance is too low"
    );

    // The native account, while labels show (two accounts): under its name.
    let n = create(&hub, "web", None).await;
    let a = fail_with(
        &hub,
        &n,
        json!({"twr": {"error": "x", "limit": {"kind": "usage", "window": "five_hour",
                                                "message": "5-hour limit reached"}}}),
    )
    .await;
    assert_eq!(
        a.summary,
        "web: claude-code:default hit its 5-hour limit: 5-hour limit reached"
    );
    assert!(a.ext.as_ref().unwrap()["twr"].get("account").is_none());
    hub.shutdown().await;
}

#[tokio::test]
async fn a_limit_on_the_native_account_without_labels_says_the_runtime_alone() {
    let env = Env::new();
    let hub = env.open(false).await;
    let t = create(&hub, "api", None).await;
    let message = "You've hit your weekly limit · resets 5am (UTC)";
    let a = fail_with(
        &hub,
        &t,
        json!({"claude": {"message": "rate_limit"},
               "twr": {"error": message,
                       "limit": {"kind": "usage", "window": "seven_day", "message": message}}}),
    )
    .await;
    // No reset time: the runtime's own words.
    assert_eq!(
        a.summary,
        format!("api: claude-code hit its weekly limit: {message}")
    );
    let twr = &a.ext.as_ref().unwrap()["twr"];
    assert_eq!(twr["block_kind"], "limited");
    assert!(twr.get("account").is_none());
    assert_eq!(twr["limit"]["window"], "seven_day");
    hub.shutdown().await;
}

#[tokio::test]
async fn a_transient_failure_on_an_account_is_still_retried_once() {
    let env = Env::new();
    let hub = env.open(true).await;
    let t = create(&hub, "api", Some("work")).await;
    let ext = json!({"claude": {"transient": true, "message": "429 Too Many Requests"}});
    send(&hub, &t, &ext.to_string());
    eventually("the retry's end", || {
        events(&hub, &t)
            .iter()
            .any(|e| e.kind() == "turn.ended" && e.turn == Some(2))
    })
    .await;
    let evs = events(&hub, &t);
    assert_eq!(block_kinds(&evs), [(Some(1), "transient".to_string())]);
    assert_eq!(env.prompts(), [ext.to_string(), RETRY_PROMPT.to_string()]);
    assert!(raised(&evs).is_empty(), "{:?}", raised(&evs));
    hub.shutdown().await;
}

#[tokio::test]
async fn a_sign_in_failure_on_a_named_account_says_twr_account_login_and_checks_it() {
    let hint = twr_adapters::claude_code::translate::AUTH_HINT;
    let ext = json!({"claude": {"auth_failed": true, "transient": false},
                     "twr": {"error": format!("Not logged in · Please run /login{hint}")}});
    let native_summary = format!("web: turn 1 failed: Not logged in · Please run /login{hint}");

    // A runtime with its native account alone: the adapter's hint stays, nothing is checked
    // (the login checks cover runtimes with named accounts).
    let env = Env::new();
    let hub = env.open(false).await;
    let n = create(&hub, "web", None).await;
    let a = fail_with(&hub, &n, ext.clone()).await;
    assert_eq!(a.summary, native_summary);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(env.logins.checks().is_empty(), "{:?}", env.logins.checks());
    hub.shutdown().await;

    let env = Env::new();
    let hub = env.open(true).await;
    // The native account where the runtime has named ones: the adapter's hint stays, and its
    // login is checked (and recorded, as it changed).
    env.logins
        .logged_out
        .lock()
        .unwrap()
        .push("claude-code:native".into());
    let n = create(&hub, "web", None).await;
    let before = env.logins.checks().len();
    let a = fail_with(&hub, &n, ext.clone()).await;
    assert_eq!(a.summary, native_summary);
    assert_eq!(a.ext.as_ref().unwrap()["twr"]["block_kind"], "user_blocked");
    eventually("the native account's check", || {
        env.logins.checks()[before..] == ["claude-code:native".to_string()]
    })
    .await;
    eventually("its login recorded", || {
        let (items, _) = hub.control_read(0, 10_000).unwrap();
        items.iter().any(|(_, e)| {
            matches!(&e.body, EventBody::AccountStatus(s)
                if s.account == "default" && s.login == Login::LoggedOut)
        })
    })
    .await;

    // A named one: how to sign it in, and its login checked (and recorded, as it changed).
    env.logins
        .logged_out
        .lock()
        .unwrap()
        .push("claude-code:work".into());
    let before = env.logins.checks().len();
    let t = create(&hub, "api", Some("work")).await;
    let a = fail_with(&hub, &t, ext).await;
    assert_eq!(
        a.summary,
        "api: turn 1 failed: Not logged in · Please run /login \
         (sign in again: twr account login claude-code work)"
    );
    eventually("the account's check", || {
        env.logins.checks()[before..] == ["claude-code:work".to_string()]
    })
    .await;
    eventually("its login recorded", || {
        let (items, _) = hub.control_read(0, 10_000).unwrap();
        items.iter().any(|(_, e)| {
            matches!(&e.body, EventBody::AccountStatus(s)
                if s.account == "work" && s.login == Login::LoggedOut)
        })
    })
    .await;
    hub.shutdown().await;
}

/// A long sign-in error is cut on the card before its recovery hint, never the hint: a named
/// account's `twr account login`, and the adapter's own on the native account, come whole
/// after the cut words.
#[tokio::test]
async fn a_long_sign_in_failure_keeps_its_hint_whole() {
    let hint = twr_adapters::claude_code::translate::AUTH_HINT;
    let words = "x".repeat(500);
    let ext = json!({"claude": {"auth_failed": true, "transient": false},
                     "twr": {"error": format!("{words}{hint}")}});
    let cut = format!("{}...", "x".repeat(197));
    let env = Env::new();
    let hub = env.open(true).await;
    let t = create(&hub, "api", Some("work")).await;
    let a = fail_with(&hub, &t, ext.clone()).await;
    assert_eq!(
        a.summary,
        format!("api: turn 1 failed: {cut} (sign in again: twr account login claude-code work)")
    );
    let n = create(&hub, "web", None).await;
    let a = fail_with(&hub, &n, ext).await;
    assert_eq!(a.summary, format!("web: turn 1 failed: {cut}{hint}"));
    hub.shutdown().await;
}

/// A thread given its account's directory by a path the account is no longer configured
/// with (config names the same directory through a link now): `twr account login` signs the
/// configured path in, so its sign-in failure names both steps, the login and the rebind.
#[tokio::test]
async fn a_sign_in_failure_on_a_retained_path_names_the_rebind() {
    let env = Env::new();
    let hub = env.open(true).await;
    let t = create(&hub, "api", Some("work")).await;
    send(&hub, &t, "hi");
    eventually("the first turn", || env.prompts().len() == 1).await;
    eventually("its end", || {
        events(&hub, &t)
            .iter()
            .any(|e| matches!(e.body, EventBody::TurnEnded(_)))
    })
    .await;
    hub.shutdown().await;
    drop(hub);
    let alias = env.home.path().join("acc/work-alias");
    std::os::unix::fs::symlink(env.home.path().join("acc/work"), &alias).unwrap();
    let mut config = env.config(true);
    config
        .accounts
        .get_mut("claude-code")
        .unwrap()
        .get_mut("work")
        .unwrap()
        .dir = Some(alias);
    let hub = env.open_with(config, true).await;
    let hint = twr_adapters::claude_code::translate::AUTH_HINT;
    let ext = json!({"claude": {"auth_failed": true, "transient": false},
                     "twr": {"error": format!("Not logged in · Please run /login{hint}")}});
    let a = fail_with(&hub, &t, ext).await;
    assert_eq!(
        a.summary,
        "api: turn 2 failed: Not logged in · Please run /login (sign in again: twr account \
         login claude-code work, then twr model api --account work)"
    );
    hub.shutdown().await;
}

/// A thread moved to another account while its turn ran (the move applies to its next
/// session): the turn's limit card names the account it ran on, and so does a sign-in
/// failure's hint, whose check is of that account.
#[tokio::test]
async fn a_failure_after_a_move_names_the_account_the_turn_ran_on() {
    let env = Env::new();
    let hub = env.open(true).await;
    let user = Caller::user("itest");
    // (Off the thread's own pin, `work`: the user's override.)
    let to_native = || ModelRequest {
        account: Some("default"),
        outside_pin: true,
        ..Default::default()
    };
    let limit = json!({"kind": "usage", "window": "five_hour", "message": "5-hour limit reached"});
    let hint = twr_adapters::claude_code::translate::AUTH_HINT;
    let auth = json!({"claude": {"auth_failed": true, "transient": false},
                      "twr": {"error": format!("Not logged in · Please run /login{hint}")}});
    for (name, ext, want) in [
        (
            "lim",
            json!({"twr": {"error": "x", "limit": limit}}),
            "lim: claude-code:work hit its 5-hour limit: 5-hour limit reached".to_string(),
        ),
        (
            "auth",
            auth,
            "auth: turn 1 failed: Not logged in · Please run /login (sign in again: twr \
             account login claude-code work)"
                .to_string(),
        ),
    ] {
        let t = create(&hub, name, Some("work")).await;
        let go = env.home.path().join(format!("go-{name}"));
        let mut prompt = ext.as_object().unwrap().clone();
        prompt.insert("hold".into(), json!(go));
        let before = env.prompts().len();
        send(&hub, &t, &Value::Object(prompt).to_string());
        eventually("the turn running", || env.prompts().len() == before + 1).await;
        let r = hub.set_thread_model(&t, to_native(), &user).await.unwrap();
        assert_eq!(r["account_changed"], true, "{r}");
        assert_eq!(hub.thread_view(&t).unwrap().account, None);
        let checks = env.logins.checks().len();
        std::fs::write(&go, "").unwrap();
        let mut out = None;
        eventually("the error item", || {
            out = raised(&events(&hub, &t)).pop().map(|(_, a)| a);
            out.is_some()
        })
        .await;
        let a = out.unwrap();
        assert_eq!(a.summary, want);
        if name == "lim" {
            assert_eq!(a.ext.as_ref().unwrap()["twr"]["account"], "work");
        } else {
            eventually("the old account's check", || {
                env.logins.checks()[checks..] == ["claude-code:work".to_string()]
            })
            .await;
        }
    }
    hub.shutdown().await;
}
