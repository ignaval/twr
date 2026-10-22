//! Socket API integration test: an in-process hub with the fake adapter, driven through
//! `twr_core::client::Client` exactly like the CLI and TUI.

use std::os::unix::fs::{FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::timeout;
use twr_core::adapter::{Adapter, AdapterHandle, AdapterInfo, SpawnConfig, Tier};
use twr_core::client::{Client, Subscription};
use twr_core::config::Config;
use twr_core::event::{
    EventBody, Grade, ReportReady, SessionPayload, Source, TurnFrom, TurnResult,
};
use twr_core::paths::Paths;
use twr_core::proto::{codes, RpcError, StreamItem, StreamKind};
use twr_hub::{server, Hub, HubOptions, NewThread};

const WAIT: Duration = Duration::from_secs(10);

struct TestHub {
    _dir: tempfile::TempDir,
    project: PathBuf,
    socket: PathBuf,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
    hub: Hub,
}

async fn start() -> TestHub {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths::with_home(dir.path());
    let project = dir.path().join("proj");
    std::fs::create_dir(&project).unwrap();
    let listener = server::bind(&paths.socket).await.unwrap();
    let mut o = HubOptions::new(paths.db_file(), Config::default());
    // The fake runtime (the built-in registry offers it only on request).
    o.adapters = Arc::new(|n: &str| -> Option<Arc<dyn Adapter>> {
        match n {
            "fake" => Some(Arc::new(twr_adapters::FakeAdapter::new())),
            other => twr_adapters::find(other),
        }
    });
    let hub = Hub::open(o).await.unwrap();
    let task = tokio::spawn(server::serve(
        hub.clone(),
        listener,
        paths.socket.clone(),
        false,
    ));
    TestHub {
        _dir: dir,
        project,
        socket: paths.socket,
        task,
        hub,
    }
}

async fn next_item(sub: &mut Subscription) -> StreamItem {
    timeout(WAIT, sub.next())
        .await
        .expect("timed out waiting for a stream item")
        .unwrap()
        .expect("stream ended")
}

/// Reads control items until one of `kind` arrives; returns all items read.
async fn until_kind(sub: &mut Subscription, kind: &str) -> Vec<StreamItem> {
    let mut seen = Vec::new();
    loop {
        let item = next_item(sub).await;
        let done = item.event.kind() == kind;
        seen.push(item);
        if done {
            return seen;
        }
    }
}

fn code(e: &anyhow::Error) -> String {
    e.downcast_ref::<RpcError>().unwrap().code.clone()
}

#[tokio::test]
async fn fake_thread_end_to_end_over_the_socket() {
    let th = start().await;
    let meta = std::fs::metadata(&th.socket).unwrap();
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    // A second hub on the same socket is refused.
    let e = server::bind(&th.socket).await.unwrap_err();
    assert!(e.to_string().contains("already listening"), "{e}");

    let mut c = Client::connect(&th.socket).await.unwrap();
    let info = c.call("hub.info", Value::Null).await.unwrap();
    assert!(info["client"].as_str().unwrap().starts_with("cli-"));
    // How loaded the machine is: a level and a summary wherever anything could be read
    // (null where nothing can be, such as a /proc that hides it).
    if !info["load"].is_null() {
        let level = info["load"]["level"].as_str().unwrap();
        assert!(["calm", "busy", "saturated"].contains(&level), "{info}");
        assert!(info["load"]["summary"].as_str().unwrap().starts_with(level));
        assert!(info["load"]["cores"].as_u64().unwrap() >= 1);
    }
    let hello = c.call("hello", json!({"client": "itest"})).await.unwrap();
    assert_eq!(hello["client"], "itest");
    let mut control = Client::subscribe_at(&th.socket, "control.subscribe", json!({"since": 0}))
        .await
        .unwrap();
    assert_eq!(control.ack()["head"], 0);

    let t = c
        .call(
            "thread.new",
            json!({"project": th.project, "runtime": "fake", "name": "t1", "aliases": ["tee"]}),
        )
        .await
        .unwrap();
    assert_eq!(t["name"], "t1");
    assert_eq!(t["status"], "idle");
    let sent = c
        .call("thread.send", json!({"thread": "tee", "prompt": "hello"}))
        .await
        .unwrap();
    assert_eq!(sent["turn"], 1);

    let items = until_kind(&mut control, "report.ready").await;
    let kinds: Vec<&str> = items.iter().map(|i| i.event.kind()).collect();
    assert_eq!(
        kinds,
        vec![
            "thread.created",
            "turn.started",
            "thread.started",
            "turn.ended",
            "decision.made",
            "report.ready"
        ]
    );
    assert!(items.iter().all(|i| i.stream == StreamKind::Control));
    let gseqs: Vec<u64> = items.iter().map(|i| i.gseq.unwrap()).collect();
    assert_eq!(gseqs, (1..=6).collect::<Vec<_>>());
    assert_eq!(items[0].event.source.to_string(), "user:itest");
    let report = &items[5].event;
    let data = report.body.data().unwrap();
    assert_eq!(data["summary"], "hello");
    assert_eq!(data["grade"], "success");

    // Permission round trip.
    c.call(
        "thread.send",
        json!({"thread": "t1", "prompt": "!perm Bash ls"}),
    )
    .await
    .unwrap();
    let items = until_kind(&mut control, "attention.raised").await;
    let raised = items.last().unwrap().event.body.data().unwrap();
    assert_eq!(raised["reason"], "permission");
    let attn = raised["attn_id"].as_str().unwrap().to_string();
    let listed = c.call("thread.list", Value::Null).await.unwrap();
    assert_eq!(listed[0]["status"], "waiting");
    let got = c.call("thread.get", json!({"thread": "t1"})).await.unwrap();
    assert_eq!(got["attention"][0]["attn_id"], json!(attn));
    let resolved = c
        .call(
            "attention.respond",
            json!({"attn_id": attn, "answer": "allow"}),
        )
        .await
        .unwrap();
    assert_eq!(resolved["kind"], "attention.resolved");
    assert_eq!(resolved["source"], "user:itest");
    let items = until_kind(&mut control, "report.ready").await;
    assert_eq!(items[0].event.kind(), "attention.resolved");
    assert_eq!(items[0].event.body.data().unwrap()["by"], "user");
    let e = c
        .call(
            "attention.respond",
            json!({"attn_id": attn, "answer": {"decision": "allow"}}),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::CONFLICT);

    let evs = c
        .call("thread.events", json!({"thread": "t1", "since_seq": 0}))
        .await
        .unwrap();
    let evs = evs["events"].as_array().unwrap();
    let tool_result = evs
        .iter()
        .find(|e| e["kind"] == "agent.tool_result")
        .unwrap();
    assert_eq!(tool_result["data"]["ok"], true);
    assert!(evs
        .windows(2)
        .all(|w| w[1]["seq"].as_u64() == Some(w[0]["seq"].as_u64().unwrap() + 1)));

    // Oversized text goes to the blob store; blob.get returns it.
    c.call(
        "thread.send",
        json!({"thread": "t1", "prompt": "!long 9000"}),
    )
    .await
    .unwrap();
    until_kind(&mut control, "report.ready").await;
    let last = evs.last().unwrap()["seq"].as_u64().unwrap();
    let more = c
        .call("thread.events", json!({"thread": "t1", "since_seq": last}))
        .await
        .unwrap();
    let long = more["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["kind"] == "agent.text" && e["data"]["final"] == true)
        .unwrap()
        .clone();
    let r = long["data"]["text_ref"].as_str().unwrap();
    let blob = c.call("blob.get", json!({"ref": r})).await.unwrap();
    assert_eq!(blob["encoding"], "utf8");
    assert_eq!(blob["data"].as_str().unwrap().len(), 9000);
    assert_eq!(
        (blob["size"].as_u64(), blob["offset"].as_u64()),
        (Some(9000), Some(0))
    );
    assert_eq!(blob["eof"], true);
    // Ranged reads.
    let part = c
        .call("blob.get", json!({"ref": r, "offset": 10, "len": 100}))
        .await
        .unwrap();
    assert_eq!(part["data"].as_str().unwrap().len(), 100);
    assert_eq!(
        (part["offset"].as_u64(), part["eof"].as_bool()),
        (Some(10), Some(false))
    );
    let tail = c
        .call("blob.get", json!({"ref": r, "offset": 8990, "len": 100}))
        .await
        .unwrap();
    assert_eq!(tail["data"].as_str().unwrap().len(), 10);
    assert_eq!(tail["eof"], true);
    let e = c
        .call("blob.get", json!({"ref": r, "offset": 9001}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::BAD_REQUEST);
    let e = c
        .call(
            "blob.get",
            json!({"ref": format!("sha256:{}", "0".repeat(64))}),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::NOT_FOUND);
    assert_eq!(c.blob_get(r).await.unwrap().unwrap(), vec![b'x'; 9000]);
    assert!(c
        .blob_get(&format!("sha256:{}", "0".repeat(64)))
        .await
        .unwrap()
        .is_none());

    // A chunk that splits a UTF-8 character comes back as base64.
    c.call(
        "thread.send",
        json!({"thread": "t1", "prompt": "é".repeat(3000)}),
    )
    .await
    .unwrap();
    until_kind(&mut control, "report.ready").await;
    let evs = c
        .call(
            "thread.events",
            json!({"thread": "t1", "since_seq": 0, "limit": 10000}),
        )
        .await
        .unwrap();
    let accented = evs["events"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|e| e["kind"] == "agent.text" && e["data"]["final"] == true)
        .unwrap()["data"]["text_ref"]
        .as_str()
        .unwrap()
        .to_string();
    let half = c
        .call("blob.get", json!({"ref": accented, "offset": 1, "len": 3}))
        .await
        .unwrap();
    assert_eq!(half["encoding"], "base64");
    assert_eq!(
        twr_core::b64::decode(half["data"].as_str().unwrap()).unwrap(),
        [0xA9, 0xC3, 0xA9]
    );
    assert_eq!(
        c.blob_get(&accented).await.unwrap().unwrap(),
        "é".repeat(3000).into_bytes()
    );

    // Questions can be answered by option index.
    c.call(
        "thread.send",
        json!({"thread": "t1", "prompt": "!ask Which db?|postgres|sqlite"}),
    )
    .await
    .unwrap();
    let items = until_kind(&mut control, "attention.raised").await;
    let q = items.last().unwrap().event.body.data().unwrap()["attn_id"]
        .as_str()
        .unwrap()
        .to_string();
    for bad in [
        json!({"attn_id": q, "option_index": 2}),
        json!({"attn_id": q, "option_index": 0, "answer": "postgres"}),
        json!({"attn_id": q}),
    ] {
        let e = c.call("attention.respond", bad).await.unwrap_err();
        assert_eq!(code(&e), codes::BAD_REQUEST);
    }
    let resolved = c
        .call(
            "attention.respond",
            json!({"attn_id": q, "option_index": 1, "message": "lighter"}),
        )
        .await
        .unwrap();
    assert_eq!(resolved["data"]["answer"]["decision"], "sqlite");
    assert_eq!(resolved["data"]["answer"]["message"], "lighter");
    until_kind(&mut control, "report.ready").await;

    // A thread subscription replays the backlog and continues live, gap- and dup-free.
    let mut ts = Client::subscribe_at(
        &th.socket,
        "thread.subscribe",
        json!({"thread": "t1", "since_seq": 3}),
    )
    .await
    .unwrap();
    let head = ts.ack()["head"].as_u64().unwrap();
    c.call("thread.send", json!({"thread": "t1", "prompt": "one more"}))
        .await
        .unwrap();
    let mut seqs = Vec::new();
    loop {
        let item = next_item(&mut ts).await;
        assert_eq!(item.stream, StreamKind::Thread);
        assert!(item.gseq.is_none());
        seqs.push(item.event.seq);
        if item.event.kind() == "report.ready" && item.event.seq > head {
            break;
        }
    }
    assert_eq!(seqs, (4..=*seqs.last().unwrap()).collect::<Vec<_>>());

    // control.read pages; a late control subscriber sees the same sequence.
    let page = c
        .call("control.read", json!({"since": 2, "limit": 3}))
        .await
        .unwrap();
    let page: Vec<u64> = page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["gseq"].as_u64().unwrap())
        .collect();
    assert_eq!(page, vec![3, 4, 5]);
    let mut late = Client::subscribe_at(&th.socket, "control.subscribe", json!({"since": 0}))
        .await
        .unwrap();
    let late_head = late.ack()["head"].as_u64().unwrap();
    for g in 1..=late_head {
        assert_eq!(next_item(&mut late).await.gseq, Some(g));
    }

    // Errors.
    let e = c.call("no.such", Value::Null).await.unwrap_err();
    assert_eq!(code(&e), codes::METHOD_NOT_FOUND);
    let e = c
        .call("thread.get", json!({"thread": "zzz"}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::NOT_FOUND);
    let e = c
        .call("thread.send", json!({"thread": "t1"}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::BAD_REQUEST);
    let mut raw = tokio::net::UnixStream::connect(&th.socket).await.unwrap();
    raw.write_all(b"{not json}\n").await.unwrap();
    let mut line = String::new();
    BufReader::new(&mut raw).read_line(&mut line).await.unwrap();
    let resp: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(resp["error"]["code"], codes::BAD_REQUEST);

    // Shutdown stops the runtime, ends subscriptions and removes the socket.
    let ok = c.call("hub.shutdown", Value::Null).await.unwrap();
    assert_eq!(ok["ok"], true);
    let rest = timeout(WAIT, async {
        let mut kinds = Vec::new();
        while let Some(item) = control.next().await.unwrap() {
            kinds.push(item.event.kind().to_string());
        }
        kinds
    })
    .await
    .unwrap();
    assert_eq!(rest.last().map(String::as_str), Some("thread.stopped"));
    timeout(WAIT, th.task).await.unwrap().unwrap().unwrap();
    assert!(!th.socket.exists());
}

/// `thread.get` / `thread.list` show the requested `model` (null: the runtime's default) and
/// the one the runtime reports (`model_actual`, from `thread.started`).
#[tokio::test]
async fn threads_show_the_requested_and_the_reported_model() {
    let th = start().await;
    let mut c = Client::connect(&th.socket).await.unwrap();
    let mut control = Client::subscribe_at(&th.socket, "control.subscribe", json!({"since": 0}))
        .await
        .unwrap();
    for (name, model) in [("t1", None), ("t2", Some("fake-big"))] {
        let mut p = json!({"project": th.project, "runtime": "fake", "name": name});
        if let Some(m) = model {
            p["model"] = json!(m);
        }
        let t = c.call("thread.new", p).await.unwrap();
        assert_eq!(
            (&t["model"], &t["model_actual"]),
            (&json!(model), &Value::Null)
        );
        c.call("thread.send", json!({"thread": name, "prompt": "hi"}))
            .await
            .unwrap();
        until_kind(&mut control, "report.ready").await;
    }
    let want = [
        ("t1", Value::Null, json!(twr_adapters::fake::DEFAULT_MODEL)),
        ("t2", json!("fake-big"), json!("fake-big")),
    ];
    let check = |listed: &Value, got: &[Value]| {
        for (i, (name, model, actual)) in want.iter().enumerate() {
            assert_eq!(listed[i]["name"], *name);
            for t in [&listed[i], &got[i]] {
                assert_eq!((&t["model"], &t["model_actual"]), (model, actual), "{t}");
            }
        }
    };
    let listed = c.call("thread.list", Value::Null).await.unwrap();
    let mut got = Vec::new();
    for (name, ..) in &want {
        got.push(c.call("thread.get", json!({"thread": name})).await.unwrap());
    }
    check(&listed, &got);
}

/// End to end with the fake runtime, which interrupts a turn and keeps its session: a message
/// sent with `thread.send` comes back with its `ref`; `message.withdraw` takes back a running
/// one (its turn ends `interrupted`, unreported) and a queued one (it never runs), each on the
/// control stream; too late, it says why.
#[tokio::test]
async fn a_message_is_taken_back_over_the_socket() {
    let th = start().await;
    let mut c = Client::connect(&th.socket).await.unwrap();
    let mut control = Client::subscribe_at(&th.socket, "control.subscribe", json!({"since": 0}))
        .await
        .unwrap();
    c.call(
        "thread.new",
        json!({"project": th.project, "runtime": "fake", "name": "t1"}),
    )
    .await
    .unwrap();
    let sent = c
        .call(
            "thread.send",
            json!({"thread": "t1", "prompt": "!sleep 60000"}),
        )
        .await
        .unwrap();
    assert_eq!(sent["turn"], 1);
    assert_eq!(sent["ref"], sent["queued"]);
    let queued = c
        .call(
            "thread.send",
            json!({"thread": "t1", "prompt": "then this"}),
        )
        .await
        .unwrap();
    assert_eq!(queued["turn"], Value::Null);
    let w = c
        .call(
            "message.withdraw",
            json!({"thread": "t1", "ref": queued["ref"]}),
        )
        .await
        .unwrap();
    assert_eq!(
        (&w["withdrawn"], &w["state"]),
        (&json!(true), &json!("queued"))
    );
    let w = c
        .call(
            "message.withdraw",
            json!({"thread": "t1", "ref": sent["ref"]}),
        )
        .await
        .unwrap();
    assert_eq!(
        (&w["withdrawn"], &w["state"]),
        (&json!(true), &json!("running"))
    );
    let items = until_kind(&mut control, "turn.ended").await;
    let kinds: Vec<&str> = items.iter().map(|i| i.event.kind()).collect();
    assert!(kinds.contains(&"prompt.withdrawn"), "{kinds:?}");
    assert!(kinds.contains(&"turn.withdrawn"), "{kinds:?}");
    match &items.last().unwrap().event.body {
        EventBody::TurnEnded(p) => assert_eq!(p.result, TurnResult::Interrupted),
        other => panic!("{other:?}"),
    }
    let late = c
        .call(
            "message.withdraw",
            json!({"thread": "t1", "ref": sent["ref"]}),
        )
        .await
        .unwrap();
    assert_eq!(late["reason"], "withdrawn", "{late}");
    // The session goes on with the next message; nothing ran in between.
    let next = c
        .call(
            "thread.send",
            json!({"thread": "t1", "prompt": "hello again"}),
        )
        .await
        .unwrap();
    assert_eq!(next["turn"], 2);
    let items = until_kind(&mut control, "report.ready").await;
    assert!(items
        .iter()
        .all(|i| i.event.kind() != "report.ready" || i.event.turn == Some(2)));
    let ended = c
        .call(
            "message.withdraw",
            json!({"thread": "t1", "ref": next["ref"]}),
        )
        .await
        .unwrap();
    assert_eq!(ended["reason"], "ended", "{ended}");
    let e = c
        .call("message.withdraw", json!({"thread": "t1"}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::BAD_REQUEST);
}

#[tokio::test]
async fn bind_replaces_stale_sockets_and_creates_a_private_dir() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("run").join("twr").join("hub.sock");
    let l = server::bind(&sock).await.unwrap();
    let dmode = std::fs::metadata(sock.parent().unwrap())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(dmode & 0o777, 0o700);
    drop(l); // leaves a socket file nobody answers on
    assert!(sock.exists());
    let _l = server::bind(&sock).await.unwrap();
    // A regular file is never removed.
    let file = dir.path().join("not-a-socket");
    std::fs::write(&file, "x").unwrap();
    assert!(server::bind(&file).await.is_err());
    assert!(file.exists());
}

/// A runtime that ignores `stop` (it only ends when the hub drops its handle), so a hub
/// shutdown takes the whole stop timeout.
struct Stubborn;

impl Adapter for Stubborn {
    fn info(&self) -> AdapterInfo {
        AdapterInfo {
            name: "stubborn",
            tier: Tier::Headless,
            description: "ignores stop",
        }
    }

    fn spawn(&self, _cfg: SpawnConfig, _prompt: Option<String>) -> anyhow::Result<AdapterHandle> {
        let (h, mut end) = AdapterHandle::pair();
        tokio::spawn(async move {
            end.emit(EventBody::ThreadStarted(SessionPayload::default()), None);
            while end.cmds.recv().await.is_some() {}
            end.exited(Some(0), "");
        });
        Ok(h)
    }
}

async fn wait_until(what: &str, f: impl Fn() -> bool) {
    timeout(WAIT, async {
        while !f() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {what}"));
}

async fn info_over(sock: &Path) -> Value {
    let mut c = Client::connect(sock).await.unwrap();
    timeout(WAIT, c.call("hub.info", Value::Null))
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn shutdown_unlinks_its_socket_at_once_and_never_a_newer_one() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("hub.sock");
    let project = dir.path().join("proj");
    std::fs::create_dir(&project).unwrap();

    let mut o = HubOptions::new(dir.path().join("a").join("twr.db"), Config::default());
    o.adapters = Arc::new(|n: &str| -> Option<Arc<dyn Adapter>> {
        (n == "stubborn").then(|| Arc::new(Stubborn) as Arc<dyn Adapter>)
    });
    o.stop_timeout = Duration::from_millis(1500);
    let listener = server::bind(&sock).await.unwrap();
    let hub_a = Hub::open(o).await.unwrap();
    let t = hub_a
        .new_thread(
            NewThread {
                project: project.to_string_lossy().into_owned(),
                runtime: Some("stubborn".into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap();
    hub_a
        .send(
            t["id"].as_str().unwrap(),
            "go",
            TurnFrom::User,
            Source::User("test".into()),
        )
        .unwrap();
    assert_eq!(hub_a.live_runtimes(), 1);
    let task_a = tokio::spawn(server::serve(hub_a.clone(), listener, sock.clone(), false));
    let pid_a = info_over(&sock).await["pid"].clone();

    // The socket disappears as soon as the hub stops accepting, while its runtime is still
    // being stopped.
    hub_a.request_shutdown();
    wait_until("the old socket to be unlinked", || !sock.exists()).await;
    assert!(!task_a.is_finished());

    // Meanwhile a new hub (its own data dir) binds the same path.
    let listener_b = server::bind(&sock).await.unwrap();
    let hub_b = Hub::open(HubOptions::new(
        dir.path().join("b").join("twr.db"),
        Config::default(),
    ))
    .await
    .unwrap();
    let task_b = tokio::spawn(server::serve(hub_b, listener_b, sock.clone(), false));

    // The old hub finishes its shutdown without touching the new socket.
    timeout(WAIT, task_a).await.unwrap().unwrap().unwrap();
    assert_eq!(hub_a.live_runtimes(), 0);
    assert!(sock.exists());
    let info = info_over(&sock).await;
    assert_eq!(info["db"], json!(dir.path().join("b").join("twr.db")));
    assert_eq!(info["pid"], pid_a, "same process, different hub");

    let mut c = Client::connect(&sock).await.unwrap();
    c.call("hub.shutdown", Value::Null).await.unwrap();
    timeout(WAIT, task_b).await.unwrap().unwrap().unwrap();
    assert!(!sock.exists());
}

/// In a directory every user may write to, the socket is bound only with the sticky bit (like
/// `/tmp`): without it another user could rename the private directory it is first bound in,
/// or the socket, while the hub sets them up, so the hub refuses it before binding anything.
#[tokio::test]
async fn bind_is_private_even_in_a_shared_directory() {
    let dir = tempfile::tempdir().unwrap();
    let shared = dir.path().join("shared");
    std::fs::create_dir(&shared).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
    let sock = shared.join("hub.sock");
    let e = server::bind(&sock).await.unwrap_err();
    assert!(
        format!("{e:#}").contains("is writable by every user (mode 777) and has no sticky bit"),
        "{e:#}"
    );
    assert_eq!(std::fs::read_dir(&shared).unwrap().count(), 0);
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).unwrap();
    let _l = server::bind(&sock).await.unwrap();
    let meta = std::fs::symlink_metadata(&sock).unwrap();
    assert!(meta.file_type().is_socket());
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);
    // An existing directory is left as it is, and no temporary entries remain.
    let dmode = std::fs::metadata(&shared).unwrap().permissions().mode();
    assert_eq!(dmode & 0o7777, 0o1777);
    let names: Vec<String> = std::fs::read_dir(&shared)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["hub.sock"]);
    // The listener answers at the final path.
    let accept = tokio::spawn(async move { _l.accept().await.map(|_| ()) });
    tokio::net::UnixStream::connect(&sock).await.unwrap();
    timeout(WAIT, accept).await.unwrap().unwrap().unwrap();
}

#[tokio::test]
async fn bad_frames_close_the_connection() {
    let th = start().await;

    // Malformed JSON: an error, then EOF (the request after it is never answered).
    let mut raw = tokio::net::UnixStream::connect(&th.socket).await.unwrap();
    raw.write_all(b"{not json}\n{\"id\":1,\"method\":\"hub.info\"}\n")
        .await
        .unwrap();
    let mut r = BufReader::new(raw);
    let mut line = String::new();
    r.read_line(&mut line).await.unwrap();
    let resp: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(resp["error"]["code"], codes::BAD_REQUEST);
    line.clear();
    let n = timeout(WAIT, r.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(n, 0, "expected EOF, got {line:?}");

    // Oversized frame: same.
    let raw = tokio::net::UnixStream::connect(&th.socket).await.unwrap();
    let (rd, mut wr) = raw.into_split();
    let writer = tokio::spawn(async move {
        let chunk = vec![b'x'; 1 << 20];
        for _ in 0..=(twr_core::proto::MAX_FRAME >> 20) {
            wr.write_all(&chunk).await.unwrap();
        }
        wr.write_all(b"\n").await.unwrap();
        wr // kept open: the EOF below must come from the hub
    });
    let mut r = BufReader::new(rd);
    let mut line = String::new();
    timeout(WAIT, r.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    let resp: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(resp["error"]["code"], codes::BAD_REQUEST);
    assert!(
        resp["error"]["message"]
            .as_str()
            .unwrap()
            .contains("MAX_FRAME"),
        "{resp}"
    );
    let _wr = timeout(WAIT, writer).await.unwrap().unwrap();
    line.clear();
    let n = timeout(WAIT, r.read_line(&mut line))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(n, 0, "expected EOF, got {line:?}");

    // Other connections are unaffected.
    let mut c = Client::connect(&th.socket).await.unwrap();
    c.call("hub.info", Value::Null).await.unwrap();
    c.call("hub.shutdown", Value::Null).await.unwrap();
    timeout(WAIT, th.task).await.unwrap().unwrap().unwrap();
}

/// A connection kept open across a hub stop (the TUI's, the MCP server's) is told why it
/// closes: the next call on it fails saying the hub stopped and that nothing was done, which
/// a client may resend on a new connection. A request the hub cannot read is refused with
/// why. Before, both came out as "Broken pipe" or "closed the connection".
#[tokio::test]
async fn a_connection_left_open_across_a_stop_is_told_why() {
    use twr_core::client::HubClosed;
    let th = start().await;
    let mut stale = Client::connect(&th.socket).await.unwrap();
    stale.call("hub.info", Value::Null).await.unwrap();

    let mut big = Client::connect(&th.socket).await.unwrap();
    let prompt = "x".repeat(twr_core::proto::MAX_FRAME + 1);
    let e = timeout(
        WAIT,
        big.call("thread.send", json!({"thread": "w1", "prompt": prompt})),
    )
    .await
    .unwrap()
    .unwrap_err();
    let closed = e.downcast_ref::<HubClosed>().expect("a HubClosed");
    assert!(!closed.resendable(), "{closed:?}");
    assert_eq!(closed.said.as_ref().unwrap().code, codes::BAD_REQUEST);
    assert!(
        format!("{e:#}").contains("refused the request for thread.send"),
        "{e:#}"
    );

    let mut c = Client::connect(&th.socket).await.unwrap();
    c.call("hub.shutdown", Value::Null).await.unwrap();
    timeout(WAIT, th.task).await.unwrap().unwrap().unwrap();
    let e = stale
        .call("thread.send", json!({"thread": "w1", "prompt": "hi"}))
        .await
        .unwrap_err();
    let closed = e.downcast_ref::<HubClosed>().expect("a HubClosed");
    assert!(closed.resendable(), "{closed:?}");
    assert_eq!(closed.said.as_ref().unwrap().code, codes::STOPPING);
    assert!(format!("{e:#}").contains("the hub is stopping"), "{e:#}");
}

// --- attach lease, attention.list, socket path length -------------------------------------------

/// Control items until `pred` matches.
async fn until_item(sub: &mut Subscription, pred: impl Fn(&StreamItem) -> bool) -> StreamItem {
    loop {
        let item = next_item(sub).await;
        if pred(&item) {
            return item;
        }
    }
}

/// A process carrying `TWR_ATTACH=<marker>` that runs until killed, with its start time.
fn attach_process(marker: &str) -> (std::process::Child, u64) {
    let child = std::process::Command::new("sleep")
        .arg("600")
        .env("TWR_ATTACH", marker)
        .spawn()
        .unwrap();
    let start = twr_runner::reap::process_start_time(child.id()).unwrap();
    (child, start)
}

/// The attach outlives its lease connection while the interactive process its client reported
/// runs (the client was killed, the user's CLI was not): the thread stays attached, cannot be
/// parked, and dispatches nothing. Once the process exits the hub detaches (reason `attach
/// process exited`) and the queued prompt runs.
#[tokio::test]
async fn an_attach_lasts_while_its_process_runs() {
    let th = start().await;
    let mut c = Client::connect(&th.socket).await.unwrap();
    c.call("hello", json!({"client": "itest"})).await.unwrap();
    let mut control = Client::subscribe_at(&th.socket, "control.subscribe", json!({"since": 0}))
        .await
        .unwrap();
    c.call(
        "thread.new",
        json!({"project": th.project, "runtime": "fake", "name": "t1"}),
    )
    .await
    .unwrap();
    c.call("thread.send", json!({"thread": "t1", "prompt": "hello"}))
        .await
        .unwrap();
    until_kind(&mut control, "report.ready").await;

    let mut lease = Client::connect(&th.socket).await.unwrap();
    lease.call("hello", json!({"client": "tty"})).await.unwrap();
    let grant = lease
        .call("attach.begin", json!({"thread": "t1"}))
        .await
        .unwrap();
    until_kind(&mut control, "thread.attached").await;
    let (mut child, start_time) = attach_process(grant["env"]["TWR_ATTACH"].as_str().unwrap());
    let pid = child.id();
    // Another process's start time: not that process.
    let e = lease
        .call(
            "attach.child",
            json!({"thread": "t1", "pid": pid, "start_time": start_time + 1}),
        )
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::BAD_REQUEST, "{e:#}");
    lease
        .call(
            "attach.child",
            json!({"thread": "t1", "pid": pid, "start_time": start_time}),
        )
        .await
        .unwrap();
    let t = c.call("thread.get", json!({"thread": "t1"})).await.unwrap();
    assert_eq!(t["attach_process"], json!({"pid": pid, "orphaned": false}));

    // The client goes away; its process does not.
    drop(lease);
    timeout(WAIT, async {
        loop {
            let t = c.call("thread.get", json!({"thread": "t1"})).await.unwrap();
            if t["attach_process"]["orphaned"] == true {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    c.call("thread.send", json!({"thread": "t1", "prompt": "queued"}))
        .await
        .unwrap();
    let e = c
        .call("thread.park", json!({"thread": "t1"}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::CONFLICT, "{e:#}");
    assert!(format!("{e:#}").contains("attached"), "{e:#}");
    // Well past a few checks of the process: still attached, nothing dispatched.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let t = c.call("thread.get", json!({"thread": "t1"})).await.unwrap();
    assert_eq!(t["status"], "attached", "{t}");
    assert_eq!(t["pending"], 1, "{t}");
    let evs = c
        .call("thread.events", json!({"thread": "t1"}))
        .await
        .unwrap();
    let kinds: Vec<&str> = evs["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    let at = kinds.iter().rposition(|k| *k == "thread.attached").unwrap();
    assert!(
        !kinds[at..].contains(&"thread.detached") && !kinds[at..].contains(&"turn.started"),
        "{kinds:?}"
    );

    // The process exits: detached, and the queued prompt runs.
    child.kill().unwrap();
    child.wait().unwrap();
    let detached = until_item(&mut control, |i| i.event.kind() == "thread.detached").await;
    let data = detached.event.body.data().unwrap();
    assert_eq!(data["ext"]["twr"]["reason"], "attach process exited");
    assert_eq!(detached.event.source.to_string(), "hub");
    let started = until_item(&mut control, |i| i.event.kind() == "turn.started").await;
    assert_eq!(started.event.body.data().unwrap()["prompt"], "queued");
    until_kind(&mut control, "report.ready").await;
    let t = c.call("thread.get", json!({"thread": "t1"})).await.unwrap();
    assert!(t.get("attach_process").is_none(), "{t}");

    c.call("hub.shutdown", Value::Null).await.unwrap();
    th.task.await.unwrap().unwrap();
}

#[tokio::test]
async fn the_attach_lease_is_its_connection() {
    let th = start().await;
    let mut c = Client::connect(&th.socket).await.unwrap();
    c.call("hello", json!({"client": "itest"})).await.unwrap();
    let mut control = Client::subscribe_at(&th.socket, "control.subscribe", json!({"since": 0}))
        .await
        .unwrap();
    c.call(
        "thread.new",
        json!({"project": th.project, "runtime": "fake", "name": "t1"}),
    )
    .await
    .unwrap();
    c.call("thread.send", json!({"thread": "t1", "prompt": "hello"}))
        .await
        .unwrap();
    until_kind(&mut control, "report.ready").await;

    // A permission is pending: attention.list shows it (with --all semantics too).
    c.call(
        "thread.send",
        json!({"thread": "t1", "prompt": "!perm Bash ls"}),
    )
    .await
    .unwrap();
    until_kind(&mut control, "attention.raised").await;
    let listed = c.call("attention.list", Value::Null).await.unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["thread_name"], "t1");
    assert_eq!(listed[0]["reason"], "permission");
    assert_eq!(listed[0]["options"], json!(["allow", "deny"]));

    // Lease 1: a turn is running, so only a forced attach goes through.
    let mut lease = Client::connect(&th.socket).await.unwrap();
    lease.call("hello", json!({"client": "tty"})).await.unwrap();
    let e = lease
        .call("attach.begin", json!({"thread": "t1"}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::CONFLICT);
    let grant = lease
        .call("attach.begin", json!({"thread": "t1", "force": true}))
        .await
        .unwrap();
    assert_eq!(grant["argv"][0], "sh");
    assert_eq!(grant["runtime"], "fake");
    assert!(grant["session_id"].is_string());
    assert!(grant["lease"].is_u64(), "{grant}");
    let attached = until_kind(&mut control, "thread.attached").await;
    assert_eq!(
        attached.last().unwrap().event.source.to_string(),
        "user:tty"
    );
    // Nothing blocks the thread any more (the forced attach closed the request).
    let listed = c
        .call("attention.list", json!({"all": true}))
        .await
        .unwrap();
    assert!(listed.as_array().unwrap().is_empty(), "{listed}");
    let sent = c
        .call("thread.send", json!({"thread": "t1", "prompt": "queued"}))
        .await
        .unwrap();
    assert!(sent["turn"].is_null());
    let t = c.call("thread.get", json!({"thread": "t1"})).await.unwrap();
    assert_eq!(t["status"], "attached");
    assert_eq!(t["attached"], "tty");

    // Another connection cannot end this attach, not even as the same client: nothing is
    // detached and the queued prompt stays queued.
    for other in ["itest", "tty"] {
        let mut b = Client::connect(&th.socket).await.unwrap();
        b.call("hello", json!({"client": other})).await.unwrap();
        let e = b
            .call("attach.end", json!({"thread": "t1"}))
            .await
            .unwrap_err();
        assert_eq!(code(&e), codes::CONFLICT, "{e:#}");
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let t = c.call("thread.get", json!({"thread": "t1"})).await.unwrap();
    assert_eq!(t["status"], "attached");
    assert_eq!(t["pending"], 1);
    assert_eq!(t["turns"], 2);

    // The grant marks the interactive process for this lease. Only the lease connection
    // reports that process, and only one carrying the marker: this test process is not it.
    let id = t["id"].as_str().unwrap();
    let marker = grant["env"]["TWR_ATTACH"].as_str().unwrap();
    assert_eq!(marker, format!("{id}:{}", grant["lease"]));
    let me = std::process::id();
    let start_time = twr_runner::reap::process_start_time(me).unwrap();
    let report = json!({"thread": "t1", "pid": me, "start_time": start_time});
    let mut other = Client::connect(&th.socket).await.unwrap();
    other.call("hello", json!({"client": "tty"})).await.unwrap();
    let e = other
        .call("attach.child", report.clone())
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::CONFLICT, "{e:#}");
    let e = lease.call("attach.child", report).await.unwrap_err();
    assert_eq!(code(&e), codes::BAD_REQUEST, "{e:#}");
    let t = c.call("thread.get", json!({"thread": "t1"})).await.unwrap();
    assert!(t.get("attach_process").is_none(), "{t}");

    // The lease connection drops (no process reported): the hub detaches and the queued
    // prompt runs.
    drop(lease);
    let detached = until_item(&mut control, |i| i.event.kind() == "thread.detached").await;
    let data = detached.event.body.data().unwrap();
    assert_eq!(data["client"], "tty");
    assert_eq!(data["ext"]["twr"]["reason"], "connection_closed");
    let started = until_item(&mut control, |i| i.event.kind() == "turn.started").await;
    assert_eq!(started.event.body.data().unwrap()["prompt"], "queued");
    until_kind(&mut control, "report.ready").await;

    // Lease 2 ends explicitly; closing its connection afterwards changes nothing.
    let mut lease = Client::connect(&th.socket).await.unwrap();
    lease
        .call("attach.begin", json!({"thread": "t1"}))
        .await
        .unwrap();
    until_kind(&mut control, "thread.attached").await;
    let ended = lease
        .call("attach.end", json!({"thread": "t1"}))
        .await
        .unwrap();
    assert_eq!(ended["detached"], true);
    let detached = until_item(&mut control, |i| i.event.kind() == "thread.detached").await;
    assert!(detached.event.body.data().unwrap().get("ext").is_none());
    drop(lease);
    let e = c
        .call("attach.end", json!({"thread": "t1"}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::CONFLICT);
    // Give a (wrong) second detach the chance to show up, then check there was none.
    c.call("thread.send", json!({"thread": "t1", "prompt": "after"}))
        .await
        .unwrap();
    let items = until_kind(&mut control, "report.ready").await;
    assert!(items.iter().all(|i| i.event.kind() != "thread.detached"));

    c.call("hub.shutdown", Value::Null).await.unwrap();
    th.task.await.unwrap().unwrap();
}

#[tokio::test]
async fn socket_paths_too_long_for_unix_sockets_are_refused_clearly() {
    let dir = tempfile::tempdir().unwrap();
    let long = dir.path().join("d".repeat(120)).join("hub.sock");
    let e = server::bind(&long).await.unwrap_err().to_string();
    let most = format!("at most {} bytes", twr_core::paths::MAX_SOCKET_PATH);
    assert!(e.contains(&most), "{e}");
    assert!(e.contains("--socket"), "{e}");
    assert!(!long.parent().unwrap().exists(), "nothing created");
    let e = Client::connect(&long).await.unwrap_err().to_string();
    assert!(e.contains(&most), "{e}");

    // Short enough itself, but the temporary socket the hub binds first (in a private
    // directory next to it) would not be.
    let base = dir.path().as_os_str().len() + 1;
    let max = twr_core::paths::MAX_SOCKET_PATH;
    let fit = dir.path().join("x".repeat(max - 2 - base)).join("s");
    assert_eq!(fit.as_os_str().len(), max);
    let e = server::bind(&fit).await.unwrap_err().to_string();
    assert!(e.contains("too long") && e.contains("--socket"), "{e}");
    assert_eq!(
        std::fs::read_dir(fit.parent().unwrap()).unwrap().count(),
        0,
        "the temporary directory is removed"
    );
}

/// Reads `method` from `since` to the end, page by page, following `more` / `next`; returns
/// every item's cursor (seq or gseq) and the number of pages.
async fn read_all(
    c: &mut Client,
    method: &str,
    params: Value,
    list: &str,
    cursor: &str,
) -> (Vec<u64>, usize) {
    let mut params = params;
    let since_key = if method == "thread.events" {
        "since_seq"
    } else {
        "since"
    };
    let mut seen = Vec::new();
    let mut pages = 0;
    loop {
        let r = c.call(method, params.clone()).await.unwrap();
        pages += 1;
        let items = r[list].as_array().unwrap();
        assert!(!items.is_empty() || r["more"] == false);
        seen.extend(items.iter().map(|i| i[cursor].as_u64().unwrap()));
        assert_eq!(
            r["next"].as_u64(),
            seen.last().copied().or(params[since_key].as_u64())
        );
        if r["more"] == false {
            return (seen, pages);
        }
        params[since_key] = r["next"].clone();
    }
}

#[tokio::test]
async fn history_pages_stay_under_the_frame_limit() {
    let th = start().await;
    let thread = th
        .hub
        .new_thread(
            NewThread {
                project: th.project.to_string_lossy().into_owned(),
                runtime: Some("fake".into()),
                name: Some("big".into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    // 5000 control events of ~3.5 KB each (under the payload cap, so inline): ~18 MB in all,
    // more than one 16 MiB frame.
    for i in 0..5000 {
        th.hub
            .append(
                &thread,
                None,
                Source::Hub,
                EventBody::ReportReady(ReportReady {
                    summary: format!("{i:05} {}", "x".repeat(3494)),
                    grade: Grade::Success,
                    written_by: "hub".into(),
                    ..Default::default()
                }),
                None,
            )
            .unwrap();
    }
    let mut c = Client::connect(&th.socket).await.unwrap();

    // The largest page a client may ask for arrives (the shared client refuses frames over
    // MAX_FRAME), bounded by size rather than count.
    let r = c
        .call(
            "thread.events",
            json!({"thread": thread, "since_seq": 0, "limit": 10_000}),
        )
        .await
        .unwrap();
    let n = r["events"].as_array().unwrap().len();
    assert!(n > 1 && n < 5001, "{n}");
    assert_eq!(r["more"], true);
    assert_eq!(r["next"], r["last_seq"]);
    assert_eq!(r["next"].as_u64(), Some(n as u64));
    assert!(serde_json::to_vec(&r).unwrap().len() <= server::PAGE_BYTES + 4096);

    let (seqs, pages) = read_all(
        &mut c,
        "thread.events",
        json!({"thread": thread, "since_seq": 0, "limit": 10_000}),
        "events",
        "seq",
    )
    .await;
    assert_eq!(seqs, (1..=5001).collect::<Vec<u64>>());
    assert!(pages >= 3, "{pages}");
    let (gseqs, pages) = read_all(
        &mut c,
        "control.read",
        json!({"since": 0, "limit": 10_000}),
        "items",
        "gseq",
    )
    .await;
    assert_eq!(gseqs, (1..=5001).collect::<Vec<u64>>());
    assert!(pages >= 3, "{pages}");

    // Count-bounded pages say whether more follow, too.
    let r = c
        .call(
            "thread.events",
            json!({"thread": thread, "since_seq": 4999, "limit": 1}),
        )
        .await
        .unwrap();
    assert_eq!(
        (r["events"].as_array().unwrap().len(), r["more"].clone()),
        (1, json!(true))
    );
    let r = c
        .call(
            "thread.events",
            json!({"thread": thread, "since_seq": 4999, "limit": 2}),
        )
        .await
        .unwrap();
    assert_eq!(
        (r["events"].as_array().unwrap().len(), r["more"].clone()),
        (2, json!(false))
    );
    let r = c
        .call("control.read", json!({"since": 5001}))
        .await
        .unwrap();
    assert_eq!(r["items"], json!([]));
    assert_eq!(
        (r["more"].clone(), r["next"].clone()),
        (json!(false), json!(5001))
    );
}

#[tokio::test]
async fn an_attach_end_that_fails_leaves_the_lease_with_its_connection() {
    let th = start().await;
    let mut c = Client::connect(&th.socket).await.unwrap();
    c.call("hello", json!({"client": "itest"})).await.unwrap();
    let mut control = Client::subscribe_at(&th.socket, "control.subscribe", json!({"since": 0}))
        .await
        .unwrap();
    c.call(
        "thread.new",
        json!({"project": th.project, "runtime": "fake", "name": "t1"}),
    )
    .await
    .unwrap();
    c.call("thread.send", json!({"thread": "t1", "prompt": "hello"}))
        .await
        .unwrap();
    until_kind(&mut control, "report.ready").await;
    let mut lease = Client::connect(&th.socket).await.unwrap();
    lease.call("hello", json!({"client": "tty"})).await.unwrap();
    lease
        .call("attach.begin", json!({"thread": "t1"}))
        .await
        .unwrap();
    // `thread.detached` cannot be recorded for now.
    let db = rusqlite::Connection::open(Paths::with_home(th._dir.path()).db_file()).unwrap();
    db.busy_timeout(Duration::from_secs(5)).unwrap();
    db.execute_batch(
        "CREATE TRIGGER inject BEFORE INSERT ON events WHEN NEW.kind = 'thread.detached'
         BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;",
    )
    .unwrap();
    let e = lease
        .call("attach.end", json!({"thread": "t1"}))
        .await
        .unwrap_err();
    assert_eq!(code(&e), codes::INTERNAL);
    // The connection still holds the lease: ending again works once the log can record it.
    db.execute_batch("DROP TRIGGER inject").unwrap();
    let r = lease
        .call("attach.end", json!({"thread": "t1"}))
        .await
        .unwrap();
    assert_eq!(r["detached"], json!(true));
    until_kind(&mut control, "thread.detached").await;
}

/// A twr client older than the hub (its own client names without `protocol`, or an older
/// `protocol`) is told to update at `hello`, before any event it cannot decode reaches it.
/// Other clients may leave `protocol` out.
#[tokio::test]
async fn a_twr_older_than_the_hub_is_told_to_update() {
    let th = start().await;
    for params in [
        json!({"client": "tui"}),
        json!({"client": "cli"}),
        json!({"client": "itest", "protocol": 0}),
    ] {
        let mut c = Client::connect(&th.socket).await.unwrap();
        let e = c.call("hello", params.clone()).await.unwrap_err();
        assert_eq!(code(&e), codes::UNSUPPORTED, "{params}");
        assert!(e.to_string().contains("twr update"), "{e}");
    }
    let mut c = Client::connect(&th.socket).await.unwrap();
    let hello = c.call("hello", json!({"client": "script"})).await.unwrap();
    assert_eq!(hello["identity"], "user:script");
    let mut c = Client::connect(&th.socket).await.unwrap();
    let hello = c.hello(json!({"client": "tui"})).await.unwrap();
    assert_eq!(hello["identity"], "user:tui");
}

/// One connection holds at most [`server::SUBS_PER_CONNECTION`] subscriptions at once: one
/// more is refused (`conflict`) and the connection goes on; another connection still
/// subscribes.
#[tokio::test]
async fn a_connection_holds_a_bounded_number_of_subscriptions() {
    let th = start().await;
    let raw = tokio::net::UnixStream::connect(&th.socket).await.unwrap();
    let (r, mut w) = raw.into_split();
    let n = server::SUBS_PER_CONNECTION as u64 + 1;
    for id in 1..=n {
        let req = json!({"id": id, "method": "control.subscribe", "params": {"since": 0}});
        w.write_all(format!("{req}\n").as_bytes()).await.unwrap();
    }
    let probe = json!({"id": n + 1, "method": "hub.info"});
    w.write_all(format!("{probe}\n").as_bytes()).await.unwrap();
    let mut lines = BufReader::new(r).lines();
    let mut answers = std::collections::BTreeMap::new();
    timeout(WAIT, async {
        while answers.len() < n as usize + 1 {
            let line = lines.next_line().await.unwrap().expect("the hub hung up");
            let v: Value = serde_json::from_str(&line).unwrap();
            // Stream items carry no id.
            if let Some(id) = v.get("id").and_then(Value::as_u64) {
                answers.insert(id, v);
            }
        }
    })
    .await
    .expect("every request answered");
    for id in 1..n {
        assert!(answers[&id].get("result").is_some(), "{}", answers[&id]);
    }
    let refused = &answers[&n];
    assert_eq!(refused["error"]["code"], codes::CONFLICT, "{refused}");
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("subscriptions already"),
        "{refused}"
    );
    assert!(answers[&(n + 1)].get("result").is_some());
    let mut other = Client::subscribe_at(&th.socket, "control.subscribe", json!({"since": 0}))
        .await
        .unwrap();
    let _ = timeout(Duration::from_millis(200), other.next()).await;
    drop(w);
    th.hub.request_shutdown();
    let _ = timeout(WAIT, th.task).await;
}

/// `control.subscribe` with `compact`: the ack says how many stored items come before the
/// live ones, and they are the latest of what folds to the latest; then live items follow.
/// Without it nothing changes; a `compact` that is no boolean, or one asked of
/// `control.read`, is refused.
#[tokio::test]
async fn a_compacted_control_subscription_says_its_backlog() {
    let th = start().await;
    let snapshot = |dirty| {
        EventBody::GitSnapshot(twr_core::event::GitSnapshot {
            dirty_files: dirty,
            ..Default::default()
        })
    };
    for dirty in 0..50 {
        th.hub
            .append("w1", None, Source::Hub, snapshot(dirty), None)
            .unwrap();
    }
    // A task that ran and ended with nothing reading it: the head, not in the backlog.
    for running in [true, false] {
        let task = EventBody::TaskUpdated(twr_core::event::TaskUpdated {
            task_id: "bg".into(),
            running,
            ..Default::default()
        });
        let source = Source::Adapter("fake".into());
        th.hub.append("w1", None, source, task, None).unwrap();
    }
    let mut full = Client::subscribe_at(&th.socket, "control.subscribe", json!({"since": 0}))
        .await
        .unwrap();
    assert_eq!(full.ack()["head"], 52);
    assert!(full.ack().get("backlog").is_none() && full.ack().get("compacted").is_none());
    let mut sub = Client::subscribe_at(
        &th.socket,
        "control.subscribe",
        json!({"since": 0, "compact": true}),
    )
    .await
    .unwrap();
    let ack = sub.ack().clone();
    assert_eq!(ack["head"], 52, "{ack}");
    assert_eq!(ack["compacted"], true, "{ack}");
    // The thread's first item (its view starts then) and its latest snapshot.
    assert_eq!(ack["backlog"], 2, "{ack}");
    assert_eq!(next_item(&mut sub).await.gseq, Some(1));
    let last = next_item(&mut sub).await;
    assert_eq!(last.gseq, Some(50));
    assert_eq!(last.event.body, snapshot(49));
    th.hub
        .append("w1", None, Source::Hub, snapshot(7), None)
        .unwrap();
    assert_eq!(next_item(&mut sub).await.gseq, Some(53));
    for g in 1..=53 {
        assert_eq!(next_item(&mut full).await.gseq, Some(g));
    }

    // From the head: nothing stored to wait for.
    let at_head = Client::subscribe_at(
        &th.socket,
        "control.subscribe",
        json!({"since": 53, "compact": true}),
    )
    .await
    .unwrap();
    assert_eq!(at_head.ack()["backlog"], 0);

    for (method, params) in [
        ("control.subscribe", json!({"since": 0, "compact": "yes"})),
        ("control.read", json!({"since": 0, "compact": true})),
    ] {
        let e = Client::subscribe_at(&th.socket, method, params)
            .await
            .unwrap_err();
        assert_eq!(code(&e), codes::BAD_REQUEST, "{method}");
    }
    th.hub.request_shutdown();
    let _ = timeout(WAIT, th.task).await;
}
