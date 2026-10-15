//! Grok Build adapter tests. Tier headless; the ACP flow is verified against a signed-in
//! grok 1.0.44 (2026-09-30).
//!
//! Fixtures (`tests/fixtures/grok/<name>.both.jsonl`), both directions interleaved: `>> `
//! lines are written to grok's stdin, `<< ` lines come from its stdout.
//! - `real_*_v1044`: REAL recordings of grok 1.0.44 (`grok --no-auto-update
//!   --permission-mode default [--allow ...] [--deny ...] agent --no-leader stdio`, the user's
//!   `config.toml` saying `permission_mode = "always-approve"`), driven by a recorder that
//!   writes exactly what this adapter writes. Redacted: paths (`/work/repo`, `/home/user`,
//!   `/work/mcp_ping.py`), host name, agent ids, model signatures, the account's subscription
//!   tier and the user's own MCP servers / hooks / skills (`availableCommands`,
//!   `availableModels` cut to two entries); the per-chunk timing `_meta` of message and thought
//!   chunks was dropped (size). The orchestrator recording used a trivial stdio MCP server
//!   with one tool, `ping`.
//! - `real_noauth_v1041`: a REAL recording of grok 1.0.41 without credentials (`initialize`
//!   works, `session/new` fails with `Authentication required`).
//! - `synthetic_*`: sessions written by hand from the ACP spec before grok could be signed in.
//!   Kept for what a recording cannot provoke on demand (replaced requests, reported modes,
//!   failed `session/set_mode`); where they differ from the real grok, the real one wins.
//!
//! Translator tests replay each fixture: stdout lines go through [`Translator::translate`];
//! at each stdin line the test either takes the next line the translator already produced
//! or performs the action the line records (prompt, respond, stop), then checks the line
//! matches. Driver tests run the adapter against a python3 stub replaying a fixture.
//! `real_grok_*` tests run the real binary and are `#[ignore]`d.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::timeout;
use twr_adapters::grok_build::acp::{mode_acceptable, narrowing_target};
use twr_adapters::grok_build::{
    self, acp_mcp_servers, agent_profile, build_args, build_env, profile_tools, Emitted,
    GrokBuildAdapter, Translator, TranslatorOptions,
};
use twr_core::adapter::{Adapter, AdapterEvent, AdapterHandle, Answer, SpawnConfig, Tier};
use twr_core::event::*;

const WAIT: Duration = Duration::from_secs(20);
const CWD: &str = "/work/repo";

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/grok")
        .join(format!("{name}.both.jsonl"))
}

fn fixture(name: &str) -> Vec<(String, String)> {
    std::fs::read_to_string(fixture_path(name))
        .unwrap()
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| (l[..2].to_string(), l[3..].to_string()))
        .collect()
}

fn parse(s: &str) -> Value {
    serde_json::from_str(s).unwrap()
}

/// A stdin line with the parts that vary between runs normalized (client version, cwd).
fn norm(mut v: Value) -> Value {
    if let Some(p) = v.get_mut("params").and_then(Value::as_object_mut) {
        if p.contains_key("cwd") {
            p.insert("cwd".into(), json!(CWD));
        }
        if let Some(ci) = p.get_mut("clientInfo").and_then(Value::as_object_mut) {
            ci.insert("version".into(), json!("*"));
        }
    }
    v
}

fn opts() -> TranslatorOptions {
    TranslatorOptions {
        cwd: CWD.into(),
        pid: Some(4242),
        exe: Some("/opt/grok".into()),
        ..Default::default()
    }
}

struct Replay {
    events: Vec<Emitted>,
    fatal: Vec<String>,
    tr: Translator,
}

fn replay(name: &str, opts: TranslatorOptions) -> Replay {
    replay_lines(fixture(name), opts)
}

fn replay_lines(lines: Vec<(String, String)>, opts: TranslatorOptions) -> Replay {
    let mut tr = Translator::new(opts);
    let mut writes: VecDeque<String> = tr.start().writes.into();
    let mut events = Vec::new();
    let mut fatal = Vec::new();
    let mut turn = 1;
    for (dir, line) in lines {
        if dir == "<<" {
            let out = tr.translate(&line);
            events.extend(out.events);
            writes.extend(out.writes);
            fatal.extend(out.fatal);
            continue;
        }
        let rec = parse(&line);
        if writes.is_empty() {
            let out = if rec["method"] == "session/prompt" {
                let text = rec["params"]["prompt"][0]["text"].as_str().unwrap();
                let out = tr.send(text, turn);
                turn += 1;
                out
            } else if rec["method"] == "session/cancel" {
                tr.stop()
            } else if let Some(o) = rec.pointer("/result/outcome").filter(|o| o.is_string()) {
                // A question (`accepted` with answers, or `cancelled`) or a plan approval.
                let id = match &rec["id"] {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let (attn, a) = match o.as_str().unwrap() {
                    "approved" => (format!("plan-{id}"), Answer::allow()),
                    "rejected" => (format!("plan-{id}"), Answer::deny()),
                    "accepted" => {
                        let answers = rec["result"]["answers"].as_object().unwrap();
                        assert_eq!(answers.len(), 1, "one question per request here");
                        let label = answers.values().next().unwrap().as_str().unwrap();
                        (format!("ask-{id}"), Answer::new(label))
                    }
                    _ => (format!("ask-{id}"), Answer::deny()),
                };
                assert!(tr.pending_attention().contains(&attn), "{attn} not pending");
                tr.respond(&attn, &a).expect("a pending item")
            } else if let Some(o) = rec.pointer("/result/outcome") {
                let id = match &rec["id"] {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let attn = format!("perm-{id}");
                assert!(tr.pending_attention().contains(&attn), "{attn} not pending");
                let a = match o["optionId"].as_str() {
                    Some(opt) if opt.contains("allow") => Answer::allow(),
                    _ => Answer::deny(),
                };
                tr.respond(&attn, &a).expect("a pending permission")
            } else {
                panic!("no action produces {line}");
            };
            events.extend(out.events);
            writes.extend(out.writes);
        }
        let ours = writes
            .pop_front()
            .unwrap_or_else(|| panic!("the adapter wrote nothing for {line}"));
        assert_eq!(norm(parse(&ours)), norm(rec), "stdin line differs");
    }
    assert!(writes.is_empty(), "unrecorded writes: {writes:?}");
    Replay { events, fatal, tr }
}

/// `kind@turn`, with runs of text deltas collapsed into one `delta@turn`.
fn shape(events: &[Emitted]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for e in events {
        // These recordings assert the existing transcript shape. Session task lifecycle
        // (shared with standard ACP) is exercised separately in background.rs.
        if matches!(e.body, EventBody::TaskUpdated(_)) {
            continue;
        }
        let kind = match &e.body {
            EventBody::AgentText(t) if t.delta => "delta".to_string(),
            EventBody::AgentText(_) => "text".to_string(),
            b => b.kind().to_string(),
        };
        let s = match e.turn {
            Some(t) => format!("{kind}@{t}"),
            None => kind,
        };
        if s.starts_with("delta") && out.last() == Some(&s) {
            continue;
        }
        out.push(s);
    }
    out
}

fn find<T>(events: &[Emitted], f: impl Fn(&EventBody) -> Option<&T>) -> Vec<&T> {
    events.iter().filter_map(|e| f(&e.body)).collect()
}

fn grok_ext(m: &Option<JsonMap>) -> &Value {
    m.as_ref()
        .and_then(|m| m.get("grok"))
        .unwrap_or(&Value::Null)
}

fn started(events: &[Emitted]) -> Vec<&SessionPayload> {
    find(events, |b| match b {
        EventBody::ThreadStarted(p) => Some(p),
        _ => None,
    })
}

fn ended(events: &[Emitted]) -> Vec<&TurnEnded> {
    find(events, |b| match b {
        EventBody::TurnEnded(p) => Some(p),
        _ => None,
    })
}

fn raised(events: &[Emitted]) -> Vec<&AttentionRaised> {
    find(events, |b| match b {
        EventBody::AttentionRaised(p) => Some(p),
        _ => None,
    })
}

fn finals(events: &[Emitted]) -> Vec<&str> {
    find(events, |b| match b {
        EventBody::AgentText(t) if t.is_final => Some(t),
        _ => None,
    })
    .into_iter()
    .map(|t| t.text.as_str())
    .collect()
}

// ---------------------------------------------------------------------------------------------
// Translator: synthetic fixtures
// ---------------------------------------------------------------------------------------------

#[test]
fn synthetic_new_session_permission_and_multi_turn() {
    let r = replay(
        "synthetic_new_permission_multiturn",
        TranslatorOptions {
            thinking: true,
            mcp_servers: acp_mcp_servers(&json!({"mcpServers": {"twr": {
                "command": "/usr/bin/twr",
                "args": ["mcp", "--socket", "/run/twr.sock"],
                "env": {"TWR_ORCH_TOKEN": "tok"},
            }}})),
            ..opts()
        },
    );
    assert_eq!(
        shape(&r.events),
        [
            "thread.started",
            "agent.thinking@1",
            "delta@1",
            "text@1",
            "agent.tool_call@1",
            "attention.raised@1",
            "agent.tool_result@1",
            "delta@1",
            "text@1",
            "turn.ended@1",
            "delta@2",
            "text@2",
            "turn.ended@2",
        ]
    );
    let s = started(&r.events)[0];
    assert_eq!(
        s.session_id.as_deref(),
        Some("0199a000-0000-7000-8000-000000000001")
    );
    let ext = s.ext.as_ref().unwrap();
    assert_eq!(ext["twr"]["pid"], 4242);
    assert_eq!(ext["twr"]["exe"], "/opt/grok");
    assert_eq!(ext["grok"]["version"], "1.0.41");
    assert_eq!(ext["grok"]["mode"], "default");
    assert_eq!(
        ext["grok"]["mode_set"],
        json!({"mode": "default", "ok": true})
    );
    assert_eq!(ext["grok"]["verified_with"], "1.0.44");
    assert!(ext["grok"].get("warning").is_none());
    assert_eq!(r.tr.current_mode(), Some("default"));

    assert_eq!(
        finals(&r.events),
        ["I'll create the file.", "Done.", "Bye."]
    );
    let calls = find(&r.events, |b| match b {
        EventBody::AgentToolCall(c) => Some(c),
        _ => None,
    });
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].call_id, "call_1");
    assert_eq!(calls[0].tool, "edit");
    assert_eq!(calls[0].input_summary, "Write hello.txt");
    assert!(calls[0].risky);
    assert_eq!(
        grok_ext(&calls[0].ext)["paths"],
        json!(["/work/repo/hello.txt"])
    );
    let results = find(&r.events, |b| match b {
        EventBody::AgentToolResult(c) => Some(c),
        _ => None,
    });
    assert_eq!(results.len(), 1);
    assert!(results[0].ok);
    assert_eq!(results[0].output_summary, "[diff /work/repo/hello.txt]");

    let a = raised(&r.events)[0];
    assert_eq!(a.attn_id, "perm-0");
    assert_eq!(a.reason, AttentionReason::Permission);
    assert_eq!(a.options, ["allow", "deny"]);
    assert_eq!(a.summary, "Allow edit: Write hello.txt");
    assert_eq!(a.call_id.as_deref(), Some("call_1"));
    assert_eq!(grok_ext(&a.ext)["rpc_id"], 0);

    let e = ended(&r.events);
    assert_eq!(e[0].result, TurnResult::Completed);
    let u = e[0].usage.as_ref().unwrap();
    assert_eq!((u.input, u.output, u.cost), (1200, 40, None));
    assert_eq!(e[1].result, TurnResult::Completed);
    assert!(e[1].usage.is_none());
    assert!(r.fatal.is_empty());
    assert!(r.tr.pending_attention().is_empty());
}

#[test]
fn thinking_is_dropped_unless_enabled() {
    let r = replay(
        "synthetic_new_permission_multiturn",
        TranslatorOptions {
            mcp_servers: acp_mcp_servers(&json!({"mcpServers": {"twr": {
                "command": "/usr/bin/twr",
                "args": ["mcp", "--socket", "/run/twr.sock"],
                "env": {"TWR_ORCH_TOKEN": "tok"},
            }}})),
            ..opts()
        },
    );
    assert!(!shape(&r.events)
        .iter()
        .any(|s| s.starts_with("agent.thinking")));
}

#[test]
fn synthetic_deny_selects_reject_once_and_refuses_fs_requests() {
    let r = replay("synthetic_deny", opts());
    assert_eq!(
        shape(&r.events),
        [
            "thread.started",
            "agent.tool_call@1",
            "attention.raised@1",
            "agent.tool_result@1",
            "delta@1",
            "text@1",
            "turn.ended@1",
        ]
    );
    let call = find(&r.events, |b| match b {
        EventBody::AgentToolCall(c) => Some(c),
        _ => None,
    })[0];
    assert_eq!(call.tool, "execute");
    assert!(call.risky);
    assert_eq!(grok_ext(&call.ext)["destructive"], true);
    let res = find(&r.events, |b| match b {
        EventBody::AgentToolResult(c) => Some(c),
        _ => None,
    })[0];
    assert!(!res.ok);
    assert_eq!(res.output_summary, "Permission denied by user");
    // No modes reported (like the real grok 1.0.44): nothing to switch, no warning.
    let g = &started(&r.events)[0].ext.as_ref().unwrap()["grok"];
    assert!(g.get("mode").is_none() && g.get("warning").is_none(), "{g}");
}

#[test]
fn synthetic_stop_cancels_the_prompt_and_pending_requests_without_answering_the_item() {
    let r = replay("synthetic_cancel", opts());
    assert_eq!(
        shape(&r.events),
        [
            "thread.started",
            "delta@1",
            "text@1",
            "agent.tool_call@1",
            "attention.raised@1",
            "agent.tool_result@1",
            "turn.ended@1",
        ]
    );
    assert_eq!(raised(&r.events)[0].attn_id, "perm-perm-a");
    // twr never resolves the item itself: the hub resolves it as thread_stopped.
    assert!(!r
        .events
        .iter()
        .any(|e| matches!(e.body, EventBody::AttentionResolved(_))));
    assert_eq!(ended(&r.events)[0].result, TurnResult::Interrupted);
    assert_eq!(
        grok_ext(&ended(&r.events)[0].ext)["stop_reason"],
        "cancelled"
    );
}

#[test]
fn synthetic_resume_uses_session_resume() {
    let r = replay(
        "synthetic_resume",
        TranslatorOptions {
            resume_session: Some("s-old".into()),
            ..opts()
        },
    );
    assert_eq!(
        shape(&r.events),
        ["thread.started", "delta@1", "text@1", "turn.ended@1"]
    );
    let s = started(&r.events)[0];
    assert_eq!(s.session_id.as_deref(), Some("s-old"));
    let g = &s.ext.as_ref().unwrap()["grok"];
    assert_eq!(g["resumed"], true);
    assert_eq!(g["open_method"], "session/resume");
    assert_eq!(ended(&r.events)[0].session_id.as_deref(), Some("s-old"));
}

/// The loaded session reports always-approve and `session/set_mode` fails, so the
/// session fails: no `thread.started`, no `session/prompt` (the recording ends with the
/// failed set_mode), and the replayed history is dropped.
#[test]
fn synthetic_load_drops_the_replayed_history_and_fails_when_set_mode_fails() {
    let mut tr = Translator::new(TranslatorOptions {
        resume_session: Some("s-old".into()),
        ..opts()
    });
    // The prompt was queued at spawn, before the session opened.
    let queued = tr.send("Write a long essay", 1);
    assert!(queued.writes.is_empty());
    let mut writes: VecDeque<String> = tr.start().writes.into();
    let mut events = Vec::new();
    let mut fatal = Vec::new();
    for (dir, line) in fixture("synthetic_load_replay") {
        if dir == "<<" {
            let out = tr.translate(&line);
            events.extend(out.events);
            writes.extend(out.writes);
            fatal.extend(out.fatal);
        } else {
            let ours = writes.pop_front().expect("a line the adapter wrote");
            assert_eq!(norm(parse(&ours)), norm(parse(&line)), "stdin line differs");
        }
    }
    assert!(
        writes.is_empty(),
        "nothing after the failed set_mode: {writes:?}"
    );
    assert_eq!(shape(&events), ["text@1", "turn.ended@1"]);
    assert!(started(&events).is_empty());
    assert!(!finals(&events).iter().any(|t| t.contains("old answer")));
    assert_eq!(ended(&events)[0].result, TurnResult::Error);
    assert_eq!(fatal.len(), 1);
    assert!(fatal[0].contains("session/set_mode"), "{}", fatal[0]);
    assert!(fatal[0].contains("always-approve"), "{}", fatal[0]);
    assert!(!tr.ready());
    assert_eq!(tr.current_mode(), Some("always-approve"));
}

// ---------------------------------------------------------------------------------------------
// Translator: real recording (no credentials)
// ---------------------------------------------------------------------------------------------

#[test]
fn real_auth_failure_ends_the_waiting_turn_with_an_error() {
    let mut lines = fixture("real_noauth_v1041");
    // The recording was made with the first prompt given at spawn: send it before replaying.
    let mut tr = Translator::new(TranslatorOptions {
        cwd: "/work/sandbox".into(),
        ..Default::default()
    });
    let mut writes: VecDeque<String> = tr.start().writes.into();
    let first = tr.send("hello", 1);
    assert!(first.writes.is_empty(), "the prompt waits for the session");
    let mut events = Vec::new();
    let mut fatal = Vec::new();
    for (dir, line) in lines.drain(..) {
        if dir == "<<" {
            let out = tr.translate(&line);
            events.extend(out.events);
            writes.extend(out.writes);
            fatal.extend(out.fatal);
        } else {
            let ours = writes.pop_front().expect("a line the adapter wrote");
            let mut rec = norm(parse(&line));
            let mut ours = norm(parse(&ours));
            for v in [&mut rec, &mut ours] {
                if let Some(p) = v["params"].as_object_mut() {
                    p.insert("cwd".into(), json!(CWD));
                }
            }
            assert_eq!(ours, rec);
        }
    }
    assert_eq!(shape(&events), ["text@1", "turn.ended@1"]);
    let text = finals(&events)[0];
    assert!(text.contains("Authentication required"), "{text}");
    assert!(text.contains("grok login"), "{text}");
    let e = ended(&events)[0];
    assert_eq!(e.result, TurnResult::Error);
    assert_eq!(grok_ext(&e.ext)["transient"], false);
    assert_eq!(fatal.len(), 1);
    assert!(fatal[0].contains("session/new failed: Authentication required"));
    assert!(!tr.ready());
    // A later prompt fails at once.
    let later = tr.send("again", 2);
    assert!(later.writes.is_empty());
    assert_eq!(shape(&later.events), ["text@2", "turn.ended@2"]);
}

// ---------------------------------------------------------------------------------------------
// Translator: real recordings (grok 1.0.44, signed in)
// ---------------------------------------------------------------------------------------------

const FAST: &str = "grok-4.7-build-fast";

/// Options of the worker recordings: model set over ACP, thinking shown.
fn worker() -> TranslatorOptions {
    TranslatorOptions {
        thinking: true,
        model: Some(FAST.into()),
        ..opts()
    }
}

fn calls(events: &[Emitted]) -> Vec<&AgentToolCall> {
    find(events, |b| match b {
        EventBody::AgentToolCall(c) => Some(c),
        _ => None,
    })
}

fn results(events: &[Emitted]) -> Vec<&AgentToolResult> {
    find(events, |b| match b {
        EventBody::AgentToolResult(c) => Some(c),
        _ => None,
    })
}

/// Shape without text and thinking events (their split depends on how grok chunked them).
fn skeleton(events: &[Emitted]) -> Vec<String> {
    shape(events)
        .into_iter()
        .filter(|s| {
            !s.starts_with("delta") && !s.starts_with("text") && !s.starts_with("agent.thinking")
        })
        .collect()
}

#[test]
fn real_permission_allow_then_a_second_turn() {
    let r = replay("real_permission_allow_v1044", worker());
    assert_eq!(
        skeleton(&r.events),
        [
            "thread.started",
            "agent.tool_call@1",
            "attention.raised@1",
            "agent.tool_result@1",
            "turn.ended@1",
            "turn.ended@2",
        ]
    );
    let s = started(&r.events)[0];
    let g = &s.ext.as_ref().unwrap()["grok"];
    assert_eq!(g["version"], "1.0.44");
    assert_eq!(g["open_method"], "session/new");
    assert_eq!(g["model"], FAST);
    assert_eq!(g["resumed"], false);
    // grok 1.0.44 reports no modes.
    assert!(g.get("mode").is_none() && g.get("warning").is_none(), "{g}");
    assert!(r.tr.current_mode().is_none());
    assert_eq!(r.tr.yolo(), Some(false));

    // The first `tool_call` has no ACP kind: grok's own (`write`) stands in.
    let c = calls(&r.events)[0];
    assert_eq!(c.tool, "edit");
    assert!(c.risky);
    assert_eq!(grok_ext(&c.ext)["tool"], "write");
    assert!(
        c.input_summary.starts_with("write {"),
        "{}",
        c.input_summary
    );
    assert!(
        c.input_summary.contains("/work/repo/hello.txt"),
        "{}",
        c.input_summary
    );

    let a = raised(&r.events)[0];
    assert_eq!(a.attn_id, "perm-0");
    assert_eq!(a.reason, AttentionReason::Permission);
    assert_eq!(a.options, ["allow", "deny"]);
    assert_eq!(a.summary, "Allow edit: Write `/work/repo/hello.txt`");
    assert_eq!(a.call_id.as_deref(), Some(c.call_id.as_str()));
    // grok offers allow_always ("allow all edits during this session"): never chosen.
    let kinds: Vec<&str> = grok_ext(&a.ext)["options"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["allow_always", "allow_once", "reject_once"]);

    let res = results(&r.events)[0];
    assert!(res.ok);
    assert_eq!(res.output_summary, "[diff /work/repo/hello.txt]");
    let e = ended(&r.events);
    assert_eq!(e[0].result, TurnResult::Completed);
    assert_eq!(e[1].result, TurnResult::Completed);
    assert_eq!(grok_ext(&e[0].ext)["model"], FAST);
    assert!(e[0].usage.as_ref().unwrap().input > 0);
    let f = finals(&r.events);
    assert!(f.last().unwrap().contains("hi"), "{f:?}");
    assert!(shape(&r.events).iter().any(|s| s == "agent.thinking@1"));
    assert!(r.fatal.is_empty());
    assert!(r.tr.pending_attention().is_empty());
}

#[test]
fn real_reject_once_ends_the_turn_as_cancelled() {
    let r = replay("real_permission_reject_v1044", worker());
    assert_eq!(
        skeleton(&r.events),
        [
            "thread.started",
            "agent.tool_call@1",
            "attention.raised@1",
            "agent.tool_result@1",
            "turn.ended@1",
        ]
    );
    // The recorded reply selects `reject-once` (checked by the replay).
    let res = results(&r.events)[0];
    assert!(!res.ok);
    let e = ended(&r.events)[0];
    assert_eq!(e.result, TurnResult::Interrupted);
    assert_eq!(grok_ext(&e.ext)["stop_reason"], "cancelled");
}

/// grok ran `sleep 30 && echo done` without asking (it counts as harmless); `session/cancel`
/// ended the turn and killed the command.
#[test]
fn real_stop_while_a_command_runs() {
    let r = replay("real_cancel_running_v1044", worker());
    assert_eq!(
        skeleton(&r.events),
        ["thread.started", "agent.tool_call@1", "turn.ended@1"]
    );
    let c = calls(&r.events)[0];
    assert_eq!(c.tool, "execute");
    assert!(c.risky);
    assert!(c.input_summary.contains("sleep 30"), "{}", c.input_summary);
    assert_eq!(ended(&r.events)[0].result, TurnResult::Interrupted);
}

#[test]
fn real_stop_with_a_pending_permission() {
    let r = replay("real_stop_pending_v1044", worker());
    assert_eq!(
        skeleton(&r.events),
        [
            "thread.started",
            "agent.tool_call@1",
            "attention.raised@1",
            "turn.ended@1",
        ]
    );
    // twr never resolves the item itself: the hub resolves it as thread_stopped.
    assert!(!r
        .events
        .iter()
        .any(|e| matches!(e.body, EventBody::AttentionResolved(_))));
    assert_eq!(ended(&r.events)[0].result, TurnResult::Interrupted);
}

#[test]
fn real_resume_remembers_the_conversation() {
    let sid = "01a0f3ba-e5a7-76a0-9490-eb5e51d7e215";
    let r = replay(
        "real_resume_v1044",
        TranslatorOptions {
            resume_session: Some(sid.into()),
            ..worker()
        },
    );
    assert_eq!(skeleton(&r.events), ["thread.started", "turn.ended@1"]);
    let s = started(&r.events)[0];
    assert_eq!(s.session_id.as_deref(), Some(sid));
    let g = &s.ext.as_ref().unwrap()["grok"];
    assert_eq!(g["open_method"], "session/resume");
    assert_eq!(g["resumed"], true);
    assert_eq!(ended(&r.events)[0].session_id.as_deref(), Some(sid));
    let f = finals(&r.events);
    assert!(f.last().unwrap().contains("hello.txt"), "{f:?}");
}

#[test]
fn real_orchestrator_has_only_mcp_tools() {
    let cfg = SpawnConfig {
        system_append: Some("You are a test orchestrator. Keep answers to one line.".into()),
        allowed_tools: vec!["mcp__pingsrv".into()],
        builtin_tools: Some(vec![]),
        ..Default::default()
    };
    let r = replay(
        "real_orchestrator_v1044",
        TranslatorOptions {
            mcp_servers: acp_mcp_servers(&json!({"mcpServers": {"pingsrv": {
                "command": "python3",
                "args": ["/work/mcp_ping.py"],
            }}})),
            rules: cfg.system_append.clone(),
            agent_profile: agent_profile(&cfg),
            denied_tools: grok_build::denied_builtins(&cfg),
            ..opts()
        },
    );
    // search_tool (runs without asking), then use_tool (asks: grok ignores MCP allow rules);
    // the model then searched for a shell tool and found none.
    let c = calls(&r.events);
    let tools: Vec<&str> = c
        .iter()
        .map(|c| grok_ext(&c.ext)["tool"].as_str().unwrap())
        .collect();
    assert_eq!(&tools[..2], ["search_tool", "use_tool"]);
    assert!(tools[2..].iter().all(|t| *t == "search_tool"), "{tools:?}");
    assert!(c.iter().all(|c| c.tool == "other"));
    // The use_tool call is summarized as the MCP tool it runs, and its result as that tool's
    // text.
    assert_eq!(grok_ext(&c[1].ext)["mcp_tool"], "pingsrv__ping");
    assert_eq!(c[1].input_summary, "pingsrv__ping");
    assert!(grok_ext(&c[0].ext).get("mcp_tool").is_none());
    let res = results(&r.events);
    let ping = res.iter().find(|r| r.call_id == c[1].call_id).unwrap();
    assert_eq!(ping.output_summary, "pong 1");
    let a = raised(&r.events);
    assert_eq!(a.len(), 1);
    assert!(a[0].summary.contains("pingsrv__ping"), "{}", a[0].summary);
    assert!(results(&r.events).iter().all(|r| r.ok));
    let text = finals(&r.events).join(" ");
    assert!(
        text.contains("pong") && text.contains("no shell tool"),
        "{text}"
    );
    let s = started(&r.events)[0];
    let g = &s.ext.as_ref().unwrap()["grok"];
    assert_eq!(g["tools"], json!(["search_tool", "use_tool"]));
    // No model was requested: the one the session reports.
    assert_eq!(g["model"], "grok-4.7");
    assert_eq!(ended(&r.events)[0].result, TurnResult::Completed);
}

#[test]
fn real_question_and_plan_approval() {
    let r = replay("real_question_plan_v1044", worker());
    let a = raised(&r.events);
    assert_eq!(a.len(), 2, "{:?}", shape(&r.events));
    assert_eq!(a[0].attn_id, "ask-0");
    assert_eq!(a[0].reason, AttentionReason::Question);
    assert_eq!(a[0].summary, "Do you prefer red or blue?");
    assert_eq!(a[0].options, ["red", "blue"]);
    assert_eq!(a[1].attn_id, "plan-1");
    assert_eq!(a[1].reason, AttentionReason::Permission);
    assert_eq!(a[1].options, ["allow", "deny"]);
    assert!(a[1].summary.contains("noop"), "{}", a[1].summary);
    assert_eq!(grok_ext(&a[1].ext)["plan"], "noop\n");
    // Plan mode entered and left (approved).
    assert_eq!(r.tr.current_mode(), Some("default"));
    let e = ended(&r.events);
    assert_eq!(e.len(), 2);
    assert!(e.iter().all(|e| e.result == TurnResult::Completed));
    let first_turn: String = r
        .events
        .iter()
        .filter(|e| e.turn == Some(1))
        .filter_map(|e| match &e.body {
            EventBody::AgentText(t) if t.is_final => Some(t.text.as_str()),
            _ => None,
        })
        .collect();
    assert!(first_turn.to_lowercase().contains("blue"), "{first_turn}");
    assert!(r.fatal.is_empty());
}

// ---------------------------------------------------------------------------------------------
// Translator: unit behavior
// ---------------------------------------------------------------------------------------------

/// A translator with an open session `s` (no modes).
fn ready() -> Translator {
    let mut tr = Translator::new(opts());
    tr.start();
    tr.translate(
        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#,
    );
    let out = tr.translate(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s"}}"#);
    assert_eq!(shape(&out.events), ["thread.started"]);
    assert!(tr.ready());
    tr
}

fn permission(id: Value, options: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": "session/request_permission",
        "params": {"sessionId": "s", "toolCall": {"toolCallId": "c1", "title": "ls", "kind": "execute"},
        "options": options}})
    .to_string()
}

/// A call of the twr MCP server's `send` through grok's `use_tool` (the shapes of
/// `real_orchestrator_v1044`): the summary leads with the MCP tool and gives its own input in
/// full, though the wrapper's input would put the name after a long prompt; the result is the
/// tool's text.
#[test]
fn an_mcp_call_through_use_tool_names_the_mcp_tool() {
    let mut tr = ready();
    tr.send("go", 1);
    let meta = json!({"x.ai/tool": {"version": 1, "name": "use_tool", "kind": "use_tool",
        "namespace": "grok_build", "label": "Use Tool", "read_only": false}});
    let input = json!({"thread": "web", "prompt": "fix the login flow and run the tests"});
    let out = tr.translate(
        &json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s",
            "update": {"sessionUpdate": "tool_call", "toolCallId": "c9", "title": "use_tool",
                "rawInput": {"tool_name": "twr__send", "tool_input": input}, "_meta": meta}}})
        .to_string(),
    );
    let c = calls(&out.events);
    assert_eq!(c[0].tool, "other");
    assert_eq!(grok_ext(&c[0].ext)["mcp_tool"], "twr__send");
    assert_eq!(grok_ext(&c[0].ext)["tool"], "use_tool");
    assert_eq!(
        c[0].input_summary,
        r#"twr__send {"prompt":"fix the login flow and run the tests","thread":"web"}"#
    );
    let text = r#"{"pending":0,"queued":false,"thread":"01J0","turn":2}"#;
    let out = tr.translate(
        &json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "s",
            "update": {"sessionUpdate": "tool_call_update", "toolCallId": "c9",
                "status": "completed", "rawOutput": {"type": "MCP", "tool_name": "send",
                    "server_name": "twr", "output": {"OkayOutput": text}}}}})
        .to_string(),
    );
    let res = results(&out.events);
    assert!(res[0].ok);
    assert_eq!(res[0].output_summary, text);
}

#[test]
fn prompts_queue_until_ready_and_run_one_at_a_time() {
    let mut tr = Translator::new(opts());
    tr.start();
    assert!(tr.send("one", 1).writes.is_empty());
    assert!(tr.send("two", 2).writes.is_empty());
    tr.translate(r#"{"jsonrpc":"2.0","id":1,"result":{"agentCapabilities":{}}}"#);
    let out = tr.translate(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s"}}"#);
    assert_eq!(out.writes.len(), 1);
    let p = parse(&out.writes[0]);
    assert_eq!(p["method"], "session/prompt");
    assert_eq!(p["params"]["prompt"][0]["text"], "one");
    let out = tr.translate(r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#);
    assert_eq!(shape(&out.events), ["turn.ended@1"]);
    assert_eq!(parse(&out.writes[0])["params"]["prompt"][0]["text"], "two");
    assert_eq!(parse(&out.writes[0])["id"], 4);
}

#[test]
fn stop_reasons_map_to_results() {
    for (reason, want) in [
        ("end_turn", TurnResult::Completed),
        ("cancelled", TurnResult::Interrupted),
        ("refusal", TurnResult::Error),
        ("max_tokens", TurnResult::Error),
        ("max_turn_requests", TurnResult::Error),
        ("something_new", TurnResult::Error),
    ] {
        let mut tr = ready();
        tr.send("x", 7);
        let out = tr.translate(
            &json!({"jsonrpc": "2.0", "id": 3, "result": {"stopReason": reason}}).to_string(),
        );
        assert_eq!(ended(&out.events)[0].result, want, "{reason}");
        assert_eq!(out.events[0].turn, Some(7));
    }
}

#[test]
fn prompt_errors_end_the_turn_with_an_error() {
    let mut tr = ready();
    tr.send("x", 1);
    let out = tr.translate(
        r#"{"jsonrpc":"2.0","id":3,"error":{"code":-32603,"message":"Rate limit exceeded"}}"#,
    );
    let e = ended(&out.events)[0];
    assert_eq!(e.result, TurnResult::Error);
    assert_eq!(grok_ext(&e.ext)["error"]["message"], "Rate limit exceeded");
    assert_eq!(grok_ext(&e.ext)["transient"], true);
}

#[test]
fn responses_to_ids_twr_did_not_issue_are_ignored() {
    let mut tr = ready();
    tr.send("x", 1);
    for l in [
        r#"{"jsonrpc":"2.0","id":"skills-reload","result":{"result":{"reloaded":0}}}"#,
        r#"{"jsonrpc":"2.0","id":99,"result":{"stopReason":"end_turn"}}"#,
        r#"{"jsonrpc":"2.0","id":"3","result":{"stopReason":"end_turn"}}"#,
        "not json",
        "[1,2]",
    ] {
        let out = tr.translate(l);
        assert!(out.events.is_empty() && out.writes.is_empty(), "{l}");
    }
}

#[test]
fn allow_never_picks_allow_always() {
    let mut tr = ready();
    tr.send("x", 1);
    // Only allow_always is offered: the item offers deny only, and an allow answer rejects.
    let out = tr.translate(&permission(
        json!(5),
        json!([
            {"optionId": "always", "name": "Always", "kind": "allow_always"},
            {"optionId": "no", "name": "No", "kind": "reject_once"},
        ]),
    ));
    let a = raised(&out.events)[0];
    assert_eq!(a.options, ["deny"]);
    assert_eq!(grok_ext(&a.ext)["no_allow_once"], true);
    let reply = tr.respond("perm-5", &Answer::allow()).unwrap();
    assert_eq!(
        parse(&reply.writes[0])["result"]["outcome"],
        json!({"outcome": "selected", "optionId": "no"})
    );
    assert!(
        tr.respond("perm-5", &Answer::allow()).is_none(),
        "answered once"
    );
}

#[test]
fn deny_never_picks_reject_always_and_falls_back_to_cancelled() {
    let mut tr = ready();
    tr.send("x", 1);
    tr.translate(&permission(
        json!(5),
        json!([
            {"optionId": "y", "kind": "allow_once"},
            {"optionId": "never", "kind": "reject_always"},
        ]),
    ));
    // reject_always would persist a "never allow" rule: a plain deny cancels instead.
    let reply = tr.respond("perm-5", &Answer::deny()).unwrap();
    assert_eq!(
        parse(&reply.writes[0])["result"]["outcome"],
        json!({"outcome": "cancelled"})
    );
    tr.translate(&permission(
        json!(7),
        json!([
            {"optionId": "never", "kind": "reject_always"},
            {"optionId": "no", "kind": "reject_once"},
            {"optionId": "y", "kind": "allow_once"},
        ]),
    ));
    let reply = tr.respond("perm-7", &Answer::deny()).unwrap();
    assert_eq!(
        parse(&reply.writes[0])["result"]["outcome"],
        json!({"outcome": "selected", "optionId": "no"})
    );
    tr.translate(&permission(
        json!(6),
        json!([{"optionId": "y", "kind": "allow_once"}]),
    ));
    let reply = tr
        .respond("perm-6", &Answer::new("some option text"))
        .unwrap();
    assert_eq!(
        parse(&reply.writes[0])["result"]["outcome"],
        json!({"outcome": "cancelled"})
    );
    let reply = tr.respond("perm-6", &Answer::allow());
    assert!(reply.is_none());
}

/// A permission request reusing a pending request's JSON-RPC id
/// replaces it. The answer names only the id, so the old item's allow (sent before the hub saw
/// the replacement) would pick the replacement's option: the old item is withdrawn as
/// superseded, its answer is ignored, and the replacement answers only by its own id.
#[test]
fn a_replaced_request_is_never_answered_by_an_answer_to_the_old_one() {
    let mut tr = ready();
    tr.send("x", 1);
    let request = |title: &str, allow: &str| {
        json!({"jsonrpc": "2.0", "id": 5, "method": "session/request_permission",
            "params": {"sessionId": "s", "toolCall": {"toolCallId": "c1", "title": title, "kind": "execute"},
            "options": [{"optionId": allow, "kind": "allow_once"}, {"optionId": "no", "kind": "reject_once"}]}})
        .to_string()
    };
    let out = tr.translate(&request("echo harmless", "yes-harmless"));
    let old = raised(&out.events)[0].clone();
    assert_eq!(old.attn_id, "perm-5");

    let out = tr.translate(&request("rm -rf precious", "yes-destroy"));
    assert!(out.writes.is_empty(), "never answered on the user's behalf");
    assert_eq!(
        shape(&out.events),
        ["attention.resolved@1", "attention.raised@1"]
    );
    match &out.events[0].body {
        EventBody::AttentionResolved(r) => {
            assert_eq!(r.attn_id, "perm-5");
            assert_eq!(r.by, ResolvedBy::Timeout);
            assert!(r.answer.is_none());
            assert_eq!(grok_ext(&r.ext)["superseded"], true);
        }
        other => panic!("{other:?}"),
    }
    let new = raised(&out.events)[0].clone();
    assert_eq!(new.attn_id, "perm-5~1");
    assert!(new.summary.contains("rm -rf precious"), "{}", new.summary);
    assert_eq!(tr.pending_attention(), ["perm-5~1"]);

    // The user's allow of `echo harmless` arrives now: nothing is written.
    assert!(tr.respond("perm-5", &Answer::allow()).is_none());
    assert_eq!(tr.pending_attention(), ["perm-5~1"]);
    let reply = tr.respond("perm-5~1", &Answer::deny()).unwrap();
    assert_eq!(
        parse(&reply.writes[0]),
        json!({"jsonrpc": "2.0", "id": 5, "result": {"outcome": {"outcome": "selected", "optionId": "no"}}})
    );

    // The id reused once more, after the answer: yet another id; the old ones answer nothing.
    let out = tr.translate(&request("ls", "yes-ls"));
    assert_eq!(shape(&out.events), ["attention.raised@1"]);
    assert_eq!(raised(&out.events)[0].attn_id, "perm-5~2");
    assert!(tr.respond("perm-5", &Answer::allow()).is_none());
    assert!(tr.respond("perm-5~1", &Answer::allow()).is_none());
    let reply = tr.respond("perm-5~2", &Answer::allow()).unwrap();
    assert_eq!(
        parse(&reply.writes[0])["result"]["outcome"],
        json!({"outcome": "selected", "optionId": "yes-ls"})
    );

    // Distinct JSON-RPC ids that print alike (`5` and `"5"`) are two requests with two ids.
    let mut tr = ready();
    tr.send("x", 1);
    tr.translate(&permission(
        json!(5),
        json!([{"optionId": "a", "kind": "allow_once"}]),
    ));
    let out = tr.translate(&permission(
        json!("5"),
        json!([{"optionId": "b", "kind": "allow_once"}]),
    ));
    assert_eq!(shape(&out.events), ["attention.raised@1"]);
    assert_eq!(tr.pending_attention(), ["perm-5", "perm-5~1"]);
    let reply = tr.respond("perm-5", &Answer::allow()).unwrap();
    assert_eq!(
        parse(&reply.writes[0]),
        json!({"jsonrpc": "2.0", "id": 5, "result": {"outcome": {"outcome": "selected", "optionId": "a"}}})
    );
    let reply = tr.respond("perm-5~1", &Answer::allow()).unwrap();
    assert_eq!(
        parse(&reply.writes[0]),
        json!({"jsonrpc": "2.0", "id": "5", "result": {"outcome": {"outcome": "selected", "optionId": "b"}}})
    );
}

#[test]
fn runtime_cancel_request_withdraws_the_item() {
    let mut tr = ready();
    tr.send("x", 1);
    tr.translate(&permission(
        json!(5),
        json!([{"optionId": "y", "kind": "allow_once"}]),
    ));
    let out =
        tr.translate(r#"{"jsonrpc":"2.0","method":"$/cancel_request","params":{"requestId":5}}"#);
    assert_eq!(shape(&out.events), ["attention.resolved@1"]);
    match &out.events[0].body {
        EventBody::AttentionResolved(r) => {
            assert_eq!(r.attn_id, "perm-5");
            assert_eq!(r.by, ResolvedBy::Timeout);
            assert!(r.answer.is_none());
        }
        _ => unreachable!(),
    }
    assert!(
        out.writes.is_empty(),
        "twr does not answer a withdrawn request"
    );
    assert!(tr.pending_attention().is_empty());
}

#[test]
fn a_finished_prompt_drops_its_pending_permissions() {
    let mut tr = ready();
    tr.send("x", 1);
    tr.translate(&permission(
        json!(5),
        json!([{"optionId": "y", "kind": "allow_once"}]),
    ));
    assert_eq!(tr.pending_attention(), ["perm-5"]);
    let out = tr.translate(r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#);
    assert_eq!(shape(&out.events), ["turn.ended@1"]);
    assert!(tr.pending_attention().is_empty());
}

#[test]
fn stop_answers_pending_requests_cancelled_and_permission_requests_after_stop() {
    let mut tr = ready();
    tr.send("x", 1);
    tr.translate(&permission(
        json!(5),
        json!([{"optionId": "y", "kind": "allow_once"}]),
    ));
    let out = tr.stop();
    assert!(out.events.is_empty());
    let w: Vec<Value> = out.writes.iter().map(|l| parse(l)).collect();
    assert_eq!(w[0]["method"], "session/cancel");
    assert_eq!(w[0]["params"]["sessionId"], "s");
    assert_eq!(w[1]["id"], 5);
    assert_eq!(w[1]["result"]["outcome"]["outcome"], "cancelled");
    // A request arriving while stopping raises nothing and is cancelled.
    let out = tr.translate(&permission(
        json!(6),
        json!([{"optionId": "y", "kind": "allow_once"}]),
    ));
    assert!(out.events.is_empty());
    assert_eq!(
        parse(&out.writes[0])["result"]["outcome"]["outcome"],
        "cancelled"
    );
    assert!(tr.stop().writes.is_empty(), "stop is idempotent");
    // Idle session: stop writes nothing.
    let mut idle = ready();
    assert!(idle.stop().writes.is_empty());
}

#[test]
fn updates_for_other_sessions_are_ignored_and_unknown_calls_are_synthesized() {
    let mut tr = ready();
    tr.send("x", 1);
    let other = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"sub","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"sub"}}}}"#;
    assert!(tr.translate(other).events.is_empty());
    let upd = r#"{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"t9","kind":"read","status":"completed","rawInput":{"path":"a.rs"},"rawOutput":"fn main"}}}"#;
    let out = tr.translate(upd);
    assert_eq!(
        shape(&out.events),
        ["agent.tool_call@1", "agent.tool_result@1"]
    );
    let tool_events: Vec<_> = out
        .events
        .iter()
        .filter(|e| !matches!(e.body, EventBody::TaskUpdated(_)))
        .collect();
    match (&tool_events[0].body, &tool_events[1].body) {
        (EventBody::AgentToolCall(c), EventBody::AgentToolResult(r)) => {
            assert_eq!(c.tool, "read");
            assert!(!c.risky);
            assert_eq!(c.input_summary, r#"{"path":"a.rs"}"#);
            assert_eq!(r.output_summary, "fn main");
        }
        _ => unreachable!(),
    }
    // A second completion of the same call is not reported again.
    assert!(tr.translate(upd).events.is_empty());
}

#[test]
fn a_session_that_cannot_be_resumed_fails() {
    let mut tr = Translator::new(TranslatorOptions {
        resume_session: Some("old".into()),
        ..opts()
    });
    tr.start();
    tr.send("x", 3);
    let out = tr.translate(
        r#"{"jsonrpc":"2.0","id":1,"result":{"agentCapabilities":{"loadSession":false}}}"#,
    );
    assert_eq!(shape(&out.events), ["text@3", "turn.ended@3"]);
    assert!(out
        .fatal
        .unwrap()
        .contains("neither session/resume nor session/load"));
}

#[test]
fn initialize_errors_are_fatal() {
    let mut tr = Translator::new(opts());
    tr.start();
    tr.send("x", 1);
    let out = tr.translate(
        r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"Unsupported protocol version"}}"#,
    );
    assert_eq!(shape(&out.events), ["text@1", "turn.ended@1"]);
    assert!(out.fatal.unwrap().contains("initialize failed"));
}

fn ask(id: Value, questions: Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": "_x.ai/ask_user_question",
        "params": {"sessionId": "s", "toolCallId": "c9", "questions": questions, "mode": "default"}})
    .to_string()
}

/// `_x.ai/ask_user_question` with two questions: one item each; the reply waits for both.
/// Free text answers too; a decline cancels the request and withdraws the other question.
#[test]
fn questions_are_relayed_one_item_each() {
    let qs = json!([
        {"question": "Color?", "options": [{"label": "red"}, {"label": "blue"}], "multiSelect": null},
        {"question": "Size?", "options": [{"label": "S"}, {"label": "L"}], "multiSelect": true},
    ]);
    let mut tr = ready();
    tr.send("x", 1);
    let out = tr.translate(&ask(json!(3), qs.clone()));
    assert!(out.writes.is_empty());
    let a = raised(&out.events);
    assert_eq!(
        a.iter().map(|a| a.attn_id.as_str()).collect::<Vec<_>>(),
        ["ask-3#q0", "ask-3#q1"]
    );
    assert_eq!(a[0].summary, "(1/2) Color?");
    assert_eq!(a[1].options, ["S", "L"]);
    assert_eq!(grok_ext(&a[1].ext)["multi_select"], true);
    assert_eq!(a[0].call_id.as_deref(), Some("c9"));
    assert!(tr
        .respond("ask-3#q1", &Answer::new("L"))
        .unwrap()
        .writes
        .is_empty());
    let mut free = Answer::allow();
    free.message = Some("green, please".into());
    let reply = tr.respond("ask-3#q0", &free).unwrap();
    assert_eq!(
        parse(&reply.writes[0]),
        json!({"jsonrpc": "2.0", "id": 3, "result": {"outcome": "accepted",
            "answers": {"Color?": "green, please", "Size?": "L"}}})
    );
    assert!(tr.pending_attention().is_empty());

    // A decline: `cancelled`, and the sibling is withdrawn.
    tr.translate(&ask(json!(4), qs));
    let reply = tr.respond("ask-4#q0", &Answer::deny()).unwrap();
    assert_eq!(
        parse(&reply.writes[0]),
        json!({"jsonrpc": "2.0", "id": 4, "result": {"outcome": "cancelled"}})
    );
    assert_eq!(shape(&reply.events), ["attention.resolved@1"]);
    assert!(tr.respond("ask-4#q1", &Answer::new("S")).is_none());
    assert!(tr.pending_attention().is_empty());
}

/// `_x.ai/exit_plan_mode`: allow approves, anything else asks grok to revise; stop answers
/// every pending request (permissions and questions `cancelled`, a plan with an error).
#[test]
fn plan_approval_and_stop_replies() {
    let plan = |id: u64| {
        json!({"jsonrpc": "2.0", "id": id, "method": "_x.ai/exit_plan_mode",
            "params": {"sessionId": "s", "toolCallId": "c7", "planContent": "\n# Plan\nstep 1\n"}})
        .to_string()
    };
    let mut tr = ready();
    tr.send("x", 1);
    let out = tr.translate(&plan(1));
    let a = raised(&out.events)[0];
    assert_eq!(a.attn_id, "plan-1");
    assert_eq!(a.summary, "Approve Grok's plan and leave plan mode: # Plan");
    let reply = tr.respond("plan-1", &Answer::deny()).unwrap();
    assert_eq!(
        parse(&reply.writes[0]),
        json!({"jsonrpc": "2.0", "id": 1, "result": {"outcome": "rejected"}})
    );
    tr.translate(&plan(2));
    let reply = tr.respond("plan-2", &Answer::allow()).unwrap();
    assert_eq!(
        parse(&reply.writes[0])["result"],
        json!({"outcome": "approved"})
    );

    tr.translate(&plan(3));
    tr.translate(&ask(json!(4), json!([{"question": "Q?", "options": []}])));
    tr.translate(&permission(
        json!(5),
        json!([{"optionId": "y", "kind": "allow_once"}]),
    ));
    let out = tr.stop();
    let w: Vec<Value> = out.writes.iter().map(|w| parse(w)).collect();
    assert_eq!(w[0]["method"], "session/cancel");
    assert!(w.contains(&json!({"jsonrpc": "2.0", "id": 3, "error": {"code": -32000, "message": "twr stopped the session"}})));
    assert!(w.contains(&json!({"jsonrpc": "2.0", "id": 4, "result": {"outcome": "cancelled"}})));
    assert!(w.contains(
        &json!({"jsonrpc": "2.0", "id": 5, "result": {"outcome": {"outcome": "cancelled"}}})
    ));
    assert_eq!(w.len(), 4);
    assert!(out.events.is_empty());
    // Requests arriving after stop are answered the same way.
    let out = tr.translate(&ask(json!(6), json!([{"question": "Q?"}])));
    assert_eq!(
        parse(&out.writes[0])["result"],
        json!({"outcome": "cancelled"})
    );
}

/// Grok's `yolo` report for the session arriving while the session opens
/// (new session: its id is not known yet; resume: it is) is not dropped. The session never
/// becomes ready and no prompt is sent; a later report reaching a ready session is checked
/// before the next prompt too.
#[test]
fn yolo_reported_while_the_session_opens_fails_it() {
    let changed = |sid: &str, yolo: bool| {
        json!({"jsonrpc": "2.0", "method": "_x.ai/sessions/changed", "params": {
            "upserted": [{"sessionId": sid, "yolo": yolo}], "removed": []}})
        .to_string()
    };
    let init = r#"{"jsonrpc":"2.0","id":1,"result":{"agentCapabilities":{"sessionCapabilities":{"resume":{}}}}}"#;
    for resume in [None, Some("S1")] {
        let mut tr = Translator::new(TranslatorOptions {
            resume_session: resume.map(str::to_string),
            ..opts()
        });
        tr.start();
        tr.send("hi", 1);
        tr.translate(init);
        // Opening: grok says the (future) session is in always-approve.
        let early = tr.translate(&changed("S1", true));
        // Resuming, the session id is known: the report fails the session at once.
        assert_eq!(early.fatal.is_some(), resume.is_some(), "{early:?}");
        let mut all = early.events.clone();
        let mut fatal = early.fatal.clone();
        let mut writes = early.writes.clone();
        if fatal.is_none() {
            let out = tr.translate(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"S1"}}"#);
            all.extend(out.events);
            fatal = out.fatal;
            writes.extend(out.writes);
        }
        let why = fatal.unwrap_or_else(|| panic!("{resume:?}: the session ran in yolo"));
        assert!(why.contains("yolo"), "{why}");
        assert!(!tr.ready());
        assert!(
            !writes.iter().any(|w| w.contains("session/prompt")),
            "{resume:?}: no prompt: {writes:?}"
        );
        assert!(!all
            .iter()
            .any(|e| matches!(e.body, EventBody::ThreadStarted(_))));
        assert!(all.iter().any(|e| matches!(
            &e.body,
            EventBody::TurnEnded(t) if t.result == TurnResult::Error
        )));
    }
    // Another session's report does not count, and a later `yolo: false` for ours is the
    // current state.
    let mut tr = Translator::new(opts());
    tr.start();
    tr.translate(init);
    tr.translate(&changed("OTHER", true));
    tr.translate(&changed("S1", true));
    tr.translate(&changed("S1", false));
    let out = tr.translate(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"S1"}}"#);
    assert!(out.fatal.is_none(), "{out:?}");
    assert!(tr.ready());
}

/// A grok translator in `mode` with session S1 open and turn 1's prompt (id 3) running.
fn running_in(mode: &str) -> Translator {
    let mut tr = Translator::new(TranslatorOptions {
        mode: mode.into(),
        ..opts()
    });
    tr.start();
    tr.translate(r#"{"jsonrpc":"2.0","id":1,"result":{"agentCapabilities":{}}}"#);
    let out = tr.translate(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"S1"}}"#);
    assert!(out.fatal.is_none(), "{out:?}");
    let out = tr.send("plan it", 1);
    assert!(out.writes[0].contains("session/prompt"));
    tr
}

fn mode_update(mode: &str) -> String {
    json!({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": "S1",
        "update": {"sessionUpdate": "current_mode_update", "currentModeId": mode}}})
    .to_string()
}

/// Asks to exit plan mode and answers with `answer` (None: no answer yet).
fn plan_exit(tr: &mut Translator, answer: Option<Answer>) {
    let out = tr.translate(
        r#"{"jsonrpc":"2.0","id":"x1","method":"_x.ai/exit_plan_mode","params":{"sessionId":"S1","toolCallId":"c1","planContent":"noop"}}"#,
    );
    let EventBody::AttentionRaised(a) = &out.events.last().unwrap().body else {
        panic!("{out:?}")
    };
    if let Some(answer) = answer {
        tr.respond(&a.attn_id.clone(), &answer).expect("pending");
    }
}

/// Plan-exit consent (as for Claude): grok leaves plan mode
/// (`current_mode_update default`) once the user approves the plan. That one change, plan →
/// ask level after the user's approval, is accepted and recorded on the turn's end; the next
/// prompt runs. Without the approval, or to anything wider than ask level, the session fails.
#[test]
fn an_approved_plan_exit_continues_at_ask_level() {
    let mut tr = running_in("plan");
    plan_exit(&mut tr, Some(Answer::allow()));
    let out = tr.translate(&mode_update("default"));
    assert!(out.fatal.is_none(), "{out:?}");
    let out = tr.translate(r#"{"jsonrpc":"2.0","id":3,"result":{"stopReason":"end_turn"}}"#);
    let EventBody::TurnEnded(t) = &out.events.last().unwrap().body else {
        panic!("{out:?}")
    };
    assert_eq!(t.result, TurnResult::Completed);
    let grok = &t.ext.as_ref().unwrap()["grok"];
    assert_eq!(
        grok["mode_changed"],
        json!({"from": "plan", "to": "default", "consent": "plan exit approved by the user"})
    );
    assert!(grok["mode_note"].as_str().unwrap().contains("plan"));
    // The session goes on in default: the next prompt is sent, a narrower report is fine.
    let out = tr.send("now do it", 2);
    assert!(
        out.fatal.is_none() && out.writes[0].contains("session/prompt"),
        "{out:?}"
    );
    assert!(tr.translate(&mode_update("plan")).fatal.is_none());

    // Without the user's approval: fatal (no answer yet, or a rejection).
    for answer in [None, Some(Answer::deny())] {
        let mut tr = running_in("plan");
        plan_exit(&mut tr, answer.clone());
        let out = tr.translate(&mode_update("default"));
        assert!(out.fatal.is_some(), "{answer:?}: {out:?}");
    }
    // Approved, but wider than ask level: fatal.
    for wider in [
        "acceptEdits",
        "auto",
        "yolo",
        "bypassPermissions",
        "made-up",
    ] {
        let mut tr = running_in("plan");
        plan_exit(&mut tr, Some(Answer::allow()));
        let out = tr.translate(&mode_update(wider));
        assert!(out.fatal.is_some(), "{wider}: {out:?}");
    }
    // Approved: default, then a further widening is still fatal.
    let mut tr = running_in("plan");
    plan_exit(&mut tr, Some(Answer::allow()));
    assert!(tr.translate(&mode_update("default")).fatal.is_none());
    assert!(tr.translate(&mode_update("acceptEdits")).fatal.is_some());
    // Only from plan: a dontAsk session's approval is no consent to default.
    let mut tr = running_in("dontAsk");
    plan_exit(&mut tr, Some(Answer::allow()));
    assert!(tr.translate(&mode_update("default")).fatal.is_some());
}

#[test]
fn modes_for_bypass_requests() {
    let avail: Vec<String> = ["default", "always-approve"].map(String::from).to_vec();
    assert!(!mode_acceptable("always-approve", ""));
    assert_eq!(narrowing_target("", &avail).as_deref(), Some("default"));
    // The user asked for bypass: always-approve is accepted (no switch).
    assert!(mode_acceptable("always-approve", "bypassPermissions"));
    // The user asked for bypass: yoloMode is true on the session request.
    let mut tr = Translator::new(TranslatorOptions {
        mode: "bypassPermissions".into(),
        ..opts()
    });
    tr.start();
    let out = tr.translate(r#"{"jsonrpc":"2.0","id":1,"result":{"agentCapabilities":{}}}"#);
    assert_eq!(parse(&out.writes[0])["params"]["_meta"]["yoloMode"], true);
}

// ---------------------------------------------------------------------------------------------
// Command line, environment, MCP mapping
// ---------------------------------------------------------------------------------------------

#[test]
fn args_for_a_plain_thread() {
    let cfg = SpawnConfig::default();
    assert_eq!(
        build_args(&cfg),
        [
            "--no-auto-update",
            "--permission-mode",
            "default",
            "agent",
            "--no-leader",
            "stdio"
        ]
    );
    // twr's `manual` is grok's `default`; the model and rules go over ACP, not the command
    // line (grok 1.0.44 ignores `agent -m` and `--rules` in `agent stdio`).
    let cfg = SpawnConfig {
        mode: "manual".into(),
        model: Some("grok-4.6".into()),
        system_append: Some("rules".into()),
        ..Default::default()
    };
    let a = build_args(&cfg);
    assert_eq!(a[2], "default");
    assert!(!a
        .iter()
        .any(|x| x == "-m" || x == "--rules" || x.contains("grok-4.6")));
    assert!(agent_profile(&cfg).is_none());
}

#[test]
fn args_for_the_orchestrator() {
    let cfg = SpawnConfig {
        mode: "acceptEdits".into(),
        system_append: Some("You are the orchestrator.".into()),
        allowed_tools: vec!["mcp__twr".into()],
        builtin_tools: Some(vec![]),
        model: Some("grok-4.5".into()),
        ..Default::default()
    };
    // No built-ins: the agent profile's toolset is only grok's MCP meta-tools (the real
    // restriction); --deny per built-in too (not enforced by grok 1.0.44 in agent stdio).
    assert_eq!(
        build_args(&cfg),
        [
            "--no-auto-update",
            "--permission-mode",
            "acceptEdits",
            "--allow",
            "mcp__twr",
            "--deny",
            "Bash",
            "--deny",
            "Read",
            "--deny",
            "Edit",
            "--deny",
            "Write",
            "--deny",
            "Grep",
            "--deny",
            "Glob",
            "--deny",
            "WebFetch",
            "--deny",
            "WebSearch",
            "agent",
            "--no-leader",
            "stdio",
        ]
    );
    assert_eq!(
        agent_profile(&cfg).unwrap(),
        json!({"name": "twr-restricted", "description": "twr: only the tools twr allows",
            "tools": ["search_tool", "use_tool"]})
    );
    assert_eq!(grok_build::denied_builtins(&cfg), grok_build::BUILTIN_DENY);
    // Some built-ins, or no restriction: nothing denied.
    for builtin_tools in [None, Some(vec!["Bash".to_string()])] {
        let cfg = SpawnConfig {
            builtin_tools,
            ..Default::default()
        };
        assert!(!build_args(&cfg).contains(&"--deny".to_string()));
    }
    // Some built-ins: grok's names for them (unknown names taken as grok names).
    assert_eq!(
        profile_tools(&[
            "Bash".into(),
            "Read".into(),
            "Glob".into(),
            "image_gen".into()
        ]),
        [
            "search_tool",
            "use_tool",
            "run_terminal_command",
            "get_command_or_subagent_output",
            "kill_command_or_subagent",
            "read_file",
            "list_dir",
            "image_gen"
        ]
    );
}

#[test]
fn denied_builtins_and_profile_tools_are_in_thread_started() {
    let mut tr = Translator::new(TranslatorOptions {
        denied_tools: vec!["Bash".into(), "Read".into()],
        agent_profile: Some(json!({"name": "p", "tools": ["search_tool", "use_tool"]})),
        ..opts()
    });
    tr.start();
    tr.translate(
        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#,
    );
    let out = tr.translate(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s"}}"#);
    let g = grok_ext(&started(&out.events)[0].ext).clone();
    assert_eq!(g["denied_tools"], json!(["Bash", "Read"]));
    assert_eq!(g["tools"], json!(["search_tool", "use_tool"]));
    assert!(g.get("warning").is_none());
    // Without them neither shows up.
    let g = grok_ext(&started(&ready_events())[0].ext).clone();
    assert!(g.get("denied_tools").is_none() && g.get("tools").is_none());
}

fn ready_events() -> Vec<Emitted> {
    let mut tr = Translator::new(opts());
    tr.start();
    tr.translate(
        r#"{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":1,"agentCapabilities":{}}}"#,
    );
    tr.translate(r#"{"jsonrpc":"2.0","id":2,"result":{"sessionId":"s"}}"#)
        .events
}

/// grok 1.0.44 drops `ui` from a `GROK_CONFIG` overlay (verified: it did not stop
/// always-approve), so twr sets no overlay and leaves an inherited `GROK_CONFIG` alone.
#[test]
fn env_disables_updates_and_sets_no_config_overlay() {
    let cfg = SpawnConfig {
        env: vec![("TWR_THREAD".into(), "t1".into())],
        ..Default::default()
    };
    assert_eq!(
        build_env(&cfg),
        [
            ("TWR_THREAD".to_string(), "t1".to_string()),
            ("GROK_DISABLE_AUTOUPDATER".to_string(), "1".to_string())
        ]
    );
}

#[test]
fn mcp_config_maps_to_acp_servers() {
    let claude = json!({"mcpServers": {
        "zeta": {"type": "http", "url": "https://x/mcp", "headers": {"Authorization": "Bearer t"}},
        "alpha": {"command": "twr", "args": ["mcp"], "env": {"TOKEN": "s", "N": 3}},
        "bare": {"command": "srv"},
    }});
    assert_eq!(
        acp_mcp_servers(&claude),
        [
            json!({"name": "alpha", "command": "twr", "args": ["mcp"], "env": [{"name": "N", "value": "3"}, {"name": "TOKEN", "value": "s"}]}),
            json!({"name": "bare", "command": "srv", "args": [], "env": []}),
            json!({"type": "http", "name": "zeta", "url": "https://x/mcp", "headers": [{"name": "Authorization", "value": "Bearer t"}]}),
        ]
    );
    let acp = json!([{"name": "x", "command": "y", "args": [], "env": []}]);
    assert_eq!(acp_mcp_servers(&acp), acp.as_array().unwrap().clone());
    assert!(acp_mcp_servers(&json!({"mcpServers": {}})).is_empty());
    assert!(acp_mcp_servers(&Value::Null).is_empty());
}

#[test]
fn attach_command_resumes_in_the_worktree() {
    let cfg = SpawnConfig {
        cwd: "/tmp/wt".into(),
        ..Default::default()
    };
    let cmd = GrokBuildAdapter::with_bin("/opt/grok")
        .attach_command(&cfg, "sess-1")
        .unwrap();
    assert_eq!(cmd.get_program(), "/opt/grok");
    let args: Vec<_> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        args,
        [
            "--no-auto-update",
            "--permission-mode",
            "default",
            "--resume",
            "sess-1"
        ]
    );
    assert_eq!(cmd.get_current_dir(), Some(Path::new("/tmp/wt")));
    let env = |cmd: &std::process::Command| -> std::collections::BTreeMap<String, Option<String>> {
        cmd.get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect()
    };
    let e = env(&cmd);
    assert_eq!(e["GROK_DISABLE_AUTOUPDATER"].as_deref(), Some("1"));
    assert!(!e.contains_key("GROK_CONFIG"));
    // twr's secrets are never passed to the interactive CLI.
    for k in ["TWR_RUNNER_TOKEN", "TWR_ORCH_TOKEN"] {
        assert_eq!(e[k], None, "{k}");
    }
    // The thread's mode and tool limits carry over; nothing is widened by attaching.
    let orch = SpawnConfig {
        mode: "plan".into(),
        allowed_tools: vec!["mcp__twr".into()],
        builtin_tools: Some(vec![]),
        ..cfg
    };
    let args: Vec<_> = GrokBuildAdapter::with_bin("/opt/grok")
        .attach_command(&orch, "sess-2")
        .unwrap()
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        &args[..5],
        [
            "--no-auto-update",
            "--permission-mode",
            "plan",
            "--allow",
            "mcp__twr",
        ]
    );
    assert_eq!(
        args.iter().filter(|a| *a == "--deny").count(),
        grok_build::BUILTIN_DENY.len()
    );
    assert_eq!(&args[args.len() - 2..], ["--resume", "sess-2"]);
    // The thread's model and effort: the interactive CLI switches a resumed session to them.
    let tuned = SpawnConfig {
        model: Some("grok-4.6".into()),
        effort: Some("high".into()),
        ..SpawnConfig::default()
    };
    let args: Vec<_> = GrokBuildAdapter::with_bin("/opt/grok")
        .attach_command(&tuned, "sess-3")
        .unwrap()
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        args,
        [
            "--no-auto-update",
            "--permission-mode",
            "default",
            "--model",
            "grok-4.6",
            "--reasoning-effort",
            "high",
            "--resume",
            "sess-3"
        ]
    );
    let a = twr_adapters::find(grok_build::NAME).unwrap();
    assert_eq!(a.info().tier, Tier::Headless);
    assert!(a.info().description.contains("verified with grok 1.0.44"));
}

// ---------------------------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------------------------

/// Answers to `_x.ai/billing` in `tests/fixtures/grok/usage.stdout.jsonl`: one recorded from
/// grok 1.0.46 on 2026-10-09 (no `creditUsagePercent`: protobuf JSON leaves a zero out), one
/// with a percentage, a monthly period with the percentage as a string, and the error of an
/// account not signed in with grok.com.
fn usage_answers() -> Vec<Value> {
    std::fs::read_to_string(fixture_path("real_noauth_v1041").with_file_name("usage.stdout.jsonl"))
        .unwrap()
        .lines()
        .map(parse)
        .collect()
}

static USAGE_NOW: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1_791_540_000);

fn usage_clock() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(USAGE_NOW.load(std::sync::atomic::Ordering::SeqCst), 0)
        .unwrap()
}

fn billing_reads(writes: &[String]) -> Vec<Value> {
    writes
        .iter()
        .map(|l| parse(l))
        .filter(|v| v["method"] == "_x.ai/billing")
        .collect()
}

fn answer(read: &Value, answer: &Value) -> String {
    let mut a = answer.clone();
    a["id"] = read["id"].clone();
    a.to_string()
}

fn limits_in(events: &[Emitted]) -> Vec<RuntimeLimits> {
    events
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::RuntimeLimits(l) => {
                assert_eq!(e.turn, None);
                Some(l.clone())
            }
            _ => None,
        })
        .collect()
}

/// The credit period of an `_x.ai/billing` answer is one window: weekly, monthly, used
/// `creditUsagePercent` (0 when left out), resetting at the period's end.
#[test]
fn billing_answers_become_windows() {
    use twr_adapters::grok_build::acp::billing_windows;
    let a = usage_answers();
    let (w, plan) = billing_windows(&a[0]["result"]);
    assert_eq!(plan.as_deref(), Some("X Premium"));
    assert_eq!(w.len(), 1);
    assert_eq!((w[0].window.as_str(), w[0].pct()), ("seven_day", 0));
    assert_eq!(
        w[0].resets_at.map(|t| t.to_rfc3339()),
        Some("2026-10-14T23:25:15.680563+00:00".into())
    );
    let (w, plan) = billing_windows(&a[1]["result"]);
    assert_eq!(plan.as_deref(), Some("SuperGrok"));
    assert_eq!((w[0].window.as_str(), w[0].pct()), ("seven_day", 37));
    let (w, plan) = billing_windows(&a[2]["result"]);
    assert_eq!(plan, None);
    assert_eq!(
        (w[0].window.as_str(), w[0].pct(), w[0].label()),
        ("month", 91, "mo".into())
    );
    // No period: nothing known.
    assert!(billing_windows(&json!({"config": {}})).0.is_empty());
    // Another period type: named by its length.
    let (w, _) = billing_windows(
        &json!({"config": {"currentPeriod": {"type": "USAGE_PERIOD_TYPE_DAILY",
        "start": "2026-10-09T00:00:00Z", "end": "2026-10-10T00:00:00Z"}, "creditUsagePercent": 5}}),
    );
    assert_eq!((w[0].label(), w[0].minutes), ("1d".into(), Some(1440)));
}

/// With `usage` set (the adapter sets it), the session reads `_x.ai/billing` once it is ready
/// and again when a prompt ends once the last read is ten minutes old, with ids of their own;
/// an error answer stops the reads. Without it nothing is read (every replay above).
#[test]
fn usage_is_read_when_ready_and_as_it_gets_old() {
    let answers = usage_answers();
    let mut tr = Translator::new(TranslatorOptions {
        usage: true,
        ..opts()
    });
    tr.set_clock(usage_clock);
    let mut writes = tr.start().writes;
    for (dir, line) in fixture("real_permission_allow_v1044") {
        if tr.ready() {
            break;
        }
        if dir == "<<" {
            writes.extend(tr.translate(&line).writes);
        }
    }
    assert!(tr.ready());
    let reads = billing_reads(&writes);
    assert_eq!(reads.len(), 1, "{writes:?}");
    assert!(reads[0]["id"].as_u64().unwrap() >= 1 << 50);
    assert_eq!(reads[0]["params"], json!({}));

    let got = limits_in(&tr.translate(&answer(&reads[0], &answers[1])).events);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].source, "read");
    assert_eq!(got[0].plan.as_deref(), Some("SuperGrok"));
    assert_eq!(got[0].windows[0].pct(), 37);

    // A prompt that ends within ten minutes reads nothing; one after, again.
    let end = |tr: &mut Translator, turn: u64| {
        let p = parse(&tr.send("hi", turn).writes[0]);
        assert_eq!(p["method"], "session/prompt");
        let r = json!({"jsonrpc": "2.0", "id": p["id"], "result": {"stopReason": "end_turn"}});
        tr.translate(&r.to_string()).writes
    };
    USAGE_NOW.fetch_add(9 * 60, std::sync::atomic::Ordering::SeqCst);
    assert!(billing_reads(&end(&mut tr, 1)).is_empty());
    USAGE_NOW.fetch_add(60, std::sync::atomic::Ordering::SeqCst);
    let reads = billing_reads(&end(&mut tr, 2));
    assert_eq!(reads.len(), 1);
    // Refused (not signed in with grok.com): no more reads.
    assert!(limits_in(&tr.translate(&answer(&reads[0], &answers[3])).events).is_empty());
    USAGE_NOW.fetch_add(3600, std::sync::atomic::Ordering::SeqCst);
    assert!(billing_reads(&end(&mut tr, 3)).is_empty());
}

// ---------------------------------------------------------------------------------------------
// Driver against a stub `grok`
// ---------------------------------------------------------------------------------------------

const STUB_PY: &str = r#"
import json, os, sys
fixture, outdir, args = sys.argv[1], sys.argv[2], sys.argv[3:]
with open(os.path.join(outdir, "run.json"), "w") as f:
    json.dump({"args": args, "cwd": os.getcwd(), "pgid": os.getpgid(0), "pid": os.getpid(),
               "env": dict(os.environ)}, f)
log = open(os.path.join(outdir, "stdin.jsonl"), "w")
usage = open(os.path.join(outdir, "usage.jsonl"), "w")
def keep(got):
    # twr's usage reads go to their own log, unanswered: the recordings have none.
    if '"_x.ai/billing"' in got:
        usage.write(got); usage.flush()
        return False
    log.write(got); log.flush()
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
sys.stderr.write("stub: stdin closed\n")
"#;

fn stub(dir: &Path, fixture: &Path) -> PathBuf {
    let py = dir.join("stub.py");
    std::fs::write(&py, STUB_PY).unwrap();
    script(
        dir,
        &format!(
            "exec python3 {} {} {} \"$@\"",
            py.display(),
            fixture.display(),
            dir.display()
        ),
    )
}

fn script(dir: &Path, body: &str) -> PathBuf {
    let path = dir.join("grok");
    install_executable(&path, &format!("#!/bin/sh\n{body}\n"));
    path
}

/// Creates an executable without this process holding it open for writing (see the Claude
/// tests: a concurrently forked child would keep the write descriptor and exec would fail
/// with ETXTBSY).
fn install_executable(path: &Path, content: &str) {
    use std::os::unix::fs::PermissionsExt;
    let src = path.with_extension("src");
    std::fs::write(&src, content).unwrap();
    let st = std::process::Command::new("cp")
        .arg(&src)
        .arg(path)
        .status()
        .unwrap();
    assert!(st.success(), "cp failed");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

type Rx = UnboundedReceiver<AdapterEvent>;

/// The runner token and the orchestrator token never reach the
/// runtime's environment, whether inherited by twr or put in `SpawnConfig::env`.
#[tokio::test]
async fn the_runtime_never_inherits_twr_secrets() {
    std::env::set_var("TWR_RUNNER_TOKEN", "runner-secret");
    let dir = tempfile::tempdir().unwrap();
    let bin = stub(dir.path(), &fixture_path("real_noauth_v1041"));
    let cfg = SpawnConfig {
        cwd: dir.path().to_path_buf(),
        env: vec![
            ("TWR_THREAD".into(), "T1".into()),
            ("TWR_ORCH_TOKEN".into(), "orch-secret".into()),
        ],
        ..Default::default()
    };
    let (h, _rx) = start(&bin, cfg, None);
    let run = dir.path().join("run.json");
    timeout(WAIT, async {
        while !std::fs::read_to_string(&run).is_ok_and(|s| s.ends_with('}')) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the stub never ran");
    let info = parse(&std::fs::read_to_string(&run).unwrap());
    let env = info["env"].as_object().unwrap();
    for k in ["TWR_RUNNER_TOKEN", "TWR_ORCH_TOKEN"] {
        assert!(!env.contains_key(k), "{k} leaked");
    }
    assert!(!info["env"].to_string().contains("secret"));
    assert_eq!(env["TWR_THREAD"], "T1");
    drop(h);
}

fn start(bin: &Path, cfg: SpawnConfig, prompt: Option<&str>) -> (AdapterHandle, Rx) {
    let mut h = GrokBuildAdapter::with_bin(bin)
        .stop_grace(Duration::from_millis(500))
        .spawn(cfg, prompt.map(str::to_string))
        .unwrap();
    let rx = h.take_events().unwrap();
    (h, rx)
}

async fn until(rx: &mut Rx, pred: impl Fn(&AdapterEvent) -> bool) -> Vec<AdapterEvent> {
    let mut seen = Vec::new();
    timeout(WAIT, async {
        loop {
            let ev = rx.recv().await.expect("adapter channel closed");
            let done = pred(&ev);
            seen.push(ev);
            if done {
                return;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out; saw {seen:?}"));
    seen
}

fn is_kind(ev: &AdapterEvent, kind: &str) -> bool {
    matches!(ev, AdapterEvent::Emit { body, .. } if body.kind() == kind)
}

async fn exited(rx: &mut Rx) -> (Option<i32>, String) {
    match until(rx, |e| matches!(e, AdapterEvent::Exited { .. }))
        .await
        .pop()
    {
        Some(AdapterEvent::Exited { code, stderr_tail }) => (code, stderr_tail),
        _ => unreachable!(),
    }
}

fn recorded_stdin(name: &str) -> Vec<Value> {
    fixture(name)
        .into_iter()
        .filter(|(d, _)| d == ">>")
        .map(|(_, l)| norm(parse(&l)))
        .collect()
}

fn written_stdin(dir: &Path) -> Vec<Value> {
    std::fs::read_to_string(dir.join("stdin.jsonl"))
        .unwrap()
        .lines()
        .map(|l| norm(parse(l)))
        .collect()
}

fn run_info(dir: &Path) -> Value {
    parse(&std::fs::read_to_string(dir.join("run.json")).unwrap())
}

#[tokio::test]
async fn driver_permission_multi_turn_matches_the_fixture_stdin() {
    permission_multi_turn("").await;
}

/// The session mode is checked against the mode grok was started with
/// (`permission_mode`: twr's `manual` is grok's `default`).
#[tokio::test]
async fn driver_checks_the_session_against_the_normalized_mode() {
    permission_multi_turn("manual").await;
}

/// Known modes map by level (restricted ones to `plan`, never wider), other
/// spellings of grok's own ids are those ids, and an unknown mode refuses to start (spawn and
/// attach) instead of running as `default`.
#[test]
fn modes_map_by_level_and_unknown_modes_are_refused() {
    let mode = |m: &str| {
        grok_build::permission_mode(&SpawnConfig {
            mode: m.into(),
            ..Default::default()
        })
    };
    for (m, want) in [
        ("", "default"),
        ("manual", "default"),
        ("ask", "default"),
        ("plan", "plan"),
        ("dontAsk", "dontAsk"),
        ("dont-ask", "dontAsk"),
        ("read-only", "plan"),
        ("readOnly", "plan"),
        ("autoEdit", "acceptEdits"),
        ("auto", "auto"),
        ("yolo", "bypassPermissions"),
    ] {
        assert_eq!(mode(m).unwrap(), want, "{m}");
    }
    let cfg = SpawnConfig {
        mode: "made-up".into(),
        ..Default::default()
    };
    assert!(mode("made-up").is_err());
    let Err(e) = GrokBuildAdapter::with_bin("/bin/false").spawn(cfg.clone(), None) else {
        panic!("an unknown mode must not start");
    };
    assert!(e.to_string().contains("unknown permission mode"), "{e}");
    assert!(GrokBuildAdapter::with_bin("/bin/false")
        .attach_command(&cfg, "s")
        .is_none());
}

async fn permission_multi_turn(mode: &str) {
    let dir = tempfile::tempdir().unwrap();
    let bin = stub(
        dir.path(),
        &fixture_path("synthetic_new_permission_multiturn"),
    );
    // The orchestrator's MCP config arrives as a 0600 file holding a token.
    let mcp = dir.path().join("mcp.json");
    std::fs::write(
        &mcp,
        json!({"mcpServers": {"twr": {
            "command": "/usr/bin/twr",
            "args": ["mcp", "--socket", "/run/twr.sock"],
            "env": {"TWR_ORCH_TOKEN": "tok"},
        }}})
        .to_string(),
    )
    .unwrap();
    let cfg = SpawnConfig {
        thread: "t1".into(),
        cwd: dir.path().to_path_buf(),
        mcp_config_file: Some(mcp),
        env: vec![("TWR_THREAD".into(), "t1".into())],
        mode: mode.into(),
        ..Default::default()
    };
    let (h, mut rx) = start(&bin, cfg, Some("Create hello.txt containing hi"));
    let evs = until(&mut rx, |e| is_kind(e, "attention.raised")).await;
    let (pid, attn) = match (&evs[0], evs.last().unwrap()) {
        (
            AdapterEvent::Emit {
                body: EventBody::ThreadStarted(s),
                ..
            },
            AdapterEvent::Emit {
                body: EventBody::AttentionRaised(a),
                turn,
                ..
            },
        ) => {
            assert_eq!(*turn, Some(1));
            (
                s.ext.as_ref().unwrap()["twr"]["pid"].as_u64().unwrap(),
                a.attn_id.clone(),
            )
        }
        other => panic!("unexpected events {other:?}"),
    };
    h.respond(&attn, Answer::allow()).unwrap();
    until(&mut rx, |e| is_kind(e, "turn.ended")).await;
    h.send("Now say bye", 2).unwrap();
    let evs = until(&mut rx, |e| is_kind(e, "turn.ended")).await;
    assert!(matches!(
        evs.last(),
        Some(AdapterEvent::Emit { turn: Some(2), .. })
    ));
    h.stop().unwrap();
    let (code, tail) = exited(&mut rx).await;
    assert_eq!(code, Some(0));
    assert!(tail.contains("stub: stdin closed"), "{tail}");
    assert_eq!(
        written_stdin(dir.path()),
        recorded_stdin("synthetic_new_permission_multiturn")
    );
    let info = run_info(dir.path());
    assert_eq!(info["pid"].as_u64(), Some(pid));
    assert_eq!(info["pgid"], info["pid"], "own process group");
    assert_eq!(
        Path::new(info["cwd"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        dir.path().canonicalize().unwrap()
    );
    assert_eq!(info["env"]["TWR_THREAD"], "t1");
    assert_eq!(info["env"]["GROK_DISABLE_AUTOUPDATER"], "1");
    let args: Vec<&str> = info["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert_eq!(args, build_args(&SpawnConfig::default()));
    // The token travels over stdin only.
    assert!(!args.iter().any(|a| a.contains("tok")));
}

/// The real recordings through the whole adapter (a stub replays them): the worker session
/// with its model, and the orchestrator with rules, agent profile, allow/deny rules and its
/// MCP server from a config file. Every line written matches the recording.
#[tokio::test]
async fn driver_replays_the_real_recordings() {
    // Worker: allow, then a second turn.
    let dir = tempfile::tempdir().unwrap();
    let bin = stub(dir.path(), &fixture_path("real_permission_allow_v1044"));
    let cfg = SpawnConfig {
        cwd: dir.path().to_path_buf(),
        model: Some(FAST.into()),
        ..Default::default()
    };
    let (h, mut rx) = start(&bin, cfg, Some("Create a file named hello.txt containing exactly: hi\nUse your file-writing tool. Do nothing else."));
    let evs = until(&mut rx, |e| is_kind(e, "attention.raised")).await;
    let attn = match evs.last() {
        Some(AdapterEvent::Emit {
            body: EventBody::AttentionRaised(a),
            ..
        }) => a.attn_id.clone(),
        other => panic!("{other:?}"),
    };
    h.respond(&attn, Answer::allow()).unwrap();
    until(&mut rx, |e| is_kind(e, "turn.ended")).await;
    h.send(
        "What text is in hello.txt? Answer in one word without using any tool.",
        2,
    )
    .unwrap();
    until(&mut rx, |e| is_kind(e, "turn.ended")).await;
    h.stop().unwrap();
    exited(&mut rx).await;
    assert_eq!(
        written_stdin(dir.path()),
        recorded_stdin("real_permission_allow_v1044")
    );

    // Orchestrator.
    let dir = tempfile::tempdir().unwrap();
    let bin = stub(dir.path(), &fixture_path("real_orchestrator_v1044"));
    let mcp = dir.path().join("mcp.json");
    std::fs::write(
        &mcp,
        json!({"mcpServers": {"pingsrv": {"command": "python3", "args": ["/work/mcp_ping.py"]}}})
            .to_string(),
    )
    .unwrap();
    let cfg = SpawnConfig {
        cwd: dir.path().to_path_buf(),
        system_append: Some("You are a test orchestrator. Keep answers to one line.".into()),
        allowed_tools: vec!["mcp__pingsrv".into()],
        builtin_tools: Some(vec![]),
        mcp_config_file: Some(mcp),
        ..Default::default()
    };
    let args = build_args(&cfg);
    let (h, mut rx) = start(&bin, cfg, Some("First call the ping tool. Then run the shell command `ls` using your shell tool. Report both results in one short line."));
    until(&mut rx, |e| is_kind(e, "attention.raised")).await;
    let attn = "perm-0".to_string();
    h.respond(&attn, Answer::allow()).unwrap();
    until(&mut rx, |e| is_kind(e, "turn.ended")).await;
    h.stop().unwrap();
    exited(&mut rx).await;
    assert_eq!(
        written_stdin(dir.path()),
        recorded_stdin("real_orchestrator_v1044")
    );
    let info = run_info(dir.path());
    let got: Vec<&str> = info["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    assert_eq!(got, args);
}

#[tokio::test]
async fn driver_stop_cancels_a_pending_permission() {
    let dir = tempfile::tempdir().unwrap();
    let bin = stub(dir.path(), &fixture_path("synthetic_cancel"));
    let cfg = SpawnConfig {
        cwd: dir.path().to_path_buf(),
        ..Default::default()
    };
    let (h, mut rx) = start(&bin, cfg, Some("Run the tests"));
    until(&mut rx, |e| is_kind(e, "attention.raised")).await;
    h.stop().unwrap();
    let evs = until(&mut rx, |e| matches!(e, AdapterEvent::Exited { .. })).await;
    assert!(!evs.iter().any(|e| is_kind(e, "attention.resolved")));
    // The stub printed the prompt's `cancelled` result before stdin closed.
    let ended: Vec<_> = evs
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::Emit {
                body: EventBody::TurnEnded(t),
                ..
            } => Some(t.result),
            _ => None,
        })
        .collect();
    assert_eq!(ended, [TurnResult::Interrupted]);
    assert_eq!(
        written_stdin(dir.path()),
        recorded_stdin("synthetic_cancel")
    );
}

#[tokio::test]
async fn driver_auth_failure_ends_the_turn_and_exits() {
    let dir = tempfile::tempdir().unwrap();
    let bin = stub(dir.path(), &fixture_path("real_noauth_v1041"));
    let cfg = SpawnConfig {
        cwd: dir.path().to_path_buf(),
        ..Default::default()
    };
    let (_h, mut rx) = start(&bin, cfg, Some("hello"));
    let evs = until(&mut rx, |e| matches!(e, AdapterEvent::Exited { .. })).await;
    let kinds: Vec<&str> = evs
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::Emit { body, .. } => Some(body.kind()),
            _ => None,
        })
        .collect();
    assert_eq!(kinds, ["agent.text", "turn.ended"]);
    match evs.last() {
        Some(AdapterEvent::Exited { stderr_tail, .. }) => {
            assert!(
                stderr_tail.contains("Authentication required"),
                "{stderr_tail}"
            );
        }
        _ => unreachable!(),
    }
}

#[tokio::test]
async fn stop_kills_a_runtime_that_ignores_stdin_eof() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("child.pid");
    let bin = script(
        dir.path(),
        &format!(
            "trap '' TERM; sleep 300 & echo $! > {}; while :; do sleep 1; done",
            pidfile.display()
        ),
    );
    let (h, mut rx) = start(&bin, SpawnConfig::default(), None);
    let child: u32 = timeout(WAIT, async {
        loop {
            let text = std::fs::read_to_string(&pidfile).unwrap_or_default();
            if let Ok(pid) = text.trim().parse() {
                return pid;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    h.stop().unwrap();
    let (code, _) = exited(&mut rx).await;
    assert_eq!(code, None, "killed by a signal");
    let gone = timeout(WAIT, async {
        loop {
            if gone_or_zombie(child) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(gone.is_ok(), "background child {child} survived stop");
}

#[tokio::test]
async fn missing_binary_or_mcp_file_fails_spawn() {
    let err = GrokBuildAdapter::with_bin("/nonexistent/grok")
        .spawn(SpawnConfig::default(), None)
        .unwrap_err();
    assert!(format!("{err:#}").contains("/nonexistent/grok"));
    let err = GrokBuildAdapter::with_bin("/bin/true")
        .spawn(
            SpawnConfig {
                mcp_config_file: Some("/nonexistent/mcp.json".into()),
                ..Default::default()
            },
            None,
        )
        .unwrap_err();
    assert!(format!("{err:#}").contains("/nonexistent/mcp.json"));
}

// ---------------------------------------------------------------------------------------------
// Real grok (ignored: needs ~/.grok/bin/grok; run with `--ignored`)
// ---------------------------------------------------------------------------------------------

fn real_grok() -> Option<PathBuf> {
    std::env::var_os(grok_build::BIN_ENV)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".grok/bin/grok")))
        .filter(|p| p.exists())
}

/// Against a grok that is not signed in (an empty `GROK_HOME`): the turn ends with a clear
/// error and the process exits, with no hang.
#[tokio::test]
#[ignore]
async fn real_grok_without_auth_ends_the_turn_with_an_error() {
    let Some(bin) = real_grok() else {
        eprintln!("no grok binary; skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let (_h, mut rx) = start(
        &bin,
        SpawnConfig {
            cwd: dir.path().to_path_buf(),
            env: vec![(
                "GROK_HOME".into(),
                home.path().to_string_lossy().into_owned(),
            )],
            ..Default::default()
        },
        Some("hello"),
    );
    let evs = until(&mut rx, |e| matches!(e, AdapterEvent::Exited { .. })).await;
    let ended: Vec<&TurnEnded> = evs
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::Emit {
                body: EventBody::TurnEnded(t),
                ..
            } => Some(t),
            _ => None,
        })
        .collect();
    assert_eq!(ended.len(), 1, "{evs:?}");
    assert_eq!(ended[0].result, TurnResult::Error);
    let msg = grok_ext(&ended[0].ext)["error"]["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(msg.contains("Authentication required"), "{msg}");
    eprintln!("turn.ended error: {msg}");
}

/// Against a SIGNED-IN grok (spends one small prompt): a write asks for permission through
/// twr even if the user's config says always-approve, allow runs it, and the turn completes.
/// Set `TWR_GROK_MODEL` to pick the model (default: the fast one).
#[tokio::test]
#[ignore]
async fn real_grok_permission_round_trip() {
    let Some(bin) = real_grok() else {
        eprintln!("no grok binary; skipping");
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let st = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir.path())
        .status()
        .unwrap();
    assert!(st.success());
    let model = std::env::var("TWR_GROK_MODEL").unwrap_or_else(|_| FAST.into());
    let (h, mut rx) = start(
        &bin,
        SpawnConfig {
            cwd: dir.path().to_path_buf(),
            model: Some(model),
            ..Default::default()
        },
        Some("Create a file named hello.txt containing exactly: hi\nUse your file-writing tool. Do nothing else."),
    );
    let evs = timeout(Duration::from_secs(180), async {
        let mut seen = Vec::new();
        loop {
            let ev = rx.recv().await.expect("adapter channel closed");
            let done = is_kind(&ev, "attention.raised") || is_kind(&ev, "turn.ended");
            seen.push(ev);
            if done {
                return seen;
            }
        }
    })
    .await
    .expect("no permission request");
    let attn = match evs.last() {
        Some(AdapterEvent::Emit {
            body: EventBody::AttentionRaised(a),
            ..
        }) => a.attn_id.clone(),
        other => panic!("expected a permission request, got {other:?}"),
    };
    h.respond(&attn, Answer::allow()).unwrap();
    let evs = timeout(Duration::from_secs(180), async {
        let mut seen = Vec::new();
        loop {
            let ev = rx.recv().await.expect("adapter channel closed");
            let done = is_kind(&ev, "turn.ended");
            seen.push(ev);
            if done {
                return seen;
            }
        }
    })
    .await
    .expect("the turn never ended");
    match evs.last() {
        Some(AdapterEvent::Emit {
            body: EventBody::TurnEnded(t),
            ..
        }) => assert_eq!(t.result, TurnResult::Completed, "{t:?}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        std::fs::read_to_string(dir.path().join("hello.txt"))
            .unwrap()
            .trim(),
        "hi"
    );
    h.stop().unwrap();
    exited(&mut rx).await;
}

/// Whether process `pid` is gone or a zombie (read with `ps`, which works without `/proc`).
fn gone_or_zombie(pid: u32) -> bool {
    let out = std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let stat = String::from_utf8_lossy(&out.stdout);
    stat.trim().is_empty() || stat.trim_start().starts_with('Z')
}

/// A session id that would read as an option gets no attach command.
#[test]
fn a_session_id_that_reads_as_an_option_gets_no_attach_command() {
    let cfg = SpawnConfig {
        cwd: "/tmp/wt".into(),
        ..Default::default()
    };
    let a = GrokBuildAdapter::with_bin("/opt/grok");
    for bad in ["--yolo", "", "a b"] {
        assert!(a.attach_command(&cfg, bad).is_none(), "{bad:?}");
    }
    assert!(a.attach_command(&cfg, "sess-1").is_some());
}
