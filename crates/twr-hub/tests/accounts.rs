//! Accounts in the hub (`hub::accounts`): config declarations and twr's records with their
//! sources, `account.add` / `account.remove` / `account.status` / `account.list` and who may
//! call them, the login checks (recorded only on change, flipping when a runtime shows its
//! labels), what a restart keeps, the start's refusals, `hub.info` and the thread JSON.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::FutureExt;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::timeout;
use twr_adapters::fake::FakeAdapter;
use twr_core::adapter::{Adapter, AdapterEnd, AdapterHandle, AdapterInfo, SpawnConfig, Tier};
use twr_core::client::Client;
use twr_core::config::{AccountConfig, Config};
use twr_core::event::{
    AccountAdded, Event, EventBody, SessionPayload, Source, ThreadCreated, HUB_THREAD,
};
use twr_core::paths::Paths;
use twr_core::proto::{codes, RpcError};
use twr_core::runner::{AgentInfo, AgentLogin};
use twr_hub::hub::{AccountProbe, NativeDirFn};
use twr_hub::store::{AppendExtras, Store};
use twr_hub::{server, Hub, HubOptions};

const WAIT: Duration = Duration::from_secs(10);

/// What the injected login check answers, by `<runtime>:<directory's name, or native>`
/// (default: found and logged in), and how many checks ran.
#[derive(Clone, Default)]
struct Logins {
    answers: Arc<Mutex<HashMap<String, (bool, AgentLogin)>>>,
    calls: Arc<AtomicUsize>,
}

impl Logins {
    fn key(rt: &str, dir: Option<&Path>) -> String {
        let name = dir
            .and_then(|d| d.file_name())
            .map_or("native".to_string(), |n| n.to_string_lossy().into_owned());
        format!("{rt}:{name}")
    }

    fn set(&self, key: &str, found: bool, login: AgentLogin) {
        self.answers
            .lock()
            .unwrap()
            .insert(key.into(), (found, login));
    }

    fn probe(&self) -> AccountProbe {
        let me = self.clone();
        Arc::new(move |rt: String, dir: Option<PathBuf>| {
            let me = me.clone();
            async move {
                me.calls.fetch_add(1, Ordering::SeqCst);
                let key = Logins::key(&rt, dir.as_deref());
                let (found, login) = me
                    .answers
                    .lock()
                    .unwrap()
                    .get(&key)
                    .copied()
                    .unwrap_or((true, AgentLogin::LoggedIn));
                AgentInfo {
                    runtime: rt,
                    found,
                    login,
                    method: (login == AgentLogin::LoggedIn).then(|| "claude.ai".into()),
                    ..Default::default()
                }
            }
            .boxed()
        })
    }
}

/// An orchestrator runtime (`grok-build`) that starts and stays up; its config goes out on
/// `spawns` (for its token).
struct OrchAdapter(mpsc::UnboundedSender<(SpawnConfig, AdapterEnd)>);

impl Adapter for OrchAdapter {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "grok-build",
            tier: Tier::Headless,
            description: "test orchestrator runtime",
        }
    }

    fn spawn(&self, cfg: SpawnConfig, _prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let (h, end) = AdapterHandle::pair();
        end.emit(
            EventBody::ThreadStarted(SessionPayload {
                session_id: Some("s-orch".into()),
                ..Default::default()
            }),
            None,
        );
        let _ = self.0.send((cfg, end));
        Ok(h)
    }
}

struct TestHub {
    paths: Paths,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
    spawns: mpsc::UnboundedReceiver<(SpawnConfig, AdapterEnd)>,
}

/// The runtimes' own directories in these tests: `<home>/native/<runtime>`.
fn native_dirs(home: &Path) -> NativeDirFn {
    let home = home.to_path_buf();
    Arc::new(move |rt: &str| Some(home.join("native").join(rt)))
}

fn hub_options(home: &Path, config: Config, probe: Option<AccountProbe>) -> HubOptions {
    let paths = Paths::with_home(home);
    let mut opts = HubOptions::new(paths.db_file(), config);
    opts.accounts_root = Some(paths.accounts_dir());
    opts.native_dir = Some(native_dirs(home));
    // The hub's environment: `CODEX_HOME` set (to its native directory), nothing else.
    let codex = home.join("native").join("codex");
    opts.native_var = Some(Arc::new(move |k: &str| {
        (k == "CODEX_HOME").then(|| codex.clone().into_os_string())
    }));
    opts.account_probe = probe;
    opts.git_tick = Some(Duration::ZERO);
    opts.disk_tick = Some(Duration::ZERO);
    opts.trash_tick = Some(Duration::ZERO);
    opts
}

async fn start(home: &Path, config: Config, probe: Option<AccountProbe>) -> TestHub {
    let paths = Paths::with_home(home);
    let listener = server::bind(&paths.socket).await.unwrap();
    let mut config = config;
    config.default_runtime = "fake".into();
    config.orchestrator.runtime = Some("grok-build".into());
    let mut opts = hub_options(home, config, probe);
    let (tx, spawns) = mpsc::unbounded_channel();
    let orch: Arc<dyn Adapter> = Arc::new(OrchAdapter(tx));
    opts.adapters = Arc::new(move |name: &str| -> Option<Arc<dyn Adapter>> {
        match name {
            "fake" => Some(Arc::new(FakeAdapter::new())),
            "grok-build" => Some(Arc::clone(&orch)),
            _ => None,
        }
    });
    opts.socket = Some(paths.socket.clone());
    opts.orchestrator_token_unverified = true;
    let hub = Hub::open(opts).await.unwrap();
    let task = tokio::spawn(server::serve(hub, listener, paths.socket.clone(), false));
    TestHub {
        paths,
        task,
        spawns,
    }
}

impl TestHub {
    async fn client(&self) -> Client {
        let mut c = Client::connect(&self.paths.socket).await.unwrap();
        c.hello(json!({ "client": "itest" })).await.unwrap();
        c
    }

    async fn stop(self) {
        let mut c = self.client().await;
        c.call("hub.shutdown", Value::Null).await.unwrap();
        self.task.await.unwrap().unwrap();
    }
}

fn code(e: &anyhow::Error) -> String {
    e.downcast_ref::<RpcError>()
        .map(|r| r.code.clone())
        .unwrap_or_else(|| format!("{e:#}"))
}

fn message(e: &anyhow::Error) -> String {
    e.downcast_ref::<RpcError>()
        .map(|r| r.message.clone())
        .unwrap_or_else(|| format!("{e:#}"))
}

fn account(cfg: &mut Config, rt: &str, name: &str, dir: Option<&Path>, native: bool) {
    cfg.accounts.entry(rt.into()).or_default().insert(
        name.into(),
        AccountConfig {
            dir: dir.map(Path::to_path_buf),
            native,
            ..Default::default()
        },
    );
}

/// `list`'s account `name` of runtime `rt`.
fn entry<'a>(list: &'a Value, rt: &str, name: &str) -> &'a Value {
    list["runtimes"][rt]["accounts"]
        .as_array()
        .unwrap_or_else(|| panic!("no {rt} in {list}"))
        .iter()
        .find(|a| a["name"] == name)
        .unwrap_or_else(|| panic!("no {rt}:{name} in {list}"))
}

/// The control stream's events of `kind`.
async fn control_of(c: &mut Client, kind: &str) -> Vec<Value> {
    let r = c
        .call("control.read", json!({"since": 0, "limit": 10000}))
        .await
        .unwrap();
    r["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["event"].clone())
        .filter(|e| e["kind"] == kind)
        .collect()
}

/// `account.add {runtime, account: name, native: true}`.
async fn name_native(c: &mut Client, rt: &str, name: &str) -> anyhow::Result<Value> {
    c.call(
        "account.add",
        json!({"runtime": rt, "account": name, "native": true}),
    )
    .await
}

/// `account.status {refresh: true}`.
async fn refresh(c: &mut Client) -> Value {
    c.call("account.status", json!({"refresh": true}))
        .await
        .unwrap()
}

/// Writes `events` into the log at `home` before any hub opens it (threads made with an
/// account, which only the next work item's `thread.new` can make).
fn seed(home: &Path, events: &[Event]) {
    let paths = Paths::with_home(home);
    let mut store = Store::open(&paths.db_file()).unwrap();
    for ev in events {
        store
            .append(ev, ev.is_control(), AppendExtras::default())
            .unwrap();
    }
}

fn thread_on(id: &str, name: &str, runtime: &str, account: Option<&str>) -> Event {
    let ext = account.map(|a| json!({"twr": {"account": a}}).as_object().cloned().unwrap());
    Event::new(
        id,
        1,
        None,
        Source::User("itest".into()),
        EventBody::ThreadCreated(ThreadCreated {
            name: name.into(),
            project: Some("api".into()),
            box_name: "local".into(),
            runtime: runtime.into(),
            mode: "default".into(),
            ext,
            ..Default::default()
        }),
    )
}

fn added(seq: u64, rt: &str, name: &str, dir: &Path) -> Event {
    Event::new(
        HUB_THREAD,
        seq,
        None,
        Source::User("itest".into()),
        EventBody::AccountAdded(AccountAdded {
            runtime: rt.into(),
            account: name.into(),
            dir: Some(dir.to_string_lossy().into_owned()),
            ..Default::default()
        }),
    )
}

/// Config declarations and twr's records side by side, each with its source; adding a config
/// account records nothing (also as a dry run), adding a record again is nothing new, and
/// another directory for either is refused.
#[tokio::test]
async fn config_and_records_with_their_sources() {
    let home = tempfile::tempdir().unwrap();
    let cfg_work = home.path().join("cfg").join("work");
    let mut config = Config::default();
    account(&mut config, "claude-code", "work", Some(&cfg_work), false);
    account(&mut config, "claude-code", "personal", None, true);
    let th = start(home.path(), config, None).await;
    let mut c = th.client().await;
    let root = th.paths.accounts_dir();

    // A config account: nothing recorded, as a dry run or not.
    for dry in [true, false] {
        let r = c
            .call(
                "account.add",
                json!({"runtime": "claude-code", "account": "work", "dry_run": dry}),
            )
            .await
            .unwrap();
        assert_eq!(r["config"], true, "{r}");
        assert_eq!(r["recorded"], false, "{r}");
        assert_eq!(r["source"], "config", "{r}");
        assert_eq!(r["dir"], json!(cfg_work), "{r}");
    }
    let e = c
        .call(
            "account.add",
            json!({"runtime": "claude-code", "account": "work", "dir": home.path().join("x")}),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::CONFLICT);
    assert!(message(&e).contains("config.toml wins"), "{}", message(&e));

    // A new one: its dry run records nothing, then it is recorded under the accounts root.
    let r = c
        .call(
            "account.add",
            json!({"runtime": "claude-code", "account": "side", "dry_run": true}),
        )
        .await
        .unwrap();
    let side_dir = root.join("claude-code").join("side");
    assert_eq!(
        (&r["dir"], &r["recorded"], &r["existing"], &r["source"]),
        (
            &json!(side_dir),
            &json!(false),
            &json!(false),
            &json!("recorded")
        ),
        "{r}"
    );
    assert!(control_of(&mut c, "account.added").await.is_empty());
    let r = c
        .call(
            "account.add",
            json!({"runtime": "claude-code", "account": "side", "share": "none"}),
        )
        .await
        .unwrap();
    assert_eq!(
        (&r["recorded"], &r["share"]),
        (&json!(true), &json!("none")),
        "{r}"
    );
    let recorded = control_of(&mut c, "account.added").await;
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert_eq!(recorded[0]["data"]["account"], "side");
    assert_eq!(recorded[0]["data"]["dir"], json!(side_dir));
    assert_eq!(recorded[0]["data"]["by"], "user");
    assert_eq!(recorded[0]["source"], "user:itest");
    // Again as it is: nothing new; with another directory or share: refused.
    let r = c
        .call(
            "account.add",
            json!({"runtime": "claude-code", "account": "side", "dir": side_dir}),
        )
        .await
        .unwrap();
    assert_eq!(
        (&r["existing"], &r["recorded"]),
        (&json!(true), &json!(false))
    );
    for p in [
        json!({"runtime": "claude-code", "account": "side", "dir": home.path().join("else")}),
        json!({"runtime": "claude-code", "account": "side", "share": "config"}),
    ] {
        let e = c.call("account.add", p).await.unwrap_err();
        assert_eq!(code(&e), codes::CONFLICT);
        assert!(message(&e).contains("remove it first"), "{}", message(&e));
    }
    assert_eq!(control_of(&mut c, "account.added").await.len(), 1);

    let list = c.call("account.list", json!({})).await.unwrap();
    assert_eq!(list["runtimes"]["claude-code"]["native"], "personal");
    assert_eq!(list["runtimes"]["claude-code"]["shown"], true);
    let names: Vec<&str> = list["runtimes"]["claude-code"]["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["personal", "side", "work"]);
    let personal = entry(&list, "claude-code", "personal");
    assert_eq!(
        (&personal["native"], &personal["source"], &personal["dir"]),
        (&json!(true), &json!("config"), &Value::Null)
    );
    assert_eq!(entry(&list, "claude-code", "work")["source"], "config");
    assert_eq!(entry(&list, "claude-code", "side")["source"], "recorded");
    assert_eq!(entry(&list, "claude-code", "side")["threads"], 0);
    // Every runtime that takes accounts is listed, the native account alone if nothing else.
    let codex = entry(&list, "codex", "default");
    assert_eq!(
        (&codex["source"], &codex["native"]),
        (&json!("native"), &json!(true))
    );
    assert_eq!(list["runtimes"]["codex"]["shown"], false);
    assert!(list["now"].is_string());
    // One runtime.
    let one = c
        .call("account.list", json!({"runtime": "codex"}))
        .await
        .unwrap();
    assert_eq!(one["runtimes"].as_object().unwrap().len(), 1, "{one}");
    let e = c
        .call("account.list", json!({"runtime": "fake"}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::BAD_REQUEST);
    // `hub.info` has them, without directories.
    let info = c.call("hub.info", Value::Null).await.unwrap();
    let side = info["accounts"]["claude-code"]["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "side")
        .unwrap()
        .clone();
    assert!(side.get("dir").is_none(), "{side}");
    assert_eq!(info["accounts"]["claude-code"]["shown"], true);
    th.stop().await;
}

/// What `account.add` refuses: a runtime that takes no accounts, bad names (`..` among them:
/// a default directory never leaves the accounts root), a relative directory, a runtime's own
/// directory, another account's directory, a native account with a directory, and a named
/// account with the native account's name.
#[tokio::test]
async fn add_refuses_conflicts() {
    let home = tempfile::tempdir().unwrap();
    let cfg_work = home.path().join("cfg").join("work");
    let mut config = Config::default();
    account(&mut config, "codex", "team", None, false);
    account(&mut config, "claude-code", "work", Some(&cfg_work), false);
    let th = start(home.path(), config, None).await;
    let mut c = th.client().await;
    let refused = |p: Value, want: &'static str, says: &'static str| (p, want, says);
    let cases = [
        refused(
            json!({"runtime": "fake", "account": "x"}),
            codes::BAD_REQUEST,
            "does not take accounts",
        ),
        refused(
            json!({"runtime": "codex", "account": ".."}),
            codes::BAD_REQUEST,
            "not an account name",
        ),
        refused(
            json!({"runtime": "codex", "account": "../x"}),
            codes::BAD_REQUEST,
            "not an account name",
        ),
        refused(
            json!({"runtime": "codex", "account": "a", "dir": "rel/dir"}),
            codes::BAD_REQUEST,
            "not an absolute path",
        ),
        refused(
            json!({"runtime": "codex", "account": "a", "dir": home.path().join("native/codex")}),
            codes::CONFLICT,
            "codex's own directory",
        ),
        refused(
            json!({"runtime": "codex", "account": "a", "dir": home.path().join("native/claude-code")}),
            codes::CONFLICT,
            "claude-code's own directory",
        ),
        refused(
            json!({"runtime": "codex", "account": "a", "dir": cfg_work}),
            codes::CONFLICT,
            "claude-code:work's directory already",
        ),
        refused(
            json!({"runtime": "grok-build", "account": "a", "dir": th.paths.accounts_dir().join("codex/team")}),
            codes::CONFLICT,
            "codex:team's directory already",
        ),
        refused(
            json!({"runtime": "codex", "account": "me", "native": true, "dir": "/x"}),
            codes::BAD_REQUEST,
            "native account has no dir",
        ),
        refused(
            json!({"runtime": "codex", "account": "default"}),
            codes::CONFLICT,
            "native account",
        ),
        refused(
            json!({"runtime": "codex", "account": "x", "share": "all"}),
            codes::BAD_REQUEST,
            "unknown variant",
        ),
    ];
    for (p, want, says) in cases {
        let e = c.call("account.add", p.clone()).await.unwrap_err();
        assert_eq!(code(&e), want, "{p}: {}", message(&e));
        assert!(message(&e).contains(says), "{p}: {}", message(&e));
    }
    // A recorded account's directory is taken too.
    c.call(
        "account.add",
        json!({"runtime": "codex", "account": "b", "dir": home.path().join("b")}),
    )
    .await
    .unwrap();
    let e = c
        .call(
            "account.add",
            json!({"runtime": "grok-build", "account": "b", "dir": home.path().join("b/")}),
        )
        .await
        .unwrap_err();
    assert!(
        message(&e).contains("codex:b's directory already"),
        "{}",
        message(&e)
    );
    assert!(message(&e).contains("twr account add"), "{}", message(&e));
    assert_eq!(control_of(&mut c, "account.added").await.len(), 1);
    th.stop().await;
}

/// `native: true` names the native account: recorded, the same again is nothing new, a later
/// name replaces it; refused with a name another account has, or when config.toml names the
/// native account otherwise (the config's name is a config account). A dry run of a named
/// account `after_native_name` answers as if that rename were done, the rename checked too.
#[tokio::test]
async fn naming_the_native_account() {
    let home = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    account(&mut config, "claude-code", "personal", None, true);
    let th = start(home.path(), config, None).await;
    let mut c = th.client().await;
    let r = name_native(&mut c, "codex", "me").await.unwrap();
    assert_eq!(
        (&r["native"], &r["recorded"], &r["dir"], &r["source"]),
        (&json!(true), &json!(true), &Value::Null, &json!("recorded")),
        "{r}"
    );
    let r = name_native(&mut c, "codex", "me").await.unwrap();
    assert_eq!(
        (&r["existing"], &r["recorded"]),
        (&json!(true), &json!(false))
    );
    let list = c.call("account.list", json!({})).await.unwrap();
    assert_eq!(list["runtimes"]["codex"]["native"], "me");
    assert_eq!(entry(&list, "codex", "me")["source"], "recorded");
    name_native(&mut c, "codex", "home").await.unwrap();
    let list = c.call("account.list", json!({})).await.unwrap();
    assert_eq!(list["runtimes"]["codex"]["native"], "home");
    // Another account's name.
    c.call(
        "account.add",
        json!({"runtime": "codex", "account": "team", "dir": home.path().join("team")}),
    )
    .await
    .unwrap();
    let e = name_native(&mut c, "codex", "team").await.unwrap_err();
    assert_eq!(code(&e), codes::CONFLICT);
    // Config names claude-code's.
    let r = name_native(&mut c, "claude-code", "personal")
        .await
        .unwrap();
    assert_eq!(
        (&r["config"], &r["recorded"]),
        (&json!(true), &json!(false))
    );
    let e = name_native(&mut c, "claude-code", "other")
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::CONFLICT);
    assert!(
        message(&e).contains("config.toml names claude-code's native account personal"),
        "{}",
        message(&e)
    );
    // A named account under the native account's name.
    let e = c
        .call(
            "account.add",
            json!({"runtime": "codex", "account": "home"}),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::CONFLICT);
    // ... is fine as a dry run after a rename that frees it (`after_native_name`), which is
    // checked too; nothing is recorded.
    let before = control_of(&mut c, "account.added").await.len();
    let dry = |after: &str| json!({"runtime": "codex", "account": "home", "dry_run": true, "after_native_name": after});
    let r = c.call("account.add", dry("mine")).await.unwrap();
    assert_eq!(
        (&r["account"], &r["native"], &r["recorded"], &r["existing"]),
        (&json!("home"), &json!(false), &json!(false), &json!(false)),
        "{r}"
    );
    let e = c.call("account.add", dry("team")).await.unwrap_err();
    assert_eq!(code(&e), codes::CONFLICT);
    assert!(
        message(&e).contains("codex:team is another account of codex already"),
        "{}",
        message(&e)
    );
    // (A rename config forbids: refused as that rename is.)
    let e = c
        .call(
            "account.add",
            json!({"runtime": "claude-code", "account": "personal", "dry_run": true,
                   "after_native_name": "other"}),
        )
        .await
        .unwrap_err();
    assert!(
        message(&e).contains("config.toml names claude-code's native account personal"),
        "{}",
        message(&e)
    );
    for bad in [
        json!({"runtime": "codex", "account": "home", "after_native_name": "mine"}),
        json!({"runtime": "codex", "account": "x", "native": true, "dry_run": true,
               "after_native_name": "mine"}),
        dry("home"),
    ] {
        let e = c.call("account.add", bad.clone()).await.unwrap_err();
        assert_eq!(code(&e), codes::BAD_REQUEST, "{bad}");
    }
    assert_eq!(control_of(&mut c, "account.added").await.len(), before);
    let list = c.call("account.list", json!({})).await.unwrap();
    assert_eq!(list["runtimes"]["codex"]["native"], "home");
    th.stop().await;
}

/// `account.remove`: of a config account only its record (none: refused, with where to remove
/// it); a recorded one is gone (listed as removed); one threads run on needs `force`, and
/// those threads then show `account_removed`; `move_to` is not yet. A restart keeps the
/// records, and a record that config.toml declares since counts as both.
#[tokio::test]
async fn removing_accounts() {
    let home = tempfile::tempdir().unwrap();
    let busy_dir = home.path().join("busy");
    seed(
        home.path(),
        &[
            added(1, "claude-code", "busy", &busy_dir),
            added(2, "claude-code", "both", &home.path().join("both")),
            thread_on("t-api", "api", "claude-code", Some("busy")),
            thread_on("t-web", "web", "claude-code", None),
        ],
    );
    let mut config = Config::default();
    account(&mut config, "claude-code", "work", None, false);
    account(
        &mut config,
        "claude-code",
        "both",
        Some(&home.path().join("both")),
        false,
    );
    let th = start(home.path(), config, None).await;
    let mut c = th.client().await;

    let e = c
        .call(
            "account.remove",
            json!({"runtime": "claude-code", "account": "work"}),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::BAD_REQUEST);
    assert_eq!(
        message(&e),
        "claude-code:work is declared in config.toml ([accounts.claude-code.work]); remove it \
         there and restart the hub"
    );
    let list = c.call("account.list", json!({})).await.unwrap();
    assert_eq!(entry(&list, "claude-code", "both")["source"], "both");
    let r = c
        .call(
            "account.remove",
            json!({"runtime": "claude-code", "account": "both"}),
        )
        .await
        .unwrap();
    assert_eq!(
        (&r["removed"], &r["config"]),
        (&json!(true), &json!(true)),
        "{r}"
    );
    assert!(r["message"].as_str().unwrap().contains("it stays"), "{r}");
    let list = c.call("account.list", json!({})).await.unwrap();
    let both = entry(&list, "claude-code", "both");
    assert_eq!(
        (&both["source"], &both["removed"]),
        (&json!("config"), &json!(false))
    );

    // Threads on it: `force`, and they stay on it.
    let list = c.call("account.list", json!({})).await.unwrap();
    assert_eq!(entry(&list, "claude-code", "busy")["threads"], 1);
    assert_eq!(entry(&list, "claude-code", "default")["threads"], 1);
    let e = c
        .call(
            "account.remove",
            json!({"runtime": "claude-code", "account": "busy", "move_to": "work"}),
        )
        .await
        .unwrap_err();
    // (This hub has no claude-code runtime to move its thread to: nothing is removed.
    // `tests/account_moves.rs` has the moves.)
    assert_eq!(code(&e), codes::CONFLICT);
    assert!(
        message(&e).contains("so nothing was removed: api ("),
        "{}",
        message(&e)
    );
    let e = c
        .call(
            "account.remove",
            json!({"runtime": "claude-code", "account": "busy"}),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::CONFLICT);
    assert!(message(&e).contains("api"), "{}", message(&e));
    let r = c
        .call(
            "account.remove",
            json!({"runtime": "claude-code", "account": "busy", "force": true}),
        )
        .await
        .unwrap();
    assert_eq!(r["threads"], json!(["api"]), "{r}");
    let list = c.call("account.list", json!({})).await.unwrap();
    assert_eq!(entry(&list, "claude-code", "busy")["removed"], true);
    let e = c
        .call(
            "account.remove",
            json!({"runtime": "claude-code", "account": "busy", "force": true}),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::NOT_FOUND);
    let e = c
        .call(
            "account.remove",
            json!({"runtime": "claude-code", "account": "ghost"}),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::NOT_FOUND);
    let removed = control_of(&mut c, "account.removed").await;
    assert_eq!(removed.len(), 2, "{removed:?}");
    assert_eq!(removed[1]["data"]["by"], "user");

    // The thread JSON: its account always, `account_removed` only for a removed one.
    let threads = c.call("thread.list", Value::Null).await.unwrap();
    let t = |name: &str| {
        threads
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == name)
            .unwrap()
            .clone()
    };
    let api = t("api");
    assert_eq!(
        (
            &api["account"],
            &api["account_shown"],
            &api["account_removed"]
        ),
        (&json!("busy"), &json!(true), &json!(true)),
        "{api}"
    );
    let web = t("web");
    assert_eq!(
        (&web["account"], &web["account_shown"]),
        (&json!("default"), &json!(true))
    );
    assert!(web.get("account_removed").is_none(), "{web}");
    let got = c
        .call("thread.get", json!({"thread": "api"}))
        .await
        .unwrap();
    assert_eq!(got["account"], "busy");

    // A recorded one with nothing on it; then removing the native account (it needs force:
    // `web` runs on it).
    c.call(
        "account.add",
        json!({"runtime": "claude-code", "account": "spare", "dir": home.path().join("spare")}),
    )
    .await
    .unwrap();
    let r = c
        .call(
            "account.remove",
            json!({"runtime": "claude-code", "account": "spare"}),
        )
        .await
        .unwrap();
    assert_eq!((&r["removed"], &r["threads"]), (&json!(true), &json!([])));
    let e = c
        .call(
            "account.remove",
            json!({"runtime": "claude-code", "account": "default"}),
        )
        .await
        .unwrap_err();
    assert!(message(&e).contains("web"), "{}", message(&e));
    let r = c
        .call(
            "account.remove",
            json!({"runtime": "claude-code", "account": "default", "force": true}),
        )
        .await
        .unwrap();
    assert_eq!(r["native"], true);
    let threads = c.call("thread.list", Value::Null).await.unwrap();
    let web = threads
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "web")
        .unwrap();
    assert_eq!(web["account_removed"], true, "{web}");
    th.stop().await;

    // A restart keeps the records (and the removals).
    let mut config = Config::default();
    account(&mut config, "claude-code", "work", None, false);
    let th = start(home.path(), config, None).await;
    let mut c = th.client().await;
    let list = c.call("account.list", json!({})).await.unwrap();
    assert_eq!(entry(&list, "claude-code", "busy")["removed"], true);
    assert_eq!(entry(&list, "claude-code", "default")["removed"], true);
    let both = entry(&list, "claude-code", "both");
    assert_eq!(
        (&both["source"], &both["removed"]),
        (&json!("recorded"), &json!(true))
    );
    th.stop().await;
}

/// A recorded native account that config.toml declares since is both; removing it removes
/// only its record (one `account.removed`), after which it is config's alone, and another
/// removal is refused as for any config account, recording nothing; a restart keeps it so.
#[tokio::test]
async fn a_configured_native_account_loses_only_its_record() {
    let home = tempfile::tempdir().unwrap();
    seed(
        home.path(),
        &[Event::new(
            HUB_THREAD,
            1,
            None,
            Source::User("itest".into()),
            EventBody::AccountAdded(AccountAdded {
                runtime: "codex".into(),
                account: "home".into(),
                native: true,
                ..Default::default()
            }),
        )],
    );
    let mut config = Config::default();
    account(&mut config, "codex", "home", None, true);
    let th = start(home.path(), config.clone(), None).await;
    let mut c = th.client().await;
    let list = c.call("account.list", json!({})).await.unwrap();
    assert_eq!(entry(&list, "codex", "home")["source"], "both");
    let remove = json!({"runtime": "codex", "account": "home"});
    let r = c.call("account.remove", remove.clone()).await.unwrap();
    assert_eq!(
        (&r["removed"], &r["config"], &r["native"]),
        (&json!(true), &json!(true), &json!(true)),
        "{r}"
    );
    let refused = "codex:home is declared in config.toml ([accounts.codex.home]); remove it \
                   there and restart the hub";
    let check = |list: &Value| {
        let home = entry(list, "codex", "home");
        assert_eq!(
            (&home["source"], &home["removed"], &home["native"]),
            (&json!("config"), &json!(false), &json!(true)),
            "{home}"
        );
    };
    check(&c.call("account.list", json!({})).await.unwrap());
    let e = c.call("account.remove", remove.clone()).await.unwrap_err();
    assert_eq!(
        (code(&e), message(&e)),
        (codes::BAD_REQUEST.into(), refused.into())
    );
    assert_eq!(control_of(&mut c, "account.removed").await.len(), 1);
    th.stop().await;

    let th = start(home.path(), config, None).await;
    let mut c = th.client().await;
    check(&c.call("account.list", json!({})).await.unwrap());
    let e = c.call("account.remove", remove).await.unwrap_err();
    assert_eq!(
        (code(&e), message(&e)),
        (codes::BAD_REQUEST.into(), refused.into())
    );
    assert_eq!(control_of(&mut c, "account.removed").await.len(), 1);
    th.stop().await;
}

/// The orchestrator lists the accounts without their directories (nor the runtimes' own ones,
/// nor their selector variables, which a user's list has) and may not add, remove or check
/// them. (A thread's agent may call none of them: `AGENT_METHODS`, checked by
/// `tests/agents.rs`.)
#[tokio::test]
async fn the_orchestrator_lists_accounts_only() {
    let home = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    account(&mut config, "claude-code", "work", None, false);
    let mut th = start(home.path(), config, None).await;
    let mut c = th.client().await;
    for name in ["w1", "w2"] {
        c.call(
            "thread.new",
            json!({"project": home.path(), "runtime": "fake", "name": name}),
        )
        .await
        .unwrap();
    }
    let r = c
        .call("say", json!({"message": "coordinate the release"}))
        .await
        .unwrap();
    assert_eq!(r["action"], "delegate", "{r}");
    let (cfg, end) = timeout(WAIT, th.spawns.recv()).await.unwrap().unwrap();
    let mcp: Value = serde_json::from_str(
        &std::fs::read_to_string(cfg.mcp_config_file.as_ref().unwrap()).unwrap(),
    )
    .unwrap();
    let token = mcp["mcpServers"]["twr"]["env"]["TWR_ORCH_TOKEN"]
        .as_str()
        .unwrap()
        .to_string();
    let mut o = Client::connect(&th.paths.socket).await.unwrap();
    o.hello(json!({"client": "orchestrator", "token": token}))
        .await
        .unwrap();
    let list = o.call("account.list", json!({})).await.unwrap();
    assert_eq!(entry(&list, "claude-code", "work")["source"], "config");
    assert!(!list.to_string().contains("\"dir\""), "{list}");
    assert!(!list.to_string().contains("native_dir"), "{list}");
    assert!(!list.to_string().contains("native_var"), "{list}");
    let mine = c.call("account.list", json!({})).await.unwrap();
    assert!(entry(&mine, "claude-code", "work")["dir"].is_string());
    // A user's has each runtime's own directory and selector as the hub has them.
    let native = home.path().join("native");
    for (rt, var, value) in [
        ("claude-code", "CLAUDE_CONFIG_DIR", None),
        ("codex", "CODEX_HOME", Some(native.join("codex"))),
        ("grok-build", "GROK_HOME", None),
    ] {
        let e = &mine["runtimes"][rt];
        assert_eq!(e["native_dir"], json!(native.join(rt)), "{mine}");
        let mut want = json!({"name": var, "set": value.is_some()});
        if let Some(v) = value {
            want["value"] = json!(v);
        }
        assert_eq!(e["native_var"], want, "{mine}");
    }
    let one = c
        .call("account.list", json!({"runtime": "codex"}))
        .await
        .unwrap();
    assert_eq!(one["runtimes"]["codex"]["native_var"]["set"], true, "{one}");
    let status = c.call("account.status", json!({})).await.unwrap();
    assert_eq!(
        status["runtimes"]["codex"]["native_dir"],
        json!(native.join("codex"))
    );
    // The orchestrator's own thread counts on its runtime's native account.
    assert_eq!(entry(&mine, "grok-build", "default")["threads"], 1);
    for (m, p) in [
        ("account.add", json!({"runtime": "codex", "account": "x"})),
        (
            "account.remove",
            json!({"runtime": "claude-code", "account": "work"}),
        ),
        ("account.status", json!({"refresh": true})),
    ] {
        let e = o.call(m, p).await.unwrap_err();
        assert_eq!(code(&e), codes::UNAUTHORIZED, "{m}");
    }
    let info = o.call("hub.info", Value::Null).await.unwrap();
    assert!(info["accounts"]["claude-code"].is_object(), "{info}");
    assert!(!info["accounts"].to_string().contains("\"dir\""));
    assert!(control_of(&mut c, "account.added").await.is_empty());
    drop(o);
    drop(end);
    th.stop().await;
}

/// The login checks (an injected probe): each account's state is recorded only when it
/// changes, a runtime shows its labels while two of its accounts count (a named one logging
/// in and out flips them), a missing CLI or directory is `missing`, `account.add` checks the
/// new account, and a restart keeps the states.
/// A check after a login twr ran (`account.status {refresh, after_login}`, what `twr account
/// login` and `add` ask) is recorded when it finds the account logged in, unchanged too, with
/// `after_login`; one that finds it logged out is recorded only as a change. It needs the
/// account and `refresh`.
#[tokio::test]
async fn a_check_after_a_login_is_recorded_as_one() {
    let home = tempfile::tempdir().unwrap();
    let work_dir = home.path().join("cfg").join("work");
    std::fs::create_dir_all(&work_dir).unwrap();
    let mut config = Config::default();
    account(&mut config, "claude-code", "work", Some(&work_dir), false);
    let logins = Logins::default();
    let th = start(home.path(), config, Some(logins.probe())).await;
    let mut c = th.client().await;
    refresh(&mut c).await;
    let before = control_of(&mut c, "account.status").await.len();
    let after_login = json!({
        "runtime": "claude-code", "account": "work", "refresh": true, "after_login": true,
    });
    let s = c.call("account.status", after_login.clone()).await.unwrap();
    assert_eq!(entry(&s, "claude-code", "work")["login"], "logged_in");
    let st = control_of(&mut c, "account.status").await;
    assert_eq!(st.len(), before + 1, "{st:?}");
    let last = &st.last().unwrap()["data"];
    assert_eq!(
        (&last["account"], &last["after_login"]),
        (&json!("work"), &json!(true)),
        "{last}"
    );
    // A plain check that finds the same records nothing, and its status has no `after_login`.
    refresh(&mut c).await;
    assert_eq!(control_of(&mut c, "account.status").await.len(), before + 1);
    // A login that did not end logged in: recorded as the change it is, not as a login.
    logins.set("claude-code:work", true, AgentLogin::LoggedOut);
    c.call("account.status", after_login).await.unwrap();
    let st = control_of(&mut c, "account.status").await;
    assert_eq!(st.len(), before + 2);
    assert!(st.last().unwrap()["data"].get("after_login").is_none());
    for bad in [
        json!({"runtime": "claude-code", "refresh": true, "after_login": true}),
        json!({"runtime": "claude-code", "account": "work", "after_login": true}),
    ] {
        let e = c.call("account.status", bad).await.unwrap_err();
        assert_eq!(code(&e), codes::BAD_REQUEST);
    }
    th.stop().await;
}

#[tokio::test]
async fn login_checks_record_changes_and_flip_labels() {
    let home = tempfile::tempdir().unwrap();
    let work_dir = home.path().join("cfg").join("work");
    std::fs::create_dir_all(&work_dir).unwrap();
    let mut config = Config::default();
    account(&mut config, "claude-code", "work", Some(&work_dir), false);
    let logins = Logins::default();
    logins.set("claude-code:work", true, AgentLogin::LoggedOut);
    let th = start(home.path(), config.clone(), Some(logins.probe())).await;
    let mut c = th.client().await;

    let s = refresh(&mut c).await;
    assert_eq!(s["runtimes"]["claude-code"]["shown"], false, "{s}");
    let native = entry(&s, "claude-code", "default");
    assert_eq!(
        (&native["login"], &native["method"]),
        (&json!("logged_in"), &json!("claude.ai"))
    );
    assert!(native["checked_at"].is_string());
    assert_eq!(entry(&s, "claude-code", "work")["login"], "logged_out");
    // Only claude-code has a named account: nothing else is checked.
    assert!(entry(&s, "codex", "default")["login"].is_null(), "{s}");
    let recorded = control_of(&mut c, "account.status").await;
    assert_eq!(recorded.len(), 2, "{recorded:?}");
    assert!(recorded
        .iter()
        .all(|e| e["data"]["runtime"] == "claude-code"));

    // Nothing changed: nothing recorded.
    let before = logins.calls.load(Ordering::SeqCst);
    refresh(&mut c).await;
    assert!(logins.calls.load(Ordering::SeqCst) >= before + 2);
    assert_eq!(control_of(&mut c, "account.status").await.len(), 2);

    // The second account logs in: labels show; it logs out again: they go.
    logins.set("claude-code:work", true, AgentLogin::LoggedIn);
    let s = refresh(&mut c).await;
    assert_eq!(s["runtimes"]["claude-code"]["shown"], true, "{s}");
    assert_eq!(control_of(&mut c, "account.status").await.len(), 3);
    let threads_json = c
        .call("account.list", json!({"runtime": "claude-code"}))
        .await
        .unwrap();
    assert_eq!(threads_json["runtimes"]["claude-code"]["shown"], true);
    logins.set("claude-code:work", true, AgentLogin::LoggedOut);
    let s = refresh(&mut c).await;
    assert_eq!(s["runtimes"]["claude-code"]["shown"], false, "{s}");
    assert_eq!(control_of(&mut c, "account.status").await.len(), 4);

    // A missing CLI.
    logins.set("claude-code:work", false, AgentLogin::Unknown);
    let s = c
        .call(
            "account.status",
            json!({"runtime": "claude-code", "account": "work", "refresh": true}),
        )
        .await
        .unwrap();
    assert_eq!(entry(&s, "claude-code", "work")["login"], "missing");
    let e = c
        .call("account.status", json!({"account": "work"}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::BAD_REQUEST);

    // A new account is checked once added.
    logins.set("codex:team", true, AgentLogin::LoggedIn);
    c.call(
        "account.add",
        json!({"runtime": "codex", "account": "team"}),
    )
    .await
    .unwrap();
    timeout(WAIT, async {
        loop {
            let st = control_of(&mut c, "account.status").await;
            if st.iter().any(|e| e["data"]["runtime"] == "codex") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the new account's check");
    let list = c.call("account.list", json!({})).await.unwrap();
    assert_eq!(entry(&list, "codex", "team")["login"], "logged_in");
    // The native codex account is checked at the next round: two codex accounts, both count.
    assert_eq!(list["runtimes"]["codex"]["shown"], true);
    th.stop().await;

    // A restart (checking nothing) keeps every state.
    let th = start(home.path(), config, None).await;
    let mut c = th.client().await;
    let list = c.call("account.list", json!({})).await.unwrap();
    assert_eq!(entry(&list, "claude-code", "work")["login"], "missing");
    assert_eq!(entry(&list, "claude-code", "default")["login"], "logged_in");
    assert_eq!(entry(&list, "codex", "team")["login"], "logged_in");
    assert_eq!(entry(&list, "codex", "team")["source"], "recorded");
    assert_eq!(list["runtimes"]["claude-code"]["shown"], false);
    th.stop().await;
}

/// A refresh whose findings cannot be recorded (an append that fails: a trigger in the hub's
/// database) is an `internal` error naming the account, not the old state as an answer; the
/// account keeps its last state, the hub keeps serving, and the next refresh records it.
#[tokio::test]
async fn a_refresh_that_cannot_be_recorded_fails() {
    let home = tempfile::tempdir().unwrap();
    let work_dir = home.path().join("cfg").join("work");
    std::fs::create_dir_all(&work_dir).unwrap();
    let mut config = Config::default();
    account(&mut config, "claude-code", "work", Some(&work_dir), false);
    let logins = Logins::default();
    logins.set("claude-code:work", true, AgentLogin::LoggedOut);
    let th = start(home.path(), config, Some(logins.probe())).await;
    let mut c = th.client().await;
    let s = refresh(&mut c).await;
    assert_eq!(
        entry(&s, "claude-code", "work")["login"],
        "logged_out",
        "{s}"
    );

    let db = twr_hub::store::connect(&th.paths.db_file()).unwrap();
    db.busy_timeout(Duration::from_secs(10)).unwrap();
    db.execute_batch(
        "CREATE TRIGGER refuse_status BEFORE INSERT ON events WHEN NEW.kind = 'account.status'
         BEGIN SELECT RAISE(ABORT, 'injected failure'); END;",
    )
    .unwrap();
    logins.set("claude-code:work", true, AgentLogin::LoggedIn);
    let e = c
        .call("account.status", json!({"refresh": true}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::INTERNAL, "{e:#}");
    let m = message(&e);
    assert!(
        m.starts_with("the login check of claude-code:work could not be recorded: ")
            && m.contains("injected failure"),
        "{m}"
    );
    let list = c.call("account.list", json!({})).await.unwrap();
    assert_eq!(entry(&list, "claude-code", "work")["login"], "logged_out");

    db.execute_batch("DROP TRIGGER refuse_status").unwrap();
    let s = refresh(&mut c).await;
    assert_eq!(
        entry(&s, "claude-code", "work")["login"],
        "logged_in",
        "{s}"
    );
    th.stop().await;
}

/// The hub does not start with a config account in a runtime's own directory; a record whose
/// directory config.toml now declares for another account is a warning (`hub.info`), and
/// `account.list` says why the record can't be used (`conflict`), and why the config account
/// can't be either until the record goes.
#[tokio::test]
async fn the_start_refuses_native_and_shared_directories() {
    let home = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    account(
        &mut config,
        "claude-code",
        "work",
        Some(&home.path().join("native/claude-code")),
        false,
    );
    let e = Hub::open(hub_options(home.path(), config, None))
        .await
        .err()
        .expect("refused");
    assert_eq!(
        e.to_string(),
        "[accounts.claude-code.work] dir is claude-code's own directory: use native = true"
    );
    let shared = home.path().join("shared");
    seed(home.path(), &[added(1, "codex", "team", &shared)]);
    let mut config = Config::default();
    account(&mut config, "claude-code", "work", Some(&shared), false);
    let hub = Hub::open(hub_options(home.path(), config, None))
        .await
        .unwrap();
    let why = format!(
        "its directory {} is claude-code:work's too ([accounts.claude-code.work]): remove the \
         account (twr account remove codex team)",
        shared.display()
    );
    assert_eq!(
        hub.info()["warnings"],
        json!([format!("codex:team can't be used: {why}")])
    );
    let list = hub.account_list(None, true).unwrap();
    assert_eq!(entry(&list, "codex", "team")["conflict"], json!(why));
    // The config account shares the record's login meanwhile: refused at use too, until the
    // record goes (or config.toml moves it).
    assert_eq!(
        entry(&list, "claude-code", "work")["conflict"],
        json!(format!(
            "its directory {} is codex:team's too (codex:team (twr account add)): change \
             [accounts.claude-code.work] dir in config.toml and restart the hub, or remove \
             codex:team (twr account remove codex team)",
            shared.display()
        ))
    );
    hub.shutdown().await;
    drop(hub);
    // Without that config the record is fine.
    let hub = timeout(WAIT, async {
        loop {
            match Hub::open(hub_options(home.path(), Config::default(), None)).await {
                Ok(h) => return h,
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(hub.info()["accounts"]["codex"]["shown"], true);
    assert_eq!(hub.info()["warnings"], json!([]));
    hub.shutdown().await;
}

/// A directory `account.add` places under the accounts root is checked once links are
/// followed: with `<root>/codex` a link to elsewhere, adding `codex:team` there (as a dry run
/// too, or naming that directory) is refused and nothing is made or recorded outside.
#[tokio::test]
async fn add_refuses_a_default_directory_a_link_leads_outside() {
    let home = tempfile::tempdir().unwrap();
    let th = start(home.path(), Config::default(), None).await;
    let mut c = th.client().await;
    let root = th.paths.accounts_dir();
    let outside = home.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("codex")).unwrap();
    for p in [
        json!({"runtime": "codex", "account": "team", "dry_run": true}),
        json!({"runtime": "codex", "account": "team"}),
        json!({"runtime": "codex", "account": "team", "dir": root.join("codex/team")}),
    ] {
        let e = c.call("account.add", p.clone()).await.unwrap_err();
        assert_eq!(code(&e), codes::BAD_REQUEST, "{p}: {}", message(&e));
        assert!(
            message(&e).contains("is not inside")
                && message(&e).contains("once links are followed"),
            "{p}: {}",
            message(&e)
        );
    }
    assert!(!outside.join("team").exists());
    assert!(control_of(&mut c, "account.added").await.is_empty());
    // Elsewhere in the root it is made as usual: `<root>/claude-code` first, private.
    let r = c
        .call(
            "account.add",
            json!({"runtime": "claude-code", "account": "work", "dry_run": true}),
        )
        .await
        .unwrap();
    assert_eq!(r["dir"], json!(root.join("claude-code/work")), "{r}");
    assert!(root.join("claude-code").is_dir());
    th.stop().await;
}

/// A config or recorded account is checked again on every `account.add`, dry runs too (the
/// CLI's add asks again once it has made the directory): a path that did not resolve at the
/// start but is another account's once made, a runtime's own directory, a default directory a
/// link leads outside the root.
#[tokio::test]
async fn add_checks_the_directories_of_config_and_recorded_accounts_again() {
    let home = tempfile::tempdir().unwrap();
    let private = home.path().join("private");
    std::fs::create_dir_all(private.join("a")).unwrap();
    // `missing` is not there yet: the two are not one directory as far as the start can tell.
    let mut config = Config::default();
    account(
        &mut config,
        "claude-code",
        "a",
        Some(&private.join("a")),
        false,
    );
    account(
        &mut config,
        "claude-code",
        "b",
        Some(&private.join("missing/../a")),
        false,
    );
    account(&mut config, "grok-build", "team", None, false);
    seed(
        home.path(),
        &[added(1, "codex", "r", &home.path().join("native/codex"))],
    );
    let th = start(home.path(), config, None).await;
    let mut c = th.client().await;
    let dry = |rt: &str, name: &str| json!({"runtime": rt, "account": name, "dry_run": true});
    let r = c
        .call("account.add", dry("claude-code", "b"))
        .await
        .unwrap();
    assert_eq!(r["config"], true, "{r}");
    // The CLI makes the directory (and with it `missing`), then asks again.
    std::fs::create_dir_all(private.join("missing")).unwrap();
    for p in [
        dry("claude-code", "b"),
        json!({"runtime": "claude-code", "account": "b", "dir": private.join("missing/../a")}),
    ] {
        let e = c.call("account.add", p.clone()).await.unwrap_err();
        assert_eq!(code(&e), codes::CONFLICT, "{p}: {}", message(&e));
        let m = message(&e);
        assert!(
            m.contains("claude-code:b can't be used")
                && m.contains("claude-code:a's too")
                && m.contains("change [accounts.claude-code.b] dir in config.toml"),
            "{p}: {m}"
        );
    }
    // A recorded account in a runtime's own directory: its fix is removing it.
    let e = c.call("account.add", dry("codex", "r")).await.unwrap_err();
    assert_eq!(code(&e), codes::CONFLICT, "{}", message(&e));
    assert!(
        message(&e).contains("codex's own directory")
            && message(&e).contains("twr account remove codex r"),
        "{}",
        message(&e)
    );
    // A config account's default directory a link leads outside the root.
    let outside = home.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::create_dir_all(th.paths.accounts_dir()).unwrap();
    std::os::unix::fs::symlink(&outside, th.paths.accounts_dir().join("grok-build")).unwrap();
    let e = c
        .call("account.add", dry("grok-build", "team"))
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::BAD_REQUEST, "{}", message(&e));
    assert!(message(&e).contains("is not inside"), "{}", message(&e));
    assert!(control_of(&mut c, "account.added").await.len() == 1);
    th.stop().await;
}

/// A login check's result is of the directory it checked: an account removed and added again
/// in another directory while its check ran does not get that check's state.
#[tokio::test]
async fn a_check_of_an_old_directory_is_not_recorded_for_the_new_one() {
    let home = tempfile::tempdir().unwrap();
    let (old, new) = (home.path().join("old/work"), home.path().join("new/work"));
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let started = Arc::new(tokio::sync::Notify::new());
    let probe: AccountProbe = {
        let (gate, started, old) = (Arc::clone(&gate), Arc::clone(&started), old.clone());
        Arc::new(move |rt: String, dir: Option<PathBuf>| {
            let (gate, started, old) = (Arc::clone(&gate), Arc::clone(&started), old.clone());
            async move {
                // The old directory's check: held until the account is in the new one, and
                // logged out; any other: logged in.
                let login = if dir.as_deref() == Some(old.as_path()) {
                    started.notify_one();
                    let _ = gate.acquire().await;
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
    };
    let th = start(home.path(), Config::default(), Some(probe)).await;
    let mut c = th.client().await;
    let add = |dir: &Path| json!({"runtime": "claude-code", "account": "work", "dir": dir});
    c.call("account.add", add(&old)).await.unwrap();
    timeout(WAIT, started.notified())
        .await
        .expect("the old directory's check");
    c.call(
        "account.remove",
        json!({"runtime": "claude-code", "account": "work"}),
    )
    .await
    .unwrap();
    c.call("account.add", add(&new)).await.unwrap();
    gate.add_permits(100);
    // The new directory's check (after the old one's: one at a time) is what is recorded.
    timeout(WAIT, async {
        loop {
            let st = control_of(&mut c, "account.status").await;
            if st
                .iter()
                .any(|e| e["data"]["account"] == "work" && e["data"]["login"] == "logged_in")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the new directory's check");
    let st = control_of(&mut c, "account.status").await;
    assert!(
        !st.iter()
            .any(|e| e["data"]["account"] == "work" && e["data"]["login"] == "logged_out"),
        "{st:?}"
    );
    let list = c.call("account.list", json!({})).await.unwrap();
    assert_eq!(entry(&list, "claude-code", "work")["login"], "logged_in");
    th.stop().await;
}

/// A named account that collides (its directory a runtime's own: recorded before the hub's
/// environment changed) is never checked: its runtime would be pointed at another login's
/// directory. Nothing is recorded for it; the others are checked as usual.
#[tokio::test]
async fn a_colliding_account_is_not_checked() {
    let home = tempfile::tempdir().unwrap();
    let codex_own = home.path().join("native").join("codex");
    let fine = home.path().join("acc").join("fine");
    std::fs::create_dir_all(&codex_own).unwrap();
    std::fs::create_dir_all(&fine).unwrap();
    seed(
        home.path(),
        &[
            added(1, "claude-code", "clash", &codex_own),
            added(2, "claude-code", "fine", &fine),
        ],
    );
    let checked: Arc<Mutex<Vec<Option<PathBuf>>>> = Arc::default();
    let probe: AccountProbe = {
        let checked = Arc::clone(&checked);
        Arc::new(move |rt: String, dir: Option<PathBuf>| {
            checked.lock().unwrap().push(dir);
            async move {
                AgentInfo {
                    runtime: rt,
                    found: true,
                    login: AgentLogin::LoggedIn,
                    ..Default::default()
                }
            }
            .boxed()
        })
    };
    let hub = Hub::open(hub_options(home.path(), Config::default(), Some(probe)))
        .await
        .unwrap();
    let user = twr_hub::Caller::user("itest");
    let status = hub
        .account_status(
            twr_hub::hub::AccountStatusParams {
                runtime: Some("claude-code".into()),
                refresh: true,
                ..Default::default()
            },
            &user,
        )
        .await
        .unwrap();
    let seen = checked.lock().unwrap().clone();
    assert!(seen.contains(&Some(fine.clone())), "{seen:?}");
    assert!(!seen.contains(&Some(codex_own.clone())), "{seen:?}");
    let clash = entry(&status, "claude-code", "clash");
    assert!(clash["conflict"].is_string(), "{clash}");
    assert!(clash.get("login").is_none_or(Value::is_null), "{clash}");
    assert_eq!(entry(&status, "claude-code", "fine")["login"], "logged_in");
    hub.shutdown().await;
}

/// The native account cannot take the name of a removed named account (its threads keep that
/// name, and would be taken for the native account's); `account.remove` says which directory
/// it removed (none for the native account).
#[tokio::test]
async fn the_native_account_never_takes_a_removed_accounts_name() {
    let home = tempfile::tempdir().unwrap();
    let side = home.path().join("acc").join("side");
    std::fs::create_dir_all(&side).unwrap();
    let hub = Hub::open(hub_options(home.path(), Config::default(), None))
        .await
        .unwrap();
    let user = twr_hub::Caller::user("itest");
    let add = |name: &str, dir: Option<&Path>, native: bool| twr_hub::hub::AccountAddParams {
        runtime: "claude-code".into(),
        account: name.into(),
        dir: dir.map(Path::to_path_buf),
        native,
        ..Default::default()
    };
    let remove = |name: &str| twr_hub::hub::AccountRemoveParams {
        runtime: "claude-code".into(),
        account: name.into(),
        ..Default::default()
    };
    hub.account_add(add("side", Some(&side), false), &user)
        .unwrap();
    let r = hub.account_remove(remove("side"), &user).await.unwrap();
    assert_eq!(r["dir"], side.to_str().unwrap(), "{r}");
    let e = hub.account_add(add("side", None, true), &user).unwrap_err();
    assert_eq!(e.code, codes::CONFLICT, "{e:?}");
    assert_eq!(
        e.message,
        "claude-code:side was an account of claude-code (removed; its threads keep the name): \
         pick another name for the native account"
    );
    // Another name is fine; removing the native account says no directory.
    hub.account_add(add("mine", None, true), &user).unwrap();
    let r = hub.account_remove(remove("mine"), &user).await.unwrap();
    assert_eq!(
        (r["native"].clone(), r["dir"].clone()),
        (json!(true), Value::Null)
    );
    hub.shutdown().await;
}

/// `Hub::open` of `home` with `config`, again until the previous hub's lock is gone.
async fn reopen(home: &Path, config: &Config) -> Hub {
    timeout(WAIT, async {
        loop {
            match Hub::open(hub_options(home, config.clone(), None)).await {
                Ok(h) => return h,
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await
    .unwrap()
}

/// A named `default` in config.toml while nothing names the native account stops the hub;
/// once `twr account name-native` named it (a record, replayed at each start), the same
/// config starts, and declares the `default` added since (config and record: both).
#[tokio::test]
async fn a_config_default_starts_once_a_record_names_the_native_account() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("acc").join("default");
    std::fs::create_dir_all(&dir).unwrap();
    let mut config = Config::default();
    account(&mut config, "codex", "default", Some(&dir), false);
    let e = Hub::open(hub_options(home.path(), config.clone(), None))
        .await
        .err()
        .expect("refused");
    assert!(
        e.to_string().starts_with(
            "[accounts.codex.default]: \"default\" is codex's native account's name until \
             something names that account: set `native = true` here or on another \
             [accounts.codex] account, pick another name, or start the hub without this one \
             and name the native account first (twr account name-native codex <name>)"
        ),
        "{e}"
    );
    // Without that declaration: name the native account, add `default` in that directory.
    let hub = reopen(home.path(), &Config::default()).await;
    let user = twr_hub::Caller::user("itest");
    let add = |name: &str, dir: Option<&Path>, native: bool| twr_hub::hub::AccountAddParams {
        runtime: "codex".into(),
        account: name.into(),
        dir: dir.map(Path::to_path_buf),
        native,
        ..Default::default()
    };
    hub.account_add(add("personal", None, true), &user).unwrap();
    hub.account_add(add("default", Some(&dir), false), &user)
        .unwrap();
    hub.shutdown().await;
    drop(hub);
    // The config again: the replayed name frees `default`, twice over.
    for _ in 0..2 {
        let hub = reopen(home.path(), &config).await;
        let list = hub.account_list(None, true).unwrap();
        assert_eq!(list["runtimes"]["codex"]["native"], "personal", "{list}");
        let named = entry(&list, "codex", "default");
        assert_eq!(
            (&named["native"], &named["source"]),
            (&json!(false), &json!("both")),
            "{named}"
        );
        assert_eq!(hub.info()["warnings"], json!([]));
        hub.shutdown().await;
        drop(hub);
    }
}

fn added_native(seq: u64, rt: &str, name: &str) -> Event {
    Event::new(
        HUB_THREAD,
        seq,
        None,
        Source::User("itest".into()),
        EventBody::AccountAdded(AccountAdded {
            runtime: rt.into(),
            account: name.into(),
            native: true,
            ..Default::default()
        }),
    )
}

/// `list`'s accounts of runtime `rt` named `name`: (native, the entry) for each.
fn entries<'a>(list: &'a Value, rt: &str, name: &str) -> Vec<(bool, &'a Value)> {
    list["runtimes"][rt]["accounts"]
        .as_array()
        .unwrap_or_else(|| panic!("no {rt} in {list}"))
        .iter()
        .filter(|a| a["name"] == name)
        .map(|a| (a["native"] == json!(true), a))
        .collect()
}

/// A recorded native `personal` and a recorded named `default`, then config declares
/// `personal` named: the native account falls back to `default`, next to the named one.
/// `account.list` marks the native entry `shadowed` (the name is the named account's), a
/// check of `default` checks the named account's directory, and each login check lands on the
/// account it was of (`account.status` says `native` for the native one), replayed too.
#[tokio::test]
async fn a_name_the_native_account_shares_is_the_named_accounts() {
    let home = tempfile::tempdir().unwrap();
    let named = home.path().join("acc").join("default");
    let personal = home.path().join("cfg").join("personal");
    std::fs::create_dir_all(&named).unwrap();
    std::fs::create_dir_all(&personal).unwrap();
    seed(
        home.path(),
        &[
            added_native(1, "codex", "personal"),
            added(2, "codex", "default", &named),
        ],
    );
    let mut config = Config::default();
    account(&mut config, "codex", "personal", Some(&personal), false);
    let logins = Logins::default();
    logins.set("codex:native", true, AgentLogin::LoggedIn);
    logins.set("codex:default", true, AgentLogin::LoggedOut);
    let th = start(home.path(), config.clone(), Some(logins.probe())).await;
    let mut c = th.client().await;
    let check = |list: &Value| {
        assert_eq!(list["runtimes"]["codex"]["native"], "default", "{list}");
        let both = entries(list, "codex", "default");
        assert_eq!(both.len(), 2, "{list}");
        let (native, named_entry) = (both[0].1, both[1].1);
        assert_eq!((both[0].0, both[1].0), (true, false));
        assert_eq!(native["shadowed"], true, "{native}");
        assert!(named_entry.get("shadowed").is_none(), "{named_entry}");
        assert_eq!(native["login"], "logged_in", "{list}");
        assert_eq!(named_entry["login"], "logged_out", "{list}");
        // (Its native record, which config declares named: both.)
        assert_eq!(entry(list, "codex", "personal")["source"], "both");
    };
    check(&refresh(&mut c).await);
    let statuses = control_of(&mut c, "account.status").await;
    let of = |native: bool| {
        statuses
            .iter()
            .filter(|e| e["data"]["account"] == "default")
            .filter(|e| (e["data"]["native"] == json!(true)) == native)
            .count()
    };
    assert_eq!((of(true), of(false)), (1, 1), "{statuses:?}");
    // `default` alone: the named account's directory is checked, not the native one.
    logins.set("codex:default", true, AgentLogin::LoggedIn);
    logins.set("codex:native", true, AgentLogin::LoggedOut);
    let s = c
        .call(
            "account.status",
            json!({"runtime": "codex", "account": "default", "refresh": true}),
        )
        .await
        .unwrap();
    let both = entries(&s, "codex", "default");
    assert_eq!(
        (&both[0].1["login"], &both[1].1["login"]),
        (&json!("logged_in"), &json!("logged_in")),
        "{s}"
    );
    logins.set("codex:default", true, AgentLogin::LoggedOut);
    logins.set("codex:native", true, AgentLogin::LoggedIn);
    check(&refresh(&mut c).await);
    // Adding `default` again is the named account, there already (not the native one's name).
    let r = c
        .call(
            "account.add",
            json!({"runtime": "codex", "account": "default", "dry_run": true}),
        )
        .await
        .unwrap();
    assert_eq!(
        (&r["native"], &r["existing"], &r["dir"]),
        (&json!(false), &json!(true), &json!(named.to_str().unwrap())),
        "{r}"
    );
    th.stop().await;
    let th = start(home.path(), config, None).await;
    let mut c = th.client().await;
    check(&c.call("account.list", json!({})).await.unwrap());
    th.stop().await;
}

/// A record whose name config declares as the other kind (a named record config declares
/// native, a native one config declares named) is both, and `account.remove` removes that
/// record, of its own kind (`account.removed` says `record_only`), keeping config's account.
/// Once config drops the declaration, nothing comes back: no named account, no native name,
/// and the native account is not removed.
#[tokio::test]
async fn a_record_config_gives_another_kind_is_removed_as_its_own_kind() {
    let home = tempfile::tempdir().unwrap();
    let work = home.path().join("acc").join("work");
    let home_dir = home.path().join("cfg").join("home");
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&home_dir).unwrap();
    seed(
        home.path(),
        &[
            added(1, "codex", "work", &work),
            added_native(2, "claude-code", "home"),
        ],
    );
    let mut config = Config::default();
    account(&mut config, "codex", "work", None, true);
    account(&mut config, "claude-code", "home", Some(&home_dir), false);
    let th = start(home.path(), config.clone(), None).await;
    let mut c = th.client().await;
    let list = c.call("account.list", json!({})).await.unwrap();
    let work_entry = entry(&list, "codex", "work");
    assert_eq!(
        (&work_entry["native"], &work_entry["source"]),
        (&json!(true), &json!("both")),
        "{list}"
    );
    let home_entry = entry(&list, "claude-code", "home");
    assert_eq!(
        (&home_entry["native"], &home_entry["source"]),
        (&json!(false), &json!("both")),
        "{list}"
    );
    assert_eq!(entry(&list, "claude-code", "default")["source"], "native");
    for (rt, name) in [("codex", "work"), ("claude-code", "home")] {
        let r = c
            .call("account.remove", json!({"runtime": rt, "account": name}))
            .await
            .unwrap();
        assert_eq!(
            (&r["removed"], &r["config"]),
            (&json!(true), &json!(true)),
            "{r}"
        );
    }
    let removals = control_of(&mut c, "account.removed").await;
    let data: Vec<(Value, Value, Value)> = removals
        .iter()
        .map(|e| {
            let d = &e["data"];
            (
                d["account"].clone(),
                d["native"].clone(),
                d["record_only"].clone(),
            )
        })
        .collect();
    assert_eq!(
        data,
        [
            (json!("work"), Value::Null, json!(true)),
            (json!("home"), json!(true), json!(true)),
        ],
        "the records' own kinds"
    );
    let configs = |list: &Value| {
        assert_eq!(entry(list, "codex", "work")["source"], "config", "{list}");
        assert_eq!(
            entry(list, "claude-code", "home")["source"],
            "config",
            "{list}"
        );
    };
    configs(&c.call("account.list", json!({})).await.unwrap());
    th.stop().await;
    let th = start(home.path(), config, None).await;
    let mut c = th.client().await;
    configs(&c.call("account.list", json!({})).await.unwrap());
    th.stop().await;
    // Config drops both: nothing comes back.
    let th = start(home.path(), Config::default(), None).await;
    let mut c = th.client().await;
    let list = c.call("account.list", json!({})).await.unwrap();
    for rt in ["codex", "claude-code"] {
        assert_eq!(list["runtimes"][rt]["native"], "default", "{list}");
        let live: Vec<&Value> = list["runtimes"][rt]["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|a| a["removed"] != json!(true))
            .collect();
        assert_eq!(live.len(), 1, "{list}");
        assert_eq!(
            (&live[0]["name"], &live[0]["native"]),
            (&json!("default"), &json!(true))
        );
    }
    th.stop().await;
}

/// The database's schema version, read as a twr of any version would (`PRAGMA user_version`).
fn db_version(home: &Path) -> i32 {
    twr_hub::store::connect(&Paths::with_home(home).db_file())
        .unwrap()
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap()
}

/// A hub that never records an account leaves the database at schema 8 (an older twr still
/// opens it, also after a config account's dry run and a list); the first recorded account
/// raises it to 9, which a twr that knows only 8 refuses, and a restart keeps it there.
#[tokio::test]
async fn recording_an_account_fences_the_database_from_older_twrs() {
    use twr_hub::store::{ACCOUNTS_SCHEMA_VERSION, SCHEMA_VERSION};
    let home = tempfile::tempdir().unwrap();
    let th = start(home.path(), Config::default(), None).await;
    let mut c = th.client().await;
    c.call("account.list", json!({})).await.unwrap();
    c.call(
        "account.add",
        json!({"runtime": "claude-code", "account": "work", "dry_run": true}),
    )
    .await
    .unwrap();
    assert_eq!(db_version(home.path()), SCHEMA_VERSION);
    let r = c
        .call(
            "account.add",
            json!({"runtime": "claude-code", "account": "work"}),
        )
        .await
        .unwrap();
    assert_eq!(r["recorded"], true, "{r}");
    assert_eq!(db_version(home.path()), ACCOUNTS_SCHEMA_VERSION);
    // Removing it leaves the fence: the log still has the account.
    c.call(
        "account.remove",
        json!({"runtime": "claude-code", "account": "work"}),
    )
    .await
    .unwrap();
    th.stop().await;
    assert_eq!(db_version(home.path()), ACCOUNTS_SCHEMA_VERSION);
    let th = start(home.path(), Config::default(), None).await;
    th.stop().await;
    assert_eq!(db_version(home.path()), ACCOUNTS_SCHEMA_VERSION);

    // A thread on a config account (nothing recorded by `account add`) fences it as well,
    // once the thread is recorded with its account.
    let home = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    account(
        &mut config,
        "claude-code",
        "work",
        Some(&home.path().join("cfg").join("work")),
        false,
    );
    let th = start(home.path(), config, None).await;
    th.stop().await;
    assert_eq!(db_version(home.path()), SCHEMA_VERSION);
    seed(
        home.path(),
        &[thread_on("t-a", "a", "claude-code", Some("work"))],
    );
    assert_eq!(db_version(home.path()), ACCOUNTS_SCHEMA_VERSION);
}
