//! End to end: hub + claude-code adapter driving a stub `claude` that replays a recorded
//! session (`tests/fixtures/claude/write_allow.both.jsonl`: one turn, a Write permission the
//! user allows). Checks the control stream, the answer relayed to the runtime's stdin, and the
//! per-thread log.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tokio::time::timeout;
use twr_adapters::claude_code::{ClaudeCodeAdapter, NAME};
use twr_core::adapter::{Adapter, Answer};
use twr_core::config::Config;
use twr_core::event::*;
use twr_hub::hub::ControlSubscription;
use twr_hub::{Hub, HubOptions, NewThread};

const WAIT: Duration = Duration::from_secs(20);

const STUB_PY: &str = r#"
import os, sys
fixture, log = sys.argv[1], sys.argv[2]
out = open(log, "w")
def keep(got):
    # twr's usage reads (`get_usage`) are not in the recording: skipped, unanswered.
    if '"get_usage"' in got:
        return False
    out.write(got); out.flush()
    return True
for line in open(fixture):
    d, body = line[:2], line[3:].rstrip("\n")
    if d == ">>":
        while True:
            got = sys.stdin.readline()
            if not got:
                sys.exit(3)
            if keep(got):
                break
    else:
        sys.stdout.write(body + "\n"); sys.stdout.flush()
for got in sys.stdin:
    keep(got)
"#;

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude")
        .join(format!("{name}.both.jsonl"))
}

fn stub(dir: &Path, fixture: &Path, log: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let py = dir.join("stub.py");
    std::fs::write(&py, STUB_PY).unwrap();
    let bin = dir.join("claude");
    let src = dir.join("claude.src");
    std::fs::write(
        &src,
        format!(
            "#!/bin/sh\nexec python3 {} {} {}\n",
            py.display(),
            fixture.display(),
            log.display()
        ),
    )
    .unwrap();
    // Written by `cp`, never open for writing in this process: a concurrently forked child
    // would inherit that descriptor and make exec fail with ETXTBSY.
    let st = std::process::Command::new("cp")
        .arg(&src)
        .arg(&bin)
        .status()
        .unwrap();
    assert!(st.success(), "cp failed");
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    bin
}

async fn next(control: &mut ControlSubscription) -> Event {
    let (_, ev) = timeout(WAIT, control.next())
        .await
        .expect("control stream timed out")
        .unwrap()
        .expect("hub stopped");
    (*ev).clone()
}

fn parse(s: &str) -> Value {
    serde_json::from_str(s).unwrap()
}

#[tokio::test]
async fn recorded_claude_session_through_the_hub() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let log = dir.path().join("stdin.jsonl");
    let bin = stub(dir.path(), &fixture_path("write_allow"), &log);

    let mut opts = HubOptions::new(dir.path().join("data").join("twr.db"), Config::default());
    let adapter: Arc<dyn Adapter> = Arc::new(ClaudeCodeAdapter::with_bin(&bin));
    opts.adapters = Arc::new(move |name: &str| (name == NAME).then(|| Arc::clone(&adapter)));
    let hub = Hub::open(opts).await.unwrap();
    let mut control = hub.subscribe_control(0);

    let t = hub
        .new_thread(
            NewThread {
                project: repo.to_string_lossy().into_owned(),
                runtime: Some(NAME.into()),
                name: Some("cc".into()),
                mode: Some("default".into()),
                ..Default::default()
            },
            "test",
        )
        .await
        .unwrap();
    let thread = t["id"].as_str().unwrap().to_string();
    let prompt = "Use the Write tool to create hi.txt containing exactly: hi   Then reply 'done'.";
    hub.send(&thread, prompt, TurnFrom::User, Source::User("test".into()))
        .unwrap();

    let mut seen: Vec<Event> = Vec::new();
    let mut attn = None;
    while attn.is_none() {
        let ev = next(&mut control).await;
        if let EventBody::AttentionRaised(a) = &ev.body {
            attn = Some(a.clone());
        }
        seen.push(ev);
    }
    let attn = attn.unwrap();
    assert_eq!(attn.reason, AttentionReason::Permission);
    // The hub records the runtime's request id in the thread's namespace (which may also
    // name the runtime's generation: `<thread>:[<gen>:]<request id>`).
    assert!(
        attn.attn_id.starts_with(&format!("{thread}:"))
            && attn
                .attn_id
                .ends_with(":f1ed5947-fd58-478b-94d1-986347fb4385"),
        "{}",
        attn.attn_id
    );
    assert_eq!(
        attn.summary,
        r#"Allow Write: /tmp/sandbox-claude/hi.txt ⟦{"content":"hi"}⟧"#
    );
    assert_eq!(
        hub.thread_view(&thread).unwrap().session_id.as_deref(),
        Some("1ae4361c-c055-4d56-8fe8-1f390147f71b")
    );

    hub.respond(&attn.attn_id, Answer::allow(), "test").unwrap();
    loop {
        let ev = next(&mut control).await;
        let done = matches!(ev.body, EventBody::ReportReady(_));
        seen.push(ev);
        if done {
            break;
        }
    }
    let kinds: Vec<&str> = seen.iter().map(|e| e.kind()).collect();
    assert_eq!(
        kinds,
        [
            "thread.created",
            "turn.started",
            "thread.started",
            "attention.raised",
            "attention.resolved",
            "turn.ended",
            "decision.made",
            "report.ready",
        ]
    );
    let started = seen.iter().find_map(|e| match &e.body {
        EventBody::ThreadStarted(p) => Some(p.clone()),
        _ => None,
    });
    let twr = &started.unwrap().ext.unwrap()["twr"];
    assert!(twr["pid"].as_u64().is_some());
    assert!(
        twr["start_time"].as_u64().is_some(),
        "hub stamps the start time"
    );
    for e in &seen {
        if matches!(
            e.body,
            EventBody::AttentionRaised(_) | EventBody::TurnEnded(_) | EventBody::ThreadStarted(_)
        ) {
            assert_eq!(e.source, Source::Adapter(NAME.into()));
            assert!(e.raw.is_some(), "{} keeps its raw line", e.kind());
        }
    }
    let ended = seen
        .iter()
        .find_map(|e| match &e.body {
            EventBody::TurnEnded(p) => Some((e.turn, p.clone())),
            _ => None,
        })
        .unwrap();
    assert_eq!(ended.0, Some(1));
    assert_eq!(ended.1.result, TurnResult::Completed);
    let usage = ended.1.usage.unwrap();
    assert!(usage.input > 0 && usage.output > 0);
    assert!(usage.cost.unwrap() > 0.0);
    let report = seen
        .iter()
        .find_map(|e| match &e.body {
            EventBody::ReportReady(r) => Some(r.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(report.grade, Grade::Success);
    assert_eq!(report.turn, Some(1));

    // The per-thread log has the agent events, all stamped with turn 1.
    let (_, evs) = hub.thread_events(&thread, 0, 1000).unwrap();
    let agent: Vec<&Event> = evs.iter().filter(|e| e.body.is_agent()).collect();
    assert!(agent.iter().all(|e| e.turn == Some(1)));
    let agent_kinds: Vec<&str> = agent
        .iter()
        .map(|e| e.kind())
        .filter(|k| *k != "agent.text")
        .collect();
    assert_eq!(agent_kinds, ["agent.tool_call", "agent.tool_result"]);
    assert!(agent
        .iter()
        .any(|e| matches!(&e.body, EventBody::AgentText(t) if t.is_final && t.text == "done")));

    // The runtime got exactly what the recording shows for the prompt and the allow.
    let recorded: Vec<Value> = std::fs::read_to_string(fixture_path("write_allow"))
        .unwrap()
        .lines()
        .filter(|l| l.starts_with(">>"))
        .map(|l| parse(&l[3..]))
        .collect();
    let written: Vec<Value> = std::fs::read_to_string(&log)
        .unwrap()
        .lines()
        .map(parse)
        .collect();
    assert_eq!(written, recorded);

    hub.stop_thread(&thread, "test").unwrap();
    loop {
        let ev = next(&mut control).await;
        if matches!(ev.body, EventBody::ThreadStopped(_)) {
            break;
        }
        assert!(!matches!(ev.body, EventBody::ThreadCrashed(_)), "{ev:?}");
    }
    hub.shutdown().await;
}
