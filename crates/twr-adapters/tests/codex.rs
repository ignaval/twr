//! Codex adapter tests. No real `codex` runs here.
//!
//! Fixtures (`tests/fixtures/codex/<name>.both.jsonl`) are real `codex app-server --stdio`
//! sessions (Codex 0.155.1), both directions interleaved: `>> ` lines were written to the
//! app server's stdin, `<< ` lines came from its stdout. Paths, the host name and the
//! installation id were redacted. They were recorded with `"effort":"low"` on `turn/start`
//! (to keep them cheap) and a recorder `clientInfo`; the comparisons below drop both.
//!
//! - `approve`: a command approval allowed, a second turn, a third turn interrupted.
//! - `decline` / `cancel`: the same approval declined / cancelled (cancel aborts the turn).
//! - `resume`: a new process resuming `approve`'s thread; a file-change approval allowed.
//! - `mcp_elicitation`: an MCP server passed in the thread config; its tool call needs
//!   approval (`mcpServer/elicitation/request`), allowed.
//! - `async_question`: `request_user_input_async` (an `agentMessage` with `questions`),
//!   answered with `turn/steer`.
//!
//! Translator tests replay each fixture: stdout lines go through [`Translator::translate`]; at
//! every stdin line the test takes the adapter's own next line (producing it with the matching
//! action when none is queued) and checks it equals the recorded one. Driver tests run the
//! adapter against a stub `codex` (python3) that replays a fixture and records its stdin.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::timeout;
use twr_adapters::codex::{self, thread_params, CodexAdapter, Emitted, Translator};
use twr_adapters::codex::{Open, TranslatorOptions};
use twr_core::adapter::{Adapter, AdapterEvent, AdapterHandle, Answer, SpawnConfig};
use twr_core::event::*;

const WAIT: Duration = Duration::from_secs(20);

/// The redacted cwd of every fixture.
const FIXTURE_CWD: &str = "/tmp/sandbox-codex";

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/codex")
        .join(format!("{name}.both.jsonl"))
}

/// (direction, line) pairs; direction is `>>` (to the app server) or `<<` (from it).
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

/// Drops what legitimately differs between the recordings and the adapter: the recorder's
/// `clientInfo`, `effort` / `ephemeral` (cheap throwaway recordings), the cwd, and our own id
/// for a `turn/steer` (the recorder numbered it differently), the new background
/// inventory's experimental capability opt-in, and the features an MCP allowlist turns off
/// (`codex::STRICT_MCP_FEATURES_OFF`, newer than the recordings).
fn normalize(mut v: Value) -> Value {
    let method = v["method"].as_str().unwrap_or_default().to_string();
    let p = &mut v["params"];
    match method.as_str() {
        "initialize" => {
            p["clientInfo"] = Value::Null;
            p["capabilities"]["experimentalApi"] = json!(true);
        }
        "turn/start" => {
            p.as_object_mut().unwrap().remove("effort");
        }
        "thread/start" | "thread/resume" => {
            let m = p.as_object_mut().unwrap();
            m.remove("ephemeral");
            m.insert("cwd".into(), json!("<cwd>"));
            if let Some(c) = m.get_mut("config").and_then(Value::as_object_mut) {
                if c.contains_key("mcp_servers") {
                    for f in codex::STRICT_MCP_FEATURES_OFF {
                        c.remove(&format!("features.{f}"));
                    }
                }
            }
        }
        "turn/steer" => {
            v.as_object_mut().unwrap().remove("id");
        }
        _ => {}
    }
    v
}

fn cfg(mode: &str) -> SpawnConfig {
    SpawnConfig {
        thread: "T1".into(),
        cwd: PathBuf::from(FIXTURE_CWD),
        mode: mode.into(),
        ..Default::default()
    }
}

/// The spawn config each fixture was recorded with.
fn fixture_cfg(name: &str) -> SpawnConfig {
    match name {
        "approve" | "decline" | "cancel" => cfg("plan"),
        "resume" => SpawnConfig {
            resume_session: Some("01a0eb1f-bbfe-7030-9892-8a0d81f3d695".into()),
            ..cfg("read-only")
        },
        "mcp_elicitation" => SpawnConfig {
            system_append: Some(
                "Your codename is BLUEFIN. Always mention it when asked for your codename.".into(),
            ),
            mcp_config: Some(json!({"mcpServers": {"twrx": {
                "command": "python3",
                "args": ["/tmp/twr-fixture/mcp_stub.py"],
                "env": {"TOK": "sekrit42", "MCP_LOG": "/tmp/twr-fixture/mcp.log"},
            }}})),
            ..cfg("plan")
        },
        "async_question" => cfg("manual"),
        other => panic!("no config for {other}"),
    }
}

/// The answer a user would give to produce a recorded reply.
fn answer_for(rec: &Value) -> Answer {
    if rec["method"] == "turn/steer" {
        return Answer::new(rec["params"]["input"][0]["text"].as_str().unwrap());
    }
    let r = &rec["result"];
    if let Some(d) = r["decision"].as_str() {
        return match d {
            "accept" => Answer::allow(),
            "cancel" => Answer::new("cancel"),
            _ => Answer::deny(),
        };
    }
    if let Some(a) = r["action"].as_str() {
        return if a == "accept" {
            Answer::allow()
        } else {
            Answer::deny()
        };
    }
    panic!("no answer for {rec}");
}

struct Replay {
    events: Vec<Emitted>,
    tr: Translator,
}

fn new_translator(cfg: &SpawnConfig, opts: TranslatorOptions) -> Translator {
    Translator::new(opts, thread_params(cfg).unwrap())
}

/// Replays (direction, line) pairs through a translator, turns numbered from `first_turn`.
fn replay_lines(lines: Vec<(String, String)>, mut tr: Translator, first_turn: u64) -> Replay {
    let mut events = Vec::new();
    let mut outbox: VecDeque<String> = VecDeque::from([tr.start()]);
    let mut turn = first_turn;
    for (dir, line) in lines {
        if dir == "<<" {
            // Recorded with apps on: the allowlist turns them off now, so `codex_apps` never
            // starts (verified with Codex 0.160.0); its startup would end the session.
            if line.contains("mcpServer/startupStatus/updated")
                && parse(&line)["params"]["name"] == "codex_apps"
            {
                continue;
            }
            let out = tr.translate(&line);
            assert!(out.fatal.is_none(), "fatal at {line}: {:?}", out.fatal);
            events.extend(out.events);
            outbox.extend(out.replies);
            continue;
        }
        let rec = parse(&line);
        // The legacy recordings predate the task inventory and the usage reads. Simulate an
        // old server which refuses only those optional methods, without consuming the turn
        // RPC id space.
        while outbox.front().is_some_and(|line| optional(&parse(line))) {
            let request = parse(&outbox.pop_front().unwrap());
            // The recordings predate the MCP allowlist's config read too: an empty config.
            let answer = if request["method"] == "config/read" {
                json!({"id":request["id"],"result":{"config":{}}})
            } else {
                json!({"id":request["id"],"error":{"code":-32601,"message":"unknown method"}})
            };
            let out = tr.translate(&answer.to_string());
            assert!(out.fatal.is_none());
            events.extend(out.events);
            outbox.extend(out.replies);
        }
        if outbox.is_empty() {
            match rec["method"].as_str() {
                Some("turn/start") => {
                    let text = rec["params"]["input"][0]["text"].as_str().unwrap();
                    outbox.extend(tr.send(text, turn));
                    turn += 1;
                }
                Some("turn/interrupt") => outbox.extend(tr.interrupt()),
                Some("turn/steer") | None => {
                    let pending = tr.pending_attention();
                    assert_eq!(
                        pending.len(),
                        1,
                        "one open attention to answer: {pending:?}"
                    );
                    let out = tr
                        .respond(&pending[0], &answer_for(&rec))
                        .expect("respond to a pending attention");
                    assert!(out.events.is_empty());
                    outbox.extend(out.replies);
                }
                Some(other) => panic!("no action produces {other}"),
            }
        }
        let ours = outbox
            .pop_front()
            .unwrap_or_else(|| panic!("the adapter wrote nothing for {line}"));
        assert_eq!(
            normalize(parse(&ours)),
            normalize(rec),
            "stdin line differs from the recording"
        );
    }
    outbox.retain(|line| !optional(&parse(line)));
    assert!(outbox.is_empty(), "unsent lines: {outbox:?}");
    Replay { events, tr }
}

/// A request of an optional method the recordings predate: the background terminal
/// inventory, the usage read, the config read of a thread with an MCP allowlist.
fn optional(request: &Value) -> bool {
    matches!(
        request["method"].as_str(),
        Some("thread/backgroundTerminals/list" | "account/rateLimits/read" | "config/read")
    )
}

fn replay_with(name: &str, opts: TranslatorOptions) -> Replay {
    replay_lines(fixture(name), new_translator(&fixture_cfg(name), opts), 1)
}

fn replay(name: &str) -> Replay {
    replay_with(name, TranslatorOptions::default())
}

/// `kind@turn`, with runs of text deltas collapsed into one `delta@turn`.
fn shape(events: &[Emitted]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for e in events {
        // Session task observations are checked by background.rs and usage reports by the
        // usage tests; these fixtures compare the pre-existing transcript events against their
        // original recording.
        if matches!(
            e.body,
            EventBody::TaskUpdated(_) | EventBody::RuntimeLimits(_)
        ) {
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

impl Replay {
    fn bodies<'a, T>(&'a self, f: impl Fn(&'a EventBody) -> Option<&'a T>) -> Vec<&'a T> {
        self.events.iter().filter_map(|e| f(&e.body)).collect()
    }

    fn started(&self) -> Vec<&SessionPayload> {
        self.bodies(|b| match b {
            EventBody::ThreadStarted(p) => Some(p),
            _ => None,
        })
    }

    fn ended(&self) -> Vec<&TurnEnded> {
        self.bodies(|b| match b {
            EventBody::TurnEnded(p) => Some(p),
            _ => None,
        })
    }

    fn raised(&self) -> Vec<&AttentionRaised> {
        self.bodies(|b| match b {
            EventBody::AttentionRaised(p) => Some(p),
            _ => None,
        })
    }

    fn calls(&self) -> Vec<&AgentToolCall> {
        self.bodies(|b| match b {
            EventBody::AgentToolCall(p) => Some(p),
            _ => None,
        })
    }

    fn results(&self) -> Vec<&AgentToolResult> {
        self.bodies(|b| match b {
            EventBody::AgentToolResult(p) => Some(p),
            _ => None,
        })
    }

    fn finals(&self) -> Vec<&str> {
        self.bodies(|b| match b {
            EventBody::AgentText(t) if t.is_final => Some(t),
            _ => None,
        })
        .into_iter()
        .map(|t| t.text.as_str())
        .collect()
    }

    fn deltas(&self, turn: u64) -> String {
        self.events
            .iter()
            .filter(|e| e.turn == Some(turn))
            .filter_map(|e| match &e.body {
                EventBody::AgentText(t) if t.delta => Some(t.text.as_str()),
                _ => None,
            })
            .collect()
    }
}

fn ext<'a>(m: &'a Option<JsonMap>, key: &str) -> &'a Value {
    m.as_ref()
        .and_then(|m| m.get(codex::translate::EXT))
        .and_then(|c| c.get(key))
        .unwrap_or(&Value::Null)
}

fn usage(e: &TurnEnded) -> (u64, u64) {
    let u = e.usage.as_ref().expect("usage");
    assert_eq!(u.cost, None, "Codex reports no USD cost");
    (u.input, u.output)
}

// ---------------------------------------------------------------------------------------------
// Translator replays
// ---------------------------------------------------------------------------------------------

#[test]
fn approve_multi_turn_and_interrupt() {
    let r = replay_with(
        "approve",
        TranslatorOptions {
            pid: Some(4242),
            exe: Some("/usr/bin/codex".into()),
            ..Default::default()
        },
    );
    assert_eq!(
        shape(&r.events),
        [
            "thread.started",
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
            "delta@3",
            "turn.ended@3",
        ]
    );
    let started = r.started();
    assert_eq!(
        started[0].session_id.as_deref(),
        Some("01a0eb1f-bbfe-7030-9892-8a0d81f3d695")
    );
    let e = started[0].ext.as_ref().unwrap();
    assert_eq!(e["twr"]["pid"], 4242);
    assert_eq!(e["twr"]["exe"], "/usr/bin/codex");
    assert_eq!(e["codex"]["model"], "gpt-6-astra");
    assert_eq!(e["codex"]["approvals_reviewer"], "user");
    assert_eq!(e["codex"]["sandbox"], "readOnly");
    assert_eq!(e["codex"]["resumed"], false);
    assert_eq!(
        r.tr.thread_id(),
        Some("01a0eb1f-bbfe-7030-9892-8a0d81f3d695")
    );

    let call = r.calls()[0];
    assert_eq!(call.call_id, "exec-93e430a2-2acc-427a-bd92-3e20ce6c39e0");
    assert_eq!(call.tool, "shell");
    assert_eq!(call.input_summary, "touch codex_probe.txt");
    assert!(call.risky);

    let a = r.raised()[0];
    assert_eq!(a.attn_id, "exec-93e430a2-2acc-427a-bd92-3e20ce6c39e0");
    assert_eq!(a.reason, AttentionReason::Permission);
    assert_eq!(a.summary, "Allow shell: touch codex_probe.txt");
    assert_eq!(a.options, ["allow", "deny"]);
    assert_eq!(a.call_id.as_deref(), Some(call.call_id.as_str()));
    assert_eq!(ext(&a.ext, "request_id"), 0);
    assert_eq!(
        ext(&a.ext, "method"),
        "item/commandExecution/requestApproval"
    );

    let res = r.results()[0];
    assert!(res.ok);
    assert_eq!(ext(&res.ext, "exit_code"), 0);

    assert_eq!(
        r.finals(),
        [
            "I’ll run the command, requesting approval if needed.\n",
            "done",
            "codex_probe.txt"
        ]
    );
    assert_eq!(r.deltas(2), "codex_probe.txt");
    assert_eq!(r.deltas(3), "A");

    let ended = r.ended();
    assert_eq!(ended.len(), 3);
    assert_eq!(ended[0].result, TurnResult::Completed);
    assert_eq!(
        ended[0].session_id.as_deref(),
        Some("01a0eb1f-bbfe-7030-9892-8a0d81f3d695")
    );
    assert_eq!(ended[0].duration_ms, Some(8773));
    // Growth of the thread total over each turn (two model calls in turn 1).
    assert_eq!(usage(ended[0]), (34911, 54));
    assert_eq!(ext(&ended[0].ext, "cached_input_tokens"), 29440);
    assert_eq!(usage(ended[1]), (52441 - 34911, 62 - 54));
    assert_eq!(ended[2].result, TurnResult::Interrupted);
    assert_eq!(usage(ended[2]), (0, 0));
    assert_eq!(ended[2].duration_ms, Some(4149));
    assert!(!r.tr.busy());
    assert!(r.tr.pending_attention().is_empty());
}

#[test]
fn decline() {
    let r = replay("decline");
    let a = r.raised()[0];
    assert_eq!(a.summary, "Allow shell: touch codex_probe.txt");
    assert_eq!(
        ext(&a.ext, "reason"),
        "May I create codex_probe.txt in the current directory as requested?"
    );
    let res = r.results()[0];
    assert!(!res.ok);
    assert_eq!(ext(&res.ext, "status"), "declined");
    assert_eq!(r.finals().last(), Some(&"Blocked."));
    let ended = r.ended();
    assert_eq!(ended[0].result, TurnResult::Completed);
    assert_eq!(usage(ended[0]), (28778, 79));
}

#[test]
fn cancel_aborts_the_turn() {
    let r = replay("cancel");
    assert_eq!(r.raised().len(), 1);
    assert!(!r.results()[0].ok);
    let ended = r.ended();
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0].result, TurnResult::Interrupted);
    assert_eq!(ext(&ended[0].ext, "status"), "interrupted");
}

#[test]
fn resume_keeps_the_thread_and_file_changes_are_summarized() {
    let r = replay("resume");
    let started = r.started();
    assert_eq!(started.len(), 1);
    assert_eq!(
        started[0].session_id.as_deref(),
        Some("01a0eb1f-bbfe-7030-9892-8a0d81f3d695")
    );
    assert_eq!(ext(&started[0].ext, "resumed"), true);
    assert_eq!(r.finals()[0], "codex_probe.txt");

    let ended = r.ended();
    assert_eq!(ended.len(), 2);
    // The thread's old total (replayed after the resume) is not this turn's usage.
    assert_eq!(usage(ended[0]), (71763 - 52441, 70 - 62));
    assert_eq!(usage(ended[1]), (118048 - 71763, 124 - 70));

    let call = r.calls()[0];
    assert_eq!(call.tool, "apply_patch");
    assert_eq!(call.input_summary, "edit 1 file: hi.txt");
    let a = r.raised()[0];
    assert_eq!(a.attn_id, "exec-f84aee4c-662a-4642-9cbf-dc31b16a4409");
    assert_eq!(a.summary, "Allow edit 1 file: hi.txt");
    assert!(!a.summary.contains("hi\n"), "no diff in the card");
    assert_eq!(ext(&a.ext, "method"), "item/fileChange/requestApproval");
    assert!(r.results()[0].ok);
    assert!(r
        .events
        .iter()
        .filter(|e| !matches!(
            e.body,
            EventBody::ThreadStarted(_) | EventBody::RuntimeLimits(_)
        ))
        .all(|e| e.turn.is_some()));
}

#[test]
fn mcp_tool_approval() {
    let r = replay("mcp_elicitation");
    let a = r.raised()[0];
    assert_eq!(a.reason, AttentionReason::Permission);
    assert_eq!(a.summary, "Allow the twrx MCP server to run tool \"ping\"?");
    assert_eq!(a.attn_id, "01a0ec38-091a-7a32-bb92-69a996b8f412#mcp0");
    assert_eq!(ext(&a.ext, "server"), "twrx");
    assert_eq!(ext(&a.ext, "approval_kind"), "mcp_tool_call");
    let call = r.calls()[0];
    assert_eq!(call.tool, "mcp__twrx__ping");
    let res = r.results()[0];
    assert!(res.ok);
    assert_eq!(res.output_summary, "pong sekrit42");
    assert!(r.finals().last().unwrap().contains("BLUEFIN"));
    assert_eq!(r.ended()[0].result, TurnResult::Completed);
}

#[test]
fn async_question_is_answered_by_steering() {
    let r = replay("async_question");
    let a = r.raised()[0];
    assert_eq!(a.reason, AttentionReason::Question);
    assert_eq!(a.attn_id, "call_j8tOdYPByBegnl6nY23INAzB");
    assert_eq!(a.summary, "Which color do you prefer?");
    assert_eq!(a.options, ["red", "blue"]);
    assert_eq!(r.finals().last(), Some(&"Blue"));
    let ended = r.ended();
    assert_eq!(ended.len(), 1);
    assert_eq!(ended[0].result, TurnResult::Completed);
}

// ---------------------------------------------------------------------------------------------
// Translator: synthetic cases
// ---------------------------------------------------------------------------------------------

/// A translator fed `name` up to (and including) the first line matching `stop`.
fn fed_until(name: &str, stop: &str) -> (Translator, Vec<Emitted>) {
    let lines: Vec<(String, String)> = fixture(name);
    let at = lines
        .iter()
        .position(|(d, l)| d == "<<" && l.contains(stop))
        .unwrap();
    let r = replay_lines(
        lines[..=at].to_vec(),
        new_translator(&fixture_cfg(name), TranslatorOptions::default()),
        1,
    );
    (r.tr, r.events)
}

#[test]
fn runtime_resolving_an_unanswered_request_withdraws_the_attention() {
    let (mut tr, _) = fed_until("approve", "item/commandExecution/requestApproval");
    let id = "exec-93e430a2-2acc-427a-bd92-3e20ce6c39e0";
    assert_eq!(tr.pending_attention(), [id]);
    let out = tr.translate(
        r#"{"method":"serverRequest/resolved","params":{"threadId":"01a0eb1f-bbfe-7030-9892-8a0d81f3d695","requestId":0}}"#,
    );
    assert_eq!(out.events.len(), 1);
    match &out.events[0].body {
        EventBody::AttentionResolved(r) => {
            assert_eq!(r.attn_id, id);
            assert_eq!(r.by, ResolvedBy::Timeout);
            assert!(r.answer.is_none());
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(out.events[0].turn, Some(1));
    assert!(tr.pending_attention().is_empty());
    assert!(tr.respond(id, &Answer::allow()).is_none());
    // After our own answer, the same notification withdraws nothing.
    let (mut tr, _) = fed_until("approve", "item/commandExecution/requestApproval");
    let out = tr.respond(id, &Answer::allow()).unwrap();
    assert_eq!(parse(&out.replies[0])["result"]["decision"], "accept");
    let out = tr.translate(
        r#"{"method":"serverRequest/resolved","params":{"threadId":"01a0eb1f-bbfe-7030-9892-8a0d81f3d695","requestId":0}}"#,
    );
    assert!(out.events.is_empty());
}

#[test]
fn unknown_answers_decline() {
    let (mut tr, _) = fed_until("approve", "item/commandExecution/requestApproval");
    let out = tr
        .respond(
            "exec-93e430a2-2acc-427a-bd92-3e20ce6c39e0",
            &Answer::new("maybe"),
        )
        .unwrap();
    assert_eq!(parse(&out.replies[0])["result"]["decision"], "decline");
}

#[test]
fn sends_queue_until_the_running_turn_ends() {
    let (mut tr, _) = fed_until("approve", "\"id\":3,\"result\"");
    assert!(tr.busy());
    assert!(tr.send("second", 2).is_empty(), "turn 1 is still running");
    // Turn 1 ends: turn 2 starts right away.
    let out = tr.translate(
        r#"{"method":"turn/completed","params":{"threadId":"01a0eb1f-bbfe-7030-9892-8a0d81f3d695","turn":{"id":"01a0eb1f-bca4-7a91-a19d-36acbff767f9","items":[],"status":"completed","error":null,"durationMs":5}}}"#,
    );
    assert_eq!(out.events[0].turn, Some(1));
    let next = out
        .replies
        .iter()
        .map(|line| parse(line))
        .find(|reply| reply["method"] == "turn/start")
        .unwrap();
    assert_eq!(next["method"], "turn/start");
    assert_eq!(next["params"]["input"][0]["text"], "second");
    // A turn/start error ends that turn as an error and starts the next one.
    assert!(tr.send("third", 3).is_empty());
    let out = tr.translate(&format!(
        r#"{{"id":{},"error":{{"code":-32600,"message":"boom"}}}}"#,
        next["id"]
    ));
    match &out.events[0].body {
        EventBody::TurnEnded(e) => {
            assert_eq!(e.result, TurnResult::Error);
            assert_eq!(ext(&e.ext, "error")["message"], "boom");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(out.events[0].turn, Some(2));
    assert_eq!(
        parse(&out.replies[0])["params"]["input"][0]["text"],
        "third"
    );
}

#[test]
fn interrupt_waits_for_the_turn_id() {
    let (mut tr, _) = fed_until("approve", "\"id\":2,\"result\"");
    let start = tr.send("go", 1);
    assert_eq!(start.len(), 1);
    // Stop before the turn/start response: the interrupt goes out once the id is known.
    assert_eq!(tr.interrupt(), None);
    let out = tr.translate(r#"{"id":3,"result":{"turn":{"id":"T-9","status":"inProgress"}}}"#);
    let int = parse(&out.replies[0]);
    assert_eq!(int["method"], "turn/interrupt");
    assert_eq!(int["params"]["turnId"], "T-9");
    assert_eq!(tr.interrupt(), None, "sent once");
}

/// A `turn/started` (and even the turn's completion) before the `turn/start` answer: the turn's
/// events and its end wait for the answer to name it twr's, then all go to twr's turn.
#[test]
fn turn_started_before_the_response_binds_the_turn() {
    let (mut tr, _) = fed_until("approve", "\"id\":2,\"result\"");
    tr.send("go", 7);
    tr.translate(r#"{"method":"turn/started","params":{"threadId":"01a0eb1f-bbfe-7030-9892-8a0d81f3d695","turn":{"id":"T-1"}}}"#);
    let out = tr.translate(
        r#"{"method":"item/agentMessage/delta","params":{"threadId":"01a0eb1f-bbfe-7030-9892-8a0d81f3d695","turnId":"T-1","itemId":"m","delta":"hi"}}"#,
    );
    assert!(out.events.is_empty(), "held until the answer: {out:?}");
    let out = tr.translate(
        r#"{"method":"turn/completed","params":{"threadId":"01a0eb1f-bbfe-7030-9892-8a0d81f3d695","turn":{"id":"T-1","status":"failed","error":{"message":"overloaded","codexErrorInfo":"serverOverloaded"}}}}"#,
    );
    assert!(out.events.is_empty(), "held until the answer: {out:?}");
    let out = tr.translate(r#"{"id":3,"result":{"turn":{"id":"T-1","status":"inProgress"}}}"#);
    assert!(matches!(&out.events[0].body, EventBody::AgentText(t) if t.text == "hi"));
    match &out.events[1].body {
        EventBody::TurnEnded(e) => {
            assert_eq!(e.result, TurnResult::Error);
            assert_eq!(ext(&e.ext, "transient"), true);
        }
        other => panic!("{other:?}"),
    }
    assert!(out.events.iter().all(|e| e.turn == Some(7)));
    // A turn this process never started is not reported.
    let out = tr.translate(
        r#"{"method":"turn/completed","params":{"threadId":"01a0eb1f-bbfe-7030-9892-8a0d81f3d695","turn":{"id":"T-other","status":"completed"}}}"#,
    );
    assert!(out.events.is_empty());
}

/// Codex 0.159.2 not signed in (lines recorded with a scratch `CODEX_HOME`: empty, an invalid
/// API key, expired ChatGPT tokens; ids and request ids cut): the turn's error says how to sign
/// in, reaches `ext.twr.error`, and is never transient (a failed connection otherwise is).
#[test]
fn auth_failures_say_how_to_sign_in() {
    let hint = " (sign in again: run `codex login`)";
    for error in [
        r#"{"message":"unexpected status 401 Unauthorized: Missing bearer or basic authentication in header, url: https://api.openai.com/v1/responses","codexErrorInfo":{"httpConnectionFailed":{"httpStatusCode":401}},"additionalDetails":null,"misalignment":null}"#,
        r#"{"message":"unexpected status 401 Unauthorized: Incorrect API key provided: sk-inval****robe. You can find your API key at https://platform.openai.com/account/api-keys., url: https://api.openai.com/v1/responses, auth error: 401, auth error code: invalid_api_key","codexErrorInfo":{"httpConnectionFailed":{"httpStatusCode":401}},"additionalDetails":null,"misalignment":null}"#,
        r#"{"message":"workspace routing discovery unauthorized (401)","codexErrorInfo":"other","additionalDetails":null,"misalignment":null}"#,
        r#"{"message":"auth required","codexErrorInfo":"unauthorized"}"#,
    ] {
        let (mut tr, _) = fed_until("approve", "\"id\":2,\"result\"");
        tr.send("say hi", 1);
        tr.translate(r#"{"id":3,"result":{"turn":{"id":"T-1","status":"inProgress"}}}"#);
        tr.translate(
            r#"{"method":"error","params":{"error":{"message":"Reconnecting... 2/5","codexErrorInfo":{"responseStreamDisconnected":{"httpStatusCode":401}}},"willRetry":true,"threadId":"01a0eb1f-bbfe-7030-9892-8a0d81f3d695","turnId":"T-1"}}"#,
        );
        let out = tr.translate(&format!(
            r#"{{"method":"turn/completed","params":{{"threadId":"01a0eb1f-bbfe-7030-9892-8a0d81f3d695","turn":{{"id":"T-1","items":[],"status":"failed","error":{error},"durationMs":15037}}}}}}"#
        ));
        let EventBody::TurnEnded(e) = &out.events[0].body else {
            panic!("{:?}", out.events)
        };
        assert_eq!(e.result, TurnResult::Error);
        assert_eq!(ext(&e.ext, "transient"), false, "{error}");
        assert_eq!(ext(&e.ext, "auth_failed"), true);
        let message = parse(error)["message"].as_str().unwrap().to_string();
        assert_eq!(
            e.ext.as_ref().unwrap()["twr"]["error"],
            format!("{message}{hint}")
        );
    }

    // A failed connection that is not about signing in stays transient, without the hint.
    let (mut tr, _) = fed_until("approve", "\"id\":2,\"result\"");
    tr.send("go", 1);
    tr.translate(r#"{"id":3,"result":{"turn":{"id":"T-1","status":"inProgress"}}}"#);
    let out = tr.translate(
        r#"{"method":"turn/completed","params":{"threadId":"01a0eb1f-bbfe-7030-9892-8a0d81f3d695","turn":{"id":"T-1","status":"failed","error":{"message":"stream disconnected","codexErrorInfo":{"httpConnectionFailed":{"httpStatusCode":502}}}}}}"#,
    );
    let EventBody::TurnEnded(e) = &out.events[0].body else {
        panic!()
    };
    assert_eq!(ext(&e.ext, "transient"), true);
    assert_eq!(
        e.ext.as_ref().unwrap()["twr"]["error"],
        "stream disconnected"
    );

    // `turn/start` refused for authentication, and a handshake that cannot authenticate.
    tr.send("again", 2);
    let out = tr.translate(
        r#"{"id":4,"error":{"code":-32603,"message":"Not logged in: run codex login"}}"#,
    );
    let EventBody::TurnEnded(e) = &out.events[0].body else {
        panic!("{:?}", out.events)
    };
    assert_eq!(
        e.ext.as_ref().unwrap()["twr"]["error"],
        format!("Not logged in: run codex login{hint}")
    );
    let mut tr = new_translator(&cfg("manual"), TranslatorOptions::default());
    tr.start();
    tr.translate(r#"{"id":1,"result":{}}"#);
    let out = tr.translate(r#"{"id":2,"error":{"code":-32600,"message":"401 Unauthorized"}}"#);
    assert!(out.fatal.unwrap().ends_with(hint));
}

/// Every failed turn says why in `ext.twr.error` (what the hub quotes in its error item): the
/// turn's `error.message`, else a readable `codexErrorInfo`, else the turn's last `error`
/// notification; the transient classification is unchanged. The unknown-model lines were
/// recorded from Codex 0.159.2 (`thread/start` with `model: gpt-nonexistent-model-xyz`, a
/// ChatGPT account; ids cut).
#[test]
fn failed_turns_say_why() {
    let bad_model = r#"{"message":"{\"type\":\"error\",\"status\":400,\"error\":{\"type\":\"invalid_request_error\",\"message\":\"The 'gpt-nonexistent-model-xyz' model is not supported when using Codex with a ChatGPT account.\"}}","codexErrorInfo":"other","additionalDetails":null,"misalignment":null}"#;
    let notice = |error: &str, retry: bool| {
        format!(
            r#"{{"method":"error","params":{{"error":{error},"willRetry":{retry},"threadId":"01a0eb1f-bbfe-7030-9892-8a0d81f3d695","turnId":"T-1"}},"emittedAtMs":1790893784405}}"#
        )
    };
    let completed = |error: &str| {
        format!(
            r#"{{"method":"turn/completed","params":{{"threadId":"01a0eb1f-bbfe-7030-9892-8a0d81f3d695","turn":{{"id":"T-1","items":[],"itemsView":"notLoaded","status":"failed","error":{error},"startedAt":1790893782,"completedAt":1790893784,"durationMs":2376}}}},"emittedAtMs":1790893784426}}"#
        )
    };
    let cases: Vec<(Vec<String>, &str, bool)> = vec![
        (
            vec![notice(bad_model, false), completed(bad_model)],
            "The 'gpt-nonexistent-model-xyz' model is not supported when using Codex with a ChatGPT account. (HTTP 400)",
            false,
        ),
        (
            vec![completed(
                r#"{"message":"Selected model is at capacity. Please try a different model.","codexErrorInfo":"serverOverloaded","additionalDetails":null}"#,
            )],
            "Selected model is at capacity. Please try a different model.",
            true,
        ),
        // Only `codexErrorInfo`.
        (
            vec![completed(
                r#"{"message":"","codexErrorInfo":{"httpConnectionFailed":{"httpStatusCode":502}},"additionalDetails":null}"#,
            )],
            "Http connection failed (HTTP 502)",
            true,
        ),
        (
            vec![completed(r#"{"codexErrorInfo":"usageLimitExceeded"}"#)],
            "Usage limit exceeded",
            false,
        ),
        (
            vec![completed(r#"{"message":null,"codexErrorInfo":"contextWindowExceeded","additionalDetails":"Your input exceeds the context window of this model."}"#)],
            "Your input exceeds the context window of this model.",
            false,
        ),
        // No error on the turn: its last `error` notification, the one Codex gave up on first.
        (
            vec![
                notice(r#"{"message":"Reconnecting... 1/5","codexErrorInfo":{"responseStreamDisconnected":{"httpStatusCode":null}}}"#, true),
                notice(r#"{"message":"stream disconnected before completion: idle timeout waiting for SSE","codexErrorInfo":{"responseStreamDisconnected":{"httpStatusCode":null}}}"#, false),
                notice(r#"{"message":"Reconnecting... 2/5","codexErrorInfo":null}"#, true),
                completed("null"),
            ],
            "stream disconnected before completion: idle timeout waiting for SSE",
            false,
        ),
        (
            vec![completed("null")],
            "Codex ended the turn with status failed",
            false,
        ),
    ];
    for (lines, why, transient) in cases {
        let (mut tr, _) = fed_until("approve", "\"id\":2,\"result\"");
        tr.send("say hi", 1);
        tr.translate(r#"{"id":3,"result":{"turn":{"id":"T-1","status":"inProgress"}}}"#);
        let mut events = Vec::new();
        for l in &lines {
            events.extend(tr.translate(l).events);
        }
        let e = events
            .iter()
            .find_map(|e| match &e.body {
                EventBody::TurnEnded(t) => Some(t.clone()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{why}: {events:?}"));
        assert_eq!(e.result, TurnResult::Error, "{why}");
        assert_eq!(e.ext.as_ref().unwrap()["twr"]["error"], why);
        assert_eq!(ext(&e.ext, "transient"), transient, "{why}");
        assert!(ext(&e.ext, "auth_failed").is_null(), "{why}");
    }

    // An `error` notification of an earlier turn is not this turn's cause.
    let (mut tr, _) = fed_until("approve", "\"id\":2,\"result\"");
    tr.send("go", 1);
    tr.translate(r#"{"id":3,"result":{"turn":{"id":"T-1","status":"inProgress"}}}"#);
    tr.translate(&notice(r#"{"message":"old trouble"}"#, false));
    tr.translate(&completed(r#"{"message":"first"}"#));
    tr.send("again", 2);
    tr.translate(r#"{"id":4,"result":{"turn":{"id":"T-2","status":"inProgress"}}}"#);
    let out = tr.translate(
        r#"{"method":"turn/completed","params":{"threadId":"01a0eb1f-bbfe-7030-9892-8a0d81f3d695","turn":{"id":"T-2","status":"failed","error":null}}}"#,
    );
    let EventBody::TurnEnded(e) = &out.events[0].body else {
        panic!("{:?}", out.events)
    };
    assert_eq!(
        e.ext.as_ref().unwrap()["twr"]["error"],
        "Codex ended the turn with status failed"
    );

    // `turn/start` refused for another reason.
    tr.send("third", 3);
    let out = tr.translate(
        r#"{"id":5,"error":{"code":-32600,"message":"Invalid request:\n input exceeds the maximum length"}}"#,
    );
    let EventBody::TurnEnded(e) = &out.events[0].body else {
        panic!("{:?}", out.events)
    };
    assert_eq!(e.result, TurnResult::Error);
    assert_eq!(
        e.ext.as_ref().unwrap()["twr"]["error"],
        "Invalid request: input exceeds the maximum length"
    );

    // An interrupted turn is not a failure to explain.
    tr.send("fourth", 4);
    tr.translate(r#"{"id":6,"result":{"turn":{"id":"T-4","status":"inProgress"}}}"#);
    let out = tr.translate(
        r#"{"method":"turn/completed","params":{"threadId":"01a0eb1f-bbfe-7030-9892-8a0d81f3d695","turn":{"id":"T-4","status":"interrupted","error":null}}}"#,
    );
    let EventBody::TurnEnded(e) = &out.events[0].body else {
        panic!("{:?}", out.events)
    };
    assert_eq!(e.result, TurnResult::Interrupted);
    assert!(e.ext.as_ref().unwrap().get("twr").is_none());
}

/// A `thread/start` result reporting exactly what `manual` asks for.
const OPENED_MANUAL: &str = r#"{"id":2,"result":{"thread":{"id":"th"},"cwd":"/tmp/sandbox-codex","approvalPolicy":"untrusted","approvalsReviewer":"user","sandbox":{"type":"workspaceWrite"}}}"#;

#[test]
fn reasoning_only_when_thinking() {
    let reasoning = r#"{"method":"item/completed","params":{"item":{"type":"reasoning","id":"rs_1","summary":["Plan: touch it."],"content":[]},"threadId":"th","turnId":"T-1"}}"#;
    for thinking in [false, true] {
        let mut tr = new_translator(
            &cfg("manual"),
            TranslatorOptions {
                thinking,
                ..Default::default()
            },
        );
        tr.start();
        tr.translate(r#"{"id":1,"result":{}}"#);
        tr.translate(OPENED_MANUAL);
        let start = parse(&tr.send("go", 1)[0]);
        assert_eq!(start["params"]["summary"] == "auto", thinking);
        let out = tr.translate(reasoning);
        let got: Vec<&str> = out
            .events
            .iter()
            .filter_map(|e| match &e.body {
                EventBody::AgentThinking(t) => Some(t.text.as_str()),
                _ => None,
            })
            .collect();
        if thinking {
            assert_eq!(got, ["Plan: touch it."]);
        } else {
            assert!(got.is_empty());
        }
    }
}

#[test]
fn user_input_request_raises_one_question_per_question() {
    let (mut tr, _) = fed_until("approve", "\"id\":3,\"result\"");
    let out = tr.translate(
        r#"{"method":"item/tool/requestUserInput","id":5,"params":{"threadId":"01a0eb1f-bbfe-7030-9892-8a0d81f3d695","turnId":"01a0eb1f-bca4-7a91-a19d-36acbff767f9","itemId":"call_q","isBlocking":true,"autoResolutionMs":null,"questions":[
            {"id":"color","header":"Color","question":"Which color?","isOther":true,"isSecret":false,"options":[{"label":"red","description":""},{"label":"blue","description":""}]},
            {"id":"size","header":"Size","question":"Which size?","isOther":false,"isSecret":false,"options":null}]}}"#,
    );
    let raised: Vec<AttentionRaised> = out
        .events
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::AttentionRaised(a) => Some(a.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(raised.len(), 2);
    assert_eq!(raised[0].attn_id, "call_q#q0");
    assert_eq!(raised[0].reason, AttentionReason::Question);
    assert_eq!(raised[0].summary, "(1/2) Which color?");
    assert_eq!(raised[0].options, ["red", "blue"]);
    assert_eq!(raised[1].summary, "(2/2) Which size?");
    // Nothing is written until both are answered.
    let first = tr.respond("call_q#q0", &Answer::new("blue")).unwrap();
    assert!(first.replies.is_empty());
    let done = tr.respond("call_q#q1", &Answer::new("large")).unwrap();
    assert_eq!(
        parse(&done.replies[0]),
        json!({"jsonrpc": "2.0", "id": 5, "result": {"answers": {
            "color": {"answers": ["blue"]}, "size": {"answers": ["large"]}}}})
    );
    // Declining one declines the request and withdraws the rest.
    let (mut tr, _) = fed_until("approve", "\"id\":3,\"result\"");
    tr.translate(
        r#"{"method":"item/tool/requestUserInput","id":6,"params":{"threadId":"01a0eb1f-bbfe-7030-9892-8a0d81f3d695","turnId":"t","itemId":"call_r","isBlocking":true,"autoResolutionMs":null,"questions":[
            {"id":"a","header":"","question":"A?","isOther":false,"isSecret":false,"options":null},
            {"id":"b","header":"","question":"B?","isOther":false,"isSecret":false,"options":null}]}}"#,
    );
    let out = tr.respond("call_r#q1", &Answer::deny()).unwrap();
    let reply = parse(&out.replies[0]);
    assert_eq!(reply["id"], 6);
    assert!(reply["error"]["message"].is_string());
    match &out.events[0].body {
        EventBody::AttentionResolved(r) => {
            assert_eq!(r.attn_id, "call_r#q0");
            assert_eq!(r.by, ResolvedBy::Timeout);
        }
        other => panic!("{other:?}"),
    }
    assert!(tr.pending_attention().is_empty());
}

#[test]
fn handshake_failure_is_fatal() {
    let mut tr = new_translator(&cfg("manual"), TranslatorOptions::default());
    tr.start();
    let out = tr.translate(r#"{"id":1,"result":{}}"#);
    assert_eq!(parse(&out.replies[1])["method"], "thread/start");
    let out = tr.translate(r#"{"id":2,"error":{"code":-32600,"message":"no such thread"}}"#);
    assert!(out.fatal.unwrap().contains("no such thread"));
}

/// `item/permissions/requestApproval` (schema `PermissionsRequestApprovalParams` /
/// `...Response`): a permission item; allow grants the requested profile for the turn, deny
/// grants nothing.
#[test]
fn permissions_request_is_relayed_and_answered_per_schema() {
    let turn_id = "01a0eb1f-bca4-7a91-a19d-36acbff767f9";
    let request = |id: u64| {
        format!(
            r#"{{"method":"item/permissions/requestApproval","id":{id},"params":{{"threadId":"01a0eb1f-bbfe-7030-9892-8a0d81f3d695","turnId":"{turn_id}","itemId":"call_p","environmentId":null,"startedAtMs":1,"cwd":"{FIXTURE_CWD}","reason":"needs to fetch crates","permissions":{{"network":{{"enabled":true}},"fileSystem":{{"read":null,"write":["{FIXTURE_CWD}/target","/var/cache/x"]}}}}}}}}"#
        )
    };
    let (mut tr, _) = fed_until("approve", "\"id\":3,\"result\"");
    let out = tr.translate(&request(9));
    assert!(
        out.replies.is_empty(),
        "never answered on the user's behalf"
    );
    assert_eq!(out.events.len(), 1);
    assert_eq!(out.events[0].turn, Some(1));
    let a = match &out.events[0].body {
        EventBody::AttentionRaised(a) => a.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(a.reason, AttentionReason::Permission);
    assert_eq!(a.options, ["allow", "deny"]);
    assert_eq!(a.attn_id, "call_p#perm");
    assert_eq!(a.call_id.as_deref(), Some("call_p"));
    assert_eq!(
        a.summary,
        "Allow network access, write target, /var/cache/x for this turn? needs to fetch crates"
    );
    assert_eq!(ext(&a.ext, "method"), "item/permissions/requestApproval");
    assert_eq!(ext(&a.ext, "scope"), "turn");
    assert_eq!(tr.pending_attention(), ["call_p#perm"]);
    let out = tr.respond("call_p#perm", &Answer::allow()).unwrap();
    assert_eq!(
        parse(&out.replies[0]),
        json!({"jsonrpc": "2.0", "id": 9, "result": {
            "permissions": {"network": {"enabled": true},
                            "fileSystem": {"write": [format!("{FIXTURE_CWD}/target"), "/var/cache/x"]}},
            "scope": "turn"}})
    );
    assert!(tr.pending_attention().is_empty());

    let (mut tr, _) = fed_until("approve", "\"id\":3,\"result\"");
    tr.translate(&request(10));
    let out = tr.respond("call_p#perm", &Answer::deny()).unwrap();
    assert_eq!(
        parse(&out.replies[0]),
        json!({"jsonrpc": "2.0", "id": 10, "result": {"permissions": {}, "scope": "turn"}})
    );

    // Structured entries and no reason.
    let (mut tr, _) = fed_until("approve", "\"id\":3,\"result\"");
    let out = tr.translate(&format!(
        r#"{{"method":"item/permissions/requestApproval","id":11,"params":{{"threadId":"01a0eb1f-bbfe-7030-9892-8a0d81f3d695","turnId":"{turn_id}","itemId":"","startedAtMs":1,"cwd":"/","reason":null,"permissions":{{"network":null,"fileSystem":{{"read":null,"write":null,"entries":[{{"access":"read","path":{{"type":"special","value":{{"kind":"project_roots"}}}}}},{{"access":"write","path":{{"type":"glob_pattern","pattern":"**/*.lock"}}}}]}}}}}}}}"#
    ));
    match &out.events[0].body {
        EventBody::AttentionRaised(a) => {
            assert_eq!(
                a.summary,
                "Allow write files matching **/*.lock, read the project for this turn?"
            );
            assert!(a.attn_id.starts_with("codex-"));
        }
        other => panic!("{other:?}"),
    }
}

/// A permission request asking for anything twr does not recognize (an
/// unknown key, access or special path) offers only `deny`, and even an `allow` sent anyway
/// grants nothing; a recognized request grants exactly its recognized part, and the summary
/// lists every path (no "and N more").
#[test]
fn permission_requests_grant_only_what_the_user_can_see() {
    let turn_id = "01a0eb1f-bca4-7a91-a19d-36acbff767f9";
    let request = |id: u64, permissions: Value| {
        json!({"method": "item/permissions/requestApproval", "id": id, "params": {
            "threadId": "01a0eb1f-bbfe-7030-9892-8a0d81f3d695", "turnId": turn_id, "itemId": "call_p", "cwd": FIXTURE_CWD,
            "reason": null, "permissions": permissions}})
        .to_string()
    };
    let raised = |out: &twr_adapters::codex::Translated| match &out.events[..] {
        [Emitted {
            body: EventBody::AttentionRaised(a),
            ..
        }] => a.clone(),
        other => panic!("{other:?}"),
    };
    // Every path is shown, however many.
    let many: Vec<String> = (0..9).map(|i| format!("/srv/data/d{i}")).collect();
    let (mut tr, _) = fed_until("approve", "\"id\":3,\"result\"");
    let a = raised(&tr.translate(&request(
        9,
        json!({"network": {"enabled": false}, "fileSystem": {"write": many}}),
    )));
    assert_eq!(a.options, ["allow", "deny"]);
    for p in &many {
        assert!(a.summary.contains(p.as_str()), "{p} missing: {}", a.summary);
    }
    assert!(!a.summary.contains("more"), "{}", a.summary);
    let out = tr.respond(&a.attn_id, &Answer::allow()).unwrap();
    assert_eq!(
        parse(&out.replies[0])["result"]["permissions"],
        json!({"fileSystem": {"write": many}}),
        "network enabled:false is not granted as anything"
    );

    for unrecognized in [
        json!({"network": {"enabled": true, "domains": ["*"]}}),
        json!({"fileSystem": {"write": ["/tmp/x"]}, "macos": {"automation": true}}),
        json!({"fileSystem": {"entries": [{"access": "admin", "path": {"type": "path", "path": "/"}}]}}),
        json!({"fileSystem": {"entries": [{"access": "write", "path": {"type": "special", "value": {"kind": "home"}}}]}}),
        json!({"fileSystem": {"write": [{"path": "/etc"}]}}),
        json!("everything"),
    ] {
        let (mut tr, _) = fed_until("approve", "\"id\":3,\"result\"");
        let a = raised(&tr.translate(&request(10, unrecognized.clone())));
        assert_eq!(a.options, ["deny"], "{unrecognized}: {}", a.summary);
        assert!(a.summary.starts_with("[deny only: "), "{}", a.summary);
        let out = tr.respond(&a.attn_id, &Answer::allow()).unwrap();
        assert_eq!(
            parse(&out.replies[0])["result"],
            json!({"permissions": {}, "scope": "turn"}),
            "{unrecognized}: an allow on a deny-only item grants nothing"
        );
    }

    // Too long to show in full: deny only.
    let long: Vec<String> = (0..60)
        .map(|i| format!("/srv/a/long/path/number/{i}"))
        .collect();
    let (mut tr, _) = fed_until("approve", "\"id\":3,\"result\"");
    let a = raised(&tr.translate(&request(11, json!({"fileSystem": {"read": long}}))));
    assert_eq!(a.options, ["deny"], "{}", a.summary);
    assert!(a.summary.contains("too long"), "{}", a.summary);
}

/// A file-change approval with `grantRoot` says, first and in full, that
/// approving also grants writes anywhere under that directory for the session.
#[test]
fn file_change_grant_root_is_shown() {
    let (mut tr, _) = fed_until("approve", "\"id\":3,\"result\"");
    let line = |id: u64, root: &str| {
        json!({"method": "item/fileChange/requestApproval", "id": id, "params": {
            "threadId": "01a0eb1f-bbfe-7030-9892-8a0d81f3d695", "turnId": "t", "itemId": "fc1", "reason": null,
            "grantRoot": root}})
        .to_string()
    };
    let out = tr.translate(&line(12, "/home/user"));
    let a = match &out.events[..] {
        [Emitted {
            body: EventBody::AttentionRaised(a),
            ..
        }] => a.clone(),
        other => panic!("{other:?}"),
    };
    assert!(
        a.summary.starts_with(
            "Approving also lets Codex write anywhere under /home/user for the rest of the session."
        ),
        "{}",
        a.summary
    );
    assert_eq!(a.options, ["allow", "deny"]);
    assert_eq!(ext(&a.ext, "grant_root"), "/home/user");

    // A root that cannot be shown in full: deny only, and an allow declines.
    let out = tr.translate(&line(13, &"/x".repeat(600)));
    let a = match &out.events[..] {
        [Emitted {
            body: EventBody::AttentionRaised(a),
            ..
        }] => a.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(a.options, ["deny"], "{}", a.summary);
    let out = tr.respond(&a.attn_id, &Answer::allow()).unwrap();
    assert_eq!(parse(&out.replies[0])["result"]["decision"], "decline");
}

/// A server request reusing a pending request's JSON-RPC id
/// replaces it. A reply names only that id, so an answer to the old item (the hub had not seen
/// the replacement yet) would grant the replacement: the old item is withdrawn as superseded,
/// its answer is ignored, and only the replacement's own id answers it, with its own content.
#[test]
fn a_replaced_request_is_never_answered_by_an_answer_to_the_old_one() {
    let turn_id = "01a0eb1f-bca4-7a91-a19d-36acbff767f9";
    let request = |write: &str| {
        format!(
            r#"{{"method":"item/permissions/requestApproval","id":9,"params":{{"threadId":"01a0eb1f-bbfe-7030-9892-8a0d81f3d695","turnId":"{turn_id}","itemId":"call_p","cwd":"{FIXTURE_CWD}","reason":null,"permissions":{{"network":null,"fileSystem":{{"read":null,"write":["{write}"]}}}}}}}}"#
        )
    };
    let (mut tr, _) = fed_until("approve", "\"id\":3,\"result\"");
    let out = tr.translate(&request(&format!("{FIXTURE_CWD}/target")));
    let old = match &out.events[0].body {
        EventBody::AttentionRaised(a) => a.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(old.attn_id, "call_p#perm");

    let out = tr.translate(&request("/"));
    assert!(out.replies.is_empty());
    assert_eq!(out.events.len(), 2, "{:?}", out.events);
    match &out.events[0].body {
        EventBody::AttentionResolved(r) => {
            assert_eq!(r.attn_id, old.attn_id);
            assert_eq!(r.by, ResolvedBy::Timeout);
            assert!(r.answer.is_none());
            assert_eq!(ext(&r.ext, "superseded"), true);
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(out.events[0].turn, Some(1));
    let new = match &out.events[1].body {
        EventBody::AttentionRaised(a) => a.clone(),
        other => panic!("{other:?}"),
    };
    assert_ne!(new.attn_id, old.attn_id);
    assert!(new.summary.contains("write /"), "{}", new.summary);
    assert_eq!(tr.pending_attention(), std::slice::from_ref(&new.attn_id));

    // The allow meant for `write target` arrives after the replacement: nothing is sent.
    assert!(tr.respond(&old.attn_id, &Answer::allow()).is_none());
    assert_eq!(tr.pending_attention(), std::slice::from_ref(&new.attn_id));
    let out = tr.respond(&new.attn_id, &Answer::deny()).unwrap();
    assert_eq!(
        parse(&out.replies[0]),
        json!({"jsonrpc": "2.0", "id": 9, "result": {"permissions": {}, "scope": "turn"}})
    );
    // `serverRequest/resolved` for the id withdraws nothing more.
    assert!(tr
        .translate(r#"{"method":"serverRequest/resolved","params":{"threadId":"01a0eb1f-bbfe-7030-9892-8a0d81f3d695","requestId":9}}"#)
        .events
        .is_empty());

    // The same for a command approval whose id is reused by another method.
    let (mut tr, _) = fed_until("approve", "item/commandExecution/requestApproval");
    let old = "exec-93e430a2-2acc-427a-bd92-3e20ce6c39e0";
    let out = tr.translate(&format!(
        r#"{{"method":"item/permissions/requestApproval","id":0,"params":{{"threadId":"01a0eb1f-bbfe-7030-9892-8a0d81f3d695","turnId":"{turn_id}","itemId":"call_q","cwd":"/","permissions":{{"network":{{"enabled":true}}}}}}}}"#
    ));
    let kinds: Vec<&str> = out.events.iter().map(|e| e.body.kind()).collect();
    assert_eq!(kinds, ["attention.resolved", "attention.raised"]);
    assert!(tr.respond(old, &Answer::allow()).is_none());
    assert_eq!(tr.pending_attention(), ["call_q#perm"]);
}

/// A command approval shows the command as it runs: a newline is not hidden in a space (the
/// audit's case: `# note` then `rm -rf ~` read as one comment), and quoted whitespace stays.
#[test]
fn a_command_approval_shows_the_command_as_it_runs() {
    let (mut tr, _) = fed_until("approve", "\"id\":3,\"result\"");
    let turn_id = "01a0eb1f-bca4-7a91-a19d-36acbff767f9";
    let request = json!({"method": "item/commandExecution/requestApproval", "id": 7,
        "params": {"threadId": "01a0eb1f-bbfe-7030-9892-8a0d81f3d695", "turnId": turn_id,
                   "itemId": "call_c", "cwd": "/",
                   "command": "echo 'a   b' # tidy up\nrm -rf ~"}});
    let out = tr.translate(&request.to_string());
    let summary = out
        .events
        .iter()
        .find_map(|e| match &e.body {
            EventBody::AttentionRaised(a) => Some(a.summary.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{out:?}"));
    assert_eq!(summary, "Allow shell: echo 'a   b' # tidy up⏎rm -rf ~");
    // A trailing space is part of the command (`value\ `), and a no-break space is no space.
    let a = command_approval("echo value\\ ", None);
    assert_eq!(a.summary, "Allow shell: echo value\\⟨U+0020⟩");
    let a = command_approval("rm\u{a0}-rf /tmp/x", None);
    assert_eq!(a.summary, "Allow shell: rm⟨U+00A0⟩-rf /tmp/x");
}

/// The raised item of a command approval of `command`, with `grantRoot` `root` if given.
fn command_approval(command: &str, root: Option<&str>) -> AttentionRaised {
    let (mut tr, _) = fed_until("approve", "\"id\":3,\"result\"");
    let mut params = json!({"threadId": "01a0eb1f-bbfe-7030-9892-8a0d81f3d695",
        "turnId": "01a0eb1f-bca4-7a91-a19d-36acbff767f9", "itemId": "call_c", "cwd": "/",
        "command": command});
    if let Some(root) = root {
        params["grantRoot"] = json!(root);
    }
    let request = json!({"method": "item/commandExecution/requestApproval", "id": 7,
                         "params": params});
    let out = tr.translate(&request.to_string());
    out.events
        .iter()
        .find_map(|e| match &e.body {
            EventBody::AttentionRaised(a) => Some(a.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{out:?}"))
}

/// A long harmless comment cannot push a dangerous command out of sight, with or without a
/// grant (whose summary keeps 400 characters of the command): the summary says it is cut,
/// and the whole command, marked, is in the item's context. Past the cap even that is cut,
/// and only `deny` is offered.
#[test]
fn a_cut_command_is_whole_in_the_item_or_deny_only() {
    let command = format!("# {}\nrm -rf ~", "harmless ".repeat(120));
    for root in [None, Some("/tmp/w")] {
        let a = command_approval(&command, root);
        assert!(!a.summary.contains("rm -rf"), "{}", a.summary);
        assert!(
            a.summary
                .ends_with(" more characters; open the card to see all"),
            "{}",
            a.summary
        );
        assert_eq!(a.options, ["allow", "deny"]);
        let ctx = twr_core::reduce::RequestContext::of(a.reason, &a.ext).unwrap();
        assert!(ctx.input.unwrap().ends_with("⏎rm -rf ~"), "{root:?}");
    }
    let huge = format!("# {}\nrm -rf ~", "x".repeat(70 * 1024));
    let a = command_approval(&huge, None);
    assert_eq!(a.options, ["deny"]);
    assert!(a.summary.starts_with("[deny only:"), "{}", a.summary);
}

/// A `grantRoot` is marked like a command: a zero-width or bidirectional character, or a mark
/// itself, shows. One too long to show (marked) makes the item deny-only.
#[test]
fn a_grant_root_shows_every_character() {
    let a = command_approval("ls", Some("/home/u/\u{200b}proj\u{202e}cod⏎"));
    assert!(
        a.summary.starts_with(
            "Approving also lets Codex write anywhere under \
             /home/u/⟨U+200B⟩proj⟨U+202E⟩cod⟨U+23CE⟩ for the rest of the session."
        ),
        "{}",
        a.summary
    );
    assert_eq!(a.options, ["allow", "deny"]);
    // Whitespace at the ends of a root, or one that only looks like a space, shows.
    for (root, shown) in [
        ("/safe ", "/safe⟨U+0020⟩ for"),
        (" /safe", "⟨U+0020⟩/safe for"),
        ("/a\u{a0}b\u{2009}c", "/a⟨U+00A0⟩b⟨U+2009⟩c for"),
    ] {
        let a = command_approval("ls", Some(root));
        assert!(
            a.summary.contains(&format!("under {shown}")),
            "{}",
            a.summary
        );
    }
    let a = command_approval("ls", Some(&format!("/home/{}", "\u{2066}".repeat(120))));
    assert_eq!(a.options, ["deny"], "{}", a.summary);
    assert!(
        a.summary.starts_with("[deny only: the directory"),
        "{}",
        a.summary
    );
}

/// An async question set asked again by the same message item replaces the old set: a
/// free-text answer to the old question is never steered in as the answer to the new one.
#[test]
fn a_repeated_async_question_takes_no_answer_meant_for_the_old_one() {
    let (mut tr, _) = fed_until("approve", "\"id\":3,\"result\"");
    let turn_id = "01a0eb1f-bca4-7a91-a19d-36acbff767f9";
    let ask = |title: &str| {
        format!(
            r#"{{"method":"item/completed","params":{{"threadId":"01a0eb1f-bbfe-7030-9892-8a0d81f3d695","turnId":"{turn_id}","item":{{"type":"agentMessage","id":"msg_q","text":"","questions":[{{"title":"{title}","options":["yes","no"]}}]}}}}}}"#
        )
    };
    let out = tr.translate(&ask("Keep the cache?"));
    let old = match &out.events[0].body {
        EventBody::AttentionRaised(a) => a.clone(),
        other => panic!("{other:?}"),
    };
    let out = tr.translate(&ask("Delete the repository?"));
    let kinds: Vec<&str> = out.events.iter().map(|e| e.body.kind()).collect();
    assert_eq!(kinds, ["attention.resolved", "attention.raised"]);
    let new = match &out.events[1].body {
        EventBody::AttentionRaised(a) => a.clone(),
        other => panic!("{other:?}"),
    };
    assert_ne!(new.attn_id, old.attn_id);
    assert!(tr.respond(&old.attn_id, &Answer::new("yes")).is_none());
    assert_eq!(tr.pending_attention(), std::slice::from_ref(&new.attn_id));
}

// ---------------------------------------------------------------------------------------------
// SpawnConfig mapping
// ---------------------------------------------------------------------------------------------

#[test]
fn thread_params_for_new_and_resumed_threads() {
    let Open {
        method,
        params,
        mode_note,
        added_instructions,
    } = thread_params(&SpawnConfig {
        model: Some("gpt-6-astra".into()),
        system_append: Some("Be brief.".into()),
        ..cfg("manual")
    })
    .unwrap();
    assert_eq!(method, "thread/start");
    assert_eq!(mode_note, None, "manual maps exactly");
    assert_eq!(
        added_instructions, None,
        "the sandbox note is for auto only"
    );
    assert_eq!(
        params,
        json!({"cwd": FIXTURE_CWD, "approvalPolicy": "untrusted", "approvalsReviewer": "user",
               "sandbox": "workspace-write", "model": "gpt-6-astra",
               "developerInstructions": "Be brief.",
               "config": {"sandbox_workspace_write.network_access": false,
                          "sandbox_workspace_write.writable_roots": []}})
    );
    let open = thread_params(&SpawnConfig {
        resume_session: Some("th-1".into()),
        ..cfg("acceptEdits")
    })
    .unwrap();
    assert_eq!(open.method, "thread/resume");
    // acceptEdits cannot be expressed without widening shell autonomy: everything asks.
    assert_eq!(
        open.params,
        json!({"threadId": "th-1", "cwd": FIXTURE_CWD, "approvalPolicy": "untrusted",
               "approvalsReviewer": "user", "sandbox": "workspace-write", "excludeTurns": true,
               "config": {"sandbox_workspace_write.network_access": false,
                          "sandbox_workspace_write.writable_roots": []}})
    );
    assert!(open.mode_note.unwrap().contains("acceptEdits"));
}

/// The reasoning effort goes in the `config` overrides of `thread/start` and `thread/resume`
/// (neither has a param of its own; Codex 0.159.2 reports it back as `reasoningEffort`), next
/// to the model, and the attach command passes both.
#[test]
fn the_effort_is_a_config_override_on_start_and_resume() {
    for resume in [None, Some("th-1".to_string())] {
        let c = SpawnConfig {
            model: Some("gpt-5.5".into()),
            effort: Some("high".into()),
            resume_session: resume.clone(),
            ..cfg("manual")
        };
        let p = thread_params(&c).unwrap().params;
        assert_eq!(p["model"], "gpt-5.5", "{p}");
        assert_eq!(p["config"]["model_reasoning_effort"], "high", "{p}");
        assert_eq!(p.get("threadId").is_some(), resume.is_some());
    }
    let none = thread_params(&cfg("manual")).unwrap().params;
    assert!(
        none["config"].get("model_reasoning_effort").is_none(),
        "{none}"
    );
    let blank = thread_params(&SpawnConfig {
        effort: Some(String::new()),
        ..cfg("manual")
    })
    .unwrap()
    .params;
    assert!(
        blank["config"].get("model_reasoning_effort").is_none(),
        "{blank}"
    );
    let attach = twr_adapters::codex::CodexAdapter::new()
        .attach_command(
            &SpawnConfig {
                model: Some("gpt-5.5".into()),
                effort: Some("xhigh".into()),
                ..cfg("manual")
            },
            "th-1",
        )
        .unwrap();
    let args: Vec<String> = attach
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let at = args.iter().position(|a| a == "--model").unwrap();
    assert_eq!(args[at + 1], "gpt-5.5");
    assert!(
        args.windows(2)
            .any(|w| w[0] == "-c" && w[1] == "model_reasoning_effort=\"xhigh\""),
        "{args:?}"
    );
}

/// `twr attach` runs `codex resume` with the thread's configured variables (`[threads] env`,
/// `[projects.<name>] env`), as its headless runtime does, on this box and on another; none of
/// twr's own entries, and its secrets still removed.
#[test]
fn attach_commands_run_with_the_configured_environment() {
    let cfg = SpawnConfig {
        env: vec![
            ("CARGO_TARGET_DIR".into(), "/cache/api".into()),
            ("TWR_THREAD".into(), "t-1".into()),
        ],
        ..cfg("manual")
    };
    let a = CodexAdapter::with_bin("/opt/codex");
    for cmd in [
        a.attach_command(&cfg, "th-1").unwrap(),
        a.remote_attach_command(&cfg, "th-1").unwrap(),
    ] {
        let envs: Vec<_> = cmd.get_envs().collect();
        let set = (
            std::ffi::OsStr::new("CARGO_TARGET_DIR"),
            Some(std::ffi::OsStr::new("/cache/api")),
        );
        assert!(envs.contains(&set), "{envs:?}");
        assert!(!envs.iter().any(|(k, _)| *k == "TWR_THREAD"), "{envs:?}");
        for k in twr_adapters::process::SCRUBBED_ENV {
            assert!(envs.contains(&(std::ffi::OsStr::new(k), None)), "{k}");
        }
    }
}

/// Claude's `dontAsk` (deny by default) never becomes "run anything without asking": Codex
/// gets `never` with a read-only sandbox, and `thread.started` says the mapping is inexact.
#[test]
fn dont_ask_is_deny_by_default_and_noted_on_thread_started() {
    let open = thread_params(&cfg("dontAsk")).unwrap();
    assert_eq!(open.params["approvalPolicy"], "never");
    assert_eq!(open.params["sandbox"], "read-only");
    let note = open.mode_note.clone().unwrap();
    assert!(note.contains("deny by default"), "{note}");
    let mut tr = Translator::new(TranslatorOptions::default(), open);
    tr.start();
    tr.translate(r#"{"id":1,"result":{}}"#);
    let out = tr.translate(
        r#"{"id":2,"result":{"thread":{"id":"th"},"approvalPolicy":"never","approvalsReviewer":"user","sandbox":{"type":"readOnly"}}}"#,
    );
    match &out.events[0].body {
        EventBody::ThreadStarted(s) => assert_eq!(ext(&s.ext, "mode_note"), note.as_str()),
        other => panic!("{other:?}"),
    }
    // An exact mapping carries no note.
    let mut tr = new_translator(&cfg("manual"), TranslatorOptions::default());
    tr.start();
    tr.translate(r#"{"id":1,"result":{}}"#);
    let out = tr.translate(OPENED_MANUAL);
    match &out.events[0].body {
        EventBody::ThreadStarted(s) => assert!(ext(&s.ext, "mode_note").is_null()),
        other => panic!("{other:?}"),
    }
}

/// `auto` keeps on-request with a workspace-write sandbox, widened by `config` overrides so
/// tests can bind sockets and commits reach the main repository's `.git`; the same overrides
/// go to the attach command, and `thread.started` reports the roots Codex runs with. Other
/// workspace-write modes pin no network and no roots; the other sandboxes get none.
#[test]
fn auto_widens_the_workspace_sandbox_on_start_resume_and_attach() {
    let dir = tempfile::tempdir().unwrap();
    let common = dir.path().join("main/.git");
    for d in ["objects", "refs", "logs", "worktrees/wt"] {
        std::fs::create_dir_all(common.join(d)).unwrap();
    }
    std::fs::write(common.join("worktrees/wt/commondir"), "../..\n").unwrap();
    let wt = dir.path().join("wt");
    std::fs::create_dir_all(&wt).unwrap();
    std::fs::write(
        wt.join(".git"),
        format!("gitdir: {}\n", common.join("worktrees/wt").display()),
    )
    .unwrap();
    // git's back-pointer from the record to the worktree.
    std::fs::write(
        common.join("worktrees/wt/gitdir"),
        format!("{}\n", wt.join(".git").display()),
    )
    .unwrap();
    let common = common.canonicalize().unwrap();
    // And the shared memory folder, where there is one (`/dev/shm`): all a folder outside
    // any repository gets with no home.
    let shm = twr_adapters::codex::auto_writable_roots(dir.path(), None, |_| None);
    let roots: Vec<String> = ["worktrees/wt", "objects", "refs", "logs"]
        .iter()
        .map(|d| common.join(d))
        .chain(shm)
        .map(|d| d.display().to_string())
        .collect();
    let auto = SpawnConfig {
        cwd: wt.clone(),
        repo: Some(dir.path().join("main")),
        env: vec![("HOME".into(), "/nonexistent-twr".into())]
            .into_iter()
            .chain(
                ["CARGO_HOME", "npm_config_cache", "NPM_CONFIG_CACHE"]
                    .map(|k| (k.to_string(), String::new())),
            )
            .collect(),
        ..cfg("auto")
    };
    for resume in [None, Some("th-1".to_string())] {
        let open = thread_params(&SpawnConfig {
            resume_session: resume,
            ..auto.clone()
        })
        .unwrap();
        let p = &open.params;
        assert_eq!(p["approvalPolicy"], "on-request");
        assert_eq!(p["sandbox"], "workspace-write");
        assert_eq!(p["config"]["sandbox_workspace_write.network_access"], true);
        assert_eq!(
            p["config"]["sandbox_workspace_write.writable_roots"],
            json!(roots)
        );
        assert!(open.mode_note.unwrap().contains("network access"));
    }
    for other in ["manual", "on-request", "never"] {
        let p = thread_params(&SpawnConfig {
            mode: other.into(),
            ..auto.clone()
        })
        .unwrap()
        .params;
        assert_eq!(
            p["config"],
            json!({"sandbox_workspace_write.network_access": false,
                   "sandbox_workspace_write.writable_roots": []}),
            "{other}"
        );
    }
    for other in ["plan", "bypassPermissions"] {
        let p = thread_params(&SpawnConfig {
            mode: other.into(),
            ..auto.clone()
        })
        .unwrap()
        .params;
        assert!(p.get("config").is_none(), "{other}: {p}");
    }

    let args: Vec<String> = CodexAdapter::with_bin("/opt/codex")
        .attach_command(&auto, "th-1")
        .unwrap()
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    for want in [
        "sandbox_workspace_write.network_access=true".to_string(),
        format!("sandbox_workspace_write.writable_roots={}", json!(roots)),
    ] {
        assert!(
            args.windows(2).any(|w| w[0] == "-c" && w[1] == want),
            "{want} in {args:?}"
        );
    }
    // An attach on a runner's box gets no roots resolved from the hub's files (and none of
    // the user's config there either).
    let remote: Vec<String> = CodexAdapter::with_bin("/opt/codex")
        .remote_attach_command(&auto, "th-1")
        .unwrap()
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert!(remote.contains(&"sandbox_workspace_write.network_access=true".to_string()));
    assert!(
        remote.contains(&"sandbox_workspace_write.writable_roots=[]".to_string()),
        "{remote:?}"
    );

    let mut tr = new_translator(&auto, TranslatorOptions::default());
    tr.start();
    // The sandbox note goes after the config's own developer instructions: the config is
    // read before the thread opens.
    let id_of = |replies: &[String], method: &str| {
        replies
            .iter()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .find(|v| v["method"] == method)
            .map(|v| v["id"].clone())
            .unwrap_or_else(|| panic!("no {method} in {replies:?}"))
    };
    let out = tr.translate(r#"{"id":1,"result":{}}"#);
    let read = id_of(&out.replies, "config/read");
    let out = tr.translate(&json!({"id": read, "result": {"config": {}}}).to_string());
    let open = id_of(&out.replies, "thread/start");
    let opened = json!({"id": open, "result": {"thread": {"id": "th"}, "cwd": wt,
        "approvalPolicy": "on-request", "approvalsReviewer": "user",
        "sandbox": {"type": "workspaceWrite", "writableRoots": roots, "networkAccess": true,
                    "excludeTmpdirEnvVar": false, "excludeSlashTmp": false}}});
    let out = tr.translate(&opened.to_string());
    match &out.events[0].body {
        EventBody::ThreadStarted(s) => {
            assert_eq!(ext(&s.ext, "sandbox_network"), true);
            assert_eq!(ext(&s.ext, "sandbox_writable_roots"), &json!(roots));
        }
        other => panic!("{other:?}"),
    }
}

/// A user's or a project's Codex config adds its `[sandbox_workspace_write]` network access
/// and writable roots to a sandbox twr did not ask them for (verified with Codex 0.160.0 when
/// the request does not override them): a thread whose sandbox reports either beyond the
/// request is refused before any prompt. `dangerFullAccess`, when asked for, allows all.
#[test]
fn a_sandbox_with_network_or_roots_twr_did_not_ask_for_is_refused() {
    use twr_adapters::codex::translate::check_policy;
    let result = |sandbox: Value| {
        json!({"thread": {"id": "th"}, "approvalPolicy": "untrusted", "cwd": FIXTURE_CWD,
               "approvalsReviewer": "user", "sandbox": sandbox})
    };
    let ww = |extra: Value| {
        let mut s = json!({"type": "workspaceWrite", "excludeTmpdirEnvVar": false,
                           "excludeSlashTmp": false});
        s.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        result(s)
    };
    let manual = thread_params(&cfg("manual")).unwrap().params;
    let ok = ww(json!({"writableRoots": [], "networkAccess": false}));
    assert_eq!(check_policy(&manual, &ok), Ok(()));
    // What Codex 0.160.0 reports with a user config of network_access and writable_roots
    // that the request does not override.
    let inherited = ww(json!({"writableRoots": ["/home/u/extra"], "networkAccess": true}));
    let e = check_policy(&manual, &inherited).unwrap_err();
    assert!(e.contains("networkAccess"), "{e}");
    let e = check_policy(&manual, &ww(json!({"writableRoots": ["/home/u/extra"]}))).unwrap_err();
    assert!(e.contains("/home/u/extra"), "{e}");
    let e = check_policy(&manual, &ww(json!({"writableRoots": "/home"}))).unwrap_err();
    assert!(e.contains("cannot check"), "{e}");
    let e = check_policy(&manual, &ww(json!({"networkAccess": "enabled"}))).unwrap_err();
    assert!(e.contains("networkAccess"), "{e}");
    // Read-only with network: never asked for.
    let plan = thread_params(&cfg("plan")).unwrap().params;
    let ro = result(json!({"type": "readOnly", "networkAccess": true}));
    assert!(check_policy(&plan, &ro).is_err());
    let ro = result(json!({"type": "readOnly", "networkAccess": false}));
    assert_eq!(check_policy(&plan, &ro), Ok(()));
    // `auto`: its own network and roots pass, one more root does not.
    let auto = SpawnConfig {
        cwd: std::env::temp_dir(),
        ..cfg("auto")
    };
    let mut asked = thread_params(&auto).unwrap().params;
    asked["config"]["sandbox_workspace_write.writable_roots"] = json!(["/srv/cache"]);
    let mut on_request = ww(json!({"writableRoots": ["/srv/cache/"], "networkAccess": true}));
    on_request["approvalPolicy"] = json!("on-request");
    on_request["cwd"] = json!(std::env::temp_dir());
    assert_eq!(check_policy(&asked, &on_request), Ok(()));
    on_request["sandbox"]["writableRoots"] = json!(["/srv/cache", "/home/u"]);
    assert!(check_policy(&asked, &on_request).is_err());
    // Full access, asked for: nothing more to check.
    let bypass = thread_params(&cfg("bypassPermissions")).unwrap().params;
    let mut full = result(json!({"type": "dangerFullAccess"}));
    full["approvalPolicy"] = json!("never");
    assert_eq!(check_policy(&bypass, &full), Ok(()));
}

/// The orchestrator's spawn config as the hub builds it (`orchestrator_spawn_config` in
/// `twr-hub/src/hub/orch.rs`): the default mode, the relay system prompt, the twr MCP server in
/// a 0600 file with the token in its env, `allowed_tools = ["mcp__twr"]`, no built-in tools.
#[test]
fn orchestrator_spawn_config_maps_to_codex() {
    let dir = tempfile::tempdir().unwrap();
    let token = "0123456789abcdef-orchestrator-token";
    let file = dir.path().join("orch-mcp-0123456789abcdef.json");
    std::fs::write(
        &file,
        json!({"mcpServers": {"twr": {"command": "/opt/twr/bin/twr",
                                       "args": ["mcp", "--socket", "/tmp/twr.sock"],
                                       "env": {"TWR_ORCH_TOKEN": token}}}})
        .to_string(),
    )
    .unwrap();
    let cfg = SpawnConfig {
        thread: "orch".into(),
        cwd: dir.path().to_path_buf(),
        mode: "manual".into(),
        system_append: Some("You are the twr orchestrator.".into()),
        mcp_config_file: Some(file),
        allowed_tools: vec!["mcp__twr".into()],
        builtin_tools: Some(vec![]),
        ..Default::default()
    };
    let open = thread_params(&cfg).unwrap();
    assert_eq!(open.method, "thread/start");
    let p = &open.params;
    assert_eq!(p["approvalPolicy"], "untrusted");
    assert_eq!(p["approvalsReviewer"], "user");
    assert_eq!(
        p["sandbox"], "read-only",
        "no built-ins: nothing to write with"
    );
    assert_eq!(p["developerInstructions"], "You are the twr orchestrator.");
    let c = &p["config"];
    assert_eq!(
        c["mcp_servers"],
        json!({"twr": {"command": "/opt/twr/bin/twr",
                       "args": ["mcp", "--socket", "/tmp/twr.sock"],
                       "env": {"TWR_ORCH_TOKEN": token},
                       "default_tools_approval_mode": "approve"}})
    );
    for k in [
        "features.shell_tool",
        "features.unified_exec",
        "features.multi_agent",
    ] {
        assert_eq!(c[k], false, "{k}");
    }
    assert_eq!(c["web_search"], "disabled");
    // Everything travels over stdin: the command line carries no config at all.
    assert_eq!(codex::build_args(), ["app-server", "--stdio"]);

    // Inline JSON works the same; a missing file fails the spawn.
    let inline = SpawnConfig {
        mcp_config_file: None,
        mcp_config: Some(
            json!({"mcpServers": {"twr": {"type": "stdio", "command": "twr",
                                                       "headers": {"a": "b"}}}}),
        ),
        ..cfg.clone()
    };
    let p = thread_params(&inline).unwrap().params;
    assert_eq!(
        p["config"]["mcp_servers"]["twr"],
        json!({"command": "twr", "http_headers": {"a": "b"},
               "default_tools_approval_mode": "approve"})
    );
    let missing = SpawnConfig {
        mcp_config_file: Some(dir.path().join("gone.json")),
        ..cfg
    };
    assert!(thread_params(&missing).is_err());
    assert!(CodexAdapter::with_bin("/nonexistent")
        .spawn(missing, None)
        .is_err());
}

#[test]
fn attach_command_resumes_in_the_worktree() {
    let cfg = SpawnConfig {
        model: Some("gpt-6-astra".into()),
        ..SpawnConfig {
            cwd: PathBuf::from("/tmp/wt"),
            mode: "acceptEdits".into(),
            ..Default::default()
        }
    };
    let cmd = CodexAdapter::with_bin("/opt/codex")
        .attach_command(&cfg, "th-1")
        .unwrap();
    assert_eq!(cmd.get_program(), "/opt/codex");
    let args: Vec<_> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        args,
        [
            "resume",
            "th-1",
            "--ask-for-approval",
            "untrusted",
            "--sandbox",
            "workspace-write",
            "-c",
            "approvals_reviewer=\"user\"",
            "-c",
            "sandbox_workspace_write.network_access=false",
            "-c",
            "sandbox_workspace_write.writable_roots=[]",
            "--model",
            "gpt-6-astra"
        ]
    );
    assert_eq!(cmd.get_current_dir(), Some(Path::new("/tmp/wt")));
    // twr's secrets are removed from the interactive CLI's environment.
    let envs: Vec<_> = cmd.get_envs().collect();
    for k in twr_adapters::process::SCRUBBED_ENV {
        assert!(envs.contains(&(std::ffi::OsStr::new(k), None)), "{k}");
    }
    assert!(twr_adapters::find(codex::NAME).is_some());
}

// ---------------------------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------------------------

/// Stdout lines of `tests/fixtures/codex/usage.stdout.jsonl`: the answer to an
/// `account/rateLimits/read` (recorded from Codex 0.160.0 on 2026-10-09, its account id and
/// credit details removed; the test gives it the request's id), an
/// `account/rateLimits/updated` (recorded, from `approve`), one of a plan with a 5-hour
/// primary and a weekly secondary window, and a sparse one (its primary null).
fn usage_lines() -> Vec<Value> {
    std::fs::read_to_string(fixture_path("approve").with_file_name("usage.stdout.jsonl"))
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

fn limits_in(events: &[Emitted]) -> Vec<RuntimeLimits> {
    events
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::RuntimeLimits(l) => {
                assert_eq!(e.turn, None, "usage belongs to no turn");
                Some(l.clone())
            }
            _ => None,
        })
        .collect()
}

/// An `account/rateLimits/read` answer to `read` carrying `notification`'s snapshot.
fn answer_with(read: &Value, notification: &Value) -> String {
    json!({"id": read["id"], "result": {"rateLimits": notification["params"]["rateLimits"]}})
        .to_string()
}

fn windows_of(l: &RuntimeLimits) -> Vec<(String, i64, Option<i64>, Option<u64>)> {
    l.windows
        .iter()
        .map(|w| {
            (
                w.window.clone(),
                w.pct(),
                w.resets_at.map(|t| t.timestamp()),
                w.minutes,
            )
        })
        .collect()
}

/// The usage is read (`account/rateLimits/read`, no turn) right after `initialized`, with an
/// id of the monitor space; its answer and every `account/rateLimits/updated` become
/// `runtime.limits`, windows named by their length; a turn's end reads again once the last
/// report is ten minutes old, and a refused read stops them.
#[test]
fn usage_reads_and_reports() {
    let lines = usage_lines();
    let mut tr = new_translator(&fixture_cfg("approve"), TranslatorOptions::default());
    tr.set_clock(usage_clock);
    let init = parse(&tr.start());
    let out = tr.translate(&json!({"id": init["id"], "result": {}}).to_string());
    let sent: Vec<Value> = out.replies.iter().map(|l| parse(l)).collect();
    assert_eq!(sent.len(), 3);
    assert_eq!(sent[0]["method"], "initialized");
    assert_eq!(sent[1]["method"], "thread/start");
    assert_eq!(sent[1]["id"], 2, "the turn RPC ids are not used");
    assert_eq!(sent[2]["method"], "account/rateLimits/read");
    assert!(sent[2]["id"].as_u64().unwrap() >= 1 << 50);

    let mut answer = lines[0].clone();
    answer["id"] = sent[2]["id"].clone();
    let got = limits_in(&tr.translate(&answer.to_string()).events);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].source, "read");
    assert_eq!(got[0].plan.as_deref(), Some("prolite"));
    assert_eq!(
        windows_of(&got[0]),
        [("seven_day".into(), 48, Some(1791976804), Some(10080))]
    );
    assert_eq!(got[0].windows[0].label(), "wk");

    let got = limits_in(&tr.translate(&lines[1].to_string()).events);
    assert_eq!(got[0].source, "event");
    assert_eq!(
        windows_of(&got[0]),
        [("seven_day".into(), 0, Some(1791255525), Some(10080))]
    );
    let got = limits_in(&tr.translate(&lines[2].to_string()).events);
    assert_eq!(got[0].plan.as_deref(), Some("plus"));
    assert_eq!(
        windows_of(&got[0]),
        [
            ("five_hour".into(), 92, Some(1791559200), Some(300)),
            ("seven_day".into(), 41, Some(1791976804), Some(10080)),
        ]
    );
    // Sparse: the null primary is unknown, not reported.
    let got = limits_in(&tr.translate(&lines[3].to_string()).events);
    assert_eq!(got[0].plan, None);
    assert_eq!(
        windows_of(&got[0]),
        [("seven_day".into(), 42, Some(1791976804), Some(10080))]
    );

    // Turn ends: a read once the last report (the notification) is ten minutes old.
    let completed = json!({"method": "turn/completed", "params": {
        "turn": {"id": "t9", "items": [], "status": "completed", "error": null}}})
    .to_string();
    USAGE_NOW.fetch_add(9 * 60, std::sync::atomic::Ordering::SeqCst);
    assert!(tr.translate(&completed).replies.is_empty());
    USAGE_NOW.fetch_add(60, std::sync::atomic::Ordering::SeqCst);
    let read = parse(&tr.translate(&completed).replies[0]);
    assert_eq!(read["method"], "account/rateLimits/read");
    // A plan with two windows: once a read had both, the runtime's reports of one of them
    // (sparse) do not put the next read off; one with both does.
    let read2 = answer_with(&read, &lines[2]);
    assert_eq!(limits_in(&tr.translate(&read2).events).len(), 1);
    for _ in 0..3 {
        USAGE_NOW.fetch_add(4 * 60, std::sync::atomic::Ordering::SeqCst);
        tr.translate(&lines[3].to_string());
    }
    let read = parse(&tr.translate(&completed).replies[0]);
    assert_eq!(
        read["method"], "account/rateLimits/read",
        "due despite the sparse reports"
    );
    tr.translate(&answer_with(&read, &lines[2]));
    USAGE_NOW.fetch_add(5 * 60, std::sync::atomic::Ordering::SeqCst);
    tr.translate(&lines[2].to_string());
    USAGE_NOW.fetch_add(9 * 60, std::sync::atomic::Ordering::SeqCst);
    assert!(
        tr.translate(&completed).replies.is_empty(),
        "a full report counts"
    );
    USAGE_NOW.fetch_add(60, std::sync::atomic::Ordering::SeqCst);
    let read = parse(&tr.translate(&completed).replies[0]);
    // Refused (an API key login, an older server): no more reads.
    let refused = json!({"id": read["id"], "error": {"code": -32600, "message": "not signed in with ChatGPT"}});
    assert!(tr.translate(&refused.to_string()).events.is_empty());
    USAGE_NOW.fetch_add(3600, std::sync::atomic::Ordering::SeqCst);
    assert!(tr.translate(&completed).replies.is_empty());
}

// ---------------------------------------------------------------------------------------------
// Driver against a stub `codex`
// ---------------------------------------------------------------------------------------------

const STUB_PY: &str = r#"
import json, os, sys
fixture, outdir, args = sys.argv[1], sys.argv[2], sys.argv[3:]
with open(os.path.join(outdir, "run.json"), "w") as f:
    json.dump({"args": args, "cwd": os.getcwd(), "thread": os.environ.get("TWR_THREAD"),
               "pgid": os.getpgid(0), "pid": os.getpid(), "env": dict(os.environ)}, f)
log = open(os.path.join(outdir, "stdin.jsonl"), "w")
def read_command():
    while True:
        got = sys.stdin.readline()
        if not got:
            return got
        req = json.loads(got)
        log.write(got); log.flush()
        if req.get("method") in ("thread/backgroundTerminals/list", "account/rateLimits/read"):
            sys.stdout.write(json.dumps({"id": req["id"], "error": {"code": -32601, "message": "unknown method"}}) + "\n"); sys.stdout.flush()
            continue
        if req.get("method") == "config/read":
            path = os.path.join(outdir, "config_read.json")
            config = json.load(open(path)) if os.path.exists(path) else {}
            sys.stdout.write(json.dumps({"id": req["id"], "result": {"config": config}}) + "\n"); sys.stdout.flush()
            continue
        return got
for line in open(fixture):
    d, body = line[:2], line[3:].rstrip("\n")
    if d == ">>":
        got = read_command()
        if not got:
            sys.exit(3)
    else:
        # Codex reports the cwd it runs in, which twr checks: the recording's is replaced,
        # with the paths under it.
        body = body.replace('/tmp/sandbox-codex', json.dumps(os.getcwd())[1:-1])
        sys.stdout.write(body + "\n"); sys.stdout.flush()
while read_command():
    pass
sys.stderr.write("stub: stdin closed\n")
"#;

/// Writes a `codex` stub replaying `fixture` into `dir`; returns its path.
fn stub(dir: &Path, fixture: &Path) -> PathBuf {
    let py = dir.join("stub.py");
    std::fs::write(&py, STUB_PY).unwrap();
    let path = dir.join("codex");
    install_executable(
        &path,
        &format!(
            "#!/bin/sh\nexec python3 {} {} {} \"$@\"\n",
            py.display(),
            fixture.display(),
            dir.display()
        ),
    );
    path
}

/// Creates the executable `path` without this process ever holding it open for writing (a
/// child forked meanwhile would inherit the descriptor and make exec fail with ETXTBSY): the
/// bytes go to a side file and `cp` writes the executable.
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
    let bin = stub(dir.path(), &fixture_path("approve"));
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
    let mut h = CodexAdapter::with_bin(bin)
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

fn find_body<T: Clone>(evs: &[AdapterEvent], f: impl Fn(&EventBody) -> Option<&T>) -> T {
    evs.iter()
        .find_map(|e| match e {
            AdapterEvent::Emit { body, .. } => f(body).cloned(),
            _ => None,
        })
        .unwrap()
}

fn recorded_stdin(name: &str) -> Vec<Value> {
    fixture(name)
        .into_iter()
        .filter(|(d, _)| d == ">>")
        .map(|(_, l)| normalize(parse(&l)))
        .collect()
}

fn written_stdin(dir: &Path) -> Vec<Value> {
    std::fs::read_to_string(dir.join("stdin.jsonl"))
        .unwrap()
        .lines()
        .map(parse)
        .filter(|v| !optional(v))
        .collect()
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

fn prompts(name: &str) -> Vec<String> {
    fixture(name)
        .into_iter()
        .map(|(_, l)| parse(&l))
        .filter(|v| v["method"] == "turn/start")
        .map(|v| {
            v["params"]["input"][0]["text"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}

#[tokio::test]
async fn driver_approve_multi_turn_and_stop_match_the_recorded_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let bin = stub(dir.path(), &fixture_path("approve"));
    let cfg = SpawnConfig {
        thread: "T1".into(),
        cwd: dir.path().to_path_buf(),
        mode: "plan".into(),
        env: vec![("TWR_THREAD".into(), "T1".into())],
        prompt_turn: Some(4),
        ..Default::default()
    };
    let p = prompts("approve");
    let (h, mut rx) = start(&bin, cfg, Some(&p[0]));

    let evs = until(&mut rx, |e| is_kind(e, "attention.raised")).await;
    match &evs[0] {
        AdapterEvent::Emit {
            body: EventBody::ThreadStarted(s),
            turn: None,
            raw,
        } => {
            assert_eq!(
                s.session_id.as_deref(),
                Some("01a0eb1f-bbfe-7030-9892-8a0d81f3d695")
            );
            let twr = &s.ext.as_ref().unwrap()["twr"];
            assert!(twr["pid"].as_u64().is_some());
            // The stub is a script: no exe to match against argv[0].
            assert!(twr.get("exe").is_none());
            assert!(raw.is_some());
        }
        other => panic!("{other:?}"),
    }
    let a = find_body(&evs, |b| match b {
        EventBody::AttentionRaised(a) => Some(a),
        _ => None,
    });
    assert!(matches!(
        evs.last(),
        Some(AdapterEvent::Emit { turn: Some(4), .. })
    ));
    h.respond(&a.attn_id, Answer::allow()).unwrap();
    let evs = until(&mut rx, |e| is_kind(e, "turn.ended")).await;
    assert!(matches!(
        evs.last(),
        Some(AdapterEvent::Emit { turn: Some(4), .. })
    ));
    h.send(p[1].clone(), 5).unwrap();
    until(&mut rx, |e| is_kind(e, "turn.ended")).await;
    h.send(p[2].clone(), 6).unwrap();
    until(&mut rx, |e| is_kind(e, "agent.text")).await;
    // Stop mid-turn: turn/interrupt, then stdin closes; the stub finishes the recording.
    h.stop().unwrap();
    let evs = until(&mut rx, |e| is_kind(e, "turn.ended")).await;
    match evs.last() {
        Some(AdapterEvent::Emit {
            body: EventBody::TurnEnded(e),
            turn: Some(6),
            ..
        }) => assert_eq!(e.result, TurnResult::Interrupted),
        other => panic!("{other:?}"),
    }
    let (code, tail) = exited(&mut rx).await;
    assert_eq!(code, Some(0));
    assert!(tail.contains("stub: stdin closed"));

    let written = written_stdin(dir.path());
    assert_eq!(
        written[2]["params"]["cwd"],
        dir.path().to_string_lossy().as_ref()
    );
    let written: Vec<Value> = written.into_iter().map(normalize).collect();
    assert_eq!(written, recorded_stdin("approve"));
    let run = parse(&std::fs::read_to_string(dir.path().join("run.json")).unwrap());
    assert_eq!(run["args"], json!(["app-server", "--stdio"]));
    assert_eq!(run["thread"], "T1");
    assert_eq!(run["cwd"], dir.path().to_string_lossy().as_ref());
    assert_eq!(run["pgid"], run["pid"], "own process group");
}

#[tokio::test]
async fn driver_resume_and_file_change() {
    let dir = tempfile::tempdir().unwrap();
    let bin = stub(dir.path(), &fixture_path("resume"));
    let cfg = SpawnConfig {
        thread: "T1".into(),
        cwd: dir.path().to_path_buf(),
        mode: "read-only".into(),
        ..Default::default()
    };
    let adapter = CodexAdapter::with_bin(&bin).stop_grace(Duration::from_millis(500));
    let mut h = adapter
        .resume(cfg, "01a0eb1f-bbfe-7030-9892-8a0d81f3d695".into())
        .unwrap();
    let mut rx = h.take_events().unwrap();
    let p = prompts("resume");
    until(&mut rx, |e| is_kind(e, "thread.started")).await;
    h.send(p[0].clone(), 3).unwrap();
    until(&mut rx, |e| is_kind(e, "turn.ended")).await;
    h.send(p[1].clone(), 4).unwrap();
    let evs = until(&mut rx, |e| is_kind(e, "attention.raised")).await;
    let a = find_body(&evs, |b| match b {
        EventBody::AttentionRaised(a) => Some(a),
        _ => None,
    });
    assert_eq!(a.summary, "Allow edit 1 file: hi.txt");
    h.respond(&a.attn_id, Answer::allow()).unwrap();
    let evs = until(&mut rx, |e| is_kind(e, "turn.ended")).await;
    let ended = find_body(&evs, |b| match b {
        EventBody::TurnEnded(e) => Some(e),
        _ => None,
    });
    assert_eq!(ended.result, TurnResult::Completed);
    drop(h);
    let (code, _) = exited(&mut rx).await;
    assert_eq!(code, Some(0));
    let written: Vec<Value> = written_stdin(dir.path())
        .into_iter()
        .map(normalize)
        .collect();
    assert_eq!(written, recorded_stdin("resume"));
}

#[tokio::test]
async fn failed_resume_stops_and_says_why() {
    let dir = tempfile::tempdir().unwrap();
    let rec = dir.path().join("rec.jsonl");
    std::fs::write(
        &rec,
        [
            ">> {}",
            r#"<< {"id":1,"result":{}}"#,
            ">> {}",
            ">> {}",
            r#"<< {"id":2,"error":{"code":-32600,"message":"thread not found"}}"#,
        ]
        .join("\n")
            + "\n",
    )
    .unwrap();
    let bin = stub(dir.path(), &rec);
    let cfg = SpawnConfig {
        cwd: dir.path().to_path_buf(),
        resume_session: Some("gone".into()),
        ..Default::default()
    };
    let (_h, mut rx) = start(&bin, cfg, Some("hi"));
    let evs = until(&mut rx, |e| matches!(e, AdapterEvent::Exited { .. })).await;
    assert!(evs.iter().all(|e| !is_kind(e, "thread.started")));
    match evs.last() {
        Some(AdapterEvent::Exited { stderr_tail, .. }) => {
            assert!(stderr_tail.contains("thread not found"), "{stderr_tail}")
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn stop_kills_a_runtime_that_ignores_stdin_eof() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("codex");
    install_executable(
        &bin,
        "#!/bin/sh\nsleep 1000 &\nwhile true; do sleep 1; done\n",
    );
    let cfg = SpawnConfig {
        cwd: dir.path().to_path_buf(),
        ..Default::default()
    };
    let (h, mut rx) = start(&bin, cfg, None);
    tokio::time::sleep(Duration::from_millis(200)).await;
    h.stop().unwrap();
    let (code, _) = exited(&mut rx).await;
    assert_eq!(code, None, "killed");
}

#[tokio::test]
async fn missing_binary_fails_spawn() {
    let err = CodexAdapter::with_bin("/nonexistent/codex").spawn(
        SpawnConfig {
            cwd: std::env::temp_dir(),
            ..Default::default()
        },
        None,
    );
    assert!(err.is_err());
}

// ---------------------------------------------------------------------------------------------
// Hub: Codex as the orchestrator
// ---------------------------------------------------------------------------------------------

/// The hub starts its orchestrator on Codex: the thread/start the stub receives carries the
/// twr MCP server (token in its env, no approval prompts), the relay system prompt and no
/// built-in tools, and the token appears nowhere on the command line.
#[tokio::test]
async fn hub_orchestrator_on_codex() {
    use twr_core::config::Config;
    use twr_hub::{Hub, HubOptions};

    let dir = tempfile::tempdir().unwrap();
    // The handshake of a recorded session, then silence until stdin closes.
    let lines: Vec<String> = std::fs::read_to_string(fixture_path("approve"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect();
    let end = lines
        .iter()
        .position(|l| l.starts_with(r#"<< {"id":2,"result""#))
        .unwrap();
    let rec = dir.path().join("handshake.jsonl");
    std::fs::write(&rec, lines[..=end].join("\n") + "\n").unwrap();
    let bin = stub(dir.path(), &rec);
    // The user's Codex config has an MCP server of its own: the orchestrator's thread turns
    // it off (Codex would merge it with twr's).
    std::fs::write(
        dir.path().join("config_read.json"),
        json!({"mcp_servers": {"mine": {"command": "python3", "enabled": true}}}).to_string(),
    )
    .unwrap();

    let mut config = Config {
        default_runtime: "fake".into(),
        ..Default::default()
    };
    config.orchestrator.runtime = Some(codex::NAME.into());
    config.orchestrator.cwd = Some(dir.path().to_path_buf());
    config.orchestrator.autostart = true;
    let mut opts = HubOptions::new(dir.path().join("data").join("twr.db"), config);
    let adapter: Arc<dyn Adapter> =
        Arc::new(CodexAdapter::with_bin(&bin).stop_grace(Duration::from_millis(500)));
    opts.adapters = Arc::new(move |name: &str| (name == codex::NAME).then(|| Arc::clone(&adapter)));
    let socket = dir.path().join("hub.sock");
    opts.socket = Some(socket.clone());
    opts.twr_exe = Some(PathBuf::from("/opt/twr/bin/twr"));
    let hub = Hub::open(opts).await.unwrap();

    let log = dir.path().join("stdin.jsonl");
    let start = timeout(WAIT, async {
        loop {
            if let Ok(s) = std::fs::read_to_string(&log) {
                if let Some(l) = s.lines().find(|l| l.contains("thread/start")) {
                    return parse(l);
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the orchestrator never started its thread");
    let p = &start["params"];
    assert_eq!(p["cwd"], dir.path().to_string_lossy().as_ref());
    assert_eq!(p["approvalsReviewer"], "user");
    assert_eq!(p["sandbox"], "read-only");
    assert!(p["developerInstructions"]
        .as_str()
        .is_some_and(|s| s.len() > 100));
    let twr = &p["config"]["mcp_servers"]["twr"];
    assert_eq!(twr["command"], "/opt/twr/bin/twr");
    assert_eq!(
        twr["args"],
        json!(["mcp", "--socket", socket.to_string_lossy()])
    );
    assert_eq!(twr["default_tools_approval_mode"], "approve");
    let token = twr["env"]["TWR_ORCH_TOKEN"].as_str().unwrap().to_string();
    assert!(token.len() >= 16);
    assert_eq!(p["config"]["features.shell_tool"], false);
    assert_eq!(
        p["config"]["mcp_servers"]["mine"],
        json!({"enabled": false})
    );
    assert_eq!(
        p["config"]["mcp_servers"].as_object().unwrap().len(),
        2,
        "{p}"
    );
    for f in codex::STRICT_MCP_FEATURES_OFF {
        assert_eq!(p["config"][format!("features.{f}")], false, "{f}");
    }
    // The config read came first, from the thread's directory.
    let stdin = std::fs::read_to_string(&log).unwrap();
    let read = stdin
        .lines()
        .position(|l| l.contains("config/read"))
        .unwrap();
    let open = stdin
        .lines()
        .position(|l| l.contains("thread/start"))
        .unwrap();
    assert!(read < open, "{stdin}");
    assert_eq!(
        parse(stdin.lines().nth(read).unwrap())["params"]["cwd"],
        dir.path().to_string_lossy().as_ref()
    );
    // Codex's own memory off: the orchestrator keeps twr's.
    for key in codex::MEMORY_OFF {
        assert_eq!(p["config"][*key], false, "{key}");
    }
    assert_eq!(
        p["config"].as_object().unwrap().len(),
        builtin_len() + 2 + codex::MEMORY_OFF.len(),
        "the built-in switches, plugins off, the MCP servers and memory off, nothing else"
    );
    let run = std::fs::read_to_string(dir.path().join("run.json")).unwrap();
    let run = parse(&run);
    assert_eq!(run["args"], json!(["app-server", "--stdio"]));
    assert!(!run["env"].to_string().contains(&token));
    hub.shutdown().await;
}

fn builtin_len() -> usize {
    codex::builtin_overrides(Some(&[])).len()
}

#[test]
fn turn_interruption_keeps_future_prompts_and_waits_for_the_runtime_id() {
    let (mut tr, _) = fed_until("approve", "\"id\":2,\"result\"");
    tr.send("notice", 1);
    tr.send("next user message", 2);
    assert!(tr.interrupt_turn().replies.is_empty());
    let bound =
        tr.translate(r#"{"id":3,"result":{"turn":{"id":"T-notice","status":"inProgress"}}}"#);
    assert_eq!(parse(&bound.replies[0])["method"], "turn/interrupt");
    let ended = tr.translate(r#"{"method":"turn/completed","params":{"threadId":"01a0eb1f-bbfe-7030-9892-8a0d81f3d695","turn":{"id":"T-notice","status":"interrupted","error":null}}}"#);
    assert!(ended
        .replies
        .iter()
        .any(|line| parse(line)["params"]["input"][0]["text"] == "next user message"));
}

/// The hub's interrupt of a turn goes out every time it asks (it asks again when the runtime
/// goes on with a turn it interrupted), and a failed `turn/interrupt` leaves no latch;
/// stopping still sends one.
#[test]
fn every_turn_interrupt_is_sent() {
    let (mut tr, _) = fed_until("approve", "\"id\":2,\"result\"");
    tr.send("go", 1);
    tr.translate(r#"{"id":3,"result":{"turn":{"id":"T-1","status":"inProgress"}}}"#);
    let first = tr.interrupt_turn();
    assert_eq!(parse(&first.replies[0])["method"], "turn/interrupt");
    let again = tr.interrupt_turn();
    assert_eq!(again.replies.len(), 1, "{again:?}");
    let id = parse(&again.replies[0])["id"].clone();
    assert_eq!(parse(&again.replies[0])["params"]["turnId"], "T-1");
    tr.translate(&format!(
        r#"{{"id":{id},"error":{{"code":-32600,"message":"no active turn"}}}}"#
    ));
    assert!(tr.interrupt().is_some(), "the failure left no latch");
    assert_eq!(tr.interrupt(), None, "stopping sends one");
}

#[tokio::test]
async fn turn_interrupt_during_startup_ends_the_notice_and_runs_the_next_prompt() {
    let dir = tempfile::tempdir().unwrap();
    let py = dir.path().join("startup.py");
    std::fs::write(
        &py,
        r#"
import json, sys, time
from pathlib import Path
root = Path(sys.argv[1])
log = (root / 'stdin.jsonl').open('w')
first = json.loads(sys.stdin.readline())
(root / 'initializing').touch()
while not (root / 'ready').exists():
    time.sleep(0.01)
def emit(v):
    print(json.dumps(v), flush=True)
emit({'id':first['id'], 'result':{'userAgent':'stub'}})
for line in sys.stdin:
    log.write(line)
    log.flush()
    request = json.loads(line)
    method = request.get('method')
    if method == 'thread/start':
        emit({'id':request['id'], 'result':{'thread':{'id':'startup-session'},
            'approvalPolicy':'untrusted', 'approvalsReviewer':'user', 'sandbox':{'type':'readOnly', 'networkAccess':False}}})
    elif method == 'turn/start':
        emit({'id':request['id'], 'result':{'turn':{'id':'user-turn', 'status':'inProgress'}}})
        emit({'method':'turn/completed', 'params':{'threadId':'startup-session',
            'turn':{'id':'user-turn', 'status':'completed', 'error':None}}})
"#,
    )
    .unwrap();
    let bin = dir.path().join("codex-startup");
    install_executable(
        &bin,
        &format!(
            "#!/bin/sh\nexec python3 {} {} \"$@\"\n",
            py.display(),
            dir.path().display()
        ),
    );
    let (handle, mut events) = start(
        &bin,
        SpawnConfig {
            cwd: dir.path().into(),
            mode: "manual".into(),
            prompt_turn: Some(1),
            ..Default::default()
        },
        Some("background notice"),
    );
    timeout(WAIT, async {
        while !dir.path().join("initializing").exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    handle.interrupt().unwrap();
    let cancelled = until(&mut events, |e| is_kind(e, "turn.ended")).await;
    assert!(cancelled.iter().any(|e| matches!(e, AdapterEvent::Emit {
        body: EventBody::TurnEnded(p), turn:Some(1), .. } if p.result == TurnResult::Interrupted)));
    assert!(!cancelled
        .iter()
        .any(|e| matches!(e, AdapterEvent::Exited { .. })));
    handle.send("next user message", 2).unwrap();
    std::fs::write(dir.path().join("ready"), "").unwrap();
    let finished = until(&mut events, |e| is_kind(e, "turn.ended")).await;
    assert!(finished.iter().any(|e| matches!(e, AdapterEvent::Emit {
        body: EventBody::TurnEnded(p), turn:Some(2), .. } if p.result == TurnResult::Completed)));
    let log = std::fs::read_to_string(dir.path().join("stdin.jsonl")).unwrap();
    assert!(!log.contains("background notice"));
    assert!(log.contains("next user message"));
    handle.stop().unwrap();
    until(&mut events, |e| matches!(e, AdapterEvent::Exited { .. })).await;
}

/// The orchestrator's MCP allowlist: Codex merges `config.mcp_servers` into the user's and the
/// project's servers, so the translator reads the effective config (`config/read` from the
/// thread's directory) and turns every other server off by name before opening the thread. A
/// server of the config under an allowlisted name, a failed read, or a server starting that
/// is not in the set ends the session.
#[test]
fn an_mcp_allowlist_turns_the_users_servers_off() {
    let strict = SpawnConfig {
        mcp_config: Some(json!({"mcpServers": {"twr": {"command": "twr"}}})),
        builtin_tools: Some(vec![]),
        ..cfg("manual")
    };
    // Up to the config read: initialize, then `initialized` and `config/read`, no thread yet.
    let opened = |config: Value| {
        let mut tr = new_translator(&strict, TranslatorOptions::default());
        tr.start();
        let out = tr.translate(r#"{"id":1,"result":{}}"#);
        let read = out
            .replies
            .iter()
            .map(|l| parse(l))
            .find(|r| r["method"] == "config/read")
            .expect("config/read before the thread opens");
        assert_eq!(read["params"]["cwd"], FIXTURE_CWD);
        assert!(!out.replies.iter().any(|l| l.contains("thread/start")));
        let out = tr.translate(&json!({"id": read["id"], "result": config}).to_string());
        (tr, out)
    };
    let (mut tr, out) = opened(json!({"config": {"mcp_servers": {
        "fs": {"command": "fs-mcp", "enabled": true},
        "proj": {"url": "http://localhost:1"},
    }}}));
    assert!(out.fatal.is_none(), "{:?}", out.fatal);
    let start = parse(&out.replies[0]);
    assert_eq!(start["method"], "thread/start");
    let c = &start["params"]["config"];
    assert_eq!(
        c["mcp_servers"],
        json!({"twr": {"command": "twr"}, "fs": {"enabled": false}, "proj": {"enabled": false}})
    );
    for f in codex::STRICT_MCP_FEATURES_OFF {
        assert_eq!(c[format!("features.{f}")], false, "{f}");
    }
    // Its own server starting is fine; any other ends the session.
    let status = |name: &str| {
        json!({"method": "mcpServer/startupStatus/updated",
               "params": {"threadId": "th", "name": name, "status": "starting"}})
        .to_string()
    };
    assert!(tr.translate(&status("twr")).fatal.is_none());
    let fatal = tr.translate(&status("codex_apps")).fatal.unwrap();
    assert!(fatal.contains("codex_apps"), "{fatal}");
    // A config with no servers: only twr's.
    let (_, out) = opened(json!({"config": {"mcp_servers": null}}));
    let start = parse(&out.replies[0]);
    assert_eq!(
        start["params"]["config"]["mcp_servers"],
        json!({"twr": {"command": "twr"}})
    );
    // The config's own `twr`: Codex would merge the two.
    let (_, out) = opened(json!({"config": {"mcp_servers": {"twr": {"command": "evil"}}}}));
    assert!(out.fatal.unwrap().contains("\"twr\""));
    assert!(out.replies.is_empty());
    // A read that fails: nothing opens.
    let mut tr = new_translator(&strict, TranslatorOptions::default());
    tr.start();
    let out = tr.translate(r#"{"id":1,"result":{}}"#);
    let read = out
        .replies
        .iter()
        .map(|l| parse(l))
        .find(|r| r["method"] == "config/read")
        .unwrap();
    let out = tr.translate(
        &json!({"id": read["id"], "error": {"code": -32601, "message": "unknown method"}})
            .to_string(),
    );
    assert!(out.fatal.unwrap().contains("config/read"));
    assert!(out.replies.is_empty());
    // A thread without a set reads no config and keeps Codex's servers.
    let mut tr = new_translator(&cfg("manual"), TranslatorOptions::default());
    tr.start();
    let out = tr.translate(r#"{"id":1,"result":{}}"#);
    assert!(!out.replies.iter().any(|l| l.contains("config/read")));
    assert!(tr.translate(&status("anything")).fatal.is_none());
}

/// `codex resume` keeps the built-in tool groups the thread turned off (`-c` overrides), and
/// there is no attach command for a thread with an MCP allowlist (the interactive CLI would
/// load the user's servers) nor for a session id that would read as an option.
#[test]
fn attach_keeps_builtins_off_and_refuses_what_it_cannot_carry() {
    let no_tools = SpawnConfig {
        builtin_tools: Some(vec![]),
        ..cfg("manual")
    };
    let cmd = CodexAdapter::with_bin("/opt/codex")
        .attach_command(&no_tools, "th-1")
        .unwrap();
    let args: Vec<String> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    for (k, v) in codex::builtin_overrides(Some(&[])) {
        let want = format!("{k}={v}");
        assert!(
            args.windows(2).any(|w| w[0] == "-c" && w[1] == want),
            "{want}: {args:?}"
        );
    }
    assert!(
        args.iter().any(|a| a == "features.shell_tool=false"),
        "{args:?}"
    );
    let strict = SpawnConfig {
        mcp_config: Some(json!({"mcpServers": {}})),
        ..no_tools.clone()
    };
    let a = CodexAdapter::with_bin("/opt/codex");
    assert!(a.attach_command(&strict, "th-1").is_none());
    assert!(a.remote_attach_command(&strict, "th-1").is_none());
    for bad in [
        "--dangerously-bypass-approvals-and-sandbox",
        "",
        "a b",
        "x\ny",
    ] {
        assert!(a.attach_command(&cfg("manual"), bad).is_none(), "{bad:?}");
    }
}
