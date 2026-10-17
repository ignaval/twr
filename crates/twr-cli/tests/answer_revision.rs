//! An answer names the revision of the question it was given for (the relay rule): a
//! permission raised again with changed input before the answer arrives is not answered by
//! what the user said about the old card. Driven through the TUI's own state and request
//! builder and through the `twr` binary, against an in-process hub whose runtime records what
//! it is told.

use std::path::Path;
use std::process::Output;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::json;
use tokio::time::timeout;
use twr_core::adapter::{
    Adapter, AdapterCmd, AdapterEnd, AdapterHandle, AdapterInfo, SpawnConfig, Tier,
};
use twr_core::client::{Client, Subscription};
use twr_core::config::Config;
use twr_core::event::*;
use twr_core::paths::Paths;
use twr_core::proto::{codes, RpcError, StreamItem};
use twr_hub::{server, Hub, HubOptions, NewThread};
use twr_tui::app::{Action, AppState};

const TWR: &str = env!("CARGO_BIN_EXE_twr");

/// The `twr` binary, without a `TWR_WORKSPACE` the test's own environment may have (the tests
/// here run in the `default` workspace of their `TWR_HOME`).
fn twr_cmd() -> std::process::Command {
    let mut c = std::process::Command::new(TWR);
    c.env_remove("TWR_WORKSPACE");
    c.env("TWR_TEST_USER_TOKEN_FILE", test_token());
    // No hub or runner of a test runs the user's login shell or real agent CLIs.
    c.env("TWR_AGENT_PROBE", "off");
    c.env("TWR_BOX_PROBE", "off");
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

/// What the runtime was told: `(attn_id, decision)` per answer it received.
type Told = Arc<Mutex<Vec<(String, String)>>>;

fn permission(id: &str, summary: &str) -> EventBody {
    EventBody::AttentionRaised(AttentionRaised {
        attn_id: id.into(),
        reason: AttentionReason::Permission,
        summary: summary.into(),
        options: vec!["allow".into(), "deny".into()],
        ..Default::default()
    })
}

/// Runtime `test`: its first turn asks to delete build artifacts, then (before any answer)
/// asks again under the same id to delete the source files; once answered it asks `p2` (never
/// raised again), and ends the turn when that is answered.
struct Reraising(Told);

impl Adapter for Reraising {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "test",
            tier: Tier::Headless,
            description: "re-raises its permission",
        }
    }

    fn spawn(&self, cfg: SpawnConfig, _prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let (h, end) = AdapterHandle::pair();
        let told = Arc::clone(&self.0);
        let turn = cfg.prompt_turn;
        tokio::spawn(play(end, turn, told));
        Ok(h)
    }
}

async fn play(mut end: AdapterEnd, turn: Option<u64>, told: Told) {
    end.emit(
        EventBody::ThreadStarted(SessionPayload {
            session_id: Some("s-reraise".into()),
            ..Default::default()
        }),
        None,
    );
    end.emit(permission("p1", "delete build artifacts?"), turn);
    end.emit(permission("p1", "delete source files?"), turn);
    while let Some(cmd) = end.cmds.recv().await {
        match cmd {
            AdapterCmd::Respond { attn_id, answer } => {
                told.lock()
                    .unwrap()
                    .push((attn_id.clone(), answer.decision.clone()));
                if attn_id == "p1" {
                    end.emit(permission("p2", "push to main?"), turn);
                } else {
                    end.emit(
                        EventBody::TurnEnded(TurnEnded {
                            result: TurnResult::Completed,
                            ..Default::default()
                        }),
                        turn,
                    );
                }
            }
            AdapterCmd::Stop | AdapterCmd::Interrupt => break,
            AdapterCmd::Send { .. } => {}
        }
    }
    end.exited(Some(0), "");
}

async fn next_item(sub: &mut Subscription) -> StreamItem {
    timeout(WAIT, sub.next())
        .await
        .expect("timed out waiting for a control item")
        .unwrap()
        .expect("stream ended")
}

async fn tui_client(socket: &Path) -> Client {
    let mut c = Client::connect(socket).await.unwrap();
    c.hello(json!({"client": twr_tui::CLIENT_NAME,
        "user_token": std::fs::read_to_string(test_token()).unwrap()}))
        .await
        .unwrap();
    c
}

/// Runs the `twr` binary against the hub (off the runtime's worker threads).
async fn twr(home: &Path, args: &[&str]) -> Output {
    let home = home.to_path_buf();
    let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    tokio::task::spawn_blocking(move || {
        twr_cmd()
            .args(&args)
            .env("TWR_HOME", &home)
            .env_remove("RUST_LOG")
            .output()
            .expect("running twr")
    })
    .await
    .unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

async fn told_len(told: &Told, n: usize) {
    timeout(WAIT, async {
        while told.lock().unwrap().len() < n {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the runtime was never told the answer");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn answers_name_the_revision_they_were_given_for() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let paths = Paths::with_home(home);
    let project = home.join("proj");
    std::fs::create_dir(&project).unwrap();
    let told: Told = Arc::default();
    let listener = server::bind(&paths.socket).await.unwrap();
    let mut o = HubOptions::new(paths.db_file(), Config::default());
    // Its own clients are the user's, also when the test runs inside a twr thread.
    o.test_user_token = Some(std::fs::read_to_string(test_token()).unwrap());
    let adapter: Arc<dyn Adapter> = Arc::new(Reraising(Arc::clone(&told)));
    o.adapters = Arc::new(move |n: &str| (n == "test").then(|| Arc::clone(&adapter)));
    let hub = Hub::open(o).await.unwrap();
    let serving = tokio::spawn(server::serve(
        hub.clone(),
        listener,
        paths.socket.clone(),
        false,
    ));
    let thread = hub
        .new_thread(
            NewThread {
                project: project.to_string_lossy().into_owned(),
                runtime: Some("test".into()),
                name: Some("t1".into()),
                no_worktree: true,
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();

    // The TUI's control stream: it renders the first card, and the permission is raised again
    // (asking something else) before the user's key reaches the hub.
    let mut control = tui_client(&paths.socket)
        .await
        .subscribe("control.subscribe", json!({"since": 0}))
        .await
        .unwrap();
    hub.send("t1", "go", TurnFrom::User, Source::User("test".into()))
        .unwrap();
    let mut rendered = AppState::new();
    let mut raises = Vec::new();
    while raises.len() < 2 {
        let item = next_item(&mut control).await;
        if let EventBody::AttentionRaised(p) = &item.event.body {
            raises.push((p.attn_id.clone(), item.gseq.unwrap()));
        }
        // What the screen showed: everything up to the first raise.
        if raises.len() < 2 {
            rendered.apply_event(&item);
        }
    }
    let (attn_id, first) = raises[0].clone();
    let current = raises[1].1;
    assert_eq!(raises[1].0, attn_id);
    rendered.set_focus(Some(thread.clone()));
    let card = rendered.top_attention(&thread).unwrap();
    assert_eq!(card.summary, "delete build artifacts?");
    // Its action popup (Enter on the empty line) drew it, with its options, long enough ago
    // to be read; its option keys answer it.
    rendered.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(rendered.view.action.is_some());
    let read = std::time::Instant::now() - twr_tui::app::CARD_SETTLE - Duration::from_millis(50);
    twr_tui::ui::draw_offscreen(&mut rendered, 160, 40, read);
    assert!(rendered.view.shown.is_some());
    let actions = rendered.handle_key(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE));
    assert_eq!(
        actions,
        [Action::Respond {
            attn_id: attn_id.clone(),
            revision: first,
            decision: "allow".into()
        }]
    );
    let (method, params) = twr_tui::action_request(&actions[0]).unwrap();
    let e = tui_client(&paths.socket)
        .await
        .call(method, params)
        .await
        .unwrap_err();
    let e = e.downcast_ref::<RpcError>().unwrap();
    assert_eq!(e.code, codes::CONFLICT, "{e:?}");
    assert!(e.message.contains("the question changed"), "{}", e.message);

    // The CLI lists the current revision; a bare id is refused once the item was raised
    // again, and so is an earlier revision.
    let listed = twr(home, &["attention"]).await;
    let current_id = format!("{attn_id}@{current}");
    assert!(stdout(&listed).contains(&current_id), "{}", stdout(&listed));
    assert!(stdout(&listed).contains("delete source files?"));
    for stale in [attn_id.clone(), format!("{attn_id}@{first}")] {
        let out = twr(home, &["respond", &stale, "allow"]).await;
        assert!(!out.status.success(), "{stale}: {}", stdout(&out));
        assert!(
            stderr(&out).contains("the question changed"),
            "{}",
            stderr(&out)
        );
    }
    // Nothing reached the runtime.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        told.lock().unwrap().is_empty(),
        "{:?}",
        told.lock().unwrap()
    );
    assert_eq!(hub.attention_list(false).len(), 1);

    // The revision the user saw now: answered and forwarded once.
    let out = twr(home, &["respond", &current_id, "deny"]).await;
    assert!(out.status.success(), "{}", stderr(&out));
    assert!(
        stdout(&out).trim().ends_with("on t1: deny"),
        "{}",
        stdout(&out)
    );
    told_len(&told, 1).await;
    assert_eq!(
        told.lock().unwrap()[0],
        ("p1".to_string(), "deny".to_string())
    );

    // A thread's top item: shown, but without a terminal to confirm on it is not answered
    // unless --yes; with --yes the question answered is shown and its revision is sent.
    timeout(WAIT, async {
        while hub.attention_list(false).is_empty() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let out = twr(home, &["respond", "t1", "1"]).await;
    assert!(!out.status.success(), "{}", stdout(&out));
    assert!(stdout(&out).contains("push to main?"), "{}", stdout(&out));
    assert!(stderr(&out).contains("unseen"), "{}", stderr(&out));
    assert_eq!(hub.attention_list(false).len(), 1);
    let out = twr(home, &["respond", "t1", "1", "--yes"]).await;
    assert!(out.status.success(), "{}", stderr(&out));
    let shown = stdout(&out);
    let p2 = hub.attention_list(false);
    assert!(p2.is_empty());
    assert!(shown.contains("push to main?"), "{shown}");
    assert!(shown.contains("@"), "{shown}");
    assert!(shown.trim().ends_with("on t1: allow"), "{shown}");
    told_len(&told, 2).await;
    assert_eq!(told.lock().unwrap().len(), 2);

    hub.shutdown().await;
    serving.abort();
}
