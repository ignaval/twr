//! Claude Code adapter tests. No real `claude` runs here.
//!
//! Fixtures (`tests/fixtures/claude/<name>.both.jsonl`) are real Claude Code 2.1.284 sessions
//! recorded with `--model haiku`, both directions interleaved: `>> ` lines were written to the
//! CLI's stdin, `<< ` lines came from its stdout. They were trimmed and redacted before
//! committing: the optional `initialize` handshake (which returns account details), the user's
//! SessionStart hook lines and `input_json_delta` stream events were dropped, `system/init`
//! was cut down to a few keys, thinking signatures and paths were replaced.
//!
//! - Translator tests replay each fixture: stdout lines go through [`Translator::translate`];
//!   at every stdin line the test produces the adapter's own line for the same action (user
//!   message, respond, interrupt) and checks it matches the recorded one.
//! - Driver tests run the adapter against a stub `claude` (python3) that replays a fixture,
//!   and check the stdin the adapter wrote, its arguments, env, cwd and stop behavior.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::time::timeout;
use twr_adapters::claude_code::translate::{DEFAULT_DECLINE_MESSAGE, DEFAULT_DENY_MESSAGE};
use twr_adapters::claude_code::TranslatorOptions;
use twr_adapters::claude_code::{
    self, build_args, ClaudeCodeAdapter, Emitted, Translated, Translator,
};
use twr_core::adapter::{Adapter, AdapterEvent, AdapterHandle, Answer, SpawnConfig};
use twr_core::event::*;
use twr_core::usage::UsageWindow;

const WAIT: Duration = Duration::from_secs(20);

fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/claude")
        .join(format!("{name}.both.jsonl"))
}

/// (direction, parsed line) pairs; direction is `>>` (to the CLI) or `<<` (from it).
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

/// The answer a user would give to produce the recorded `control_response`.
fn answer_for(recorded: &Value) -> Answer {
    let r = &recorded["response"]["response"];
    match r["behavior"].as_str().unwrap() {
        "allow" => match r["updatedInput"]["answers"].as_object() {
            Some(a) => Answer::new(a.values().next().unwrap().as_str().unwrap()),
            None => Answer::allow(),
        },
        _ => Answer {
            decision: "deny".into(),
            message: r["message"].as_str().map(str::to_string),
            ..Default::default()
        },
    }
}

struct Replay {
    events: Vec<Emitted>,
}

/// Replays a fixture through a translator (turns numbered from `first_turn`).
fn replay_with(name: &str, opts: TranslatorOptions, first_turn: u64) -> Replay {
    replay_lines(fixture(name), opts, first_turn)
}

/// Replays (direction, line) pairs through a translator (turns numbered from `first_turn`).
fn replay_lines(lines: Vec<(String, String)>, opts: TranslatorOptions, first_turn: u64) -> Replay {
    let mut tr = Translator::new(opts);
    let mut events = Vec::new();
    let mut turn = first_turn;
    for (dir, line) in lines {
        let rec = parse(&line);
        if dir == "<<" {
            let out = tr.translate(&line);
            assert!(out.reply.is_none(), "unexpected reply to {line}");
            events.extend(out.events);
            continue;
        }
        let ours = match rec["type"].as_str().unwrap() {
            "user" => {
                let text = rec["message"]["content"].as_str().unwrap();
                let l = tr.user_message(text, turn);
                turn += 1;
                l
            }
            "control_response" => {
                let id = rec["response"]["request_id"].as_str().unwrap();
                assert!(tr.pending_attention().contains(&id.to_string()));
                let out = tr
                    .respond(id, &answer_for(&rec))
                    .expect("respond to a pending attention");
                assert!(out.events.is_empty());
                out.reply.expect("a reply for a complete answer")
            }
            "control_request" => {
                assert_eq!(rec["request"]["subtype"], "interrupt");
                let l = tr.interrupt().expect("a turn is running");
                let mut v = parse(&l);
                v["request_id"] = rec["request_id"].clone();
                v.to_string()
            }
            other => panic!("unexpected stdin line type {other}"),
        };
        assert_eq!(parse(&ours), rec, "stdin line differs from the recording");
    }
    Replay { events }
}

fn replay(name: &str) -> Replay {
    replay_with(name, TranslatorOptions::default(), 1)
}

/// `kind@turn`, with runs of text deltas collapsed into one `delta@turn`.
fn shape(events: &[Emitted]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for e in events {
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
    m.as_ref().and_then(|m| m.get(key)).unwrap_or(&Value::Null)
}

// ---------------------------------------------------------------------------------------------
// Translator fixture replays
// ---------------------------------------------------------------------------------------------

#[test]
fn permission_allow_and_multi_turn() {
    let opts = TranslatorOptions {
        pid: Some(4242),
        exe: Some("claude".into()),
        ..Default::default()
    };
    let r = replay_with("session_allow_multiturn", opts, 1);
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
            "delta@2",
            "text@2",
            "turn.ended@2",
            "delta@3",
            "text@3",
            "turn.ended@3",
        ]
    );
    // One thread.started per process, with the pid, although init repeats every turn.
    let s = r.started();
    assert_eq!(s.len(), 1);
    assert_eq!(
        s[0].session_id.as_deref(),
        Some("0496081e-2080-4641-a4d0-1b600fc0bc2b")
    );
    assert_eq!(ext(&s[0].ext, "twr")["pid"], 4242);
    assert_eq!(ext(&s[0].ext, "twr")["exe"], "claude");
    assert_eq!(
        ext(&s[0].ext, "claude")["model"],
        "claude-haiku-4-5-20251001"
    );

    let call = r.calls()[0];
    assert_eq!(call.tool, "Bash");
    assert_eq!(call.input_summary, "touch hello.txt");
    assert!(call.risky);
    assert!(call.input_ref.is_none());

    let a = r.raised()[0];
    assert_eq!(a.reason, AttentionReason::Permission);
    assert_eq!(a.attn_id, "825b8303-c6b5-42b8-aee4-3b0cad0387e7");
    assert_eq!(a.summary, "Allow Bash: touch hello.txt");
    assert_eq!(a.options, ["allow", "deny"]);
    assert_eq!(a.call_id.as_deref(), Some(call.call_id.as_str()));

    let res = r.results()[0];
    assert_eq!(res.call_id, call.call_id);
    assert!(res.ok);
    assert_eq!(res.output_summary, "(Bash completed with no output)");
    assert_eq!(ext(&res.ext, "claude")["tool"], "Bash");

    assert_eq!(r.finals(), ["done", "hello.txt", "three"]);
    assert_eq!(r.deltas(1), "done");
    assert_eq!(r.deltas(2), "hello.txt");

    // total_cost_usd is cumulative (0.0169994, 0.0199333, 0.0227686): costs are deltas.
    let ended = r.ended();
    let costs: Vec<f64> = ended
        .iter()
        .map(|e| e.usage.as_ref().unwrap().cost.unwrap())
        .collect();
    assert!((costs[0] - 0.0169994).abs() < 1e-9, "{costs:?}");
    assert!((costs[1] - 0.0029339).abs() < 1e-9, "{costs:?}");
    assert!((costs[2] - 0.0028353).abs() < 1e-9, "{costs:?}");
    let e = ended[0];
    assert_eq!(e.result, TurnResult::Completed);
    assert_eq!(
        e.session_id.as_deref(),
        Some("0496081e-2080-4641-a4d0-1b600fc0bc2b")
    );
    assert_eq!(e.duration_ms, Some(3441));
    let u = e.usage.as_ref().unwrap();
    assert_eq!(u.input, 18 + 5944 + 41734);
    assert_eq!(u.output, 184);
    let c = ext(&e.ext, "claude");
    assert_eq!(c["num_turns"], 2);
    assert_eq!(c["stop_reason"], "end_turn");
    assert_eq!(c["terminal_reason"], "completed");
    assert_eq!(c["permission_denials"], json!([]));
    assert_eq!(c["cache_read_input_tokens"], 41734);
    assert!(c.get("transient").is_none());
    // Non-delta events keep the raw line; deltas do not.
    for ev in &r.events {
        let delta = matches!(&ev.body, EventBody::AgentText(t) if t.delta);
        assert_eq!(ev.raw.is_none(), delta, "{:?}", ev.body.kind());
    }
}

#[test]
fn permission_deny() {
    let r = replay("session_deny");
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
    let a = r.raised()[0];
    assert_eq!(a.summary, "Allow Bash: touch denied.txt");
    assert_eq!(
        ext(&a.ext, "claude")["blocked_path"],
        "/tmp/sandbox-claude/denied.txt"
    );
    let res = r.results()[0];
    assert!(!res.ok);
    assert_eq!(res.output_summary, "Host denied: not allowed in this test");
    let e = r.ended()[0];
    assert_eq!(e.result, TurnResult::Completed);
    let denials = &ext(&e.ext, "claude")["permission_denials"];
    assert_eq!(denials[0]["tool_use_id"], "toolu_014utEiQNMrgKWoTPHRefjwA");
}

#[test]
fn deny_without_message_uses_the_default() {
    let mut tr = Translator::new(TranslatorOptions::default());
    tr.user_message("x", 1);
    for (dir, line) in fixture("session_deny") {
        if dir == "<<" {
            tr.translate(&line);
            if line.contains("can_use_tool") {
                break;
            }
        }
    }
    let id = "903c090d-23fd-4290-bc59-de48762538d7";
    assert!(tr.respond("unknown", &Answer::deny()).is_none());
    let v = parse(&tr.respond(id, &Answer::deny()).unwrap().reply.unwrap());
    assert_eq!(
        v["response"]["response"],
        json!({"behavior": "deny", "message": DEFAULT_DENY_MESSAGE})
    );
    // Answered once: a second respond is ignored.
    assert!(tr.respond(id, &Answer::allow()).is_none());
}

#[test]
fn write_permission() {
    let r = replay("write_allow");
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
    let call = r.calls()[0];
    assert_eq!(call.tool, "Write");
    assert_eq!(call.input_summary, "/tmp/sandbox-claude/hi.txt");
    assert!(call.risky);
    // The content it would write shows after the path.
    assert_eq!(
        r.raised()[0].summary,
        r#"Allow Write: /tmp/sandbox-claude/hi.txt ⟦{"content":"hi"}⟧"#
    );
    assert!(r.results()[0].ok);
}

#[test]
fn ask_user_question() {
    let r = replay("ask_user_question");
    // Recorded without --include-partial-messages: no deltas.
    assert_eq!(
        shape(&r.events),
        [
            "thread.started",
            "agent.tool_call@1",
            "attention.raised@1",
            "agent.tool_result@1",
            "text@1",
            "turn.ended@1",
        ]
    );
    let call = r.calls()[0];
    assert_eq!(call.tool, "AskUserQuestion");
    assert_eq!(call.input_summary, "Red or blue?");
    assert!(!call.risky);
    let a = r.raised()[0];
    assert_eq!(a.reason, AttentionReason::Question);
    assert_eq!(a.summary, "Red or blue?");
    assert_eq!(a.options, ["Red", "Blue"]);
    assert_eq!(a.call_id.as_deref(), Some("toolu_01UX4QRif7CBkejkbTEw8uR1"));
    assert_eq!(ext(&a.ext, "claude")["header"], "Color");
    assert_eq!(r.finals(), ["Blue."]);
}

/// A translator with one turn running and `request` (a `can_use_tool` request object) raised.
fn raise(request: Value) -> (Translator, AttentionRaised) {
    let mut tr = Translator::new(TranslatorOptions::default());
    tr.user_message("x", 1);
    tr.translate(r#"{"type":"system","subtype":"init","session_id":"s"}"#);
    let line = json!({"type": "control_request", "request_id": "req-1", "request": request});
    let out = tr.translate(&line.to_string());
    assert!(out.reply.is_none());
    let EventBody::AttentionRaised(a) = &out.events[0].body else {
        panic!("{:?}", out.events)
    };
    assert_eq!(out.events[0].turn, Some(1));
    let a = a.clone();
    (tr, a)
}

/// The `response.response` of a control_response line.
fn inner(line: &str) -> Value {
    let v = parse(line);
    assert_eq!(v["type"], "control_response");
    assert_eq!(v["response"]["subtype"], "success");
    assert_eq!(v["response"]["request_id"], "req-1");
    v["response"]["response"].clone()
}

/// Answers `attn_id`, which must complete its request without other events; returns the
/// `response.response` written.
fn answer(tr: &mut Translator, attn_id: &str, a: &Answer) -> Value {
    let out = tr.respond(attn_id, a).expect("pending");
    assert!(out.events.is_empty(), "{:?}", out.events);
    inner(&out.reply.expect("a reply"))
}

fn ask_request(questions: Value) -> Value {
    json!({
        "subtype": "can_use_tool",
        "tool_name": "AskUserQuestion",
        "display_name": "AskUserQuestion",
        "input": {"questions": questions},
        "tool_use_id": "toolu_q",
        "requires_user_interaction": true,
    })
}

fn color_question() -> Value {
    json!([{"question": "Red or blue?", "header": "Color", "options": [
        {"label": "Red", "description": "r"}, {"label": "Blue", "description": "b"}
    ], "multiSelect": false}])
}

/// A permission card shows every field of the input that changes what is approved, not only
/// the command or path: a Bash call's flags (`dangerouslyDisableSandbox`, `run_in_background`,
/// a field twr does not know) after its command, between `⟦` `⟧`, marked like the command (its
/// `description` and `timeout` aside); a long one cut there, with the whole input in
/// `full_input`. The tool name is marked too. The card once showed only the command, so a
/// request to run it outside the sandbox read the same as one inside it.
#[test]
fn a_permission_card_shows_the_inputs_other_fields() {
    let bash = |input: Value| {
        raise(
            json!({"subtype": "can_use_tool", "tool_name": "Bash", "input": input,
                     "tool_use_id": "toolu_b"}),
        )
        .1
    };
    let a = bash(json!({"command": "ls", "description": "list", "timeout": 5}));
    assert_eq!(a.summary, "Allow Bash: ls");
    let a = bash(json!({"command": "ls", "description": "list",
                        "dangerouslyDisableSandbox": true, "run_in_background": true}));
    assert_eq!(
        a.summary,
        r#"Allow Bash: ls ⟦{"dangerouslyDisableSandbox":true,"run_in_background":true}⟧"#
    );
    assert_eq!(a.options, ["allow", "deny"]);
    assert!(ext(&a.ext, "claude").get("full_input").is_none());
    // An unknown field, marked; a command that is not a string is shown as a field.
    let a = bash(json!({"command": ["rm", "-rf", "/"], "x\u{202e}": "a\nb"}));
    assert_eq!(
        a.summary,
        r#"Allow Bash: ⟦{"command":["rm","-rf","/"],"x⟨U+202E⟩":"a\nb"}⟧"#
    );
    // Long flags: cut on the summary, whole in `full_input`.
    let domains: Vec<String> = (0..40).map(|i| format!("host{i}.example.com")).collect();
    let a = bash(json!({"command": "curl x", "allowed_domains": domains}));
    assert!(
        a.summary
            .ends_with(" more characters; open the card to see all⟧"),
        "{}",
        a.summary
    );
    let full = ext(&a.ext, "claude")["full_input"].as_str().unwrap();
    assert_eq!(
        full,
        format!("curl x ⟦{}⟧", json!({ "allowed_domains": domains }))
    );
    // A file write's content, and a tool name with an invisible character in it.
    let (_, a) = raise(
        json!({"subtype": "can_use_tool", "tool_name": "mcp__x__\u{202e}y",
                              "input": {"a": 1}, "tool_use_id": "toolu_m"}),
    );
    assert_eq!(a.summary, r#"Allow mcp__x__⟨U+202E⟩y: {"a":1}"#);
    let (_, a) = raise(json!({"subtype": "can_use_tool", "tool_name": "Edit",
        "input": {"file_path": "/a", "old_string": "x", "new_string": "y"},
        "tool_use_id": "toolu_e"}));
    assert_eq!(
        a.summary,
        r#"Allow Edit: /a ⟦{"new_string":"y","old_string":"x"}⟧"#
    );
}

#[test]
fn exit_plan_mode_is_a_permission() {
    // Same shape as AskUserQuestion's can_use_tool, but not a question: allow/deny only.
    let request = json!({
        "subtype": "can_use_tool",
        "tool_name": "ExitPlanMode",
        "display_name": "ExitPlanMode",
        "input": {"plan": "1. Edit main.rs\n2. Run tests"},
        "tool_use_id": "toolu_p",
        "requires_user_interaction": true,
    });
    let (mut tr, a) = raise(request.clone());
    assert_eq!(a.reason, AttentionReason::Permission);
    assert_eq!(a.options, ["allow", "deny"]);
    // Its lines stay visible, as in any permission's input.
    assert_eq!(
        a.summary,
        "Allow ExitPlanMode: 1. Edit main.rs⏎2. Run tests"
    );
    assert_eq!(ext(&a.ext, "claude")["requires_user_interaction"], true);
    assert_eq!(ext(&a.ext, "claude")["tool"], "ExitPlanMode");

    // Free text (or an option-looking label) on a permission denies.
    let r = answer(&mut tr, "req-1", &Answer::new("sure, go ahead"));
    assert_eq!(
        r,
        json!({"behavior": "deny", "message": DEFAULT_DENY_MESSAGE})
    );

    let (mut tr, _) = raise(request);
    let r = answer(&mut tr, "req-1", &Answer::allow());
    assert_eq!(
        r,
        json!({"behavior": "allow", "updatedInput": {"plan": "1. Edit main.rs\n2. Run tests"}})
    );
}

fn exit_plan_request() -> Value {
    json!({
        "subtype": "can_use_tool",
        "tool_name": "ExitPlanMode",
        "input": {"plan": "1. Edit main.rs"},
        "tool_use_id": "toolu_p",
        "requires_user_interaction": true,
    })
}

/// A translator started in `mode` with turn 1 queued.
fn in_mode(mode: &str) -> Translator {
    let mut tr = Translator::new(TranslatorOptions {
        mode: mode.into(),
        ..Default::default()
    });
    tr.user_message("x", 1);
    tr
}

fn init_in(mode: &str) -> String {
    json!({"type": "system", "subtype": "init", "session_id": "s", "permissionMode": mode})
        .to_string()
}

fn turn_errors(out: &Translated) -> Vec<Option<u64>> {
    out.events
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::TurnEnded(t) if t.result == TurnResult::Error => Some(e.turn),
            _ => None,
        })
        .collect()
}

/// Every `permissionMode` Claude reports (each turn's `system/init`, and any
/// other system message carrying one) is checked against the thread's mode. A wider (or
/// unknown) mode interrupts the turn, ends every waiting turn with an error, reads nothing
/// more and stops the session; a narrower or same-level one is fine.
#[test]
fn a_session_reporting_a_wider_mode_is_ended() {
    // At the first init: no thread.started, the queued turn ends with an error.
    let mut tr = in_mode("default");
    tr.user_message("y", 2);
    let out = tr.translate(&init_in("bypassPermissions"));
    let why = out.fatal.clone().expect("fatal");
    assert!(why.contains("bypassPermissions"), "{why}");
    assert!(!out
        .events
        .iter()
        .any(|e| matches!(e.body, EventBody::ThreadStarted(_))));
    assert_eq!(turn_errors(&out), [Some(1), Some(2)]);
    let interrupt = parse(out.reply.as_deref().expect("the turn is interrupted"));
    assert_eq!(interrupt["request"]["subtype"], "interrupt");
    // Nothing more is read, and nothing is answered.
    assert_eq!(
        tr.translate(r#"{"type":"result","subtype":"success","is_error":false}"#),
        Translated::default()
    );

    // Mid-turn, through a system message other than init.
    let mut tr = in_mode("manual");
    assert!(
        tr.translate(&init_in("default")).fatal.is_none(),
        "same level"
    );
    let out = tr.translate(
        r#"{"type":"system","subtype":"status","status":null,"permissionMode":"acceptEdits"}"#,
    );
    assert!(out.fatal.is_some(), "{out:?}");
    assert_eq!(turn_errors(&out), [Some(1)]);
    assert!(out.reply.is_some());

    // Narrower is fine; an unknown report is not.
    let mut tr = in_mode("acceptEdits");
    assert!(tr.translate(&init_in("plan")).fatal.is_none());
    let mut tr = in_mode("default");
    assert!(tr.translate(&init_in("someNewMode")).fatal.is_some());
    // No permissionMode at all: nothing to check (the recorded fixtures).
    let mut tr = in_mode("plan");
    assert!(tr
        .translate(r#"{"type":"system","subtype":"init","session_id":"s"}"#)
        .fatal
        .is_none());
}

/// The one widening allowed is plan → an ask-level mode after the user
/// approved an `ExitPlanMode` request in this session; it is recorded on the turn's end. Any
/// further widening still ends the session, and without the approval (or after a deny) leaving
/// plan mode does too.
#[test]
fn leaving_plan_mode_needs_the_users_approval() {
    let plan_turn = |answer: Option<Answer>| {
        let mut tr = in_mode("plan");
        assert!(tr.translate(&init_in("plan")).fatal.is_none());
        let line = json!({"type": "control_request", "request_id": "req-1",
            "request": exit_plan_request()});
        tr.translate(&line.to_string());
        if let Some(a) = answer {
            tr.respond("req-1", &a).expect("pending");
        }
        tr
    };
    let status = |mode: &str| {
        json!({"type": "system", "subtype": "status", "permissionMode": mode}).to_string()
    };
    let mut tr = plan_turn(Some(Answer::allow()));
    let out = tr.translate(&status("default"));
    assert!(out.fatal.is_none(), "{out:?}");
    let out =
        tr.translate(r#"{"type":"result","subtype":"success","is_error":false,"session_id":"s"}"#);
    let EventBody::TurnEnded(t) = &out.events[0].body else {
        panic!("{out:?}")
    };
    assert_eq!(
        ext(&t.ext, "claude")["mode_changed"],
        json!({"from": "plan", "to": "default", "consent": "ExitPlanMode approved by the user"})
    );
    assert!(ext(&t.ext, "claude")["mode_note"]
        .as_str()
        .unwrap()
        .contains("ExitPlanMode"));
    // The next turn runs in default; anything wider still ends the session.
    tr.user_message("next", 2);
    assert!(tr.translate(&init_in("default")).fatal.is_none());
    assert!(tr.translate(&init_in("plan")).fatal.is_none(), "narrower");
    assert!(tr.translate(&status("acceptEdits")).fatal.is_some());

    // Approved, but Claude jumps past the ask level: ended.
    let mut tr = plan_turn(Some(Answer::allow()));
    assert!(tr.translate(&status("acceptEdits")).fatal.is_some());
    // Not approved (no answer yet, or denied): ended.
    let mut tr = plan_turn(None);
    assert!(tr.translate(&status("default")).fatal.is_some());
    let mut tr = plan_turn(Some(Answer::deny()));
    assert!(tr.translate(&status("default")).fatal.is_some());
    // Another tool's approval is no consent.
    let mut tr = in_mode("plan");
    tr.translate(&init_in("plan"));
    let mut req = exit_plan_request();
    req["tool_name"] = json!("Bash");
    tr.translate(
        &json!({"type": "control_request", "request_id": "req-1", "request": req}).to_string(),
    );
    tr.respond("req-1", &Answer::allow()).unwrap();
    assert!(tr.translate(&status("default")).fatal.is_some());
}

/// Claude's own ids pass as written, other spellings of them and
/// other known modes map (restricted → plan), and an unknown mode neither starts nor attaches.
#[test]
fn modes_map_by_level_and_unknown_modes_are_refused() {
    let mode = |m: &str| {
        claude_code::permission_mode(&SpawnConfig {
            mode: m.into(),
            ..Default::default()
        })
    };
    for (m, want) in [
        ("", "default"),
        ("manual", "manual"),
        ("dontAsk", "dontAsk"),
        ("dont_ask", "dontAsk"),
        ("read-only", "plan"),
        ("ask", "default"),
        ("autoEdit", "acceptEdits"),
        ("yolo", "bypassPermissions"),
    ] {
        assert_eq!(mode(m).unwrap(), want, "{m}");
    }
    let cfg = SpawnConfig {
        mode: "made-up".into(),
        ..Default::default()
    };
    let Err(e) = ClaudeCodeAdapter::with_bin("/bin/false").spawn(cfg.clone(), None) else {
        panic!("an unknown mode must not start");
    };
    assert!(e.to_string().contains("unknown permission mode"), "{e}");
    assert!(ClaudeCodeAdapter::with_bin("/bin/false")
        .attach_command(&cfg, "s")
        .is_none());
}

#[test]
fn questions_input_on_another_tool_is_a_permission() {
    // An MCP tool whose input looks like AskUserQuestion's is still an allow/deny permission:
    // an option-looking or free-text answer denies, it never becomes an allow.
    let mut request = ask_request(json!([{"question": "Deploy to production?", "options": [
        {"label": "Deploy now", "description": "d"},
        {"label": "Cancel deployment", "description": "c"}
    ]}]));
    request["tool_name"] = json!("mcp__deploy");
    request["display_name"] = json!("mcp__deploy");
    let (mut tr, a) = raise(request.clone());
    assert_eq!(a.reason, AttentionReason::Permission);
    assert_eq!(a.attn_id, "req-1");
    assert_eq!(a.options, ["allow", "deny"]);
    assert!(
        a.summary.starts_with("Allow mcp__deploy: "),
        "{}",
        a.summary
    );
    assert_eq!(ext(&a.ext, "claude")["tool"], "mcp__deploy");
    for text in ["Cancel deployment", "Deploy now"] {
        let (mut tr, _) = raise(request.clone());
        assert_eq!(
            answer(&mut tr, "req-1", &Answer::new(text)),
            json!({"behavior": "deny", "message": DEFAULT_DENY_MESSAGE})
        );
    }
    let r = answer(&mut tr, "req-1", &Answer::allow());
    assert_eq!(r["behavior"], "allow");
    assert_eq!(r["updatedInput"], request["input"]);
    assert!(r["updatedInput"].get("answers").is_none());
}

#[test]
fn question_decline_wire_shape() {
    for decision in ["deny", "decline"] {
        let (mut tr, a) = raise(ask_request(color_question()));
        assert_eq!(a.reason, AttentionReason::Question);
        let r = answer(&mut tr, "req-1", &Answer::new(decision));
        assert_eq!(
            r,
            json!({"behavior": "deny", "message": DEFAULT_DECLINE_MESSAGE})
        );
        assert!(tr.pending_attention().is_empty());
    }
    // A decline message is passed on.
    let (mut tr, _) = raise(ask_request(color_question()));
    let not_now = Answer {
        decision: "deny".into(),
        message: Some("not now".into()),
        ..Default::default()
    };
    let r = answer(&mut tr, "req-1", &not_now);
    assert_eq!(r, json!({"behavior": "deny", "message": "not now"}));
    // A bare allow names no option: declined, not allowed with a made-up answer.
    let (mut tr, _) = raise(ask_request(color_question()));
    let r = answer(&mut tr, "req-1", &Answer::allow());
    assert_eq!(r["behavior"], "deny");
    // Free text is AskUserQuestion's "Other" answer.
    let (mut tr, _) = raise(ask_request(color_question()));
    let r = answer(&mut tr, "req-1", &Answer::new("Green"));
    assert_eq!(r["behavior"], "allow");
    assert_eq!(
        r["updatedInput"]["answers"],
        json!({"Red or blue?": "Green"})
    );
}

fn two_questions() -> Value {
    json!([
        {"question": "Red or blue?", "header": "Color", "options": [
            {"label": "Red", "description": "r"}, {"label": "Blue", "description": "b"}
        ], "multiSelect": false},
        {"question": "Size?", "header": "Size", "options": [
            {"label": "Small", "description": "s"}, {"label": "Large", "description": "l"}
        ], "multiSelect": true},
    ])
}

/// Raises a two-question AskUserQuestion; returns the translator and both items.
fn raise_two() -> (Translator, Vec<AttentionRaised>) {
    let mut tr = Translator::new(TranslatorOptions::default());
    tr.user_message("x", 1);
    tr.translate(r#"{"type":"system","subtype":"init","session_id":"s"}"#);
    let line = json!({"type": "control_request", "request_id": "req-1",
                      "request": ask_request(two_questions())});
    let out = tr.translate(&line.to_string());
    assert!(out.reply.is_none());
    let raised = out
        .events
        .iter()
        .map(|e| {
            assert_eq!(e.turn, Some(1));
            match &e.body {
                EventBody::AttentionRaised(a) => a.clone(),
                other => panic!("{other:?}"),
            }
        })
        .collect();
    (tr, raised)
}

#[test]
fn multi_question_raises_one_item_per_question() {
    let (tr, raised) = raise_two();
    assert_eq!(raised.len(), 2);
    let (a, b) = (&raised[0], &raised[1]);
    assert_eq!(a.attn_id, "req-1#q0");
    assert_eq!(b.attn_id, "req-1#q1");
    assert_eq!(a.summary, "(1/2) Red or blue?");
    assert_eq!(b.summary, "(2/2) Size?");
    assert_eq!(a.options, ["Red", "Blue"]);
    assert_eq!(b.options, ["Small", "Large"]);
    for x in [a, b] {
        assert_eq!(x.reason, AttentionReason::Question);
        assert_eq!(x.call_id.as_deref(), Some("toolu_q"));
        assert_eq!(ext(&x.ext, "claude")["request_id"], "req-1");
        assert_eq!(ext(&x.ext, "claude")["question_count"], 2);
    }
    assert_eq!(ext(&a.ext, "claude")["header"], "Color");
    assert_eq!(ext(&b.ext, "claude")["header"], "Size");
    assert_eq!(ext(&b.ext, "claude")["multi_select"], true);
    assert!(ext(&a.ext, "claude").get("multi_select").is_none());
    assert_eq!(tr.pending_attention(), ["req-1#q0", "req-1#q1"]);
}

#[test]
fn multi_question_answers_on_the_wire() {
    for order in [[0, 1], [1, 0]] {
        let (mut tr, raised) = raise_two();
        // The request id itself is not an answerable item.
        assert!(tr.respond("req-1", &Answer::new("Blue")).is_none());
        let answers = ["Blue", "a medium one"];
        let first = tr
            .respond(&raised[order[0]].attn_id, &Answer::new(answers[order[0]]))
            .unwrap();
        // Nothing is sent before every question has an answer.
        assert!(
            first.reply.is_none() && first.events.is_empty(),
            "{first:?}"
        );
        assert_eq!(tr.pending_attention(), [raised[order[1]].attn_id.clone()]);
        // An answered item cannot be answered again.
        assert!(tr
            .respond(&raised[order[0]].attn_id, &Answer::new("Red"))
            .is_none());
        let r = answer(
            &mut tr,
            &raised[order[1]].attn_id,
            &Answer::new(answers[order[1]]),
        );
        assert_eq!(
            r,
            json!({"behavior": "allow", "updatedInput": {
                "questions": two_questions(),
                "answers": {"Red or blue?": "Blue", "Size?": "a medium one"},
            }})
        );
        assert!(tr.pending_attention().is_empty());
    }
    // A `|` is part of the answer, not a separator.
    let (mut tr, _) = raise_two();
    tr.respond("req-1#q0", &Answer::new("Red | Large")).unwrap();
    let r = answer(&mut tr, "req-1#q1", &Answer::new("Small"));
    assert_eq!(
        r["updatedInput"]["answers"],
        json!({"Red or blue?": "Red | Large", "Size?": "Small"})
    );
    // Explicit answers are looked up by question text.
    let (mut tr, _) = raise_two();
    let mut ans = Answer::new("ignored");
    ans.extra.insert(
        "answers".into(),
        json!({"Red or blue?": "Red", "Size?": "Small"}),
    );
    assert!(tr.respond("req-1#q1", &ans).unwrap().reply.is_none());
    let r = answer(&mut tr, "req-1#q0", &ans);
    assert_eq!(
        r["updatedInput"]["answers"],
        json!({"Red or blue?": "Red", "Size?": "Small"})
    );
}

#[test]
fn multi_question_decline_declines_the_request_once() {
    for (declined, sibling) in [("req-1#q0", "req-1#q1"), ("req-1#q1", "req-1#q0")] {
        let (mut tr, _) = raise_two();
        let out = tr.respond(declined, &Answer::new("decline")).unwrap();
        assert_eq!(
            inner(&out.reply.unwrap()),
            json!({"behavior": "deny", "message": DEFAULT_DECLINE_MESSAGE})
        );
        assert_eq!(out.events.len(), 1);
        let EventBody::AttentionResolved(r) = &out.events[0].body else {
            panic!("{:?}", out.events)
        };
        assert_eq!(r.attn_id, sibling);
        assert_eq!(r.by, ResolvedBy::Timeout);
        assert!(r.answer.is_none());
        assert_eq!(out.events[0].turn, Some(1));
        assert!(tr.pending_attention().is_empty());
        // The sibling is gone: no second response.
        assert!(tr.respond(sibling, &Answer::new("Small")).is_none());
    }
    // Declining after the other was answered: deny once, nothing left to withdraw.
    let (mut tr, _) = raise_two();
    assert!(tr
        .respond("req-1#q0", &Answer::new("Red"))
        .unwrap()
        .reply
        .is_none());
    let not_now = Answer {
        decision: "deny".into(),
        message: Some("not now".into()),
        ..Default::default()
    };
    let out = tr.respond("req-1#q1", &not_now).unwrap();
    assert!(out.events.is_empty());
    assert_eq!(
        inner(&out.reply.unwrap()),
        json!({"behavior": "deny", "message": "not now"})
    );
}

#[test]
fn runtime_cancel_resolves_the_attention() {
    let (mut tr, a) = raise(ask_request(color_question()));
    assert_eq!(tr.pending_attention(), ["req-1"]);
    let out = tr.translate(r#"{"type":"control_cancel_request","request_id":"req-1"}"#);
    assert!(out.reply.is_none());
    assert_eq!(out.events.len(), 1);
    let EventBody::AttentionResolved(r) = &out.events[0].body else {
        panic!("{:?}", out.events)
    };
    assert_eq!(r.attn_id, a.attn_id);
    assert_eq!(r.by, ResolvedBy::Timeout);
    assert!(r.answer.is_none());
    assert_eq!(out.events[0].turn, Some(1));
    assert!(out.events[0].raw.is_some());
    assert!(tr.pending_attention().is_empty());
    assert!(tr.respond("req-1", &Answer::new("Red")).is_none());
    // A cancel for something not pending (already answered, unknown) emits nothing.
    assert!(tr
        .translate(r#"{"type":"control_cancel_request","request_id":"req-1"}"#)
        .events
        .is_empty());
}

#[test]
fn runtime_cancel_resolves_every_open_question() {
    let (mut tr, _) = raise_two();
    tr.respond("req-1#q0", &Answer::new("Red")).unwrap();
    let out = tr.translate(r#"{"type":"control_cancel_request","request_id":"req-1"}"#);
    let ids: Vec<&str> = out
        .events
        .iter()
        .map(|e| match &e.body {
            EventBody::AttentionResolved(r) if r.by == ResolvedBy::Timeout => r.attn_id.as_str(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(ids, ["req-1#q1"]);
    assert!(tr.respond("req-1#q1", &Answer::new("Small")).is_none());

    let (mut tr, _) = raise_two();
    let out = tr.translate(r#"{"type":"control_cancel_request","request_id":"req-1"}"#);
    assert_eq!(out.events.len(), 2);
    assert!(tr.pending_attention().is_empty());
}

/// A `can_use_tool` for Bash `command`.
fn bash_request(command: &str) -> Value {
    json!({
        "subtype": "can_use_tool",
        "tool_name": "Bash",
        "input": {"command": command},
        "tool_use_id": "toolu_b",
    })
}

/// The events of a translated `control_request` for `req-1` carrying `request`.
fn reraise(tr: &mut Translator, request: Value) -> Vec<Emitted> {
    let line = json!({"type": "control_request", "request_id": "req-1", "request": request});
    let out = tr.translate(&line.to_string());
    assert!(out.reply.is_none());
    out.events
}

/// Claude re-raised a pending request id with other input, and the hub had
/// not yet ingested the replacement when the user's answer to the old (still hub-current)
/// question arrived. The answer to the old id must never approve the replacement: the old item
/// is withdrawn as superseded, the replacement gets a new id, and the old id answers nothing.
#[test]
fn a_replaced_request_is_never_answered_by_an_answer_to_the_old_one() {
    let (mut tr, old) = raise(bash_request("echo harmless"));
    assert_eq!(old.attn_id, "req-1");
    assert!(old.summary.contains("echo harmless"), "{}", old.summary);

    let evs = reraise(&mut tr, bash_request("rm -rf precious"));
    assert_eq!(evs.len(), 2, "{evs:?}");
    let EventBody::AttentionResolved(w) = &evs[0].body else {
        panic!("{evs:?}")
    };
    assert_eq!(w.attn_id, "req-1");
    assert_eq!(w.by, ResolvedBy::Timeout);
    assert!(w.answer.is_none());
    assert_eq!(ext(&w.ext, "claude")["superseded"], true);
    assert_eq!(evs[0].turn, Some(1));
    let EventBody::AttentionRaised(new) = &evs[1].body else {
        panic!("{evs:?}")
    };
    assert_ne!(new.attn_id, old.attn_id);
    assert_eq!(new.attn_id, "req-1~1");
    assert!(new.summary.contains("rm -rf precious"), "{}", new.summary);
    assert_eq!(tr.pending_attention(), ["req-1~1"]);

    // The user's allow of `echo harmless`, delivered after the replacement: nothing is written
    // (the runtime keeps waiting), nothing changes.
    assert!(tr.respond("req-1", &Answer::allow()).is_none());
    assert!(tr
        .respond(
            "req-1",
            &Answer {
                decision: "deny".into(),
                message: Some("free text for the old question".into()),
                ..Default::default()
            }
        )
        .is_none());
    assert_eq!(tr.pending_attention(), ["req-1~1"]);

    // The replacement is answered only through its own id, with its own input.
    assert_eq!(
        answer(&mut tr, "req-1~1", &Answer::allow()),
        json!({"behavior": "allow", "updatedInput": {"command": "rm -rf precious"}})
    );
    assert!(tr.pending_attention().is_empty());
    assert!(tr.respond("req-1~1", &Answer::allow()).is_none());

    // A third incarnation (the request id reused once more) gets yet another id.
    let evs = reraise(&mut tr, bash_request("ls"));
    assert_eq!(evs.len(), 1, "no open item to withdraw: {evs:?}");
    let EventBody::AttentionRaised(third) = &evs[0].body else {
        panic!("{evs:?}")
    };
    assert_eq!(third.attn_id, "req-1~2");
    assert!(tr.respond("req-1", &Answer::allow()).is_none());
    assert!(tr.respond("req-1~1", &Answer::allow()).is_none());
    assert_eq!(
        answer(&mut tr, "req-1~2", &Answer::deny()),
        json!({"behavior": "deny", "message": DEFAULT_DENY_MESSAGE})
    );
}

/// The same for questions: a free-text ("Other") answer to a replaced question never becomes
/// the answer of its replacement (a permission, or another question), and a partly answered
/// multi-question request that is replaced keeps none of its answers.
#[test]
fn a_replaced_question_takes_no_answer_meant_for_the_old_one() {
    let (mut tr, old) = raise(ask_request(color_question()));
    assert_eq!(old.attn_id, "req-1");
    let evs = reraise(&mut tr, bash_request("rm -rf precious"));
    let ids: Vec<(&str, &str)> = evs
        .iter()
        .map(|e| match &e.body {
            EventBody::AttentionResolved(r) => ("resolved", r.attn_id.as_str()),
            EventBody::AttentionRaised(a) => ("raised", a.attn_id.as_str()),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(ids, [("resolved", "req-1"), ("raised", "req-1~1")]);
    assert!(tr.respond("req-1", &Answer::new("Red")).is_none());
    assert!(tr.respond("req-1", &Answer::new("allow")).is_none());
    assert!(tr.respond("req-1", &Answer::allow()).is_none());
    assert_eq!(tr.pending_attention(), ["req-1~1"]);

    // A multi-question request, one answered, then replaced by a single question.
    let (mut tr, _) = raise_two();
    tr.respond("req-1#q0", &Answer::new("Red")).unwrap();
    let evs = reraise(&mut tr, ask_request(color_question()));
    let ids: Vec<(&str, &str)> = evs
        .iter()
        .map(|e| match &e.body {
            EventBody::AttentionResolved(r) => ("resolved", r.attn_id.as_str()),
            EventBody::AttentionRaised(a) => ("raised", a.attn_id.as_str()),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(ids, [("resolved", "req-1#q1"), ("raised", "req-1~1")]);
    assert!(tr.respond("req-1#q1", &Answer::new("Small")).is_none());
    assert!(tr.respond("req-1#q0", &Answer::new("Red")).is_none());
    // The replacement's answer carries only its own answer, not the old request's `Red`.
    assert_eq!(
        answer(&mut tr, "req-1~1", &Answer::new("Blue")),
        json!({"behavior": "allow", "updatedInput": {
            "questions": color_question(),
            "answers": {"Red or blue?": "Blue"},
        }})
    );
}

#[test]
fn interrupt() {
    let r = replay("interrupt");
    assert_eq!(
        shape(&r.events),
        [
            "thread.started",
            "delta@1",
            "text@1",
            "turn.ended@1",
            "delta@2",
            "text@2",
            "turn.ended@2",
        ]
    );
    let ended = r.ended();
    let e = ended[0];
    assert_eq!(e.result, TurnResult::Interrupted);
    // The CLI reports 0 for an interrupted turn: unknown, not free.
    assert_eq!(e.usage.as_ref().unwrap().cost, None);
    assert_eq!(
        ext(&e.ext, "claude")["terminal_reason"],
        "aborted_streaming"
    );
    assert_eq!(ended[1].result, TurnResult::Completed);
    assert!((ended[1].usage.as_ref().unwrap().cost.unwrap() - 0.0046028).abs() < 1e-9);
    assert_eq!(r.finals()[1], "still here");
}

/// A task notification's `result` that asked the model nothing (`claude_code::translate`).
const NOTIFICATION: &str = r#"{"type":"result","subtype":"success","is_error":false,"num_turns":0,"total_cost_usd":0,"origin":{"kind":"task-notification"},"session_id":"s"}"#;

#[test]
fn interrupt_without_terminal_reason() {
    // Older/newer CLIs may not send terminal_reason: stop_reason null and the
    // "[Request interrupted by user]" text still identify our interrupt.
    let lines: Vec<(String, String)> = fixture("interrupt")
        .into_iter()
        .map(|(d, l)| {
            let mut v = parse(&l);
            if v["type"] == "result" && v["terminal_reason"] == "aborted_streaming" {
                v.as_object_mut().unwrap().remove("terminal_reason");
                (d, v.to_string())
            } else {
                (d, l)
            }
        })
        .collect();
    assert!(lines.iter().all(|(_, l)| !l.contains("aborted_streaming")));
    let r = replay_lines(lines, TranslatorOptions::default(), 1);
    let ended = r.ended();
    assert_eq!(ended[0].result, TurnResult::Interrupted);
    assert_eq!(ended[0].usage.as_ref().unwrap().cost, None);
    assert_eq!(ended[1].result, TurnResult::Completed);

    // stop_reason set, but the interrupted text was seen in this turn.
    let mut tr = Translator::new(TranslatorOptions::default());
    tr.user_message("x", 1);
    tr.translate(r#"{"type":"system","subtype":"init","session_id":"s"}"#);
    assert!(tr.interrupt().is_some());
    tr.translate(
        r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]},"parent_tool_use_id":null}"#,
    );
    let out = tr.translate(
        r#"{"type":"result","subtype":"error_during_execution","is_error":true,"stop_reason":"tool_use","total_cost_usd":0}"#,
    );
    let EventBody::TurnEnded(e) = &out.events[0].body else {
        panic!()
    };
    assert_eq!(e.result, TurnResult::Interrupted);

    // The same with a task notification's `result` (which asks the model nothing) between the
    // interrupted text and the turn's own `result`: the turn keeps what it saw.
    let mut tr = Translator::new(TranslatorOptions::default());
    tr.user_message("x", 1);
    tr.translate(r#"{"type":"system","subtype":"init","session_id":"s"}"#);
    assert!(tr.interrupt().is_some());
    tr.translate(
        r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"[Request interrupted by user for tool use]"}]},"parent_tool_use_id":null}"#,
    );
    assert!(tr.translate(NOTIFICATION).events.is_empty());
    let out = tr.translate(
        r#"{"type":"result","subtype":"error_during_execution","is_error":true,"stop_reason":"tool_use","total_cost_usd":0}"#,
    );
    let EventBody::TurnEnded(e) = &out.events[0].body else {
        panic!()
    };
    assert_eq!(
        (out.events[0].turn, e.result),
        (Some(1), TurnResult::Interrupted)
    );

    // Our interrupt, but a clean result with a stop reason: completed (the turn beat it).
    tr.user_message("y", 2);
    assert!(tr.interrupt().is_some());
    let out = tr.translate(
        r#"{"type":"result","subtype":"success","is_error":false,"stop_reason":"end_turn","total_cost_usd":0.1}"#,
    );
    let EventBody::TurnEnded(e) = &out.events[0].body else {
        panic!()
    };
    assert_eq!(e.result, TurnResult::Completed);
}

#[test]
fn aborted_turn_without_our_interrupt_is_an_error() {
    let mut tr = Translator::new(TranslatorOptions::default());
    tr.user_message("x", 1);
    let out = tr.translate(
        r#"{"type":"result","subtype":"error_during_execution","is_error":true,"terminal_reason":"aborted_streaming","total_cost_usd":0,"session_id":"s"}"#,
    );
    let EventBody::TurnEnded(e) = &out.events[0].body else {
        panic!()
    };
    assert_eq!(e.result, TurnResult::Error);
    assert_eq!(ext(&e.ext, "claude")["transient"], false);
    // Interrupt with nothing running writes nothing.
    assert!(tr.interrupt().is_none());
}

#[test]
fn transient_errors_are_classified() {
    let mut tr = Translator::new(TranslatorOptions::default());
    tr.user_message("x", 1);
    let out = tr.translate(
        r#"{"type":"result","subtype":"success","is_error":true,"api_error_status":529,"result":"API Error: 529 overloaded","total_cost_usd":0.01,"session_id":"s"}"#,
    );
    let EventBody::TurnEnded(e) = &out.events[0].body else {
        panic!()
    };
    assert_eq!(e.result, TurnResult::Error);
    assert_eq!(ext(&e.ext, "claude")["transient"], true);
    assert_eq!(
        ext(&e.ext, "claude")["transient_reason"],
        "api_error_status 529"
    );
}

/// Claude Code 2.1.287 not signed in (lines recorded with an empty `CLAUDE_CONFIG_DIR`, an
/// invalid `ANTHROPIC_API_KEY` or expired OAuth credentials; cut down): the turn's error says
/// what to do, is never transient, and reaches `ext.twr.error` (the hub's error items).
#[test]
fn auth_failures_say_how_to_sign_in() {
    let assistant = |text: &str| {
        json!({"type":"assistant","message":{"id":"x","model":"<synthetic>","role":"assistant","stop_reason":"stop_sequence","type":"message","content":[{"type":"text","text":text}]},"parent_tool_use_id":null,"session_id":"s","error":"authentication_failed","is_api_error_message":true}).to_string()
    };
    let result = |text: &str, status: Value| {
        json!({"type":"result","subtype":"success","is_error":true,"api_error_status":status,"duration_ms":1357,"duration_api_ms":0,"num_turns":1,"result":text,"stop_reason":"stop_sequence","terminal_reason":"api_error","session_id":"s","total_cost_usd":0,"usage":{"input_tokens":0,"output_tokens":0},"permission_denials":[]}).to_string()
    };
    let retry = r#"{"type":"system","subtype":"api_retry","attempt":1,"max_retries":1,"retry_delay_ms":578,"error_status":401,"error":"authentication_failed","session_id":"s"}"#;
    let init = r#"{"type":"system","subtype":"init","session_id":"s","permissionMode":"default"}"#;
    for (text, status, lines) in [
        (
            "Failed to authenticate: OAuth session expired and could not be refreshed",
            Value::Null,
            vec![init.to_string()],
        ),
        (
            "Not logged in · Please run /login",
            Value::Null,
            vec![init.to_string()],
        ),
        (
            "Failed to authenticate. API Error: 401 API key is invalid.",
            json!(401),
            vec![init.to_string(), retry.to_string()],
        ),
    ] {
        let mut tr = Translator::new(TranslatorOptions::default());
        tr.user_message("say hi", 1);
        let mut events = Vec::new();
        for l in lines
            .iter()
            .cloned()
            .chain([assistant(text), result(text, status)])
        {
            events.extend(tr.translate(&l).events);
        }
        let ended = events
            .iter()
            .find_map(|e| match &e.body {
                EventBody::TurnEnded(t) => Some((t.clone(), e.turn)),
                _ => None,
            })
            .unwrap();
        assert_eq!(ended.1, Some(1));
        assert_eq!(ended.0.result, TurnResult::Error);
        assert_eq!(
            ext(&ended.0.ext, "twr")["error"],
            format!("{text} (sign in again: run `claude`, then /login)")
        );
        assert_eq!(ext(&ended.0.ext, "claude")["transient"], false);
        assert_eq!(ext(&ended.0.ext, "claude")["auth_failed"], true);
    }

    // The `assistant` error alone is enough, whatever the result says.
    let mut tr = Translator::new(TranslatorOptions::default());
    tr.user_message("x", 1);
    tr.translate(init);
    tr.translate(&assistant("Something new"));
    let out = tr.translate(&result("Something new", Value::Null));
    let EventBody::TurnEnded(e) = &out.events[0].body else {
        panic!()
    };
    assert!(ext(&e.ext, "twr")["error"]
        .as_str()
        .unwrap()
        .ends_with("then /login)"));
    // Also with a task notification's `result` between it and the turn's own.
    let mut other = Translator::new(TranslatorOptions::default());
    other.user_message("x", 1);
    other.translate(init);
    other.translate(&assistant("Something new"));
    assert!(other.translate(NOTIFICATION).events.is_empty());
    let out = other.translate(&result("Something new", Value::Null));
    let EventBody::TurnEnded(e) = &out.events[0].body else {
        panic!()
    };
    assert_eq!(out.events[0].turn, Some(1));
    assert!(ext(&e.ext, "twr")["error"]
        .as_str()
        .unwrap()
        .ends_with("then /login)"));

    // Per turn: a later failure that is not about signing in gets no hint, and a transient one
    // stays transient.
    tr.user_message("y", 2);
    let out = tr.translate(&result("API Error: 529 overloaded", json!(529)));
    let EventBody::TurnEnded(e) = &out.events[0].body else {
        panic!()
    };
    assert_eq!(ext(&e.ext, "twr")["error"], "API Error: 529 overloaded");
    assert_eq!(ext(&e.ext, "claude")["transient"], true);

    // A successful turn mentioning a login is just a turn.
    tr.user_message("z", 3);
    let out = tr.translate(
        r#"{"type":"result","subtype":"success","is_error":false,"result":"Run /login to sign in.","stop_reason":"end_turn","session_id":"s"}"#,
    );
    let EventBody::TurnEnded(e) = &out.events[0].body else {
        panic!()
    };
    assert_eq!(e.result, TurnResult::Completed);
    assert!(e.ext.as_ref().unwrap().get("twr").is_none());
}

/// Every failed turn says why in `ext.twr.error` (what the hub quotes in its error item): one
/// line from Claude Code's own report, the transient classification unchanged. The 404, budget
/// and max-turns lines were recorded from Claude Code 2.1.287 (`--model haiku`, an unknown
/// model, `--max-budget-usd 0.000001`, `--max-turns 1`) and cut down; the 529 text is the one
/// that CLI version builds after its retries.
#[test]
fn failed_turns_say_why() {
    let api_error = |error: &str, text: &str| {
        json!({"type":"assistant","message":{"id":"x","model":"<synthetic>","role":"assistant","stop_reason":"stop_sequence","type":"message","content":[{"type":"text","text":text}]},"parent_tool_use_id":null,"session_id":"s","error":error,"is_api_error_message":true}).to_string()
    };
    let cases: Vec<(Vec<String>, &str, bool)> = vec![
        // Overloaded (529) after Claude Code's retries.
        (
            vec![
                api_error("overloaded", "API Error: Repeated 529 Overloaded errors"),
                r#"{"type":"result","subtype":"success","is_error":true,"api_error_status":529,"duration_ms":61234,"duration_api_ms":0,"num_turns":1,"result":"API Error: Repeated 529 Overloaded errors","stop_reason":"stop_sequence","terminal_reason":"api_error","session_id":"s","total_cost_usd":0}"#.into(),
            ],
            "API Error: Repeated 529 Overloaded errors",
            true,
        ),
        // Rate limited (429), with the API's error body.
        (
            vec![
                api_error("rate_limit", "API Error: 429 rate limited"),
                r#"{"type":"result","subtype":"success","is_error":true,"api_error_status":429,"num_turns":1,"result":"API Error: 429 {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"This request would exceed the rate limit for your organization.\"}}","terminal_reason":"api_error","session_id":"s","total_cost_usd":0}"#.into(),
            ],
            r#"API Error: 429 {"type":"error","error":{"type":"rate_limit_error","message":"This request would exceed the rate limit for your organization."}}"#,
            true,
        ),
        // A rate limit whose result has no text: the status and the API error message.
        (
            vec![
                api_error("rate_limit", "Rate limit reached"),
                r#"{"type":"result","subtype":"error_during_execution","is_error":true,"api_error_status":429,"num_turns":1,"terminal_reason":"api_error","session_id":"s","total_cost_usd":0}"#.into(),
            ],
            "API error 429: Rate limit reached",
            true,
        ),
        // An unknown model (recorded).
        (
            vec![
                api_error("model_not_found", "There's an issue with the selected model (claude-nonexistent-model-xyz). It may not exist or you may not have access to it. Run --model to pick a different model."),
                r#"{"duration_api_ms":0,"stop_reason":"stop_sequence","session_id":"s","total_cost_usd":0,"usage":{"input_tokens":0,"output_tokens":0},"permission_denials":[],"terminal_reason":"api_error","is_error":true,"num_turns":1,"subtype":"success","api_error_status":404,"result":"There's an issue with the selected model (claude-nonexistent-model-xyz). It may not exist or you may not have access to it. Run --model to pick a different model.","type":"result","duration_ms":1706}"#.into(),
            ],
            "There's an issue with the selected model (claude-nonexistent-model-xyz). It may not exist or you may not have access to it. Run --model to pick a different model.",
            false,
        ),
        // error_during_execution with `errors`.
        (
            vec![
                r#"{"type":"result","subtype":"error_during_execution","is_error":true,"duration_ms":812,"num_turns":3,"stop_reason":null,"session_id":"s","total_cost_usd":0.02,"errors":["Tool permission stream closed before response received","[ede_diagnostic] result_type=user"]}"#.into(),
            ],
            "Tool permission stream closed before response received; [ede_diagnostic] result_type=user",
            false,
        ),
        // Max turns (recorded).
        (
            vec![
                r#"{"duration_api_ms":1712,"stop_reason":"tool_use","session_id":"s","total_cost_usd":0.0108398,"usage":{"input_tokens":10,"output_tokens":109},"permission_denials":[],"terminal_reason":"max_turns","is_error":true,"num_turns":2,"subtype":"error_max_turns","errors":["Reached maximum number of turns (1)"],"type":"result","duration_ms":3547}"#.into(),
            ],
            "Reached maximum number of turns (1)",
            false,
        ),
        // Max budget (recorded).
        (
            vec![
                r#"{"duration_api_ms":0,"stop_reason":"end_turn","session_id":"s","total_cost_usd":0.006639,"usage":{"input_tokens":0,"output_tokens":0},"permission_denials":[],"terminal_reason":"budget_exhausted","is_error":true,"num_turns":1,"subtype":"error_max_budget_usd","errors":["Reached maximum budget ($0.000001)"],"type":"result","duration_ms":1496}"#.into(),
            ],
            "Reached maximum budget ($0.000001)",
            false,
        ),
        // Nothing but a terminal reason.
        (
            vec![
                r#"{"type":"result","subtype":"error_during_execution","is_error":true,"terminal_reason":"aborted_streaming","total_cost_usd":0,"session_id":"s"}"#.into(),
            ],
            "Claude Code ended the turn: aborted_streaming",
            false,
        ),
    ];
    for (lines, why, transient) in cases {
        let mut tr = Translator::new(TranslatorOptions::default());
        tr.user_message("say hi", 1);
        let mut events = Vec::new();
        for l in [
            r#"{"type":"system","subtype":"init","session_id":"s","permissionMode":"default"}"#
                .to_string(),
        ]
        .into_iter()
        .chain(lines)
        {
            events.extend(tr.translate(&l).events);
        }
        let e = events
            .iter()
            .find_map(|e| match &e.body {
                EventBody::TurnEnded(t) => Some(t.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(e.result, TurnResult::Error, "{why}");
        assert_eq!(ext(&e.ext, "twr")["error"], why);
        assert_eq!(ext(&e.ext, "claude")["transient"], transient, "{why}");
        assert!(ext(&e.ext, "claude").get("auth_failed").is_none(), "{why}");
    }

    // Long causes are cut to about 500 chars.
    let mut tr = Translator::new(TranslatorOptions::default());
    tr.user_message("x", 1);
    let long = json!({"type":"result","subtype":"success","is_error":true,"result":"x".repeat(2000),"session_id":"s"});
    let out = tr.translate(&long.to_string());
    let EventBody::TurnEnded(e) = &out.events[0].body else {
        panic!()
    };
    let why = ext(&e.ext, "twr")["error"].as_str().unwrap();
    assert!(why.chars().count() <= 501, "{}", why.len());
    // One line, without terminal escapes or other control characters.
    tr.user_message("v", 2);
    let out = tr.translate(
        r#"{"type":"result","subtype":"success","is_error":true,"result":"\u001b[31mAPI Error:\u001b[0m 500\r\n\tInternal\u0007 error","session_id":"s"}"#,
    );
    let EventBody::TurnEnded(e) = &out.events[0].body else {
        panic!()
    };
    assert_eq!(ext(&e.ext, "twr")["error"], "API Error: 500 Internal error");

    // An API error message of an earlier turn is not this turn's cause.
    tr.user_message("y", 3);
    tr.translate(&api_error(
        "overloaded",
        "API Error: Repeated 529 Overloaded errors",
    ));
    tr.translate(r#"{"type":"result","subtype":"success","is_error":true,"api_error_status":529,"result":"API Error: Repeated 529 Overloaded errors","session_id":"s"}"#);
    tr.user_message("z", 4);
    let out = tr.translate(
        r#"{"type":"result","subtype":"error_during_execution","is_error":true,"terminal_reason":"api_error","session_id":"s"}"#,
    );
    let EventBody::TurnEnded(e) = &out.events[0].body else {
        panic!()
    };
    assert_eq!(
        ext(&e.ext, "twr")["error"],
        "Claude Code ended the turn: api_error"
    );

    // An interrupt the user asked for is not a failure to explain.
    tr.user_message("w", 5);
    tr.translate(r#"{"type":"system","subtype":"init","session_id":"s"}"#);
    assert!(tr.interrupt().is_some());
    let out = tr.translate(
        r#"{"type":"result","subtype":"error_during_execution","is_error":true,"terminal_reason":"aborted_streaming","total_cost_usd":0,"session_id":"s"}"#,
    );
    let EventBody::TurnEnded(e) = &out.events[0].body else {
        panic!()
    };
    assert_eq!(e.result, TurnResult::Interrupted);
    assert!(ext(&e.ext, "twr").is_null());
}

#[test]
fn resume_keeps_the_session_and_costs_restart() {
    let r1 = replay("resume_part1");
    let r2 = replay_with(
        "resume_part2",
        TranslatorOptions {
            resumed: true,
            ..Default::default()
        },
        2,
    );
    assert_eq!(
        shape(&r2.events),
        ["thread.started", "text@2", "turn.ended@2"]
    );
    let (s1, s2) = (r1.started()[0], r2.started()[0]);
    assert_eq!(s1.session_id, s2.session_id);
    assert_eq!(
        s2.session_id.as_deref(),
        Some("02f89c0d-a971-4b50-aee7-5b68c144e4f6")
    );
    assert_eq!(ext(&s2.ext, "claude")["resumed"], true);
    assert_eq!(r2.finals(), ["PAPAYA"]);
    // A new process starts its cumulative cost at zero.
    let cost = r2.ended()[0].usage.as_ref().unwrap().cost.unwrap();
    assert!((cost - 0.0030889).abs() < 1e-9);
}

#[test]
fn thinking_blocks_only_when_enabled() {
    let line = r#"{"type":"assistant","message":{"id":"m","content":[{"type":"thinking","thinking":"hmm","signature":"x"}]},"parent_tool_use_id":null,"session_id":"s"}"#;
    let mut off = Translator::new(TranslatorOptions::default());
    assert!(off.translate(line).events.is_empty());
    let mut on = Translator::new(TranslatorOptions {
        thinking: true,
        ..Default::default()
    });
    let out = on.translate(line);
    assert!(matches!(&out.events[0].body, EventBody::AgentThinking(t) if t.text == "hmm"));
    // The recorded sessions only carry redacted (empty) thinking: nothing is emitted.
    let r = replay_with(
        "session_deny",
        TranslatorOptions {
            thinking: true,
            ..Default::default()
        },
        1,
    );
    assert!(r
        .events
        .iter()
        .all(|e| !matches!(e.body, EventBody::AgentThinking(_))));
}

#[test]
fn turns_queue_fifo() {
    // Two prompts written back to back run as two turns, stamped in order.
    let mut tr = Translator::new(TranslatorOptions::default());
    tr.user_message("a", 5);
    tr.user_message("b", 6);
    let init = r#"{"type":"system","subtype":"init","session_id":"s"}"#;
    let text = |t: &str| {
        format!(
            r#"{{"type":"assistant","message":{{"id":"m","content":[{{"type":"text","text":"{t}"}}]}},"parent_tool_use_id":null}}"#
        )
    };
    let result = r#"{"type":"result","subtype":"success","total_cost_usd":0.1,"session_id":"s"}"#;
    let mut evs = Vec::new();
    for l in [init, &text("A"), result, init, &text("B"), result] {
        evs.extend(tr.translate(l).events);
    }
    assert_eq!(
        shape(&evs),
        [
            "thread.started",
            "text@5",
            "turn.ended@5",
            "text@6",
            "turn.ended@6"
        ]
    );
    assert!(!tr.busy());
}

// ---------------------------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------------------------

/// Stdout lines of `tests/fixtures/claude/usage.stdout.jsonl`: a status-only
/// `rate_limit_event` (recorded, 2.1.284), one with `unifiedWindows` and one with only the
/// limiting window's `utilization` (both as the 2.1.295 schema describes them), and the answer
/// to a `get_usage` read (recorded from 2.1.295 on 2026-10-09, trimmed).
fn usage_lines() -> Vec<String> {
    std::fs::read_to_string(fixture_path("usage").with_file_name("usage.stdout.jsonl"))
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

fn limits_in(out: &Translated) -> Vec<(RuntimeLimits, Option<u64>)> {
    out.events
        .iter()
        .filter_map(|e| match &e.body {
            EventBody::RuntimeLimits(l) => Some((l.clone(), e.turn)),
            _ => None,
        })
        .collect()
}

fn ts(s: &str) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::parse_from_rfc3339(s).unwrap().into()
}

fn window(id: &str, pct: f64, resets: Option<&str>) -> UsageWindow {
    UsageWindow {
        window: id.into(),
        used_pct: pct,
        resets_at: resets.map(ts),
        minutes: None,
        observed_at: None,
    }
}

/// A `rate_limit_event` with usage windows becomes `runtime.limits` (`source: event`, out of
/// any turn); one with a status only says nothing; a `get_usage` answer becomes
/// `runtime.limits` (`source: read`) with the plan.
#[test]
fn usage_windows_from_limit_events_and_reads() {
    let lines = usage_lines();
    let mut tr = Translator::new(TranslatorOptions::default());
    tr.user_message("x", 1);
    assert!(limits_in(&tr.translate(&lines[0])).is_empty());

    let got = limits_in(&tr.translate(&lines[1]));
    assert_eq!(got.len(), 1);
    let (l, turn) = &got[0];
    assert_eq!(*turn, None);
    assert_eq!(l.source, "event");
    assert_eq!(l.plan, None);
    assert_eq!(l.windows.len(), 2);
    assert_eq!(l.windows[0].window, "five_hour");
    assert!((l.windows[0].used_pct - 22.0).abs() < 1e-9);
    assert_eq!(l.windows[0].resets_at, Some(ts("2026-10-09T15:20:00Z")));
    assert_eq!(l.windows[1].window, "seven_day");
    assert!((l.windows[1].used_pct - 44.0).abs() < 1e-9);
    assert_eq!(l.windows[1].resets_at, Some(ts("2026-10-14T05:00:00Z")));

    // Without `unifiedWindows`: the limiting window alone.
    let got = limits_in(&tr.translate(&lines[2]));
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].0.windows.len(), 1);
    assert_eq!(got[0].0.windows[0].window, "seven_day");
    assert!((got[0].0.windows[0].used_pct - 91.0).abs() < 1e-9);

    let got = limits_in(&tr.translate(&lines[3]));
    assert_eq!(
        got,
        vec![(
            RuntimeLimits {
                windows: vec![
                    window("five_hour", 2.0, Some("2026-10-09T15:20:00.055826+00:00")),
                    window("seven_day", 35.0, Some("2026-10-14T05:00:00.055850+00:00")),
                ],
                source: "read".into(),
                plan: Some("max".into()),
                ..Default::default()
            },
            None
        )]
    );
}

static USAGE_NOW: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(1_791_540_000);

fn usage_clock() -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(USAGE_NOW.load(std::sync::atomic::Ordering::SeqCst), 0)
        .unwrap()
}

fn advance(minutes: i64) {
    USAGE_NOW.fetch_add(minutes * 60, std::sync::atomic::Ordering::SeqCst);
}

fn usage_reads(out: &Translated) -> Vec<Value> {
    out.requests.iter().map(|l| parse(l)).collect()
}

/// twr asks for the plan's usage (`get_usage`, no model turn) at the first `init`, and again
/// at an `init` or `result` once the last report is ten minutes old; a refused read stops
/// them.
#[test]
fn usage_is_read_when_due() {
    let result = r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","stop_reason":"end_turn","session_id":"s"}"#;
    let mut tr = Translator::new(TranslatorOptions::default());
    tr.set_clock(usage_clock);
    tr.user_message("x", 1);
    let out = tr.translate(&init_in("default"));
    assert_eq!(
        usage_reads(&out),
        vec![
            json!({"type": "control_request", "request_id": "twr_usage_1",
                    "request": {"subtype": "get_usage", "skip_behaviors": true}})
        ]
    );
    assert!(out.reply.is_none());
    advance(9);
    assert!(tr.translate(result).requests.is_empty());
    advance(1);
    let out = tr.translate(result);
    assert_eq!(usage_reads(&out)[0]["request_id"], "twr_usage_2");

    // A report of Claude's own counts as fresh data.
    advance(10);
    tr.translate(&usage_lines()[1]);
    advance(9);
    tr.user_message("y", 2);
    assert!(tr.translate(&init_in("default")).requests.is_empty());
    advance(1);
    assert_eq!(
        usage_reads(&tr.translate(result))[0]["request_id"],
        "twr_usage_3"
    );

    // Refused (an older Claude Code): no more reads.
    let refused = json!({"type": "control_response", "response": {"subtype": "error",
        "request_id": "twr_usage_3", "error": "Unsupported control request subtype: get_usage"}});
    assert!(tr.translate(&refused.to_string()).events.is_empty());
    advance(60);
    tr.user_message("z", 3);
    assert!(tr.translate(&init_in("default")).requests.is_empty());
    assert!(tr.translate(result).requests.is_empty());

    // An API key session: plan limits never apply, no more reads either.
    let mut tr = Translator::new(TranslatorOptions::default());
    tr.set_clock(usage_clock);
    tr.user_message("x", 1);
    assert_eq!(tr.translate(&init_in("default")).requests.len(), 1);
    let api_key = json!({"type": "control_response", "response": {"subtype": "success",
        "request_id": "twr_usage_1", "response": {"subscription_type": null,
        "rate_limits_available": false, "rate_limits": null}}});
    assert!(tr.translate(&api_key.to_string()).events.is_empty());
    advance(60);
    assert!(tr.translate(result).requests.is_empty());

    // The acks of twr's interrupts are not reads.
    let ack = json!({"type": "control_response", "response": {"subtype": "success",
        "request_id": "twr_interrupt_1", "response": {}}});
    assert!(tr.translate(&ack.to_string()).events.is_empty());
}

// ---------------------------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------------------------

#[test]
fn args_for_new_and_resumed_sessions() {
    let cfg = SpawnConfig {
        mode: "manual".into(),
        model: Some("haiku".into()),
        system_append: Some("be brief".into()),
        mcp_config: Some(json!({"mcpServers": {}})),
        allowed_tools: vec!["mcp__twr__send".into(), "Read".into()],
        ..Default::default()
    };
    let a = build_args(&cfg, "11111111-2222-4333-8444-555555555555");
    let joined = a.join(" ");
    assert!(joined.starts_with(
        "-p --input-format stream-json --output-format stream-json --verbose \
         --include-partial-messages --permission-mode manual --permission-prompts host \
         --permission-prompt-tool stdio --session-id 11111111-2222-4333-8444-555555555555"
    ));
    assert!(joined.contains("--model haiku"));
    assert!(joined.contains("--append-system-prompt be brief"));
    assert!(joined.contains("--mcp-config {\"mcpServers\":{}} --strict-mcp-config"));
    assert!(joined.ends_with("--allowedTools mcp__twr__send,Read"));

    let cfg = SpawnConfig {
        resume_session: Some("abc".into()),
        ..Default::default()
    };
    let a = build_args(&cfg, "abc");
    assert!(a.join(" ").contains("--permission-mode default"));
    assert!(a.join(" ").ends_with("--resume abc"));
    assert!(!a.contains(&"--session-id".to_string()));
    assert!(!a.contains(&"--model".to_string()));
    assert!(!a.contains(&"--tools".to_string()));

    // No built-in tools at all: `--tools ""` (one empty argument).
    let cfg = SpawnConfig {
        builtin_tools: Some(vec![]),
        ..Default::default()
    };
    let a = build_args(&cfg, "abc");
    assert_eq!(&a[a.len() - 2..], ["--tools".to_string(), String::new()]);
    let cfg = SpawnConfig {
        builtin_tools: Some(vec!["Read".into(), "Grep".into()]),
        ..Default::default()
    };
    assert!(build_args(&cfg, "abc")
        .join(" ")
        .ends_with("--tools Read,Grep"));
    // Settings layered over the user's (the orchestrator's guard hook): `--settings <json>`.
    let settings = serde_json::json!({"hooks": {"PreToolUse": []}});
    let cfg = SpawnConfig {
        settings: Some(settings.clone()),
        ..Default::default()
    };
    let a = build_args(&cfg, "abc");
    assert_eq!(
        &a[a.len() - 2..],
        ["--settings".to_string(), settings.to_string()]
    );
    let cmd = ClaudeCodeAdapter::with_bin("/opt/claude")
        .attach_command(&cfg, "abc")
        .unwrap();
    let args: Vec<_> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert!(
        args.windows(2)
            .any(|w| w == ["--settings", &settings.to_string()]),
        "{args:?}"
    );
}

#[test]
fn attach_command_resumes_in_the_worktree() {
    let cfg = SpawnConfig {
        cwd: "/tmp/wt".into(),
        mode: "acceptEdits".into(),
        ..Default::default()
    };
    let cmd = ClaudeCodeAdapter::with_bin("/opt/claude")
        .attach_command(&cfg, "sess-1")
        .unwrap();
    assert_eq!(cmd.get_program(), "/opt/claude");
    let args: Vec<_> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        args,
        ["--resume", "sess-1", "--permission-mode", "acceptEdits"]
    );
    assert_eq!(cmd.get_current_dir(), Some(Path::new("/tmp/wt")));
    // twr's secrets are removed from the interactive CLI's environment.
    let envs: Vec<_> = cmd.get_envs().collect();
    for k in twr_adapters::process::SCRUBBED_ENV {
        assert!(envs.contains(&(std::ffi::OsStr::new(k), None)), "{k}");
    }
    assert!(twr_adapters::find(claude_code::NAME).is_some());
}

/// `twr attach` runs the interactive CLI with the thread's configured variables (`[threads]
/// env`, `[projects.<name>] env`), as its headless runtime does, on this box and on another;
/// none of twr's own entries, and its secrets still removed.
#[test]
fn attach_commands_run_with_the_configured_environment() {
    let cfg = SpawnConfig {
        mode: "acceptEdits".into(),
        env: vec![
            ("CARGO_TARGET_DIR".into(), "/cache/api".into()),
            ("TWR_THREAD".into(), "t-1".into()),
        ],
        ..Default::default()
    };
    let a = ClaudeCodeAdapter::with_bin("/opt/claude");
    for cmd in [
        a.attach_command(&cfg, "s").unwrap(),
        a.remote_attach_command(&cfg, "s").unwrap(),
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

#[test]
fn attach_command_keeps_the_tool_restrictions() {
    // An attached orchestrator gets the same tool limits as its headless session.
    let cfg = SpawnConfig {
        mode: "dontAsk".into(),
        builtin_tools: Some(vec![]),
        allowed_tools: vec!["mcp__twr".into()],
        mcp_config: Some(serde_json::json!({ "mcpServers": {} })),
        ..Default::default()
    };
    let cmd = ClaudeCodeAdapter::with_bin("/opt/claude")
        .attach_command(&cfg, "sess-1")
        .unwrap();
    let args: Vec<_> = cmd
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        args,
        [
            "--resume",
            "sess-1",
            "--permission-mode",
            "dontAsk",
            "--mcp-config",
            r#"{"mcpServers":{}}"#,
            "--strict-mcp-config",
            "--allowedTools",
            "mcp__twr",
            "--tools",
            "",
        ]
    );
}

// ---------------------------------------------------------------------------------------------
// Driver against a stub `claude`
// ---------------------------------------------------------------------------------------------

const STUB_PY: &str = r#"
import json, os, sys
fixture, outdir, args = sys.argv[1], sys.argv[2], sys.argv[3:]
with open(os.path.join(outdir, "run.json"), "w") as f:
    json.dump({"args": args, "cwd": os.getcwd(), "thread": os.environ.get("TWR_THREAD"),
               "pgid": os.getpgid(0), "pid": os.getpid(), "env": dict(os.environ)}, f)
log = open(os.path.join(outdir, "stdin.jsonl"), "w")
usage = open(os.path.join(outdir, "usage.jsonl"), "w")
def keep(got):
    # twr's usage reads go to their own log: the recordings have none.
    if '"get_usage"' in got:
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

/// Writes a `claude` stub replaying `fixture` into `dir`; returns its path.
fn stub(dir: &Path, fixture: &Path) -> PathBuf {
    stub_with(dir, fixture, SILENT_PROBE)
}

/// [`stub`] answering `--help` / `--version` with `probe` (`case` arms, see [`cli_script`]).
fn stub_with(dir: &Path, fixture: &Path, probe: &str) -> PathBuf {
    let py = dir.join("stub.py");
    std::fs::write(&py, STUB_PY).unwrap();
    cli_script(
        dir,
        probe,
        &format!(
            "exec python3 {} {} {} \"$@\"",
            py.display(),
            fixture.display(),
            dir.display()
        ),
    )
}

/// `--help` and `--version` print nothing: the adapter learns nothing and passes every option.
const SILENT_PROBE: &str = "--help|--version) exit 0;;";

fn script(dir: &Path, body: &str) -> PathBuf {
    cli_script(dir, SILENT_PROBE, body)
}

/// A `claude` stub in `dir` that runs `body`, except for the adapter's `--help` / `--version`
/// probe, answered by `probe`: the arms of a `case "$1" in` (a stub that only ran `body`
/// would replay its fixture, or hang, there).
fn cli_script(dir: &Path, probe: &str, body: &str) -> PathBuf {
    let path = dir.join("claude");
    install_executable(
        &path,
        &format!("#!/bin/sh\ncase \"$1\" in\n{probe}\nesac\n{body}\n"),
    );
    path
}

/// Creates the executable `path` with `content` without this process ever holding it open for
/// writing. Tests run in parallel threads that fork: a child forked while this process had the
/// file open for writing keeps that descriptor until it execs, and exec'ing the file meanwhile
/// fails with ETXTBSY. So the bytes go to a side file and `cp` (a separate process, whose
/// descriptors no other fork inherits) writes the executable.
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

fn start(bin: &Path, cfg: SpawnConfig, prompt: Option<&str>) -> (AdapterHandle, Rx) {
    let mut h = ClaudeCodeAdapter::with_bin(bin)
        .stop_grace(Duration::from_millis(500))
        .spawn(cfg, prompt.map(str::to_string))
        .unwrap();
    let rx = h.take_events().unwrap();
    (h, rx)
}

/// Reads events until one matches; returns (body, turn) of everything read.
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

fn raised_in(evs: &[AdapterEvent]) -> AttentionRaised {
    evs.iter()
        .find_map(|e| match e {
            AdapterEvent::Emit {
                body: EventBody::AttentionRaised(a),
                ..
            } => Some(a.clone()),
            _ => None,
        })
        .unwrap()
}

fn recorded_stdin(name: &str) -> Vec<Value> {
    fixture(name)
        .into_iter()
        .filter(|(d, _)| d == ">>")
        .map(|(_, l)| parse(&l))
        .collect()
}

fn written_stdin(dir: &Path) -> Vec<Value> {
    std::fs::read_to_string(dir.join("stdin.jsonl"))
        .unwrap()
        .lines()
        .map(parse)
        .collect()
}

fn run_info(dir: &Path) -> Value {
    parse(&std::fs::read_to_string(dir.join("run.json")).unwrap())
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

/// At the driver: a session reporting a wider mode is interrupted, its turn ends
/// with an error, and the process is stopped with the reason in the stderr tail.
#[tokio::test]
async fn driver_stops_a_session_in_a_wider_mode() {
    let dir = tempfile::tempdir().unwrap();
    let rest = dir.path().join("rest.jsonl");
    let bin = script(
        dir.path(),
        &format!(
            "read first\necho '{}'\ncat > {}",
            init_in("bypassPermissions"),
            rest.display()
        ),
    );
    let cfg = SpawnConfig {
        cwd: dir.path().to_path_buf(),
        mode: "manual".into(),
        ..Default::default()
    };
    let (_h, mut rx) = start(&bin, cfg, Some("hi"));
    let evs = until(&mut rx, |e| is_kind(e, "turn.ended")).await;
    assert!(matches!(
        evs.last(),
        Some(AdapterEvent::Emit { body: EventBody::TurnEnded(t), turn: Some(1), .. })
            if t.result == TurnResult::Error
    ));
    assert!(!evs.iter().any(|e| is_kind(e, "thread.started")));
    let (_, tail) = exited(&mut rx).await;
    assert!(
        tail.contains("twr: ") && tail.contains("bypassPermissions"),
        "{tail}"
    );
    let rest = std::fs::read_to_string(&rest).unwrap();
    assert!(rest.contains("\"interrupt\""), "{rest}");
}

/// The orchestrator's MCP config holds its token: passed as a file path, it never shows in the
/// runtime's argv (readable by every local user) or environment.
#[tokio::test]
async fn an_mcp_config_file_keeps_its_secret_off_argv_and_env() {
    let dir = tempfile::tempdir().unwrap();
    let bin = stub(dir.path(), &fixture_path("session_allow_multiturn"));
    let secret = "0123456789abcdef-orchestrator-token";
    let file = dir.path().join("orch-mcp-test.json");
    std::fs::write(
        &file,
        json!({"mcpServers": {"twr": {"command": "/opt/twr", "args": ["mcp"],
                                       "env": {"TWR_ORCH_TOKEN": secret}}}})
        .to_string(),
    )
    .unwrap();
    let cfg = SpawnConfig {
        thread: "T1".into(),
        cwd: dir.path().to_path_buf(),
        mode: "dontAsk".into(),
        env: vec![("TWR_THREAD".into(), "T1".into())],
        // A file wins over inline JSON.
        mcp_config: Some(json!({"mcpServers": {"inline": {}}})),
        mcp_config_file: Some(file.clone()),
        allowed_tools: vec!["mcp__twr".into()],
        builtin_tools: Some(vec![]),
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
    let info = run_info(dir.path());
    let args: Vec<String> = serde_json::from_value(info["args"].clone()).unwrap();
    let at = args.iter().position(|a| a == "--mcp-config").unwrap();
    assert_eq!(args[at + 1], file.to_string_lossy());
    assert_eq!(args[at + 2], "--strict-mcp-config");
    assert!(
        !args
            .iter()
            .any(|a| a.contains(secret) || a.contains("inline")),
        "{args:?}"
    );
    let env = info["env"].as_object().unwrap();
    assert!(env
        .values()
        .all(|v| !v.as_str().unwrap_or_default().contains(secret)));
    assert!(!env.contains_key("TWR_ORCH_TOKEN"));
    assert_eq!(env["TWR_THREAD"], "T1");
    drop(h);
}

/// The runner token and the orchestrator token never reach the
/// runtime's environment, whether inherited by twr or put in `SpawnConfig::env`.
#[tokio::test]
async fn the_runtime_never_inherits_twr_secrets() {
    std::env::set_var("TWR_RUNNER_TOKEN", "runner-secret");
    let dir = tempfile::tempdir().unwrap();
    let bin = stub(dir.path(), &fixture_path("session_allow_multiturn"));
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
    let info = run_info(dir.path());
    let env = info["env"].as_object().unwrap();
    for k in ["TWR_RUNNER_TOKEN", "TWR_ORCH_TOKEN"] {
        assert!(!env.contains_key(k), "{k} leaked");
    }
    assert!(!info["env"].to_string().contains("secret"));
    assert_eq!(env["TWR_THREAD"], "T1");
    drop(h);
}

/// A runtime given `--settings` (the orchestrator's file guard) never runs with the variables
/// that would keep Claude Code from running its hook (bare mode, safe mode, a hook command
/// wrapper), whether configured or inherited; a runtime without keeps them. Its attach command
/// removes them too. The inherited case runs in a child process of this test binary that gets
/// them in its environment ([`guarded_spawn_child`]): this process's own is never changed.
#[tokio::test]
async fn a_guarded_runtime_never_inherits_what_drops_its_hook() {
    use twr_adapters::claude_code::guard;
    for guarded in [true, false] {
        // Configured.
        let dir = tempfile::tempdir().unwrap();
        let env = spawned_env(
            dir.path(),
            guarded,
            guard::HOOK_ENV
                .iter()
                .map(|k| (k.to_string(), "1".to_string()))
                .collect(),
        )
        .await;
        for k in guard::HOOK_ENV {
            assert_eq!(env.contains_key(k), !guarded, "{k}, guarded: {guarded}");
        }
        // Inherited.
        let dir = tempfile::tempdir().unwrap();
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["guarded_spawn_child", "--exact", "--test-threads=1"])
            .envs(guard::HOOK_ENV.iter().map(|k| (k, "1")))
            .env(CHILD_DIR_ENV, dir.path())
            .env(CHILD_GUARDED_ENV, if guarded { "1" } else { "0" })
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        let env = run_info(dir.path())["env"].as_object().unwrap().clone();
        for k in guard::HOOK_ENV {
            assert_eq!(
                env.contains_key(k),
                !guarded,
                "inherited {k}, guarded: {guarded}"
            );
        }
        // The attach command.
        let cfg = guarded_cfg(dir.path(), guarded, Vec::new());
        let cmd = ClaudeCodeAdapter::with_bin("/opt/claude")
            .attach_command(&cfg, "sess-1")
            .unwrap();
        let envs: Vec<_> = cmd.get_envs().collect();
        for k in guard::HOOK_ENV {
            let removed = envs.contains(&(std::ffi::OsStr::new(k), None));
            assert_eq!(removed, guarded, "{k}, guarded: {guarded}");
        }
    }
}

/// [`guarded_spawn_child`]'s directory: set only in the child process.
const CHILD_DIR_ENV: &str = "TWR_TEST_GUARDED_SPAWN_DIR";

/// Whether [`guarded_spawn_child`] spawns with the guard's `--settings` (`1`).
const CHILD_GUARDED_ENV: &str = "TWR_TEST_GUARDED_SPAWN";

/// Not a test of its own: in the child process [`a_guarded_runtime_never_inherits_what_drops_its_hook`]
/// starts (with [`CHILD_DIR_ENV`] set), spawns a runtime that inherits that process's
/// environment, so the stub records it; elsewhere it does nothing.
#[tokio::test]
async fn guarded_spawn_child() {
    let Some(dir) = std::env::var_os(CHILD_DIR_ENV) else {
        return;
    };
    let guarded = std::env::var(CHILD_GUARDED_ENV).is_ok_and(|g| g == "1");
    spawned_env(Path::new(&dir), guarded, Vec::new()).await;
}

/// A runtime config in `dir`, with the guard's `--settings` when `guarded`, and `env`.
fn guarded_cfg(dir: &Path, guarded: bool, env: Vec<(String, String)>) -> SpawnConfig {
    SpawnConfig {
        cwd: dir.to_path_buf(),
        env,
        settings: guarded
            .then(|| twr_adapters::claude_code::guard::settings(Path::new("/opt/twr"))),
        ..Default::default()
    }
}

/// The environment a stub runtime in `dir` ran with ([`guarded_cfg`]).
async fn spawned_env(
    dir: &Path,
    guarded: bool,
    env: Vec<(String, String)>,
) -> serde_json::Map<String, Value> {
    let bin = stub(dir, &fixture_path("session_allow_multiturn"));
    let (h, _rx) = start(&bin, guarded_cfg(dir, guarded, env), None);
    let run = dir.join("run.json");
    timeout(WAIT, async {
        while !std::fs::read_to_string(&run).is_ok_and(|s| s.ends_with('}')) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the stub never ran");
    drop(h);
    run_info(dir)["env"].as_object().unwrap().clone()
}

/// An over-long stdout line is never buffered whole; a `result` line that
/// long still ends its turn (as an error) and the session goes on.
#[tokio::test]
async fn an_oversized_result_line_ends_the_turn_and_reading_goes_on() {
    let dir = tempfile::tempdir().unwrap();
    let init = r#"{"type":"system","subtype":"init","session_id":"s-big","model":"m","permissionMode":"default"}"#;
    let big = format!(
        r#"{{"type":"result","subtype":"success","is_error":false,"result":"{}","session_id":"s-big","total_cost_usd":0.1}}"#,
        "x".repeat(200_000)
    );
    let small = r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","session_id":"s-big","total_cost_usd":0.2}"#;
    let user = r#"{"type":"user"}"#;
    let fx = dir.path().join("big.both.jsonl");
    std::fs::write(
        &fx,
        format!(">> {user}\n<< {init}\n<< {big}\n>> {user}\n<< {init}\n<< {small}\n"),
    )
    .unwrap();
    let bin = stub(dir.path(), &fx);
    let mut h = ClaudeCodeAdapter::with_bin(&bin)
        .stop_grace(Duration::from_millis(500))
        .max_line(64 * 1024)
        .spawn(
            SpawnConfig {
                cwd: dir.path().to_path_buf(),
                ..Default::default()
            },
            Some("one".into()),
        )
        .unwrap();
    let mut rx = h.take_events().unwrap();
    let evs = until(&mut rx, |e| is_kind(e, "turn.ended")).await;
    match evs.last() {
        Some(AdapterEvent::Emit {
            body: EventBody::TurnEnded(e),
            turn,
            raw,
        }) => {
            assert_eq!(*turn, Some(1));
            assert_eq!(e.result, TurnResult::Error);
            let twr = &e.ext.as_ref().unwrap()["twr"];
            assert_eq!(twr["error"], "oversized runtime message");
            assert!(twr["oversized_len"].as_u64().unwrap() > 200_000);
            assert!(raw.is_none());
        }
        other => panic!("{other:?}"),
    }
    h.send("two", 2).unwrap();
    let evs = until(&mut rx, |e| is_kind(e, "turn.ended")).await;
    match evs.last() {
        Some(AdapterEvent::Emit {
            body: EventBody::TurnEnded(e),
            turn,
            ..
        }) => {
            assert_eq!(*turn, Some(2));
            assert_eq!(e.result, TurnResult::Completed);
        }
        other => panic!("{other:?}"),
    }
    h.stop().unwrap();
    exited(&mut rx).await;
}

#[tokio::test]
async fn driver_allow_multi_turn_matches_the_recorded_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let bin = stub(dir.path(), &fixture_path("session_allow_multiturn"));
    let cfg = SpawnConfig {
        thread: "T1".into(),
        cwd: dir.path().to_path_buf(),
        mode: "default".into(),
        env: vec![("TWR_THREAD".into(), "T1".into())],
        prompt_turn: Some(4),
        ..Default::default()
    };
    let prompt =
        "Use the Bash tool to run exactly: touch hello.txt  -- then reply 'done'. Nothing else.";
    let (h, mut rx) = start(&bin, cfg, Some(prompt));

    let evs = until(&mut rx, |e| is_kind(e, "attention.raised")).await;
    match &evs[0] {
        AdapterEvent::Emit {
            body: EventBody::ThreadStarted(p),
            turn: None,
            ..
        } => {
            let pid = ext(&p.ext, "twr")["pid"].as_u64().unwrap();
            let info = run_info(dir.path());
            // The stub execs python, so the reported pid is the stub's, and it leads its group.
            assert_eq!(info["pid"].as_u64(), Some(pid));
            assert_eq!(info["pgid"].as_u64(), Some(pid));
        }
        other => panic!("{other:?}"),
    }
    let a = raised_in(&evs);
    h.respond("not-pending", Answer::allow()).unwrap();
    h.respond(&a.attn_id, Answer::allow()).unwrap();
    let evs = until(&mut rx, |e| is_kind(e, "turn.ended")).await;
    assert!(evs
        .iter()
        .all(|e| matches!(e, AdapterEvent::Emit { turn: Some(4), .. })));

    // Turns 5 and 6 sent back to back: Claude queues them and they keep their numbers.
    h.send(
        "What was the filename you just created? Answer with only the filename.",
        5,
    )
    .unwrap();
    h.send("Now reply with only the word: three", 6).unwrap();
    let evs = until(&mut rx, |e| {
        matches!(
            e,
            AdapterEvent::Emit {
                body: EventBody::TurnEnded(_),
                turn: Some(6),
                ..
            }
        )
    })
    .await;
    let turns: Vec<_> = evs
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::Emit {
                body: EventBody::TurnEnded(_),
                turn,
                ..
            } => *turn,
            _ => None,
        })
        .collect();
    assert_eq!(turns, [5, 6]);

    h.stop().unwrap();
    let (code, tail) = exited(&mut rx).await;
    assert_eq!(code, Some(0));
    assert_eq!(tail, "stub: stdin closed\n");
    // One usage read, after the first `init`: the turns all ran within ten minutes.
    let reads: Vec<Value> = std::fs::read_to_string(dir.path().join("usage.jsonl"))
        .unwrap()
        .lines()
        .map(parse)
        .collect();
    assert_eq!(reads.len(), 1, "{reads:?}");
    assert_eq!(reads[0]["request"]["subtype"], "get_usage");

    assert_eq!(
        written_stdin(dir.path()),
        recorded_stdin("session_allow_multiturn")
    );
    let info = run_info(dir.path());
    assert_eq!(info["thread"], "T1");
    assert_eq!(
        Path::new(info["cwd"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        dir.path().canonicalize().unwrap()
    );
    let args: Vec<String> = serde_json::from_value(info["args"].clone()).unwrap();
    assert_eq!(&args[..3], ["-p", "--input-format", "stream-json"]);
    assert!(args.contains(&"--permission-prompt-tool".to_string()));
    let sid = &args[args.iter().position(|a| a == "--session-id").unwrap() + 1];
    assert!(uuid::Uuid::parse_str(sid).is_ok(), "{sid}");
}

/// A replaced request at the driver: the runtime replaces a pending request before the hub
/// reads the replacement, and the answer to the old request arrives afterwards. Nothing is
/// written for it (the runtime keeps waiting); the replacement is answered only by its own id.
#[tokio::test]
async fn driver_never_answers_a_replaced_request_with_the_old_answer() {
    let dir = tempfile::tempdir().unwrap();
    let request = |cmd: &str| {
        json!({"type": "control_request", "request_id": "req-1", "request": bash_request(cmd)})
            .to_string()
    };
    let fx = dir.path().join("replace.both.jsonl");
    std::fs::write(
        &fx,
        format!(
            ">> (prompt)\n<< {}\n<< {}\n<< {}\n",
            r#"{"type":"system","subtype":"init","session_id":"s-1"}"#,
            request("echo harmless"),
            request("rm -rf precious"),
        ),
    )
    .unwrap();
    let bin = stub(dir.path(), &fx);
    let (h, mut rx) = start(&bin, SpawnConfig::default(), Some("go"));
    // The driver has translated both requests before the answer comes in: the "hub" below
    // answers the first one without having looked at what followed it.
    let evs = until(&mut rx, |e| {
        matches!(e, AdapterEvent::Emit { body: EventBody::AttentionRaised(a), .. } if a.attn_id != "req-1")
    })
    .await;
    let raised: Vec<&AttentionRaised> = evs
        .iter()
        .filter_map(|e| match e {
            AdapterEvent::Emit {
                body: EventBody::AttentionRaised(a),
                ..
            } => Some(a),
            _ => None,
        })
        .collect();
    assert_eq!(raised.len(), 2, "{evs:?}");
    assert_eq!(raised[0].attn_id, "req-1");
    let replacement = raised[1].attn_id.clone();
    assert!(evs.iter().any(|e| matches!(
        e,
        AdapterEvent::Emit { body: EventBody::AttentionResolved(r), .. }
            if r.attn_id == "req-1" && r.by == ResolvedBy::Timeout && r.answer.is_none()
    )));

    h.respond("req-1", Answer::allow()).unwrap();
    // Commands run in order: had the stale allow been written, it would come first.
    h.respond(&replacement, Answer::deny()).unwrap();
    let log = dir.path().join("stdin.jsonl");
    let lines = timeout(WAIT, async {
        loop {
            let text = std::fs::read_to_string(&log).unwrap_or_default();
            let lines: Vec<String> = text.lines().map(str::to_string).collect();
            if lines.len() >= 2 {
                return lines;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the answer to the replacement never reached the runtime");
    // Line 0 is the prompt; the only control_response is the deny of the replacement.
    assert_eq!(parse(&lines[0])["type"], "user");
    let replies: Vec<Value> = lines[1..].iter().map(|l| parse(l)).collect();
    assert_eq!(
        replies,
        [json!({"type": "control_response", "response": {
            "subtype": "success",
            "request_id": "req-1",
            "response": {"behavior": "deny", "message": DEFAULT_DENY_MESSAGE},
        }})]
    );
    h.stop().unwrap();
    exited(&mut rx).await;
    // Everything written: the prompt, the deny, and the stop's interrupt; never an allow.
    let written = written_stdin(dir.path());
    let responses: Vec<&Value> = written
        .iter()
        .filter(|l| l["type"] == "control_response")
        .collect();
    assert_eq!(responses.len(), 1, "{written:?}");
    assert!(
        !written.iter().any(|l| l.to_string().contains("allow")),
        "{written:?}"
    );
}

#[tokio::test]
async fn driver_deny_matches_the_recorded_stdin() {
    let dir = tempfile::tempdir().unwrap();
    let bin = stub(dir.path(), &fixture_path("session_deny"));
    let prompt = "Use the Bash tool to run exactly: touch denied.txt  -- if it fails, reply 'denied' and do not retry.";
    let (h, mut rx) = start(&bin, SpawnConfig::default(), Some(prompt));
    let a = raised_in(&until(&mut rx, |e| is_kind(e, "attention.raised")).await);
    let answer = Answer {
        decision: "deny".into(),
        message: Some("Host denied: not allowed in this test".into()),
        ..Default::default()
    };
    h.respond(&a.attn_id, answer).unwrap();
    let evs = until(&mut rx, |e| is_kind(e, "turn.ended")).await;
    assert!(evs.iter().any(|e| matches!(
        e,
        AdapterEvent::Emit { body: EventBody::AgentToolResult(r), .. } if !r.ok
    )));
    h.stop().unwrap();
    assert_eq!(exited(&mut rx).await.0, Some(0));
    assert_eq!(written_stdin(dir.path()), recorded_stdin("session_deny"));
}

#[tokio::test]
async fn driver_resume_passes_the_session() {
    let dir = tempfile::tempdir().unwrap();
    let bin = stub(dir.path(), &fixture_path("resume_part2"));
    let mut h = ClaudeCodeAdapter::with_bin(&bin)
        .resume(
            SpawnConfig::default(),
            "02f89c0d-a971-4b50-aee7-5b68c144e4f6".into(),
        )
        .unwrap();
    let mut rx = h.take_events().unwrap();
    h.send("What is the codeword? Reply with only the codeword.", 2)
        .unwrap();
    let evs = until(&mut rx, |e| is_kind(e, "turn.ended")).await;
    assert!(matches!(
        &evs[0],
        AdapterEvent::Emit {
            body: EventBody::ThreadStarted(_),
            turn: None,
            ..
        }
    ));
    h.stop().unwrap();
    exited(&mut rx).await;
    let args: Vec<String> = serde_json::from_value(run_info(dir.path())["args"].clone()).unwrap();
    let i = args.iter().position(|a| a == "--resume").unwrap();
    assert_eq!(args[i + 1], "02f89c0d-a971-4b50-aee7-5b68c144e4f6");
    assert_eq!(written_stdin(dir.path()), recorded_stdin("resume_part2"));
}

#[tokio::test]
async fn crash_reports_code_and_stderr_tail() {
    let dir = tempfile::tempdir().unwrap();
    let noise = "x".repeat(6000);
    let bin = script(
        dir.path(),
        &format!("echo '{noise}' >&2; echo 'fatal: bad flag' >&2; exit 2"),
    );
    let (_h, mut rx) = start(&bin, SpawnConfig::default(), Some("hi"));
    let (code, tail) = exited(&mut rx).await;
    assert_eq!(code, Some(2));
    assert_eq!(tail.len(), claude_code::STDERR_TAIL);
    assert!(tail.ends_with("fatal: bad flag\n"));
}

#[tokio::test]
async fn stop_kills_a_runtime_that_ignores_stdin_eof_and_its_group() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("child.pid");
    // Ignores stdin EOF and TERM; leaves a background child in its process group.
    let bin = script(
        dir.path(),
        &format!(
            "trap '' TERM; sleep 300 & echo $! > {}; while :; do sleep 1; done",
            pidfile.display()
        ),
    );
    let (h, mut rx) = start(&bin, SpawnConfig::default(), None);
    // The shell creates the file before writing the pid: wait for a whole number.
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
            // Gone, or a zombie waiting for its (dead) parent to be reaped by init.
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
async fn missing_binary_fails_spawn() {
    let err = ClaudeCodeAdapter::with_bin("/nonexistent/claude")
        .spawn(SpawnConfig::default(), None)
        .unwrap_err();
    assert!(format!("{err:#}").contains("/nonexistent/claude"));
}

#[tokio::test]
async fn exit_is_reported_while_a_leftover_process_holds_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("child.pid");
    let bin = script(
        dir.path(),
        &format!("sleep 30 & echo $! > {}; exit 0", pidfile.display()),
    );
    let started = std::time::Instant::now();
    let (_h, mut rx) = start(&bin, SpawnConfig::default(), None);
    let (code, _) = exited(&mut rx).await;
    assert_eq!(code, Some(0));
    assert!(started.elapsed() < Duration::from_secs(10));
    if let Ok(pid) = std::fs::read_to_string(&pidfile) {
        let _ = std::process::Command::new("kill").arg(pid.trim()).status();
    }
}

/// Waits until process `pid` is gone or a zombie.
async fn process_gone(pid: u32) -> bool {
    timeout(WAIT, async {
        loop {
            if gone_or_zombie(pid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok()
}

#[tokio::test]
async fn exit_kills_leftovers_in_the_group() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("child.pid");
    // Crashes on its own (no stop), leaving a background child in its process group.
    let bin = script(
        dir.path(),
        &format!("sleep 30 & echo $! > {}; exit 1", pidfile.display()),
    );
    let (_h, mut rx) = start(&bin, SpawnConfig::default(), None);
    let (code, _) = exited(&mut rx).await;
    assert_eq!(code, Some(1));
    let child: u32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let gone = process_gone(child).await;
    if !gone {
        let _ = std::process::Command::new("kill")
            .arg(child.to_string())
            .status();
    }
    assert!(gone, "background child {child} survived the runtime's exit");
}

// Linux only: macOS has no `setsid` command.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn stderr_reader_is_dropped_after_exit() {
    let dir = tempfile::tempdir().unwrap();
    let started = dir.path().join("started");
    let tried = dir.path().join("tried");
    let wrote = dir.path().join("wrote");
    // A leftover that left the process group (so the group kill misses it) keeps stderr open,
    // then writes to it. Once the adapter stopped reading, that write fails (EPIPE/SIGPIPE).
    // The runtime waits for the leftover to be in its own session before exiting.
    let bin = script(
        dir.path(),
        &format!(
            "setsid sh -c 'touch {s}; sleep 2; touch {t}; echo late >&2 && touch {w}' \
             >/dev/null </dev/null &\n\
             while [ ! -e {s} ]; do sleep 0.05; done; exit 0",
            s = started.display(),
            t = tried.display(),
            w = wrote.display()
        ),
    );
    let (_h, mut rx) = start(&bin, SpawnConfig::default(), None);
    let (code, _) = exited(&mut rx).await;
    assert_eq!(code, Some(0));
    timeout(WAIT, async {
        while !tried.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("leftover never ran");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!wrote.exists(), "stderr was still being read after exit");
}

#[tokio::test]
async fn driver_answers_unsupported_control_requests_with_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = dir.path().join("unsupported.both.jsonl");
    std::fs::write(
        &fixture,
        concat!(
            r#"<< {"type":"control_request","request_id":"hook-1","request":{"subtype":"hook_callback","callback_id":"x"}}"#,
            "\n",
            r#">> {"placeholder":"the adapter's reply"}"#,
            "\n",
            r#"<< {"type":"system","subtype":"init","session_id":"s"}"#,
            "\n",
        ),
    )
    .unwrap();
    let bin = stub(dir.path(), &fixture);
    let (h, mut rx) = start(&bin, SpawnConfig::default(), None);
    // The reply is written before the next stdout line is handled.
    until(&mut rx, |e| is_kind(e, "thread.started")).await;
    h.stop().unwrap();
    assert_eq!(exited(&mut rx).await.0, Some(0));
    // (stop then interrupts the turn the init opened: not checked here.)
    let written = written_stdin(dir.path());
    assert_eq!(written[0]["type"], "control_response");
    assert_eq!(written[0]["response"]["subtype"], "error");
    assert_eq!(written[0]["response"]["request_id"], "hook-1");
    assert!(written[0]["response"]["error"]
        .as_str()
        .unwrap()
        .contains("hook_callback"));
}

#[tokio::test]
async fn stop_is_not_blocked_by_a_runtime_that_stops_reading_stdin() {
    let dir = tempfile::tempdir().unwrap();
    // Never reads stdin, ignores TERM: a 256 KiB prompt fills the pipe and the write blocks.
    let bin = script(dir.path(), "trap '' TERM; while :; do sleep 1; done");
    let grace = Duration::from_millis(500);
    let mut h = ClaudeCodeAdapter::with_bin(&bin)
        .stop_grace(grace)
        .spawn(SpawnConfig::default(), Some("x".repeat(256 * 1024)))
        .unwrap();
    let mut rx = h.take_events().unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    // More queued input must not block the driver either.
    h.send("y".repeat(256 * 1024), 2).unwrap();
    let stopped = std::time::Instant::now();
    h.stop().unwrap();
    let (code, _) = timeout(grace + Duration::from_secs(1), exited(&mut rx))
        .await
        .expect("stop did not complete within grace + 1s");
    assert_eq!(code, None, "killed by a signal");
    assert!(stopped.elapsed() < grace + Duration::from_secs(1));
}

#[tokio::test]
async fn driver_multi_question_answers_once_and_declines_once() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = dir.path().join("questions.both.jsonl");
    let request = |id: &str| {
        json!({"type": "control_request", "request_id": id,
        "request": {
            "subtype": "can_use_tool", "tool_name": "AskUserQuestion",
            "input": {"questions": [
                {"question": "Red or blue?", "options": [{"label": "Red"}, {"label": "Blue"}]},
                {"question": "Size?", "options": [{"label": "Small"}, {"label": "Large"}]},
            ]},
            "tool_use_id": format!("toolu_{id}"),
        }})
        .to_string()
    };
    let lines = [
        format!(
            "<< {}",
            json!({"type": "system", "subtype": "init", "session_id": "s"})
        ),
        format!("<< {}", request("A")),
        ">> {}".to_string(),
        format!("<< {}", request("B")),
        ">> {}".to_string(),
        format!(
            "<< {}",
            json!({"type": "result", "subtype": "success", "stop_reason": "end_turn"})
        ),
    ];
    std::fs::write(&fixture, lines.join("\n") + "\n").unwrap();
    let bin = stub(dir.path(), &fixture);
    let (h, mut rx) = start(&bin, SpawnConfig::default(), None);

    let raised = |evs: &[AdapterEvent]| -> Vec<String> {
        evs.iter()
            .filter_map(|e| match e {
                AdapterEvent::Emit {
                    body: EventBody::AttentionRaised(a),
                    ..
                } => Some(a.attn_id.clone()),
                _ => None,
            })
            .collect()
    };
    let evs = until(&mut rx, |e| {
        matches!(e, AdapterEvent::Emit { body: EventBody::AttentionRaised(a), .. } if a.attn_id == "A#q1")
    })
    .await;
    assert_eq!(raised(&evs), ["A#q0", "A#q1"]);
    // Answered in reverse order: one allow with both answers.
    h.respond("A#q1", Answer::new("Large")).unwrap();
    h.respond("A#q0", Answer::new("Blue")).unwrap();
    let evs = until(&mut rx, |e| {
        matches!(e, AdapterEvent::Emit { body: EventBody::AttentionRaised(a), .. } if a.attn_id == "B#q1")
    })
    .await;
    assert_eq!(raised(&evs), ["B#q0", "B#q1"]);
    // Declining one question declines the request and withdraws its sibling.
    h.respond("B#q0", Answer::new("decline")).unwrap();
    let evs = until(&mut rx, |e| is_kind(e, "attention.resolved")).await;
    match evs.last() {
        Some(AdapterEvent::Emit {
            body: EventBody::AttentionResolved(r),
            ..
        }) => {
            assert_eq!(r.attn_id, "B#q1");
            assert_eq!(r.by, ResolvedBy::Timeout);
            assert!(r.answer.is_none());
        }
        other => panic!("{other:?}"),
    }
    until(&mut rx, |e| is_kind(e, "turn.ended")).await;
    h.stop().unwrap();
    assert_eq!(exited(&mut rx).await.0, Some(0));

    let written = written_stdin(dir.path());
    assert_eq!(written.len(), 2, "{written:?}");
    assert_eq!(written[0]["response"]["request_id"], "A");
    assert_eq!(
        written[0]["response"]["response"]["updatedInput"]["answers"],
        json!({"Red or blue?": "Blue", "Size?": "Large"})
    );
    assert_eq!(written[1]["response"]["request_id"], "B");
    assert_eq!(
        written[1]["response"]["response"],
        json!({"behavior": "deny", "message": DEFAULT_DECLINE_MESSAGE})
    );
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

// ---------------------------------------------------------------------------------------------
// What the installed `claude` supports (`--help` / `--version`)
// ---------------------------------------------------------------------------------------------

/// A `--help` listing every option the adapter passes, the `--permission-mode` choices
/// `modes`, and `--permission-prompts` when `prompts` (Claude Code 2.1.259 and newer).
fn help_text(prompts: bool, modes: &[&str]) -> String {
    let mut h = String::from("Usage: claude [options] [command] [prompt]\n\nOptions:\n");
    for o in [
        "--allowedTools, --allowed-tools <tools...>",
        "--append-system-prompt <prompt>",
        "--include-partial-messages",
        "--input-format <format>",
        "--mcp-config <configs...>",
        "--model <model>",
        "--output-format <format>",
    ] {
        h.push_str(&format!("  {o:<38}Something\n"));
    }
    let choices: Vec<String> = modes.iter().map(|m| format!("\"{m}\"")).collect();
    h.push_str(&format!(
        "  {:<38}Permission mode to use for the session\n{:40}(choices: {})\n",
        "--permission-mode <mode>",
        "",
        choices.join(", ")
    ));
    if prompts {
        h.push_str("  --permission-prompts <target>         Who answers permission prompts\n");
    }
    for o in [
        "-p, --print",
        "-r, --resume [value]",
        "--session-id <uuid>",
        "--strict-mcp-config",
        "--tools <tools...>",
        "--verbose",
    ] {
        h.push_str(&format!("  {o:<38}Something\n"));
    }
    h.push_str("\nCommands:\n  auth                                  Manage authentication\n");
    h
}

const MODES_NOW: &[&str] = &[
    "acceptEdits",
    "auto",
    "bypassPermissions",
    "manual",
    "dontAsk",
    "plan",
];
const MODES_BEFORE_2_1_200: &[&str] = &[
    "acceptEdits",
    "auto",
    "bypassPermissions",
    "default",
    "dontAsk",
    "plan",
];

/// `case` arms answering the probe as Claude Code `version` with `help`; every `--help` run is
/// counted in `<dir>/probes`.
fn cli_probe(dir: &Path, version: &str, help: &str) -> String {
    std::fs::write(dir.join("help.txt"), help).unwrap();
    format!(
        "--help) echo probed >> {d}/probes; cat {d}/help.txt; exit 0;;\n\
         --version) echo '{version} (Claude Code)'; exit 0;;",
        d = dir.display()
    )
}

fn probes(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join("probes"))
        .unwrap_or_default()
        .lines()
        .count()
}

/// A fixture that only starts the session.
fn init_only(dir: &Path) -> PathBuf {
    let fixture = dir.join("init.both.jsonl");
    std::fs::write(
        &fixture,
        concat!(
            r#"<< {"type":"system","subtype":"init","session_id":"s"}"#,
            "\n"
        ),
    )
    .unwrap();
    fixture
}

/// Starts `bin` with `cfg`, waits for the session, stops it; returns the arguments it got.
async fn args_of_a_run(bin: &Path, dir: &Path, cfg: SpawnConfig) -> Vec<String> {
    let (h, mut rx) = start(bin, cfg, None);
    until(&mut rx, |e| is_kind(e, "thread.started")).await;
    h.stop().unwrap();
    exited(&mut rx).await;
    serde_json::from_value(run_info(dir)["args"].clone()).unwrap()
}

fn has_pair(args: &[String], a: &str, b: &str) -> bool {
    args.windows(2).any(|w| w[0] == a && w[1] == b)
}

#[test]
fn args_follow_the_plan() {
    let cfg = SpawnConfig {
        mode: "manual".into(),
        ..Default::default()
    };
    let plan = claude_code::cli::Plan {
        system_prompt_snapshot: false,
        permission_prompts: false,
        mode: "default",
    };
    let a = claude_code::build_args_with(&cfg, "sid", plan).join(" ");
    assert_eq!(
        a,
        "-p --input-format stream-json --output-format stream-json --verbose \
         --include-partial-messages --permission-mode default --permission-prompt-tool stdio \
         --session-id sid"
    );
}

/// A Claude Code before 2.1.259 has no `--permission-prompts` and gets none (the user's
/// 2.1.197 failed with "unknown option '--permission-prompts'"); `--permission-prompt-tool
/// stdio` still routes its prompts to twr. Before 2.1.200 it calls `manual` `default`.
#[tokio::test]
async fn an_older_claude_gets_only_the_options_it_has() {
    let dir = tempfile::tempdir().unwrap();
    let probe = cli_probe(
        dir.path(),
        "2.1.197",
        &help_text(false, MODES_BEFORE_2_1_200),
    );
    let bin = stub_with(dir.path(), &init_only(dir.path()), &probe);
    let cfg = SpawnConfig {
        mode: "manual".into(),
        ..Default::default()
    };
    let args = args_of_a_run(&bin, dir.path(), cfg).await;
    assert!(
        !args.iter().any(|a| a == "--permission-prompts"),
        "{args:?}"
    );
    assert!(
        has_pair(&args, "--permission-prompt-tool", "stdio"),
        "{args:?}"
    );
    assert!(has_pair(&args, "--permission-mode", "default"), "{args:?}");
    assert_eq!(probes(dir.path()), 1);

    // Read once per binary: the next spawn uses what was found.
    let cfg = SpawnConfig {
        mode: "plan".into(),
        ..Default::default()
    };
    let args = args_of_a_run(&bin, dir.path(), cfg).await;
    assert!(has_pair(&args, "--permission-mode", "plan"), "{args:?}");
    assert!(
        !args.iter().any(|a| a == "--permission-prompts"),
        "{args:?}"
    );
    assert_eq!(probes(dir.path()), 1);

    // `claude update` replaces the binary: read again.
    let probe = cli_probe(dir.path(), "2.1.287", &help_text(true, MODES_NOW));
    let bin = stub_with(
        dir.path(),
        &init_only(dir.path()),
        &format!("{probe}\n# updated"),
    );
    let cfg = SpawnConfig {
        mode: "manual".into(),
        ..Default::default()
    };
    let args = args_of_a_run(&bin, dir.path(), cfg).await;
    assert!(has_pair(&args, "--permission-prompts", "host"), "{args:?}");
    assert!(has_pair(&args, "--permission-mode", "manual"), "{args:?}");
    assert_eq!(probes(dir.path()), 2);
}

/// What this box read of its own `claude` fits only this box's attaches: the attach the hub
/// builds for a thread on a runner's box (whose `claude` may be newer) passes the thread's
/// effort and mode as they are, where the local one leaves out what the probed version lacks
/// (it would refuse the whole command line). Neither probes.
#[tokio::test]
async fn only_a_local_attach_is_fitted_to_the_probed_claude() {
    let dir = tempfile::tempdir().unwrap();
    let probe = cli_probe(
        dir.path(),
        "2.1.197",
        &help_text(false, MODES_BEFORE_2_1_200),
    );
    let bin = stub_with(dir.path(), &init_only(dir.path()), &probe);
    // A spawn reads (and caches) what this `claude` takes: no `--effort`, `manual` is `default`.
    args_of_a_run(&bin, dir.path(), SpawnConfig::default()).await;
    assert_eq!(probes(dir.path()), 1);
    let cfg = SpawnConfig {
        cwd: dir.path().into(),
        mode: "manual".into(),
        effort: Some("high".into()),
        ..Default::default()
    };
    let adapter = ClaudeCodeAdapter::with_bin(&bin);
    let args = |cmd: std::process::Command| -> Vec<String> {
        assert_eq!(cmd.get_program(), bin.as_os_str());
        cmd.get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    };
    assert_eq!(
        args(adapter.attach_command(&cfg, "s-1").unwrap()),
        ["--resume", "s-1", "--permission-mode", "default"]
    );
    assert_eq!(
        args(adapter.remote_attach_command(&cfg, "s-1").unwrap()),
        [
            "--resume",
            "s-1",
            "--permission-mode",
            "manual",
            "--effort",
            "high"
        ]
    );
    assert_eq!(probes(dir.path()), 1);
}

#[tokio::test]
async fn a_current_claude_gets_permission_prompts_host() {
    let dir = tempfile::tempdir().unwrap();
    let probe = cli_probe(dir.path(), "2.1.287", &help_text(true, MODES_NOW));
    let bin = stub_with(dir.path(), &init_only(dir.path()), &probe);
    let args = args_of_a_run(&bin, dir.path(), SpawnConfig::default()).await;
    assert!(has_pair(&args, "--permission-prompts", "host"), "{args:?}");
    assert!(
        has_pair(&args, "--permission-prompt-tool", "stdio"),
        "{args:?}"
    );
    // `default` (unset) is passed as written: current versions still take it.
    assert!(has_pair(&args, "--permission-mode", "default"), "{args:?}");
}

/// A version older than the minimum does not start: the first spawn (while the binary is
/// being probed) reports it as the runtime's exit, later ones fail the spawn; both say what to
/// do, program first.
#[tokio::test]
async fn a_too_old_claude_is_refused_with_the_update_hint() {
    let dir = tempfile::tempdir().unwrap();
    let probe = cli_probe(
        dir.path(),
        "2.1.100",
        &help_text(false, MODES_BEFORE_2_1_200),
    );
    let bin = stub_with(dir.path(), &init_only(dir.path()), &probe);
    let want = format!(
        "`{}` 2.1.100 is too old for twr (needs {} or newer): run `claude update`",
        bin.display(),
        claude_code::cli::MIN_VERSION
    );
    let (_h, mut rx) = start(&bin, SpawnConfig::default(), Some("hi"));
    let (code, tail) = exited(&mut rx).await;
    assert_eq!(code, Some(1));
    assert_eq!(tail, format!("{want}\n"));
    let err = ClaudeCodeAdapter::with_bin(&bin)
        .spawn(SpawnConfig::default(), None)
        .unwrap_err();
    assert_eq!(format!("{err:#}"), want);
    // It never ran.
    assert!(!dir.path().join("run.json").exists());
    assert_eq!(probes(dir.path()), 1);
}

/// A permission mode the installed version does not list is refused, never replaced by
/// another one.
#[tokio::test]
async fn a_mode_the_claude_lacks_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let probe = cli_probe(
        dir.path(),
        "2.1.150",
        &help_text(false, &["acceptEdits", "default", "plan"]),
    );
    let bin = stub_with(dir.path(), &init_only(dir.path()), &probe);
    // Probed by a first spawn.
    args_of_a_run(&bin, dir.path(), SpawnConfig::default()).await;
    let cfg = SpawnConfig {
        mode: "auto".into(),
        ..Default::default()
    };
    let err = ClaudeCodeAdapter::with_bin(&bin)
        .spawn(cfg, None)
        .unwrap_err();
    assert_eq!(
        format!("{err:#}"),
        format!(
            "`{}` 2.1.150 does not support permission mode \"auto\" (the thread's mode; \
             `twr mode <thread> <mode>` changes it, `[threads] default_mode` sets it for new \
             threads): run `claude update`",
            bin.display()
        )
    );
}

/// `--help` that does not answer in time: nothing is guessed, every option is passed.
#[tokio::test]
async fn a_help_that_hangs_passes_every_option() {
    let dir = tempfile::tempdir().unwrap();
    let probe = "--help|--version) sleep 30; exit 0;;";
    let bin = stub_with(dir.path(), &init_only(dir.path()), probe);
    let (h, mut rx) = {
        let mut h = ClaudeCodeAdapter::with_bin(&bin)
            .stop_grace(Duration::from_millis(500))
            .probe_timeout(Duration::from_millis(300))
            .spawn(SpawnConfig::default(), None)
            .unwrap();
        let rx = h.take_events().unwrap();
        (h, rx)
    };
    until(&mut rx, |e| is_kind(e, "thread.started")).await;
    h.stop().unwrap();
    exited(&mut rx).await;
    let args: Vec<String> = serde_json::from_value(run_info(dir.path())["args"].clone()).unwrap();
    assert!(has_pair(&args, "--permission-prompts", "host"), "{args:?}");
}

/// The backstop: a `claude` that refuses its command line (here its `--help` failed, so the
/// adapter passed everything) gets the update hint as the last line of the crash.
#[tokio::test]
async fn a_refused_command_line_gets_the_update_hint() {
    let dir = tempfile::tempdir().unwrap();
    let bin = cli_script(
        dir.path(),
        "--help|--version) exit 1;;",
        "echo \"error: unknown option '--permission-prompts'\" >&2; exit 1",
    );
    let (_h, mut rx) = start(&bin, SpawnConfig::default(), Some("hi"));
    let (code, tail) = exited(&mut rx).await;
    assert_eq!(code, Some(1));
    let last = tail.lines().last().unwrap();
    assert_eq!(
        last,
        format!(
            "`{}` refused twr's command line (error: unknown option '--permission-prompts'): \
             run `claude update` (twr needs Claude Code {} or newer)",
            bin.display(),
            claude_code::cli::MIN_VERSION
        )
    );
    assert!(tail.starts_with("error: unknown option '--permission-prompts'\n"));
}

#[test]
fn system_prompt_refresh_is_optional_and_supported_on_resume() {
    let cfg = SpawnConfig {
        refresh_system_prompt: true,
        resume_session: Some("sid".into()),
        ..Default::default()
    };
    for supported in [false, true] {
        let args = claude_code::build_args_with(
            &cfg,
            "sid",
            claude_code::cli::Plan {
                permission_prompts: true,
                mode: "default",
                system_prompt_snapshot: supported,
            },
        );
        assert_eq!(
            has_pair(&args, "--system-prompt-snapshot", "off"),
            supported
        );
        assert!(has_pair(&args, "--resume", "sid"));
    }
    let worker = SpawnConfig::default();
    assert!(!build_args(&worker, "sid")
        .iter()
        .any(|a| a == "--system-prompt-snapshot"));
}

/// A session id that would read as an option (`claude --resume` takes an optional value, so
/// `--dangerously-skip-permissions` there is a flag) gets no attach command, and no headless
/// resume either.
#[test]
fn a_session_id_that_reads_as_an_option_is_never_passed() {
    let cfg = SpawnConfig {
        cwd: "/tmp/wt".into(),
        mode: "plan".into(),
        ..Default::default()
    };
    let a = ClaudeCodeAdapter::with_bin("/opt/claude");
    for bad in ["--dangerously-skip-permissions", "-x", "", "a b"] {
        assert!(a.attach_command(&cfg, bad).is_none(), "{bad:?}");
        assert!(a.remote_attach_command(&cfg, bad).is_none(), "{bad:?}");
    }
    assert!(a
        .attach_command(&cfg, "02f89c0d-a971-4b50-aee7-5b68c144e4f6")
        .is_some());
}
